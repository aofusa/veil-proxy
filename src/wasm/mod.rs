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
pub use grpc_integration::{on_grpc_initial_metadata, on_grpc_message, on_grpc_trailing_metadata, on_grpc_close};
#[cfg(feature = "grpc")]
pub use grpc_integration::{GrpcCallResponse, process_grpc_response, status as grpc_status};
pub use integration::{on_request_complete, on_context_destroy, on_http_call_complete, on_tick, on_queue_ready, WasmHttpCallResult, TickConfig, PendingHttpCallInfo, process_pending_http_calls, resume_after_http_call};
pub use queue_notify::{subscribe_to_queue, unsubscribe_from_queue, notify_queue_subscribers, get_queue_stats, queue_enqueued, process_pending_notifications, QueueStats};
pub use tick_manager::{register_tick, process_ticks, get_min_tick_period, get_tick_stats, TickStats};
pub use registry::ModuleRegistry;
pub use types::*;
pub use persistent_context::{
    store_context, take_context, context_exists, context_has_pending_calls, remove_context,
    take_all_pending_http_calls, take_pending_http_calls_for_module,
    deliver_http_call_response, get_context_stats, cleanup_old_contexts,
    PendingHttpCallWithContext, ContextStats,
};




/// Initialize the WASM extension system
pub fn init(config: &WasmConfig) -> anyhow::Result<FilterEngine> {
    FilterEngine::new(config)
}
