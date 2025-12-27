//! CRS Level 3 Rules - Strict Protection
//!
//! Maximum protection with comprehensive ruleset.
//! Higher false positive rate, suitable for security-critical applications.
//! Includes Level 1 + Level 2 + additional strict rules.

use crate::rules::{CompiledRule, Severity, WafAction};

/// Get all CRS Level 3 rules (includes Level 1 + Level 2)
pub fn get_rules() -> Vec<CompiledRule> {
    let mut rules = crate::crs_level2::get_rules();
    
    // Protocol Anomalies
    rules.extend(get_protocol_anomaly_rules());
    
    // Advanced Evasion Detection
    rules.extend(get_evasion_rules());
    
    // Data Leakage Detection
    rules.extend(get_data_leakage_rules());
    
    // Additional Strict XSS
    rules.extend(get_strict_xss_rules());
    
    // Additional Strict SQLi
    rules.extend(get_strict_sqli_rules());
    
    rules
}

fn get_protocol_anomaly_rules() -> Vec<CompiledRule> {
    vec![
        CompiledRule::new(
            "crs-920100",
            "Protocol Anomaly - Invalid HTTP Version",
            r"(?i)HTTP/[0-3]\.[0-9]{2,}",
            Severity::High,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-920110",
            "Protocol Anomaly - Missing Host Header",
            r"(?i)^(GET|POST|PUT|DELETE|PATCH)\s+/[^\s]*\s+HTTP/\d",
            Severity::Low,
            WafAction::Log,
        ),
        CompiledRule::new(
            "crs-920120",
            "Protocol Anomaly - Content-Type Mismatch",
            r"(?i)content-type:\s*multipart/form-data[^;]*boundary=[^a-zA-Z0-9]",
            Severity::Medium,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-920130",
            "Protocol Anomaly - Duplicate Headers",
            r"(?i)(transfer-encoding|content-length)[^:]*:[^:]+\1",
            Severity::High,
            WafAction::Block,
        ),
    ]
}

fn get_evasion_rules() -> Vec<CompiledRule> {
    vec![
        CompiledRule::new(
            "crs-921100",
            "Evasion - Null Byte Injection",
            r"%00|\\x00|\\u0000",
            Severity::Critical,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-921110",
            "Evasion - Unicode Obfuscation",
            r"(?i)(%u[0-9a-f]{4}|\\u[0-9a-f]{4})",
            Severity::Medium,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-921120",
            "Evasion - Double URL Encoding",
            r"%25[0-9a-fA-F]{2}",
            Severity::Medium,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-921130",
            "Evasion - Overlong UTF-8",
            r"%c0%af|%c1%9c|%e0%80%af",
            Severity::Critical,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-921140",
            "Evasion - Backslash Obfuscation",
            r"\\[xuU][0-9a-fA-F]{2,8}",
            Severity::Medium,
            WafAction::Block,
        ),
    ]
}

fn get_data_leakage_rules() -> Vec<CompiledRule> {
    vec![
        CompiledRule::new(
            "crs-950100",
            "Data Leakage - Credit Card Number",
            r"\b(?:4[0-9]{12}(?:[0-9]{3})?|5[1-5][0-9]{14}|3[47][0-9]{13}|6(?:011|5[0-9]{2})[0-9]{12})\b",
            Severity::Critical,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-950110",
            "Data Leakage - SSN Pattern",
            r"\b\d{3}-\d{2}-\d{4}\b",
            Severity::High,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-950120",
            "Data Leakage - Private Key",
            r"(?i)(-----BEGIN\s+(RSA\s+)?PRIVATE\s+KEY-----)",
            Severity::Critical,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-950130",
            "Data Leakage - AWS Key",
            r"(?i)(AKIA[0-9A-Z]{16}|aws_secret_access_key)",
            Severity::Critical,
            WafAction::Block,
        ),
    ]
}

fn get_strict_xss_rules() -> Vec<CompiledRule> {
    vec![
        CompiledRule::new(
            "crs-941300",
            "XSS - Angle Brackets",
            r"<[^>]*[^\w\s>][^>]*>",
            Severity::Low,
            WafAction::Log,
        ),
        CompiledRule::new(
            "crs-941310",
            "XSS - HTML Entity Encoding",
            r"(?i)(&(#[0-9]+|#x[0-9a-f]+|[a-z]+);){2,}",
            Severity::Medium,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-941320",
            "XSS - Style Tag",
            r"(?i)<\s*style[^>]*>",
            Severity::Medium,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-941330",
            "XSS - Base Tag",
            r"(?i)<\s*base[^>]*href\s*=",
            Severity::High,
            WafAction::Block,
        ),
    ]
}

fn get_strict_sqli_rules() -> Vec<CompiledRule> {
    vec![
        CompiledRule::new(
            "crs-942300",
            "SQL Injection - Function Calls",
            r"(?i)(ascii|substring|length|version|database|user)\s*\(",
            Severity::Medium,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-942310",
            "SQL Injection - Information Schema",
            r"(?i)information_schema\.(tables|columns|schemata)",
            Severity::Critical,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-942320",
            "SQL Injection - MySQL Specific",
            r"(?i)(load_file|into\s+(out|dump)file|@@(version|datadir))",
            Severity::Critical,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-942330",
            "SQL Injection - PostgreSQL Specific",
            r"(?i)(pg_catalog|pg_tables|current_database\(\))",
            Severity::High,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-942340",
            "SQL Injection - MSSQL Specific",
            r"(?i)(xp_cmdshell|sp_executesql|master\.\.sysdatabases)",
            Severity::Critical,
            WafAction::Block,
        ),
    ]
}
