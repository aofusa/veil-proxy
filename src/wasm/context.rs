//! HTTP Context for Proxy-Wasm
//!
//! Holds the state for a single HTTP request/response processing.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use super::capabilities::ModuleCapabilities;
use super::types::{HttpCallResponse, LocalResponse, Metric, PendingHttpCall};

/// HTTP context for a single request
pub struct HttpContext {
    // === Context IDs ===
    /// Unique context ID
    pub context_id: i32,
    /// Root context ID
    pub root_context_id: i32,

    // === Request ===
    /// Request headers
    pub request_headers: Vec<(String, String)>,
    /// Request body
    pub request_body: Vec<u8>,
    /// Request trailers
    pub request_trailers: Vec<(String, String)>,
    /// Request path
    pub request_path: String,
    /// Request method
    pub request_method: String,
    /// Request query string
    pub request_query: String,
    /// Is request body complete
    pub request_body_complete: bool,

    // === Response ===
    /// Response status code
    pub response_status: u16,
    /// Response headers
    pub response_headers: Vec<(String, String)>,
    /// Response body
    pub response_body: Vec<u8>,
    /// Response trailers
    pub response_trailers: Vec<(String, String)>,
    /// Is response body complete
    pub response_body_complete: bool,

    // === Metadata ===
    /// Client IP address
    pub client_ip: String,
    /// Plugin name
    pub plugin_name: String,
    /// Plugin configuration
    pub plugin_configuration: Vec<u8>,
    /// VM configuration
    pub vm_configuration: Vec<u8>,

    // === Modification Flags ===
    /// Request headers modified
    pub request_headers_modified: bool,
    /// Request body modified
    pub request_body_modified: bool,
    /// Response headers modified
    pub response_headers_modified: bool,
    /// Response body modified
    pub response_body_modified: bool,

    // === Local Response ===
    /// Local response to send (if set)
    pub local_response: Option<LocalResponse>,

    // === HTTP Calls ===
    /// Pending HTTP calls
    pub pending_http_calls: HashMap<u32, PendingHttpCall>,
    /// Next HTTP call token
    pub next_http_call_token: u32,
    /// HTTP call responses (token -> response)
    pub http_call_responses: HashMap<u32, HttpCallResponse>,
    /// Current HTTP call token being processed
    pub current_http_call_token: Option<u32>,

    // === Metrics ===
    /// Defined metrics (id -> metric)
    pub metrics: HashMap<i32, Metric>,
    /// Next metric ID
    pub next_metric_id: i32,

    // === Shared Data ===
    /// Shared data (key -> (value, cas))
    pub shared_data: Arc<RwLock<HashMap<String, (Vec<u8>, u32)>>>,
    /// Next CAS value
    pub shared_data_cas: u32,

    // === Custom Properties ===
    /// User-defined properties (set via proxy_set_property)
    pub custom_properties: HashMap<String, Vec<u8>>,

    // === Capabilities ===
    /// Module capabilities
    pub capabilities: ModuleCapabilities,

    // === Timer ===
    /// Tick period in milliseconds (0 = disabled)
    pub tick_period_ms: u32,

    // === gRPC Calls (feature = "grpc") ===
    /// Pending gRPC calls (call_id -> (path, message, timeout_ms))
    #[cfg(feature = "grpc")]
    pub pending_grpc_calls: HashMap<u32, (String, Vec<u8>, u32)>,
    /// Next gRPC call ID
    #[cfg(feature = "grpc")]
    pub next_grpc_call_id: u32,
    /// Cancelled gRPC call IDs
    #[cfg(feature = "grpc")]
    pub cancelled_grpc_calls: std::collections::HashSet<u32>,

    // === gRPC Streams (feature = "grpc") ===
    /// Active gRPC streams (stream_id -> GrpcStream)
    #[cfg(feature = "grpc")]
    pub pending_grpc_streams: HashMap<u32, GrpcStream>,
    /// Next gRPC stream ID
    #[cfg(feature = "grpc")]
    pub next_grpc_stream_id: u32,
}

/// gRPC stream state
#[cfg(feature = "grpc")]
#[derive(Debug, Clone)]
pub struct GrpcStream {
    /// Stream ID
    pub stream_id: u32,
    /// Upstream service name
    pub upstream: String,
    /// gRPC service name
    pub service: String,
    /// gRPC method name
    pub method: String,
    /// Stream state
    pub state: GrpcStreamState,
    /// Pending messages to send
    pub pending_messages: Vec<Vec<u8>>,
    /// Initial metadata
    pub initial_metadata: Vec<(String, String)>,
}

/// gRPC stream state
#[cfg(feature = "grpc")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrpcStreamState {
    /// Stream is open for bidirectional communication
    Open,
    /// Client has closed their send side (half-closed)
    HalfClosed,
    /// Stream is fully closed
    Closed,
}

