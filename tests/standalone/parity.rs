//! プロトコル間の機能の差（B-266〜B-277・F-285〜F-287）の回帰試験に使うクライアント。
//!
//! 同じ要求を HTTP/1.1（平文）・h2c・HTTP/2（TLS）・HTTP/3 で送り、応答を同じ形（[`HttpResponse`]）で返す。
//! HTTP/2・HTTP/3 のヘッダの名前は小文字（`HttpResponse::header` は大文字小文字を区別しない）。

use super::*;
use bytes::{Buf, Bytes};
use std::sync::Arc;

/// クライアントのプロトコル
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proto {
    H1,
    H2c,
    H2Tls,
    H3,
}

/// 試す全プロトコル（HTTP/3 は `http3` feature のときだけ）
pub fn all_protos() -> Vec<Proto> {
    #[allow(unused_mut)]
    let mut v = vec![Proto::H1, Proto::H2c, Proto::H2Tls];
    #[cfg(feature = "http3")]
    v.push(Proto::H3);
    v
}

/// 平文（HTTP/1.1・h2c）と TLS（HTTP/2・HTTP/3）のポート
#[derive(Clone, Copy, Debug)]
pub struct Ports {
    pub clear: u16,
    pub tls: u16,
}

impl Ports {
    /// 空いているポートを 2 つ取る
    pub fn alloc() -> Self {
        Self {
            clear: free_port(),
            tls: free_port(),
        }
    }
}

/// HTTP/1.1・h2c・HTTP/2（TLS）・HTTP/3 を受ける `[server]`・`[tls]`・`[http3]` の設定
pub fn listeners_toml(ports: Ports, cert: &Path, key: &Path) -> String {
    format!(
        r#"
[server]
listen = "127.0.0.1:{tls}"
listen_cleartext = "127.0.0.1:{clear}"
enable_secure_redirect = false
allow_h2c = true
http2_enabled = true
threads = 1
[logging]
level = "{level}"
[tls]
cert_path = "{cert}"
key_path = "{key}"
ktls_enabled = false
[http3]
enabled = true
"#,
        level = std::env::var("PARITY_LOG").unwrap_or_else(|_| "warn".to_string()),
        tls = ports.tls,
        clear = ports.clear,
        cert = tp(cert),
        key = tp(key),
    )
}

/// 設定を書いて起動し、平文のポートが開くまで待つ
pub fn spawn_config(dir: &Path, ports: Ports, config: &str) -> VeilProcess {
    let cfg_path = dir.join("config.toml");
    std::fs::write(&cfg_path, config).unwrap();
    let veil = VeilProcess::spawn(&cfg_path, &[], &[]);
    assert!(
        wait_for_port(ports.clear, Duration::from_secs(30))
            && wait_for_port(ports.tls, Duration::from_secs(10)),
        "{}",
        veil.log()
    );
    veil
}

