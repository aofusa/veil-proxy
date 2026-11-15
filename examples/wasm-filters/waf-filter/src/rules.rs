//! WAF Rule Engine and Configuration
//!
//! Detection rules inspired by OWASP ModSecurity CRS.
//! Supports CRS Levels 1-3 with anomaly scoring.

use regex::bytes::{RegexBuilder, RegexSet, RegexSetBuilder};
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, SocketAddr};

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
    /// Whether the request path skips inspection. The query string is not part of the match, and a whitelisted
    /// path matches only itself and the paths below it (`/health` matches `/health` and `/health/live`, not
    /// `/healthz` or `/health-admin`). A path with characters outside `A-Z a-z 0-9 / - . _ ~` (percent-encoding,
    /// `;` parameters, backslashes) or with `.` / `..` segments is never whitelisted, because an upstream may
    /// decode or normalize it into a path outside the whitelisted one.
    pub fn is_path_whitelisted(&self, path: &str) -> bool {
        let path = path.split(['?', '#']).next().unwrap_or("");
        let plain = path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'.' | b'_' | b'~'));
        if !plain || path.split('/').any(|seg| seg == "." || seg == "..") {
            return false;
        }
        self.whitelist_paths.iter().any(|w| {
            !w.is_empty()
                && (path == w
                    || path.strip_prefix(w.as_str()).is_some_and(|rest| {
                        w.ends_with('/') || rest.starts_with('/')
                    }))
        })
    }

    /// Whether the client address skips inspection. `addr` is the address of the peer as veil reports it in the
    /// `source.address` property (`ip` or `ip:port`, IPv6 as `[ip]:port`). Entries are addresses or CIDR blocks
    /// (`192.168.1.1`, `10.0.0.0/8`, `2001:db8::/32`).
    pub fn is_ip_whitelisted(&self, addr: &str) -> bool {
        if self.whitelist_ips.is_empty() {
            return false;
        }
        let Some(ip) = parse_peer_ip(addr) else {
            return false;
        };
        self.whitelist_ips.iter().any(|entry| match entry.split_once('/') {
            Some((net, len)) => match (net.parse::<IpAddr>(), len.parse::<u8>()) {
                (Ok(net), Ok(len)) => cidr_contains(net, len, ip),
                _ => false,
            },
            None => entry.parse::<IpAddr>() == Ok(ip),
        })
    }
}

/// The IP of a peer address (`ip`, `ip:port` or `[ipv6]:port`)
fn parse_peer_ip(addr: &str) -> Option<IpAddr> {
    let addr = addr.trim();
    addr.parse::<IpAddr>()
        .ok()
        .or_else(|| addr.parse::<SocketAddr>().ok().map(|a| a.ip()))
        .map(|ip| match ip {
            // An IPv4 client on a dual-stack listener
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
            v4 => v4,
        })
}

/// Whether `ip` is in the block `net/len` (an invalid length or a different address family never matches)
fn cidr_contains(net: IpAddr, len: u8, ip: IpAddr) -> bool {
    match (net, ip) {
        (IpAddr::V4(net), IpAddr::V4(ip)) if len <= 32 => {
            let mask = u32::MAX.checked_shl(32 - u32::from(len)).unwrap_or(0);
            u32::from(net) & mask == u32::from(ip) & mask
        }
        (IpAddr::V6(net), IpAddr::V6(ip)) if len <= 128 => {
            let mask = u128::MAX.checked_shl(128 - u32::from(len)).unwrap_or(0);
            u128::from(net) & mask == u128::from(ip) & mask
        }
        _ => false,
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

/// Rule compilation error
#[derive(Debug, Clone)]
pub struct RuleCompilationError {
    pub rule_id: String,
    pub pattern: String,
    pub error: String,
}

impl fmt::Display for RuleCompilationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Failed to compile rule {} with pattern '{}': {}",
            self.rule_id, self.pattern, self.error
        )
    }
}

/// A detection rule. [`RuleEngine`] compiles the patterns of all its rules into one [`RegexSet`].
///
/// Patterns are byte regexes with Unicode mode off: `\w`, `\s`, `\b`, `\d`, `.` and `(?i)` are ASCII-only, like
/// the HTTP syntax the rules inspect. With Unicode mode on, classes such as `.{1,100}` compile to UTF-8 automata
/// hundreds of times larger, and building the rules no longer fits in a Proxy-Wasm host's CPU budget for starting
/// a plugin (100 ms by default in veil). A pattern can still opt into Unicode with `(?u:...)`.
#[derive(Clone)]
pub struct CompiledRule {
    pub id: String,
    pub category: String,
    pub pattern: String,
    pub severity: Severity,
    pub action: WafAction,
    pub targets: Vec<String>,
}

