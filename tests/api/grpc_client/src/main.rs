//! veil の gRPC サーバ（F-197）の相互運用試験。tonic のクライアント（HTTP/2）と quinn + h3（HTTP/3）で
//! examples/api/rust-grpc を叩き、`ok <名前>` / `FAIL <名前>: <理由>` を 1 行ずつ出す。1 件でも失敗すれば終了コード 1。
//!
//! gRPC の interop テストのサブセット（empty_unary・large_unary・client_streaming・server_streaming・
//! ping_pong・timeout_on_sleeping_server・status_code_and_message・cancel_after_first_response 相当）を含む。

use bytes::{Buf, BufMut, Bytes, BytesMut};
use prost::Message;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::StreamExt;
use tonic::codec::CompressionEncoding;
use tonic::{Code, Request};

pub mod demo {
    tonic::include_proto!("demo.v1");
}
use demo::demo_client::DemoClient;
use demo::*;

static mut FAILED: bool = false;

fn report(name: &str, r: Result<(), String>) {
    match r {
        Ok(()) => println!("ok   {}", name),
        Err(e) => {
            println!("FAIL {}: {}", name, e);
            // 単一スレッドの main からだけ呼ぶ
            unsafe { FAILED = true };
        }
    }
}

macro_rules! ensure {
    ($cond:expr, $($fmt:tt)*) => {
        if !$cond {
            return Err(format!($($fmt)*));
        }
    };
}

type Client = DemoClient<tonic::transport::Channel>;

async fn unary(c: &mut Client) -> Result<(), String> {
    let mut req = Request::new(HelloRequest {
        name: "veil".into(),
    });
    req.metadata_mut()
        .insert("x-caller", "tonic".parse().unwrap());
    let resp = c.say_hello(req).await.map_err(|s| s.to_string())?;
    ensure!(
        resp.metadata()
            .get("x-caller-echo")
            .map(|v| v.to_str().unwrap())
            == Some("tonic"),
        "metadata echo missing: {:?}",
        resp.metadata()
    );
    // tonic は単項の応答でトレーラをメタデータへ合流させる
    ensure!(
        resp.metadata().get("x-served-by").is_some(),
        "trailer missing: {:?}",
        resp.metadata()
    );
    ensure!(
        resp.get_ref().message == "Hello veil",
        "got {:?}",
        resp.get_ref()
    );
    Ok(())
}

async fn empty_unary(c: &mut Client) -> Result<(), String> {
    let resp = c
        .say_hello(HelloRequest::default())
        .await
        .map_err(|s| s.to_string())?;
    ensure!(
        resp.get_ref().message == "Hello ",
        "got {:?}",
        resp.get_ref()
    );
    Ok(())
}

async fn large_unary(c: &mut Client) -> Result<(), String> {
    let name = "x".repeat(3 * 1024 * 1024);
    let resp = c
        .clone()
        .max_decoding_message_size(8 * 1024 * 1024)
        .say_hello(HelloRequest { name })
        .await
        .map_err(|s| s.to_string())?;
    ensure!(
        resp.get_ref().message.len() == 6 + 3 * 1024 * 1024,
        "len {}",
        resp.get_ref().message.len()
    );
    Ok(())
}

async fn too_large(c: &mut Client) -> Result<(), String> {
    let name = "x".repeat(5 * 1024 * 1024);
    match c.say_hello(HelloRequest { name }).await {
        Err(s) if s.code() == Code::ResourceExhausted => Ok(()),
        other => Err(format!(
            "want RESOURCE_EXHAUSTED, got {:?}",
            other.map(|_| ())
        )),
    }
}

async fn gzip(c: &mut Client) -> Result<(), String> {
    let resp = c
        .clone()
        .send_compressed(CompressionEncoding::Gzip)
        .say_hello(HelloRequest {
            name: "z".repeat(10_000),
        })
        .await
        .map_err(|s| s.to_string())?;
    ensure!(
        resp.get_ref().message.len() == 10_006,
        "len {}",
        resp.get_ref().message.len()
    );
    Ok(())
}

