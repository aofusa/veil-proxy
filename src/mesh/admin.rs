//! Envoy の管理 API の模倣（F-222）。127.0.0.1:15000 で、pilot-agent・istioctl が使うものだけを返す。
//!
//! - `/ready`・`/stats`（`?usedonly&filter=` の正規表現）: pilot-agent の準備確認（`server.state`・
//!   `listener_manager.workers_started`・`cluster_manager.cds.update_success` 等）
//! - `/stats/prometheus`: Istio の標準メトリクス（15090 から転送される）
//! - `/drain_listeners`（`?inboundonly&graceful`）・`/quitquitquit`: 終了時の排出
//! - `/server_info`・`/config_dump`・`/clusters`・`/listeners`・`/memory`・`/logging`
//!
//! 管理 API は制御用の専用スレッドで、ブロッキングの `std::net` で動かす（データプレーン外）。

use super::state::state;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::time::Duration;

/// 管理 API の経路（メソッド・パス・クエリ → ステータス・Content-Type・本文）。
pub type Route = fn(&str, &str, &str) -> (u16, &'static str, String);

pub fn spawn(addr: SocketAddr) {
    spawn_with(addr, route);
}

/// 経路を差し替えて管理 API を開く（Linkerd の 4191 も同じ作り。F-223）。
pub fn spawn_with(addr: SocketAddr, route: Route) {
    let r = std::thread::Builder::new()
        .name("veil-admin".into())
        .spawn(move || {
            let l = match TcpListener::bind(addr) {
                Ok(l) => l,
                Err(e) => {
                    ftlog::error!("[admin] cannot listen on {}: {}", addr, e);
                    return;
                }
            };
            for s in l.incoming() {
                if crate::config::SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                    return;
                }
                if let Ok(s) = s {
                    let _ = std::thread::Builder::new()
                        .name("veil-admin-conn".into())
                        .spawn(move || {
                            let _ = handle(s, route);
                        });
                }
            }
        });
    if let Err(e) = r {
        ftlog::error!("[admin] cannot spawn: {}", e);
    }
}

fn handle(mut s: TcpStream, route: Route) -> std::io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    loop {
        let n = s.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 64 * 1024 {
            break;
        }
    }
    let mut hb = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut hb);
    let _ = req.parse(&buf);
    let method = req.method.unwrap_or("GET").to_string();
    let target = req.path.unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };
    let (status, ctype, body) = route(&method, &path, &query);
    let head = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        status,
        super::httpcore::reason(status),
        ctype,
        body.len()
    );
    s.write_all(head.as_bytes())?;
    s.write_all(body.as_bytes())?;
    Ok(())
}

fn query_param<'a>(q: &'a str, name: &str) -> Option<&'a str> {
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (k == name).then_some(v)
    })
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn route(method: &str, path: &str, query: &str) -> (u16, &'static str, String) {
    let st = state();
    match path {
        "/ready" => {
            let ok = st.ready() && super::stats::WORKERS_STARTED.load(Ordering::Relaxed) > 0;
            if st.draining.load(Ordering::Relaxed) {
                (503, "text/plain", "DRAINING\n".into())
            } else if ok {
                (200, "text/plain", "LIVE\n".into())
            } else {
                (503, "text/plain", "PRE_INITIALIZING\n".into())
            }
        }
        "/stats" => {
            let filter = query_param(query, "filter").map(url_decode);
            if query.contains("format=prometheus") {
                (
                    200,
                    "text/plain; version=0.0.4",
                    super::stats::prometheus_text(),
                )
            } else {
                (
                    200,
                    "text/plain",
                    super::stats::envoy_stats_text(filter.as_deref()),
                )
            }
        }
        "/stats/prometheus" => (
            200,
            "text/plain; version=0.0.4",
            super::stats::prometheus_text(),
        ),
        "/server_info" => {
            let state = if st.draining.load(Ordering::Relaxed) {
                "DRAINING"
            } else if st.ready() {
                "LIVE"
            } else {
                "INITIALIZING"
            };
            let v = serde_json::json!({
                "version": format!("veil/{}", env!("CARGO_PKG_VERSION")),
                "state": state,
                "uptime_current_epoch": format!("{}s", super::httpcore::now_ms() / 1000),
                "command_line_options": {"concurrency": super::stats::WORKERS_STARTED.load(Ordering::Relaxed)},
                "node": {"id": super::peer::local().map(|l| l.id.clone()).unwrap_or_default()},
            });
            (200, "application/json", v.to_string())
        }
        "/drain_listeners" if method == "POST" => {
            if query.contains("inboundonly") {
                st.draining_inbound.store(true, Ordering::Relaxed);
            } else {
                st.draining.store(true, Ordering::Relaxed);
            }
            (200, "text/plain", "OK\n".into())
        }
        "/quitquitquit" if method == "POST" => {
            st.draining.store(true, Ordering::Relaxed);
            crate::config::SHUTDOWN_FLAG.store(true, Ordering::Relaxed);
            (200, "text/plain", "OK\n".into())
        }
        "/healthcheck/fail" if method == "POST" => {
            st.draining.store(true, Ordering::Relaxed);
            (200, "text/plain", "OK\n".into())
        }
        "/healthcheck/ok" if method == "POST" => {
            st.draining.store(false, Ordering::Relaxed);
            (200, "text/plain", "OK\n".into())
        }
        "/config_dump" => (
            200,
            "application/json",
            super::xds::dump::config_dump(query),
        ),
        "/clusters" if query.contains("format=json") => {
            (200, "application/json", super::xds::dump::clusters_json())
        }
        "/clusters" => (200, "text/plain", super::xds::dump::clusters_text()),
        "/listeners" => (200, "text/plain", super::xds::dump::listeners_text()),
        "/memory" => (
            200,
            "application/json",
            "{\"allocated\":\"0\",\"heap_size\":\"0\"}".into(),
        ),
        "/logging" => (200, "text/plain", "active loggers:\n  veil: info\n".into()),
        "/certs" => {
            let v: Vec<serde_json::Value> = super::identity::summary()
                .into_iter()
                .map(|(n, u, na)| serde_json::json!({"name": n, "subject_alt_names": u, "expiration_time": na}))
                .collect();
            (
                200,
                "application/json",
                serde_json::json!({"certificates": v}).to_string(),
            )
        }
        "/" => (200, "text/plain", "veil (envoy compatible admin)\n".into()),
        _ => (404, "text/plain", "not found\n".into()),
    }
}
