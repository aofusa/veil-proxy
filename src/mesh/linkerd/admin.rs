//! Linkerd のプロキシの管理ポート（4191。F-223）: `/ready`（証明書が届いた）・`/live`・`/metrics`・
//! `/shutdown`（`LINKERD2_PROXY_SHUTDOWN_ENDPOINT_ENABLED`）・`/env.json`・`/proxy-log-level`。
//!
//! メッシュの Pod（viz の Prometheus 等）は相手のプロキシ経由で mTLS（HTTP/2 に昇格することもある）で来るので、
//! データプレーンのワーカーで受ける（自分の ID の TLS を終端し、HTTP/1.1・HTTP/2 の両方に答える）。

use crate::mesh::detect::{self, Detected};
use crate::mesh::server::DownStream;
use crate::proxy::{H2RequestCtx, H2RespMsg, H2Service};
use crate::runtime::io::{AsyncReadRent, AsyncWriteRentExt};
use crate::runtime::tcp::TcpStream;
use crate::stream_channel::{Notify, Receiver, Sender};
use bytes::Bytes;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// 管理ポートへの接続。
pub async fn serve(cfg: &super::env::Config, stream: TcpStream, peer: SocketAddr) {
    let (det, initial) = detect::sniff(&stream, Some(Duration::from_secs(10))).await;
    let (mut ds, acc) = match det {
        Detected::Closed => return,
        Detected::Tls { sni: Some(sni), .. } if sni == cfg.local_name => {
            let Some((mut ds, _id)) = super::conn::accept_tls(stream, initial, peer).await else {
                return;
            };
            let rest = if ds.alpn_protocol() == Some(super::proto::TRANSPORT_ALPN) {
                match super::conn::read_transport_header(&mut ds).await {
                    Some((_, rest)) => rest,
                    None => return,
                }
            } else {
                Vec::new()
            };
            (ds, rest)
        }
        _ => match crate::mesh::server::plain(stream, initial).await {
            Ok(ds) => (ds, Vec::new()),
            Err(_) => return,
        },
    };
    let (det, acc) = super::conn::read_classify(&mut ds, acc).await;
    match det {
        Detected::H2c => serve_h2(ds, acc, peer).await,
        Detected::Http1 => serve_h1(ds, acc, peer.ip()).await,
        _ => {}
    }
}

async fn serve_h1(mut ds: DownStream, mut acc: Vec<u8>, peer: std::net::IpAddr) {
    loop {
        let head_end = loop {
            if let Some(i) = memchr::memmem::find(&acc, b"\r\n\r\n") {
                break i + 4;
            }
            if acc.len() > 64 * 1024 {
                return;
            }
            let buf: Vec<u8> = Vec::with_capacity(4096);
            let (r, buf) =
                match crate::runtime::timeout(Duration::from_secs(60), ds.read(buf)).await {
                    Ok(r) => r,
                    Err(_) => return,
                };
            match r {
                Ok(0) | Err(_) => return,
                Ok(n) => acc.extend_from_slice(&buf[..n]),
            }
        };
        let (method, target, close, body_len) = {
            let mut hb = [httparse::EMPTY_HEADER; 64];
            let mut req = httparse::Request::new(&mut hb);
            if !matches!(
                req.parse(&acc[..head_end]),
                Ok(httparse::Status::Complete(_))
            ) {
                return;
            }
            let close = req.headers.iter().any(|h| {
                h.name.eq_ignore_ascii_case("connection") && h.value.eq_ignore_ascii_case(b"close")
            });
            let len = req
                .headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case("content-length"))
                .and_then(|h| {
                    std::str::from_utf8(h.value)
                        .ok()?
                        .trim()
                        .parse::<usize>()
                        .ok()
                })
                .unwrap_or(0);
            (
                req.method.unwrap_or("GET").to_string(),
                req.path.unwrap_or("/").to_string(),
                close,
                len,
            )
        };
        // 本文は読み捨てる（POST /shutdown 等）
        while acc.len() < head_end + body_len {
            let buf: Vec<u8> = Vec::with_capacity(4096);
            let (r, buf) = ds.read(buf).await;
            match r {
                Ok(0) | Err(_) => return,
                Ok(n) => acc.extend_from_slice(&buf[..n]),
            }
        }
        acc.drain(..head_end + body_len);
        let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
        let (status, ctype, body) = route(&method, path, query, peer);
        let head = format!(
            "HTTP/1.1 {} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\n{}\r\n",
            status,
            crate::mesh::httpcore::reason(status),
            ctype,
            body.len(),
            if close { "connection: close\r\n" } else { "" }
        );
        let mut out = head.into_bytes();
        out.extend_from_slice(body.as_bytes());
        if ds.write_all(out).await.0.is_err() || close {
            return;
        }
    }
}

/// HTTP/2 の管理ポートの要求の処理（`/shutdown`・`/env.json` の判定に接続の相手を使う）
struct AdminH2 {
    peer: std::net::IpAddr,
}

