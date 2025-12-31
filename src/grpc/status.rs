//! gRPC Status Codes (0-16)
//!
//! gRPC uses numeric status codes transmitted via the `grpc-status` trailer.
//! See: https://grpc.github.io/grpc/core/md_doc_statuscodes.html

use std::fmt;

/// gRPC status code values (0-16)
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GrpcStatusCode {
    /// Not an error; returned on success
    Ok = 0,
    /// The operation was cancelled
    Cancelled = 1,
    /// Unknown error
    Unknown = 2,
    /// Client specified an invalid argument
    InvalidArgument = 3,
    /// Deadline expired before operation could complete
    DeadlineExceeded = 4,
    /// Requested entity was not found
    NotFound = 5,
    /// Entity that we attempted to create already exists
    AlreadyExists = 6,
    /// Caller does not have permission
    PermissionDenied = 7,
    /// Resource has been exhausted
    ResourceExhausted = 8,
    /// Operation rejected due to failed precondition
    FailedPrecondition = 9,
    /// Operation was aborted
    Aborted = 10,
    /// Operation was attempted past the valid range
    OutOfRange = 11,
    /// Operation is not implemented
    Unimplemented = 12,
    /// Internal error
    Internal = 13,
    /// Service is currently unavailable
    Unavailable = 14,
    /// Unrecoverable data loss or corruption
    DataLoss = 15,
    /// Request does not have valid authentication credentials
    Unauthenticated = 16,
}

impl GrpcStatusCode {
    /// Create from numeric value
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Ok),
            1 => Some(Self::Cancelled),
            2 => Some(Self::Unknown),
            3 => Some(Self::InvalidArgument),
            4 => Some(Self::DeadlineExceeded),
            5 => Some(Self::NotFound),
            6 => Some(Self::AlreadyExists),
            7 => Some(Self::PermissionDenied),
            8 => Some(Self::ResourceExhausted),
            9 => Some(Self::FailedPrecondition),
            10 => Some(Self::Aborted),
            11 => Some(Self::OutOfRange),
            12 => Some(Self::Unimplemented),
            13 => Some(Self::Internal),
            14 => Some(Self::Unavailable),
            15 => Some(Self::DataLoss),
            16 => Some(Self::Unauthenticated),
            _ => None,
        }
    }

    /// Parse from string (grpc-status header value)
    pub fn from_str(s: &str) -> Option<Self> {
        s.parse::<u8>().ok().and_then(Self::from_u8)
    }

    /// Parse from bytes
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        std::str::from_utf8(bytes).ok().and_then(Self::from_str)
    }

    /// Get numeric value
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Check if this is a success status
    pub fn is_ok(self) -> bool {
        self == Self::Ok
    }

    /// Check if this is an error status
    pub fn is_error(self) -> bool {
        self != Self::Ok
    }

    /// Get description string
    pub fn description(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Cancelled => "Cancelled",
            Self::Unknown => "Unknown",
            Self::InvalidArgument => "Invalid Argument",
            Self::DeadlineExceeded => "Deadline Exceeded",
            Self::NotFound => "Not Found",
            Self::AlreadyExists => "Already Exists",
            Self::PermissionDenied => "Permission Denied",
            Self::ResourceExhausted => "Resource Exhausted",
            Self::FailedPrecondition => "Failed Precondition",
            Self::Aborted => "Aborted",
            Self::OutOfRange => "Out of Range",
            Self::Unimplemented => "Unimplemented",
            Self::Internal => "Internal",
            Self::Unavailable => "Unavailable",
            Self::DataLoss => "Data Loss",
            Self::Unauthenticated => "Unauthenticated",
        }
    }

    /// Convert HTTP status to closest gRPC status
    pub fn from_http_status(http_status: u16) -> Self {
        match http_status {
            200 => Self::Ok,
            400 => Self::InvalidArgument,
            401 => Self::Unauthenticated,
            403 => Self::PermissionDenied,
            404 => Self::NotFound,
            409 => Self::Aborted,
            429 => Self::ResourceExhausted,
            499 => Self::Cancelled,
            500 => Self::Internal,
            501 => Self::Unimplemented,
            503 => Self::Unavailable,
            504 => Self::DeadlineExceeded,
            _ if http_status >= 200 && http_status < 300 => Self::Ok,
            _ if http_status >= 400 && http_status < 500 => Self::InvalidArgument,
            _ => Self::Unknown,
        }
    }

    /// Convert gRPC status to closest HTTP status
    pub fn to_http_status(self) -> u16 {
        match self {
            Self::Ok => 200,
            Self::Cancelled => 499,
            Self::Unknown => 500,
            Self::InvalidArgument => 400,
            Self::DeadlineExceeded => 504,
            Self::NotFound => 404,
            Self::AlreadyExists => 409,
            Self::PermissionDenied => 403,
            Self::ResourceExhausted => 429,
            Self::FailedPrecondition => 400,
            Self::Aborted => 409,
            Self::OutOfRange => 400,
            Self::Unimplemented => 501,
            Self::Internal => 500,
            Self::Unavailable => 503,
            Self::DataLoss => 500,
            Self::Unauthenticated => 401,
        }
    }
}

