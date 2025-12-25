//! キャッシュベンチマーク
//!
//! プロキシキャッシュのパフォーマンスを測定します。
//! - キャッシュヒット率とパフォーマンス
//! - メモリキャッシュ vs ディスクキャッシュ
//! - ETag/304レスポンスの効果
//!
//! 使用方法:
//!   1. E2E環境を起動: ./tests/e2e_setup.sh start
//!   2. ベンチマーク実行: cargo bench --bench cache
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

/// リクエストを送信してレスポンスを取得（TLS経由）
fn send_request(port: u16, path: &str, headers: &[(&str, &str)]) -> Result<Vec<u8>, std::io::Error> {
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
    
    let mut request = format!("GET {} HTTP/1.1\r\nHost: localhost\r\n", path);
    for (name, value) in headers {
        request.push_str(&format!("{}: {}\r\n", name, value));
    }
    request.push_str("Connection: close\r\n\r\n");
    
    tls_stream.write_all(request.as_bytes())?;
    
    let mut response = Vec::new();
    // TLS close_notify なしでの接続終了を許容
    // HTTP/1.1 では Content-Length やチャンク転送で完全性を保証するため、
    // close_notify がなくても実用上問題ない
    match tls_stream.read_to_end(&mut response) {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
            // サーバーが close_notify を送信せずに接続を閉じた場合は許容
            // レスポンスが既に受信されていれば問題なし
        }
        Err(e) => return Err(e),
    }
    
    Ok(response)
}

/// キャッシュなしでのリクエスト
fn benchmark_no_cache(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping cache benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("cache_no_cache");
    group.measurement_time(Duration::from_secs(10));
    
    group.bench_function("uncached_request", |b| {
        b.iter(|| {
            let _ = send_request(PROXY_PORT, "/", &[]);
        });
    });
    
    group.finish();
}

/// キャッシュヒット（同じリクエストを複数回）
fn benchmark_cache_hit(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping cache hit benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("cache_hit");
    group.measurement_time(Duration::from_secs(10));
    
    // 最初のリクエスト（キャッシュミス）
    group.bench_function("first_request", |b| {
        b.iter(|| {
            let _ = send_request(PROXY_PORT, "/", &[]);
        });
    });
    
    // 2回目以降のリクエスト（キャッシュヒット想定）
    group.bench_function("cached_request", |b| {
        // 事前に1回リクエストしてキャッシュを準備
        let _ = send_request(PROXY_PORT, "/", &[]);
        
        b.iter(|| {
            let _ = send_request(PROXY_PORT, "/", &[]);
        });
    });
    
    group.finish();
}

/// ETag/304レスポンスの効果
fn benchmark_etag_304(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping ETag benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("cache_etag");
    group.measurement_time(Duration::from_secs(10));
    
    // 最初のリクエスト（ETag取得）
    group.bench_function("first_request_with_etag", |b| {
        b.iter(|| {
            let _ = send_request(PROXY_PORT, "/", &[]);
        });
    });
    
    // If-None-Match付きリクエスト（304レスポンス想定）
    group.bench_function("conditional_request", |b| {
        // 事前にETagを取得
        let response = send_request(PROXY_PORT, "/", &[]).unwrap();
        let etag = extract_etag(&response);
        
        b.iter(|| {
            if let Some(etag) = &etag {
                let _ = send_request(PROXY_PORT, "/", &[("If-None-Match", etag)]);
            }
        });
    });
    
    group.finish();
}

/// レスポンスからETagを抽出
fn extract_etag(response: &[u8]) -> Option<String> {
    let response_str = String::from_utf8_lossy(response);
    for line in response_str.lines() {
        if line.to_lowercase().starts_with("etag:") {
            return Some(line[5..].trim().to_string());
        }
    }
    None
}

/// キャッシュサイズ別のパフォーマンス（シミュレート）
fn benchmark_cache_size(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping cache size benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("cache_size");
    
    // 小さいファイル
    group.bench_function("small_file", |b| {
        b.iter(|| {
            let _ = send_request(PROXY_PORT, "/", &[]);
        });
    });
    
    // 大きいファイル
    group.bench_function("large_file", |b| {
        b.iter(|| {
            let _ = send_request(PROXY_PORT, "/large.txt", &[]);
        });
    });
    
    group.finish();
}

/// 連続リクエストでのキャッシュ効果
fn benchmark_cache_sequential(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping sequential cache benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("cache_sequential");
    group.measurement_time(Duration::from_secs(15));
    
    for request_count in [10, 50, 100].iter() {
        // キャッシュなし（毎回異なるパス）
        group.bench_with_input(
            BenchmarkId::new("no_cache", request_count),
            request_count,
            |b, &count| {
                b.iter(|| {
                    for i in 0..count {
                        let path = format!("/?nocache={}", i);
                        let _ = send_request(PROXY_PORT, &path, &[]);
                    }
                });
            },
        );
        
        // キャッシュあり（同じパス）
        group.bench_with_input(
            BenchmarkId::new("with_cache", request_count),
            request_count,
            |b, &count| {
                // 事前に1回リクエストしてキャッシュを準備
                let _ = send_request(PROXY_PORT, "/", &[]);
                
                b.iter(|| {
                    for _ in 0..count {
                        let _ = send_request(PROXY_PORT, "/", &[]);
                    }
                });
            },
        );
    }
    
    group.finish();
}

criterion_group!(
    benches,
    benchmark_no_cache,
    benchmark_cache_hit,
    benchmark_etag_304,
    benchmark_cache_size,
    benchmark_cache_sequential,
);
criterion_main!(benches);

