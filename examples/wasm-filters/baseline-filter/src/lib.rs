//! Proxy-Wasm filter that answers requests by itself (F-271).
//!
//! - `/baseline11?a=1&b=2`: responds with the sum of the query parameters, plus the integer in the request body
//!   (POST with Content-Length or chunked). The body is read in `on_http_request_body`.
//! - `/dataset`: responds with the `plugin_data.dataset` property (`[[wasm.modules]] data_files`).
//! - `/rewrite`: replaces the request body with `rewritten:<body>` and lets the request continue upstream.
//! - `/trap`: traps (to check that a reused instance is replaced after a trap).
//!
//! Every local response carries `x-configure-count` (how many times `on_configure` ran in this instance) and
//! `x-instance-seq` (how many HTTP contexts this instance has created), which show whether veil reuses the
//! instance (`instance_reuse = true`).

use proxy_wasm::traits::*;
use proxy_wasm::types::*;
use std::cell::Cell;

thread_local! {
    static CONFIGURE_COUNT: Cell<u32> = const { Cell::new(0) };
    static CONTEXTS: Cell<u32> = const { Cell::new(0) };
}

proxy_wasm::main! {{
    proxy_wasm::set_root_context(|_| -> Box<dyn RootContext> { Box::new(Root) });
}}

struct Root;

impl Context for Root {}

impl RootContext for Root {
    fn on_configure(&mut self, _size: usize) -> bool {
        CONFIGURE_COUNT.with(|c| c.set(c.get() + 1));
        true
    }

    fn get_type(&self) -> Option<ContextType> {
        Some(ContextType::HttpContext)
    }

    fn create_http_context(&self, _context_id: u32) -> Option<Box<dyn HttpContext>> {
        CONTEXTS.with(|c| c.set(c.get() + 1));
        Some(Box::new(Baseline))
    }
}

struct Baseline;

impl Context for Baseline {}

fn request_path(ctx: &dyn HttpContext) -> String {
    ctx.get_property(vec!["request", "path"])
        .and_then(|v| String::from_utf8(v).ok())
        .unwrap_or_default()
}

fn query_sum(path: &str) -> i64 {
    let Some((_, q)) = path.split_once('?') else {
        return 0;
    };
    q.split('&')
        .filter_map(|kv| kv.split_once('='))
        .filter_map(|(_, v)| v.parse::<i64>().ok())
        .sum()
}

/// The path without the query. A `/fresh` prefix is ignored, so tests can route the same requests to a second
/// module.
fn route(path: &str) -> &str {
    let p = path.split('?').next().unwrap_or("");
    p.strip_prefix("/fresh").unwrap_or(p)
}

impl Baseline {
    fn respond(&self, body: &[u8]) {
        let configured = CONFIGURE_COUNT.with(|c| c.get()).to_string();
        let seq = CONTEXTS.with(|c| c.get()).to_string();
        self.send_http_response(
            200,
            vec![
                ("content-type", "text/plain"),
                ("x-configure-count", &configured),
                ("x-instance-seq", &seq),
            ],
            Some(body),
        );
    }
}

impl HttpContext for Baseline {
    fn on_http_request_headers(&mut self, _num_headers: usize, end_of_stream: bool) -> Action {
        let path = request_path(self);
        match route(&path) {
            "/trap" => panic!("trap requested"),
            "/dataset" => {
                let data = self
                    .get_property(vec!["plugin_data", "dataset"])
                    .unwrap_or_default();
                self.respond(&data);
                Action::Pause
            }
            "/baseline11" if end_of_stream => {
                self.respond(query_sum(&path).to_string().as_bytes());
                Action::Pause
            }
            _ => Action::Continue,
        }
    }

    fn on_http_request_body(&mut self, body_size: usize, end_of_stream: bool) -> Action {
        if !end_of_stream {
            return Action::Pause;
        }
        let path = request_path(self);
        let body = self.get_http_request_body(0, body_size).unwrap_or_default();
        match route(&path) {
            "/baseline11" => {
                let n: i64 = std::str::from_utf8(&body)
                    .ok()
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(0);
                self.respond((query_sum(&path) + n).to_string().as_bytes());
                Action::Pause
            }
            "/rewrite" => {
                let mut out = b"rewritten:".to_vec();
                out.extend_from_slice(&body);
                self.set_http_request_body(0, body_size, &out);
                Action::Continue
            }
            _ => Action::Continue,
        }
    }
}
