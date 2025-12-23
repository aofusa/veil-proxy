//! スループットベンチマーク
//!
//! HTTPリクエストのスループットを測定します。
//!
//! 使用方法:
//!   1. E2E環境を起動: ./tests/e2e_setup.sh start
//!   2. ベンチマーク実行: cargo bench --bench throughput
//!   3. 環境停止: ./tests/e2e_setup.sh stop

use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId, Throughput};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const PROXY_PORT: u16 = 8443;
const BACKEND_PORT: u16 = 9001;

/// プロキシサーバーが起動しているか確認
fn is_proxy_running() -> bool {
    TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).is_ok()
}

/// バックエンドサーバーが起動しているか確認
fn is_backend_running() -> bool {
    TcpStream::connect(format!("127.0.0.1:{}", BACKEND_PORT)).is_ok()
}

/// シンプルなHTTPリクエストを送信
fn send_http_request(addr: &str) -> Result<usize, std::io::Error> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    
    // HTTPリクエスト
    let request = "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    stream.write_all(request.as_bytes())?;
    
    // レスポンス受信
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    
    Ok(response.len())
}

/// HTTPリクエストスループットベンチマーク
fn benchmark_http_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("http_throughput");
    
    // 直接バックエンドへのリクエスト
    if is_backend_running() {
        group.bench_function("direct_backend", |b| {
            b.iter(|| {
                let _ = send_http_request(&format!("127.0.0.1:{}", BACKEND_PORT));
            });
        });
    } else {
        eprintln!("Backend server not running, skipping direct_backend benchmark");
    }
    
    // プロキシ経由のリクエスト
    if is_proxy_running() {
        group.bench_function("via_proxy", |b| {
            b.iter(|| {
                let _ = send_http_request(&format!("127.0.0.1:{}", PROXY_PORT));
            });
        });
    } else {
        eprintln!("Proxy server not running, skipping via_proxy benchmark");
    }
    
    group.finish();
}

/// レスポンスサイズ別スループット
fn benchmark_response_size(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping response_size benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("response_size");
    
    // 各パスに対応するレスポンスサイズをテスト
    for (path, size) in [
        ("/", 20),           // 小さいレスポンス
        ("/large.txt", 13000), // 大きいレスポンス
    ].iter() {
        group.throughput(Throughput::Bytes(*size as u64));
        group.bench_with_input(
            BenchmarkId::new("bytes", size),
            path,
            |b, path| {
                b.iter(|| {
                    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).unwrap();
                    let request = format!(
                        "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                        path
                    );
                    stream.write_all(request.as_bytes()).unwrap();
                    
                    let mut response = Vec::new();
                    stream.read_to_end(&mut response).unwrap();
                });
            },
        );
    }
    
    group.finish();
}

/// 並行リクエスト数別スループット
fn benchmark_concurrent_requests(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping concurrent benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("concurrent_requests");
    group.measurement_time(Duration::from_secs(10));
    
    for concurrent in [1, 4, 8, 16].iter() {
        group.bench_with_input(
            BenchmarkId::new("threads", concurrent),
            concurrent,
            |b, &concurrent| {
                b.iter(|| {
                    let handles: Vec<_> = (0..concurrent)
                        .map(|_| {
                            std::thread::spawn(|| {
                                let _ = send_http_request(&format!("127.0.0.1:{}", PROXY_PORT));
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
    benchmark_http_throughput,
    benchmark_response_size,
    benchmark_concurrent_requests,
);
criterion_main!(benches);

