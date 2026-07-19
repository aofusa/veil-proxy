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

#[cfg(all(feature = "jemalloc", not(feature = "mimalloc")))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

// kTLS はカーネルオフロード実装のため Linux 専用（F-120 設計 2 節）。`ktls` feature が
// 有効でも非 Linux では `veil_ktls` が立たず、下の `simple_tls`（ユーザ空間 rustls）へ
// 自動フォールバックする。
#[cfg(veil_ktls)]
pub mod ktls;

#[cfg(veil_ktls)]
pub mod ktls_rustls;

#[cfg(feature = "http2")]
pub mod protocol;

#[cfg(feature = "http2")]
pub mod http2;

/// HTTP/3 サーバ（ngtcp2 / quiche バックエンド切替。`build.rs` の cfg 参照）
#[cfg(any(feature = "http3", feature = "http3-quiche"))]
pub mod http3;

/// 後方互換エイリアス（`entry` / `config` / 既存テストが `http3_server::` を参照）
#[cfg(any(feature = "http3", feature = "http3-quiche"))]
pub use http3 as http3_server;

/// 後方互換エイリアス（ストリーミングアクター）
#[cfg(any(feature = "http3", feature = "http3-quiche"))]
pub use http3::stream as http3_stream;

#[cfg(any(feature = "http3", feature = "http3-quiche"))]
pub mod udp;

/// HTTP/2 / HTTP/3 アクターモデル共通の単一スレッドチャネル/Notify（F-116）。
/// `#![cfg(any(feature = "http2", feature = "http3", feature = "http3-quiche"))]` で内部を feature ゲートする。
pub mod stream_channel;

pub mod buffering;
pub mod cache;
pub mod routing;
pub mod runtime;
pub mod security;

#[cfg(feature = "wasm")]
pub mod wasm;

#[cfg(feature = "grpc")]
pub mod grpc;

pub mod logging;
pub mod metrics;
pub mod system;

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

#[cfg(not(veil_ktls))]
pub mod simple_tls;

/// rustls 暗号プロバイダ選択（F-122: OpenBSD は ring、他は aws_lc_rs）。
pub mod tls_provider;

pub mod config;
pub mod tls_reload;
pub use crate::config::*;
pub mod fuzz_api;
/// HTTP/3 / QPACK ワイヤ純関数パーサ（F-112、ホットパス外・ファジング用）。
pub mod http3_wire;
pub mod upstream;
pub use crate::upstream::*;
pub mod proxy;
pub mod server;

mod entry;
pub use entry::run;

// bin(旧 main.rs）から移設したクレート内部テスト群（lib+bin 構成対応）
#[cfg(test)]
mod legacy_bin_tests;
