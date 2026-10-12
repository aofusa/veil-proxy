//! `tokio::net` 互換（F-198、unix）。
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
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{ready, Context, Poll};
use std::thread::ThreadId;

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
            if owner == std::thread::current().id()
                && !crate::tokio::waker::exiting()
                && crate::runtime::has_driver()
            {
                drop(w);
            } else {
                std::mem::forget(w);
            }
        }
    }
}

// SAFETY: 待ちの Future は fd と整数だけを持つ。触るのは作ったスレッドだけ（他スレッドでは forget する）
unsafe impl Send for WaitSlot {}

/// fd と、読み・書きの準備待ち。
pub(crate) struct IoSource {
    fd: OwnedFd,
    read: Mutex<WaitSlot>,
    write: Mutex<WaitSlot>,
    /// 準備できたことを覚える（tokio と同じく、操作が `WouldBlock` を返すまで準備できたままとみなす）
    read_ready: std::sync::atomic::AtomicBool,
    write_ready: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for IoSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IoSource")
            .field("fd", &self.fd.as_raw_fd())
            .finish()
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
    fn new(fd: OwnedFd) -> IoSource {
        IoSource {
            fd,
            read: Mutex::new(WaitSlot::default()),
            write: Mutex::new(WaitSlot::default()),
            read_ready: std::sync::atomic::AtomicBool::new(false),
            write_ready: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
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
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let rdhup = libc::POLLRDHUP;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let rdhup = 0;
        let mut pfd = libc::pollfd {
            fd: self.raw(),
            events: libc::POLLIN | rdhup,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pfd, 1, 0) } > 0 {
            if pfd.revents & (rdhup | libc::POLLHUP) != 0 && ready.is_readable() {
                ready |= Ready::READ_CLOSED;
            }
            if pfd.revents & libc::POLLHUP != 0 && ready.is_writable() {
                ready |= Ready::WRITE_CLOSED;
            }
            if pfd.revents & libc::POLLERR != 0 {
                ready |= Ready::ERROR;
            }
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

fn cvt(r: isize) -> io::Result<usize> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r as usize)
    }
}

fn cvt_i(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const SEND_FLAGS: libc::c_int = libc::MSG_NOSIGNAL;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const SEND_FLAGS: libc::c_int = 0;

fn sys_recv(fd: RawFd, buf: &mut ReadBuf<'_>, flags: libc::c_int) -> io::Result<usize> {
    // SAFETY: 未初期化領域へ書くだけ。書いた分だけ assume_init する
    let dst = unsafe { buf.unfilled_mut() };
    let n = cvt(unsafe { libc::recv(fd, dst.as_mut_ptr().cast(), dst.len(), flags) })?;
    unsafe { buf.assume_init(n) };
    buf.advance(n);
    Ok(n)
}

fn sys_send(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    cvt(unsafe { libc::send(fd, buf.as_ptr().cast(), buf.len(), SEND_FLAGS) })
}

fn sys_writev(fd: RawFd, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
    let n = bufs.len().min(1024) as libc::c_int;
    cvt(unsafe { libc::writev(fd, bufs.as_ptr().cast(), n) })
}

fn set_nonblocking_cloexec(fd: RawFd) -> io::Result<()> {
    unsafe {
        let fl = cvt_i(libc::fcntl(fd, libc::F_GETFL))?;
        cvt_i(libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK))?;
        let fd_fl = cvt_i(libc::fcntl(fd, libc::F_GETFD))?;
        cvt_i(libc::fcntl(fd, libc::F_SETFD, fd_fl | libc::FD_CLOEXEC))?;
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    unsafe {
        // SIGPIPE を出さない（MSG_NOSIGNAL の代わり）
        #[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "netbsd"))]
        {
            let one: libc::c_int = 1;
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                &one as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
    Ok(())
}

fn new_socket(domain: libc::c_int, ty: libc::c_int) -> io::Result<OwnedFd> {
    let fd = cvt_i(unsafe { libc::socket(domain, ty, 0) })?;
    // SAFETY: 作ったばかりの fd
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    set_nonblocking_cloexec(fd)?;
    Ok(owned)
}

// 理由付き allow: BSD・macOS の sockaddr_in には sin_len 等の追加のフィールドがあり、..zeroed() が要る（Linux では不要）
#[allow(clippy::needless_update)]
fn sockaddr(addr: &SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: ゼロ初期化した sockaddr_storage に書くだけ
    let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let len = match addr {
        SocketAddr::V4(a) => {
            let sin = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: a.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(a.ip().octets()),
                },
                ..unsafe { std::mem::zeroed() }
            };
            unsafe { std::ptr::write(&mut ss as *mut _ as *mut libc::sockaddr_in, sin) };
            std::mem::size_of::<libc::sockaddr_in>()
        }
        SocketAddr::V6(a) => {
            let sin6 = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: a.port().to_be(),
                sin6_flowinfo: a.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: a.ip().octets(),
                },
                sin6_scope_id: a.scope_id(),
                ..unsafe { std::mem::zeroed() }
            };
            unsafe { std::ptr::write(&mut ss as *mut _ as *mut libc::sockaddr_in6, sin6) };
            std::mem::size_of::<libc::sockaddr_in6>()
        }
    };
    (ss, len as libc::socklen_t)
}

fn sock_err(fd: RawFd) -> io::Result<Option<io::Error>> {
    let mut err: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    cvt_i(unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            &mut err as *mut _ as *mut libc::c_void,
            &mut len,
        )
    })?;
    Ok((err != 0).then(|| io::Error::from_raw_os_error(err)))
}

