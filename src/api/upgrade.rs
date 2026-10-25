// F-261: ハンドラの HTTP/1.1 プロトコルアップグレード（101 と接続の引き渡し）。
// 設計は docs/backlog/features/F-261-handler-http11-upgrade.md。本体（プロキシ・Web サーバ）の経路は変えない:
// h1.rs の `cfg(feature = "handler")` の分岐が 101 を書いた後の接続をここへ渡す。
//! HTTP/1.1 protocol upgrades from handlers (`101 Switching Protocols`).
//!
//! A handler returns [`Response::switching_protocols`](super::Response::switching_protocols); after the 101 is
//! written, the connection is handed to the given function as an [`Upgraded`]. Plaintext, rustls and kTLS
//! connections all look the same. HTTP/2 and HTTP/3 have no upgrades (the response becomes 400).
//!
//! ```no_run
//! use veil::prelude::*;
//! use veil::api::upgrade::Upgraded;
//!
//! #[veil::route(path = "/echo", method = "GET")]
//! async fn echo(req: Request) -> Response {
//!     if req.upgrade_protocol() != Some(&b"x-echo"[..]) {
//!         return Response::error(426, "Upgrade Required");
//!     }
//!     Response::switching_protocols("x-echo", |mut up: Upgraded| async move {
//!         let mut buf = bytes::BytesMut::with_capacity(4096);
//!         loop {
//!             let (res, b) = up.read(buf).await;
//!             buf = b;
//!             match res {
//!                 Ok(n) if n > 0 => {
//!                     let data = buf.split().freeze();
//!                     if up.write_all(data).await.is_err() {
//!                         break;
//!                     }
//!                 }
//!                 _ => break,
//!             }
//!         }
//!     })
//! }
//! ```

use super::response::Response;
use crate::runtime::io::{AsyncReadRent as _, AsyncWriteRentExt as _};
use bytes::{Bytes, BytesMut};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

#[cfg(veil_ktls)]
pub(crate) use crate::ktls_rustls::KtlsServerStream as ServerTls;
#[cfg(not(veil_ktls))]
pub(crate) use crate::simple_tls::SimpleTlsServerStream as ServerTls;

/// The function that takes over an upgraded connection.
pub(crate) type UpgradeFn = Box<dyn FnOnce(Upgraded) -> Pin<Box<dyn Future<Output = ()>>> + Send>;

/// The upgrade carried by a response (the protocol of the `Upgrade` header and the function).
pub(crate) struct UpgradeHook {
    pub(crate) protocol: Bytes,
    pub(crate) on_upgrade: UpgradeFn,
}

impl UpgradeHook {
    pub(crate) fn new<F, Fut>(protocol: &str, on_upgrade: F) -> UpgradeHook
    where
        F: FnOnce(Upgraded) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        UpgradeHook {
            protocol: Bytes::copy_from_slice(protocol.as_bytes()),
            on_upgrade: Box::new(move |u| Box::pin(on_upgrade(u))),
        }
    }
}

/// `[api] upgrade_idle_timeout_secs`（0 = 無し）。読み取りの待ちに効かせる。
static IDLE_TIMEOUT_SECS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn set_idle_timeout_secs(secs: u64) {
    IDLE_TIMEOUT_SECS.store(secs, Ordering::Relaxed);
}

/// Protocols a handler may not upgrade to (`h2c` collides with veil's own h2c detection).
const RESERVED: &[&[u8]] = &[b"h2c"];

