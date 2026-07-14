//! # シンプルな rustls TLS ストリームモジュール
//!
//! kTLS 無効時に使用される、rustls を直接使用した TLS ストリーム実装。
//! monoio の AsyncReadRent/AsyncWriteRent を実装します.

use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;

use crate::runtime::buf::{IoBuf, IoBufMut};
use crate::runtime::io::{IoVecBuf, IoVecBufMut};
use crate::runtime::tcp::TcpStream;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};

// ====================
// TLS ストリーム状態
// ====================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
    /// rustls によるユーザーランド TLS
    Rustls,
    /// 平文 HTTP/1.1（TLSなし）
    Plain,
}

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

/// rustls の復号済み平文を `drained` の未初期化スペア容量へ直接読み込む（中間バッファ・
/// リードごとのヒープ確保なし）。`received_plaintext`（rustls 既定 16KB 上限）を空に保つ。
#[inline]
fn drain_rustls_into<R: std::io::Read>(drained: &mut Vec<u8>, mut rd: R) {
    loop {
        drained.reserve(16384);
        let start = drained.len();
        let spare = drained.spare_capacity_mut();
        // SAFETY: read は書き込み専用。戻り値 m バイトのみ set_len で確定し、未初期化領域は
        // 長さに含めない。
        let spare_u8 =
            unsafe { std::slice::from_raw_parts_mut(spare.as_mut_ptr().cast::<u8>(), spare.len()) };
        match rd.read(spare_u8) {
            Ok(0) => break,
            Ok(m) => unsafe { drained.set_len(start + m) },
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
}

// ====================
// サーバー TLS ストリーム
// ====================

pub struct SimpleTlsServerStream {
    inner: TcpStream,
    conn: Option<ServerConnection>,
    mode: TlsMode,
    drained_buffer: Vec<u8>,
}

impl crate::runtime::io::BufferedReadState for SimpleTlsServerStream {
    /// ユーザ空間 rustls が復号済みで未消費の平文（ドレインバッファ残量）を保持していれば
    /// `true`。F-116: HTTP/2 多重化メインループの可読待機前チェックに使う。
    #[inline]
    fn has_buffered_read_data(&self) -> bool {
        !self.drained_buffer.is_empty()
    }
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
        self.conn.as_ref().and_then(|c| c.alpn_protocol())
    }

    /// HTTP/2 がネゴシエートされたかどうか
    #[inline]
    pub fn is_http2(&self) -> bool {
        self.alpn_protocol() == Some(b"h2")
    }

    /// 2 つの不連続バッファ（ヘッダ + ボディ）を全量書き込む（F-59）
    ///
    /// 平文（`TlsMode::Plain`）接続では 1 回の `IORING_OP_SENDMSG`（scatter-gather）で
    /// 送出し、syscall/SQE を半減する。rustls モードはユーザー空間でレコード化するため
    /// 従来の 2 回書き込みへフォールバックする。
    pub async fn write_all_vectored<A: IoBuf, B: IoBuf>(
        &mut self,
        a: A,
        b: B,
    ) -> (io::Result<()>, A, B) {
        if self.mode == TlsMode::Plain {
            return self.inner.write_all_vectored(a, b).await;
        }
        use crate::runtime::io::AsyncWriteRentExt;
        let (res, a) = self.write_all(a).await;
        if let Err(e) = res {
            return (Err(e), a, b);
        }
        if b.bytes_init() > 0 {
            let (res, b) = self.write_all(b).await;
            if let Err(e) = res {
                return (Err(e), a, b);
            }
            return (Ok(()), a, b);
        }
        (Ok(()), a, b)
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
    /// rustls received_plaintext（既定 16KB 上限）の溢れ防止用に、復号済み平文を
    /// 退避するバッファ。大容量 TLS バックエンド応答の読み取りで上限超過を防ぐ。
    drained_buffer: Vec<u8>,
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

    /// F-44: ストリームを構成要素に分解する。
    ///
    /// HTTP/3 ストリーミング経路の全二重 TLS ラッパー（`http3_stream::TlsBackend`）構築用。
    /// 戻り値は `(TCP ストリーム, rustls セッション, 復号済みドレインバッファ)`。
    pub fn into_parts(self) -> (TcpStream, ClientConnection, Vec<u8>) {
        (self.inner, self.conn, self.drained_buffer)
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
    initial_data: &mut Option<Vec<u8>>,
) -> io::Result<()> {
    let fd = stream.as_raw_fd();
    let mut read_buf = vec![0u8; 16384];

    while conn.is_handshaking() {
        // 先行読み取りデータがあれば先に処理
        if let Some(data) = initial_data.take() {
            if !data.is_empty() {
                conn.read_tls(&mut &data[..])?;
                conn.process_new_packets()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                // データの処理後にハンドシェイクが終わる可能性があるため、ループの先頭に戻る
                continue;
            }
        }

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
                        stream.writable().await?;
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
                        stream.readable().await?;
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
                Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0")),
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    stream.writable().await?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    Ok(())
}

async fn do_client_handshake(stream: &TcpStream, conn: &mut ClientConnection) -> io::Result<()> {
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
                        stream.writable().await?;
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
                        stream.readable().await?;
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
                Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0")),
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    stream.writable().await?;
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
    mut initial_data: Option<Vec<u8>>,
) -> io::Result<SimpleTlsServerStream> {
    let mut conn =
        ServerConnection::new(config).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    do_server_handshake(&stream, &mut conn, &mut initial_data).await?;

    Ok(SimpleTlsServerStream {
        inner: stream,
        conn: Some(conn),
        mode: TlsMode::Rustls,
        drained_buffer: Vec::new(),
    })
}

/// 平文（TLSなし）接続をアクセプト（H2C対応用）
pub async fn accept_plain(
    stream: TcpStream,
    initial_data: Option<Vec<u8>>,
) -> io::Result<SimpleTlsServerStream> {
    Ok(SimpleTlsServerStream {
        inner: stream,
        conn: None,
        mode: TlsMode::Plain,
        drained_buffer: initial_data.unwrap_or_default(),
    })
}

pub async fn connect(
    stream: TcpStream,
    config: Arc<ClientConfig>,
    server_name: ServerName<'static>,
) -> io::Result<SimpleTlsClientStream> {
    let mut conn = ClientConnection::new(config, server_name)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    do_client_handshake(&stream, &mut conn).await?;

    Ok(SimpleTlsClientStream {
        inner: stream,
        conn,
        drained_buffer: Vec::new(),
    })
}

// ====================
// AsyncReadRent 実装（サーバー）
// ====================

impl crate::runtime::io::AsyncReadRent for SimpleTlsServerStream {
    async fn read<T: IoBufMut>(&mut self, mut buf: T) -> crate::runtime::io::BufResult<usize, T> {
        // ドレインバッファがあれば優先的に返す
        if !self.drained_buffer.is_empty() {
            let len = std::cmp::min(self.drained_buffer.len(), buf.bytes_total());
            unsafe {
                std::ptr::copy_nonoverlapping(self.drained_buffer.as_ptr(), buf.write_ptr(), len);
                buf.set_init(len);
            }
            self.drained_buffer.drain(..len);
            return (Ok(len), buf);
        }

        // 平文モードの場合は直接読み取る
        if self.mode == TlsMode::Plain {
            return self.inner.read(buf).await;
        }

        let fd = self.inner.as_raw_fd();
        let mut read_buf = vec![0u8; 16384];

        loop {
            // rustls が復号済みの平文を drained_buffer の uninit スペアへ直書きで取り出す
            // （received_plaintext 既定 16KB 上限の溢れ防止。大容量 h2/TLS アップロード対応）。
            {
                let conn = match self.conn.as_mut() {
                    Some(c) => c,
                    None => return (Err(io::Error::other("TLS connection closed")), buf),
                };
                drain_rustls_into(&mut self.drained_buffer, conn.reader());
            }

            if !self.drained_buffer.is_empty() {
                let len = std::cmp::min(self.drained_buffer.len(), buf.bytes_total());
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        self.drained_buffer.as_ptr(),
                        buf.write_ptr(),
                        len,
                    );
                    buf.set_init(len);
                }
                self.drained_buffer.drain(..len);
                return (Ok(len), buf);
            }

            match raw_read(fd, &mut read_buf) {
                Ok(0) => return (Ok(0), buf),
                Ok(n) => {
                    let conn = match self.conn.as_mut() {
                        Some(c) => c,
                        None => return (Err(io::Error::other("TLS connection closed")), buf),
                    };
                    let mut consumed = 0;
                    while consumed < n {
                        let tls_read = match conn.read_tls(&mut &read_buf[consumed..n]) {
                            Ok(0) => break,
                            Ok(r) => r,
                            Err(e) => return (Err(e), buf),
                        };
                        consumed += tls_read;
                        if let Err(e) = conn.process_new_packets() {
                            return (Err(io::Error::new(io::ErrorKind::InvalidData, e)), buf);
                        }
                        drain_rustls_into(&mut self.drained_buffer, conn.reader());
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if let Err(e) = self.inner.readable().await {
                        return (Err(e), buf);
                    }
                }
                Err(e) => return (Err(e), buf),
            }
        }
    }

    async fn readv<T: IoVecBufMut>(&mut self, buf: T) -> crate::runtime::io::BufResult<usize, T> {
        // IoVec stub のため未サポート
        (Err(io::Error::other("readv not supported")), buf)
    }
}

impl crate::runtime::io::AsyncWriteRent for SimpleTlsServerStream {
    async fn write<T: IoBuf>(&mut self, buf: T) -> crate::runtime::io::BufResult<usize, T> {
        if self.mode == TlsMode::Plain {
            return self.inner.write(buf).await;
        }

        let slice = unsafe { std::slice::from_raw_parts(buf.read_ptr(), buf.bytes_init()) };
        let conn = match self.conn.as_mut() {
            Some(c) => c,
            None => return (Err(io::Error::other("TLS connection closed")), buf),
        };

        // rustls の送信平文バッファ（既定 64KB）には上限があり、巨大 slice を
        // `write_all` すると受理不能時に WriteZero（"failed to write whole buffer"）で
        // 失敗する（F-116: HTTP/2 多重化の連結送出は 64KB を超えうる）。`write` は
        // 部分書き込みを返してよい契約のため、rustls が受理できた分だけ書き込み、
        // TLS レコードを送出して受理バイト数を返す（呼び出し側の
        // `AsyncWriteRentExt::write_all`〈B-27 SlicedIoBuf〉が残りを継続する）。
        let accepted = {
            let mut wr = conn.writer();
            match std::io::Write::write(&mut wr, slice) {
                Ok(0) if !slice.is_empty() => {
                    return (
                        Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "rustls writer accepted 0 bytes",
                        )),
                        buf,
                    );
                }
                Ok(n) => n,
                Err(e) => return (Err(e), buf),
            }
        };

        let fd = self.inner.as_raw_fd();
        while conn.wants_write() {
            let mut write_buf = Vec::new();
            if let Err(e) = conn.write_tls(&mut write_buf) {
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
                        if let Err(e) = self.inner.writable().await {
                            return (Err(e), buf);
                        }
                    }
                    Err(e) => return (Err(e), buf),
                }
            }
        }

        (Ok(accepted), buf)
    }

    async fn writev<T: IoVecBuf>(&mut self, buf: T) -> crate::runtime::io::BufResult<usize, T> {
        // IoVec stub のため未サポート
        (Err(io::Error::other("writev not supported")), buf)
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

impl crate::runtime::io::AsyncReadRent for SimpleTlsClientStream {
    async fn read<T: IoBufMut>(&mut self, mut buf: T) -> crate::runtime::io::BufResult<usize, T> {
        // ドレインバッファがあれば優先的に返す
        if !self.drained_buffer.is_empty() {
            let len = std::cmp::min(self.drained_buffer.len(), buf.bytes_total());
            unsafe {
                std::ptr::copy_nonoverlapping(self.drained_buffer.as_ptr(), buf.write_ptr(), len);
                buf.set_init(len);
            }
            self.drained_buffer.drain(..len);
            return (Ok(len), buf);
        }

        let fd = self.inner.as_raw_fd();
        let mut read_buf = vec![0u8; 16384];

        loop {
            // rustls が復号済みの平文を drained_buffer の uninit スペアへ直書きで取り出す
            // （received_plaintext 既定 16KB 上限の溢れ防止）。
            drain_rustls_into(&mut self.drained_buffer, self.conn.reader());

            if !self.drained_buffer.is_empty() {
                let len = std::cmp::min(self.drained_buffer.len(), buf.bytes_total());
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        self.drained_buffer.as_ptr(),
                        buf.write_ptr(),
                        len,
                    );
                    buf.set_init(len);
                }
                self.drained_buffer.drain(..len);
                return (Ok(len), buf);
            }

            match raw_read(fd, &mut read_buf) {
                Ok(0) => {
                    return (Ok(0), buf);
                }
                Ok(n) => {
                    let mut consumed = 0;
                    while consumed < n {
                        let tls_read = match self.conn.read_tls(&mut &read_buf[consumed..n]) {
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
                        drain_rustls_into(&mut self.drained_buffer, self.conn.reader());
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if let Err(e) = self.inner.readable().await {
                        return (Err(e), buf);
                    }
                }
                Err(e) => {
                    return (Err(e), buf);
                }
            }
        }
    }

    async fn readv<T: IoVecBufMut>(&mut self, buf: T) -> crate::runtime::io::BufResult<usize, T> {
        // IoVec stub のため未サポート
        (Err(io::Error::other("readv not supported")), buf)
    }
}

impl crate::runtime::io::AsyncWriteRent for SimpleTlsClientStream {
    async fn write<T: IoBuf>(&mut self, buf: T) -> crate::runtime::io::BufResult<usize, T> {
        let slice = unsafe { std::slice::from_raw_parts(buf.read_ptr(), buf.bytes_init()) };

        // rustls の送信平文バッファ（既定 64KB）には上限があり、巨大 slice を
        // `write_all` すると受理不能時に WriteZero（"failed to write whole buffer"）で
        // 失敗する（F-116: HTTP/2 多重化の連結送出は 64KB を超えうる）。`write` は
        // 部分書き込みを返してよい契約のため、rustls が受理できた分だけ書き込み、
        // TLS レコードを送出して受理バイト数を返す（呼び出し側の
        // `AsyncWriteRentExt::write_all`〈B-27 SlicedIoBuf〉が残りを継続する）。
        let accepted = {
            let mut wr = self.conn.writer();
            match std::io::Write::write(&mut wr, slice) {
                Ok(0) if !slice.is_empty() => {
                    return (
                        Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "rustls writer accepted 0 bytes",
                        )),
                        buf,
                    );
                }
                Ok(n) => n,
                Err(e) => return (Err(e), buf),
            }
        };

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
                        if let Err(e) = self.inner.writable().await {
                            return (Err(e), buf);
                        }
                    }
                    Err(e) => return (Err(e), buf),
                }
            }
        }

        (Ok(accepted), buf)
    }

    async fn writev<T: IoVecBuf>(&mut self, buf: T) -> crate::runtime::io::BufResult<usize, T> {
        // IoVec stub のため未サポート
        (Err(io::Error::other("writev not supported")), buf)
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

    pub async fn accept(
        &self,
        stream: TcpStream,
        initial_data: Option<Vec<u8>>,
    ) -> io::Result<SimpleTlsServerStream> {
        // F-03: ホットリロードされた証明書があればそれを使う（毎ハンドシェイクでスナップショット取得）
        let config =
            crate::tls_reload::current_global_tls_config().unwrap_or_else(|| self.config.clone());
        accept(stream, config, initial_data).await
    }

    /// 平文（TLSなし）接続をアクセプト（H2C対応用）
    pub async fn accept_plain(
        &self,
        stream: TcpStream,
        initial_data: Option<Vec<u8>>,
    ) -> io::Result<SimpleTlsServerStream> {
        accept_plain(stream, initial_data).await
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