/// `proto` で 1 要求を送って応答を返す（HEAD と 304 の `body` は、ヘッダの後ろに実際に届いたもの）
pub fn fetch(
    proto: Proto,
    ports: Ports,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> HttpResponse {
    let r = match proto {
        Proto::H1 => h1_fetch(ports.clear, method, path, headers, body).map_err(|e| e.to_string()),
        Proto::H2c | Proto::H2Tls => block_on_timeout(h2_fetch(
            proto == Proto::H2Tls,
            if proto == Proto::H2Tls {
                ports.tls
            } else {
                ports.clear
            },
            method.to_string(),
            path.to_string(),
            owned(headers),
            Bytes::copy_from_slice(body),
        )),
        Proto::H3 => h3_fetch(ports.tls, method, path, headers, body),
    };
    r.unwrap_or_else(|e| panic!("{proto:?} {method} {path}: {e}"))
}

/// `proto` で GET を送り、本文に `needle` が届いた時点で（応答の終わりを待たずに）返す。
/// HTTP/1.1 の `body` はヘッダの後ろの生のバイト（chunked なら枠のまま）。`wait` 以内に届かなければ `Err`
pub fn fetch_until(
    proto: Proto,
    ports: Ports,
    path: &str,
    headers: &[(&str, &str)],
    needle: &[u8],
    wait: Duration,
) -> Result<HttpResponse, String> {
    let has = |b: &[u8]| b.windows(needle.len()).any(|w| w == needle);
    match proto {
        Proto::H1 => {
            let mut s = std::net::TcpStream::connect(("127.0.0.1", ports.clear))
                .map_err(|e| e.to_string())?;
            let mut req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n");
            for (n, v) in headers {
                req.push_str(&format!("{n}: {v}\r\n"));
            }
            req.push_str("\r\n");
            s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
            let deadline = Instant::now() + wait;
            let mut buf = Vec::new();
            let mut tmp = [0u8; 16384];
            loop {
                if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    if has(&buf[end + 4..]) {
                        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                        let mut lines = head.split("\r\n");
                        let status = lines
                            .next()
                            .and_then(|l| l.split(' ').nth(1))
                            .and_then(|c| c.parse().ok())
                            .ok_or_else(|| format!("bad status line: {head:?}"))?;
                        let headers = lines
                            .filter_map(|l| l.split_once(':'))
                            .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
                            .collect();
                        return Ok(HttpResponse {
                            status,
                            headers,
                            body: buf[end + 4..].to_vec(),
                        });
                    }
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(format!(
                        "timed out; got {:?}",
                        String::from_utf8_lossy(&buf)
                    ));
                }
                let _ = s.set_read_timeout(Some(left));
                match s.read(&mut tmp) {
                    Ok(n) if n > 0 => buf.extend_from_slice(&tmp[..n]),
                    Ok(_) => {
                        return Err(format!("closed; got {:?}", String::from_utf8_lossy(&buf)))
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
        }
        Proto::H2c | Proto::H2Tls => {
            let tls = proto == Proto::H2Tls;
            let port = if tls { ports.tls } else { ports.clear };
            let req = h2_request(tls, port, "GET", path, &owned(headers));
            let needle = needle.to_vec();
            block_on_timeout(async move {
                tokio::time::timeout(wait, async move {
                    let mut client = h2_connect(tls, port).await?;
                    let (resp, _send) =
                        client.send_request(req, true).map_err(|e| e.to_string())?;
                    let resp = resp.await.map_err(|e| e.to_string())?;
                    let mut r = HttpResponse {
                        status: resp.status().as_u16(),
                        headers: header_pairs(resp.headers()),
                        body: Vec::new(),
                    };
                    let mut body = resp.into_body();
                    while let Some(chunk) = body.data().await {
                        let chunk = chunk.map_err(|e| e.to_string())?;
                        let _ = body.flow_control().release_capacity(chunk.len());
                        r.body.extend_from_slice(&chunk);
                        if r.body.windows(needle.len()).any(|w| w == needle.as_slice()) {
                            return Ok(r);
                        }
                    }
                    Err("stream ended before the needle".to_string())
                })
                .await
                .map_err(|_| "timed out".to_string())?
            })
        }
        #[cfg(feature = "http3")]
        Proto::H3 => {
            let req = h3_request(ports.tls, "GET", path, headers);
            let port = ports.tls;
            let needle = needle.to_vec();
            block_on_timeout(async move {
                tokio::time::timeout(wait, async move {
                    let (_endpoint, mut send) = h3_connect(port).await?;
                    let mut stream = send.send_request(req).await.map_err(|e| e.to_string())?;
                    stream.finish().await.map_err(|e| e.to_string())?;
                    let resp = stream.recv_response().await.map_err(|e| e.to_string())?;
                    let mut r = HttpResponse {
                        status: resp.status().as_u16(),
                        headers: header_pairs(resp.headers()),
                        body: Vec::new(),
                    };
                    while let Some(mut chunk) =
                        stream.recv_data().await.map_err(|e| e.to_string())?
                    {
                        let n = chunk.remaining();
                        r.body.extend_from_slice(&chunk.copy_to_bytes(n));
                        if r.body.windows(needle.len()).any(|w| w == needle.as_slice()) {
                            return Ok(r);
                        }
                    }
                    Err("stream ended before the needle".to_string())
                })
                .await
                .map_err(|_| "timed out".to_string())?
            })
        }
        #[cfg(not(feature = "http3"))]
        Proto::H3 => Err("http3 feature is disabled".to_string()),
    }
}

/// [`fetch`] と同じだが、送受信の失敗（リセット・切断）を `Err` で返す（HTTP/2・HTTP/3 のみ）
pub fn try_fetch(proto: Proto, ports: Ports, path: &str) -> Result<HttpResponse, String> {
    match proto {
        Proto::H1 => Err("try_fetch does not support HTTP/1.1".to_string()),
        Proto::H2c | Proto::H2Tls => block_on_timeout(h2_fetch(
            proto == Proto::H2Tls,
            if proto == Proto::H2Tls {
                ports.tls
            } else {
                ports.clear
            },
            "GET".to_string(),
            path.to_string(),
            Vec::new(),
            Bytes::new(),
        )),
        Proto::H3 => h3_fetch(ports.tls, "GET", path, &[], b""),
    }
}

fn header_pairs(h: &http::HeaderMap) -> Vec<(String, String)> {
    h.iter()
        .map(|(n, v)| {
            (
                n.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect()
}

fn owned(headers: &[(&str, &str)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect()
}

fn block_on_timeout<T>(
    f: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        tokio::time::timeout(Duration::from_secs(15), f)
            .await
            .map_err(|_| "timed out".to_string())?
    })
}

fn h1_fetch(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<HttpResponse> {
    let mut s = connect_local(port, Duration::from_secs(2))?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    if !body.is_empty() {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes())?;
    s.write_all(body)?;
    let mut raw = Vec::new();
    // 閉じ方（RST）によらず読めた分で解析する
    let _ = s.read_to_end(&mut raw);
    let mut r = parse_http1_response(&raw)?;
    if method == "HEAD" || r.status == 304 {
        let end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        r.body = raw[end..].to_vec();
    }
    Ok(r)
}

/// rustls のクライアント設定（証明書は検証しない）
pub fn tls_client_config(alpn: &[u8]) -> rustls::ClientConfig {
    let _ = veil::tls_provider::provider::default_provider().install_default();
    let mut c = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    c.alpn_protocols = vec![alpn.to_vec()];
    c
}

/// HTTP/2 の接続を張る（TLS なら ALPN h2）。接続の駆動は spawn する
pub async fn h2_connect(tls: bool, port: u16) -> Result<h2::client::SendRequest<Bytes>, String> {
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|e| e.to_string())?;
    if tls {
        let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_client_config(b"h2")));
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let io = connector
            .connect(name, tcp)
            .await
            .map_err(|e| e.to_string())?;
        let (client, conn) = h2::client::handshake(io).await.map_err(|e| e.to_string())?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        Ok(client)
    } else {
        let (client, conn) = h2::client::handshake(tcp)
            .await
            .map_err(|e| e.to_string())?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        Ok(client)
    }
}

/// HTTP/2 の要求（ヘッダだけ）を作る
pub fn h2_request(
    tls: bool,
    port: u16,
    method: &str,
    path: &str,
    headers: &[(String, String)],
) -> http::Request<()> {
    let scheme = if tls { "https" } else { "http" };
    let mut b = http::Request::builder()
        .method(method)
        .uri(format!("{scheme}://localhost:{port}{path}"));
    for (n, v) in headers {
        b = b.header(n.as_str(), v.as_str());
    }
    b.body(()).unwrap()
}

async fn h2_fetch(
    tls: bool,
    port: u16,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Bytes,
) -> Result<HttpResponse, String> {
    let mut client = h2_connect(tls, port).await?;
    let req = h2_request(tls, port, &method, &path, &headers);
    let (resp, mut send) = client
        .send_request(req, body.is_empty())
        .map_err(|e| e.to_string())?;
    if !body.is_empty() {
        send.send_data(body, true).map_err(|e| e.to_string())?;
    }
    let resp = resp.await.map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .map(|(n, v)| {
            (
                n.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let mut body = resp.into_body();
    let mut out = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        let _ = body.flow_control().release_capacity(chunk.len());
        out.extend_from_slice(&chunk);
    }
    Ok(HttpResponse {
        status,
        headers,
        body: out,
    })
}

/// HTTP/3 の接続を張る（quinn + h3）。接続の駆動は spawn する。`Endpoint` は接続より長く持つこと
#[cfg(feature = "http3")]
pub async fn h3_connect(
    port: u16,
) -> Result<
    (
        quinn::Endpoint,
        h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>,
    ),
    String,
> {
    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(tls_client_config(b"h3"))
        .map_err(|e| e.to_string())?;
    let mut endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).map_err(|e| e.to_string())?;
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic)));
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let conn = endpoint
        .connect(addr, "localhost")
        .map_err(|e| e.to_string())?
        .await
        .map_err(|e| e.to_string())?;
    let (mut driver, send) = h3::client::new(h3_quinn::Connection::new(conn))
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        let _ = driver.wait_idle().await;
    });
    Ok((endpoint, send))
}

