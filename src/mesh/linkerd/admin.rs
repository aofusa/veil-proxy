//! Linkerd のプロキシの管理ポート（4191。F-223）: `/ready`（証明書が届いた）・`/live`・`/metrics`・
//! `/shutdown`（`LINKERD2_PROXY_SHUTDOWN_ENDPOINT_ENABLED`）・`/env.json`・`/proxy-log-level`。

use std::sync::atomic::Ordering;

pub fn route(method: &str, path: &str, _query: &str) -> (u16, &'static str, String) {
    let cfg = super::conn::CONFIG.get();
    match path {
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
                (404, "text/plain", "shutdown endpoint is not enabled\n".into())
            }
        }
        "/env.json" => {
            let mut m = serde_json::Map::new();
            for (k, v) in std::env::vars() {
                if k.starts_with("LINKERD2_PROXY_") {
                    m.insert(k, serde_json::Value::String(v));
                }
            }
            (200, "application/json", serde_json::Value::Object(m).to_string())
        }
        "/proxy-log-level" => (200, "text/plain", cfg.map(|c| c.log.clone()).unwrap_or_default()),
        _ => (404, "text/plain", "not found\n".into()),
    }
}