/// fd を std の型として借りる（アドレスの取得・ソケットオプション用。閉じない）。
fn with_std<T: FromRawFd + IntoRawFd, R>(fd: RawFd, f: impl FnOnce(&T) -> R) -> R {
    // SAFETY: 借りるだけで、終わったら into_raw_fd で手放す
    let s = std::mem::ManuallyDrop::new(unsafe { T::from_raw_fd(fd) });
    f(&s)
}

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
        let domain = if addr.is_ipv4() {
            libc::AF_INET
        } else {
            libc::AF_INET6
        };
        let fd = new_socket(domain, libc::SOCK_STREAM)?;
        TcpStream::connect_fd(fd, addr).await
    }

    async fn connect_fd(fd: OwnedFd, addr: SocketAddr) -> io::Result<TcpStream> {
        let (ss, len) = sockaddr(&addr);
        let r = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                &ss as *const _ as *const libc::sockaddr,
                len,
            )
        };
        let s = TcpStream {
            io: Arc::new(IoSource::new(fd)),
        };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINPROGRESS) && e.kind() != io::ErrorKind::WouldBlock
            {
                return Err(e);
            }
        }
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
        let fd: OwnedFd = stream.into();
        set_nonblocking_cloexec(fd.as_raw_fd())?;
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
        set_linger_raw(self.io.raw(), Some(std::time::Duration::ZERO))
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn quickack(&self) -> io::Result<bool> {
        getopt_int(self.io.raw(), libc::IPPROTO_TCP, libc::TCP_QUICKACK).map(|v| v != 0)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn set_quickack(&self, quickack: bool) -> io::Result<()> {
        setopt_int(
            self.io.raw(),
            libc::IPPROTO_TCP,
            libc::TCP_QUICKACK,
            quickack as libc::c_int,
        )
    }

    pub fn linger(&self) -> io::Result<Option<std::time::Duration>> {
        let mut l: libc::linger = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::linger>() as libc::socklen_t;
        cvt_i(unsafe {
            libc::getsockopt(
                self.io.raw(),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                &mut l as *mut _ as *mut libc::c_void,
                &mut len,
            )
        })?;
        Ok((l.l_onoff != 0).then(|| std::time::Duration::from_secs(l.l_linger as u64)))
    }

    pub fn set_linger(&self, dur: Option<std::time::Duration>) -> io::Result<()> {
        let l = libc::linger {
            l_onoff: dur.is_some() as libc::c_int,
            l_linger: dur.map(|d| d.as_secs() as libc::c_int).unwrap_or(0),
        };
        cvt_i(unsafe {
            libc::setsockopt(
                self.io.raw(),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                &l as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::linger>() as libc::socklen_t,
            )
        })
        .map(drop)
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
            cvt(unsafe { libc::recv(self.io.raw(), buf.as_mut_ptr().cast(), buf.len(), 0) }),
        )
    }

    pub fn try_read_buf<B: bytes::BufMut>(&self, buf: &mut B) -> io::Result<usize> {
        let dst = buf.chunk_mut();
        let n = self.io.track(
            false,
            cvt(unsafe { libc::recv(self.io.raw(), dst.as_mut_ptr().cast(), dst.len(), 0) }),
        )?;
        // SAFETY: n バイトは recv で初期化された
        unsafe { buf.advance_mut(n) };
        Ok(n)
    }

    pub fn try_read_vectored(&self, bufs: &mut [io::IoSliceMut<'_>]) -> io::Result<usize> {
        cvt(unsafe {
            libc::readv(
                self.io.raw(),
                bufs.as_mut_ptr().cast(),
                bufs.len().min(1024) as libc::c_int,
            )
        })
    }

    pub fn try_write(&self, buf: &[u8]) -> io::Result<usize> {
        self.io.track(true, sys_send(self.io.raw(), buf))
    }

    pub fn try_write_vectored(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        self.io.track(true, sys_writev(self.io.raw(), bufs))
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
        self.io
            .poll_io(false, cx, || sys_recv(fd, buf, libc::MSG_PEEK))
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
            .poll_io(false, cx, || sys_recv(fd, buf, 0))
            .map_ok(drop)
    }

    fn poll_write_priv(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let fd = self.io.raw();
        self.io.poll_io(true, cx, || sys_send(fd, buf))
    }

    fn poll_write_vectored_priv(
        &self,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let fd = self.io.raw();
        self.io.poll_io(true, cx, || sys_writev(fd, bufs))
    }

    fn shutdown_std(&self, how: std::net::Shutdown) -> io::Result<()> {
        let r = with_std::<std::net::TcpStream, _>(self.io.raw(), |s| s.shutdown(how));
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

impl AsRawFd for TcpStream {
    fn as_raw_fd(&self) -> RawFd {
        self.io.raw()
    }
}

impl AsFd for TcpStream {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.io.fd.as_fd()
    }
}

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

fn accept_fd(fd: RawFd) -> io::Result<(OwnedFd, SocketAddr)> {
    let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let nfd =
        cvt_i(unsafe { libc::accept(fd, &mut ss as *mut _ as *mut libc::sockaddr, &mut len) })?;
    // SAFETY: accept が返した fd
    let owned = unsafe { OwnedFd::from_raw_fd(nfd) };
    set_nonblocking_cloexec(nfd)?;
    let addr = with_std::<std::net::TcpStream, _>(nfd, |s| s.peer_addr())?;
    Ok((owned, addr))
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
        let fd: OwnedFd = listener.into();
        set_nonblocking_cloexec(fd.as_raw_fd())?;
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
        let (s, a) = ready!(self.io.poll_io(false, cx, || accept_fd(fd)))?;
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

impl AsRawFd for TcpListener {
    fn as_raw_fd(&self) -> RawFd {
        self.io.raw()
    }
}

impl AsFd for TcpListener {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.io.fd.as_fd()
    }
}

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
    fd: OwnedFd,
}

fn setopt_int(fd: RawFd, level: libc::c_int, opt: libc::c_int, v: libc::c_int) -> io::Result<()> {
    cvt_i(unsafe {
        libc::setsockopt(
            fd,
            level,
            opt,
            &v as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    })
    .map(drop)
}

fn getopt_int(fd: RawFd, level: libc::c_int, opt: libc::c_int) -> io::Result<libc::c_int> {
    let mut v: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    cvt_i(unsafe {
        libc::getsockopt(
            fd,
            level,
            opt,
            &mut v as *mut _ as *mut libc::c_void,
            &mut len,
        )
    })?;
    Ok(v)
}

fn get_linger(fd: RawFd) -> io::Result<Option<std::time::Duration>> {
    let mut l: libc::linger = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::linger>() as libc::socklen_t;
    cvt_i(unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            &mut l as *mut _ as *mut libc::c_void,
            &mut len,
        )
    })?;
    Ok((l.l_onoff != 0).then(|| std::time::Duration::from_secs(l.l_linger as u64)))
}

fn set_linger_raw(fd: RawFd, dur: Option<std::time::Duration>) -> io::Result<()> {
    let l = libc::linger {
        l_onoff: dur.is_some() as libc::c_int,
        l_linger: dur.map(|d| d.as_secs() as libc::c_int).unwrap_or(0),
    };
    cvt_i(unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            &l as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        )
    })
    .map(drop)
}

impl TcpSocket {
    pub fn linger(&self) -> io::Result<Option<std::time::Duration>> {
        get_linger(self.fd.as_raw_fd())
    }

    pub fn set_zero_linger(&self) -> io::Result<()> {
        set_linger_raw(self.fd.as_raw_fd(), Some(std::time::Duration::ZERO))
    }

    pub fn tos_v4(&self) -> io::Result<u32> {
        getopt_int(self.fd.as_raw_fd(), libc::IPPROTO_IP, libc::IP_TOS).map(|v| v as u32)
    }

    pub fn set_tos_v4(&self, tos: u32) -> io::Result<()> {
        setopt_int(
            self.fd.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_TOS,
            tos as libc::c_int,
        )
    }

    pub fn tclass_v6(&self) -> io::Result<u32> {
        getopt_int(self.fd.as_raw_fd(), libc::IPPROTO_IPV6, libc::IPV6_TCLASS).map(|v| v as u32)
    }

    pub fn set_tclass_v6(&self, tclass: u32) -> io::Result<()> {
        setopt_int(
            self.fd.as_raw_fd(),
            libc::IPPROTO_IPV6,
            libc::IPV6_TCLASS,
            tclass as libc::c_int,
        )
    }

