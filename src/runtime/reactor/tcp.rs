//! 非同期 TcpListener / TcpStream（reactor バックエンド）
//!
//! `runtime::uring::tcp` と同一の公開 API（型・メソッドシグネチャ）を提供する。
//! 実装方式は「try-first」: まず非ブロッキング syscall（`accept4`/`read`/`write`/
//! `sendmsg`/`connect`）を試し、`EAGAIN` なら oneshot readiness を登録して `Pending` を
//! 返し、起床後に再試行する。
//!
//! io_uring 版と異なり、readiness モデルではカーネルが Future 保有のバッファを非同期に
//! 参照し続けることが構造的に無い（syscall は Future 起床後に同期実行する）ため、
//! `OpGuard`/detach に相当する後始末機構は不要である。各 Future の `Drop` は特別な処理を
//! 必要としない（登録済み Waker を残しても、後続の再登録で上書きされるか、無関係になった
//! タスクへの無害な spurious wake になるのみ）。

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::os::unix::io::{AsRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::runtime::buf::{IoBuf, IoBufMut};
use crate::runtime::executor::{register_read, register_write, unregister};

// SO_* ソケットオプション
const TCP_NODELAY: libc::c_int = 1;

// ====================
// ソケットアドレス変換ユーティリティ（uring 版と同一実装）
// ====================

fn sockaddr_to_storage(addr: &SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let len = match addr {
        SocketAddr::V4(v4) => {
            let sin = unsafe { &mut *(&mut storage as *mut _ as *mut libc::sockaddr_in) };
            sin.sin_family = libc::AF_INET as _;
            sin.sin_port = v4.port().to_be();
            sin.sin_addr.s_addr = u32::from_ne_bytes(v4.ip().octets());
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t
        }
        SocketAddr::V6(v6) => {
            let sin6 = unsafe { &mut *(&mut storage as *mut _ as *mut libc::sockaddr_in6) };
            sin6.sin6_family = libc::AF_INET6 as _;
            sin6.sin6_port = v6.port().to_be();
            sin6.sin6_addr.s6_addr = v6.ip().octets();
            sin6.sin6_flowinfo = v6.flowinfo();
            sin6.sin6_scope_id = v6.scope_id();
            std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t
        }
    };
    (storage, len)
}

fn storage_to_sockaddr(storage: &libc::sockaddr_storage) -> io::Result<SocketAddr> {
    match storage.ss_family as libc::c_int {
        libc::AF_INET => {
            let sin = unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
            let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            let port = u16::from_be(sin.sin_port);
            Ok(SocketAddr::V4(std::net::SocketAddrV4::new(ip, port)))
        }
        libc::AF_INET6 => {
            let sin6 = unsafe { &*(storage as *const _ as *const libc::sockaddr_in6) };
            let ip = std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            let port = u16::from_be(sin6.sin6_port);
            Ok(SocketAddr::V6(std::net::SocketAddrV6::new(
                ip,
                port,
                sin6.sin6_flowinfo,
                sin6.sin6_scope_id,
            )))
        }
        family => Err(io::Error::other(format!(
            "unsupported address family: {}",
            family
        ))),
    }
}

/// ノンブロッキングソケットを作成する（`O_NONBLOCK | O_CLOEXEC`）。
fn create_nonblocking_socket(domain: libc::c_int) -> io::Result<RawFd> {
    let fd = unsafe {
        libc::socket(
            domain,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// `EAGAIN`/`EWOULDBLOCK` か判定する。
#[inline]
fn is_would_block(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::WouldBlock
}

// ====================
// TcpListener
// ====================

/// 非同期 TCP リスナー。
pub struct TcpListener {
    fd: RawFd,
}

impl TcpListener {
    /// アドレスにバインドしてリッスンを開始する。
    pub fn bind(addr: impl std::net::ToSocketAddrs) -> io::Result<Self> {
        Self::bind_impl(addr, false)
    }

    /// SO_REUSEPORT を設定してバインドする。
    pub fn bind_reuse_port(addr: impl std::net::ToSocketAddrs) -> io::Result<Self> {
        Self::bind_impl(addr, true)
    }

    fn bind_impl(addr: impl std::net::ToSocketAddrs, reuse_port: bool) -> io::Result<Self> {
        let addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address"))?;

        let domain = if addr.is_ipv6() {
            libc::AF_INET6
        } else {
            libc::AF_INET
        };
        let fd = create_nonblocking_socket(domain)?;

        let optval: libc::c_int = 1;
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
            if reuse_port {
                // FreeBSD の `SO_REUSEPORT` は複数 bind を許すのみでカーネル分散を行わない
                // （Linux の `SO_REUSEPORT` 相当の負荷分散は `SO_REUSEPORT_LB`、FreeBSD 12+）。
                // thread-per-core の accept 分散を成立させるには LB 版が必要なため、
                // FreeBSD では `SO_REUSEPORT_LB` を使う（設計ドキュメント 3.3 節）。
                #[cfg(target_os = "freebsd")]
                let reuseport_opt = libc::SO_REUSEPORT_LB;
                #[cfg(not(target_os = "freebsd"))]
                let reuseport_opt = libc::SO_REUSEPORT;
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    reuseport_opt,
                    &optval as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
        }

        let (storage, len) = sockaddr_to_storage(&addr);
        let ret = unsafe { libc::bind(fd, &storage as *const _ as *const libc::sockaddr, len) };
        if ret < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }

        let ret = unsafe { libc::listen(fd, 1024) };
        if ret < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }

        Ok(Self { fd })
    }

    /// 新しい接続を非同期で受け入れる。
    pub fn accept(&self) -> Accept<'_> {
        Accept {
            listener_fd: self.fd,
            _marker: std::marker::PhantomData,
        }
    }

    /// ローカルアドレスを取得する。
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let ret = unsafe {
            libc::getsockname(
                self.fd,
                &mut storage as *mut _ as *mut libc::sockaddr,
                &mut len,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        storage_to_sockaddr(&storage)
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        unregister(self.fd);
        unsafe { libc::close(self.fd) };
    }
}

impl AsRawFd for TcpListener {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

// ====================
// Accept Future
// ====================

/// accept Future（`accept4` の try-first ラッパ）。
pub struct Accept<'a> {
    listener_fd: RawFd,
    _marker: std::marker::PhantomData<&'a TcpListener>,
}

impl<'a> Future for Accept<'a> {
    type Output = io::Result<(TcpStream, SocketAddr)>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
            let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            let fd = unsafe {
                libc::accept4(
                    self.listener_fd,
                    &mut storage as *mut _ as *mut libc::sockaddr,
                    &mut len,
                    libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                )
            };
            if fd >= 0 {
                // 先に TcpStream を構築する: アドレス変換が失敗しても Drop 経由で
                // fd がクローズされ、リークしない。
                let stream = TcpStream { fd };
                let peer_addr = storage_to_sockaddr(&storage)?;
                return Poll::Ready(Ok((stream, peer_addr)));
            }
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if is_would_block(&e) {
                register_read(self.listener_fd, cx.waker().clone());
                return Poll::Pending;
            }
            return Poll::Ready(Err(e));
        }
    }
}

