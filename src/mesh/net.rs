//! 透過転送のネットワーク部分（F-216）。
//!
//! - iptables の REDIRECT で曲げられた接続の元の宛先は `getsockopt(SO_ORIGINAL_DST)`（接続ごとに 1 本。
//!   Envoy の `original_dst` リスナーフィルタと同じ。ブロックしない syscall で、io_uring のオペコードは増やさない）。
//! - 送信元を指定した上流接続（Istio の inbound は 127.0.0.6 から。iptables の除外条件）は、非ブロッキングの
//!   `connect(2)` と書き込み可能待ち（既存の `wait_writable_fd`）で行う（ランタイムの `Connect` は変えない）。

use crate::runtime::handle::AsRawFd;
use crate::runtime::tcp::TcpStream;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

const SO_ORIGINAL_DST: libc::c_int = 80;
const IP6T_SO_ORIGINAL_DST: libc::c_int = 80;

/// 接続の元の宛先（REDIRECT されていなければ自分のアドレスが返る）。
pub fn original_dst(fd: libc::c_int) -> Option<SocketAddr> {
    // SAFETY: sockaddr_storage は sockaddr_in / sockaddr_in6 を収められる。len はその大きさ。
    unsafe {
        let mut storage: libc::sockaddr_storage = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let r = libc::getsockopt(
            fd,
            libc::SOL_IP,
            SO_ORIGINAL_DST,
            &mut storage as *mut _ as *mut libc::c_void,
            &mut len,
        );
        if r == 0 {
            if let Some(a) = storage_to_addr(&storage) {
                return Some(a);
            }
        }
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let r = libc::getsockopt(
            fd,
            libc::SOL_IPV6,
            IP6T_SO_ORIGINAL_DST,
            &mut storage as *mut _ as *mut libc::c_void,
            &mut len,
        );
        if r == 0 {
            return storage_to_addr(&storage);
        }
    }
    None
}

/// 接続のローカルアドレス（`getsockname`）。
pub fn local_addr(fd: libc::c_int) -> Option<SocketAddr> {
    // SAFETY: 同上
    unsafe {
        let mut storage: libc::sockaddr_storage = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        if libc::getsockname(fd, &mut storage as *mut _ as *mut libc::sockaddr, &mut len) == 0 {
            return storage_to_addr(&storage);
        }
    }
    None
}

fn storage_to_addr(s: &libc::sockaddr_storage) -> Option<SocketAddr> {
    match s.ss_family as libc::c_int {
        libc::AF_INET => {
            // SAFETY: ss_family が AF_INET なので sockaddr_in として読める
            let sin = unsafe { &*(s as *const _ as *const libc::sockaddr_in) };
            let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            Some(SocketAddr::new(IpAddr::V4(ip), u16::from_be(sin.sin_port)))
        }
        libc::AF_INET6 => {
            // SAFETY: ss_family が AF_INET6 なので sockaddr_in6 として読める
            let sin6 = unsafe { &*(s as *const _ as *const libc::sockaddr_in6) };
            let ip = Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            Some(SocketAddr::new(
                IpAddr::V6(ip),
                u16::from_be(sin6.sin6_port),
            ))
        }
        _ => None,
    }
}

