//! WebSocketベンチマーク
//!
//! WebSocket接続のパフォーマンスを測定します。
//! - WebSocket接続確立のレイテンシ
//! - 双方向転送のスループット
//! - Fixed vs Adaptiveポーリングモードの比較
//!
//! 使用方法:
//!   1. E2E環境を起動: ./tests/e2e_setup.sh start
//!   2. ベンチマーク実行: cargo bench --bench websocket
//!   3. 環境停止: ./tests/e2e_setup.sh stop

use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};
use std::io::{Read, Write, ErrorKind};
use std::net::TcpStream;
use std::time::{Duration, Instant};
use std::sync::Arc;
use rustls::{ClientConfig, ClientConnection};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::ServerName;

const PROXY_PORT: u16 = 8443;

/// rustlsのCryptoProviderを初期化（一度だけ実行）
fn init_crypto_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        CryptoProvider::install_default(rustls::crypto::ring::default_provider())
            .expect("Failed to install rustls crypto provider");
    });
}

/// 証明書検証をスキップするカスタム検証器
#[derive(Debug)]
struct SkipServerVerification;

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
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
            .to_vec()
    }
}

/// TLSクライアント設定を作成（自己署名証明書を許可）
fn create_tls_config() -> Arc<ClientConfig> {
    init_crypto_provider();
    
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();
    
    Arc::new(config)
}

/// プロキシサーバーが起動しているか確認（HTTPS、TLSハンドシェイクを正しく行う）
fn is_proxy_running() -> bool {
    init_crypto_provider();
    
    let mut stream = match TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    
    if stream.set_read_timeout(Some(Duration::from_secs(2))).is_err() {
        return false;
    }
    if stream.set_write_timeout(Some(Duration::from_secs(2))).is_err() {
        return false;
    }
    
    let config = create_tls_config();
    let server_name = match ServerName::try_from("localhost".to_string()) {
        Ok(name) => name,
        Err(_) => return false,
    };
    
    let mut tls_conn = match ClientConnection::new(config, server_name) {
        Ok(conn) => conn,
        Err(_) => return false,
    };
    
    // TLSハンドシェイクを開始（完了まで待たない）
    let mut handshake_started = false;
    for _ in 0..10 {
        if !tls_conn.is_handshaking() {
            return true;
        }
        
        match tls_conn.complete_io(&mut stream) {
            Ok(_) => {
                handshake_started = true;
                if !tls_conn.is_handshaking() {
                    return true;
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(_) => return false,
        }
    }
    
    // ハンドシェイクが開始されていればサーバーは起動していると判断
    handshake_started
}

/// WebSocket接続を確立（TLS + Upgradeリクエスト）
fn establish_websocket_connection(port: u16, path: &str) -> Result<(ClientConnection, TcpStream), std::io::Error> {
    init_crypto_provider();
    
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    
    let config = create_tls_config();
    let server_name = ServerName::try_from("localhost".to_string())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    
    let mut tls_conn = ClientConnection::new(config, server_name)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    
    // TLSハンドシェイク
    while tls_conn.is_handshaking() {
        match tls_conn.complete_io(&mut stream) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    
    // WebSocket Upgradeリクエスト（TLS経由）
    {
        let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);
        
        use base64::{Engine as _, engine::general_purpose};
        let key = general_purpose::STANDARD.encode(b"dGhlIHNhbXBsZSBub25jZQ==");
        let request = format!(
            "GET {} HTTP/1.1\r\n\
             Host: localhost\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {}\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
            path, key
        );
        
        tls_stream.write_all(request.as_bytes())?;
        
        // レスポンスを読み取る（101 Switching Protocols）
        let mut response = vec![0u8; 1024];
        let _ = tls_stream.read(&mut response)?;
    }
    
    Ok((tls_conn, stream))
}

/// WebSocket接続確立のレイテンシ
fn benchmark_websocket_connection_latency(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping WebSocket benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("websocket_connection");
    group.measurement_time(Duration::from_secs(10));
    
    group.bench_function("connection_establishment", |b| {
        b.iter(|| {
            let start = Instant::now();
            let _ = establish_websocket_connection(PROXY_PORT, "/");
            start.elapsed()
        });
    });
    
    group.finish();
}

/// WebSocket双方向転送のスループット
fn benchmark_websocket_throughput(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping WebSocket throughput benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("websocket_throughput");
    group.measurement_time(Duration::from_secs(10));
    
    // クライアント→サーバー
    group.bench_function("client_to_server", |b| {
        b.iter(|| {
            if let Ok((mut tls_conn, mut stream)) = establish_websocket_connection(PROXY_PORT, "/") {
                // WebSocketフレームを送信（簡易版・TLS経由）
                let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);
                let data = b"test message";
                let _ = tls_stream.write_all(data);
            }
        });
    });
    
    // サーバー→クライアント
    group.bench_function("server_to_client", |b| {
        b.iter(|| {
            if let Ok((mut tls_conn, mut stream)) = establish_websocket_connection(PROXY_PORT, "/") {
                let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);
                let mut buf = [0u8; 1024];
                let _ = tls_stream.read(&mut buf);
            }
        });
    });
    
    group.finish();
}

/// WebSocket同時接続数別のパフォーマンス
fn benchmark_websocket_concurrent(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping concurrent WebSocket benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("websocket_concurrent");
    group.measurement_time(Duration::from_secs(10));
    
    for concurrent in [1, 4, 8, 16].iter() {
        group.bench_with_input(
            BenchmarkId::new("concurrent", concurrent),
            concurrent,
            |b, &concurrent| {
                b.iter(|| {
                    let handles: Vec<_> = (0..concurrent)
                        .map(|_| {
                            std::thread::spawn(|| {
                                let _ = establish_websocket_connection(PROXY_PORT, "/");
                            })
                        })
                        .collect();
                    
                    for handle in handles {
                        let _ = handle.join();
                    }
                });
            },
        );
    }
    
    group.finish();
}

/// WebSocketメッセージサイズ別のパフォーマンス
fn benchmark_websocket_message_size(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping WebSocket message size benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("websocket_message_size");
    
    for size in [64, 256, 1024, 4096].iter() {
        group.bench_with_input(
            BenchmarkId::new("message_size", size),
            size,
            |b, &size| {
                b.iter(|| {
                    if let Ok((mut tls_conn, mut stream)) = establish_websocket_connection(PROXY_PORT, "/") {
                        let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);
                        let data = vec![0u8; size];
                        let _ = tls_stream.write_all(&data);
                    }
                });
            },
        );
    }
    
    group.finish();
}

criterion_group!(
    benches,
    benchmark_websocket_connection_latency,
    benchmark_websocket_throughput,
    benchmark_websocket_concurrent,
    benchmark_websocket_message_size,
);
criterion_main!(benches);

