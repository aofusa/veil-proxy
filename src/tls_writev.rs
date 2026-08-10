//! # rustls 暗号文の `writev(2)` 直接送出（F-150）
//!
//! rustls の `ConnectionCommon::write_tls` は、内部の暗号文チャンク列
//! （TLS レコード単位の `Vec<u8>` のデック、`ChunkVecBuffer`）を
//! `wr.write_vectored(&[IoSlice; <=64])` **1 回**で吐き出す
//! （rustls-0.23 `src/vecbuf.rs::write_to`）。書き込み先が中間 `Vec<u8>` だと、
//! この `write_vectored` は暗号文をまるごと `Vec` へ memcpy し、さらに容量拡張の
//! `realloc` コピーが乗る（`src/simple_tls.rs`・`src/ktls_rustls.rs` の旧実装）。
//!
//! 本モジュールはソケット fd を直接保持し `writev(2)`（Windows は `WSASend`）を
//! 発行する [`FdVectoredWriter`] を提供し、この中間コピーと malloc/free を
//! 完全に消す（AGENTS.md ホットパス絶対規則「ゼロコピーを徹底する」）。

use std::io::{self, IoSlice, Write};

use crate::runtime::handle::{AsRawFd as _, RawFd};
use crate::runtime::tcp::TcpStream;

// ====================
// iovec 本数の上限
// ====================

/// `writev(2)` に渡せる iovec の最大本数。
///
/// BSD 系（macOS/FreeBSD/OpenBSD/NetBSD/Dragonfly）は `libc::IOV_MAX` を定数として
/// 公開しているためそれを使う。Linux 等 `libc` クレートが `IOV_MAX` を公開しない
/// 環境では、実際のカーネル上限（Linux は `UIO_MAXIOV` = 1024）に合わせた
/// フォールバック定数を使う。rustls の `ChunkVecBuffer::write_to` は実際には
/// 高々 64 個の `IoSlice` しか積まないため、通常はこの上限に達しない
/// （防御的なクランプ）。
#[cfg(unix)]
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
))]
const MAX_IOVEC: usize = libc::IOV_MAX as usize;

#[cfg(unix)]
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
)))]
const MAX_IOVEC: usize = 1024;

/// Windows 版 `WSASend` に渡す `WSABUF` 配列のスタック上限。rustls の
/// `ChunkVecBuffer::write_to` は高々 64 個の `IoSlice` しか積まないため、
/// この本数を超えることはない（防御的なクランプ）。
#[cfg(windows)]
const MAX_WSABUF: usize = 64;

// ====================
// FdVectoredWriter
// ====================

/// ソケット fd を直接保持し `writev(2)`／`WSASend` を発行する `io::Write` 実装。
///
/// rustls の `write_tls` はこの実装に対して `write_vectored` のみを呼ぶ
/// （`ChunkVecBuffer::write_to` の実装契約）が、`io::Write` トレイト契約として
/// `write`/`flush` も実装する。
pub(crate) struct FdVectoredWriter {
    fd: RawFd,
}

impl FdVectoredWriter {
    #[inline]
    pub(crate) fn new(fd: RawFd) -> Self {
        Self { fd }
    }
}

