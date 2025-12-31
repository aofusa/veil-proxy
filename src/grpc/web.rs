//! gRPC-Web Protocol Support
//!
//! Converts between browser-compatible gRPC-Web and native gRPC.
//!
//! ## Content Types
//! - `application/grpc-web`: Binary format with base64-encoded trailers
//! - `application/grpc-web-text`: Fully base64-encoded body
//! - `application/grpc-web+proto`: Binary with protobuf
//! - `application/grpc-web+json`: Binary with JSON
//!
//! ## Features
//! - Request/response conversion
//! - CORS header handling (per-route configuration)
//! - Trailer encoding for browsers

use std::time::Duration;

/// gRPC-Web content type prefix
pub const GRPC_WEB_CONTENT_TYPE_PREFIX: &[u8] = b"application/grpc-web";

/// gRPC-Web text content type (fully base64 encoded)
pub const GRPC_WEB_TEXT_CONTENT_TYPE: &[u8] = b"application/grpc-web-text";

/// Check if content-type indicates a gRPC-Web request
#[inline]
pub fn is_grpc_web_content_type(content_type: &[u8]) -> bool {
    content_type.len() >= GRPC_WEB_CONTENT_TYPE_PREFIX.len()
        && content_type[..GRPC_WEB_CONTENT_TYPE_PREFIX.len()]
            .eq_ignore_ascii_case(GRPC_WEB_CONTENT_TYPE_PREFIX)
}

/// Check if content-type is text format (fully base64)
#[inline]
pub fn is_grpc_web_text(content_type: &[u8]) -> bool {
    content_type.eq_ignore_ascii_case(GRPC_WEB_TEXT_CONTENT_TYPE)
        || content_type
            .get(..GRPC_WEB_TEXT_CONTENT_TYPE.len())
            .map(|ct| ct.eq_ignore_ascii_case(GRPC_WEB_TEXT_CONTENT_TYPE))
            .unwrap_or(false)
}

/// gRPC-Web CORS configuration for a route
#[derive(Debug, Clone)]
pub struct GrpcWebCorsConfig {
    /// Allowed origins (use "*" for any origin)
    pub allowed_origins: Vec<String>,
    /// Allowed headers
    pub allowed_headers: Vec<String>,
    /// Exposed headers (headers browser can access)
    pub exposed_headers: Vec<String>,
    /// Allow credentials
    pub allow_credentials: bool,
    /// Max age for preflight cache (seconds)
    pub max_age: Duration,
}

impl Default for GrpcWebCorsConfig {
    fn default() -> Self {
        Self {
            allowed_origins: vec!["*".to_string()],
            allowed_headers: vec![
                "content-type".to_string(),
                "x-grpc-web".to_string(),
                "x-user-agent".to_string(),
                "grpc-timeout".to_string(),
            ],
            exposed_headers: vec![
                "grpc-status".to_string(),
                "grpc-message".to_string(),
            ],
            allow_credentials: false,
            max_age: Duration::from_secs(86400), // 24 hours
        }
    }
}

impl GrpcWebCorsConfig {
    /// Create config that allows any origin
    pub fn permissive() -> Self {
        Self::default()
    }

    /// Create restrictive config with specific origins
    pub fn with_origins(origins: Vec<String>) -> Self {
        Self {
            allowed_origins: origins,
            ..Self::default()
        }
    }

    /// Check if origin is allowed
    pub fn is_origin_allowed(&self, origin: &str) -> bool {
        self.allowed_origins.iter().any(|o| o == "*" || o == origin)
    }

    /// Get CORS headers for preflight response
    pub fn preflight_headers(&self, origin: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut headers = Vec::new();

        // Access-Control-Allow-Origin
        if self.is_origin_allowed(origin) {
            let allowed_origin = if self.allowed_origins.contains(&"*".to_string()) {
                "*"
            } else {
                origin
            };
            headers.push((
                b"access-control-allow-origin".to_vec(),
                allowed_origin.as_bytes().to_vec(),
            ));
        }

        // Access-Control-Allow-Methods
        headers.push((
            b"access-control-allow-methods".to_vec(),
            b"POST, OPTIONS".to_vec(),
        ));

        // Access-Control-Allow-Headers
        let allowed_headers = self.allowed_headers.join(", ");
        headers.push((
            b"access-control-allow-headers".to_vec(),
            allowed_headers.into_bytes(),
        ));

        // Access-Control-Expose-Headers
        if !self.exposed_headers.is_empty() {
            let exposed = self.exposed_headers.join(", ");
            headers.push((
                b"access-control-expose-headers".to_vec(),
                exposed.into_bytes(),
            ));
        }

        // Access-Control-Allow-Credentials
        if self.allow_credentials {
            headers.push((
                b"access-control-allow-credentials".to_vec(),
                b"true".to_vec(),
            ));
        }

        // Access-Control-Max-Age
        headers.push((
            b"access-control-max-age".to_vec(),
            self.max_age.as_secs().to_string().into_bytes(),
        ));

        headers
    }

