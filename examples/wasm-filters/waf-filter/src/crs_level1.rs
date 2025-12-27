//! CRS Level 1 Rules - Basic Protection
//!
//! Minimal set of high-confidence rules with low false positive rates.
//! Suitable for high-volume APIs and applications that prioritize availability.

use crate::rules::{CompiledRule, Severity, WafAction};

/// Get all CRS Level 1 rules
pub fn get_rules() -> Vec<CompiledRule> {
    let mut rules = Vec::new();
    
    // SQL Injection - Critical patterns only
    rules.extend(get_sqli_rules());
    
    // XSS - Critical patterns only
    rules.extend(get_xss_rules());
    
    // Path Traversal - Critical patterns only
    rules.extend(get_path_traversal_rules());
    
    rules
}

fn get_sqli_rules() -> Vec<CompiledRule> {
    vec![
        CompiledRule::new(
            "crs-942100",
            "SQL Injection - UNION SELECT",
            r"(?i)\bunion\b\s+(all\s+)?select\b",
            Severity::Critical,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-942110",
            "SQL Injection - Basic SELECT FROM",
            r"(?i)\bselect\b.{1,100}\bfrom\b.{1,100}\bwhere\b",
            Severity::Critical,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-942120",
            "SQL Injection - DROP/DELETE/TRUNCATE",
            r"(?i)(\bdrop\b\s+\b(table|database|schema)\b|\bdelete\b\s+\bfrom\b|\btruncate\b\s+\btable\b)",
            Severity::Critical,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-942130",
            "SQL Injection - INSERT INTO",
            r"(?i)\binsert\b\s+\binto\b\s+\w+\s*\(",
            Severity::High,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-942140",
            "SQL Injection - UPDATE SET",
            r"(?i)\bupdate\b\s+\w+\s+\bset\b\s+\w+\s*=",
            Severity::High,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-942150",
            "SQL Injection - Boolean Logic",
            r#"(?i)('|")\s*(or|and)\s*('|")?\s*(=|1|true)"#,
            Severity::Critical,
            WafAction::Block,
        ),
    ]
}

fn get_xss_rules() -> Vec<CompiledRule> {
    vec![
        CompiledRule::new(
            "crs-941100",
            "XSS - Script Tag",
            r"(?i)<\s*script[^>]*>",
            Severity::Critical,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-941110",
            "XSS - JavaScript URI",
            r"(?i)javascript\s*:",
            Severity::Critical,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-941120",
            "XSS - Event Handlers",
            r"(?i)\bon(error|load|click|mouse|focus|blur|change|submit|key|touch)\s*=",
            Severity::Critical,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-941130",
            "XSS - Iframe/Frame/Object/Embed",
            r"(?i)<\s*(iframe|frame|object|embed)[^>]*>",
            Severity::High,
            WafAction::Block,
        ),
    ]
}

fn get_path_traversal_rules() -> Vec<CompiledRule> {
    vec![
        CompiledRule::new(
            "crs-930100",
            "Path Traversal - Basic",
            r"(\.\.(/|\\|%2f|%5c)){2,}",
            Severity::Critical,
            WafAction::Block,
        ),
        CompiledRule::new(
            "crs-930110",
            "Path Traversal - Sensitive Files",
            r"(?i)(/etc/(passwd|shadow|hosts)|/proc/self|c:\\\\windows\\\\system32)",
            Severity::Critical,
            WafAction::Block,
        ),
    ]
}
