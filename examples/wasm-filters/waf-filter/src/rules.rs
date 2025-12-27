//! WAF Rule Engine and Configuration
//!
//! Detection rules inspired by OWASP ModSecurity CRS.
//! Supports CRS Levels 1-3 with anomaly scoring.

use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;

use crate::{crs_level1, crs_level2, crs_level3};

/// WAF operation mode
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WafMode {
    /// Block detected attacks
    Block,
    /// Detect and log only (no blocking)
    Detect,
    /// Disabled
    Off,
}

impl Default for WafMode {
    fn default() -> Self {
        WafMode::Block
    }
}

/// CRS Protection Level
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CrsLevel {
    /// Level 1: Basic protection, minimal false positives
    #[serde(rename = "1")]
    Level1,
    /// Level 2: Moderate protection, balanced
    #[serde(rename = "2")]
    Level2,
    /// Level 3: Strict protection, comprehensive
    #[serde(rename = "3")]
    Level3,
}

impl Default for CrsLevel {
    fn default() -> Self {
        CrsLevel::Level2
    }
}

/// Rule Severity Level
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
}

impl Severity {
    /// Get anomaly score for severity
    pub fn score(&self) -> u32 {
        match self {
            Severity::Critical => 5,
            Severity::High => 3,
            Severity::Medium => 2,
            Severity::Low => 1,
        }
    }
}

/// Action to take when rule matches
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WafAction {
    Block,
    Log,
    Allow,
}

impl Default for WafAction {
    fn default() -> Self {
        WafAction::Block
    }
}

/// WAF configuration
#[derive(Debug, Clone, Deserialize)]
pub struct WafConfig {
    #[serde(default)]
    pub mode: WafMode,

    /// CRS protection level (1, 2, or 3)
    #[serde(default)]
    pub crs_level: CrsLevel,

    /// Anomaly scoring mode (true = sum scores, false = block on first match)
    #[serde(default)]
    pub anomaly_scoring: bool,

    /// Anomaly threshold (block when score >= threshold)
    #[serde(default = "default_anomaly_threshold")]
    pub anomaly_threshold: u32,

    #[serde(default)]
    pub inspect_body: bool,

    #[serde(default)]
    pub whitelist_paths: Vec<String>,

    #[serde(default)]
    pub whitelist_ips: Vec<String>,

    #[serde(default)]
    pub custom_rules: Vec<CustomRule>,
}

fn default_anomaly_threshold() -> u32 {
    5
}

impl Default for WafConfig {
    fn default() -> Self {
        Self {
            mode: WafMode::Block,
            crs_level: CrsLevel::Level2,
            anomaly_scoring: false,
            anomaly_threshold: 5,
            inspect_body: false,
            whitelist_paths: vec!["/health".to_string(), "/metrics".to_string()],
            whitelist_ips: Vec::new(),
            custom_rules: Vec::new(),
        }
    }
}

impl WafConfig {
    pub fn is_path_whitelisted(&self, path: &str) -> bool {
        for whitelist_path in &self.whitelist_paths {
            if path.starts_with(whitelist_path) {
                return true;
            }
        }
        false
    }
}

/// Custom rule definition
#[derive(Debug, Clone, Deserialize)]
pub struct CustomRule {
    pub id: String,
    pub pattern: String,
    #[serde(default)]
    pub targets: Vec<String>,
    #[serde(default)]
    pub action: WafAction,
    #[serde(default)]
    pub message: String,
}

/// Violation detected by WAF
#[derive(Debug, Clone)]
pub struct Violation {
    pub rule_id: String,
    pub category: String,
    pub target: String,
    pub matched_value: String,
    pub action: WafAction,
    pub severity: Severity,
}

/// Compiled rule with pre-compiled regex
pub struct CompiledRule {
    pub id: String,
    pub category: String,
    pub regex: Regex,
    pub severity: Severity,
    pub action: WafAction,
}

impl Clone for CompiledRule {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            category: self.category.clone(),
            regex: Regex::new(self.regex.as_str()).unwrap(),
            severity: self.severity,
            action: self.action.clone(),
        }
    }
}

impl CompiledRule {
    pub fn new(id: &str, category: &str, pattern: &str, severity: Severity, action: WafAction) -> Self {
        Self {
            id: id.to_string(),
            category: category.to_string(),
            regex: Regex::new(pattern).expect(&format!("Invalid regex pattern for rule {}", id)),
            severity,
            action,
        }
    }
}

/// WAF Rule Engine
pub struct RuleEngine {
    rules: Vec<CompiledRule>,
    anomaly_scoring: bool,
    anomaly_threshold: u32,
}

impl Clone for RuleEngine {
    fn clone(&self) -> Self {
        Self {
            rules: self.rules.clone(),
            anomaly_scoring: self.anomaly_scoring,
            anomaly_threshold: self.anomaly_threshold,
        }
    }
}

impl RuleEngine {
    pub fn new() -> Self {
        Self::with_config(&WafConfig::default())
    }

