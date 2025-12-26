//! WAF Rule Engine and Configuration
//!
//! Detection rules inspired by OWASP ModSecurity CRS

use lazy_static::lazy_static;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;

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

    #[serde(default = "default_true")]
    pub xss_enabled: bool,

    #[serde(default = "default_true")]
    pub sqli_enabled: bool,

    #[serde(default = "default_true")]
    pub path_traversal_enabled: bool,

    #[serde(default = "default_true")]
    pub command_injection_enabled: bool,

    #[serde(default)]
    pub inspect_body: bool,

    #[serde(default)]
    pub whitelist_paths: Vec<String>,

    #[serde(default)]
    pub whitelist_ips: Vec<String>,

    #[serde(default)]
    pub custom_rules: Vec<CustomRule>,
}

fn default_true() -> bool {
    true
}

impl Default for WafConfig {
    fn default() -> Self {
        Self {
            mode: WafMode::Block,
            xss_enabled: true,
            sqli_enabled: true,
            path_traversal_enabled: true,
            command_injection_enabled: true,
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
}

/// Rule for detection
struct Rule {
    id: String,
    category: String,
    regex: Regex,
    action: WafAction,
}

/// WAF Rule Engine
#[derive(Clone)]
pub struct RuleEngine {
    rules: Vec<CompiledRule>,
}

#[derive(Clone)]
struct CompiledRule {
    id: String,
    category: String,
    pattern: String, // Store pattern for Clone
    action: WafAction,
}

// Pre-compiled regex patterns
lazy_static! {
    // SQL Injection patterns (OWASP CRS inspired)
    static ref SQLI_PATTERNS: Vec<(&'static str, &'static str)> = vec![
        ("sqli-001", r"(?i)(\bunion\b.*\bselect\b|\bselect\b.*\bfrom\b.*\bwhere\b)"),
        ("sqli-002", r"(?i)(\binsert\b.*\binto\b|\bdelete\b.*\bfrom\b|\bdrop\b.*\b(table|database)\b)"),
        ("sqli-003", r"(?i)(\bupdate\b.*\bset\b.*=)"),
        ("sqli-004", r#"(?i)(('|")\s*(or|and)\s*('|")?\s*(=|1|true))"#),
        ("sqli-005", r"(--|#|/\*|\*/|;)"),
        ("sqli-006", r"(?i)(benchmark\s*\(|sleep\s*\(|waitfor\s+delay)"),
        ("sqli-007", r"(?i)(concat\s*\(|char\s*\(|0x[0-9a-f]+)"),
    ];

    // XSS patterns (OWASP CRS inspired)
    static ref XSS_PATTERNS: Vec<(&'static str, &'static str)> = vec![
        ("xss-001", r"(?i)(<script|</script>)"),
        ("xss-002", r"(?i)(javascript\s*:|vbscript\s*:)"),
        ("xss-003", r"(?i)(\bon\w+\s*=)"),
        ("xss-004", r"(?i)(<iframe|<frame|<embed|<object)"),
        ("xss-005", r"(?i)(<svg.*?on\w+\s*=)"),
        ("xss-006", r"(?i)(expression\s*\()"),
        ("xss-007", r"(?i)(data\s*:\s*text/html)"),
        ("xss-008", r"(?i)(<img.*?onerror\s*=)"),
    ];

    // Path Traversal patterns
    static ref PATH_TRAVERSAL_PATTERNS: Vec<(&'static str, &'static str)> = vec![
        ("traversal-001", r"(\.\.(/|\\|%2f|%5c))"),
        ("traversal-002", r"(?i)(%2e%2e(%2f|%5c))"),
        ("traversal-003", r"(?i)(\.\.%c0%af|\.\.%c1%9c)"),
        ("traversal-004", r"(/etc/passwd|/etc/shadow|/etc/hosts)"),
        ("traversal-005", r"(?i)(c:\\windows|c:\\boot\.ini)"),
    ];

    // Command Injection patterns
    static ref COMMAND_INJECTION_PATTERNS: Vec<(&'static str, &'static str)> = vec![
        ("cmdi-001", r"(;|\||\||`|&&|\|\|)\s*\w+"),
        ("cmdi-002", r"\$\([^)]+\)"),
        ("cmdi-003", r"(?i)(^|;)\s*(cat|ls|pwd|id|whoami|uname|curl|wget|nc|bash|sh|python|perl|ruby|php)\b"),
        ("cmdi-004", r"(?i)/bin/(sh|bash|cat|ls|curl|wget)"),
    ];
}

impl RuleEngine {
    pub fn new() -> Self {
        Self::with_config(&WafConfig::default())
    }

    pub fn with_config(config: &WafConfig) -> Self {
        let mut rules = Vec::new();

        // Add SQL Injection rules
        if config.sqli_enabled {
            for (id, pattern) in SQLI_PATTERNS.iter() {
                rules.push(CompiledRule {
                    id: id.to_string(),
                    category: "SQL Injection".to_string(),
                    pattern: pattern.to_string(),
                    action: WafAction::Block,
                });
            }
        }

        // Add XSS rules
        if config.xss_enabled {
            for (id, pattern) in XSS_PATTERNS.iter() {
                rules.push(CompiledRule {
                    id: id.to_string(),
                    category: "Cross-Site Scripting (XSS)".to_string(),
                    pattern: pattern.to_string(),
                    action: WafAction::Block,
                });
            }
        }

        // Add Path Traversal rules
        if config.path_traversal_enabled {
            for (id, pattern) in PATH_TRAVERSAL_PATTERNS.iter() {
                rules.push(CompiledRule {
                    id: id.to_string(),
                    category: "Path Traversal".to_string(),
                    pattern: pattern.to_string(),
                    action: WafAction::Block,
                });
            }
        }

        // Add Command Injection rules
        if config.command_injection_enabled {
            for (id, pattern) in COMMAND_INJECTION_PATTERNS.iter() {
                rules.push(CompiledRule {
                    id: id.to_string(),
                    category: "Command Injection".to_string(),
                    pattern: pattern.to_string(),
                    action: WafAction::Block,
                });
            }
        }

        // Add custom rules
        for custom_rule in &config.custom_rules {
            rules.push(CompiledRule {
                id: custom_rule.id.clone(),
                category: format!("Custom: {}", custom_rule.message),
                pattern: custom_rule.pattern.clone(),
                action: custom_rule.action.clone(),
            });
        }

        Self { rules }
    }

    /// Inspect targets and return first violation found
    pub fn inspect(&self, targets: &HashMap<String, String>) -> Option<Violation> {
        for (target_name, target_value) in targets {
            // URL decode for inspection
            let decoded = self.url_decode(target_value);

            for rule in &self.rules {
                // Compile regex on demand (could be optimized with caching)
                if let Ok(regex) = Regex::new(&rule.pattern) {
                    if regex.is_match(&decoded) {
                        return Some(Violation {
                            rule_id: rule.id.clone(),
                            category: rule.category.clone(),
                            target: target_name.clone(),
                            matched_value: decoded.clone(),
                            action: rule.action.clone(),
                        });
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
    fn test_sqli_detection() {
        let engine = RuleEngine::new();
        let mut targets = HashMap::new();
        targets.insert("query".to_string(), "id=1 UNION SELECT * FROM users".to_string());

        let result = engine.inspect(&targets);
        assert!(result.is_some());
        assert!(result.unwrap().category.contains("SQL Injection"));
    }

    #[test]
    fn test_xss_detection() {
        let engine = RuleEngine::new();
        let mut targets = HashMap::new();
        targets.insert("query".to_string(), "<script>alert('xss')</script>".to_string());

        let result = engine.inspect(&targets);
        assert!(result.is_some());
        assert!(result.unwrap().category.contains("XSS"));
    }

    #[test]
    fn test_path_traversal_detection() {
        let engine = RuleEngine::new();
        let mut targets = HashMap::new();
        targets.insert("uri".to_string(), "/files/../../../etc/passwd".to_string());

        let result = engine.inspect(&targets);
        assert!(result.is_some());
        assert!(result.unwrap().category.contains("Path Traversal"));
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
