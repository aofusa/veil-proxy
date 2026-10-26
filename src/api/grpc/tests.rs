//! Unit tests of the gRPC server layer (the handler functions are called directly, without starting a worker).

use super::health::{HealthCheckRequest, HealthCheckResponse, HealthCodec};
use super::*;
use crate::api::request::Request as ApiRequest;

fn frame<T>(m: &T) -> Bytes
where
    HealthCodec: Codec<T>,
{
    encode_message(&HealthCodec, m).unwrap()
}

fn grpc_request(version: HttpVersion, frames: Vec<Bytes>, extra: &[(&str, &str)]) -> ApiRequest {
    let mut headers = vec![(
        Bytes::from_static(b"content-type"),
        Bytes::from_static(b"application/grpc"),
    )];
    for (n, v) in extra {
        headers.push((
            Bytes::copy_from_slice(n.as_bytes()),
            Bytes::copy_from_slice(v.as_bytes()),
        ));
    }
    let mut req = ApiRequest::new("POST", "/pkg.Svc/M", headers, Bytes::new());
    req.version = version;
    let (mut tx, slot) = crate::api::stream::request_body_channel(frames.len().max(1));
    futures::executor::block_on(async {
        for f in frames {
            tx.send(f).await.unwrap();
        }
    });
    drop(tx);
    req.body_stream = Some(slot);
    req
}

/// Splits a response into (headers, concatenated DATA, trailers).
fn collect(mut resp: crate::api::response::Response) -> (Metadata, Vec<u8>, Metadata) {
    let headers = resp.headers().to_vec();
    let mut data = Vec::new();
    let mut trailers = Vec::new();
    if let Some(mut s) = resp.take_stream() {
        futures::executor::block_on(async {
            while let Some(c) = s.next().await {
                match c {
                    BodyChunk::Data(b) => data.extend_from_slice(&b),
                    BodyChunk::Trailers(t) => trailers = t,
                    BodyChunk::Abort(r) => panic!("abort: {}", r),
                }
            }
        });
    }
    (headers, data, trailers)
}

fn field<'a>(m: &'a Metadata, name: &str) -> Option<&'a [u8]> {
    m.iter()
        .find(|(n, _)| n == name.as_bytes())
        .map(|(_, v)| v.as_ref())
}

fn decode_all<T>(data: &[u8]) -> Vec<T>
where
    HealthCodec: Codec<T>,
{
    let mut d = GrpcFrameDecoder::new();
    d.push(data);
    let mut out = Vec::new();
    while let Some(f) = d.decode_next().unwrap() {
        out.push(HealthCodec.decode(Bytes::from(f.data)).unwrap());
    }
    out
}

fn handler_of(app: App) -> HandlerFn {
    app.routes.last().expect("registered").func.clone()
}

fn echo_app() -> App {
    App::new().grpc_unary(
        "/pkg.Svc/M",
        HealthCodec,
        |req: Request<HealthCheckRequest>| async move {
            if req.get_ref().service == "fail" {
                return Err(
                    Status::invalid_argument("bad 名前 100%").with_metadata("X-Detail", "d")
                );
            }
            let who = req
                .metadata_value("x-who")
                .map(|v| String::from_utf8_lossy(v).into_owned())
                .unwrap_or_default();
            Ok(Response::new(HealthCheckRequest {
                service: format!("{}:{}", who, req.into_inner().service),
            })
            .with_metadata("x-head", "h")
            .with_trailer("x-tail", "t"))
        },
    )
}

#[test]
fn percent_encoding_roundtrip() {
    let s = "bad 名前 100%\n";
    let enc = percent_encode(s);
    assert!(enc.iter().all(|b| (0x20..=0x7e).contains(b)));
    assert_eq!(percent_decode(&enc), s);
}

