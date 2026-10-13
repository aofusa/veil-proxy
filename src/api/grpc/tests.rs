//! gRPC サーバ層の単体テスト（ワーカーを起動せず、ハンドラ関数を直接呼ぶ）。

use super::health::{HealthCheckRequest, HealthCheckResponse};
use super::*;
use crate::api::request::Request as ApiRequest;
use futures::SinkExt;

fn frame<T: prost::Message>(m: &T) -> Bytes {
    encode_message(m)
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

/// 応答を (ヘッダ, DATA の連結, トレーラ) にする。
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

fn decode_all<T: prost::Message + Default>(data: &[u8]) -> Vec<T> {
    let mut d = GrpcFrameDecoder::new();
    d.push(data);
    let mut out = Vec::new();
    while let Some(f) = d.decode_next().unwrap() {
        out.push(T::decode(&f.data[..]).unwrap());
    }
    out
}

fn handler_of(app: App) -> HandlerFn {
    app.routes.last().expect("registered").func.clone()
}

fn echo_app() -> App {
    App::new().grpc_unary(
        "/pkg.Svc/M",
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
    let raw = prost::Message::encode_to_vec(&HealthCheckRequest {
        service: "gz".into(),
    });
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

#[test]
fn bidi_and_client_streaming() {
    let bidi = handler_of(App::new().grpc_bidi(
        "/pkg.Svc/Chat",
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
