//! Per-Module Capability Restrictions
//!
//! All capabilities default to false (deny by default).

use serde::{Deserialize, Serialize};

/// Per-module capability settings
///
/// All capabilities default to false (deny by default).
/// Modules must be explicitly granted permissions.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModuleCapabilities {
    // === Basic Functions ===
    /// Allow logging via proxy_log
    #[serde(default)]
    pub allow_logging: bool,

    /// Allow metrics operations
    #[serde(default)]
    pub allow_metrics: bool,

    /// Allow shared data access
    #[serde(default)]
    pub allow_shared_data: bool,

    // === Request Headers ===
    /// Allow reading request headers
    #[serde(default)]
    pub allow_request_headers_read: bool,

    /// Allow modifying request headers
    #[serde(default)]
    pub allow_request_headers_write: bool,

    // === Request Body ===
    /// Allow reading request body
    #[serde(default)]
    pub allow_request_body_read: bool,

    /// Allow modifying request body
    #[serde(default)]
    pub allow_request_body_write: bool,

    // === Response Headers ===
    /// Allow reading response headers
    #[serde(default)]
    pub allow_response_headers_read: bool,

    /// Allow modifying response headers
    #[serde(default)]
    pub allow_response_headers_write: bool,

    // === Response Body ===
    /// Allow reading response body
    #[serde(default)]
    pub allow_response_body_read: bool,

    /// Allow modifying response body
    #[serde(default)]
    pub allow_response_body_write: bool,

    // === Control ===
    /// Allow sending local response
    #[serde(default)]
    pub allow_send_local_response: bool,

    // === External Calls ===
    /// Allow HTTP calls to upstreams
    #[serde(default)]
    pub allow_http_calls: bool,

    /// Allowed upstream names (whitelist)
    #[serde(default)]
    pub allowed_upstreams: Vec<String>,

    /// Maximum concurrent HTTP calls
    #[serde(default = "default_max_http_calls")]
    pub max_http_calls: usize,

    // === Properties ===
    /// Allowed property path patterns
    #[serde(default)]
    pub allowed_properties: Vec<String>,

    // === Resource Limits ===
    /// Maximum shared data size in bytes
    #[serde(default = "default_max_shared_data_size")]
    pub max_shared_data_size: usize,

    /// Maximum execution time in milliseconds
    #[serde(default = "default_max_execution_time")]
    pub max_execution_time_ms: u64,
}

fn default_max_http_calls() -> usize {
    10
}

fn default_max_shared_data_size() -> usize {
    1_048_576 // 1MB
}

fn default_max_execution_time() -> u64 {
    100
}

impl Default for ModuleCapabilities {
    fn default() -> Self {
        // All capabilities default to false (deny by default)
        Self {
            allow_logging: false,
            allow_metrics: false,
            allow_shared_data: false,
            allow_request_headers_read: false,
            allow_request_headers_write: false,
            allow_request_body_read: false,
            allow_request_body_write: false,
            allow_response_headers_read: false,
            allow_response_headers_write: false,
            allow_response_body_read: false,
            allow_response_body_write: false,
            allow_send_local_response: false,
            allow_http_calls: false,
            allowed_upstreams: Vec::new(),
            max_http_calls: default_max_http_calls(),
            allowed_properties: Vec::new(),
            max_shared_data_size: default_max_shared_data_size(),
            max_execution_time_ms: default_max_execution_time(),
        }
    }
}

/// Capability preset for common use cases
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityPreset {
    /// Minimal: read-only access, no modifications
    Minimal,
    /// Standard: header read/write, local response
    Standard,
    /// Extended: body read/write, HTTP calls
    Extended,
}

impl CapabilityPreset {
    /// Convert preset to capabilities
    pub fn to_capabilities(&self) -> ModuleCapabilities {
        match self {
            CapabilityPreset::Minimal => ModuleCapabilities {
                allow_logging: true,
                allow_request_headers_read: true,
                allow_response_headers_read: true,
                allowed_properties: vec![
                    "request.path".to_string(),
                    "request.method".to_string(),
                    "source.address".to_string(),
                ],
                ..Default::default()
            },
            CapabilityPreset::Standard => ModuleCapabilities {
                allow_logging: true,
                allow_metrics: true,
                allow_request_headers_read: true,
                allow_request_headers_write: true,
                allow_response_headers_read: true,
                allow_response_headers_write: true,
                allow_send_local_response: true,
                allowed_properties: vec![
                    "request.*".to_string(),
                    "response.*".to_string(),
                    "source.address".to_string(),
                ],
                ..Default::default()
            },
            CapabilityPreset::Extended => ModuleCapabilities {
                allow_logging: true,
                allow_metrics: true,
                allow_shared_data: true,
                allow_request_headers_read: true,
                allow_request_headers_write: true,
                allow_request_body_read: true,
                allow_request_body_write: true,
                allow_response_headers_read: true,
                allow_response_headers_write: true,
                allow_response_body_read: true,
                allow_response_body_write: true,
                allow_send_local_response: true,
                allow_http_calls: true,
                allowed_properties: vec!["*".to_string()],
                ..Default::default()
            },
        }
    }
}

impl ModuleCapabilities {
    /// Check if a property path is allowed
    pub fn is_property_allowed(&self, path: &str) -> bool {
        if self.allowed_properties.is_empty() {
            return false;
        }

        for pattern in &self.allowed_properties {
            if pattern == "*" {
                return true;
            }
            if pattern.ends_with(".*") {
                let prefix = &pattern[..pattern.len() - 2];
                if path.starts_with(prefix) {
                    return true;
                }
            }
            if pattern == path {
                return true;
            }
        }
        false
    }

    /// Check if an upstream is allowed
    pub fn is_upstream_allowed(&self, upstream: &str) -> bool {
        if !self.allow_http_calls {
            return false;
        }
        if self.allowed_upstreams.is_empty() {
            // Empty means all upstreams allowed (if http_calls enabled)
            return true;
        }
        self.allowed_upstreams.iter().any(|u| u == upstream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_all_false() {
        let caps = ModuleCapabilities::default();
        assert!(!caps.allow_logging);
        assert!(!caps.allow_metrics);
        assert!(!caps.allow_shared_data);
        assert!(!caps.allow_request_headers_read);
        assert!(!caps.allow_request_headers_write);
        assert!(!caps.allow_http_calls);
    }

    #[test]
    fn test_property_allowed() {
        let caps = ModuleCapabilities {
            allowed_properties: vec![
                "request.*".to_string(),
                "source.address".to_string(),
            ],
            ..Default::default()
        };

        assert!(caps.is_property_allowed("request.path"));
        assert!(caps.is_property_allowed("request.method"));
        assert!(caps.is_property_allowed("source.address"));
        assert!(!caps.is_property_allowed("response.code"));
    }

    #[test]
    fn test_upstream_allowed() {
        let caps = ModuleCapabilities {
            allow_http_calls: true,
            allowed_upstreams: vec!["webdis".to_string()],
            ..Default::default()
        };

        assert!(caps.is_upstream_allowed("webdis"));
        assert!(!caps.is_upstream_allowed("other"));
    }
}
