//! パススルー Proxy-Wasm フィルタ
//!
//! リクエスト・レスポンスを一切変更せず `Action::Continue` を返すだけの最小フィルタ。
//! wasmtime ランタイム呼び出し（インスタンス取得・コンテキストスイッチ・ABI 往復）
//! 自体のベースラインオーバーヘッドを計測するために使う（F-89、tools/perf の
//! `h2_1_feat_wasm` 構成から参照）。

use proxy_wasm::traits::*;
use proxy_wasm::types::*;

proxy_wasm::main! {{
    proxy_wasm::set_log_level(LogLevel::Warn);
    proxy_wasm::set_root_context(|_| -> Box<dyn RootContext> {
        Box::new(PassthroughRoot)
    });
}}

struct PassthroughRoot;

impl Context for PassthroughRoot {}

impl RootContext for PassthroughRoot {
    fn get_type(&self) -> Option<ContextType> {
        Some(ContextType::HttpContext)
    }

    fn create_http_context(&self, _context_id: u32) -> Option<Box<dyn HttpContext>> {
        Some(Box::new(PassthroughFilter))
    }
}

struct PassthroughFilter;

impl Context for PassthroughFilter {}

impl HttpContext for PassthroughFilter {
    fn on_http_request_headers(&mut self, _num_headers: usize, _end_of_stream: bool) -> Action {
        Action::Continue
    }

    fn on_http_response_headers(&mut self, _num_headers: usize, _end_of_stream: bool) -> Action {
        Action::Continue
    }
}