fn addr_to_storage(addr: &SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: ゼロ初期化した sockaddr_storage に sockaddr_in / sockaddr_in6 を書く
    unsafe {
        let mut storage: libc::sockaddr_storage = std::mem::zeroed();
        match addr {
            SocketAddr::V4(a) => {
                let sin = &mut *(&mut storage as *mut _ as *mut libc::sockaddr_in);
                sin.sin_family = libc::AF_INET as libc::sa_family_t;
                sin.sin_port = a.port().to_be();
                sin.sin_addr.s_addr = u32::from(*a.ip()).to_be();
                (
                    storage,
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            }
            SocketAddr::V6(a) => {
                let sin6 = &mut *(&mut storage as *mut _ as *mut libc::sockaddr_in6);
                sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                sin6.sin6_port = a.port().to_be();
                sin6.sin6_addr.s6_addr = a.ip().octets();
                sin6.sin6_scope_id = a.scope_id();
                (
                    storage,
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            }
        }
    }
}

/// 上流へ接続する（`bind` があれば送信元をその IP にする）。
pub async fn connect(
    addr: SocketAddr,
    bind: Option<SocketAddr>,
    timeout: Duration,
) -> io::Result<TcpStream> {
    let fut = async {
        let Some(bind) = bind else {
            return TcpStream::connect(addr).await;
        };
        connect_bound(addr, bind).await
    };
    match crate::runtime::timeout(timeout, fut).await {
        Ok(r) => {
            let s = r?;
            let _ = s.set_nodelay(true);
            Ok(s)
        }
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "connect timeout")),
    }
}

/// 閉じるときに fd を閉じる（接続途中の失敗用）。
struct FdGuard(libc::c_int);

impl Drop for FdGuard {
    fn drop(&mut self) {
        if self.0 >= 0 {
            // SAFETY: 自分が作った fd
            unsafe { libc::close(self.0) };
        }
    }
}

async fn connect_bound(addr: SocketAddr, bind: SocketAddr) -> io::Result<TcpStream> {
    let domain = if addr.is_ipv6() {
        libc::AF_INET6
    } else {
        libc::AF_INET
    };
    // SAFETY: 新しいソケットを作るだけ
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
    let mut guard = FdGuard(fd);
    // 送信元のポートは自動（同じ IP からの接続が多いので IP_BIND_ADDRESS_NO_PORT で枯渇を避ける）
    let one: libc::c_int = 1;
    // SAFETY: 有効な fd と int のオプション
    unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_IP,
            libc::IP_BIND_ADDRESS_NO_PORT,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        libc::setsockopt(
            fd,
            libc::IPPROTO_IP,
            libc::IP_FREEBIND,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
    let (bs, bl) = addr_to_storage(&bind);
    // SAFETY: storage は bl バイトの有効な sockaddr
    if unsafe { libc::bind(fd, &bs as *const _ as *const libc::sockaddr, bl) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (cs, cl) = addr_to_storage(&addr);
    // SAFETY: 同上
    let r = unsafe { libc::connect(fd, &cs as *const _ as *const libc::sockaddr, cl) };
    if r != 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(e);
        }
        crate::runtime::tcp::wait_writable_fd(fd).await?;
        let mut err: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: SO_ERROR は int
        unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &mut err as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if err != 0 {
            return Err(io::Error::from_raw_os_error(err));
        }
    }
    guard.0 = -1;
    // SAFETY: 接続済みのソケット fd の所有権を TcpStream へ移す
    Ok(unsafe { TcpStream::from_raw_fd(fd) })
}

/// ストリームの fd（`AsRawFd` の別名。ランタイムの型差を吸収）。
pub fn fd_of<S: AsRawFd>(s: &S) -> libc::c_int {
    s.as_raw_fd() as libc::c_int
}

#[cfg(test)]
// 理由付き allow: テストは同期 I/O（std::net・std::fs）で前提を作る（データプレーンの外）。
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn storage_roundtrip() {
        for a in ["127.0.0.6:0", "10.1.2.3:8080", "[::1]:443"] {
            let a: SocketAddr = a.parse().unwrap();
            let (s, _) = addr_to_storage(&a);
            assert_eq!(storage_to_addr(&s), Some(a));
        }
    }

    #[test]
    fn original_dst_of_plain_socket_is_none_or_self() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let c = std::net::TcpStream::connect(addr).unwrap();
        let (s, _) = l.accept().unwrap();
        use std::os::fd::AsRawFd as _;
        // REDIRECT されていない接続では conntrack が無く失敗するか、自分の宛先が返る
        if let Some(d) = original_dst(s.as_raw_fd()) {
            assert_eq!(d, addr);
        }
        assert_eq!(local_addr(s.as_raw_fd()), Some(addr));
        drop(c);
    }
}