/// Whether `p` can be sent as the `Upgrade` value (`token` or `token/token`, RFC 9110 section 7.8).
fn valid_protocol(p: &[u8]) -> bool {
    !p.is_empty()
        && p.iter()
            .all(|&b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~/".contains(&b))
}

// F-261: ハンドラの戻り値がアップグレードのときだけ呼ばれる（通常の要求の経路は `Option` の確認 1 回）。
/// Checks an upgrade response. `allowed` is set by the HTTP/1.1 path for requests that may be upgraded
/// (`Connection: upgrade` and `Upgrade` present, HTTP/1.1, no body). Otherwise the response becomes 400;
/// a reserved or malformed protocol becomes 500.
pub(crate) fn check_response(resp: &mut Response, allowed: bool) {
    let Some(up) = resp.upgrade.as_ref() else {
        return;
    };
    if RESERVED.iter().any(|r| up.protocol.eq_ignore_ascii_case(r)) || !valid_protocol(&up.protocol)
    {
        // 開発時の誤り（要求ごとに出るが、正しく書かれたハンドラでは起きない）
        ftlog::warn!(
            "[api] a handler returned an upgrade to the reserved or invalid protocol '{}'",
            String::from_utf8_lossy(&up.protocol)
        );
        *resp = Response::error(500, "Internal Server Error");
        return;
    }
    if !allowed {
        *resp = Response::error(400, "Bad Request: not an HTTP/1.1 upgrade request");
    }
}

/// Writes the head of `101 Switching Protocols` (no `Content-Length`, no `Connection: keep-alive`).
pub(crate) fn write_http1_101(out: &mut Vec<u8>, resp: &Response, protocol: &[u8]) {
    out.extend_from_slice(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: ");
    out.extend_from_slice(protocol);
    out.extend_from_slice(b"\r\n");
    for (n, v) in &resp.headers {
        if n.eq_ignore_ascii_case(b"connection")
            || n.eq_ignore_ascii_case(b"upgrade")
            || n.eq_ignore_ascii_case(b"content-length")
            || n.eq_ignore_ascii_case(b"transfer-encoding")
        {
            continue;
        }
        out.extend_from_slice(n);
        out.extend_from_slice(b": ");
        out.extend_from_slice(v);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
}

/// Runs the upgrade function on the connection (called by the HTTP/1.1 path after the 101 is written; the
/// connection stays counted in `max_concurrent_connections` while this runs).
pub(crate) async fn run(
    on_upgrade: UpgradeFn,
    stream: ServerTls,
    leftover: BytesMut,
    client_ip: &str,
    cache: &mut super::dispatch::CallCache,
) {
    let secs = IDLE_TIMEOUT_SECS.load(Ordering::Relaxed);
    let up = Upgraded {
        stream,
        prebuf: leftover.freeze(),
        client_ip: client_ip.to_string(),
        idle_timeout: (secs > 0).then(|| Duration::from_secs(secs)),
    };
    let fut = on_upgrade(up);
    // tokio 互換レイヤー（F-198）: ハンドラと同じく別スレッドからの起床を所有スレッドへ戻す
    #[cfg(feature = "tokio-compat")]
    let fut = crate::tokio::waker::WithSafeWakerIn::new(fut, cache.waker_cache());
    #[cfg(not(feature = "tokio-compat"))]
    let _ = cache;
    if futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(fut))
        .await
        .is_err()
    {
        ftlog::error!("[api] upgrade handler panicked");
    }
}

/// An upgraded HTTP/1.1 connection (plaintext, rustls or kTLS; the same type for all).
///
/// Bytes the client sent right after the request (in the same packet as the headers) are returned first by
/// [`read`](Upgraded::read). The connection closes when this is dropped.
pub struct Upgraded {
    stream: ServerTls,
    /// 要求の解析で読みすぎた分（ヘッダの直後に届いたバイト列）。先に返す。
    prebuf: Bytes,
    client_ip: String,
    idle_timeout: Option<Duration>,
}

impl std::fmt::Debug for Upgraded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upgraded")
            .field("client_ip", &self.client_ip)
            .field("buffered", &self.prebuf.len())
            .finish()
    }
}

/// Reserved spare capacity when `read` is given a buffer without room (bytes).
const READ_RESERVE: usize = 16 * 1024;