/// B-127: 解凍すると上限（4MiB）を大きく超えるメッセージ（圧縮後は 64KB 程度）は RESOURCE_EXHAUSTED。
/// その後もサーバが応答すること。
async fn gzip_bomb(c: &mut Client) -> Result<(), String> {
    let r = c
        .clone()
        .send_compressed(CompressionEncoding::Gzip)
        .say_hello(HelloRequest {
            name: "x".repeat(64 * 1024 * 1024),
        })
        .await;
    match r {
        Err(s) if s.code() == Code::ResourceExhausted => {}
        other => {
            return Err(format!(
                "want RESOURCE_EXHAUSTED, got {:?}",
                other.map(|_| ())
            ))
        }
    }
    unary(c).await
}

async fn server_streaming(c: &mut Client) -> Result<(), String> {
    let mut s = c
        .count_up(CountRequest { count: 1000 })
        .await
        .map_err(|s| s.to_string())?
        .into_inner();
    let mut n = 0u32;
    while let Some(m) = s.message().await.map_err(|s| s.to_string())? {
        n += 1;
        ensure!(m.n == n, "out of order: {} at {}", m.n, n);
    }
    ensure!(n == 1000, "got {} messages", n);
    Ok(())
}

async fn server_streaming_error(c: &mut Client) -> Result<(), String> {
    match c.count_up(CountRequest { count: 1_000_000 }).await {
        Err(s) if s.code() == Code::InvalidArgument => Ok(()),
        other => Err(format!(
            "want INVALID_ARGUMENT, got {:?}",
            other.map(|_| ())
        )),
    }
}

async fn client_streaming(c: &mut Client) -> Result<(), String> {
    let nums = tokio_stream::iter((1..=100).map(|v| Number { value: v }));
    let r = c.sum(nums).await.map_err(|s| s.to_string())?.into_inner();
    ensure!(r.total == 5050 && r.count == 100, "got {:?}", r);
    // 0 件
    let r = c
        .sum(tokio_stream::iter(Vec::<Number>::new()))
        .await
        .map_err(|s| s.to_string())?
        .into_inner();
    ensure!(r.total == 0 && r.count == 0, "got {:?}", r);
    Ok(())
}

