//! `veil::tokio::net` の OS 依存の呼び出し（Windows、Winsock。F-204）。
//!
//! `sys_unix.rs` と同じ名前・同じ意味の関数を Winsock で書く。エラーは `WSAGetLastError()`
//! （`io::Error::from_raw_os_error` は WSA のコードも `WouldBlock` 等へ写す）。準備待ちは veil の
//! reactor（WSAPoll）。ソケットの作成は reactor の `create_nonblocking_socket`（`WSAStartup` 済み・FIONBIO）。

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

/// 所有するソケット。
pub(crate) type Owned = OwnedSocket;
/// ランタイムの準備待ちに渡すソケットの番号（`runtime::handle::RawFd` = `isize`）。
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

/// 1 回の呼び出しで渡す長さ（Winsock は `i32`）。
fn clamp_len(len: usize) -> i32 {
    len.min(i32::MAX as usize) as i32
}

/// ソケットを std の型として借りる（アドレスの取得・ソケットオプション用。閉じない）。
pub(crate) fn with_std<T: FromRawSocket + IntoRawSocket, R>(fd: Raw, f: impl FnOnce(&T) -> R) -> R {
    // SAFETY: 借りるだけで、終わったら forget で手放す
    let s = std::mem::ManuallyDrop::new(unsafe { T::from_raw_socket(fd as RawSocket) });
    f(&s)
}

/// std から取り込んだ・accept したソケットの準備（ノンブロッキング。Windows に close-on-exec・SIGPIPE は無い）。
pub(crate) fn prepare(fd: Raw) -> io::Result<()> {
    let mut on: u32 = 1;
    cvt(unsafe { ws::ioctlsocket(sock(fd), FIONBIO, &mut on) }).map(drop)
}

/// TCP のソケットを作る。
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

/// `dst` へ読む（未初期化でよい）。
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

/// 非ブロッキングの connect を始める（進行中は `Ok`。結果は書き込み可能になってから `sock_err` で見る。
/// WSAPoll は失敗した connect を `POLLHUP|POLLERR` で返すことがあるので、`SO_ERROR` を必ず読む）。
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

/// `recvfrom`（`dst` は未初期化でよい。`peek` は `MSG_PEEK`）。
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

/// 今の状態（相手が閉じた・切れた・エラー）を待たずに見る（`(read_closed, hup, error)`）。
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

/// `shutdown` の後始末: WSAPoll はローカルの `shutdown(SD_RECEIVE)` を通知しないので、読み取りの待ち手を
/// 起こして `recv` をやり直させる（B-88。reactor の `TcpStream::shutdown` と同じ）。
pub(crate) fn after_shutdown(fd: Raw, how: std::net::Shutdown) {
    if how != std::net::Shutdown::Write && crate::runtime::has_driver() {
        crate::runtime::executor::wake_all_readers(fd);
    }
}