// ====================
// TcpStream
// ====================

/// 非同期 TCP ストリーム。
pub struct TcpStream {
    pub(crate) fd: RawFd,
}

impl TcpStream {
    /// raw fd から作成する。
    ///
    /// # Safety
    /// `fd` は有効な非ブロッキングソケット FD であること。
    pub unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Self { fd }
    }

    /// アドレスに非同期で接続する。
    pub fn connect(addr: SocketAddr) -> Connect {
        Connect {
            addr,
            fd: -1,
            registered: false,
        }
    }

    /// 文字列アドレス（"host:port"）から接続する。
    ///
    /// DNS 解決はブロッキングで行う（コールドパスのみ）。
    pub async fn connect_str(addr: &str) -> io::Result<TcpStream> {
        use std::net::ToSocketAddrs;
        let socket_addr = addr
            .to_socket_addrs()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address resolved"))?;
        TcpStream::connect(socket_addr).await
    }

    /// バッファに非同期で読み込む。バッファの所有権を取り、完了時に `(Result<usize>, T)` を返す。
    pub fn read<T: IoBufMut>(&self, buf: T) -> ReadFuture<T> {
        ReadFuture {
            fd: self.fd,
            buf: Some(buf),
        }
    }

    /// バッファを非同期で書き込む。バッファの所有権を取り、完了時に `(Result<usize>, T)` を返す。
    pub fn write<T: IoBuf>(&self, buf: T) -> WriteFuture<T> {
        WriteFuture {
            fd: self.fd,
            buf: Some(buf),
        }
    }

    /// 2 つの不連続バッファを 1 回の `sendmsg`（scatter-gather）で書き込む（F-59 互換）。
    pub fn writev2<A: IoBuf, B: IoBuf>(&self, a: A, b: B, skip: usize) -> SendMsgFuture<A, B> {
        SendMsgFuture {
            fd: self.fd,
            bufs: Some((a, b)),
            skip,
        }
    }

    /// 2 つの不連続バッファを全量書き込む。
    pub async fn write_all_vectored<A: IoBuf, B: IoBuf>(
        &self,
        a: A,
        b: B,
    ) -> (io::Result<()>, A, B) {
        let total = a.bytes_init() + b.bytes_init();
        let mut sent = 0usize;
        let (mut a, mut b) = (a, b);
        while sent < total {
            let (res, ra, rb) = self.writev2(a, b, sent).await;
            a = ra;
            b = rb;
            match res {
                Ok(0) => {
                    return (
                        Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "sendmsg returned zero",
                        )),
                        a,
                        b,
                    );
                }
                Ok(n) => sent += n,
                Err(e) => return (Err(e), a, b),
            }
        }
        (Ok(()), a, b)
    }

    /// 読み取り可能になるまで待つ。
    pub fn readable(&self) -> Readable<'_> {
        Readable {
            fd: self.fd,
            _marker: std::marker::PhantomData,
        }
    }

    /// 書き込み可能になるまで待つ。
    pub fn writable(&self) -> Writable<'_> {
        Writable {
            fd: self.fd,
            _marker: std::marker::PhantomData,
        }
    }

    /// TCP_NODELAY を設定する。
    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        let optval: libc::c_int = if nodelay { 1 } else { 0 };
        let ret = unsafe {
            libc::setsockopt(
                self.fd,
                libc::IPPROTO_TCP,
                TCP_NODELAY,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// ピアアドレスを取得する。
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let ret = unsafe {
            libc::getpeername(
                self.fd,
                &mut storage as *mut _ as *mut libc::sockaddr,
                &mut len,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        storage_to_sockaddr(&storage)
    }

    /// ローカルアドレスを取得する。
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let ret = unsafe {
            libc::getsockname(
                self.fd,
                &mut storage as *mut _ as *mut libc::sockaddr,
                &mut len,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        storage_to_sockaddr(&storage)
    }

    /// ソケットをシャットダウンする。
    pub fn shutdown(&self, how: std::net::Shutdown) -> io::Result<()> {
        let how = match how {
            std::net::Shutdown::Read => libc::SHUT_RD,
            std::net::Shutdown::Write => libc::SHUT_WR,
            std::net::Shutdown::Both => libc::SHUT_RDWR,
        };
        let ret = unsafe { libc::shutdown(self.fd, how) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

// ====================
// AsyncReadRent / AsyncWriteRent 実装
// ====================

impl crate::runtime::io::AsyncReadRent for TcpStream {
    fn read<T: crate::runtime::buf::IoBufMut>(
        &mut self,
        buf: T,
    ) -> impl std::future::Future<Output = crate::runtime::io::BufResult<usize, T>> {
        let fd = self.fd;
        ReadFuture { fd, buf: Some(buf) }
    }
}

impl crate::runtime::io::AsyncWriteRent for TcpStream {
    fn write<T: crate::runtime::buf::IoBuf>(
        &mut self,
        buf: T,
    ) -> impl std::future::Future<Output = crate::runtime::io::BufResult<usize, T>> {
        let fd = self.fd;
        WriteFuture { fd, buf: Some(buf) }
    }

    fn shutdown(&mut self) -> impl std::future::Future<Output = std::io::Result<()>> {
        let result = TcpStream::shutdown(self, std::net::Shutdown::Both);
        async move { result }
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unregister(self.fd);
            unsafe { libc::close(self.fd) };
        }
    }
}

impl AsRawFd for TcpStream {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

// ====================
// Connect Future
// ====================

/// connect Future（非ブロッキング `connect(2)` → writable 待ち → `SO_ERROR` 確認）。
pub struct Connect {
    addr: SocketAddr,
    fd: RawFd,
    registered: bool,
}

impl Connect {
    /// 接続失敗時の後始末: 登録済みなら FdTable から除去してから close する。
    ///
    /// unregister を省くと閉じた fd の FdRecord（known_to_kernel/armed）が残り、
    /// OS が同じ fd 番号を再利用した際に stale な状態を引き継いでしまう。
    fn fail(&mut self, e: io::Error) -> io::Error {
        let fd = self.fd;
        self.fd = -1;
        if self.registered {
            unregister(fd);
            self.registered = false;
        }
        unsafe { libc::close(fd) };
        e
    }
}

impl Future for Connect {
    type Output = io::Result<TcpStream>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.fd < 0 {
            let domain = if self.addr.is_ipv6() {
                libc::AF_INET6
            } else {
                libc::AF_INET
            };
            let fd = match create_nonblocking_socket(domain) {
                Ok(fd) => fd,
                Err(e) => return Poll::Ready(Err(e)),
            };
            self.fd = fd;

            let (storage, len) = sockaddr_to_storage(&self.addr);
            let ret =
                unsafe { libc::connect(fd, &storage as *const _ as *const libc::sockaddr, len) };
            if ret == 0 {
                // 即座に接続完了（ローカルソケット等）。
                let fd = self.fd;
                self.fd = -1;
                return Poll::Ready(Ok(TcpStream { fd }));
            }
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINPROGRESS) {
                unsafe { libc::close(fd) };
                self.fd = -1;
                return Poll::Ready(Err(e));
            }
        }

        // 接続完了（POLLOUT/エラー）を poll(2) で確認してから SO_ERROR を読む。
        //
        // 「一度 register 済みなら起床＝完了」と見なしてはならない: タスクの起床は
        // この fd の writable イベント以外でも起こる（例: `timeout(CONNECT_TIMEOUT,
        // connect)` は select 系のためタイマー起床時にも内側の Connect を再 poll する）。
        // 接続未完了のソケットは SO_ERROR が 0 を返すため、readiness を確認せずに
        // SO_ERROR だけ見ると「未接続ソケットを接続成功として返す」誤判定になる。
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
        if ret < 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Poll::Ready(Err(self.fail(e)));
            }
            // EINTR: readiness 未確定として待機継続する。
        }
        if ret <= 0 || pfd.revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) == 0 {
            // 接続未完了: writable 待ちを（再）登録して待機する。
            register_write(self.fd, cx.waker().clone());
            self.registered = true;
            return Poll::Pending;
        }

        // 接続完了（または失敗）: SO_ERROR で接続結果を確認する。
        let mut err: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let ret = unsafe {
            libc::getsockopt(
                self.fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &mut err as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if ret < 0 {
            let e = io::Error::last_os_error();
            return Poll::Ready(Err(self.fail(e)));
        }
        if err != 0 {
            let e = io::Error::from_raw_os_error(err);
            return Poll::Ready(Err(self.fail(e)));
        }

        let fd = self.fd;
        self.fd = -1;
        Poll::Ready(Ok(TcpStream { fd }))
    }
}

impl Drop for Connect {
    fn drop(&mut self) {
        if self.fd >= 0 {
            if self.registered {
                unregister(self.fd);
            }
            unsafe { libc::close(self.fd) };
        }
    }
}

// ====================
// Read Future
// ====================

/// 読み込み Future（非ブロッキング `read(2)` の try-first ラッパ）。
pub struct ReadFuture<T: IoBufMut> {
    fd: RawFd,
    buf: Option<T>,
}

impl<T: IoBufMut> Future for ReadFuture<T> {
    type Output = (io::Result<usize>, T);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        loop {
            let buf = this
                .buf
                .as_mut()
                .expect("ReadFuture polled after completion");
            let ret = unsafe {
                libc::read(
                    this.fd,
                    buf.write_ptr() as *mut libc::c_void,
                    buf.bytes_total(),
                )
            };
            if ret >= 0 {
                // SAFETY: カーネルが ret バイトを初期化した。
                unsafe { buf.set_init(ret as usize) };
                let buf = this.buf.take().expect("buffer present at completion");
                return Poll::Ready((Ok(ret as usize), buf));
            }
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if is_would_block(&e) {
                register_read(this.fd, cx.waker().clone());
                return Poll::Pending;
            }
            let buf = this.buf.take().expect("buffer present on error");
            return Poll::Ready((Err(e), buf));
        }
    }
}

// ====================
// Write Future
// ====================

/// 書き込み Future（非ブロッキング `write(2)` の try-first ラッパ）。
pub struct WriteFuture<T: IoBuf> {
    fd: RawFd,
    buf: Option<T>,
}

impl<T: IoBuf> Future for WriteFuture<T> {
    type Output = (io::Result<usize>, T);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        loop {
            let buf = this
                .buf
                .as_ref()
                .expect("WriteFuture polled after completion");
            let ret = unsafe {
                libc::send(
                    this.fd,
                    buf.read_ptr() as *const libc::c_void,
                    buf.bytes_init(),
                    libc::MSG_NOSIGNAL,
                )
            };
            if ret >= 0 {
                let buf = this.buf.take().expect("buffer present at completion");
                return Poll::Ready((Ok(ret as usize), buf));
            }
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if is_would_block(&e) {
                register_write(this.fd, cx.waker().clone());
                return Poll::Pending;
            }
            let buf = this.buf.take().expect("buffer present on error");
            return Poll::Ready((Err(e), buf));
        }
    }
}

// ====================
// SendMsg (scatter-gather) Future（F-59 互換）
// ====================

/// scatter-gather 書き込み Future（`sendmsg(2)` の try-first ラッパ）。
pub struct SendMsgFuture<A: IoBuf, B: IoBuf> {
    fd: RawFd,
    bufs: Option<(A, B)>,
    skip: usize,
}

impl<A: IoBuf, B: IoBuf> Future for SendMsgFuture<A, B> {
    type Output = (io::Result<usize>, A, B);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        loop {
            let (a, b) = this
                .bufs
                .as_ref()
                .expect("SendMsgFuture polled after completion");
            let (a_ptr, a_len) = (a.read_ptr(), a.bytes_init());
            let (b_ptr, b_len) = (b.read_ptr(), b.bytes_init());
            let skip = this.skip;
            debug_assert!(skip < a_len + b_len);

            let mut iovecs = [libc::iovec {
                iov_base: std::ptr::null_mut(),
                iov_len: 0,
            }; 2];
            let mut iov_count = 0usize;
            if skip < a_len {
                iovecs[iov_count] = libc::iovec {
                    iov_base: unsafe { a_ptr.add(skip) } as *mut libc::c_void,
                    iov_len: a_len - skip,
                };
                iov_count += 1;
                if b_len > 0 {
                    iovecs[iov_count] = libc::iovec {
                        iov_base: b_ptr as *mut libc::c_void,
                        iov_len: b_len,
                    };
                    iov_count += 1;
                }
            } else {
                let b_skip = skip - a_len;
                iovecs[iov_count] = libc::iovec {
                    iov_base: unsafe { b_ptr.add(b_skip) } as *mut libc::c_void,
                    iov_len: b_len - b_skip,
                };
                iov_count += 1;
            }

            let mut msghdr: libc::msghdr = unsafe { std::mem::zeroed() };
            msghdr.msg_iov = iovecs.as_mut_ptr();
            msghdr.msg_iovlen = iov_count as _;

            let ret = unsafe { libc::sendmsg(this.fd, &msghdr, libc::MSG_NOSIGNAL) };
            if ret >= 0 {
                let (a, b) = this.bufs.take().expect("buffers present at completion");
                return Poll::Ready((Ok(ret as usize), a, b));
            }
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if is_would_block(&e) {
                register_write(this.fd, cx.waker().clone());
                return Poll::Pending;
            }
            let (a, b) = this.bufs.take().expect("buffers present on error");
            return Poll::Ready((Err(e), a, b));
        }
    }
}