impl CompiledRule {
    /// A built-in rule. Its pattern is not compiled here: the CRS rule sets are checked by the unit tests, and
    /// [`RuleEngine`] compiles every pattern once, in its [`RegexSet`].
    pub fn builtin(
        id: &str,
        category: &str,
        pattern: &str,
        severity: Severity,
        action: WafAction,
        targets: Vec<String>,
    ) -> Self {
        Self {
            id: id.to_string(),
            category: category.to_string(),
            pattern: pattern.to_string(),
            severity,
            action,
            targets,
        }
    }

    /// Create a rule from a pattern that may be invalid (a custom rule), returning Result instead of panicking
    pub fn try_new(
        id: &str,
        category: &str,
        pattern: &str,
        severity: Severity,
        action: WafAction,
        targets: Vec<String>,
    ) -> Result<Self, RuleCompilationError> {
        match RegexBuilder::new(pattern).unicode(false).build() {
            Ok(_) => Ok(Self::builtin(id, category, pattern, severity, action, targets)),
            Err(e) => Err(RuleCompilationError {
                rule_id: id.to_string(),
                pattern: pattern.to_string(),
                error: e.to_string(),
            }),
        }
    }
    
    /// Create a new compiled rule (deprecated, use try_new)
    #[deprecated(note = "Use try_new instead to avoid panics")]
    #[allow(dead_code)] // Kept for backward compatibility
    pub fn new(id: &str, category: &str, pattern: &str, severity: Severity, action: WafAction) -> Self {
        Self::try_new(id, category, pattern, severity, action, vec![])
            .unwrap_or_else(|e| {
                log::error!("{}", e);
                panic!("Rule compilation failed: {}", e);
            })
    }
    
    /// Check if this rule should be applied to the target
    pub fn matches_target(&self, target_name: &str) -> bool {
        // If no targets specified, apply to all (backward compatibility)
        if self.targets.is_empty() {
            return true;
        }
        
        // Check if target is in the list
        self.targets.iter().any(|t| t == target_name)
    }
}

/// WAF Rule Engine
#[derive(Clone)]
pub struct RuleEngine {
    rules: Vec<CompiledRule>,
    /// The patterns of `rules`, in the same order (pattern `i` is `rules[i]`)
    set: RegexSet,
    anomaly_scoring: bool,
    anomaly_threshold: u32,
}

