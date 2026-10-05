//! Veil ライブラリクレートルート（`cargo fuzz`・統合テスト向け）。
//! バイナリエントリは `src/main.rs`。

// ====================
// メモリアロケータ選択
// ====================

#[cfg(feature = "mimalloc")]
use mimalloc::MiMalloc;

#[cfg(all(feature = "mimalloc", not(feature = "alloc-stats")))]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[cfg(feature = "jemalloc")]
use tikv_jemallocator::Jemalloc;

// B-89: FreeBSD 向けの tikv-jemalloc は libthr のフック（`_malloc_thread_cleanup` 等）を
// プレフィックス無しで定義して libc 内蔵 jemalloc のフックを上書きし、スレッドの
// 生成・終了を繰り返すと libc の malloc がクラッシュする。FreeBSD の malloc(3) は
// 元から jemalloc なので、FreeBSD では `jemalloc` feature を使わせない。
#[cfg(all(feature = "jemalloc", target_os = "freebsd"))]
compile_error!(
    "the `jemalloc` feature must not be used on FreeBSD (B-89): tikv-jemalloc overrides \
     libthr's malloc hooks and corrupts the libc allocator; FreeBSD's malloc(3) already is jemalloc"
);

#[cfg(all(
    feature = "jemalloc",
    not(feature = "mimalloc"),
    not(feature = "alloc-stats")
))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

// F-165 Phase 1: `alloc-stats` feature 有効時は、既存の選択ロジックが選ぶはずの
// アロケータ（mimalloc → jemalloc → システムアロケータの優先順）を
// `CountingAllocator` で包んだものに差し替える。opt-in・診断ビルド専用。
#[cfg(feature = "alloc-stats")]
pub mod alloc_stats;

#[cfg(all(feature = "alloc-stats", feature = "mimalloc"))]
#[global_allocator]
static GLOBAL: alloc_stats::CountingAllocator<MiMalloc> = alloc_stats::CountingAllocator(MiMalloc);

#[cfg(all(
    feature = "alloc-stats",
    feature = "jemalloc",
    not(feature = "mimalloc")
))]
#[global_allocator]
static GLOBAL: alloc_stats::CountingAllocator<Jemalloc> = alloc_stats::CountingAllocator(Jemalloc);

#[cfg(all(
    feature = "alloc-stats",
    not(feature = "mimalloc"),
    not(feature = "jemalloc")
))]
#[global_allocator]
static GLOBAL: alloc_stats::CountingAllocator<std::alloc::System> =
    alloc_stats::CountingAllocator(std::alloc::System);

// kTLS はカーネルオフロード実装のため Linux/FreeBSD 専用（F-120 設計 2 節 / F-126）。
// `ktls` feature が有効でも非対応 OS（OpenBSD）では `veil_ktls` が立たず、下の
// `simple_tls`（ユーザ空間 rustls）へ自動フォールバックする。
#[cfg(veil_ktls)]
pub mod ktls;

#[cfg(veil_ktls)]
pub mod ktls_rustls;

/// FreeBSD kTLS 送受信オフロード実装（F-126）。Linux 経路（`src/ktls.rs` の
/// Linux 専用構造体・setsockopt 呼び出し）とは完全に分離されたモジュール。
#[cfg(all(veil_ktls, target_os = "freebsd"))]
pub mod ktls_freebsd;

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

/// HTTP/2 / HTTP/3 アクターモデル共通の単一スレッドチャネル/Notify（F-116）。
/// `#![cfg(any(feature = "http2", feature = "http3"))]` で内部を feature ゲートする。
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

/// Proxy-Wasm プラグイン設定（TOML テーブル記法・ルート単位上書き、F-148）。
/// `wasm` feature に依存せず常にコンパイルされる（`config.rs` が型として使うため）。
pub mod wasm_plugin_config;

#[cfg(feature = "access-log")]
pub mod access_log;

#[cfg(feature = "l4-proxy")]
pub mod l4;

#[cfg(not(veil_ktls))]
pub mod simple_tls;

/// rustls 暗号文チャンクを `writev(2)`（Windows は `WSASend`）で直接カーネルへ渡す
/// ゼロコピー送信ヘルパー（F-150）。`simple_tls`（`veil_ktls` 無効時）・
/// `ktls_rustls`（`veil_ktls` 時の rustls フォールバック経路）の両方が使うため、
/// `veil_ktls` の有無に関わらず常にコンパイルする。
pub(crate) mod tls_writev;

/// rustls 暗号プロバイダ選択（F-122: OpenBSD は ring、他は aws_lc_rs）。
pub mod tls_provider;

/// リッスンアドレス表現（F-164: `unix:<path>` UDS 対応）。`config`/`server`/`entry` の
/// いずれからも参照するためクレートルート直下に置く。
pub mod listen_addr;

pub mod config;
pub mod config_override;
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
