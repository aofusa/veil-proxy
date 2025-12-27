//! Redis Backend via Webdis HTTP API
//!
//! Webdis exposes Redis commands as HTTP endpoints:
//! - GET /GET/{key} -> {"GET": "value"} or {"GET": null}
//! - GET /SET/{key}/{value} -> {"SET": [true, "OK"]}
//! - GET /SET/{key}/{value}/EX/{ttl} -> {"SET": [true, "OK"]}
//! - GET /DEL/{key} -> {"DEL": 1}

use serde::Deserialize;

/// Redis GET response
#[derive(Debug, Deserialize)]
pub struct RedisGetResponse {
    #[serde(rename = "GET")]
    pub value: Option<String>,
}

/// Redis SET response
#[derive(Debug, Deserialize)]
pub struct RedisSetResponse {
    #[serde(rename = "SET")]
    pub result: (bool, String),
}

/// Build Redis GET request path
pub fn build_get_path(key: &str) -> String {
    format!("/GET/{}", url_encode(key))
}

/// Build Redis SET request path with optional TTL
pub fn build_set_path(key: &str, value: &str, ttl_secs: Option<u64>) -> String {
    let encoded_key = url_encode(key);
    let encoded_value = url_encode(value);
    
    match ttl_secs {
        Some(ttl) => format!("/SET/{}/{}/EX/{}", encoded_key, encoded_value, ttl),
        None => format!("/SET/{}/{}", encoded_key, encoded_value),
    }
}

/// Parse Redis GET response
pub fn parse_get_response(body: &[u8]) -> Option<String> {
    let response: RedisGetResponse = serde_json::from_slice(body).ok()?;
    response.value
}

/// Simple URL encoding for Redis keys/values
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
        assert_eq!(build_get_path("mykey"), "/GET/mykey");
        assert_eq!(build_get_path("key:with:colons"), "/GET/key%3Awith%3Acolons");
    }

    #[test]
    fn test_build_set_path() {
        assert_eq!(build_set_path("k", "v", None), "/SET/k/v");
        assert_eq!(build_set_path("k", "v", Some(300)), "/SET/k/v/EX/300");
    }

    #[test]
    fn test_parse_get_response() {
        let body = br#"{"GET": "cached_value"}"#;
        assert_eq!(parse_get_response(body), Some("cached_value".to_string()));

        let body_null = br#"{"GET": null}"#;
        assert_eq!(parse_get_response(body_null), None);
    }
}
