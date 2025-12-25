//! 圧縮ベンチマーク
//!
//! レスポンス圧縮のパフォーマンスを測定します。
//! - gzip/brotli/zstdの圧縮率と速度比較
//! - 圧縮有効/無効でのスループット比較
//! - レスポンスサイズ別の圧縮効果
//!
//! 使用方法:
//!   1. E2E環境を起動: ./tests/e2e_setup.sh start
//!   2. ベンチマーク実行: cargo bench --bench compression
//!   3. 環境停止: ./tests/e2e_setup.sh stop

use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};
use std::io::{Read, Write, ErrorKind};
use std::net::TcpStream;
use std::time::Duration;
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

/// 圧縮形式を指定してリクエストを送信（TLS経由）
fn send_request_with_encoding(port: u16, path: &str, encoding: &str) -> Result<(usize, usize), std::io::Error> {
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
    
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: {}\r\nConnection: close\r\n\r\n",
        path, encoding
    );
    tls_stream.write_all(request.as_bytes())?;
    
    let mut response = Vec::new();
    tls_stream.read_to_end(&mut response)?;
    
    let response_size = response.len();
    Ok((response_size, response_size))
}

/// 圧縮形式別のスループット比較
fn benchmark_compression_formats(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping compression benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("compression_formats");
    
    // 圧縮なし
    group.bench_function("no_compression", |b| {
        b.iter(|| {
            let _ = send_request_with_encoding(PROXY_PORT, "/large.txt", "identity");
        });
    });
    
    // gzip
    group.bench_function("gzip", |b| {
        b.iter(|| {
            let _ = send_request_with_encoding(PROXY_PORT, "/large.txt", "gzip");
        });
    });
    
    // brotli
    group.bench_function("brotli", |b| {
        b.iter(|| {
            let _ = send_request_with_encoding(PROXY_PORT, "/large.txt", "br");
        });
    });
    
    // zstd
    group.bench_function("zstd", |b| {
        b.iter(|| {
            let _ = send_request_with_encoding(PROXY_PORT, "/large.txt", "zstd");
        });
    });
    
    group.finish();
}

/// レスポンスサイズ別の圧縮効果
fn benchmark_compression_by_size(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping compression size benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("compression_by_size");
    
    for (path, size_label) in [
        ("/", "small"),
        ("/large.txt", "large"),
    ].iter() {
        // 圧縮なし
        group.bench_with_input(
            BenchmarkId::new("no_compression", size_label),
            path,
            |b, path| {
                b.iter(|| {
                    let _ = send_request_with_encoding(PROXY_PORT, path, "identity");
                });
            },
        );
        
        // gzip
        group.bench_with_input(
            BenchmarkId::new("gzip", size_label),
            path,
            |b, path| {
                b.iter(|| {
                    let _ = send_request_with_encoding(PROXY_PORT, path, "gzip");
                });
            },
        );
        
        // brotli
        group.bench_with_input(
            BenchmarkId::new("brotli", size_label),
            path,
            |b, path| {
                b.iter(|| {
                    let _ = send_request_with_encoding(PROXY_PORT, path, "br");
                });
            },
        );
    }
    
    group.finish();
}

/// 圧縮有効/無効でのスループット比較
fn benchmark_compression_throughput(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping compression throughput benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("compression_throughput");
    group.measurement_time(Duration::from_secs(10));
    
    // 圧縮なし
    group.bench_function("without_compression", |b| {
        b.iter(|| {
            for _ in 0..10 {
                let _ = send_request_with_encoding(PROXY_PORT, "/large.txt", "identity");
            }
        });
    });
    
    // 圧縮あり（gzip）
    group.bench_function("with_gzip", |b| {
        b.iter(|| {
            for _ in 0..10 {
                let _ = send_request_with_encoding(PROXY_PORT, "/large.txt", "gzip");
            }
        });
    });
    
    // 圧縮あり（brotli）
    group.bench_function("with_brotli", |b| {
        b.iter(|| {
            for _ in 0..10 {
                let _ = send_request_with_encoding(PROXY_PORT, "/large.txt", "br");
            }
        });
    });
    
    group.finish();
}

criterion_group!(
    benches,
    benchmark_compression_formats,
    benchmark_compression_by_size,
    benchmark_compression_throughput,
);
criterion_main!(benches);

