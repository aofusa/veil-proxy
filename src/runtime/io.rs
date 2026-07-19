//! 非同期 I/O トレイト（monoio 互換レイヤー）
//!
//! monoio の `AsyncReadRent` / `AsyncWriteRentExt` に相当するトレイトを自前定義する。
//! 所有権ベースの I/O モデルを維持しつつ、monoio への依存を排除する。

use super::buf::{IoBuf, IoBufMut};
use std::io;

// ====================
// IoVec トレイト（readv/writev 用、stub 実装）
// ====================

/// 複数バッファの読み取り用トレイト（stub）
///
/// # Safety
///
/// 実装型は readv 系 API がバッファポインタ/長さを信頼できることを保証すること
/// （現状 stub のため実装型なし）。
pub unsafe trait IoVecBufMut: 'static {}

/// 複数バッファの書き込み用トレイト（stub）
///
/// # Safety
///
/// 実装型は writev 系 API がバッファポインタ/長さを信頼できることを保証すること
/// （現状 stub のため実装型なし）。
pub unsafe trait IoVecBuf: 'static {}

// BufResult: monoio 互換の結果型エイリアス
pub type BufResult<T, B> = (io::Result<T>, B);

/// 非同期読み取りトレイト（所有権ベース、monoio::io::AsyncReadRent 互換）
pub trait AsyncReadRent {
    /// バッファに読み取る（バッファの所有権を取る）
    fn read<T: IoBufMut>(
        &mut self,
        buf: T,
    ) -> impl std::future::Future<Output = BufResult<usize, T>>;

    /// 複数バッファに読み取る（stub - 現在は unimplemented）
    fn readv<T: IoVecBufMut>(
        &mut self,
        buf: T,
    ) -> impl std::future::Future<Output = BufResult<usize, T>> {
        async move { (Err(std::io::Error::other("readv not implemented")), buf) }
    }
}

/// 非同期書き込みトレイト（所有権ベース、monoio::io::AsyncWriteRent 互換）
pub trait AsyncWriteRent {
    /// バッファを書き込む（バッファの所有権を取る）
    fn write<T: IoBuf>(&mut self, buf: T)
        -> impl std::future::Future<Output = BufResult<usize, T>>;

    /// 複数バッファを書き込む（stub - 現在は unimplemented）
    fn writev<T: IoVecBuf>(
        &mut self,
        buf: T,
    ) -> impl std::future::Future<Output = BufResult<usize, T>> {
        async move { (Err(io::Error::other("writev not implemented")), buf) }
    }

    /// フラッシュ（デフォルト実装は no-op）
    fn flush(&mut self) -> impl std::future::Future<Output = io::Result<()>> {
        async move { Ok(()) }
    }

    /// シャットダウン
    fn shutdown(&mut self) -> impl std::future::Future<Output = io::Result<()>>;
}

/// AsyncWriteRent の拡張メソッド（monoio::io::AsyncWriteRentExt 互換）
///
/// バイナリクレート内部専用のトレイトのため、auto trait 境界（Send 等）を
/// 呼び出し側で指定する必要がなく、`async fn` のままで問題ない。
#[allow(async_fn_in_trait)]
pub trait AsyncWriteRentExt: AsyncWriteRent {
    /// バッファを全て書き込む（ループで write を呼ぶ）
    ///
    /// B-27: 部分書き込み（short write）時は `SlicedIoBuf` で送信済みオフセットを進め、
    /// **残りを追加アロケーションなしで書き続ける**。旧実装は 1 回の `write` で書き
    /// 切れないと WriteZero エラーを返しており、kTLS 経路（`write` が単発 io_uring
    /// SEND のため sndbuf 満杯で部分書き込みが起こる）の高並行 HTTP/2 送信で、
    /// 送信済みプレフィックスだけがワイヤに残ってフレーム同期が壊れていた。
    async fn write_all<T: IoBuf>(&mut self, buf: T) -> BufResult<usize, T> {
        let total = buf.bytes_init();
        let mut sliced = super::buf::SlicedIoBuf::new(buf);
        let mut written = 0usize;

        while written < total {
            let (result, returned) = self.write(sliced).await;
            sliced = returned;
            match result {
                Ok(0) => {
                    return (
                        Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "write returned 0 bytes",
                        )),
                        sliced.into_inner(),
                    );
                }
                Ok(n) => {
                    written += n;
                    sliced.advance(n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // 同一オフセットで再試行（送信バッファの空き待ちは write 実装側の
                    // POLL_ADD / io_uring 完了に委ねる）
                    continue;
                }
                Err(e) => return (Err(e), sliced.into_inner()),
            }
        }

        (Ok(written), sliced.into_inner())
    }
}

// AsyncWriteRent を実装する型は自動的に AsyncWriteRentExt も実装する
impl<T: AsyncWriteRent> AsyncWriteRentExt for T {}