    pub fn new_v4() -> io::Result<TcpSocket> {
        Ok(TcpSocket {
            fd: new_socket(libc::AF_INET, libc::SOCK_STREAM)?,
        })
    }

    pub fn new_v6() -> io::Result<TcpSocket> {
        Ok(TcpSocket {
            fd: new_socket(libc::AF_INET6, libc::SOCK_STREAM)?,
        })
    }

    pub fn set_reuseaddr(&self, v: bool) -> io::Result<()> {
        setopt_int(
            self.fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            v as libc::c_int,
        )
    }

    pub fn reuseaddr(&self) -> io::Result<bool> {
        getopt_int(self.fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_REUSEADDR).map(|v| v != 0)
    }

    pub fn set_reuseport(&self, v: bool) -> io::Result<()> {
        setopt_int(
            self.fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_REUSEPORT,
            v as libc::c_int,
        )
    }

    pub fn reuseport(&self) -> io::Result<bool> {
        getopt_int(self.fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_REUSEPORT).map(|v| v != 0)
    }

    pub fn set_keepalive(&self, v: bool) -> io::Result<()> {
        setopt_int(
            self.fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_KEEPALIVE,
            v as libc::c_int,
        )
    }

    pub fn keepalive(&self) -> io::Result<bool> {
        getopt_int(self.fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_KEEPALIVE).map(|v| v != 0)
    }

    pub fn set_nodelay(&self, v: bool) -> io::Result<()> {
        setopt_int(
            self.fd.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_NODELAY,
            v as libc::c_int,
        )
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        getopt_int(self.fd.as_raw_fd(), libc::IPPROTO_TCP, libc::TCP_NODELAY).map(|v| v != 0)
    }

    pub fn set_send_buffer_size(&self, size: u32) -> io::Result<()> {
        setopt_int(
            self.fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            size as libc::c_int,
        )
    }

    pub fn send_buffer_size(&self) -> io::Result<u32> {
        getopt_int(self.fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDBUF).map(|v| v as u32)
    }

    pub fn set_recv_buffer_size(&self, size: u32) -> io::Result<()> {
        setopt_int(
            self.fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            size as libc::c_int,
        )
    }

    pub fn recv_buffer_size(&self) -> io::Result<u32> {
        getopt_int(self.fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_RCVBUF).map(|v| v as u32)
    }

    pub fn set_linger(&self, dur: Option<std::time::Duration>) -> io::Result<()> {
        let l = libc::linger {
            l_onoff: dur.is_some() as libc::c_int,
            l_linger: dur.map(|d| d.as_secs() as libc::c_int).unwrap_or(0),
        };
        cvt_i(unsafe {
            libc::setsockopt(
                self.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                &l as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::linger>() as libc::socklen_t,
            )
        })
        .map(drop)
    }

    pub fn bind(&self, addr: SocketAddr) -> io::Result<()> {
        let (ss, len) = sockaddr(&addr);
        cvt_i(unsafe {
            libc::bind(
                self.fd.as_raw_fd(),
                &ss as *const _ as *const libc::sockaddr,
                len,
            )
        })
        .map(drop)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        with_std::<std::net::TcpListener, _>(self.fd.as_raw_fd(), |l| l.local_addr())
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        sock_err(self.fd.as_raw_fd())
    }

    pub async fn connect(self, addr: SocketAddr) -> io::Result<TcpStream> {
        TcpStream::connect_fd(self.fd, addr).await
    }

    pub fn listen(self, backlog: u32) -> io::Result<TcpListener> {
        cvt_i(unsafe {
            libc::listen(
                self.fd.as_raw_fd(),
                backlog.min(i32::MAX as u32) as libc::c_int,
            )
        })?;
        Ok(TcpListener {
            io: IoSource::new(self.fd),
        })
    }

    pub fn from_std_stream(stream: std::net::TcpStream) -> TcpSocket {
        TcpSocket { fd: stream.into() }
    }
}

impl AsRawFd for TcpSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl AsFd for TcpSocket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl FromRawFd for TcpSocket {
    unsafe fn from_raw_fd(fd: RawFd) -> TcpSocket {
        TcpSocket {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        }
    }
}

impl IntoRawFd for TcpSocket {
    fn into_raw_fd(self) -> RawFd {
        self.fd.into_raw_fd()
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
        let fd: OwnedFd = socket.into();
        set_nonblocking_cloexec(fd.as_raw_fd())?;
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
            .poll_io(false, cx, || sys_recv(fd, buf, 0))
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
        let n = cvt(unsafe { libc::recv(self.io.raw(), dst.as_mut_ptr().cast(), dst.len(), 0) })?;
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
        let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let dst = buf.chunk_mut();
        let n = cvt(unsafe {
            libc::recvfrom(
                self.io.raw(),
                dst.as_mut_ptr().cast(),
                dst.len(),
                0,
                &mut ss as *mut _ as *mut libc::sockaddr,
                &mut len,
            )
        })?;
        // SAFETY: n バイトは recvfrom で初期化された
        unsafe { buf.advance_mut(n) };
        Ok((n, from_sockaddr(&ss)?))
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
        let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let mut b = [0u8; 1];
        cvt(unsafe {
            libc::recvfrom(
                self.io.raw(),
                b.as_mut_ptr().cast(),
                0,
                libc::MSG_PEEK,
                &mut ss as *mut _ as *mut libc::sockaddr,
                &mut len,
            )
        })?;
        from_sockaddr(&ss)
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
        getopt_int(self.io.raw(), libc::IPPROTO_IP, libc::IP_TOS).map(|v| v as u32)
    }

    pub fn set_tos_v4(&self, tos: u32) -> io::Result<()> {
        setopt_int(
            self.io.raw(),
            libc::IPPROTO_IP,
            libc::IP_TOS,
            tos as libc::c_int,
        )
    }

    pub fn tclass_v6(&self) -> io::Result<u32> {
        getopt_int(self.io.raw(), libc::IPPROTO_IPV6, libc::IPV6_TCLASS).map(|v| v as u32)
    }

    pub fn set_tclass_v6(&self, tclass: u32) -> io::Result<()> {
        setopt_int(
            self.io.raw(),
            libc::IPPROTO_IPV6,
            libc::IPV6_TCLASS,
            tclass as libc::c_int,
        )
    }
}

/// sockaddr_storage を SocketAddr にする。
fn from_sockaddr(ss: &libc::sockaddr_storage) -> io::Result<SocketAddr> {
    match ss.ss_family as libc::c_int {
        libc::AF_INET => {
            // SAFETY: family が AF_INET
            let a = unsafe { &*(ss as *const _ as *const libc::sockaddr_in) };
            Ok(SocketAddr::new(
                std::net::Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr)).into(),
                u16::from_be(a.sin_port),
            ))
        }
        libc::AF_INET6 => {
            // SAFETY: family が AF_INET6
            let a = unsafe { &*(ss as *const _ as *const libc::sockaddr_in6) };
            Ok(SocketAddr::V6(std::net::SocketAddrV6::new(
                std::net::Ipv6Addr::from(a.sin6_addr.s6_addr),
                u16::from_be(a.sin6_port),
                a.sin6_flowinfo,
                a.sin6_scope_id,
            )))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported address family",
        )),
    }
}

