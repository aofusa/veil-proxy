//! 非同期 TcpListener / TcpStream（reactor バックエンド、Windows/Winsock 実装）
//!
//! `unix.rs` と同一の公開 API（型・メソッドシグネチャ）を提供する。実装方式も同じ
//! try-first パターン: 非ブロッキング Winsock 呼び出しを先に試し、`WSAEWOULDBLOCK`
//! なら readiness を登録して `Pending` を返し、起床後に再試行する。
//!
//! Winsock は `errno`/`GetLastError` ではなく `WSAGetLastError()` にエラーコードを積む
//! ため、`io::Error::last_os_error()`（`GetLastError` ベース）ではなく本ファイルの
//! `last_wsa_error()` を必ず使う。`io::Error::from_raw_os_error` は標準ライブラリが
//! Windows では WSA エラーコードも正しく `ErrorKind`（`WouldBlock` 等）へマップするため、
//! `is_would_block()` はプラットフォーム共通のまま使い回せる。

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Once;
use std::task::{Context, Poll};

use windows_sys::Win32::Networking::WinSock::{
    self, closesocket, connect as ws_connect, getpeername, getsockname, getsockopt, ioctlsocket,
    recv, send, setsockopt, shutdown as ws_shutdown, socket, WSAGetLastError, WSAStartup, AF_INET,
    AF_INET6, FIONBIO, INVALID_SOCKET, IPPROTO_TCP, SD_BOTH, SD_RECEIVE, SD_SEND, SOCKADDR,
    SOCKADDR_IN, SOCKADDR_IN6, SOCKET, SOCKET_ERROR, SOCK_STREAM, SOL_SOCKET, SO_ERROR,
    SO_REUSEADDR, TCP_NODELAY as WIN_TCP_NODELAY, WSADATA, WSAEINPROGRESS, WSAEWOULDBLOCK,
};

use crate::runtime::buf::{IoBuf, IoBufMut};
use crate::runtime::executor::{register_read, register_write, unregister};
use crate::runtime::handle::{win, AsRawFd, RawFd};

/// `WSAStartup` を一度だけ実行する。プロセス内のどの Winsock API 呼び出しよりも
/// 先に済ませる必要があるため、ソケット生成の入口（`create_nonblocking_socket`）で
/// 呼ぶ（`Once` でプロセス生涯 1 回のみ実行される）。
fn ensure_wsa_started() {
    static INIT: Once = Once::new();
    INIT.call_once(|| unsafe {
        let mut data: WSADATA = std::mem::zeroed();
        // 2.2 を要求する（現行 Windows は常に対応）。
        WSAStartup(0x0202, &mut data);
    });
    // `WSACleanup` はプロセス終了時に OS が後始末するため明示的には呼ばない
    // （複数スレッドが同時に Winsock を使う本プロセスの構造上、参照カウント管理が
    // 煩雑になるだけで実利が無いため）。
}

/// 直近の Winsock 呼び出しのエラーを `io::Error` として取得する。
///
/// `GetLastError`（`io::Error::last_os_error()`）ではなく `WSAGetLastError()` を
/// 使う必要がある点が Winsock API の注意点（別のエラーコード空間）。
#[inline]
fn last_wsa_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

/// `EAGAIN`/`EWOULDBLOCK` 相当（`WSAEWOULDBLOCK`）か判定する。
///
/// `io::Error::from_raw_os_error` は Windows でも WSA エラーコードを
/// `ErrorKind::WouldBlock` へマップするため、Unix 版と同じ判定式を使い回せる。
#[inline]
fn is_would_block(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::WouldBlock
}

