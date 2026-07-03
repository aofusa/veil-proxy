//! E2Eテスト用バックエンドサーバー
//!
//! - WebSocket Echo Server (WS_PORT env var, default 9005)
//! - HTTP 500 Error Server (ERROR_PORT env var, default 9006)
//! - HTTP Chunked Streaming Server (CHUNKED_PORT env var, default 9007)

use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info};

/// WebSocket Echoサーバー: 受信メッセージをそのまま返送する
async fn run_ws_echo_server(addr: SocketAddr) {
    let listener = TcpListener::bind(addr).await
        .unwrap_or_else(|e| panic!("Failed to bind WS server on {}: {}", addr, e));
    info!("WebSocket echo server listening on {}", addr);

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                debug!("New WS connection from {}", peer);
                tokio::spawn(async move {
                    if let Err(e) = handle_ws(stream).await {
                        debug!("WS connection error: {}", e);
                    }
                });
            }
            Err(e) => error!("WS accept error: {}", e),
        }
    }
}

async fn handle_ws(stream: TcpStream) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut sink, mut source) = ws.split();
    while let Some(msg) = source.next().await {
        let msg = msg?;
        if msg.is_close() {
            break;
        }
        sink.send(msg).await?;
    }
    Ok(())
}

/// HTTP 500エラーサーバー: 常にHTTP 500を返す
async fn run_http_error_server(addr: SocketAddr) {
    let listener = TcpListener::bind(addr).await
        .unwrap_or_else(|e| panic!("Failed to bind error server on {}: {}", addr, e));
    info!("HTTP 500 error server listening on {}", addr);

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                debug!("New HTTP connection from {}", peer);
                tokio::spawn(async move {
                    if let Err(e) = handle_http_error(stream).await {
                        debug!("HTTP error handler error: {}", e);
                    }
                });
            }
            Err(e) => error!("HTTP accept error: {}", e),
        }
    }
}

async fn handle_http_error(mut stream: TcpStream) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // リクエストヘッダーを読み捨て（\r\n\r\n まで）してから 500 を返す
    let mut buf = vec![0u8; 4096];
    let mut total = 0;
    loop {
        let n = stream.read(&mut buf[total..]).await?;
        if n == 0 {
            break;
        }
        total += n;
        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if total == buf.len() {
            buf.resize(buf.len() * 2, 0);
        }
    }

    let response = b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nContent-Length: 21\r\nConnection: close\r\n\r\nInternal Server Error";
    stream.write_all(response).await?;
    stream.flush().await?;
    Ok(())
}

/// F-32 用 chunked ストリーミングサーバー。
///
/// `Transfer-Encoding: chunked` で決定論的なボディ（`body[i] = (i % 256)`）を、
/// 非2冪のチャンクサイズ（7000B）で送る。合計サイズは HTTP/2 の初期フロー制御ウィンドウ
/// （65535B）を大きく超える `CHUNKED_TOTAL` バイトにし、プロキシのストリーミング転送で
/// WINDOW_UPDATE 待ち（バックプレッシャ）と複数 DATA フレーム分割を必ず通る。
/// content-type は非圧縮対象の application/octet-stream（圧縮フォールバックを避け、
/// 新規ストリーミング経路を確実に実行させる）。
const CHUNKED_TOTAL: usize = 200_000;
const CHUNKED_CHUNK: usize = 7000;

/// テストが期待値を再構成できるよう公開する決定論的ボディ生成ロジック。
fn chunked_body_byte(i: usize) -> u8 {
    (i % 256) as u8
}

async fn run_chunked_server(addr: SocketAddr) {
    let listener = TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("Failed to bind chunked server on {}: {}", addr, e));
    info!("HTTP chunked streaming server listening on {}", addr);

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                debug!("New chunked HTTP connection from {}", peer);
                tokio::spawn(async move {
                    if let Err(e) = handle_chunked(stream).await {
                        debug!("Chunked handler error: {}", e);
                    }
                });
            }
            Err(e) => error!("Chunked accept error: {}", e),
        }
    }
}

