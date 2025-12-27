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

    // === Capabilities ===
    /// Module capabilities
    pub capabilities: ModuleCapabilities,

    // === Timer ===
    /// Tick period in milliseconds (0 = disabled)
    pub tick_period_ms: u32,
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
            capabilities,
            tick_period_ms: 0,
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
