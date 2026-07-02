//! Veil ライブラリクレートルート（`cargo fuzz`・統合テスト向け）。
//! バイナリエントリは `src/main.rs`。

// ====================
// メモリアロケータ選択
// ====================

#[cfg(feature = "mimalloc")]
use mimalloc::MiMalloc;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[cfg(feature = "jemalloc")]
use tikv_jemallocator::Jemalloc;

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[cfg(feature = "ktls")]
pub mod ktls;

#[cfg(feature = "ktls")]
pub mod ktls_rustls;

#[cfg(feature = "http2")]
pub mod protocol;

#[cfg(feature = "http2")]
pub mod http2;

#[cfg(feature = "http3")]
pub mod http3_server;

#[cfg(feature = "http3")]
pub mod http3_stream;

#[cfg(feature = "http3")]
pub mod udp;

pub mod runtime;
pub mod security;
pub mod buffering;
pub mod cache;
pub mod routing;

#[cfg(feature = "wasm")]
pub mod wasm;

#[cfg(feature = "grpc")]
pub mod grpc;

pub mod logging;
pub mod system;
pub mod metrics;

pub mod constants;
pub mod http_utils;

#[cfg(feature = "opentelemetry")]
pub mod otel;

pub mod pool;
pub mod resilience;

#[cfg(feature = "access-log")]
pub mod access_log;

#[cfg(feature = "l4-proxy")]
pub mod l4;

#[cfg(not(feature = "ktls"))]
pub mod simple_tls;

pub mod tls_reload;
pub mod config;
pub use crate::config::*;
pub mod fuzz_api;
pub mod upstream;
pub use crate::upstream::*;
pub mod server;
pub mod proxy;

mod entry;
pub use entry::run;