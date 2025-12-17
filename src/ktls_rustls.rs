//! # rustls + monoio + kTLS 統合モジュール
//!
//! このモジュールは、rustls を monoio ランタイムと統合し、
//! ktls2 クレートによる kTLS（Kernel TLS）のサポートを提供します。
//!
//! ## 主要コンポーネント
//!
//! - [`RustlsConfig`]: TLS 設定を管理
//! - [`KtlsTlsStream`]: monoio の AsyncReadRent/AsyncWriteRent を実装した TLS ストリーム
//! - [`RustlsAcceptor`]: サーバー側の TLS アクセプター
//! - [`RustlsConnector`]: クライアント側の TLS コネクター
//!
//! ## kTLS サポート
//!
//! TLS ハンドシェイク完了後、ktls2 を使用して kTLS を有効化することで、
//! データ転送フェーズの暗号化/復号化をカーネルにオフロードします。
//!
//! ### メリット
//!
//! - CPU 使用率の削減（20-40%）
//! - sendfile(2) によるゼロコピー送信
//! - コンテキストスイッチの削減
//!
//! ### 要件
//!
//! - Linux 5.15+（推奨）
//! - `modprobe tls` でカーネルモジュールをロード
//! - AES-GCM 暗号スイート

// 将来の拡張用に残している未使用コードの警告を抑制
#![allow(dead_code)]

use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;

use monoio::buf::{IoBuf, IoBufMut, IoVecBuf, IoVecBufMut};
use monoio::net::TcpStream;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};

// ====================
// TLS ストリーム状態
// ====================

/// kTLS が有効化された後の TLS ストリーム状態
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
    /// rustls によるユーザーランド TLS（ハンドシェイク中またはフォールバック）
    Rustls,
    /// kTLS によるカーネルオフロード（送信のみ）
    KtlsTxOnly,
    /// kTLS によるカーネルオフロード（送受信両方）
    KtlsFull,
}

// ====================
// サーバー側 TLS ストリーム
// ====================

/// rustls + kTLS TLS ストリーム（サーバー側）
pub struct KtlsServerStream {
    /// 基盤となる TCP ストリーム
    inner: TcpStream,
    /// rustls サーバーコネクション（kTLS 有効化前は Some、有効化後は None）
    conn: Option<ServerConnection>,
    /// 現在の TLS モード
    mode: TlsMode,
}

impl KtlsServerStream {
    /// 基盤となる TCP ストリームへの参照を取得
    pub fn get_ref(&self) -> &TcpStream {
        &self.inner
    }

    /// 基盤となる TCP ストリームへの可変参照を取得
    pub fn get_mut(&mut self) -> &mut TcpStream {
        &mut self.inner
    }

    /// 現在の TLS モードを取得
    pub fn mode(&self) -> TlsMode {
        self.mode
    }

    /// kTLS が有効かどうか（送信または送受信）
    pub fn is_ktls_enabled(&self) -> bool {
        matches!(self.mode, TlsMode::KtlsTxOnly | TlsMode::KtlsFull)
    }

    /// kTLS 送信が有効かどうか
    pub fn is_ktls_send_enabled(&self) -> bool {
        matches!(self.mode, TlsMode::KtlsTxOnly | TlsMode::KtlsFull)
    }

    /// kTLS 受信が有効かどうか
    pub fn is_ktls_recv_enabled(&self) -> bool {
        matches!(self.mode, TlsMode::KtlsFull)
    }

    /// ファイルディスクリプタを取得
    pub fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }

    /// rustls コネクションへの参照を取得（kTLS 有効化後は None）
    pub fn rustls_conn(&self) -> Option<&ServerConnection> {
        self.conn.as_ref()
    }
}

impl AsRawFd for KtlsServerStream {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

// ====================
// クライアント側 TLS ストリーム
// ====================

/// rustls + kTLS TLS ストリーム（クライアント側）
pub struct KtlsClientStream {
    /// 基盤となる TCP ストリーム
    inner: TcpStream,
    /// rustls クライアントコネクション（kTLS 有効化前は Some、有効化後は None）
    conn: Option<ClientConnection>,
    /// 現在の TLS モード
    mode: TlsMode,
}

impl KtlsClientStream {
    /// 基盤となる TCP ストリームへの参照を取得
    pub fn get_ref(&self) -> &TcpStream {
        &self.inner
    }

    /// 基盤となる TCP ストリームへの可変参照を取得
    pub fn get_mut(&mut self) -> &mut TcpStream {
        &mut self.inner
    }

    /// 現在の TLS モードを取得
    pub fn mode(&self) -> TlsMode {
        self.mode
    }

    /// kTLS が有効かどうか
    pub fn is_ktls_enabled(&self) -> bool {
        matches!(self.mode, TlsMode::KtlsTxOnly | TlsMode::KtlsFull)
    }