fn sockaddr_to_storage(addr: &SocketAddr) -> (Vec<u8>, i32) {
    match addr {
        SocketAddr::V4(v4) => {
            let mut sin: SOCKADDR_IN = unsafe { std::mem::zeroed() };
            sin.sin_family = AF_INET;
            sin.sin_port = v4.port().to_be();
            sin.sin_addr.S_un.S_addr = u32::from_ne_bytes(v4.ip().octets());
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    &sin as *const _ as *const u8,
                    std::mem::size_of::<SOCKADDR_IN>(),
                )
            }
            .to_vec();
            (bytes, std::mem::size_of::<SOCKADDR_IN>() as i32)
        }
        SocketAddr::V6(v6) => {
            let mut sin6: SOCKADDR_IN6 = unsafe { std::mem::zeroed() };
            sin6.sin6_family = AF_INET6;
            sin6.sin6_port = v6.port().to_be();
            sin6.sin6_addr.u.Byte = v6.ip().octets();
            sin6.sin6_flowinfo = v6.flowinfo();
            // union フィールドへの書き込みは安全（読み取りのみ unsafe）。
            sin6.Anonymous.sin6_scope_id = v6.scope_id();
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    &sin6 as *const _ as *const u8,
                    std::mem::size_of::<SOCKADDR_IN6>(),
                )
            }
            .to_vec();
            (bytes, std::mem::size_of::<SOCKADDR_IN6>() as i32)
        }
    }
}

fn storage_to_sockaddr(buf: &[u8]) -> io::Result<SocketAddr> {
    if buf.len() >= std::mem::size_of::<SOCKADDR_IN>() {
        let family = unsafe { (*(buf.as_ptr() as *const SOCKADDR)).sa_family };
        if family == AF_INET {
            let sin = unsafe { &*(buf.as_ptr() as *const SOCKADDR_IN) };
            let ip = std::net::Ipv4Addr::from(u32::from_be(unsafe { sin.sin_addr.S_un.S_addr }));
            let port = u16::from_be(sin.sin_port);
            return Ok(SocketAddr::V4(std::net::SocketAddrV4::new(ip, port)));
        }
        if family == AF_INET6 && buf.len() >= std::mem::size_of::<SOCKADDR_IN6>() {
            let sin6 = unsafe { &*(buf.as_ptr() as *const SOCKADDR_IN6) };
            let ip = std::net::Ipv6Addr::from(unsafe { sin6.sin6_addr.u.Byte });
            let port = u16::from_be(sin6.sin6_port);
            let scope_id = unsafe { sin6.Anonymous.sin6_scope_id };
            return Ok(SocketAddr::V6(std::net::SocketAddrV6::new(
                ip,
                port,
                sin6.sin6_flowinfo,
                scope_id,
            )));
        }
    }
    Err(io::Error::other("unsupported address family"))
}

/// ノンブロッキングソケットを作成する。
fn create_nonblocking_socket(domain: i32) -> io::Result<RawFd> {
    ensure_wsa_started();
    let sock = unsafe { socket(domain, SOCK_STREAM, IPPROTO_TCP as i32) };
    if sock == INVALID_SOCKET {
        return Err(last_wsa_error());
    }
    let mut nonblocking: u32 = 1;
    if unsafe { ioctlsocket(sock, FIONBIO, &mut nonblocking) } == SOCKET_ERROR {
        let e = last_wsa_error();
        unsafe { closesocket(sock) };
        return Err(e);
    }
    Ok(win::from_socket(sock))
}

fn set_reuseaddr(sock: SOCKET) {
    let optval: i32 = 1;
    unsafe {
        setsockopt(
            sock,
            SOL_SOCKET,
            SO_REUSEADDR,
            &optval as *const _ as *const u8,
            std::mem::size_of::<i32>() as i32,
        );
    }
}

// ====================
// TcpListener
// ====================

/// 非同期 TCP リスナー。
pub struct TcpListener {
    fd: RawFd,
}

impl TcpListener {
    pub fn bind(addr: impl std::net::ToSocketAddrs) -> io::Result<Self> {
        Self::bind_impl(addr, false)
    }

    /// Windows には Linux 的な `SO_REUSEPORT`（カーネル分散）が無いため、
    /// `SO_REUSEADDR` のみを設定する（thread-per-core の accept 分散は保証されない。
    /// F-125 設計ドキュメント参照。ビルド通過のみが目的で実機性能検証は対象外）。
    pub fn bind_reuse_port(addr: impl std::net::ToSocketAddrs) -> io::Result<Self> {
        Self::bind_impl(addr, true)
    }