/// 1 通送るたびに返事を待つ（要求と応答が全二重で流れることの確認）。
async fn ping_pong(c: &mut Client) -> Result<(), String> {
    let (tx, rx) = tokio::sync::mpsc::channel::<ChatMessage>(1);
    tx.send(ChatMessage { text: "m0".into() }).await.unwrap();
    let mut s = c
        .chat(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .map_err(|s| s.to_string())?
        .into_inner();
    for i in 0..20 {
        let got = tokio::time::timeout(Duration::from_secs(5), s.message())
            .await
            .map_err(|_| format!("no reply for message {} (not full duplex?)", i))?
            .map_err(|s| s.to_string())?
            .ok_or("stream ended early")?;
        ensure!(got.text == format!("M{}", i), "got {:?}", got);
        if i < 19 {
            tx.send(ChatMessage {
                text: format!("m{}", i + 1),
            })
            .await
            .unwrap();
        }
    }
    drop(tx);
    ensure!(
        s.message().await.map_err(|s| s.to_string())?.is_none(),
        "stream did not end"
    );
    Ok(())
}

async fn deadline(c: &mut Client) -> Result<(), String> {
    let mut req = Request::new(SleepRequest { millis: 3000 });
    req.set_timeout(Duration::from_millis(200));
    let t = std::time::Instant::now();
    // tonic はクライアント側でも同じ期限で打ち切る（Cancelled）。サーバ側の強制は HTTP/3 の試験で見る。
    match c.sleep(req).await {
        Err(s) if s.code() == Code::DeadlineExceeded || s.code() == Code::Cancelled => {
            ensure!(
                t.elapsed() < Duration::from_secs(2),
                "took {:?}",
                t.elapsed()
            );
            Ok(())
        }
        other => Err(format!(
            "want DEADLINE_EXCEEDED, got {:?}",
            other.map(|_| ())
        )),
    }
}

async fn status_code_and_message(c: &mut Client) -> Result<(), String> {
    let msg = "nope: 名前 100% \t done";
    match c
        .fail(FailRequest {
            code: 7,
            message: msg.into(),
        })
        .await
    {
        Err(s) if s.code() == Code::PermissionDenied && s.message() == msg => Ok(()),
        other => Err(format!("got {:?}", other.map(|_| ()))),
    }
}

async fn cancel_after_first_response(c: &mut Client) -> Result<(), String> {
    let mut s = c
        .count_up(CountRequest { count: 100_000 })
        .await
        .map_err(|s| s.to_string())?
        .into_inner();
    let first = s.message().await.map_err(|s| s.to_string())?;
    ensure!(first.map(|m| m.n) == Some(1), "first message");
    drop(s);
    // 取り消し後も同じ接続で次の呼び出しが通る
    unary(c).await
}

async fn unimplemented(ch: tonic::transport::Channel) -> Result<(), String> {
    let mut grpc = tonic::client::Grpc::new(ch);
    grpc.ready().await.map_err(|e| e.to_string())?;
    let codec = tonic::codec::ProstCodec::<HelloRequest, HelloReply>::default();
    let path = http::uri::PathAndQuery::from_static("/demo.v1.Demo/NoSuchMethod");
    match grpc
        .unary(Request::new(HelloRequest::default()), path, codec)
        .await
    {
        Err(s) if s.code() == Code::Unimplemented => Ok(()),
        other => Err(format!("want UNIMPLEMENTED, got {:?}", other.map(|_| ()))),
    }
}

async fn health(ch: tonic::transport::Channel) -> Result<(), String> {
    use tonic_health::pb::health_client::HealthClient;
    use tonic_health::pb::{health_check_response::ServingStatus, HealthCheckRequest};
    let mut h = HealthClient::new(ch);
    let r = h
        .check(HealthCheckRequest {
            service: String::new(),
        })
        .await
        .map_err(|s| s.to_string())?;
    ensure!(
        r.get_ref().status == ServingStatus::Serving as i32,
        "status {:?}",
        r.get_ref()
    );
    match h
        .check(HealthCheckRequest {
            service: "no.Such".into(),
        })
        .await
    {
        Err(s) if s.code() == Code::NotFound => {}
        other => return Err(format!("unknown service: {:?}", other.map(|_| ()))),
    }
    let mut w = h
        .watch(HealthCheckRequest {
            service: String::new(),
        })
        .await
        .map_err(|s| s.to_string())?
        .into_inner();
    let first = tokio::time::timeout(Duration::from_secs(5), w.next())
        .await
        .map_err(|_| "watch: no first status".to_string())?;
    ensure!(
        matches!(first, Some(Ok(ref m)) if m.status == ServingStatus::Serving as i32),
        "watch first {:?}",
        first
    );
    Ok(())
}

/// 多数の同時呼び出し（多重化）。
async fn next_reflection(
    s: &mut tonic::Streaming<tonic_reflection::pb::v1::ServerReflectionResponse>,
) -> Result<tonic_reflection::pb::v1::server_reflection_response::MessageResponse, String> {
    s.message()
        .await
        .map_err(|e| e.to_string())?
        .and_then(|r| r.message_response)
        .ok_or_else(|| "missing response".to_string())
}

/// サーバリフレクション（F-209 #3）: 一覧・シンボルからファイルの記述子・見つからないときのエラー。v1alpha も。
async fn reflection(ch: tonic::transport::Channel) -> Result<(), String> {
    use tonic_reflection::pb::v1::server_reflection_client::ServerReflectionClient;
    use tonic_reflection::pb::v1::server_reflection_request::MessageRequest;
    use tonic_reflection::pb::v1::server_reflection_response::MessageResponse;
    use tonic_reflection::pb::v1::ServerReflectionRequest;
    let req = |m: MessageRequest| ServerReflectionRequest {
        host: String::new(),
        message_request: Some(m),
    };
    let reqs = vec![
        req(MessageRequest::ListServices("*".into())),
        req(MessageRequest::FileContainingSymbol("demo.v1.Demo".into())),
        req(MessageRequest::FileContainingSymbol(
            "demo.v1.Demo.SayHello".into(),
        )),
        req(MessageRequest::FileByFilename("demo.proto".into())),
        req(MessageRequest::FileContainingSymbol("demo.v1.Nope".into())),
    ];
    let mut s = ServerReflectionClient::new(ch.clone())
        .server_reflection_info(tokio_stream::iter(reqs))
        .await
        .map_err(|e| e.to_string())?
        .into_inner();
    match next_reflection(&mut s).await? {
        MessageResponse::ListServicesResponse(l) => {
            let names: Vec<_> = l.service.iter().map(|s| s.name.as_str()).collect();
            for want in [
                "demo.v1.Demo",
                "grpc.health.v1.Health",
                "grpc.reflection.v1.ServerReflection",
            ] {
                ensure!(names.contains(&want), "{} not in {:?}", want, names);
            }
        }
        other => return Err(format!("list: {:?}", other)),
    }
    for _ in 0..3 {
        match next_reflection(&mut s).await? {
            MessageResponse::FileDescriptorResponse(f) => {
                let fd = prost_types::FileDescriptorProto::decode(&f.file_descriptor_proto[0][..])
                    .map_err(|e| e.to_string())?;
                ensure!(fd.name() == "demo.proto", "file {:?}", fd.name());
                ensure!(
                    fd.service
                        .iter()
                        .any(|s| s.name() == "Demo"
                            && s.method.iter().any(|m| m.name() == "SayHello")),
                    "service Demo missing"
                );
            }
            other => return Err(format!("file: {:?}", other)),
        }
    }
    match next_reflection(&mut s).await? {
        MessageResponse::ErrorResponse(e) => ensure!(e.error_code == 5, "code {}", e.error_code),
        other => return Err(format!("missing symbol: {:?}", other)),
    }
    // v1alpha（古いクライアント）
    use tonic_reflection::pb::v1alpha as alpha;
    let mut s = alpha::server_reflection_client::ServerReflectionClient::new(ch)
        .server_reflection_info(tokio_stream::iter(vec![alpha::ServerReflectionRequest {
            host: String::new(),
            message_request: Some(
                alpha::server_reflection_request::MessageRequest::ListServices("*".into()),
            ),
        }]))
        .await
        .map_err(|e| e.to_string())?
        .into_inner();
    match s
        .message()
        .await
        .map_err(|e| e.to_string())?
        .and_then(|r| r.message_response)
    {
        Some(alpha::server_reflection_response::MessageResponse::ListServicesResponse(l)) => {
            ensure!(
                l.service.iter().any(|s| s.name == "demo.v1.Demo"),
                "v1alpha list"
            );
        }
        other => return Err(format!("v1alpha: {:?}", other)),
    }
    Ok(())
}

async fn concurrent(c: &Client) -> Result<(), String> {
    let mut set = tokio::task::JoinSet::new();
    for i in 0..200 {
        let mut c = c.clone();
        set.spawn(async move {
            c.say_hello(HelloRequest {
                name: format!("n{}", i),
            })
            .await
            .map(|r| r.into_inner().message == format!("Hello n{}", i))
        });
    }
    let mut ok = 0;
    while let Some(r) = set.join_next().await {
        if matches!(r, Ok(Ok(true))) {
            ok += 1;
        }
    }
    ensure!(ok == 200, "{} / 200 succeeded", ok);
    Ok(())
}

// ===================== HTTP/3 =====================

#[derive(Debug)]
struct SkipVerify;

impl rustls::client::danger::ServerCertVerifier for SkipVerify {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
            .to_vec()
    }
}

