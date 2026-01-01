//! Persistent Context Registry for Async HTTP Call Support
//!
//! Stores HttpContext instances across callback invocations to support
//! async operations like HTTP calls.

use std::collections::HashMap;
use std::sync::{atomic::AtomicU64, atomic::Ordering, RwLock};

use once_cell::sync::Lazy;

use super::context::HttpContext;
use super::types::PendingHttpCall;

/// Global context registry for persistent contexts
static CONTEXT_REGISTRY: Lazy<RwLock<ContextRegistry>> =
    Lazy::new(|| RwLock::new(ContextRegistry::new()));

/// Next context ID counter
static NEXT_CONTEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Registry for storing persistent HttpContext instances
pub struct ContextRegistry {
    /// Map from context_id to (module_name, HttpContext)
    contexts: HashMap<u64, StoredContext>,
}

/// A stored context with metadata
pub struct StoredContext {
    /// Module name that owns this context
    pub module_name: String,
    /// The stored HttpContext
    pub context: HttpContext,
    /// Whether this context has pending async operations
    pub has_pending_calls: bool,
}

impl ContextRegistry {
    fn new() -> Self {
        Self {
            contexts: HashMap::new(),
        }
    }
}

/// Allocate a new unique context ID
pub fn allocate_context_id() -> u64 {
    NEXT_CONTEXT_ID.fetch_add(1, Ordering::SeqCst)
}

/// Store a context for later retrieval
///
/// Returns the context ID assigned to the stored context.
pub fn store_context(module_name: &str, context: HttpContext) -> u64 {
    let context_id = allocate_context_id();
    let has_pending = !context.pending_http_calls.is_empty();
    
    if let Ok(mut registry) = CONTEXT_REGISTRY.write() {
        registry.contexts.insert(context_id, StoredContext {
            module_name: module_name.to_string(),
            context,
            has_pending_calls: has_pending,
        });
        
        ftlog::debug!(
            "[wasm:context] Stored context {} for module '{}' (pending_calls={})",
            context_id,
            module_name,
            has_pending
        );
    }
    
    context_id
}

/// Retrieve and remove a stored context
pub fn take_context(context_id: u64) -> Option<StoredContext> {
    if let Ok(mut registry) = CONTEXT_REGISTRY.write() {
        let ctx = registry.contexts.remove(&context_id);
        if ctx.is_some() {
            ftlog::debug!("[wasm:context] Retrieved context {}", context_id);
        }
        ctx
    } else {
        None
    }
}

/// Check if a context exists and has pending calls
pub fn context_has_pending_calls(context_id: u64) -> bool {
    if let Ok(registry) = CONTEXT_REGISTRY.read() {
        registry.contexts.get(&context_id)
            .map(|sc| sc.has_pending_calls)
            .unwrap_or(false)
    } else {
        false
    }
}

/// Check if a context exists
pub fn context_exists(context_id: u64) -> bool {
    if let Ok(registry) = CONTEXT_REGISTRY.read() {
        registry.contexts.contains_key(&context_id)
    } else {
        false
    }
}

/// Information about a pending HTTP call with context reference
#[derive(Debug, Clone)]
pub struct PendingHttpCallWithContext {
    /// Context ID where the call originated
    pub context_id: u64,
    /// Module name
    pub module_name: String,
    /// Call token
    pub token: u32,
    /// The pending HTTP call data
    pub call: PendingHttpCall,
}

/// Take all pending HTTP calls from all contexts
///
/// Returns a list of pending calls with their context IDs and module names.
/// The calls are removed from the contexts.
pub fn take_all_pending_http_calls() -> Vec<PendingHttpCallWithContext> {
    let mut result = Vec::new();
    
    if let Ok(mut registry) = CONTEXT_REGISTRY.write() {
        for (&context_id, stored) in registry.contexts.iter_mut() {
            if stored.has_pending_calls {
                let calls = std::mem::take(&mut stored.context.pending_http_calls);
                for (token, call) in calls {
                    result.push(PendingHttpCallWithContext {
                        context_id,
                        module_name: stored.module_name.clone(),
                        token,
                        call,
                    });
                }
                stored.has_pending_calls = false;
            }
        }
    }
    
    if !result.is_empty() {
        ftlog::debug!("[wasm:context] Took {} pending HTTP calls", result.len());
    }
    
    result
}

/// Take pending HTTP calls for a specific module
pub fn take_pending_http_calls_for_module(module_name: &str) -> Vec<PendingHttpCallWithContext> {
    let mut result = Vec::new();
    
    if let Ok(mut registry) = CONTEXT_REGISTRY.write() {
        for (&context_id, stored) in registry.contexts.iter_mut() {
            if stored.module_name == module_name && stored.has_pending_calls {
                let calls = std::mem::take(&mut stored.context.pending_http_calls);
                for (token, call) in calls {
                    result.push(PendingHttpCallWithContext {
                        context_id,
                        module_name: module_name.to_string(),
                        token,
                        call,
                    });
                }
                stored.has_pending_calls = false;
            }
        }
    }
    
    result
}

