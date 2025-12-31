//! gRPC Header Handling
//!
//! gRPC uses specific HTTP/2 headers for request/response control:
//!
//! ## Request Headers
//! - `content-type`: `application/grpc[+proto|+json]`
//! - `grpc-timeout`: Timeout value (e.g., "10S", "100m", "1H")
//! - `grpc-encoding`: Request message encoding (gzip, deflate, identity)
//! - `grpc-accept-encoding`: Accepted response encodings
//!
//! ## Response Trailers
//! - `grpc-status`: Numeric status code (0-16)
//! - `grpc-message`: URL-encoded error message

use std::time::Duration;

/// gRPC content-type prefix
pub const GRPC_CONTENT_TYPE_PREFIX: &[u8] = b"application/grpc";

/// gRPC content-type for Protobuf (default)
pub const GRPC_CONTENT_TYPE_PROTO: &[u8] = b"application/grpc+proto";

/// gRPC content-type for JSON
pub const GRPC_CONTENT_TYPE_JSON: &[u8] = b"application/grpc+json";

/// Check if content-type indicates a gRPC request
///
/// Returns true if content-type starts with "application/grpc"
#[inline]
pub fn is_grpc_content_type(content_type: &[u8]) -> bool {
    content_type.len() >= GRPC_CONTENT_TYPE_PREFIX.len()
        && content_type[..GRPC_CONTENT_TYPE_PREFIX.len()]
            .eq_ignore_ascii_case(GRPC_CONTENT_TYPE_PREFIX)
}

/// Check if HTTP headers indicate a gRPC request
///
/// Looks for `content-type: application/grpc*`
pub fn is_grpc_request(headers: &[(impl AsRef<[u8]>, impl AsRef<[u8]>)]) -> bool {
    headers.iter().any(|(name, value)| {
        name.as_ref().eq_ignore_ascii_case(b"content-type")
            && is_grpc_content_type(value.as_ref())
    })
}

/// Check if httparse headers indicate a gRPC request
pub fn is_grpc_request_httparse(headers: &[httparse::Header]) -> bool {
    headers.iter().any(|h| {
        h.name.eq_ignore_ascii_case("content-type") && is_grpc_content_type(h.value)
    })
}

/// Parse grpc-timeout header value
///
/// Format: `<value><unit>`
/// - H: hours
/// - M: minutes
/// - S: seconds
/// - m: milliseconds
/// - u: microseconds
/// - n: nanoseconds
///
/// Examples: "10S" = 10 seconds, "100m" = 100 milliseconds
pub fn parse_grpc_timeout(value: &[u8]) -> Option<Duration> {
    if value.is_empty() {
        return None;
    }

    let s = std::str::from_utf8(value).ok()?;
    let (num_str, unit) = s.split_at(s.len() - 1);
    let num: u64 = num_str.parse().ok()?;

    match unit {
        "H" => Some(Duration::from_secs(num * 3600)),
        "M" => Some(Duration::from_secs(num * 60)),
        "S" => Some(Duration::from_secs(num)),
        "m" => Some(Duration::from_millis(num)),
        "u" => Some(Duration::from_micros(num)),
        "n" => Some(Duration::from_nanos(num)),
        _ => None,
    }
}

/// Format duration as grpc-timeout header value
///
/// Chooses the most appropriate unit to minimize header size
pub fn format_grpc_timeout(duration: Duration) -> String {
    let nanos = duration.as_nanos();

    if nanos == 0 {
        return "0n".to_string();
    }

    // Try to use the largest unit that gives an integer value
    let hours = duration.as_secs() / 3600;
    if hours > 0 && duration.as_secs() % 3600 == 0 {
        return format!("{}H", hours);
    }

    let minutes = duration.as_secs() / 60;
    if minutes > 0 && duration.as_secs() % 60 == 0 {
        return format!("{}M", minutes);
    }

    let secs = duration.as_secs();
    if secs > 0 && duration.subsec_nanos() == 0 {
        return format!("{}S", secs);
    }

    let millis = duration.as_millis();
    if millis > 0 && nanos % 1_000_000 == 0 {
        return format!("{}m", millis);
    }

    let micros = duration.as_micros();
    if micros > 0 && nanos % 1000 == 0 {
        return format!("{}u", micros);
    }

    format!("{}n", nanos)
}