impl Upgraded {
    /// Reads into the spare capacity of `buf` (it is extended; `Ok(0)` means the peer closed). Ownership of the
    /// buffer passes through the call, as in veil's runtime. With `[api] upgrade_idle_timeout_secs`, a read that
    /// waits longer fails with `TimedOut`.
    ///
    /// Do not drop this future while it is pending (on io_uring the data of a read in flight would be lost);
    /// to wait for data together with something else, `select` on [`readable`](Upgraded::readable) and then read.
    pub async fn read(&mut self, mut buf: BytesMut) -> (io::Result<usize>, BytesMut) {
        if buf.capacity() == buf.len() {
            buf.reserve(READ_RESERVE);
        }
        if !self.prebuf.is_empty() {
            let n = self.prebuf.len().min(buf.capacity() - buf.len());
            buf.extend_from_slice(&self.prebuf[..n]);
            let _ = self.prebuf.split_to(n);
            return (Ok(n), buf);
        }
        match self.idle_timeout {
            Some(d) => match crate::runtime::time::timeout(d, self.stream.read(buf)).await {
                Ok(r) => r,
                Err(_) => (
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "upgraded connection idle timeout",
                    )),
                    BytesMut::new(),
                ),
            },
            None => self.stream.read(buf).await,
        }
    }

    /// Waits until a read would not block (already-buffered bytes count). Safe to drop while pending.
    pub async fn readable(&self) -> io::Result<()> {
        if !self.prebuf.is_empty() {
            return Ok(());
        }
        self.stream.wait_next_request().await
    }

    /// Whether bytes are already buffered (a [`read`](Upgraded::read) returns without waiting).
    pub fn has_buffered(&self) -> bool {
        !self.prebuf.is_empty()
            || crate::runtime::io::BufferedReadState::has_buffered_read_data(&self.stream)
    }

    /// Writes all of `buf` (not copied).
    pub async fn write_all(&mut self, buf: Bytes) -> io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        self.stream.write_all(buf).await.0.map(drop)
    }

    /// Writes all buffers in order. Two buffers (such as a frame header and its payload) go out in one
    /// vectored write; more are coalesced into one buffer when small.
    pub async fn write_vectored_all(&mut self, bufs: &[Bytes]) -> io::Result<()> {
        match bufs {
            [] => Ok(()),
            [a] => self.write_all(a.clone()).await,
            [a, b] => self
                .stream
                .write_all_vectored(a.clone(), b.clone())
                .await
                .0
                .map(drop),
            _ => {
                let total: usize = bufs.iter().map(|b| b.len()).sum();
                if total <= COALESCE_MAX {
                    let mut out = BytesMut::with_capacity(total);
                    for b in bufs {
                        out.extend_from_slice(b);
                    }
                    self.write_all(out.freeze()).await
                } else {
                    for pair in bufs.chunks(2) {
                        match pair {
                            [a, b] => self
                                .stream
                                .write_all_vectored(a.clone(), b.clone())
                                .await
                                .0
                                .map(drop)?,
                            [a] => self.write_all(a.clone()).await?,
                            _ => {}
                        }
                    }
                    Ok(())
                }
            }
        }
    }

    /// Closes the sending side (the peer reads EOF).
    pub async fn shutdown(&mut self) -> io::Result<()> {
        match self.stream.get_ref().shutdown(std::net::Shutdown::Write) {
            Err(e) if e.kind() == io::ErrorKind::NotConnected => Ok(()),
            r => r,
        }
    }

    /// The client's IP address.
    pub fn client_ip(&self) -> &str {
        &self.client_ip
    }

    /// A future that resolves when graceful shutdown starts. Long-lived connections should `select` on it and
    /// close; connections still open after `[server] graceful_shutdown_timeout_secs` are dropped.
    pub fn shutdown_signal(&self) -> ShutdownSignal {
        ShutdownSignal::new()
    }

    /// Wraps the connection in an object implementing `veil::tokio::io::{AsyncRead, AsyncWrite}`, so that
    /// tokio-based protocol libraries (tokio-tungstenite and so on) can run on it. Reads and writes go through
    /// a task on the same worker thread (one extra copy).
    #[cfg(feature = "tokio-compat")]
    pub fn into_tokio_io(self) -> tokio_io::UpgradedIo {
        tokio_io::UpgradedIo::new(self)
    }
}

/// `write_vectored_all` で 3 個以上のバッファを 1 個にまとめる上限（これより大きいときは 2 個ずつ書く）。
const COALESCE_MAX: usize = 64 * 1024;

/// Resolves when graceful shutdown starts (see [`Upgraded::shutdown_signal`]).
pub struct ShutdownSignal {
    fut: Pin<Box<dyn Future<Output = ()>>>,
}

impl ShutdownSignal {
    fn new() -> ShutdownSignal {
        ShutdownSignal {
            fut: Box::pin(async {
                // F-187 のアイドル待ちと同じタイマー（graceful shutdown の開始でワーカーが即満了させる）。
                // 長い期限の満了は空振りなので待ち直す。
                loop {
                    if is_shutting_down() {
                        return;
                    }
                    let _ = crate::runtime::time::timeout_idle(
                        Duration::from_secs(24 * 3600),
                        true,
                        futures::future::pending::<()>(),
                    )
                    .await;
                }
            }),
        }
    }
}

