//! `tokio::net` 互換（F-198。Windows は F-204）。
//!
//! ノンブロッキングのソケットへ直接 `recv`/`send` し、`EAGAIN` のときだけ veil のランタイムの準備待ち
//! （`runtime::tcp::wait_readable_fd` / `wait_writable_fd`。io_uring は POLL_ADD、reactor は poller 登録）で待つ。
//! poll ベースの `AsyncRead`/`AsyncWrite` を veil の完了型 I/O の上に作るための橋で、データプレーンは通らない。
//!
//! 準備待ちはそれを始めたスレッドのランタイムに属する。ソケットを別のスレッドで poll し直したら、前の待ちは
//! 捨てずに手放す（`mem::forget`。他スレッドのランタイムの表に触れないため）。タスクはスレッド間を移動しないので
//! 通常は起きない。

use super::io::{AsyncRead, AsyncWrite, Interest, ReadBuf, Ready};
use std::future::{poll_fn, Future};
use std::io;
use std::net::SocketAddr;
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{
    AsRawSocket, AsSocket, BorrowedSocket, FromRawSocket, IntoRawSocket, RawSocket,
};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{ready, Context, Poll};
use std::thread::ThreadId;

#[cfg(unix)]
#[path = "sys_unix.rs"]
pub(crate) mod sys;
#[cfg(windows)]
#[path = "sys_windows.rs"]
pub(crate) mod sys;

// Unix ドメインソケット・`AsyncFd`（unix だけ。tokio も Windows では提供しない）
#[cfg(unix)]
mod unix_impl;
#[cfg(unix)]
pub use unix_impl::*;

use sys::{sock_err, with_std};

/// fd（unix）・SOCKET（Windows）の trait を tokio と同じく付ける。
macro_rules! socket_traits {
    ($t:ty, |$s:ident| $owned:expr) => {
        #[cfg(unix)]
        impl AsRawFd for $t {
            fn as_raw_fd(&self) -> RawFd {
                let $s = self;
                $owned.as_raw_fd()
            }
        }
        #[cfg(unix)]
        impl AsFd for $t {
            fn as_fd(&self) -> BorrowedFd<'_> {
                let $s = self;
                $owned.as_fd()
            }
        }
        #[cfg(windows)]
        impl AsRawSocket for $t {
            fn as_raw_socket(&self) -> RawSocket {
                let $s = self;
                $owned.as_raw_socket()
            }
        }
        #[cfg(windows)]
        impl AsSocket for $t {
            fn as_socket(&self) -> BorrowedSocket<'_> {
                let $s = self;
                $owned.as_socket()
            }
        }
    };
}

/// ソケットオプション（`sys::getopt` / `sys::setopt` が OS ごとの番号にする）。
#[derive(Clone, Copy)]
pub(crate) enum Opt {
    ReuseAddr,
    #[cfg(unix)]
    ReusePort,
    KeepAlive,
    NoDelay,
    SndBuf,
    RcvBuf,
    Tos,
    TClass,
    #[cfg(any(target_os = "linux", target_os = "android"))]
    QuickAck,
}

// ===================== 準備待ち =====================

enum Wait {
    R(crate::runtime::tcp::ReadableFd),
    W(crate::runtime::tcp::WritableFd),
}

impl Wait {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self {
            Wait::R(f) => Pin::new(f).poll(cx),
            Wait::W(f) => Pin::new(f).poll(cx),
        }
    }
}

#[derive(Default)]
struct WaitSlot {
    pending: Option<(ThreadId, Wait)>,
    /// 同じ向きを待っている全員（準備できたら全員を起こす。tokio と同じ）
    waiters: Arc<WaitAll>,
}

/// 登録された Waker を全部起こす Waker。
#[derive(Default)]
struct WaitAll {
    list: Mutex<Vec<std::task::Waker>>,
    /// `poll_*_ready` の待ち手（tokio と同じく最後に poll した 1 つだけを覚える）
    last: Mutex<Option<std::task::Waker>>,
}

impl WaitAll {
    /// `AsyncFd` の `poll_*_ready`（unix だけ）が使う。
    #[cfg(unix)]
    fn set_last(&self, w: &std::task::Waker) {
        let old = self
            .last
            .lock()
            .unwrap()
            .replace(crate::tokio::waker::safe_waker(w));
        drop(old);
    }

    fn add(&self, w: &std::task::Waker) {
        let mut l = self.list.lock().unwrap();
        if !l.iter().any(|x| x.will_wake(w)) {
            l.push(crate::tokio::waker::safe_waker(w));
        }
    }
}

impl std::task::Wake for WaitAll {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        let ws: Vec<_> = std::mem::take(&mut *self.list.lock().unwrap());
        let last = self.last.lock().unwrap().take();
        for w in ws.into_iter().chain(last) {
            w.wake();
        }
    }
}

impl WaitSlot {
    fn clear(&mut self) {
        if let Some((owner, w)) = self.pending.take() {
            // 作ったスレッドでランタイムが生きているときだけ drop する（他は forget。reactor では drop 処理が無い）
            let w = std::mem::ManuallyDrop::new(w);
            if owner == std::thread::current().id()
                && !crate::tokio::waker::exiting()
                && crate::runtime::has_driver()
            {
                let _ = std::mem::ManuallyDrop::into_inner(w);
            }
        }
    }
}

// SAFETY: 待ちの Future は fd と整数だけを持つ。触るのは作ったスレッドだけ（他スレッドでは forget する）
unsafe impl Send for WaitSlot {}

/// fd と、読み・書きの準備待ち。
pub(crate) struct IoSource {
    fd: sys::Owned,
    read: Mutex<WaitSlot>,
    write: Mutex<WaitSlot>,
    /// 準備できたことを覚える（tokio と同じく、操作が `WouldBlock` を返すまで準備できたままとみなす）
    read_ready: std::sync::atomic::AtomicBool,
    write_ready: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for IoSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IoSource").field("fd", &self.raw()).finish()
    }
}

/// ソケットを作る・std から取り込むときの検査（tokio と同じく、ランタイムの外・I/O 無効なら panic）。
#[track_caller]
fn check_io_context() {
    if !crate::runtime::has_driver() && crate::tokio::runtime::Handle::try_current().is_err() {
        panic!(
            "there is no reactor running, must be called from the context of a Tokio 1.x runtime"
        );
    }
    if !crate::tokio::runtime::io_enabled_in_context() {
        panic!("A Tokio 1.x context was found, but IO is disabled. Call `enable_io` on the runtime builder to enable IO.");
    }
}

