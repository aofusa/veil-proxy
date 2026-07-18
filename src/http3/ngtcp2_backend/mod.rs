//! ngtcp2 + nghttp3 + aws-lc-sys ベースの HTTP/3 実装。
//!
//! Linux / FreeBSD / macOS の既定バックエンド。
//! 公式 C ライブラリ（ngtcp2 / nghttp3）を自前 FFI 経由で使用し、
//! TLS は既存の aws-lc-sys を共有する。

#![cfg(veil_http3_ngtcp2)]

mod conn;
mod crypto;
mod h3;
mod server;

pub use server::{run_http3_server, run_http3_server_async};