/// HTTP/3 の要求（ヘッダだけ）を作る
pub fn h3_request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> http::Request<()> {
    let mut b = http::Request::builder()
        .method(method)
        .uri(format!("https://localhost:{port}{path}"));
    for (n, v) in headers {
        b = b.header(*n, *v);
    }
    b.body(()).unwrap()
}

#[cfg(feature = "http3")]
fn h3_fetch(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<HttpResponse, String> {
    let req = h3_request(port, method, path, headers);
    let body = Bytes::copy_from_slice(body);
    block_on_timeout(async move {
        let (_endpoint, mut send) = h3_connect(port).await?;
        let mut stream = send.send_request(req).await.map_err(|e| e.to_string())?;
        if !body.is_empty() {
            stream.send_data(body).await.map_err(|e| e.to_string())?;
        }
        stream.finish().await.map_err(|e| e.to_string())?;
        let resp = stream.recv_response().await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(n, v)| {
                (
                    n.as_str().to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let mut out = Vec::new();
        while let Some(mut chunk) = stream.recv_data().await.map_err(|e| e.to_string())? {
            let n = chunk.remaining();
            out.extend_from_slice(&chunk.copy_to_bytes(n));
        }
        Ok(HttpResponse {
            status,
            headers,
            body: out,
        })
    })
}

#[cfg(not(feature = "http3"))]
fn h3_fetch(
    _port: u16,
    _method: &str,
    _path: &str,
    _headers: &[(&str, &str)],
    _body: &[u8],
) -> Result<HttpResponse, String> {
    Err("http3 feature is disabled".to_string())
}

/// 上流（HTTP/1.1）が受けた要求 1 つ。`complete` は本文の終わり（Content-Length 分・chunked の終端）まで届いたか
#[derive(Clone, Debug)]
pub struct UpstreamSeen {
    pub head: String,
    pub body: Vec<u8>,
    pub complete: bool,
}

/// 受けた要求を記録する HTTP/1.1 の上流。`respond` が (ステータス, 追加のヘッダ行, 本文) を返す。
/// 本文の終わりまで届いた要求にだけ応答し、途中で切れたものは `complete = false` で記録する
pub fn spawn_recording_upstream(
    respond: impl Fn(&str) -> (u16, String, Vec<u8>) + Send + Sync + 'static,
) -> (u16, Arc<std::sync::Mutex<Vec<UpstreamSeen>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let respond = Arc::new(respond);
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let seen = seen2.clone();
            let respond = respond.clone();
            std::thread::spawn(move || {
                let _ = s.set_read_timeout(Some(Duration::from_secs(20)));
                let mut buf = Vec::new();
                let mut tmp = [0u8; 16384];
                loop {
                    // ヘッダ
                    let end = loop {
                        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break end + 4;
                        }
                        match s.read(&mut tmp) {
                            Ok(n) if n > 0 => buf.extend_from_slice(&tmp[..n]),
                            _ => return,
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                    buf.drain(..end);
                    let lower = head.to_ascii_lowercase();
                    let chunked = lower.contains("transfer-encoding: chunked");
                    let cl: usize = lower
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    // 本文
                    let mut body = Vec::new();
                    let complete = loop {
                        if chunked {
                            if let Some(decoded) = decode_chunked_complete(&buf) {
                                body = decoded.0;
                                buf.drain(..decoded.1);
                                break true;
                            }
                        } else if buf.len() >= cl {
                            body = buf.drain(..cl).collect();
                            break true;
                        }
                        match s.read(&mut tmp) {
                            Ok(n) if n > 0 => buf.extend_from_slice(&tmp[..n]),
                            _ => break false,
                        }
                    };
                    let done = UpstreamSeen {
                        head: head.clone(),
                        body,
                        complete,
                    };
                    seen.lock().unwrap().push(done);
                    if !complete {
                        return;
                    }
                    let (status, extra, rbody) = respond(&head);
                    let mut resp = format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n{extra}\r\n",
                        rbody.len()
                    )
                    .into_bytes();
                    resp.extend_from_slice(&rbody);
                    if s.write_all(&resp).is_err() {
                        return;
                    }
                }
            });
        }
    });
    (port, seen)
}

