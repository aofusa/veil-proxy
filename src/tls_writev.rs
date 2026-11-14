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
    /// close_notify のアラートを送信待ちに積む（積み済み・致命的なアラートを送った後なら何もしない）
    fn sink_send_close_notify(&mut self);
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

    #[inline]
    fn sink_send_close_notify(&mut self) {
        self.send_close_notify();
    }
}

/// Box で持つコネクション（`KtlsServerStream` / `SimpleTlsServerStream`）用。
impl<T: TlsWriteSink + ?Sized> TlsWriteSink for Box<T> {
    #[inline]
    fn sink_wants_write(&self) -> bool {
        (**self).sink_wants_write()
    }

    #[inline]
    fn sink_write_tls(&mut self, wr: &mut dyn io::Write) -> io::Result<usize> {
        (**self).sink_write_tls(wr)
    }

    #[inline]
    fn sink_send_close_notify(&mut self) {
        (**self).sink_send_close_notify()
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

    #[inline]
    fn sink_send_close_notify(&mut self) {
        self.send_close_notify();
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
// close_notify（B-401）
// ====================
//
// RFC 8446 §6.1: 誤りのアラートを送っていない限り、書き込み側を閉じる前に close_notify を送る。送らずに閉じると、
// EOF まで読む OpenSSL 3 のクライアントは切り詰めの攻撃と見分けられないので、その接続のセッションを捨てる（次の接続が
// 完全なハンドシェイクになる）。veil は TLS の接続を close_notify 無しで閉じていたので、`Connection: close` の応答を
// EOF まで読むクライアントが毎回完全なハンドシェイクをしていた。

/// 閉じる直前の送出に使う、待たない `io::Write`。`MSG_DONTWAIT` で送信バッファが一杯なら `WouldBlock` を返し
/// （io_uring の CONNECT で作ったソケットはノンブロッキングとは限らない）、`MSG_NOSIGNAL` で相手が先に閉じていても
/// SIGPIPE を上げない（veil は SIGPIPE を無視しないプロセスにライブラリとして組み込まれることがある）。
///
/// macOS の送信は `MSG_DONTWAIT` を見ない（ブロッキングのソケットでは空きを待つ。`O_NONBLOCK` だけを見る）。macOS の
/// veil のソケットは reactor が作るので常にノンブロッキング（`create_nonblocking_socket`・accept の `fcntl`）で、待たない。
#[cfg(unix)]
struct NoWaitWriter {
    fd: RawFd,
}

#[cfg(unix)]
impl Write for NoWaitWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_vectored(&[IoSlice::new(buf)])
    }

    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        if bufs.is_empty() {
            return Ok(0);
        }
        // macOS には `MSG_NOSIGNAL` が無い（reactor の TcpStream と同じく、ソケットの `SO_NOSIGPIPE` で抑える）
        #[cfg(target_os = "macos")]
        const FLAGS: libc::c_int = libc::MSG_DONTWAIT;
        #[cfg(not(target_os = "macos"))]
        const FLAGS: libc::c_int = libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL;
        // SAFETY: msghdr はゼロ初期化が正しい初期値。`IoSlice` は unix で `iovec` と同じレイアウト
        // （`FdVectoredWriter::write_vectored` と同じ）で、指す先は呼び出しの間だけ生きる `bufs`。
        // 本数は `MAX_IOVEC` でクランプし、`bufs` の要素数を超えない。sendmsg は iovec を読むだけ
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = bufs.as_ptr() as *mut libc::iovec;
        msg.msg_iovlen = bufs.len().min(MAX_IOVEC) as _;
        let ret = unsafe { libc::sendmsg(self.fd, &msg, FLAGS) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret as usize)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(unix)]
#[inline]
fn no_wait_writer(fd: RawFd) -> NoWaitWriter {
    NoWaitWriter { fd }
}

/// Windows のソケットはノンブロッキング（WSAPoll の reactor）で、SIGPIPE も無い。
#[cfg(windows)]
#[inline]
fn no_wait_writer(fd: RawFd) -> FdVectoredWriter {
    FdVectoredWriter::new(fd)
}

/// rustls の送信待ちの暗号文を、待たずに書けるだけ書く。
fn write_tls_now(fd: RawFd, conn: &mut impl TlsWriteSink) -> io::Result<()> {
    while conn.sink_wants_write() {
        let mut w = no_wait_writer(fd);
        if conn.sink_write_tls(&mut w)? == 0 {
            break;
        }
    }
    Ok(())
}

/// close_notify を積んで、待たずに送る。`Drop` で閉じる直前に呼ぶ（待てない）。送信バッファが一杯・相手が閉じた等で
/// 送れなければ諦める（接続を閉じることに変わりは無い）。接続ごとに 1 回の `sendmsg(2)`。
pub(crate) fn send_close_notify_now(fd: RawFd, conn: &mut impl TlsWriteSink) {
    conn.sink_send_close_notify();
    let _ = write_tls_now(fd, conn);
}

/// close_notify を積んで送る。送信バッファが空くのを `wait` まで待つ（`AsyncWriteRent::shutdown` から呼ぶ）。
pub(crate) async fn send_close_notify(
    stream: &TcpStream,
    conn: &mut impl TlsWriteSink,
    wait: std::time::Duration,
) {
    conn.sink_send_close_notify();
    let fd = stream.as_raw_fd();
    let _ = crate::runtime::time::timeout(wait, async {
        loop {
            match write_tls_now(fd, conn) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if stream.writable().await.is_err() {
                        return;
                    }
                }
                _ => return,
            }
        }
    })
    .await;
}

