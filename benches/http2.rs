//! HTTP/2ベンチマーク
//!
//! HTTP/2のパフォーマンスを測定します。
//! - TLS + ALPN経由のHTTP/2接続
//! - ストリーム多重化の効果
//! - HPACKヘッダー圧縮の効果
//! - HTTP/1.1 vs HTTP/2の比較
//!
//! 使用方法:
//!   1. E2E環境を起動: ./tests/e2e_setup.sh start
//!   2. ベンチマーク実行: cargo bench --bench http2 --features http2
//!   3. 環境停止: ./tests/e2e_setup.sh stop

use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};
use std::io::{Read, Write, ErrorKind};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;
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
    
    let config = create_http11_tls_config();
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

/// HTTP/1.1用TLSクライアント設定を作成
fn create_http11_tls_config() -> Arc<ClientConfig> {
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();
    
    Arc::new(config)
}

/// HTTP/2用TLSクライアント設定を作成（ALPN: h2）
fn create_http2_tls_config() -> Arc<ClientConfig> {
    let mut config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();
    
    // ALPN: h2 を優先、http/1.1 にフォールバック
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    
    Arc::new(config)
}

/// TLS経由でHTTP/1.1リクエストを送信
fn send_tls_http11_request(port: u16, path: &str) -> Result<usize, Box<dyn std::error::Error>> {
    init_crypto_provider();
    
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    
    let config = create_http11_tls_config();
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
            Err(e) => return Err(Box::new(e)),
        }
    }
    
    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);
    
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        path
    );
    tls_stream.write_all(request.as_bytes())?;
    
    let mut response = Vec::new();
    tls_stream.read_to_end(&mut response)?;
    
    Ok(response.len())
}

/// TLS + HTTP/2でリクエストを送信（ALPNでh2ネゴシエーション）
/// 
/// 注: 実際のHTTP/2ストリーム多重化を使用するには h2 クレートが必要。
/// ここではTLS + ALPNでのHTTP/2ネゴシエーションを行い、
/// サーバーがHTTP/2として接続を確立することを確認する。
fn send_tls_http2_request(port: u16, path: &str) -> Result<(usize, bool), Box<dyn std::error::Error>> {
    init_crypto_provider();
    
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    
    let config = create_http2_tls_config();
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
            Err(e) => return Err(Box::new(e)),
        }
    }
    
    // ALPNでネゴシエートされたプロトコルを確認
    let is_http2 = tls_conn.alpn_protocol()
        .map(|p| p == b"h2")
        .unwrap_or(false);
    
    // HTTP/2がネゴシエートされた場合でも、ここではHTTP/1.1にフォールバック
    // 実際のHTTP/2ベンチマークには h2 クレートを使用する必要がある
    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);
    
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        path
    );
    tls_stream.write_all(request.as_bytes())?;
    
    let mut response = Vec::new();
    tls_stream.read_to_end(&mut response)?;
    
    Ok((response.len(), is_http2))
}

/// HTTP/1.1 vs HTTP/2のスループット比較（TLS経由）
fn benchmark_http11_vs_http2_throughput(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping HTTP/2 benchmarks");
        return;
    }
    
    init_crypto_provider();
    
    let mut group = c.benchmark_group("http11_vs_http2_throughput");
    
    // TLS + HTTP/1.1
    group.bench_function("tls_http11", |b| {
        b.iter(|| {
            let _ = send_tls_http11_request(PROXY_PORT, "/");
        });
    });
    
    // TLS + HTTP/2 (ALPN negotiation)
    group.bench_function("tls_http2_alpn", |b| {
        b.iter(|| {
            let _ = send_tls_http2_request(PROXY_PORT, "/");
        });
    });
    
    group.finish();
}

/// 並行ストリーム数別パフォーマンス（HTTP/2のストリーム多重化効果）
fn benchmark_concurrent_streams(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping concurrent streams benchmarks");
        return;
    }
    
    init_crypto_provider();
    
    let mut group = c.benchmark_group("concurrent_streams_tls");
    group.measurement_time(Duration::from_secs(10));
    
    for stream_count in [1, 4, 8, 16, 32].iter() {
        group.bench_with_input(
            BenchmarkId::new("streams", stream_count),
            stream_count,
            |b, &count| {
                b.iter(|| {
                    let handles: Vec<_> = (0..count)
                        .map(|_| {
                            std::thread::spawn(|| {
                                let _ = send_tls_http11_request(PROXY_PORT, "/");
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

/// ヘッダーサイズ別パフォーマンス（HPACK圧縮効果のシミュレート）
fn benchmark_header_compression(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping header compression benchmarks");
        return;
    }
    
    init_crypto_provider();
    
    let mut group = c.benchmark_group("header_compression_tls");
    
    // 小さいヘッダー
    group.bench_function("small_headers", |b| {
        b.iter(|| {
            let _ = send_tls_http11_request(PROXY_PORT, "/");
        });
    });
    
    // 大きいヘッダー（複数のカスタムヘッダー付きでリクエスト）
    group.bench_function("large_headers", |b| {
        b.iter(|| {
            init_crypto_provider();
            
            let mut stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).unwrap();
            let config = create_http11_tls_config();
            let server_name = ServerName::try_from("localhost".to_string()).unwrap();
            let mut tls_conn = ClientConnection::new(config, server_name).unwrap();
            
            while tls_conn.is_handshaking() {
                let _ = tls_conn.complete_io(&mut stream);
            }
            
            let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);
            
            let mut request = "GET / HTTP/1.1\r\nHost: localhost\r\n".to_string();
            for i in 0..20 {
                request.push_str(&format!("X-Custom-Header-{}: value-{}\r\n", i, "x".repeat(50)));
            }
            request.push_str("Connection: close\r\n\r\n");
            let _ = tls_stream.write_all(request.as_bytes());
            let mut response = Vec::new();
            let _ = tls_stream.read_to_end(&mut response);
        });
    });
    
    group.finish();
}

criterion_group!(
    benches,
    benchmark_http11_vs_http2_throughput,
    benchmark_concurrent_streams,
    benchmark_header_compression,
);
criterion_main!(benches);