/// Deliver HTTP call response to a stored context
///
/// Updates the context with the response data. The context can then be
/// used to resume the WASM module execution.
pub fn deliver_http_call_response(
    context_id: u64,
    token: u32,
    response: super::types::HttpCallResponse,
) -> bool {
    if let Ok(mut registry) = CONTEXT_REGISTRY.write() {
        if let Some(stored) = registry.contexts.get_mut(&context_id) {
            stored.context.http_call_responses.insert(token, response);
            stored.context.current_http_call_token = Some(token);
            
            ftlog::debug!(
                "[wasm:context] Delivered HTTP call response to context {} token {}",
                context_id,
                token
            );
            return true;
        }
    }
    
    ftlog::warn!(
        "[wasm:context] Failed to deliver HTTP call response: context {} not found",
        context_id
    );
    false
}

/// Get statistics about stored contexts
pub fn get_context_stats() -> ContextStats {
    if let Ok(registry) = CONTEXT_REGISTRY.read() {
        let total = registry.contexts.len();
        let with_pending = registry.contexts.values()
            .filter(|c| c.has_pending_calls)
            .count();
        
        ContextStats {
            total_contexts: total,
            contexts_with_pending_calls: with_pending,
        }
    } else {
        ContextStats::default()
    }
}

/// Remove a context (for cleanup)
pub fn remove_context(context_id: u64) -> bool {
    if let Ok(mut registry) = CONTEXT_REGISTRY.write() {
        let removed = registry.contexts.remove(&context_id).is_some();
        if removed {
            ftlog::debug!("[wasm:context] Removed context {}", context_id);
        }
        removed
    } else {
        false
    }
}

/// Cleanup expired contexts (contexts older than max_age_secs)
/// 
/// Note: This is a placeholder - for proper cleanup we'd need timestamps
pub fn cleanup_old_contexts(max_count: usize) {
    if let Ok(mut registry) = CONTEXT_REGISTRY.write() {
        // Simple cleanup: remove oldest contexts if count exceeds max
        if registry.contexts.len() > max_count {
            let excess = registry.contexts.len() - max_count;
            let mut ids_to_remove: Vec<u64> = registry.contexts.keys()
                .take(excess)
                .copied()
                .collect();
            
            for id in ids_to_remove.drain(..) {
                registry.contexts.remove(&id);
            }
            
            ftlog::debug!("[wasm:context] Cleaned up {} old contexts", excess);
        }
    }
}

/// Statistics about stored contexts
#[derive(Debug, Default, Clone)]
pub struct ContextStats {
    pub total_contexts: usize,
    pub contexts_with_pending_calls: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasm::capabilities::ModuleCapabilities;
    
    #[test]
    fn test_store_and_retrieve_context() {
        let ctx = HttpContext::new(1, ModuleCapabilities::default());
        let context_id = store_context("test_module", ctx);
        
        assert!(context_exists(context_id));
        
        let stored = take_context(context_id);
        assert!(stored.is_some());
        assert_eq!(stored.unwrap().module_name, "test_module");
        
        // Should be removed now
        assert!(!context_exists(context_id));
    }
    
    #[test]
    fn test_pending_http_calls() {
        let mut ctx = HttpContext::new(1, ModuleCapabilities::default());
        ctx.pending_http_calls.insert(1, PendingHttpCall {
            token: 1,
            upstream: "backend".to_string(),
            timeout_ms: 1000,
            headers: vec![],
            body: vec![],
            trailers: vec![],
        });
        
        let context_id = store_context("test_http_module", ctx);
        
        let pending = take_all_pending_http_calls();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].context_id, context_id);
        assert_eq!(pending[0].token, 1);
        assert_eq!(pending[0].module_name, "test_http_module");
        
        // Cleanup
        remove_context(context_id);
    }
    
    #[test]
    fn test_deliver_response() {
        let ctx = HttpContext::new(1, ModuleCapabilities::default());
        let context_id = store_context("test_response_module", ctx);
        
        let response = super::super::types::HttpCallResponse {
            status_code: 200,
            headers: vec![],
            body: vec![1, 2, 3],
            trailers: vec![],
        };
        
        let delivered = deliver_http_call_response(context_id, 42, response);
        assert!(delivered);
        
        // Verify the response was stored
        let stored = take_context(context_id);
        assert!(stored.is_some());
        let ctx = stored.unwrap().context;
        assert!(ctx.http_call_responses.contains_key(&42));
        assert_eq!(ctx.current_http_call_token, Some(42));
    }
    
    #[test]
    fn test_context_stats() {
        let stats = get_context_stats();
        // Just verify it doesn't panic
        assert!(stats.total_contexts >= 0);
    }
}