impl IoSource {
    fn new(fd: sys::Owned) -> IoSource {
        IoSource {
            fd,
            read: Mutex::new(WaitSlot::default()),
            write: Mutex::new(WaitSlot::default()),
            read_ready: std::sync::atomic::AtomicBool::new(false),
            write_ready: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn raw(&self) -> sys::Raw {
        sys::raw(&self.fd)
    }

    fn ready_flag(&self, write: bool) -> &std::sync::atomic::AtomicBool {
        if write {
            &self.write_ready
        } else {
            &self.read_ready
        }
    }

    /// 相手が閉じた・エラーの状態を `ready` に足す（tokio の `READ_CLOSED`・`WRITE_CLOSED`。poll(2) で今の状態を見る）。
    fn with_closed(&self, mut ready: Ready) -> Ready {
        let (read_closed, hup, err) = sys::closed_state(self.raw());
        if read_closed && ready.is_readable() {
            ready |= Ready::READ_CLOSED;
        }
        if hup && ready.is_writable() {
            ready |= Ready::WRITE_CLOSED;
        }
        if err {
            ready |= Ready::ERROR;
        }
        ready
    }

    /// 操作の結果を見て、`WouldBlock` なら準備の記憶を消す。
    fn track<R>(&self, write: bool, r: io::Result<R>) -> io::Result<R> {
        if matches!(&r, Err(e) if e.kind() == io::ErrorKind::WouldBlock) {
            self.ready_flag(write)
                .store(false, std::sync::atomic::Ordering::Release);
        }
        r
    }

    /// `coop` は協調スケジューリングの予算を使うか（tokio と同じく `poll_*_ready` と読み書きは使い、
    /// `readable()`・`writable()`・`ready()` は使わない）。
    fn poll_ready(&self, write: bool, coop: bool, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // tokio と同じく、ランタイムが止まった後の準備待ちはエラー
        if !crate::runtime::has_driver() {
            return Poll::Ready(Err(io::Error::other(
                "A Tokio 1.x context was found, but it is being shutdown.",
            )));
        }
        if coop {
            ready!(crate::tokio::task::coop::poll_proceed_and_make_progress(cx));
        }
        if self
            .ready_flag(write)
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Poll::Ready(Ok(()));
        }
        let me = std::thread::current().id();
        let mut slot = if write {
            self.write.lock().unwrap()
        } else {
            self.read.lock().unwrap()
        };
        if slot.pending.as_ref().is_some_and(|(o, _)| *o != me) {
            slot.clear();
        }
        let fd = self.raw();
        slot.waiters.add(cx.waker());
        let all = std::task::Waker::from(slot.waiters.clone());
        let (_, w) = slot.pending.get_or_insert_with(|| {
            (
                me,
                if write {
                    Wait::W(crate::runtime::tcp::wait_writable_fd(fd))
                } else {
                    Wait::R(crate::runtime::tcp::wait_readable_fd(fd))
                },
            )
        });
        let r = w.poll(&mut Context::from_waker(&all));
        crate::tokio::waker::touch_exit_sentinel();
        let r = ready!(r);
        slot.pending = None;
        if r.is_ok() {
            self.ready_flag(write)
                .store(true, std::sync::atomic::Ordering::Release);
        }
        Poll::Ready(r)
    }

    pub(crate) fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_ready(false, true, cx)
    }

    pub(crate) fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_ready(true, true, cx)
    }

    fn poll_read_ready_nocoop(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_ready(false, false, cx)
    }

    fn poll_write_ready_nocoop(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_ready(true, false, cx)
    }

    /// 準備ができるまで `f` を繰り返す（`WouldBlock` なら準備待ち）。
    fn poll_io<R>(
        &self,
        write: bool,
        cx: &mut Context<'_>,
        mut f: impl FnMut() -> io::Result<R>,
    ) -> Poll<io::Result<R>> {
        loop {
            match f() {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.ready_flag(write)
                        .store(false, std::sync::atomic::Ordering::Release);
                    ready!(self.poll_ready(write, true, cx))?;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                r => return Poll::Ready(r),
            }
        }
    }
}

impl IoSource {
    /// fd を手放す前の後始末: 準備待ちを取り消し（io_uring の POLL_ADD が fd を握ったままにならないように）、
    /// reactor の fd の記録を消す（同じ番号の fd が次に使われたとき、登録が ADD からやり直しになるように）。
    fn release(&mut self) {
        self.read.get_mut().unwrap().clear();
        self.write.get_mut().unwrap().clear();
        #[cfg(veil_rt_reactor)]
        if crate::runtime::has_driver() && !crate::tokio::waker::exiting() {
            crate::runtime::executor::unregister(self.raw());
        }
    }
}

impl Drop for IoSource {
    fn drop(&mut self) {
        self.release();
    }
}

// ===== OS 依存の呼び出しは sys（sys_unix.rs / sys_windows.rs）=====

// ===================== アドレス解決 =====================

mod sealed {
    use std::future::Future;
    use std::io;
    use std::net::SocketAddr;

    pub trait ToSocketAddrsPriv {
        fn to_addrs(&self) -> impl Future<Output = io::Result<Vec<SocketAddr>>> + Send;
    }
}

/// `tokio::net::ToSocketAddrs`。
pub trait ToSocketAddrs: sealed::ToSocketAddrsPriv {}

async fn resolve(host: String) -> io::Result<Vec<SocketAddr>> {
    // 理由付き allow: 名前解決はハンドラ用プール（offload）か、ランタイムの外のスレッドでだけ行う
    #[allow(clippy::disallowed_methods)]
    fn lookup(h: &str) -> io::Result<Vec<SocketAddr>> {
        std::net::ToSocketAddrs::to_socket_addrs(h).map(|i| i.collect())
    }
    if let Ok(a) = host.parse::<SocketAddr>() {
        return Ok(vec![a]);
    }
    if crate::runtime::has_driver() {
        crate::runtime::offload::offload_handler(move || lookup(&host))
            .await
            .unwrap_or_else(|| Err(io::Error::other("name resolution panicked")))
    } else {
        lookup(&host)
    }
}

macro_rules! direct_addrs {
    ($($t:ty => |$s:ident| $e:expr;)*) => {
        $(
            impl sealed::ToSocketAddrsPriv for $t {
                fn to_addrs(&self) -> impl Future<Output = io::Result<Vec<SocketAddr>>> + Send {
                    let $s = self;
                    let v: Vec<SocketAddr> = $e;
                    async move { Ok(v) }
                }
            }
            impl ToSocketAddrs for $t {}
        )*
    };
}