impl H2Service for AdminH2 {
    fn streaming_plan<S>(
        &self,
        _conn: &crate::http2::Http2Connection<S>,
        _stream_id: u32,
        _client_ip: &str,
        _client_socket_addr: SocketAddr,
    ) -> Option<u64>
    where
        S: AsyncReadRent + AsyncWriteRentExt + Unpin,
    {
        None
    }

    fn spawn(
        &self,
        ctx: H2RequestCtx,
        _req_rx: Option<Receiver<Bytes>>,
        resp_tx: Sender<H2RespMsg>,
        notify: Notify,
    ) {
        let method = String::from_utf8_lossy(&ctx.method).into_owned();
        let target = String::from_utf8_lossy(&ctx.path).into_owned();
        let orig_proto = ctx
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case(b"l5d-orig-proto"));
        let peer = self.peer;
        crate::runtime::spawn(async move {
            let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
            let (status, ctype, body) = route(&method, path, query, peer);
            let mut headers = vec![
                (
                    Bytes::from_static(b"content-type"),
                    Bytes::from_static(ctype.as_bytes()),
                ),
                (
                    Bytes::from_static(b"content-length"),
                    Bytes::from(body.len().to_string()),
                ),
            ];
            if orig_proto {
                headers.push((
                    Bytes::from_static(b"l5d-orig-proto"),
                    Bytes::from_static(b"HTTP/1.1"),
                ));
            }
            let empty = body.is_empty();
            let ok = resp_tx
                .send(H2RespMsg::Head {
                    status,
                    headers,
                    end_stream: empty,
                })
                .await
                .is_ok();
            notify.notify();
            if ok && !empty {
                let _ = resp_tx.send(H2RespMsg::Body(Bytes::from(body))).await;
                notify.notify();
            }
            drop(resp_tx);
            notify.notify();
        });
    }
}

async fn serve_h2(ds: DownStream, acc: Vec<u8>, peer: SocketAddr) {
    let settings = crate::config::CURRENT_CONFIG
        .load()
        .http2_config
        .to_http2_settings();
    let mut conn = crate::http2::Http2Connection::new_with_initial_buffer(ds, settings, acc);
    if conn.handshake().await.is_err() {
        return;
    }
    let mut metric = crate::metrics::ActiveConnectionMetric::new(false);
    let ip = peer.ip().to_string();
    let svc = AdminH2 { peer: peer.ip() };
    let _ = crate::proxy::handle_http2_requests_with(&mut conn, &ip, &mut metric, None, &svc).await;
}

pub fn route(
    method: &str,
    path: &str,
    _query: &str,
    peer: std::net::IpAddr,
) -> (u16, &'static str, String) {
    let cfg = super::conn::CONFIG.get();
    // `/shutdown`・`/env.json` はポッドの中（ループバック）からだけ受ける（linkerd2-proxy と同じ）。
    // 管理ポートは 0.0.0.0 で待つので、以前はクラスタ内のどこからでも止め・設定を読めた
    // （F-236 の再走査の F8）。IPv4 射影のループバックもループバックとして扱う。
    let local = peer.to_canonical().is_loopback();
    match path {
        "/shutdown" | "/env.json" if !local => (403, "text/plain", "forbidden\n".into()),
        "/ready" => {
            if super::identity::READY.load(Ordering::Relaxed) {
                (200, "text/plain", "ready\n".into())
            } else {
                (503, "text/plain", "not ready\n".into())
            }
        }
        "/live" => (200, "text/plain", "live\n".into()),
        "/metrics" => (200, "text/plain; version=0.0.4", super::metrics::text()),
        "/shutdown" if method == "POST" => {
            if cfg.is_some_and(|c| c.shutdown_enabled) {
                crate::mesh::state::state()
                    .draining
                    .store(true, Ordering::Relaxed);
                crate::config::SHUTDOWN_FLAG.store(true, Ordering::Relaxed);
                (200, "text/plain", "shutdown\n".into())
            } else {
                (
                    404,
                    "text/plain",
                    "shutdown endpoint is not enabled\n".into(),
                )
            }
        }
        "/env.json" => {
            let mut m = serde_json::Map::new();
            for (k, v) in std::env::vars() {
                if k.starts_with("LINKERD2_PROXY_") {
                    m.insert(k, serde_json::Value::String(v));
                }
            }
            (
                200,
                "application/json",
                serde_json::Value::Object(m).to_string(),
            )
        }
        "/proxy-log-level" => (
            200,
            "text/plain",
            cfg.map(|c| c.log.clone()).unwrap_or_default(),
        ),
        _ => (404, "text/plain", "not found\n".into()),
    }
}

#[cfg(test)]
mod admin_access_tests {
    use super::*;

    /// F-236 の再走査の F8: `/shutdown`・`/env.json` はループバックからだけ受ける
    #[test]
    fn shutdown_and_env_are_loopback_only() {
        let remote: std::net::IpAddr = "10.1.2.3".parse().unwrap();
        assert_eq!(route("POST", "/shutdown", "", remote).0, 403);
        assert_eq!(route("GET", "/env.json", "", remote).0, 403);
        let mapped: std::net::IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        assert_eq!(route("GET", "/env.json", "", mapped).0, 200);
        assert_eq!(route("GET", "/live", "", remote).0, 200);
    }
}
