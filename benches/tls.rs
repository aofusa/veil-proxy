//! TLS/kTLSベンチマーク
//!
//! TLS処理のパフォーマンスを測定します。
//! - TLS処理のオーバーヘッド
//! - kTLS有効/無効でのCPU使用率比較（シミュレート）
//! - kTLS有効/無効でのスループット比較
//! - TLSハンドシェイクのレイテンシ
//!
//! 使用方法:
//!   1. E2E環境を起動: ./tests/e2e_setup.sh start
//!   2. ベンチマーク実行: cargo bench --bench tls
//!   3. 環境停止: ./tests/e2e_setup.sh stop
//!
//! 注意: 実際のkTLSベンチマークには、kTLS有効/無効の両方のビルドが必要です。

use criterion::{criterion_group, criterion_main, Criterion};
use std::io::{Read, Write, ErrorKind};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};
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
    use std::io::ErrorKind;
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
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();
    
    Arc::new(config)
}

/// TLS接続でのリクエスト送信
fn send_tls_request(port: u16, path: &str) -> Result<usize, Box<dyn std::error::Error>> {
    init_crypto_provider();
    
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    
    let config = create_tls_config();
    
    // SNI名を決定
    let server_name = ServerName::try_from("localhost".to_string())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    
    // TLS接続を確立
    let mut tls_conn = ClientConnection::new(config, server_name)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    
    // ハンドシェイクを実行（同期）
    while tls_conn.is_handshaking() {
        match tls_conn.complete_io(&mut stream) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                // 非ブロッキングI/Oの場合は短い待機
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(e) => return Err(Box::new(e)),
        }
    }
    
    // rustls::Streamを使用して読み書き
    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);
    
    let request = format!("GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n", path);
    tls_stream.write_all(request.as_bytes())?;
    
    let mut response = Vec::new();
    tls_stream.read_to_end(&mut response)?;
    
    Ok(response.len())
}

/// TLSハンドシェイクのレイテンシ測定
fn benchmark_tls_handshake_latency(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping TLS benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("tls_handshake_latency");
    group.measurement_time(Duration::from_secs(10));
    
    group.bench_function("handshake", |b| {
        init_crypto_provider();
        b.iter(|| {
            let start = Instant::now();
            let mut stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).unwrap();
            let config = create_tls_config();
            let server_name = ServerName::try_from("localhost".to_string()).unwrap();
            let mut tls_conn = ClientConnection::new(config, server_name).unwrap();
            
            // ハンドシェイクを完了
            while tls_conn.is_handshaking() {
                if let Err(e) = tls_conn.complete_io(&mut stream) {
                    if e.kind() != ErrorKind::WouldBlock {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            
            start.elapsed()
        });
    });
    
    group.finish();
}

/// TLS接続でのスループット
fn benchmark_tls_throughput(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping TLS throughput benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("tls_throughput");
    group.measurement_time(Duration::from_secs(10));
    
    // TLS接続でのリクエスト
    group.bench_function("tls_request", |b| {
        b.iter(|| {
            let _ = send_tls_request(PROXY_PORT, "/");
        });
    });
    
    // 複数リクエスト（接続再利用なし）
    group.bench_function("tls_multiple_requests", |b| {
        b.iter(|| {
            for _ in 0..10 {
                let _ = send_tls_request(PROXY_PORT, "/");
            }
        });
    });
    
    group.finish();
}

/// TLS接続でのレイテンシ
fn benchmark_tls_latency(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping TLS latency benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("tls_latency");
    group.measurement_time(Duration::from_secs(10));
    
    group.bench_function("tls_request_latency", |b| {
        b.iter(|| {
            let start = Instant::now();
            let _ = send_tls_request(PROXY_PORT, "/");
            start.elapsed()
        });
    });
    
    group.finish();
}

/// kTLS効果のシミュレーション
/// 注意: 実際のkTLSベンチマークには、kTLS有効/無効の両方のビルドと実行が必要
fn benchmark_ktls_simulation(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping kTLS simulation benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("ktls_simulation");
    group.measurement_time(Duration::from_secs(10));
    
    // 通常のTLS（rustls使用、kTLS無効と想定）
    group.bench_function("rustls_only", |b| {
        b.iter(|| {
            let _ = send_tls_request(PROXY_PORT, "/");
        });
    });
    
    // 注意: 実際のkTLSベンチマークには、kTLS有効ビルドでの実行が必要
    // ここでは同じ測定を実行（実際の比較には別ビルドが必要）
    group.bench_function("ktls_enabled", |b| {
        b.iter(|| {
            let _ = send_tls_request(PROXY_PORT, "/");
        });
    });
    
    group.finish();
}

criterion_group!(
    benches,
    benchmark_tls_handshake_latency,
    benchmark_tls_throughput,
    benchmark_tls_latency,
    benchmark_ktls_simulation,
);
criterion_main!(benches);

