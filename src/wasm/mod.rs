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
pub(crate) mod host;
pub mod http_executor;
pub mod integration;
// OpenBSD は wasmtime 既定のファイバスタック（MAP_STACK なし）で SIGSEGV するため、
// MAP_STACK 付きでスタックを確保する StackCreator を差し込む（B-52）。
#[cfg(target_os = "openbsd")]
mod openbsd_stack;
pub mod persistent_context;
pub mod queue_notify;
mod registry;
pub mod tick_manager;
mod types;

#[cfg(test)]
mod tests;

pub use capabilities::{CapabilityPreset, ModuleCapabilities};
pub use constants::*;
pub use context::{HostState, HttpContext};
pub use engine::{
    BodyFilterResult, FilterEngine, FilterResult, NetworkAction, NetworkFilterResult,
};
pub use grpc_integration::{
    on_grpc_close, on_grpc_initial_metadata, on_grpc_message, on_grpc_trailing_metadata,
};
#[cfg(feature = "grpc")]
pub use grpc_integration::{process_grpc_response, status as grpc_status, GrpcCallResponse};
pub use integration::{
    empty_wasm_modules, on_context_destroy, on_http_call_complete, on_queue_ready,
    on_request_complete, on_request_complete_async, on_tick, process_pending_http_calls,
    resume_after_http_call, PendingHttpCallInfo, TickConfig, WasmHttpCallResult,
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

// ============================================================================
// F-134: Proxy-Wasm ABI 適合度テスト向けサポート API
// ============================================================================
//
// `tests/proxy_wasm_conformance.rs`（外部統合テストクレート）はホスト関数を
// `Linker` へ登録した実体（`host::add_host_functions`、`pub(crate)`）へ直接
// アクセスできない。本番の `registry.rs::create_engine` はプーリングアロケータ・
// Pulley 切替・epoch/fuel 等の本番専用設定を伴うため流用せず、ホスト関数の
// 引数検証・ステータスコードのみを検証する最小構成をここに用意する。

/// 適合度テスト用の `Engine` を構築する（async host functions を使うため
/// `async_support(true)` のみ必要）。
pub fn build_conformance_test_engine() -> anyhow::Result<wasmtime::Engine> {
    let mut config = wasmtime::Config::new();
    config.async_support(true);
    wasmtime::Engine::new(&config)
}

/// 本番と同じホスト関数一式を登録した `Linker` を構築する。
pub fn build_conformance_test_linker(
    engine: &wasmtime::Engine,
) -> anyhow::Result<wasmtime::Linker<HostState>> {
    let mut linker = wasmtime::Linker::new(engine);
    host::add_host_functions(&mut linker)?;
    Ok(linker)
}
