// F-204
//! OS-specific calls for `veil::tokio::net` (Windows, Winsock).
//!
//! These are functions with the same names and meaning as in `sys_unix.rs`, written with Winsock. Errors come from
//! `WSAGetLastError()` (`io::Error::from_raw_os_error` also maps WSA codes to `WouldBlock` and so on). Readiness waits
//! use veil's reactor (WSAPoll). Sockets are created with the reactor's `create_nonblocking_socket` (`WSAStartup`
//! already done, FIONBIO set).

use super::Opt;
use crate::runtime::handle::win;
use crate::tokio::io::ReadBuf;
use std::io;
use std::net::SocketAddr;
use std::os::windows::io::{AsRawSocket, FromRawSocket, IntoRawSocket, OwnedSocket, RawSocket};
use windows_sys::Win32::Networking::WinSock::{
    self as ws, AF_INET, AF_INET6, FIONBIO, INVALID_SOCKET, IPPROTO_IP, IPPROTO_IPV6, IPPROTO_TCP,
    IPV6_TCLASS, IP_TOS, LINGER, MSG_PEEK, POLLERR, POLLHUP, POLLRDNORM, SOCKADDR, SOCKADDR_IN,
    SOCKADDR_IN6, SOCKADDR_STORAGE, SOCKET_ERROR, SOL_SOCKET, SO_ERROR, SO_KEEPALIVE, SO_LINGER,
    SO_RCVBUF, SO_REUSEADDR, SO_SNDBUF, TCP_NODELAY, WSABUF, WSAEINPROGRESS, WSAEMSGSIZE,
    WSAPOLLFD,
};

/// An owned socket.
pub(crate) type Owned = OwnedSocket;
/// The socket number passed to the runtime's readiness wait (`runtime::handle::RawFd` = `isize`).
pub(crate) type Raw = crate::runtime::handle::RawFd;

pub(crate) fn raw(o: &Owned) -> Raw {
    o.as_raw_socket() as Raw
}

fn sock(fd: Raw) -> ws::SOCKET {
    win::to_socket(fd)
}

fn last_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { ws::WSAGetLastError() })
}

fn cvt(r: i32) -> io::Result<i32> {
    if r == SOCKET_ERROR {
        Err(last_error())
    } else {
        Ok(r)
    }
}

/// The length passed in one call (Winsock uses `i32`).
fn clamp_len(len: usize) -> i32 {
    len.min(i32::MAX as usize) as i32
}

/// Borrows a socket as a std type (for address queries and socket options; it is not closed).
pub(crate) fn with_std<T: FromRawSocket + IntoRawSocket, R>(fd: Raw, f: impl FnOnce(&T) -> R) -> R {
    // SAFETY: 借りるだけで、終わったら forget で手放す
    let s = std::mem::ManuallyDrop::new(unsafe { T::from_raw_socket(fd as RawSocket) });
    f(&s)
}

/// Prepares a socket adopted from std or accepted (non-blocking; Windows has no close-on-exec or SIGPIPE).
pub(crate) fn prepare(fd: Raw) -> io::Result<()> {
    let mut on: u32 = 1;
    cvt(unsafe { ws::ioctlsocket(sock(fd), FIONBIO, &mut on) }).map(drop)
}

/// Creates a TCP socket.
pub(crate) fn new_stream_socket(v6: bool) -> io::Result<Owned> {
    let fd = crate::runtime::tcp::create_nonblocking_socket(if v6 {
        AF_INET6 as i32
    } else {
        AF_INET as i32
    })?;
    // SAFETY: 作ったばかりのソケット
    Ok(unsafe { OwnedSocket::from_raw_socket(fd as RawSocket) })
}