direct_addrs! {
    SocketAddr => |s| vec![*s];
    std::net::SocketAddrV4 => |s| vec![SocketAddr::V4(*s)];
    std::net::SocketAddrV6 => |s| vec![SocketAddr::V6(*s)];
    (std::net::IpAddr, u16) => |s| vec![SocketAddr::new(s.0, s.1)];
    (std::net::Ipv4Addr, u16) => |s| vec![SocketAddr::new(s.0.into(), s.1)];
    (std::net::Ipv6Addr, u16) => |s| vec![SocketAddr::new(s.0.into(), s.1)];
    &[SocketAddr] => |s| s.to_vec();
}

impl sealed::ToSocketAddrsPriv for str {
    fn to_addrs(&self) -> impl Future<Output = io::Result<Vec<SocketAddr>>> + Send {
        resolve(self.to_string())
    }
}
impl ToSocketAddrs for str {}

impl sealed::ToSocketAddrsPriv for String {
    fn to_addrs(&self) -> impl Future<Output = io::Result<Vec<SocketAddr>>> + Send {
        resolve(self.clone())
    }
}
impl ToSocketAddrs for String {}

impl sealed::ToSocketAddrsPriv for (&str, u16) {
    fn to_addrs(&self) -> impl Future<Output = io::Result<Vec<SocketAddr>>> + Send {
        let (h, p) = *self;
        let s = if h.contains(':') && !h.starts_with('[') {
            format!("[{}]:{}", h, p)
        } else {
            format!("{}:{}", h, p)
        };
        resolve(s)
    }
}
impl ToSocketAddrs for (&str, u16) {}

impl sealed::ToSocketAddrsPriv for (String, u16) {
    fn to_addrs(&self) -> impl Future<Output = io::Result<Vec<SocketAddr>>> + Send {
        let s = if self.0.contains(':') && !self.0.starts_with('[') {
            format!("[{}]:{}", self.0, self.1)
        } else {
            format!("{}:{}", self.0, self.1)
        };
        resolve(s)
    }
}
impl ToSocketAddrs for (String, u16) {}

impl<T: ToSocketAddrs + ?Sized + Sync> sealed::ToSocketAddrsPriv for &T {
    fn to_addrs(&self) -> impl Future<Output = io::Result<Vec<SocketAddr>>> + Send {
        (**self).to_addrs()
    }
}
impl<T: ToSocketAddrs + ?Sized + Sync> ToSocketAddrs for &T {}

/// 名前を解決する（`tokio::net::lookup_host`）。
pub async fn lookup_host<T: ToSocketAddrs>(
    host: T,
) -> io::Result<impl Iterator<Item = SocketAddr>> {
    Ok(host.to_addrs().await?.into_iter())
}

// ===================== TcpStream =====================

/// `tokio::net::TcpStream`。
#[derive(Debug)]
pub struct TcpStream {
    io: Arc<IoSource>,
}

