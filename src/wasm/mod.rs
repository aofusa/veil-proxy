//! Proxy-Wasm v0.2.1 Extension Module for veil-proxy
//!
//! This module implements a WebAssembly-based extension system
//! compatible with Proxy-Wasm ABI v0.2.1.
//!
//! # Features
//! - Pure Proxy-Wasm v0.2.1 compliant (Nginx/Envoy compatible)
//! - AOT compilation with .cwasm files
//! - Pooling allocator for fast instantiation
//! - Per-module capability restrictions
//!
//! # Usage
//! Enable the `wasm` feature in Cargo.toml:
//! ```toml
//! cargo build --features wasm
//! ```

mod capabilities;
mod constants;
mod context;
mod engine;
pub mod grpc_integration;
mod host;
pub mod http_executor;
pub mod integration;
pub mod persistent_context;
pub mod queue_notify;
mod registry;
pub mod tick_manager;
mod types;

#[cfg(test)]
mod tests;

pub use capabilities::{CapabilityPreset, ModuleCapabilities};
pub use constants::*;
pub use context::HttpContext;
pub use engine::{BodyFilterResult, FilterEngine, FilterResult};
pub use grpc_integration::{
    on_grpc_close, on_grpc_initial_metadata, on_grpc_message, on_grpc_trailing_metadata,
};
#[cfg(feature = "grpc")]
pub use grpc_integration::{process_grpc_response, status as grpc_status, GrpcCallResponse};
pub use integration::{
    on_context_destroy, on_http_call_complete, on_queue_ready, on_request_complete,
    on_request_complete_async, on_tick, process_pending_http_calls, resume_after_http_call,
    PendingHttpCallInfo, TickConfig, WasmHttpCallResult,
};
pub use persistent_context::{
    cleanup_old_contexts,
    context_exists,
    context_has_pending_calls,
    deliver_http_call_response,
    get_context_stats,
    get_global_pending_call_count,
    // Global pending calls (for tick thread processing)
    register_global_pending_call,
    remove_context,
    store_context,
    take_all_pending_http_calls,
    take_context,
    take_global_pending_calls,
    take_pending_http_calls_for_module,
    ContextStats,
    GlobalPendingCall,
    PendingHttpCallWithContext,
};
pub use queue_notify::{
    get_queue_stats, notify_queue_subscribers, process_pending_notifications, queue_enqueued,
    subscribe_to_queue, unsubscribe_from_queue, QueueStats,
};
pub use registry::ModuleRegistry;
pub use tick_manager::{
    get_min_tick_period, get_tick_stats, process_ticks, register_tick, TickStats,
};
pub use types::*;

/// Initialize the WASM extension system
pub fn init(config: &WasmConfig) -> anyhow::Result<FilterEngine> {
    FilterEngine::new(config)
}