#[test]
fn unary_ok_with_metadata_and_trailers() {
    let f = handler_of(echo_app());
    let req = grpc_request(
        HttpVersion::Http2,
        vec![frame(&HealthCheckRequest {
            service: "x".into(),
        })],
        &[("x-who", "me")],
    );
    let (h, data, t) = collect(futures::executor::block_on(f(req)));
    assert_eq!(field(&h, "content-type"), Some(&b"application/grpc"[..]));
    assert_eq!(field(&h, "x-head"), Some(&b"h"[..]));
    let msgs: Vec<HealthCheckRequest> = decode_all(&data);
    assert_eq!(msgs[0].service, "me:x");
    assert_eq!(field(&t, "grpc-status"), Some(&b"0"[..]));
    assert_eq!(field(&t, "x-tail"), Some(&b"t"[..]));
}

#[test]
fn unary_error_is_trailers_only() {
    let f = handler_of(echo_app());
    let req = grpc_request(
        HttpVersion::Http2,
        vec![frame(&HealthCheckRequest {
            service: "fail".into(),
        })],
        &[],
    );
    let resp = futures::executor::block_on(f(req));
    assert!(!resp.is_streaming());
    let h = resp.headers().to_vec();
    assert_eq!(field(&h, "grpc-status"), Some(&b"3"[..]));
    assert_eq!(
        percent_decode(field(&h, "grpc-message").unwrap()),
        "bad 名前 100%"
    );
    assert_eq!(field(&h, "x-detail"), Some(&b"d"[..]));
}

#[test]
fn split_frames_and_missing_or_extra_messages() {
    let f = handler_of(echo_app());
    // 1 メッセージを 3 断片に分けて届ける
    let whole = frame(&HealthCheckRequest {
        service: "split".into(),
    });
    let parts = vec![whole.slice(..2), whole.slice(2..7), whole.slice(7..)];
    let (_, data, t) = collect(futures::executor::block_on(f(grpc_request(
        HttpVersion::Http2,
        parts,
        &[],
    ))));
    assert_eq!(decode_all::<HealthCheckRequest>(&data)[0].service, ":split");
    assert_eq!(field(&t, "grpc-status"), Some(&b"0"[..]));
    // 0 個・2 個は INTERNAL
    for frames in [vec![], vec![whole.clone(), whole.clone()]] {
        let resp = futures::executor::block_on(f(grpc_request(HttpVersion::Http2, frames, &[])));
        assert_eq!(
            field(&resp.headers().to_vec(), "grpc-status"),
            Some(&b"13"[..])
        );
    }
    // 途中で切れた本文は INTERNAL
    let resp = futures::executor::block_on(f(grpc_request(
        HttpVersion::Http2,
        vec![whole.slice(..whole.len() - 1)],
        &[],
    )));
    assert_eq!(
        field(&resp.headers().to_vec(), "grpc-status"),
        Some(&b"13"[..])
    );
}

#[test]
fn rejects_wrong_content_type_http1_and_unknown_encoding() {
    let f = handler_of(echo_app());
    let mut req = ApiRequest::new("POST", "/pkg.Svc/M", vec![], Bytes::new());
    req.version = HttpVersion::Http2;
    assert_eq!(futures::executor::block_on(f(req)).status_code(), 415);
    let resp = futures::executor::block_on(f(grpc_request(HttpVersion::Http11, vec![], &[])));
    assert_eq!(
        field(&resp.headers().to_vec(), "grpc-status"),
        Some(&b"13"[..])
    );
    let resp = futures::executor::block_on(f(grpc_request(
        HttpVersion::Http2,
        vec![],
        &[("grpc-encoding", "snappy")],
    )));
    assert_eq!(
        field(&resp.headers().to_vec(), "grpc-status"),
        Some(&b"12"[..])
    );
}

#[test]
fn oversized_message_is_resource_exhausted() {
    let f = handler_of(echo_app());
    let mut big = BytesMut::new();
    big.put_u8(0);
    big.put_u32((max_message_size() + 1) as u32);
    let resp =
        futures::executor::block_on(f(grpc_request(HttpVersion::Http2, vec![big.freeze()], &[])));
    assert_eq!(
        field(&resp.headers().to_vec(), "grpc-status"),
        Some(&b"8"[..])
    );
}