pub(crate) fn recv_buf(fd: Raw, buf: &mut ReadBuf<'_>, peek: bool) -> io::Result<usize> {
    // SAFETY: 未初期化領域へ書くだけ。書いた分だけ assume_init する
    let dst = unsafe { buf.unfilled_mut() };
    let n = cvt(unsafe {
        ws::recv(
            sock(fd),
            dst.as_mut_ptr().cast(),
            clamp_len(dst.len()),
            if peek { MSG_PEEK } else { 0 },
        )
    })? as usize;
    unsafe { buf.assume_init(n) };
    buf.advance(n);
    Ok(n)
}

/// Receives into `dst` (which may be uninitialised).
pub(crate) fn recv_into(fd: Raw, dst: *mut u8, len: usize) -> io::Result<usize> {
    cvt(unsafe { ws::recv(sock(fd), dst, clamp_len(len), 0) }).map(|n| n as usize)
}

pub(crate) fn send(fd: Raw, buf: &[u8]) -> io::Result<usize> {
    cvt(unsafe { ws::send(sock(fd), buf.as_ptr(), clamp_len(buf.len()), 0) }).map(|n| n as usize)
}

// std の `IoSlice` / `IoSliceMut` は Windows では `WSABUF` と同じ配置（std が保証する）。
pub(crate) fn writev(fd: Raw, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
    let mut sent: u32 = 0;
    cvt(unsafe {
        ws::WSASend(
            sock(fd),
            bufs.as_ptr() as *const WSABUF,
            bufs.len().min(1024) as u32,
            &mut sent,
            0,
            std::ptr::null_mut(),
            None,
        )
    })?;
    Ok(sent as usize)
}

pub(crate) fn readv(fd: Raw, bufs: &mut [io::IoSliceMut<'_>]) -> io::Result<usize> {
    let mut got: u32 = 0;
    let mut flags: u32 = 0;
    cvt(unsafe {
        ws::WSARecv(
            sock(fd),
            bufs.as_mut_ptr() as *const WSABUF,
            bufs.len().min(1024) as u32,
            &mut got,
            &mut flags,
            std::ptr::null_mut(),
            None,
        )
    })?;
    Ok(got as usize)
}

fn to_storage(addr: &SocketAddr) -> (SOCKADDR_STORAGE, i32) {
    // SAFETY: ゼロ初期化した SOCKADDR_STORAGE に書くだけ
    let mut ss: SOCKADDR_STORAGE = unsafe { std::mem::zeroed() };
    let len = match addr {
        SocketAddr::V4(a) => {
            let mut sin: SOCKADDR_IN = unsafe { std::mem::zeroed() };
            sin.sin_family = AF_INET;
            sin.sin_port = a.port().to_be();
            sin.sin_addr.S_un.S_addr = u32::from_ne_bytes(a.ip().octets());
            unsafe { std::ptr::write(&mut ss as *mut _ as *mut SOCKADDR_IN, sin) };
            std::mem::size_of::<SOCKADDR_IN>()
        }
        SocketAddr::V6(a) => {
            let mut sin6: SOCKADDR_IN6 = unsafe { std::mem::zeroed() };
            sin6.sin6_family = AF_INET6;
            sin6.sin6_port = a.port().to_be();
            sin6.sin6_flowinfo = a.flowinfo();
            sin6.sin6_addr.u.Byte = a.ip().octets();
            sin6.Anonymous.sin6_scope_id = a.scope_id();
            unsafe { std::ptr::write(&mut ss as *mut _ as *mut SOCKADDR_IN6, sin6) };
            std::mem::size_of::<SOCKADDR_IN6>()
        }
    };
    (ss, len as i32)
}

fn from_storage(ss: &SOCKADDR_STORAGE) -> io::Result<SocketAddr> {
    match ss.ss_family {
        AF_INET => {
            // SAFETY: family が AF_INET
            let a = unsafe { &*(ss as *const _ as *const SOCKADDR_IN) };
            Ok(SocketAddr::new(
                std::net::Ipv4Addr::from(u32::from_be(unsafe { a.sin_addr.S_un.S_addr })).into(),
                u16::from_be(a.sin_port),
            ))
        }
        AF_INET6 => {
            // SAFETY: family が AF_INET6
            let a = unsafe { &*(ss as *const _ as *const SOCKADDR_IN6) };
            Ok(SocketAddr::V6(std::net::SocketAddrV6::new(
                std::net::Ipv6Addr::from(unsafe { a.sin6_addr.u.Byte }),
                u16::from_be(a.sin6_port),
                a.sin6_flowinfo,
                unsafe { a.Anonymous.sin6_scope_id },
            )))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported address family",
        )),
    }
}

