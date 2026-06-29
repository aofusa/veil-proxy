//! Proxy-Wasm フィルタ オーバーヘッドベンチマーク（F-08）
//!
//! WASM フィルタ適用ルート（`/wasm/*`、header_filter モジュール）と
//! 非適用ルート（`/`）のレイテンシを比較し、**1 リクエストあたりの WASM
//! フィルタオーバーヘッド**を定量化する。回帰検知のための基準値を提供する。
//!
//! 他のベンチ（latency / http2 / tls など）と同様、稼働中のプロキシに対する
//! ブラックボックス計測。veil はバイナリクレート（lib なし）のため、内部 API を
//! 直接叩くマイクロベンチではなく E2E 経路で測定する。
//!
//! # 使用方法
//! ```bash
//! # WASM フィルタ付きで E2E 環境を起動（proxy は --features wasm でビルドされる）
//! ./tests/e2e_setup.sh start
//! # ベンチ実行
//! cargo bench --bench wasm --features wasm
//! # 環境停止
//! ./tests/e2e_setup.sh stop
//! ```
//!
//! プロキシ未起動、または `/wasm/*` ルート（WASM 設定）が無い場合はスキップする。
//!
//! # 計測シナリオ
//! - `no_wasm`        : `GET /`（WASM 非適用） … ベースライン
//! - `header_filter`  : `GET /wasm/bench`（header_filter 適用）
//!
//! それぞれ「新規接続あたり」と「Keep-Alive で接続コストを償却した 1 リクエスト
//! あたり」を測る。後者の差分が WASM フィルタ純粋オーバーヘッドの近似となる。
//!
//! # 期待オーダー（参考・環境依存）
//! WASM 実行は CPU バウンドでネットワーク I/O より桁違いに小さい。Pooling Allocator +
//! `InstancePre` によりインスタンス化はウォーム状態。ヘッダフィルタ 1 本で 1 リクエスト
//! あたり概ね数 µs〜数十 µs 程度（マシン・wasmtime 版・モジュール依存）。RSS は別途
//! `/usr/bin/time -v` 等で計測すること。

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection};
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

const PROXY_PORT: u16 = 8443; // HTTPS ポート

/// WASM 適用パス（e2e_setup の `/wasm/*` ルート、header_filter 適用）
const WASM_PATH: &str = "/wasm/bench";
/// 非 WASM ベースラインパス
const BASELINE_PATH: &str = "/";

/// rustls の CryptoProvider を初期化（一度だけ実行）
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

/// TLS クライアント設定を作成（自己署名証明書を許可）
fn create_tls_config() -> Arc<ClientConfig> {
    init_crypto_provider();

    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();

    Arc::new(config)
}

/// TLS 接続を確立してハンドシェイクまで完了させる
fn connect_tls() -> Option<(TcpStream, ClientConnection)> {
    let stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok()?;

    let config = create_tls_config();
    let server_name = ServerName::try_from("localhost".to_string()).ok()?;
    let mut tls_conn = ClientConnection::new(config, server_name).ok()?;

    let mut stream = stream;
    while tls_conn.is_handshaking() {
        match tls_conn.complete_io(&mut stream) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
    Some((stream, tls_conn))
}

/// 1 接続 1 リクエスト（Connection: close）のレイテンシを測定
fn measure_request_latency(path: &str) -> Duration {
    let start = Instant::now();
    let Some((mut stream, mut tls_conn)) = connect_tls() else {
        return Duration::from_secs(10);
    };
    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);

    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        path
    );
    if tls_stream.write_all(request.as_bytes()).is_err() {
        return Duration::from_secs(10);
    }

    let mut response = Vec::new();
    let _ = tls_stream.read_to_end(&mut response);

    start.elapsed()
}

/// Keep-Alive で 1 接続を使い回し count 回リクエストして総時間を返す。
/// TLS ハンドシェイク/接続確立コストを償却し、WASM フィルタ純オーバーヘッドを際立たせる。
fn measure_keepalive(path: &str, count: usize) -> Duration {
    let Some((mut stream, mut tls_conn)) = connect_tls() else {
        return Duration::from_secs(10);
    };
    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);

    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n",
        path
    );

    let start = Instant::now();
    let mut buf = [0u8; 16384];
    for _ in 0..count {
        if tls_stream.write_all(request.as_bytes()).is_err() {
            return Duration::from_secs(10);
        }
        // ヘッダ終端（\r\n\r\n）まで読み、Content-Length 分を読み切る簡易リーダ。
        if read_one_response(&mut tls_stream, &mut buf).is_none() {
            return Duration::from_secs(10);
        }
    }
    start.elapsed()
}