/// `AsyncWriteRent::shutdown` で close_notify の送出を待つ上限（相手が読まずに送信バッファが空かない接続で
/// 閉じる処理を止めない。HTTP/2 の GOAWAY の後の読み捨ての上限と同じ）。
pub(crate) const CLOSE_NOTIFY_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

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

/// 受信した暗号文 `data` を**全量** rustls へ投入し、レコードを処理する（B-90）。
///
/// rustls の `read_tls` は 1 回の呼び出しで最大 4KB（deframer の `READ_SIZE`）しか
/// 取り込まない。`conn.read_tls(&mut &buf[..n])` を 1 回だけ呼んで戻り値を捨てると、
/// `n` が 4KB を超えたときに**残りの暗号文が黙って失われ**、以降のレコード境界がずれて
/// 相手側は `cannot decrypt peer's message` になる。ハンドシェイクでは、クライアントの
/// Finished と直後のアプリケーションデータ（リクエストヘッダ + ボディ先頭）が 1 回の
/// read に入ると発生する（FreeBSD の E2E で HTTP/3 → TLS バックエンドの 1.5MB アップロードが
/// 毎回失敗していた B-68 の真因）。
///
/// ハンドシェイク中・直後の 1 回分（最大 16KB の暗号文）は受信平文上限（16KB）を超えないため、
/// ここでは平文の排出は行わない（排出が必要なデータ転送経路は各ストリームの read が担う）。
pub(crate) fn read_tls_all<D>(
    conn: &mut rustls::ConnectionCommon<D>,
    data: &[u8],
) -> io::Result<()> {
    let mut consumed = 0;
    while consumed < data.len() {
        let n = conn.read_tls(&mut &data[consumed..])?;
        if n == 0 {
            // close_notify 受信後は rustls がこれ以上読まない（以降のバイトは無意味）。
            break;
        }
        consumed += n;
        conn.process_new_packets()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    }
    Ok(())
}

#[cfg(test)]
mod read_tls_all_tests {
    use super::read_tls_all;
    use std::io::{Read as _, Write as _};
    use std::sync::Arc;

