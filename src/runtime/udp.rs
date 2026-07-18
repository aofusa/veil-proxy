//! 汎用非同期 UDP ソケット（L4 UDP プロキシ用、F-124）
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
//! 完全に共通のコードで実現できるため、cfg 分岐のない単一実装にしている。

use crate::runtime::tcp::{wait_readable_fd, wait_writable_fd};
use std::io;
use std::net::SocketAddr;
use std::os::unix::io::{AsRawFd, RawFd};

/// SocketAddr を libc::sockaddr_storage に変換する（TCP 側と同一ロジック）。
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

/// libc::sockaddr_storage を SocketAddr に変換する（TCP 側と同一ロジック）。
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

fn new_nonblocking_dgram_socket(domain: libc::c_int) -> io::Result<RawFd> {
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
    Ok(fd)
}

/// 汎用非同期 UDP ソケット（L4 プロキシ用）。
///
/// io_uring / reactor いずれのバックエンドでも同一コードで動作する
/// （readiness 待ちは `runtime::tcp::wait_readable_fd`/`wait_writable_fd` に委譲）。
pub struct UdpSocket {
    fd: RawFd,
    local_addr: SocketAddr,
}

impl UdpSocket {
    /// 指定アドレスへ bind する。
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let domain = if addr.is_ipv6() {
            libc::AF_INET6
        } else {
            libc::AF_INET
        };
        let fd = new_nonblocking_dgram_socket(domain)?;

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
        let ret = unsafe { libc::bind(fd, &storage as *const _ as *const libc::sockaddr, len) };
        if ret < 0 {
            unsafe { libc::close(fd) };
            return Err(io::Error::last_os_error());
        }

        let local_addr = Self::query_local_addr(fd)?;
        Ok(Self { fd, local_addr })
    }

    /// upstream への「疑似接続」（`connect(2)`）。以降は `send`/`recv`（宛先省略）を使え、
    /// カーネルが宛先以外からのデータグラムを破棄するためセッション分離にも寄与する。
    pub fn connect(&self, addr: SocketAddr) -> io::Result<()> {
        let (storage, len) = sockaddr_to_storage(&addr);
        let ret =
            unsafe { libc::connect(self.fd, &storage as *const _ as *const libc::sockaddr, len) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// `connect` 済みソケットに対する送信。
    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        loop {
            let ret =
                unsafe { libc::send(self.fd, buf.as_ptr() as *const libc::c_void, buf.len(), 0) };
            if ret >= 0 {
                return Ok(ret as usize);
            }
            let err = io::Error::last_os_error();
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
            let ret =
                unsafe { libc::recv(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
            if ret >= 0 {
                return Ok(ret as usize);
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                wait_readable_fd(self.fd).await?;
                continue;
            }
            return Err(err);
        }
    }

    /// 宛先アドレスを指定して送信する（リスナーソケットからクライアントへの応答用）。
    pub async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        let (storage, len) = sockaddr_to_storage(&addr);
        loop {
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
            if ret >= 0 {
                return Ok(ret as usize);
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                wait_writable_fd(self.fd).await?;
                continue;
            }
            return Err(err);
        }
    }

    /// 送信元アドレス付きで受信する（リスナーソケットの recvfrom ループ用）。
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        loop {
            let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
            let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            let ret = unsafe {
                libc::recvfrom(
                    self.fd,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    0,
                    &mut storage as *mut _ as *mut libc::sockaddr,
                    &mut len,
                )
            };
            if ret >= 0 {
                let from = storage_to_sockaddr(&storage)?;
                return Ok((ret as usize, from));
            }
            let err = io::Error::last_os_error();
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
}

impl AsRawFd for UdpSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_driver_available() -> bool {
        #[cfg(veil_rt_uring)]
        {
            crate::runtime::ring::IoUring::new(8, 0).is_ok()
        }
        #[cfg(veil_rt_reactor)]
        {
            let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
            if fd >= 0 {
                unsafe { libc::close(fd) };
                true
            } else {
                // BSD kqueue reactor: epoll は無いので常に利用可能とみなす
                cfg!(not(target_os = "linux"))
            }
        }
    }

    #[test]
    fn test_bind_assigns_local_addr() {
        let sock = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).expect("bind");
        assert!(sock.local_addr().port() > 0);
    }

    #[test]
    fn test_send_to_recv_from_roundtrip() {
        if !runtime_driver_available() {
            eprintln!("runtime driver unavailable; skipping test_send_to_recv_from_roundtrip");
            return;
        }
        crate::runtime::block_on(async move {
            let server = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).expect("bind server");
            let server_addr = server.local_addr();
            let client = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).expect("bind client");
            let client_addr = client.local_addr();

            client
                .send_to(b"hello", server_addr)
                .await
                .expect("send_to");
            let mut buf = [0u8; 16];
            let (n, from) = server.recv_from(&mut buf).await.expect("recv_from");
            assert_eq!(&buf[..n], b"hello");
            assert_eq!(from, client_addr);

            server.send_to(b"world", from).await.expect("reply send_to");
            let mut buf2 = [0u8; 16];
            let (n2, _) = client.recv_from(&mut buf2).await.expect("recv_from reply");
            assert_eq!(&buf2[..n2], b"world");
        });
    }

    #[test]
    fn test_connect_send_recv_roundtrip() {
        if !runtime_driver_available() {
            eprintln!("runtime driver unavailable; skipping test_connect_send_recv_roundtrip");
            return;
        }
        crate::runtime::block_on(async move {
            let server = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).expect("bind server");
            let server_addr = server.local_addr();
            let client = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).expect("bind client");
            client.connect(server_addr).expect("connect");

            client.send(b"ping").await.expect("send");
            let mut buf = [0u8; 16];
            let (n, from) = server.recv_from(&mut buf).await.expect("recv_from");
            assert_eq!(&buf[..n], b"ping");

            server.send_to(b"pong", from).await.expect("send_to reply");
            let mut buf2 = [0u8; 16];
            let n2 = client.recv(&mut buf2).await.expect("recv");
            assert_eq!(&buf2[..n2], b"pong");
        });
    }
}