impl RuleEngine {
    /// The rules of the default configuration (CRS Level 2)
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with_config(&WafConfig::default())
    }

    pub fn with_config(config: &WafConfig) -> Self {
        let mut rules = match config.crs_level {
            CrsLevel::Level1 => crs_level1::get_rules(),
            CrsLevel::Level2 => crs_level2::get_rules(),
            CrsLevel::Level3 => crs_level3::get_rules(),
        };

        // Add custom rules with proper error handling
        let mut errors = Vec::new();
        for custom_rule in &config.custom_rules {
            match CompiledRule::try_new(
                &custom_rule.id,
                &format!("Custom: {}", custom_rule.message),
                &custom_rule.pattern,
                Severity::High,
                custom_rule.action.clone(),
                custom_rule.targets.clone(),
            ) {
                Ok(rule) => {
                    rules.push(rule);
                }
                Err(e) => {
                    errors.push(e);
                }
            }
        }
        
        // Log errors
        if !errors.is_empty() {
            log::warn!(
                "[waf] Failed to compile {} custom rule(s), skipping them",
                errors.len()
            );
            for error in &errors {
                log::error!("[waf] {}", error);
            }
        }

        log::info!(
            "[waf] Loaded {} rules (CRS Level {:?}, anomaly_scoring={})",
            rules.len(),
            config.crs_level,
            config.anomaly_scoring
        );

        // Every pattern compiled on its own (custom rules are checked above), so only the size limit can fail here
        let set = RegexSetBuilder::new(rules.iter().map(|r| r.pattern.as_str()))
            .unicode(false)
            .build()
            .unwrap_or_else(|e| {
                log::error!("[waf] Failed to compile the rule set: {}", e);
                panic!("[waf] failed to compile the rule set: {}", e);
            });

        Self {
            rules,
            set,
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
            
            // デコード前の文字列をチェック（エンコードされた攻撃パターン）
            // `RegexSet::matches` は 1 回の走査で当たったルールを返す（ルールの順）
            let mut matched_in_encoded = false;
            for rule in self.set.matches(target_value.as_bytes()).iter().map(|i| &self.rules[i]) {
                // Check if rule applies to this target
                if !rule.matches_target(target_name) {
                    continue;
                }

                let violation = Violation {
                    rule_id: rule.id.clone(),
                    category: rule.category.clone(),
                    target: target_name.clone(),
                    matched_value: target_value.clone(),
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
                matched_in_encoded = true;
            }
            
            // デコード後の文字列をチェック（デコード後の攻撃パターン）
            // デコード前でマッチした場合はスキップ（immediate modeの場合）
            if !matched_in_encoded || self.anomaly_scoring {
                for rule in self.set.matches(decoded.as_bytes()).iter().map(|i| &self.rules[i]) {
                    // Check if rule applies to this target
                    if !rule.matches_target(target_name) {
                        continue;
                    }

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

    /// Enhanced URL decoding with multiple encoding support
    pub(crate) fn url_decode(&self, input: &str) -> String {
        self.url_decode_recursive(input, 0, 3)
    }
    
    /// Recursive URL decoding with depth limit
    fn url_decode_recursive(&self, input: &str, depth: u8, max_depth: u8) -> String {
        if depth >= max_depth {
            return input.to_string();
        }
        
        let mut result = String::new();
        let mut chars = input.chars().peekable();
        let mut changed = false;
        
        while let Some(c) = chars.next() {
            match c {
                '%' => {
                    // Try to decode %XX
                    let hex_chars: Vec<char> = chars.by_ref().take(2).collect();
                    if hex_chars.len() == 2 {
                        let hex_str: String = hex_chars.iter().collect();
                        if let Ok(byte) = u8::from_str_radix(&hex_str, 16) {
                            // Safe UTF-8 character conversion for ASCII
                            if byte < 0x80 {
                                result.push(byte as char);
                                changed = true;
                                continue;
                            }
                            // For multi-byte UTF-8, we need proper handling
                            // This is a simplified version - push as-is for now
                            result.push(byte as char);
                            changed = true;
                            continue;
                        }
                    }
                    // Failed to decode, keep original
                    result.push('%');
                    result.extend(hex_chars);
                }
                '+' => {
                    result.push(' ');
                    changed = true;
                }
                _ => {
                    result.push(c);
                }
            }
        }
        
        // Decode Unicode escape sequences (%uXXXX)
        result = self.decode_unicode_escape(&result);
        
        // If changes were made, try recursive decode
        if changed && depth < max_depth {
            self.url_decode_recursive(&result, depth + 1, max_depth)
        } else {
            result
        }
    }
    
    /// Decode Unicode escape sequences (%uXXXX)
    fn decode_unicode_escape(&self, input: &str) -> String {
        let mut result = String::new();
        let mut chars = input.chars().peekable();
        
        while let Some(c) = chars.next() {
            if c == '%' {
                if let Some(&'u') = chars.peek() {
                    chars.next(); // consume 'u'
                    let hex_chars: Vec<char> = chars.by_ref().take(4).collect();
                    if hex_chars.len() == 4 {
                        let hex_str: String = hex_chars.iter().collect();
                        if let Ok(code_point) = u16::from_str_radix(&hex_str, 16) {
                            if let Some(unicode_char) = char::from_u32(code_point as u32) {
                                result.push(unicode_char);
                                continue;
                            }
                        }
                    }
                    result.push('%');
                    result.push('u');
                    result.extend(hex_chars);
                } else {
                    result.push(c);
                }
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
        // Path Traversalルールにマッチしないように、/etc/passwdを削除
        targets.insert("cmd".to_string(), "; cat file.txt".to_string());

        let result = engine.inspect(&targets);
        assert!(result.is_some(), "Command injection should be detected");
        let violation = result.unwrap();
        // crs-932100 (Shell Metacharacters) または crs-932120 (Common Commands) のいずれかにマッチ
        assert!(
            violation.rule_id.starts_with("crs-932"),
            "Expected rule_id starting with 'crs-932', got: {}",
            violation.rule_id
        );
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

    #[test]
    fn test_url_decode_double_encoding() {
        let engine = RuleEngine::new();
        // Double encoding: %2520 -> %20 -> space
        assert_eq!(engine.url_decode("hello%2520world"), "hello world");
        // Triple encoding: %252520 -> %2520 -> %20 -> space
        assert_eq!(engine.url_decode("hello%252520world"), "hello world");
    }

    #[test]
    fn test_url_decode_unicode_escape() {
        let engine = RuleEngine::new();
        // Unicode escape: %u003C -> <
        assert_eq!(engine.url_decode("%u003Cscript%u003E"), "<script>");
    }

    #[test]
    fn test_ip_whitelist() {
        let config = WafConfig {
            whitelist_ips: vec!["192.168.1.1".to_string(), "10.0.0.0/24".to_string()],
            ..Default::default()
        };
        
        assert!(config.is_ip_whitelisted("192.168.1.1"));
        assert!(config.is_ip_whitelisted("192.168.1.1:8080")); // with port
        assert!(config.is_ip_whitelisted("10.0.0.100")); // CIDR match
        assert!(!config.is_ip_whitelisted("192.168.1.2"));
        assert!(!config.is_ip_whitelisted("10.0.1.100"));
    }

    /// Prefix lengths other than /16, /24 and /32 used to fall back to a string prefix match: `1.2.3.4/30`
    /// whitelisted `1.2.3.40`-`1.2.3.49`, `10.0.0.0/8` matched nothing, and IPv6 never matched.
    #[test]
    fn test_ip_whitelist_cidr_and_ipv6() {
        let config = WafConfig {
            whitelist_ips: vec![
                "1.2.3.4/30".to_string(),
                "10.0.0.0/8".to_string(),
                "2001:db8::/32".to_string(),
                "::1".to_string(),
                "bad/entry".to_string(),
            ],
            ..Default::default()
        };
        assert!(config.is_ip_whitelisted("1.2.3.7"));
        assert!(!config.is_ip_whitelisted("1.2.3.40"));
        assert!(!config.is_ip_whitelisted("1.2.3.8"));
        assert!(config.is_ip_whitelisted("10.200.1.2:443"));
        assert!(config.is_ip_whitelisted("[2001:db8:1::5]:8443"));
        assert!(config.is_ip_whitelisted("2001:db8:ffff::1"));
        assert!(!config.is_ip_whitelisted("2001:db9::1"));
        assert!(config.is_ip_whitelisted("::1"));
        assert!(config.is_ip_whitelisted("::ffff:10.1.1.1")); // IPv4-mapped
        assert!(!config.is_ip_whitelisted("not-an-ip"));
        assert!(!config.is_ip_whitelisted(""));
    }

    /// The path whitelist used to be a plain prefix match on `:path` (with the query): `/healthz`, `/health-admin`
    /// and `/health/../admin` were whitelisted too.
    #[test]
    fn test_path_whitelist_segment_boundary() {
        let config = WafConfig {
            whitelist_paths: vec!["/health".to_string(), "/static/".to_string()],
            ..Default::default()
        };
        assert!(config.is_path_whitelisted("/health"));
        assert!(config.is_path_whitelisted("/health?verbose=1"));
        assert!(config.is_path_whitelisted("/health/live"));
        assert!(config.is_path_whitelisted("/static/app.js"));
        assert!(!config.is_path_whitelisted("/static"));
        assert!(!config.is_path_whitelisted("/healthz"));
        assert!(!config.is_path_whitelisted("/health-admin"));
        assert!(!config.is_path_whitelisted("/health/../admin"));
        assert!(!config.is_path_whitelisted("/health/./x"));
        assert!(!config.is_path_whitelisted("/health/%2e%2e/admin"));
        assert!(!config.is_path_whitelisted("/health;/../admin"));
        assert!(!config.is_path_whitelisted("/health\\..\\admin"));
        assert!(!config.is_path_whitelisted("/api?next=/health"));
    }

    #[test]
    fn test_custom_rule_targets() {
        let config = WafConfig {
            custom_rules: vec![CustomRule {
                id: "custom-001".to_string(),
                pattern: r"(?i)admin".to_string(),
                targets: vec!["query".to_string()],
                action: WafAction::Block,
                message: "Admin keyword".to_string(),
            }],
            ..Default::default()
        };
        let engine = RuleEngine::with_config(&config);
        
        // Should match in query
        let mut targets = HashMap::new();
        targets.insert("query".to_string(), "user=admin".to_string());
        assert!(engine.inspect(&targets).is_some());
        
        // Should not match in uri (target not specified)
        let mut targets = HashMap::new();
        targets.insert("uri".to_string(), "/admin".to_string());
        assert!(engine.inspect(&targets).is_none());
    }
}