impl AsRawFd for UdpSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.io.raw()
    }
}

impl AsFd for UdpSocket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.io.fd.as_fd()
    }
}

// ===================== Unix ドメインソケット =====================

/// `tokio::net::UnixStream`。
#[derive(Debug)]
pub struct UnixStream {
    inner: TcpStream,
}

impl UnixStream {
    pub async fn connect<P: AsRef<std::path::Path>>(path: P) -> io::Result<UnixStream> {
        let fd = new_socket(libc::AF_UNIX, libc::SOCK_STREAM)?;
        let p = path.as_ref().as_os_str().as_encoded_bytes();
        let mut sun: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        if p.len() >= sun.sun_path.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "path too long"));
        }
        sun.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (d, s) in sun.sun_path.iter_mut().zip(p) {
            *d = *s as libc::c_char;
        }
        // 抽象名前空間（先頭が NUL）は終端の NUL を含めない
        let extra = if p.first() == Some(&0) { 0 } else { 1 };
        let len = (std::mem::size_of::<libc::sa_family_t>() + p.len() + extra) as libc::socklen_t;
        let r = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                &sun as *const _ as *const libc::sockaddr,
                len,
            )
        };
        let s = TcpStream {
            io: Arc::new(IoSource::new(fd)),
        };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINPROGRESS) && e.kind() != io::ErrorKind::WouldBlock
            {
                return Err(e);
            }
        }
        // tokio と同じく、すぐ繋がっても一度ランタイムの準備待ちを通して SO_ERROR を見る
        poll_fn(|cx| s.io.poll_write_ready(cx)).await?;
        if let Some(e) = sock_err(s.io.raw())? {
            return Err(e);
        }
        Ok(UnixStream { inner: s })
    }

    /// アドレス（抽象名前空間を含む）へ接続する。
    pub async fn connect_addr(addr: &std::os::unix::net::SocketAddr) -> io::Result<UnixStream> {
        UnixStream::from_std(std::os::unix::net::UnixStream::connect_addr(addr)?)
    }

    pub fn into_std(self) -> io::Result<std::os::unix::net::UnixStream> {
        let s = self.inner.into_std()?;
        let fd: OwnedFd = s.into();
        Ok(std::os::unix::net::UnixStream::from(fd))
    }

    /// 相手の資格情報。
    pub fn peer_cred(&self) -> io::Result<UCred> {
        peer_cred(self.inner.io.raw())
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        sock_err(self.inner.io.raw())
    }

    pub fn pair() -> io::Result<(UnixStream, UnixStream)> {
        let (a, b) = std::os::unix::net::UnixStream::pair()?;
        Ok((UnixStream::from_std(a)?, UnixStream::from_std(b)?))
    }

    #[track_caller]
    pub fn from_std(stream: std::os::unix::net::UnixStream) -> io::Result<UnixStream> {
        check_io_context();
        stream.set_nonblocking(true)?;
        let fd: OwnedFd = stream.into();
        set_nonblocking_cloexec(fd.as_raw_fd())?;
        Ok(UnixStream {
            inner: TcpStream {
                io: Arc::new(IoSource::new(fd)),
            },
        })
    }

    pub fn local_addr(&self) -> io::Result<std::os::unix::net::SocketAddr> {
        with_std::<std::os::unix::net::UnixStream, _>(self.inner.io.raw(), |s| s.local_addr())
    }

    pub fn peer_addr(&self) -> io::Result<std::os::unix::net::SocketAddr> {
        with_std::<std::os::unix::net::UnixStream, _>(self.inner.io.raw(), |s| s.peer_addr())
    }

    pub async fn readable(&self) -> io::Result<()> {
        self.inner.readable().await
    }

    pub async fn writable(&self) -> io::Result<()> {
        self.inner.writable().await
    }

    pub fn try_read(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.try_read(buf)
    }

    pub fn try_write(&self, buf: &[u8]) -> io::Result<usize> {
        self.inner.try_write(buf)
    }

    pub fn try_read_buf<B: bytes::BufMut>(&self, buf: &mut B) -> io::Result<usize> {
        self.inner.try_read_buf(buf)
    }

    pub fn try_read_vectored(&self, bufs: &mut [io::IoSliceMut<'_>]) -> io::Result<usize> {
        self.inner.try_read_vectored(bufs)
    }

    pub fn try_write_vectored(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        self.inner.try_write_vectored(bufs)
    }

    pub async fn ready(&self, interest: Interest) -> io::Result<Ready> {
        self.inner.ready(interest).await
    }

    pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.poll_read_ready(cx)
    }

    pub fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.poll_write_ready(cx)
    }

    pub fn split(&mut self) -> (tcp::ReadHalf<'_>, tcp::WriteHalf<'_>) {
        self.inner.split()
    }

    pub fn into_split(self) -> (tcp::OwnedReadHalf, tcp::OwnedWriteHalf) {
        self.inner.into_split()
    }
}

impl AsyncRead for UnixStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.inner.poll_read_priv(cx, buf)
    }
}

impl AsyncWrite for UnixStream {
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
        Poll::Ready(self.inner.shutdown_std(std::net::Shutdown::Write))
    }
}

impl AsRawFd for UnixStream {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.io.raw()
    }
}

/// `tokio::net::UnixListener`。
#[derive(Debug)]
pub struct UnixListener {
    io: IoSource,
}

impl UnixListener {
    #[track_caller]
    pub fn bind<P: AsRef<std::path::Path>>(path: P) -> io::Result<UnixListener> {
        check_io_context();
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            use std::os::linux::net::SocketAddrExt;
            let p = path.as_ref().as_os_str().as_encoded_bytes();
            if p.first() == Some(&0) {
                let addr = std::os::unix::net::SocketAddr::from_abstract_name(&p[1..])?;
                return UnixListener::from_std(std::os::unix::net::UnixListener::bind_addr(&addr)?);
            }
        }
        UnixListener::from_std(std::os::unix::net::UnixListener::bind(path)?)
    }

    #[track_caller]
    pub fn from_std(listener: std::os::unix::net::UnixListener) -> io::Result<UnixListener> {
        check_io_context();
        listener.set_nonblocking(true)?;
        let fd: OwnedFd = listener.into();
        set_nonblocking_cloexec(fd.as_raw_fd())?;
        Ok(UnixListener {
            io: IoSource::new(fd),
        })
    }

    pub async fn accept(&self) -> io::Result<(UnixStream, std::os::unix::net::SocketAddr)> {
        poll_fn(|cx| self.poll_accept(cx)).await
    }

    pub fn poll_accept(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<(UnixStream, std::os::unix::net::SocketAddr)>> {
        let fd = self.io.raw();
        let r = ready!(self.io.poll_io(false, cx, || with_std::<
            std::os::unix::net::UnixListener,
            _,
        >(fd, |l| l.accept())))?;
        let (s, a) = r;
        Poll::Ready(Ok((UnixStream::from_std(s)?, a)))
    }

    pub fn local_addr(&self) -> io::Result<std::os::unix::net::SocketAddr> {
        with_std::<std::os::unix::net::UnixListener, _>(self.io.raw(), |l| l.local_addr())
    }

    pub fn bind_addr(addr: &std::os::unix::net::SocketAddr) -> io::Result<UnixListener> {
        UnixListener::from_std(std::os::unix::net::UnixListener::bind_addr(addr)?)
    }

    pub fn into_std(self) -> io::Result<std::os::unix::net::UnixListener> {
        let mut io = std::mem::ManuallyDrop::new(self.io);
        io.release();
        // SAFETY: Drop を走らせずに fd を取り出す
        let fd = unsafe { std::ptr::read(&io.fd) };
        let l = std::os::unix::net::UnixListener::from(fd);
        l.set_nonblocking(false)?;
        Ok(l)
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        sock_err(self.io.raw())
    }
}