#[cfg(feature = "compression")]
#[test]
fn gzip_compressed_request() {
    let f = handler_of(echo_app());
    let mut raw = BytesMut::new();
    HealthCodec
        .encode(
            &HealthCheckRequest {
                service: "gz".into(),
            },
            &mut raw,
        )
        .unwrap();
    let gz = GrpcCompression::Gzip.compress(&raw, 6).unwrap();
    let mut fr = BytesMut::new();
    fr.put_u8(1);
    fr.put_u32(gz.len() as u32);
    fr.extend_from_slice(&gz);
    let (_, data, t) = collect(futures::executor::block_on(f(grpc_request(
        HttpVersion::Http2,
        vec![fr.freeze()],
        &[("grpc-encoding", "gzip")],
    ))));
    assert_eq!(field(&t, "grpc-status"), Some(&b"0"[..]), "{:?}", t);
    assert_eq!(decode_all::<HealthCheckRequest>(&data)[0].service, ":gz");
}

/// The `grpc-status` of a response (from the headers for Trailers-Only, otherwise from the trailers).
fn status_of(resp: crate::api::response::Response) -> Option<Vec<u8>> {
    if let Some(v) = field(&resp.headers().to_vec(), "grpc-status") {
        return Some(v.to_vec());
    }
    let (_, _, t) = collect(resp);
    field(&t, "grpc-status").map(|v| v.to_vec())
}

// B-127
/// A message that expands far beyond the limit when decompressed gives RESOURCE_EXHAUSTED without being fully decompressed.
#[cfg(feature = "compression")]
#[test]
fn gzip_bomb_is_resource_exhausted() {
    let f = handler_of(echo_app());
    // 上限（4MiB）の 16 倍に展開される 0 の列（圧縮後は 64KB 程度）
    let bomb = vec![0u8; max_message_size() * 16];
    for (name, enc) in [
        ("gzip", GrpcCompression::Gzip),
        ("deflate", GrpcCompression::Deflate),
    ] {
        let c = enc.compress(&bomb, 9).unwrap();
        assert!(c.len() < max_message_size());
        let mut fr = BytesMut::new();
        fr.put_u8(1);
        fr.put_u32(c.len() as u32);
        fr.extend_from_slice(&c);
        let resp = futures::executor::block_on(f(grpc_request(
            HttpVersion::Http2,
            vec![fr.freeze()],
            &[("grpc-encoding", name)],
        )));
        assert_eq!(status_of(resp).as_deref(), Some(&b"8"[..]), "{}", name);
    }
}

// B-127: 以前は `compression` feature 無しのビルドで gzip を解凍せずに通していた。
/// A build without the `compression` feature refuses gzip with UNIMPLEMENTED.
#[cfg(not(feature = "compression"))]
#[test]
fn gzip_without_compression_feature_is_unimplemented() {
    let f = handler_of(echo_app());
    let resp = futures::executor::block_on(f(grpc_request(
        HttpVersion::Http2,
        vec![],
        &[("grpc-encoding", "gzip")],
    )));
    assert_eq!(status_of(resp).as_deref(), Some(&b"12"[..]));
}

fn web_request(version: HttpVersion, ct: &str, body: Bytes, extra: &[(&str, &str)]) -> ApiRequest {
    let mut headers = vec![(
        Bytes::from_static(b"content-type"),
        Bytes::copy_from_slice(ct.as_bytes()),
    )];
    for (n, v) in extra {
        headers.push((
            Bytes::copy_from_slice(n.as_bytes()),
            Bytes::copy_from_slice(v.as_bytes()),
        ));
    }
    let mut req = ApiRequest::new("POST", "/pkg.Svc/M", headers, Bytes::new());
    req.version = version;
    let (mut tx, slot) = crate::api::stream::request_body_channel(1);
    futures::executor::block_on(async { tx.send(body).await.unwrap() });
    drop(tx);
    req.body_stream = Some(slot);
    req
}