impl std::fmt::Debug for ShutdownSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ShutdownSignal")
    }
}

impl Future for ShutdownSignal {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.fut.as_mut().poll(cx)
    }
}

/// Whether graceful shutdown has started.
pub fn is_shutting_down() -> bool {
    crate::config::SHUTDOWN_FLAG.load(Ordering::Relaxed)
}

// ===================== ブロッキングなコールバック（C・Python 用） =====================

// F-261: C の `veil_response_upgrade`・Python の `veil.Protocol` が使う。L4 の TCP ハンドラ（F-210）と同じ
// イベント型（受信ごとにハンドラ用プールでコールバックを 1 回。同じ接続のコールバックは並行しない）。
#[cfg(feature = "l4-handler")]
impl Upgraded {
    /// Serves the connection with blocking callbacks: `connect` runs on the handler pool once and returns an
    /// [`L4Session`](super::l4::L4Session) (`None` closes), then `on_data` runs on the pool for each receive.
    /// Data written to the [`L4Out`](super::l4::L4Out) is sent after the callback returns.
    pub async fn serve_blocking<F>(mut self, connect: F)
    where
        F: FnOnce(&mut super::l4::L4Out) -> Option<Box<dyn super::l4::L4Session>> + Send + 'static,
    {
        use super::l4::{L4Out, L4Session};
        use std::sync::{Arc, Mutex};
        let Some((session, out)) = crate::runtime::offload::offload_handler(move || {
            let mut out = L4Out::default();
            (connect(&mut out), out)
        })
        .await
        else {
            return;
        };
        let (writes, close) = out.into_parts();
        if self.write_vectored_all(&writes).await.is_err() {
            return;
        }
        let Some(session) = session else {
            return;
        };
        let session: Arc<Mutex<Box<dyn L4Session>>> = Arc::new(Mutex::new(session));
        if !close {
            let mut buf = BytesMut::new();
            loop {
                let (res, b) = self.read(buf).await;
                buf = b;
                match res {
                    Ok(n) if n > 0 => {}
                    _ => break,
                }
                let data = buf.split().freeze();
                let s = session.clone();
                let Some(out) = crate::runtime::offload::offload_handler(move || {
                    let mut out = L4Out::default();
                    s.lock().unwrap().on_data(&data, &mut out);
                    out
                })
                .await
                else {
                    break;
                };
                let (writes, close) = out.into_parts();
                if self.write_vectored_all(&writes).await.is_err() || close {
                    break;
                }
            }
        }
        let _ = self.shutdown().await;
        let s = session.clone();
        let _ =
            crate::runtime::offload::offload_handler(move || s.lock().unwrap().on_close()).await;
    }
}

// ===================== tokio 互換（tokio-compat） =====================

#[cfg(feature = "tokio-compat")]
/// The tokio-compatible view of an upgraded connection ([`Upgraded::into_tokio_io`]).
pub mod tokio_io {
    use super::Upgraded;
    use bytes::{Buf, BytesMut};
    use std::cell::RefCell;
    use std::io;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::task::{Context, Poll, Waker};

    /// 受信側の上限（これを超えて溜まったら読み取りを止める＝ TCP の背圧）。
    const RX_HIGH: usize = 256 * 1024;
    /// 送信側の上限（これを超えたら `poll_write` を待たせる）。
    const TX_HIGH: usize = 256 * 1024;

    #[derive(Default)]
    struct Shared {
        rx: BytesMut,
        rx_eof: bool,
        rx_err: Option<io::Error>,
        tx: BytesMut,
        tx_busy: bool,
        tx_err: Option<io::Error>,
        shutdown_req: bool,
        shutdown_done: bool,
        dropped: bool,
        reader: Option<Waker>,
        writer: Option<Waker>,
        pump: Option<Waker>,
    }

    fn wake(w: &mut Option<Waker>) {
        if let Some(w) = w.take() {
            w.wake();
        }
    }

    /// Implements `veil::tokio::io::{AsyncRead, AsyncWrite}` over an [`Upgraded`] connection.
    pub struct UpgradedIo {
        shared: Rc<RefCell<Shared>>,
    }