    fn bind_impl(addr: impl std::net::ToSocketAddrs, _reuse_port: bool) -> io::Result<Self> {
        let addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address"))?;
        let domain = if addr.is_ipv6() { AF_INET6 } else { AF_INET };
        let fd = create_nonblocking_socket(domain as i32)?;
        let sock = win::to_socket(fd);
        set_reuseaddr(sock);

        let (storage, len) = sockaddr_to_storage(&addr);
        let ret = unsafe { WinSock::bind(sock, storage.as_ptr() as *const SOCKADDR, len) };
        if ret == SOCKET_ERROR {
            let e = last_wsa_error();
            unsafe { closesocket(sock) };
            return Err(e);
        }
        let ret = unsafe { WinSock::listen(sock, 1024) };
        if ret == SOCKET_ERROR {
            let e = last_wsa_error();
            unsafe { closesocket(sock) };
            return Err(e);
        }
        Ok(Self { fd })
    }

    pub fn accept(&self) -> Accept<'_> {
        Accept {
            listener_fd: self.fd,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        let sock = win::to_socket(self.fd);
        let mut buf = [0u8; 128];
        let mut len = buf.len() as i32;
        let ret = unsafe { getsockname(sock, buf.as_mut_ptr() as *mut SOCKADDR, &mut len) };
        if ret == SOCKET_ERROR {
            return Err(last_wsa_error());
        }
        storage_to_sockaddr(&buf)
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        unregister(self.fd);
        unsafe { closesocket(win::to_socket(self.fd)) };
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

pub struct Accept<'a> {
    listener_fd: RawFd,
    _marker: std::marker::PhantomData<&'a TcpListener>,
}

impl<'a> Future for Accept<'a> {
    type Output = io::Result<(TcpStream, SocketAddr)>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let listener_sock = win::to_socket(self.listener_fd);
        let mut buf = [0u8; 128];
        let mut len = buf.len() as i32;
        let accepted =
            unsafe { WinSock::accept(listener_sock, buf.as_mut_ptr() as *mut SOCKADDR, &mut len) };
        if accepted != INVALID_SOCKET {
            let fd = win::from_socket(accepted);
            let mut nonblocking: u32 = 1;
            unsafe { ioctlsocket(accepted, FIONBIO, &mut nonblocking) };
            let stream = TcpStream { fd };
            let peer_addr = storage_to_sockaddr(&buf)?;
            return Poll::Ready(Ok((stream, peer_addr)));
        }
        let e = last_wsa_error();
        if is_would_block(&e) {
            register_read(self.listener_fd, cx.waker().clone());
            return Poll::Pending;
        }
        Poll::Ready(Err(e))
    }
}

// ====================
// TcpStream
// ====================

pub struct TcpStream {
    pub(crate) fd: RawFd,
}