/// chunked の本文が終端まで揃っていれば (本文, 消費したバイト数)
fn decode_chunked_complete(data: &[u8]) -> Option<(Vec<u8>, usize)> {
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        let line_end = data[pos..].windows(2).position(|w| w == b"\r\n")? + pos;
        let size_str = std::str::from_utf8(&data[pos..line_end]).ok()?;
        let size = usize::from_str_radix(size_str.split(';').next()?.trim(), 16).ok()?;
        pos = line_end + 2;
        if size == 0 {
            // トレーラーは無い前提（空行で終わる）
            if data.get(pos..pos + 2)? == b"\r\n" {
                return Some((out, pos + 2));
            }
            return None;
        }
        out.extend_from_slice(data.get(pos..pos + size)?);
        pos += size;
        if data.get(pos..pos + 2)? != b"\r\n" {
            return None;
        }
        pos += 2;
    }
}

/// h2c の上流が受けた要求 1 つ（`complete` は END_STREAM まで届いたか）
#[derive(Clone, Debug)]
pub struct H2UpstreamSeen {
    pub path: String,
    pub body_len: usize,
    pub complete: bool,
}

/// 受けた要求を記録する h2c の上流（本文の終わりまで届いたものに 200 を返す）
pub fn spawn_recording_h2c_upstream() -> (u16, Arc<std::sync::Mutex<Vec<H2UpstreamSeen>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.set_nonblocking(true).unwrap();
    let port = l.local_addr().unwrap().port();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let l = tokio::net::TcpListener::from_std(l).unwrap();
            loop {
                let Ok((tcp, _)) = l.accept().await else {
                    continue;
                };
                let seen = seen2.clone();
                tokio::spawn(async move {
                    let Ok(mut conn) = h2::server::handshake(tcp).await else {
                        return;
                    };
                    while let Some(Ok((req, mut respond))) = conn.accept().await {
                        let seen = seen.clone();
                        tokio::spawn(async move {
                            let path = req.uri().path().to_string();
                            let mut body = req.into_body();
                            let mut len = 0;
                            let complete = loop {
                                match body.data().await {
                                    Some(Ok(c)) => {
                                        len += c.len();
                                        let _ = body.flow_control().release_capacity(c.len());
                                    }
                                    Some(Err(_)) => break false,
                                    None => break true,
                                }
                            };
                            seen.lock().unwrap().push(H2UpstreamSeen {
                                path,
                                body_len: len,
                                complete,
                            });
                            if complete {
                                let resp = http::Response::builder().status(200).body(()).unwrap();
                                let _ = respond.send_response(resp, true);
                            }
                        });
                    }
                });
            }
        });
    });
    (port, seen)
}