fn frame<M: Message>(m: &M) -> Bytes {
    let mut b = BytesMut::with_capacity(5 + m.encoded_len());
    b.put_u8(0);
    b.put_u32(m.encoded_len() as u32);
    m.encode(&mut b).unwrap();
    b.freeze()
}

/// HTTP/3 で 1 呼び出し（要求メッセージ列 → 応答メッセージ列と grpc-status）。
async fn h3_call<Req: Message, Resp: Message + Default>(
    send: &mut h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>,
    authority: &str,
    path: &str,
    reqs: &[Req],
) -> Result<(Vec<Resp>, String), String> {
    h3_call_with(send, authority, path, reqs, None).await
}

async fn h3_call_with<Req: Message, Resp: Message + Default>(
    send: &mut h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>,
    authority: &str,
    path: &str,
    reqs: &[Req],
    grpc_timeout: Option<&str>,
) -> Result<(Vec<Resp>, String), String> {
    h3_call_full(send, authority, path, reqs, grpc_timeout, None)
        .await
        .map(|(m, s, _)| (m, s))
}

/// 応答の `grpc-encoding` も返す版（`accept_encoding` を `grpc-accept-encoding` で送る）。
async fn h3_call_full<Req: Message, Resp: Message + Default>(
    send: &mut h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>,
    authority: &str,
    path: &str,
    reqs: &[Req],
    grpc_timeout: Option<&str>,
    accept_encoding: Option<&str>,
) -> Result<(Vec<Resp>, String, Option<String>), String> {
    let mut request = http::Request::builder()
        .method("POST")
        .uri(format!("https://{}{}", authority, path))
        .header("content-type", "application/grpc")
        .header("te", "trailers");
    if let Some(t) = grpc_timeout {
        request = request.header("grpc-timeout", t);
    }
    if let Some(e) = accept_encoding {
        request = request.header("grpc-accept-encoding", e);
    }
    let request = request.body(()).unwrap();
    let mut stream = send
        .send_request(request)
        .await
        .map_err(|e| e.to_string())?;
    for r in reqs {
        stream
            .send_data(frame(r))
            .await
            .map_err(|e| e.to_string())?;
    }
    stream.finish().await.map_err(|e| e.to_string())?;
    let resp = stream.recv_response().await.map_err(|e| e.to_string())?;
    if let Some(s) = resp.headers().get("grpc-status") {
        // Trailers-Only
        return Ok((Vec::new(), s.to_str().unwrap_or("").to_string(), None));
    }
    let encoding = resp
        .headers()
        .get("grpc-encoding")
        .map(|v| v.to_str().unwrap_or("").to_string());
    let mut buf = BytesMut::new();
    while let Some(mut chunk) = stream.recv_data().await.map_err(|e| e.to_string())? {
        while chunk.has_remaining() {
            let c = chunk.chunk();
            buf.extend_from_slice(c);
            let n = c.len();
            chunk.advance(n);
        }
    }
    let trailers = stream.recv_trailers().await.map_err(|e| e.to_string())?;
    let status = trailers
        .as_ref()
        .and_then(|t| t.get("grpc-status"))
        .map(|v| v.to_str().unwrap_or("").to_string())
        .unwrap_or_else(|| "<none>".into());
    let mut out = Vec::new();
    while buf.len() >= 5 {
        let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
        if buf.len() < 5 + len {
            return Err("truncated response frame".into());
        }
        let payload = &buf[5..5 + len];
        let m = if buf[0] == 1 {
            // 圧縮フラグ: grpc-encoding が gzip のときだけ（それ以外は誤り）
            if encoding.as_deref() != Some("gzip") {
                return Err(format!(
                    "compressed frame with grpc-encoding {:?}",
                    encoding
                ));
            }
            let mut d = Vec::new();
            std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(payload), &mut d)
                .map_err(|e| e.to_string())?;
            Resp::decode(&d[..]).map_err(|e| e.to_string())?
        } else {
            Resp::decode(payload).map_err(|e| e.to_string())?
        };
        out.push(m);
        buf.advance(5 + len);
    }
    Ok((out, status, encoding))
}