/// gRPC compression encoding
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GrpcEncoding {
    /// No compression
    #[default]
    Identity,
    /// Gzip compression
    Gzip,
    /// Deflate compression
    Deflate,
}

impl GrpcEncoding {
    /// Parse from grpc-encoding header value
    pub fn from_bytes(value: &[u8]) -> Option<Self> {
        if value.eq_ignore_ascii_case(b"identity") {
            Some(Self::Identity)
        } else if value.eq_ignore_ascii_case(b"gzip") {
            Some(Self::Gzip)
        } else if value.eq_ignore_ascii_case(b"deflate") {
            Some(Self::Deflate)
        } else {
            None
        }
    }

    /// Get header value
    pub fn as_bytes(&self) -> &'static [u8] {
        match self {
            Self::Identity => b"identity",
            Self::Gzip => b"gzip",
            Self::Deflate => b"deflate",
        }
    }

    /// Check if this encoding requires compression
    pub fn is_compressed(&self) -> bool {
        !matches!(self, Self::Identity)
    }
}

/// Parsed gRPC headers container
#[derive(Debug, Clone, Default)]
pub struct GrpcHeaders {
    /// Timeout duration from grpc-timeout header
    pub timeout: Option<Duration>,
    /// Request encoding from grpc-encoding header
    pub encoding: GrpcEncoding,
    /// Accepted response encodings from grpc-accept-encoding
    pub accept_encoding: Vec<GrpcEncoding>,
    /// Message type from grpc-message-type header
    pub message_type: Option<String>,
    /// Authority (host) for the request
    pub authority: Option<String>,
    /// Path (service/method)
    pub path: Option<String>,
}

impl GrpcHeaders {
    /// Create new empty headers
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse gRPC-specific headers from HTTP/2 header list
    pub fn from_headers(headers: &[(impl AsRef<[u8]>, impl AsRef<[u8]>)]) -> Self {
        let mut result = Self::new();

        for (name, value) in headers {
            let name = name.as_ref();
            let value = value.as_ref();

            if name.eq_ignore_ascii_case(b"grpc-timeout") {
                result.timeout = parse_grpc_timeout(value);
            } else if name.eq_ignore_ascii_case(b"grpc-encoding") {
                result.encoding = GrpcEncoding::from_bytes(value).unwrap_or_default();
            } else if name.eq_ignore_ascii_case(b"grpc-accept-encoding") {
                result.accept_encoding = Self::parse_accept_encoding(value);
            } else if name.eq_ignore_ascii_case(b"grpc-message-type") {
                result.message_type = std::str::from_utf8(value).ok().map(String::from);
            } else if name == b":authority" {
                result.authority = std::str::from_utf8(value).ok().map(String::from);
            } else if name == b":path" {
                result.path = std::str::from_utf8(value).ok().map(String::from);
            }
        }

        result
    }

