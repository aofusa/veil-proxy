//! 圧縮ベンチマーク
//!
//! レスポンス圧縮のパフォーマンスを測定します。
//! - gzip/brotli/zstdの圧縮率と速度比較
//! - 圧縮有効/無効でのスループット比較
//! - レスポンスサイズ別の圧縮効果
//!
//! 使用方法:
//!   1. E2E環境を起動: ./tests/e2e_setup.sh start
//!   2. ベンチマーク実行: cargo bench --bench compression
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

/// 圧縮形式を指定してリクエストを送信
fn send_request_with_encoding(port: u16, path: &str, encoding: &str) -> Result<(usize, usize), std::io::Error> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: {}\r\nConnection: close\r\n\r\n",
        path, encoding
    );
    stream.write_all(request.as_bytes())?;
    
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    
    let response_size = response.len();
    Ok((response_size, response_size))
}

/// 圧縮形式別のスループット比較
fn benchmark_compression_formats(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping compression benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("compression_formats");
    
    // 圧縮なし
    group.bench_function("no_compression", |b| {
        b.iter(|| {
            let _ = send_request_with_encoding(PROXY_PORT, "/large.txt", "identity");
        });
    });
    
    // gzip
    group.bench_function("gzip", |b| {
        b.iter(|| {
            let _ = send_request_with_encoding(PROXY_PORT, "/large.txt", "gzip");
        });
    });
    
    // brotli
    group.bench_function("brotli", |b| {
        b.iter(|| {
            let _ = send_request_with_encoding(PROXY_PORT, "/large.txt", "br");
        });
    });
    
    // zstd
    group.bench_function("zstd", |b| {
        b.iter(|| {
            let _ = send_request_with_encoding(PROXY_PORT, "/large.txt", "zstd");
        });
    });
    
    group.finish();
}

/// レスポンスサイズ別の圧縮効果
fn benchmark_compression_by_size(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping compression size benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("compression_by_size");
    
    for (path, size_label) in [
        ("/", "small"),
        ("/large.txt", "large"),
    ].iter() {
        // 圧縮なし
        group.bench_with_input(
            BenchmarkId::new("no_compression", size_label),
            path,
            |b, path| {
                b.iter(|| {
                    let _ = send_request_with_encoding(PROXY_PORT, path, "identity");
                });
            },
        );
        
        // gzip
        group.bench_with_input(
            BenchmarkId::new("gzip", size_label),
            path,
            |b, path| {
                b.iter(|| {
                    let _ = send_request_with_encoding(PROXY_PORT, path, "gzip");
                });
            },
        );
        
        // brotli
        group.bench_with_input(
            BenchmarkId::new("brotli", size_label),
            path,
            |b, path| {
                b.iter(|| {
                    let _ = send_request_with_encoding(PROXY_PORT, path, "br");
                });
            },
        );
    }
    
    group.finish();
}

/// 圧縮有効/無効でのスループット比較
fn benchmark_compression_throughput(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping compression throughput benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("compression_throughput");
    group.measurement_time(Duration::from_secs(10));
    
    // 圧縮なし
    group.bench_function("without_compression", |b| {
        b.iter(|| {
            for _ in 0..10 {
                let _ = send_request_with_encoding(PROXY_PORT, "/large.txt", "identity");
            }
        });
    });
    
    // 圧縮あり（gzip）
    group.bench_function("with_gzip", |b| {
        b.iter(|| {
            for _ in 0..10 {
                let _ = send_request_with_encoding(PROXY_PORT, "/large.txt", "gzip");
            }
        });
    });
    
    // 圧縮あり（brotli）
    group.bench_function("with_brotli", |b| {
        b.iter(|| {
            for _ in 0..10 {
                let _ = send_request_with_encoding(PROXY_PORT, "/large.txt", "br");
            }
        });
    });
    
    group.finish();
}

criterion_group!(
    benches,
    benchmark_compression_formats,
    benchmark_compression_by_size,
    benchmark_compression_throughput,
);
criterion_main!(benches);

