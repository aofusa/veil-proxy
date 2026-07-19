//! HTTP/3 サーバ（QUIC バックエンド切替）
//!
//! ## バックエンド選択（`build.rs` が cfg 発行）
//!
//! | cfg | 条件 | 実装 |
//! |-----|------|------|
//! | `veil_http3_ngtcp2` | Linux / FreeBSD / macOS 既定 | 公式 ngtcp2 + nghttp3 + aws-lc-sys |
//! | `veil_http3_quiche` | OpenBSD / Windows 既定、または `--features http3-quiche` | Cloudflare quiche |
//!
//! 公開 API（`Http3ServerConfig` / `run_http3_server` 等）は両バックエンドで同一。

#![cfg(any(feature = "http3", feature = "http3-quiche"))]

pub mod common;

/// F-32 ストリーミングアクター（ngtcp2 / quiche 両バックエンドで共有）。
pub mod stream;

#[cfg(veil_http3_quiche)]
mod quiche_backend;

#[cfg(veil_http3_ngtcp2)]
mod ngtcp2_backend;

// 共通型は常に公開
pub use common::{
    compress_body_h3, proxy_to_backend_async_with_tls, BackendProxyResult, Http3ServerConfig,
};

// サーバ起動 API はバックエンド別
#[cfg(veil_http3_quiche)]
pub use quiche_backend::{run_http3_server, run_http3_server_async};

#[cfg(veil_http3_ngtcp2)]
pub use ngtcp2_backend::{run_http3_server, run_http3_server_async};
