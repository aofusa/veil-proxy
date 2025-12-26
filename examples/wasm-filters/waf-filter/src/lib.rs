//! Web Application Firewall (WAF) Proxy-Wasm Filter
//!
//! Protects against common web attacks:
//! - SQL Injection (SQLi)
//! - Cross-Site Scripting (XSS)
//! - Path Traversal
//! - Command Injection
//!
//! Inspired by OWASP ModSecurity Core Rule Set (CRS)

use proxy_wasm::traits::*;
use proxy_wasm::types::*;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;

mod rules;

use rules::{RuleEngine, WafAction, WafConfig};

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
            log::info!("[waf] Using default configuration");
            return true;
        }

        if let Some(config_bytes) = self.get_plugin_configuration() {
            match serde_json::from_slice::<WafConfig>(&config_bytes) {
                Ok(config) => {
                    log::info!("[waf] Configuration loaded: mode={:?}", config.mode);
                    self.config = config;
                    self.engine = RuleEngine::with_config(&self.config);
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
        if self.config.mode == rules::WafMode::Off {
            return Action::Continue;
        }

        // Check whitelist
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
            targets.insert("uri".to_string(), path);
        }

        // Query string (extracted from path)
        if let Some(path) = self.get_http_request_header(":path") {
            if let Some(pos) = path.find('?') {
                targets.insert("query".to_string(), path[pos + 1..].to_string());
            }
        }

        // User-Agent
        if let Some(ua) = self.get_http_request_header("user-agent") {
            targets.insert("user-agent".to_string(), ua);
        }

        // Referer
        if let Some(referer) = self.get_http_request_header("referer") {
            targets.insert("referer".to_string(), referer);
        }

        // Cookie
        if let Some(cookie) = self.get_http_request_header("cookie") {
            targets.insert("cookie".to_string(), cookie);
        }

        // Run rule engine
        let result = self.engine.inspect(&targets);

        if let Some(violation) = result {
            log::warn!(
                "[waf:{}] {} detected: rule={}, target={}, value={}",
                self.context_id,
                violation.category,
                violation.rule_id,
                violation.target,
                &violation.matched_value[..std::cmp::min(50, violation.matched_value.len())]
            );

            // Determine action
            let action = if self.config.mode == rules::WafMode::Detect {
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
                        ],
                        Some(format!("Blocked by WAF: {}", violation.category).as_bytes()),
                    );
                    return Action::Pause;
                }
                WafAction::Log => {
                    // Already logged above, continue
                }
                WafAction::Allow => {
                    // Allow through
                }
            }
        }

        Action::Continue
    }

    fn on_http_request_body(&mut self, body_size: usize, end_of_stream: bool) -> Action {
        // Skip body inspection if disabled or in detect-only mode for performance
        if self.config.mode == rules::WafMode::Off || !self.config.inspect_body {
            return Action::Continue;
        }

        // Only inspect when we have the full body
        if !end_of_stream {
            return Action::Continue;
        }

        // Get body
        if let Some(body) = self.get_http_request_body(0, body_size) {
            if let Ok(body_str) = String::from_utf8(body) {
                let mut targets = HashMap::new();
                targets.insert("body".to_string(), body_str);

                let result = self.engine.inspect(&targets);

                if let Some(violation) = result {
                    log::warn!(
                        "[waf:{}] {} detected in body: rule={}",
                        self.context_id,
                        violation.category,
                        violation.rule_id
                    );

                    if self.config.mode == rules::WafMode::Block
                        && violation.action == WafAction::Block
                    {
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
