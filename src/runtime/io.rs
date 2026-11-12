//! 非同期 I/O トレイト（所有権ベース）
//!
//! `AsyncReadRent` / `AsyncWriteRentExt` を自前定義する。io_uring の完了型 I/O に合わせ、
//! バッファの所有権を I/O の間カーネル側へ渡し、完了時に返す。

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

// BufResult: 結果とバッファを一緒に返す型エイリアス
pub type BufResult<T, B> = (io::Result<T>, B);

/// 非同期読み取りトレイト（所有権ベース）
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

    /// 直前に `read` が返した EOF（`Ok(0)`）が相手の正しい終了か（F-282）。TLS は close_notify を受けたときだけ
    /// `true`（受けずに TCP が閉じたら切り詰めの可能性がある）。TLS 以外は常に `true`
    fn eof_was_clean(&self) -> bool {
        true
    }
}

/// 非同期書き込みトレイト（所有権ベース）
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

/// AsyncWriteRent の拡張メソッド
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
// 非同期ファイル I/O
// ====================

/// ファイルを読み取る（std::fs::read の非同期版）
///
/// ファイル全体の読み込み。ホットパス（静的ファイル配信の memory モード等）からも
/// 呼ばれるため、ブロッキング FS を offload（専用スレッドプール + eventfd 完了待機）へ
/// 退避してイベントループを塞がない（B-26。リング未初期化のコンテキストでは inline 実行）。
pub async fn read(path: impl AsRef<std::path::Path>) -> io::Result<Vec<u8>> {
    let path = path.as_ref().to_owned();
    // 理由付き allow: offload ワーカースレッド内で実行されるためイベントループ非ブロック。
    #[allow(clippy::disallowed_methods)]
    crate::runtime::offload::offload(move || std::fs::read(path)).await
}

/// ファイルを削除する
///
/// B-26: offload 経由でイベントループを塞がない。
pub async fn remove_file(path: impl AsRef<std::path::Path>) -> io::Result<()> {
    let path = path.as_ref().to_owned();
    // 理由付き allow: offload ワーカースレッド内で実行されるためイベントループ非ブロック。
    #[allow(clippy::disallowed_methods)]
    crate::runtime::offload::offload(move || std::fs::remove_file(path)).await
}

// ====================
// 非同期ファイル
// ====================

/// 非同期ファイル（簡略実装、コールドパスのみ使用）
pub struct File {
    inner: FileInner,
}

/// 所有する fd か、open_file_cache と共有する fd（静的配信の fd キャッシュ）か。
enum FileInner {
    Owned(std::fs::File),
    Shared(std::sync::Arc<std::fs::File>),
}

impl FileInner {
    #[inline]
    fn get(&self) -> &std::fs::File {
        match self {
            FileInner::Owned(f) => f,
            FileInner::Shared(f) => f,
        }
    }
}

impl File {
    /// ファイルを開く
    pub async fn open(path: impl AsRef<std::path::Path>) -> io::Result<Self> {
        let inner = std::fs::File::open(path)?;
        Ok(Self {
            inner: FileInner::Owned(inner),
        })
    }

    /// open_file_cache が保持する開いた fd を共有する（open/close を伴わない）。
    ///
    /// 読み取りは `pread`/`sendfile` のようにオフセットを明示する API だけを使うため、
    /// 複数のリクエストが同じ fd を同時に使ってもファイル位置を奪い合わない。
    #[inline]
    pub fn from_shared(file: std::sync::Arc<std::fs::File>) -> Self {
        Self {
            inner: FileInner::Shared(file),
        }
    }

    /// ファイルを読み取る
    pub async fn read_to_end(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        use std::io::Read;
        let mut f: &std::fs::File = self.inner.get();
        f.read_to_end(buf)
    }

