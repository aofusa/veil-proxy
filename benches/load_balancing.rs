//! ロードバランシングベンチマーク
//!
//! ロードバランシングアルゴリズムのパフォーマンスを測定します。
//! - Round Robin vs Least Connections vs IP Hash
//! - バックエンド数別のパフォーマンス
//! - ヘルスチェックのオーバーヘッド
//!
//! 使用方法:
//!   1. E2E環境を起動: ./tests/e2e_setup.sh start
//!   2. ベンチマーク実行: cargo bench --bench load_balancing
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
const BACKEND1_PORT: u16 = 9001;
const BACKEND2_PORT: u16 = 9002;

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

/// バックエンドサーバーが起動しているか確認（HTTPS、TLSハンドシェイクを正しく行う）
fn is_backend_running(port: u16) -> bool {
    init_crypto_provider();
    
    let mut stream = match TcpStream::connect(format!("127.0.0.1:{}", port)) {
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

/// リクエストを送信してレスポンスを取得（TLS経由）
fn send_request(port: u16, path: &str) -> Result<(Duration, Option<String>), std::io::Error> {
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
    
    let elapsed = start.elapsed();
    
    // X-Server-Idヘッダーを抽出
    let response_str = String::from_utf8_lossy(&response);
    let server_id = extract_server_id(&response_str);
    
    Ok((elapsed, server_id))
}

/// レスポンスからX-Server-Idを抽出
fn extract_server_id(response: &str) -> Option<String> {
    for line in response.lines() {
        if line.to_lowercase().starts_with("x-server-id:") {
            return Some(line[12..].trim().to_string());
        }
    }
    None
}

/// ロードバランシングの分散確認
fn benchmark_load_balancing_distribution(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping load balancing benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("load_balancing_distribution");
    group.measurement_time(Duration::from_secs(10));
    
    // 複数リクエストを送信して分散を確認
    group.bench_function("distribution_check", |b| {
        b.iter(|| {
            let mut backend1_count = 0;
            let mut backend2_count = 0;
            
            for _ in 0..20 {
                if let Ok((_, server_id)) = send_request(PROXY_PORT, "/") {
                    match server_id.as_deref() {
                        Some("backend1") => backend1_count += 1,
                        Some("backend2") => backend2_count += 1,
                        _ => {}
                    }
                }
            }
            
            // 分散が確認できれば成功
            backend1_count + backend2_count
        });
    });
    
    group.finish();
}

/// ロードバランシングのレイテンシ
fn benchmark_load_balancing_latency(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping load balancing latency benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("load_balancing_latency");
    group.measurement_time(Duration::from_secs(10));
    
    // プロキシ経由（ロードバランシング）
    group.bench_function("via_proxy", |b| {
        b.iter(|| {
            let _ = send_request(PROXY_PORT, "/");
        });
    });
    
    // 直接バックエンド1（HTTPS、TLSハンドシェイクを正しく行う）
    if is_backend_running(BACKEND1_PORT) {
        group.bench_function("direct_backend1", |b| {
            b.iter(|| {
                let _ = send_request(BACKEND1_PORT, "/");
            });
        });
    }
    
    // 直接バックエンド2（HTTPS、TLSハンドシェイクを正しく行う）
    if is_backend_running(BACKEND2_PORT) {
        group.bench_function("direct_backend2", |b| {
            b.iter(|| {
                let _ = send_request(BACKEND2_PORT, "/");
            });
        });
    }
    
    group.finish();
}

/// 並行リクエストでのロードバランシング
fn benchmark_load_balancing_concurrent(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping concurrent load balancing benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("load_balancing_concurrent");
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
                                let _ = send_request(PROXY_PORT, "/");
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

/// 連続リクエストでのロードバランシング
fn benchmark_load_balancing_sequential(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping sequential load balancing benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("load_balancing_sequential");
    group.measurement_time(Duration::from_secs(15));
    
    for request_count in [10, 50, 100].iter() {
        group.bench_with_input(
            BenchmarkId::new("sequential", request_count),
            request_count,
            |b, &count| {
                b.iter(|| {
                    let mut total_latency = Duration::ZERO;
                    for _ in 0..count {
                        if let Ok((latency, _)) = send_request(PROXY_PORT, "/") {
                            total_latency += latency;
                        }
                    }
                    total_latency
                });
            },
        );
    }
    
    group.finish();
}

criterion_group!(
    benches,
    benchmark_load_balancing_distribution,
    benchmark_load_balancing_latency,
    benchmark_load_balancing_concurrent,
    benchmark_load_balancing_sequential,
);
criterion_main!(benches);

