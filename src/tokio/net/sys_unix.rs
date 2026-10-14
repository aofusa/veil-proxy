//! `veil::tokio::net` の OS 依存の呼び出し（unix、libc。F-204 で `net.rs` から移しただけで挙動は同じ）。
//!
//! `mod.rs` の TCP・UDP は OS に依らない下の関数（`recv`・`send`・`connect`・`getopt` 等）だけを使い、
//! Windows では `sys_windows.rs` が同じ名前で Winsock を呼ぶ。Unix ドメインソケット（`unix.rs`）は
//! ここの libc の補助関数（`new_socket`・`sockaddr`・`with_std` 等）を直接使う。

use super::Opt;
use crate::tokio::io::ReadBuf;
use std::io;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};

/// 所有するソケット。
pub(crate) type Owned = OwnedFd;
/// ランタイムの準備待ちに渡すソケットの番号。
pub(crate) type Raw = RawFd;

pub(crate) fn raw(o: &Owned) -> Raw {
    o.as_raw_fd()
}

pub(crate) fn cvt(r: isize) -> io::Result<usize> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r as usize)
    }
}

pub(crate) fn cvt_i(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) const SEND_FLAGS: libc::c_int = libc::MSG_NOSIGNAL;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) const SEND_FLAGS: libc::c_int = 0;

pub(crate) fn sys_recv(fd: RawFd, buf: &mut ReadBuf<'_>, flags: libc::c_int) -> io::Result<usize> {
    // SAFETY: 未初期化領域へ書くだけ。書いた分だけ assume_init する
    let dst = unsafe { buf.unfilled_mut() };
    let n = cvt(unsafe { libc::recv(fd, dst.as_mut_ptr().cast(), dst.len(), flags) })?;
    unsafe { buf.assume_init(n) };
    buf.advance(n);
    Ok(n)
}

pub(crate) fn sys_send(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    cvt(unsafe { libc::send(fd, buf.as_ptr().cast(), buf.len(), SEND_FLAGS) })
}

pub(crate) fn sys_writev(fd: RawFd, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
    let n = bufs.len().min(1024) as libc::c_int;
    cvt(unsafe { libc::writev(fd, bufs.as_ptr().cast(), n) })
}

pub(crate) fn set_nonblocking_cloexec(fd: RawFd) -> io::Result<()> {
    unsafe {
        let fl = cvt_i(libc::fcntl(fd, libc::F_GETFL))?;
        cvt_i(libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK))?;
        let fd_fl = cvt_i(libc::fcntl(fd, libc::F_GETFD))?;
        cvt_i(libc::fcntl(fd, libc::F_SETFD, fd_fl | libc::FD_CLOEXEC))?;
    }
    // SIGPIPE を出さない（MSG_NOSIGNAL の代わり。OpenBSD に SO_NOSIGPIPE は無い）
    #[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "netbsd"))]
    unsafe {
        let one: libc::c_int = 1;
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
    Ok(())
}

pub(crate) fn new_socket(domain: libc::c_int, ty: libc::c_int) -> io::Result<OwnedFd> {
    let fd = cvt_i(unsafe { libc::socket(domain, ty, 0) })?;
    // SAFETY: 作ったばかりの fd
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    set_nonblocking_cloexec(fd)?;
    Ok(owned)
}