impl TcpStream {
    /// 順に接続を試す（最初に成功したもの）。
    pub async fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<TcpStream> {
        let addrs = addr.to_addrs().await?;
        let mut last = None;
        for a in addrs {
            match TcpStream::connect_addr(a).await {
                Ok(s) => return Ok(s),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "could not resolve to any address",
            )
        }))
    }

    async fn connect_addr(addr: SocketAddr) -> io::Result<TcpStream> {
        let fd = sys::new_stream_socket(addr.is_ipv6())?;
        TcpStream::connect_fd(fd, addr).await
    }

    async fn connect_fd(fd: sys::Owned, addr: SocketAddr) -> io::Result<TcpStream> {
        let r = sys::connect(sys::raw(&fd), &addr);
        let s = TcpStream {
            io: Arc::new(IoSource::new(fd)),
        };
        r?;
        // tokio と同じく、すぐ繋がっても一度ランタイムの準備待ちを通して SO_ERROR を見る
        poll_fn(|cx| s.io.poll_write_ready(cx)).await?;
        if let Some(e) = sock_err(s.io.raw())? {
            return Err(e);
        }
        Ok(s)
    }

    /// std の接続から作る（ノンブロッキングにする）。
    #[track_caller]
    pub fn from_std(stream: std::net::TcpStream) -> io::Result<TcpStream> {
        check_io_context();
        stream.set_nonblocking(true)?;
        let fd: sys::Owned = stream.into();
        sys::prepare(sys::raw(&fd))?;
        Ok(TcpStream {
            io: Arc::new(IoSource::new(fd)),
        })
    }

    /// std の接続に戻す（ブロッキングに戻す）。
    pub fn into_std(self) -> io::Result<std::net::TcpStream> {
        let io = Arc::try_unwrap(self.io)
            .map_err(|_| io::Error::other("TcpStream is still shared (split halves alive)"))?;
        let mut io = std::mem::ManuallyDrop::new(io);
        io.release();
        // SAFETY: ManuallyDrop の中の fd を取り出す（IoSource の Drop は走らせない）
        let fd = unsafe { std::ptr::read(&io.fd) };
        let s = std::net::TcpStream::from(fd);
        s.set_nonblocking(false)?;
        Ok(s)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        with_std::<std::net::TcpStream, _>(self.io.raw(), |s| s.local_addr())
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        with_std::<std::net::TcpStream, _>(self.io.raw(), |s| s.peer_addr())
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        sock_err(self.io.raw())
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        with_std::<std::net::TcpStream, _>(self.io.raw(), |s| s.nodelay())
    }

    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        with_std::<std::net::TcpStream, _>(self.io.raw(), |s| s.set_nodelay(nodelay))
    }

    pub fn ttl(&self) -> io::Result<u32> {
        with_std::<std::net::TcpStream, _>(self.io.raw(), |s| s.ttl())
    }

    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        with_std::<std::net::TcpStream, _>(self.io.raw(), |s| s.set_ttl(ttl))
    }

    pub fn set_zero_linger(&self) -> io::Result<()> {
        sys::set_linger(self.io.raw(), Some(std::time::Duration::ZERO))
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn quickack(&self) -> io::Result<bool> {
        sys::getopt(self.io.raw(), Opt::QuickAck).map(|v| v != 0)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn set_quickack(&self, quickack: bool) -> io::Result<()> {
        sys::setopt(self.io.raw(), Opt::QuickAck, quickack as i32)
    }

    pub fn linger(&self) -> io::Result<Option<std::time::Duration>> {
        sys::linger(self.io.raw())
    }

    pub fn set_linger(&self, dur: Option<std::time::Duration>) -> io::Result<()> {
        sys::set_linger(self.io.raw(), dur)
    }

    pub async fn ready(&self, interest: Interest) -> io::Result<Ready> {
        poll_fn(|cx| {
            let mut ready = Ready::EMPTY;
            if interest.is_readable() {
                if let Poll::Ready(r) = self.io.poll_read_ready_nocoop(cx) {
                    r?;
                    ready |= Ready::READABLE;
                }
            }
            if interest.is_writable() {
                if let Poll::Ready(r) = self.io.poll_write_ready_nocoop(cx) {
                    r?;
                    ready |= Ready::WRITABLE;
                }
            }
            if ready.is_empty() {
                Poll::Pending
            } else {
                Poll::Ready(Ok(self.io.with_closed(ready)))
            }
        })
        .await
    }

    pub async fn readable(&self) -> io::Result<()> {
        poll_fn(|cx| self.io.poll_read_ready_nocoop(cx)).await
    }

    pub async fn writable(&self) -> io::Result<()> {
        poll_fn(|cx| self.io.poll_write_ready_nocoop(cx)).await
    }

    pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.io.poll_read_ready(cx)
    }

    pub fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.io.poll_write_ready(cx)
    }

    pub fn try_read(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.io.track(
            false,
            sys::recv_into(self.io.raw(), buf.as_mut_ptr(), buf.len()),
        )
    }

    pub fn try_read_buf<B: bytes::BufMut>(&self, buf: &mut B) -> io::Result<usize> {
        let dst = buf.chunk_mut();
        let n = self.io.track(
            false,
            sys::recv_into(self.io.raw(), dst.as_mut_ptr(), dst.len()),
        )?;
        // SAFETY: n バイトは recv で初期化された
        unsafe { buf.advance_mut(n) };
        Ok(n)
    }

    pub fn try_read_vectored(&self, bufs: &mut [io::IoSliceMut<'_>]) -> io::Result<usize> {
        sys::readv(self.io.raw(), bufs)
    }

    pub fn try_write(&self, buf: &[u8]) -> io::Result<usize> {
        self.io.track(true, sys::send(self.io.raw(), buf))
    }

    pub fn try_write_vectored(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        self.io.track(true, sys::writev(self.io.raw(), bufs))
    }

    pub async fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        poll_fn(|cx| {
            let mut rb = ReadBuf::new(&mut *buf);
            ready!(self.poll_peek(cx, &mut rb))?;
            Poll::Ready(Ok(rb.filled().len()))
        })
        .await
    }

    pub fn poll_peek(
        &self,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<usize>> {
        let fd = self.io.raw();
        self.io.poll_io(false, cx, || sys::recv_buf(fd, buf, true))
    }

    /// 借用の読み・書きの半分。
    pub fn split(&mut self) -> (tcp::ReadHalf<'_>, tcp::WriteHalf<'_>) {
        (tcp::ReadHalf(self), tcp::WriteHalf(self))
    }

    /// 所有の読み・書きの半分（`reunite` で戻す）。
    pub fn into_split(self) -> (tcp::OwnedReadHalf, tcp::OwnedWriteHalf) {
        (
            tcp::OwnedReadHalf {
                inner: TcpStream {
                    io: self.io.clone(),
                },
            },
            tcp::OwnedWriteHalf {
                inner: self,
                shutdown_on_drop: true,
            },
        )
    }

    fn poll_read_priv(&self, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let fd = self.io.raw();
        self.io
            .poll_io(false, cx, || sys::recv_buf(fd, buf, false))
            .map_ok(drop)
    }

    fn poll_write_priv(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let fd = self.io.raw();
        self.io.poll_io(true, cx, || sys::send(fd, buf))
    }

    fn poll_write_vectored_priv(
        &self,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let fd = self.io.raw();
        self.io.poll_io(true, cx, || sys::writev(fd, bufs))
    }

    fn shutdown_std(&self, how: std::net::Shutdown) -> io::Result<()> {
        let r = with_std::<std::net::TcpStream, _>(self.io.raw(), |s| s.shutdown(how));
        if r.is_ok() {
            sys::after_shutdown(self.io.raw(), how);
        }
        match r {
            Err(e) if e.kind() == io::ErrorKind::NotConnected => Ok(()),
            r => r,
        }
    }
}

impl AsyncRead for TcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.poll_read_priv(cx, buf)
    }
}

impl AsyncWrite for TcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_write_priv(cx, buf)
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.poll_write_vectored_priv(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        true
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(self.shutdown_std(std::net::Shutdown::Write))
    }
}

socket_traits!(TcpStream, |s| s.io.fd);

impl TryFrom<std::net::TcpStream> for TcpStream {
    type Error = io::Error;
    fn try_from(s: std::net::TcpStream) -> io::Result<TcpStream> {
        TcpStream::from_std(s)
    }
}

/// `tokio::net::tcp`（分割した半分）。
pub mod tcp {
    use super::*;

