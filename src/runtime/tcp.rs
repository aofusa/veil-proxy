//! 非同期 TcpListener / TcpStream
//!
//! io_uring の ACCEPT / READ / WRITE / CONNECT オペコードを使用した
//! 非同期 TCP ソケット実装。monoio の同名型との API 互換を持つ。
//!
//! ## 設計
//!
//! - 所有権ベース I/O: `read(buf)` / `write(buf)` はバッファの所有権を取り、
//!   完了時に返却する（io_uring のゼロコピーモデルに合わせた設計）
//! - `readable()` / `writable()` は POLL_ADD を使用して準備完了を待つ
//! - ノンブロッキングソケットを使用し、libc::read/write と組み合わせて使用可能

#![allow(dead_code)]

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::os::unix::io::{AsRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::runtime::buf::{IoBuf, IoBufMut};
use crate::runtime::executor::{
    detach_op, next_user_data, peek_op_result, register_op, remove_op, set_op_waker, submit_sqes,
    take_op_result, with_ring,
};
use crate::runtime::ring::{
    IORING_OP_ACCEPT, IORING_OP_CONNECT, IORING_OP_POLL_ADD, IORING_OP_RECV, IORING_OP_SEND,
};

// POLL イベントフラグ
const POLLIN: i16 = 0x0001;
const POLLOUT: i16 = 0x0004;

// SO_* ソケットオプション
const TCP_NODELAY: libc::c_int = 1;

// ====================
// ソケットアドレス変換ユーティリティ
// ====================

/// SocketAddr を libc::sockaddr_storage に変換する
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

/// libc::sockaddr_storage を SocketAddr に変換する
fn storage_to_sockaddr(storage: &libc::sockaddr_storage) -> io::Result<SocketAddr> {
    match storage.ss_family as libc::c_int {
        libc::AF_INET => {
            let sin = unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
            // s_addr はネットワークバイトオーダ。Ipv4Addr::from(u32) はホストオーダの u32 を
            // a.b.c.d として解釈するため from_be 後の値をそのまま渡す。以前は
            // from_be(...).to_ne_bytes() で二重変換して IP が逆順（例: 127.0.0.1 → 1.0.0.127）
            // になっていた。これは accept が返すクライアント IP を破壊し、IP ハッシュ・
            // source_ip 条件・アクセスログ等を誤らせる重大バグだった。
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
        family => Err(io::Error::new(
            io::ErrorKind::Other,
            format!("unsupported address family: {}", family),
        )),
    }
}

/// ノンブロッキングソケットを作成する
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

// ====================
// TcpListener
// ====================

/// 非同期 TCP リスナー
pub struct TcpListener {
    fd: RawFd,
}

impl TcpListener {
    /// アドレスにバインドしてリッスンを開始する
    pub fn bind(addr: impl std::net::ToSocketAddrs) -> io::Result<Self> {
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

        // SO_REUSEADDR
        let optval: libc::c_int = 1;
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }

        let (storage, len) = sockaddr_to_storage(&addr);
        let ret = unsafe { libc::bind(fd, &storage as *const _ as *const libc::sockaddr, len) };
        if ret < 0 {
            unsafe { libc::close(fd) };
            return Err(io::Error::last_os_error());
        }

        let ret = unsafe { libc::listen(fd, 128) };
        if ret < 0 {
            unsafe { libc::close(fd) };
            return Err(io::Error::last_os_error());
        }

        Ok(Self { fd })
    }

    /// SO_REUSEPORT を設定してバインドする
    pub fn bind_reuse_port(addr: impl std::net::ToSocketAddrs) -> io::Result<Self> {
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
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEPORT,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }

        let (storage, len) = sockaddr_to_storage(&addr);
        let ret = unsafe { libc::bind(fd, &storage as *const _ as *const libc::sockaddr, len) };
        if ret < 0 {
            unsafe { libc::close(fd) };
            return Err(io::Error::last_os_error());
        }

        let ret = unsafe { libc::listen(fd, 128) };
        if ret < 0 {
            unsafe { libc::close(fd) };
            return Err(io::Error::last_os_error());
        }

        Ok(Self { fd })
    }

    /// 新しい接続を非同期で受け入れる（io_uring ACCEPT）
    pub fn accept(&self) -> Accept<'_> {
        Accept {
            listener_fd: self.fd,
            user_data: 0,
            addr_storage: Box::new(unsafe { std::mem::zeroed() }),
            addr_len: Box::new(std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t),
            submitted: false,
            _marker: std::marker::PhantomData,
        }
    }

    /// ローカルアドレスを取得する
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