// 理由付き allow: BSD・macOS の sockaddr_in には sin_len 等の追加のフィールドがあり、..zeroed() が要る（Linux では不要）
#[allow(clippy::needless_update)]
pub(crate) fn sockaddr(addr: &SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
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

pub(crate) fn sock_err(fd: RawFd) -> io::Result<Option<io::Error>> {
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
pub(crate) fn with_std<T: FromRawFd + IntoRawFd, R>(fd: RawFd, f: impl FnOnce(&T) -> R) -> R {
    // SAFETY: 借りるだけで、終わったら into_raw_fd で手放す
    let s = std::mem::ManuallyDrop::new(unsafe { T::from_raw_fd(fd) });
    f(&s)
}

pub(crate) fn setopt_int(
    fd: RawFd,
    level: libc::c_int,
    opt: libc::c_int,
    v: libc::c_int,
) -> io::Result<()> {
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

pub(crate) fn getopt_int(
    fd: RawFd,
    level: libc::c_int,
    opt: libc::c_int,
) -> io::Result<libc::c_int> {
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

pub(crate) fn get_linger(fd: RawFd) -> io::Result<Option<std::time::Duration>> {
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

pub(crate) fn set_linger_raw(fd: RawFd, dur: Option<std::time::Duration>) -> io::Result<()> {
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

pub(crate) fn accept_fd(fd: RawFd) -> io::Result<(OwnedFd, SocketAddr)> {
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

/// sockaddr_storage を SocketAddr にする。
pub(crate) fn from_sockaddr(ss: &libc::sockaddr_storage) -> io::Result<SocketAddr> {
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

// ===================== OS に依らない入口（mod.rs から使う） =====================

/// std から取り込んだ・accept したソケットの準備（ノンブロッキング・close-on-exec・SIGPIPE 無し）。
pub(crate) fn prepare(fd: Raw) -> io::Result<()> {
    set_nonblocking_cloexec(fd)
}

/// TCP のソケットを作る。
pub(crate) fn new_stream_socket(v6: bool) -> io::Result<Owned> {
    new_socket(
        if v6 { libc::AF_INET6 } else { libc::AF_INET },
        libc::SOCK_STREAM,
    )
}

pub(crate) fn recv_buf(fd: Raw, buf: &mut ReadBuf<'_>, peek: bool) -> io::Result<usize> {
    sys_recv(fd, buf, if peek { libc::MSG_PEEK } else { 0 })
}

/// `dst` へ読む（未初期化でよい）。
pub(crate) fn recv_into(fd: Raw, dst: *mut u8, len: usize) -> io::Result<usize> {
    cvt(unsafe { libc::recv(fd, dst.cast(), len, 0) })
}

pub(crate) fn send(fd: Raw, buf: &[u8]) -> io::Result<usize> {
    sys_send(fd, buf)
}

pub(crate) fn writev(fd: Raw, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
    sys_writev(fd, bufs)
}

pub(crate) fn readv(fd: Raw, bufs: &mut [io::IoSliceMut<'_>]) -> io::Result<usize> {
    cvt(unsafe {
        libc::readv(
            fd,
            bufs.as_mut_ptr().cast(),
            bufs.len().min(1024) as libc::c_int,
        )
    })
}

/// 非ブロッキングの connect を始める（進行中は `Ok`。結果は書き込み可能になってから `sock_err` で見る）。
pub(crate) fn connect(fd: Raw, addr: &SocketAddr) -> io::Result<()> {
    let (ss, len) = sockaddr(addr);
    let r = unsafe { libc::connect(fd, &ss as *const _ as *const libc::sockaddr, len) };
    if r < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINPROGRESS) && e.kind() != io::ErrorKind::WouldBlock {
            return Err(e);
        }
    }
    Ok(())
}

pub(crate) fn bind(fd: Raw, addr: &SocketAddr) -> io::Result<()> {
    let (ss, len) = sockaddr(addr);
    cvt_i(unsafe { libc::bind(fd, &ss as *const _ as *const libc::sockaddr, len) }).map(drop)
}

pub(crate) fn listen(fd: Raw, backlog: u32) -> io::Result<()> {
    cvt_i(unsafe { libc::listen(fd, backlog.min(i32::MAX as u32) as libc::c_int) }).map(drop)
}

pub(crate) fn accept(fd: Raw) -> io::Result<(Owned, SocketAddr)> {
    accept_fd(fd)
}

/// `recvfrom`（`dst` は未初期化でよい。`peek` は `MSG_PEEK`）。
pub(crate) fn recv_from_into(
    fd: Raw,
    dst: *mut u8,
    len: usize,
    peek: bool,
) -> io::Result<(usize, SocketAddr)> {
    let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut sl = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let n = cvt(unsafe {
        libc::recvfrom(
            fd,
            dst.cast(),
            len,
            if peek { libc::MSG_PEEK } else { 0 },
            &mut ss as *mut _ as *mut libc::sockaddr,
            &mut sl,
        )
    })?;
    Ok((n, from_sockaddr(&ss)?))
}

fn opt_name(opt: Opt) -> (libc::c_int, libc::c_int) {
    match opt {
        Opt::ReuseAddr => (libc::SOL_SOCKET, libc::SO_REUSEADDR),
        Opt::ReusePort => (libc::SOL_SOCKET, libc::SO_REUSEPORT),
        Opt::KeepAlive => (libc::SOL_SOCKET, libc::SO_KEEPALIVE),
        Opt::NoDelay => (libc::IPPROTO_TCP, libc::TCP_NODELAY),
        Opt::SndBuf => (libc::SOL_SOCKET, libc::SO_SNDBUF),
        Opt::RcvBuf => (libc::SOL_SOCKET, libc::SO_RCVBUF),
        Opt::Tos => (libc::IPPROTO_IP, libc::IP_TOS),
        Opt::TClass => (libc::IPPROTO_IPV6, libc::IPV6_TCLASS),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        Opt::QuickAck => (libc::IPPROTO_TCP, libc::TCP_QUICKACK),
    }
}

pub(crate) fn getopt(fd: Raw, opt: Opt) -> io::Result<i32> {
    let (level, name) = opt_name(opt);
    getopt_int(fd, level, name)
}

pub(crate) fn setopt(fd: Raw, opt: Opt, v: i32) -> io::Result<()> {
    let (level, name) = opt_name(opt);
    setopt_int(fd, level, name, v)
}

pub(crate) fn linger(fd: Raw) -> io::Result<Option<std::time::Duration>> {
    get_linger(fd)
}

pub(crate) fn set_linger(fd: Raw, dur: Option<std::time::Duration>) -> io::Result<()> {
    set_linger_raw(fd, dur)
}

/// 今の状態（相手が閉じた・切れた・エラー）を待たずに見る（`(read_closed, hup, error)`）。
pub(crate) fn closed_state(fd: Raw) -> (bool, bool, bool) {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let rdhup = libc::POLLRDHUP;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let rdhup = 0;
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN | rdhup,
        revents: 0,
    };
    if unsafe { libc::poll(&mut pfd, 1, 0) } > 0 {
        (
            pfd.revents & (rdhup | libc::POLLHUP) != 0,
            pfd.revents & libc::POLLHUP != 0,
            pfd.revents & libc::POLLERR != 0,
        )
    } else {
        (false, false, false)
    }
}

/// `shutdown` の後始末（unix はローカルの shutdown で待ちが起きるので何もしない）。
pub(crate) fn after_shutdown(_fd: Raw, _how: std::net::Shutdown) {}
