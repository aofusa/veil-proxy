//! Cloudflare quiche ベースの HTTP/3 実装。
//!
//! OpenBSD / Windows の既定バックエンド、および
//! `--features http3-quiche` 指定時に使用する。

#![cfg(veil_http3_quiche)]

mod server;

pub use server::{run_http3_server, run_http3_server_async};
