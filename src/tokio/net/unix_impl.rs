//! Unix ドメインソケット・`AsyncFd`（unix だけ。F-204 で `net.rs` から移しただけ）。

use super::sys::{cvt, cvt_i, new_socket, set_nonblocking_cloexec, sys_recv};
use super::*;
use std::os::fd::OwnedFd;

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