impl TcpStream {
    /// # Safety
    /// `fd` は有効な非ブロッキングソケットハンドルであること。
    pub unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Self { fd }
    }

    pub fn connect(addr: SocketAddr) -> Connect {
        Connect {
            addr,
            fd: crate::runtime::handle::INVALID_FD,
            registered: false,
        }
    }

    pub async fn connect_str(addr: &str) -> io::Result<TcpStream> {
        use std::net::ToSocketAddrs;
        let socket_addr = addr
            .to_socket_addrs()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address resolved"))?;
        TcpStream::connect(socket_addr).await
    }

    pub fn read<T: IoBufMut>(&self, buf: T) -> ReadFuture<T> {
        ReadFuture {
            fd: self.fd,
            buf: Some(buf),
        }
    }

    pub fn write<T: IoBuf>(&self, buf: T) -> WriteFuture<T> {
        WriteFuture {
            fd: self.fd,
            buf: Some(buf),
        }
    }

    /// 2 つの不連続バッファを書き込む。Windows は `WSASend`（`WSABUF` scatter-gather）を
    /// 使う（Unix 版の `sendmsg` に相当）。
    pub fn writev2<A: IoBuf, B: IoBuf>(&self, a: A, b: B, skip: usize) -> SendMsgFuture<A, B> {
        SendMsgFuture {
            fd: self.fd,
            bufs: Some((a, b)),
            skip,
        }
    }

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
                            "WSASend returned zero",
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

    pub fn readable(&self) -> Readable<'_> {
        Readable {
            fd: self.fd,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn writable(&self) -> Writable<'_> {
        Writable {
            fd: self.fd,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        let optval: i32 = if nodelay { 1 } else { 0 };
        let ret = unsafe {
            setsockopt(
                win::to_socket(self.fd),
                IPPROTO_TCP as i32,
                WIN_TCP_NODELAY,
                &optval as *const _ as *const u8,
                std::mem::size_of::<i32>() as i32,
            )
        };
        if ret == SOCKET_ERROR {
            return Err(last_wsa_error());
        }
        Ok(())
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        let mut buf = [0u8; 128];
        let mut len = buf.len() as i32;
        let ret = unsafe {
            getpeername(
                win::to_socket(self.fd),
                buf.as_mut_ptr() as *mut SOCKADDR,
                &mut len,
            )
        };
        if ret == SOCKET_ERROR {
            return Err(last_wsa_error());
        }
        storage_to_sockaddr(&buf)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        let mut buf = [0u8; 128];
        let mut len = buf.len() as i32;
        let ret = unsafe {
            getsockname(
                win::to_socket(self.fd),
                buf.as_mut_ptr() as *mut SOCKADDR,
                &mut len,
            )
        };
        if ret == SOCKET_ERROR {
            return Err(last_wsa_error());
        }
        storage_to_sockaddr(&buf)
    }

    pub fn shutdown(&self, how: std::net::Shutdown) -> io::Result<()> {
        let how = match how {
            std::net::Shutdown::Read => SD_RECEIVE,
            std::net::Shutdown::Write => SD_SEND,
            std::net::Shutdown::Both => SD_BOTH,
        };
        let ret = unsafe { ws_shutdown(win::to_socket(self.fd), how) };
        if ret == SOCKET_ERROR {
            return Err(last_wsa_error());
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
            unsafe { closesocket(win::to_socket(self.fd)) };
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

pub struct Connect {
    addr: SocketAddr,
    fd: RawFd,
    registered: bool,
}

impl Connect {
    fn fail(&mut self, e: io::Error) -> io::Error {
        let fd = self.fd;
        self.fd = crate::runtime::handle::INVALID_FD;
        if self.registered {
            unregister(fd);
            self.registered = false;
        }
        unsafe { closesocket(win::to_socket(fd)) };
        e
    }
}

impl Future for Connect {
    type Output = io::Result<TcpStream>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.fd < 0 {
            let domain = if self.addr.is_ipv6() {
                AF_INET6
            } else {
                AF_INET
            };
            let fd = match create_nonblocking_socket(domain as i32) {
                Ok(fd) => fd,
                Err(e) => return Poll::Ready(Err(e)),
            };
            self.fd = fd;

            let (storage, len) = sockaddr_to_storage(&self.addr);
            let sock = win::to_socket(fd);
            let ret = unsafe { ws_connect(sock, storage.as_ptr() as *const SOCKADDR, len) };
            if ret == 0 {
                let fd = self.fd;
                self.fd = crate::runtime::handle::INVALID_FD;
                return Poll::Ready(Ok(TcpStream { fd }));
            }
            let e = last_wsa_error();
            if e.raw_os_error() != Some(WSAEINPROGRESS) && e.raw_os_error() != Some(WSAEWOULDBLOCK)
            {
                unsafe { closesocket(sock) };
                self.fd = crate::runtime::handle::INVALID_FD;
                return Poll::Ready(Err(e));
            }
        }

        // writable（接続完了）を WSAPoll 相当の 0 タイムアウトチェックで確認してから
        // SO_ERROR を読む（`Readable`/`Writable` と同じ即時チェックパターン）。
        let sock = win::to_socket(self.fd);
        let mut pfd = WinSock::WSAPOLLFD {
            fd: sock,
            events: WinSock::POLLWRNORM as i16,
            revents: 0,
        };
        let ret = unsafe { WinSock::WSAPoll(&mut pfd, 1, 0) };
        if ret == SOCKET_ERROR {
            let e = last_wsa_error();
            return Poll::Ready(Err(self.fail(e)));
        }
        if ret <= 0
            || pfd.revents & ((WinSock::POLLWRNORM | WinSock::POLLERR | WinSock::POLLHUP) as i16)
                == 0
        {
            register_write(self.fd, cx.waker().clone());
            self.registered = true;
            return Poll::Pending;
        }

        let mut err: i32 = 0;
        let mut len = std::mem::size_of::<i32>() as i32;
        let ret = unsafe {
            getsockopt(
                sock,
                SOL_SOCKET,
                SO_ERROR,
                &mut err as *mut _ as *mut u8,
                &mut len,
            )
        };
        if ret == SOCKET_ERROR {
            let e = last_wsa_error();
            return Poll::Ready(Err(self.fail(e)));
        }
        if err != 0 {
            let e = io::Error::from_raw_os_error(err);
            return Poll::Ready(Err(self.fail(e)));
        }

        let fd = self.fd;
        self.fd = crate::runtime::handle::INVALID_FD;
        Poll::Ready(Ok(TcpStream { fd }))
    }
}

impl Drop for Connect {
    fn drop(&mut self) {
        if self.fd >= 0 {
            if self.registered {
                unregister(self.fd);
            }
            unsafe { closesocket(win::to_socket(self.fd)) };
        }
    }
}

// ====================
// Read Future
// ====================

pub struct ReadFuture<T: IoBufMut> {
    fd: RawFd,
    buf: Option<T>,
}

impl<T: IoBufMut> Future for ReadFuture<T> {
    type Output = (io::Result<usize>, T);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        let buf = this
            .buf
            .as_mut()
            .expect("ReadFuture polled after completion");
        let ret = unsafe {
            recv(
                win::to_socket(this.fd),
                buf.write_ptr(),
                buf.bytes_total() as i32,
                0,
            )
        };
        if ret >= 0 {
            unsafe { buf.set_init(ret as usize) };
            let buf = this.buf.take().expect("buffer present at completion");
            return Poll::Ready((Ok(ret as usize), buf));
        }
        let e = last_wsa_error();
        if is_would_block(&e) {
            register_read(this.fd, cx.waker().clone());
            return Poll::Pending;
        }
        let buf = this.buf.take().expect("buffer present on error");
        Poll::Ready((Err(e), buf))
    }
}

// ====================
// Write Future
// ====================

pub struct WriteFuture<T: IoBuf> {
    fd: RawFd,
    buf: Option<T>,
}

impl<T: IoBuf> Future for WriteFuture<T> {
    type Output = (io::Result<usize>, T);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        let buf = this
            .buf
            .as_ref()
            .expect("WriteFuture polled after completion");
        let ret = unsafe {
            send(
                win::to_socket(this.fd),
                buf.read_ptr(),
                buf.bytes_init() as i32,
                0,
            )
        };
        if ret >= 0 {
            let buf = this.buf.take().expect("buffer present at completion");
            return Poll::Ready((Ok(ret as usize), buf));
        }
        let e = last_wsa_error();
        if is_would_block(&e) {
            register_write(this.fd, cx.waker().clone());
            return Poll::Pending;
        }
        let buf = this.buf.take().expect("buffer present on error");
        Poll::Ready((Err(e), buf))
    }
}