    /// 借用の読みの半分。
    #[derive(Debug)]
    pub struct ReadHalf<'a>(pub(super) &'a TcpStream);

    /// 借用の書きの半分。
    #[derive(Debug)]
    pub struct WriteHalf<'a>(pub(super) &'a TcpStream);

    impl AsyncRead for ReadHalf<'_> {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            self.0.poll_read_priv(cx, buf)
        }
    }

    impl AsyncWrite for WriteHalf<'_> {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.0.poll_write_priv(cx, buf)
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(self.0.shutdown_std(std::net::Shutdown::Write))
        }
    }

    impl ReadHalf<'_> {
        pub async fn peek(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.peek(buf).await
        }
        pub fn peer_addr(&self) -> io::Result<SocketAddr> {
            self.0.peer_addr()
        }
        pub fn local_addr(&self) -> io::Result<SocketAddr> {
            self.0.local_addr()
        }
    }

    impl WriteHalf<'_> {
        pub fn peer_addr(&self) -> io::Result<SocketAddr> {
            self.0.peer_addr()
        }
        pub fn local_addr(&self) -> io::Result<SocketAddr> {
            self.0.local_addr()
        }
    }

    /// 所有の読みの半分。
    #[derive(Debug)]
    pub struct OwnedReadHalf {
        pub(super) inner: TcpStream,
    }

    /// 所有の書きの半分（drop で書き方向を閉じる）。
    #[derive(Debug)]
    pub struct OwnedWriteHalf {
        pub(super) inner: TcpStream,
        pub(super) shutdown_on_drop: bool,
    }

    /// 対でない半分を `reunite` しようとした。
    #[derive(Debug)]
    pub struct ReuniteError(pub OwnedReadHalf, pub OwnedWriteHalf);

    impl std::fmt::Display for ReuniteError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("tried to reunite halves that are not from the same socket")
        }
    }

    impl std::error::Error for ReuniteError {}

    impl OwnedReadHalf {
        pub fn reunite(self, mut other: OwnedWriteHalf) -> Result<TcpStream, ReuniteError> {
            if Arc::ptr_eq(&self.inner.io, &other.inner.io) {
                other.shutdown_on_drop = false;
                drop(self);
                let io = other.inner.io.clone();
                drop(other);
                Ok(TcpStream { io })
            } else {
                Err(ReuniteError(self, other))
            }
        }
        pub async fn peek(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.inner.peek(buf).await
        }
        pub fn peer_addr(&self) -> io::Result<SocketAddr> {
            self.inner.peer_addr()
        }
        pub fn local_addr(&self) -> io::Result<SocketAddr> {
            self.inner.local_addr()
        }
        pub async fn readable(&self) -> io::Result<()> {
            self.inner.readable().await
        }
        pub fn try_read(&self, buf: &mut [u8]) -> io::Result<usize> {
            self.inner.try_read(buf)
        }
    }

    impl OwnedWriteHalf {
        pub fn reunite(self, other: OwnedReadHalf) -> Result<TcpStream, ReuniteError> {
            other.reunite(self)
        }
        pub fn forget(mut self) {
            self.shutdown_on_drop = false;
        }
        pub fn peer_addr(&self) -> io::Result<SocketAddr> {
            self.inner.peer_addr()
        }
        pub fn local_addr(&self) -> io::Result<SocketAddr> {
            self.inner.local_addr()
        }
        pub async fn writable(&self) -> io::Result<()> {
            self.inner.writable().await
        }
        pub fn try_write(&self, buf: &[u8]) -> io::Result<usize> {
            self.inner.try_write(buf)
        }
    }

    impl Drop for OwnedWriteHalf {
        fn drop(&mut self) {
            if self.shutdown_on_drop {
                let _ = self.inner.shutdown_std(std::net::Shutdown::Write);
            }
        }
    }

    impl AsyncRead for OwnedReadHalf {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            self.inner.poll_read_priv(cx, buf)
        }
    }

    impl AsyncWrite for OwnedWriteHalf {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.inner.poll_write_priv(cx, buf)
        }
        fn poll_write_vectored(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bufs: &[io::IoSlice<'_>],
        ) -> Poll<io::Result<usize>> {
            self.inner.poll_write_vectored_priv(cx, bufs)
        }
        fn is_write_vectored(&self) -> bool {
            true
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            let r = self.inner.shutdown_std(std::net::Shutdown::Write);
            if r.is_ok() {
                self.get_mut().shutdown_on_drop = false;
            }
            Poll::Ready(r)
        }
    }
}

// ===================== TcpListener =====================

/// `tokio::net::TcpListener`。
#[derive(Debug)]
pub struct TcpListener {
    io: IoSource,
}

impl TcpListener {
    /// 待ち受ける（最初に bind できたアドレス）。
    pub async fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<TcpListener> {
        let addrs = addr.to_addrs().await?;
        let mut last = None;
        for a in addrs {
            match std::net::TcpListener::bind(a) {
                Ok(l) => return TcpListener::from_std(l),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "could not resolve to any address",
            )
        }))
    }

    #[track_caller]
    pub fn from_std(listener: std::net::TcpListener) -> io::Result<TcpListener> {
        check_io_context();
        listener.set_nonblocking(true)?;
        let fd: sys::Owned = listener.into();
        sys::prepare(sys::raw(&fd))?;
        Ok(TcpListener {
            io: IoSource::new(fd),
        })
    }

    pub fn into_std(self) -> io::Result<std::net::TcpListener> {
        let mut io = std::mem::ManuallyDrop::new(self.io);
        io.release();
        // SAFETY: Drop を走らせずに fd を取り出す
        let fd = unsafe { std::ptr::read(&io.fd) };
        let l = std::net::TcpListener::from(fd);
        l.set_nonblocking(false)?;
        Ok(l)
    }

    pub async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        poll_fn(|cx| self.poll_accept(cx)).await
    }

    pub fn poll_accept(&self, cx: &mut Context<'_>) -> Poll<io::Result<(TcpStream, SocketAddr)>> {
        let fd = self.io.raw();
        let (s, a) = ready!(self.io.poll_io(false, cx, || sys::accept(fd)))?;
        Poll::Ready(Ok((
            TcpStream {
                io: Arc::new(IoSource::new(s)),
            },
            a,
        )))
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        with_std::<std::net::TcpListener, _>(self.io.raw(), |l| l.local_addr())
    }

    pub fn ttl(&self) -> io::Result<u32> {
        with_std::<std::net::TcpListener, _>(self.io.raw(), |l| l.ttl())
    }

    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        with_std::<std::net::TcpListener, _>(self.io.raw(), |l| l.set_ttl(ttl))
    }
}

socket_traits!(TcpListener, |s| s.io.fd);

impl TryFrom<std::net::TcpListener> for TcpListener {
    type Error = io::Error;
    fn try_from(l: std::net::TcpListener) -> io::Result<TcpListener> {
        TcpListener::from_std(l)
    }
}

// ===================== TcpSocket =====================

/// `tokio::net::TcpSocket`（bind・オプションを設定してから connect / listen する）。
#[derive(Debug)]
pub struct TcpSocket {
    fd: sys::Owned,
}

impl TcpSocket {
    fn raw(&self) -> sys::Raw {
        sys::raw(&self.fd)
    }

    fn set_flag(&self, opt: Opt, v: bool) -> io::Result<()> {
        sys::setopt(self.raw(), opt, v as i32)
    }

    fn flag(&self, opt: Opt) -> io::Result<bool> {
        sys::getopt(self.raw(), opt).map(|v| v != 0)
    }

    pub fn linger(&self) -> io::Result<Option<std::time::Duration>> {
        sys::linger(self.raw())
    }

    pub fn set_zero_linger(&self) -> io::Result<()> {
        sys::set_linger(self.raw(), Some(std::time::Duration::ZERO))
    }

