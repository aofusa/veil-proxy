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
pub unsafe trait IoVecBufMut: 'static {}

/// 複数バッファの書き込み用トレイト（stub）
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
        async move {
            (
                Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "readv not implemented",
                )),
                buf,
            )
        }
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
        async move {
            (
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "writev not implemented",
                )),
                buf,
            )
        }
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
    async fn write_all<T: IoBuf>(&mut self, buf: T) -> BufResult<usize, T> {
        let total = buf.bytes_init();
        let mut written = 0;
        let mut buf = buf;

        // 書き込み済みデータを追跡するためのオフセット付きラッパー
        // 実際の実装では、IoBuf のスライスをサポートするか、
        // Vec<u8> に変換して書き込む
        // ここでは簡略実装として、一度で書き込めない場合はエラーを返す
        let (result, returned_buf) = self.write(buf).await;
        match result {
            Ok(n) => {
                written += n;
                buf = returned_buf;
            }
            Err(e) => return (Err(e), returned_buf),
        }

        if written < total {
            // 部分書き込みの場合、残りをループで書き込む
            // 簡略実装: WriteZero エラーを返す
            (
                Err(io::Error::new(io::ErrorKind::WriteZero, "partial write")),
                buf,
            )
        } else {
            (Ok(written), buf)
        }
    }
}

// AsyncWriteRent を実装する型は自動的に AsyncWriteRentExt も実装する
impl<T: AsyncWriteRent> AsyncWriteRentExt for T {}

// ====================
// 非同期ファイル I/O（monoio::fs 互換）
// ====================

/// ファイルを読み取る（std::fs::read の非同期版）
///
/// monoio::fs::read の互換実装。
/// ホットパスでは使用しないため、std::fs を使う。
pub async fn read(path: impl AsRef<std::path::Path>) -> io::Result<Vec<u8>> {
    let path = path.as_ref().to_owned();
    // ブロッキング操作を許容（設定ファイル読み込み等のコールドパスのみ使用）
    std::fs::read(path)
}

/// ファイルを削除する（monoio::fs::remove_file の互換実装）
pub async fn remove_file(path: impl AsRef<std::path::Path>) -> io::Result<()> {
    let path = path.as_ref().to_owned();
    std::fs::remove_file(path)
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

    /// オフセット位置から読み取る（pread 使用、コールドパスのみ）
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

    /// オフセット位置からバッファを全部読む（pread ループ）
    pub async fn read_exact_at<T: IoBufMut>(&self, mut buf: T, offset: u64) -> BufResult<usize, T> {
        use std::os::unix::io::AsRawFd;
        let fd = self.inner.as_raw_fd();
        let total = buf.bytes_total();
        let mut read = 0;
        while read < total {
            let ret = unsafe {
                libc::pread(
                    fd,
                    (buf.write_ptr() as *mut u8).add(read) as *mut libc::c_void,
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
}

impl std::os::unix::io::AsRawFd for File {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.inner.as_raw_fd()
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
