//! 汎用非同期 UDP ソケット（L4 UDP プロキシ用、F-124 / F-131 Windows 対応）
//!
//! `src/udp/socket.rs`（`QuicUdpSocket`）は HTTP/3(QUIC) 専用で GSO/GRO・`http3`
//! feature ゲート付きのため、L4 の平文 UDP プロキシには流用しない。本モジュールは
//! そのための最小構成の汎用 UDP ソケットを提供する。
//!
//! ## 設計（ホットパス絶対規則: 新規 io_uring オペコード追加禁止）
//!
//! `recvfrom`/`sendto`/`connect`/`send`/`recv` はいずれもノンブロッキング fd に対して
//! try-first で発行し、`EAGAIN`/`EWOULDBLOCK` の場合のみ
//! `crate::runtime::tcp::wait_readable_fd` / `wait_writable_fd`（io_uring では
//! `IORING_OP_POLL_ADD`、reactor では poller 登録。どちらも「任意の fd の readiness を
//! 待つ」既存 API で UDP 専用の新規オペコードを一切追加しない）で待機して再試行する。
//! この try-first + 既存 readiness 待機の組み合わせは io_uring / reactor の両バックエンドで
//! 完全に共通のコードで実現できるため、非 Linux / Windows でも同一設計で動作する。

use crate::runtime::handle::{AsRawFd, RawFd};
use crate::runtime::tcp::{wait_readable_fd, wait_writable_fd};
use std::io;
use std::net::SocketAddr;

#[cfg(windows)]
use std::sync::Once;
#[cfg(windows)]
use windows_sys::Win32::Networking::WinSock::{
    self, closesocket, connect as ws_connect, getsockname as ws_getsockname, ioctlsocket,
    recv as ws_recv, recvfrom as ws_recvfrom, send as ws_send, sendto as ws_sendto,
    setsockopt as ws_setsockopt, socket as ws_socket, WSAGetLastError, WSAStartup, AF_INET,
    AF_INET6, FIONBIO, INVALID_SOCKET, IPPROTO_UDP, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6,
    SOCKET_ERROR, SOCK_DGRAM, SOL_SOCKET, SO_REUSEADDR, WSADATA, WSAEWOULDBLOCK,
};

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(windows)]
fn ensure_wsa_started() {
    static INIT: Once = Once::new();
    INIT.call_once(|| unsafe {
        let mut data: WSADATA = std::mem::zeroed();
        WSAStartup(0x0202, &mut data);
    });
}

#[cfg(windows)]
#[inline]
fn last_wsa_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

#[cfg(windows)]
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

#[cfg(windows)]
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

