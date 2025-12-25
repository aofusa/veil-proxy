//! メモリ使用量ベンチマーク
//!
//! メモリ使用量と効率を測定します。
//! - 接続数別のメモリ使用量
//! - キャッシュサイズ別のメモリ使用量
//! - バッファプールの効果
//!
//! 使用方法:
//!   1. E2E環境を起動: ./tests/e2e_setup.sh start
//!   2. ベンチマーク実行: cargo bench --bench memory
//!   3. 環境停止: ./tests/e2e_setup.sh stop
//!
//! 注意: メモリ使用量の測定にはsysinfoクレートを使用します。

use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};
use std::io::{Read, Write, ErrorKind};
use std::net::TcpStream;
use std::time::Duration;
use std::sync::Arc;
use sysinfo::System;
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

/// HTTPSリクエストを送信（TLSハンドシェイクを正しく行う）
fn send_https_request(port: u16, path: &str) -> Result<(), Box<dyn std::error::Error>> {
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
            Err(e) => return Err(Box::new(e)),
        }
    }
    
    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);
    
    let request = format!("GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n", path);
    tls_stream.write_all(request.as_bytes())?;
    
    let mut response = Vec::new();
    let _ = tls_stream.read_to_end(&mut response);
    
    Ok(())
}

/// プロセスのメモリ使用量を取得
fn get_memory_usage() -> Option<u64> {
    let mut system = System::new();
    system.refresh_all();
    
    // veilプロセスを検索
    for (_pid, process) in system.processes() {
        if process.name().to_string_lossy().contains("veil") {
            return Some(process.memory() * 1024); // KB to bytes
        }
    }
    None
}

/// 接続数別のメモリ使用量
fn benchmark_memory_by_connections(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping memory benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("memory_connections");
    
    // ベースライン（接続なし）
    let baseline = get_memory_usage().unwrap_or(0);
    
    for connection_count in [10, 50, 100, 200].iter() {
        group.bench_with_input(
            BenchmarkId::new("connections", connection_count),
            connection_count,
            |b, &count| {
                b.iter(|| {
                    // TLS接続を確立（ハンドシェイクまで）
                    let mut connections = Vec::new();
                    for _ in 0..count {
                        init_crypto_provider();
                        if let Ok(mut stream) = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)) {
                            if stream.set_read_timeout(Some(Duration::from_secs(2))).is_ok() 
                                && stream.set_write_timeout(Some(Duration::from_secs(2))).is_ok() {
                                let config = create_tls_config();
                                if let Ok(server_name) = ServerName::try_from("localhost".to_string()) {
                                    if let Ok(mut tls_conn) = ClientConnection::new(config, server_name) {
                                        // ハンドシェイクを開始（完了まで待たない）
                                        let mut handshake_started = false;
                                        for _ in 0..5 {
                                            if !tls_conn.is_handshaking() {
                                                handshake_started = true;
                                                break;
                                            }
                                            match tls_conn.complete_io(&mut stream) {
                                                Ok(_) => {
                                                    handshake_started = true;
                                                    if !tls_conn.is_handshaking() {
                                                        break;
                                                    }
                                                }
                                                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                                                    std::thread::sleep(Duration::from_millis(5));
                                                    continue;
                                                }
                                                Err(_) => break,
                                            }
                                        }
                                        if handshake_started {
                                            connections.push((tls_conn, stream));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    
                    // メモリ使用量を測定
                    let memory = get_memory_usage().unwrap_or(0);
                    let memory_delta = memory.saturating_sub(baseline);
                    
                    // 接続を閉じる
                    drop(connections);
                    
                    memory_delta
                });
            },
        );
    }
    
    group.finish();
}

/// リクエスト数別のメモリ使用量
fn benchmark_memory_by_requests(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping memory request benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("memory_requests");
    
    let baseline = get_memory_usage().unwrap_or(0);
    
    for request_count in [100, 500, 1000, 5000].iter() {
        group.bench_with_input(
            BenchmarkId::new("requests", request_count),
            request_count,
            |b, &count| {
                b.iter(|| {
                    // HTTPSリクエストを送信
                    for _ in 0..count {
                        let _ = send_https_request(PROXY_PORT, "/");
                    }
                    
                    // メモリ使用量を測定
                    let memory = get_memory_usage().unwrap_or(0);
                    memory.saturating_sub(baseline)
                });
            },
        );
    }
    
    group.finish();
}

/// キャッシュサイズ別のメモリ使用量（シミュレート）
fn benchmark_memory_by_cache_size(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping memory cache benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("memory_cache");
    
    let baseline = get_memory_usage().unwrap_or(0);
    
    // 異なるサイズのファイルをリクエストしてキャッシュを生成
    for file_size in ["small", "large"].iter() {
        let path = if *file_size == "small" { "/" } else { "/large.txt" };
        
        group.bench_with_input(
            BenchmarkId::new("cache_size", file_size),
            path,
            |b, path| {
                b.iter(|| {
                    // 複数回リクエストしてキャッシュを生成
                    for _ in 0..100 {
                        let _ = send_https_request(PROXY_PORT, path);
                    }
                    
                    // メモリ使用量を測定
                    let memory = get_memory_usage().unwrap_or(0);
                    memory.saturating_sub(baseline)
                });
            },
        );
    }
    
    group.finish();
}

/// 長時間実行でのメモリリーク検出
fn benchmark_memory_leak_detection(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping memory leak benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("memory_leak");
    group.measurement_time(Duration::from_secs(30));
    
    group.bench_function("long_running", |b| {
        b.iter(|| {
            let initial_memory = get_memory_usage().unwrap_or(0);
            
            // 長時間リクエストを送信
            for _ in 0..1000 {
                let _ = send_https_request(PROXY_PORT, "/");
            }
            
            let final_memory = get_memory_usage().unwrap_or(0);
            final_memory.saturating_sub(initial_memory)
        });
    });
    
    group.finish();
}

criterion_group!(
    benches,
    benchmark_memory_by_connections,
    benchmark_memory_by_requests,
    benchmark_memory_by_cache_size,
    benchmark_memory_leak_detection,
);
criterion_main!(benches);

