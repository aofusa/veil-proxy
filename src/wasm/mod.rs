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
mod host;
mod registry;
mod types;

pub use capabilities::{CapabilityPreset, ModuleCapabilities};
pub use constants::*;
pub use context::HttpContext;
pub use engine::FilterEngine;
pub use registry::ModuleRegistry;
pub use types::*;

/// Initialize the WASM extension system
pub fn init(config: &WasmConfig) -> anyhow::Result<FilterEngine> {
    FilterEngine::new(config)
}