/// accept Future（IORING_OP_ACCEPT）
pub struct Accept<'a> {
    listener_fd: RawFd,
    user_data: u64,
    addr_storage: Box<libc::sockaddr_storage>,
    addr_len: Box<libc::socklen_t>,
    submitted: bool,
    _marker: std::marker::PhantomData<&'a TcpListener>,
}

impl<'a> Future for Accept<'a> {
    type Output = io::Result<(TcpStream, SocketAddr)>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.submitted {
            let user_data = next_user_data();
            self.user_data = user_data;
            register_op(user_data);

            let listener_fd = self.listener_fd;
            let addr_ptr = self.addr_storage.as_mut() as *mut libc::sockaddr_storage;
            let addr_len_ptr = self.addr_len.as_mut() as *mut libc::socklen_t;

            with_ring(|ring| {
                if let Some(sqe) = ring.get_sqe() {
                    sqe.opcode = IORING_OP_ACCEPT;
                    sqe.fd = listener_fd;
                    sqe.addr_or_splice_off_in = addr_ptr as u64;
                    sqe.off_or_addr2 = addr_len_ptr as u64;
                    sqe.op_flags = (libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC) as u32;
                    sqe.user_data = user_data;
                }
            });

            if let Err(e) = submit_sqes() {
                remove_op(user_data);
                return Poll::Ready(Err(e));
            }

            self.submitted = true;
        }

        match peek_op_result(self.user_data) {
            Some(res) => {
                take_op_result(self.user_data);
                if res < 0 {
                    Poll::Ready(Err(io::Error::from_raw_os_error(-res)))
                } else {
                    let fd = res;
                    let peer_addr = storage_to_sockaddr(&self.addr_storage)?;
                    Poll::Ready(Ok((TcpStream { fd }, peer_addr)))
                }
            }
            None => {
                set_op_waker(self.user_data, cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl Drop for Accept<'_> {
    fn drop(&mut self) {
        // 提出済みかつ未 take（タイムアウト等で in-flight のまま drop された）の場合、
        // カーネルは addr バッファに書き込む可能性があるため、バッファを生かしたまま
        // detach し、ASYNC_CANCEL で孤立 accept を除去する。accept が接続を確保済み
        // だった場合は、その fd を後始末でクローズする（接続を奪ったまま放置しない）。
        if self.submitted {
            let storage = std::mem::replace(&mut self.addr_storage, Box::new(unsafe {
                std::mem::zeroed()
            }));
            let len = std::mem::replace(&mut self.addr_len, Box::new(0));
            detach_op(
                self.user_data,
                Box::new(move |res| {
                    // op の完了/キャンセルが確定したのでここで解放する。
                    drop(storage);
                    drop(len);
                    if res >= 0 {
                        // accept が成功して fd を得ていたが Future は消えている → クローズ。
                        unsafe { libc::close(res) };
                    }
                }),
            );
        }
    }
}

// ====================
// TcpStream
// ====================

/// 非同期 TCP ストリーム
pub struct TcpStream {
    pub(crate) fd: RawFd,
}

impl TcpStream {
    /// raw fd から作成する
    ///
    /// # Safety
    /// `fd` は有効なソケット FD であること
    pub unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Self { fd }
    }

    /// アドレスに非同期で接続する（io_uring CONNECT）
    pub fn connect(addr: SocketAddr) -> Connect {
        Connect {
            addr,
            fd: -1,
            user_data: 0,
            addr_storage: Box::new(unsafe { std::mem::zeroed() }),
            addr_len: 0,
            submitted: false,
        }
    }

    /// 文字列アドレス（"host:port"）から接続する
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

    /// バッファに非同期で読み込む（io_uring RECV）
    ///
    /// バッファの所有権を取り、完了時に `(Result<usize>, T)` を返す。
    pub fn read<T: IoBufMut>(&self, buf: T) -> ReadFuture<T> {
        ReadFuture {
            fd: self.fd,
            buf,
            user_data: 0,
            submitted: false,
        }
    }

    /// バッファを非同期で書き込む（io_uring SEND）
    ///
    /// バッファの所有権を取り、完了時に `(Result<usize>, T)` を返す。
    pub fn write<T: IoBuf>(&self, buf: T) -> WriteFuture<T> {
        WriteFuture {
            fd: self.fd,
            buf,
            user_data: 0,
            submitted: false,
        }
    }

    /// 読み取り可能になるまで待つ（POLL_ADD POLLIN）
    pub fn readable(&self) -> Readable<'_> {
        Readable {
            fd: self.fd,
            user_data: 0,
            submitted: false,
            _marker: std::marker::PhantomData,
        }
    }

    /// 書き込み可能になるまで待つ（POLL_ADD POLLOUT）
    pub fn writable(&self) -> Writable<'_> {
        Writable {
            fd: self.fd,
            user_data: 0,
            submitted: false,
            _marker: std::marker::PhantomData,
        }
    }