/// HTTP/3 の双方向ストリーミングが全二重か（F-209 #5）: 1 通送って応答を受け取ってから次を送る。
/// 要求本文を受け終えてから呼ぶ半二重の実装では最初の応答が来ずにタイムアウトする。
async fn h3_ping_pong(
    send: &mut h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>,
    authority: &str,
) -> Result<(), String> {
    let request = http::Request::builder()
        .method("POST")
        .uri(format!("https://{}/demo.v1.Demo/Chat", authority))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(())
        .unwrap();
    let mut stream = send
        .send_request(request)
        .await
        .map_err(|e| e.to_string())?;
    let mut buf = BytesMut::new();
    let mut got_head = false;
    for word in ["ping", "pong", "veil"] {
        stream
            .send_data(frame(&ChatMessage { text: word.into() }))
            .await
            .map_err(|e| e.to_string())?;
        let reply = tokio::time::timeout(Duration::from_secs(5), async {
            if !got_head {
                stream.recv_response().await.map_err(|e| e.to_string())?;
                got_head = true;
            }
            loop {
                if buf.len() >= 5 {
                    let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
                    if buf.len() >= 5 + len {
                        let m = ChatMessage::decode(&buf[5..5 + len]).map_err(|e| e.to_string())?;
                        buf.advance(5 + len);
                        return Ok::<_, String>(m);
                    }
                }
                match stream.recv_data().await.map_err(|e| e.to_string())? {
                    Some(mut chunk) => {
                        while chunk.has_remaining() {
                            let c = chunk.chunk();
                            buf.extend_from_slice(c);
                            let n = c.len();
                            chunk.advance(n);
                        }
                    }
                    None => return Err("stream ended".into()),
                }
            }
        })
        .await
        .map_err(|_| {
            format!(
                "no reply to {:?} before the request ended (half duplex)",
                word
            )
        })??;
        ensure!(reply.text == word.to_uppercase(), "got {:?}", reply);
    }
    stream.finish().await.map_err(|e| e.to_string())?;
    while stream
        .recv_data()
        .await
        .map_err(|e| e.to_string())?
        .is_some()
    {}
    let trailers = stream.recv_trailers().await.map_err(|e| e.to_string())?;
    let status = trailers
        .as_ref()
        .and_then(|t| t.get("grpc-status"))
        .map(|v| v.to_str().unwrap_or("").to_string());
    ensure!(status.as_deref() == Some("0"), "grpc-status {:?}", status);
    Ok(())
}