async fn handle_chunked(mut stream: TcpStream) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // リクエストヘッダーを読み捨て（\r\n\r\n まで）
    let mut buf = vec![0u8; 4096];
    let mut total = 0;
    loop {
        let n = stream.read(&mut buf[total..]).await?;
        if n == 0 {
            return Ok(());
        }
        total += n;
        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if total == buf.len() {
            buf.resize(buf.len() * 2, 0);
        }
    }

    // chunked レスポンスを構築（テストバックエンドなのでアロケーションは許容）
    let mut out: Vec<u8> = Vec::with_capacity(CHUNKED_TOTAL + CHUNKED_TOTAL / CHUNKED_CHUNK * 16 + 256);
    out.extend_from_slice(
        b"HTTP/1.1 200 OK\r\n\
          Content-Type: application/octet-stream\r\n\
          Transfer-Encoding: chunked\r\n\
          Connection: close\r\n\r\n",
    );
    let mut offset = 0;
    while offset < CHUNKED_TOTAL {
        let len = CHUNKED_CHUNK.min(CHUNKED_TOTAL - offset);
        // チャンクサイズ行（16進）
        out.extend_from_slice(format!("{:x}\r\n", len).as_bytes());
        // チャンクデータ（決定論的パターン）
        for i in 0..len {
            out.push(chunked_body_byte(offset + i));
        }
        out.extend_from_slice(b"\r\n");
        offset += len;
    }
    // 終端チャンク + トレーラー終端
    out.extend_from_slice(b"0\r\n\r\n");

    stream.write_all(&out).await?;
    stream.flush().await?;
    Ok(())
}

/// ボディエコーサーバー（F-32: HTTP/2 リクエスト方向ストリーミング検証用）
///
/// リクエストボディ（`Transfer-Encoding: chunked` または `Content-Length`）を読み取り、
/// そのままレスポンスボディとして返す。プロキシのリクエストストリーミング経路は
/// chunked でバックエンドへ転送するため、chunked のデコードを正しく行えること、
/// および大容量ボディがバイト単位で完全一致で届くことを検証できる。
async fn run_echo_server(addr: SocketAddr) {
    let listener = TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("Failed to bind echo server on {}: {}", addr, e));
    info!("HTTP body-echo server listening on {}", addr);

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                debug!("New echo HTTP connection from {}", peer);
                tokio::spawn(async move {
                    if let Err(e) = handle_echo(stream).await {
                        debug!("Echo handler error: {}", e);
                    }
                });
            }
            Err(e) => error!("Echo accept error: {}", e),
        }
    }
}

/// TLS 版ボディエコーサーバー（F-44: TLS バックエンドストリーミングの E2E 用）
async fn run_tls_echo_server(addr: SocketAddr, cert_path: String, key_path: String) {
    use std::sync::Arc;
    use tokio_rustls::rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};

    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&cert_path)
        .unwrap_or_else(|e| panic!("Failed to read cert {}: {}", cert_path, e))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|e| panic!("Failed to parse cert {}: {}", cert_path, e));
    let key = PrivateKeyDer::from_pem_file(&key_path)
        .unwrap_or_else(|e| panic!("Failed to read key {}: {}", key_path, e));
    let config = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap_or_else(|e| panic!("Failed to build TLS config: {}", e));
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

    let listener = TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("Failed to bind TLS echo server on {}: {}", addr, e));
    info!("HTTPS body-echo server listening on {}", addr);

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                debug!("New echo HTTPS connection from {}", peer);
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    match acceptor.accept(stream).await {
                        Ok(tls) => {
                            if let Err(e) = handle_echo(tls).await {
                                debug!("TLS echo handler error: {}", e);
                            }
                        }
                        Err(e) => debug!("TLS echo handshake error: {}", e),
                    }
                });
            }
            Err(e) => error!("TLS echo accept error: {}", e),
        }
    }
}

