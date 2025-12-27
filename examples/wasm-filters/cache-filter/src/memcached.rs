//! Memcached Backend via HTTP API
//!
//! Uses a generic HTTP interface to Memcached:
//! - GET /get?key={key} -> {"value": "...", "found": true}
//! - POST /set {"key": "...", "value": "...", "ttl": 300}
//!
//! Implementations like memcached-http-proxy or custom adapters.

use serde::{Deserialize, Serialize};

/// Memcached GET response
#[derive(Debug, Deserialize)]
pub struct MemcachedGetResponse {
    pub value: Option<String>,
    pub found: bool,
}

/// Memcached SET request
#[derive(Debug, Serialize)]
pub struct MemcachedSetRequest<'a> {
    pub key: &'a str,
    pub value: &'a str,
    pub ttl: u64,
}

/// Build Memcached GET request path
pub fn build_get_path(key: &str) -> String {
    format!("/get?key={}", url_encode(key))
}

/// Build Memcached SET request body
pub fn build_set_body(key: &str, value: &str, ttl_secs: u64) -> Vec<u8> {
    let request = MemcachedSetRequest { key, value, ttl: ttl_secs };
    serde_json::to_vec(&request).unwrap_or_default()
}

/// Parse Memcached GET response
pub fn parse_get_response(body: &[u8]) -> Option<String> {
    let response: MemcachedGetResponse = serde_json::from_slice(body).ok()?;
    if response.found {
        response.value
    } else {
        None
    }
}

/// Simple URL encoding
fn url_encode(s: &str) -> String {
    let mut result = String::new();
    for c in s.chars() {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => {
                result.push(c);
            }
            _ => {
                for b in c.to_string().as_bytes() {
                    result.push_str(&format!("%{:02X}", b));
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_get_path() {
        assert_eq!(build_get_path("mykey"), "/get?key=mykey");
    }

    #[test]
    fn test_build_set_body() {
        let body = build_set_body("k", "v", 300);
        let parsed: MemcachedSetRequest = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed.key, "k");
        assert_eq!(parsed.value, "v");
        assert_eq!(parsed.ttl, 300);
    }

    #[test]
    fn test_parse_get_response() {
        let body = br#"{"value": "cached", "found": true}"#;
        assert_eq!(parse_get_response(body), Some("cached".to_string()));

        let body_miss = br#"{"value": null, "found": false}"#;
        assert_eq!(parse_get_response(body_miss), None);
    }
}