/// QUIC + h3 の接続（証明書は検証しない）。
async fn h3_connect(
    addr: std::net::SocketAddr,
) -> Result<h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>, String> {
    let _ =
        rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipVerify))
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let qc = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(qc)));
    let conn = endpoint
        .connect(addr, "localhost")
        .unwrap()
        .await
        .map_err(|e| e.to_string())?;
    let (mut driver, send) = h3::client::new(h3_quinn::Connection::new(conn))
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        let _ = driver.wait_idle().await;
    });
    Ok(send)
}

// F-266/F-270: curl に HTTP/3 が無い環境向けの 1 要求（`--h3-fetch host:port METHOD PATH [-H "k: v"]... [--data-file F]`）。
// 標準出力に「状態行・ヘッダ（小文字）・空行・本文（生のバイト）」を書く。
async fn h3_fetch(args: &[String]) -> Result<(), String> {
    let addr: std::net::SocketAddr = args[0].parse().map_err(|e| format!("{:?}", e))?;
    let (method, path) = (&args[1], &args[2]);
    let mut request = http::Request::builder()
        .method(method.as_str())
        .uri(format!("https://{}{}", addr, path));
    let mut file = None;
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "-H" => {
                let (k, v) = args[i + 1].split_once(':').ok_or("-H needs k: v")?;
                request = request.header(k.trim(), v.trim());
            }
            "--data-file" => file = Some(args[i + 1].clone()),
            other => return Err(format!("unknown option {}", other)),
        }
        i += 2;
    }
    let mut send = h3_connect(addr).await?;
    let mut stream = send
        .send_request(request.body(()).unwrap())
        .await
        .map_err(|e| e.to_string())?;
    if let Some(f) = file {
        use tokio::io::AsyncReadExt;
        let mut f = tokio::fs::File::open(f).await.map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; 65536];
        loop {
            let n = f.read(&mut buf).await.map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            stream
                .send_data(Bytes::copy_from_slice(&buf[..n]))
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    stream.finish().await.map_err(|e| e.to_string())?;
    let resp = stream.recv_response().await.map_err(|e| e.to_string())?;
    let mut out = format!("HTTP/3 {}\r\n", resp.status().as_u16()).into_bytes();
    for (k, v) in resp.headers() {
        out.extend_from_slice(k.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(v.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    while let Some(mut chunk) = stream.recv_data().await.map_err(|e| e.to_string())? {
        while chunk.has_remaining() {
            let c = chunk.chunk();
            out.extend_from_slice(c);
            let n = c.len();
            chunk.advance(n);
        }
    }
    use std::io::Write;
    std::io::stdout().write_all(&out).map_err(|e| e.to_string())
}

async fn h3_checks(addr: std::net::SocketAddr) {
    let _ =
        rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipVerify))
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let qc = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(qc)));
    let conn = match endpoint.connect(addr, "localhost").unwrap().await {
        Ok(c) => c,
        Err(e) => return report("h3 connect", Err(e.to_string())),
    };
    let (mut driver, mut send) = h3::client::new(h3_quinn::Connection::new(conn))
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = driver.wait_idle().await;
    });
    let authority = addr.to_string();

    let r = h3_call::<_, HelloReply>(
        &mut send,
        &authority,
        "/demo.v1.Demo/SayHello",
        &[HelloRequest { name: "h3".into() }],
    )
    .await;
    report(
        "h3 unary",
        r.and_then(|(m, s)| {
            ensure!(s == "0", "grpc-status {}", s);
            ensure!(m.len() == 1 && m[0].message == "Hello h3", "got {:?}", m);
            Ok(())
        }),
    );

    let r = h3_call::<_, CountReply>(
        &mut send,
        &authority,
        "/demo.v1.Demo/CountUp",
        &[CountRequest { count: 500 }],
    )
    .await;
    report(
        "h3 server streaming",
        r.and_then(|(m, s)| {
            ensure!(s == "0", "grpc-status {}", s);
            ensure!(
                m.len() == 500 && m[499].n == 500,
                "got {} messages",
                m.len()
            );
            Ok(())
        }),
    );

    let nums: Vec<Number> = (1..=10).map(|v| Number { value: v }).collect();
    let r = h3_call::<_, SumReply>(&mut send, &authority, "/demo.v1.Demo/Sum", &nums).await;
    report(
        "h3 client streaming",
        r.and_then(|(m, s)| {
            ensure!(s == "0", "grpc-status {}", s);
            ensure!(m.len() == 1 && m[0].total == 55, "got {:?}", m);
            Ok(())
        }),
    );

    let msgs = vec![
        ChatMessage { text: "a".into() },
        ChatMessage { text: "b".into() },
    ];
    let r = h3_call::<_, ChatMessage>(&mut send, &authority, "/demo.v1.Demo/Chat", &msgs).await;
    report(
        "h3 bidi",
        r.and_then(|(m, s)| {
            ensure!(s == "0", "grpc-status {}", s);
            ensure!(m.len() == 2 && m[1].text == "B", "got {:?}", m);
            Ok(())
        }),
    );

    // F-209 #2: 1KB 以上の応答は gzip（サンプルは grpc_response_compression = "gzip"）。受けると言わなければ非圧縮
    let big = "z".repeat(4000);
    let r = h3_call_full::<_, HelloReply>(
        &mut send,
        &authority,
        "/demo.v1.Demo/SayHello",
        &[HelloRequest { name: big.clone() }],
        None,
        Some("identity,gzip"),
    )
    .await;
    report(
        "h3 gzip response (F-209)",
        r.and_then(|(m, s, enc)| {
            ensure!(s == "0", "grpc-status {}", s);
            ensure!(enc.as_deref() == Some("gzip"), "grpc-encoding {:?}", enc);
            ensure!(
                m.len() == 1 && m[0].message.len() == 4006,
                "got {:?}",
                m.len()
            );
            Ok(())
        }),
    );
    let r = h3_call_full::<_, HelloReply>(
        &mut send,
        &authority,
        "/demo.v1.Demo/SayHello",
        &[HelloRequest { name: big }],
        None,
        None,
    )
    .await;
    report(
        "h3 no compression without grpc-accept-encoding (F-209)",
        r.and_then(|(m, s, enc)| {
            ensure!(s == "0", "grpc-status {}", s);
            ensure!(enc.is_none(), "grpc-encoding {:?}", enc);
            ensure!(
                m.len() == 1 && m[0].message.len() == 4006,
                "got {:?}",
                m.len()
            );
            Ok(())
        }),
    );

    report(
        "h3 bidi ping_pong (full duplex, F-209)",
        h3_ping_pong(&mut send, &authority).await,
    );

    let t = std::time::Instant::now();
    let r = h3_call_with::<_, Empty>(
        &mut send,
        &authority,
        "/demo.v1.Demo/Sleep",
        &[SleepRequest { millis: 3000 }],
        Some("200m"),
    )
    .await;
    report(
        "h3 server enforces grpc-timeout",
        r.and_then(|(_, s)| {
            ensure!(s == "4", "grpc-status {}", s);
            ensure!(
                t.elapsed() < Duration::from_secs(2),
                "took {:?}",
                t.elapsed()
            );
            Ok(())
        }),
    );

    let r = h3_call::<_, Empty>(&mut send, &authority, "/nope.v1.Nope/Call", &[Empty {}]).await;
    report(
        "h3 unknown service is UNIMPLEMENTED (F-209)",
        r.and_then(|(_, s)| {
            ensure!(s == "12", "grpc-status {}", s);
            Ok(())
        }),
    );

    let r = h3_call::<_, Empty>(
        &mut send,
        &authority,
        "/demo.v1.Demo/Fail",
        &[FailRequest {
            code: 5,
            message: "missing".into(),
        }],
    )
    .await;
    report(
        "h3 error status",
        r.and_then(|(_, s)| {
            ensure!(s == "5", "grpc-status {}", s);
            Ok(())
        }),
    );
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--h3-fetch") {
        if let Err(e) = h3_fetch(&args[i + 1..]).await {
            eprintln!("h3 fetch: {}", e);
            std::process::exit(1);
        }
        return;
    }
    let arg = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    if let Some(h2c) = arg("--h2c") {
        let ch = tonic::transport::Endpoint::from_shared(format!("http://{}", h2c))
            .unwrap()
            .connect()
            .await
            .expect("connect");
        let mut c = DemoClient::new(ch.clone()).accept_compressed(CompressionEncoding::Gzip);
        report("h2 unary + metadata + trailers", unary(&mut c).await);
        report("h2 empty_unary", empty_unary(&mut c).await);
        report("h2 large_unary (3MiB)", large_unary(&mut c).await);
        report("h2 message too large", too_large(&mut c).await);
        report("h2 gzip request", gzip(&mut c).await);
        report("h2 gzip bomb (B-127)", gzip_bomb(&mut c).await);
        report("h2 server_streaming", server_streaming(&mut c).await);
        report(
            "h2 server_streaming error",
            server_streaming_error(&mut c).await,
        );
        report("h2 client_streaming", client_streaming(&mut c).await);
        report("h2 ping_pong (full duplex)", ping_pong(&mut c).await);
        report("h2 timeout_on_sleeping_server", deadline(&mut c).await);
        report(
            "h2 status_code_and_message",
            status_code_and_message(&mut c).await,
        );
        report(
            "h2 cancel_after_first_response",
            cancel_after_first_response(&mut c).await,
        );
        report("h2 unimplemented method", unimplemented(ch.clone()).await);
        report("h2 health check/watch", health(ch.clone()).await);
        if !args.iter().any(|a| a == "--no-reflection") {
            report(
                "h2 server reflection v1/v1alpha (F-209)",
                reflection(ch.clone()).await,
            );
        }
        report("h2 200 concurrent calls", concurrent(&c).await);
    }
    if let Some(h3) = arg("--h3") {
        h3_checks(h3.parse().expect("--h3 host:port")).await;
    }
    if unsafe { FAILED } {
        std::process::exit(1);
    }
}