    /// サーバが Finished 待ちになるところまでメモリ上でハンドシェイクを進める。
    pub(super) fn pair_until_client_finished(
    ) -> (rustls::ClientConnection, rustls::ServerConnection) {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let provider = Arc::new(crate::tls_provider::provider::default_provider());
        let cert_der = ck.cert.der().clone();
        let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()),
        );
        let server_cfg = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert_der).unwrap();
        let client_cfg = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let mut client = rustls::ClientConnection::new(
            Arc::new(client_cfg),
            rustls::pki_types::ServerName::try_from("localhost").unwrap(),
        )
        .unwrap();
        let mut server = rustls::ServerConnection::new(Arc::new(server_cfg)).unwrap();
        // ClientHello → サーバの応答フライト → クライアントがハンドシェイク完了（Finished は未送出）。
        let mut buf = Vec::new();
        client.write_tls(&mut buf).unwrap();
        read_tls_all(&mut server, &buf).unwrap();
        let mut buf = Vec::new();
        while server.wants_write() {
            server.write_tls(&mut buf).unwrap();
        }
        read_tls_all(&mut client, &buf).unwrap();
        assert!(!client.is_handshaking());
        assert!(
            server.is_handshaking(),
            "server must still wait for client Finished"
        );
        (client, server)
    }

    /// クライアントの Finished と 4KB を超えるアプリケーションデータが 1 回の read に
    /// 入っても、全量がサーバへ届くこと（B-90。単発の `read_tls` だと 4KB 以降が失われる）。
    #[test]
    fn finished_and_app_data_in_one_read_are_fully_consumed() {
        let (mut client, mut server) = pair_until_client_finished();
        let payload: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        client.writer().write_all(&payload).unwrap();
        let mut wire = Vec::new();
        while client.wants_write() {
            client.write_tls(&mut wire).unwrap();
        }
        assert!(
            wire.len() > 4096,
            "the flight must exceed rustls' 4KB read size"
        );

        read_tls_all(&mut server, &wire).unwrap();
        assert!(!server.is_handshaking());
        let mut got = Vec::new();
        let _ = server.reader().read_to_end(&mut got);
        assert_eq!(got, payload);
    }

    /// 対照: 単発の `read_tls` は 4KB しか取り込まない（本ヘルパーが必要な理由の固定）。
    #[test]
    fn single_read_tls_consumes_at_most_4k() {
        let (mut client, mut server) = pair_until_client_finished();
        client.writer().write_all(&[7u8; 10_000]).unwrap();
        let mut wire = Vec::new();
        while client.wants_write() {
            client.write_tls(&mut wire).unwrap();
        }
        let n = server.read_tls(&mut &wire[..]).unwrap();
        assert!(
            n < wire.len(),
            "rustls read_tls consumed {} of {}",
            n,
            wire.len()
        );
    }
}

#[cfg(all(test, unix))]
mod close_notify_tests {
    use super::read_tls_all;
    use super::read_tls_all_tests::pair_until_client_finished;
    use super::send_close_notify_now;
    use std::io;

    /// メモリ上でハンドシェイクを終わらせる（クライアントの Finished をサーバへ、サーバの残り（TLS 1.3 のチケット）を
    /// クライアントへ渡す）。
    fn established_pair() -> (rustls::ClientConnection, rustls::ServerConnection) {
        let (mut client, mut server) = pair_until_client_finished();
        let mut wire = Vec::new();
        while client.wants_write() {
            client.write_tls(&mut wire).unwrap();
        }
        read_tls_all(&mut server, &wire).unwrap();
        assert!(!server.is_handshaking());
        let mut wire = Vec::new();
        while server.wants_write() {
            server.write_tls(&mut wire).unwrap();
        }
        read_tls_all(&mut client, &wire).unwrap();
        (client, server)
    }

    fn socketpair(nonblocking_rx: bool) -> (i32, i32) {
        let mut fds = [0i32; 2];
        let ret =
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(ret, 0, "socketpair failed: {}", io::Error::last_os_error());
        if nonblocking_rx {
            let flags = unsafe { libc::fcntl(fds[1], libc::F_GETFL, 0) };
            assert_eq!(
                unsafe { libc::fcntl(fds[1], libc::F_SETFL, flags | libc::O_NONBLOCK) },
                0
            );
        }
        (fds[0], fds[1])
    }