#[cfg(unix)]
fn new_nonblocking_dgram_socket(domain: libc::c_int) -> io::Result<RawFd> {
    #[cfg(target_os = "macos")]
    let fd = {
        let fd = unsafe { libc::socket(domain, libc::SOCK_DGRAM, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        unsafe {
            libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        fd
    };
    #[cfg(not(target_os = "macos"))]
    let fd = unsafe {
        libc::socket(
            domain,
            libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(target_os = "macos")]
    {
        let optval: libc::c_int = 1;
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
    Ok(fd)
}

#[cfg(windows)]
fn new_nonblocking_dgram_socket(domain: i32) -> io::Result<RawFd> {
    ensure_wsa_started();
    let sock = unsafe { ws_socket(domain, SOCK_DGRAM as i32, IPPROTO_UDP as i32) };
    if sock == INVALID_SOCKET {
        return Err(last_wsa_error());
    }
    let mut nonblocking: u32 = 1;
    if unsafe { ioctlsocket(sock, FIONBIO, &mut nonblocking) } == SOCKET_ERROR {
        let e = last_wsa_error();
        unsafe { closesocket(sock) };
        return Err(e);
    }
    Ok(crate::runtime::handle::win::from_socket(sock))
}

/// 汎用非同期 UDP ソケット（L4 プロキシ用）。
pub struct UdpSocket {
    fd: RawFd,
    local_addr: SocketAddr,
}

impl UdpSocket {
    /// 指定アドレスへ bind する。
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        #[cfg(unix)]
        let domain = if addr.is_ipv6() {
            libc::AF_INET6
        } else {
            libc::AF_INET
        };
        #[cfg(windows)]
        let domain = if addr.is_ipv6() {
            AF_INET6 as i32
        } else {
            AF_INET as i32
        };

        let fd = new_nonblocking_dgram_socket(domain)?;

        #[cfg(unix)]
        {
            let optval: libc::c_int = 1;
            let ret = unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_REUSEADDR,
                    &optval as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            if ret < 0 {
                unsafe { libc::close(fd) };
                return Err(io::Error::last_os_error());
            }
            let (storage, len) = sockaddr_to_storage(&addr);
            let ret =
                unsafe { libc::bind(fd, &storage as *const _ as *const libc::sockaddr, len) };
            if ret < 0 {
                unsafe { libc::close(fd) };
                return Err(io::Error::last_os_error());
            }
        }

        #[cfg(windows)]
        {
            let sock = crate::runtime::handle::win::to_socket(fd);
            let optval: i32 = 1;
            let ret = unsafe {
                ws_setsockopt(
                    sock,
                    SOL_SOCKET,
                    SO_REUSEADDR,
                    &optval as *const _ as *const u8,
                    std::mem::size_of::<i32>() as i32,
                )
            };
            if ret == SOCKET_ERROR {
                unsafe { closesocket(sock) };
                return Err(last_wsa_error());
            }
            let (storage, len) = sockaddr_to_storage(&addr);
            let ret = unsafe { WinSock::bind(sock, storage.as_ptr() as *const SOCKADDR, len) };
            if ret == SOCKET_ERROR {
                unsafe { closesocket(sock) };
                return Err(last_wsa_error());
            }
        }

        let local_addr = Self::query_local_addr(fd)?;
        Ok(Self { fd, local_addr })
    }

    /// upstream への「疑似接続」（`connect(2)`）。
    pub fn connect(&self, addr: SocketAddr) -> io::Result<()> {
        let (storage, len) = sockaddr_to_storage(&addr);
        #[cfg(unix)]
        {
            let ret = unsafe {
                libc::connect(self.fd, &storage as *const _ as *const libc::sockaddr, len)
            };
            if ret < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        #[cfg(windows)]
        {
            let sock = crate::runtime::handle::win::to_socket(self.fd);
            let ret = unsafe { ws_connect(sock, storage.as_ptr() as *const SOCKADDR, len) };
            if ret == SOCKET_ERROR {
                return Err(last_wsa_error());
            }
        }
        Ok(())
    }

    /// `connect` 済みソケットに対する送信。
    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        loop {
            #[cfg(unix)]
            let ret =
                unsafe { libc::send(self.fd, buf.as_ptr() as *const libc::c_void, buf.len(), 0) };
            #[cfg(windows)]
            let ret = unsafe {
                ws_send(
                    crate::runtime::handle::win::to_socket(self.fd),
                    buf.as_ptr(),
                    buf.len() as i32,
                    0,
                ) as isize
            };

            if ret >= 0 {
                return Ok(ret as usize);
            }
            #[cfg(unix)]
            let err = io::Error::last_os_error();
            #[cfg(windows)]
            let err = last_wsa_error();

            if err.kind() == io::ErrorKind::WouldBlock {
                wait_writable_fd(self.fd).await?;
                continue;
            }
            return Err(err);
        }
    }

    /// `connect` 済みソケットに対する受信。
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            #[cfg(unix)]
            let ret = unsafe {
                libc::recv(
                    self.fd,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    0,
                )
            };
            #[cfg(windows)]
            let ret = unsafe {
                ws_recv(
                    crate::runtime::handle::win::to_socket(self.fd),
                    buf.as_mut_ptr(),
                    buf.len() as i32,
                    0,
                ) as isize
            };

            if ret >= 0 {
                return Ok(ret as usize);
            }
            #[cfg(unix)]
            let err = io::Error::last_os_error();
            #[cfg(windows)]
            let err = last_wsa_error();

            if err.kind() == io::ErrorKind::WouldBlock {
                wait_readable_fd(self.fd).await?;
                continue;
            }
            return Err(err);
        }
    }

    /// 宛先アドレスを指定して送信する。
    pub async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        let (storage, len) = sockaddr_to_storage(&addr);
        loop {
            #[cfg(unix)]
            let ret = unsafe {
                libc::sendto(
                    self.fd,
                    buf.as_ptr() as *const libc::c_void,
                    buf.len(),
                    0,
                    &storage as *const _ as *const libc::sockaddr,
                    len,
                )
            };
            #[cfg(windows)]
            let ret = unsafe {
                ws_sendto(
                    crate::runtime::handle::win::to_socket(self.fd),
                    buf.as_ptr(),
                    buf.len() as i32,
                    0,
                    storage.as_ptr() as *const SOCKADDR,
                    len,
                ) as isize
            };

            if ret >= 0 {
                return Ok(ret as usize);
            }
            #[cfg(unix)]
            let err = io::Error::last_os_error();
            #[cfg(windows)]
            let err = last_wsa_error();

            if err.kind() == io::ErrorKind::WouldBlock {
                wait_writable_fd(self.fd).await?;
                continue;
            }
            return Err(err);
        }
    }

    /// 送信元アドレス付きで受信する。
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        loop {
            #[cfg(unix)]
            let (ret, from) = {
                let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
                let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
                let r = unsafe {
                    libc::recvfrom(
                        self.fd,
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                        0,
                        &mut storage as *mut _ as *mut libc::sockaddr,
                        &mut len,
                    )
                };
                (
                    r,
                    if r >= 0 {
                        Some(storage_to_sockaddr(&storage)?)
                    } else {
                        None
                    },
                )
            };

            #[cfg(windows)]
            let (ret, from) = {
                let mut buf_storage = [0u8; 128];
                let mut len = buf_storage.len() as i32;
                let r = unsafe {
                    ws_recvfrom(
                        crate::runtime::handle::win::to_socket(self.fd),
                        buf.as_mut_ptr(),
                        buf.len() as i32,
                        0,
                        buf_storage.as_mut_ptr() as *mut SOCKADDR,
                        &mut len,
                    )
                };
                (
                    r as isize,
                    if r >= 0 {
                        Some(storage_to_sockaddr(&buf_storage)?)
                    } else {
                        None
                    },
                )
            };

            if ret >= 0 {
                return Ok((ret as usize, from.unwrap()));
            }
            #[cfg(unix)]
            let err = io::Error::last_os_error();
            #[cfg(windows)]
            let err = last_wsa_error();

            if err.kind() == io::ErrorKind::WouldBlock {
                wait_readable_fd(self.fd).await?;
                continue;
            }
            return Err(err);
        }
    }

    /// ローカルアドレスを取得する。
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    fn query_local_addr(fd: RawFd) -> io::Result<SocketAddr> {
        #[cfg(unix)]
        {
            let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
            let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            let ret = unsafe {
                libc::getsockname(fd, &mut storage as *mut _ as *mut libc::sockaddr, &mut len)
            };
            if ret < 0 {
                return Err(io::Error::last_os_error());
            }
            storage_to_sockaddr(&storage)
        }
        #[cfg(windows)]
        {
            let mut buf = [0u8; 128];
            let mut len = buf.len() as i32;
            let ret = unsafe {
                ws_getsockname(
                    crate::runtime::handle::win::to_socket(fd),
                    buf.as_mut_ptr() as *mut SOCKADDR,
                    &mut len,
                )
            };
            if ret == SOCKET_ERROR {
                return Err(last_wsa_error());
            }
            storage_to_sockaddr(&buf)
        }
    }
}

impl AsRawFd for UdpSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::close(self.fd);
        }
        #[cfg(windows)]
        unsafe {
            closesocket(crate::runtime::handle::win::to_socket(self.fd));
        }
    }
}
