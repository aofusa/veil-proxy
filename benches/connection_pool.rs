//! コネクションプールベンチマーク
//!
//! コネクションプールのパフォーマンスを測定します。
//! - プール有効/無効でのレイテンシ比較
//! - プールサイズ別のパフォーマンス
//! - アイドルタイムアウトの効果
//!
//! 使用方法:
//!   1. E2E環境を起動: ./tests/e2e_setup.sh start
//!   2. ベンチマーク実行: cargo bench --bench connection_pool
//!   3. 環境停止: ./tests/e2e_setup.sh stop

use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const PROXY_PORT: u16 = 8443;

/// プロキシサーバーが起動しているか確認
fn is_proxy_running() -> bool {
    TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).is_ok()
}

/// 新規接続でリクエストを送信（プール未使用）
fn send_request_new_connection(port: u16, path: &str) -> Result<Duration, std::io::Error> {
    let start = Instant::now();
    
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    
    let request = format!("GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n", path);
    stream.write_all(request.as_bytes())?;
    
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    
    Ok(start.elapsed())
}

/// Keep-Alive接続でリクエストを送信（プール使用をシミュレート）
fn send_request_keep_alive(port: u16, path: &str, stream: &mut Option<TcpStream>) -> Result<Duration, std::io::Error> {
    let start = Instant::now();
    
    // 接続が再利用可能か確認
    if stream.is_none() || stream.as_ref().unwrap().peer_addr().is_err() {
        *stream = Some(TcpStream::connect(format!("127.0.0.1:{}", port))?);
        stream.as_mut().unwrap().set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.as_mut().unwrap().set_write_timeout(Some(Duration::from_secs(5)))?;
    }
    
    let request = format!("GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n", path);
    stream.as_mut().unwrap().write_all(request.as_bytes())?;
    
    let mut response = Vec::new();
    stream.as_mut().unwrap().read_to_end(&mut response)?;
    
    Ok(start.elapsed())
}

/// プール有効/無効でのレイテンシ比較
fn benchmark_pool_vs_no_pool(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping connection pool benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("pool_vs_no_pool");
    group.measurement_time(Duration::from_secs(10));
    
    // プールなし（毎回新規接続）
    group.bench_function("no_pool", |b| {
        b.iter(|| {
            let _ = send_request_new_connection(PROXY_PORT, "/");
        });
    });
    
    // プールあり（Keep-Alive接続再利用）
    group.bench_function("with_pool", |b| {
        let mut stream: Option<TcpStream> = None;
        b.iter(|| {
            let _ = send_request_keep_alive(PROXY_PORT, "/", &mut stream);
        });
    });
    
    group.finish();
}

/// 連続リクエストでのプール効果
fn benchmark_pool_sequential_requests(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping sequential pool benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("pool_sequential");
    group.measurement_time(Duration::from_secs(15));
    
    for request_count in [5, 10, 20, 50].iter() {
        // プールなし
        group.bench_with_input(
            BenchmarkId::new("no_pool", request_count),
            request_count,
            |b, &count| {
                b.iter(|| {
                    let mut total = Duration::ZERO;
                    for _ in 0..count {
                        if let Ok(duration) = send_request_new_connection(PROXY_PORT, "/") {
                            total += duration;
                        }
                    }
                    total
                });
            },
        );
        
        // プールあり
        group.bench_with_input(
            BenchmarkId::new("with_pool", request_count),
            request_count,
            |b, &count| {
                b.iter(|| {
                    let mut stream: Option<TcpStream> = None;
                    let mut total = Duration::ZERO;
                    for _ in 0..count {
                        if let Ok(duration) = send_request_keep_alive(PROXY_PORT, "/", &mut stream) {
                            total += duration;
                        }
                    }
                    total
                });
            },
        );
    }
    
    group.finish();
}

/// 並行リクエストでのプール効果
fn benchmark_pool_concurrent_requests(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping concurrent pool benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("pool_concurrent");
    group.measurement_time(Duration::from_secs(10));
    
    for concurrent in [1, 4, 8, 16].iter() {
        // プールなし
        group.bench_with_input(
            BenchmarkId::new("no_pool", concurrent),
            concurrent,
            |b, &concurrent| {
                b.iter(|| {
                    let handles: Vec<_> = (0..concurrent)
                        .map(|_| {
                            std::thread::spawn(|| {
                                let _ = send_request_new_connection(PROXY_PORT, "/");
                            })
                        })
                        .collect();
                    
                    for handle in handles {
                        let _ = handle.join();
                    }
                });
            },
        );
        
        // プールあり（各スレッドで接続再利用）
        group.bench_with_input(
            BenchmarkId::new("with_pool", concurrent),
            concurrent,
            |b, &concurrent| {
                b.iter(|| {
                    let handles: Vec<_> = (0..concurrent)
                        .map(|_| {
                            std::thread::spawn(|| {
                                let mut stream: Option<TcpStream> = None;
                                for _ in 0..5 {
                                    let _ = send_request_keep_alive(PROXY_PORT, "/", &mut stream);
                                }
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
    benchmark_pool_vs_no_pool,
    benchmark_pool_sequential_requests,
    benchmark_pool_concurrent_requests,
);
criterion_main!(benches);