// ====================
// SendMsg (scatter-gather) Future（`WSASend` 版）
// ====================

pub struct SendMsgFuture<A: IoBuf, B: IoBuf> {
    fd: RawFd,
    bufs: Option<(A, B)>,
    skip: usize,
}

impl<A: IoBuf, B: IoBuf> Future for SendMsgFuture<A, B> {
    type Output = (io::Result<usize>, A, B);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        let (a, b) = this
            .bufs
            .as_ref()
            .expect("SendMsgFuture polled after completion");
        let (a_ptr, a_len) = (a.read_ptr(), a.bytes_init());
        let (b_ptr, b_len) = (b.read_ptr(), b.bytes_init());
        let skip = this.skip;
        debug_assert!(skip < a_len + b_len);

        let mut wsabufs = [WinSock::WSABUF {
            len: 0,
            buf: std::ptr::null_mut(),
        }; 2];
        let mut count = 0u32;
        if skip < a_len {
            wsabufs[count as usize] = WinSock::WSABUF {
                len: (a_len - skip) as u32,
                buf: unsafe { a_ptr.add(skip) as *mut u8 },
            };
            count += 1;
            if b_len > 0 {
                wsabufs[count as usize] = WinSock::WSABUF {
                    len: b_len as u32,
                    buf: b_ptr as *mut u8,
                };
                count += 1;
            }
        } else {
            let b_skip = skip - a_len;
            wsabufs[count as usize] = WinSock::WSABUF {
                len: (b_len - b_skip) as u32,
                buf: unsafe { b_ptr.add(b_skip) as *mut u8 },
            };
            count += 1;
        }

