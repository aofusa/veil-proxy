//! Sample Proxy-Wasm filter for gRPC request header / response trailer rewriting (F-133 E2E)
//!
//! - `on_http_request_headers`: adds `x-wasm-request-rewrite: applied` to the request
//!   forwarded to the backend, so an E2E test can verify (via a backend that echoes the
//!   header back into response metadata) that WASM-rewritten gRPC request headers reach
//!   the backend.
//! - `on_http_response_trailers`: rewrites the gRPC `grpc-status`/`grpc-message` trailers
//!   unconditionally, so an E2E test can verify the client observes the rewritten values.
//! - `on_http_request_trailers`: refuses a request whose trailers carry `x-wasm-reject: 1`
//!   with a 403 local response, so an E2E test can verify the request trailer phase runs.

use proxy_wasm::traits::*;
use proxy_wasm::types::*;

proxy_wasm::main! {{
    proxy_wasm::set_log_level(LogLevel::Info);
    proxy_wasm::set_root_context(|_| -> Box<dyn RootContext> {
        Box::new(GrpcTrailerFilterRoot)
    });
}}

struct GrpcTrailerFilterRoot;

impl Context for GrpcTrailerFilterRoot {}

impl RootContext for GrpcTrailerFilterRoot {
    fn get_type(&self) -> Option<ContextType> {
        Some(ContextType::HttpContext)
    }

    fn create_http_context(&self, context_id: u32) -> Option<Box<dyn HttpContext>> {
        Some(Box::new(GrpcTrailerFilter { context_id }))
    }
}

struct GrpcTrailerFilter {
    context_id: u32,
}

impl Context for GrpcTrailerFilter {}

impl HttpContext for GrpcTrailerFilter {
    fn on_http_request_headers(&mut self, _num_headers: usize, _end_of_stream: bool) -> Action {
        self.add_http_request_header("x-wasm-request-rewrite", "applied");
        log::info!(
            "[grpc-trailer-filter] added request header, context={}",
            self.context_id
        );
        Action::Continue
    }

    // F-313: HTTP/2 と HTTP/3 で要求のトレーラの段が走ることを確かめるため
    fn on_http_request_trailers(&mut self, _num_trailers: usize) -> Action {
        if self.get_http_request_trailer("x-wasm-reject").as_deref() == Some("1") {
            self.send_http_response(
                403,
                vec![("x-wasm-trailer-reject", "1")],
                Some(b"rejected by request trailers"),
            );
            return Action::Pause;
        }
        Action::Continue
    }

    fn on_http_response_trailers(&mut self, _num_trailers: usize) -> Action {
        self.set_http_response_trailer("grpc-status", Some("0"));
        self.set_http_response_trailer("grpc-message", Some("rewritten-by-wasm"));
        log::info!(
            "[grpc-trailer-filter] rewrote grpc trailers, context={}",
            self.context_id
        );
        Action::Continue
    }
}
