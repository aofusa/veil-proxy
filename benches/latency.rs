//! レイテンシベンチマーク
//!
//! HTTPリクエストのレイテンシを測定します。
//!
//! 使用方法:
//!   1. E2E環境を起動: ./tests/e2e_setup.sh start
//!   2. ベンチマーク実行: cargo bench --bench latency
//!   3. 環境停止: ./tests/e2e_setup.sh stop

use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};
use std::io::{Read, Write, ErrorKind};
use std::net::TcpStream;
use std::time::{Duration, Instant};
use std::sync::Arc;
use rustls::{ClientConfig, ClientConnection};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::ServerName;

const PROXY_PORT: u16 = 8443;  // HTTPSポート
const BACKEND_PORT: u16 = 9001;

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
fn is_backend_running() -> bool {
    init_crypto_provider();
    
    let mut stream = match TcpStream::connect(format!("127.0.0.1:{}", BACKEND_PORT)) {
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

/// HTTPSリクエストのレイテンシを測定
fn measure_request_latency(addr: &str, port: u16) -> Duration {
    let start = Instant::now();
    
    init_crypto_provider();
    
    let mut stream = match TcpStream::connect(format!("127.0.0.1:{}", port)) {
        Ok(s) => s,
        Err(_) => return Duration::from_secs(10),
    };
    
    if stream.set_read_timeout(Some(Duration::from_secs(5))).is_err() {
        return Duration::from_secs(10);
    }
    if stream.set_write_timeout(Some(Duration::from_secs(5))).is_err() {
        return Duration::from_secs(10);
    }
    
    let config = create_tls_config();
    let server_name = match ServerName::try_from("localhost".to_string()) {
        Ok(name) => name,
        Err(_) => return Duration::from_secs(10),
    };
    
    let mut tls_conn = match ClientConnection::new(config, server_name) {
        Ok(conn) => conn,
        Err(_) => return Duration::from_secs(10),
    };
    
    // TLSハンドシェイク
    while tls_conn.is_handshaking() {
        match tls_conn.complete_io(&mut stream) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(_) => return Duration::from_secs(10),
        }
    }
    
    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);
    
    let request = "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    if tls_stream.write_all(request.as_bytes()).is_err() {
        return Duration::from_secs(10);
    }
    
    let mut response = Vec::new();
    let _ = tls_stream.read_to_end(&mut response);
    
    start.elapsed()
}

/// 単一リクエストレイテンシベンチマーク
fn benchmark_single_request_latency(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping latency benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("single_request_latency");
    group.measurement_time(Duration::from_secs(10));
    
    // プロキシ経由
    group.bench_function("via_proxy", |b| {
        b.iter(|| {
            measure_request_latency("127.0.0.1", PROXY_PORT)
        });
    });
    
    // 直接バックエンド
    if is_backend_running() {
        group.bench_function("direct_backend", |b| {
            b.iter(|| {
                measure_request_latency("127.0.0.1", BACKEND_PORT)
            });
        });
    }
    
    group.finish();
}

/// 連続リクエストレイテンシ（Keep-Alive想定なしの新規接続）
fn benchmark_sequential_requests(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping sequential benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("sequential_requests");
    group.measurement_time(Duration::from_secs(15));
    
    for request_count in [10, 50, 100].iter() {
        group.bench_with_input(
            BenchmarkId::new("count", request_count),
            request_count,
            |b, &count| {
                b.iter(|| {
                    let mut total_latency = Duration::ZERO;
                    for _ in 0..count {
                        total_latency += measure_request_latency("127.0.0.1", PROXY_PORT);
                    }
                    total_latency
                });
            },
        );
    }
    
    group.finish();
}

/// TLS接続確立レイテンシ
fn benchmark_tls_connect_latency(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping connect benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("tls_connect_latency");
    group.measurement_time(Duration::from_secs(10));
    
    group.bench_function("tls_handshake", |b| {
        init_crypto_provider();
        b.iter(|| {
            let start = Instant::now();
            let mut stream = match TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)) {
                Ok(s) => s,
                Err(_) => return Duration::from_secs(10),
            };
            
            if stream.set_read_timeout(Some(Duration::from_secs(2))).is_err() {
                return Duration::from_secs(10);
            }
            if stream.set_write_timeout(Some(Duration::from_secs(2))).is_err() {
                return Duration::from_secs(10);
            }
            
            let config = create_tls_config();
            let server_name = match ServerName::try_from("localhost".to_string()) {
                Ok(name) => name,
                Err(_) => return Duration::from_secs(10),
            };
            
            let mut tls_conn = match ClientConnection::new(config, server_name) {
                Ok(conn) => conn,
                Err(_) => return Duration::from_secs(10),
            };
            
            // TLSハンドシェイクを完了
            while tls_conn.is_handshaking() {
                match tls_conn.complete_io(&mut stream) {
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(_) => return Duration::from_secs(10),
                }
            }
            
            start.elapsed()
        });
    });
    
    group.finish();
}

/// レイテンシ分布の測定（パーセンタイル算出用）
fn benchmark_latency_distribution(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping distribution benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("latency_distribution");
    group.sample_size(1000); // より多くのサンプルでパーセンタイルを正確に
    group.measurement_time(Duration::from_secs(30));
    
    group.bench_function("p50_p99_p999", |b| {
        b.iter(|| {
            measure_request_latency("127.0.0.1", PROXY_PORT)
        });
    });
    
    group.finish();
}

criterion_group!(
    benches,
    benchmark_single_request_latency,
    benchmark_sequential_requests,
    benchmark_tls_connect_latency,
    benchmark_latency_distribution,
);
criterion_main!(benches);