// ====================
// 復号済み・先読みバッファの残量問い合わせ（F-116 HTTP/2 多重化）
// ====================

/// ストリームが「POLLIN では通知されない、既に手元にある未消費バイト」を保持しているか
/// を問い合わせるトレイト。
///
/// HTTP/2 多重化メインループは、完全フレームが読めず・書くものも無いとき
/// `wait_readable_fd`（`POLL_ADD`）でソケット可読を待つ。しかし TLS ストリーム
/// （ユーザ空間 rustls）は復号済み平文を内部バッファに退避し得るし、
/// [`BufferedStream`](crate::proxy::BufferedStream) はプロトコル検出時の先読みデータを
/// 抱え得る。これらが残っている間は `POLLIN` が発火しない（既にカーネルから読み終えている）
/// ため、待機前に本メソッドで「未消費データ無し」を確認しないとデッドロックする。
///
/// kTLS で受信オフロード済みのストリームはカーネルが復号するため内部退避を持たず、
/// `false` を返す（`POLLIN` が信頼できる）。
pub trait BufferedReadState {
    /// 復号済み／先読み済みで、まだ読み出されていないバイトを保持していれば `true`。
    fn has_buffered_read_data(&self) -> bool;
}

impl BufferedReadState for super::tcp::TcpStream {
    /// 生 TCP は内部退避バッファを持たない（全て `POLLIN` で通知される）。
    #[inline]
    fn has_buffered_read_data(&self) -> bool {
        false
    }
}

// ====================
// 非同期ファイル I/O（monoio::fs 互換）
// ====================

/// ファイルを読み取る（std::fs::read の非同期版）
///
/// monoio::fs::read の互換実装。ホットパス（静的ファイル配信の memory モード等）からも
/// 呼ばれるため、ブロッキング FS を offload（専用スレッドプール + eventfd 完了待機）へ
/// 退避してイベントループを塞がない（B-26。リング未初期化のコンテキストでは inline 実行）。
pub async fn read(path: impl AsRef<std::path::Path>) -> io::Result<Vec<u8>> {
    let path = path.as_ref().to_owned();
    // 理由付き allow: offload ワーカースレッド内で実行されるためイベントループ非ブロック。
    #[allow(clippy::disallowed_methods)]
    crate::runtime::offload::offload(move || std::fs::read(path)).await
}

/// ファイルを削除する（monoio::fs::remove_file の互換実装）
///
/// B-26: offload 経由でイベントループを塞がない。
pub async fn remove_file(path: impl AsRef<std::path::Path>) -> io::Result<()> {
    let path = path.as_ref().to_owned();
    // 理由付き allow: offload ワーカースレッド内で実行されるためイベントループ非ブロック。
    #[allow(clippy::disallowed_methods)]
    crate::runtime::offload::offload(move || std::fs::remove_file(path)).await
}

// ====================
// 非同期ファイル（monoio::fs::File 互換）
// ====================

/// 非同期ファイル（簡略実装、コールドパスのみ使用）
pub struct File {
    inner: std::fs::File,
}

impl File {
    /// ファイルを開く
    pub async fn open(path: impl AsRef<std::path::Path>) -> io::Result<Self> {
        let inner = std::fs::File::open(path)?;
        Ok(Self { inner })
    }

    /// ファイルを読み取る
    pub async fn read_to_end(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        use std::io::Read;
        self.inner.read_to_end(buf)
    }

    /// オフセット位置から読み取る（Unix: `pread`、Windows: `seek_read`。コールドパスのみ）
    #[cfg(unix)]
    pub async fn read_at<T: IoBufMut>(&self, mut buf: T, offset: u64) -> BufResult<usize, T> {
        use std::os::unix::io::AsRawFd;
        let fd = self.inner.as_raw_fd();
        let ret = unsafe {
            libc::pread(
                fd,
                buf.write_ptr() as *mut libc::c_void,
                buf.bytes_total(),
                offset as libc::off_t,
            )
        };
        if ret < 0 {
            return (Err(io::Error::last_os_error()), buf);
        }
        unsafe {
            buf.set_init(ret as usize);
        }
        (Ok(ret as usize), buf)
    }

    /// オフセット位置から読み取る（Windows 版: `FileExt::seek_read`）。
    #[cfg(windows)]
    pub async fn read_at<T: IoBufMut>(&self, mut buf: T, offset: u64) -> BufResult<usize, T> {
        use std::os::windows::fs::FileExt;
        let slice = unsafe {
            std::slice::from_raw_parts_mut(buf.write_ptr(), buf.bytes_total())
        };
        match self.inner.seek_read(slice, offset) {
            Ok(n) => {
                unsafe {
                    buf.set_init(n);
                }
                (Ok(n), buf)
            }
            Err(e) => (Err(e), buf),
        }
    }

