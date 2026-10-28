//! 非同期 I/O トレイト（AsyncReader / AsyncWriter）
//!
//! F-233 で `src/config.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// 非同期I/Oトレイト（コード重複解消）
// ====================

/// 非同期読み込みトレイト（SafeReadBuffer対応）
///
/// 読み込み操作で `SafeReadBuffer` を受け取り、返却します。
/// ランタイムの `IoBufMut::set_init()` コールバック（`src/runtime/buf.rs`）により、読み込み完了時に
/// 自動的に `valid_len` が設定されます。
#[allow(async_fn_in_trait)]
pub trait AsyncReader {
    async fn read_buf(&mut self, buf: SafeReadBuffer) -> (io::Result<usize>, SafeReadBuffer);

    /// 直前の EOF が相手の正しい終了か（`AsyncReadRent::eof_was_clean`。F-282）
    fn eof_was_clean(&self) -> bool {
        true
    }
}

/// 非同期書き込みトレイト
///
/// 書き込み操作では `Vec<u8>` を受け取ります。
/// 書き込みデータは既に有効なデータなので、SafeReadBuffer は不要です。
#[allow(async_fn_in_trait)]
pub trait AsyncWriter {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>);
}

// TcpStream用の実装
impl AsyncReader for TcpStream {
    async fn read_buf(&mut self, buf: SafeReadBuffer) -> (io::Result<usize>, SafeReadBuffer) {
        use crate::runtime::io::AsyncReadRent;
        self.read(buf).await
    }
}

impl AsyncWriter for TcpStream {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.write_all(buf).await
    }
}

// KtlsServerStream用の実装（rustls + ktls2）
#[cfg(veil_ktls)]
impl AsyncReader for KtlsServerStream {
    async fn read_buf(&mut self, buf: SafeReadBuffer) -> (io::Result<usize>, SafeReadBuffer) {
        use crate::runtime::io::AsyncReadRent;
        self.read(buf).await
    }
}

#[cfg(veil_ktls)]
impl AsyncWriter for KtlsServerStream {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.write_all(buf).await
    }
}

// KtlsClientStream用の実装（rustls + ktls2）
#[cfg(veil_ktls)]
impl AsyncReader for KtlsClientStream {
    async fn read_buf(&mut self, buf: SafeReadBuffer) -> (io::Result<usize>, SafeReadBuffer) {
        use crate::runtime::io::AsyncReadRent;
        self.read(buf).await
    }

    fn eof_was_clean(&self) -> bool {
        crate::runtime::io::AsyncReadRent::eof_was_clean(self)
    }
}

#[cfg(veil_ktls)]
impl AsyncWriter for KtlsClientStream {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.write_all(buf).await
    }
}

// SimpleTlsServerStream用の実装（rustls のみ）
#[cfg(not(veil_ktls))]
impl AsyncReader for crate::simple_tls::SimpleTlsServerStream {
    async fn read_buf(&mut self, buf: SafeReadBuffer) -> (io::Result<usize>, SafeReadBuffer) {
        use crate::runtime::io::AsyncReadRent;
        self.read(buf).await
    }
}

#[cfg(not(veil_ktls))]
impl AsyncWriter for crate::simple_tls::SimpleTlsServerStream {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.write_all(buf).await
    }
}

// SimpleTlsClientStream用の実装（rustls のみ）
#[cfg(not(veil_ktls))]
impl AsyncReader for crate::simple_tls::SimpleTlsClientStream {
    async fn read_buf(&mut self, buf: SafeReadBuffer) -> (io::Result<usize>, SafeReadBuffer) {
        use crate::runtime::io::AsyncReadRent;
        self.read(buf).await
    }

    fn eof_was_clean(&self) -> bool {
        crate::runtime::io::AsyncReadRent::eof_was_clean(self)
    }
}

#[cfg(not(veil_ktls))]
impl AsyncWriter for crate::simple_tls::SimpleTlsClientStream {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.write_all(buf).await
    }
}