        let mut sent: u32 = 0;
        let ret = unsafe {
            WinSock::WSASend(
                win::to_socket(this.fd),
                wsabufs.as_ptr(),
                count,
                &mut sent,
                0,
                std::ptr::null_mut(),
                None,
            )
        };
        if ret == 0 {
            let (a, b) = this.bufs.take().expect("buffers present at completion");
            return Poll::Ready((Ok(sent as usize), a, b));
        }
        let e = last_wsa_error();
        if is_would_block(&e) {
            register_write(this.fd, cx.waker().clone());
            return Poll::Pending;
        }
        let (a, b) = this.bufs.take().expect("buffers present on error");
        Poll::Ready((Err(e), a, b))
    }
}

// ====================
// Readable / Writable Future
// ====================

fn poll_ready_now(fd: RawFd, want_write: bool) -> bool {
    let mut pfd = WinSock::WSAPOLLFD {
        fd: win::to_socket(fd),
        events: if want_write {
            WinSock::POLLWRNORM as i16
        } else {
            WinSock::POLLRDNORM as i16
        },
        revents: 0,
    };
    let ret = unsafe { WinSock::WSAPoll(&mut pfd, 1, 0) };
    let mask = if want_write {
        (WinSock::POLLWRNORM | WinSock::POLLERR | WinSock::POLLHUP) as i16
    } else {
        (WinSock::POLLRDNORM | WinSock::POLLERR | WinSock::POLLHUP) as i16
    };
    ret > 0 && pfd.revents & mask != 0
}

pub struct Readable<'a> {
    fd: RawFd,
    _marker: std::marker::PhantomData<&'a TcpStream>,
}

impl<'a> Future for Readable<'a> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if poll_ready_now(self.fd, false) {
            return Poll::Ready(Ok(()));
        }
        register_read(self.fd, cx.waker().clone());
        Poll::Pending
    }
}

pub struct Writable<'a> {
    fd: RawFd,
    _marker: std::marker::PhantomData<&'a TcpStream>,
}

impl<'a> Future for Writable<'a> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if poll_ready_now(self.fd, true) {
            return Poll::Ready(Ok(()));
        }
        register_write(self.fd, cx.waker().clone());
        Poll::Pending
    }
}

// ====================
// 汎用 FD 待機 Future（任意のソケットハンドルに使用）
// ====================

pub struct ReadableFd {
    fd: RawFd,
}

impl Future for ReadableFd {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if poll_ready_now(self.fd, false) {
            return Poll::Ready(Ok(()));
        }
        register_read(self.fd, cx.waker().clone());
        Poll::Pending
    }
}

pub struct WritableFd {
    fd: RawFd,
}

impl Future for WritableFd {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if poll_ready_now(self.fd, true) {
            return Poll::Ready(Ok(()));
        }
        register_write(self.fd, cx.waker().clone());
        Poll::Pending
    }
}

pub fn wait_readable_fd(fd: RawFd) -> ReadableFd {
    ReadableFd { fd }
}

pub fn wait_writable_fd(fd: RawFd) -> WritableFd {
    WritableFd { fd }
}