/// 相手の資格情報（`UnixStream::peer_cred`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UCred {
    pid: Option<libc::pid_t>,
    uid: libc::uid_t,
    gid: libc::gid_t,
}

impl UCred {
    pub fn uid(&self) -> libc::uid_t {
        self.uid
    }
    pub fn gid(&self) -> libc::gid_t {
        self.gid
    }
    pub fn pid(&self) -> Option<libc::pid_t> {
        self.pid
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_cred(fd: RawFd) -> io::Result<UCred> {
    let mut c: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    cvt_i(unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut c as *mut _ as *mut libc::c_void,
            &mut len,
        )
    })?;
    Ok(UCred {
        pid: Some(c.pid),
        uid: c.uid,
        gid: c.gid,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn peer_cred(fd: RawFd) -> io::Result<UCred> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    cvt_i(unsafe { libc::getpeereid(fd, &mut uid, &mut gid) })?;
    Ok(UCred {
        pid: None,
        uid,
        gid,
    })
}

/// `tokio::net::UnixDatagram`。
#[derive(Debug)]
pub struct UnixDatagram {
    io: IoSource,
}

impl UnixDatagram {
    pub fn poll_send_to<P: AsRef<std::path::Path>>(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: P,
    ) -> Poll<io::Result<usize>> {
        let t = target.as_ref();
        self.io
            .poll_io(true, cx, || self.std(|d| d.send_to(buf, t)))
    }

    pub fn poll_send_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.io.poll_write_ready(cx)
    }

    pub fn poll_recv_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.io.poll_read_ready(cx)
    }

    pub fn try_recv_buf<B: bytes::BufMut>(&self, buf: &mut B) -> io::Result<usize> {
        let dst = buf.chunk_mut();
        let n = self.io.track(
            false,
            cvt(unsafe { libc::recv(self.io.raw(), dst.as_mut_ptr().cast(), dst.len(), 0) }),
        )?;
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
    ) -> io::Result<(usize, std::os::unix::net::SocketAddr)> {
        let dst = buf.chunk_mut();
        // SAFETY: 未初期化領域へ読み込み、読んだ分だけ advance_mut する
        let slice = unsafe { std::slice::from_raw_parts_mut(dst.as_mut_ptr(), dst.len()) };
        let (n, a) = self.io.track(false, self.std(|d| d.recv_from(slice)))?;
        unsafe { buf.advance_mut(n) };
        Ok((n, a))
    }

    pub async fn recv_buf_from<B: bytes::BufMut>(
        &self,
        buf: &mut B,
    ) -> io::Result<(usize, std::os::unix::net::SocketAddr)> {
        poll_fn(|cx| {
            self.io
                .poll_io(false, cx, || self.try_recv_buf_from(&mut *buf))
        })
        .await
    }

    fn std<R>(&self, f: impl FnOnce(&std::os::unix::net::UnixDatagram) -> R) -> R {
        with_std::<std::os::unix::net::UnixDatagram, _>(self.io.raw(), f)
    }

    pub fn bind<P: AsRef<std::path::Path>>(path: P) -> io::Result<UnixDatagram> {
        UnixDatagram::from_std(std::os::unix::net::UnixDatagram::bind(path)?)
    }

    pub fn bind_addr(addr: &std::os::unix::net::SocketAddr) -> io::Result<UnixDatagram> {
        UnixDatagram::from_std(std::os::unix::net::UnixDatagram::bind_addr(addr)?)
    }

    pub fn unbound() -> io::Result<UnixDatagram> {
        UnixDatagram::from_std(std::os::unix::net::UnixDatagram::unbound()?)
    }

    pub fn pair() -> io::Result<(UnixDatagram, UnixDatagram)> {
        let (a, b) = std::os::unix::net::UnixDatagram::pair()?;
        Ok((UnixDatagram::from_std(a)?, UnixDatagram::from_std(b)?))
    }

    #[track_caller]
    pub fn from_std(d: std::os::unix::net::UnixDatagram) -> io::Result<UnixDatagram> {
        check_io_context();
        d.set_nonblocking(true)?;
        let fd: OwnedFd = d.into();
        set_nonblocking_cloexec(fd.as_raw_fd())?;
        Ok(UnixDatagram {
            io: IoSource::new(fd),
        })
    }

    pub fn into_std(self) -> io::Result<std::os::unix::net::UnixDatagram> {
        let mut io = std::mem::ManuallyDrop::new(self.io);
        io.release();
        // SAFETY: Drop を走らせずに fd を取り出す
        let fd = unsafe { std::ptr::read(&io.fd) };
        let d = std::os::unix::net::UnixDatagram::from(fd);
        d.set_nonblocking(false)?;
        Ok(d)
    }

    pub fn connect<P: AsRef<std::path::Path>>(&self, path: P) -> io::Result<()> {
        self.std(|d| d.connect(path))
    }

    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        poll_fn(|cx| self.io.poll_io(true, cx, || self.std(|d| d.send(buf)))).await
    }

    pub fn try_send(&self, buf: &[u8]) -> io::Result<usize> {
        self.std(|d| d.send(buf))
    }

    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        poll_fn(|cx| {
            self.io
                .poll_io(false, cx, || self.std(|d| d.recv(&mut *buf)))
        })
        .await
    }

    pub fn try_recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.std(|d| d.recv(buf))
    }

    pub fn poll_recv(&self, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let fd = self.io.raw();
        self.io
            .poll_io(false, cx, || sys_recv(fd, buf, 0))
            .map_ok(drop)
    }

    pub fn poll_send(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.io.poll_io(true, cx, || self.std(|d| d.send(buf)))
    }

    pub async fn send_to<P: AsRef<std::path::Path>>(
        &self,
        buf: &[u8],
        target: P,
    ) -> io::Result<usize> {
        let t = target.as_ref().to_owned();
        poll_fn(|cx| {
            self.io
                .poll_io(true, cx, || self.std(|d| d.send_to(buf, &t)))
        })
        .await
    }

    pub fn try_send_to<P: AsRef<std::path::Path>>(
        &self,
        buf: &[u8],
        target: P,
    ) -> io::Result<usize> {
        self.std(|d| d.send_to(buf, target))
    }

    pub async fn recv_from(
        &self,
        buf: &mut [u8],
    ) -> io::Result<(usize, std::os::unix::net::SocketAddr)> {
        poll_fn(|cx| {
            self.io
                .poll_io(false, cx, || self.std(|d| d.recv_from(&mut *buf)))
        })
        .await
    }

    pub fn try_recv_from(
        &self,
        buf: &mut [u8],
    ) -> io::Result<(usize, std::os::unix::net::SocketAddr)> {
        self.std(|d| d.recv_from(buf))
    }

    pub fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<std::os::unix::net::SocketAddr>> {
        let (n, a) = ready!(self.io.poll_io(false, cx, || self
            .std(|d| d.recv_from(buf.initialize_unfilled()))))?;
        buf.advance(n);
        Poll::Ready(Ok(a))
    }

    pub async fn readable(&self) -> io::Result<()> {
        poll_fn(|cx| self.io.poll_read_ready_nocoop(cx)).await
    }

    pub async fn writable(&self) -> io::Result<()> {
        poll_fn(|cx| self.io.poll_write_ready_nocoop(cx)).await
    }

    pub fn local_addr(&self) -> io::Result<std::os::unix::net::SocketAddr> {
        self.std(|d| d.local_addr())
    }

    pub fn peer_addr(&self) -> io::Result<std::os::unix::net::SocketAddr> {
        self.std(|d| d.peer_addr())
    }

    pub fn shutdown(&self, how: std::net::Shutdown) -> io::Result<()> {
        self.std(|d| d.shutdown(how))
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        sock_err(self.io.raw())
    }
}