impl fmt::Display for GrpcStatusCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_u8())
    }
}

impl Default for GrpcStatusCode {
    fn default() -> Self {
        Self::Ok
    }
}

/// Complete gRPC status with optional message and details
#[derive(Debug, Clone, Default)]
pub struct GrpcStatus {
    /// Status code
    pub code: GrpcStatusCode,
    /// Human-readable error message (grpc-message header)
    pub message: Option<String>,
    /// Binary status details (google.rpc.Status, optional)
    pub details: Option<Vec<u8>>,
}

impl GrpcStatus {
    /// Create success status
    pub fn ok() -> Self {
        Self {
            code: GrpcStatusCode::Ok,
            message: None,
            details: None,
        }
    }

    /// Create error status with message
    pub fn error(code: GrpcStatusCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: Some(message.into()),
            details: None,
        }
    }

    /// Create error status from code only
    pub fn from_code(code: GrpcStatusCode) -> Self {
        Self {
            code,
            message: None,
            details: None,
        }
    }

    /// Check if success
    pub fn is_ok(&self) -> bool {
        self.code.is_ok()
    }

    /// URL-encode the message for grpc-message header
    pub fn encoded_message(&self) -> Option<String> {
        self.message.as_ref().map(|m| {
            // Percent-encode per gRPC spec
            m.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                        c.to_string()
                    } else {
                        format!("%{:02X}", c as u32)
                    }
                })
                .collect()
        })
    }

    /// Decode URL-encoded grpc-message
    pub fn decode_message(encoded: &[u8]) -> Option<String> {
        let s = std::str::from_utf8(encoded).ok()?;
        let mut result = String::new();
        let mut chars = s.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '%' {
                let hex: String = chars.by_ref().take(2).collect();
                if hex.len() == 2 {
                    if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                        result.push(byte as char);
                    }
                }
            } else {
                result.push(c);
            }
        }

        Some(result)
    }

    /// Format as trailers for HTTP/2 response
    pub fn to_trailers(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut trailers = vec![(
            b"grpc-status".to_vec(),
            self.code.to_string().into_bytes(),
        )];

        if let Some(msg) = &self.encoded_message() {
            trailers.push((b"grpc-message".to_vec(), msg.as_bytes().to_vec()));
        }

        trailers
    }
}

impl From<GrpcStatusCode> for GrpcStatus {
    fn from(code: GrpcStatusCode) -> Self {
        Self::from_code(code)
    }
}

impl fmt::Display for GrpcStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.code.description(), self.code)?;
        if let Some(msg) = &self.message {
            write!(f, ": {}", msg)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_code_roundtrip() {
        for i in 0..=16 {
            let code = GrpcStatusCode::from_u8(i).unwrap();
            assert_eq!(code.as_u8(), i);
        }
    }

    #[test]
    fn test_status_code_from_str() {
        assert_eq!(GrpcStatusCode::from_str("0"), Some(GrpcStatusCode::Ok));
        assert_eq!(GrpcStatusCode::from_str("13"), Some(GrpcStatusCode::Internal));
        assert_eq!(GrpcStatusCode::from_str("invalid"), None);
        assert_eq!(GrpcStatusCode::from_str("99"), None);
    }

    #[test]
    fn test_http_status_conversion() {
        assert_eq!(GrpcStatusCode::from_http_status(200), GrpcStatusCode::Ok);
        assert_eq!(GrpcStatusCode::from_http_status(404), GrpcStatusCode::NotFound);
        assert_eq!(GrpcStatusCode::from_http_status(500), GrpcStatusCode::Internal);

        assert_eq!(GrpcStatusCode::Ok.to_http_status(), 200);
        assert_eq!(GrpcStatusCode::NotFound.to_http_status(), 404);
        assert_eq!(GrpcStatusCode::Internal.to_http_status(), 500);
    }

    #[test]
    fn test_grpc_status_encoding() {
        let status = GrpcStatus::error(GrpcStatusCode::NotFound, "Resource not found");
        let trailers = status.to_trailers();

        assert_eq!(trailers.len(), 2);
        assert_eq!(trailers[0].0, b"grpc-status");
        assert_eq!(trailers[0].1, b"5");
        assert_eq!(trailers[1].0, b"grpc-message");
    }

    #[test]
    fn test_message_url_encoding() {
        let status = GrpcStatus::error(GrpcStatusCode::InvalidArgument, "Invalid: name");
        let encoded = status.encoded_message().unwrap();
        assert!(encoded.contains("%3A")); // : encoded

        // Decode back
        let decoded = GrpcStatus::decode_message(encoded.as_bytes()).unwrap();
        assert_eq!(decoded, "Invalid: name");
    }

    #[test]
    fn test_ok_status() {
        let status = GrpcStatus::ok();
        assert!(status.is_ok());
        assert_eq!(status.code, GrpcStatusCode::Ok);
        assert!(status.message.is_none());
    }
}