    /// TCP_NODELAY を設定する
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

    /// ピアアドレスを取得する
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

    /// ローカルアドレスを取得する
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

    /// ソケットをシャットダウンする
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
        // &self メソッドを &mut self 経由で呼ぶ（内部状態を変更しない）
        let fd = self.fd;
        ReadFuture {
            fd,
            buf,
            user_data: 0,
            submitted: false,
        }
    }
}

impl crate::runtime::io::AsyncWriteRent for TcpStream {
    fn write<T: crate::runtime::buf::IoBuf>(
        &mut self,
        buf: T,
    ) -> impl std::future::Future<Output = crate::runtime::io::BufResult<usize, T>> {
        let fd = self.fd;
        WriteFuture {
            fd,
            buf,
            user_data: 0,
            submitted: false,
        }
    }

    fn shutdown(&mut self) -> impl std::future::Future<Output = std::io::Result<()>> {
        // SHUT_RDWR でシャットダウン（同期操作だが async として wrap）
        let result = TcpStream::shutdown(self, std::net::Shutdown::Both);
        async move { result }
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        if self.fd >= 0 {
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

/// connect Future（IORING_OP_CONNECT）
pub struct Connect {
    addr: SocketAddr,
    fd: RawFd,
    user_data: u64,
    addr_storage: Box<libc::sockaddr_storage>,
    addr_len: libc::socklen_t,
    submitted: bool,
}

impl Future for Connect {
    type Output = io::Result<TcpStream>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.submitted {
            // ソケット作成
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

            let user_data = next_user_data();
            self.user_data = user_data;
            register_op(user_data);

            let (storage, len) = sockaddr_to_storage(&self.addr);
            *self.addr_storage = storage;
            self.addr_len = len;

            let addr_ptr = self.addr_storage.as_ref() as *const libc::sockaddr_storage;
            let addr_len = self.addr_len;

            with_ring(|ring| {
                if let Some(sqe) = ring.get_sqe() {
                    sqe.opcode = IORING_OP_CONNECT;
                    sqe.fd = fd;
                    sqe.addr_or_splice_off_in = addr_ptr as u64;
                    sqe.off_or_addr2 = addr_len as u64;
                    sqe.user_data = user_data;
                }
            });

            if let Err(e) = submit_sqes() {
                unsafe { libc::close(fd) };
                remove_op(user_data);
                return Poll::Ready(Err(e));
            }

            self.submitted = true;
        }

        match peek_op_result(self.user_data) {
            Some(res) => {
                take_op_result(self.user_data);
                if res < 0 && res != -libc::EINPROGRESS {
                    let fd = self.fd;
                    self.fd = -1;
                    unsafe { libc::close(fd) };
                    Poll::Ready(Err(io::Error::from_raw_os_error(-res)))
                } else {
                    let fd = self.fd;
                    self.fd = -1; // TcpStream に所有権を移譲
                    Poll::Ready(Ok(TcpStream { fd }))
                }
            }
            None => {
                set_op_waker(self.user_data, cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl Drop for Connect {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
        }
    }
}

// ====================
// Read Future
// ====================

/// 読み込み Future（IORING_OP_RECV）
pub struct ReadFuture<T: IoBufMut> {
    fd: RawFd,
    buf: T,
    user_data: u64,
    submitted: bool,
}

impl<T: IoBufMut> Future for ReadFuture<T> {
    type Output = (io::Result<usize>, T);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: ReadFuture はアンピン可能（ポインタを保持しない）
        let this = unsafe { self.get_unchecked_mut() };

        if !this.submitted {
            let user_data = next_user_data();
            this.user_data = user_data;
            register_op(user_data);

            let fd = this.fd;
            let buf_ptr = this.buf.write_ptr() as u64;
            let buf_len = this.buf.bytes_total() as u32;

            with_ring(|ring| {
                if let Some(sqe) = ring.get_sqe() {
                    sqe.opcode = IORING_OP_RECV;
                    sqe.fd = fd;
                    sqe.addr_or_splice_off_in = buf_ptr;
                    sqe.len = buf_len;
                    sqe.user_data = user_data;
                }
            });

            if let Err(e) = submit_sqes() {
                remove_op(user_data);
                // SAFETY: バッファを返却する（drop しないよう forget）
                let buf = unsafe { std::ptr::read(&this.buf) };
                std::mem::forget(unsafe { std::ptr::read(this) });
                return Poll::Ready((Err(e), buf));
            }

            this.submitted = true;
        }

        match peek_op_result(this.user_data) {
            Some(res) => {
                take_op_result(this.user_data);
                let n = res;

                // buf をムーブアウト
                let buf = unsafe { std::ptr::read(&this.buf) };
                // this は ReadFuture 全体として forget（buf を二重 drop しないよう）
                let ud = this.user_data;
                let fd = this.fd;
                std::mem::forget(unsafe { std::ptr::read(this) });
                let _ = (ud, fd); // suppress warnings

                if n < 0 {
                    Poll::Ready((Err(io::Error::from_raw_os_error(-n)), buf))
                } else {
                    // SAFETY: n バイトが初期化されたことを記録
                    let mut buf = buf;
                    unsafe { buf.set_init(n as usize) };
                    Poll::Ready((Ok(n as usize), buf))
                }
            }
            None => {
                set_op_waker(this.user_data, cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

// ====================
// Write Future
// ====================

/// 書き込み Future（IORING_OP_SEND）
pub struct WriteFuture<T: IoBuf> {
    fd: RawFd,
    buf: T,
    user_data: u64,
    submitted: bool,
}

impl<T: IoBuf> Future for WriteFuture<T> {
    type Output = (io::Result<usize>, T);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: WriteFuture はアンピン可能（ポインタを保持しない）
        let this = unsafe { self.get_unchecked_mut() };

        if !this.submitted {
            let user_data = next_user_data();
            this.user_data = user_data;
            register_op(user_data);

            let fd = this.fd;
            let buf_ptr = this.buf.read_ptr() as u64;
            let buf_len = this.buf.bytes_init() as u32;

            with_ring(|ring| {
                if let Some(sqe) = ring.get_sqe() {
                    sqe.opcode = IORING_OP_SEND;
                    sqe.fd = fd;
                    sqe.addr_or_splice_off_in = buf_ptr;
                    sqe.len = buf_len;
                    sqe.user_data = user_data;
                }
            });

            if let Err(e) = submit_sqes() {
                remove_op(user_data);
                let buf = unsafe { std::ptr::read(&this.buf) };
                std::mem::forget(unsafe { std::ptr::read(this) });
                return Poll::Ready((Err(e), buf));
            }

            this.submitted = true;
        }

        match peek_op_result(this.user_data) {
            Some(res) => {
                take_op_result(this.user_data);
                let n = res;
                let buf = unsafe { std::ptr::read(&this.buf) };
                std::mem::forget(unsafe { std::ptr::read(this) });

                if n < 0 {
                    Poll::Ready((Err(io::Error::from_raw_os_error(-n)), buf))
                } else {
                    Poll::Ready((Ok(n as usize), buf))
                }
            }
            None => {
                set_op_waker(this.user_data, cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

// ====================
// Readable / Writable Future
// ====================

/// 読み取り可能まで待つ Future（POLL_ADD POLLIN）
pub struct Readable<'a> {
    fd: RawFd,
    user_data: u64,
    submitted: bool,
    _marker: std::marker::PhantomData<&'a TcpStream>,
}

impl<'a> Future for Readable<'a> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.submitted {
            let user_data = next_user_data();
            self.user_data = user_data;
            register_op(user_data);

            let fd = self.fd;
            with_ring(|ring| {
                if let Some(sqe) = ring.get_sqe() {
                    sqe.opcode = IORING_OP_POLL_ADD;
                    sqe.fd = fd;
                    sqe.op_flags = POLLIN as u32;
                    sqe.user_data = user_data;
                }
            });

            if let Err(e) = submit_sqes() {
                remove_op(user_data);
                return Poll::Ready(Err(e));
            }

            self.submitted = true;
        }

        match peek_op_result(self.user_data) {
            Some(res) => {
                take_op_result(self.user_data);
                if res < 0 {
                    Poll::Ready(Err(io::Error::from_raw_os_error(-res)))
                } else {
                    Poll::Ready(Ok(()))
                }
            }
            None => {
                set_op_waker(self.user_data, cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

/// 書き込み可能まで待つ Future（POLL_ADD POLLOUT）
pub struct Writable<'a> {
    fd: RawFd,
    user_data: u64,
    submitted: bool,
    _marker: std::marker::PhantomData<&'a TcpStream>,
}

impl<'a> Future for Writable<'a> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.submitted {
            let user_data = next_user_data();
            self.user_data = user_data;
            register_op(user_data);

            let fd = self.fd;
            with_ring(|ring| {
                if let Some(sqe) = ring.get_sqe() {
                    sqe.opcode = IORING_OP_POLL_ADD;
                    sqe.fd = fd;
                    sqe.op_flags = POLLOUT as u32;
                    sqe.user_data = user_data;
                }
            });

            if let Err(e) = submit_sqes() {
                remove_op(user_data);
                return Poll::Ready(Err(e));
            }

            self.submitted = true;
        }

        match peek_op_result(self.user_data) {
            Some(res) => {
                take_op_result(self.user_data);
                if res < 0 {
                    Poll::Ready(Err(io::Error::from_raw_os_error(-res)))
                } else {
                    Poll::Ready(Ok(()))
                }
            }
            None => {
                set_op_waker(self.user_data, cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

// ====================
// 汎用 FD 待機 Future（UDP 等、任意の FD に使用）
// ====================

/// 任意の FD が読み込み可能になるまで待つ Future
pub struct ReadableFd {
    fd: RawFd,
    user_data: u64,
    submitted: bool,
}

impl Future for ReadableFd {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.submitted {
            let user_data = next_user_data();
            self.user_data = user_data;
            register_op(user_data);

            let fd = self.fd;
            with_ring(|ring| {
                if let Some(sqe) = ring.get_sqe() {
                    sqe.opcode = IORING_OP_POLL_ADD;
                    sqe.fd = fd;
                    sqe.op_flags = POLLIN as u32;
                    sqe.user_data = user_data;
                }
            });

            if let Err(e) = submit_sqes() {
                remove_op(user_data);
                return Poll::Ready(Err(e));
            }

            self.submitted = true;
        }

        match peek_op_result(self.user_data) {
            Some(res) => {
                take_op_result(self.user_data);
                if res < 0 {
                    Poll::Ready(Err(io::Error::from_raw_os_error(-res)))
                } else {
                    Poll::Ready(Ok(()))
                }
            }
            None => {
                set_op_waker(self.user_data, cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

/// 任意の FD が書き込み可能になるまで待つ Future
pub struct WritableFd {
    fd: RawFd,
    user_data: u64,
    submitted: bool,
}

impl Future for WritableFd {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.submitted {
            let user_data = next_user_data();
            self.user_data = user_data;
            register_op(user_data);

            let fd = self.fd;
            with_ring(|ring| {
                if let Some(sqe) = ring.get_sqe() {
                    sqe.opcode = IORING_OP_POLL_ADD;
                    sqe.fd = fd;
                    sqe.op_flags = POLLOUT as u32;
                    sqe.user_data = user_data;
                }
            });

            if let Err(e) = submit_sqes() {
                remove_op(user_data);
                return Poll::Ready(Err(e));
            }

            self.submitted = true;
        }

        match peek_op_result(self.user_data) {
            Some(res) => {
                take_op_result(self.user_data);
                if res < 0 {
                    Poll::Ready(Err(io::Error::from_raw_os_error(-res)))
                } else {
                    Poll::Ready(Ok(()))
                }
            }
            None => {
                set_op_waker(self.user_data, cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

/// 任意の FD が読み込み可能になるまで待つ
pub fn wait_readable_fd(fd: RawFd) -> ReadableFd {
    ReadableFd {
        fd,
        user_data: 0,
        submitted: false,
    }
}

/// 任意の FD が書き込み可能になるまで待つ
pub fn wait_writable_fd(fd: RawFd) -> WritableFd {
    WritableFd {
        fd,
        user_data: 0,
        submitted: false,
    }
}