/// Starts a non-blocking connect (`Ok` while in progress; check the result with `sock_err` once it is writable).
/// WSAPoll can report a failed connect as `POLLHUP|POLLERR`, so `SO_ERROR` is always read.
pub(crate) fn connect(fd: Raw, addr: &SocketAddr) -> io::Result<()> {
    let (ss, len) = to_storage(addr);
    let r = unsafe { ws::connect(sock(fd), &ss as *const _ as *const SOCKADDR, len) };
    if r == SOCKET_ERROR {
        let e = last_error();
        if e.raw_os_error() != Some(WSAEINPROGRESS) && e.kind() != io::ErrorKind::WouldBlock {
            return Err(e);
        }
    }
    Ok(())
}

pub(crate) fn bind(fd: Raw, addr: &SocketAddr) -> io::Result<()> {
    let (ss, len) = to_storage(addr);
    cvt(unsafe { ws::bind(sock(fd), &ss as *const _ as *const SOCKADDR, len) }).map(drop)
}

pub(crate) fn listen(fd: Raw, backlog: u32) -> io::Result<()> {
    cvt(unsafe { ws::listen(sock(fd), backlog.min(i32::MAX as u32) as i32) }).map(drop)
}

pub(crate) fn accept(fd: Raw) -> io::Result<(Owned, SocketAddr)> {
    let mut ss: SOCKADDR_STORAGE = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<SOCKADDR_STORAGE>() as i32;
    let s = unsafe { ws::accept(sock(fd), &mut ss as *mut _ as *mut SOCKADDR, &mut len) };
    if s == INVALID_SOCKET {
        return Err(last_error());
    }
    // SAFETY: accept が返したソケット
    let owned = unsafe { OwnedSocket::from_raw_socket(s as RawSocket) };
    prepare(raw(&owned))?;
    Ok((owned, from_storage(&ss)?))
}

/// `recvfrom` (`dst` may be uninitialised; `peek` uses `MSG_PEEK`).
pub(crate) fn recv_from_into(
    fd: Raw,
    dst: *mut u8,
    len: usize,
    peek: bool,
) -> io::Result<(usize, SocketAddr)> {
    let mut ss: SOCKADDR_STORAGE = unsafe { std::mem::zeroed() };
    let mut sl = std::mem::size_of::<SOCKADDR_STORAGE>() as i32;
    let r = unsafe {
        ws::recvfrom(
            sock(fd),
            dst,
            clamp_len(len),
            if peek { MSG_PEEK } else { 0 },
            &mut ss as *mut _ as *mut SOCKADDR,
            &mut sl,
        )
    };
    match cvt(r) {
        Ok(n) => Ok((n as usize, from_storage(&ss)?)),
        // 覗くだけで、データグラムがバッファより大きい（送り主だけを知りたい 0 バイトの peek 等）: Windows は
        // WSAEMSGSIZE を返すが送り主は埋まっている（socket2 の peek_sender と同じ扱い）
        Err(e) if peek && e.raw_os_error() == Some(WSAEMSGSIZE) => Ok((len, from_storage(&ss)?)),
        Err(e) => Err(e),
    }
}

