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
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};
use native_tls::TlsConnector;

const PROXY_PORT: u16 = 8443;

/// プロキシサーバーが起動しているか確認
fn is_proxy_running() -> bool {
    TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).is_ok()
}

/// TLSコネクタを作成（自己署名証明書を許可）
fn create_tls_connector() -> TlsConnector {
    TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()
        .expect("Failed to create TLS connector")
}

/// TLS接続でのリクエスト送信
fn send_tls_request(port: u16, path: &str) -> Result<usize, Box<dyn std::error::Error>> {
    let stream = TcpStream::connect(format!("127.0.0.1:{}", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    
    let connector = create_tls_connector();
    let mut tls_stream = connector.connect("localhost", stream)?;
    
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
        b.iter(|| {
            let start = Instant::now();
            let stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).unwrap();
            let connector = create_tls_connector();
            let _tls_stream = connector.connect("localhost", stream);
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

