//! gRPC Integration Helpers
//!
//! Provides helper functions for integrating gRPC callbacks
//! with the WASM filter engine.

use std::sync::Arc;

use super::engine::FilterEngine;

/// gRPC call response data
#[cfg(feature = "grpc")]
#[derive(Debug, Clone)]
pub struct GrpcCallResponse {
    /// Module name that initiated the call
    pub module_name: String,
    /// Call ID returned by proxy_grpc_call
    pub call_id: u32,
    /// gRPC status code (0 = OK)
    pub status_code: i32,
    /// Initial metadata from the server
    pub initial_metadata: Vec<(String, String)>,
    /// Response message bytes
    pub message: Vec<u8>,
    /// Trailing metadata from the server
    pub trailing_metadata: Vec<(String, String)>,
}

/// Notify a WASM module of gRPC initial metadata
/// 
/// Called when the gRPC server sends initial metadata (headers).
#[cfg(feature = "grpc")]
pub fn on_grpc_initial_metadata(
    engine: &Arc<FilterEngine>,
    module_name: &str,
    call_id: u32,
    headers: &[(String, String)],
) {
    engine.on_grpc_receive_initial_metadata(module_name, call_id, headers);
}

/// Notify a WASM module of a gRPC message
/// 
/// Called when the gRPC server sends a response message.
#[cfg(feature = "grpc")]
pub fn on_grpc_message(
    engine: &Arc<FilterEngine>,
    module_name: &str,
    call_id: u32,
    message: &[u8],
) {
    engine.on_grpc_receive(module_name, call_id, message);
}

/// Notify a WASM module of gRPC trailing metadata
/// 
/// Called when the gRPC server sends trailing metadata (trailers).
#[cfg(feature = "grpc")]
pub fn on_grpc_trailing_metadata(
    engine: &Arc<FilterEngine>,
    module_name: &str,
    call_id: u32,
    trailers: &[(String, String)],
) {
    engine.on_grpc_receive_trailing_metadata(module_name, call_id, trailers);
}

/// Notify a WASM module that a gRPC call has closed
/// 
/// Called when the gRPC call completes or fails.
#[cfg(feature = "grpc")]
pub fn on_grpc_close(
    engine: &Arc<FilterEngine>,
    module_name: &str,
    call_id: u32,
    status_code: i32,
) {
    engine.on_grpc_close(module_name, call_id, status_code);
}

/// Process a complete gRPC response
/// 
/// Convenience function that calls all gRPC callbacks in order:
/// 1. Initial metadata
/// 2. Message
/// 3. Trailing metadata
/// 4. Close
#[cfg(feature = "grpc")]
pub fn process_grpc_response(
    engine: &Arc<FilterEngine>,
    response: GrpcCallResponse,
) {
    // 1. Initial metadata
    if !response.initial_metadata.is_empty() {
        on_grpc_initial_metadata(
            engine,
            &response.module_name,
            response.call_id,
            &response.initial_metadata,
        );
    }
    
    // 2. Message
    if !response.message.is_empty() {
        on_grpc_message(
            engine,
            &response.module_name,
            response.call_id,
            &response.message,
        );
    }
    
    // 3. Trailing metadata
    if !response.trailing_metadata.is_empty() {
        on_grpc_trailing_metadata(
            engine,
            &response.module_name,
            response.call_id,
            &response.trailing_metadata,
        );
    }
    
    // 4. Close
    on_grpc_close(
        engine,
        &response.module_name,
        response.call_id,
        response.status_code,
    );
}

