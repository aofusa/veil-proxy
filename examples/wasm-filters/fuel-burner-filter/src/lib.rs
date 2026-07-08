//! CPU を消費し続ける Proxy-Wasm フィルタ（セキュリティプローブ W-06 用）。

use proxy_wasm::traits::*;
use proxy_wasm::types::*;

proxy_wasm::main! {{
    proxy_wasm::set_root_context(|_| -> Box<dyn RootContext> {
        Box::new(FuelBurnerRoot)
    });
}}

struct FuelBurnerRoot;

impl Context for FuelBurnerRoot {}

impl RootContext for FuelBurnerRoot {
    fn get_type(&self) -> Option<ContextType> {
        Some(ContextType::HttpContext)
    }

    fn create_http_context(&self, _context_id: u32) -> Option<Box<dyn HttpContext>> {
        Some(Box::new(FuelBurner))
    }
}

struct FuelBurner;

impl Context for FuelBurner {}

impl HttpContext for FuelBurner {
    fn on_http_response_headers(&mut self, _num_headers: usize, _end_of_stream: bool) -> Action {
        // fuel 枯渇を誘発する無限ループ（ホストの fuel/epoch で遮断される想定）
        loop {}
    }
}