    /// オフセット位置からバッファを全部読む（Unix: pread ループ）
    #[cfg(unix)]
    pub async fn read_exact_at<T: IoBufMut>(&self, mut buf: T, offset: u64) -> BufResult<usize, T> {
        use std::os::unix::io::AsRawFd;
        let fd = self.inner.as_raw_fd();
        let total = buf.bytes_total();
        let mut read = 0;
        while read < total {
            let ret = unsafe {
                libc::pread(
                    fd,
                    buf.write_ptr().add(read) as *mut libc::c_void,
                    total - read,
                    (offset + read as u64) as libc::off_t,
                )
            };
            if ret < 0 {
                return (Err(io::Error::last_os_error()), buf);
            }
            if ret == 0 {
                break; // EOF
            }
            read += ret as usize;
        }
        unsafe {
            buf.set_init(read);
        }
        (Ok(read), buf)
    }

    /// オフセット位置からバッファを全部読む（Windows 版: `seek_read` ループ）。
    #[cfg(windows)]
    pub async fn read_exact_at<T: IoBufMut>(&self, mut buf: T, offset: u64) -> BufResult<usize, T> {
        use std::os::windows::fs::FileExt;
        let total = buf.bytes_total();
        let mut read = 0usize;
        while read < total {
            let slice = unsafe {
                std::slice::from_raw_parts_mut(buf.write_ptr().add(read), total - read)
            };
            match self.inner.seek_read(slice, offset + read as u64) {
                Ok(0) => break, // EOF
                Ok(n) => read += n,
                Err(e) => return (Err(e), buf),
            }
        }
        unsafe {
            buf.set_init(read);
        }
        (Ok(read), buf)
    }
}

#[cfg(unix)]
impl std::os::unix::io::AsRawFd for File {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.inner.as_raw_fd()
    }
}

#[cfg(windows)]
impl std::os::windows::io::AsRawHandle for File {
    fn as_raw_handle(&self) -> std::os::windows::io::RawHandle {
        std::os::windows::io::AsRawHandle::as_raw_handle(&self.inner)
    }
}

/// OpenOptions（monoio::fs::OpenOptions 互換）
pub struct OpenOptions {
    inner: std::fs::OpenOptions,
}

impl OpenOptions {
    pub fn new() -> Self {
        Self {
            inner: std::fs::OpenOptions::new(),
        }
    }

    pub fn read(mut self, read: bool) -> Self {
        self.inner.read(read);
        self
    }

    pub fn write(mut self, write: bool) -> Self {
        self.inner.write(write);
        self
    }

    pub fn create(mut self, create: bool) -> Self {
        self.inner.create(create);
        self
    }

    pub fn append(mut self, append: bool) -> Self {
        self.inner.append(append);
        self
    }

    pub async fn open(self, path: impl AsRef<std::path::Path>) -> io::Result<File> {
        let inner = self.inner.open(path)?;
        Ok(File { inner })
    }
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1 回の write で最大 `max_per_write` バイトしか書けないモックストリーム。
    /// B-27: write_all が short write の残りを正しいオフセットから書き続けることを検証する。
    struct ShortWriteStream {
        written: Vec<u8>,
        max_per_write: usize,
    }

    impl AsyncWriteRent for ShortWriteStream {
        async fn write<T: IoBuf>(&mut self, buf: T) -> BufResult<usize, T> {
            let len = buf.bytes_init().min(self.max_per_write);
            let slice = unsafe { std::slice::from_raw_parts(buf.read_ptr(), len) };
            self.written.extend_from_slice(slice);
            (Ok(len), buf)
        }

        async fn shutdown(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn write_all_continues_after_short_write() {
        let mut stream = ShortWriteStream {
            written: Vec::new(),
            max_per_write: 7,
        };
        let data: Vec<u8> = (0..100u8).collect();
        let fut = stream.write_all(data.clone());
        let (result, returned) = futures::executor::block_on(fut);
        assert_eq!(result.unwrap(), 100, "write_all は全量書き込みを返す");
        assert_eq!(
            stream.written, data,
            "short write 継続でバイト列が欠落・重複しない"
        );
        assert_eq!(returned, data, "元のバッファがそのまま返却される");
    }

    #[test]
    fn write_all_single_full_write() {
        let mut stream = ShortWriteStream {
            written: Vec::new(),
            max_per_write: usize::MAX,
        };
        let data = b"hello".to_vec();
        let (result, _) = futures::executor::block_on(stream.write_all(data.clone()));
        assert_eq!(result.unwrap(), 5);
        assert_eq!(stream.written, data);
    }

    #[test]
    fn write_all_zero_write_is_error() {
        let mut stream = ShortWriteStream {
            written: Vec::new(),
            max_per_write: 0,
        };
        let data = b"x".to_vec();
        let (result, _) = futures::executor::block_on(stream.write_all(data));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WriteZero);
    }
}