/// gRPC status codes
#[cfg(feature = "grpc")]
pub mod status {
    /// OK - The call completed successfully
    pub const OK: i32 = 0;
    /// Cancelled - The operation was cancelled
    pub const CANCELLED: i32 = 1;
    /// Unknown - Unknown error
    pub const UNKNOWN: i32 = 2;
    /// InvalidArgument - Invalid argument was provided
    pub const INVALID_ARGUMENT: i32 = 3;
    /// DeadlineExceeded - Deadline expired before operation could complete
    pub const DEADLINE_EXCEEDED: i32 = 4;
    /// NotFound - Some requested entity was not found
    pub const NOT_FOUND: i32 = 5;
    /// AlreadyExists - Entity that we attempted to create already exists
    pub const ALREADY_EXISTS: i32 = 6;
    /// PermissionDenied - Caller does not have permission
    pub const PERMISSION_DENIED: i32 = 7;
    /// ResourceExhausted - Some resource has been exhausted
    pub const RESOURCE_EXHAUSTED: i32 = 8;
    /// FailedPrecondition - System is not in correct state
    pub const FAILED_PRECONDITION: i32 = 9;
    /// Aborted - The operation was aborted
    pub const ABORTED: i32 = 10;
    /// OutOfRange - Operation was attempted past the valid range
    pub const OUT_OF_RANGE: i32 = 11;
    /// Unimplemented - Operation is not implemented
    pub const UNIMPLEMENTED: i32 = 12;
    /// Internal - Internal errors
    pub const INTERNAL: i32 = 13;
    /// Unavailable - The service is currently unavailable
    pub const UNAVAILABLE: i32 = 14;
    /// DataLoss - Unrecoverable data loss or corruption
    pub const DATA_LOSS: i32 = 15;
    /// Unauthenticated - Request does not have valid authentication credentials
    pub const UNAUTHENTICATED: i32 = 16;
}

#[cfg(all(test, feature = "grpc"))]
mod tests {
    use super::*;
    
    #[test]
    fn test_grpc_call_response_creation() {
        let response = GrpcCallResponse {
            module_name: "grpc_module".to_string(),
            call_id: 1,
            status_code: status::OK,
            initial_metadata: vec![
                ("content-type".to_string(), "application/grpc".to_string()),
            ],
            message: b"response data".to_vec(),
            trailing_metadata: vec![
                ("grpc-status".to_string(), "0".to_string()),
            ],
        };
        
        assert_eq!(response.module_name, "grpc_module");
        assert_eq!(response.call_id, 1);
        assert_eq!(response.status_code, 0);
        assert!(!response.message.is_empty());
    }
    
    #[test]
    fn test_grpc_status_codes() {
        assert_eq!(status::OK, 0);
        assert_eq!(status::CANCELLED, 1);
        assert_eq!(status::UNKNOWN, 2);
        assert_eq!(status::INVALID_ARGUMENT, 3);
        assert_eq!(status::DEADLINE_EXCEEDED, 4);
        assert_eq!(status::NOT_FOUND, 5);
        assert_eq!(status::UNAVAILABLE, 14);
        assert_eq!(status::UNAUTHENTICATED, 16);
    }
}

// Non-grpc feature stubs for compilation
#[cfg(not(feature = "grpc"))]
pub fn on_grpc_initial_metadata(
    _engine: &Arc<FilterEngine>,
    _module_name: &str,
    _call_id: u32,
    _headers: &[(String, String)],
) {
    // No-op when grpc feature is disabled
}

#[cfg(not(feature = "grpc"))]
pub fn on_grpc_message(
    _engine: &Arc<FilterEngine>,
    _module_name: &str,
    _call_id: u32,
    _message: &[u8],
) {
    // No-op when grpc feature is disabled
}

#[cfg(not(feature = "grpc"))]
pub fn on_grpc_trailing_metadata(
    _engine: &Arc<FilterEngine>,
    _module_name: &str,
    _call_id: u32,
    _trailers: &[(String, String)],
) {
    // No-op when grpc feature is disabled
}

#[cfg(not(feature = "grpc"))]
pub fn on_grpc_close(
    _engine: &Arc<FilterEngine>,
    _module_name: &str,
    _call_id: u32,
    _status_code: i32,
) {
    // No-op when grpc feature is disabled
}