// ====================
// Readable / Writable Future
// ====================

/// 読み取り可能まで待つ Future。
pub struct Readable<'a> {
    fd: RawFd,
    _marker: std::marker::PhantomData<&'a TcpStream>,
}

impl<'a> Future for Readable<'a> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // POLLIN/EPOLLIN 相当を即座に確認するため 0 バイト peek は行わず、まず fd の
        // readiness を epoll に問い合わせる（poll(2) を使い syscall 1 発で判定する）。
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
        if ret > 0 && pfd.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
            return Poll::Ready(Ok(()));
        }
        register_read(self.fd, cx.waker().clone());
        Poll::Pending
    }
}

/// 書き込み可能まで待つ Future。
pub struct Writable<'a> {
    fd: RawFd,
    _marker: std::marker::PhantomData<&'a TcpStream>,
}

impl<'a> Future for Writable<'a> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
        if ret > 0 && pfd.revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) != 0 {
            return Poll::Ready(Ok(()));
        }
        register_write(self.fd, cx.waker().clone());
        Poll::Pending
    }
}

// ====================
// 汎用 FD 待機 Future（UDP 等、任意の FD に使用）
// ====================

/// 任意の FD が読み込み可能になるまで待つ Future。
pub struct ReadableFd {
    fd: RawFd,
}

impl Future for ReadableFd {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
        if ret > 0 && pfd.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
            return Poll::Ready(Ok(()));
        }
        register_read(self.fd, cx.waker().clone());
        Poll::Pending
    }
}

/// 任意の FD が書き込み可能になるまで待つ Future。
pub struct WritableFd {
    fd: RawFd,
}

impl Future for WritableFd {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
        if ret > 0 && pfd.revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) != 0 {
            return Poll::Ready(Ok(()));
        }
        register_write(self.fd, cx.waker().clone());
        Poll::Pending
    }
}

/// 任意の FD が読み込み可能になるまで待つ。
pub fn wait_readable_fd(fd: RawFd) -> ReadableFd {
    ReadableFd { fd }
}

/// 任意の FD が書き込み可能になるまで待つ。
pub fn wait_writable_fd(fd: RawFd) -> WritableFd {
    WritableFd { fd }
}