fn opt_name(opt: Opt) -> (i32, i32) {
    match opt {
        Opt::ReuseAddr => (SOL_SOCKET, SO_REUSEADDR),
        Opt::KeepAlive => (SOL_SOCKET, SO_KEEPALIVE),
        Opt::NoDelay => (IPPROTO_TCP, TCP_NODELAY),
        Opt::SndBuf => (SOL_SOCKET, SO_SNDBUF),
        Opt::RcvBuf => (SOL_SOCKET, SO_RCVBUF),
        Opt::Tos => (IPPROTO_IP, IP_TOS),
        Opt::TClass => (IPPROTO_IPV6, IPV6_TCLASS),
    }
}

pub(crate) fn getopt(fd: Raw, opt: Opt) -> io::Result<i32> {
    let (level, name) = opt_name(opt);
    let mut v: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as i32;
    cvt(unsafe { ws::getsockopt(sock(fd), level, name, &mut v as *mut _ as *mut u8, &mut len) })?;
    // BOOL のオプション（TCP_NODELAY 等）は 1 バイトしか書かれないことがある
    if len == 1 {
        v &= 0xff;
    }
    Ok(v)
}

pub(crate) fn setopt(fd: Raw, opt: Opt, v: i32) -> io::Result<()> {
    let (level, name) = opt_name(opt);
    cvt(unsafe {
        ws::setsockopt(
            sock(fd),
            level,
            name,
            &v as *const _ as *const u8,
            std::mem::size_of::<i32>() as i32,
        )
    })
    .map(drop)
}

pub(crate) fn sock_err(fd: Raw) -> io::Result<Option<io::Error>> {
    let mut err: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as i32;
    cvt(unsafe {
        ws::getsockopt(
            sock(fd),
            SOL_SOCKET,
            SO_ERROR,
            &mut err as *mut _ as *mut u8,
            &mut len,
        )
    })?;
    Ok((err != 0).then(|| io::Error::from_raw_os_error(err)))
}

pub(crate) fn linger(fd: Raw) -> io::Result<Option<std::time::Duration>> {
    let mut l: LINGER = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<LINGER>() as i32;
    cvt(unsafe {
        ws::getsockopt(
            sock(fd),
            SOL_SOCKET,
            SO_LINGER,
            &mut l as *mut _ as *mut u8,
            &mut len,
        )
    })?;
    Ok((l.l_onoff != 0).then(|| std::time::Duration::from_secs(l.l_linger as u64)))
}

pub(crate) fn set_linger(fd: Raw, dur: Option<std::time::Duration>) -> io::Result<()> {
    let l = LINGER {
        l_onoff: dur.is_some() as u16,
        l_linger: dur
            .map(|d| d.as_secs().min(u16::MAX as u64) as u16)
            .unwrap_or(0),
    };
    cvt(unsafe {
        ws::setsockopt(
            sock(fd),
            SOL_SOCKET,
            SO_LINGER,
            &l as *const _ as *const u8,
            std::mem::size_of::<LINGER>() as i32,
        )
    })
    .map(drop)
}

/// Reads the current state (peer closed, hung up, error) without waiting, as `(read_closed, hup, error)`.
pub(crate) fn closed_state(fd: Raw) -> (bool, bool, bool) {
    let mut pfd = WSAPOLLFD {
        fd: sock(fd),
        events: POLLRDNORM,
        revents: 0,
    };
    if unsafe { ws::WSAPoll(&mut pfd, 1, 0) } > 0 {
        (
            pfd.revents & POLLHUP != 0,
            pfd.revents & POLLHUP != 0,
            pfd.revents & POLLERR != 0,
        )
    } else {
        (false, false, false)
    }
}

// B-88: reactor の `TcpStream::shutdown` と同じ。
/// Cleanup after `shutdown`. WSAPoll does not report a local `shutdown(SD_RECEIVE)`, so the readers
/// are woken to retry `recv`.
pub(crate) fn after_shutdown(fd: Raw, how: std::net::Shutdown) {
    if how != std::net::Shutdown::Write && crate::runtime::has_driver() {
        crate::runtime::executor::wake_all_readers(fd);
    }
}