/// Splits a body into frames of (flag, contents).
fn web_frames(mut body: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    while body.len() >= 5 {
        let len = u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize;
        out.push((body[0], body[5..5 + len].to_vec()));
        body = &body[5 + len..];
    }
    assert!(body.is_empty(), "trailing bytes");
    out
}

fn b64(data: &[u8]) -> Vec<u8> {
    let mut e = web::Base64Encoder::default();
    let mut out = e.encode(data).to_vec();
    out.extend_from_slice(&e.finish());
    out
}

fn unb64(data: &[u8]) -> Vec<u8> {
    let mut d = web::Base64Decoder::default();
    let mut out = BytesMut::new();
    d.push(data, &mut out).unwrap();
    d.finish().unwrap();
    out.to_vec()
}

// F-209 #4
/// gRPC-Web (binary) is accepted over HTTP/1.1 too, and the trailers are returned in the last frame (0x80) of the body.
#[test]
fn grpc_web_binary_over_http1() {
    let f = handler_of(echo_app());
    let msg = frame(&HealthCheckRequest {
        service: "web".into(),
    });
    let (h, data, t) = collect(futures::executor::block_on(f(web_request(
        HttpVersion::Http11,
        "application/grpc-web+proto",
        msg,
        &[("x-who", "b")],
    ))));
    assert!(t.is_empty(), "no HTTP trailers for gRPC-Web: {:?}", t);
    assert_eq!(
        field(&h, "content-type"),
        Some(&b"application/grpc-web+proto"[..])
    );
    let frames = web_frames(&data);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].0, 0);
    let m: HealthCheckRequest = HealthCodec
        .decode(Bytes::from(frames[0].1.clone()))
        .unwrap();
    assert_eq!(m.service, "b:web");
    assert_eq!(frames[1].0, 0x80);
    let trailers = String::from_utf8(frames[1].1.clone()).unwrap();
    assert!(trailers.contains("grpc-status: 0\r\n"), "{}", trailers);
    assert!(trailers.contains("x-tail: t\r\n"), "{}", trailers);
}

// F-209 #4
/// `-text` is base64 for both request and response. An error puts the status in both the headers and the body's trailer frame.
#[test]
fn grpc_web_text_and_errors() {
    let f = handler_of(echo_app());
    let msg = frame(&HealthCheckRequest {
        service: "txt".into(),
    });
    let (h, data, _) = collect(futures::executor::block_on(f(web_request(
        HttpVersion::Http2,
        "application/grpc-web-text",
        Bytes::from(b64(&msg)),
        &[],
    ))));
    assert_eq!(
        field(&h, "content-type"),
        Some(&b"application/grpc-web-text"[..])
    );
    let frames = web_frames(&unb64(&data));
    let m: HealthCheckRequest = HealthCodec
        .decode(Bytes::from(frames[0].1.clone()))
        .unwrap();
    assert_eq!(m.service, ":txt");
    assert_eq!(frames[1].0, 0x80);

    let bad = frame(&HealthCheckRequest {
        service: "fail".into(),
    });
    let resp = futures::executor::block_on(f(web_request(
        HttpVersion::Http11,
        "application/grpc-web-text+proto",
        Bytes::from(b64(&bad)),
        &[],
    )));
    let h = resp.headers().to_vec();
    assert_eq!(field(&h, "grpc-status"), Some(&b"3"[..]));
    let frames = web_frames(&unb64(resp.body()));
    assert_eq!(frames.len(), 1);
    assert!(String::from_utf8_lossy(&frames[0].1).contains("grpc-status: 3"));

    // 壊れた base64 は INTERNAL
    let resp = futures::executor::block_on(f(web_request(
        HttpVersion::Http2,
        "application/grpc-web-text",
        Bytes::from_static(b"!!!!"),
        &[],
    )));
    assert_eq!(
        field(&resp.headers().to_vec(), "grpc-status"),
        Some(&b"13"[..])
    );
    // 通常の gRPC は今どおり HTTP/1.1 を断る
    let resp = futures::executor::block_on(f(grpc_request(HttpVersion::Http11, vec![], &[])));
    assert_eq!(
        field(&resp.headers().to_vec(), "grpc-status"),
        Some(&b"13"[..])
    );
}

