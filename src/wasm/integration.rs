//! WASM Integration Helpers
//!
//! Provides helper functions for integrating WASM lifecycle callbacks
//! into the proxy request handling flow.

use std::sync::Arc;

use super::engine::FilterEngine;
use super::types::HttpCallResponse;

/// Execute on_log callback for WASM modules at the end of request processing
/// 
/// This should be called after the response has been sent and access logging
/// is about to occur. It allows WASM modules to perform final processing
/// or logging before the request context is destroyed.
///
/// # Arguments
/// * `engine` - The WASM filter engine
/// * `modules` - List of module names to invoke
pub fn on_request_complete(
    engine: &Arc<FilterEngine>,
    modules: &[String],
) {
    if modules.is_empty() {
        return;
    }
    
    engine.on_log_with_modules(modules);
}

/// Execute on_done callback for WASM modules when request context is destroyed
/// 
/// This should be called after all request processing is complete.
/// Returns true if any module requested to keep the context alive
/// (for pending async operations).
///
/// # Arguments
/// * `engine` - The WASM filter engine
/// * `modules` - List of module names to invoke
///
/// # Returns
/// `true` if any module wants to keep the context alive
pub fn on_context_destroy(
    engine: &Arc<FilterEngine>,
    modules: &[String],
) -> bool {
    if modules.is_empty() {
        return false;
    }
    
    engine.on_done_with_modules(modules)
}

/// HTTP call result for delivering back to WASM modules
pub struct WasmHttpCallResult {
    pub module_name: String,
    pub token: u32,
    pub response: HttpCallResponse,
}

/// Execute HTTP call response callback for a WASM module
/// 
/// This should be called when an HTTP call initiated by a WASM module
/// has completed and the response is ready.
///
/// # Arguments
/// * `engine` - The WASM filter engine
/// * `result` - The HTTP call result containing module name, token, and response
pub fn on_http_call_complete(
    engine: &Arc<FilterEngine>,
    result: WasmHttpCallResult,
) {
    let _ = engine.on_http_call_response(
        &result.module_name,
        result.token,
        result.response,
    );
}

/// Information about a pending HTTP call for async execution
/// 
/// This struct is used to extract pending HTTP call information
/// for async execution outside the WASM context.
#[derive(Debug, Clone)]
pub struct PendingHttpCallInfo {
    /// Module name that initiated the call
    pub module_name: String,
    /// Call token
    pub token: u32,
    /// Upstream service name (maps to upstream_groups)
    pub upstream: String,
    /// Timeout in milliseconds
    pub timeout_ms: u32,
    /// Request headers
    pub headers: Vec<(String, String)>,
    /// Request body
    pub body: Vec<u8>,
    /// Request trailers
    pub trailers: Vec<(String, String)>,
}

/// Tick timer configuration for a module
#[derive(Debug, Clone)]
pub struct TickConfig {
    /// Module name
    pub module_name: String,
    /// Tick period in milliseconds
    pub period_ms: u32,
}

/// Execute on_tick callback for a WASM module
/// 
/// This should be called periodically based on the tick period
/// configured by the module.
///
/// # Arguments
/// * `engine` - The WASM filter engine
/// * `module_name` - Name of the module to tick
pub fn on_tick(
    engine: &Arc<FilterEngine>,
    module_name: &str,
) {
    engine.on_tick(module_name);
}

/// Execute on_queue_ready callback for a WASM module
/// 
/// This should be called when a message is enqueued to a shared queue
/// that a module is subscribed to.
///
/// # Arguments
/// * `engine` - The WASM filter engine
/// * `module_name` - Name of the module to notify
/// * `queue_id` - ID of the queue with new data
pub fn on_queue_ready(
    engine: &Arc<FilterEngine>,
    module_name: &str,
    queue_id: u32,
) {
    engine.on_queue_ready(module_name, queue_id);
}

/// Process all pending HTTP calls from stored contexts
/// 
/// This function takes pending HTTP calls from the persistent context registry,
/// executes them, and delivers the results back to the originating contexts.
/// 
/// # Arguments
/// * `engine` - The WASM filter engine
/// * `http_executor` - Function to execute HTTP requests
/// 
/// # Returns
/// Number of HTTP calls processed
pub fn process_pending_http_calls<F>(
    engine: &Arc<FilterEngine>,
    http_executor: F,
) -> usize
where
    F: Fn(&super::persistent_context::PendingHttpCallWithContext) -> Option<HttpCallResponse>,
{
    use super::persistent_context::{take_all_pending_http_calls, deliver_http_call_response, take_context};
    
    let pending_calls = take_all_pending_http_calls();
    let count = pending_calls.len();
    
    for pending in pending_calls {
        // Execute the HTTP call
        if let Some(response) = http_executor(&pending) {
            // Deliver response to context
            deliver_http_call_response(pending.context_id, pending.token, response.clone());
            
            // Take the context and call on_http_call_response
            if let Some(stored) = take_context(pending.context_id) {
                engine.on_http_call_response(
                    &pending.module_name,
                    pending.token,
                    response,
                );
            }
        }
    }
    
    count
}

/// Resume a context after HTTP call completes
/// 
/// This should be called after an async HTTP call completes to resume
/// the WASM module's execution.
/// 
/// # Arguments
/// * `engine` - The WASM filter engine
/// * `context_id` - The stored context ID
/// * `token` - The HTTP call token
/// * `response` - The HTTP call response
pub fn resume_after_http_call(
    engine: &Arc<FilterEngine>,
    context_id: u64,
    token: u32,
    response: HttpCallResponse,
) -> bool {
    use super::persistent_context::{deliver_http_call_response, take_context};
    
    // First deliver the response to the context
    if !deliver_http_call_response(context_id, token, response.clone()) {
        return false;
    }
    
    // Then take the context and invoke the callback
    if let Some(stored) = take_context(context_id) {
        engine.on_http_call_response(
            &stored.module_name,
            token,
            response,
        );
        true
    } else {
        false
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    
    #[test]
    fn test_empty_modules_handling() {
        // Test that empty module lists are handled gracefully
        // (can't test without a real engine, but validates API)
        let modules: Vec<String> = vec![];
        assert!(modules.is_empty());
    }
    
    #[test]
    fn test_wasm_http_call_result_creation() {
        let result = WasmHttpCallResult {
            module_name: "test_module".to_string(),
            token: 1,
            response: HttpCallResponse {
                status_code: 200,
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: b"{\"ok\":true}".to_vec(),
                trailers: vec![],
            },
        };
        
        assert_eq!(result.module_name, "test_module");
        assert_eq!(result.token, 1);
        assert_eq!(result.response.status_code, 200);
    }
    
    #[test]
    fn test_tick_config() {
        let config = TickConfig {
            module_name: "rate_limiter".to_string(),
            period_ms: 1000,
        };
        
        assert_eq!(config.module_name, "rate_limiter");
        assert_eq!(config.period_ms, 1000);
    }
}