    fn read_available(fd: i32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
            if n <= 0 {
                return out;
            }
            out.extend_from_slice(&chunk[..n as usize]);
        }
    }

    /// 閉じる直前に送った close_notify を相手が「正しく閉じた」と読める。2 回呼んでも 2 つ目は送らない（B-401）。
    #[test]
    fn close_notify_reaches_the_peer_once() {
        let (mut client, mut server) = established_pair();
        let (tx, rx) = socketpair(true);
        send_close_notify_now(tx, &mut server);
        let wire = read_available(rx);
        assert!(!wire.is_empty(), "no close_notify was written");
        read_tls_all(&mut client, &wire).unwrap();
        let state = client.process_new_packets().unwrap();
        assert!(state.peer_has_closed(), "the peer did not see close_notify");

        send_close_notify_now(tx, &mut server);
        assert!(read_available(rx).is_empty(), "close_notify was sent twice");
        unsafe {
            libc::close(tx);
            libc::close(rx);
        }
    }

    /// 送信バッファが一杯でも待たない（`Drop` から呼ぶ。ブロッキングのソケットでも返る。B-401）。
    #[test]
    fn close_notify_does_not_wait_for_a_full_socket() {
        let (_client, mut server) = established_pair();
        // tx はブロッキングのまま（io_uring の CONNECT で作ったソケットはノンブロッキングとは限らない）。macOS の送信は
        // MSG_DONTWAIT を見ないので、veil のソケット（reactor が作るので常にノンブロッキング）と同じくノンブロッキングにする
        let (tx, rx) = socketpair(false);
        #[cfg(target_os = "macos")]
        unsafe {
            let flags = libc::fcntl(tx, libc::F_GETFL, 0);
            assert_eq!(libc::fcntl(tx, libc::F_SETFL, flags | libc::O_NONBLOCK), 0);
        }
        let fill = [0u8; 4096];
        loop {
            let n = unsafe {
                libc::send(
                    tx,
                    fill.as_ptr() as *const libc::c_void,
                    fill.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n < 0 {
                assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
                break;
            }
        }
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            send_close_notify_now(tx, &mut server);
            done_tx.send(()).unwrap();
        });
        let result = done_rx.recv_timeout(std::time::Duration::from_secs(5));
        // 待ってしまった場合も試験を終わらせる（相手を閉じると書き込みは EPIPE で戻る）
        unsafe { libc::close(rx) };
        worker.join().unwrap();
        unsafe { libc::close(tx) };
        assert!(
            result.is_ok(),
            "send_close_notify_now blocked on a full socket"
        );
    }

    /// 相手が先に閉じていても返る（`MSG_NOSIGNAL`。誤りは捨てる）。
    #[test]
    fn close_notify_to_a_closed_peer_returns() {
        let (_client, mut server) = established_pair();
        let (tx, rx) = socketpair(false);
        unsafe { libc::close(rx) };
        send_close_notify_now(tx, &mut server);
        unsafe { libc::close(tx) };
    }
}

/// ユーザー空間 TLS の受信スクラッチ（暗号文 1 回分の生 read 先）のサイズ。
const RX_SCRATCH_SIZE: usize = 16 * 1024;

thread_local! {
    /// スレッドごとに 1 本だけ持つ受信スクラッチ。以前は `read()` の呼び出しごとに
    /// `vec![0u8; 16384]`（16KB の確保 + ゼロ埋め）していた（ホットパス規則違反）。
    /// 借用は「生 read → rustls へ投入」の同期区間だけで、`.await` を跨がないため
    /// 同じスレッドの他タスクと競合しない。
    static RX_SCRATCH: std::cell::RefCell<Box<[u8]>> =
        std::cell::RefCell::new(vec![0u8; RX_SCRATCH_SIZE].into_boxed_slice());
}

/// 受信スクラッチを借りて `f` を実行する（同期区間専用。`f` の中で `.await` しないこと）。
#[inline]
pub(crate) fn with_rx_scratch<R>(f: impl FnOnce(&mut [u8]) -> R) -> R {
    RX_SCRATCH.with(|s| f(&mut s.borrow_mut()))
}

/// [`with_rx_scratch`] の中で行う「生 read → rustls 投入」1 回分の結果。
pub(crate) enum RxFed {
    /// 暗号文を読んで rustls へ投入した（平文はループ先頭で取り出す）。
    Fed,
    /// 相手が切断した（EOF）。
    Eof,
    /// 読めるデータが無い（readable を待つ）。
    WouldBlock,
}