// F-209 #4
/// CORS (`[api] grpc_web_cors_origins`): the preflight and response headers are returned only for an allowed origin.
#[test]
fn grpc_web_cors() {
    let f = handler_of(echo_app());
    web::set_cors_origins(vec!["https://app.example".into()]);
    let preflight = |origin: &str| {
        let req = ApiRequest::new(
            "OPTIONS",
            "/pkg.Svc/M",
            vec![
                (
                    Bytes::from_static(b"origin"),
                    Bytes::copy_from_slice(origin.as_bytes()),
                ),
                (
                    Bytes::from_static(b"access-control-request-headers"),
                    Bytes::from_static(b"content-type,x-grpc-web"),
                ),
            ],
            Bytes::new(),
        );
        futures::executor::block_on(f(req))
    };
    let ok = preflight("https://app.example");
    assert_eq!(ok.status_code(), 204);
    let h = ok.headers().to_vec();
    assert_eq!(
        field(&h, "access-control-allow-origin"),
        Some(&b"https://app.example"[..])
    );
    assert_eq!(
        field(&h, "access-control-allow-headers"),
        Some(&b"content-type,x-grpc-web"[..])
    );
    assert_eq!(preflight("https://evil.example").status_code(), 403);
    let msg = frame(&HealthCheckRequest {
        service: "c".into(),
    });
    let (h, _, _) = collect(futures::executor::block_on(f(web_request(
        HttpVersion::Http11,
        "application/grpc-web",
        msg,
        &[("origin", "https://app.example")],
    ))));
    assert_eq!(
        field(&h, "access-control-allow-origin"),
        Some(&b"https://app.example"[..])
    );
    assert!(field(&h, "access-control-expose-headers").is_some());
}

#[test]
fn bidi_and_client_streaming() {
    let bidi = handler_of(App::new().grpc_bidi(
        "/pkg.Svc/Chat",
        HealthCodec,
        |req: Request<Streaming<HealthCheckRequest>>| async move {
            let s = req.into_inner().map(|m| {
                m.map(|m| HealthCheckRequest {
                    service: m.service.to_uppercase(),
                })
            });
            Ok(Response::new(s))
        },
    ));
    let frames = vec![
        frame(&HealthCheckRequest {
            service: "a".into(),
        }),
        frame(&HealthCheckRequest {
            service: "b".into(),
        }),
    ];
    let (_, data, t) = collect(futures::executor::block_on(bidi(grpc_request(
        HttpVersion::Http2,
        frames.clone(),
        &[],
    ))));
    let got: Vec<String> = decode_all::<HealthCheckRequest>(&data)
        .into_iter()
        .map(|m| m.service)
        .collect();
    assert_eq!(got, ["A", "B"]);
    assert_eq!(field(&t, "grpc-status"), Some(&b"0"[..]));

    let client = handler_of(App::new().grpc_client_streaming(
        "/pkg.Svc/Up",
        HealthCodec,
        |req: Request<Streaming<HealthCheckRequest>>| async move {
            let mut s = req.into_inner();
            let mut n = 0;
            while let Some(m) = s.message().await? {
                n += m.service.len();
            }
            Ok(Response::new(HealthCheckResponse { status: n as i32 }))
        },
    ));
    let (_, data, _) = collect(futures::executor::block_on(client(grpc_request(
        HttpVersion::Http2,
        frames,
        &[],
    ))));
    assert_eq!(decode_all::<HealthCheckResponse>(&data)[0].status, 2);
}

