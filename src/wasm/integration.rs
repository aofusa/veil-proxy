//! WASM Integration Helpers
//!
//! Provides helper functions for integrating WASM lifecycle callbacks
//! into the proxy request handling flow.

use std::sync::Arc;

use super::engine::FilterEngine;
use super::types::HttpCallResponse;
use crate::wasm_plugin_config::ModuleRef;

/// Execute on_log callback for WASM modules at the end of request processing
///
/// This should be called after the response has been sent and access logging
/// is about to occur. It allows WASM modules to perform final processing
/// or logging before the request context is destroyed.
///
/// # Arguments
/// * `engine` - The WASM filter engine
/// * `modules` - List of module references to invoke
pub fn on_request_complete(engine: &Arc<FilterEngine>, modules: &[ModuleRef]) {
    if modules.is_empty() {
        return;
    }

    futures::executor::block_on(engine.on_log_with_modules(modules, None));
}

/// Execute on_log callback for WASM modules at the end of request processing ASYNCHRONOUSLY
/// 空のモジュールリスト（F-43: リクエストごとの確保排除）。F-321: スレッドに 1 個（プロセスで 1 個の `Arc` だと、
/// Wasm の無いルートの要求ごとに全ワーカーが同じ参照カウントへ書く）
pub fn empty_wasm_modules() -> crate::wasm_plugin_config::WasmModules {
    thread_local! {
        static EMPTY: Arc<Vec<ModuleRef>> = Arc::new(Vec::new());
    }
    crate::wasm_plugin_config::WasmModules::shared(EMPTY.with(Arc::clone))
}

/// 要求の終わり（`on_log`）。`instance_reuse` のモジュールはここで要求の http context を消す（B-177）。
/// エンジンは要求の並びが持つもの（要求の入口で置いた。B-501）
pub async fn on_request_complete_async(modules: &crate::wasm_plugin_config::WasmModules) {
    if modules.is_empty() {
        return;
    }
    if let Some(engine) = modules.engine() {
        engine.on_log_with_modules_async(modules).await;
    }
}

/// ワーカーのエンジンの写し（B-501）。要求の並びはこれを `Rc` で持つ（要求ごとに全ワーカー共有の `Arc` の
/// 参照カウントへ書かない。F-321 と同じ）。元の `Arc` を持っておくので、エンジンが差し替わるまで同じ番地が
/// 別のエンジンに使い回されることはない
pub struct WorkerEngine(Arc<FilterEngine>);

impl WorkerEngine {
    /// エンジン
    #[inline]
    pub fn engine(&self) -> &Arc<FilterEngine> {
        &self.0
    }
}

thread_local! {
    static WORKER_ENGINE: std::cell::RefCell<Option<std::rc::Rc<WorkerEngine>>> =
        const { std::cell::RefCell::new(None) };
}

/// `engine`（今の設定のエンジン）のワーカーの写し（B-501）。設定が差し替わってエンジンが変わったら写し直す
pub fn worker_engine(engine: &Arc<FilterEngine>) -> std::rc::Rc<WorkerEngine> {
    WORKER_ENGINE.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(rc) = c.as_ref() {
            if Arc::ptr_eq(&rc.0, engine) {
                return std::rc::Rc::clone(rc);
            }
        }
        let rc = std::rc::Rc::new(WorkerEngine(Arc::clone(engine)));
        *c = Some(std::rc::Rc::clone(&rc));
        rc
    })
}

/// ワーカーのエンジンの写しを手放す（ワーカーのスレッドの終わりで、ランタイムが生きているうちに呼ぶ。
/// スレッドローカルの破棄の中でエンジンを落とさない。B-118 と同じ）
pub fn release_worker_engine() {
    let _ = WORKER_ENGINE.try_with(|c| {
        let old = c.borrow_mut().take();
        drop(old);
    });
}

/// ワーカーのエンジンの写しが `current`（今の設定のエンジン）でなければ手放す（ワーカーのアイドル点で呼ぶ。
/// リロードの後に Wasm の要求の来ないワーカーが古いエンジンを持ち続けない）
pub fn release_stale_worker_engine(current: Option<&Arc<FilterEngine>>) {
    let stale = WORKER_ENGINE
        .try_with(|c| {
            c.borrow()
                .as_ref()
                .is_some_and(|rc| !current.is_some_and(|cur| Arc::ptr_eq(&rc.0, cur)))
        })
        .unwrap_or(false);
    if stale {
        release_worker_engine();
    }
}

/// Execute on_done callback for WASM modules when request context is destroyed
///
/// This should be called after all request processing is complete.
/// Returns true if any module requested to keep the context alive
/// (for pending async operations).
///
/// # Arguments
/// * `engine` - The WASM filter engine
/// * `modules` - List of module references to invoke
///
/// # Returns
/// `true` if any module wants to keep the context alive
pub fn on_context_destroy(engine: &Arc<FilterEngine>, modules: &[ModuleRef]) -> bool {
    if modules.is_empty() {
        return false;
    }

    futures::executor::block_on(engine.on_done_with_modules(modules))
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
pub fn on_http_call_complete(engine: &Arc<FilterEngine>, result: WasmHttpCallResult) {
    let _ = futures::executor::block_on(engine.on_http_call_response(
        &result.module_name,
        result.token,
        result.response,
    ));
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
pub fn on_tick(engine: &Arc<FilterEngine>, module_name: &str) {
    futures::executor::block_on(engine.on_tick(module_name));
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
pub fn on_queue_ready(engine: &Arc<FilterEngine>, module_name: &str, queue_id: u32) {
    futures::executor::block_on(engine.on_queue_ready(module_name, queue_id));
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
pub fn process_pending_http_calls<F>(engine: &Arc<FilterEngine>, http_executor: F) -> usize
where
    F: Fn(&super::persistent_context::PendingHttpCallWithContext) -> Option<HttpCallResponse>,
{
    use super::persistent_context::{
        deliver_http_call_response, take_all_pending_http_calls, take_context,
    };

    let pending_calls = take_all_pending_http_calls();
    let count = pending_calls.len();

    for pending in pending_calls {
        // Execute the HTTP call
        if let Some(response) = http_executor(&pending) {
            // Deliver response to context
            deliver_http_call_response(pending.context_id, pending.token, response.clone());

            // Take the context and call on_http_call_response
            if let Some(_stored) = take_context(pending.context_id) {
                let _ = futures::executor::block_on(engine.on_http_call_response(
                    &pending.module_name,
                    pending.token,
                    response,
                ));
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
        let _ = futures::executor::block_on(engine.on_http_call_response(
            &stored.module_name,
            token,
            response,
        ));
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
                headers: vec![(b"content-type".to_vec(), b"application/json".to_vec())],
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
