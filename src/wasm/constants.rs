//! Proxy-Wasm ABI Constants
//!
//! Constants defined by Proxy-Wasm ABI v0.2.1

// ============================================================
// proxy_status_t - Return status for host functions
// ============================================================

/// Success
pub const PROXY_RESULT_OK: i32 = 0;
/// Key/value not found
pub const PROXY_RESULT_NOT_FOUND: i32 = 1;
/// Invalid argument
pub const PROXY_RESULT_BAD_ARGUMENT: i32 = 2;
/// Serialization error
pub const PROXY_RESULT_SERIALIZATION_FAILURE: i32 = 3;
/// Parse error
pub const PROXY_RESULT_PARSE_FAILURE: i32 = 4;
/// Expression evaluation failed
pub const PROXY_RESULT_BAD_EXPRESSION: i32 = 5;
/// Invalid memory access
pub const PROXY_RESULT_INVALID_MEMORY_ACCESS: i32 = 6;
/// Empty result
pub const PROXY_RESULT_EMPTY: i32 = 7;
/// Compare-and-swap mismatch
pub const PROXY_RESULT_CAS_MISMATCH: i32 = 8;
/// Returned result is too large
pub const PROXY_RESULT_RESULT_MISMATCH: i32 = 9;
/// Internal error
pub const PROXY_RESULT_INTERNAL_FAILURE: i32 = 10;
/// Unimplemented
pub const PROXY_RESULT_UNIMPLEMENTED: i32 = 12;
/// Operation not allowed (capability denied)
pub const PROXY_RESULT_NOT_ALLOWED: i32 = 13;

// ============================================================
// proxy_action_t - Action returned by callbacks
// ============================================================

/// Continue processing
pub const ACTION_CONTINUE: i32 = 0;
/// Pause processing
pub const ACTION_PAUSE: i32 = 1;

// ============================================================
// proxy_log_level_t - Log levels
// ============================================================

pub const LOG_TRACE: i32 = 0;
pub const LOG_DEBUG: i32 = 1;
pub const LOG_INFO: i32 = 2;
pub const LOG_WARN: i32 = 3;
pub const LOG_ERROR: i32 = 4;
pub const LOG_CRITICAL: i32 = 5;

// ============================================================
// proxy_map_type_t - Header map types
// ============================================================

/// Request headers from downstream
pub const HTTP_REQUEST_HEADERS: i32 = 0;
/// Request trailers from downstream
pub const HTTP_REQUEST_TRAILERS: i32 = 1;
/// Response headers from upstream
pub const HTTP_RESPONSE_HEADERS: i32 = 2;
/// Response trailers from upstream
pub const HTTP_RESPONSE_TRAILERS: i32 = 3;
/// gRPC receive initial metadata (not supported)
pub const GRPC_RECEIVE_INITIAL_METADATA: i32 = 4;
/// gRPC receive trailing metadata (not supported)
pub const GRPC_RECEIVE_TRAILING_METADATA: i32 = 5;
/// HTTP call response headers
pub const HTTP_CALL_RESPONSE_HEADERS: i32 = 6;
/// HTTP call response trailers
pub const HTTP_CALL_RESPONSE_TRAILERS: i32 = 7;

// ============================================================
// proxy_buffer_type_t - Buffer types
// ============================================================

/// Request body from downstream
pub const HTTP_REQUEST_BODY: i32 = 0;
/// Response body from upstream
pub const HTTP_RESPONSE_BODY: i32 = 1;
/// HTTP call response body
pub const HTTP_CALL_RESPONSE_BODY: i32 = 2;
/// gRPC receive buffer (not supported)
pub const GRPC_RECEIVE_BUFFER: i32 = 3;
/// Plugin configuration
pub const PLUGIN_CONFIGURATION: i32 = 4;
/// VM configuration
pub const VM_CONFIGURATION: i32 = 5;

// ============================================================
// proxy_stream_type_t - Stream types
// ============================================================

/// HTTP request stream
pub const STREAM_TYPE_HTTP_REQUEST: i32 = 0;
/// HTTP response stream
pub const STREAM_TYPE_HTTP_RESPONSE: i32 = 1;
/// Downstream connection
pub const STREAM_TYPE_DOWNSTREAM: i32 = 2;
/// Upstream connection
pub const STREAM_TYPE_UPSTREAM: i32 = 3;

// ============================================================
// proxy_metric_type_t - Metric types
// ============================================================

/// Counter metric (cumulative)
pub const METRIC_TYPE_COUNTER: i32 = 0;
/// Gauge metric (current value)
pub const METRIC_TYPE_GAUGE: i32 = 1;
/// Histogram metric
pub const METRIC_TYPE_HISTOGRAM: i32 = 2;
