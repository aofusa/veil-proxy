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
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const PROXY_PORT: u16 = 8443;

/// プロキシサーバーが起動しているか確認
fn is_proxy_running() -> bool {
    TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).is_ok()
}

/// リクエストを送信してレスポンスを取得
fn send_request(port: u16, path: &str, headers: &[(&str, &str)]) -> Result<Vec<u8>, std::io::Error> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    
    let mut request = format!("GET {} HTTP/1.1\r\nHost: localhost\r\n", path);
    for (name, value) in headers {
        request.push_str(&format!("{}: {}\r\n", name, value));
    }
    request.push_str("Connection: close\r\n\r\n");
    
    stream.write_all(request.as_bytes())?;
    
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    
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