impl AsRawFd for UnixDatagram {
    fn as_raw_fd(&self) -> RawFd {
        self.io.raw()
    }
}

/// `tokio::net::UnixSocket`（bind・設定してから listen / connect / datagram にする）。
#[derive(Debug)]
pub struct UnixSocket {
    fd: OwnedFd,
    datagram: bool,
}

fn sockaddr_un(path: &std::path::Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let p = path.as_os_str().as_encoded_bytes();
    let mut sun: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if p.len() >= sun.sun_path.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "path too long"));
    }
    sun.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (d, s) in sun.sun_path.iter_mut().zip(p) {
        *d = *s as libc::c_char;
    }
    let len = (std::mem::size_of::<libc::sa_family_t>() + p.len() + 1) as libc::socklen_t;
    Ok((sun, len))
}

impl UnixSocket {
    pub fn new_stream() -> io::Result<UnixSocket> {
        Ok(UnixSocket {
            fd: new_socket(libc::AF_UNIX, libc::SOCK_STREAM)?,
            datagram: false,
        })
    }

    pub fn new_datagram() -> io::Result<UnixSocket> {
        Ok(UnixSocket {
            fd: new_socket(libc::AF_UNIX, libc::SOCK_DGRAM)?,
            datagram: true,
        })
    }

    pub fn bind<P: AsRef<std::path::Path>>(&self, path: P) -> io::Result<()> {
        let (sun, len) = sockaddr_un(path.as_ref())?;
        cvt_i(unsafe {
            libc::bind(
                self.fd.as_raw_fd(),
                &sun as *const _ as *const libc::sockaddr,
                len,
            )
        })
        .map(drop)
    }

    pub fn listen(self, backlog: u32) -> io::Result<UnixListener> {
        if self.datagram {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "listen cannot be called on a datagram socket",
            ));
        }
        cvt_i(unsafe {
            libc::listen(
                self.fd.as_raw_fd(),
                backlog.min(i32::MAX as u32) as libc::c_int,
            )
        })?;
        Ok(UnixListener {
            io: IoSource::new(self.fd),
        })
    }

    pub async fn connect<P: AsRef<std::path::Path>>(self, path: P) -> io::Result<UnixStream> {
        if self.datagram {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "connect cannot be called on a datagram socket",
            ));
        }
        let (sun, len) = sockaddr_un(path.as_ref())?;
        let r = unsafe {
            libc::connect(
                self.fd.as_raw_fd(),
                &sun as *const _ as *const libc::sockaddr,
                len,
            )
        };
        let s = TcpStream {
            io: Arc::new(IoSource::new(self.fd)),
        };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINPROGRESS) && e.kind() != io::ErrorKind::WouldBlock
            {
                return Err(e);
            }
        }
        // tokio と同じく、すぐ繋がっても一度ランタイムの準備待ちを通して SO_ERROR を見る
        poll_fn(|cx| s.io.poll_write_ready(cx)).await?;
        if let Some(e) = sock_err(s.io.raw())? {
            return Err(e);
        }
        Ok(UnixStream { inner: s })
    }

    pub fn datagram(self) -> io::Result<UnixDatagram> {
        if !self.datagram {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "datagram cannot be called on a stream socket",
            ));
        }
        Ok(UnixDatagram {
            io: IoSource::new(self.fd),
        })
    }
}

impl AsRawFd for UnixSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

// ===================== AsyncFd（io::unix） =====================

/// 任意の fd の準備待ち（`tokio::io::unix::AsyncFd`）。準備の状態は「待ちが完了した」ことを覚え、
/// `clear_ready` か `try_io` の `WouldBlock` で忘れる。
pub struct AsyncFd<T: AsRawFd> {
    inner: Option<T>,
    io: IoSourceRef,
    interest: Interest,
    read_ready: std::sync::atomic::AtomicBool,
    write_ready: std::sync::atomic::AtomicBool,
    /// `WouldBlock` で忘れた（tokio はエッジで覚えるので、次の準備待ちが完了するまで poll(2) で見直さない）
    read_cleared: std::sync::atomic::AtomicBool,
    write_cleared: std::sync::atomic::AtomicBool,
}

/// fd を所有しない準備待ち（AsyncFd 用）。
struct IoSourceRef {
    fd: RawFd,
    read: Mutex<WaitSlot>,
    write: Mutex<WaitSlot>,
}

impl IoSourceRef {
    /// `single` は `poll_*_ready`（最後の Waker だけを覚える）。
    fn poll_ready(&self, write: bool, single: bool, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // tokio と同じく、ランタイムが止まった後の準備待ちはエラー
        if !crate::runtime::has_driver() {
            return Poll::Ready(Err(io::Error::other(
                "A Tokio 1.x context was found, but it is being shutdown.",
            )));
        }
        // 予算を使うのは poll_*_ready だけ（ready()・readable() は tokio と同じく使わない）
        if single {
            ready!(crate::tokio::task::coop::poll_proceed_and_make_progress(cx));
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
        let fd = self.fd;
        if single {
            slot.waiters.set_last(cx.waker());
        } else {
            slot.waiters.add(cx.waker());
        }
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
        Poll::Ready(r)
    }
}

impl Drop for IoSourceRef {
    fn drop(&mut self) {
        self.read.get_mut().unwrap().clear();
        self.write.get_mut().unwrap().clear();
        #[cfg(veil_rt_reactor)]
        if crate::runtime::has_driver() && !crate::tokio::waker::exiting() {
            crate::runtime::executor::unregister(self.fd);
        }
    }
}

/// `AsyncFd::try_new` の失敗（元の値を返す）。
pub struct AsyncFdTryNewError<T> {
    inner: T,
    cause: io::Error,
}

impl<T> AsyncFdTryNewError<T> {
    pub fn into_parts(self) -> (T, io::Error) {
        (self.inner, self.cause)
    }
}

impl<T> std::fmt::Debug for AsyncFdTryNewError<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.cause.fmt(f)
    }
}

