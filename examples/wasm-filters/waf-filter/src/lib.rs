//! Web Application Firewall (WAF) Proxy-Wasm Filter
//!
//! Protects against common web attacks with configurable CRS levels:
//! - Level 1: Basic protection (SQLi, XSS, Path Traversal)
//! - Level 2: Moderate protection (+Command Injection, RFI/LFI, Scanner Detection)
//! - Level 3: Strict protection (+Protocol Anomalies, Evasion, Data Leakage)
//!
//! Inspired by OWASP ModSecurity Core Rule Set (CRS)

use proxy_wasm::traits::*;
use proxy_wasm::types::*;
use std::collections::HashMap;

mod crs_level1;
mod crs_level2;
mod crs_level3;
mod rules;

use rules::{RuleEngine, WafAction, WafConfig, WafMode};

proxy_wasm::main! {{
    proxy_wasm::set_log_level(LogLevel::Info);
    proxy_wasm::set_root_context(|_| -> Box<dyn RootContext> {
        Box::new(WafFilterRoot::new())
    });
}}

struct WafFilterRoot {
    config: WafConfig,
    engine: RuleEngine,
}

impl WafFilterRoot {
    fn new() -> Self {
        Self {
            config: WafConfig::default(),
            engine: RuleEngine::new(),
        }
    }
}

impl Context for WafFilterRoot {}

impl RootContext for WafFilterRoot {
    fn on_configure(&mut self, plugin_configuration_size: usize) -> bool {
        if plugin_configuration_size == 0 {
            log::info!("[waf] Using default configuration (CRS Level 2)");
            return true;
        }

        if let Some(config_bytes) = self.get_plugin_configuration() {
            match serde_json::from_slice::<WafConfig>(&config_bytes) {
                Ok(config) => {
                    log::info!(
                        "[waf] Configuration loaded: mode={:?}, crs_level={:?}, anomaly_scoring={}",
                        config.mode,
                        config.crs_level,
                        config.anomaly_scoring
                    );
                    self.engine = RuleEngine::with_config(&config);
                    self.config = config;
                }
                Err(e) => {
                    log::error!("[waf] Failed to parse configuration: {}", e);
                    return false;
                }
            }
        }
        true
    }

    fn get_type(&self) -> Option<ContextType> {
        Some(ContextType::HttpContext)
    }

    fn create_http_context(&self, context_id: u32) -> Option<Box<dyn HttpContext>> {
        Some(Box::new(WafFilter {
            context_id,
            config: self.config.clone(),
            engine: self.engine.clone(),
        }))
    }
}

struct WafFilter {
    context_id: u32,
    config: WafConfig,
    engine: RuleEngine,
}

impl Context for WafFilter {}

impl HttpContext for WafFilter {
    fn on_http_request_headers(&mut self, _num_headers: usize, _end_of_stream: bool) -> Action {
        // Skip if WAF is disabled
        if self.config.mode == WafMode::Off {
            return Action::Continue;
        }

        // Check IP whitelist (優先度: パスホワイトリストより先)
        let client_ip = self.get_http_request_header("x-forwarded-for")
            .or_else(|| self.get_http_request_header("x-real-ip"))
            .or_else(|| self.get_http_request_header("x-client-ip"));
        
        if let Some(ip) = client_ip {
            // X-Forwarded-Forは複数IPを含む可能性がある（最初のIPを使用）
            let first_ip = ip.split(',').next().unwrap_or(&ip).trim();
            if self.config.is_ip_whitelisted(first_ip) {
                log::debug!("[waf:{}] IP whitelisted: {}", self.context_id, first_ip);
                return Action::Continue;
            }
        }

        // Check path whitelist
        if let Some(path) = self.get_http_request_header(":path") {
            if self.config.is_path_whitelisted(&path) {
                log::debug!("[waf:{}] Path whitelisted: {}", self.context_id, path);
                return Action::Continue;
            }
        }

        // Collect targets for inspection
        let mut targets = HashMap::new();

        // URI/Path
        if let Some(path) = self.get_http_request_header(":path") {
            targets.insert("uri".to_string(), path.clone());
            
            // Query string
            if let Some(pos) = path.find('?') {
                targets.insert("query".to_string(), path[pos + 1..].to_string());
            }
        }

        // Headers
        if let Some(ua) = self.get_http_request_header("user-agent") {
            targets.insert("user-agent".to_string(), ua);
        }
        if let Some(referer) = self.get_http_request_header("referer") {
            targets.insert("referer".to_string(), referer);
        }
        if let Some(cookie) = self.get_http_request_header("cookie") {
            targets.insert("cookie".to_string(), cookie);
        }

        // Run rule engine
        if let Some(violation) = self.engine.inspect(&targets) {
            log::warn!(
                "[waf:{}] {} detected: rule={}, severity={:?}, target={}, value={}",
                self.context_id,
                violation.category,
                violation.rule_id,
                violation.severity,
                violation.target,
                &violation.matched_value[..std::cmp::min(50, violation.matched_value.len())]
            );

            let action = if self.config.mode == WafMode::Detect {
                WafAction::Log
            } else {
                violation.action.clone()
            };

            match action {
                WafAction::Block => {
                    self.send_http_response(
                        403,
                        vec![
                            ("content-type", "text/plain"),
                            ("x-waf-block", &violation.rule_id),
                            ("x-waf-category", &violation.category),
                        ],
                        Some(format!("Blocked by WAF: {}", violation.category).as_bytes()),
                    );
                    return Action::Pause;
                }
                WafAction::Log | WafAction::Allow => {}
            }
        }

        Action::Continue
    }

    fn on_http_request_body(&mut self, body_size: usize, end_of_stream: bool) -> Action {
        if self.config.mode == WafMode::Off || !self.config.inspect_body {
            return Action::Continue;
        }

        if !end_of_stream {
            return Action::Continue;
        }

        if let Some(body) = self.get_http_request_body(0, body_size) {
            if let Ok(body_str) = String::from_utf8(body) {
                let mut targets = HashMap::new();
                targets.insert("body".to_string(), body_str);

                if let Some(violation) = self.engine.inspect(&targets) {
                    log::warn!(
                        "[waf:{}] {} detected in body: rule={}",
                        self.context_id,
                        violation.category,
                        violation.rule_id
                    );

                    if self.config.mode == WafMode::Block && violation.action == WafAction::Block {
                        self.send_http_response(
                            403,
                            vec![
                                ("content-type", "text/plain"),
                                ("x-waf-block", &violation.rule_id),
                            ],
                            Some(format!("Blocked by WAF: {}", violation.category).as_bytes()),
                        );
                        return Action::Pause;
                    }
                }
            }
        }

        Action::Continue
    }

    fn on_log(&mut self) {
        log::debug!("[waf:{}] Request completed", self.context_id);
    }
}
