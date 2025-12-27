//! CRS Level 2 Rules - Moderate Protection
//!
//! Extended ruleset with balanced protection and false positive rates.
//! Includes Level 1 + additional detection rules.

use crate::rules::{CompiledRule, Severity, WafAction};

/// Get all CRS Level 2 rules (includes Level 1)
pub fn get_rules() -> Vec<CompiledRule> {
    let mut rules = crate::crs_level1::get_rules();
    
    // Additional SQL Injection rules
    rules.extend(get_sqli_rules());
    
    // Additional XSS rules
    rules.extend(get_xss_rules());
    
    // Command Injection
    rules.extend(get_command_injection_rules());
    
    // RFI/LFI rules
    rules.extend(get_rfi_lfi_rules());
    
    // Scanner Detection
    rules.extend(get_scanner_detection_rules());
    
    rules
}

fn get_sqli_rules() -> Vec<CompiledRule> {
    vec![
        CompiledRule::new(
            "crs-942200",
            "SQL Injection - Comment Markers",
            r"(--|#|/\*|\*/)",
            Severity::Medium,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-942210",
            "SQL Injection - Stacked Queries",
            r";\s*(select|insert|update|delete|drop|union|create|alter)\b",
            Severity::High,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-942220",
            "SQL Injection - Time-based Blind",
            r"(?i)(benchmark\s*\(|sleep\s*\(|waitfor\s+delay\s|pg_sleep)",
            Severity::Critical,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-942230",
            "SQL Injection - Hex Encoding",
            r"(?i)(0x[0-9a-f]{8,}|char\s*\(\s*\d+\s*\)|concat\s*\()",
            Severity::High,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-942240",
            "SQL Injection - HAVING/GROUP BY",
            r"(?i)(\bhaving\b\s+\d|group\s+by\s+\d)",
            Severity::Medium,
            WafAction::Block,
        ),
    ]
}

fn get_xss_rules() -> Vec<CompiledRule> {
    vec![
        CompiledRule::new(
            "crs-941200",
            "XSS - SVG Events",
            r"(?i)<\s*svg[^>]*\s+on\w+\s*=",
            Severity::High,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-941210",
            "XSS - Data URI",
            r"(?i)data\s*:\s*(text/html|application/javascript)",
            Severity::High,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-941220",
            "XSS - VBScript",
            r"(?i)vbscript\s*:",
            Severity::High,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-941230",
            "XSS - Expression",
            r"(?i)expression\s*\(",
            Severity::High,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-941240",
            "XSS - Meta/Link Tags",
            r"(?i)<\s*(meta|link)[^>]*(http-equiv|href)\s*=",
            Severity::Medium,
            WafAction::Block,
        ),
    ]
}

fn get_command_injection_rules() -> Vec<CompiledRule> {
    vec![
        CompiledRule::new(
            "crs-932100",
            "Command Injection - Shell Metacharacters",
            r"[;|`$]\s*\w+",
            Severity::High,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-932110",
            "Command Injection - Command Substitution",
            r"\$\([^)]+\)|\$\{[^}]+\}",
            Severity::High,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-932120",
            "Command Injection - Common Commands",
            r"(?i)(^|[;&|])\s*(cat|ls|id|whoami|uname|curl|wget|nc|bash|sh|python|perl|php|ruby)\s",
            Severity::Critical,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-932130",
            "Command Injection - Unix Paths",
            r"(?i)/(bin|usr/bin|sbin)/(sh|bash|cat|ls|curl|wget|nc)",
            Severity::Critical,
            WafAction::Block,
        ),
    ]
}

fn get_rfi_lfi_rules() -> Vec<CompiledRule> {
    vec![
        CompiledRule::new(
            "crs-931100",
            "LFI - PHP Wrapper",
            r"(?i)(php|file|data|expect|input|zip|phar)://",
            Severity::High,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-931110",
            "RFI - Remote URL Injection",
            r"(?i)[?&](file|url|path|src|include)=https?://",
            Severity::Critical,
            WafAction::Block,
        ),
    ]
}

fn get_scanner_detection_rules() -> Vec<CompiledRule> {
    vec![
        CompiledRule::new(
            "crs-913100",
            "Scanner Detection - Nikto",
            r"(?i)nikto|nmap|masscan|dirbuster|gobuster|wfuzz|sqlmap",
            Severity::Medium,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-913110",
            "Scanner Detection - Aggressive Crawler",
            r"(?i)(python-requests|go-http-client|java/).*(bot|crawler|spider)",
            Severity::Low,
            WafAction::Log,
        ),
    ]
}