    /// Get CORS headers for actual response
    pub fn response_headers(&self, origin: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut headers = Vec::new();

        if self.is_origin_allowed(origin) {
            let allowed_origin = if self.allowed_origins.contains(&"*".to_string()) {
                "*"
            } else {
                origin
            };
            headers.push((
                b"access-control-allow-origin".to_vec(),
                allowed_origin.as_bytes().to_vec(),
            ));

            if !self.exposed_headers.is_empty() {
                let exposed = self.exposed_headers.join(", ");
                headers.push((
                    b"access-control-expose-headers".to_vec(),
                    exposed.into_bytes(),
                ));
            }

            if self.allow_credentials {
                headers.push((
                    b"access-control-allow-credentials".to_vec(),
                    b"true".to_vec(),
                ));
            }
        }

        headers
    }
}

/// gRPC-Web full configuration for a route
#[derive(Debug, Clone, Default)]
pub struct GrpcWebConfig {
    /// Enable gRPC-Web support for this route
    pub enabled: bool,
    /// CORS configuration
    pub cors: GrpcWebCorsConfig,
}

impl GrpcWebConfig {
    /// Create enabled config with default CORS
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            cors: GrpcWebCorsConfig::default(),
        }
    }

    /// Create enabled config with specific origins
    pub fn with_origins(origins: Vec<String>) -> Self {
        Self {
            enabled: true,
            cors: GrpcWebCorsConfig::with_origins(origins),
        }
    }
}

/// Decode base64-encoded gRPC-Web request body
pub fn decode_grpc_web_body(body: &[u8], is_text: bool) -> Result<Vec<u8>, GrpcWebError> {
    if is_text {
        // Full base64 decode
        base64_decode(body)
    } else {
        // Binary format - no decode needed
        Ok(body.to_vec())
    }
}

/// Encode gRPC-Web response with trailers
///
/// Format: [message frames] + [trailer frame]
/// Trailer frame: 0x80 + 4-byte length + trailers as HTTP headers format
pub fn encode_grpc_web_response(
    body: &[u8],
    trailers: &[(Vec<u8>, Vec<u8>)],
    is_text: bool,
) -> Vec<u8> {
    let mut response = Vec::new();

    // Message body
    response.extend_from_slice(body);

    // Trailer frame
    let trailer_data = encode_trailers(trailers);
    
    // Trailer frame header: 0x80 (trailer flag) + 4-byte length
    response.push(0x80);
    response.extend_from_slice(&(trailer_data.len() as u32).to_be_bytes());
    response.extend_from_slice(&trailer_data);

    if is_text {
        // Base64 encode entire response
        base64_encode(&response)
    } else {
        response
    }
}

/// Encode trailers in HTTP header format (name: value\r\n)
fn encode_trailers(trailers: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut data = Vec::new();
    for (name, value) in trailers {
        data.extend_from_slice(name);
        data.extend_from_slice(b": ");
        data.extend_from_slice(value);
        data.extend_from_slice(b"\r\n");
    }
    data
}

/// Simple base64 encode
fn base64_encode(data: &[u8]) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = Vec::with_capacity((data.len() + 2) / 3 * 4);

    for chunk in data.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
        let b2 = chunk.get(2).copied().unwrap_or(0) as usize;

        result.push(ALPHABET[b0 >> 2]);
        result.push(ALPHABET[((b0 & 0x03) << 4) | (b1 >> 4)]);

        if chunk.len() > 1 {
            result.push(ALPHABET[((b1 & 0x0f) << 2) | (b2 >> 6)]);
        } else {
            result.push(b'=');
        }

        if chunk.len() > 2 {
            result.push(ALPHABET[b2 & 0x3f]);
        } else {
            result.push(b'=');
        }
    }

    result
}