impl<T> std::fmt::Display for AsyncFdTryNewError<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.cause.fmt(f)
    }
}

impl<T> std::error::Error for AsyncFdTryNewError<T> {}

/// `try_io` で `WouldBlock` だった。
#[derive(Debug)]
pub struct TryIoError(());

impl<T: AsRawFd> std::fmt::Debug for AsyncFd<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncFd").field("fd", &self.io.fd).finish()
    }
}

impl<T: AsRawFd> AsyncFd<T> {
    #[track_caller]
    pub fn new(inner: T) -> io::Result<AsyncFd<T>> {
        AsyncFd::with_interest(inner, Interest::READABLE | Interest::WRITABLE)
    }

    #[track_caller]
    pub fn with_interest(inner: T, interest: Interest) -> io::Result<AsyncFd<T>> {
        AsyncFd::try_with_interest(inner, interest).map_err(|e| e.cause)
    }

    #[track_caller]
    pub fn try_new(inner: T) -> Result<AsyncFd<T>, AsyncFdTryNewError<T>> {
        AsyncFd::try_with_interest(inner, Interest::READABLE | Interest::WRITABLE)
    }

    #[track_caller]
    pub fn try_with_interest(
        inner: T,
        interest: Interest,
    ) -> Result<AsyncFd<T>, AsyncFdTryNewError<T>> {
        check_io_context();
        let fd = inner.as_raw_fd();
        // 登録できない fd（閉じている・-1）は tokio と同じく作成時に失敗させる
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return Err(AsyncFdTryNewError {
                inner,
                cause: io::Error::last_os_error(),
            });
        }
        Ok(AsyncFd {
            inner: Some(inner),
            io: IoSourceRef {
                fd,
                read: Mutex::new(WaitSlot::default()),
                write: Mutex::new(WaitSlot::default()),
            },
            interest,
            read_ready: std::sync::atomic::AtomicBool::new(false),
            write_ready: std::sync::atomic::AtomicBool::new(false),
            read_cleared: std::sync::atomic::AtomicBool::new(false),
            write_cleared: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn get_ref(&self) -> &T {
        self.inner.as_ref().unwrap()
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.inner.as_mut().unwrap()
    }

    pub fn into_inner(mut self) -> T {
        self.inner.take().unwrap()
    }

    pub fn interest(&self) -> Interest {
        self.interest
    }

    fn flag(&self, write: bool) -> &std::sync::atomic::AtomicBool {
        if write {
            &self.write_ready
        } else {
            &self.read_ready
        }
    }

    fn poll_flag(&self, write: bool, single: bool, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.flag(write).load(std::sync::atomic::Ordering::Acquire) {
            return Poll::Ready(Ok(()));
        }
        ready!(self.io.poll_ready(write, single, cx))?;
        self.flag(write)
            .store(true, std::sync::atomic::Ordering::Release);
        let cleared = if write {
            &self.write_cleared
        } else {
            &self.read_cleared
        };
        cleared.store(false, std::sync::atomic::Ordering::Release);
        Poll::Ready(Ok(()))
    }

    pub fn poll_read_ready<'a>(
        &'a self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<AsyncFdReadyGuard<'a, T>>> {
        ready!(self.poll_flag(false, true, cx))?;
        Poll::Ready(Ok(AsyncFdReadyGuard {
            fd: self,
            ready: Ready::READABLE,
        }))
    }

    pub fn poll_write_ready<'a>(
        &'a self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<AsyncFdReadyGuard<'a, T>>> {
        ready!(self.poll_flag(true, true, cx))?;
        Poll::Ready(Ok(AsyncFdReadyGuard {
            fd: self,
            ready: Ready::WRITABLE,
        }))
    }

    pub fn poll_read_ready_mut<'a>(
        &'a mut self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<AsyncFdReadyMutGuard<'a, T>>> {
        ready!(self.poll_flag(false, true, cx))?;
        Poll::Ready(Ok(AsyncFdReadyMutGuard {
            fd: self,
            ready: Ready::READABLE,
        }))
    }

    pub fn poll_write_ready_mut<'a>(
        &'a mut self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<AsyncFdReadyMutGuard<'a, T>>> {
        ready!(self.poll_flag(true, true, cx))?;
        Poll::Ready(Ok(AsyncFdReadyMutGuard {
            fd: self,
            ready: Ready::WRITABLE,
        }))
    }

    fn poll_ready_interest(
        &self,
        interest: Interest,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<Ready>> {
        let mut ready = Ready::EMPTY;
        if interest.is_readable() {
            if let Poll::Ready(r) = self.poll_flag(false, false, cx) {
                r?;
                ready |= Ready::READABLE;
            }
        }
        if interest.is_writable() {
            if let Poll::Ready(r) = self.poll_flag(true, false, cx) {
                r?;
                ready |= Ready::WRITABLE;
            }
        }
        if ready.is_empty() {
            Poll::Pending
        } else {
            Poll::Ready(Ok(ready))
        }
    }

    pub async fn ready(&self, interest: Interest) -> io::Result<AsyncFdReadyGuard<'_, T>> {
        let ready = poll_fn(|cx| self.poll_ready_interest(interest, cx)).await?;
        Ok(AsyncFdReadyGuard { fd: self, ready })
    }

    pub async fn ready_mut(
        &mut self,
        interest: Interest,
    ) -> io::Result<AsyncFdReadyMutGuard<'_, T>> {
        let ready = poll_fn(|cx| self.poll_ready_interest(interest, cx)).await?;
        Ok(AsyncFdReadyMutGuard { fd: self, ready })
    }

    pub async fn readable(&self) -> io::Result<AsyncFdReadyGuard<'_, T>> {
        self.ready(Interest::READABLE).await
    }