#[cfg(unix)]
impl Write for FdVectoredWriter {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let result =
            unsafe { libc::write(self.fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result as usize)
        }
    }

    #[inline]
    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        if bufs.is_empty() {
            return Ok(0);
        }
        let len = bufs.len().min(MAX_IOVEC);

        // SAFETY: `std::io::IoSlice` は Unix において `libc::iovec`（`{iov_base, iov_len}`）
        // と同一のメモリレイアウトであることが標準ライブラリで保証されている
        // （`IoSlice` は unix 実装上 `iovec` を `repr(transparent)` でラップするのみ）。
        // よってポインタキャストのみで安全に `writev` へ渡せる。渡す本数は `MAX_IOVEC`
        // でクランプ済みであり、`bufs` の実際の要素数を超えないため範囲外アクセスは
        // 発生しない。
        let iov = bufs.as_ptr() as *const libc::iovec;
        let result = unsafe { libc::writev(self.fd, iov, len as libc::c_int) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result as usize)
        }
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
impl Write for FdVectoredWriter {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        use crate::runtime::handle::win;
        let result = unsafe {
            windows_sys::Win32::Networking::WinSock::send(
                win::to_socket(self.fd),
                buf.as_ptr(),
                buf.len() as i32,
                0,
            )
        };
        if result < 0 {
            Err(io::Error::from_raw_os_error(unsafe {
                windows_sys::Win32::Networking::WinSock::WSAGetLastError()
            }))
        } else {
            Ok(result as usize)
        }
    }

    #[inline]
    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        if bufs.is_empty() {
            return Ok(0);
        }
        use crate::runtime::handle::win;
        use windows_sys::Win32::Networking::WinSock;

        let len = bufs.len().min(MAX_WSABUF);
        let mut wsabufs = [WinSock::WSABUF {
            len: 0,
            buf: std::ptr::null_mut(),
        }; MAX_WSABUF];
        for (dst, src) in wsabufs.iter_mut().zip(bufs.iter()).take(len) {
            *dst = WinSock::WSABUF {
                len: src.len() as u32,
                buf: src.as_ptr() as *mut u8,
            };
        }

        let mut sent: u32 = 0;
        let ret = unsafe {
            WinSock::WSASend(
                win::to_socket(self.fd),
                wsabufs.as_ptr(),
                len as u32,
                &mut sent,
                0,
                std::ptr::null_mut(),
                None,
            )
        };
        if ret == 0 {
            Ok(sent as usize)
        } else {
            Err(io::Error::from_raw_os_error(unsafe {
                WinSock::WSAGetLastError()
            }))
        }
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ====================
// TlsWriteSink
// ====================

/// `rustls::ServerConnection` と `rustls::ClientConnection` の両方を同じ
/// [`flush_tls_writev`] ヘルパで扱うための最小トレイト。
pub(crate) trait TlsWriteSink {
    fn sink_wants_write(&self) -> bool;
    fn sink_write_tls(&mut self, wr: &mut dyn io::Write) -> io::Result<usize>;
}

impl TlsWriteSink for rustls::ServerConnection {
    #[inline]
    fn sink_wants_write(&self) -> bool {
        self.wants_write()
    }

    #[inline]
    fn sink_write_tls(&mut self, wr: &mut dyn io::Write) -> io::Result<usize> {
        self.write_tls(wr)
    }
}

impl TlsWriteSink for rustls::ClientConnection {
    #[inline]
    fn sink_wants_write(&self) -> bool {
        self.wants_write()
    }

    #[inline]
    fn sink_write_tls(&mut self, wr: &mut dyn io::Write) -> io::Result<usize> {
        self.write_tls(wr)
    }
}

// ====================
// flush_tls_writev
// ====================

