//! # バッファリング制御モジュール
//!
//! 低速クライアントによるバックエンドスレッド占有を防止するためのバッファリング機能を提供します。
//!
//! ## 概要
//!
//! プロキシサーバーでは、クライアントの通信速度がバックエンドより遅い場合、
//! バックエンドからのレスポンスを全て受信するまでバックエンド接続を保持し続ける必要があります。
//! これにより、バックエンドのスレッドやコネクションが長時間占有される問題が発生します。
//!
//! このモジュールは、バックエンドからのレスポンスを一時的にバッファリングし、
//! バックエンド接続を早期に解放することで、この問題を解決します。
//!
//! ## バッファリングモード
//!
//! - `Streaming`: ストリーミング転送（従来の動作）
//! - `Full`: フルバッファリング（全レスポンスをバッファ後に転送）
//! - `Adaptive`: 適応型（小さいレスポンスはフル、大きいレスポンスはストリーミング）
//!
//! ## 使用例
//!
//! ```toml
//! [path_routes."example.com"."/api/".buffering]
//! mode = "adaptive"
//! adaptive_threshold = 1048576  # 1MB
//! max_memory_buffer = 10485760  # 10MB
//! ```

mod config;
mod handler;

pub use config::{BufferingConfig, BufferingMode};
pub use handler::{BufferedResponse, BufferingHandler};

// ディスクバッファ操作を公開（Linux限定）
#[cfg(target_os = "linux")]
pub use handler::disk_buffer;