    /// kTLS 送信が有効かどうか
    pub fn is_ktls_send_enabled(&self) -> bool {
        matches!(self.mode, TlsMode::KtlsTxOnly | TlsMode::KtlsFull)
    }

    /// kTLS 受信が有効かどうか
    pub fn is_ktls_recv_enabled(&self) -> bool {
        matches!(self.mode, TlsMode::KtlsFull)
    }

    /// ファイルディスクリプタを取得
    pub fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl AsRawFd for KtlsClientStream {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

// ====================
// ハンドシェイク実装
// ====================

/// libc::read のラッパー（ノンブロッキング対応）
#[inline]
fn raw_read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    let result = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

/// libc::write のラッパー（ノンブロッキング対応）
#[inline]
fn raw_write(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    let result = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

/// rustls コネクションを使用して非同期ハンドシェイクを実行（サーバー側）
/// 
/// 重要: ハンドシェイク完了後、kTLS のシークレット抽出に備えて
/// バッファリングされた全ての TLS レコードを送信する必要があります。
/// TLS 1.3 ではハンドシェイク完了後もセッションチケット等が送信されます。
async fn do_server_handshake(
    stream: &TcpStream,
    conn: &mut ServerConnection,
) -> io::Result<()> {
    let fd = stream.as_raw_fd();
    let mut read_buf = vec![0u8; 16384];

    while conn.is_handshaking() {
        // 書き込みが必要な場合
        while conn.wants_write() {
            let mut write_buf = Vec::new();
            conn.write_tls(&mut write_buf)?;
            
            let mut written = 0;
            while written < write_buf.len() {
                match raw_write(fd, &write_buf[written..]) {
                    Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0")),
                    Ok(n) => written += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        stream.writable(false).await?;
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        // 読み込みが必要な場合
        if conn.wants_read() {
            loop {
                match raw_read(fd, &mut read_buf) {
                    Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF during handshake")),
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
    // これを送信しないと dangerous_extract_secrets() が失敗する
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
                    stream.writable(false).await?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    Ok(())
}

/// rustls コネクションを使用して非同期ハンドシェイクを実行（クライアント側）
/// 
/// 重要: ハンドシェイク完了後、kTLS のシークレット抽出に備えて
/// バッファリングされた全ての TLS レコードを送信する必要があります。
async fn do_client_handshake(
    stream: &TcpStream,
    conn: &mut ClientConnection,
) -> io::Result<()> {
    let fd = stream.as_raw_fd();
    let mut read_buf = vec![0u8; 16384];

    while conn.is_handshaking() {
        // 書き込みが必要な場合
        while conn.wants_write() {
            let mut write_buf = Vec::new();
            conn.write_tls(&mut write_buf)?;
            
            let mut written = 0;
            while written < write_buf.len() {
                match raw_write(fd, &write_buf[written..]) {
                    Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0")),
                    Ok(n) => written += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        stream.writable(false).await?;
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        // 読み込みが必要な場合
        if conn.wants_read() {
            loop {
                match raw_read(fd, &mut read_buf) {
                    Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF during handshake")),
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
    // これを送信しないと dangerous_extract_secrets() が失敗する
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
                    stream.writable(false).await?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    Ok(())
}

// ====================
// kTLS 有効化（低レベル実装）
// ====================

/// kTLS 有効化の結果
#[cfg(feature = "ktls")]
pub enum KtlsEnableResult<C> {
    /// kTLS 有効化成功
    Enabled(TlsMode),
    /// kTLS 有効化失敗、rustls へフォールバック（conn を返す）
    Fallback(C, String),
    /// 致命的エラー（復旧不可）
    Fatal(io::Error),
}

/// kTLS を有効化する（サーバー側）
/// 
/// ULP設定を先に試み、失敗時はconnを返してrustlsへフォールバック可能にする
#[cfg(feature = "ktls")]
fn try_enable_ktls_server(
    stream: &TcpStream,
    conn: ServerConnection,
) -> KtlsEnableResult<ServerConnection> {
    let fd = stream.as_raw_fd();
    
    // Step 1: ULP設定を試みる（connを消費しない）
    // これが失敗した場合はフォールバック可能
    if let Err(e) = setup_ulp(fd) {
        let msg = format!("ULP setup failed: {}", e);
        ftlog::warn!("kTLS: {}, falling back to rustls", msg);
        return KtlsEnableResult::Fallback(conn, msg);
    }
    
    // Step 2: 暗号スイートを取得
    let cipher_suite = match conn.negotiated_cipher_suite() {
        Some(cs) => cs,
        None => {
            let msg = "No negotiated cipher suite".to_string();
            ftlog::warn!("kTLS: {}, falling back to rustls", msg);
            return KtlsEnableResult::Fallback(conn, msg);
        }
    };
    
    // Step 3: rustls::Connection に変換してシークレット抽出とkTLS設定
    // この時点で conn は消費される
    let rustls_conn = rustls::Connection::Server(conn);
    match setup_ktls_after_ulp(fd, rustls_conn, cipher_suite) {
        Ok(()) => KtlsEnableResult::Enabled(TlsMode::KtlsFull),
        Err(e) => {
            // シークレット抽出後の失敗は致命的（connは既に消費済み）
            KtlsEnableResult::Fatal(e)
        }
    }
}

/// kTLS を有効化する（クライアント側）
/// 
/// ULP設定を先に試み、失敗時はconnを返してrustlsへフォールバック可能にする
#[cfg(feature = "ktls")]
fn try_enable_ktls_client(
    stream: &TcpStream,
    conn: ClientConnection,
) -> KtlsEnableResult<ClientConnection> {
    let fd = stream.as_raw_fd();
    
    // Step 1: ULP設定を試みる（connを消費しない）
    if let Err(e) = setup_ulp(fd) {
        let msg = format!("ULP setup failed: {}", e);
        ftlog::warn!("kTLS: {}, falling back to rustls", msg);
        return KtlsEnableResult::Fallback(conn, msg);
    }
    
    // Step 2: 暗号スイートを取得
    let cipher_suite = match conn.negotiated_cipher_suite() {
        Some(cs) => cs,
        None => {
            let msg = "No negotiated cipher suite".to_string();
            ftlog::warn!("kTLS: {}, falling back to rustls", msg);
            return KtlsEnableResult::Fallback(conn, msg);
        }
    };
    
    // Step 3: rustls::Connection に変換してシークレット抽出とkTLS設定
    let rustls_conn = rustls::Connection::Client(conn);
    match setup_ktls_after_ulp(fd, rustls_conn, cipher_suite) {
        Ok(()) => KtlsEnableResult::Enabled(TlsMode::KtlsFull),
        Err(e) => KtlsEnableResult::Fatal(e),
    }
}

/// ULP設定後のkTLS設定（シークレット抽出とTX/RX設定）
#[cfg(feature = "ktls")]
fn setup_ktls_after_ulp(
    fd: RawFd,
    conn: rustls::Connection,
    cipher_suite: rustls::SupportedCipherSuite,
) -> io::Result<()> {
    use ktls2::CryptoInfo;
    
    // シークレットを抽出
    let secrets = conn.dangerous_extract_secrets()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to extract secrets: {:?}", e)))?;
    
    // TX 設定
    let tx = CryptoInfo::from_rustls(cipher_suite, secrets.tx)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("TX crypto info: {:?}", e)))?;
    setup_tls_info(fd, Direction::Tx, &tx)?;
    
    // RX 設定
    let rx = CryptoInfo::from_rustls(cipher_suite, secrets.rx)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("RX crypto info: {:?}", e)))?;
    setup_tls_info(fd, Direction::Rx, &rx)?;
    
    Ok(())
}

/// setsockopt で TLS ULP を設定
#[cfg(feature = "ktls")]
fn setup_ulp(fd: RawFd) -> io::Result<()> {
    const SOL_TCP: libc::c_int = 6;
    const TCP_ULP: libc::c_int = 31;
    
    let result = unsafe {
        libc::setsockopt(
            fd,
            SOL_TCP,
            TCP_ULP,
            "tls".as_ptr() as *const libc::c_void,
            3,
        )
    };
    
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(feature = "ktls")]
#[derive(Clone, Copy)]
enum Direction {
    Tx,
    Rx,
}

/// setsockopt で TLS 暗号化情報を設定
#[cfg(feature = "ktls")]
fn setup_tls_info(fd: RawFd, dir: Direction, info: &ktls2::CryptoInfo) -> io::Result<()> {
    const SOL_TLS: libc::c_int = 282;
    const TLS_TX: libc::c_int = 1;
    const TLS_RX: libc::c_int = 2;
    
    let direction = match dir {
        Direction::Tx => TLS_TX,
        Direction::Rx => TLS_RX,
    };
    
    let result = unsafe {
        libc::setsockopt(
            fd,
            SOL_TLS,
            direction,
            info.as_ptr(),
            info.size() as libc::socklen_t,
        )
    };
    
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// ====================
// サーバー accept
// ====================

/// TLS ハンドシェイクを実行しサーバー TLS ストリームを作成
/// 
/// kTLS の有効化に失敗した場合の動作は `allow_fallback` で制御されます：
/// - true: rustls にフォールバックして接続を継続
/// - false: kTLS 必須モード、有効化失敗時はエラーを返す
/// 
/// 致命的なエラー（シークレット抽出後の失敗等）は常にエラーを返します。
pub async fn accept(
    stream: TcpStream,
    config: Arc<ServerConfig>,
    enable_ktls: bool,
    allow_fallback: bool,
) -> io::Result<KtlsServerStream> {
    let mut conn = ServerConnection::new(config)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // ハンドシェイクを実行
    do_server_handshake(&stream, &mut conn).await?;

    // kTLS の有効化を試みる
    #[cfg(feature = "ktls")]
    let (mode, conn_option) = if enable_ktls {
        match try_enable_ktls_server(&stream, conn) {
            KtlsEnableResult::Enabled(mode) => {
                // kTLS 有効化成功
                (mode, None)
            }
            KtlsEnableResult::Fallback(returned_conn, reason) => {
                if allow_fallback {
                    // ULP設定失敗等、復旧可能なエラー - rustls にフォールバック
                    // warnログは try_enable_ktls_server 内で既に出力済み
                    let _ = reason;
                    (TlsMode::Rustls, Some(returned_conn))
                } else {
                    // フォールバック無効 - 接続を拒否
                    ftlog::error!(
                        "kTLS unavailable ({}) and fallback disabled, rejecting connection",
                        reason
                    );
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("kTLS required but unavailable: {}", reason)
                    ));
                }
            }
            KtlsEnableResult::Fatal(e) => {
                // シークレット抽出後の失敗等、致命的エラー
                ftlog::error!("kTLS fatal error: {}, connection cannot continue", e);
                return Err(e);
            }
        }
    } else {
        (TlsMode::Rustls, Some(conn))
    };

    #[cfg(not(feature = "ktls"))]
    let (mode, conn_option) = {
        let _ = enable_ktls;
        let _ = allow_fallback;
        (TlsMode::Rustls, Some(conn))
    };

    Ok(KtlsServerStream {
        inner: stream,
        conn: conn_option,
        mode,
    })
}

/// TLS ハンドシェイクを実行しクライアント TLS ストリームを作成
/// 
/// kTLS の有効化に失敗した場合の動作は `allow_fallback` で制御されます：
/// - true: rustls にフォールバックして接続を継続
/// - false: kTLS 必須モード、有効化失敗時はエラーを返す
/// 
/// 致命的なエラー（シークレット抽出後の失敗等）は常にエラーを返します。
pub async fn connect(
    stream: TcpStream,
    config: Arc<ClientConfig>,
    server_name: ServerName<'static>,
    enable_ktls: bool,
    allow_fallback: bool,
) -> io::Result<KtlsClientStream> {
    let mut conn = ClientConnection::new(config, server_name)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // ハンドシェイクを実行
    do_client_handshake(&stream, &mut conn).await?;

    // kTLS の有効化を試みる
    #[cfg(feature = "ktls")]
    let (mode, conn_option) = if enable_ktls {
        match try_enable_ktls_client(&stream, conn) {
            KtlsEnableResult::Enabled(mode) => {
                // kTLS 有効化成功
                (mode, None)
            }
            KtlsEnableResult::Fallback(returned_conn, reason) => {
                if allow_fallback {
                    // ULP設定失敗等、復旧可能なエラー - rustls にフォールバック
                    // warnログは try_enable_ktls_client 内で既に出力済み
                    let _ = reason;
                    (TlsMode::Rustls, Some(returned_conn))
                } else {
                    // フォールバック無効 - 接続を拒否
                    ftlog::error!(
                        "kTLS unavailable ({}) and fallback disabled, rejecting connection",
                        reason
                    );
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("kTLS required but unavailable: {}", reason)
                    ));
                }
            }
            KtlsEnableResult::Fatal(e) => {
                // シークレット抽出後の失敗等、致命的エラー
                ftlog::error!("kTLS fatal error: {}, connection cannot continue", e);
                return Err(e);
            }
        }
    } else {
        (TlsMode::Rustls, Some(conn))
    };

    #[cfg(not(feature = "ktls"))]
    let (mode, conn_option) = {
        let _ = enable_ktls;
        let _ = allow_fallback;
        (TlsMode::Rustls, Some(conn))
    };

    Ok(KtlsClientStream {
        inner: stream,
        conn: conn_option,
        mode,
    })
}

// ====================
// AsyncReadRent / AsyncWriteRent 実装（サーバー側）
// ====================

impl monoio::io::AsyncReadRent for KtlsServerStream {
    async fn read<T: IoBufMut>(&mut self, mut buf: T) -> monoio::BufResult<usize, T> {
        // kTLS が有効な場合は直接 TCP から読み込み（カーネルが復号化）
        if self.mode != TlsMode::Rustls {
            return self.inner.read(buf).await;
        }

        // rustls 経由で読み込み
        let conn = match &mut self.conn {
            Some(c) => c,
            None => return (Err(io::Error::new(io::ErrorKind::Other, "No TLS connection")), buf),
        };

        let fd = self.inner.as_raw_fd();
        let mut read_buf = vec![0u8; 16384];

        loop {
            // 既にデコードされたデータがあるか確認
            let slice = unsafe {
                std::slice::from_raw_parts_mut(buf.write_ptr(), buf.bytes_total())
            };
            
            let mut rd = conn.reader();
            match std::io::Read::read(&mut rd, slice) {
                Ok(n) if n > 0 => {
                    unsafe { buf.set_init(n) };
                    return (Ok(n), buf);
                }
                Ok(0) if !conn.wants_read() => {
                    // EOF
                    return (Ok(0), buf);
                }
                _ => {}
            }

            // TLS データを読み込む
            loop {
                match raw_read(fd, &mut read_buf) {
                    Ok(0) => return (Ok(0), buf),
                    Ok(n) => {
                        if let Err(e) = conn.read_tls(&mut &read_buf[..n]) {
                            return (Err(e), buf);
                        }
                        if let Err(e) = conn.process_new_packets() {
                            return (Err(io::Error::new(io::ErrorKind::InvalidData, e)), buf);
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
        // kTLS が有効な場合は直接 TCP から読み込み
        if self.mode != TlsMode::Rustls {
            return self.inner.readv(buf).await;
        }

        // 簡易実装: iovec の最初のバッファのみ使用
        let iovec_ptr = buf.write_iovec_ptr();
        let iovec_len = buf.write_iovec_len();

        if iovec_len == 0 {
            return (Ok(0), buf);
        }

        let conn = match &mut self.conn {
            Some(c) => c,
            None => return (Err(io::Error::new(io::ErrorKind::Other, "No TLS connection")), buf),
        };

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
            let mut rd = conn.reader();
            match std::io::Read::read(&mut rd, slice) {
                Ok(n) if n > 0 => {
                    return (Ok(n), buf);
                }
                Ok(0) if !conn.wants_read() => {
                    return (Ok(0), buf);
                }
                _ => {}
            }

            loop {
                match raw_read(fd, &mut read_buf) {
                    Ok(0) => return (Ok(0), buf),
                    Ok(n) => {
                        if let Err(e) = conn.read_tls(&mut &read_buf[..n]) {
                            return (Err(e), buf);
                        }
                        if let Err(e) = conn.process_new_packets() {
                            return (Err(io::Error::new(io::ErrorKind::InvalidData, e)), buf);
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

impl monoio::io::AsyncWriteRent for KtlsServerStream {
    async fn write<T: IoBuf>(&mut self, buf: T) -> monoio::BufResult<usize, T> {
        // kTLS が有効な場合は直接 TCP に書き込み（カーネルが暗号化）
        if self.mode != TlsMode::Rustls {
            return self.inner.write(buf).await;
        }

        // rustls 経由で書き込み
        let conn = match &mut self.conn {
            Some(c) => c,
            None => return (Err(io::Error::new(io::ErrorKind::Other, "No TLS connection")), buf),
        };

        let slice = unsafe { std::slice::from_raw_parts(buf.read_ptr(), buf.bytes_init()) };
        
        // データを TLS レコードにエンコード
        let mut wr = conn.writer();
        if let Err(e) = std::io::Write::write_all(&mut wr, slice) {
            return (Err(e), buf);
        }

        // TLS レコードを送信
        let fd = self.inner.as_raw_fd();
        while conn.wants_write() {
            let mut write_buf = Vec::new();
            if let Err(e) = conn.write_tls(&mut write_buf) {
                return (Err(e), buf);
            }

            let mut written = 0;
            while written < write_buf.len() {
                match raw_write(fd, &write_buf[written..]) {
                    Ok(0) => return (Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0")), buf),
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
        // kTLS が有効な場合は直接 TCP に書き込み
        if self.mode != TlsMode::Rustls {
            return self.inner.writev(buf).await;
        }

        // 簡易実装: iovec の最初のバッファのみ使用
        let iovec_ptr = buf.read_iovec_ptr();
        let iovec_len = buf.read_iovec_len();

        if iovec_len == 0 {
            return (Ok(0), buf);
        }

        let conn = match &mut self.conn {
            Some(c) => c,
            None => return (Err(io::Error::new(io::ErrorKind::Other, "No TLS connection")), buf),
        };

        let slice = unsafe {
            let iov = &*iovec_ptr;
            if iov.iov_len == 0 {
                return (Ok(0), buf);
            }
            std::slice::from_raw_parts(iov.iov_base as *const u8, iov.iov_len)
        };

        let mut wr = conn.writer();
        if let Err(e) = std::io::Write::write_all(&mut wr, slice) {
            return (Err(e), buf);
        }

        let fd = self.inner.as_raw_fd();
        while conn.wants_write() {
            let mut write_buf = Vec::new();
            if let Err(e) = conn.write_tls(&mut write_buf) {
                return (Err(e), buf);
            }

            let mut written = 0;
            while written < write_buf.len() {
                match raw_write(fd, &write_buf[written..]) {
                    Ok(0) => return (Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0")), buf),
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
// AsyncReadRent / AsyncWriteRent 実装（クライアント側）
// ====================

impl monoio::io::AsyncReadRent for KtlsClientStream {
    async fn read<T: IoBufMut>(&mut self, mut buf: T) -> monoio::BufResult<usize, T> {
        if self.mode != TlsMode::Rustls {
            return self.inner.read(buf).await;
        }

        let conn = match &mut self.conn {
            Some(c) => c,
            None => return (Err(io::Error::new(io::ErrorKind::Other, "No TLS connection")), buf),
        };

        let fd = self.inner.as_raw_fd();
        let mut read_buf = vec![0u8; 16384];

        loop {
            let slice = unsafe {
                std::slice::from_raw_parts_mut(buf.write_ptr(), buf.bytes_total())
            };

            let mut rd = conn.reader();
            match std::io::Read::read(&mut rd, slice) {
                Ok(n) if n > 0 => {
                    unsafe { buf.set_init(n) };
                    return (Ok(n), buf);
                }
                Ok(0) if !conn.wants_read() => {
                    return (Ok(0), buf);
                }
                _ => {}
            }

            loop {
                match raw_read(fd, &mut read_buf) {
                    Ok(0) => return (Ok(0), buf),
                    Ok(n) => {
                        if let Err(e) = conn.read_tls(&mut &read_buf[..n]) {
                            return (Err(e), buf);
                        }
                        if let Err(e) = conn.process_new_packets() {
                            return (Err(io::Error::new(io::ErrorKind::InvalidData, e)), buf);
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
        if self.mode != TlsMode::Rustls {
            return self.inner.readv(buf).await;
        }

        let iovec_ptr = buf.write_iovec_ptr();
        let iovec_len = buf.write_iovec_len();

        if iovec_len == 0 {
            return (Ok(0), buf);
        }

        let conn = match &mut self.conn {
            Some(c) => c,
            None => return (Err(io::Error::new(io::ErrorKind::Other, "No TLS connection")), buf),
        };

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
            let mut rd = conn.reader();
            match std::io::Read::read(&mut rd, slice) {
                Ok(n) if n > 0 => {
                    return (Ok(n), buf);
                }
                Ok(0) if !conn.wants_read() => {
                    return (Ok(0), buf);
                }
                _ => {}
            }

            loop {
                match raw_read(fd, &mut read_buf) {
                    Ok(0) => return (Ok(0), buf),
                    Ok(n) => {
                        if let Err(e) = conn.read_tls(&mut &read_buf[..n]) {
                            return (Err(e), buf);
                        }
                        if let Err(e) = conn.process_new_packets() {
                            return (Err(io::Error::new(io::ErrorKind::InvalidData, e)), buf);
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

impl monoio::io::AsyncWriteRent for KtlsClientStream {
    async fn write<T: IoBuf>(&mut self, buf: T) -> monoio::BufResult<usize, T> {
        if self.mode != TlsMode::Rustls {
            return self.inner.write(buf).await;
        }

        let conn = match &mut self.conn {
            Some(c) => c,
            None => return (Err(io::Error::new(io::ErrorKind::Other, "No TLS connection")), buf),
        };

        let slice = unsafe { std::slice::from_raw_parts(buf.read_ptr(), buf.bytes_init()) };

        let mut wr = conn.writer();
        if let Err(e) = std::io::Write::write_all(&mut wr, slice) {
            return (Err(e), buf);
        }

        let fd = self.inner.as_raw_fd();
        while conn.wants_write() {
            let mut write_buf = Vec::new();
            if let Err(e) = conn.write_tls(&mut write_buf) {
                return (Err(e), buf);
            }

            let mut written = 0;
            while written < write_buf.len() {
                match raw_write(fd, &write_buf[written..]) {
                    Ok(0) => return (Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0")), buf),
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
        if self.mode != TlsMode::Rustls {
            return self.inner.writev(buf).await;
        }

        let iovec_ptr = buf.read_iovec_ptr();
        let iovec_len = buf.read_iovec_len();

        if iovec_len == 0 {
            return (Ok(0), buf);
        }

        let conn = match &mut self.conn {
            Some(c) => c,
            None => return (Err(io::Error::new(io::ErrorKind::Other, "No TLS connection")), buf),
        };

        let slice = unsafe {
            let iov = &*iovec_ptr;
            if iov.iov_len == 0 {
                return (Ok(0), buf);
            }
            std::slice::from_raw_parts(iov.iov_base as *const u8, iov.iov_len)
        };

        let mut wr = conn.writer();
        if let Err(e) = std::io::Write::write_all(&mut wr, slice) {
            return (Err(e), buf);
        }

        let fd = self.inner.as_raw_fd();
        while conn.wants_write() {
            let mut write_buf = Vec::new();
            if let Err(e) = conn.write_tls(&mut write_buf) {
                return (Err(e), buf);
            }

            let mut written = 0;
            while written < write_buf.len() {
                match raw_write(fd, &write_buf[written..]) {
                    Ok(0) => return (Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0")), buf),
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
// TLS アクセプター / コネクター
// ====================

/// TLS アクセプター（サーバー側）
#[derive(Clone)]
pub struct RustlsAcceptor {
    config: Arc<ServerConfig>,
    /// kTLS を有効化するかどうか
    enable_ktls: bool,
    /// kTLS 有効化失敗時に rustls へフォールバックを許可するかどうか
    allow_fallback: bool,
}

impl RustlsAcceptor {
    /// 新しいアクセプターを作成
    pub fn new(config: Arc<ServerConfig>) -> Self {
        RustlsAcceptor {
            config,
            enable_ktls: false,
            allow_fallback: true,  // デフォルトはフォールバック有効
        }
    }

    /// kTLS を有効化
    pub fn with_ktls(mut self, enable: bool) -> Self {
        self.enable_ktls = enable;
        self
    }

    /// kTLS 有効化失敗時のフォールバックを設定
    /// 
    /// - true: kTLS 失敗時は rustls で継続（デフォルト）
    /// - false: kTLS 必須（失敗時は接続拒否）
    pub fn with_fallback(mut self, allow: bool) -> Self {
        self.allow_fallback = allow;
        self
    }

    /// TLS ハンドシェイクを実行
    pub async fn accept(&self, stream: TcpStream) -> io::Result<KtlsServerStream> {
        accept(stream, self.config.clone(), self.enable_ktls, self.allow_fallback).await
    }
}

/// TLS コネクター（クライアント側）
#[derive(Clone)]
pub struct RustlsConnector {
    config: Arc<ClientConfig>,
    /// kTLS を有効化するかどうか
    enable_ktls: bool,
    /// kTLS 有効化失敗時に rustls へフォールバックを許可するかどうか
    allow_fallback: bool,
}

impl RustlsConnector {
    /// 新しいコネクターを作成
    pub fn new(config: Arc<ClientConfig>) -> Self {
        RustlsConnector {
            config,
            enable_ktls: false,
            allow_fallback: true,  // デフォルトはフォールバック有効
        }
    }

    /// kTLS を有効化
    pub fn with_ktls(mut self, enable: bool) -> Self {
        self.enable_ktls = enable;
        self
    }

    /// kTLS 有効化失敗時のフォールバックを設定
    /// 
    /// - true: kTLS 失敗時は rustls で継続（デフォルト）
    /// - false: kTLS 必須（失敗時は接続拒否）
    pub fn with_fallback(mut self, allow: bool) -> Self {
        self.allow_fallback = allow;
        self
    }

    /// TLS ハンドシェイクを実行
    pub async fn connect(
        &self,
        stream: TcpStream,
        server_name: &str,
    ) -> io::Result<KtlsClientStream> {
        let server_name = ServerName::try_from(server_name.to_string())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        connect(stream, self.config.clone(), server_name, self.enable_ktls, self.allow_fallback).await
    }
}

// ====================
// sendfile サポート（kTLS 有効時）
// ====================

/// kTLS が有効なソケットに対して sendfile システムコールを実行
pub fn sendfile_ktls(
    socket_fd: RawFd,
    file_fd: RawFd,
    offset: &mut i64,
    count: usize,
) -> io::Result<usize> {
    let result = unsafe { libc::sendfile(socket_fd, file_fd, offset as *mut i64, count) };

    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

impl KtlsServerStream {
    /// sendfile によるゼロコピー送信（kTLS 有効時のみ）
    pub fn sendfile(&self, file_fd: RawFd, offset: &mut i64, count: usize) -> io::Result<usize> {
        if !self.is_ktls_send_enabled() {
            return Err(io::Error::other("kTLS TX is not enabled. Cannot use sendfile."));
        }

        sendfile_ktls(self.as_raw_fd(), file_fd, offset, count)
    }
}

impl KtlsClientStream {
    /// sendfile によるゼロコピー送信（kTLS 有効時のみ）
    pub fn sendfile(&self, file_fd: RawFd, offset: &mut i64, count: usize) -> io::Result<usize> {
        if !self.is_ktls_send_enabled() {
            return Err(io::Error::other("kTLS TX is not enabled. Cannot use sendfile."));
        }

        sendfile_ktls(self.as_raw_fd(), file_fd, offset, count)
    }
}

// ====================
// splice サポート
// ====================

/// splice システムコールのフラグ
pub mod splice_flags {
    pub const SPLICE_F_NONBLOCK: libc::c_uint = 0x02;
    pub const SPLICE_F_MOVE: libc::c_uint = 0x01;
    pub const SPLICE_F_MORE: libc::c_uint = 0x04;
}

/// splice(2) システムコールのラッパー
pub fn splice(
    fd_in: RawFd,
    off_in: Option<&mut i64>,
    fd_out: RawFd,
    off_out: Option<&mut i64>,
    len: usize,
    flags: libc::c_uint,
) -> io::Result<usize> {
    let off_in_ptr = match off_in {
        Some(off) => off as *mut i64,
        None => std::ptr::null_mut(),
    };
    let off_out_ptr = match off_out {
        Some(off) => off as *mut i64,
        None => std::ptr::null_mut(),
    };

    let result = unsafe { libc::splice(fd_in, off_in_ptr, fd_out, off_out_ptr, len, flags) };

    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

/// パイプを作成
pub fn create_pipe() -> io::Result<(RawFd, RawFd)> {
    let mut fds: [libc::c_int; 2] = [0; 2];
    let result = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };

    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok((fds[0], fds[1]))
    }
}

/// パイプを閉じる
pub fn close_pipe(read_fd: RawFd, write_fd: RawFd) {
    unsafe {
        libc::close(read_fd);
        libc::close(write_fd);
    }
}

/// パイプのバッファサイズを設定
pub fn set_pipe_size(pipe_fd: RawFd, size: i32) -> io::Result<i32> {
    let result = unsafe { libc::fcntl(pipe_fd, libc::F_SETPIPE_SZ, size) };

    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

/// splice を使用したゼロコピー転送
pub fn splice_transfer(
    src_fd: RawFd,
    dst_fd: RawFd,
    pipe_read_fd: RawFd,
    pipe_write_fd: RawFd,
    chunk_size: usize,
) -> io::Result<usize> {
    use splice_flags::*;

    // Step 1: src_fd → パイプ
    let to_pipe = splice(
        src_fd,
        None,
        pipe_write_fd,
        None,
        chunk_size,
        SPLICE_F_NONBLOCK | SPLICE_F_MOVE,
    )?;

    if to_pipe == 0 {
        return Ok(0);
    }

    // Step 2: パイプ → dst_fd
    let mut transferred = 0;
    while transferred < to_pipe {
        let remaining = to_pipe - transferred;
        match splice(
            pipe_read_fd,
            None,
            dst_fd,
            None,
            remaining,
            SPLICE_F_NONBLOCK | SPLICE_F_MOVE,
        ) {
            Ok(0) => break,
            Ok(n) => transferred += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                break;
            }
            Err(e) => return Err(e),
        }
    }

    Ok(transferred)
}

/// 再利用可能なパイプペアを管理する構造体
pub struct SplicePipe {
    read_fd: RawFd,
    write_fd: RawFd,
}

impl SplicePipe {
    /// 新しいパイプペアを作成
    pub fn new() -> io::Result<Self> {
        let (read_fd, write_fd) = create_pipe()?;
        let _ = set_pipe_size(write_fd, 1024 * 1024);

        Ok(SplicePipe { read_fd, write_fd })
    }

    pub fn read_fd(&self) -> RawFd {
        self.read_fd
    }

    pub fn write_fd(&self) -> RawFd {
        self.write_fd
    }

    pub fn transfer(&self, src_fd: RawFd, dst_fd: RawFd, chunk_size: usize) -> io::Result<usize> {
        splice_transfer(src_fd, dst_fd, self.read_fd, self.write_fd, chunk_size)
    }
}

impl Drop for SplicePipe {
    fn drop(&mut self) {
        close_pipe(self.read_fd, self.write_fd);
    }
}

impl Default for SplicePipe {
    fn default() -> Self {
        Self::new().expect("Failed to create splice pipe")
    }
}

// ====================
// kTLS サポートチェック
// ====================

/// kTLS が利用可能かどうかをチェック
pub fn is_ktls_available() -> bool {
    // /proc/modules で tls モジュールがロードされているか確認
    if let Ok(modules) = std::fs::read_to_string("/proc/modules") {
        if !modules.lines().any(|line| line.starts_with("tls ")) {
            return false;
        }
    } else {
        return false;
    }

    // カーネルバージョンをチェック
    if let Ok(version) = std::fs::read_to_string("/proc/version") {
        if let Some(ver_str) = version.split_whitespace().nth(2) {
            let parts: Vec<&str> = ver_str.split('.').collect();
            if parts.len() >= 2 {
                if let (Ok(major), Ok(minor)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                    if major < 5 || (major == 5 && minor < 15) {
                        ftlog::warn!(
                            "ktls2: Kernel {}.{} detected. kTLS works best on 5.15+",
                            major,
                            minor
                        );
                    }
                }
            }
        }
    }

    true
}

// ====================
// クライアント設定ヘルパー
// ====================

/// クライアント TLS 設定を作成
/// 
/// # Arguments
/// 
/// * `enable_ktls` - kTLS を有効化する場合は true。
///                   true の場合、シークレット抽出が有効化される。
pub fn client_config(enable_ktls: bool) -> Arc<ClientConfig> {
    let mut root_store = RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let mut config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    // kTLS が有効な場合のみシークレット抽出を有効化
    if enable_ktls {
        config.enable_secret_extraction = true;
    }

    Arc::new(config)
}

/// デフォルトのクライアント TLS 設定を作成（kTLS 無効）
/// 
/// 後方互換性のためのラッパー関数
pub fn default_client_config() -> Arc<ClientConfig> {
    client_config(false)
}
