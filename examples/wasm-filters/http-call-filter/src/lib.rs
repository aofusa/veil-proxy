//! Proxy-Wasm HTTP コールフィルタ（F-62 Pause/resume の E2E・ベンチ用）
//!
//! `on_http_request_headers` で設定された上流（プラグイン設定、デフォルト
//! "backend-pool"）へ `dispatch_http_call` を発行して `Action::Pause` を返す。
//! ホストがコールを解決して `on_http_call_response` で resume されたら、
//! コール結果のステータスを含むローカルレスポンスを返す。

use proxy_wasm::traits::*;
use proxy_wasm::types::*;
use std::time::Duration;

proxy_wasm::main! {{
    proxy_wasm::set_log_level(LogLevel::Info);
    proxy_wasm::set_root_context(|_| -> Box<dyn RootContext> {
        Box::new(HttpCallFilterRoot { upstream: String::new() })
    });
}}

struct HttpCallFilterRoot {
    upstream: String,
}

impl Context for HttpCallFilterRoot {}

impl RootContext for HttpCallFilterRoot {
    fn on_configure(&mut self, _config_size: usize) -> bool {
        if let Some(config) = self.get_plugin_configuration() {
            self.upstream = String::from_utf8_lossy(&config).trim().to_string();
        }
        true
    }

    fn get_type(&self) -> Option<ContextType> {
        Some(ContextType::HttpContext)
    }

    fn create_http_context(&self, _context_id: u32) -> Option<Box<dyn HttpContext>> {
        let upstream = if self.upstream.is_empty() {
            "backend-pool".to_string()
        } else {
            self.upstream.clone()
        };
        Some(Box::new(HttpCallFilter { upstream }))
    }
}

struct HttpCallFilter {
    upstream: String,
}

impl Context for HttpCallFilter {
    fn on_http_call_response(
        &mut self,
        _token_id: u32,
        _num_headers: usize,
        _body_size: usize,
        _num_trailers: usize,
    ) {
        // コール結果のステータスを取得してローカルレスポンスで返す
        let status = self
            .get_http_call_response_headers()
            .into_iter()
            .find(|(k, _)| k == ":status")
            .map(|(_, v)| v)
            .unwrap_or_else(|| "none".to_string());

        log::info!("http_call completed with status {}", status);

        self.send_http_response(
            200,
            vec![
                ("X-Wasm-Http-Call-Status", status.as_str()),
                ("X-Wasm-Http-Call", "resumed"),
            ],
            Some(b"wasm-http-call-ok"),
        );
    }
}

impl HttpContext for HttpCallFilter {
    fn on_http_request_headers(&mut self, _num_headers: usize, _end_of_stream: bool) -> Action {
        match self.dispatch_http_call(
            &self.upstream,
            vec![
                (":method", "GET"),
                (":path", "/"),
                (":authority", "backend"),
            ],
            None,
            vec![],
            Duration::from_secs(5),
        ) {
            Ok(_token) => Action::Pause,
            Err(e) => {
                log::error!("dispatch_http_call failed: {:?}", e);
                self.send_http_response(
                    500,
                    vec![("X-Wasm-Http-Call", "dispatch-failed")],
                    Some(b"dispatch failed"),
                );
                Action::Continue
            }
        }
    }
}
