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
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;
use sysinfo::System;

const PROXY_PORT: u16 = 8443;

/// プロキシサーバーが起動しているか確認
fn is_proxy_running() -> bool {
    TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).is_ok()
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
                    // 接続を確立
                    let mut streams = Vec::new();
                    for _ in 0..count {
                        if let Ok(stream) = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)) {
                            streams.push(stream);
                        }
                    }
                    
                    // メモリ使用量を測定
                    let memory = get_memory_usage().unwrap_or(0);
                    let memory_delta = memory.saturating_sub(baseline);
                    
                    // 接続を閉じる
                    drop(streams);
                    
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
                    // リクエストを送信
                    for _ in 0..count {
                        if let Ok(mut stream) = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)) {
                            let request = "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
                            let _ = stream.write_all(request.as_bytes());
                            let mut response = Vec::new();
                            let _ = stream.read_to_end(&mut response);
                        }
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
                        if let Ok(mut stream) = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)) {
                            let request = format!("GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n", path);
                            let _ = stream.write_all(request.as_bytes());
                            let mut response = Vec::new();
                            let _ = stream.read_to_end(&mut response);
                        }
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
                if let Ok(mut stream) = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)) {
                    let request = "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(request.as_bytes());
                    let mut response = Vec::new();
                    let _ = stream.read_to_end(&mut response);
                }
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

