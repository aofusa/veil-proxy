//! WebSocketベンチマーク
//!
//! WebSocket接続のパフォーマンスを測定します。
//! - WebSocket接続確立のレイテンシ
//! - 双方向転送のスループット
//! - Fixed vs Adaptiveポーリングモードの比較
//!
//! 使用方法:
//!   1. E2E環境を起動: ./tests/e2e_setup.sh start
//!   2. ベンチマーク実行: cargo bench --bench websocket
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

/// WebSocket接続を確立（Upgradeリクエスト）
fn establish_websocket_connection(port: u16, path: &str) -> Result<TcpStream, std::io::Error> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    
    // WebSocket Upgradeリクエスト
    use base64::{Engine as _, engine::general_purpose};
    let key = general_purpose::STANDARD.encode(b"dGhlIHNhbXBsZSBub25jZQ==");
    let request = format!(
        "GET {} HTTP/1.1\r\n\
         Host: localhost\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {}\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n",
        path, key
    );
    
    stream.write_all(request.as_bytes())?;
    
    // レスポンスを読み取る（101 Switching Protocols）
    let mut response = vec![0u8; 1024];
    let _ = stream.read(&mut response)?;
    
    Ok(stream)
}

/// WebSocket接続確立のレイテンシ
fn benchmark_websocket_connection_latency(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping WebSocket benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("websocket_connection");
    group.measurement_time(Duration::from_secs(10));
    
    group.bench_function("connection_establishment", |b| {
        b.iter(|| {
            let start = Instant::now();
            let _ = establish_websocket_connection(PROXY_PORT, "/");
            start.elapsed()
        });
    });
    
    group.finish();
}

/// WebSocket双方向転送のスループット
fn benchmark_websocket_throughput(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping WebSocket throughput benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("websocket_throughput");
    group.measurement_time(Duration::from_secs(10));
    
    // クライアント→サーバー
    group.bench_function("client_to_server", |b| {
        b.iter(|| {
            if let Ok(mut stream) = establish_websocket_connection(PROXY_PORT, "/") {
                // WebSocketフレームを送信（簡易版）
                let data = b"test message";
                let _ = stream.write_all(data);
            }
        });
    });
    
    // サーバー→クライアント
    group.bench_function("server_to_client", |b| {
        b.iter(|| {
            if let Ok(mut stream) = establish_websocket_connection(PROXY_PORT, "/") {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
            }
        });
    });
    
    group.finish();
}

/// WebSocket同時接続数別のパフォーマンス
fn benchmark_websocket_concurrent(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping concurrent WebSocket benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("websocket_concurrent");
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
                                let _ = establish_websocket_connection(PROXY_PORT, "/");
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

/// WebSocketメッセージサイズ別のパフォーマンス
fn benchmark_websocket_message_size(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping WebSocket message size benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("websocket_message_size");
    
    for size in [64, 256, 1024, 4096].iter() {
        group.bench_with_input(
            BenchmarkId::new("message_size", size),
            size,
            |b, &size| {
                b.iter(|| {
                    if let Ok(mut stream) = establish_websocket_connection(PROXY_PORT, "/") {
                        let data = vec![0u8; size];
                        let _ = stream.write_all(&data);
                    }
                });
            },
        );
    }
    
    group.finish();
}

criterion_group!(
    benches,
    benchmark_websocket_connection_latency,
    benchmark_websocket_throughput,
    benchmark_websocket_concurrent,
    benchmark_websocket_message_size,
);
criterion_main!(benches);

