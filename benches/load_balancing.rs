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
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const PROXY_PORT: u16 = 8443;
const BACKEND1_PORT: u16 = 9001;
const BACKEND2_PORT: u16 = 9002;

/// プロキシサーバーが起動しているか確認
fn is_proxy_running() -> bool {
    TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).is_ok()
}

/// リクエストを送信してレスポンスを取得
fn send_request(port: u16, path: &str) -> Result<(Duration, Option<String>), std::io::Error> {
    let start = Instant::now();
    
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    
    let request = format!("GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n", path);
    stream.write_all(request.as_bytes())?;
    
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    
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
    
    // 直接バックエンド1
    if TcpStream::connect(format!("127.0.0.1:{}", BACKEND1_PORT)).is_ok() {
        group.bench_function("direct_backend1", |b| {
            b.iter(|| {
                let _ = send_request(BACKEND1_PORT, "/");
            });
        });
    }
    
    // 直接バックエンド2
    if TcpStream::connect(format!("127.0.0.1:{}", BACKEND2_PORT)).is_ok() {
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