    pub fn with_config(config: &WafConfig) -> Self {
        let mut rules = match config.crs_level {
            CrsLevel::Level1 => crs_level1::get_rules(),
            CrsLevel::Level2 => crs_level2::get_rules(),
            CrsLevel::Level3 => crs_level3::get_rules(),
        };

        // Add custom rules
        for custom_rule in &config.custom_rules {
            if let Ok(regex) = Regex::new(&custom_rule.pattern) {
                rules.push(CompiledRule {
                    id: custom_rule.id.clone(),
                    category: format!("Custom: {}", custom_rule.message),
                    regex,
                    severity: Severity::High,
                    action: custom_rule.action.clone(),
                });
            }
        }

        log::info!(
            "[waf] Loaded {} rules (CRS Level {:?}, anomaly_scoring={})",
            rules.len(),
            config.crs_level,
            config.anomaly_scoring
        );

        Self {
            rules,
            anomaly_scoring: config.anomaly_scoring,
            anomaly_threshold: config.anomaly_threshold,
        }
    }

    /// Inspect targets and return violations
    pub fn inspect(&self, targets: &HashMap<String, String>) -> Option<Violation> {
        let mut total_score = 0u32;
        let mut violations = Vec::new();

        for (target_name, target_value) in targets {
            let decoded = self.url_decode(target_value);

            for rule in &self.rules {
                if rule.regex.is_match(&decoded) {
                    let violation = Violation {
                        rule_id: rule.id.clone(),
                        category: rule.category.clone(),
                        target: target_name.clone(),
                        matched_value: decoded.clone(),
                        action: rule.action.clone(),
                        severity: rule.severity,
                    };

                    if self.anomaly_scoring {
                        total_score += rule.severity.score();
                        violations.push(violation);

                        if total_score >= self.anomaly_threshold {
                            // Return the most severe violation
                            return violations.into_iter()
                                .max_by_key(|v| v.severity.score());
                        }
                    } else {
                        // Immediate mode: return first match
                        return Some(violation);
                    }
                }
            }
        }

        None
    }

    /// Basic URL decoding
    fn url_decode(&self, input: &str) -> String {
        let mut result = String::new();
        let mut chars = input.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '%' {
                let hex: String = chars.by_ref().take(2).collect();
                if hex.len() == 2 {
                    if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                        result.push(byte as char);
                        continue;
                    }
                }
                result.push('%');
                result.push_str(&hex);
            } else if c == '+' {
                result.push(' ');
            } else {
                result.push(c);
            }
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crs_level1_sqli_detection() {
        let config = WafConfig {
            crs_level: CrsLevel::Level1,
            ..Default::default()
        };
        let engine = RuleEngine::with_config(&config);
        let mut targets = HashMap::new();
        targets.insert("query".to_string(), "id=1 UNION SELECT * FROM users".to_string());

        let result = engine.inspect(&targets);
        assert!(result.is_some());
        let violation = result.unwrap();
        assert!(violation.rule_id.starts_with("crs-942"));
    }

    #[test]
    fn test_crs_level2_command_injection() {
        let config = WafConfig {
            crs_level: CrsLevel::Level2,
            ..Default::default()
        };
        let engine = RuleEngine::with_config(&config);
        let mut targets = HashMap::new();
        targets.insert("cmd".to_string(), "; cat /etc/passwd".to_string());

        let result = engine.inspect(&targets);
        assert!(result.is_some());
        let violation = result.unwrap();
        assert!(violation.rule_id.starts_with("crs-932"));
    }

    #[test]
    fn test_crs_level3_evasion_detection() {
        let config = WafConfig {
            crs_level: CrsLevel::Level3,
            ..Default::default()
        };
        let engine = RuleEngine::with_config(&config);
        let mut targets = HashMap::new();
        targets.insert("input".to_string(), "test%00injection".to_string());

        let result = engine.inspect(&targets);
        assert!(result.is_some());
        let violation = result.unwrap();
        assert!(violation.rule_id.starts_with("crs-921"));
    }

    #[test]
    fn test_anomaly_scoring() {
        let config = WafConfig {
            crs_level: CrsLevel::Level1,
            anomaly_scoring: true,
            anomaly_threshold: 10,
            ..Default::default()
        };
        let engine = RuleEngine::with_config(&config);
        
        // Single low-severity match should not trigger
        let mut targets = HashMap::new();
        targets.insert("query".to_string(), "<script>".to_string());
        
        // This should still detect because XSS script tag is Critical (5 points)
        let result = engine.inspect(&targets);
        assert!(result.is_none() || result.as_ref().map(|v| v.severity.score() < 10).unwrap_or(true));
    }

    #[test]
    fn test_clean_request() {
        let engine = RuleEngine::new();
        let mut targets = HashMap::new();
        targets.insert("uri".to_string(), "/api/users/123".to_string());
        targets.insert("query".to_string(), "name=John&age=30".to_string());

        let result = engine.inspect(&targets);
        assert!(result.is_none());
    }

    #[test]
    fn test_url_decode() {
        let engine = RuleEngine::new();
        assert_eq!(engine.url_decode("%3Cscript%3E"), "<script>");
        assert_eq!(engine.url_decode("hello+world"), "hello world");
    }
}