#[test]
fn server_streaming_error_mid_stream() {
    let f = handler_of(App::new().grpc_server_streaming(
        "/pkg.Svc/Watch",
        HealthCodec,
        |_req: Request<HealthCheckRequest>| async move {
            Ok(Response::new(futures::stream::iter(vec![
                Ok(HealthCheckResponse { status: 1 }),
                Err(Status::aborted("stop")),
                Ok(HealthCheckResponse { status: 2 }),
            ])))
        },
    ));
    let (_, data, t) = collect(futures::executor::block_on(f(grpc_request(
        HttpVersion::Http2,
        vec![frame(&HealthCheckRequest::default())],
        &[],
    ))));
    assert_eq!(decode_all::<HealthCheckResponse>(&data).len(), 1);
    assert_eq!(field(&t, "grpc-status"), Some(&b"10"[..]));
}

#[test]
fn unknown_method_of_service_is_unimplemented() {
    let app = echo_app();
    let fallback = app
        .routes
        .iter()
        .find(|r| r.path == "/pkg.Svc/{*method}")
        .expect("fallback route")
        .func
        .clone();
    let resp = futures::executor::block_on(fallback(grpc_request(HttpVersion::Http2, vec![], &[])));
    assert_eq!(
        field(&resp.headers().to_vec(), "grpc-status"),
        Some(&b"12"[..])
    );
}

#[test]
fn health_check_and_watch() {
    let app = App::new().grpc_health();
    let func = |p: &str| {
        app.routes
            .iter()
            .find(|r| r.path == p)
            .unwrap()
            .func
            .clone()
    };
    let check = func("/grpc.health.v1.Health/Check");
    let watch = func("/grpc.health.v1.Health/Watch");
    let (_, data, _) = collect(futures::executor::block_on(check(grpc_request(
        HttpVersion::Http2,
        vec![frame(&HealthCheckRequest::default())],
        &[],
    ))));
    assert_eq!(decode_all::<HealthCheckResponse>(&data)[0].status, 1);
    let resp = futures::executor::block_on(check(grpc_request(
        HttpVersion::Http2,
        vec![frame(&HealthCheckRequest {
            service: "nope.Svc".into(),
        })],
        &[],
    )));
    assert_eq!(
        field(&resp.headers().to_vec(), "grpc-status"),
        Some(&b"5"[..])
    );

    super::health::set_serving_status("w.Svc", super::health::ServingStatus::Serving);
    let mut resp = futures::executor::block_on(watch(grpc_request(
        HttpVersion::Http2,
        vec![frame(&HealthCheckRequest {
            service: "w.Svc".into(),
        })],
        &[],
    )));
    let mut s = resp.take_stream().unwrap();
    let first = futures::executor::block_on(s.next()).unwrap();
    super::health::set_serving_status("w.Svc", super::health::ServingStatus::NotServing);
    let second = futures::executor::block_on(s.next()).unwrap();
    let status = |c: BodyChunk| match c {
        BodyChunk::Data(b) => decode_all::<HealthCheckResponse>(&b)[0].status,
        other => panic!("{:?}", other),
    };
    assert_eq!(status(first), 1);
    assert_eq!(status(second), 2);
}