    pub fn tos_v4(&self) -> io::Result<u32> {
        sys::getopt(self.raw(), Opt::Tos).map(|v| v as u32)
    }

    pub fn set_tos_v4(&self, tos: u32) -> io::Result<()> {
        sys::setopt(self.raw(), Opt::Tos, tos as i32)
    }

    pub fn tclass_v6(&self) -> io::Result<u32> {
        sys::getopt(self.raw(), Opt::TClass).map(|v| v as u32)
    }

    pub fn set_tclass_v6(&self, tclass: u32) -> io::Result<()> {
        sys::setopt(self.raw(), Opt::TClass, tclass as i32)
    }

    pub fn new_v4() -> io::Result<TcpSocket> {
        Ok(TcpSocket {
            fd: sys::new_stream_socket(false)?,
        })
    }

    pub fn new_v6() -> io::Result<TcpSocket> {
        Ok(TcpSocket {
            fd: sys::new_stream_socket(true)?,
        })
    }

    pub fn set_reuseaddr(&self, v: bool) -> io::Result<()> {
        self.set_flag(Opt::ReuseAddr, v)
    }

    pub fn reuseaddr(&self) -> io::Result<bool> {
        self.flag(Opt::ReuseAddr)
    }

    /// unix だけ（tokio と同じ）。
    #[cfg(unix)]
    pub fn set_reuseport(&self, v: bool) -> io::Result<()> {
        self.set_flag(Opt::ReusePort, v)
    }

    #[cfg(unix)]
    pub fn reuseport(&self) -> io::Result<bool> {
        self.flag(Opt::ReusePort)
    }

    pub fn set_keepalive(&self, v: bool) -> io::Result<()> {
        self.set_flag(Opt::KeepAlive, v)
    }

    pub fn keepalive(&self) -> io::Result<bool> {
        self.flag(Opt::KeepAlive)
    }

    pub fn set_nodelay(&self, v: bool) -> io::Result<()> {
        self.set_flag(Opt::NoDelay, v)
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        self.flag(Opt::NoDelay)
    }

    pub fn set_send_buffer_size(&self, size: u32) -> io::Result<()> {
        sys::setopt(self.raw(), Opt::SndBuf, size as i32)
    }

    pub fn send_buffer_size(&self) -> io::Result<u32> {
        sys::getopt(self.raw(), Opt::SndBuf).map(|v| v as u32)
    }

    pub fn set_recv_buffer_size(&self, size: u32) -> io::Result<()> {
        sys::setopt(self.raw(), Opt::RcvBuf, size as i32)
    }

    pub fn recv_buffer_size(&self) -> io::Result<u32> {
        sys::getopt(self.raw(), Opt::RcvBuf).map(|v| v as u32)
    }

    pub fn set_linger(&self, dur: Option<std::time::Duration>) -> io::Result<()> {
        sys::set_linger(self.raw(), dur)
    }

    pub fn bind(&self, addr: SocketAddr) -> io::Result<()> {
        sys::bind(self.raw(), &addr)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        with_std::<std::net::TcpListener, _>(self.raw(), |l| l.local_addr())
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        sock_err(self.raw())
    }

    pub async fn connect(self, addr: SocketAddr) -> io::Result<TcpStream> {
        TcpStream::connect_fd(self.fd, addr).await
    }

    pub fn listen(self, backlog: u32) -> io::Result<TcpListener> {
        sys::listen(self.raw(), backlog)?;
        Ok(TcpListener {
            io: IoSource::new(self.fd),
        })
    }

    pub fn from_std_stream(stream: std::net::TcpStream) -> TcpSocket {
        TcpSocket { fd: stream.into() }
    }
}

socket_traits!(TcpSocket, |s| s.fd);

#[cfg(unix)]
impl FromRawFd for TcpSocket {
    unsafe fn from_raw_fd(fd: RawFd) -> TcpSocket {
        TcpSocket {
            fd: unsafe { sys::Owned::from_raw_fd(fd) },
        }
    }
}

#[cfg(unix)]
impl IntoRawFd for TcpSocket {
    fn into_raw_fd(self) -> RawFd {
        self.fd.into_raw_fd()
    }
}

#[cfg(windows)]
impl FromRawSocket for TcpSocket {
    unsafe fn from_raw_socket(sock: RawSocket) -> TcpSocket {
        TcpSocket {
            fd: unsafe { sys::Owned::from_raw_socket(sock) },
        }
    }
}

#[cfg(windows)]
impl IntoRawSocket for TcpSocket {
    fn into_raw_socket(self) -> RawSocket {
        self.fd.into_raw_socket()
    }
}

// ===================== UdpSocket =====================

/// `tokio::net::UdpSocket`。
#[derive(Debug)]
pub struct UdpSocket {
    io: IoSource,
}