/// rustls コネクションが保持する送信待ちの暗号文をすべて `writev(2)` で吐き出す。
///
/// # 不変条件
///
/// `WouldBlock` が返るとき、rustls の `ChunkVecBuffer::write_to` は
/// （呼び出した `write_vectored` が `Err` を返したため）`consume` を呼ばずに
/// エラーを伝播する。つまり送信待ちキューのデータは 1 バイトも失われない。
/// `stream.writable().await` の後に `sink_write_tls` を再実行すれば、
/// 同じデータが（今度は成功する分だけ）再送出される。
///
/// `Ok(0)` は「送るものが無い」ことを示す防御的なケースで、無限ループを避けるため
/// ここで打ち切る（`sink_wants_write()` が真のまま `Ok(0)` を返すことは通常
/// 起こらないが、そうなった場合でもハングしないようにする）。
pub(crate) async fn flush_tls_writev(
    stream: &TcpStream,
    conn: &mut impl TlsWriteSink,
) -> io::Result<()> {
    while conn.sink_wants_write() {
        let mut w = FdVectoredWriter::new(stream.as_raw_fd());
        match conn.sink_write_tls(&mut w) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                stream.writable().await?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// ====================
// テスト
// ====================

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::io::RawFd as StdRawFd;

    /// テスト用の `socketpair(AF_UNIX, SOCK_STREAM)` ペアを作る。戻り値は
    /// `(送信側 fd, 受信側 fd)`。呼び出し側で `close` する。
    fn make_socketpair() -> (StdRawFd, StdRawFd) {
        let mut fds = [0i32; 2];
        let ret =
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(ret, 0, "socketpair failed: {}", io::Error::last_os_error());
        (fds[0], fds[1])
    }

    fn set_nonblocking(fd: StdRawFd) {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL, 0) };
        assert!(flags >= 0);
        let ret = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        assert_eq!(ret, 0);
    }

    fn set_sndbuf(fd: StdRawFd, size: i32) {
        let ret = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                &size as *const i32 as *const libc::c_void,
                std::mem::size_of::<i32>() as libc::socklen_t,
            )
        };
        assert_eq!(ret, 0, "setsockopt(SO_SNDBUF) failed");
    }

    fn read_all_available(fd: StdRawFd, max: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut chunk = vec![0u8; 65536];
        loop {
            let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&chunk[..n as usize]);
            if out.len() >= max {
                break;
            }
        }
        out
    }

    fn close_fd(fd: StdRawFd) {
        unsafe {
            libc::close(fd);
        }
    }

    #[test]
    fn write_vectored_matches_contiguous_write() {
        let (tx, rx) = make_socketpair();

        let part_a = b"hello, ".to_vec();
        let part_b = b"tls writev".to_vec();
        let part_c = b"! zero-copy".to_vec();
        let expected = [part_a.as_slice(), part_b.as_slice(), part_c.as_slice()].concat();

        let mut writer = FdVectoredWriter::new(tx);
        let iov = [
            IoSlice::new(&part_a),
            IoSlice::new(&part_b),
            IoSlice::new(&part_c),
        ];
        let n = writer.write_vectored(&iov).expect("write_vectored failed");
        assert_eq!(n, expected.len());

        let received = read_all_available(rx, expected.len());
        assert_eq!(received, expected);

        close_fd(tx);
        close_fd(rx);
    }

    #[test]
    fn write_vectored_partial_write_reports_actual_count() {
        let (tx, rx) = make_socketpair();
        set_nonblocking(tx);
        // 送信バッファを小さく絞る（カーネルは指定値の概ね倍程度に丸めることが多いが、
        // いずれにせよ「大きな入力に対して有限」であればテストは成立する）。
        set_sndbuf(tx, 4096);

        // 合計 512KB 分の IoSlice を積み、送信バッファを確実に超えさせる。
        let chunk = vec![0xABu8; 4096];
        let iov: Vec<IoSlice<'_>> = (0..128).map(|_| IoSlice::new(&chunk)).collect();
        let total: usize = iov.iter().map(|s| s.len()).sum();

        let mut writer = FdVectoredWriter::new(tx);
        let n = writer.write_vectored(&iov).expect("write_vectored failed");

        assert!(n > 0, "expected some bytes written");
        assert!(n < total, "expected a partial write, got {n} of {total}");

        let received = read_all_available(rx, n);
        assert_eq!(received.len(), n);

        close_fd(tx);
        close_fd(rx);
    }

    #[test]
    fn write_vectored_would_block_when_full() {
        let (tx, rx) = make_socketpair();
        set_nonblocking(tx);
        set_sndbuf(tx, 4096);

        let mut writer = FdVectoredWriter::new(tx);
        let chunk = vec![0xCDu8; 4096];

        // 送信バッファ + 受信バッファが埋まるまで書き込み続ける。
        let mut filled = false;
        for _ in 0..64 {
            let iov = [IoSlice::new(&chunk)];
            match writer.write_vectored(&iov) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    filled = true;
                    break;
                }
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert!(filled, "expected WouldBlock after filling send buffer");

        close_fd(tx);
        close_fd(rx);
    }

    #[test]
    fn write_vectored_empty_is_zero() {
        let (tx, rx) = make_socketpair();
        let mut writer = FdVectoredWriter::new(tx);
        let n = writer.write_vectored(&[]).expect("write_vectored failed");
        assert_eq!(n, 0);
        close_fd(tx);
        close_fd(rx);
    }

    #[test]
    fn iov_max_clamp() {
        let (tx, rx) = make_socketpair();
        set_nonblocking(tx);

        // MAX_IOVEC を上回る本数の 1 バイト IoSlice を渡してもエラーにならず、
        // クランプされた本数ぶんが送られること（エラー無しであれば十分）。
        let bytes = vec![0x41u8; MAX_IOVEC + 100];
        let iov: Vec<IoSlice<'_>> = bytes
            .iter()
            .map(std::slice::from_ref)
            .map(IoSlice::new)
            .collect();

        let mut writer = FdVectoredWriter::new(tx);
        let n = writer.write_vectored(&iov).expect("write_vectored failed");
        assert!(n > 0);
        assert!(n <= MAX_IOVEC);

        let received = read_all_available(rx, n);
        assert_eq!(received.len(), n);

        close_fd(tx);
        close_fd(rx);
    }
}