    impl std::fmt::Debug for UpgradedIo {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("UpgradedIo")
        }
    }

    impl UpgradedIo {
        pub(super) fn new(up: Upgraded) -> UpgradedIo {
            let shared = Rc::new(RefCell::new(Shared::default()));
            crate::runtime::spawn(pump(up, shared.clone()));
            UpgradedIo { shared }
        }
    }

    impl Drop for UpgradedIo {
        fn drop(&mut self) {
            let mut s = self.shared.borrow_mut();
            s.dropped = true;
            wake(&mut s.pump);
        }
    }

    enum Next {
        Write(bytes::Bytes),
        Shutdown,
        Read,
        Stop,
    }

    /// 接続を持つタスク: 送信の依頼と可読を待ち、書く・読む（読み取りの Future を途中で捨てない）。
    async fn pump(mut up: Upgraded, shared: Rc<RefCell<Shared>>) {
        use futures::FutureExt;
        let mut buf = BytesMut::new();
        loop {
            let notified = std::future::poll_fn(|cx| {
                let mut s = shared.borrow_mut();
                if s.dropped {
                    return Poll::Ready(Next::Stop);
                }
                if !s.tx.is_empty() {
                    s.tx_busy = true;
                    let out = s.tx.split().freeze();
                    return Poll::Ready(Next::Write(out));
                }
                if s.shutdown_req && !s.shutdown_done {
                    return Poll::Ready(Next::Shutdown);
                }
                s.pump = Some(cx.waker().clone());
                Poll::Pending
            });
            let can_read = {
                let s = shared.borrow();
                !s.rx_eof && s.rx_err.is_none() && s.rx.len() < RX_HIGH
            };
            let next = if can_read {
                futures::select_biased! {
                    n = notified.fuse() => n,
                    r = up.readable().fuse() => match r {
                        Ok(()) => Next::Read,
                        Err(e) => {
                            let mut s = shared.borrow_mut();
                            s.rx_err = Some(e);
                            wake(&mut s.reader);
                            continue;
                        }
                    },
                }
            } else {
                notified.await
            };
            match next {
                Next::Stop => break,
                Next::Write(out) => {
                    let r = up.write_all(out).await;
                    let mut s = shared.borrow_mut();
                    s.tx_busy = false;
                    if let Err(e) = r {
                        s.tx_err = Some(e);
                    }
                    wake(&mut s.writer);
                }
                Next::Shutdown => {
                    let _ = up.shutdown().await;
                    let mut s = shared.borrow_mut();
                    s.shutdown_done = true;
                    wake(&mut s.writer);
                }
                Next::Read => {
                    let (r, b) = up.read(buf).await;
                    buf = b;
                    let mut s = shared.borrow_mut();
                    match r {
                        Ok(0) => s.rx_eof = true,
                        Ok(_) => {
                            let data = buf.split();
                            s.rx.extend_from_slice(&data);
                        }
                        Err(e) => s.rx_err = Some(e),
                    }
                    wake(&mut s.reader);
                }
            }
        }
    }

    impl crate::tokio::io::AsyncRead for UpgradedIo {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut crate::tokio::io::ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let mut s = self.shared.borrow_mut();
            if !s.rx.is_empty() {
                let n = s.rx.len().min(buf.remaining());
                buf.put_slice(&s.rx[..n]);
                s.rx.advance(n);
                // 背圧で止めていた読み取りを再開させる
                wake(&mut s.pump);
                return Poll::Ready(Ok(()));
            }
            if let Some(e) = s.rx_err.take() {
                return Poll::Ready(Err(e));
            }
            if s.rx_eof {
                return Poll::Ready(Ok(()));
            }
            s.reader = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    impl crate::tokio::io::AsyncWrite for UpgradedIo {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let mut s = self.shared.borrow_mut();
            if let Some(e) = s.tx_err.take() {
                return Poll::Ready(Err(e));
            }
            if s.shutdown_req {
                return Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe)));
            }
            if s.tx.len() >= TX_HIGH {
                s.writer = Some(cx.waker().clone());
                return Poll::Pending;
            }
            s.tx.extend_from_slice(buf);
            wake(&mut s.pump);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let mut s = self.shared.borrow_mut();
            if let Some(e) = s.tx_err.take() {
                return Poll::Ready(Err(e));
            }
            if s.tx.is_empty() && !s.tx_busy {
                return Poll::Ready(Ok(()));
            }
            s.writer = Some(cx.waker().clone());
            Poll::Pending
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let mut s = self.shared.borrow_mut();
            if s.shutdown_done {
                return Poll::Ready(Ok(()));
            }
            s.shutdown_req = true;
            s.writer = Some(cx.waker().clone());
            wake(&mut s.pump);
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_of_101_has_no_length_or_keepalive() {
        let resp = Response::switching_protocols("x-echo", |_u| async {})
            .with_header("X-A", "1")
            .with_header("Content-Length", "5")
            .with_header("Connection", "keep-alive");
        let mut out = Vec::new();
        write_http1_101(&mut out, &resp, b"x-echo");
        let h = String::from_utf8(out).unwrap();
        assert!(
            h.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
            "{}",
            h
        );
        assert!(
            h.contains("Connection: Upgrade\r\nUpgrade: x-echo\r\n"),
            "{}",
            h
        );
        assert!(h.contains("X-A: 1\r\n"), "{}", h);
        assert!(!h.to_ascii_lowercase().contains("content-length"), "{}", h);
        assert!(!h.contains("keep-alive"), "{}", h);
        assert!(h.ends_with("\r\n\r\n"));
    }

    #[test]
    fn upgrade_rejected_when_not_allowed() {
        let mut r = Response::switching_protocols("x-echo", |_u| async {});
        check_response(&mut r, false);
        assert_eq!(r.status_code(), 400);
        assert!(!r.is_upgrade());
        let mut r = Response::switching_protocols("x-echo", |_u| async {});
        check_response(&mut r, true);
        assert_eq!(r.status_code(), 101);
        assert!(r.is_upgrade());
    }

    #[test]
    fn reserved_and_invalid_protocols_are_500() {
        for p in ["h2c", "H2C", "bad\r\nx: y", ""] {
            let mut r = Response::switching_protocols(p, |_u| async {});
            check_response(&mut r, true);
            assert_eq!(r.status_code(), 500, "{:?}", p);
        }
        let mut r = Response::switching_protocols("websocket", |_u| async {});
        check_response(&mut r, true);
        assert_eq!(r.status_code(), 101);
    }

    #[test]
    fn request_upgrade_protocol() {
        let h = |pairs: &[(&'static str, &'static str)]| {
            pairs
                .iter()
                .map(|(n, v)| {
                    (
                        Bytes::from_static(n.as_bytes()),
                        Bytes::from_static(v.as_bytes()),
                    )
                })
                .collect::<Vec<_>>()
        };
        let req = super::super::Request::new(
            "GET",
            "/",
            h(&[("Connection", "keep-alive, Upgrade"), ("Upgrade", "x-echo")]),
            "",
        );
        assert_eq!(req.upgrade_protocol(), Some(&b"x-echo"[..]));
        let req = super::super::Request::new("GET", "/", h(&[("Upgrade", "x-echo")]), "");
        assert_eq!(req.upgrade_protocol(), None);
        let req = super::super::Request::new("GET", "/", h(&[("Connection", "upgrade")]), "");
        assert_eq!(req.upgrade_protocol(), None);
        let req = super::super::dispatch::build_request(
            Bytes::from_static(b"GET"),
            Bytes::from_static(b"/"),
            h(&[("connection", "upgrade"), ("upgrade", "x-echo")]),
            Bytes::new(),
            "127.0.0.1",
            super::super::HttpVersion::Http2,
        );
        assert_eq!(req.upgrade_protocol(), None);
    }

    #[test]
    fn upgrade_on_http2_is_400() {
        let entry = crate::api::handler::HandlerEntry::new(
            "u",
            "/",
            &["GET"],
            std::sync::Arc::new(
                |_req: super::super::Request| -> crate::api::handler::HandlerFuture {
                    Box::pin(async { Response::switching_protocols("x-echo", |_u| async {}) })
                },
            ),
            Default::default(),
        )
        .unwrap();
        let req = super::super::dispatch::build_request(
            Bytes::from_static(b"GET"),
            Bytes::from_static(b"/"),
            Vec::new(),
            Bytes::new(),
            "127.0.0.1",
            super::super::HttpVersion::Http2,
        );
        let resp = futures::executor::block_on(super::super::dispatch::call(&entry, req, None));
        assert_eq!(resp.status_code(), 400);
    }
}