// F-208
/// The hand-written health-check encoding produces the same bytes as prost (a dev-dependency) and each can read the other.
#[test]
fn health_codec_matches_prost() {
    #[derive(Clone, PartialEq, prost::Message)]
    struct PReq {
        #[prost(string, tag = "1")]
        service: String,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    struct PResp {
        #[prost(int32, tag = "1")]
        status: i32,
    }
    for svc in ["", "a", "pkg.Service", &"x".repeat(300)] {
        let mut ours = BytesMut::new();
        HealthCodec
            .encode(
                &HealthCheckRequest {
                    service: svc.into(),
                },
                &mut ours,
            )
            .unwrap();
        let theirs = prost::Message::encode_to_vec(&PReq {
            service: svc.into(),
        });
        assert_eq!(&ours[..], &theirs[..], "{:?}", svc);
        let back: HealthCheckRequest = HealthCodec.decode(Bytes::from(theirs)).unwrap();
        assert_eq!(back.service, svc);
    }
    for st in [0, 1, 2, 3, -1] {
        let mut ours = BytesMut::new();
        HealthCodec
            .encode(&HealthCheckResponse { status: st }, &mut ours)
            .unwrap();
        let theirs = prost::Message::encode_to_vec(&PResp { status: st });
        assert_eq!(&ours[..], &theirs[..], "{}", st);
        let back: HealthCheckResponse = HealthCodec.decode(Bytes::from(theirs)).unwrap();
        assert_eq!(back.status, st);
    }
    // 壊れたメッセージは INTERNAL
    let r: Result<HealthCheckRequest, _> =
        HealthCodec.decode(Bytes::from_static(&[0x0a, 0x05, b'a']));
    assert_eq!(r.unwrap_err().code(), Code::Internal);
}

// F-208
/// `BytesCodec` passes unencoded bytes through as they are (the C and Python bridge, and use without generated code).
#[test]
fn bytes_codec_roundtrip() {
    let f = handler_of(App::new().grpc_unary(
        "/raw.Svc/Echo",
        BytesCodec,
        |req: Request<Bytes>| async move {
            let mut v = b"echo:".to_vec();
            v.extend_from_slice(req.get_ref());
            Ok(Response::new(Bytes::from(v)))
        },
    ));
    let msg = encode_message(&BytesCodec, &Bytes::from_static(b"\x00\x01raw")).unwrap();
    let (_, data, t) = collect(futures::executor::block_on(f(grpc_request(
        HttpVersion::Http2,
        vec![msg],
        &[],
    ))));
    assert_eq!(field(&t, "grpc-status"), Some(&b"0"[..]));
    assert_eq!(&data[5..], b"echo:\x00\x01raw");
    assert_eq!(
        u32::from_be_bytes(data[1..5].try_into().unwrap()) as usize,
        data.len() - 5
    );
}

// F-209 #2
/// Response compression needs both the call's choice (or the setting) and the client's `grpc-accept-encoding`.
#[cfg(feature = "compression")]
#[test]
fn response_compression_needs_client_acceptance() {
    let f = handler_of(App::new().grpc_unary(
        "/raw.Svc/Big",
        BytesCodec,
        |_req: Request<Bytes>| async move {
            Ok(Response::new(Bytes::from(vec![b'a'; 4096])).with_compression(Compression::Gzip))
        },
    ));
    let call = |accept: &[(&str, &str)]| {
        let msg = encode_message(&BytesCodec, &Bytes::from_static(b"x")).unwrap();
        collect(futures::executor::block_on(f(grpc_request(
            HttpVersion::Http2,
            vec![msg],
            accept,
        ))))
    };
    // 受ける: grpc-encoding: gzip、圧縮フラグ 1、戻すと元の 4096 バイト
    let (h, data, t) = call(&[("grpc-accept-encoding", "identity, gzip")]);
    assert_eq!(field(&t, "grpc-status"), Some(&b"0"[..]));
    assert_eq!(field(&h, "grpc-encoding"), Some(&b"gzip"[..]));
    assert_eq!(data[0], 1);
    let len = u32::from_be_bytes(data[1..5].try_into().unwrap()) as usize;
    assert!(len < 4096, "{}", len);
    let raw = Compression::Gzip.decompress(&data[5..5 + len]).unwrap();
    assert_eq!(raw, vec![b'a'; 4096]);
    // 受けると言っていない: 非圧縮
    let (h, data, _) = call(&[]);
    assert_eq!(field(&h, "grpc-encoding"), None);
    assert_eq!(data[0], 0);
    assert_eq!(data.len(), 5 + 4096);
    // deflate しか受けない: 指定の gzip は使わない
    let (h, data, _) = call(&[("grpc-accept-encoding", "deflate")]);
    assert_eq!(field(&h, "grpc-encoding"), None);
    assert_eq!(data[0], 0);
}

#[test]
fn response_encoding_negotiation() {
    assert_eq!(accepted_encodings(Some("identity, gzip ,DEFLATE")), 3);
    assert_eq!(accepted_encodings(None), 0);
    assert_eq!(
        response_encoding(Some(Compression::Identity), 3),
        Compression::Identity
    );
    let gz = response_encoding(Some(Compression::Gzip), 1);
    assert_eq!(gz == Compression::Gzip, cfg!(feature = "compression"));
    assert_eq!(
        response_encoding(Some(Compression::Gzip), 2),
        Compression::Identity
    );
}

// F-209 #6
/// `with_details` sends `grpc-status-details-bin` (base64, without `=`).
#[test]
fn status_details_bin() {
    let t = Status::invalid_argument("bad")
        .with_details(Bytes::from_static(b"\x08\x03\x12\x03bad"))
        .to_trailers();
    let v = field(&t, "grpc-status-details-bin").unwrap();
    assert_eq!(v, b"CAMSA2JhZA");
    let mut d = web::Base64Decoder::default();
    let mut out = BytesMut::new();
    d.push(b"CAMSA2JhZA==", &mut out).unwrap();
    assert_eq!(&out[..], b"\x08\x03\x12\x03bad");
}

// F-209 #7
/// An interceptor can refuse a call by looking at the metadata (Trailers-Only). Requests that are not gRPC pass through.
#[test]
fn interceptor_rejects_and_passes() {
    let app = App::new()
        .grpc_interceptor("/pkg.Svc/", |r: &Request<()>| {
            assert_eq!(r.method(), "/pkg.Svc/M");
            match r.metadata_value("authorization") {
                Some(b"Bearer ok") => Ok(()),
                _ => Err(Status::unauthenticated("no token")),
            }
        })
        .grpc_unary(
            "/pkg.Svc/M",
            HealthCodec,
            |req: Request<HealthCheckRequest>| async move {
                assert_eq!(req.method(), "/pkg.Svc/M");
                Ok(Response::new(HealthCheckResponse { status: 1 }))
            },
        );
    let f = app.last_route_wrapped();
    let msg = || frame(&HealthCheckRequest::default());
    let resp = futures::executor::block_on(f(grpc_request(HttpVersion::Http2, vec![msg()], &[])));
    assert_eq!(
        field(&resp.headers().to_vec(), "grpc-status"),
        Some(&b"16"[..])
    );
    let (_, _, t) = collect(futures::executor::block_on(f(grpc_request(
        HttpVersion::Http2,
        vec![msg()],
        &[("authorization", "Bearer ok")],
    ))));
    assert_eq!(field(&t, "grpc-status"), Some(&b"0"[..]));
}

// F-209 #8
/// The limit on sent messages.
#[test]
fn send_limit() {
    assert!(send_limit_error(10, 0).is_none());
    assert!(send_limit_error(10, 10).is_none());
    assert_eq!(
        send_limit_error(11, 10).unwrap().code(),
        Code::ResourceExhausted
    );
}

// F-209 #9
/// An ordinary gRPC request matching no route gets a Trailers-Only UNIMPLEMENTED. gRPC-Web and others still get 404.
#[test]
fn unknown_route_is_unimplemented_for_grpc() {
    let h = |ct: &'static [u8]| vec![(&b"content-type"[..], ct)];
    let r = unknown_route_response(h(b"application/grpc").into_iter(), b"/nope.Svc/M").unwrap();
    assert_eq!(field(&r, "grpc-status"), Some(&b"12"[..]));
    assert!(percent_decode(field(&r, "grpc-message").unwrap()).contains("nope.Svc"));
    assert!(unknown_route_response(h(b"application/grpc-web").into_iter(), b"/x/y").is_none());
    assert!(unknown_route_response(h(b"text/plain").into_iter(), b"/x/y").is_none());
    assert!(unknown_route_response(Vec::new().into_iter(), b"/x/y").is_none());
}
