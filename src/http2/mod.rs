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

pub mod connection;
pub mod error;
pub mod frame;
pub mod hpack;
pub mod settings;
pub mod stream;
pub(crate) mod upstream_mux;

pub use connection::{Http2Connection, ProcessedRequest};
pub use error::{Http2Error, Http2ErrorCode};
pub use settings::Http2Settings;
pub use stream::{Stream, StreamManager, StreamState};

/// ヘッダ名スロット（F-166/F-165 A1、F-168 H-2 で上流クライアント/`connection.rs` 共通化）。
///
/// 既に小文字のヘッダ名（大半のケース）は元スライスを借用するだけでコピーしない。
/// 大文字を含むヘッダ名（稀）のみ `Owned` に小文字化したバッファを持つ。この
/// バッファは `crate::pool::lowered_header_name_buf_get`/`_put` の再利用プールから
/// 借りており、HPACK エンコード完了後にプールへ返却する（warmup 後はヒープ確保ゼロ）。
pub(crate) enum NameSlot<'a> {
    Borrowed(&'a [u8]),
    Owned(Vec<u8>),
}

impl NameSlot<'_> {
    #[inline]
    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            NameSlot::Borrowed(s) => s,
            NameSlot::Owned(v) => v.as_slice(),
        }
    }
}