impl UdpSocket {
    pub async fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<UdpSocket> {
        let addrs = addr.to_addrs().await?;
        let mut last = None;
        for a in addrs {
            match std::net::UdpSocket::bind(a) {
                Ok(s) => return UdpSocket::from_std(s),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "could not resolve to any address",
            )
        }))
    }

    #[track_caller]
    pub fn from_std(socket: std::net::UdpSocket) -> io::Result<UdpSocket> {
        check_io_context();
        socket.set_nonblocking(true)?;
        let fd: sys::Owned = socket.into();
        sys::prepare(sys::raw(&fd))?;
        Ok(UdpSocket {
            io: IoSource::new(fd),
        })
    }

    pub fn into_std(self) -> io::Result<std::net::UdpSocket> {
        let mut io = std::mem::ManuallyDrop::new(self.io);
        io.release();
        // SAFETY: Drop を走らせずに fd を取り出す
        let fd = unsafe { std::ptr::read(&io.fd) };
        let s = std::net::UdpSocket::from(fd);
        s.set_nonblocking(false)?;
        Ok(s)
    }

    fn std<R>(&self, f: impl FnOnce(&std::net::UdpSocket) -> R) -> R {
        with_std::<std::net::UdpSocket, _>(self.io.raw(), f)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.std(|s| s.local_addr())
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.std(|s| s.peer_addr())
    }

    pub async fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<()> {
        let addrs = addr.to_addrs().await?;
        let mut last = None;
        for a in addrs {
            match self.std(|s| s.connect(a)) {
                Ok(()) => return Ok(()),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "could not resolve to any address",
            )
        }))
    }

    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        poll_fn(|cx| self.poll_send(cx, buf)).await
    }

    pub fn poll_send(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.io.poll_io(true, cx, || self.std(|s| s.send(buf)))
    }

    pub fn try_send(&self, buf: &[u8]) -> io::Result<usize> {
        self.std(|s| s.send(buf))
    }

    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        poll_fn(|cx| {
            self.io
                .poll_io(false, cx, || self.std(|s| s.recv(&mut *buf)))
        })
        .await
    }

    pub fn poll_recv(&self, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let fd = self.io.raw();
        self.io
            .poll_io(false, cx, || sys::recv_buf(fd, buf, false))
            .map_ok(drop)
    }

    pub fn try_recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.std(|s| s.recv(buf))
    }

    pub async fn send_to<A: ToSocketAddrs>(&self, buf: &[u8], target: A) -> io::Result<usize> {
        let addrs = target.to_addrs().await?;
        let a = *addrs.first().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "no addresses to send data to")
        })?;
        poll_fn(|cx| self.poll_send_to(cx, buf, a)).await
    }

    pub fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: SocketAddr,
    ) -> Poll<io::Result<usize>> {
        self.io
            .poll_io(true, cx, || self.std(|s| s.send_to(buf, target)))
    }

    pub fn try_send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        self.std(|s| s.send_to(buf, target))
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        poll_fn(|cx| {
            self.io
                .poll_io(false, cx, || self.std(|s| s.recv_from(&mut *buf)))
        })
        .await
    }

    pub fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<SocketAddr>> {
        let (n, a) = ready!(self.io.poll_io(false, cx, || self
            .std(|s| s.recv_from(buf.initialize_unfilled()))))?;
        buf.advance(n);
        Poll::Ready(Ok(a))
    }

    pub fn try_recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.std(|s| s.recv_from(buf))
    }

    pub async fn peek_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        poll_fn(|cx| {
            self.io
                .poll_io(false, cx, || self.std(|s| s.peek_from(&mut *buf)))
        })
        .await
    }

    pub async fn readable(&self) -> io::Result<()> {
        poll_fn(|cx| self.io.poll_read_ready_nocoop(cx)).await
    }

    pub async fn writable(&self) -> io::Result<()> {
        poll_fn(|cx| self.io.poll_write_ready_nocoop(cx)).await
    }

    pub fn poll_recv_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.io.poll_read_ready(cx)
    }

    pub fn poll_send_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.io.poll_write_ready(cx)
    }

    pub fn set_broadcast(&self, on: bool) -> io::Result<()> {
        self.std(|s| s.set_broadcast(on))
    }

    pub fn broadcast(&self) -> io::Result<bool> {
        self.std(|s| s.broadcast())
    }

    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        self.std(|s| s.set_ttl(ttl))
    }

    pub fn ttl(&self) -> io::Result<u32> {
        self.std(|s| s.ttl())
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        sock_err(self.io.raw())
    }

    pub async fn ready(&self, interest: Interest) -> io::Result<Ready> {
        poll_fn(|cx| {
            let mut ready = Ready::EMPTY;
            if interest.is_readable() {
                if let Poll::Ready(r) = self.io.poll_read_ready_nocoop(cx) {
                    r?;
                    ready |= Ready::READABLE;
                }
            }
            if interest.is_writable() {
                if let Poll::Ready(r) = self.io.poll_write_ready_nocoop(cx) {
                    r?;
                    ready |= Ready::WRITABLE;
                }
            }
            if ready.is_empty() {
                Poll::Pending
            } else {
                Poll::Ready(Ok(ready))
            }
        })
        .await
    }

    pub fn try_recv_buf<B: bytes::BufMut>(&self, buf: &mut B) -> io::Result<usize> {
        let dst = buf.chunk_mut();
        let n = sys::recv_into(self.io.raw(), dst.as_mut_ptr(), dst.len())?;
        // SAFETY: n バイトは recv で初期化された
        unsafe { buf.advance_mut(n) };
        Ok(n)
    }

    pub async fn recv_buf<B: bytes::BufMut>(&self, buf: &mut B) -> io::Result<usize> {
        poll_fn(|cx| self.io.poll_io(false, cx, || self.try_recv_buf(&mut *buf))).await
    }

    pub fn try_recv_buf_from<B: bytes::BufMut>(
        &self,
        buf: &mut B,
    ) -> io::Result<(usize, SocketAddr)> {
        let dst = buf.chunk_mut();
        let (n, from) = sys::recv_from_into(self.io.raw(), dst.as_mut_ptr(), dst.len(), false)?;
        // SAFETY: n バイトは recvfrom で初期化された
        unsafe { buf.advance_mut(n) };
        Ok((n, from))
    }

    pub async fn recv_buf_from<B: bytes::BufMut>(
        &self,
        buf: &mut B,
    ) -> io::Result<(usize, SocketAddr)> {
        poll_fn(|cx| {
            self.io
                .poll_io(false, cx, || self.try_recv_buf_from(&mut *buf))
        })
        .await
    }

    pub fn poll_peek_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<SocketAddr>> {
        let (n, a) = ready!(self.io.poll_io(false, cx, || self
            .std(|s| s.peek_from(buf.initialize_unfilled()))))?;
        buf.advance(n);
        Poll::Ready(Ok(a))
    }

    pub fn try_peek_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.std(|s| s.peek_from(buf))
    }

    fn peek_sender_raw(&self) -> io::Result<SocketAddr> {
        let mut b = [0u8; 1];
        sys::recv_from_into(self.io.raw(), b.as_mut_ptr(), 0, true).map(|(_, a)| a)
    }

    pub async fn peek_sender(&self) -> io::Result<SocketAddr> {
        poll_fn(|cx| self.poll_peek_sender(cx)).await
    }

    pub fn poll_peek_sender(&self, cx: &mut Context<'_>) -> Poll<io::Result<SocketAddr>> {
        self.io.poll_io(false, cx, || self.peek_sender_raw())
    }

    pub fn try_peek_sender(&self) -> io::Result<SocketAddr> {
        self.peek_sender_raw()
    }

    pub fn multicast_loop_v4(&self) -> io::Result<bool> {
        self.std(|s| s.multicast_loop_v4())
    }

    pub fn set_multicast_loop_v4(&self, on: bool) -> io::Result<()> {
        self.std(|s| s.set_multicast_loop_v4(on))
    }

    pub fn multicast_ttl_v4(&self) -> io::Result<u32> {
        self.std(|s| s.multicast_ttl_v4())
    }

    pub fn set_multicast_ttl_v4(&self, ttl: u32) -> io::Result<()> {
        self.std(|s| s.set_multicast_ttl_v4(ttl))
    }

    pub fn multicast_loop_v6(&self) -> io::Result<bool> {
        self.std(|s| s.multicast_loop_v6())
    }

    pub fn set_multicast_loop_v6(&self, on: bool) -> io::Result<()> {
        self.std(|s| s.set_multicast_loop_v6(on))
    }

    pub fn join_multicast_v4(
        &self,
        multiaddr: std::net::Ipv4Addr,
        interface: std::net::Ipv4Addr,
    ) -> io::Result<()> {
        self.std(|s| s.join_multicast_v4(&multiaddr, &interface))
    }

    pub fn join_multicast_v6(
        &self,
        multiaddr: &std::net::Ipv6Addr,
        interface: u32,
    ) -> io::Result<()> {
        self.std(|s| s.join_multicast_v6(multiaddr, interface))
    }

    pub fn leave_multicast_v4(
        &self,
        multiaddr: std::net::Ipv4Addr,
        interface: std::net::Ipv4Addr,
    ) -> io::Result<()> {
        self.std(|s| s.leave_multicast_v4(&multiaddr, &interface))
    }

    pub fn leave_multicast_v6(
        &self,
        multiaddr: &std::net::Ipv6Addr,
        interface: u32,
    ) -> io::Result<()> {
        self.std(|s| s.leave_multicast_v6(multiaddr, interface))
    }

    pub fn tos_v4(&self) -> io::Result<u32> {
        sys::getopt(self.io.raw(), Opt::Tos).map(|v| v as u32)
    }

    pub fn set_tos_v4(&self, tos: u32) -> io::Result<()> {
        sys::setopt(self.io.raw(), Opt::Tos, tos as i32)
    }

    pub fn tclass_v6(&self) -> io::Result<u32> {
        sys::getopt(self.io.raw(), Opt::TClass).map(|v| v as u32)
    }

    pub fn set_tclass_v6(&self, tclass: u32) -> io::Result<()> {
        sys::setopt(self.io.raw(), Opt::TClass, tclass as i32)
    }
}