impl HttpContext {
    /// Create a new HTTP context
    pub fn new(context_id: i32, capabilities: ModuleCapabilities) -> Self {
        Self {
            context_id,
            root_context_id: 0,
            request_headers: Vec::new(),
            request_body: Vec::new(),
            request_trailers: Vec::new(),
            request_path: String::new(),
            request_method: String::new(),
            request_query: String::new(),
            request_body_complete: false,
            response_status: 0,
            response_headers: Vec::new(),
            response_body: Vec::new(),
            response_trailers: Vec::new(),
            response_body_complete: false,
            client_ip: String::new(),
            plugin_name: String::new(),
            plugin_configuration: Vec::new(),
            vm_configuration: Vec::new(),
            request_headers_modified: false,
            request_body_modified: false,
            response_headers_modified: false,
            response_body_modified: false,
            local_response: None,
            pending_http_calls: HashMap::new(),
            next_http_call_token: 1,
            http_call_responses: HashMap::new(),
            current_http_call_token: None,
            metrics: HashMap::new(),
            next_metric_id: 1,
            shared_data: Arc::new(RwLock::new(HashMap::new())),
            shared_data_cas: 1,
            custom_properties: HashMap::new(),
            capabilities,
            tick_period_ms: 0,
            #[cfg(feature = "grpc")]
            pending_grpc_calls: HashMap::new(),
            #[cfg(feature = "grpc")]
            next_grpc_call_id: 1,
            #[cfg(feature = "grpc")]
            cancelled_grpc_calls: std::collections::HashSet::new(),
            #[cfg(feature = "grpc")]
            pending_grpc_streams: HashMap::new(),
            #[cfg(feature = "grpc")]
            next_grpc_stream_id: 1,
        }
    }

    /// Set request data
    pub fn set_request(
        &mut self,
        method: &str,
        path: &str,
        headers: Vec<(String, String)>,
        client_ip: &str,
    ) {
        self.request_method = method.to_string();
        self.request_path = path.to_string();
        self.request_headers = headers;
        self.client_ip = client_ip.to_string();

        // Extract query string
        if let Some(pos) = path.find('?') {
            self.request_query = path[pos + 1..].to_string();
        }
    }

    /// Set request body
    pub fn set_request_body(&mut self, body: Vec<u8>, complete: bool) {
        self.request_body = body;
        self.request_body_complete = complete;
    }

    /// Set response data
    pub fn set_response(&mut self, status: u16, headers: Vec<(String, String)>) {
        self.response_status = status;
        self.response_headers = headers;
    }

    /// Set response body
    pub fn set_response_body(&mut self, body: Vec<u8>, complete: bool) {
        self.response_body = body;
        self.response_body_complete = complete;
    }

    /// Get next HTTP call token
    pub fn allocate_http_call_token(&mut self) -> u32 {
        let token = self.next_http_call_token;
        self.next_http_call_token += 1;
        token
    }

    /// Get next metric ID
    pub fn allocate_metric_id(&mut self) -> i32 {
        let id = self.next_metric_id;
        self.next_metric_id += 1;
        id
    }

    /// Check if request headers are modified
    pub fn has_request_modifications(&self) -> bool {
        self.request_headers_modified || self.request_body_modified
    }

    /// Check if response headers are modified
    pub fn has_response_modifications(&self) -> bool {
        self.response_headers_modified || self.response_body_modified
    }

    /// Check if local response should be sent
    pub fn should_send_local_response(&self) -> bool {
        self.local_response.is_some()
    }

    /// Get next gRPC call ID
    #[cfg(feature = "grpc")]
    pub fn next_grpc_call_id(&mut self) -> u32 {
        let id = self.next_grpc_call_id;
        self.next_grpc_call_id += 1;
        id
    }

    /// Register a pending gRPC call
    #[cfg(feature = "grpc")]
    pub fn register_grpc_call(&mut self, call_id: u32, path: String, message: Vec<u8>, timeout_ms: u32) {
        self.pending_grpc_calls.insert(call_id, (path, message, timeout_ms));
    }

    /// Cancel a gRPC call
    #[cfg(feature = "grpc")]
    pub fn cancel_grpc_call(&mut self, call_id: u32) -> bool {
        if self.pending_grpc_calls.remove(&call_id).is_some() {
            self.cancelled_grpc_calls.insert(call_id);
            true
        } else {
            false
        }
    }

    /// Take pending HTTP calls for execution
    pub fn take_pending_http_calls(&mut self) -> HashMap<u32, crate::wasm::types::PendingHttpCall> {
        std::mem::take(&mut self.pending_http_calls)
    }

    /// Check if there are pending HTTP calls
    pub fn has_pending_http_calls(&self) -> bool {
        !self.pending_http_calls.is_empty()
    }

    /// Take pending gRPC calls for execution
    #[cfg(feature = "grpc")]
    pub fn take_pending_grpc_calls(&mut self) -> HashMap<u32, (String, Vec<u8>, u32)> {
        std::mem::take(&mut self.pending_grpc_calls)
    }
}

/// Host state for Wasmtime
pub struct HostState {
    /// HTTP context
    pub http_ctx: HttpContext,
}

impl HostState {
    /// Create a new host state
    pub fn new(http_ctx: HttpContext) -> Self {
        Self { http_ctx }
    }
}