/// Simple base64 decode
fn base64_decode(data: &[u8]) -> Result<Vec<u8>, GrpcWebError> {
    fn decode_char(c: u8) -> Result<u8, GrpcWebError> {
        match c {
            b'A'..=b'Z' => Ok(c - b'A'),
            b'a'..=b'z' => Ok(c - b'a' + 26),
            b'0'..=b'9' => Ok(c - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            b'=' => Ok(0), // Padding
            _ => Err(GrpcWebError::InvalidBase64),
        }
    }

    let mut result = Vec::with_capacity(data.len() * 3 / 4);
    let mut iter = data.iter().filter(|&&c| c != b'\n' && c != b'\r');

    loop {
        let chunk: Vec<u8> = iter.by_ref().take(4).copied().collect();
        if chunk.is_empty() {
            break;
        }
        if chunk.len() < 4 {
            return Err(GrpcWebError::InvalidBase64);
        }

        let b0 = decode_char(chunk[0])?;
        let b1 = decode_char(chunk[1])?;
        let b2 = decode_char(chunk[2])?;
        let b3 = decode_char(chunk[3])?;

        result.push((b0 << 2) | (b1 >> 4));
        if chunk[2] != b'=' {
            result.push((b1 << 4) | (b2 >> 2));
        }
        if chunk[3] != b'=' {
            result.push((b2 << 6) | b3);
        }
    }

    Ok(result)
}

/// gRPC-Web specific errors
#[derive(Debug, Clone)]
pub enum GrpcWebError {
    /// Invalid base64 encoding
    InvalidBase64,
    /// Invalid trailer format
    InvalidTrailers,
    /// Missing required header
    MissingHeader(String),
}

impl std::fmt::Display for GrpcWebError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidBase64 => write!(f, "Invalid base64 encoding"),
            Self::InvalidTrailers => write!(f, "Invalid gRPC-Web trailers"),
            Self::MissingHeader(h) => write!(f, "Missing header: {}", h),
        }
    }
}

impl std::error::Error for GrpcWebError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_grpc_web_content_type() {
        assert!(is_grpc_web_content_type(b"application/grpc-web"));
        assert!(is_grpc_web_content_type(b"application/grpc-web+proto"));
        assert!(is_grpc_web_content_type(b"application/grpc-web+json"));
        assert!(is_grpc_web_content_type(b"Application/GRPC-WEB"));

        assert!(!is_grpc_web_content_type(b"application/grpc"));
        assert!(!is_grpc_web_content_type(b"application/json"));
    }

    #[test]
    fn test_is_grpc_web_text() {
        assert!(is_grpc_web_text(b"application/grpc-web-text"));
        assert!(is_grpc_web_text(b"application/grpc-web-text+proto"));

        assert!(!is_grpc_web_text(b"application/grpc-web"));
        assert!(!is_grpc_web_text(b"application/grpc-web+proto"));
    }

    #[test]
    fn test_cors_origin_check() {
        let config = GrpcWebCorsConfig::default();
        assert!(config.is_origin_allowed("http://example.com"));
        assert!(config.is_origin_allowed("http://localhost:3000"));

        let restricted = GrpcWebCorsConfig::with_origins(vec!["http://example.com".to_string()]);
        assert!(restricted.is_origin_allowed("http://example.com"));
        assert!(!restricted.is_origin_allowed("http://other.com"));
    }

    #[test]
    fn test_base64_roundtrip() {
        let original = b"Hello, gRPC-Web!";
        let encoded = base64_encode(original);
        let decoded = base64_decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_encode_grpc_web_response() {
        let body = vec![0, 0, 0, 0, 5, b'H', b'e', b'l', b'l', b'o'];
        let trailers = vec![
            (b"grpc-status".to_vec(), b"0".to_vec()),
        ];

        let response = encode_grpc_web_response(&body, &trailers, false);

        // Body should be included
        assert!(response.starts_with(&body));
        // Trailer frame header should be 0x80
        assert_eq!(response[body.len()], 0x80);
    }

    #[test]
    fn test_preflight_headers() {
        let config = GrpcWebCorsConfig::default();
        let headers = config.preflight_headers("http://example.com");

        assert!(headers
            .iter()
            .any(|(n, _)| n == b"access-control-allow-origin"));
        assert!(headers
            .iter()
            .any(|(n, _)| n == b"access-control-allow-methods"));
        assert!(headers
            .iter()
            .any(|(n, _)| n == b"access-control-allow-headers"));
    }
}