    /// オフセット位置から読み取る（Unix: `pread`、Windows: `seek_read`）。
    ///
    /// F-304: Linux はページキャッシュにある分を `preadv2(RWF_NOWAIT)` でワーカーの上で読み、ディスクを待つときだけ
    /// offload で読む（ワーカーを塞がない）。`RWF_NOWAIT` を受け付けないファイルシステム（overlayfs 等）は従来の `pread`
    #[cfg(unix)]
    pub async fn read_at<T: IoBufMut>(&self, mut buf: T, offset: u64) -> BufResult<usize, T> {
        use std::os::unix::io::AsRawFd;
        let fd = self.inner.get().as_raw_fd();
        #[cfg(target_os = "linux")]
        {
            let len = buf.bytes_total();
            match nowait::pread(fd, buf.write_ptr(), len, offset) {
                nowait::Read::Done(n) => {
                    // SAFETY: カーネルが先頭 n バイトを書いた
                    unsafe {
                        buf.set_init(n);
                    }
                    return (Ok(n), buf);
                }
                nowait::Read::Miss => {
                    return match self.read_offloaded(offset, len, false).await {
                        Ok(data) => {
                            // SAFETY: data.len() <= len（offload は len までしか読まない）
                            unsafe {
                                std::ptr::copy_nonoverlapping(
                                    data.as_ptr(),
                                    buf.write_ptr(),
                                    data.len(),
                                );
                                buf.set_init(data.len());
                            }
                            (Ok(data.len()), buf)
                        }
                        Err(e) => (Err(e), buf),
                    };
                }
                nowait::Read::Unsupported => {}
            }
        }
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

    /// ページキャッシュに無い範囲を offload で読む（F-304）。書き込み先は閉包が持つ `Vec`（offload の途中で要求が
    /// 取り消されても、呼び出し側のバッファへ書かない）。`exact` なら `len` まで（EOF で止まる）読み切る
    #[cfg(target_os = "linux")]
    async fn read_offloaded(&self, offset: u64, len: usize, exact: bool) -> io::Result<Vec<u8>> {
        let file = match &self.inner {
            FileInner::Shared(f) => std::sync::Arc::clone(f),
            FileInner::Owned(f) => std::sync::Arc::new(f.try_clone()?),
        };
        crate::runtime::offload::offload(move || {
            use std::os::unix::fs::FileExt;
            let mut data = vec![0u8; len];
            let mut n = 0;
            while n < len {
                match file.read_at(&mut data[n..], offset + n as u64) {
                    Ok(0) => break,
                    Ok(k) => {
                        n += k;
                        if !exact {
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            data.truncate(n);
            Ok(data)
        })
        .await
    }

    /// オフセット位置から読み取る（Windows 版: `FileExt::seek_read`）。
    #[cfg(windows)]
    pub async fn read_at<T: IoBufMut>(&self, mut buf: T, offset: u64) -> BufResult<usize, T> {
        use std::os::windows::fs::FileExt;
        let slice = unsafe { std::slice::from_raw_parts_mut(buf.write_ptr(), buf.bytes_total()) };
        match self.inner.get().seek_read(slice, offset) {
            Ok(n) => {
                unsafe {
                    buf.set_init(n);
                }
                (Ok(n), buf)
            }
            Err(e) => (Err(e), buf),
        }
    }

    /// オフセット位置からバッファを全部読む（Unix: pread ループ。F-304: Linux は [`File::read_at`] と同じく
    /// ページキャッシュにある分をワーカーの上で読み、残りがディスクを待つなら残りを 1 回の offload で読む）
    #[cfg(unix)]
    pub async fn read_exact_at<T: IoBufMut>(&self, mut buf: T, offset: u64) -> BufResult<usize, T> {
        use std::os::unix::io::AsRawFd;
        let fd = self.inner.get().as_raw_fd();
        let total = buf.bytes_total();
        let mut read = 0;
        while read < total {
            #[cfg(target_os = "linux")]
            {
                // SAFETY: read < total なので書き込み先はバッファの中
                let ptr = unsafe { buf.write_ptr().add(read) };
                match nowait::pread(fd, ptr, total - read, offset + read as u64) {
                    nowait::Read::Done(0) => break, // EOF
                    nowait::Read::Done(n) => {
                        read += n;
                        continue;
                    }
                    nowait::Read::Miss => {
                        match self
                            .read_offloaded(offset + read as u64, total - read, true)
                            .await
                        {
                            Ok(data) => {
                                // SAFETY: data.len() <= total - read（offload は残りの長さまでしか読まない）
                                unsafe {
                                    std::ptr::copy_nonoverlapping(
                                        data.as_ptr(),
                                        buf.write_ptr().add(read),
                                        data.len(),
                                    );
                                }
                                read += data.len();
                            }
                            Err(e) => return (Err(e), buf),
                        }
                        // offload は EOF か残り全部まで読んだ
                        break;
                    }
                    nowait::Read::Unsupported => {}
                }
            }
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
            let slice =
                unsafe { std::slice::from_raw_parts_mut(buf.write_ptr().add(read), total - read) };
            match self.inner.get().seek_read(slice, offset + read as u64) {
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

/// `len` バイトのファイル全体をページキャッシュだけから読む（F-304。`preadv2(RWF_NOWAIT)` なのでワーカーの上で呼べる）。
/// ページキャッシュに無い・`RWF_NOWAIT` を受け付けない・ファイルが縮んだときは `None`（呼び出し側は offload の経路で読む）。
/// 使うのは静的配信の本体キャッシュ（`cache` feature）だけ
#[cfg(all(target_os = "linux", feature = "cache"))]
pub(crate) fn read_cached_exact(file: &std::fs::File, len: usize) -> Option<Vec<u8>> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    let mut buf: Vec<u8> = Vec::with_capacity(len);
    let mut read = 0usize;
    while read < len {
        // SAFETY: read < len <= capacity なので書き込み先は確保済みの領域の中
        let ptr = unsafe { buf.as_mut_ptr().add(read) };
        match nowait::pread(fd, ptr, len - read, read as u64) {
            nowait::Read::Done(0) => return None,
            nowait::Read::Done(n) => read += n,
            nowait::Read::Miss | nowait::Read::Unsupported => return None,
        }
    }
    // SAFETY: 先頭 len バイトをカーネルが書いた
    unsafe { buf.set_len(len) };
    Some(buf)
}

/// ページキャッシュにある分だけを読む `preadv2(RWF_NOWAIT)`（F-304。Linux 4.14+）
#[cfg(target_os = "linux")]
mod nowait {
    use std::io;
    use std::os::unix::io::RawFd;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// `<linux/fs.h>` の `RWF_NOWAIT`
    const RWF_NOWAIT: libc::c_int = 0x8;

    /// `RWF_NOWAIT` を受け付けないファイル（`FMODE_NOWAIT` を持たない overlayfs・9p 等）・`preadv2` の無いカーネル・
    /// seccomp で拒否する環境を一度でも見たか。見たら以後は従来の `pread` だけを使う（失敗する syscall を毎回足さない。
    /// 対応するファイルシステムと混在する配置では、そちらも従来の `pread` になる）
    static UNSUPPORTED: AtomicBool = AtomicBool::new(false);

    pub(super) enum Read {
        /// 読めた（0 は EOF）
        Done(usize),
        /// 先頭がページキャッシュに無い（読むにはディスクを待つ）
        Miss,
        /// ここでは決めない（呼び出し側の従来の `pread` が読み、エラーも従来どおりに返す）
        Unsupported,
    }

    /// `fd` の `offset` から `ptr..ptr+len` へ、ページキャッシュにある分だけ読む
    pub(super) fn pread(fd: RawFd, ptr: *mut u8, len: usize, offset: u64) -> Read {
        if UNSUPPORTED.load(Ordering::Relaxed) {
            return Read::Unsupported;
        }
        let iov = libc::iovec {
            iov_base: ptr.cast(),
            iov_len: len,
        };
        loop {
            // SAFETY: ptr..ptr+len は呼び出し側が持つ書き込み可能な領域、iov はこの呼び出しの間だけ有効なスタックの値。
            // 位置は (pos_l, pos_h) で渡す（64 ビットのカーネルは pos_l だけを使い、32 ビットは上位を pos_h から取る）
            let ret = unsafe {
                libc::syscall(
                    libc::SYS_preadv2,
                    fd,
                    &iov as *const libc::iovec,
                    1 as libc::c_int,
                    offset as libc::c_ulong,
                    ((offset >> 16) >> 16) as libc::c_ulong,
                    RWF_NOWAIT,
                )
            };
            if ret >= 0 {
                return Read::Done(ret as usize);
            }
            let e = io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(libc::EAGAIN) => Read::Miss,
                Some(libc::EINTR) => continue,
                Some(libc::EOPNOTSUPP | libc::ENOSYS | libc::EPERM) => {
                    UNSUPPORTED.store(true, Ordering::Relaxed);
                    Read::Unsupported
                }
                _ => Read::Unsupported,
            };
        }
    }
}

#[cfg(unix)]
impl std::os::unix::io::AsRawFd for File {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.inner.get().as_raw_fd()
    }
}

#[cfg(windows)]
impl std::os::windows::io::AsRawHandle for File {
    fn as_raw_handle(&self) -> std::os::windows::io::RawHandle {
        std::os::windows::io::AsRawHandle::as_raw_handle(self.inner.get())
    }
}

/// OpenOptions
pub struct OpenOptions {
    inner: std::fs::OpenOptions,
    // F-123/F-153: FreeBSD capability mode / Linux openat2 で読み取り専用 open を
    // dirfd 相対の封じ込め付き open へ切り替えるため、書き込み系フラグの有無を追跡する
    // （読み取り専用のみ相対化対象。書き込み系は登録済み静的ルート dirfd に書き込み
    // 権限が無いため、常に通常経路を使う）。
    #[cfg(any(target_os = "freebsd", target_os = "linux"))]
    write_like: bool,
}

impl OpenOptions {
    pub fn new() -> Self {
        Self {
            inner: std::fs::OpenOptions::new(),
            #[cfg(any(target_os = "freebsd", target_os = "linux"))]
            write_like: false,
        }
    }

    pub fn read(mut self, read: bool) -> Self {
        self.inner.read(read);
        self
    }

    pub fn write(mut self, write: bool) -> Self {
        self.inner.write(write);
        #[cfg(any(target_os = "freebsd", target_os = "linux"))]
        {
            self.write_like |= write;
        }
        self
    }

    pub fn create(mut self, create: bool) -> Self {
        self.inner.create(create);
        #[cfg(any(target_os = "freebsd", target_os = "linux"))]
        {
            self.write_like |= create;
        }
        self
    }

    pub fn append(mut self, append: bool) -> Self {
        self.inner.append(append);
        #[cfg(any(target_os = "freebsd", target_os = "linux"))]
        {
            self.write_like |= append;
        }
        self
    }

    /// 開く。F-304: 読み取り専用で経路と属性がカーネルのキャッシュにあれば（Linux）ワーカーの上でブロックせずに開き、
    /// それ以外（ディスクを読み得る open）は offload で開く（ワーカーを塞がない）
    pub async fn open(self, path: impl AsRef<std::path::Path>) -> io::Result<File> {
        let path = path.as_ref();
        #[cfg(target_os = "linux")]
        if !self.write_like {
            if let Some(res) = crate::cache::resolve::open_cached_for_request(path) {
                return res.map(|(inner, _st)| File {
                    inner: FileInner::Owned(inner),
                });
            }
        }
        let path = path.to_path_buf();
        crate::runtime::offload::offload(move || self.open_blocking(&path)).await
    }

    /// 同期の open（offload の中で呼ぶ）
    fn open_blocking(self, path: &std::path::Path) -> io::Result<File> {
        // F-123: FreeBSD capability mode 下では絶対パス open が禁止されるため、読み取り
        // 専用 open は登録済みルート dirfd 相対の openat へ切り替える（非該当・非 cap-mode
        // 時は下の通常経路。Linux/その他 OS はこのブロック自体がコンパイルされない）。
        #[cfg(target_os = "freebsd")]
        if !self.write_like {
            if let Some(res) = crate::security::capsicum::open_static_ro(path) {
                return res.map(|inner| File {
                    inner: FileInner::Owned(inner),
                });
            }
        }
        // F-153: Linux は登録済み静的ルート dirfd に対する openat2(RESOLVE_BENEATH) へ
        // 切り替える（`readlink` を伴う旧経路の canonicalize を経由しない）。対象外
        // （未登録ルート・古いカーネルで openat2 が恒久的に利用不可と判明済み）の場合は
        // `None` が返るため、下の通常経路（`std::fs::OpenOptions`）へ安全にフォールバックする。
        #[cfg(target_os = "linux")]
        if !self.write_like {
            if let Some(res) = crate::cache::resolve::open_beneath_for_request(path) {
                return res.map(|(inner, _meta)| File {
                    inner: FileInner::Owned(inner),
                });
            }
        }
        let inner = self.inner.open(path)?;
        Ok(File {
            inner: FileInner::Owned(inner),
        })
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

    /// F-304: `read_at`・`read_exact_at` が位置・長さ・EOF を従来の `pread` と同じに扱うこと（ページキャッシュに
    /// ある分は `preadv2(RWF_NOWAIT)`、無ければ offload）。ページキャッシュを捨てた後も同じ内容を返す
    #[cfg(target_os = "linux")]
    #[test]
    fn read_at_and_read_exact_at_match_file_contents() {
        use std::io::Write;
        use std::os::unix::io::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f304_read.bin");
        let data: Vec<u8> = (0..200_000u32).map(|i| (i * 7 + i / 251) as u8).collect();
        {
            // 理由付き allow: テストのフィクスチャ作成（ホットパス外）
            #[allow(clippy::disallowed_methods)]
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(&data).unwrap();
            f.sync_all().unwrap();
        }
        // 理由付き allow: テストのフィクスチャ（ホットパス外）
        #[allow(clippy::disallowed_methods)]
        let std_file = std::fs::File::open(&path).unwrap();
        let check = |file: &File| {
            for &(off, len) in &[(0usize, 4096usize), (70_000, 65_536), (199_000, 4096)] {
                let (res, buf) =
                    futures::executor::block_on(file.read_at(Vec::with_capacity(len), off as u64));
                let n = res.unwrap();
                assert!(n > 0 && n <= len);
                assert_eq!(&buf[..n], &data[off..off + n]);
            }
            let (res, buf) =
                futures::executor::block_on(file.read_exact_at(Vec::with_capacity(data.len()), 0));
            assert_eq!(res.unwrap(), data.len());
            assert_eq!(buf, data);
            // EOF を越える長さは EOF まで
            let (res, buf) =
                futures::executor::block_on(file.read_exact_at(Vec::with_capacity(8192), 196_000));
            assert_eq!(res.unwrap(), 4000);
            assert_eq!(buf, &data[196_000..]);
            // EOF の位置は 0
            let (res, _) =
                futures::executor::block_on(file.read_at(Vec::with_capacity(16), 200_000));
            assert_eq!(res.unwrap(), 0);
        };
        let shared = File::from_shared(std::sync::Arc::new(std_file.try_clone().unwrap()));
        check(&shared);
        // ページキャッシュを捨てる（書き戻し済みのページは特権なしで捨てられる）。対応するファイルシステムでは
        // RWF_NOWAIT が EAGAIN になり、offload の経路で読む
        // SAFETY: 開いている fd への助言だけ
        unsafe {
            libc::posix_fadvise(std_file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
        }
        let owned = File {
            inner: FileInner::Owned(std_file),
        };
        check(&owned);
    }

    /// F-304: offload の読み取りは呼び出し側のバッファへ書かず、`exact` なら EOF まで・そうでなければ 1 回だけ読む
    #[cfg(target_os = "linux")]
    #[test]
    fn read_offloaded_reads_requested_range() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f304_offload.bin");
        let data: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        // 理由付き allow: テストのフィクスチャ作成（ホットパス外）
        #[allow(clippy::disallowed_methods)]
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&data).unwrap();
        // 理由付き allow: テストのフィクスチャ（ホットパス外）
        #[allow(clippy::disallowed_methods)]
        let file = File {
            inner: FileInner::Owned(std::fs::File::open(&path).unwrap()),
        };
        let got = futures::executor::block_on(file.read_offloaded(1000, 5000, true)).unwrap();
        assert_eq!(got, &data[1000..6000]);
        let got = futures::executor::block_on(file.read_offloaded(9000, 5000, true)).unwrap();
        assert_eq!(got, &data[9000..]);
        let got = futures::executor::block_on(file.read_offloaded(2000, 100, false)).unwrap();
        assert_eq!(got, &data[2000..2100]);
        let got = futures::executor::block_on(file.read_offloaded(10_000, 100, true)).unwrap();
        assert!(got.is_empty());
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