    /// Parse grpc-accept-encoding header (comma-separated values)
    fn parse_accept_encoding(value: &[u8]) -> Vec<GrpcEncoding> {
        std::str::from_utf8(value)
            .ok()
            .map(|s| {
                s.split(',')
                    .filter_map(|part| GrpcEncoding::from_bytes(part.trim().as_bytes()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get service name from path (e.g., "/package.Service/Method" -> "package.Service")
    pub fn service_name(&self) -> Option<&str> {
        self.path.as_ref().and_then(|p| {
            let p = p.strip_prefix('/')?;
            let idx = p.rfind('/')?;
            Some(&p[..idx])
        })
    }

    /// Get method name from path (e.g., "/package.Service/Method" -> "Method")
    pub fn method_name(&self) -> Option<&str> {
        self.path.as_ref().and_then(|p| {
            let idx = p.rfind('/')?;
            Some(&p[idx + 1..])
        })
    }

    /// Build gRPC request headers for proxying
    pub fn to_request_headers(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut headers = vec![
            (b"content-type".to_vec(), GRPC_CONTENT_TYPE_PROTO.to_vec()),
        ];

        if let Some(timeout) = self.timeout {
            headers.push((
                b"grpc-timeout".to_vec(),
                format_grpc_timeout(timeout).into_bytes(),
            ));
        }

        if self.encoding.is_compressed() {
            headers.push((
                b"grpc-encoding".to_vec(),
                self.encoding.as_bytes().to_vec(),
            ));
        }

        if !self.accept_encoding.is_empty() {
            let value = self
                .accept_encoding
                .iter()
                .map(|e| std::str::from_utf8(e.as_bytes()).unwrap())
                .collect::<Vec<_>>()
                .join(",");
            headers.push((b"grpc-accept-encoding".to_vec(), value.into_bytes()));
        }

        headers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_grpc_content_type() {
        assert!(is_grpc_content_type(b"application/grpc"));
        assert!(is_grpc_content_type(b"application/grpc+proto"));
        assert!(is_grpc_content_type(b"application/grpc+json"));
        assert!(is_grpc_content_type(b"Application/GRPC")); // Case insensitive

        assert!(!is_grpc_content_type(b"application/json"));
        assert!(!is_grpc_content_type(b"text/plain"));
        assert!(!is_grpc_content_type(b""));
    }

    #[test]
    fn test_parse_grpc_timeout() {
        assert_eq!(parse_grpc_timeout(b"10S"), Some(Duration::from_secs(10)));
        assert_eq!(parse_grpc_timeout(b"100m"), Some(Duration::from_millis(100)));
        assert_eq!(parse_grpc_timeout(b"1H"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_grpc_timeout(b"5M"), Some(Duration::from_secs(300)));
        assert_eq!(parse_grpc_timeout(b"1000u"), Some(Duration::from_micros(1000)));
        assert_eq!(parse_grpc_timeout(b"1000000n"), Some(Duration::from_nanos(1_000_000)));

        assert_eq!(parse_grpc_timeout(b""), None);
        assert_eq!(parse_grpc_timeout(b"10X"), None);
        assert_eq!(parse_grpc_timeout(b"abc"), None);
    }

    #[test]
    fn test_format_grpc_timeout() {
        assert_eq!(format_grpc_timeout(Duration::from_secs(3600)), "1H");
        assert_eq!(format_grpc_timeout(Duration::from_secs(60)), "1M");
        assert_eq!(format_grpc_timeout(Duration::from_secs(10)), "10S");
        assert_eq!(format_grpc_timeout(Duration::from_millis(100)), "100m");
        assert_eq!(format_grpc_timeout(Duration::from_micros(500)), "500u");
    }

    #[test]
    fn test_grpc_headers_parsing() {
        let headers: Vec<(&[u8], &[u8])> = vec![
            (b":path", b"/package.Service/Method"),
            (b":authority", b"localhost:50051"),
            (b"content-type", b"application/grpc+proto"),
            (b"grpc-timeout", b"30S"),
            (b"grpc-encoding", b"gzip"),
            (b"grpc-accept-encoding", b"gzip,identity"),
        ];

        let parsed = GrpcHeaders::from_headers(&headers);

        assert_eq!(parsed.timeout, Some(Duration::from_secs(30)));
        assert_eq!(parsed.encoding, GrpcEncoding::Gzip);
        assert_eq!(parsed.accept_encoding.len(), 2);
        assert_eq!(parsed.path.as_deref(), Some("/package.Service/Method"));
        assert_eq!(parsed.authority.as_deref(), Some("localhost:50051"));
        assert_eq!(parsed.service_name(), Some("package.Service"));
        assert_eq!(parsed.method_name(), Some("Method"));
    }

    #[test]
    fn test_grpc_encoding() {
        assert_eq!(GrpcEncoding::from_bytes(b"identity"), Some(GrpcEncoding::Identity));
        assert_eq!(GrpcEncoding::from_bytes(b"gzip"), Some(GrpcEncoding::Gzip));
        assert_eq!(GrpcEncoding::from_bytes(b"GZIP"), Some(GrpcEncoding::Gzip)); // Case insensitive
        assert_eq!(GrpcEncoding::from_bytes(b"deflate"), Some(GrpcEncoding::Deflate));
        assert_eq!(GrpcEncoding::from_bytes(b"unknown"), None);

        assert!(!GrpcEncoding::Identity.is_compressed());
        assert!(GrpcEncoding::Gzip.is_compressed());
    }

    #[test]
    fn test_is_grpc_request() {
        let grpc_headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (b"content-type".to_vec(), b"application/grpc".to_vec()),
        ];
        assert!(is_grpc_request(&grpc_headers));

        let http_headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (b"content-type".to_vec(), b"application/json".to_vec()),
        ];
        assert!(!is_grpc_request(&http_headers));
    }
}