async fn handle_echo<S>(mut stream: S) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // ヘッダー終端 (\r\n\r\n) まで読む（ボディの先頭も buf に入りうる）
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut tmp = [0u8; 8192];
    let header_end;
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
        if buf.len() > 1 << 20 {
            return Ok(()); // ヘッダーが大きすぎる
        }
    }

    let header_str = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
    let is_chunked = header_str.contains("transfer-encoding: chunked");
    let content_length: Option<usize> = header_str
        .split("content-length:")
        .nth(1)
        .and_then(|s| s.split("\r\n").next())
        .and_then(|s| s.trim().parse().ok());

    // ボディを読み取り、デコードして echo 用バッファへ
    let mut body: Vec<u8> = Vec::new();
    let mut rest = buf[header_end..].to_vec();

    if is_chunked {
        // chunked デコード（rest を起点に必要に応じて読み足す）
        let mut pos = 0usize;
        'outer: loop {
            // チャンクサイズ行を読む
            let size_line_end = loop {
                if let Some(p) = rest[pos..].windows(2).position(|w| w == b"\r\n") {
                    break pos + p;
                }
                let n = stream.read(&mut tmp).await?;
                if n == 0 {
                    break 'outer;
                }
                rest.extend_from_slice(&tmp[..n]);
            };
            let size_hex = String::from_utf8_lossy(&rest[pos..size_line_end]);
            let chunk_size = usize::from_str_radix(size_hex.trim(), 16).unwrap_or(0);
            pos = size_line_end + 2; // skip CRLF
            if chunk_size == 0 {
                break; // 終端チャンク
            }
            // チャンクデータ + 末尾 CRLF を確保
            while rest.len() < pos + chunk_size + 2 {
                let n = stream.read(&mut tmp).await?;
                if n == 0 {
                    break 'outer;
                }
                rest.extend_from_slice(&tmp[..n]);
            }
            body.extend_from_slice(&rest[pos..pos + chunk_size]);
            pos += chunk_size + 2; // skip data + CRLF
        }
    } else if let Some(len) = content_length {
        body.extend_from_slice(&rest);
        while body.len() < len {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        body.truncate(len);
    } else {
        // ボディ無し
        body.extend_from_slice(&rest);
    }

    // echo レスポンス
    let mut out: Vec<u8> = Vec::with_capacity(body.len() + 128);
    out.extend_from_slice(b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: ");
    out.extend_from_slice(body.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
    out.extend_from_slice(&body);

    stream.write_all(&out).await?;
    stream.flush().await?;
    Ok(())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info".to_string())
                .as_str(),
        )
        .init();

    let ws_port: u16 = std::env::var("WS_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9005);
    let error_port: u16 = std::env::var("ERROR_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9006);
    let chunked_port: u16 = std::env::var("CHUNKED_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9007);
    let echo_port: u16 = std::env::var("ECHO_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9008);
    let tls_echo_port: u16 = std::env::var("TLS_ECHO_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9018);
    let tls_cert = std::env::var("TLS_CERT_PATH").unwrap_or_else(|_| "cert.pem".to_string());
    let tls_key = std::env::var("TLS_KEY_PATH").unwrap_or_else(|_| "key.pem".to_string());

    let ws_addr: SocketAddr = format!("127.0.0.1:{}", ws_port).parse().unwrap();
    let error_addr: SocketAddr = format!("127.0.0.1:{}", error_port).parse().unwrap();
    let chunked_addr: SocketAddr = format!("127.0.0.1:{}", chunked_port).parse().unwrap();
    let echo_addr: SocketAddr = format!("127.0.0.1:{}", echo_port).parse().unwrap();
    let tls_echo_addr: SocketAddr = format!("127.0.0.1:{}", tls_echo_port).parse().unwrap();

    info!(
        "Starting test-backends: WS={}, HTTP-error={}, chunked={}, echo={}, tls-echo={}",
        ws_addr, error_addr, chunked_addr, echo_addr, tls_echo_addr
    );

    tokio::join!(
        run_ws_echo_server(ws_addr),
        run_http_error_server(error_addr),
        run_chunked_server(chunked_addr),
        run_echo_server(echo_addr),
        run_tls_echo_server(tls_echo_addr, tls_cert, tls_key),
    );
}