    pub async fn readable_mut(&mut self) -> io::Result<AsyncFdReadyMutGuard<'_, T>> {
        self.ready_mut(Interest::READABLE).await
    }

    pub async fn writable(&self) -> io::Result<AsyncFdReadyGuard<'_, T>> {
        self.ready(Interest::WRITABLE).await
    }

    pub async fn writable_mut(&mut self) -> io::Result<AsyncFdReadyMutGuard<'_, T>> {
        self.ready_mut(Interest::WRITABLE).await
    }

    /// 準備を待って `f` を呼ぶ（`WouldBlock` なら準備を忘れて待ち直す）。
    pub async fn async_io<R>(
        &self,
        interest: Interest,
        mut f: impl FnMut(&T) -> io::Result<R>,
    ) -> io::Result<R> {
        loop {
            let mut guard = self.ready(interest).await?;
            match guard.try_io(|fd| f(fd.get_ref())) {
                Ok(r) => return r,
                Err(_would_block) => continue,
            }
        }
    }

    pub async fn async_io_mut<R>(
        &mut self,
        interest: Interest,
        mut f: impl FnMut(&mut T) -> io::Result<R>,
    ) -> io::Result<R> {
        loop {
            let mut guard = self.ready_mut(interest).await?;
            match guard.try_io(|fd| f(fd.get_mut())) {
                Ok(r) => return r,
                Err(_would_block) => continue,
            }
        }
    }

    /// 準備を待たずに `f` を呼ぶ（`WouldBlock` なら準備を忘れる）。
    /// 準備できているか（覚えていなければ poll(2) で今の状態を見る）。
    fn probe_ready(&self, interest: Interest) -> bool {
        let mut ready = (interest.is_readable()
            && self.read_ready.load(std::sync::atomic::Ordering::Acquire))
            || (interest.is_writable()
                && self.write_ready.load(std::sync::atomic::Ordering::Acquire));
        if !ready {
            use std::sync::atomic::Ordering::Acquire;
            let mut events = 0;
            if interest.is_readable() && !self.read_cleared.load(Acquire) {
                events |= libc::POLLIN;
            }
            if interest.is_writable() && !self.write_cleared.load(Acquire) {
                events |= libc::POLLOUT;
            }
            if events == 0 {
                return false;
            }
            let mut pfd = libc::pollfd {
                fd: self.io.fd,
                events,
                revents: 0,
            };
            if unsafe { libc::poll(&mut pfd, 1, 0) } > 0 {
                let err = pfd.revents & (libc::POLLERR | libc::POLLHUP) != 0;
                if pfd.revents & libc::POLLIN != 0 || (err && interest.is_readable()) {
                    self.read_ready
                        .store(true, std::sync::atomic::Ordering::Release);
                    ready = true;
                }
                if pfd.revents & libc::POLLOUT != 0 || (err && interest.is_writable()) {
                    self.write_ready
                        .store(true, std::sync::atomic::Ordering::Release);
                    ready = true;
                }
            }
        }
        ready
    }

    /// 準備できていれば `f` を呼ぶ（できていなければ呼ばずに `WouldBlock`。`WouldBlock` なら準備を忘れる）。
    pub fn try_io_mut<R>(
        &mut self,
        interest: Interest,
        f: impl FnOnce(&mut T) -> io::Result<R>,
    ) -> io::Result<R> {
        if !self.probe_ready(interest) {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let r = f(self.inner.as_mut().unwrap());
        if matches!(&r, Err(e) if e.kind() == io::ErrorKind::WouldBlock) {
            self.clear_interest(interest);
        }
        r
    }

    pub fn try_io<R>(
        &self,
        interest: Interest,
        f: impl FnOnce(&T) -> io::Result<R>,
    ) -> io::Result<R> {
        if !self.probe_ready(interest) {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let r = f(self.get_ref());
        if matches!(&r, Err(e) if e.kind() == io::ErrorKind::WouldBlock) {
            self.clear_interest(interest);
        }
        r
    }

    fn clear_interest(&self, interest: Interest) {
        let mut r = Ready::EMPTY;
        if interest.is_readable() {
            r |= Ready::READABLE;
        }
        if interest.is_writable() {
            r |= Ready::WRITABLE;
        }
        self.clear(r);
    }

    fn clear(&self, ready: Ready) {
        use std::sync::atomic::Ordering::Release;
        if ready.is_readable() {
            self.read_ready.store(false, Release);
            self.read_cleared.store(true, Release);
        }
        if ready.is_writable() {
            self.write_ready.store(false, Release);
            self.write_cleared.store(true, Release);
        }
    }
}

impl<T: AsRawFd> AsRawFd for AsyncFd<T> {
    fn as_raw_fd(&self) -> RawFd {
        self.io.fd
    }
}

/// 準備ができたことを示すガード。
pub struct AsyncFdReadyGuard<'a, T: AsRawFd> {
    fd: &'a AsyncFd<T>,
    ready: Ready,
}

impl<T: AsRawFd> std::fmt::Debug for AsyncFdReadyGuard<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncFdReadyGuard")
            .field("ready", &self.ready)
            .finish()
    }
}

impl<'a, T: AsRawFd> AsyncFdReadyGuard<'a, T> {
    pub fn clear_ready(&mut self) {
        self.fd.clear(self.ready);
    }

    pub fn clear_ready_matching(&mut self, ready: Ready) {
        self.fd.clear(ready & self.ready);
        self.ready = self.ready - ready;
    }

    pub fn retain_ready(&mut self) {}

    pub fn ready(&self) -> Ready {
        self.ready
    }

    pub fn try_io<R>(
        &mut self,
        f: impl FnOnce(&'a AsyncFd<T>) -> io::Result<R>,
    ) -> Result<io::Result<R>, TryIoError> {
        match f(self.fd) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.clear_ready();
                Err(TryIoError(()))
            }
            r => Ok(r),
        }
    }

    pub fn get_ref(&self) -> &'a AsyncFd<T> {
        self.fd
    }

    pub fn get_inner(&self) -> &'a T {
        self.fd.get_ref()
    }
}

/// 準備ができたことを示すガード（可変）。
pub struct AsyncFdReadyMutGuard<'a, T: AsRawFd> {
    fd: &'a mut AsyncFd<T>,
    ready: Ready,
}

impl<T: AsRawFd> std::fmt::Debug for AsyncFdReadyMutGuard<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncFdReadyMutGuard")
            .field("ready", &self.ready)
            .finish()
    }
}

impl<T: AsRawFd> AsyncFdReadyMutGuard<'_, T> {
    pub fn clear_ready(&mut self) {
        self.fd.clear(self.ready);
    }

    pub fn clear_ready_matching(&mut self, ready: Ready) {
        self.fd.clear(ready & self.ready);
        self.ready = self.ready - ready;
    }

    pub fn retain_ready(&mut self) {}

    pub fn ready(&self) -> Ready {
        self.ready
    }

    pub fn try_io<R>(
        &mut self,
        f: impl FnOnce(&mut AsyncFd<T>) -> io::Result<R>,
    ) -> Result<io::Result<R>, TryIoError> {
        match f(self.fd) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.clear_ready();
                Err(TryIoError(()))
            }
            r => Ok(r),
        }
    }

    pub fn get_ref(&self) -> &AsyncFd<T> {
        self.fd
    }

    pub fn get_mut(&mut self) -> &mut AsyncFd<T> {
        self.fd
    }

    pub fn get_inner(&self) -> &T {
        self.fd.get_ref()
    }

    pub fn get_inner_mut(&mut self) -> &mut T {
        self.fd.get_mut()
    }
}

impl AsRawFd for UnixListener {
    fn as_raw_fd(&self) -> RawFd {
        self.io.raw()
    }
}

/// `tokio::net::unix`（分割した半分の型は TCP と共通）。
pub mod unix {
    pub use super::tcp::{OwnedReadHalf, OwnedWriteHalf, ReadHalf, ReuniteError, WriteHalf};
    pub use super::UCred;
    pub use std::os::unix::net::SocketAddr;
}
