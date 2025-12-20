//! # HTTP/2 プロトコル実装
//!
//! RFC 7540 (HTTP/2) と RFC 7541 (HPACK) の実装。
//! monoio 非同期ランタイムと kTLS と統合して動作します。
//!
//! ## モジュール構成
//!
//! - `frame`: HTTP/2 フレームのエンコード/デコード
//! - `hpack`: HPACK ヘッダー圧縮
//! - `stream`: HTTP/2 ストリーム管理
//! - `connection`: HTTP/2 コネクション管理
//! - `error`: HTTP/2 エラー定義
//!
//! ## 使用例
//!
//! ```rust,ignore
//! use http2::connection::Http2Connection;
//! use http2::settings::Http2Settings;
//!
//! let settings = Http2Settings::default();
//! let mut conn = Http2Connection::new(tls_stream, settings);
//! conn.handshake().await?;
//! conn.run(|stream| async { /* handle request */ }).await?;
//! ```

pub mod frame;
pub mod hpack;
pub mod stream;
pub mod connection;
pub mod error;
pub mod settings;
pub mod client;

pub use error::{Http2Error, Http2ErrorCode};
pub use settings::Http2Settings;
pub use connection::{Http2Connection, ProcessedRequest};
pub use stream::{Stream, StreamState, StreamManager};
pub use client::{H2cClient, H2cResponse};
