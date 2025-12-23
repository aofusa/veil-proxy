//! レイテンシベンチマーク
//!
//! HTTPリクエストのレイテンシを測定します。
//!
//! 使用方法:
//!   1. E2E環境を起動: ./tests/e2e_setup.sh start
//!   2. ベンチマーク実行: cargo bench --bench latency
//!   3. 環境停止: ./tests/e2e_setup.sh stop

use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const PROXY_PORT: u16 = 8443;
const BACKEND_PORT: u16 = 9001;

/// プロキシサーバーが起動しているか確認
fn is_proxy_running() -> bool {
    TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).is_ok()
}

/// HTTPリクエストのレイテンシを測定
fn measure_request_latency(addr: &str) -> Duration {
    let start = Instant::now();
    
    let mut stream = match TcpStream::connect(addr) {
        Ok(s) => s,
        Err(_) => return Duration::from_secs(10), // タイムアウト
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    
    let request = "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    if stream.write_all(request.as_bytes()).is_err() {
        return Duration::from_secs(10);
    }
    
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    
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
            measure_request_latency(&format!("127.0.0.1:{}", PROXY_PORT))
        });
    });
    
    // 直接バックエンド
    if TcpStream::connect(format!("127.0.0.1:{}", BACKEND_PORT)).is_ok() {
        group.bench_function("direct_backend", |b| {
            b.iter(|| {
                measure_request_latency(&format!("127.0.0.1:{}", BACKEND_PORT))
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
                        total_latency += measure_request_latency(
                            &format!("127.0.0.1:{}", PROXY_PORT)
                        );
                    }
                    total_latency
                });
            },
        );
    }
    
    group.finish();
}

/// TCP接続確立レイテンシ
fn benchmark_tcp_connect_latency(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping connect benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("tcp_connect_latency");
    group.measurement_time(Duration::from_secs(10));
    
    group.bench_function("connect_only", |b| {
        b.iter(|| {
            let start = Instant::now();
            let stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT));
            let latency = start.elapsed();
            drop(stream);
            latency
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
            measure_request_latency(&format!("127.0.0.1:{}", PROXY_PORT))
        });
    });
    
    group.finish();
}

criterion_group!(
    benches,
    benchmark_single_request_latency,
    benchmark_sequential_requests,
    benchmark_tcp_connect_latency,
    benchmark_latency_distribution,
);
criterion_main!(benches);

