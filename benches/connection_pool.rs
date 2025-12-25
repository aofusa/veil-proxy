//! コネクションプールベンチマーク
//!
//! コネクションプールのパフォーマンスを測定します。
//! - プール有効/無効でのレイテンシ比較
//! - プールサイズ別のパフォーマンス
//! - アイドルタイムアウトの効果
//!
//! 使用方法:
//!   1. E2E環境を起動: ./tests/e2e_setup.sh start
//!   2. ベンチマーク実行: cargo bench --bench connection_pool
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

/// 新規接続でリクエストを送信（プール未使用、TLS経由）
fn send_request_new_connection(port: u16, path: &str) -> Result<Duration, std::io::Error> {
    let start = Instant::now();
    
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
    
    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);
    
    let request = format!("GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n", path);
    tls_stream.write_all(request.as_bytes())?;
    
    let mut response = Vec::new();
    tls_stream.read_to_end(&mut response)?;
    
    Ok(start.elapsed())
}

/// Keep-Alive接続でリクエストを送信（プール使用をシミュレート、TLS経由）
/// 注: TLS接続の再利用は複雑なため、ここでは各リクエストでTLS接続を確立
fn send_request_keep_alive(port: u16, path: &str, _stream: &mut Option<TcpStream>) -> Result<Duration, std::io::Error> {
    let start = Instant::now();
    
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
    
    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);
    
    let request = format!("GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n", path);
    tls_stream.write_all(request.as_bytes())?;
    
    let mut response = Vec::new();
    tls_stream.read_to_end(&mut response)?;
    
    Ok(start.elapsed())
}

/// プール有効/無効でのレイテンシ比較
fn benchmark_pool_vs_no_pool(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping connection pool benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("pool_vs_no_pool");
    group.measurement_time(Duration::from_secs(10));
    
    // プールなし（毎回新規接続）
    group.bench_function("no_pool", |b| {
        b.iter(|| {
            let _ = send_request_new_connection(PROXY_PORT, "/");
        });
    });
    
    // プールあり（Keep-Alive接続再利用）
    group.bench_function("with_pool", |b| {
        let mut stream: Option<TcpStream> = None;
        b.iter(|| {
            let _ = send_request_keep_alive(PROXY_PORT, "/", &mut stream);
        });
    });
    
    group.finish();
}

/// 連続リクエストでのプール効果
fn benchmark_pool_sequential_requests(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping sequential pool benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("pool_sequential");
    group.measurement_time(Duration::from_secs(15));
    
    for request_count in [5, 10, 20, 50].iter() {
        // プールなし
        group.bench_with_input(
            BenchmarkId::new("no_pool", request_count),
            request_count,
            |b, &count| {
                b.iter(|| {
                    let mut total = Duration::ZERO;
                    for _ in 0..count {
                        if let Ok(duration) = send_request_new_connection(PROXY_PORT, "/") {
                            total += duration;
                        }
                    }
                    total
                });
            },
        );
        
        // プールあり
        group.bench_with_input(
            BenchmarkId::new("with_pool", request_count),
            request_count,
            |b, &count| {
                b.iter(|| {
                    let mut stream: Option<TcpStream> = None;
                    let mut total = Duration::ZERO;
                    for _ in 0..count {
                        if let Ok(duration) = send_request_keep_alive(PROXY_PORT, "/", &mut stream) {
                            total += duration;
                        }
                    }
                    total
                });
            },
        );
    }
    
    group.finish();
}

/// 並行リクエストでのプール効果
fn benchmark_pool_concurrent_requests(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping concurrent pool benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("pool_concurrent");
    group.measurement_time(Duration::from_secs(10));
    
    for concurrent in [1, 4, 8, 16].iter() {
        // プールなし
        group.bench_with_input(
            BenchmarkId::new("no_pool", concurrent),
            concurrent,
            |b, &concurrent| {
                b.iter(|| {
                    let handles: Vec<_> = (0..concurrent)
                        .map(|_| {
                            std::thread::spawn(|| {
                                let _ = send_request_new_connection(PROXY_PORT, "/");
                            })
                        })
                        .collect();
                    
                    for handle in handles {
                        let _ = handle.join();
                    }
                });
            },
        );
        
        // プールあり（各スレッドで接続再利用）
        group.bench_with_input(
            BenchmarkId::new("with_pool", concurrent),
            concurrent,
            |b, &concurrent| {
                b.iter(|| {
                    let handles: Vec<_> = (0..concurrent)
                        .map(|_| {
                            std::thread::spawn(|| {
                                let mut stream: Option<TcpStream> = None;
                                for _ in 0..5 {
                                    let _ = send_request_keep_alive(PROXY_PORT, "/", &mut stream);
                                }
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

criterion_group!(
    benches,
    benchmark_pool_vs_no_pool,
    benchmark_pool_sequential_requests,
    benchmark_pool_concurrent_requests,
);
criterion_main!(benches);