/// Keep-Alive 接続から HTTP レスポンス 1 つを読み切る（ヘッダ + Content-Length ボディ）。
/// 成功時に Some(())、失敗時に None。chunked は簡易対応（読めた分でフレーム終端を探す）。
fn read_one_response<S: Read>(stream: &mut S, buf: &mut [u8]) -> Option<()> {
    let mut acc: Vec<u8> = Vec::with_capacity(1024);
    // ヘッダ終端まで読む
    let header_end = loop {
        let n = stream.read(buf).ok()?;
        if n == 0 {
            return None;
        }
        acc.extend_from_slice(&buf[..n]);
        if let Some(pos) = find_subsequence(&acc, b"\r\n\r\n") {
            break pos + 4;
        }
        if acc.len() > 1 << 20 {
            return None; // ヘッダが大きすぎる
        }
    };

    let header_str = String::from_utf8_lossy(&acc[..header_end]).to_ascii_lowercase();
    let body_already = acc.len() - header_end;

    if let Some(cl) = header_str
        .split("content-length:")
        .nth(1)
        .and_then(|s| s.split("\r\n").next())
        .and_then(|s| s.trim().parse::<usize>().ok())
    {
        let mut remaining = cl.saturating_sub(body_already);
        while remaining > 0 {
            let n = stream.read(buf).ok()?;
            if n == 0 {
                return None;
            }
            remaining = remaining.saturating_sub(n);
        }
        return Some(());
    }

    if header_str.contains("transfer-encoding:") && header_str.contains("chunked") {
        // 終端チャンク "0\r\n\r\n" を探すまで読む（簡易）。
        loop {
            if find_subsequence(&acc, b"0\r\n\r\n").is_some() {
                return Some(());
            }
            let n = stream.read(buf).ok()?;
            if n == 0 {
                return None;
            }
            acc.extend_from_slice(&buf[..n]);
            if acc.len() > 1 << 20 {
                return Some(());
            }
        }
    }

    // ボディ無し（204/304 等）
    Some(())
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// プロキシが起動しているか
fn is_proxy_running() -> bool {
    connect_tls().is_some()
}

/// `/wasm/*` ルート（WASM 設定）が利用可能か。レスポンスが返れば true。
fn is_wasm_route_available() -> bool {
    let Some((mut stream, mut tls_conn)) = connect_tls() else {
        return false;
    };
    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        WASM_PATH
    );
    if tls_stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = Vec::new();
    let _ = tls_stream.read_to_end(&mut response);
    // 404 でなく何らかの HTTP レスポンスが返ればルートは存在するとみなす。
    let head = String::from_utf8_lossy(&response[..response.len().min(64)]);
    head.starts_with("HTTP/1.1 2") || head.starts_with("HTTP/1.1 3") || head.starts_with("HTTP/1.1 5")
}

/// 新規接続あたりのオーバーヘッド比較
fn benchmark_wasm_overhead_per_connection(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping WASM benchmarks");
        return;
    }
    if !is_wasm_route_available() {
        eprintln!("/wasm/* route not available (start e2e with WASM config); skipping");
        return;
    }

    let mut group = c.benchmark_group("wasm_overhead_per_connection");
    group.measurement_time(Duration::from_secs(10));

    group.bench_function("no_wasm", |b| {
        b.iter(|| measure_request_latency(BASELINE_PATH));
    });
    group.bench_function("header_filter", |b| {
        b.iter(|| measure_request_latency(WASM_PATH));
    });

    group.finish();
}

/// Keep-Alive で接続コストを償却した 1 リクエストあたりのオーバーヘッド比較。
/// no_wasm と header_filter の差が WASM フィルタの純オーバーヘッド近似。
fn benchmark_wasm_overhead_keepalive(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping WASM keepalive benchmarks");
        return;
    }
    if !is_wasm_route_available() {
        eprintln!("/wasm/* route not available (start e2e with WASM config); skipping");
        return;
    }

    let mut group = c.benchmark_group("wasm_overhead_keepalive");
    group.measurement_time(Duration::from_secs(15));

    const COUNT: usize = 50;
    for (label, path) in [("no_wasm", BASELINE_PATH), ("header_filter", WASM_PATH)] {
        group.bench_with_input(BenchmarkId::new("requests_x50", label), &path, |b, &path| {
            b.iter(|| measure_keepalive(path, COUNT));
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    benchmark_wasm_overhead_per_connection,
    benchmark_wasm_overhead_keepalive,
);
criterion_main!(benches);