socket_traits!(UdpSocket, |s| s.io.fd);

/// `tokio::net::windows::named_pipe`（Windows の名前付きパイプ）。veil の tokio 互換レイヤーは名前付きパイプを
/// 提供しない（開く関数は `Unsupported` を返す）。型は依存クレート（hyper-util 等）が名前で参照するので用意する。
#[cfg(windows)]
pub mod windows {
    pub mod named_pipe {
        use crate::tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
        use std::io;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        fn unsupported() -> io::Error {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "named pipes are not supported by veil::tokio",
            )
        }

        /// 名前付きパイプのクライアント（作れない）。
        #[derive(Debug)]
        pub struct NamedPipeClient {
            _private: (),
        }

        /// 名前付きパイプのサーバ（作れない）。
        #[derive(Debug)]
        pub struct NamedPipeServer {
            _private: (),
        }

        /// パイプの種類。
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[non_exhaustive]
        pub enum PipeMode {
            Byte,
            Message,
        }

        /// クライアントの設定（`open` は `Unsupported`。設定の関数は tokio と同じ形で、値は使わない）。
        #[derive(Debug, Clone, Default)]
        pub struct ClientOptions {
            _private: (),
        }

        impl ClientOptions {
            pub fn new() -> ClientOptions {
                ClientOptions::default()
            }
            pub fn read(&mut self, _read: bool) -> &mut Self {
                self
            }
            pub fn write(&mut self, _write: bool) -> &mut Self {
                self
            }
            pub fn pipe_mode(&mut self, _mode: PipeMode) -> &mut Self {
                self
            }
            pub fn open(&self, _addr: impl AsRef<std::ffi::OsStr>) -> io::Result<NamedPipeClient> {
                Err(unsupported())
            }
        }

        /// サーバの設定（`create` は `Unsupported`。設定の関数は tokio と同じ形・同じ検査で、値は使わない）。
        #[derive(Debug, Clone, Default)]
        pub struct ServerOptions {
            _private: (),
        }

        impl ServerOptions {
            pub fn new() -> ServerOptions {
                ServerOptions::default()
            }
            pub fn pipe_mode(&mut self, _mode: PipeMode) -> &mut Self {
                self
            }
            pub fn access_inbound(&mut self, _allowed: bool) -> &mut Self {
                self
            }
            pub fn access_outbound(&mut self, _allowed: bool) -> &mut Self {
                self
            }
            pub fn first_pipe_instance(&mut self, _first: bool) -> &mut Self {
                self
            }
            pub fn reject_remote_clients(&mut self, _reject: bool) -> &mut Self {
                self
            }
            /// tokio と同じく 255 以上は panic（`PIPE_UNLIMITED_INSTANCES` と区別できないため）。
            #[track_caller]
            pub fn max_instances(&mut self, instances: usize) -> &mut Self {
                assert!(instances < 255, "cannot specify more than 254 instances");
                self
            }
            pub fn in_buffer_size(&mut self, _buffer: u32) -> &mut Self {
                self
            }
            pub fn out_buffer_size(&mut self, _buffer: u32) -> &mut Self {
                self
            }
            pub fn create(
                &self,
                _addr: impl AsRef<std::ffi::OsStr>,
            ) -> io::Result<NamedPipeServer> {
                Err(unsupported())
            }
        }

        macro_rules! no_io {
            ($t:ty) => {
                impl AsyncRead for $t {
                    fn poll_read(
                        self: Pin<&mut Self>,
                        _: &mut Context<'_>,
                        _: &mut ReadBuf<'_>,
                    ) -> Poll<io::Result<()>> {
                        Poll::Ready(Err(unsupported()))
                    }
                }
                impl AsyncWrite for $t {
                    fn poll_write(
                        self: Pin<&mut Self>,
                        _: &mut Context<'_>,
                        _: &[u8],
                    ) -> Poll<io::Result<usize>> {
                        Poll::Ready(Err(unsupported()))
                    }
                    fn poll_flush(
                        self: Pin<&mut Self>,
                        _: &mut Context<'_>,
                    ) -> Poll<io::Result<()>> {
                        Poll::Ready(Ok(()))
                    }
                    fn poll_shutdown(
                        self: Pin<&mut Self>,
                        _: &mut Context<'_>,
                    ) -> Poll<io::Result<()>> {
                        Poll::Ready(Ok(()))
                    }
                }
            };
        }
        no_io!(NamedPipeClient);
        no_io!(NamedPipeServer);
    }
}
