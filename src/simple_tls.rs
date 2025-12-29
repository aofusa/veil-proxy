//! # シンプルな rustls TLS ストリームモジュール
//!
//! kTLS 無効時に使用される、rustls を直接使用した TLS ストリーム実装。
//! monoio の AsyncReadRent/AsyncWriteRent を実装します.

#![allow(dead_code)]

use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;

use monoio::buf::{IoBuf, IoBufMut, IoVecBuf, IoVecBufMut};
use monoio::net::TcpStream;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};

// ====================
// libc ヘルパー
// ====================

#[inline]
fn raw_read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    let result = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

#[inline]
fn raw_write(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    let result = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

// ====================
// サーバー TLS ストリーム
// ====================

pub struct SimpleTlsServerStream {
    inner: TcpStream,
    conn: ServerConnection,
}

impl SimpleTlsServerStream {
    pub fn get_ref(&self) -> &TcpStream {
        &self.inner
    }

    pub fn get_mut(&mut self) -> &mut TcpStream {
        &mut self.inner
    }

    /// kTLS は無効
    pub fn is_ktls_enabled(&self) -> bool {
        false
    }

    /// kTLS 送信は無効
    pub fn is_ktls_send_enabled(&self) -> bool {
        false
    }
    
    /// ALPN でネゴシエートされたプロトコルを取得
    /// 
    /// TLS ハンドシェイク完了後に呼び出すことで、
    /// クライアントと合意したプロトコルを取得できます。
    /// 
    /// # Returns
    /// 
    /// - `Some(b"h2")`: HTTP/2 がネゴシエートされた
    /// - `Some(b"http/1.1")`: HTTP/1.1 がネゴシエートされた
    /// - `None`: ALPN 未設定または未ネゴシエート
    pub fn alpn_protocol(&self) -> Option<&[u8]> {
        self.conn.alpn_protocol()
    }
    
    /// HTTP/2 がネゴシエートされたかどうか
    #[inline]
    pub fn is_http2(&self) -> bool {
        self.alpn_protocol() == Some(b"h2")
    }
}

impl AsRawFd for SimpleTlsServerStream {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

// ====================
// クライアント TLS ストリーム
// ====================

pub struct SimpleTlsClientStream {
    inner: TcpStream,
    conn: ClientConnection,
}

impl SimpleTlsClientStream {
    pub fn get_ref(&self) -> &TcpStream {
        &self.inner
    }

    pub fn get_mut(&mut self) -> &mut TcpStream {
        &mut self.inner
    }

    /// kTLS は無効
    pub fn is_ktls_enabled(&self) -> bool {
        false
    }

    /// kTLS 送信は無効
    pub fn is_ktls_send_enabled(&self) -> bool {
        false
    }
}

impl AsRawFd for SimpleTlsClientStream {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

// ====================
// サーバーハンドシェイク
// ====================

async fn do_server_handshake(
    stream: &TcpStream,
    conn: &mut ServerConnection,
) -> io::Result<()> {
    let fd = stream.as_raw_fd();
    let mut read_buf = vec![0u8; 16384];

    while conn.is_handshaking() {
        while conn.wants_write() {
            let mut write_buf = Vec::new();
            conn.write_tls(&mut write_buf)?;

            let mut written = 0;
            while written < write_buf.len() {
                match raw_write(fd, &write_buf[written..]) {
                    Ok(0) => {
                        return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0"))
                    }
                    Ok(n) => written += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        stream.writable(false).await?;
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        if conn.wants_read() {
            loop {
                match raw_read(fd, &mut read_buf) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "EOF during handshake",
                        ))
                    }
                    Ok(n) => {
                        conn.read_tls(&mut &read_buf[..n])?;
                        conn.process_new_packets()
                            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                        break;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        stream.readable(false).await?;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }

    // ハンドシェイク完了後、バッファリングされた TLS レコードを全て送信
    // TLS 1.3 ではセッションチケット (NewSessionTicket) がハンドシェイク後に送信される
    while conn.wants_write() {
        let mut write_buf = Vec::new();
        conn.write_tls(&mut write_buf)?;

        if write_buf.is_empty() {
            break;
        }

        let mut written = 0;
        while written < write_buf.len() {
            match raw_write(fd, &write_buf[written..]) {
                Ok(0) => {
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0"))
                }
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    stream.writable(false).await?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    Ok(())
}

async fn do_client_handshake(
    stream: &TcpStream,
    conn: &mut ClientConnection,
) -> io::Result<()> {
    let fd = stream.as_raw_fd();
    let mut read_buf = vec![0u8; 16384];
    while conn.is_handshaking() {
        while conn.wants_write() {
            let mut write_buf = Vec::new();
            conn.write_tls(&mut write_buf)?;

            let mut written = 0;
            while written < write_buf.len() {
                match raw_write(fd, &write_buf[written..]) {
                    Ok(0) => {
                        return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0"))
                    }
                    Ok(n) => written += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        stream.writable(false).await?;
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        if conn.wants_read() {
            loop {
                match raw_read(fd, &mut read_buf) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "EOF during handshake",
                        ))
                    }
                    Ok(n) => {
                        conn.read_tls(&mut &read_buf[..n])?;
                        conn.process_new_packets()
                            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                        break;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        stream.readable(false).await?;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }

    // ハンドシェイク完了後、バッファリングされた TLS レコードを全て送信
    // TLS 1.3 ではクライアントの Finished メッセージがここで送信される
    while conn.wants_write() {
        let mut write_buf = Vec::new();
        conn.write_tls(&mut write_buf)?;

        if write_buf.is_empty() {
            break;
        }

        let mut written = 0;
        while written < write_buf.len() {
            match raw_write(fd, &write_buf[written..]) {
                Ok(0) => {
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0"))
                }
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    stream.writable(false).await?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    Ok(())
}

// ====================
// accept / connect
// ====================

pub async fn accept(
    stream: TcpStream,
    config: Arc<ServerConfig>,
) -> io::Result<SimpleTlsServerStream> {
    let mut conn = ServerConnection::new(config)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    do_server_handshake(&stream, &mut conn).await?;

    Ok(SimpleTlsServerStream { inner: stream, conn })
}

pub async fn connect(
    stream: TcpStream,
    config: Arc<ClientConfig>,
    server_name: ServerName<'static>,
) -> io::Result<SimpleTlsClientStream> {
    let mut conn = ClientConnection::new(config, server_name)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    do_client_handshake(&stream, &mut conn).await?;

    Ok(SimpleTlsClientStream { inner: stream, conn })
}

// ====================
// AsyncReadRent 実装（サーバー）
// ====================

impl monoio::io::AsyncReadRent for SimpleTlsServerStream {
    async fn read<T: IoBufMut>(&mut self, mut buf: T) -> monoio::BufResult<usize, T> {
        let fd = self.inner.as_raw_fd();
        let mut read_buf = vec![0u8; 16384];

        loop {
            let slice =
                unsafe { std::slice::from_raw_parts_mut(buf.write_ptr(), buf.bytes_total()) };

            let mut rd = self.conn.reader();
            match std::io::Read::read(&mut rd, slice) {
                Ok(n) if n > 0 => {
                    unsafe { buf.set_init(n) };
                    return (Ok(n), buf);
                }
                Ok(0) if !self.conn.wants_read() => {
                    return (Ok(0), buf);
                }
                Ok(_) => {}  // Not EOF yet, need more TLS data
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}  // Need more TLS data
                Err(e) => return (Err(e), buf),  // Return actual errors
            }

            loop {
                match raw_read(fd, &mut read_buf) {
                    Ok(0) => return (Ok(0), buf),
                    Ok(n) => {
                        // read_tls が全てのデータを消費するまでループ
                        let mut consumed = 0;
                        while consumed < n {
                            let remaining = &read_buf[consumed..n];
                            let tls_read = match self.conn.read_tls(&mut &*remaining) {
                                Ok(0) => break,
                                Ok(r) => r,
                                Err(e) => return (Err(e), buf),
                            };
                            consumed += tls_read;
                            if let Err(e) = self.conn.process_new_packets() {
                                return (Err(io::Error::new(io::ErrorKind::InvalidData, e)), buf);
                            }
                        }
                        break;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        if let Err(e) = self.inner.readable(false).await {
                            return (Err(e), buf);
                        }
                    }
                    Err(e) => return (Err(e), buf),
                }
            }
        }
    }

    async fn readv<T: IoVecBufMut>(&mut self, mut buf: T) -> monoio::BufResult<usize, T> {
        let iovec_ptr = buf.write_iovec_ptr();
        let iovec_len = buf.write_iovec_len();

        if iovec_len == 0 {
            return (Ok(0), buf);
        }

        let slice = unsafe {
            let iov = &*iovec_ptr;
            if iov.iov_len == 0 {
                return (Ok(0), buf);
            }
            std::slice::from_raw_parts_mut(iov.iov_base as *mut u8, iov.iov_len)
        };

        let fd = self.inner.as_raw_fd();
        let mut read_buf = vec![0u8; 16384];

        loop {
            let mut rd = self.conn.reader();
            match std::io::Read::read(&mut rd, slice) {
                Ok(n) if n > 0 => {
                    return (Ok(n), buf);
                }
                Ok(0) if !self.conn.wants_read() => {
                    return (Ok(0), buf);
                }
                Ok(_) => {}  // Not EOF yet, need more TLS data
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}  // Need more TLS data
                Err(e) => return (Err(e), buf),  // Return actual errors
            }

            loop {
                match raw_read(fd, &mut read_buf) {
                    Ok(0) => return (Ok(0), buf),
                    Ok(n) => {
                        // read_tls が全てのデータを消費するまでループ
                        let mut consumed = 0;
                        while consumed < n {
                            let remaining = &read_buf[consumed..n];
                            let tls_read = match self.conn.read_tls(&mut &*remaining) {
                                Ok(0) => break,
                                Ok(r) => r,
                                Err(e) => return (Err(e), buf),
                            };
                            consumed += tls_read;
                            if let Err(e) = self.conn.process_new_packets() {
                                return (Err(io::Error::new(io::ErrorKind::InvalidData, e)), buf);
                            }
                        }
                        break;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        if let Err(e) = self.inner.readable(false).await {
                            return (Err(e), buf);
                        }
                    }
                    Err(e) => return (Err(e), buf),
                }
            }
        }
    }
}

impl monoio::io::AsyncWriteRent for SimpleTlsServerStream {
    async fn write<T: IoBuf>(&mut self, buf: T) -> monoio::BufResult<usize, T> {
        let slice = unsafe { std::slice::from_raw_parts(buf.read_ptr(), buf.bytes_init()) };

        let mut wr = self.conn.writer();
        if let Err(e) = std::io::Write::write_all(&mut wr, slice) {
            return (Err(e), buf);
        }

        let fd = self.inner.as_raw_fd();
        while self.conn.wants_write() {
            let mut write_buf = Vec::new();
            if let Err(e) = self.conn.write_tls(&mut write_buf) {
                return (Err(e), buf);
            }

            let mut written = 0;
            while written < write_buf.len() {
                match raw_write(fd, &write_buf[written..]) {
                    Ok(0) => {
                        return (
                            Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0")),
                            buf,
                        )
                    }
                    Ok(n) => written += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        if let Err(e) = self.inner.writable(false).await {
                            return (Err(e), buf);
                        }
                    }
                    Err(e) => return (Err(e), buf),
                }
            }
        }

        (Ok(slice.len()), buf)
    }

    async fn writev<T: IoVecBuf>(&mut self, buf: T) -> monoio::BufResult<usize, T> {
        let iovec_ptr = buf.read_iovec_ptr();
        let iovec_len = buf.read_iovec_len();

        if iovec_len == 0 {
            return (Ok(0), buf);
        }

        let slice = unsafe {
            let iov = &*iovec_ptr;
            if iov.iov_len == 0 {
                return (Ok(0), buf);
            }
            std::slice::from_raw_parts(iov.iov_base as *const u8, iov.iov_len)
        };

        let mut wr = self.conn.writer();
        if let Err(e) = std::io::Write::write_all(&mut wr, slice) {
            return (Err(e), buf);
        }

        let fd = self.inner.as_raw_fd();
        while self.conn.wants_write() {
            let mut write_buf = Vec::new();
            if let Err(e) = self.conn.write_tls(&mut write_buf) {
                return (Err(e), buf);
            }

            let mut written = 0;
            while written < write_buf.len() {
                match raw_write(fd, &write_buf[written..]) {
                    Ok(0) => {
                        return (
                            Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0")),
                            buf,
                        )
                    }
                    Ok(n) => written += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        if let Err(e) = self.inner.writable(false).await {
                            return (Err(e), buf);
                        }
                    }
                    Err(e) => return (Err(e), buf),
                }
            }
        }

        (Ok(slice.len()), buf)
    }

    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ====================
// AsyncReadRent 実装（クライアント）
// ====================

impl monoio::io::AsyncReadRent for SimpleTlsClientStream {
    async fn read<T: IoBufMut>(&mut self, mut buf: T) -> monoio::BufResult<usize, T> {
        let fd = self.inner.as_raw_fd();
        let mut read_buf = vec![0u8; 16384];

        loop {
            let slice =
                unsafe { std::slice::from_raw_parts_mut(buf.write_ptr(), buf.bytes_total()) };

            let mut rd = self.conn.reader();
            match std::io::Read::read(&mut rd, slice) {
                Ok(n) if n > 0 => {
                    unsafe { buf.set_init(n) };
                    return (Ok(n), buf);
                }
                Ok(0) if !self.conn.wants_read() => {
                    return (Ok(0), buf);
                }
                Ok(_) => {}  // Not EOF yet, need more TLS data
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}  // Need more TLS data
                Err(e) => {
                    return (Err(e), buf);  // Return actual errors
                }
            }

            loop {
                match raw_read(fd, &mut read_buf) {
                    Ok(0) => {
                        return (Ok(0), buf);
                    }
                    Ok(n) => {
                        // read_tls が全てのデータを消費するまでループ
                        let mut consumed = 0;
                        while consumed < n {
                            let remaining = &read_buf[consumed..n];
                            let tls_read = match self.conn.read_tls(&mut &*remaining) {
                                Ok(0) => break, // rustls がこれ以上読めない
                                Ok(r) => r,
                                Err(e) => {
                                    return (Err(e), buf);
                                }
                            };
                            consumed += tls_read;
                            if let Err(e) = self.conn.process_new_packets() {
                                return (Err(io::Error::new(io::ErrorKind::InvalidData, e)), buf);
                            }
                        }
                        break;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        if let Err(e) = self.inner.readable(false).await {
                            return (Err(e), buf);
                        }
                    }
                    Err(e) => {
                        return (Err(e), buf);
                    }
                }
            }
        }
    }

    async fn readv<T: IoVecBufMut>(&mut self, mut buf: T) -> monoio::BufResult<usize, T> {
        let iovec_ptr = buf.write_iovec_ptr();
        let iovec_len = buf.write_iovec_len();

        if iovec_len == 0 {
            return (Ok(0), buf);
        }

        let slice = unsafe {
            let iov = &*iovec_ptr;
            if iov.iov_len == 0 {
                return (Ok(0), buf);
            }
            std::slice::from_raw_parts_mut(iov.iov_base as *mut u8, iov.iov_len)
        };

        let fd = self.inner.as_raw_fd();
        let mut read_buf = vec![0u8; 16384];

        loop {
            let mut rd = self.conn.reader();
            match std::io::Read::read(&mut rd, slice) {
                Ok(n) if n > 0 => {
                    return (Ok(n), buf);
                }
                Ok(0) if !self.conn.wants_read() => {
                    return (Ok(0), buf);
                }
                Ok(_) => {}  // Not EOF yet, need more TLS data
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}  // Need more TLS data
                Err(e) => return (Err(e), buf),  // Return actual errors
            }

            loop {
                match raw_read(fd, &mut read_buf) {
                    Ok(0) => return (Ok(0), buf),
                    Ok(n) => {
                        // read_tls が全てのデータを消費するまでループ
                        let mut consumed = 0;
                        while consumed < n {
                            let remaining = &read_buf[consumed..n];
                            let tls_read = match self.conn.read_tls(&mut &*remaining) {
                                Ok(0) => break,
                                Ok(r) => r,
                                Err(e) => return (Err(e), buf),
                            };
                            consumed += tls_read;
                            if let Err(e) = self.conn.process_new_packets() {
                                return (Err(io::Error::new(io::ErrorKind::InvalidData, e)), buf);
                            }
                        }
                        break;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        if let Err(e) = self.inner.readable(false).await {
                            return (Err(e), buf);
                        }
                    }
                    Err(e) => return (Err(e), buf),
                }
            }
        }
    }
}

impl monoio::io::AsyncWriteRent for SimpleTlsClientStream {
    async fn write<T: IoBuf>(&mut self, buf: T) -> monoio::BufResult<usize, T> {
        let slice = unsafe { std::slice::from_raw_parts(buf.read_ptr(), buf.bytes_init()) };

        let mut wr = self.conn.writer();
        if let Err(e) = std::io::Write::write_all(&mut wr, slice) {
            return (Err(e), buf);
        }

        let fd = self.inner.as_raw_fd();
        while self.conn.wants_write() {
            let mut write_buf = Vec::new();
            if let Err(e) = self.conn.write_tls(&mut write_buf) {
                return (Err(e), buf);
            }

            let mut written = 0;
            while written < write_buf.len() {
                match raw_write(fd, &write_buf[written..]) {
                    Ok(0) => {
                        return (
                            Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0")),
                            buf,
                        )
                    }
                    Ok(n) => written += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        if let Err(e) = self.inner.writable(false).await {
                            return (Err(e), buf);
                        }
                    }
                    Err(e) => return (Err(e), buf),
                }
            }
        }

        (Ok(slice.len()), buf)
    }

    async fn writev<T: IoVecBuf>(&mut self, buf: T) -> monoio::BufResult<usize, T> {
        let iovec_ptr = buf.read_iovec_ptr();
        let iovec_len = buf.read_iovec_len();

        if iovec_len == 0 {
            return (Ok(0), buf);
        }

        let slice = unsafe {
            let iov = &*iovec_ptr;
            if iov.iov_len == 0 {
                return (Ok(0), buf);
            }
            std::slice::from_raw_parts(iov.iov_base as *const u8, iov.iov_len)
        };

        let mut wr = self.conn.writer();
        if let Err(e) = std::io::Write::write_all(&mut wr, slice) {
            return (Err(e), buf);
        }

        let fd = self.inner.as_raw_fd();
        while self.conn.wants_write() {
            let mut write_buf = Vec::new();
            if let Err(e) = self.conn.write_tls(&mut write_buf) {
                return (Err(e), buf);
            }

            let mut written = 0;
            while written < write_buf.len() {
                match raw_write(fd, &write_buf[written..]) {
                    Ok(0) => {
                        return (
                            Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0")),
                            buf,
                        )
                    }
                    Ok(n) => written += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        if let Err(e) = self.inner.writable(false).await {
                            return (Err(e), buf);
                        }
                    }
                    Err(e) => return (Err(e), buf),
                }
            }
        }

        (Ok(slice.len()), buf)
    }

    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ====================
// アクセプター / コネクター
// ====================

#[derive(Clone)]
pub struct SimpleTlsAcceptor {
    config: Arc<ServerConfig>,
}

impl SimpleTlsAcceptor {
    pub fn new(config: Arc<ServerConfig>) -> Self {
        SimpleTlsAcceptor { config }
    }

    /// kTLS 設定は無視（互換性のため）
    pub fn with_ktls(self, _enable: bool) -> Self {
        self
    }

    pub async fn accept(&self, stream: TcpStream) -> io::Result<SimpleTlsServerStream> {
        accept(stream, self.config.clone()).await
    }
}

#[derive(Clone)]
pub struct SimpleTlsConnector {
    config: Arc<ClientConfig>,
}

impl SimpleTlsConnector {
    pub fn new(config: Arc<ClientConfig>) -> Self {
        SimpleTlsConnector { config }
    }

    /// kTLS 設定は無視（互換性のため）
    pub fn with_ktls(self, _enable: bool) -> Self {
        self
    }

    pub async fn connect(
        &self,
        stream: TcpStream,
        server_name: &str,
    ) -> io::Result<SimpleTlsClientStream> {
        let server_name = ServerName::try_from(server_name.to_string())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        connect(stream, self.config.clone(), server_name).await
    }
}

/// デフォルトのクライアント設定を作成
pub fn default_client_config() -> Arc<ClientConfig> {
    let mut root_store = RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    Arc::new(config)
}

/// 証明書検証をスキップするカスタム検証器（テスト/自己署名証明書用）
#[derive(Debug)]
pub struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
            .to_vec()
    }
}

/// 証明書検証をスキップするクライアント設定（テスト用/自己署名証明書）
pub fn insecure_client_config() -> Arc<ClientConfig> {
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();

    Arc::new(config)
}
