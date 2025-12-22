//! # High-Performance Reverse Proxy Server
//!
//! io_uring (monoio) と rustls + ktls2 を使用した高性能リバースプロキシサーバー。
//!
//! ## 特徴
//!
//! - **非同期I/O**: monoio (io_uring) による効率的なI/O処理
//! - **TLS**: rustls によるPure Rust TLS実装
//! - **kTLS**: rustls + ktls2 によるカーネルTLSオフロード対応
//! - **コネクションプール**: バックエンド接続の再利用
//! - **バッファプール**: メモリアロケーションの削減
//! - **Keep-Alive**: HTTP/1.1 Keep-Alive完全サポート
//!
//! ## kTLS（Kernel TLS）サポート
//!
//! ### 概要
//!
//! kTLSはLinuxカーネルの機能で、TLSデータ転送フェーズの暗号化/復号化を
//! カーネルレベルで行うことにより、以下のパフォーマンス向上を実現します：
//!
//! | 項目 | 効果 |
//! |------|------|
//! | CPU使用率 | 20-40%削減（高負荷時） |
//! | スループット | 最大2倍向上 |
//! | レイテンシ | コンテキストスイッチ削減 |
//! | ゼロコピー | sendfile + TLS暗号化 |
//!
//! ### 有効化方法
//!
//! kTLSはrustls + ktls2経由でサポートされています。
//!
//! ```bash
//! # 1. カーネルモジュールのロード
//! sudo modprobe tls
//!
//! # 2. ktlsフィーチャー付きでビルド
//! cargo build --release --features ktls
//!
//! # 3. 設定ファイルで有効化
//! # config.toml:
//! # [tls]
//! # cert_path = "cert.pem"
//! # key_path = "key.pem"
//! # ktls_enabled = true
//! ```
//!
//! ### 要件
//!
//! - Linux 5.15以上（推奨）
//! - `tls`カーネルモジュールがロード済み
//! - AES-GCM暗号スイート（TLS 1.2/1.3）
//! - ktlsフィーチャーでビルド（`--features ktls`）
//!
//! ### セキュリティ考慮事項
//!
//! | リスク | 緩和策 |
//! |--------|--------|
//! | カーネルバグ | カーネルバージョン固定、定期的なパッチ適用 |
//! | セッションキー露出 | TLSハンドシェイクはrustlsで実行 |
//! | DoS攻撃 | カーネルリソース監視、レート制限 |
//! | NICファームウェア脆弱性 | ハードウェアオフロード無効化オプション |
//!
//! ### パフォーマンス測定
//!
//! kTLSの効果を測定するには：
//!
//! ```bash
//! # 1. ベースライン（kTLS無効 / rustls使用）
//! cargo build --release
//! ./target/release/veil &
//! wrk -t4 -c100 -d30s https://localhost/
//!
//! # 2. kTLS有効（rustls + ktls2使用）
//! cargo build --release --features ktls
//! # config.tomlでktls_enabled = true
//! ./target/release/veil &
//! wrk -t4 -c100 -d30s https://localhost/
//!
//! # CPU使用率の比較
//! # スループット（req/sec）の比較
//! ```
//!
//! ### 参考資料
//!
//! - [Linux Kernel TLS](https://docs.kernel.org/networking/tls.html)
//! - [rustls](https://github.com/rustls/rustls): Pure Rust TLS実装
//! - [ktls2](https://crates.io/crates/ktls2): rustls用kTLS統合クレート

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

// ktls_rustls モジュール（kTLS 対応）
#[cfg(feature = "ktls")]
mod ktls_rustls;

// ====================
// HTTP/2・HTTP/3 モジュール
// ====================
//
// HTTP/2 (h2): TLS ALPN ネゴシエーションによる HTTP/2 サポート
// HTTP/3 (h3): QUIC/UDP ベースの HTTP/3 サポート

/// プロトコル抽象化（ALPN ネゴシエーション）
#[cfg(feature = "http2")]
pub mod protocol;

/// HTTP/2 プロトコル実装 (RFC 7540)
/// - HPACK ヘッダー圧縮
/// - フレーム処理（DATA, HEADERS, SETTINGS, etc.）
/// - ストリーム管理・フロー制御
/// - コネクション管理
#[cfg(feature = "http2")]
pub mod http2;

/// HTTP/3 サーバー (monoio + quiche ベース)
/// - QUIC プロトコル (RFC 9000)
/// - HTTP/3 (RFC 9114)
/// - monoio io_uring で UDP I/O を処理
/// - タイマー管理 (quiche::timeout + monoio::time::timeout)
/// - H3 インスタンスの永続化 (QPACK 動的テーブル等の状態維持)
#[cfg(feature = "http3")]
pub mod http3_server;

/// セキュリティ強化モジュール
/// - io_uring操作制限（IORING_REGISTER_RESTRICTIONS）
/// - seccompシステムコール制限
/// - Landlockファイルシステム制限
pub mod security;

/// バッファリング制御モジュール
/// - 低速クライアントによるバックエンド占有防止
/// - フルバッファリング・適応型バッファリング
pub mod buffering;

/// プロキシキャッシュモジュール
/// - インメモリキャッシュ（DashMap + LRU）
/// - ディスクキャッシュ（monoio::fs 非同期I/O）
/// - Cache-Control / Vary ヘッダー対応
pub mod cache;

use httparse::{Request, Status};
use monoio::fs::OpenOptions;
use monoio::buf::{IoBuf, IoBufMut};
use monoio::io::{AsyncReadRent, AsyncWriteRentExt};
use monoio::net::{TcpListener, TcpStream};
use monoio::RuntimeBuilder;
use monoio::time::timeout;
use clap::Parser;
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::fs::File;
use std::io;
use std::io::BufReader;
use std::net::SocketAddr;
#[cfg(target_os = "linux")]
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "linux")]
use std::sync::atomic::AtomicUsize;
use std::thread;
use std::time::Duration;
use ftlog::{info, error, warn, debug, LevelFilter};
use memchr::memchr3;
use time::OffsetDateTime;
use arc_swap::ArcSwap;
use once_cell::sync::Lazy;
use prometheus::{
    CounterVec, Histogram, HistogramOpts, HistogramVec, IntGaugeVec,
    Opts, Registry, TextEncoder, Encoder,
};

// rustls 共通インポート
use rustls::ServerConfig;
use rustls::crypto::CryptoProvider;
use rustls_pemfile::{certs, private_key};

// ktls_rustls（kTLS 対応）
#[cfg(feature = "ktls")]
use ktls_rustls::{RustlsAcceptor, RustlsConnector, KtlsServerStream, KtlsClientStream};

// ====================
// TLS ストリーム型エイリアス
// ====================
// 
// ktls フィーチャーの有無に応じて、使用する TLS ストリーム型を切り替えます。
// kTLS 有効時は ktls_rustls モジュールの型を使用し、
// 無効時はシンプルな rustls ラッパーを使用します。

#[cfg(feature = "ktls")]
type ServerTls = KtlsServerStream;

#[cfg(feature = "ktls")]
type ClientTls = KtlsClientStream;

// kTLS 無効時は直接 rustls を使用するシンプルなラッパー
#[cfg(not(feature = "ktls"))]
mod simple_tls;

#[cfg(not(feature = "ktls"))]
type ServerTls = simple_tls::SimpleTlsServerStream;

#[cfg(not(feature = "ktls"))]
type ClientTls = simple_tls::SimpleTlsClientStream;

// ====================
// kTLS設定情報
// ====================
//
// kTLSはrustls + ktls2経由でサポートされています。
// `cargo build --features ktls` でビルドしてください。
//

/// kTLS設定情報
#[derive(Clone, Debug, Default)]
pub struct KtlsConfig {
    /// kTLSを有効化するかどうか
    pub enabled: bool,
    /// TLS TX（送信）のkTLSを有効化
    pub enable_tx: bool,
    /// TLS RX（受信）のkTLSを有効化
    pub enable_rx: bool,
    /// kTLS有効化失敗時にrustlsへフォールバックするかどうか
    /// false: kTLS必須（失敗時は接続拒否）
    /// true: kTLS失敗時はrustlsで継続（デフォルト）
    pub fallback_enabled: bool,
}

// ====================
// Huge Pages (Large OS Pages) 設定
// ====================
//
// mimallocでHuge Pages（2MB）を優先使用することで、
// TLB（Translation Lookaside Buffer）ミスを削減し、
// パフォーマンスを5-10%向上させます。
//
// 特にkTLS/splice時のカーネル連携で効果的です。
//
// ## 要件（Linux）
// - /proc/sys/vm/nr_hugepages に十分な値を設定
//   例: echo 128 | sudo tee /proc/sys/vm/nr_hugepages
// - または /etc/sysctl.conf に vm.nr_hugepages=128 を追加
//
// ## コンテナ環境
// Docker/K8sでは以下の設定が必要な場合があります：
// - --cap-add=SYS_ADMIN または --privileged
// - ホスト側でHuge Pagesを事前に確保
//

/// Huge Pages の可用性情報
#[derive(Debug)]
struct HugePagesInfo {
    /// Huge Pagesが利用可能かどうか
    available: bool,
    /// 確保済みHuge Pages総数
    total: u64,
    /// 空きHuge Pages数
    free: u64,
    /// Huge Pagesサイズ（KB）
    page_size_kb: u64,
}

/// /proc/meminfo から Huge Pages 情報を取得
/// 
/// Linux以外の環境やファイルアクセスに失敗した場合は
/// available=false を返します。
#[cfg(target_os = "linux")]
fn check_huge_pages_availability() -> HugePagesInfo {
    use std::fs::File as StdFile;
    use std::io::{BufRead, BufReader as StdBufReader};
    
    let mut info = HugePagesInfo {
        available: false,
        total: 0,
        free: 0,
        page_size_kb: 0,
    };
    
    let file = match StdFile::open("/proc/meminfo") {
        Ok(f) => f,
        Err(_) => return info,
    };
    
    let reader = StdBufReader::new(file);
    for line in reader.lines().flatten() {
        if line.starts_with("HugePages_Total:") {
            if let Some(val) = line.split_whitespace().nth(1) {
                info.total = val.parse().unwrap_or(0);
            }
        } else if line.starts_with("HugePages_Free:") {
            if let Some(val) = line.split_whitespace().nth(1) {
                info.free = val.parse().unwrap_or(0);
            }
        } else if line.starts_with("Hugepagesize:") {
            if let Some(val) = line.split_whitespace().nth(1) {
                info.page_size_kb = val.parse().unwrap_or(0);
            }
        }
    }
    
    info.available = info.total > 0;
    info
}

/// Linux以外の環境ではHuge Pagesは利用不可
#[cfg(not(target_os = "linux"))]
fn check_huge_pages_availability() -> HugePagesInfo {
    HugePagesInfo {
        available: false,
        total: 0,
        free: 0,
        page_size_kb: 0,
    }
}

/// mimalloc の Large OS Pages 設定を有効化し、状態をログ出力
/// 
/// Huge Pagesが利用可能な場合は有効化し、
/// 利用不可の場合は警告を出力して通常ページにフォールバックします。
fn configure_huge_pages(enabled: bool) {
    if !enabled {
        info!("Huge Pages: Disabled in configuration");
        return;
    }
    
    let hp_info = check_huge_pages_availability();
    
    if hp_info.available {
        // libmimalloc-sys を使用して Large OS Pages を有効化
        #[cfg(target_os = "linux")]
        {
            unsafe {
                // mi_option_large_os_pages = 6 (2MiB large pages)
                libmimalloc_sys::mi_option_set(
                    libmimalloc_sys::mi_option_large_os_pages,
                    1,
                );
            }
        }
        
        info!("Huge Pages: Enabled (Total: {}, Free: {}, Size: {}KB)", 
              hp_info.total, hp_info.free, hp_info.page_size_kb);
        info!("Huge Pages: TLB miss reduction active, expected 5-10% performance improvement");
        
        // 空きページが少ない場合は警告
        if hp_info.free < hp_info.total / 2 {
            warn!("Huge Pages: Free pages running low ({}/{}), consider increasing nr_hugepages", 
                  hp_info.free, hp_info.total);
        }
    } else {
        warn!("Huge Pages: Requested but not available on this system");
        #[cfg(target_os = "linux")]
        {
            warn!("Huge Pages: To enable, run: echo 128 | sudo tee /proc/sys/vm/nr_hugepages");
            warn!("Huge Pages: In containers, ensure hugepages are allocated on the host");
        }
        #[cfg(not(target_os = "linux"))]
        {
            warn!("Huge Pages: Only supported on Linux");
        }
        info!("Huge Pages: Falling back to standard 4KB pages");
    }
}

// ====================
// Coarse Timer（粗いタイマー）
// ====================
//
// Nginxと同様の最適化。システムコール（clock_gettime）の呼び出しを削減するため、
// 時刻をキャッシュし、一定間隔でのみ更新する。
//
// - ログのタイムスタンプ表示用: キャッシュした OffsetDateTime を使用
// - 処理時間計測用: std::time::Instant を使用（モノトニック・高精度）
//
// スレッドローカルでキャッシュするため、マルチスレッド環境でもロックフリー。

use std::cell::Cell;
use std::time::Instant;

/// Coarse Timer の更新間隔（ミリ秒）
/// 100ms間隔で時刻を更新。ログのタイムスタンプには十分な精度。
const COARSE_TIMER_UPDATE_INTERVAL_MS: u128 = 100;

thread_local! {
    /// キャッシュされた時刻（ログ表示用）
    static CACHED_TIME: Cell<OffsetDateTime> = Cell::new(OffsetDateTime::now_utc());
    /// 最後に時刻を更新したInstant
    static LAST_UPDATE: Cell<Instant> = Cell::new(Instant::now());
}

/// Coarse Timer から現在時刻を取得（ログ表示用）
/// 
/// キャッシュされた時刻を返す。COARSE_TIMER_UPDATE_INTERVAL_MS 経過していれば更新。
/// システムコールの呼び出しを大幅に削減。
#[inline]
fn coarse_now() -> OffsetDateTime {
    CACHED_TIME.with(|cached| {
        LAST_UPDATE.with(|last| {
            let now_instant = Instant::now();
            let elapsed = now_instant.duration_since(last.get()).as_millis();
            
            if elapsed >= COARSE_TIMER_UPDATE_INTERVAL_MS {
                // 更新間隔を超えた場合のみシステムコールを発行
                let now_time = OffsetDateTime::now_utc();
                cached.set(now_time);
                last.set(now_instant);
                now_time
            } else {
                // キャッシュされた時刻を返す
                cached.get()
            }
        })
    })
}


// ====================
// Prometheusメトリクス
// ====================
//
// リクエスト数、レイテンシ、エラー率などを計測し、
// Prometheusフォーマットでエクスポートします。
//
// メトリクスエンドポイント: /__metrics (設定で変更可能)
//
// ## 計測対象
//
// - http_requests_total: リクエスト総数（method, status, hostラベル付き）
// - http_request_duration_seconds: リクエスト処理時間のヒストグラム
// - http_request_size_bytes: リクエストボディサイズのヒストグラム
// - http_response_size_bytes: レスポンスボディサイズのヒストグラム
// - http_active_connections: アクティブな接続数（ホスト別）
// - http_upstream_health: アップストリームの健康状態
//
// ====================

/// Prometheusメトリクスレジストリ（グローバル）
static METRICS_REGISTRY: Lazy<Registry> = Lazy::new(|| {
    Registry::new()
});

/// HTTPリクエスト総数カウンター（method, status, host ラベル付き）
static HTTP_REQUESTS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    let opts = Opts::new("http_requests_total", "Total number of HTTP requests")
        .namespace("veil_proxy");
    let counter = CounterVec::new(opts, &["method", "status", "host"]).unwrap();
    METRICS_REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

/// HTTPリクエスト処理時間ヒストグラム（method, host ラベル付き）
static HTTP_REQUEST_DURATION_SECONDS: Lazy<HistogramVec> = Lazy::new(|| {
    let opts = HistogramOpts::new("http_request_duration_seconds", "HTTP request duration in seconds")
        .namespace("veil_proxy")
        .buckets(vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]);
    let histogram = HistogramVec::new(opts, &["method", "host"]).unwrap();
    METRICS_REGISTRY.register(Box::new(histogram.clone())).unwrap();
    histogram
});

/// HTTPリクエストボディサイズヒストグラム
static HTTP_REQUEST_SIZE_BYTES: Lazy<Histogram> = Lazy::new(|| {
    let opts = HistogramOpts::new("http_request_size_bytes", "HTTP request body size in bytes")
        .namespace("veil_proxy")
        .buckets(vec![100.0, 1000.0, 10000.0, 100000.0, 1000000.0, 10000000.0]);
    let histogram = Histogram::with_opts(opts).unwrap();
    METRICS_REGISTRY.register(Box::new(histogram.clone())).unwrap();
    histogram
});

/// HTTPレスポンスボディサイズヒストグラム
static HTTP_RESPONSE_SIZE_BYTES: Lazy<Histogram> = Lazy::new(|| {
    let opts = HistogramOpts::new("http_response_size_bytes", "HTTP response body size in bytes")
        .namespace("veil_proxy")
        .buckets(vec![100.0, 1000.0, 10000.0, 100000.0, 1000000.0, 10000000.0, 100000000.0]);
    let histogram = Histogram::with_opts(opts).unwrap();
    METRICS_REGISTRY.register(Box::new(histogram.clone())).unwrap();
    histogram
});

/// アクティブ接続数ゲージ（ホスト別）
/// TODO: 接続開始/終了時にインクリメント/デクリメントを追加
#[allow(dead_code)]
static HTTP_ACTIVE_CONNECTIONS: Lazy<IntGaugeVec> = Lazy::new(|| {
    let opts = Opts::new("http_active_connections", "Number of active HTTP connections")
        .namespace("veil_proxy");
    let gauge = IntGaugeVec::new(opts, &["host"]).unwrap();
    METRICS_REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

/// アップストリーム健康状態ゲージ（upstream, server ラベル付き）
/// 1 = healthy, 0 = unhealthy
/// TODO: ヘルスチェック結果に応じて更新
#[allow(dead_code)]
static HTTP_UPSTREAM_HEALTH: Lazy<IntGaugeVec> = Lazy::new(|| {
    let opts = Opts::new("http_upstream_health", "Upstream server health status (1=healthy, 0=unhealthy)")
        .namespace("veil_proxy");
    let gauge = IntGaugeVec::new(opts, &["upstream", "server"]).unwrap();
    METRICS_REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

/// Prometheusメトリクスをテキストフォーマットでエンコード
fn encode_prometheus_metrics() -> Vec<u8> {
    let encoder = TextEncoder::new();
    let metric_families = METRICS_REGISTRY.gather();
    let mut buffer = Vec::new();
    encoder.encode(&metric_families, &mut buffer).unwrap_or_default();
    buffer
}

/// メトリクスを記録（リクエスト完了時に呼び出し）
/// 
/// ## パフォーマンス最適化
/// 
/// status.to_string() による毎回のアロケーションを回避するため、
/// itoa クレートを使用してスタック上のバッファに書き込みます。
/// これにより、高負荷時（数万RPS）でもヒープアロケーションを削減。
#[inline]
fn record_request_metrics(
    method: &str,
    host: &str,
    status: u16,
    req_body_size: u64,
    resp_body_size: u64,
    duration_secs: f64,
) {
    // ステータスコードを事前割り当てバッファで文字列化（アロケーション回避）
    let mut status_buf = itoa::Buffer::new();
    let status_str = status_buf.format(status);
    
    // リクエスト総数をインクリメント
    HTTP_REQUESTS_TOTAL
        .with_label_values(&[method, status_str, host])
        .inc();
    
    // 処理時間を記録
    HTTP_REQUEST_DURATION_SECONDS
        .with_label_values(&[method, host])
        .observe(duration_secs);
    
    // リクエスト/レスポンスサイズを記録
    HTTP_REQUEST_SIZE_BYTES.observe(req_body_size as f64);
    HTTP_RESPONSE_SIZE_BYTES.observe(resp_body_size as f64);
}

/// メトリクスエンドポイント用のHTTPレスポンスを生成
fn build_metrics_response() -> Vec<u8> {
    let body = encode_prometheus_metrics();
    let mut response = Vec::with_capacity(256 + body.len());
    response.extend_from_slice(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: ");
    
    let mut num_buf = itoa::Buffer::new();
    response.extend_from_slice(num_buf.format(body.len()).as_bytes());
    response.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
    response.extend_from_slice(&body);
    response
}

// ====================
// 定数定義（パフォーマンスチューニング済み）
// ====================

// エラーレスポンス用静的バッファ
static ERR_MSG_BAD_REQUEST: &[u8] = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
static ERR_MSG_FORBIDDEN: &[u8] = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
static ERR_MSG_NOT_FOUND: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
static ERR_MSG_METHOD_NOT_ALLOWED: &[u8] = b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
static ERR_MSG_TOO_MANY_REQUESTS: &[u8] = b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
static ERR_MSG_BAD_GATEWAY: &[u8] = b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
static ERR_MSG_REQUEST_TOO_LARGE: &[u8] = b"HTTP/1.1 413 Request Entity Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
static ERR_MSG_GATEWAY_TIMEOUT: &[u8] = b"HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

// HTTP ヘッダー部品（事前計算）
static HTTP_200_PREFIX: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: ";
static CONTENT_LENGTH_HEADER: &[u8] = b"\r\nContent-Length: ";

// HTTPリクエスト構築用定数（ホットパス最適化）
static HEADER_HTTP11_HOST: &[u8] = b" HTTP/1.1\r\nHost: ";
static HEADER_COLON: &[u8] = b": ";
static HEADER_CRLF: &[u8] = b"\r\n";
static HEADER_SPACE: &[u8] = b" ";
static HEADER_PORT_COLON: &[u8] = b":";
static HEADER_CONNECTION_KEEPALIVE_END: &[u8] = b"Connection: keep-alive\r\n\r\n";

// バッファサイズ（ページアライン・L2キャッシュ最適化）
const BUF_SIZE: usize = 65536;           // 64KB - io_uring最適サイズ
const HEADER_BUF_CAPACITY: usize = 512;  // HTTPヘッダー用

// ====================
// 安全なバッファラッパー（SafeReadBuffer）
// ====================
//
// 未初期化メモリへのアクセスリスクを型システムで防止します。
//
// ## 設計原則
//
// 1. io_uringへの読み込みには内部バッファ全体を使用
// 2. 読み込み完了後、有効データ長（valid_len）を設定
// 3. ユーザーコードは valid_len 経由でのみデータにアクセス可能
//
// ## 安全性保証
//
// - `as_valid_slice()` は読み込まれたデータのみを返す
// - 未初期化領域へのアクセスはコンパイル時に防止される
// - `buf.len()` の誤用によるセキュリティリスクを排除
//
// ====================

/// 安全な読み込みバッファラッパー
/// 
/// io_uring読み込み操作で使用され、読み込まれたデータ長を追跡することで
/// 未初期化メモリへのアクセスを型レベルで防止します。
/// 
/// # 使用例
/// 
/// ```rust,ignore
/// let mut buf = SafeReadBuffer::new(BUF_SIZE);
/// // io_uring読み込み操作（内部バッファを使用）
/// let (result, mut returned_buf) = stream.read(buf.into_inner()).await;
/// // 読み込み成功後、有効長を設定
/// returned_buf.set_valid_len(n);
/// // 安全なアクセス：有効データのみが返される
/// let data = returned_buf.as_valid_slice();
/// ```
pub struct SafeReadBuffer {
    /// 内部バッファ（BUF_SIZE容量）
    inner: Vec<u8>,
    /// 有効データ長（読み込み操作で設定される）
    valid_len: usize,
}

impl SafeReadBuffer {
    /// 新しいバッファを作成
    /// 
    /// # Arguments
    /// * `cap` - バッファ容量
    /// 
    /// # Safety
    /// io_uringに渡すために一時的に長さを確保しますが、
    /// ユーザーコードからは valid_len 経由でしかアクセスできません。
    #[inline(always)]
    #[allow(clippy::uninit_vec)]
    pub fn new(cap: usize) -> Self {
        let mut v = Vec::with_capacity(cap);
        // SAFETY: io_uringに渡すための事前確保
        // 読み込み前は valid_len = 0 なので未初期化領域にはアクセスできない
        // SafeReadBuffer は as_valid_slice() を通じてのみデータにアクセスするため、
        // 未初期化領域への誤アクセスは型レベルで防止されている
        unsafe { v.set_len(cap); }
        Self { inner: v, valid_len: 0 }
    }
    
    /// 既存のVec<u8>からバッファを作成
    /// 
    /// プール返却時に使用。valid_len は 0 にリセットされます。
    #[inline(always)]
    #[allow(clippy::uninit_vec)]
    pub fn from_vec(mut v: Vec<u8>, cap: usize) -> Self {
        if v.capacity() >= cap {
            // SAFETY: capacity >= cap を確認済み
            unsafe { v.set_len(cap); }
        } else {
            // 容量不足の場合は新規作成
            v = Vec::with_capacity(cap);
            unsafe { v.set_len(cap); }
        }
        Self { inner: v, valid_len: 0 }
    }

    /// 読み込み完了後に有効データ長を設定
    /// 
    /// # Arguments
    /// * `len` - 読み込まれたバイト数
    /// 
    /// # Note
    /// バッファ容量を超える値は自動的にクランプされます。
    #[inline(always)]
    pub fn set_valid_len(&mut self, len: usize) {
        self.valid_len = len.min(self.inner.len());
    }

    /// 有効データのスライスを取得
    /// 
    /// 読み込まれたデータのみを返します。
    /// 未初期化領域にはアクセスできません。
    #[inline(always)]
    pub fn as_valid_slice(&self) -> &[u8] {
        &self.inner[..self.valid_len]
    }
    
    /// 有効データ長を取得
    #[inline(always)]
    pub fn valid_len(&self) -> usize {
        self.valid_len
    }
    
    /// バッファ容量を取得
    #[inline(always)]
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }
    
    /// 内部バッファの長さを取得（io_uring用）
    #[inline(always)]
    pub fn len(&self) -> usize {
        self.inner.len()
    }
    
    /// 有効データが空かどうかを確認
    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.valid_len == 0
    }
    
    /// 内部Vecを取り出す（プール返却用）
    /// 
    /// # Warning
    /// 返されたVecは未初期化データを含む可能性があります。
    /// 必ず SafeReadBuffer::from_vec() でラップし直してください。
    #[inline(always)]
    pub fn into_inner(self) -> Vec<u8> {
        self.inner
    }
    
    /// 有効データをtruncateして内部Vecを取り出す
    /// 
    /// 書き込み操作用。有効データのみを含むVecを返します。
    #[inline(always)]
    pub fn into_truncated(mut self) -> Vec<u8> {
        self.inner.truncate(self.valid_len);
        self.inner
    }
}

// monoio の IoBuf トレイト実装
// SAFETY: inner は有効なヒープメモリを指し、read_ptr() は有効なポインタを返す
unsafe impl IoBuf for SafeReadBuffer {
    #[inline(always)]
    fn read_ptr(&self) -> *const u8 {
        self.inner.read_ptr()
    }

    #[inline(always)]
    fn bytes_init(&self) -> usize {
        self.inner.bytes_init()
    }
}

// monoio の IoBufMut トレイト実装
// SAFETY: inner は有効な書き込み可能なヒープメモリを指す
unsafe impl IoBufMut for SafeReadBuffer {
    #[inline(always)]
    fn write_ptr(&mut self) -> *mut u8 {
        self.inner.write_ptr()
    }

    #[inline(always)]
    fn bytes_total(&mut self) -> usize {
        self.inner.bytes_total()
    }

    #[inline(always)]
    unsafe fn set_init(&mut self, pos: usize) {
        self.inner.set_init(pos);
        // io_uringからの読み込み完了時に呼ばれる
        // valid_len も更新する
        self.valid_len = pos;
    }
}

// セキュリティ制限
const MAX_HEADER_SIZE: usize = 8192;     // 8KB - ヘッダーサイズ上限
const MAX_BODY_SIZE: usize = 10485760;   // 10MB - ボディサイズ上限

// タイムアウト設定
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

// バックエンドコネクションプール設定（デフォルト値）
const BACKEND_POOL_MAX_IDLE_PER_HOST: usize = 8;    // ホストあたりの最大アイドル接続数
const BACKEND_POOL_IDLE_TIMEOUT_SECS: u64 = 30;     // アイドル接続のタイムアウト（秒）

// ====================
// セキュリティ設定構造体
// ====================
//
// ルートごとのセキュリティ設定を保持します。
// config.toml の各ルートに security セクションを追加することで
// 個別の制限を設定できます。
// ====================

/// セキュリティ設定のデフォルト値関数
fn default_max_body_size() -> usize { MAX_BODY_SIZE }
fn default_max_header_size() -> usize { MAX_HEADER_SIZE }
fn default_client_header_timeout() -> u64 { 30 }
fn default_client_body_timeout() -> u64 { 30 }
fn default_backend_connect_timeout() -> u64 { 10 }
fn default_max_idle_connections() -> usize { BACKEND_POOL_MAX_IDLE_PER_HOST }
fn default_idle_connection_timeout() -> u64 { BACKEND_POOL_IDLE_TIMEOUT_SECS }

// WebSocket ポーリング設定のデフォルト値
fn default_websocket_poll_timeout_ms() -> u64 { 1 }
fn default_websocket_poll_max_timeout_ms() -> u64 { 100 }
fn default_websocket_backoff_multiplier() -> f64 { 2.0 }

// ====================
// IP制限機能（CIDR対応）
// ====================
//
// allowed_ips と denied_ips でルートごとのIP制限を設定できます。
// CIDR記法（例: "192.168.1.0/24"）と単一IP（例: "10.0.0.1"）の両方をサポート。
//
// 評価順序: deny → allow（denyが優先）
// - denied_ips にマッチ → 拒否
// - allowed_ips が空 → 許可
// - allowed_ips にマッチ → 許可
// - それ以外 → 拒否
// ====================

/// CIDR範囲を表す構造体
#[derive(Clone, Debug)]
pub struct CidrRange {
    /// ネットワークアドレス（IPv4は32ビット、IPv6は128ビット）
    network: u128,
    /// プレフィックス長
    prefix_len: u8,
    /// IPv6かどうか
    is_ipv6: bool,
}

impl CidrRange {
    /// CIDR文字列をパース（例: "192.168.1.0/24" または "10.0.0.1"）
    pub fn parse(s: &str) -> Option<Self> {
        let (ip_str, prefix_len) = if let Some(idx) = s.find('/') {
            let prefix: u8 = s[idx + 1..].parse().ok()?;
            (&s[..idx], prefix)
        } else {
            // プレフィックスなし = 単一IP
            (s, 255) // 255は後で適切な値に変換
        };
        
        // IPv4をパース
        if let Some(ipv4) = Self::parse_ipv4(ip_str) {
            let prefix = if prefix_len == 255 { 32 } else { prefix_len };
            if prefix > 32 {
                return None;
            }
            // IPv4を128ビットの上位に配置（IPv6-mapped形式ではなく単純に格納）
            let network = (ipv4 as u128) & Self::mask_v4(prefix);
            return Some(CidrRange {
                network,
                prefix_len: prefix,
                is_ipv6: false,
            });
        }
        
        // IPv6をパース
        if let Some(ipv6) = Self::parse_ipv6(ip_str) {
            let prefix = if prefix_len == 255 { 128 } else { prefix_len };
            if prefix > 128 {
                return None;
            }
            let network = ipv6 & Self::mask_v6(prefix);
            return Some(CidrRange {
                network,
                prefix_len: prefix,
                is_ipv6: true,
            });
        }
        
        None
    }
    
    /// IPv4アドレス文字列をパース
    fn parse_ipv4(s: &str) -> Option<u32> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 4 {
            return None;
        }
        
        let mut result: u32 = 0;
        for (i, part) in parts.iter().enumerate() {
            let octet: u8 = part.parse().ok()?;
            result |= (octet as u32) << (24 - i * 8);
        }
        Some(result)
    }
    
    /// IPv6アドレス文字列をパース（簡易実装）
    fn parse_ipv6(s: &str) -> Option<u128> {
        // :: の展開を処理
        let parts: Vec<&str> = s.split(':').collect();
        
        // :: がある場合の処理
        let has_double_colon = s.contains("::");
        if has_double_colon {
            let sides: Vec<&str> = s.split("::").collect();
            if sides.len() > 2 {
                return None;
            }
            
            let left_parts: Vec<&str> = if sides[0].is_empty() {
                vec![]
            } else {
                sides[0].split(':').collect()
            };
            
            let right_parts: Vec<&str> = if sides.len() < 2 || sides[1].is_empty() {
                vec![]
            } else {
                sides[1].split(':').collect()
            };
            
            let missing = 8 - left_parts.len() - right_parts.len();
            let mut all_parts: Vec<u16> = Vec::with_capacity(8);
            
            for part in &left_parts {
                all_parts.push(u16::from_str_radix(part, 16).ok()?);
            }
            for _ in 0..missing {
                all_parts.push(0);
            }
            for part in &right_parts {
                all_parts.push(u16::from_str_radix(part, 16).ok()?);
            }
            
            if all_parts.len() != 8 {
                return None;
            }
            
            let mut result: u128 = 0;
            for (i, &part) in all_parts.iter().enumerate() {
                result |= (part as u128) << (112 - i * 16);
            }
            return Some(result);
        }
        
        // :: がない場合
        if parts.len() != 8 {
            return None;
        }
        
        let mut result: u128 = 0;
        for (i, part) in parts.iter().enumerate() {
            let segment: u16 = u16::from_str_radix(part, 16).ok()?;
            result |= (segment as u128) << (112 - i * 16);
        }
        Some(result)
    }
    
    /// IPv4用のネットマスクを生成
    #[inline]
    fn mask_v4(prefix: u8) -> u128 {
        if prefix == 0 {
            0
        } else if prefix >= 32 {
            0xFFFF_FFFF
        } else {
            ((1u128 << prefix) - 1) << (32 - prefix)
        }
    }
    
    /// IPv6用のネットマスクを生成
    #[inline]
    fn mask_v6(prefix: u8) -> u128 {
        if prefix == 0 {
            0
        } else if prefix >= 128 {
            u128::MAX
        } else {
            ((1u128 << prefix) - 1) << (128 - prefix)
        }
    }
    
    /// IPアドレスがこのCIDR範囲に含まれるかチェック
    pub fn contains(&self, ip: &str) -> bool {
        if self.is_ipv6 {
            // IPv6
            if let Some(ipv6) = Self::parse_ipv6(ip) {
                let masked = ipv6 & Self::mask_v6(self.prefix_len);
                return masked == self.network;
            }
        } else {
            // IPv4
            if let Some(ipv4) = Self::parse_ipv4(ip) {
                let masked = (ipv4 as u128) & Self::mask_v4(self.prefix_len);
                return masked == self.network;
            }
        }
        false
    }
}

/// IPフィルター（許可/拒否リスト）
#[derive(Clone, Debug, Default)]
pub struct IpFilter {
    /// 許可するIP/CIDR範囲（空 = すべて許可）
    pub allowed: Vec<CidrRange>,
    /// 拒否するIP/CIDR範囲
    pub denied: Vec<CidrRange>,
}

impl IpFilter {
    /// 文字列リストからIpFilterを構築
    pub fn from_lists(allowed_ips: &[String], denied_ips: &[String]) -> Self {
        let allowed: Vec<CidrRange> = allowed_ips
            .iter()
            .filter_map(|s| CidrRange::parse(s))
            .collect();
        
        let denied: Vec<CidrRange> = denied_ips
            .iter()
            .filter_map(|s| CidrRange::parse(s))
            .collect();
        
        Self { allowed, denied }
    }
    
    /// IPアドレスが許可されているかチェック
    /// 評価順序: deny → allow（denyが優先）
    pub fn is_allowed(&self, ip: &str) -> bool {
        // denyリストにマッチしたら拒否
        for cidr in &self.denied {
            if cidr.contains(ip) {
                return false;
            }
        }
        
        // allowリストが空なら許可
        if self.allowed.is_empty() {
            return true;
        }
        
        // allowリストにマッチしたら許可
        for cidr in &self.allowed {
            if cidr.contains(ip) {
                return true;
            }
        }
        
        // どちらにもマッチしない場合は拒否
        false
    }
    
    /// フィルターが設定されているか（空でないか）
    pub fn is_configured(&self) -> bool {
        !self.allowed.is_empty() || !self.denied.is_empty()
    }
}

// ====================
// WebSocket ポーリング設定
// ====================

/// WebSocketポーリングモード
/// 
/// - `Fixed`: 固定タイムアウト（低レイテンシ優先）
/// - `Adaptive`: バックオフ方式による動的調整（CPU効率優先）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WebSocketPollMode {
    /// 固定タイムアウト - 常に同じタイムアウト値を使用
    /// 低レイテンシが最優先の場合（リアルタイムゲームなど）に推奨
    Fixed,
    /// バックオフ方式 - アクティブ時は短く、アイドル時は長くなる
    /// CPU効率とレイテンシのバランスを取る場合（チャットなど）に推奨
    #[default]
    Adaptive,
}

impl<'de> serde::Deserialize<'de> for WebSocketPollMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.to_lowercase().as_str() {
            "fixed" => Ok(WebSocketPollMode::Fixed),
            "adaptive" => Ok(WebSocketPollMode::Adaptive),
            other => Err(serde::de::Error::custom(format!(
                "unknown websocket_poll_mode: '{}', expected 'fixed' or 'adaptive'",
                other
            ))),
        }
    }
}

/// WebSocketポーリング設定
/// 
/// この設定は、WebSocket双方向転送時のポーリング動作を制御します。
/// 
/// ## モード
/// 
/// - **Fixed**: 常に `initial_timeout_ms` でポーリング
/// - **Adaptive**: データ転送時は `initial_timeout_ms` を使用し、
///   アイドル時は `max_timeout_ms` まで徐々に延長
/// 
/// ## 設定例
/// 
/// ```toml
/// # リアルタイムゲーム（低レイテンシ最優先）
/// websocket_poll_mode = "fixed"
/// websocket_poll_timeout_ms = 1
/// 
/// # チャットアプリ（バランス重視）
/// websocket_poll_mode = "adaptive"
/// websocket_poll_timeout_ms = 1
/// websocket_poll_max_timeout_ms = 50
/// ```
#[derive(Clone, Debug)]
pub struct WebSocketPollConfig {
    /// ポーリングモード
    pub mode: WebSocketPollMode,
    /// 初期タイムアウト（ミリ秒）
    /// Fixedモード: この値を固定で使用
    /// Adaptiveモード: この値から開始
    pub initial_timeout_ms: u64,
    /// 最大タイムアウト（ミリ秒）- Adaptiveモードでのみ使用
    /// タイムアウトはこの値を超えて延長されない
    pub max_timeout_ms: u64,
    /// バックオフ倍率 - Adaptiveモードでのみ使用
    /// タイムアウト発生時に現在値に掛ける倍率
    pub backoff_multiplier: f64,
}

impl Default for WebSocketPollConfig {
    fn default() -> Self {
        Self {
            mode: WebSocketPollMode::Adaptive,
            initial_timeout_ms: default_websocket_poll_timeout_ms(),
            max_timeout_ms: default_websocket_poll_max_timeout_ms(),
            backoff_multiplier: default_websocket_backoff_multiplier(),
        }
    }
}

/// ルートごとのセキュリティ設定
#[derive(Deserialize, Clone, Debug)]
pub struct SecurityConfig {
    /// リクエストボディ最大サイズ（バイト）
    #[serde(default = "default_max_body_size")]
    pub max_request_body_size: usize,
    
    /// Chunked転送時の累積最大サイズ（バイト）
    #[serde(default = "default_max_body_size")]
    pub max_chunked_body_size: usize,
    
    /// クライアントヘッダー受信タイムアウト（秒）
    #[serde(default = "default_client_header_timeout")]
    pub client_header_timeout_secs: u64,
    
    /// クライアントボディ受信タイムアウト（秒）
    #[serde(default = "default_client_body_timeout")]
    pub client_body_timeout_secs: u64,
    
    /// 許可するHTTPメソッド（空 = すべて許可）
    #[serde(default)]
    pub allowed_methods: Vec<String>,
    
    /// 分間リクエスト数上限（0 = 無制限）
    #[serde(default)]
    pub rate_limit_requests_per_min: u64,
    
    /// バックエンド接続タイムアウト（秒）
    #[serde(default = "default_backend_connect_timeout")]
    pub backend_connect_timeout_secs: u64,
    
    /// ホストごとの最大アイドル接続数
    #[serde(default = "default_max_idle_connections")]
    pub max_idle_connections_per_host: usize,
    
    /// アイドル接続の維持時間（秒）
    #[serde(default = "default_idle_connection_timeout")]
    pub idle_connection_timeout_secs: u64,
    
    /// リクエストヘッダー最大サイズ（バイト）
    #[serde(default = "default_max_header_size")]
    pub max_request_header_size: usize,
    
    /// 許可するIPアドレス/CIDR（空 = すべて許可）
    /// 例: ["192.168.1.0/24", "10.0.0.1"]
    #[serde(default)]
    pub allowed_ips: Vec<String>,
    
    /// 拒否するIPアドレス/CIDR（denyが優先）
    /// 例: ["192.168.1.100", "10.0.0.0/8"]
    #[serde(default)]
    pub denied_ips: Vec<String>,
    
    // ====================
    // ヘッダー操作設定
    // ====================
    
    /// リクエストに追加するヘッダー（バックエンドへ転送前）
    /// 例: { "X-Real-IP" = "$client_ip", "X-Forwarded-Proto" = "https" }
    /// 
    /// 特殊変数:
    /// - $client_ip: クライアントのIPアドレス
    /// - $host: リクエストのHostヘッダー
    /// - $request_uri: リクエストURI
    #[serde(default)]
    pub add_request_headers: HashMap<String, String>,
    
    /// リクエストから削除するヘッダー（バックエンドへ転送前）
    /// 例: ["X-Debug", "X-Internal-Token"]
    #[serde(default)]
    pub remove_request_headers: Vec<String>,
    
    /// レスポンスに追加するヘッダー（クライアントへ返送前）
    /// 例: { "X-Frame-Options" = "DENY", "Strict-Transport-Security" = "max-age=31536000" }
    #[serde(default)]
    pub add_response_headers: HashMap<String, String>,
    
    /// レスポンスから削除するヘッダー（クライアントへ返送前）
    /// 例: ["Server", "X-Powered-By"]
    #[serde(default)]
    pub remove_response_headers: Vec<String>,
    
    // ====================
    // WebSocket設定
    // ====================
    
    /// WebSocketポーリングモード
    /// 
    /// - `"fixed"`: 固定タイムアウト（低レイテンシ優先）
    /// - `"adaptive"`: バックオフ方式による動的調整（CPU効率優先）
    /// 
    /// デフォルト: `"adaptive"`
    #[serde(default)]
    pub websocket_poll_mode: WebSocketPollMode,
    
    /// WebSocketポーリング初期タイムアウト（ミリ秒）
    /// 
    /// - fixedモード: この値を固定で使用
    /// - adaptiveモード: この値から開始し、アイドル時に徐々に延長
    /// 
    /// デフォルト: `1`
    #[serde(default = "default_websocket_poll_timeout_ms")]
    pub websocket_poll_timeout_ms: u64,
    
    /// WebSocketポーリング最大タイムアウト（ミリ秒）
    /// 
    /// adaptiveモードでのみ使用。
    /// タイムアウトはこの値を超えて延長されない。
    /// 
    /// デフォルト: `100`
    #[serde(default = "default_websocket_poll_max_timeout_ms")]
    pub websocket_poll_max_timeout_ms: u64,
    
    /// WebSocketバックオフ倍率
    /// 
    /// adaptiveモードでタイムアウト発生時に現在値に掛ける倍率。
    /// 
    /// 例: `2.0` → 1ms → 2ms → 4ms → 8ms → ... → 100ms（最大値）
    /// 
    /// デフォルト: `2.0`
    #[serde(default = "default_websocket_backoff_multiplier")]
    pub websocket_poll_backoff_multiplier: f64,
}

impl SecurityConfig {
    /// IP制限フィルターを構築
    pub fn ip_filter(&self) -> IpFilter {
        IpFilter::from_lists(&self.allowed_ips, &self.denied_ips)
    }
    
    /// ヘッダー操作が設定されているかどうか
    pub fn has_header_operations(&self) -> bool {
        !self.add_request_headers.is_empty() ||
        !self.remove_request_headers.is_empty() ||
        !self.add_response_headers.is_empty() ||
        !self.remove_response_headers.is_empty()
    }
    
    /// セキュリティチェックが設定されているかどうか
    /// 
    /// ## パフォーマンス最適化
    /// 
    /// セキュリティ設定が全てデフォルト値の場合、ホットパスでの
    /// 複数のチェックを完全にスキップできます。
    /// これにより、設定がないルートでは5-10%の高速化が期待できます。
    /// 
    /// チェック対象:
    /// - IP制限（allowed_ips, denied_ips）
    /// - HTTPメソッド制限（allowed_methods）
    /// - レートリミット（rate_limit_requests_per_min）
    #[inline]
    pub fn has_security_checks(&self) -> bool {
        !self.allowed_ips.is_empty() ||
        !self.denied_ips.is_empty() ||
        !self.allowed_methods.is_empty() ||
        self.rate_limit_requests_per_min > 0
    }
    
    /// WebSocketポーリング設定を構築
    /// 
    /// SecurityConfigのWebSocket関連フィールドから
    /// WebSocketPollConfig構造体を生成します。
    #[inline]
    pub fn websocket_poll_config(&self) -> WebSocketPollConfig {
        WebSocketPollConfig {
            mode: self.websocket_poll_mode,
            initial_timeout_ms: self.websocket_poll_timeout_ms,
            max_timeout_ms: self.websocket_poll_max_timeout_ms,
            backoff_multiplier: self.websocket_poll_backoff_multiplier,
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            max_request_body_size: default_max_body_size(),
            max_chunked_body_size: default_max_body_size(),
            client_header_timeout_secs: default_client_header_timeout(),
            client_body_timeout_secs: default_client_body_timeout(),
            allowed_methods: Vec::new(),
            rate_limit_requests_per_min: 0,
            backend_connect_timeout_secs: default_backend_connect_timeout(),
            max_idle_connections_per_host: default_max_idle_connections(),
            idle_connection_timeout_secs: default_idle_connection_timeout(),
            max_request_header_size: default_max_header_size(),
            allowed_ips: Vec::new(),
            denied_ips: Vec::new(),
            add_request_headers: HashMap::new(),
            remove_request_headers: Vec::new(),
            add_response_headers: HashMap::new(),
            remove_response_headers: Vec::new(),
            // WebSocket設定
            websocket_poll_mode: WebSocketPollMode::default(),
            websocket_poll_timeout_ms: default_websocket_poll_timeout_ms(),
            websocket_poll_max_timeout_ms: default_websocket_poll_max_timeout_ms(),
            websocket_poll_backoff_multiplier: default_websocket_backoff_multiplier(),
        }
    }
}

// ====================
// 圧縮設定（プロキシバックエンド用）
// ====================
//
// ルートごとにレスポンス圧縮を設定できます。
// デフォルトは無効で、kTLS最適化を維持します。
// 
// 有効にすると、バックエンドからのレスポンスを動的に圧縮し、
// クライアントへ転送します。この場合、kTLSのゼロコピー最適化は
// 迂回されます。
// ====================

/// クライアントがサポートする圧縮方式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptedEncoding {
    /// Zstandard (zstd) - 最高効率
    Zstd,
    /// Brotli (br) - 高圧縮率
    Brotli,
    /// Gzip - 標準的な圧縮
    Gzip,
    /// Deflate（互換性のため）
    Deflate,
    /// 圧縮なし
    Identity,
}

impl AcceptedEncoding {
    /// Accept-Encodingヘッダーから最適な圧縮方式を選択
    /// 
    /// 優先順位: zstd > br > gzip > deflate > identity
    /// q値（品質値）も考慮します。
    pub fn parse(value: &[u8]) -> Self {
        let value_str = match std::str::from_utf8(value) {
            Ok(s) => s.to_ascii_lowercase(),
            Err(_) => return Self::Identity,
        };
        
        // q値を考慮した解析
        let mut best = (Self::Identity, 0.0f32);
        
        for part in value_str.split(',') {
            let part = part.trim();
            let (encoding, q) = if let Some((enc, q_part)) = part.split_once(";q=") {
                (enc.trim(), q_part.trim().parse().unwrap_or(1.0))
            } else {
                (part, 1.0)
            };
            
            let candidate = match encoding {
                "zstd" => (Self::Zstd, q),
                "br" => (Self::Brotli, q),
                "gzip" => (Self::Gzip, q),
                "deflate" => (Self::Deflate, q),
                "*" => (Self::Gzip, q * 0.9), // * は gzip として扱う
                _ => continue,
            };
            
            // q値が高いもの、または同じq値ならZstd > Brotliを優先
            if candidate.1 > best.1 || 
               (candidate.1 == best.1 && matches!(candidate.0, Self::Zstd)) ||
               (candidate.1 == best.1 && matches!(candidate.0, Self::Brotli) && !matches!(best.0, Self::Zstd)) {
                best = candidate;
            }
        }
        
        best.0
    }
    
    /// Content-Encodingヘッダー値を返す
    pub fn as_header_value(&self) -> &'static [u8] {
        match self {
            Self::Zstd => b"zstd",
            Self::Brotli => b"br",
            Self::Gzip => b"gzip",
            Self::Deflate => b"deflate",
            Self::Identity => b"identity",
        }
    }
}

/// ルートごとの圧縮設定
#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct CompressionConfig {
    /// 圧縮を有効にするかどうか
    /// デフォルト: false（kTLS最適化を維持）
    pub enabled: bool,
    
    /// 圧縮方式の優先順位
    /// サポート: "zstd", "br" (Brotli), "gzip", "deflate"
    /// デフォルト: ["zstd", "br", "gzip"]
    pub preferred_encodings: Vec<String>,
    
    /// Gzip圧縮レベル (1-9)
    /// 1: 最速（圧縮率低）、9: 最遅（圧縮率高）
    /// デフォルト: 4（バランス重視）
    #[serde(default = "default_gzip_level")]
    pub gzip_level: u32,
    
    /// Brotli圧縮レベル (0-11)
    /// 0: 最速、11: 最遅（圧縮率最高）
    /// デフォルト: 4（バランス重視）
    #[serde(default = "default_brotli_level")]
    pub brotli_level: u32,
    
    /// Zstd圧縮レベル (1-22)
    /// 1: 最速、22: 最遅（圧縮率最高）
    /// デフォルト: 3（高速重視）
    #[serde(default = "default_zstd_level")]
    pub zstd_level: i32,
    
    /// 最小圧縮サイズ（バイト）
    /// これより小さいレスポンスは圧縮オーバーヘッドの方が大きいためスキップ
    /// デフォルト: 1024 (1KB)
    #[serde(default = "default_compression_min_size")]
    pub min_size: usize,
    
    /// 圧縮対象のMIMEタイプ（プレフィックスマッチ）
    /// デフォルト: ["text/", "application/json", "application/javascript", ...]
    #[serde(default = "default_compressible_types")]
    pub compressible_types: Vec<String>,
    
    /// 圧縮をスキップするMIMEタイプ（プレフィックスマッチ）
    /// これらにマッチするレスポンスは圧縮対象から除外
    /// デフォルト: ["image/", "video/", "audio/", ...]
    #[serde(default = "default_skip_types")]
    pub skip_types: Vec<String>,
}

// 圧縮設定のデフォルト値
fn default_gzip_level() -> u32 { 4 }
fn default_brotli_level() -> u32 { 4 }
fn default_zstd_level() -> i32 { 3 }  // zstdは1-22、3が高速でバランス良好
fn default_compression_min_size() -> usize { 1024 }

fn default_compressible_types() -> Vec<String> {
    vec![
        "text/".into(),
        "application/json".into(),
        "application/javascript".into(),
        "application/xml".into(),
        "application/xhtml+xml".into(),
        "application/rss+xml".into(),
        "application/atom+xml".into(),
        "image/svg+xml".into(),
        "application/wasm".into(),
    ]
}

fn default_skip_types() -> Vec<String> {
    vec![
        "image/".into(),
        "video/".into(),
        "audio/".into(),
        "application/octet-stream".into(),
        "application/zip".into(),
        "application/gzip".into(),
        "application/x-gzip".into(),
        "application/x-brotli".into(),
    ]
}

fn default_preferred_encodings() -> Vec<String> {
    vec!["zstd".into(), "br".into(), "gzip".into()]
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            enabled: false,  // デフォルト無効（kTLS最適化維持）
            preferred_encodings: default_preferred_encodings(),
            gzip_level: default_gzip_level(),
            brotli_level: default_brotli_level(),
            zstd_level: default_zstd_level(),
            min_size: default_compression_min_size(),
            compressible_types: default_compressible_types(),
            skip_types: default_skip_types(),
        }
    }
}

impl CompressionConfig {
    /// 設定の妥当性を検証
    pub fn validate(&self) -> Result<(), String> {
        if self.gzip_level < 1 || self.gzip_level > 9 {
            return Err(format!("invalid gzip_level: {} (must be 1-9)", self.gzip_level));
        }
        if self.brotli_level > 11 {
            return Err(format!("invalid brotli_level: {} (must be 0-11)", self.brotli_level));
        }
        if self.zstd_level < 1 || self.zstd_level > 22 {
            return Err(format!("invalid zstd_level: {} (must be 1-22)", self.zstd_level));
        }
        for enc in &self.preferred_encodings {
            match enc.as_str() {
                "zstd" | "gzip" | "br" | "deflate" => {}
                _ => return Err(format!("unknown encoding: {}", enc)),
            }
        }
        Ok(())
    }
    
    /// レスポンスを圧縮すべきか判定
    /// 
    /// # Arguments
    /// * `client_encoding` - クライアントがサポートする圧縮方式
    /// * `content_type` - レスポンスのContent-Type
    /// * `content_length` - レスポンスのContent-Length（既知の場合）
    /// * `existing_encoding` - バックエンドからのContent-Encoding
    /// 
    /// # Returns
    /// 圧縮すべき場合は使用する圧縮方式、それ以外はNone
    pub fn should_compress(
        &self,
        client_encoding: AcceptedEncoding,
        content_type: Option<&[u8]>,
        content_length: Option<usize>,
        existing_encoding: Option<&[u8]>,
    ) -> Option<AcceptedEncoding> {
        // 1. 圧縮が無効
        if !self.enabled {
            return None;
        }
        
        // 2. クライアントが圧縮非対応
        if client_encoding == AcceptedEncoding::Identity {
            return None;
        }
        
        // 3. バックエンドが既に圧縮済み
        if let Some(enc) = existing_encoding {
            if !enc.is_empty() && !enc.eq_ignore_ascii_case(b"identity") {
                return None;
            }
        }
        
        // 4. Content-Type確認
        if let Some(ct) = content_type {
            let ct_str = std::str::from_utf8(ct).unwrap_or("");
            
            // スキップ対象をチェック
            for skip in &self.skip_types {
                if ct_str.starts_with(skip) {
                    return None;
                }
            }
            
            // 圧縮対象をチェック
            let is_compressible = self.compressible_types.iter()
                .any(|t| ct_str.starts_with(t));
            
            if !is_compressible {
                return None;
            }
        } else {
            // Content-Typeがない場合は圧縮しない
            return None;
        }
        
        // 5. サイズ確認
        if let Some(len) = content_length {
            if len < self.min_size {
                return None;
            }
        }
        
        // 6. クライアントがサポートし、かつ設定で許可されている圧縮方式を選択
        let client_supports = |enc: &str| -> bool {
            match (enc, client_encoding) {
                ("zstd", AcceptedEncoding::Zstd) => true,
                ("br", AcceptedEncoding::Brotli | AcceptedEncoding::Zstd) => true,
                ("gzip", AcceptedEncoding::Gzip | AcceptedEncoding::Brotli | AcceptedEncoding::Zstd) => true,
                ("deflate", AcceptedEncoding::Deflate | AcceptedEncoding::Gzip | AcceptedEncoding::Brotli | AcceptedEncoding::Zstd) => true,
                _ => false,
            }
        };
        
        for enc in &self.preferred_encodings {
            if client_supports(enc) {
                return match enc.as_str() {
                    "zstd" if client_encoding == AcceptedEncoding::Zstd => Some(AcceptedEncoding::Zstd),
                    "br" if matches!(client_encoding, AcceptedEncoding::Brotli | AcceptedEncoding::Zstd) => Some(AcceptedEncoding::Brotli),
                    "gzip" if matches!(client_encoding, AcceptedEncoding::Gzip | AcceptedEncoding::Brotli | AcceptedEncoding::Zstd) => Some(AcceptedEncoding::Gzip),
                    "deflate" => Some(AcceptedEncoding::Deflate),
                    _ => continue,
                };
            }
        }
        
        // クライアントの圧縮方式を使用
        Some(client_encoding)
    }
}

/// HTTP/3用の圧縮設定を解決
/// 
/// 優先順位:
/// 1. パスごとの設定 (compression.enabled = false なら圧縮しない)
/// 2. HTTP/3専用設定 (http3.compression_enabled + http3.compression.*)
/// 3. パスごとのデフォルト値
/// 
/// # 引数
/// * `path_compression` - パスごとの圧縮設定
/// * `http3_config` - HTTP/3セクションの設定
/// 
/// # 戻り値
/// 解決された圧縮設定
pub fn resolve_http3_compression_config(
    path_compression: &CompressionConfig,
    http3_config: &Http3ConfigSection,
) -> CompressionConfig {
    // パスごとの設定で明示的に有効化されている場合はそれを優先
    // （パス設定が既に有効なら、HTTP/3設定で上書きするだけ）
    if path_compression.enabled {
        // パス設定が有効な場合、HTTP/3専用パラメータで上書き
        let h3_comp = &http3_config.compression;
        return CompressionConfig {
            enabled: true,
            preferred_encodings: h3_comp.preferred_encodings
                .clone()
                .unwrap_or_else(|| path_compression.preferred_encodings.clone()),
            gzip_level: h3_comp.gzip_level
                .unwrap_or(path_compression.gzip_level),
            brotli_level: h3_comp.brotli_level
                .unwrap_or(path_compression.brotli_level),
            zstd_level: h3_comp.zstd_level
                .unwrap_or(path_compression.zstd_level),
            min_size: h3_comp.min_size
                .unwrap_or(path_compression.min_size),
            compressible_types: h3_comp.compressible_types
                .clone()
                .unwrap_or_else(|| path_compression.compressible_types.clone()),
            skip_types: h3_comp.skip_types
                .clone()
                .unwrap_or_else(|| path_compression.skip_types.clone()),
        };
    }
    
    // パスごとの設定で圧縮が無効の場合
    // HTTP/3の compression_enabled をチェック
    if http3_config.compression_enabled {
        // HTTP/3で圧縮が有効化されている場合、HTTP/3専用設定を適用
        let h3_comp = &http3_config.compression;
        return CompressionConfig {
            enabled: true, // HTTP/3では有効
            preferred_encodings: h3_comp.preferred_encodings
                .clone()
                .unwrap_or_else(|| path_compression.preferred_encodings.clone()),
            gzip_level: h3_comp.gzip_level
                .unwrap_or(path_compression.gzip_level),
            brotli_level: h3_comp.brotli_level
                .unwrap_or(path_compression.brotli_level),
            zstd_level: h3_comp.zstd_level
                .unwrap_or(path_compression.zstd_level),
            min_size: h3_comp.min_size
                .unwrap_or(path_compression.min_size),
            compressible_types: h3_comp.compressible_types
                .clone()
                .unwrap_or_else(|| path_compression.compressible_types.clone()),
            skip_types: h3_comp.skip_types
                .clone()
                .unwrap_or_else(|| path_compression.skip_types.clone()),
        };
    }
    
    // HTTP/3圧縮も無効の場合はパス設定をそのまま使用（圧縮無効）
    path_compression.clone()
}

/// グローバルセキュリティ設定
#[derive(Deserialize, Clone, Debug, Default)]
pub struct GlobalSecurityConfig {
    /// 起動後に降格するユーザー名（非root推奨）
    #[serde(default)]
    pub drop_privileges_user: Option<String>,
    
    /// 起動後に降格するグループ名
    #[serde(default)]
    pub drop_privileges_group: Option<String>,
    
    /// グローバル同時接続上限（0 = 無制限）
    #[serde(default)]
    pub max_concurrent_connections: usize,
    
    // ====================
    // io_uring / seccomp セキュリティ設定
    // ====================
    
    /// seccompフィルタを有効化（Linux専用）
    /// システムコールを制限してio_uringの悪用を防止
    #[serde(default)]
    pub enable_seccomp: bool,
    
    /// seccompモード
    /// - "disabled": 無効
    /// - "log": 違反をログに記録（ブロックしない）
    /// - "filter": 違反をEPERMで拒否
    /// - "strict": 違反したプロセスをSIGKILL
    #[serde(default = "default_seccomp_mode")]
    pub seccomp_mode: String,
    
    /// Landlockファイルシステム制限を有効化（Linux 5.13+）
    #[serde(default)]
    pub enable_landlock: bool,
    
    /// Landlock読み取り専用パス
    #[serde(default = "default_landlock_read_paths")]
    pub landlock_read_paths: Vec<String>,
    
    /// Landlock読み書きパス
    #[serde(default = "default_landlock_write_paths")]
    pub landlock_write_paths: Vec<String>,
    
    // ====================
    // サンドボックス設定（bubblewrap相当）
    // ====================
    //
    // Linuxのnamespace分離、bind mounts、capabilities制限を
    // プログラム起動時に適用することで、bubblewrapと同等の
    // セキュリティ分離を実現します。
    //
    // 適用順序:
    // 1. サンドボックス（namespace分離、bind mounts、capabilities）
    // 2. 権限降格（setuid/setgid）
    // 3. Landlock（ファイルシステム制限）
    // 4. seccomp（システムコール制限）
    //
    
    /// サンドボックスを有効化
    /// bubblewrap相当のnamespace分離、bind mounts、capabilities制限を適用
    #[serde(default)]
    pub enable_sandbox: bool,
    
    /// PID namespace分離
    /// サンドボックス内のプロセスは外部のプロセスを見ることができなくなります
    #[serde(default)]
    pub sandbox_unshare_pid: bool,
    
    /// Mount namespace分離
    /// サンドボックス内で独自のマウントポイントを持ちます
    #[serde(default = "default_sandbox_unshare_mount")]
    pub sandbox_unshare_mount: bool,
    
    /// UTS namespace分離
    /// サンドボックス内で独自のホスト名を持ちます
    #[serde(default = "default_sandbox_unshare_uts")]
    pub sandbox_unshare_uts: bool,
    
    /// IPC namespace分離
    /// サンドボックス内で独自のIPC（共有メモリ、セマフォ等）を持ちます
    #[serde(default = "default_sandbox_unshare_ipc")]
    pub sandbox_unshare_ipc: bool,
    
    /// User namespace分離
    /// 注: 複雑なケースがあるためデフォルトは無効
    #[serde(default)]
    pub sandbox_unshare_user: bool,
    
    /// Network namespace分離
    /// 警告: trueにするとネットワーク通信ができなくなります
    /// サーバーでは通常false（--share-net相当）
    #[serde(default)]
    pub sandbox_unshare_net: bool,
    
    /// 読み取り専用バインドマウント
    /// source:dest 形式で指定（例: "/usr:/usr"）
    #[serde(default = "default_sandbox_ro_binds")]
    pub sandbox_ro_bind_mounts: Vec<String>,
    
    /// 読み書きバインドマウント
    /// source:dest 形式で指定（例: "/var/log:/var/log"）
    #[serde(default)]
    pub sandbox_rw_bind_mounts: Vec<String>,
    
    /// tmpfsマウント先
    /// 指定されたパスにtmpfs（メモリファイルシステム）をマウント
    #[serde(default = "default_sandbox_tmpfs")]
    pub sandbox_tmpfs_mounts: Vec<String>,
    
    /// /proc をマウントするかどうか
    #[serde(default = "default_true")]
    pub sandbox_mount_proc: bool,
    
    /// /dev に最小限のデバイスノードを作成するかどうか
    #[serde(default = "default_true")]
    pub sandbox_mount_dev: bool,
    
    /// ドロップするケイパビリティのリスト
    /// 例: ["CAP_SYS_ADMIN", "CAP_NET_RAW"]
    #[serde(default)]
    pub sandbox_drop_capabilities: Vec<String>,
    
    /// 保持するケイパビリティのリスト（他は全てドロップ）
    /// drop_capabilitiesより優先されます
    /// 例: ["CAP_NET_BIND_SERVICE"]
    #[serde(default)]
    pub sandbox_keep_capabilities: Vec<String>,
    
    /// サンドボックス内のホスト名
    #[serde(default = "default_sandbox_hostname")]
    pub sandbox_hostname: Option<String>,
    
    /// PR_SET_NO_NEW_PRIVSを設定するかどうか
    #[serde(default = "default_true")]
    pub sandbox_no_new_privs: bool,
}

fn default_sandbox_unshare_mount() -> bool { true }
fn default_sandbox_unshare_uts() -> bool { true }
fn default_sandbox_unshare_ipc() -> bool { true }
fn default_true() -> bool { true }

fn default_sandbox_ro_binds() -> Vec<String> {
    vec![
        "/usr:/usr".to_string(),
        "/lib:/lib".to_string(),
        "/lib64:/lib64".to_string(),
        "/etc/ssl:/etc/ssl".to_string(),
        // DNS解決に必要なファイル
        "/etc/resolv.conf:/etc/resolv.conf".to_string(),
        "/etc/hosts:/etc/hosts".to_string(),
        "/etc/nsswitch.conf:/etc/nsswitch.conf".to_string(),
        "/etc/gai.conf:/etc/gai.conf".to_string(),
        // systemd-resolved使用時に必要（存在しない場合は無視される）
        "/run/systemd/resolve:/run/systemd/resolve".to_string(),
        // ユーザー/グループ情報
        "/etc/passwd:/etc/passwd".to_string(),
        "/etc/group:/etc/group".to_string(),
    ]
}

fn default_sandbox_tmpfs() -> Vec<String> {
    vec![
        "/tmp".to_string(),
        // 注: /run はsystemd-resolvedのDNS解決に必要なため除外
        // 必要な場合は明示的に追加してください
    ]
}

fn default_sandbox_hostname() -> Option<String> {
    Some("veil-sandbox".to_string())
}

fn default_seccomp_mode() -> String {
    "disabled".to_string()
}

fn default_landlock_read_paths() -> Vec<String> {
    vec![
        "/etc".to_string(),
        "/usr".to_string(),
        "/lib".to_string(),
        "/lib64".to_string(),
    ]
}

fn default_landlock_write_paths() -> Vec<String> {
    vec![
        "/var/log".to_string(),
        "/tmp".to_string(),
    ]
}

// ====================
// Prometheusメトリクス設定セクション
// ====================

/// Prometheusメトリクス設定
/// 
/// メトリクスエンドポイントの有効化、パス変更、アクセス制限を設定します。
/// 
/// 例:
/// ```toml
/// [prometheus]
/// enabled = true
/// path = "/metrics"
/// allowed_ips = ["127.0.0.1", "10.0.0.0/8"]
/// ```
#[derive(Deserialize, Clone, Debug)]
pub struct PrometheusConfig {
    /// メトリクスエンドポイントを有効化するかどうか
    /// デフォルト: true
    #[serde(default = "default_prometheus_enabled")]
    pub enabled: bool,
    
    /// メトリクスエンドポイントのパス
    /// デフォルト: "/__metrics"
    #[serde(default = "default_prometheus_path")]
    pub path: String,
    
    /// メトリクスエンドポイントへのアクセスを許可するIPアドレス/CIDR
    /// 空の場合はすべてのIPからアクセス可能
    /// 例: ["127.0.0.1", "10.0.0.0/8", "192.168.0.0/16"]
    #[serde(default)]
    pub allowed_ips: Vec<String>,
}

fn default_prometheus_enabled() -> bool { false }
fn default_prometheus_path() -> String { "/__metrics".to_string() }

impl Default for PrometheusConfig {
    fn default() -> Self {
        Self {
            enabled: default_prometheus_enabled(),
            path: default_prometheus_path(),
            allowed_ips: Vec::new(),
        }
    }
}

impl PrometheusConfig {
    /// IPアドレスがメトリクスエンドポイントへのアクセスを許可されているか確認
    pub fn is_ip_allowed(&self, client_ip: &str) -> bool {
        // allowed_ipsが空の場合はすべてのIPを許可
        if self.allowed_ips.is_empty() {
            return true;
        }
        
        // クライアントIPをパース
        let client_addr: std::net::IpAddr = match client_ip.parse() {
            Ok(addr) => addr,
            Err(_) => return false,
        };
        
        for allowed in &self.allowed_ips {
            // CIDR表記かチェック
            if allowed.contains('/') {
                // CIDR表記の場合
                if let Some((network, prefix_len)) = allowed.split_once('/') {
                    if let (Ok(network_addr), Ok(prefix)) = (network.parse::<std::net::IpAddr>(), prefix_len.parse::<u8>()) {
                        if Self::ip_in_cidr(&client_addr, &network_addr, prefix) {
                            return true;
                        }
                    }
                }
            } else {
                // 単一IPアドレスの場合
                if let Ok(allowed_addr) = allowed.parse::<std::net::IpAddr>() {
                    if client_addr == allowed_addr {
                        return true;
                    }
                }
            }
        }
        
        false
    }
    
    /// IPアドレスがCIDRブロック内にあるかチェック
    fn ip_in_cidr(ip: &std::net::IpAddr, network: &std::net::IpAddr, prefix_len: u8) -> bool {
        match (ip, network) {
            (std::net::IpAddr::V4(ip), std::net::IpAddr::V4(net)) => {
                if prefix_len > 32 {
                    return false;
                }
                let mask = if prefix_len == 0 { 0 } else { !0u32 << (32 - prefix_len) };
                let ip_bits = u32::from_be_bytes(ip.octets());
                let net_bits = u32::from_be_bytes(net.octets());
                (ip_bits & mask) == (net_bits & mask)
            }
            (std::net::IpAddr::V6(ip), std::net::IpAddr::V6(net)) => {
                if prefix_len > 128 {
                    return false;
                }
                let ip_bits = u128::from_be_bytes(ip.octets());
                let net_bits = u128::from_be_bytes(net.octets());
                let mask = if prefix_len == 0 { 0 } else { !0u128 << (128 - prefix_len) };
                (ip_bits & mask) == (net_bits & mask)
            }
            _ => false, // IPv4とIPv6の混在は不一致
        }
    }
}

// ====================
// 権限降格機能
// ====================
//
// root権限で起動した後、非特権ユーザーに降格することで
// セキュリティを向上させます。
//
// 注意: 特権ポート（1024未満）を使用する場合は、
// リスナー作成後に権限降格を行う必要があります。
// 現在のSO_REUSEPORT設計では、各スレッドがリスナーを作成するため、
// CAP_NET_BIND_SERVICEケイパビリティを付与するか、
// 非特権ポート（1024以上）を使用することを推奨します。
//
// ケイパビリティ付与例:
//   sudo setcap 'cap_net_bind_service=+ep' ./target/release/veil
// ====================

/// ユーザー名からUIDを取得
#[cfg(target_os = "linux")]
fn get_uid_by_name(username: &str) -> Option<u32> {
    use std::ffi::CString;
    
    let username_cstr = CString::new(username).ok()?;
    
    unsafe {
        let pwd = libc::getpwnam(username_cstr.as_ptr());
        if pwd.is_null() {
            None
        } else {
            Some((*pwd).pw_uid)
        }
    }
}

/// グループ名からGIDを取得
#[cfg(target_os = "linux")]
fn get_gid_by_name(groupname: &str) -> Option<u32> {
    use std::ffi::CString;
    
    let groupname_cstr = CString::new(groupname).ok()?;
    
    unsafe {
        let grp = libc::getgrnam(groupname_cstr.as_ptr());
        if grp.is_null() {
            None
        } else {
            Some((*grp).gr_gid)
        }
    }
}

/// 権限降格を実行
/// 
/// グループ→ユーザーの順で降格する（逆順では失敗する可能性あり）
#[cfg(target_os = "linux")]
fn drop_privileges(security: &GlobalSecurityConfig) -> io::Result<()> {
    // rootでない場合は何もしない
    if unsafe { libc::getuid() } != 0 {
        info!("Not running as root, skipping privilege drop");
        return Ok(());
    }
    
    // グループ降格（先に行う）
    if let Some(ref group_name) = security.drop_privileges_group {
        let gid = get_gid_by_name(group_name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, 
                format!("Group '{}' not found", group_name)))?;
        
        if unsafe { libc::setgid(gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        
        // 補助グループをクリア
        if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
            warn!("Failed to clear supplementary groups: {}", io::Error::last_os_error());
        }
        
        info!("Dropped group privileges to '{}' (gid={})", group_name, gid);
    }
    
    // ユーザー降格
    if let Some(ref user_name) = security.drop_privileges_user {
        let uid = get_uid_by_name(user_name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, 
                format!("User '{}' not found", user_name)))?;
        
        if unsafe { libc::setuid(uid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        
        info!("Dropped user privileges to '{}' (uid={})", user_name, uid);
    }
    
    // 降格成功の確認
    if security.drop_privileges_user.is_some() || security.drop_privileges_group.is_some() {
        let current_uid = unsafe { libc::getuid() };
        let current_gid = unsafe { libc::getgid() };
        info!("Current privileges: uid={}, gid={}", current_uid, current_gid);
        
        // rootに戻れないことを確認
        if security.drop_privileges_user.is_some() {
            if unsafe { libc::setuid(0) } == 0 {
                warn!("WARNING: Process can still regain root privileges!");
            }
        }
    }
    
    Ok(())
}

/// Linux以外のプラットフォーム用のスタブ
#[cfg(not(target_os = "linux"))]
fn drop_privileges(_security: &GlobalSecurityConfig) -> io::Result<()> {
    warn!("Privilege dropping is only supported on Linux");
    Ok(())
}

// ====================
// サンドボックス設定構築
// ====================

/// GlobalSecurityConfigからSandboxConfigを構築
/// 
/// 設定ファイルのsandbox_*フィールドをsecurity::SandboxConfigに変換します。
fn build_sandbox_config(global_security: &GlobalSecurityConfig) -> security::SandboxConfig {
    // 読み取り専用バインドマウントのパース
    let ro_bind_mounts: Vec<security::BindMount> = global_security.sandbox_ro_bind_mounts
        .iter()
        .filter_map(|s| {
            let parts: Vec<&str> = s.splitn(2, ':').collect();
            if parts.len() == 2 {
                Some(security::BindMount::new(parts[0], parts[1]))
            } else if parts.len() == 1 && !parts[0].is_empty() {
                // source:dest が同じ場合は source のみでも可
                Some(security::BindMount::new(parts[0], parts[0]))
            } else {
                warn!("Invalid ro-bind mount format: '{}' (expected 'source:dest')", s);
                None
            }
        })
        .collect();
    
    // 読み書きバインドマウントのパース
    let rw_bind_mounts: Vec<security::BindMount> = global_security.sandbox_rw_bind_mounts
        .iter()
        .filter_map(|s| {
            let parts: Vec<&str> = s.splitn(2, ':').collect();
            if parts.len() == 2 {
                Some(security::BindMount::new(parts[0], parts[1]))
            } else if parts.len() == 1 && !parts[0].is_empty() {
                Some(security::BindMount::new(parts[0], parts[0]))
            } else {
                warn!("Invalid rw-bind mount format: '{}' (expected 'source:dest')", s);
                None
            }
        })
        .collect();
    
    security::SandboxConfig {
        enabled: global_security.enable_sandbox,
        unshare_pid: global_security.sandbox_unshare_pid,
        unshare_mount: global_security.sandbox_unshare_mount,
        unshare_uts: global_security.sandbox_unshare_uts,
        unshare_ipc: global_security.sandbox_unshare_ipc,
        unshare_user: global_security.sandbox_unshare_user,
        unshare_net: global_security.sandbox_unshare_net,
        new_root: None,
        ro_bind_mounts,
        rw_bind_mounts,
        tmpfs_mounts: global_security.sandbox_tmpfs_mounts.clone(),
        mount_proc: global_security.sandbox_mount_proc,
        mount_dev: global_security.sandbox_mount_dev,
        drop_capabilities: global_security.sandbox_drop_capabilities.clone(),
        keep_capabilities: global_security.sandbox_keep_capabilities.clone(),
        hostname: global_security.sandbox_hostname.clone(),
        no_new_privs: global_security.sandbox_no_new_privs,
    }
}

// ====================
// Graceful Shutdown / Hot Reload フラグ
// ====================

/// シャットダウンフラグ（Ctrl+C等でtrueに設定）
/// HTTP/3モジュールからも参照できるようにpub
pub static SHUTDOWN_FLAG: AtomicBool = AtomicBool::new(false);

/// 設定リロード要求フラグ（SIGHUP でトリガー）
/// Arc<AtomicBool> として初期化（signal-hook の要件）
static RELOAD_FLAG: Lazy<Arc<AtomicBool>> = Lazy::new(|| Arc::new(AtomicBool::new(false)));

// ====================
// セキュアなメモリ操作
// ====================

/// セキュアなバイト配列のゼロ化
/// 
/// メモリ上の機密データを安全にゼロ化します。
/// コンパイラによる最適化（デッドストア削除）を防ぐため、
/// volatile 書き込みを使用します。
fn secure_zero(data: &mut [u8]) {
    // volatile 書き込みで最適化を防止
    for byte in data.iter_mut() {
        unsafe {
            std::ptr::write_volatile(byte, 0);
        }
    }
    // メモリバリアで確実に書き込みを完了
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
}

/// Arc<Vec<u8>> をセキュアにゼロ化して解放
/// 
/// Arc の参照カウントが 1（自分だけ）の場合、
/// 内部の Vec をゼロ化してからドロップします。
/// 参照カウントが 2 以上の場合は警告を出力します。
fn secure_clear_arc_vec(arc: &mut Arc<Vec<u8>>, name: &str) {
    match Arc::get_mut(arc) {
        Some(vec) => {
            let len = vec.len();
            secure_zero(vec);
            vec.clear();
            vec.shrink_to_fit();
            info!("[Security] {} securely zeroed ({} bytes)", name, len);
        }
        None => {
            // 他の参照が存在する場合（通常は発生しない）
            warn!("[Security] {} cannot be zeroed: other references exist", name);
        }
    }
}

// ====================
// 同時接続数カウンター
// ====================
//
// グローバルなアトミックカウンターで現在の接続数を追跡します。
// max_concurrent_connections が設定されている場合、上限を超える接続は拒否されます。
// ====================

static CURRENT_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

// ====================
// レートリミッター（スライディングウィンドウ方式）
// ====================
//
// クライアントIPごとに分間リクエスト数を追跡します。
// スレッドローカルで管理し、ロックフリーで高パフォーマンスを実現。
// ====================

/// レートリミットのエントリ
struct RateLimitEntry {
    /// 現在のウィンドウ（分）のリクエスト数
    current_count: u32,
    /// 前のウィンドウ（分）のリクエスト数
    previous_count: u32,
    /// 現在のウィンドウの開始時刻（分単位のタイムスタンプ）
    current_minute: u64,
}

impl RateLimitEntry {
    fn new(current_minute: u64) -> Self {
        Self {
            current_count: 1,
            previous_count: 0,
            current_minute,
        }
    }
    
    /// リクエストを記録し、現在のレートを返す（スライディングウィンドウ方式）
    /// 返り値: 推定される分間リクエスト数
    fn record_request(&mut self, now_minute: u64, now_second_in_minute: u32) -> u32 {
        if now_minute > self.current_minute {
            if now_minute == self.current_minute + 1 {
                // 次の分に移行
                self.previous_count = self.current_count;
                self.current_count = 1;
            } else {
                // 2分以上経過 - リセット
                self.previous_count = 0;
                self.current_count = 1;
            }
            self.current_minute = now_minute;
        } else {
            self.current_count += 1;
        }
        
        // スライディングウィンドウによる推定レート計算
        // 現在の分の経過割合に基づいて重み付け
        let weight = (60 - now_second_in_minute) as f32 / 60.0;
        let estimated = (self.previous_count as f32 * weight) + self.current_count as f32;
        estimated.ceil() as u32
    }
}

/// スレッドローカルなレートリミットマップ
/// キー: クライアントIPアドレス（文字列）
/// 値: RateLimitEntry
struct RateLimiter {
    entries: HashMap<String, RateLimitEntry>,
    last_cleanup: std::time::Instant,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            last_cleanup: std::time::Instant::now(),
        }
    }
    
    /// リクエストをチェックし、レート制限を超えていないか確認
    /// 戻り値: (許可されたか, 現在のレート)
    /// 
    /// ## パフォーマンス最適化
    /// 
    /// SystemTime::now()の代わりにCoarse Timerを使用してシステムコールを削減。
    /// 100ms程度の精度低下は、レートリミットの用途では許容範囲。
    fn check_and_record(&mut self, client_ip: &str, limit: u64) -> (bool, u32) {
        // 定期的なクリーンアップ（5分ごと）
        if self.last_cleanup.elapsed().as_secs() > 300 {
            self.cleanup();
            self.last_cleanup = std::time::Instant::now();
        }
        
        // Coarse Timerから現在時刻を取得（システムコール削減）
        // OffsetDateTime から Unix タイムスタンプを計算
        let now_time = coarse_now();
        let now_secs = now_time.unix_timestamp() as u64;
        let now_minute = now_secs / 60;
        let now_second_in_minute = (now_secs % 60) as u32;
        
        let rate = if let Some(entry) = self.entries.get_mut(client_ip) {
            entry.record_request(now_minute, now_second_in_minute)
        } else {
            self.entries.insert(client_ip.to_string(), RateLimitEntry::new(now_minute));
            1
        };
        
        (rate as u64 <= limit, rate)
    }
    
    /// 古いエントリをクリーンアップ
    /// 
    /// Coarse Timerを使用してシステムコールを削減。
    fn cleanup(&mut self) {
        // Coarse Timerから現在時刻を取得
        let now_time = coarse_now();
        let now_minute = now_time.unix_timestamp() as u64 / 60;
        
        // 2分以上古いエントリを削除
        self.entries.retain(|_, entry| {
            now_minute.saturating_sub(entry.current_minute) < 2
        });
    }
}

thread_local! {
    static RATE_LIMITER: RefCell<RateLimiter> = RefCell::new(RateLimiter::new());
}

/// レートリミットをチェック
/// 戻り値: レート制限内であればtrue
fn check_rate_limit(client_ip: &str, limit: u64) -> bool {
    if limit == 0 {
        return true; // 0 = 無制限
    }
    
    RATE_LIMITER.with(|limiter| {
        let (allowed, _rate) = limiter.borrow_mut().check_and_record(client_ip, limit);
        allowed
    })
}

// ====================
// TLSコネクタ（スレッドローカル）
// ====================

// rustls 用の TLS コネクター（kTLS 有効時は ktls_rustls を使用）
// kTLSフィーチャー有効時はシークレット抽出を有効化し、kTLS利用可能な状態にする
#[cfg(feature = "ktls")]
thread_local! {
    static TLS_CONNECTOR: RustlsConnector = {
        // kTLSフィーチャーが有効な場合はシークレット抽出を有効化
        // kTLSを有効にし、失敗時はrustlsにフォールバック
        let config = ktls_rustls::client_config(true);
        RustlsConnector::new(config)
            .with_ktls(true)        // kTLSを有効化
            .with_fallback(true)    // kTLS失敗時はrustlsにフォールバック
    };
}

// rustls 用の TLS コネクター（kTLS 無効時は simple_tls を使用）
#[cfg(not(feature = "ktls"))]
thread_local! {
    static TLS_CONNECTOR: simple_tls::SimpleTlsConnector = {
        let config = simple_tls::default_client_config();
        simple_tls::SimpleTlsConnector::new(config)
    };
}

// ====================
// バックエンドコネクションプール
// ====================
//
// スレッドローカルなコネクションプールにより、バックエンドへの接続を再利用します。
// HTTP用とHTTPS用で別々のプールを管理し、ホスト:ポートをキーにしています。
// ====================

/// プールされた接続のエントリ
struct PooledConnection<T> {
    stream: T,
    created_at: std::time::Instant,
    /// この接続のアイドルタイムアウト（秒）
    idle_timeout_secs: u64,
}

impl<T> PooledConnection<T> {
    fn new(stream: T, idle_timeout_secs: u64) -> Self {
        Self {
            stream,
            created_at: std::time::Instant::now(),
            idle_timeout_secs,
        }
    }
    
    /// 接続がまだ有効かどうかを判定（タイムアウトチェック）
    fn is_valid(&self) -> bool {
        self.created_at.elapsed().as_secs() < self.idle_timeout_secs
    }
}

/// HTTPバックエンド用コネクションプール（TcpStream）
struct HttpConnectionPool {
    connections: HashMap<String, VecDeque<PooledConnection<TcpStream>>>,
}

impl HttpConnectionPool {
    fn new() -> Self {
        Self {
            connections: HashMap::new(),
        }
    }
    
    /// プールから接続を取得（有効な接続がなければNone）
    fn get(&mut self, key: &str) -> Option<TcpStream> {
        if let Some(queue) = self.connections.get_mut(key) {
            while let Some(entry) = queue.pop_front() {
                if entry.is_valid() {
                    return Some(entry.stream);
                }
                // 無効な接続は破棄
            }
        }
        None
    }
    
    /// 接続をプールに返却（設定可能なパラメータ付き）
    fn put(&mut self, key: String, stream: TcpStream, max_idle: usize, idle_timeout_secs: u64) {
        let queue = self.connections.entry(key).or_insert_with(VecDeque::new);
        
        // 古い接続を削除（設定可能な最大数を使用）
        while queue.len() >= max_idle {
            queue.pop_front();
        }
        
        queue.push_back(PooledConnection::new(stream, idle_timeout_secs));
    }
}

/// HTTPSバックエンド用コネクションプール（ClientTls型エイリアス使用）
struct HttpsConnectionPool {
    connections: HashMap<String, VecDeque<PooledConnection<ClientTls>>>,
}

impl HttpsConnectionPool {
    fn new() -> Self {
        Self {
            connections: HashMap::new(),
        }
    }
    
    /// プールから接続を取得（有効な接続がなければNone）
    fn get(&mut self, key: &str) -> Option<ClientTls> {
        if let Some(queue) = self.connections.get_mut(key) {
            while let Some(entry) = queue.pop_front() {
                if entry.is_valid() {
                    return Some(entry.stream);
                }
                // 無効な接続は破棄
            }
        }
        None
    }
    
    /// 接続をプールに返却（設定可能なパラメータ付き）
    fn put(&mut self, key: String, stream: ClientTls, max_idle: usize, idle_timeout_secs: u64) {
        let queue = self.connections.entry(key).or_insert_with(VecDeque::new);
        
        // 古い接続を削除（設定可能な最大数を使用）
        while queue.len() >= max_idle {
            queue.pop_front();
        }
        
        queue.push_back(PooledConnection::new(stream, idle_timeout_secs));
    }
}

thread_local! {
    static HTTP_POOL: RefCell<HttpConnectionPool> = RefCell::new(HttpConnectionPool::new());
    static HTTPS_POOL: RefCell<HttpsConnectionPool> = RefCell::new(HttpsConnectionPool::new());
}

// kTLS 有効時のスレッドローカル Splice パイプ
// splice(2) によるゼロコピー転送に使用
#[cfg(feature = "ktls")]
thread_local! {
    static SPLICE_PIPE: RefCell<Option<ktls_rustls::SplicePipe>> = RefCell::new(None);
}

/// スレッドローカルな Splice パイプを取得または初期化
#[cfg(feature = "ktls")]
fn get_splice_pipe() -> std::cell::Ref<'static, Option<ktls_rustls::SplicePipe>> {
    SPLICE_PIPE.with(|p| {
        {
            let mut pipe = p.borrow_mut();
            if pipe.is_none() {
                match ktls_rustls::SplicePipe::new() {
                    Ok(new_pipe) => {
                        *pipe = Some(new_pipe);
                        ftlog::info!("Splice pipe initialized for this thread");
                    }
                    Err(e) => {
                        ftlog::warn!("Failed to create splice pipe: {}", e);
                    }
                }
            }
        }
        // Safety: ライフタイムを'staticに拡張（thread_localなので安全）
        unsafe { std::mem::transmute(p.borrow()) }
    })
}

// ====================
// Raw I/O ヘルパー関数（kTLS + splice 用）
// ====================
//
// monoio の所有権ベースの I/O を回避するため、
// libc::read/write を直接使用します。
// 非同期待機は TcpStream::readable()/writable() を使用。
// ====================

/// libc::read のラッパー（ノンブロッキング対応）
#[cfg(feature = "ktls")]
#[inline]
fn raw_read(fd: std::os::unix::io::RawFd, buf: &mut [u8]) -> io::Result<usize> {
    let result = unsafe {
        libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

/// libc::write のラッパー（ノンブロッキング対応）
#[cfg(feature = "ktls")]
#[inline]
fn raw_write(fd: std::os::unix::io::RawFd, buf: &[u8]) -> io::Result<usize> {
    let result = unsafe {
        libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len())
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

/// 非同期 raw read（TcpStream から FD 経由で読み取り）
/// 
/// monoio の所有権ベース I/O を回避し、libc::read を直接使用。
/// WouldBlock の場合は readable() で待機してリトライ。
#[cfg(feature = "ktls")]
async fn async_raw_read(stream: &TcpStream, buf: &mut [u8]) -> io::Result<usize> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    
    loop {
        match raw_read(fd, buf) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // 読み取り可能になるまで待機
                stream.readable(false).await?;
            }
            Err(e) => return Err(e),
        }
    }
}

/// 非同期 raw write（TcpStream へ FD 経由で書き込み）
/// 
/// monoio の所有権ベース I/O を回避し、libc::write を直接使用。
/// WouldBlock の場合は writable() で待機してリトライ。
#[cfg(feature = "ktls")]
async fn async_raw_write(stream: &TcpStream, buf: &[u8]) -> io::Result<usize> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    let mut written = 0;
    
    while written < buf.len() {
        match raw_write(fd, &buf[written..]) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0")),
            Ok(n) => written += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // 書き込み可能になるまで待機
                stream.writable(false).await?;
            }
            Err(e) => return Err(e),
        }
    }
    
    Ok(written)
}

/// 非同期 raw write all（全バイト書き込み完了まで）
#[cfg(feature = "ktls")]
async fn async_raw_write_all(stream: &TcpStream, buf: &[u8]) -> io::Result<()> {
    let written = async_raw_write(stream, buf).await?;
    if written < buf.len() {
        Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write all bytes"))
    } else {
        Ok(())
    }
}

// ====================
// バッファプール（パフォーマンス最適化版）
// ====================
//
// 注意: monoioのAsyncWriteRentExtはバッファの所有権を取るため、
// 完全なゼロコピーは実現できません。バッファプールによりアロケーション
// コストを削減していますが、Arc<Vec<u8>>からのコピーは避けられません。
//
// ## パフォーマンス最適化: ゼロ埋め削除
//
// バッファの再利用時にゼロ埋め（memset）を行わず、`set_len()` のみを使用。
// これにより64KB × N回のmemsetコストを完全に削除しています。
//
// ## セキュリティ保証（SafeReadBuffer による型レベル保護）
//
// SafeReadBuffer ラッパーにより、未初期化メモリへのアクセスを
// 型システムで防止しています。
//
// - `as_valid_slice()` は読み込まれたデータのみを返す
// - `buf.len()` の誤用によるセキュリティリスクを排除
// - Heartbleed類似の脆弱性を構造的に防止
//
// ====================

thread_local! {
    /// スレッドローカルバッファプール
    /// 
    /// 内部では Vec<u8> を保持し、取得時に SafeReadBuffer でラップします。
    /// これにより、既存のメモリ効率を維持しながら型安全性を向上させています。
    #[allow(clippy::uninit_vec)]
    static BUF_POOL: RefCell<Vec<Vec<u8>>> = RefCell::new(
        (0..32).map(|_| {
            let mut buf = Vec::with_capacity(BUF_SIZE);
            // SAFETY: SafeReadBuffer でラップされるため、
            // valid_len 経由でしかアクセスできない
            unsafe {
                buf.set_len(BUF_SIZE);
            }
            buf
        }).collect()
    );
}

/// 安全なバッファ取得ヘルパー
/// 
/// プールから SafeReadBuffer を取得します。
/// 取得されたバッファは valid_len = 0 で初期化されており、
/// io_uring読み込み完了後に set_valid_len() で有効長を設定します。
/// 
/// # 使用例
/// 
/// ```rust,ignore
/// let read_buf = buf_get();
/// let (res, mut returned_buf) = stream.read(read_buf).await;
/// if let Ok(n) = res {
///     returned_buf.set_valid_len(n);
///     // 安全なアクセス：有効データのみが返される
///     accumulated.extend_from_slice(returned_buf.as_valid_slice());
/// }
/// buf_put(returned_buf);
/// ```
#[inline(always)]
fn buf_get() -> SafeReadBuffer {
    BUF_POOL.with(|p| {
        p.borrow_mut().pop()
            .map(|v| SafeReadBuffer::from_vec(v, BUF_SIZE))
            .unwrap_or_else(|| SafeReadBuffer::new(BUF_SIZE))
    })
}

/// バッファ返却ヘルパー（SafeReadBuffer版）
/// 
/// SafeReadBuffer をプールに返却します。
/// 内部の Vec<u8> を取り出してプールに格納します。
/// 
/// # セキュリティ
/// 
/// 返却されたバッファは次回取得時に SafeReadBuffer でラップされるため、
/// 以前のデータが漏洩することはありません（valid_len = 0 で初期化）。
#[inline(always)]
fn buf_put(buf: SafeReadBuffer) {
    buf_put_vec(buf.into_inner());
}

/// バッファ返却ヘルパー（Vec<u8>版、書き込み後の返却用）
/// 
/// 書き込み操作で使用された Vec<u8> をプールに返却します。
/// 主に `into_truncated()` 後の書き込み完了時に使用されます。
#[inline(always)]
#[allow(clippy::uninit_vec)]
fn buf_put_vec(mut buf: Vec<u8>) {
    BUF_POOL.with(|p| {
        let mut pool = p.borrow_mut();
        if pool.len() < 128 {
            // バッファの容量が十分であることを確認
            if buf.capacity() >= BUF_SIZE {
                // SAFETY: 
                // - capacity() >= BUF_SIZE を事前に確認済み
                // - 次回取得時は SafeReadBuffer でラップされる
                unsafe {
                    buf.set_len(BUF_SIZE);
                }
            } else {
                // 容量が足りない場合は新規作成（通常は発生しない）
                buf = Vec::with_capacity(BUF_SIZE);
                unsafe { buf.set_len(BUF_SIZE); }
            }
            pool.push(buf);
        }
    });
}

// ====================
// 設定構造体
// ====================

/// Upstream サーバーエントリ（文字列または構造体）
/// 
/// 以下の2つの形式をサポート:
/// - 文字列形式: "http://localhost:8080"
/// - 構造体形式: { url = "https://192.168.1.100:443", sni_name = "api.example.com" }
#[derive(Clone, Debug)]
struct UpstreamServerEntry {
    url: String,
    sni_name: Option<String>,
}

impl<'de> serde::Deserialize<'de> for UpstreamServerEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{MapAccess, Visitor};
        
        struct UpstreamServerEntryVisitor;
        
        impl<'de> Visitor<'de> for UpstreamServerEntryVisitor {
            type Value = UpstreamServerEntry;
            
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a string URL or an object with 'url' and optional 'sni_name'")
            }
            
            // 文字列形式: "http://localhost:8080"
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(UpstreamServerEntry {
                    url: v.to_string(),
                    sni_name: None,
                })
            }
            
            // 構造体形式: { url = "...", sni_name = "..." }
            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut url: Option<String> = None;
                let mut sni_name: Option<String> = None;
                
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "url" => url = Some(map.next_value()?),
                        "sni_name" => sni_name = Some(map.next_value()?),
                        _ => { let _: serde::de::IgnoredAny = map.next_value()?; }
                    }
                }
                
                let url = url.ok_or_else(|| serde::de::Error::missing_field("url"))?;
                Ok(UpstreamServerEntry { url, sni_name })
            }
        }
        
        deserializer.deserialize_any(UpstreamServerEntryVisitor)
    }
}

/// Upstream 設定（ロードバランシング用）
#[derive(Deserialize, Clone, Debug)]
struct UpstreamConfig {
    /// ロードバランシングアルゴリズム
    /// - "round_robin": ラウンドロビン（デフォルト）
    /// - "least_conn": Least Connections
    /// - "ip_hash": クライアントIPハッシュ
    #[serde(default)]
    algorithm: LoadBalanceAlgorithm,
    /// バックエンドサーバーエントリ一覧
    /// 文字列形式と構造体形式の両方をサポート
    servers: Vec<UpstreamServerEntry>,
    /// 健康チェック設定（オプション）
    #[serde(default)]
    health_check: Option<HealthCheckConfig>,
}

/// 健康チェック設定
#[derive(Deserialize, Clone, Debug)]
struct HealthCheckConfig {
    /// チェック間隔（秒）
    #[serde(default = "default_health_check_interval")]
    interval_secs: u64,
    /// チェック対象パス
    #[serde(default = "default_health_check_path")]
    path: String,
    /// タイムアウト（秒）
    #[serde(default = "default_health_check_timeout")]
    timeout_secs: u64,
    /// 成功と判断するHTTPステータスコード（デフォルト: 200-399）
    #[serde(default = "default_healthy_statuses")]
    healthy_statuses: Vec<u16>,
    /// 何回連続で失敗したら unhealthy とするか
    #[serde(default = "default_unhealthy_threshold")]
    unhealthy_threshold: u32,
    /// 何回連続で成功したら healthy に戻すか
    #[serde(default = "default_healthy_threshold")]
    healthy_threshold: u32,
}

fn default_health_check_interval() -> u64 { 10 }
fn default_health_check_path() -> String { "/".to_string() }
fn default_health_check_timeout() -> u64 { 5 }
fn default_healthy_statuses() -> Vec<u16> { vec![200, 201, 202, 204, 301, 302, 304] }
fn default_unhealthy_threshold() -> u32 { 3 }
fn default_healthy_threshold() -> u32 { 2 }

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            interval_secs: default_health_check_interval(),
            path: default_health_check_path(),
            timeout_secs: default_health_check_timeout(),
            healthy_statuses: default_healthy_statuses(),
            unhealthy_threshold: default_unhealthy_threshold(),
            healthy_threshold: default_healthy_threshold(),
        }
    }
}

#[derive(Deserialize)]
struct Config {
    server: ServerConfigSection,
    tls: TlsConfigSection,
    #[serde(default)]
    performance: PerformanceConfigSection,
    /// グローバルセキュリティ設定（権限降格など）
    #[serde(default)]
    security: GlobalSecurityConfig,
    /// ログ設定（非同期ログの最適化）
    #[serde(default)]
    logging: LoggingConfigSection,
    /// Prometheusメトリクス設定
    #[serde(default)]
    prometheus: PrometheusConfig,
    /// HTTP/2 設定セクション
    #[serde(default)]
    http2: Http2ConfigSection,
    /// HTTP/3 設定セクション
    #[serde(default)]
    http3: Http3ConfigSection,
    /// Upstream グループ定義（ロードバランシング用）
    #[serde(default)]
    upstreams: Option<HashMap<String, UpstreamConfig>>,
    host_routes: Option<HashMap<String, BackendConfig>>,
    path_routes: Option<HashMap<String, HashMap<String, BackendConfig>>>,
}

// ====================
// HTTP/2 設定セクション (RFC 7540)
// ====================

/// HTTP/2 詳細設定
/// 
/// HTTP/2 プロトコルのパラメータを設定します。
/// 有効化は `server.http2_enabled` で行います。
#[derive(Deserialize, Clone)]
pub struct Http2ConfigSection {
    /// SETTINGS_HEADER_TABLE_SIZE (HPACK動的テーブルサイズ)
    /// デフォルト: 4096 (4KB)
    /// 高パフォーマンス: 65536 (64KB)
    #[serde(default = "default_h2_header_table_size")]
    pub header_table_size: u32,
    
    /// SETTINGS_MAX_CONCURRENT_STREAMS (同時ストリーム数)
    /// デフォルト: 100
    #[serde(default = "default_h2_max_concurrent_streams")]
    pub max_concurrent_streams: u32,
    
    /// SETTINGS_INITIAL_WINDOW_SIZE (ストリームウィンドウサイズ)
    /// デフォルト: 65535 (64KB - 1)
    #[serde(default = "default_h2_initial_window_size")]
    pub initial_window_size: u32,
    
    /// SETTINGS_MAX_FRAME_SIZE (最大フレームサイズ)
    /// デフォルト: 16384 (16KB)
    #[serde(default = "default_h2_max_frame_size")]
    pub max_frame_size: u32,
    
    /// SETTINGS_MAX_HEADER_LIST_SIZE (最大ヘッダーリストサイズ)
    /// デフォルト: 16384 (16KB)
    #[serde(default = "default_h2_max_header_list_size")]
    pub max_header_list_size: u32,
    
    /// コネクションウィンドウサイズ（コネクション全体のフロー制御）
    /// デフォルト: 65535 (64KB - 1)
    #[serde(default = "default_h2_connection_window_size")]
    pub connection_window_size: u32,
}

// HTTP/2 設定のデフォルト値（high_performance と同等）
// HPACK動的テーブルサイズを大きくすることで、ヘッダー圧縮効率が向上
fn default_h2_header_table_size() -> u32 { 65536 }      // 64KB (より多くのヘッダーをキャッシュ)
fn default_h2_max_concurrent_streams() -> u32 { 256 }   // より多くの同時ストリーム
fn default_h2_initial_window_size() -> u32 { 1048576 }  // 1MB (より大きなウィンドウ)
fn default_h2_max_frame_size() -> u32 { 65536 }         // 64KB (より大きなフレーム)
fn default_h2_max_header_list_size() -> u32 { 65536 }   // 64KB
fn default_h2_connection_window_size() -> u32 { 1048576 } // 1MB

impl Default for Http2ConfigSection {
    fn default() -> Self {
        Self {
            header_table_size: default_h2_header_table_size(),
            max_concurrent_streams: default_h2_max_concurrent_streams(),
            initial_window_size: default_h2_initial_window_size(),
            max_frame_size: default_h2_max_frame_size(),
            max_header_list_size: default_h2_max_header_list_size(),
            connection_window_size: default_h2_connection_window_size(),
        }
    }
}

impl Http2ConfigSection {
    /// HTTP/2 設定を Http2Settings に変換
    #[cfg(feature = "http2")]
    pub fn to_http2_settings(&self) -> http2::Http2Settings {
        http2::Http2Settings {
            header_table_size: self.header_table_size,
            max_concurrent_streams: self.max_concurrent_streams,
            initial_window_size: self.initial_window_size,
            max_frame_size: self.max_frame_size,
            max_header_list_size: self.max_header_list_size,
            enable_push: false, // サーバーではpush無効
            connection_window_size: self.connection_window_size,
        }
    }
}

// ====================
// HTTP/3 設定セクション (RFC 9114, QUIC RFC 9000)
// ====================

/// HTTP/3 専用圧縮設定
/// 
/// HTTP/3接続時に使用する圧縮パラメータを設定します。
/// 未設定のフィールドはパスごとの設定または全体設定を継承します。
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Http3CompressionConfig {
    /// 圧縮方式の優先順位
    /// サポート: "zstd", "br" (Brotli), "gzip", "deflate"
    /// 未設定時はパスごとの設定を使用
    pub preferred_encodings: Option<Vec<String>>,
    
    /// Gzip圧縮レベル (1-9)
    /// 未設定時はパスごとの設定を使用
    pub gzip_level: Option<u32>,
    
    /// Brotli圧縮レベル (0-11)
    /// 未設定時はパスごとの設定を使用
    pub brotli_level: Option<u32>,
    
    /// Zstd圧縮レベル (1-22)
    /// 未設定時はパスごとの設定を使用
    pub zstd_level: Option<i32>,
    
    /// 最小圧縮サイズ（バイト）
    /// 未設定時はパスごとの設定を使用
    pub min_size: Option<usize>,
    
    /// 圧縮対象のMIMEタイプ（プレフィックスマッチ）
    /// 未設定時はパスごとの設定を使用
    pub compressible_types: Option<Vec<String>>,
    
    /// 圧縮をスキップするMIMEタイプ（プレフィックスマッチ）
    /// 未設定時はパスごとの設定を使用
    pub skip_types: Option<Vec<String>>,
}

impl Http3CompressionConfig {
    /// 設定の妥当性を検証
    pub fn validate(&self) -> Result<(), String> {
        if let Some(level) = self.gzip_level {
            if level < 1 || level > 9 {
                return Err(format!("http3.compression.gzip_level: {} (must be 1-9)", level));
            }
        }
        if let Some(level) = self.brotli_level {
            if level > 11 {
                return Err(format!("http3.compression.brotli_level: {} (must be 0-11)", level));
            }
        }
        if let Some(level) = self.zstd_level {
            if level < 1 || level > 22 {
                return Err(format!("http3.compression.zstd_level: {} (must be 1-22)", level));
            }
        }
        if let Some(ref encodings) = self.preferred_encodings {
            for enc in encodings {
                if !["zstd", "br", "gzip", "deflate"].contains(&enc.as_str()) {
                    return Err(format!("http3.compression.preferred_encodings: unknown encoding '{}'", enc));
                }
            }
        }
        Ok(())
    }
}

/// HTTP/3 詳細設定
/// 
/// HTTP/3 (QUIC) プロトコルのパラメータを設定します。
/// 有効化は `server.http3_enabled` で行います。
#[derive(Deserialize, Clone)]
pub struct Http3ConfigSection {
    /// HTTP/3リッスンアドレス（UDP）
    /// 未指定の場合は server.listen と同じアドレスを使用
    #[serde(default)]
    pub listen: Option<String>,
    
    /// 最大アイドルタイムアウト（ミリ秒）
    /// デフォルト: 30000 (30秒)
    #[serde(default = "default_h3_max_idle_timeout")]
    pub max_idle_timeout: u64,
    
    /// 最大UDPペイロードサイズ
    /// デフォルト: 1350 (MTU考慮)
    #[serde(default = "default_h3_max_udp_payload_size")]
    pub max_udp_payload_size: u64,
    
    /// 初期最大データサイズ（コネクション全体）
    /// デフォルト: 10000000 (10MB)
    #[serde(default = "default_h3_initial_max_data")]
    pub initial_max_data: u64,
    
    /// 初期最大ストリームデータサイズ（双方向ローカル）
    #[serde(default = "default_h3_initial_max_stream_data")]
    pub initial_max_stream_data_bidi_local: u64,
    
    /// 初期最大ストリームデータサイズ（双方向リモート）
    #[serde(default = "default_h3_initial_max_stream_data")]
    pub initial_max_stream_data_bidi_remote: u64,
    
    /// 初期最大ストリームデータサイズ（単方向）
    #[serde(default = "default_h3_initial_max_stream_data")]
    pub initial_max_stream_data_uni: u64,
    
    /// 初期最大双方向ストリーム数
    #[serde(default = "default_h3_max_streams")]
    pub initial_max_streams_bidi: u64,
    
    /// 初期最大単方向ストリーム数
    #[serde(default = "default_h3_max_streams")]
    pub initial_max_streams_uni: u64,
    
    /// HTTP/3接続時の圧縮を常に有効化
    /// デフォルト: false
    /// 
    /// true の場合、パスごとの設定で明示的に無効化されていない限り、
    /// すべてのHTTP/3レスポンスで圧縮を試みます。
    /// パスごとの compression.enabled = false の場合はそちらが優先されます。
    #[serde(default)]
    pub compression_enabled: bool,
    
    /// HTTP/3専用の圧縮パラメータ
    /// 
    /// パスごとの圧縮設定より優先されます。
    /// 未設定のフィールドはパスごとの設定を継承します。
    #[serde(default)]
    pub compression: Http3CompressionConfig,
}

fn default_h3_max_idle_timeout() -> u64 { 30000 }
fn default_h3_max_udp_payload_size() -> u64 { 1350 }
fn default_h3_initial_max_data() -> u64 { 10_000_000 }
fn default_h3_initial_max_stream_data() -> u64 { 1_000_000 }
fn default_h3_max_streams() -> u64 { 100 }

impl Default for Http3ConfigSection {
    fn default() -> Self {
        Self {
            listen: None,
            max_idle_timeout: default_h3_max_idle_timeout(),
            max_udp_payload_size: default_h3_max_udp_payload_size(),
            initial_max_data: default_h3_initial_max_data(),
            initial_max_stream_data_bidi_local: default_h3_initial_max_stream_data(),
            initial_max_stream_data_bidi_remote: default_h3_initial_max_stream_data(),
            initial_max_stream_data_uni: default_h3_initial_max_stream_data(),
            initial_max_streams_bidi: default_h3_max_streams(),
            initial_max_streams_uni: default_h3_max_streams(),
            compression_enabled: false,
            compression: Http3CompressionConfig::default(),
        }
    }
}

#[cfg(feature = "http3")]
impl Http3ConfigSection {
    /// HTTP/3 設定を Http3ServerConfig に変換
    pub fn to_http3_config(&self, cert_path: &str, key_path: &str) -> http3_server::Http3ServerConfig {
        http3_server::Http3ServerConfig {
            cert_path: cert_path.to_string(),
            key_path: key_path.to_string(),
            cert_pem: None,  // quicheはファイルパスからの読み込みのみサポート
            key_pem: None,
            max_idle_timeout: self.max_idle_timeout,
            max_udp_payload_size: self.max_udp_payload_size,
            initial_max_data: self.initial_max_data,
            initial_max_stream_data_bidi_local: self.initial_max_stream_data_bidi_local,
            initial_max_stream_data_bidi_remote: self.initial_max_stream_data_bidi_remote,
            initial_max_stream_data_uni: self.initial_max_stream_data_uni,
            initial_max_streams_bidi: self.initial_max_streams_bidi,
            initial_max_streams_uni: self.initial_max_streams_uni,
        }
    }
}

#[derive(Deserialize)]
struct ServerConfigSection {
    listen: String,
    /// HTTPリスナーアドレス（オプション）
    /// 
    /// 指定した場合、HTTPアクセスをHTTPSにリダイレクトするリスナーを起動します。
    /// 例: "0.0.0.0:80"
    /// 
    /// リダイレクトのみを行い、コンテンツは配信しません（セキュリティ考慮）。
    #[serde(default)]
    http: Option<String>,
    /// ワーカースレッド数
    /// 未指定または0の場合はCPUコア数と同じスレッド数を使用
    #[serde(default)]
    threads: Option<usize>,
    
    // ====================
    // HTTP/2・HTTP/3 設定
    // ====================
    
    /// HTTP/2 を有効化するかどうか
    /// 
    /// TLS ALPN ネゴシエーションにより HTTP/2 (h2) をサポートします。
    /// HTTP/1.1 へのフォールバックも可能です。
    /// 
    /// 効果:
    /// - ストリーム多重化によるレイテンシ削減
    /// - HPACK ヘッダー圧縮によるオーバーヘッド削減
    /// - サーバープッシュ（無効化推奨）
    /// 
    /// 注意: `--features http2` でビルドする必要があります
    #[serde(default)]
    http2_enabled: bool,
    
    /// HTTP/3 を有効化するかどうか
    /// 
    /// QUIC/UDP ベースの HTTP/3 プロトコルをサポートします。
    /// 
    /// 効果:
    /// - 0-RTT 接続確立
    /// - 接続マイグレーション
    /// - Head-of-Line ブロッキング解消
    /// 
    /// 注意: 
    /// - `--features http3` でビルドする必要があります
    /// - HTTP/3 は UDP ベースのため kTLS は使用不可
    /// - GSO/GRO による高パフォーマンス UDP 処理を使用
    /// - リッスンアドレスは [http3].listen で設定
    #[serde(default)]
    http3_enabled: bool,
}

#[derive(Deserialize)]
struct TlsConfigSection {
    cert_path: String,
    key_path: String,
    /// kTLSを有効化するかどうか（Linux 5.15+、modprobe tls 必須）
    /// 
    /// kTLS有効化時の効果:
    /// - TLSデータ転送フェーズでカーネルオフロード
    /// - sendfileでゼロコピー送信（TLS暗号化済み）
    /// - 高負荷時にCPU 20-40%節約、スループット最大2倍
    /// 
    /// 注意事項:
    /// - TLSハンドシェイクはrustlsで実行（セキュリティ維持）
    /// - AES-GCM暗号スイートのみサポート
    /// - カーネルバグの影響範囲に注意
    #[serde(default)]
    ktls_enabled: bool,
    /// kTLS有効化失敗時にrustlsへフォールバックするかどうか
    /// 
    /// - false: kTLS必須モード（失敗時は接続拒否）
    /// - true: kTLS失敗時はrustlsで継続（デフォルト）
    /// 
    /// フォールバック無効化のメリット:
    /// - パフォーマンス予測可能性（確実にkTLSを使用）
    /// - デバッグ容易性（kTLS/rustls混在なし）
    /// - 環境問題の早期発見
    #[serde(default = "default_ktls_fallback")]
    ktls_fallback_enabled: bool,
}

/// kTLSフォールバックのデフォルト値（true = フォールバック有効）
fn default_ktls_fallback() -> bool {
    true
}

/// パフォーマンス設定
#[derive(Deserialize, Clone, Default)]
struct PerformanceConfigSection {
    /// SO_REUSEPORTの振り分け方式
    /// - "kernel": カーネルデフォルト（3元タプルハッシュ）
    /// - "cbpf": クライアントIPベースのCBPF振り分け（Linux 4.6+必須）
    #[serde(default)]
    reuseport_balancing: ReuseportBalancing,
    /// Huge Pages (Large OS Pages) の使用
    /// 
    /// mimallocでHuge Pages（2MB）を優先使用し、TLBミスを削減します。
    /// 
    /// 効果:
    /// - TLB（Translation Lookaside Buffer）ミス削減
    /// - 大容量メモリ使用時のページフォルト減少
    /// - kTLS/splice時のカーネル連携で5-10%パフォーマンス向上
    /// 
    /// 要件（Linux）:
    /// - /proc/sys/vm/nr_hugepages に十分な値を設定
    /// - コンテナ環境では追加設定が必要な場合あり
    #[serde(default)]
    huge_pages_enabled: bool,
}

// ====================
// ログ設定
// ====================
//
// ftlogは内部でバックグラウンドスレッドとチャネルを使用した
// 非同期ログライブラリです。以下の設定で最適化が可能です。
//
// ## grokの指摘に対する検証結果
// 
// grokは「ftlogは同期ログ」と主張していましたが、これは不正確です。
// ftlogは以下の非同期アーキテクチャを使用しています：
// - ログマクロ → 内部チャネルにプッシュ（ノンブロッキング）
// - バックグラウンドスレッド → チャネルから読み取りファイルI/O
//
// したがって、tokio::sync::mpscを使った追加の非同期化層は不要であり、
// むしろオーバーヘッドを増やす可能性があります。
//
// ## 推奨される最適化
// - channel_size: 高負荷時のバックプレッシャーを軽減
// - flush_interval_ms: ディスクI/O頻度の調整
// - level: 本番環境ではinfo以上を推奨
// ====================

/// ログ出力形式
#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
enum LogFormat {
    /// テキスト形式（デフォルト）
    #[default]
    Text,
    /// JSON形式
    Json,
}

/// ログ設定セクション
#[derive(Deserialize, Clone, Debug)]
struct LoggingConfigSection {
    /// ログレベル
    /// - "trace": 全てのログ（開発/デバッグ用）
    /// - "debug": デバッグ情報
    /// - "info": 一般情報（デフォルト）
    /// - "warn": 警告のみ
    /// - "error": エラーのみ
    #[serde(default = "default_log_level")]
    level: String,
    
    /// ログ出力形式
    /// - "text": テキスト形式（デフォルト）
    /// - "json": JSON形式
    #[serde(default)]
    format: LogFormat,
    
    /// ログチャネルサイズ
    /// 
    /// ftlog内部のチャネルバッファサイズです。
    /// 高負荷時のバックプレッシャーを軽減するために大きな値を設定します。
    /// 
    /// デフォルト: 100000
    /// 推奨範囲: 10000 - 1000000
    #[serde(default = "default_channel_size")]
    channel_size: usize,
    
    /// フラッシュ間隔（ミリ秒）
    /// 
    /// ログバッファをファイルにフラッシュする間隔です。
    /// 小さい値: 即座にログが書き込まれるがI/O負荷増
    /// 大きい値: I/O効率が良いがログ遅延
    /// 
    /// デフォルト: 1000 (1秒)
    /// 推奨範囲: 100 - 5000
    #[serde(default = "default_flush_interval")]
    flush_interval_ms: u64,
    
    /// 最大ログファイルサイズ（バイト）
    /// 
    /// ログファイルの最大サイズ。超過すると新しいファイルに切り替え。
    /// 0の場合はローテーションなし。
    /// 
    /// 注意: ftlogは現在日次ローテーションのみをサポート。
    /// サイズベースローテーションは将来的な拡張で対応予定。
    /// 
    /// デフォルト: 104857600 (100MB)
    #[serde(default = "default_max_log_size")]
    #[allow(dead_code)]
    max_log_size: u64,
    
    /// ログファイルパス
    /// 
    /// ログファイルの出力先パス。
    /// 指定しない場合は標準エラー出力に出力。
    #[serde(default)]
    file_path: Option<String>,
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_channel_size() -> usize {
    100000  // ftlogデフォルト(100)より大幅に増加し、高負荷時のドロップを防止
}

fn default_flush_interval() -> u64 {
    1000  // 1秒
}

fn default_max_log_size() -> u64 {
    104857600  // 100MB
}

impl Default for LoggingConfigSection {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: LogFormat::default(),
            channel_size: default_channel_size(),
            flush_interval_ms: default_flush_interval(),
            max_log_size: default_max_log_size(),
            file_path: None,
        }
    }
}

/// ログレベル文字列をLevelFilterに変換
fn parse_log_level(level: &str) -> LevelFilter {
    match level.to_lowercase().as_str() {
        "trace" => LevelFilter::Trace,
        "debug" => LevelFilter::Debug,
        "info" => LevelFilter::Info,
        "warn" | "warning" => LevelFilter::Warn,
        "error" => LevelFilter::Error,
        "off" => LevelFilter::Off,
        _ => LevelFilter::Info,
    }
}

/// SO_REUSEPORTの振り分け方式
#[derive(Clone, Copy, Debug, PartialEq, Default)]
enum ReuseportBalancing {
    /// カーネルデフォルト（3元タプルハッシュ: protocol + source IP + source port）
    #[default]
    Kernel,
    /// クライアントIPベースのCBPF振り分け
    /// 同一クライアントIPからの接続を常に同じワーカースレッドに振り分け
    /// CPUキャッシュ効率とセッション再開効率を向上
    Cbpf,
}

impl<'de> serde::Deserialize<'de> for ReuseportBalancing {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "kernel" => Ok(ReuseportBalancing::Kernel),
            "cbpf" => Ok(ReuseportBalancing::Cbpf),
            other => Err(serde::de::Error::custom(format!(
                "unknown reuseport_balancing value: '{}', expected 'kernel' or 'cbpf'",
                other
            ))),
        }
    }
}

#[derive(Clone)]
enum BackendConfig {
    /// 単一URLプロキシ（後方互換性のため維持）
    /// - sni_name: TLS接続時のSNI名（IP直打ち時にドメイン名を指定可能）
    /// - use_h2c: H2C (HTTP/2 over cleartext) を使用するかどうか
    /// - compression: 圧縮設定（オプション）
    /// - buffering: バッファリング設定（オプション）
    /// - cache: キャッシュ設定（オプション）
    Proxy { 
        url: String, 
        sni_name: Option<String>, 
        use_h2c: bool, 
        security: SecurityConfig, 
        compression: CompressionConfig,
        buffering: buffering::BufferingConfig,
        cache: cache::CacheConfig,
    },
    /// Upstream グループ参照（ロードバランシング用）
    /// - compression: 圧縮設定（オプション）
    /// - buffering: バッファリング設定（オプション）
    /// - cache: キャッシュ設定（オプション）
    ProxyUpstream { 
        upstream: String, 
        security: SecurityConfig, 
        compression: CompressionConfig,
        buffering: buffering::BufferingConfig,
        cache: cache::CacheConfig,
    },
    /// File バックエンド設定
    /// - path: ファイルまたはディレクトリのパス
    /// - mode: "sendfile" または "memory"
    /// - index: ディレクトリアクセス時に返すファイル名（デフォルト: "index.html"）
    /// - security: ルートごとのセキュリティ設定
    /// - cache: キャッシュ設定（オプション、静的ファイルのキャッシュ用）
    File { 
        path: String, 
        mode: String, 
        index: Option<String>, 
        security: SecurityConfig,
        cache: cache::CacheConfig,
    },
    /// Redirect バックエンド設定
    /// - redirect_url: リダイレクト先URL（$request_uri, $host, $path 変数使用可能）
    /// - redirect_status: ステータスコード（301, 302, 307, 308）
    /// - preserve_path: 元のパスをリダイレクト先に追加するか
    Redirect { redirect_url: String, redirect_status: u16, preserve_path: bool },
}

impl<'de> serde::Deserialize<'de> for BackendConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{MapAccess, Visitor};
        
        struct BackendConfigVisitor;
        
        impl<'de> Visitor<'de> for BackendConfigVisitor {
            type Value = BackendConfig;
            
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a backend configuration object")
            }
            
            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut backend_type: Option<String> = None;
                let mut url: Option<String> = None;
                let mut upstream: Option<String> = None;
                let mut path: Option<String> = None;
                let mut mode: Option<String> = None;
                let mut index: Option<String> = None;
                let mut security: Option<SecurityConfig> = None;
                // Redirect 用フィールド
                let mut redirect_url: Option<String> = None;
                let mut redirect_status: Option<u16> = None;
                let mut preserve_path: Option<bool> = None;
                // SNI 用フィールド（Proxy用）
                let mut sni_name: Option<String> = None;
                // H2C 用フィールド（Proxy用）
                let mut use_h2c: Option<bool> = None;
                // 圧縮設定（Proxy用）
                let mut compression: Option<CompressionConfig> = None;
                // バッファリング設定（Proxy用）
                let mut buffering_config: Option<buffering::BufferingConfig> = None;
                // キャッシュ設定（Proxy/File用）
                let mut cache_config: Option<cache::CacheConfig> = None;
                
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "type" => backend_type = Some(map.next_value()?),
                        "url" => url = Some(map.next_value()?),
                        "upstream" => upstream = Some(map.next_value()?),
                        "path" => path = Some(map.next_value()?),
                        "mode" => mode = Some(map.next_value()?),
                        "index" => index = Some(map.next_value()?),
                        "security" => security = Some(map.next_value()?),
                        "compression" => compression = Some(map.next_value()?),
                        "buffering" => buffering_config = Some(map.next_value()?),
                        "cache" => cache_config = Some(map.next_value()?),
                        "redirect_url" => redirect_url = Some(map.next_value()?),
                        "redirect_status" => redirect_status = Some(map.next_value()?),
                        "preserve_path" => preserve_path = Some(map.next_value()?),
                        "sni_name" => sni_name = Some(map.next_value()?),
                        "use_h2c" | "h2c" => use_h2c = Some(map.next_value()?),
                        _ => { let _: serde::de::IgnoredAny = map.next_value()?; }
                    }
                }
                
                let backend_type = backend_type.unwrap_or_else(|| "File".to_string());
                let security = security.unwrap_or_default();
                let compression = compression.unwrap_or_default();
                let buffering = buffering_config.unwrap_or_default();
                let cache = cache_config.unwrap_or_default();
                
                match backend_type.as_str() {
                    "Proxy" => {
                        // upstream が指定されている場合はロードバランシング用
                        if let Some(upstream_name) = upstream {
                            Ok(BackendConfig::ProxyUpstream { 
                                upstream: upstream_name, 
                                security, 
                                compression,
                                buffering,
                                cache,
                            })
                        } else {
                            let url = url.ok_or_else(|| serde::de::Error::missing_field("url or upstream"))?;
                            let use_h2c = use_h2c.unwrap_or(false);
                            Ok(BackendConfig::Proxy { 
                                url, 
                                sni_name, 
                                use_h2c, 
                                security, 
                                compression,
                                buffering,
                                cache,
                            })
                        }
                    }
                    "Redirect" => {
                        let redirect_url = redirect_url.ok_or_else(|| serde::de::Error::missing_field("redirect_url"))?;
                        let redirect_status = redirect_status.unwrap_or(301);
                        // ステータスコードの検証（301, 302, 303, 307, 308のみ許可）
                        if !matches!(redirect_status, 301 | 302 | 303 | 307 | 308) {
                            return Err(serde::de::Error::custom(format!(
                                "invalid redirect_status: {}, expected 301, 302, 303, 307, or 308",
                                redirect_status
                            )));
                        }
                        let preserve_path = preserve_path.unwrap_or(false);
                        Ok(BackendConfig::Redirect { redirect_url, redirect_status, preserve_path })
                    }
                    "File" | _ => {
                        let path = path.ok_or_else(|| serde::de::Error::missing_field("path"))?;
                        let mode = mode.unwrap_or_else(|| "sendfile".to_string());
                        Ok(BackendConfig::File { path, mode, index, security, cache })
                    }
                }
            }
        }
        
        deserializer.deserialize_map(BackendConfigVisitor)
    }
}

// ====================
// ランタイムBackend
// ====================

#[derive(Clone)]
enum Backend {
    /// Proxy バックエンド（ロードバランシング対応）
    /// - Arc<UpstreamGroup>: アップストリームグループ（単一または複数バックエンド）
    /// - Arc<SecurityConfig>: ルートごとのセキュリティ設定
    /// - Arc<CompressionConfig>: 圧縮設定
    /// - Arc<buffering::BufferingConfig>: バッファリング設定
    /// - Arc<cache::CacheConfig>: キャッシュ設定
    Proxy(
        Arc<UpstreamGroup>, 
        Arc<SecurityConfig>, 
        Arc<CompressionConfig>,
        Arc<buffering::BufferingConfig>,
        Arc<cache::CacheConfig>,
    ),
    /// MemoryFile バックエンド
    /// - Arc<Vec<u8>>: ファイルコンテンツ
    /// - Arc<str>: MIMEタイプ
    /// - Arc<SecurityConfig>: ルートごとのセキュリティ設定
    MemoryFile(Arc<Vec<u8>>, Arc<str>, Arc<SecurityConfig>),
    /// SendFile バックエンド
    /// - Arc<PathBuf>: ベースパス
    /// - bool: ディレクトリかどうか
    /// - Option<Arc<str>>: インデックスファイル名（None = "index.html"）
    /// - Arc<SecurityConfig>: ルートごとのセキュリティ設定
    /// - Arc<cache::CacheConfig>: キャッシュ設定
    SendFile(Arc<PathBuf>, bool, Option<Arc<str>>, Arc<SecurityConfig>, Arc<cache::CacheConfig>),
    /// Redirect バックエンド
    /// - Arc<str>: リダイレクト先URL
    /// - u16: ステータスコード（301, 302, 307, 308）
    /// - bool: 元のパスを保持するか
    Redirect(Arc<str>, u16, bool),
}

impl Backend {
    /// このバックエンドのセキュリティ設定を取得
    #[inline]
    fn security(&self) -> &SecurityConfig {
        // デフォルトのセキュリティ設定（Redirect用）
        static DEFAULT_SECURITY: Lazy<SecurityConfig> = Lazy::new(SecurityConfig::default);
        
        match self {
            Backend::Proxy(_, security, _, _, _) => security,
            Backend::MemoryFile(_, _, security) => security,
            Backend::SendFile(_, _, _, security, _) => security,
            Backend::Redirect(_, _, _) => &DEFAULT_SECURITY,
        }
    }
    
    /// このバックエンドのバッファリング設定を取得
    #[inline]
    #[allow(dead_code)]
    fn buffering(&self) -> Option<&buffering::BufferingConfig> {
        match self {
            Backend::Proxy(_, _, _, buffering, _) => Some(buffering),
            _ => None,
        }
    }
    
    /// このバックエンドのキャッシュ設定を取得
    #[inline]
    #[allow(dead_code)]
    fn cache(&self) -> Option<&cache::CacheConfig> {
        match self {
            Backend::Proxy(_, _, _, _, cache) => Some(cache),
            Backend::SendFile(_, _, _, _, cache) => Some(cache),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct ProxyTarget {
    host: String,
    port: u16,
    use_tls: bool,
    path_prefix: String,
    /// SNI (Server Name Indication) に使用するホスト名
    /// Noneの場合はhostを使用。IP直打ちの場合にドメイン名を指定可能
    sni_name: Option<String>,
    /// H2C (HTTP/2 over cleartext) を使用するかどうか
    /// true の場合、非TLSバックエンドにHTTP/2で接続
    /// HTTP/2 Upgrade 経由ではなく、Prior Knowledge モードを使用
    use_h2c: bool,
}

impl ProxyTarget {
    fn parse(url: &str) -> Option<Self> {
        let (scheme, rest) = if url.starts_with("https://") {
            (true, &url[8..])
        } else if url.starts_with("http://") {
            (false, &url[7..])
        } else {
            return None;
        };

        let (host_port, path) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, "/"),
        };

        let (host, port) = match host_port.find(':') {
            Some(idx) => {
                let h = &host_port[..idx];
                let p = host_port[idx + 1..].parse().ok()?;
                (h.to_string(), p)
            }
            None => (host_port.to_string(), if scheme { 443 } else { 80 }),
        };

        Some(ProxyTarget {
            host,
            port,
            use_tls: scheme,
            path_prefix: path.to_string(),
            sni_name: None,
            use_h2c: false, // デフォルトでは無効
        })
    }
    
    /// SNI名を設定したコピーを作成
    fn with_sni_name(mut self, sni_name: Option<String>) -> Self {
        self.sni_name = sni_name;
        self
    }
    
    /// H2C設定を変更したコピーを作成
    fn with_h2c(mut self, use_h2c: bool) -> Self {
        // H2Cは非TLSの場合のみ有効
        if !self.use_tls {
            self.use_h2c = use_h2c;
        }
        self
    }
    
    /// TLS接続時に使用するSNI名を取得
    #[inline]
    fn sni(&self) -> &str {
        self.sni_name.as_deref().unwrap_or(&self.host)
    }
    
    /// デフォルトポートかどうかを判定
    #[inline]
    fn is_default_port(&self) -> bool {
        if self.use_tls {
            self.port == 443
        } else {
            self.port == 80
        }
    }
}

// ====================
// ロードバランシング（Upstream Group）
// ====================
//
// 複数のバックエンドサーバーへのリクエスト分散をサポートします。
//
// ## サポートするアルゴリズム
// - RoundRobin: 順番に振り分け（デフォルト）
// - LeastConnections: 接続数が最も少ないサーバーを選択
// - IpHash: クライアントIPに基づいて一貫したサーバーを選択
//
// ## 設定例
// ```toml
// [upstreams."backend-pool"]
// algorithm = "round_robin"
// servers = ["http://localhost:8080", "http://localhost:8081"]
//
// [path_routes."example.com"."/api/"]
// type = "Proxy"
// upstream = "backend-pool"
// ```
// ====================

/// ロードバランシングアルゴリズム
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum LoadBalanceAlgorithm {
    /// ラウンドロビン（順番に振り分け）
    #[default]
    RoundRobin,
    /// Least Connections（接続数が最も少ないサーバー）
    LeastConnections,
    /// IP Hash（クライアントIPに基づく一貫したルーティング）
    IpHash,
}

impl<'de> serde::Deserialize<'de> for LoadBalanceAlgorithm {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.to_lowercase().as_str() {
            "round_robin" | "roundrobin" => Ok(LoadBalanceAlgorithm::RoundRobin),
            "least_conn" | "least_connections" | "leastconn" => Ok(LoadBalanceAlgorithm::LeastConnections),
            "ip_hash" | "iphash" => Ok(LoadBalanceAlgorithm::IpHash),
            other => Err(serde::de::Error::custom(format!(
                "unknown load balance algorithm: '{}', expected 'round_robin', 'least_conn', or 'ip_hash'",
                other
            ))),
        }
    }
}

/// Upstream サーバーの状態
#[derive(Clone)]
struct UpstreamServer {
    /// バックエンドターゲット
    target: ProxyTarget,
    /// 現在のアクティブ接続数（Least Connections用）
    active_connections: Arc<AtomicUsize>,
    /// サーバーが利用可能かどうか（ヘルスチェック用）
    healthy: Arc<AtomicBool>,
    /// 連続成功回数（健康チェック用）
    consecutive_successes: Arc<AtomicUsize>,
    /// 連続失敗回数（健康チェック用）
    consecutive_failures: Arc<AtomicUsize>,
}

impl UpstreamServer {
    fn new(target: ProxyTarget) -> Self {
        Self {
            target,
            active_connections: Arc::new(AtomicUsize::new(0)),
            healthy: Arc::new(AtomicBool::new(true)),
            consecutive_successes: Arc::new(AtomicUsize::new(0)),
            consecutive_failures: Arc::new(AtomicUsize::new(0)),
        }
    }
    
    /// 接続カウンターを増加
    fn acquire(&self) {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
    }
    
    /// 接続カウンターを減少
    fn release(&self) {
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
    }
    
    /// 現在の接続数を取得
    fn connections(&self) -> usize {
        self.active_connections.load(Ordering::Relaxed)
    }
    
    /// サーバーが健全かどうか
    fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }
    
    /// 健康チェック成功を記録
    fn record_success(&self, healthy_threshold: u32) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        let successes = self.consecutive_successes.fetch_add(1, Ordering::Relaxed) + 1;
        
        // 閾値に達したら healthy に設定
        if successes >= healthy_threshold as usize && !self.is_healthy() {
            self.healthy.store(true, Ordering::SeqCst);
            info!("Upstream {}:{} is now healthy", self.target.host, self.target.port);
        }
    }
    
    /// 健康チェック失敗を記録
    fn record_failure(&self, unhealthy_threshold: u32) {
        self.consecutive_successes.store(0, Ordering::Relaxed);
        let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        
        // 閾値に達したら unhealthy に設定
        if failures >= unhealthy_threshold as usize && self.is_healthy() {
            self.healthy.store(false, Ordering::SeqCst);
            warn!("Upstream {}:{} is now unhealthy", self.target.host, self.target.port);
        }
    }
}

/// Upstream グループ（複数バックエンドのロードバランシング）
#[derive(Clone)]
pub struct UpstreamGroup {
    /// グループ名（ログ出力用）
    #[allow(dead_code)]
    name: String,
    /// バックエンドサーバーリスト
    servers: Vec<UpstreamServer>,
    /// ロードバランシングアルゴリズム
    algorithm: LoadBalanceAlgorithm,
    /// ラウンドロビン用カウンター
    rr_counter: Arc<AtomicUsize>,
    /// 健康チェック設定（オプション）
    health_check: Option<HealthCheckConfig>,
}

impl UpstreamGroup {
    /// 新しい Upstream グループを作成
    fn new(name: String, entries: Vec<UpstreamServerEntry>, algorithm: LoadBalanceAlgorithm, health_check: Option<HealthCheckConfig>) -> Option<Self> {
        let servers: Vec<UpstreamServer> = entries.iter()
            .filter_map(|entry| {
                ProxyTarget::parse(&entry.url)
                    .map(|target| target.with_sni_name(entry.sni_name.clone()))
                    .map(UpstreamServer::new)
            })
            .collect();
        
        if servers.is_empty() {
            return None;
        }
        
        Some(Self {
            name,
            servers,
            algorithm,
            rr_counter: Arc::new(AtomicUsize::new(0)),
            health_check,
        })
    }
    
    /// 単一サーバーからグループを作成
    fn single(target: ProxyTarget) -> Self {
        Self {
            name: String::new(),
            servers: vec![UpstreamServer::new(target)],
            algorithm: LoadBalanceAlgorithm::RoundRobin,
            rr_counter: Arc::new(AtomicUsize::new(0)),
            health_check: None, // 単一サーバーでは健康チェックなし
        }
    }
    
    /// 次のバックエンドサーバーを選択
    /// 
    /// # Arguments
    /// * `client_ip` - クライアントIPアドレス（IpHash用）
    /// 
    /// # Returns
    /// 選択されたサーバーへの参照（健全なサーバーがない場合は None）
    fn select(&self, client_ip: &str) -> Option<&UpstreamServer> {
        let healthy_servers: Vec<(usize, &UpstreamServer)> = self.servers.iter()
            .enumerate()
            .filter(|(_, s)| s.is_healthy())
            .collect();
        
        if healthy_servers.is_empty() {
            return None;
        }
        
        let selected_idx = match self.algorithm {
            LoadBalanceAlgorithm::RoundRobin => {
                let counter = self.rr_counter.fetch_add(1, Ordering::Relaxed);
                counter % healthy_servers.len()
            }
            LoadBalanceAlgorithm::LeastConnections => {
                healthy_servers.iter()
                    .enumerate()
                    .min_by_key(|(_, (_, s))| s.connections())
                    .map(|(i, _)| i)
                    .unwrap_or(0)
            }
            LoadBalanceAlgorithm::IpHash => {
                // シンプルなハッシュ関数（FNV-1a風）
                let mut hash: u64 = 14695981039346656037;
                for byte in client_ip.bytes() {
                    hash ^= byte as u64;
                    hash = hash.wrapping_mul(1099511628211);
                }
                (hash as usize) % healthy_servers.len()
            }
        };
        
        healthy_servers.get(selected_idx).map(|(_, s)| *s)
    }
    
    /// サーバー数を取得
    fn len(&self) -> usize {
        self.servers.len()
    }
}

// ====================
// nginx風パスルーター（最長プレフィックス一致）
// ====================
//
// nginxの location ディレクティブと同様のマッチング動作を実現。
//
// ## nginxのマッチング規則（優先順位順）
//
// | 優先度 | nginx記法 | 説明                     | 本実装での対応       |
// |--------|-----------|--------------------------|---------------------|
// | 1      | = /path   | 完全一致                 | 将来対応予定         |
// | 2      | ^~ /path  | 優先プレフィックス       | 将来対応予定         |
// | 3      | ~ regex   | 正規表現                 | 対応なし             |
// | 4      | /path     | **最長プレフィックス一致** | ✓ 実装済み         |
//
// ## 動作例
//
// 設定:
//   "/static" = { path = "./www/static/" }  ← 末尾スラッシュなしでもOK
//   "/api"    = { url = "http://backend:8080" }
//   "/"       = { path = "./www/index.html" }
//
// リクエスト → マッチするルート:
//   /                     → /       (完全一致)
//   /static               → /static (完全一致) → index.html を返す
//   /static/              → /static (ディレクトリアクセス) → index.html を返す
//   /static/css/style.css → /static (プレフィックスマッチ)
//   /api/users            → /api    (プレフィックスマッチ)
//   /favicon.ico          → 404     (マッチなし)
//   /unknown              → 404     (マッチなし)
//
// ## パフォーマンス
//
// matchit (Radix Tree) を使用: O(log n)
// | ルート数 | 線形探索 O(n) | Radix Tree O(log n) |
// |----------|---------------|---------------------|
// | 10       | ~100ns        | ~50ns               |
// | 100      | ~1μs          | ~100ns              |
// | 1000     | ~10μs         | ~150ns              |

/// nginx風パスルーター
/// 
/// 最長プレフィックス一致による高速なルーティングを提供。
/// "/" が登録されている場合はcatch-all（フォールバック）として機能。
#[derive(Clone)]
struct PathRouter {
    /// matchit Router（Radix Tree実装）
    router: Arc<matchit::Router<usize>>,
    /// Backendの実体を保持
    backends: Arc<Vec<(Arc<[u8]>, Backend)>>,
}

impl PathRouter {
    /// 新しいPathRouterを構築
    /// 
    /// nginx風の最長プレフィックス一致を実現するため、
    /// 各プレフィックスに対して完全一致とワイルドカードの両方を登録。
    fn new(entries: Vec<(String, Backend)>) -> io::Result<Self> {
        let mut router = matchit::Router::new();
        let mut backends = Vec::with_capacity(entries.len());
        
        // バックエンドを登録（インデックスを確定）
        for (prefix, backend) in entries.iter() {
            backends.push((Arc::from(prefix.as_bytes()), backend.clone()));
        }
        
        // ルートを登録
        // matchitの優先順位: 静的パス > パラメータ > ワイルドカード
        // より具体的なプレフィックスが自動的に優先される
        for (i, (prefix, _)) in entries.iter().enumerate() {
            if prefix == "/" {
                // "/" は完全一致のみ（"/" へのリクエストのみマッチ）
                // catch-all動作なし: マッチしないパスは404エラー
                if let Err(e) = router.insert("/".to_string(), i) {
                    warn!("Route registration failed for '/': {}", e);
                }
            } else if prefix.ends_with('/') {
                // "/api/" スタイル（ディレクトリ）
                let base = prefix.trim_end_matches('/');
                
                // "/api" への完全一致
                let _ = router.insert(base.to_string(), i);
                // "/api/" への完全一致
                let _ = router.insert(prefix.clone(), i);
                // "/api/{*rest}" でサブパスにマッチ
                let _ = router.insert(format!("{base}/{{*rest}}"), i);
            } else {
                // "/api" スタイル（通常のプレフィックス）
                // 末尾スラッシュなしでもディレクトリ配信やプロキシが動作するように
                // 3つのパターンを登録：
                // 1. "/api" への完全一致
                let _ = router.insert(prefix.clone(), i);
                // 2. "/api/" への完全一致（ディレクトリアクセス）
                let _ = router.insert(format!("{prefix}/"), i);
                // 3. "/api/{*rest}" でサブパスにマッチ
                let _ = router.insert(format!("{prefix}/{{*rest}}"), i);
            }
        }
        
        Ok(Self {
            router: Arc::new(router),
            backends: Arc::new(backends),
        })
    }
    
    /// パスに最長一致するバックエンドを検索（nginx風）
    /// 
    /// matchitが内部でRadix Treeを使用し、
    /// 最も具体的な（＝最長の）プレフィックスを自動的に選択。
    fn find_longest(&self, path: &[u8]) -> Option<(&[u8], &Backend)> {
        let path_str = std::str::from_utf8(path).ok()?;
        
        match self.router.at(path_str) {
            Ok(matched) => {
                let idx = *matched.value;
                let (prefix, backend) = &self.backends[idx];
                Some((prefix.as_ref(), backend))
            }
            Err(_) => None,
        }
    }
}

// 旧SortedPathMapとの互換性のため、型エイリアスを提供
// TODO: 将来的にはPathRouterに完全移行
type SortedPathMap = PathRouter;

// ====================
// 非同期I/Oトレイト（コード重複解消）
// ====================

/// 非同期読み込みトレイト（SafeReadBuffer対応）
/// 
/// 読み込み操作で `SafeReadBuffer` を受け取り、返却します。
/// monoio の `set_init()` コールバックにより、読み込み完了時に
/// 自動的に `valid_len` が設定されます。
trait AsyncReader {
    async fn read_buf(&mut self, buf: SafeReadBuffer) -> (io::Result<usize>, SafeReadBuffer);
}

/// 非同期書き込みトレイト
/// 
/// 書き込み操作では `Vec<u8>` を受け取ります。
/// 書き込みデータは既に有効なデータなので、SafeReadBuffer は不要です。
trait AsyncWriter {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>);
}

// TcpStream用の実装
impl AsyncReader for TcpStream {
    async fn read_buf(&mut self, buf: SafeReadBuffer) -> (io::Result<usize>, SafeReadBuffer) {
        self.read(buf).await
    }
}

impl AsyncWriter for TcpStream {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.write_all(buf).await
    }
}

// KtlsServerStream用の実装（rustls + ktls2）
#[cfg(feature = "ktls")]
impl AsyncReader for KtlsServerStream {
    async fn read_buf(&mut self, buf: SafeReadBuffer) -> (io::Result<usize>, SafeReadBuffer) {
        self.read(buf).await
    }
}

#[cfg(feature = "ktls")]
impl AsyncWriter for KtlsServerStream {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.write_all(buf).await
    }
}

// KtlsClientStream用の実装（rustls + ktls2）
#[cfg(feature = "ktls")]
impl AsyncReader for KtlsClientStream {
    async fn read_buf(&mut self, buf: SafeReadBuffer) -> (io::Result<usize>, SafeReadBuffer) {
        self.read(buf).await
    }
}

#[cfg(feature = "ktls")]
impl AsyncWriter for KtlsClientStream {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.write_all(buf).await
    }
}

// SimpleTlsServerStream用の実装（rustls のみ）
#[cfg(not(feature = "ktls"))]
impl AsyncReader for simple_tls::SimpleTlsServerStream {
    async fn read_buf(&mut self, buf: SafeReadBuffer) -> (io::Result<usize>, SafeReadBuffer) {
        self.read(buf).await
    }
}

#[cfg(not(feature = "ktls"))]
impl AsyncWriter for simple_tls::SimpleTlsServerStream {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.write_all(buf).await
    }
}

// SimpleTlsClientStream用の実装（rustls のみ）
#[cfg(not(feature = "ktls"))]
impl AsyncReader for simple_tls::SimpleTlsClientStream {
    async fn read_buf(&mut self, buf: SafeReadBuffer) -> (io::Result<usize>, SafeReadBuffer) {
        self.read(buf).await
    }
}

#[cfg(not(feature = "ktls"))]
impl AsyncWriter for simple_tls::SimpleTlsClientStream {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.write_all(buf).await
    }
}

// ====================
// 設定読み込み
// ====================

/// 設定ファイルのバリデーション
/// 
/// 設定読み込み前に、必須ファイルの存在とパスの妥当性をチェックします。
fn validate_config(config: &Config) -> io::Result<()> {
    // TLS証明書ファイルの存在チェック
    let cert_path = Path::new(&config.tls.cert_path);
    if !cert_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("TLS certificate file not found: {}", config.tls.cert_path)
        ));
    }
    
    let key_path = Path::new(&config.tls.key_path);
    if !key_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("TLS key file not found: {}", config.tls.key_path)
        ));
    }
    
    // バインドアドレスの妥当性チェック
    if config.server.listen.parse::<SocketAddr>().is_err() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Invalid listen address: {}", config.server.listen)
        ));
    }
    
    // Upstream設定の妥当性チェック
    if let Some(ref upstreams) = config.upstreams {
        for (name, upstream) in upstreams {
            if upstream.servers.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Upstream '{}' has no servers configured", name)
                ));
            }
            
            for entry in &upstream.servers {
                if ProxyTarget::parse(&entry.url).is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("Invalid server URL in upstream '{}': {}", name, entry.url)
                    ));
                }
            }
        }
    }
    
    // ホストルートの妥当性チェック
    if let Some(ref host_routes) = config.host_routes {
        for (host, backend) in host_routes {
            validate_backend_config(backend, host)?;
        }
    }
    
    // パスルートの妥当性チェック
    if let Some(ref path_routes) = config.path_routes {
        for (host, paths) in path_routes {
            for (path, backend) in paths {
                validate_backend_config(backend, &format!("{}:{}", host, path))?;
            }
        }
    }
    
    Ok(())
}

/// バックエンド設定の妥当性チェック
fn validate_backend_config(config: &BackendConfig, route_name: &str) -> io::Result<()> {
    match config {
        BackendConfig::Proxy { url, .. } => {
            if ProxyTarget::parse(url).is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Invalid proxy URL for route '{}': {}", route_name, url)
                ));
            }
        }
        BackendConfig::ProxyUpstream { upstream, .. } => {
            // upstream の存在は load_backend でチェックされる
            if upstream.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Empty upstream name for route '{}'", route_name)
                ));
            }
        }
        BackendConfig::File { path, mode, .. } => {
            let file_path = Path::new(path);
            if !file_path.exists() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("File/directory not found for route '{}': {}", route_name, path)
                ));
            }
            
            if !["sendfile", "memory", ""].contains(&mode.as_str()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Invalid mode for route '{}': {} (expected 'sendfile' or 'memory')", route_name, mode)
                ));
            }
        }
        BackendConfig::Redirect { redirect_url, redirect_status, .. } => {
            if redirect_url.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Empty redirect_url for route '{}'", route_name)
                ));
            }
            if !matches!(*redirect_status, 301 | 302 | 303 | 307 | 308) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Invalid redirect_status for route '{}': {} (expected 301, 302, 303, 307, or 308)", route_name, redirect_status)
                ));
            }
        }
    }
    
    Ok(())
}

// rustls 用の TLS 設定読み込み（統一）
fn load_tls_config(
    tls_config: &TlsConfigSection, 
    ktls_enabled: bool,
    #[allow(unused_variables)] http2_enabled: bool,
) -> io::Result<Arc<ServerConfig>> {
    let cert_file = File::open(&tls_config.cert_path)?;
    let key_file = File::open(&tls_config.key_path)?;

    let mut cert_reader = BufReader::new(cert_file);
    let cert_chain = certs(&mut cert_reader).collect::<Result<Vec<_>, _>>()?;

    let mut key_reader = BufReader::new(key_file);
    let keys = private_key(&mut key_reader)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Private key not found"))?;

    // kTLS 有効時のみ config を変更するため、条件付きで mut を使用
    #[allow(unused_mut)]
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, keys)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // kTLS が有効な場合のみシークレット抽出を有効化
    // これにより dangerous_extract_secrets() が使用可能になる
    #[cfg(feature = "ktls")]
    if ktls_enabled {
        config.enable_secret_extraction = true;
        info!("TLS secret extraction enabled for kTLS support");
    }

    // kTLS 無効時は警告を抑制
    #[cfg(not(feature = "ktls"))]
    let _ = ktls_enabled;

    // HTTP/2 有効時は ALPN を設定
    #[cfg(feature = "http2")]
    if http2_enabled {
        config = protocol::configure_alpn_h2(config, false);
        info!("HTTP/2 enabled via ALPN negotiation (h2, http/1.1)");
    }

    Ok(Arc::new(config))
}

/// 設定読み込みの戻り値型（統一）
struct LoadedConfig {
    listen_addr: String,
    /// HTTPリスナーアドレス（HTTPSリダイレクト用、オプション）
    listen_http_addr: Option<SocketAddr>,
    tls_config: Arc<ServerConfig>,
    /// TLS証明書パス（ログ・表示用）
    tls_cert_path: String,
    /// TLS秘密鍵パス（ログ・表示用）
    tls_key_path: String,
    /// TLS証明書（PEM形式、事前読み込み済み）
    /// 
    /// Landlock適用前に読み込まれた証明書データ。
    /// HTTP/3ではmemfd経由でquicheに渡すことで、
    /// Landlockによるファイルシステム制限下でも動作可能。
    tls_cert_pem: Arc<Vec<u8>>,
    /// TLS秘密鍵（PEM形式、事前読み込み済み）
    /// 
    /// Landlock適用前に読み込まれた秘密鍵データ。
    /// HTTP/3ではmemfd経由でquicheに渡す。
    tls_key_pem: Arc<Vec<u8>>,
    host_routes: Arc<HashMap<Box<[u8]>, Backend>>,
    path_routes: Arc<HashMap<Box<[u8]>, PathRouter>>,
    ktls_config: KtlsConfig,
    reuseport_balancing: ReuseportBalancing,
    num_threads: usize,
    /// Huge Pages (Large OS Pages) を有効化するかどうか
    huge_pages_enabled: bool,
    /// グローバルセキュリティ設定
    global_security: GlobalSecurityConfig,
    /// ログ設定
    logging: LoggingConfigSection,
    /// Prometheusメトリクス設定
    prometheus_config: PrometheusConfig,
    /// Upstream グループ（健康チェック用）
    upstream_groups: Arc<HashMap<String, Arc<UpstreamGroup>>>,
    /// HTTP/2 を有効化するかどうか
    http2_enabled: bool,
    /// HTTP/3 を有効化するかどうか
    http3_enabled: bool,
    /// HTTP/3 リスナーアドレス (UDP)
    http3_listen: Option<String>,
    /// HTTP/2 設定（詳細設定）
    http2_config: Http2ConfigSection,
    /// HTTP/3 設定（詳細設定）
    http3_config: Http3ConfigSection,
}

// ====================
// ホットリロード対応のランタイム設定
// ====================
//
// ArcSwap を使用することで、設定変更時にロックフリーで
// 新しい設定に切り替えることができます。
//
// ## メリット
// - 読み込みはロックフリーで非常に高速（数ナノ秒）
// - 設定更新中もリクエスト処理を継続可能
// - 古い設定を参照中のリクエストは安全に完了
//
// ## 使用方法
// ```rust
// // 設定の読み込み（ロックフリー）
// let config = CURRENT_CONFIG.load();
// 
// // 設定の更新（アトミック）
// CURRENT_CONFIG.store(Arc::new(new_config));
// ```

/// ランタイムで使用する設定（ホットリロード対応）
/// 
/// 一部のフィールドはホットリロード機能のために保持されているが、
/// 現在は読み取られていない（将来的にTLS再設定などで使用予定）
struct RuntimeConfig {
    /// ホストベースのルーティング（O(1) HashMap）
    host_routes: Arc<HashMap<Box<[u8]>, Backend>>,
    /// パスベースのルーティング（O(log n) Radix Tree）
    path_routes: Arc<HashMap<Box<[u8]>, PathRouter>>,
    /// TLS設定（ホットリロード時の参照用）
    #[allow(dead_code)]
    tls_config: Option<Arc<ServerConfig>>,
    /// kTLS設定（ホットリロード時の参照用）
    #[allow(dead_code)]
    ktls_config: Arc<KtlsConfig>,
    /// グローバルセキュリティ設定（ホットリロード時の参照用）
    #[allow(dead_code)]
    global_security: Arc<GlobalSecurityConfig>,
    /// Prometheusメトリクス設定
    prometheus_config: Arc<PrometheusConfig>,
    /// Upstream グループ（健康チェック用）
    upstream_groups: Arc<HashMap<String, Arc<UpstreamGroup>>>,
    /// HTTP/2 有効化フラグ
    http2_enabled: bool,
    /// HTTP/2 設定（詳細設定）
    http2_config: Http2ConfigSection,
    /// HTTP/3 設定（圧縮設定の解決に使用）
    http3_config: Http3ConfigSection,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            host_routes: Arc::new(HashMap::new()),
            path_routes: Arc::new(HashMap::new()),
            tls_config: None,
            ktls_config: Arc::new(KtlsConfig::default()),
            global_security: Arc::new(GlobalSecurityConfig::default()),
            prometheus_config: Arc::new(PrometheusConfig::default()),
            upstream_groups: Arc::new(HashMap::new()),
            http2_enabled: false,
            http2_config: Http2ConfigSection::default(),
            http3_config: Http3ConfigSection::default(),
        }
    }
}

/// グローバルな設定保持用（ホットリロード対応）
/// 読み込みはロックフリーで非常に高速
static CURRENT_CONFIG: Lazy<ArcSwap<RuntimeConfig>> =
    Lazy::new(|| ArcSwap::from_pointee(RuntimeConfig::default()));

/// デフォルトの設定ファイルパス
const DEFAULT_CONFIG_PATH: &str = "/etc/veil/config.toml";

/// グローバルな設定ファイルパス（ホットリロード用）
/// コマンドライン引数で指定されたパス、またはデフォルトパスを保持
static CONFIG_PATH: Lazy<ArcSwap<PathBuf>> =
    Lazy::new(|| ArcSwap::from_pointee(PathBuf::from(DEFAULT_CONFIG_PATH)));

/// 設定をホットリロードする
/// 
/// 実行中のリクエストは古い設定を参照し続け、
/// 新規リクエストは新しい設定を使用します。
/// 
/// ## セキュリティに関する注意
/// 
/// TLS証明書・秘密鍵はホットリロードの対象外です。
/// これはLandlockによるファイルシステム制限を適用後、
/// 証明書ファイルへのアクセスを禁止するためです。
/// 
/// 証明書を更新する場合は、サーバーを再起動してください。
fn reload_config(path: &Path) -> io::Result<()> {
    let loaded = load_config_without_tls(path)?;
    
    // 現在のTLS設定を維持（ホットリロード対象外）
    let current = CURRENT_CONFIG.load();
    
    let runtime_config = RuntimeConfig {
        host_routes: loaded.host_routes,
        path_routes: loaded.path_routes,
        // TLS設定は起動時のものを維持（セキュリティ上の理由）
        tls_config: current.tls_config.clone(),
        ktls_config: current.ktls_config.clone(),
        global_security: Arc::new(loaded.global_security),
        prometheus_config: Arc::new(loaded.prometheus_config),
        upstream_groups: loaded.upstream_groups,
        http2_enabled: loaded.http2_enabled,
        http2_config: loaded.http2_config,
        http3_config: loaded.http3_config,
    };
    
    // アトミックに設定を入れ替え
    CURRENT_CONFIG.store(Arc::new(runtime_config));
    
    info!("Configuration reloaded successfully (TLS certificates unchanged - restart required for TLS updates)");
    Ok(())
}

/// ホットリロード用の設定（TLS証明書を除く）
/// 
/// Landlock適用後はTLS証明書ファイルへのアクセスが制限されるため、
/// ホットリロード時はルーティング設定等のみを更新します。
struct LoadedConfigWithoutTls {
    host_routes: Arc<HashMap<Box<[u8]>, Backend>>,
    path_routes: Arc<HashMap<Box<[u8]>, PathRouter>>,
    global_security: GlobalSecurityConfig,
    prometheus_config: PrometheusConfig,
    upstream_groups: Arc<HashMap<String, Arc<UpstreamGroup>>>,
    http2_enabled: bool,
    http2_config: Http2ConfigSection,
    http3_config: Http3ConfigSection,
}

/// TLS証明書を除いた設定をロード（ホットリロード用）
/// 
/// Landlock適用後は証明書ファイルへのアクセスが制限されるため、
/// この関数ではTLS関連の読み込みをスキップします。
fn load_config_without_tls(path: &Path) -> io::Result<LoadedConfigWithoutTls> {
    let config_str = fs::read_to_string(path)?;
    let config: Config = toml::from_str(&config_str)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("TOML parse error: {}", e)))?;
    
    // 設定ファイルのバリデーション
    validate_config(&config)?;

    // HTTP/2・HTTP/3 設定を読み込み
    let http2_enabled = config.server.http2_enabled;
    let http2_config = config.http2.clone();
    let http3_config = config.http3.clone();

    // Upstream グループを構築（ロードバランシング用）
    let mut upstream_groups: HashMap<String, Arc<UpstreamGroup>> = HashMap::new();
    if let Some(upstreams) = &config.upstreams {
        for (name, cfg) in upstreams {
            if let Some(group) = UpstreamGroup::new(
                name.clone(), 
                cfg.servers.clone(), 
                cfg.algorithm,
                cfg.health_check.clone(),
            ) {
                info!("Reloaded upstream '{}' with {} servers ({:?})", 
                      name, group.len(), cfg.algorithm);
                upstream_groups.insert(name.clone(), Arc::new(group));
            } else {
                warn!("Failed to reload upstream '{}': no valid servers", name);
            }
        }
    }

    let mut host_routes_bytes: HashMap<Box<[u8]>, Backend> = HashMap::new();
    if let Some(host_routes) = config.host_routes {
        for (k, v) in host_routes {
            let backend = load_backend(&v, &upstream_groups)?;
            host_routes_bytes.insert(k.into_bytes().into_boxed_slice(), backend);
        }
    }

    let mut path_routes_bytes: HashMap<Box<[u8]>, PathRouter> = HashMap::new();
    if let Some(path_routes) = config.path_routes {
        for (host, path_map) in path_routes {
            let mut entries: Vec<(String, Backend)> = Vec::with_capacity(path_map.len());
            for (k, v) in path_map {
                entries.push((k, load_backend(&v, &upstream_groups)?));
            }
            entries.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
            let router = PathRouter::new(entries)?;
            path_routes_bytes.insert(
                host.into_bytes().into_boxed_slice(),
                router
            );
        }
    }

    Ok(LoadedConfigWithoutTls {
        host_routes: Arc::new(host_routes_bytes),
        path_routes: Arc::new(path_routes_bytes),
        global_security: config.security,
        prometheus_config: config.prometheus,
        upstream_groups: Arc::new(upstream_groups),
        http2_enabled,
        http2_config,
        http3_config,
    })
}

fn load_config(path: &Path) -> io::Result<LoadedConfig> {
    let config_str = fs::read_to_string(path)?;
    let config: Config = toml::from_str(&config_str)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("TOML parse error: {}", e)))?;
    
    // 設定ファイルのバリデーション
    validate_config(&config)?;

    // kTLS設定（TLS設定より先に読み込む）
    let ktls_config = KtlsConfig {
        enabled: config.tls.ktls_enabled,
        enable_tx: config.tls.ktls_enabled,
        enable_rx: config.tls.ktls_enabled,
        fallback_enabled: config.tls.ktls_fallback_enabled,
    };

    // HTTP/2・HTTP/3 設定を読み込み
    // 有効化フラグは server セクションで管理、詳細設定は [http2]/[http3] セクション
    let http2_enabled = config.server.http2_enabled;
    let http3_enabled = config.server.http3_enabled;
    let http2_config = config.http2.clone();
    let http3_config = config.http3.clone();
    let http3_listen = http3_config.listen.clone();
    
    // TLS設定（kTLS有効時はシークレット抽出を有効化、HTTP/2有効時はALPN設定）
    let tls_config = load_tls_config(&config.tls, ktls_config.enabled, http2_enabled)?;
    
    // Upstream グループを構築（ロードバランシング用）
    let mut upstream_groups: HashMap<String, Arc<UpstreamGroup>> = HashMap::new();
    if let Some(upstreams) = &config.upstreams {
        for (name, cfg) in upstreams {
            if let Some(group) = UpstreamGroup::new(
                name.clone(), 
                cfg.servers.clone(), 
                cfg.algorithm,
                cfg.health_check.clone(),
            ) {
                info!("Loaded upstream '{}' with {} servers ({:?})", 
                      name, group.len(), cfg.algorithm);
                if cfg.health_check.is_some() {
                    info!("  Health check enabled for '{}'", name);
                }
                upstream_groups.insert(name.clone(), Arc::new(group));
            } else {
                warn!("Failed to load upstream '{}': no valid servers", name);
            }
        }
    }

    let mut host_routes_bytes: HashMap<Box<[u8]>, Backend> = HashMap::new();
    if let Some(host_routes) = config.host_routes {
        for (k, v) in host_routes {
            let backend = load_backend(&v, &upstream_groups)?;
            host_routes_bytes.insert(k.into_bytes().into_boxed_slice(), backend);
        }
    }

    let mut path_routes_bytes: HashMap<Box<[u8]>, PathRouter> = HashMap::new();
    if let Some(path_routes) = config.path_routes {
        for (host, path_map) in path_routes {
            let mut entries: Vec<(String, Backend)> = Vec::with_capacity(path_map.len());
            for (k, v) in path_map {
                entries.push((k, load_backend(&v, &upstream_groups)?));
            }
            // 長さ降順でソート（最長一致の優先順位を維持）
            // PathRouter内部でRadix Treeに登録する際に使用
            entries.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
            
            // PathRouter（Radix Treeベース）を構築
            let router = PathRouter::new(entries)?;
            
            path_routes_bytes.insert(
                host.into_bytes().into_boxed_slice(),
                router
            );
        }
    }

    // スレッド数の決定: 未指定または0の場合はCPUコア数を使用
    let num_threads = match config.server.threads {
        Some(n) if n > 0 => n,
        _ => num_cpus::get(),
    };

    // HTTPリスナーアドレスをパース（HTTPSリダイレクト用）
    let listen_http_addr = config.server.http.as_ref().and_then(|addr| {
        match addr.parse::<SocketAddr>() {
            Ok(socket_addr) => Some(socket_addr),
            Err(e) => {
                warn!("Invalid HTTP listen address '{}': {}", addr, e);
                None
            }
        }
    });

    // Prometheusメトリクス設定をログ出力
    if config.prometheus.enabled {
        info!("Prometheus metrics enabled at path: {}", config.prometheus.path);
        if !config.prometheus.allowed_ips.is_empty() {
            info!("  Allowed IPs: {:?}", config.prometheus.allowed_ips);
        }
    } else {
        info!("Prometheus metrics disabled");
    }

    // TLS証明書をバイト列として読み込み（HTTP/3用、Landlock適用前に読み込み）
    // これによりLandlock適用後も証明書ファイルへのアクセスなしで動作可能
    let tls_cert_pem = fs::read(&config.tls.cert_path)
        .map_err(|e| io::Error::new(e.kind(), format!("Failed to read TLS cert '{}': {}", config.tls.cert_path, e)))?;
    let tls_key_pem = fs::read(&config.tls.key_path)
        .map_err(|e| io::Error::new(e.kind(), format!("Failed to read TLS key '{}': {}", config.tls.key_path, e)))?;
    
    info!("TLS certificates pre-loaded for Landlock compatibility (cert: {} bytes, key: {} bytes)",
          tls_cert_pem.len(), tls_key_pem.len());

    Ok(LoadedConfig {
        listen_addr: config.server.listen,
        listen_http_addr,
        tls_config,
        tls_cert_path: config.tls.cert_path.clone(),
        tls_key_path: config.tls.key_path.clone(),
        tls_cert_pem: Arc::new(tls_cert_pem),
        tls_key_pem: Arc::new(tls_key_pem),
        host_routes: Arc::new(host_routes_bytes),
        path_routes: Arc::new(path_routes_bytes),
        ktls_config,
        reuseport_balancing: config.performance.reuseport_balancing,
        num_threads,
        huge_pages_enabled: config.performance.huge_pages_enabled,
        global_security: config.security,
        logging: config.logging,
        prometheus_config: config.prometheus,
        upstream_groups: Arc::new(upstream_groups),
        http2_enabled,
        http3_enabled,
        http3_listen,
        http2_config,
        http3_config,
    })
}

/// 設定ファイルからログ設定のみを読み込む（ログ初期化前用）
fn load_logging_config(path: &Path) -> io::Result<LoggingConfigSection> {
    let config_str = fs::read_to_string(path)?;
    let config: Config = toml::from_str(&config_str)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("TOML parse error: {}", e)))?;
    Ok(config.logging)
}

// ====================
// JSON形式ログフォーマッタ
// ====================

use ftlog::{FtLogFormat, Level, Record};
use std::borrow::Cow;
use std::fmt::{Display, Formatter, Result as FmtResult};

/// JSON形式ログフォーマッタ
/// 
/// ログメッセージをJSON形式で出力するカスタムフォーマッタです。
/// 出力形式:
/// ```json
/// {"timestamp":"2024-01-01T00:00:00.000Z","level":"INFO","target":"veil","file":"main.rs","line":123,"message":"..."}
/// ```
struct JsonLogFormat;

/// JSON形式ログメッセージ
struct JsonLogMessage {
    level: Level,
    target: Cow<'static, str>,
    file: Cow<'static, str>,
    line: Option<u32>,
    args: String,
}

impl Display for JsonLogMessage {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        // タイムスタンプを取得（RFC 3339形式）
        let now = time::OffsetDateTime::now_utc();
        let timestamp = now.format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| now.to_string());
        
        // JSON形式でフォーマット
        // メッセージ内の特殊文字をエスケープ
        write!(
            f,
            r#"{{"timestamp":"{}","level":"{}","target":"{}","file":"{}","line":{},"message":"{}"}}"#,
            timestamp,
            self.level,
            escape_json(&self.target),
            escape_json(&self.file),
            self.line.unwrap_or(0),
            escape_json(&self.args)
        )
    }
}

impl FtLogFormat for JsonLogFormat {
    fn msg(&self, record: &Record) -> Box<dyn Send + Sync + Display> {
        Box::new(JsonLogMessage {
            level: record.level(),
            target: record.target().to_string().into(),
            file: record
                .file_static()
                .map(Cow::Borrowed)
                .or_else(|| record.file().map(|s| Cow::Owned(s.to_owned())))
                .unwrap_or(Cow::Borrowed("")),
            line: record.line(),
            args: format!("{}", record.args()),
        })
    }
}

/// JSON文字列内の特殊文字をエスケープ
fn escape_json(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => result.push_str(r#"\""#),
            '\\' => result.push_str(r"\\"),
            '\n' => result.push_str(r"\n"),
            '\r' => result.push_str(r"\r"),
            '\t' => result.push_str(r"\t"),
            c if c.is_control() => {
                // 制御文字はUnicodeエスケープ
                result.push_str(&format!(r"\u{:04x}", c as u32));
            }
            c => result.push(c),
        }
    }
    result
}

/// JSON形式ログ用カスタムWriter
/// 
/// ftlogが出力するログ行からプレフィックス（タイムスタンプと遅延時間）を削除し、
/// JSONのみを出力します。
/// 
/// ftlogの出力形式: `{timestamp} {delay}ms {json_message}\n`
/// 出力形式: `{json_message}\n`
struct JsonLogWriter<W: io::Write + Send> {
    inner: W,
}

impl<W: io::Write + Send> JsonLogWriter<W> {
    fn new(writer: W) -> Self {
        Self { inner: writer }
    }
}

impl<W: io::Write + Send> io::Write for JsonLogWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // ftlogの出力からJSON部分を抽出
        // 形式: "{timestamp} {delay}ms {json}\n"
        // JSONは '{' で始まるため、最初の '{' を見つける
        if let Some(json_start) = buf.iter().position(|&b| b == b'{') {
            // JSON部分のみを書き込み
            self.inner.write_all(&buf[json_start..])?;
            Ok(buf.len())
        } else {
            // JSONが見つからない場合はそのまま書き込み
            self.inner.write_all(buf)?;
            Ok(buf.len())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// JSON形式ログ用FileAppender
/// 
/// ファイルへのJSON形式ログ出力用のカスタムAppenderです。
/// ftlogのプレフィックスを削除してJSONのみをファイルに書き込みます。
struct JsonFileAppender {
    writer: JsonLogWriter<std::io::BufWriter<std::fs::File>>,
}

impl JsonFileAppender {
    fn new(path: &str) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let buf_writer = std::io::BufWriter::new(file);
        Ok(Self {
            writer: JsonLogWriter::new(buf_writer),
        })
    }
}

impl io::Write for JsonFileAppender {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// 標準エラー出力用JSON形式Writer
struct JsonStderrWriter {
    writer: JsonLogWriter<std::io::Stderr>,
}

impl JsonStderrWriter {
    fn new() -> Self {
        Self {
            writer: JsonLogWriter::new(std::io::stderr()),
        }
    }
}

impl io::Write for JsonStderrWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// ftlogを設定に基づいて初期化
/// 
/// ftlogは内部でバックグラウンドスレッドとチャネルを使用した非同期ログライブラリです。
/// 以下の最適化を行います：
/// - channel_size: 高負荷時のログドロップを防止するため大きなバッファを使用
/// - max_log_level: 不要なログを除外してオーバーヘッドを削減
/// 
/// ## grokの提案に対する補足
/// 
/// grokはtokio::sync::mpscを使った非同期化を提案しましたが、これは以下の理由で不適切です：
/// 1. ftlogは既に非同期（内部でチャネル＋バックグラウンドスレッドを使用）
/// 2. tokio::sync::mpscはmonoioランタイムと互換性がない
/// 3. 追加の非同期化層はオーバーヘッドを増やすだけ
/// 
/// ftlog公式ドキュメントより：
/// > ftlog mitigates this bottleneck by sending messages to a dedicated logger
/// > thread and computing as little as possible in the main/worker thread.
/// 
/// 代わりに、ftlogの設定を最適化することで同等以上の効果を得られます。
fn init_logging(config: &LoggingConfigSection) -> ftlog::LoggerGuard {
    let level = parse_log_level(&config.level);
    let use_json = config.format == LogFormat::Json;
    
    // ファイル出力が設定されている場合
    if let Some(ref file_path) = config.file_path {
        if use_json {
            // JSON形式: カスタムWriterを使用してftlogのプレフィックスを削除
            let json_appender = JsonFileAppender::new(file_path)
                .expect("Failed to create JSON file appender");
            
            ftlog::builder()
                .max_log_level(level)
                .bounded(config.channel_size, false)
                .format(JsonLogFormat)
                .root(json_appender)
                .try_init()
                .expect("Failed to initialize ftlog with JSON file appender")
        } else {
            // テキスト形式: ftlogの標準FileAppenderを使用
            let file_appender = ftlog::appender::FileAppender::builder()
                .path(file_path)
                .rotate(ftlog::appender::Period::Day)
                .build();
            
            ftlog::builder()
                .max_log_level(level)
                .bounded(config.channel_size, false)
                .root(file_appender)
                .try_init()
                .expect("Failed to initialize ftlog with file appender")
        }
    } else {
        // 標準エラー出力
        if use_json {
            // JSON形式: カスタムWriterを使用してftlogのプレフィックスを削除
            let json_writer = JsonStderrWriter::new();
            
            ftlog::builder()
                .max_log_level(level)
                .bounded(config.channel_size, false)
                .format(JsonLogFormat)
                .root(json_writer)
                .try_init()
                .expect("Failed to initialize ftlog with JSON stderr writer")
        } else {
            // テキスト形式（デフォルト）
            ftlog::builder()
                .max_log_level(level)
                .bounded(config.channel_size, false)
                .try_init()
                .expect("Failed to initialize ftlog")
        }
    }
}

fn load_backend(
    config: &BackendConfig,
    upstream_groups: &HashMap<String, Arc<UpstreamGroup>>,
) -> io::Result<Backend> {
    match config {
        BackendConfig::Proxy { url, sni_name, use_h2c, security, compression, buffering, cache } => {
            // 単一URLの場合は UpstreamGroup::single で単一サーバーのグループを作成
            let target = ProxyTarget::parse(url)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid proxy URL"))?
                .with_sni_name(sni_name.clone())
                .with_h2c(*use_h2c);
            
            if *use_h2c && !target.use_tls {
                info!("H2C (HTTP/2 over cleartext) enabled for backend: {}", url);
            }
            
            // 圧縮設定のログ出力
            if compression.enabled {
                info!("Response compression enabled for backend: {} (gzip_level={}, brotli_level={})", 
                      url, compression.gzip_level, compression.brotli_level);
            }
            
            // バッファリング設定のログ出力
            if buffering.is_enabled() {
                info!("Response buffering enabled for backend: {} (mode={:?}, max_memory={})", 
                      url, buffering.mode, buffering.max_memory_buffer);
            }
            
            // キャッシュ設定のログ出力
            if cache.enabled {
                info!("Proxy cache enabled for backend: {} (max_memory={}, ttl={}s)", 
                      url, cache.max_memory_size, cache.default_ttl_secs);
            }
            
            let group = UpstreamGroup::single(target);
            Ok(Backend::Proxy(
                Arc::new(group), 
                Arc::new(security.clone()), 
                Arc::new(compression.clone()),
                Arc::new(buffering.clone()),
                Arc::new(cache.clone()),
            ))
        }
        BackendConfig::ProxyUpstream { upstream, security, compression, buffering, cache } => {
            // Upstream グループ参照
            let group = upstream_groups.get(upstream)
                .ok_or_else(|| io::Error::new(
                    io::ErrorKind::InvalidInput, 
                    format!("Upstream '{}' not found", upstream)
                ))?;
            
            // 圧縮設定のログ出力
            if compression.enabled {
                info!("Response compression enabled for upstream: {} (gzip_level={}, brotli_level={})", 
                      upstream, compression.gzip_level, compression.brotli_level);
            }
            
            // バッファリング設定のログ出力
            if buffering.is_enabled() {
                info!("Response buffering enabled for upstream: {} (mode={:?}, max_memory={})", 
                      upstream, buffering.mode, buffering.max_memory_buffer);
            }
            
            // キャッシュ設定のログ出力
            if cache.enabled {
                info!("Proxy cache enabled for upstream: {} (max_memory={}, ttl={}s)", 
                      upstream, cache.max_memory_size, cache.default_ttl_secs);
            }
            
            Ok(Backend::Proxy(
                group.clone(), 
                Arc::new(security.clone()), 
                Arc::new(compression.clone()),
                Arc::new(buffering.clone()),
                Arc::new(cache.clone()),
            ))
        }
        BackendConfig::File { path, mode, index, security, cache } => {
            let metadata = fs::metadata(path)?;
            let is_dir = metadata.is_dir();
            // インデックスファイル名を Arc<str> に変換（None = デフォルトで "index.html"）
            let index_file: Option<Arc<str>> = index.as_ref().map(|s| Arc::from(s.as_str()));
            let security = Arc::new(security.clone());
            let cache = Arc::new(cache.clone());
            
            // キャッシュ設定のログ出力
            if cache.enabled {
                info!("File cache enabled for path: {} (max_memory={}, ttl={}s)", 
                      path, cache.max_memory_size, cache.default_ttl_secs);
            }
            
            match mode.as_str() {
                "memory" => {
                    if is_dir {
                        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Memory mode not supported for directories"));
                    }
                    let data = fs::read(path)?;
                    let mime_type = mime_guess::from_path(path).first_or_octet_stream();
                    
                    Ok(Backend::MemoryFile(Arc::new(data), Arc::from(mime_type.as_ref()), security))
                }
                "sendfile" | "" => Ok(Backend::SendFile(Arc::new(PathBuf::from(path)), is_dir, index_file, security, cache)),
                _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid mode")),
            }
        }
        BackendConfig::Redirect { redirect_url, redirect_status, preserve_path } => {
            Ok(Backend::Redirect(Arc::from(redirect_url.as_str()), *redirect_status, *preserve_path))
        }
    }
}

// ====================
// コマンドライン引数パース
// ====================

// ====================
// HTTP to HTTPS リダイレクトハンドラー
// ====================
//
// HTTPアクセスをHTTPSにリダイレクトするための軽量ハンドラー。
// セキュリティ上の理由から、HTTPではリダイレクトのみを行い、
// コンテンツは一切配信しません。
//
// 301 Moved Permanently を使用することで、ブラウザがリダイレクト先を
// キャッシュし、以降のアクセスでは直接HTTPSに接続します。

/// HTTP 301リダイレクトレスポンスのテンプレート
const HTTP_301_REDIRECT_TEMPLATE: &[u8] = b"HTTP/1.1 301 Moved Permanently\r\nLocation: ";
const HTTP_301_REDIRECT_SUFFIX: &[u8] = b"\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";

/// HTTPリクエストを処理し、HTTPSにリダイレクトする
/// 
/// リクエストからHostヘッダーとパスを読み取り、
/// https://{host}{path} への301リダイレクトを返します。
async fn handle_http_redirect(mut stream: TcpStream) {
    // リクエストを読み取るためのバッファ（ヘッダーのみなので小さめ）
    let mut buffer = vec![0u8; 4096];
    
    // タイムアウト付きで読み取り
    let read_result = timeout(Duration::from_secs(5), stream.read(buffer)).await;
    
    let (result, buf) = match read_result {
        Ok(r) => r,
        Err(_) => {
            // タイムアウト
            return;
        }
    };
    buffer = buf;
    
    let bytes_read = match result {
        Ok(n) if n > 0 => n,
        _ => return,
    };
    
    // HTTPリクエストをパース
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = Request::new(&mut headers);
    
    let path = match req.parse(&buffer[..bytes_read]) {
        Ok(Status::Complete(_)) | Ok(Status::Partial) => {
            req.path.unwrap_or("/")
        }
        Err(_) => "/",
    };
    
    // Hostヘッダーを取得
    let host = req.headers.iter()
        .find(|h| h.name.eq_ignore_ascii_case("Host"))
        .map(|h| std::str::from_utf8(h.value).unwrap_or(""))
        .unwrap_or("");
    
    // リダイレクトURLを構築
    let redirect_url = if host.is_empty() {
        format!("https://localhost{}", path)
    } else {
        // ホストにポート番号が含まれている場合は除去（HTTPSのデフォルトポート443を使用）
        let clean_host = host.split(':').next().unwrap_or(host);
        format!("https://{}{}", clean_host, path)
    };
    
    // 301レスポンスを構築
    let mut response = Vec::with_capacity(
        HTTP_301_REDIRECT_TEMPLATE.len() + redirect_url.len() + HTTP_301_REDIRECT_SUFFIX.len()
    );
    response.extend_from_slice(HTTP_301_REDIRECT_TEMPLATE);
    response.extend_from_slice(redirect_url.as_bytes());
    response.extend_from_slice(HTTP_301_REDIRECT_SUFFIX);
    
    // レスポンスを送信
    let _ = timeout(Duration::from_secs(5), stream.write_all(response)).await;
}

/// High-Performance Reverse Proxy Server
/// 
/// io_uring (monoio) と rustls を使用した高性能リバースプロキシサーバー
#[derive(Parser, Debug)]
#[command(name = "veil")]
#[command(author, version, about, long_about = None)]
struct CliArgs {
    /// 設定ファイルのパス
    #[arg(short, long, default_value = DEFAULT_CONFIG_PATH)]
    config: PathBuf,
}

// ====================
// メイン関数
// ====================

fn main() {
    // コマンドライン引数を解析（--help, --version は clap が自動処理）
    let cli_args = CliArgs::parse();
    
    // 設定ファイルパスをグローバル変数に保存（ホットリロード用）
    CONFIG_PATH.store(Arc::new(cli_args.config.clone()));
    let config_path = cli_args.config;
    
    // プロセスレベルで暗号プロバイダーをインストール（ring使用）
    CryptoProvider::install_default(rustls::crypto::ring::default_provider())
        .expect("Failed to install rustls crypto provider");
    
    // ログ設定を先に読み込む（ログ初期化前）
    // 設定ファイルが読めない場合はデフォルト設定を使用
    let logging_config = load_logging_config(&config_path)
        .unwrap_or_else(|_| LoggingConfigSection::default());
    
    // ftlogを設定に基づいて初期化
    // ftlogは内部でバックグラウンドスレッドとチャネルを使用した非同期ログライブラリ
    // 追加の非同期化層（tokio::sync::mpsc等）は不要
    let _guard = init_logging(&logging_config);

    let mut loaded_config = match load_config(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Config load error: {}", e);
            return;
        }
    };
    
    // Huge Pages (Large OS Pages) 設定
    // mimallocでHuge Pagesを有効化し、TLBミスを削減
    // 注: グローバルアロケータは静的初期化されるため、
    //     この設定は以降の新規割り当てに影響する
    configure_huge_pages(loaded_config.huge_pages_enabled);
    
    // TLS アクセプターを作成
    #[cfg(feature = "ktls")]
    let acceptor = RustlsAcceptor::new(loaded_config.tls_config.clone())
        .with_ktls(loaded_config.ktls_config.enabled)
        .with_fallback(loaded_config.ktls_config.fallback_enabled);
    
    #[cfg(not(feature = "ktls"))]
    let acceptor = simple_tls::SimpleTlsAcceptor::new(loaded_config.tls_config.clone())
        .with_ktls(loaded_config.ktls_config.enabled);
    
    let listen_addr = loaded_config.listen_addr.parse::<SocketAddr>()
        .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 443)));
    let ktls_config = Arc::new(loaded_config.ktls_config.clone());
    
    // CURRENT_CONFIG を初期化（ホットリロード対応）
    // ワーカースレッドは CURRENT_CONFIG.load() を使用して最新の設定を取得
    let runtime_config = RuntimeConfig {
        host_routes: loaded_config.host_routes.clone(),
        path_routes: loaded_config.path_routes.clone(),
        tls_config: Some(loaded_config.tls_config.clone()),
        ktls_config: ktls_config.clone(),
        global_security: Arc::new(loaded_config.global_security.clone()),
        prometheus_config: Arc::new(loaded_config.prometheus_config.clone()),
        upstream_groups: loaded_config.upstream_groups.clone(),
        http2_enabled: loaded_config.http2_enabled,
        http2_config: loaded_config.http2_config.clone(),
        http3_config: loaded_config.http3_config.clone(),
    };
    CURRENT_CONFIG.store(Arc::new(runtime_config));
    info!("Runtime configuration initialized (hot reload enabled via SIGHUP)");
    
    // グローバルプロキシキャッシュの初期化
    // デフォルト設定でグローバルキャッシュを初期化（各ルートのcache設定で有効化される）
    let global_cache_config = cache::CacheConfig {
        enabled: true,
        max_memory_size: 100 * 1024 * 1024, // 100MB
        disk_path: None,
        max_disk_size: 1024 * 1024 * 1024, // 1GB
        memory_threshold: 64 * 1024, // 64KB
        default_ttl_secs: 300, // 5分
        ..Default::default()
    };
    
    match cache::init_global_cache(global_cache_config) {
        Ok(()) => {
            info!("Global proxy cache initialized (max_memory=100MB, default_ttl=300s)");
        }
        Err(e) => {
            warn!("Failed to initialize global cache: {}", e);
        }
    }
    
    // HTTP/2・HTTP/3 の設定ログ
    if loaded_config.http2_enabled {
        info!("HTTP/2 enabled via ALPN negotiation");
    }
    if loaded_config.http3_enabled {
        info!("HTTP/3 enabled (UDP listener: {})", 
              loaded_config.http3_listen.as_deref().unwrap_or(&loaded_config.listen_addr));
    }

    let hostname = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "unknown".to_string());
    
    let num_threads = loaded_config.num_threads;
    
    info!("============================================");
    info!("High-Performance Reverse Proxy Server");
    info!("Config File: {}", config_path.display());
    info!("Hostname: {}", hostname);
    info!("Listen Address: {}", listen_addr);
    info!("Threads: {} (CPU cores: {})", num_threads, num_cpus::get());
    info!("CPU Affinity: Enabled (pinning workers to cores)");
    info!("Reuseport Balancing: {:?}", loaded_config.reuseport_balancing);
    info!("Read Timeout: {:?}", READ_TIMEOUT);
    info!("Write Timeout: {:?}", WRITE_TIMEOUT);
    info!("Connect Timeout: {:?}", CONNECT_TIMEOUT);
    info!("Idle Timeout: {:?}", IDLE_TIMEOUT);
    
    // ログ設定のログ出力
    info!("Logging: level={}, channel_size={}, flush_interval={}ms",
          loaded_config.logging.level,
          loaded_config.logging.channel_size,
          loaded_config.logging.flush_interval_ms);
    if let Some(ref file_path) = loaded_config.logging.file_path {
        info!("Logging: output to file '{}'", file_path);
    } else {
        info!("Logging: output to stderr (async buffered via ftlog)");
    }
    
    // kTLS設定のログ出力
    log_ktls_status(&ktls_config);
    
    info!("============================================");

    // Graceful Shutdown用のシグナルハンドラを設定
    setup_signal_handler();
    
    // 設定リロードスレッドを起動（SIGHUP で設定を動的更新）
    spawn_reload_thread();
    
    // 健康チェックスレッドを起動（Upstream の健康状態を監視）
    spawn_health_check_thread();

    let mut handles = Vec::with_capacity(num_threads);

    // CPUアフィニティ設定のためのコアID取得
    let core_ids = core_affinity::get_core_ids();
    let core_ids_available = core_ids.as_ref().map(|ids| ids.len()).unwrap_or(0);
    
    if core_ids.is_some() && core_ids_available > 0 {
        info!("CPU Affinity: {} cores available, pinning {} worker threads", 
              core_ids_available, num_threads);
    } else {
        warn!("CPU Affinity: Could not detect core IDs, workers will not be pinned");
    }

    // SO_REUSEPORT振り分け設定
    let reuseport_balancing = loaded_config.reuseport_balancing;

    // ====================
    // サンドボックス適用（bubblewrap相当）
    // ====================
    //
    // Linuxのnamespace分離、bind mounts、capabilities制限を適用します。
    // 権限降格やseccomp/Landlockより先に適用します。
    //
    // 適用順序:
    // 1. サンドボックス（namespace分離、bind mounts、capabilities）
    // 2. 権限降格（setuid/setgid）
    // 3. Landlock（ファイルシステム制限）
    // 4. seccomp（システムコール制限）
    // ====================
    
    if loaded_config.global_security.enable_sandbox {
        // サンドボックスサポート状況をレポート
        security::report_sandbox_support();
        
        // サンドボックス設定を構築
        let sandbox_config = build_sandbox_config(&loaded_config.global_security);
        
        match security::apply_sandbox(&sandbox_config) {
            Ok(()) => {
                info!("Sandbox restrictions applied successfully");
                if sandbox_config.unshare_mount {
                    info!("Sandbox: Mount namespace isolated");
                }
                if sandbox_config.unshare_uts {
                    info!("Sandbox: UTS namespace isolated (hostname: {})", 
                          sandbox_config.hostname.as_deref().unwrap_or("default"));
                }
                if sandbox_config.unshare_ipc {
                    info!("Sandbox: IPC namespace isolated");
                }
                if sandbox_config.unshare_pid {
                    info!("Sandbox: PID namespace isolated");
                }
                if !sandbox_config.keep_capabilities.is_empty() {
                    info!("Sandbox: Keeping only capabilities: {:?}", sandbox_config.keep_capabilities);
                } else if !sandbox_config.drop_capabilities.is_empty() {
                    info!("Sandbox: Dropped capabilities: {:?}", sandbox_config.drop_capabilities);
                }
            }
            Err(e) => {
                // サンドボックス適用失敗は警告として扱い、続行する
                // 本番環境ではエラー扱いにすることも検討
                warn!("Failed to apply sandbox restrictions: {} - continuing without sandbox", e);
                warn!("Hint: Sandbox may require root privileges or CAP_SYS_ADMIN");
            }
        }
    }

    // 権限降格（設定されている場合）
    // 注意: 特権ポート（1024未満）を使用する場合は、
    // CAP_NET_BIND_SERVICEケイパビリティを付与するか、
    // 権限降格を無効にする必要があります。
    if let Err(e) = drop_privileges(&loaded_config.global_security) {
        error!("Failed to drop privileges: {}", e);
        return;
    }

    // ====================
    // io_uring / seccomp セキュリティ制限
    // ====================
    //
    // 権限降格後、ワーカースレッド起動前にセキュリティ制限を適用します。
    // これにより、io_uringの悪用リスクを低減します。
    //
    // 注意: seccompはプロセス全体に適用され、不可逆です。
    // ワーカースレッド起動後は新しいスレッドにも自動的に継承されます。
    // ====================
    
    // セキュリティ機能のサポート状況をレポート
    security::report_security_status();
    
    // セキュリティ設定を構築
    let security_config = security::SecurityConfig {
        enable_io_uring_restrictions: false, // monoioでは現在未サポート
        enable_seccomp: loaded_config.global_security.enable_seccomp,
        seccomp_mode: security::SeccompMode::from_str(&loaded_config.global_security.seccomp_mode),
        enable_landlock: loaded_config.global_security.enable_landlock,
        landlock_read_paths: loaded_config.global_security.landlock_read_paths.clone(),
        landlock_write_paths: loaded_config.global_security.landlock_write_paths.clone(),
    };
    
    // セキュリティ制限を適用
    if security_config.enable_seccomp || security_config.enable_landlock {
        match security::apply_security_restrictions(&security_config) {
            Ok(()) => {
                info!("Security restrictions applied successfully");
                if security_config.enable_seccomp {
                    info!("seccomp: mode={:?}", security_config.seccomp_mode);
                }
                if security_config.enable_landlock {
                    info!("Landlock: read_paths={:?}, write_paths={:?}",
                          security_config.landlock_read_paths,
                          security_config.landlock_write_paths);
                }
            }
            Err(e) => {
                // セキュリティ制限の適用失敗は警告として扱い、続行する
                // 本番環境ではエラー扱いにすることも検討
                warn!("Failed to apply security restrictions: {} - continuing without them", e);
            }
        }
    }

    // 同時接続数制限
    let max_connections = loaded_config.global_security.max_concurrent_connections;
    
    for thread_id in 0..num_threads {
        let acceptor_clone = acceptor.clone();
        // 注: host_routes と path_routes は CURRENT_CONFIG から取得するため、ここでは不要
        // ホットリロード時に各接続が最新の設定を参照できるようにする
        let addr = listen_addr;
        let balancing = reuseport_balancing;
        let workers = num_threads;
        let max_conn = max_connections;
        
        // このスレッドに割り当てるコアIDを決定
        // コア数よりスレッド数が多い場合はモジュロ演算でラップアラウンド
        let assigned_core = core_ids.as_ref().map(|ids| {
            let core_index = thread_id % ids.len();
            ids[core_index]
        });

        let handle = thread::spawn(move || {
            // スレッド開始直後にCPUアフィニティを設定
            // これによりL1/L2キャッシュミスを削減し、レイテンシのジッターを安定化
            if let Some(core_id) = assigned_core {
                if core_affinity::set_for_current(core_id) {
                    info!("[Thread {}] Pinned to CPU core {:?}", thread_id, core_id);
                } else {
                    warn!("[Thread {}] Failed to pin to CPU core {:?}, running unpinned", 
                          thread_id, core_id);
                }
            }
            
            let mut rt = RuntimeBuilder::<monoio::IoUringDriver>::new()
                .enable_timer()
                .build()
                .expect("Failed to create runtime");
            rt.block_on(async move {
                let listener = match create_listener(addr, balancing, workers, thread_id) {
                    Ok(l) => l,
                    Err(e) => {
                        error!("[Thread {}] Bind error: {}", thread_id, e);
                        return;
                    }
                };

                info!("[Thread {}] Worker started", thread_id);

                loop {
                    // Shutdown チェック
                    if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                        info!("[Thread {}] Shutting down...", thread_id);
                        break;
                    }

                    // タイムアウト付きaccept（Graceful Shutdown対応）
                    let accept_result = timeout(Duration::from_secs(1), listener.accept()).await;
                    
                    let (stream, peer_addr) = match accept_result {
                        Ok(Ok(s)) => s,
                        Ok(Err(e)) => {
                            error!("[Thread {}] Accept error: {}", thread_id, e);
                            continue;
                        }
                        Err(_) => {
                            // タイムアウト - ループを継続してshutdownチェック
                            continue;
                        }
                    };
                    
                    // 同時接続数制限チェック
                    if max_conn > 0 {
                        let current = CURRENT_CONNECTIONS.load(Ordering::Relaxed);
                        if current >= max_conn {
                            warn!("[Thread {}] Connection limit reached ({}/{}), rejecting connection from {}", 
                                  thread_id, current, max_conn, peer_addr);
                            drop(stream);
                            continue;
                        }
                    }
                    
                    // 接続カウンター増加
                    CURRENT_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
                    
                    let _ = stream.set_nodelay(true);
                    
                    let acceptor = acceptor_clone.clone();
                    
                    monoio::spawn(async move {
                        // handle_connection 内で CURRENT_CONFIG から最新の設定を取得
                        // これによりホットリロード時に新しい設定が即座に反映される
                        handle_connection(stream, acceptor, peer_addr).await;
                        // 接続カウンター減少
                        CURRENT_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
                    });
                }
                
                info!("[Thread {}] Worker stopped", thread_id);
            });
        });
        handles.push(handle);
    }

    // HTTP to HTTPS リダイレクトワーカー（設定されている場合のみ）
    if let Some(http_addr) = loaded_config.listen_http_addr {
        info!("============================================");
        info!("HTTP to HTTPS Redirect Server");
        info!("HTTP Listen Address: {}", http_addr);
        info!("All HTTP requests will be redirected to HTTPS (301)");
        info!("============================================");
        
        let http_handle = thread::spawn(move || {
            let mut rt = RuntimeBuilder::<monoio::IoUringDriver>::new()
                .enable_timer()
                .build()
                .expect("Failed to create HTTP runtime");
            
            rt.block_on(async move {
                // HTTPリスナーを作成（SO_REUSEADDRを有効化）
                let listener = match TcpListener::bind(http_addr) {
                    Ok(l) => l,
                    Err(e) => {
                        error!("[HTTP] Bind error on {}: {}", http_addr, e);
                        return;
                    }
                };
                
                info!("[HTTP] Redirect worker started");
                
                loop {
                    // Shutdown チェック
                    if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                        info!("[HTTP] Shutting down...");
                        break;
                    }
                    
                    // タイムアウト付きaccept
                    let accept_result = timeout(Duration::from_secs(1), listener.accept()).await;
                    
                    let (stream, _peer_addr) = match accept_result {
                        Ok(Ok(s)) => s,
                        Ok(Err(e)) => {
                            error!("[HTTP] Accept error: {}", e);
                            continue;
                        }
                        Err(_) => {
                            // タイムアウト - ループを継続してshutdownチェック
                            continue;
                        }
                    };
                    
                    let _ = stream.set_nodelay(true);
                    
                    // 軽量なリダイレクト処理をspawn
                    monoio::spawn(async move {
                        handle_http_redirect(stream).await;
                    });
                }
                
                info!("[HTTP] Redirect worker stopped");
            });
        });
        handles.push(http_handle);
    }

    // HTTP/3 (QUIC/UDP) サーバー（設定されている場合のみ）
    // TCP側と同様に複数スレッドで並列起動し、CPUコアにピンニング
    // 
    // 注意: quicheはファイルパスからの証明書読み込みのみサポートしているため、
    // HTTP/3を使用する場合はLandlock設定で証明書パスを許可する必要があります。
    #[cfg(feature = "http3")]
    if loaded_config.http3_enabled {
        let http3_addr_str = loaded_config.http3_listen
            .clone()
            .unwrap_or_else(|| loaded_config.listen_addr.clone());
        
        let http3_addr: SocketAddr = match http3_addr_str.parse() {
            Ok(addr) => addr,
            Err(e) => {
                error!("Invalid HTTP/3 listen address '{}': {}", http3_addr_str, e);
                return;
            }
        };
        
        // TLS証明書パス
        let tls_cert_path = loaded_config.tls_cert_path.clone();
        let tls_key_path = loaded_config.tls_key_path.clone();
        
        // TLS証明書データ（事前読み込み済み、memfd経由でquicheに渡す）
        let tls_cert_pem = loaded_config.tls_cert_pem.clone();
        let tls_key_pem = loaded_config.tls_key_pem.clone();
        
        // Landlock有効時の情報: memfd経由で証明書をロードするため、
        // ファイルパスをlandlock_read_pathsに追加する必要はない
        if loaded_config.global_security.enable_landlock {
            info!("[HTTP/3] Landlock enabled - using memfd for certificate loading");
            info!("[HTTP/3] No need to add certificate paths to landlock_read_paths");
        }
        
        info!("============================================");
        info!("HTTP/3 (QUIC/UDP) Server");
        info!("HTTP/3 Listen Address: {} (UDP)", http3_addr);
        info!("HTTP/3 Workers: {} (SO_REUSEPORT enabled)", num_threads);
        info!("TLS Cert: {} (pre-loaded, {} bytes)", tls_cert_path, tls_cert_pem.len());
        info!("TLS Key: {} (pre-loaded, {} bytes)", tls_key_path, tls_key_pem.len());
        info!("TLS loading method: memfd (Landlock compatible)");
        info!("============================================");
        
        // TCP側と同様に複数スレッドで起動（SO_REUSEPORTでパケット分散）
        for thread_id in 0..num_threads {
            let cert_pem = tls_cert_pem.clone();
            let key_pem = tls_key_pem.clone();
            let addr = http3_addr;
            
            // CPUコアにピンニング
            let assigned_core = core_ids.as_ref().map(|ids| {
                let core_index = thread_id % ids.len();
                ids[core_index]
            });
            
            let http3_handle = thread::spawn(move || {
                // スレッド開始直後にCPUアフィニティを設定
                if let Some(core_id) = assigned_core {
                    if core_affinity::set_for_current(core_id) {
                        info!("[HTTP/3 Worker {}] Pinned to CPU core {:?}", thread_id, core_id);
                    } else {
                        warn!("[HTTP/3 Worker {}] Failed to pin to CPU core {:?}", thread_id, core_id);
                    }
                }
                
                // memfd経由で証明書をロード（Landlock対応）
                // 事前読み込み済みのPEMデータをmemfdに書き込み、
                // /proc/self/fd/<fd>パス経由でquicheに渡す
                // 
                // セキュリティ: Vec をクローンした後、Arc を即座にドロップして
                // メインスレッドでのゼロ化を可能にする
                let cert_data = (*cert_pem).clone();
                let key_data = (*key_pem).clone();
                
                // Arc 参照を即座にドロップ（参照カウントを減らす）
                drop(cert_pem);
                drop(key_pem);
                
                let config = http3_server::Http3ServerConfig {
                    cert_path: String::new(),  // memfd使用時は不要
                    key_path: String::new(),   // memfd使用時は不要
                    cert_pem: Some(cert_data),
                    key_pem: Some(key_data),
                    ..Default::default()
                };
                
                info!("[HTTP/3 Worker {}] Starting...", thread_id);
                
                if let Err(e) = http3_server::run_http3_server(addr, config) {
                    error!("[HTTP/3 Worker {}] Server error: {}", thread_id, e);
                }
                
                info!("[HTTP/3 Worker {}] Stopped", thread_id);
            });
            handles.push(http3_handle);
        }
        
        // ローカル変数の Arc をドロップ（参照カウントを減らす）
        drop(tls_cert_pem);
        drop(tls_key_pem);
    }
    
    // HTTP/3 ワーカーが証明書データをクローンするまで短時間待機
    // その後、LoadedConfig の証明書データをセキュアにゼロ化
    #[cfg(feature = "http3")]
    if loaded_config.http3_enabled {
        // ワーカースレッドが Arc 参照をドロップするまで少し待機
        std::thread::sleep(std::time::Duration::from_millis(100));
        
        // LoadedConfig の証明書データをセキュアにゼロ化
        secure_clear_arc_vec(&mut loaded_config.tls_cert_pem, "TLS certificate (LoadedConfig)");
        secure_clear_arc_vec(&mut loaded_config.tls_key_pem, "TLS private key (LoadedConfig)");
        
        info!("[Security] Pre-loaded TLS credentials have been securely cleared from memory");
    }

    for handle in handles {
        let _ = handle.join();
    }
    
    info!("Server shutdown complete");
}

/// kTLSの状態をログ出力
fn log_ktls_status(ktls_config: &KtlsConfig) {
    if ktls_config.enabled {
        // rustls + ktls2 使用時
        #[cfg(feature = "ktls")]
        {
            if ktls_rustls::is_ktls_available() {
                info!("kTLS: Enabled via rustls + ktls2 (TX={}, RX={})", ktls_config.enable_tx, ktls_config.enable_rx);
                info!("kTLS: Kernel TLS offload active - reduced CPU usage expected");
                if ktls_config.fallback_enabled {
                    info!("kTLS: Fallback to rustls enabled (graceful degradation)");
                } else {
                    info!("kTLS: Fallback disabled (kTLS required, connections will fail if unavailable)");
                }
            } else {
                warn!("kTLS: Requested but kernel support not available");
                warn!("kTLS: Ensure 'modprobe tls' has been run and kernel 5.15+ is used");
                if ktls_config.fallback_enabled {
                    warn!("kTLS: Falling back to userspace TLS via rustls");
                } else {
                    error!("kTLS: Fallback disabled but kTLS unavailable - connections will fail!");
                    error!("kTLS: Either enable fallback or run 'modprobe tls'");
                }
            }
        }
        // kTLS フィーチャー無効時
        #[cfg(not(feature = "ktls"))]
        {
            warn!("kTLS: Enabled in config but ktls feature is not enabled");
            warn!("kTLS: Rebuild with: cargo build --features ktls for kTLS support");
        }
    } else {
        info!("kTLS: Disabled (using userspace TLS via rustls)");
    }
}

/// シグナルハンドラのセットアップ
fn setup_signal_handler() {
    // SIGINT, SIGTERM をキャッチしてシャットダウンフラグを設定
    ctrlc::set_handler(move || {
        info!("Received shutdown signal, initiating graceful shutdown...");
        SHUTDOWN_FLAG.store(true, Ordering::SeqCst);
    }).expect("Failed to set signal handler");
    
    // SIGHUP をキャッチして設定リロードをトリガー（Linux/Unix）
    #[cfg(unix)]
    {
        use signal_hook::consts::SIGHUP;
        use signal_hook::flag as signal_flag;
        
        // SIGHUP で RELOAD_FLAG を true に設定
        // signal-hook はシグナルセーフな方法でフラグを更新
        if let Err(e) = signal_flag::register(SIGHUP, Arc::clone(&RELOAD_FLAG)) {
            warn!("Failed to register SIGHUP handler: {}", e);
        } else {
            info!("SIGHUP handler registered for configuration hot reload");
        }
    }
}

/// 設定リロードスレッドを起動
/// 
/// RELOAD_FLAG を監視し、シグナルを受け取ったら設定をリロードします。
/// ワーカースレッドは CURRENT_CONFIG を参照するため、
/// リロード後の新規リクエストは自動的に新しい設定を使用します。
fn spawn_reload_thread() {
    thread::spawn(move || {
        info!("Configuration reload thread started");
        
        loop {
            thread::sleep(Duration::from_millis(500));
            
            // シャットダウン中はリロードしない
            if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                break;
            }
            
            // リロードフラグをチェック
            if RELOAD_FLAG.swap(false, Ordering::SeqCst) {
                info!("SIGHUP received, reloading configuration...");
                
                // グローバル変数から設定ファイルパスを取得
                let config_path = CONFIG_PATH.load();
                
                match reload_config(&config_path) {
                    Ok(()) => {
                        info!("Configuration reloaded successfully");
                        info!("New requests will use updated routes");
                    }
                    Err(e) => {
                        error!("Failed to reload configuration: {}", e);
                        error!("Keeping previous configuration");
                    }
                }
            }
        }
        
        info!("Configuration reload thread stopped");
    });
}

/// 健康チェックスレッドを起動
/// 
/// CURRENT_CONFIG から Upstream グループを取得し、
/// 設定された間隔で各サーバーにHTTPリクエストを送信してヘルスをチェック。
fn spawn_health_check_thread() {
    thread::spawn(move || {
        info!("Health check thread started");
        
        loop {
            // シャットダウン中はチェックしない
            if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                break;
            }
            
            // 設定を取得
            let config = CURRENT_CONFIG.load();
            
            // 各 Upstream グループをチェック
            for (name, group) in config.upstream_groups.iter() {
                if let Some(ref hc_config) = group.health_check {
                    // 各サーバーをチェック
                    for server in &group.servers {
                        if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                            break;
                        }
                        
                        let target = &server.target;
                        let addr = format!("{}:{}", target.host, target.port);
                        
                        // 同期的な TCP 接続でヘルスチェック
                        let check_result = perform_health_check(
                            &addr,
                            &target.host,
                            &hc_config.path,
                            target.use_tls,
                            Duration::from_secs(hc_config.timeout_secs),
                            &hc_config.healthy_statuses,
                        );
                        
                        if check_result {
                            server.record_success(hc_config.healthy_threshold);
                        } else {
                            server.record_failure(hc_config.unhealthy_threshold);
                            ftlog::debug!("Health check failed for {} (upstream: {})", addr, name);
                        }
                    }
                }
            }
            
            // 次のチェックまで待機（最短間隔を使用）
            // シャットダウン時に迅速に終了するため、短い間隔で分割してスリープ
            let min_interval = config.upstream_groups.values()
                .filter_map(|g| g.health_check.as_ref())
                .map(|hc| hc.interval_secs)
                .min()
                .unwrap_or(10);
            
            // 500ms間隔でシャットダウンフラグをチェック
            let sleep_iterations = (min_interval * 2) as usize; // 500ms × 2 = 1秒
            for _ in 0..sleep_iterations {
                if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                    break;
                }
                thread::sleep(Duration::from_millis(500));
            }
        }
        
        info!("Health check thread stopped");
    });
}

/// 同期的な健康チェックを実行
/// 
/// TCP 接続して HTTP GET リクエストを送信し、レスポンスをチェック。
fn perform_health_check(
    addr: &str,
    host: &str,
    path: &str,
    _use_tls: bool,  // TODO: TLS バックエンドの健康チェック
    timeout: Duration,
    healthy_statuses: &[u16],
) -> bool {
    use std::net::TcpStream as StdTcpStream;
    use std::io::{Read, Write};
    
    // TCP 接続
    let mut stream = match StdTcpStream::connect_timeout(
        &addr.parse().unwrap_or_else(|_| std::net::SocketAddr::from(([127, 0, 0, 1], 80))),
        timeout,
    ) {
        Ok(s) => s,
        Err(_) => return false,
    };
    
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    
    // HTTP リクエスト送信
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: HealthCheck/1.0\r\n\r\n",
        path, host
    );
    
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    
    // レスポンス読み取り
    let mut response = [0u8; 1024];
    let n = match stream.read(&mut response) {
        Ok(n) if n > 0 => n,
        _ => return false,
    };
    
    // ステータスコードを抽出
    let response_str = String::from_utf8_lossy(&response[..n]);
    if let Some(status_line) = response_str.lines().next() {
        // "HTTP/1.1 200 OK" のようなパターン
        let parts: Vec<&str> = status_line.split_whitespace().collect();
        if parts.len() >= 2 {
            if let Ok(status_code) = parts[1].parse::<u16>() {
                return healthy_statuses.contains(&status_code);
            }
        }
    }
    
    false
}

// ====================
// SO_REUSEPORT CBPF ロードバランシング
// ====================

/// CBPFプログラムがアタッチ済みかどうかを追跡するグローバルカウンター
/// 最初のワーカーのみがCBPFプログラムをアタッチする
#[cfg(target_os = "linux")]
static CBPF_ATTACHED: AtomicUsize = AtomicUsize::new(0);

/// クライアントIPハッシュに基づく振り分けCBPFプログラムを生成
/// 
/// このBPFプログラムは、accept()時に呼び出され、
/// クライアントのソースIPアドレスをハッシュしてワーカーインデックスを返す
/// 
/// # 引数
/// * `num_workers` - ワーカースレッド数
/// 
/// # 戻り値
/// BPF命令列（sock_filter配列）
#[cfg(target_os = "linux")]
fn create_reuseport_cbpf_program(num_workers: u32) -> Vec<libc::sock_filter> {
    // BPF命令セット:
    // 1. ソースIPアドレスを取得（sk_reuseport_mdからオフセット0でソースIPを読み取り）
    // 2. ワーカー数でmod演算
    // 3. 結果をソケットインデックスとして返す
    //
    // BPF_LD + BPF_W + BPF_ABS: 32ビットワードをパケットから絶対オフセットで読み込み
    // BPF_ALU + BPF_MOD + BPF_K: 即値でmod演算
    // BPF_RET + BPF_A: Aレジスタの値を返す
    vec![
        // LD A, [0]: ソースIPアドレスを読み込み（sk_reuseport_md構造体のオフセット0）
        libc::sock_filter {
            code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            jt: 0,
            jf: 0,
            k: 0, // remote_ip4 のオフセット
        },
        // ALU MOD #num_workers: A = A % num_workers
        libc::sock_filter {
            code: (libc::BPF_ALU | libc::BPF_MOD | libc::BPF_K) as u16,
            jt: 0,
            jf: 0,
            k: num_workers,
        },
        // RET A: Aレジスタの値（ソケットインデックス）を返す
        libc::sock_filter {
            code: (libc::BPF_RET | libc::BPF_A) as u16,
            jt: 0,
            jf: 0,
            k: 0,
        },
    ]
}

/// CBPFプログラムをソケットにアタッチする
/// 
/// SO_ATTACH_REUSEPORT_CBPF を使用して、クライアントIPベースの
/// 振り分けロジックをカーネルに設定する
/// 
/// # 引数
/// * `fd` - ソケットファイルディスクリプタ
/// * `num_workers` - ワーカースレッド数
/// 
/// # 戻り値
/// 成功時はOk(()), 失敗時はエラー
#[cfg(target_os = "linux")]
fn attach_reuseport_cbpf(fd: i32, num_workers: usize) -> io::Result<()> {
    let program = create_reuseport_cbpf_program(num_workers as u32);
    
    #[repr(C)]
    struct SockFprog {
        len: u16,
        filter: *const libc::sock_filter,
    }
    
    let prog = SockFprog {
        len: program.len() as u16,
        filter: program.as_ptr(),
    };
    
    // SO_ATTACH_REUSEPORT_CBPF の値（Linux 4.5+）
    // include/uapi/asm-generic/socket.h: #define SO_ATTACH_REUSEPORT_CBPF 51
    const SO_ATTACH_REUSEPORT_CBPF: libc::c_int = 51;
    
    let result = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            SO_ATTACH_REUSEPORT_CBPF,
            &prog as *const _ as *const libc::c_void,
            std::mem::size_of::<SockFprog>() as libc::socklen_t,
        )
    };
    
    if result < 0 {
        let err = io::Error::last_os_error();
        warn!("Failed to attach CBPF program: {} (errno: {})", err, err.raw_os_error().unwrap_or(-1));
        return Err(err);
    }
    
    Ok(())
}

/// リスナーソケットを作成する（SO_REUSEPORT + オプションのCBPF振り分け）
/// 
/// # 引数
/// * `addr` - バインドするアドレス
/// * `balancing` - 振り分け方式
/// * `num_workers` - ワーカースレッド数（CBPF使用時に必要）
/// * `worker_id` - このワーカーのID（最初のワーカーがCBPFをアタッチ）
fn create_listener(
    addr: SocketAddr,
    #[allow(unused_variables)] balancing: ReuseportBalancing,
    #[allow(unused_variables)] num_workers: usize,
    #[allow(unused_variables)] worker_id: usize,
) -> io::Result<TcpListener> {
    let config = monoio::net::ListenerConfig::default()
        .reuse_port(true)
        .backlog(8192);
    let listener = TcpListener::bind_with_config(addr, &config)?;
    
    // Linux環境でCBPF振り分けが有効な場合、最初のワーカーのみCBPFプログラムをアタッチ
    // 後続のワーカーはreuseportグループに参加し、自動的にBPFプログラムを継承する
    #[cfg(target_os = "linux")]
    if balancing == ReuseportBalancing::Cbpf {
        // CAS操作で最初の1回だけアタッチを実行
        let prev = CBPF_ATTACHED.compare_exchange(
            0,
            1,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        
        if prev.is_ok() {
            // このワーカーが最初にリスナーを作成した
            let fd = listener.as_raw_fd();
            match attach_reuseport_cbpf(fd, num_workers) {
                Ok(()) => {
                    info!("[Worker {}] CBPF reuseport load balancing enabled (client IP hash -> {} workers)", 
                          worker_id, num_workers);
                }
                Err(e) => {
                    // CBPFアタッチに失敗した場合はカーネルデフォルトにフォールバック
                    warn!("[Worker {}] CBPF attach failed, falling back to kernel default: {}", 
                          worker_id, e);
                    // フラグをリセットして他のワーカーも試行できるようにする（オプション）
                    // CBPF_ATTACHED.store(0, Ordering::SeqCst);
                }
            }
        }
    }
    
    Ok(listener)
}

// ====================
// 接続処理
// ====================

// ====================
// 共通セキュリティチェック（HTTP/1.1, HTTP/2, HTTP/3 共用）
// ====================
//
// プロトコル非依存のセキュリティチェック関数群。
// 各プロトコルハンドラーから呼び出されます。

/// セキュリティチェック結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityCheckResult {
    /// 許可（処理を継続）
    Allowed,
    /// IP拒否（403 Forbidden）
    IpDenied,
    /// メソッド不許可（405 Method Not Allowed）
    MethodNotAllowed,
    /// レート制限超過（429 Too Many Requests）
    RateLimitExceeded,
    /// リクエストサイズ超過（413 Request Entity Too Large）
    RequestTooLarge,
}

impl SecurityCheckResult {
    /// HTTPステータスコードに変換
    #[inline]
    pub fn status_code(&self) -> u16 {
        match self {
            SecurityCheckResult::Allowed => 200,
            SecurityCheckResult::IpDenied => 403,
            SecurityCheckResult::MethodNotAllowed => 405,
            SecurityCheckResult::RateLimitExceeded => 429,
            SecurityCheckResult::RequestTooLarge => 413,
        }
    }
    
    /// エラーメッセージを取得
    #[inline]
    pub fn message(&self) -> &'static [u8] {
        match self {
            SecurityCheckResult::Allowed => b"OK",
            SecurityCheckResult::IpDenied => b"Forbidden",
            SecurityCheckResult::MethodNotAllowed => b"Method Not Allowed",
            SecurityCheckResult::RateLimitExceeded => b"Too Many Requests",
            SecurityCheckResult::RequestTooLarge => b"Request Entity Too Large",
        }
    }
}

/// 統合セキュリティチェック（すべてのプロトコル共用）
/// 
/// ## チェック項目
/// 1. IP制限（allowed_ips, denied_ips）
/// 2. HTTPメソッド制限（allowed_methods）
/// 3. レートリミット（rate_limit_requests_per_min）
/// 4. ボディサイズ制限（max_request_body_size）
/// 
/// ## パフォーマンス
/// 設定がデフォルトの場合、has_security_checks() で早期リターンし、
/// オーバーヘッドを最小化。
#[inline]
fn check_security(
    security: &SecurityConfig,
    client_ip: &str,
    method: &[u8],
    content_length: usize,
    is_chunked: bool,
) -> SecurityCheckResult {
    // IP制限チェック
    let ip_filter = security.ip_filter();
    if ip_filter.is_configured() && !ip_filter.is_allowed(client_ip) {
        return SecurityCheckResult::IpDenied;
    }
    
    // 許可メソッドチェック
    if !security.allowed_methods.is_empty() {
        let method_str = std::str::from_utf8(method).unwrap_or("GET");
        let is_allowed = security.allowed_methods.iter()
            .any(|m| m.eq_ignore_ascii_case(method_str));
        if !is_allowed {
            return SecurityCheckResult::MethodNotAllowed;
        }
    }
    
    // レートリミットチェック
    if security.rate_limit_requests_per_min > 0 {
        if !check_rate_limit(client_ip, security.rate_limit_requests_per_min) {
            return SecurityCheckResult::RateLimitExceeded;
        }
    }
    
    // ボディサイズ制限（chunked以外）
    if !is_chunked && content_length > security.max_request_body_size {
        return SecurityCheckResult::RequestTooLarge;
    }
    
    SecurityCheckResult::Allowed
}

// ====================
// HTTP/2 ハンドラー
// ====================
//
// HTTP/2 (RFC 7540) 接続を処理します。
// ALPN ネゴシエーションで h2 が選択された場合に呼び出されます。
// HTTP/1.1 と同等のセキュリティ機能とルーティングをサポート。

/// I/Oエラーが接続終了を示すものかどうかを判定
/// 
/// kTLSを使用している場合、クライアントが正常に接続を閉じた後でも
/// 次のフレーム読み込み時に以下のエラーが発生することがあります:
/// 
/// - EIO (os error 5): Input/output error - kTLS特有のエラー
///   kTLSではTLSレコードの処理がカーネル空間で行われるため、
///   クライアントがTLS close_notifyを送信せずに接続を閉じた場合や、
///   タイミングによってこのエラーが発生します。
/// - ConnectionReset: 接続がリセットされた（RST受信）
/// - BrokenPipe: パイプが壊れた（相手側が閉じた後の書き込み試行）
/// - UnexpectedEof: 予期しないEOF（相手側が閉じた）
/// - ConnectionAborted: 接続が中断された
/// 
/// **重要**: これらのエラーはクライアントが接続を閉じた場合の正常な動作であり、
/// サーバー側の問題ではありません。リクエスト処理は正常に完了しています。
/// ログには警告として出力しますが、接続は正常終了として扱います。
#[cfg(feature = "http2")]
#[inline]
fn is_connection_closed_error(e: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    
    match e.kind() {
        ErrorKind::ConnectionReset => true,
        ErrorKind::BrokenPipe => true,
        ErrorKind::UnexpectedEof => true,
        ErrorKind::ConnectionAborted => true,
        _ => {
            // kTLS使用時のEIO (os error 5) をチェック
            // これはkTLS特有の動作であり、クライアントが接続を閉じた後に
            // 次のフレームを読み込もうとした際に発生します。
            // リクエスト処理自体は正常に完了しているため、問題ありません。
            if let Some(os_error) = e.raw_os_error() {
                // EIO = 5 (Linux)
                os_error == 5
            } else {
                false
            }
        }
    }
}

/// HTTP/2 リクエストを処理
/// 
/// HTTP/2 コネクションのメインループを実行し、各ストリームのリクエストを処理します。
/// HTTP/1.1 と同等のセキュリティチェック、ルーティング、プロキシ機能をサポート。
#[cfg(feature = "http2")]
async fn handle_http2_connection<S>(
    tls_stream: S,
    host_routes: &Arc<HashMap<Box<[u8]>, Backend>>,
    path_routes: &Arc<HashMap<Box<[u8]>, SortedPathMap>>,
    client_ip: &str,
) where
    S: monoio::io::AsyncReadRent + monoio::io::AsyncWriteRentExt + Unpin,
{
    use http2::Http2Connection;
    
    // HTTP/2 設定をCURRENT_CONFIGから取得（ホットリロード対応）
    let config = CURRENT_CONFIG.load();
    let settings = config.http2_config.to_http2_settings();
    
    // HTTP/2 コネクションを作成
    let mut conn = Http2Connection::new(tls_stream, settings);
    
    // ハンドシェイク（プリフェース確認 + SETTINGS 交換）
    if let Err(e) = conn.handshake().await {
        warn!("[HTTP/2] Handshake error: {}", e);
        return;
    }
    
    info!("[HTTP/2] Connection established from {}", client_ip);
    
    // カスタムリクエストハンドラーを使用してメインループ実行
    let result = handle_http2_requests(&mut conn, host_routes, path_routes, client_ip).await;
    
    if let Err(e) = result {
        warn!("[HTTP/2] Connection error: {}", e);
    }
    
    info!("[HTTP/2] Connection closed from {}", client_ip);
}

/// HTTP/2 メインループ（カスタムリクエスト処理）
#[cfg(feature = "http2")]
async fn handle_http2_requests<S>(
    conn: &mut http2::Http2Connection<S>,
    host_routes: &Arc<HashMap<Box<[u8]>, Backend>>,
    path_routes: &Arc<HashMap<Box<[u8]>, SortedPathMap>>,
    client_ip: &str,
) -> Result<(), http2::Http2Error>
where
    S: monoio::io::AsyncReadRent + monoio::io::AsyncWriteRentExt + Unpin,
{
    use http2::Http2Error;
    use std::io;
    
    loop {
        // フレームを読み込み
        let frame = match conn.read_frame().await {
            Ok(f) => f,
            Err(Http2Error::ConnectionClosed) => break,
            Err(Http2Error::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(Http2Error::Io(ref e)) if is_connection_closed_error(e) => {
                // クライアントが接続を閉じた場合に発生するエラー
                // kTLS使用時はEIO (os error 5) が発生することがある
                // 
                // 注意: このエラーはクライアントが正常に接続を閉じた場合の動作であり、
                // サーバー側の問題ではありません。リクエスト処理は正常に完了しています。
                // HTTP/2では、クライアントがレスポンス受信後にGOAWAYを送信せずに
                // 接続を閉じることがあり、その場合に次のフレーム読み込みでこのエラーが発生します。
                warn!(
                    "[HTTP/2] Connection closed by client (expected behavior): {} (client: {})",
                    e, client_ip
                );
                break;
            }
            Err(e) => {
                // その他のエラー時は GOAWAY を送信
                let _ = conn.send_goaway(e.error_code(), e.to_string().as_bytes()).await;
                return Err(e);
            }
        };
        
        // フレームを処理
        match conn.process_frame(frame).await {
            Ok(Some(req)) => {
                // リクエストが完了 - HTTP/1.1と同様のロジックで処理
                let stream_id = req.stream_id;
                
                // ストリーム情報を取得
                let (method, path, authority, body_len) = {
                    if let Some(stream) = conn.get_stream(stream_id) {
                        let method = stream.method().map(|m| m.to_vec()).unwrap_or_else(|| b"GET".to_vec());
                        let path = stream.path().map(|p| p.to_vec()).unwrap_or_else(|| b"/".to_vec());
                        // :authority を取得、見つからない場合は host ヘッダーにフォールバック
                        let authority = stream.authority()
                            .map(|a| a.to_vec())
                            .or_else(|| {
                                // :authority が無い場合は host ヘッダーを確認
                                stream.request_headers.iter()
                                    .find(|h| h.name.eq_ignore_ascii_case(b"host"))
                                    .map(|h| h.value.clone())
                            })
                            .unwrap_or_default();
                        let body_len = stream.request_body.len();
                        (method, path, authority, body_len)
                    } else {
                        continue;
                    }
                };
                
                // 処理時間計測開始
                let start_instant = Instant::now();
                
                // HTTP/2 リクエスト処理
                let result = handle_http2_single_request(
                    conn,
                    stream_id,
                    &method,
                    &path,
                    &authority,
                    body_len,
                    host_routes,
                    path_routes,
                    client_ip,
                ).await;
                
                // メトリクス記録
                let duration = start_instant.elapsed().as_secs_f64();
                let (status, resp_size) = result.unwrap_or((500, 0));
                
                let method_str = std::str::from_utf8(&method).unwrap_or("UNKNOWN");
                let host_str = std::str::from_utf8(&authority).unwrap_or("-");
                record_request_metrics(method_str, host_str, status, body_len as u64, resp_size, duration);
            }
            Ok(None) => {
                // フレーム処理完了、次のフレームへ
            }
            Err(e) => {
                if e.should_goaway() {
                    let _ = conn.send_goaway(e.error_code(), e.to_string().as_bytes()).await;
                    return Err(e);
                } else if let Some(id) = e.rst_stream_id() {
                    let _ = conn.send_rst_stream(id, e.error_code()).await;
                }
            }
        }
        
        // クリーンアップ
        conn.cleanup_closed();
    }
    
    Ok(())
}

/// HTTP/2 単一リクエスト処理
#[cfg(feature = "http2")]
async fn handle_http2_single_request<S>(
    conn: &mut http2::Http2Connection<S>,
    stream_id: u32,
    method: &[u8],
    path: &[u8],
    authority: &[u8],
    body_len: usize,
    host_routes: &Arc<HashMap<Box<[u8]>, Backend>>,
    path_routes: &Arc<HashMap<Box<[u8]>, SortedPathMap>>,
    client_ip: &str,
) -> Option<(u16, u64)>
where
    S: monoio::io::AsyncReadRent + monoio::io::AsyncWriteRentExt + Unpin,
{
    // メトリクスエンドポイントの処理（設定可能なパス）
    {
        let config = CURRENT_CONFIG.load();
        let prom_config = &config.prometheus_config;
        
        let path_str = std::str::from_utf8(path).unwrap_or("/");
        if prom_config.enabled 
            && path_str == prom_config.path 
            && method == b"GET" 
        {
            // IPアドレス制限チェック
            if !prom_config.is_ip_allowed(client_ip) {
                let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
                let _ = conn.send_response(stream_id, 403, headers, Some(b"Forbidden")).await;
                return Some((403, 9));
            }
            
            let body = encode_prometheus_metrics();
            let headers: &[(&[u8], &[u8])] = &[
                (b"content-type", b"text/plain; version=0.0.4; charset=utf-8"),
                (b"server", b"veil/http2"),
            ];
            if let Err(e) = conn.send_response(stream_id, 200, headers, Some(&body)).await {
                warn!("[HTTP/2] Metrics response error: {}", e);
                return None;
            }
            return Some((200, body.len() as u64));
        }
    }
    
    // Backend選択（HTTP/1.1と同じロジック）
    // まず authority でルート検索、見つからない場合は空の authority でデフォルトルートを検索
    let backend_result = find_backend(authority, path, host_routes, path_routes)
        .or_else(|| {
            // authority が空でない場合、デフォルトルートを検索
            if !authority.is_empty() {
                debug!("[HTTP/2] No route found for authority '{}', trying default routes", 
                    String::from_utf8_lossy(authority));
                find_backend(b"", path, host_routes, path_routes)
            } else {
                None
            }
        });
    
    let (prefix, backend) = match backend_result {
        Some(b) => b,
        None => {
            warn!(
                "[HTTP/2] No backend found for authority='{}' path='{}'",
                String::from_utf8_lossy(authority),
                String::from_utf8_lossy(path)
            );
            let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
            let _ = conn.send_response(stream_id, 400, headers, Some(b"Bad Request")).await;
            return Some((400, 11));
        }
    };
    
    // セキュリティチェック（共通関数を使用）
    let security = backend.security();
    let check_result = check_security(security, client_ip, method, body_len, false);
    
    if check_result != SecurityCheckResult::Allowed {
        let status = check_result.status_code();
        let msg = check_result.message();
        let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
        let _ = conn.send_response(stream_id, status, headers, Some(msg)).await;
        return Some((status, msg.len() as u64));
    }
    
    // Accept-Encoding を取得
    let client_encoding = if let Some(stream) = conn.get_stream(stream_id) {
        stream.request_headers.iter()
            .find(|h| h.name.eq_ignore_ascii_case(b"accept-encoding"))
            .map(|h| AcceptedEncoding::parse(&h.value))
            .unwrap_or(AcceptedEncoding::Identity)
    } else {
        AcceptedEncoding::Identity
    };
    
    // Backend処理
    match backend {
        Backend::Proxy(upstream_group, _, compression, _buffering, _cache) => {
            handle_http2_proxy(conn, stream_id, &upstream_group, &compression, client_encoding, method, path, &prefix, client_ip).await
        }
        Backend::MemoryFile(data, mime_type, security) => {
            // ファイル完全一致チェック
            let path_str = std::str::from_utf8(path).unwrap_or("/");
            let prefix_str = std::str::from_utf8(&prefix).unwrap_or("");
            
            let remainder = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
                &path_str[prefix_str.len()..]
            } else {
                ""
            };
            
            let clean_remainder = remainder.trim_matches('/');
            if !clean_remainder.is_empty() {
                let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
                let _ = conn.send_response(stream_id, 404, headers, Some(b"Not Found")).await;
                return Some((404, 9));
            }
            
            let mut headers: Vec<(&[u8], &[u8])> = vec![
                (b"content-type", mime_type.as_bytes()),
                (b"server", b"veil/http2"),
            ];
            
            // セキュリティヘッダー追加
            let security_headers: Vec<(Vec<u8>, Vec<u8>)> = security.add_response_headers.iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
                .collect();
            
            for (k, v) in &security_headers {
                headers.push((k.as_slice(), v.as_slice()));
            }
            
            if let Err(e) = conn.send_response(stream_id, 200, &headers, Some(&data)).await {
                warn!("[HTTP/2] Memory file response error: {}", e);
                return None;
            }
            Some((200, data.len() as u64))
        }
        Backend::SendFile(base_path, is_dir, index_file, security, _cache) => {
            handle_http2_sendfile(conn, stream_id, &base_path, is_dir, index_file.as_deref(), path, &prefix, &security).await
        }
        Backend::Redirect(redirect_url, status_code, preserve_path) => {
            handle_http2_redirect(conn, stream_id, &redirect_url, status_code, preserve_path, path, &prefix).await
        }
    }
}

/// HTTP/2 プロキシ処理（HTTP/1.1バックエンドへ変換）
#[cfg(feature = "http2")]
async fn handle_http2_proxy<S>(
    conn: &mut http2::Http2Connection<S>,
    stream_id: u32,
    upstream_group: &Arc<UpstreamGroup>,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    method: &[u8],
    req_path: &[u8],
    prefix: &[u8],
    client_ip: &str,
) -> Option<(u16, u64)>
where
    S: monoio::io::AsyncReadRent + monoio::io::AsyncWriteRentExt + Unpin,
{
    // サーバー選択
    let server = match upstream_group.select(client_ip) {
        Some(s) => s,
        None => {
            let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
            let _ = conn.send_response(stream_id, 502, headers, Some(b"Bad Gateway")).await;
            return Some((502, 11));
        }
    };
    
    server.acquire();
    let target = &server.target;
    
    // リクエストパス構築
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    let sub_path = if prefix.is_empty() {
        path_str.to_string()
    } else {
        let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
        if path_str.starts_with(prefix_str) {
            let remaining = &path_str[prefix_str.len()..];
            let base = target.path_prefix.trim_end_matches('/');
            
            if remaining.is_empty() {
                if base.is_empty() { "/".to_string() } else { format!("{}/", base) }
            } else if remaining.starts_with('/') {
                if base.is_empty() { remaining.to_string() } else { format!("{}{}", base, remaining) }
            } else {
                if base.is_empty() { format!("/{}", remaining) } else { format!("{}/{}", base, remaining) }
            }
        } else {
            path_str.to_string()
        }
    };
    
    let final_path = if sub_path.is_empty() { "/" } else { &sub_path };
    
    // リクエストボディを取得
    let request_body = if let Some(stream) = conn.get_stream(stream_id) {
        stream.request_body.clone()
    } else {
        Vec::new()
    };
    
    // HTTP/1.1 リクエスト構築
    let mut request = Vec::with_capacity(1024);
    request.extend_from_slice(method);
    request.extend_from_slice(b" ");
    request.extend_from_slice(final_path.as_bytes());
    request.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    request.extend_from_slice(target.host.as_bytes());
    
    if !target.is_default_port() {
        request.extend_from_slice(b":");
        let mut port_buf = itoa::Buffer::new();
        request.extend_from_slice(port_buf.format(target.port).as_bytes());
    }
    
    request.extend_from_slice(b"\r\n");
    
    // リクエストヘッダーを追加（疑似ヘッダー以外）
    if let Some(stream) = conn.get_stream(stream_id) {
        for header in &stream.request_headers {
            // 疑似ヘッダーをスキップ
            if header.name.starts_with(b":") {
                continue;
            }
            // ホップバイホップヘッダーをスキップ
            if header.name.eq_ignore_ascii_case(b"connection") ||
               header.name.eq_ignore_ascii_case(b"keep-alive") ||
               header.name.eq_ignore_ascii_case(b"transfer-encoding") {
                continue;
            }
            request.extend_from_slice(&header.name);
            request.extend_from_slice(b": ");
            request.extend_from_slice(&header.value);
            request.extend_from_slice(b"\r\n");
        }
    }
    
    // Content-Length追加（ボディがある場合）
    if !request_body.is_empty() {
        request.extend_from_slice(b"Content-Length: ");
        let mut len_buf = itoa::Buffer::new();
        request.extend_from_slice(len_buf.format(request_body.len()).as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    
    request.extend_from_slice(b"Connection: keep-alive\r\n\r\n");
    request.extend_from_slice(&request_body);
    
    // バックエンドに接続して転送
    let addr = format!("{}:{}", target.host, target.port);
    let result = if target.use_tls {
        handle_http2_proxy_https(conn, stream_id, &addr, target.sni(), request, compression, client_encoding).await
    } else {
        handle_http2_proxy_http(conn, stream_id, &addr, request, compression, client_encoding).await
    };
    
    server.release();
    result
}

/// HTTP/2 → HTTP/1.1 プロキシ（HTTPバックエンド）
#[cfg(feature = "http2")]
async fn handle_http2_proxy_http<S>(
    conn: &mut http2::Http2Connection<S>,
    stream_id: u32,
    addr: &str,
    request: Vec<u8>,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
) -> Option<(u16, u64)>
where
    S: monoio::io::AsyncReadRent + monoio::io::AsyncWriteRentExt + Unpin,
{
    // バックエンドに接続
    let connect_result = timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await;
    
    let mut backend = match connect_result {
        Ok(Ok(stream)) => {
            let _ = stream.set_nodelay(true);
            stream
        }
        Ok(Err(e)) => {
            warn!("[HTTP/2] Backend connect error: {}", e);
            let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
            let _ = conn.send_response(stream_id, 502, headers, Some(b"Bad Gateway")).await;
            return Some((502, 11));
        }
        Err(_) => {
            let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
            let _ = conn.send_response(stream_id, 504, headers, Some(b"Gateway Timeout")).await;
            return Some((504, 15));
        }
    };
    
    // リクエスト送信
    let (write_res, _) = backend.write_all(request).await;
    if write_res.is_err() {
        let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
        let _ = conn.send_response(stream_id, 502, headers, Some(b"Bad Gateway")).await;
        return Some((502, 11));
    }
    
    // レスポンス受信
    let mut response_buf = Vec::with_capacity(BUF_SIZE);
    
    loop {
        let buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, backend.read(buf)).await;
        
        let (res, mut returned_buf) = match read_result {
            Ok(r) => r,
            Err(_) => {
                let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
                let _ = conn.send_response(stream_id, 504, headers, Some(b"Gateway Timeout")).await;
                return Some((504, 15));
            }
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                break;
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
                let _ = conn.send_response(stream_id, 502, headers, Some(b"Bad Gateway")).await;
                return Some((502, 11));
            }
        };
        
        returned_buf.set_valid_len(n);
        response_buf.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);
        
        // ヘッダーが完了したかチェック
        if let Some(parsed) = parse_http_response(&response_buf) {
            // HTTP/1.1 レスポンスを HTTP/2 に変換
            let status = parsed.status_code;
            let body_start = parsed.header_len;
            let body = &response_buf[body_start..];
            
            // レスポンスヘッダーを解析
            let mut headers_storage = [httparse::EMPTY_HEADER; 64];
            let mut resp = httparse::Response::new(&mut headers_storage);
            let _ = resp.parse(&response_buf);
            
            // Content-Type と Content-Encoding を取得
            let content_type = resp.headers.iter()
                .find(|h| h.name.eq_ignore_ascii_case("content-type"))
                .map(|h| h.value);
            let existing_encoding = resp.headers.iter()
                .find(|h| h.name.eq_ignore_ascii_case("content-encoding"))
                .map(|h| h.value);
            
            // Content-Length が chunked の場合は計算
            let final_body = if parsed.is_chunked {
                // Chunked レスポンスの場合、終端検出しながら読み込み
                let mut decoder = ChunkedDecoder::new_unlimited();
                let mut full_body = body.to_vec();
                decoder.feed(body);
                
                while !decoder.is_complete() {
                    let buf = buf_get();
                    let read_result = timeout(READ_TIMEOUT, backend.read(buf)).await;
                    let (res, mut returned_buf) = match read_result {
                        Ok(r) => r,
                        Err(_) => break,
                    };
                    
                    let n = match res {
                        Ok(0) => { buf_put(returned_buf); break; }
                        Ok(n) => n,
                        Err(_) => { buf_put(returned_buf); break; }
                    };
                    
                    returned_buf.set_valid_len(n);
                    full_body.extend_from_slice(returned_buf.as_valid_slice());
                    decoder.feed(returned_buf.as_valid_slice());
                    buf_put(returned_buf);
                }
                // Chunkedデコード: 生のボディを抽出
                decode_chunked_body(&full_body)
            } else if let Some(content_len) = parsed.content_length {
                // 残りのボディを読む
                let mut full_body = body.to_vec();
                while full_body.len() < content_len {
                    let buf = buf_get();
                    let read_result = timeout(READ_TIMEOUT, backend.read(buf)).await;
                    let (res, mut returned_buf) = match read_result {
                        Ok(r) => r,
                        Err(_) => break,
                    };
                    
                    let n = match res {
                        Ok(0) => { buf_put(returned_buf); break; }
                        Ok(n) => n,
                        Err(_) => { buf_put(returned_buf); break; }
                    };
                    
                    returned_buf.set_valid_len(n);
                    full_body.extend_from_slice(returned_buf.as_valid_slice());
                    buf_put(returned_buf);
                }
                full_body
            } else {
                body.to_vec()
            };
            
            // 圧縮すべきかどうかを判定
            let should_compress = compression.should_compress(
                client_encoding,
                content_type,
                Some(final_body.len()),
                existing_encoding,
            );
            
            // HTTP/2用のヘッダーを構築（ホップバイホップヘッダー除外）
            let mut h2_headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(16);
            h2_headers.push((b"server", b"veil/http2"));
            
            // 圧縮が有効な場合は Content-Encoding を追加
            let encoding_value: Vec<u8>;
            if let Some(enc) = should_compress {
                encoding_value = match enc {
                    AcceptedEncoding::Zstd => b"zstd".to_vec(),
                    AcceptedEncoding::Brotli => b"br".to_vec(),
                    AcceptedEncoding::Gzip => b"gzip".to_vec(),
                    AcceptedEncoding::Deflate => b"deflate".to_vec(),
                    AcceptedEncoding::Identity => Vec::new(),
                };
                if !encoding_value.is_empty() {
                    h2_headers.push((b"content-encoding", &encoding_value));
                    h2_headers.push((b"vary", b"Accept-Encoding"));
                }
            }
            
            for header in resp.headers.iter() {
                if header.name.is_empty() {
                    continue;
                }
                // ホップバイホップヘッダーを除外
                if header.name.eq_ignore_ascii_case("connection") ||
                   header.name.eq_ignore_ascii_case("keep-alive") ||
                   header.name.eq_ignore_ascii_case("transfer-encoding") ||
                   header.name.eq_ignore_ascii_case("upgrade") {
                    continue;
                }
                // 圧縮時は Content-Length と Content-Encoding をスキップ
                if should_compress.is_some() && (
                    header.name.eq_ignore_ascii_case("content-length") ||
                    header.name.eq_ignore_ascii_case("content-encoding")
                ) {
                    continue;
                }
                h2_headers.push((header.name.as_bytes(), header.value));
            }
            
            // 圧縮処理
            let response_body = if let Some(enc) = should_compress {
                compress_body_h2(&final_body, enc, compression)
            } else {
                final_body
            };
            
            // HTTP/2 レスポンス送信
            if let Err(e) = conn.send_response(stream_id, status, &h2_headers, Some(&response_body)).await {
                warn!("[HTTP/2] Response send error: {}", e);
                return None;
            }
            
            return Some((status, response_body.len() as u64));
        }
        
        // ヘッダーが大きすぎる
        if response_buf.len() > MAX_HEADER_SIZE {
            let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
            let _ = conn.send_response(stream_id, 502, headers, Some(b"Bad Gateway")).await;
            return Some((502, 11));
        }
    }
    
    // ストリーム終了（空レスポンス）
    let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
    let _ = conn.send_response(stream_id, 502, headers, Some(b"Bad Gateway")).await;
    Some((502, 11))
}

/// HTTP/2 → HTTP/1.1 プロキシ（HTTPSバックエンド）
#[cfg(feature = "http2")]
async fn handle_http2_proxy_https<S>(
    conn: &mut http2::Http2Connection<S>,
    stream_id: u32,
    addr: &str,
    sni: &str,
    request: Vec<u8>,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
) -> Option<(u16, u64)>
where
    S: monoio::io::AsyncReadRent + monoio::io::AsyncWriteRentExt + Unpin,
{
    // バックエンドに TCP 接続
    let connect_result = timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await;
    
    let backend_tcp = match connect_result {
        Ok(Ok(stream)) => {
            let _ = stream.set_nodelay(true);
            stream
        }
        Ok(Err(e)) => {
            warn!("[HTTP/2] Backend connect error: {}", e);
            let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
            let _ = conn.send_response(stream_id, 502, headers, Some(b"Bad Gateway")).await;
            return Some((502, 11));
        }
        Err(_) => {
            let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
            let _ = conn.send_response(stream_id, 504, headers, Some(b"Gateway Timeout")).await;
            return Some((504, 15));
        }
    };
    
    // TLS ハンドシェイク
    let connector = TLS_CONNECTOR.with(|c| c.clone());
    let tls_result = timeout(CONNECT_TIMEOUT, connector.connect(backend_tcp, sni)).await;
    
    let mut backend = match tls_result {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            warn!("[HTTP/2] TLS handshake error: {}", e);
            let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
            let _ = conn.send_response(stream_id, 502, headers, Some(b"Bad Gateway")).await;
            return Some((502, 11));
        }
        Err(_) => {
            let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
            let _ = conn.send_response(stream_id, 504, headers, Some(b"Gateway Timeout")).await;
            return Some((504, 15));
        }
    };
    
    // リクエスト送信
    let (write_res, _) = backend.write_all(request).await;
    if write_res.is_err() {
        let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
        let _ = conn.send_response(stream_id, 502, headers, Some(b"Bad Gateway")).await;
        return Some((502, 11));
    }
    
    // レスポンス受信（HTTP と同様）
    let mut response_buf = Vec::with_capacity(BUF_SIZE);
    
    loop {
        let buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, backend.read(buf)).await;
        
        let (res, mut returned_buf) = match read_result {
            Ok(r) => r,
            Err(_) => {
                let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
                let _ = conn.send_response(stream_id, 504, headers, Some(b"Gateway Timeout")).await;
                return Some((504, 15));
            }
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                break;
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                break;
            }
        };
        
        returned_buf.set_valid_len(n);
        response_buf.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);
        
        // ヘッダーが完了したかチェック
        if let Some(parsed) = parse_http_response(&response_buf) {
            let status = parsed.status_code;
            let body_start = parsed.header_len;
            let body = &response_buf[body_start..];
            
            // レスポンスヘッダーを解析
            let mut headers_storage = [httparse::EMPTY_HEADER; 64];
            let mut resp = httparse::Response::new(&mut headers_storage);
            let _ = resp.parse(&response_buf);
            
            // Content-Type と Content-Encoding を取得
            let content_type = resp.headers.iter()
                .find(|h| h.name.eq_ignore_ascii_case("content-type"))
                .map(|h| h.value);
            let existing_encoding = resp.headers.iter()
                .find(|h| h.name.eq_ignore_ascii_case("content-encoding"))
                .map(|h| h.value);
            
            // ボディを読む（Chunked対応）
            let final_body = if parsed.is_chunked {
                // Chunked レスポンスの場合、終端検出しながら読み込み
                let mut decoder = ChunkedDecoder::new_unlimited();
                let mut full_body = body.to_vec();
                decoder.feed(body);
                
                while !decoder.is_complete() {
                    let buf = buf_get();
                    let read_result = timeout(READ_TIMEOUT, backend.read(buf)).await;
                    let (res, mut returned_buf) = match read_result {
                        Ok(r) => r,
                        Err(_) => break,
                    };
                    
                    let n = match res {
                        Ok(0) => { buf_put(returned_buf); break; }
                        Ok(n) => n,
                        Err(_) => { buf_put(returned_buf); break; }
                    };
                    
                    returned_buf.set_valid_len(n);
                    full_body.extend_from_slice(returned_buf.as_valid_slice());
                    decoder.feed(returned_buf.as_valid_slice());
                    buf_put(returned_buf);
                }
                // Chunkedデコード: 生のボディを抽出
                decode_chunked_body(&full_body)
            } else if let Some(content_len) = parsed.content_length {
                let mut full_body = body.to_vec();
                while full_body.len() < content_len {
                    let buf = buf_get();
                    let read_result = timeout(READ_TIMEOUT, backend.read(buf)).await;
                    let (res, mut returned_buf) = match read_result {
                        Ok(r) => r,
                        Err(_) => break,
                    };
                    
                    let n = match res {
                        Ok(0) => { buf_put(returned_buf); break; }
                        Ok(n) => n,
                        Err(_) => { buf_put(returned_buf); break; }
                    };
                    
                    returned_buf.set_valid_len(n);
                    full_body.extend_from_slice(returned_buf.as_valid_slice());
                    buf_put(returned_buf);
                }
                full_body
            } else {
                body.to_vec()
            };
            
            // 圧縮すべきかどうかを判定
            let should_compress = compression.should_compress(
                client_encoding,
                content_type,
                Some(final_body.len()),
                existing_encoding,
            );
            
            // HTTP/2用のヘッダーを構築
            let mut h2_headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(16);
            h2_headers.push((b"server", b"veil/http2"));
            
            // 圧縮が有効な場合は Content-Encoding を追加
            let encoding_value: Vec<u8>;
            if let Some(enc) = should_compress {
                encoding_value = match enc {
                    AcceptedEncoding::Zstd => b"zstd".to_vec(),
                    AcceptedEncoding::Brotli => b"br".to_vec(),
                    AcceptedEncoding::Gzip => b"gzip".to_vec(),
                    AcceptedEncoding::Deflate => b"deflate".to_vec(),
                    AcceptedEncoding::Identity => Vec::new(),
                };
                if !encoding_value.is_empty() {
                    h2_headers.push((b"content-encoding", &encoding_value));
                    h2_headers.push((b"vary", b"Accept-Encoding"));
                }
            }
            
            for header in resp.headers.iter() {
                if header.name.is_empty() {
                    continue;
                }
                // ホップバイホップヘッダーを除外
                if header.name.eq_ignore_ascii_case("connection") ||
                   header.name.eq_ignore_ascii_case("keep-alive") ||
                   header.name.eq_ignore_ascii_case("transfer-encoding") {
                    continue;
                }
                // 圧縮時は Content-Length と Content-Encoding をスキップ
                if should_compress.is_some() && (
                    header.name.eq_ignore_ascii_case("content-length") ||
                    header.name.eq_ignore_ascii_case("content-encoding")
                ) {
                    continue;
                }
                h2_headers.push((header.name.as_bytes(), header.value));
            }
            
            // 圧縮処理
            let response_body = if let Some(enc) = should_compress {
                compress_body_h2(&final_body, enc, compression)
            } else {
                final_body
            };
            
            if let Err(e) = conn.send_response(stream_id, status, &h2_headers, Some(&response_body)).await {
                warn!("[HTTP/2] Response send error: {}", e);
                return None;
            }
            
            return Some((status, response_body.len() as u64));
        }
        
        if response_buf.len() > MAX_HEADER_SIZE {
            break;
        }
    }
    
    let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
    let _ = conn.send_response(stream_id, 502, headers, Some(b"Bad Gateway")).await;
    Some((502, 11))
}

/// Chunkedエンコードされたボディをデコードして生のデータを抽出
/// 
/// RFC 7230 Section 4.1に準拠した簡易的なChunkedデコーダ。
/// Transfer-Encoding: chunked 形式のボディから、生のデータを抽出します。
#[cfg(feature = "http2")]
fn decode_chunked_body(chunked_data: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(chunked_data.len());
    let mut pos = 0;
    
    while pos < chunked_data.len() {
        // チャンクサイズを読み取り（16進数）
        let size_start = pos;
        while pos < chunked_data.len() && chunked_data[pos] != b'\r' {
            pos += 1;
        }
        
        if pos >= chunked_data.len() {
            break;
        }
        
        // チャンクサイズを解析
        let size_str = match std::str::from_utf8(&chunked_data[size_start..pos]) {
            Ok(s) => s.trim(),
            Err(_) => break,
        };
        
        // チャンク拡張（;以降）を除去
        let size_str = size_str.split(';').next().unwrap_or(size_str);
        
        let chunk_size = match u64::from_str_radix(size_str, 16) {
            Ok(s) => s as usize,
            Err(_) => break,
        };
        
        // 終端チャンク（サイズ0）
        if chunk_size == 0 {
            break;
        }
        
        // \r\n をスキップ
        pos += 2;
        if pos >= chunked_data.len() {
            break;
        }
        
        // チャンクデータをコピー
        let end = std::cmp::min(pos + chunk_size, chunked_data.len());
        result.extend_from_slice(&chunked_data[pos..end]);
        pos = end;
        
        // チャンク終端の \r\n をスキップ
        if pos + 2 <= chunked_data.len() {
            pos += 2;
        }
    }
    
    result
}

/// HTTP/2 用レスポンスボディ圧縮ヘルパー関数
/// 
/// バイト配列を受け取り、指定されたエンコーディングで圧縮して返します。
/// 圧縮に失敗した場合は元のデータをそのまま返します。
#[cfg(feature = "http2")]
fn compress_body_h2(body: &[u8], encoding: AcceptedEncoding, compression: &CompressionConfig) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    
    match encoding {
        AcceptedEncoding::Zstd => {
            match zstd::encode_all(std::io::Cursor::new(body), compression.zstd_level) {
                Ok(compressed) => compressed,
                Err(_) => body.to_vec(),
            }
        }
        AcceptedEncoding::Gzip => {
            let level = Compression::new(compression.gzip_level);
            let mut encoder = GzEncoder::new(Vec::with_capacity(body.len()), level);
            if encoder.write_all(body).is_err() {
                return body.to_vec();
            }
            encoder.finish().unwrap_or_else(|_| body.to_vec())
        }
        AcceptedEncoding::Brotli => {
            let mut compressed = Vec::with_capacity(body.len());
            let params = brotli::enc::BrotliEncoderParams {
                quality: compression.brotli_level as i32,
                ..Default::default()
            };
            let mut input = std::io::Cursor::new(body);
            if brotli::BrotliCompress(&mut input, &mut compressed, &params).is_err() {
                return body.to_vec();
            }
            compressed
        }
        AcceptedEncoding::Deflate => {
            use flate2::write::DeflateEncoder;
            let level = Compression::new(compression.gzip_level);
            let mut encoder = DeflateEncoder::new(Vec::with_capacity(body.len()), level);
            if encoder.write_all(body).is_err() {
                return body.to_vec();
            }
            encoder.finish().unwrap_or_else(|_| body.to_vec())
        }
        AcceptedEncoding::Identity => body.to_vec(),
    }
}

/// HTTP/2 ファイル配信
#[cfg(feature = "http2")]
async fn handle_http2_sendfile<S>(
    conn: &mut http2::Http2Connection<S>,
    stream_id: u32,
    base_path: &PathBuf,
    is_dir: bool,
    index_file: Option<&str>,
    req_path: &[u8],
    prefix: &[u8],
    security: &SecurityConfig,
) -> Option<(u16, u64)>
where
    S: monoio::io::AsyncReadRent + monoio::io::AsyncWriteRentExt + Unpin,
{
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
    
    // プレフィックス除去後のサブパス
    let sub_path = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
        &path_str[prefix_str.len()..]
    } else {
        path_str
    };
    
    let clean_sub = sub_path.trim_start_matches('/');
    
    // パストラバーサル防止
    if clean_sub.contains("..") {
        let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
        let _ = conn.send_response(stream_id, 403, headers, Some(b"Forbidden")).await;
        return Some((403, 9));
    }
    
    // ファイルパス構築
    let file_path = if is_dir {
        let mut p = base_path.clone();
        if clean_sub.is_empty() || clean_sub == "/" {
            p.push(index_file.unwrap_or("index.html"));
        } else {
            p.push(clean_sub);
            if p.is_dir() {
                p.push(index_file.unwrap_or("index.html"));
            }
        }
        p
    } else {
        if !clean_sub.is_empty() {
            let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
            let _ = conn.send_response(stream_id, 404, headers, Some(b"Not Found")).await;
            return Some((404, 9));
        }
        base_path.clone()
    };
    
    // ファイル読み込み
    let data = match std::fs::read(&file_path) {
        Ok(d) => d,
        Err(_) => {
            let headers: &[(&[u8], &[u8])] = &[(b"server", b"veil/http2")];
            let _ = conn.send_response(stream_id, 404, headers, Some(b"Not Found")).await;
            return Some((404, 9));
        }
    };
    
    let mime_type = mime_guess::from_path(&file_path).first_or_octet_stream();
    let mime_str = mime_type.as_ref();
    
    let mut headers: Vec<(&[u8], &[u8])> = vec![
        (b"content-type", mime_str.as_bytes()),
        (b"server", b"veil/http2"),
    ];
    
    // セキュリティヘッダー追加
    let security_headers: Vec<(Vec<u8>, Vec<u8>)> = security.add_response_headers.iter()
        .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
        .collect();
    
    for (k, v) in &security_headers {
        headers.push((k.as_slice(), v.as_slice()));
    }
    
    if let Err(e) = conn.send_response(stream_id, 200, &headers, Some(&data)).await {
        warn!("[HTTP/2] File response error: {}", e);
        return None;
    }
    
    Some((200, data.len() as u64))
}

/// HTTP/2 リダイレクト処理
#[cfg(feature = "http2")]
async fn handle_http2_redirect<S>(
    conn: &mut http2::Http2Connection<S>,
    stream_id: u32,
    redirect_url: &str,
    status_code: u16,
    preserve_path: bool,
    req_path: &[u8],
    prefix: &[u8],
) -> Option<(u16, u64)>
where
    S: monoio::io::AsyncReadRent + monoio::io::AsyncWriteRentExt + Unpin,
{
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
    
    // パス部分（prefix除去後）
    let sub_path = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
        &path_str[prefix_str.len()..]
    } else {
        path_str
    };
    
    // 変数置換とパス追加
    let mut final_url = redirect_url
        .replace("$request_uri", path_str)
        .replace("$path", sub_path);
    
    if preserve_path && !sub_path.is_empty() {
        if final_url.ends_with('/') && sub_path.starts_with('/') {
            final_url.push_str(&sub_path[1..]);
        } else if !final_url.ends_with('/') && !sub_path.starts_with('/') {
            final_url.push('/');
            final_url.push_str(sub_path);
        } else {
            final_url.push_str(sub_path);
        }
    }
    
    let headers: &[(&[u8], &[u8])] = &[
        (b"location", final_url.as_bytes()),
        (b"server", b"veil/http2"),
    ];
    
    if let Err(e) = conn.send_response(stream_id, status_code, headers, None).await {
        warn!("[HTTP/2] Redirect response error: {}", e);
        return None;
    }
    
    Some((status_code, 0))
}

// kTLS 有効時の接続処理（rustls + ktls2）
#[cfg(feature = "ktls")]
async fn handle_connection(
    stream: TcpStream,
    acceptor: RustlsAcceptor,
    peer_addr: SocketAddr,
) {
    // CURRENT_CONFIG から最新の設定を取得（ホットリロード対応）
    // ArcSwap::load() はロックフリーで数ナノ秒
    let config = CURRENT_CONFIG.load();
    let host_routes = config.host_routes.clone();
    let path_routes = config.path_routes.clone();
    #[cfg(feature = "http2")]
    let http2_enabled = config.http2_enabled;
    
    // TLSハンドシェイクにタイムアウトを設定
    // rustls でハンドシェイク後、ktls2 で kTLS を有効化
    let tls_result = timeout(CONNECT_TIMEOUT, acceptor.accept(stream)).await;
    
    let tls_stream = match tls_result {
        Ok(Ok(tls)) => tls,
        Ok(Err(e)) => {
            warn!("TLS handshake error: {}", e);
            return;
        }
        Err(_) => {
            warn!("TLS handshake timeout");
            return;
        }
    };
    
    // クライアントIPアドレスを文字列に変換
    let client_ip = peer_addr.ip().to_string();

    // HTTP/2 が有効かつネゴシエートされた場合は HTTP/2 ハンドラーを使用
    #[cfg(feature = "http2")]
    if http2_enabled && tls_stream.is_http2() {
        handle_http2_connection(tls_stream, &host_routes, &path_routes, &client_ip).await;
        return;
    }

    // HTTP/1.1 ハンドラー
    handle_requests(tls_stream, &host_routes, &path_routes, &client_ip).await;
}

// kTLS 無効時の接続処理（rustls のみ）
#[cfg(not(feature = "ktls"))]
async fn handle_connection(
    stream: TcpStream,
    acceptor: simple_tls::SimpleTlsAcceptor,
    peer_addr: SocketAddr,
) {
    // CURRENT_CONFIG から最新の設定を取得（ホットリロード対応）
    // ArcSwap::load() はロックフリーで数ナノ秒
    let config = CURRENT_CONFIG.load();
    let host_routes = config.host_routes.clone();
    let path_routes = config.path_routes.clone();
    #[cfg(feature = "http2")]
    let http2_enabled = config.http2_enabled;
    
    // TLSハンドシェイクにタイムアウトを設定
    let tls_result = timeout(CONNECT_TIMEOUT, acceptor.accept(stream)).await;
    
    let tls_stream = match tls_result {
        Ok(Ok(tls)) => tls,
        Ok(Err(e)) => {
            warn!("TLS handshake error: {}", e);
            return;
        }
        Err(_) => {
            warn!("TLS handshake timeout");
            return;
        }
    };
    
    // クライアントIPアドレスを文字列に変換
    let client_ip = peer_addr.ip().to_string();

    // HTTP/2 が有効かつネゴシエートされた場合は HTTP/2 ハンドラーを使用
    #[cfg(feature = "http2")]
    if http2_enabled && tls_stream.is_http2() {
        handle_http2_connection(tls_stream, &host_routes, &path_routes, &client_ip).await;
        return;
    }

    // HTTP/1.1 ハンドラー
    handle_requests(tls_stream, &host_routes, &path_routes, &client_ip).await;
}

// ====================
// リクエスト処理ループ
// ====================

// 統一されたリクエスト処理ループ（型エイリアスを使用）
async fn handle_requests(
    mut tls_stream: ServerTls,
    host_routes: &Arc<HashMap<Box<[u8]>, Backend>>,
    path_routes: &Arc<HashMap<Box<[u8]>, SortedPathMap>>,
    client_ip: &str,
) {
    let mut accumulated = Vec::with_capacity(BUF_SIZE);

    loop {
        // 読み込み（アイドルタイムアウト付き）
        let read_buf = buf_get();
        let read_result = timeout(IDLE_TIMEOUT, tls_stream.read(read_buf)).await;
        
        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => {
                // アイドルタイムアウト - 接続を閉じる
                return;
            }
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                return;
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                return;
            }
        };
        
        // 読み込んだデータを蓄積（SafeReadBufferの型安全なアクセス）
        returned_buf.set_valid_len(n);
        accumulated.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);

        // ヘッダーサイズ制限チェック
        if accumulated.len() > MAX_HEADER_SIZE {
            let err_buf = ERR_MSG_REQUEST_TOO_LARGE.to_vec();
            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
            return;
        }

        // HTTPリクエストをパース
        let mut headers_storage = [httparse::EMPTY_HEADER; 64];
        let mut req = Request::new(&mut headers_storage);
        
        match req.parse(&accumulated) {
            Ok(Status::Complete(header_len)) => {
                // HTTPメソッド取得
                let method_bytes: Box<[u8]> = req.method
                    .map(|m| m.as_bytes().into())
                    .unwrap_or_else(|| Box::from(b"GET" as &[u8]));
                
                // ヘッダー情報抽出
                let host_bytes: Box<[u8]> = req.headers.iter()
                    .find(|h| h.name.eq_ignore_ascii_case("host"))
                    .map(|h| Box::from(h.value))
                    .unwrap_or_else(|| Box::from([] as [u8; 0]));
                
                let path_bytes: Box<[u8]> = req.path
                    .map(|p| p.as_bytes().into())
                    .unwrap_or_else(|| Box::from(b"/" as &[u8]));
                
                let user_agent: Box<[u8]> = req.headers.iter()
                    .find(|h| h.name.eq_ignore_ascii_case("user-agent"))
                    .map(|h| Box::from(h.value))
                    .unwrap_or_else(|| Box::from([] as [u8; 0]));
                
                let content_length: usize = req.headers.iter()
                    .find(|h| h.name.eq_ignore_ascii_case("content-length"))
                    .and_then(|h| std::str::from_utf8(h.value).ok())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                
                // Transfer-Encoding: chunked チェック（改善版）
                let is_chunked: bool = req.headers.iter()
                    .find(|h| h.name.eq_ignore_ascii_case("transfer-encoding"))
                    .map(|h| is_chunked_encoding(h.value))
                    .unwrap_or(false);
                
                // Connection ヘッダーチェック（Keep-Alive / Upgrade対応）
                let connection_header: Option<&[u8]> = req.headers.iter()
                    .find(|h| h.name.eq_ignore_ascii_case("connection"))
                    .map(|h| h.value);
                
                let client_wants_close: bool = connection_header
                    .map(|v| v.eq_ignore_ascii_case(b"close"))
                    .unwrap_or(false);
                
                // WebSocket Upgrade チェック
                // Connection: upgrade と Upgrade: websocket の両方が必要
                let is_upgrade_connection: bool = connection_header
                    .map(|v| {
                        // "upgrade" または "keep-alive, upgrade" などのパターンに対応
                        v.to_ascii_lowercase()
                            .windows(7)
                            .any(|w| w == b"upgrade")
                    })
                    .unwrap_or(false);
                
                let is_websocket_upgrade: bool = req.headers.iter()
                    .find(|h| h.name.eq_ignore_ascii_case("upgrade"))
                    .map(|h| h.value.eq_ignore_ascii_case(b"websocket"))
                    .unwrap_or(false);
                
                let is_websocket: bool = is_upgrade_connection && is_websocket_upgrade;

                // ボディサイズ制限
                if !is_chunked && content_length > MAX_BODY_SIZE {
                    drop(req);
                    let err_buf = ERR_MSG_REQUEST_TOO_LARGE.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                    return;
                }

                let headers_for_proxy: Vec<(Box<[u8]>, Box<[u8]>)> = req.headers.iter()
                    .filter(|h| !h.name.is_empty())
                    .map(|h| (h.name.as_bytes().into(), h.value.into()))
                    .collect();
                
                drop(req);
                
                // メトリクスエンドポイントの処理（設定可能なパス）
                // Prometheusスクレイピング用の特別なパス
                {
                    let config = CURRENT_CONFIG.load();
                    let prom_config = &config.prometheus_config;
                    
                    // パスとメソッドをチェック
                    let path_str = std::str::from_utf8(&path_bytes).unwrap_or("/");
                    if prom_config.enabled 
                        && path_str == prom_config.path 
                        && method_bytes.as_ref() == b"GET" 
                    {
                        let start_instant = Instant::now();
                        
                        // IPアドレス制限チェック
                        if !prom_config.is_ip_allowed(client_ip) {
                            let err_buf = ERR_MSG_FORBIDDEN.to_vec();
                            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                            log_access(&method_bytes, &host_bytes, &path_bytes, &user_agent, 0, 403, 0, start_instant);
                            accumulated.clear();
                            return;
                        }
                        
                        let metrics_response = build_metrics_response();
                        let resp_size = metrics_response.len() as u64;
                        
                        let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(metrics_response)).await;
                        match write_result {
                            Ok((Ok(_), _)) => {
                                log_access(&method_bytes, &host_bytes, &path_bytes, &user_agent, 0, 200, resp_size, start_instant);
                            }
                            _ => {}
                        }
                        
                        // メトリクスエンドポイントは常に接続を閉じる
                        accumulated.clear();
                        return;
                    }
                }

                // Backend選択
                let backend_result = find_backend(
                    &host_bytes,
                    &path_bytes,
                    host_routes,
                    path_routes,
                );

                let (prefix, backend) = match backend_result {
                    Some(b) => b,
                    None => {
                        let err_buf = ERR_MSG_BAD_REQUEST.to_vec();
                        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                        return;
                    }
                };
                
                // セキュリティ設定を取得
                let security = backend.security();
                
                // IP制限チェック（deny → allow の順で評価）
                let ip_filter = security.ip_filter();
                if ip_filter.is_configured() && !ip_filter.is_allowed(client_ip) {
                    let err_buf = ERR_MSG_FORBIDDEN.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                    return;
                }
                
                // 許可メソッドチェック
                if !security.allowed_methods.is_empty() {
                    let method_str = std::str::from_utf8(&method_bytes).unwrap_or("GET");
                    let is_allowed = security.allowed_methods.iter()
                        .any(|m| m.eq_ignore_ascii_case(method_str));
                    if !is_allowed {
                        let err_buf = ERR_MSG_METHOD_NOT_ALLOWED.to_vec();
                        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                        return;
                    }
                }
                
                // ルートごとのボディサイズ制限（chunked以外）
                if !is_chunked && content_length > security.max_request_body_size {
                    let err_buf = ERR_MSG_REQUEST_TOO_LARGE.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                    return;
                }
                
                // レートリミットチェック
                if security.rate_limit_requests_per_min > 0 {
                    if !check_rate_limit(client_ip, security.rate_limit_requests_per_min) {
                        let err_buf = ERR_MSG_TOO_MANY_REQUESTS.to_vec();
                        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                        return;
                    }
                }

                // 処理時間計測開始（Instant: モノトニック・高精度）
                let start_instant = Instant::now();
                
                // 初期ボディ（ヘッダー後のデータ）
                let initial_body: Vec<u8> = if header_len < accumulated.len() {
                    accumulated[header_len..].to_vec()
                } else {
                    Vec::new()
                };

                // バッファクリア（次のリクエストに備える）
                accumulated.clear();

                // WebSocket Upgrade の場合は専用ハンドラーを使用
                if is_websocket {
                    // WebSocket はプロキシバックエンドでのみサポート
                    if let Backend::Proxy(ref upstream_group, ref security, _, _, _) = backend {
                        info!("WebSocket upgrade request detected for path: {}", 
                              std::str::from_utf8(&path_bytes).unwrap_or("-"));
                        
                        // UpstreamGroup からサーバーを選択
                        let server = match upstream_group.select(client_ip) {
                            Some(s) => s,
                            None => {
                                error!("No healthy upstream servers for WebSocket");
                                let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                                let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                                return;
                            }
                        };
                        
                        server.acquire();
                        
                        // WebSocket プロキシ処理（双方向転送）
                        let ws_result = handle_websocket_proxy(
                            tls_stream,
                            &server.target,
                            security,
                            &method_bytes,
                            &path_bytes,
                            &prefix,
                            &headers_for_proxy,
                            &initial_body,
                        ).await;
                        
                        server.release();
                        
                        match ws_result {
                            Some((status, resp_size)) => {
                                log_access(&method_bytes, &host_bytes, &path_bytes, &user_agent, content_length as u64, status, resp_size, start_instant);
                            }
                            None => {}
                        }
                        // WebSocket 接続終了後は HTTP 接続も終了
                        return;
                    } else {
                        // ファイルバックエンドでは WebSocket 非対応
                        let err_buf = ERR_MSG_BAD_REQUEST.to_vec();
                        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                        return;
                    }
                }

                // Backend処理
                let result = handle_backend(
                    tls_stream,
                    backend,
                    &method_bytes,
                    &path_bytes,
                    prefix,
                    content_length,
                    is_chunked,
                    &headers_for_proxy,
                    &initial_body,
                    client_wants_close,
                    client_ip,
                ).await;

                match result {
                    Some((stream_back, status, resp_size, should_close)) => {
                        log_access(&method_bytes, &host_bytes, &path_bytes, &user_agent, content_length as u64, status, resp_size, start_instant);
                        tls_stream = stream_back;
                        
                        // Connection: close が要求された場合、またはエラー時は接続を閉じる
                        if should_close {
                            return;
                        }
                        // Keep-Alive: ループを継続して次のリクエストを待機
                    }
                    None => {
                        return;
                    }
                }
            }
            Ok(Status::Partial) => {
                // データ不足、次の読み込みを待つ
                continue;
            }
            Err(_) => {
                let err_buf = ERR_MSG_BAD_REQUEST.to_vec();
                let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                return;
            }
        }
    }
}

// ====================
// HTTPヘッダー検証（Header Injection防止）
// ====================
//
// httparseがパースしたヘッダーを再検証し、不正な文字を含む
// ヘッダーを除外することで、HTTP Request Smuggling攻撃を防止します。
// 多層防御（Defense in Depth）の原則に基づく追加チェックです。
// ====================

/// ヘッダー名が有効か検証（RFC 7230 token準拠）
/// 
/// token = 1*tchar
/// tchar = "!" / "#" / "$" / "%" / "&" / "'" / "*" / "+" / "-" / "." /
///         "^" / "_" / "`" / "|" / "~" / DIGIT / ALPHA
#[inline]
fn is_valid_header_name(name: &[u8]) -> bool {
    if name.is_empty() {
        return false;
    }
    for &b in name {
        let is_tchar = matches!(b,
            b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' |
            b'^' | b'_' | b'`' | b'|' | b'~' |
            b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z'
        );
        if !is_tchar {
            return false;
        }
    }
    true
}

/// ヘッダー値が有効か検証（Header Injection防止）
/// 
/// RFC 7230 field-value に基づき、以下を禁止:
/// - CR (\r): ヘッダーインジェクションの主要ベクトル
/// - LF (\n): ヘッダーインジェクションの主要ベクトル
/// - NUL (0x00): セキュリティ上の理由
/// 
/// obs-fold（折り返しヘッダー）は許容しない方針とする。
/// これにより、プロキシとバックエンド間の解釈の違いを悪用した
/// HTTP Request Smuggling攻撃を防止する。
/// 
/// ## 実装詳細
/// 
/// `memchr`クレートのSIMD最適化された`memchr3`関数を使用して、
/// 3つの禁止文字（CR, LF, NUL）を並列に検索します。
/// 
/// - AVX2対応CPUでは32バイト単位で並列検査
/// - SSE2対応CPUでは16バイト単位で並列検査
/// - 小さな入力では自動的に最適なフォールバックを選択
/// 
/// これにより、大きなヘッダー値（Cookie、Authorization等）の
/// 検証パフォーマンスが向上します。
#[inline]
fn is_valid_header_value(value: &[u8]) -> bool {
    // memchr3: 3つの文字を一度に検索（SIMD最適化）
    // いずれかの禁止文字が見つかった場合はSome(位置)を返す
    // 見つからなければNone -> 有効なヘッダー値
    memchr3(b'\r', b'\n', 0, value).is_none()
}

// ====================
// Transfer-Encoding: chunked 検出（改善版）
// ====================

/// Transfer-Encoding ヘッダー値から chunked かどうかを正確に判定
#[inline]
fn is_chunked_encoding(value: &[u8]) -> bool {
    // カンマ区切りの各値をチェック
    for part in value.split(|&b| b == b',') {
        // 空白をトリム
        let trimmed: Vec<u8> = part.iter()
            .skip_while(|&&b| b == b' ' || b == b'\t')
            .copied()
            .collect();
        let trimmed: Vec<u8> = trimmed.iter()
            .rev()
            .skip_while(|&&b| b == b' ' || b == b'\t')
            .copied()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        
        if trimmed.eq_ignore_ascii_case(b"chunked") {
            return true;
        }
    }
    false
}

// ====================
// Backend選択
// ====================

fn find_backend(
    host: &[u8],
    path: &[u8],
    host_routes: &Arc<HashMap<Box<[u8]>, Backend>>,
    path_routes: &Arc<HashMap<Box<[u8]>, SortedPathMap>>,
) -> Option<(Box<[u8]>, Backend)> {
    if let Some(backend) = host_routes.get(host) {
        return Some((Box::new([]), backend.clone()));
    }
    
    if let Some(sorted_map) = path_routes.get(host) {
        if let Some((prefix, backend)) = sorted_map.find_longest(path) {
            return Some((prefix.into(), backend.clone()));
        }
    }
    
    None
}

// ====================
// Backend処理
// ====================

// 統一された Backend 処理（型エイリアスを使用）
async fn handle_backend(
    mut tls_stream: ServerTls,
    backend: Backend,
    method: &[u8],
    req_path: &[u8],
    prefix: Box<[u8]>,
    content_length: usize,
    is_chunked: bool,
    headers: &[(Box<[u8]>, Box<[u8]>)],
    initial_body: &[u8],
    client_wants_close: bool,
    client_ip: &str,
) -> Option<(ServerTls, u16, u64, bool)> {
    match backend {
        Backend::Proxy(upstream_group, security, compression, buffering, cache) => {
            handle_proxy(
                tls_stream, 
                &upstream_group, 
                &security, 
                &compression,
                &buffering,
                &cache,
                method, 
                req_path, 
                &prefix, 
                content_length, 
                is_chunked, 
                headers, 
                initial_body, 
                client_wants_close, 
                client_ip
            ).await
        }
        Backend::MemoryFile(data, mime_type, security) => {
            // ファイル完全一致チェック
            // MemoryFileはファイル指定なので、プレフィックス以降にパスがあれば404
            let path_str = std::str::from_utf8(req_path).unwrap_or("/");
            let prefix_str = std::str::from_utf8(&prefix).unwrap_or("");
            
            let remainder = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
                &path_str[prefix_str.len()..]
            } else {
                ""
            };
            
            let clean_remainder = remainder.trim_matches('/');
            if !clean_remainder.is_empty() {
                // ファイル指定なのにさらにパスが続いている場合は404
                let err_buf = ERR_MSG_NOT_FOUND.to_vec();
                let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                return Some((tls_stream, 404, 0, true));
            }
            
            // Keep-Alive対応: クライアントの要求に応じてConnectionヘッダーを動的に生成
            let mut header = Vec::with_capacity(HEADER_BUF_CAPACITY);
            header.extend_from_slice(HTTP_200_PREFIX);
            header.extend_from_slice(mime_type.as_bytes());
            header.extend_from_slice(CONTENT_LENGTH_HEADER);
            let mut num_buf = itoa::Buffer::new();
            header.extend_from_slice(num_buf.format(data.len()).as_bytes());
            header.extend_from_slice(b"\r\n");
            
            // 追加レスポンスヘッダー（セキュリティヘッダーなど）
            for (header_name, header_value) in &security.add_response_headers {
                header.extend_from_slice(header_name.as_bytes());
                header.extend_from_slice(b": ");
                header.extend_from_slice(header_value.as_bytes());
                header.extend_from_slice(b"\r\n");
            }
            
            if client_wants_close {
                header.extend_from_slice(b"Connection: close\r\n\r\n");
            } else {
                header.extend_from_slice(b"Connection: keep-alive\r\n\r\n");
            }
            
            // ヘッダー送信（タイムアウト付き）
            let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(header)).await;
            match write_result {
                Ok((Ok(_), _)) => {}
                _ => return None,
            }
            
            // ボディ送信（タイムアウト付き）
            let data_len = data.len() as u64;
            let data_buf = data.to_vec();
            let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(data_buf)).await;
            match write_result {
                Ok((Ok(_), _)) => {
                    Some((tls_stream, 200, data_len, client_wants_close))
                }
                _ => None,
            }
        }
        Backend::SendFile(base_path, is_dir, index_file, security, _cache) => {
            handle_sendfile(tls_stream, &base_path, is_dir, index_file.as_deref(), req_path, &prefix, client_wants_close, &security).await
        }
        Backend::Redirect(redirect_url, status_code, preserve_path) => {
            handle_redirect(tls_stream, &redirect_url, status_code, preserve_path, req_path, &prefix, client_wants_close).await
        }
    }
}

// ====================
// リダイレクト処理
// ====================
//
// 設定されたURLへのHTTPリダイレクトを返します。
// ステータスコード: 301, 302, 303, 307, 308 をサポート
//
// 特殊変数:
// - $request_uri: 元のリクエストURI
// - $host: リクエストのHostヘッダー
// - $path: 元のパス（prefix除去後）
// ====================

/// リダイレクトレスポンスを生成して送信
async fn handle_redirect(
    mut tls_stream: ServerTls,
    redirect_url: &str,
    status_code: u16,
    preserve_path: bool,
    req_path: &[u8],
    prefix: &[u8],
    client_wants_close: bool,
) -> Option<(ServerTls, u16, u64, bool)> {
    // リダイレクト先URLを構築
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
    
    // パス部分（prefix除去後）
    let sub_path = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
        &path_str[prefix_str.len()..]
    } else {
        path_str
    };
    
    // 変数置換とパス追加
    let mut final_url = redirect_url
        .replace("$request_uri", path_str)
        .replace("$path", sub_path);
    
    // preserve_path が true の場合、元のパスを追加
    if preserve_path && !sub_path.is_empty() {
        // URLにすでにパスがある場合は結合
        if final_url.ends_with('/') && sub_path.starts_with('/') {
            final_url.push_str(&sub_path[1..]);
        } else if !final_url.ends_with('/') && !sub_path.starts_with('/') {
            final_url.push('/');
            final_url.push_str(sub_path);
        } else {
            final_url.push_str(sub_path);
        }
    }
    
    // ステータス行を構築
    let status_line = match status_code {
        301 => "HTTP/1.1 301 Moved Permanently\r\n",
        302 => "HTTP/1.1 302 Found\r\n",
        303 => "HTTP/1.1 303 See Other\r\n",
        307 => "HTTP/1.1 307 Temporary Redirect\r\n",
        308 => "HTTP/1.1 308 Permanent Redirect\r\n",
        _ => "HTTP/1.1 301 Moved Permanently\r\n",
    };
    
    // レスポンス構築
    let mut response = Vec::with_capacity(256 + final_url.len());
    response.extend_from_slice(status_line.as_bytes());
    response.extend_from_slice(b"Location: ");
    response.extend_from_slice(final_url.as_bytes());
    response.extend_from_slice(b"\r\nContent-Length: 0\r\n");
    
    if client_wants_close {
        response.extend_from_slice(b"Connection: close\r\n\r\n");
    } else {
        response.extend_from_slice(b"Connection: keep-alive\r\n\r\n");
    }
    
    // レスポンス送信
    let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(response)).await;
    match write_result {
        Ok((Ok(_), _)) => {
            Some((tls_stream, status_code, 0, client_wants_close))
        }
        _ => None,
    }
}

// ====================
// HTTPレスポンスパーサー（httparse使用）
// ====================

/// httparseを使用したレスポンス解析結果
struct ParsedResponse {
    /// ステータスコード
    status_code: u16,
    /// ヘッダー終端位置（ボディ開始位置）
    header_len: usize,
    /// Content-Length（存在する場合）
    content_length: Option<usize>,
    /// Transfer-Encoding: chunked かどうか
    is_chunked: bool,
    /// Connection: close かどうか（HTTP/1.1ではデフォルトはkeep-alive）
    is_connection_close: bool,
}

/// HTTPレスポンスをhttparseで解析
/// 
/// httparseを使用することで以下のメリットがある:
/// - RFC準拠の堅牢なパース
/// - \r\n と \n の両方に対応
/// - ヘッダー折り返し（obs-fold）の処理
/// - 不正なHTTPレスポンスの検出
fn parse_http_response(data: &[u8]) -> Option<ParsedResponse> {
    let mut headers_storage = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers_storage);
    
    match response.parse(data) {
        Ok(Status::Complete(header_len)) => {
            let status_code = response.code.unwrap_or(502);
            
            // Content-Length を取得
            let content_length = response.headers.iter()
                .find(|h| h.name.eq_ignore_ascii_case("content-length"))
                .and_then(|h| std::str::from_utf8(h.value).ok())
                .and_then(|s| s.trim().parse().ok());
            
            // Transfer-Encoding: chunked をチェック
            let is_chunked = response.headers.iter()
                .find(|h| h.name.eq_ignore_ascii_case("transfer-encoding"))
                .map(|h| is_chunked_encoding(h.value))
                .unwrap_or(false);
            
            // Connection: close をチェック（HTTP/1.1ではデフォルトはkeep-alive）
            let is_connection_close = response.headers.iter()
                .find(|h| h.name.eq_ignore_ascii_case("connection"))
                .map(|h| {
                    // 値をトリムして比較
                    let value = h.value;
                    let trimmed: Vec<u8> = value.iter()
                        .skip_while(|&&b| b == b' ' || b == b'\t')
                        .copied()
                        .collect();
                    trimmed.eq_ignore_ascii_case(b"close")
                })
                .unwrap_or(false);
            
            Some(ParsedResponse {
                status_code,
                header_len,
                content_length,
                is_chunked,
                is_connection_close,
            })
        }
        Ok(Status::Partial) => None, // データ不足
        Err(_) => None, // パースエラー
    }
}

// ====================
// Chunked Transfer Encoding パーサー（RFC 7230 Section 4.1 準拠）
// ====================
//
// Chunked-Bodyの構文:
//   chunked-body   = *chunk last-chunk trailer-part CRLF
//   chunk          = chunk-size [ chunk-ext ] CRLF chunk-data CRLF
//   chunk-size     = 1*HEXDIG
//   last-chunk     = 1*("0") [ chunk-ext ] CRLF
//   trailer-part   = *( header-field CRLF )
//
// トレーラーが存在する場合でも正確に終端を検出するために
// ステートマシンベースのパーサーを使用します。
// ====================

/// Chunkedデコーダの状態
#[derive(Debug, Clone, Copy, PartialEq)]
enum ChunkedState {
    /// チャンクサイズの16進数を読み取り中
    ReadingChunkSize,
    /// チャンク拡張（;以降）を読み取り中（サイズ行の終わりまでスキップ）
    ReadingChunkExtension,
    /// チャンクサイズ行の\r後、\nを期待
    ExpectingChunkSizeLF,
    /// チャンクデータを読み取り中（残りバイト数をchunk_remainingで追跡）
    ReadingChunkData,
    /// チャンクデータ後の\rを期待
    ExpectingChunkDataCR,
    /// チャンクデータ後の\nを期待
    ExpectingChunkDataLF,
    /// トレーラーヘッダーまたは終端の空行を読み取り中
    /// 空行（\r\n）で完了、それ以外はトレーラーヘッダー
    ReadingTrailerLine,
    /// トレーラー行または終端の\r後、\nを期待
    ExpectingTrailerLF,
    /// 転送完了
    Complete,
    /// サイズ制限超過（DoS対策）
    SizeLimitExceeded,
}

/// Chunkedデコーダのフィード結果
#[derive(Debug, Clone, Copy, PartialEq)]
enum ChunkedFeedResult {
    /// まだ転送中
    Continue,
    /// 転送完了
    Complete,
    /// サイズ制限超過
    SizeLimitExceeded,
}

/// Chunked転送デコーダ（ステートマシン）
/// 
/// RFC 7230 Section 4.1に準拠し、トレーラーの有無にかかわらず
/// 正確に終端を検出します。
/// 
/// DoS対策として累積サイズの制限機能を持ちます。
#[derive(Debug, Clone)]
struct ChunkedDecoder {
    /// 現在の状態
    state: ChunkedState,
    /// 現在のチャンクの残りバイト数
    chunk_remaining: u64,
    /// チャンクサイズの解析中に蓄積する16進数値
    size_accumulator: u64,
    /// サイズに少なくとも1文字は含まれているか
    size_has_digit: bool,
    /// トレーラー行が空かどうか（終端検出用）
    trailer_line_empty: bool,
    /// 累積ボディサイズ（DoS対策）
    total_body_size: u64,
    /// 最大許容ボディサイズ（0の場合は制限なし）
    max_body_size: u64,
}

impl ChunkedDecoder {
    /// 新しいChunkedDecoderを作成（サイズ制限付き）
    /// 
    /// # Arguments
    /// * `max_body_size` - 最大許容ボディサイズ（0の場合は制限なし）
    fn new(max_body_size: u64) -> Self {
        Self {
            state: ChunkedState::ReadingChunkSize,
            chunk_remaining: 0,
            size_accumulator: 0,
            size_has_digit: false,
            trailer_line_empty: true,
            total_body_size: 0,
            max_body_size,
        }
    }
    
    /// 新しいChunkedDecoderを作成（制限なし - レスポンス用）
    fn new_unlimited() -> Self {
        Self::new(0)
    }
    
    /// 転送が完了したかどうかを確認
    #[inline]
    fn is_complete(&self) -> bool {
        self.state == ChunkedState::Complete
    }
    
    /// データをフィードして状態を更新
    /// 完了またはサイズ制限超過の場合は適切な結果を返す
    fn feed(&mut self, data: &[u8]) -> ChunkedFeedResult {
        for &byte in data {
            match self.feed_byte(byte) {
                ChunkedFeedResult::Continue => continue,
                result => return result,
            }
        }
        ChunkedFeedResult::Continue
    }
    
    /// 1バイトを処理して状態を更新
    /// 完了またはサイズ制限超過の場合は適切な結果を返す
    #[inline]
    fn feed_byte(&mut self, byte: u8) -> ChunkedFeedResult {
        match self.state {
            ChunkedState::ReadingChunkSize => {
                match byte {
                    b'0'..=b'9' => {
                        self.size_accumulator = self.size_accumulator.saturating_mul(16)
                            .saturating_add((byte - b'0') as u64);
                        self.size_has_digit = true;
                    }
                    b'a'..=b'f' => {
                        self.size_accumulator = self.size_accumulator.saturating_mul(16)
                            .saturating_add((byte - b'a' + 10) as u64);
                        self.size_has_digit = true;
                    }
                    b'A'..=b'F' => {
                        self.size_accumulator = self.size_accumulator.saturating_mul(16)
                            .saturating_add((byte - b'A' + 10) as u64);
                        self.size_has_digit = true;
                    }
                    b';' => {
                        // チャンク拡張の開始
                        self.state = ChunkedState::ReadingChunkExtension;
                    }
                    b'\r' => {
                        self.state = ChunkedState::ExpectingChunkSizeLF;
                    }
                    _ => {
                        // 不正な文字 - 回復のためスキップ（緩い解析）
                    }
                }
            }
            
            ChunkedState::ReadingChunkExtension => {
                // チャンク拡張はCRまでスキップ
                if byte == b'\r' {
                    self.state = ChunkedState::ExpectingChunkSizeLF;
                }
            }
            
            ChunkedState::ExpectingChunkSizeLF => {
                if byte == b'\n' {
                    if !self.size_has_digit {
                        // サイズが解析できなかった場合、リセット
                        self.state = ChunkedState::ReadingChunkSize;
                    } else if self.size_accumulator == 0 {
                        // 最後のチャンク（サイズ0）- トレーラーセクションへ
                        self.state = ChunkedState::ReadingTrailerLine;
                        self.trailer_line_empty = true;
                    } else {
                        // 通常のチャンク - データセクションへ
                        // サイズ制限チェック（DoS対策）
                        if self.max_body_size > 0 {
                            let new_total = self.total_body_size.saturating_add(self.size_accumulator);
                            if new_total > self.max_body_size {
                                self.state = ChunkedState::SizeLimitExceeded;
                                return ChunkedFeedResult::SizeLimitExceeded;
                            }
                            self.total_body_size = new_total;
                        }
                        self.chunk_remaining = self.size_accumulator;
                        self.state = ChunkedState::ReadingChunkData;
                    }
                    // 次のチャンクのためにリセット
                    self.size_accumulator = 0;
                    self.size_has_digit = false;
                } else {
                    // LFが来なかった - リセット（緩い解析）
                    self.state = ChunkedState::ReadingChunkSize;
                    self.size_accumulator = 0;
                    self.size_has_digit = false;
                }
            }
            
            ChunkedState::ReadingChunkData => {
                self.chunk_remaining = self.chunk_remaining.saturating_sub(1);
                if self.chunk_remaining == 0 {
                    self.state = ChunkedState::ExpectingChunkDataCR;
                }
            }
            
            ChunkedState::ExpectingChunkDataCR => {
                if byte == b'\r' {
                    self.state = ChunkedState::ExpectingChunkDataLF;
                } else {
                    // 不正な形式 - 次のチャンクを探す（緩い解析）
                    self.state = ChunkedState::ReadingChunkSize;
                }
            }
            
            ChunkedState::ExpectingChunkDataLF => {
                if byte == b'\n' {
                    self.state = ChunkedState::ReadingChunkSize;
                } else {
                    // 不正な形式 - リセット
                    self.state = ChunkedState::ReadingChunkSize;
                }
            }
            
            ChunkedState::ReadingTrailerLine => {
                match byte {
                    b'\r' => {
                        self.state = ChunkedState::ExpectingTrailerLF;
                    }
                    _ => {
                        // トレーラーヘッダーの内容
                        self.trailer_line_empty = false;
                    }
                }
            }
            
            ChunkedState::ExpectingTrailerLF => {
                if byte == b'\n' {
                    if self.trailer_line_empty {
                        // 空行 = 転送完了
                        self.state = ChunkedState::Complete;
                        return ChunkedFeedResult::Complete;
                    } else {
                        // トレーラーヘッダー行が完了、次の行へ
                        self.state = ChunkedState::ReadingTrailerLine;
                        self.trailer_line_empty = true;
                    }
                } else {
                    // 不正な形式だが、トレーラー読み取りを継続
                    self.state = ChunkedState::ReadingTrailerLine;
                    self.trailer_line_empty = false;
                }
            }
            
            ChunkedState::Complete => {
                return ChunkedFeedResult::Complete;
            }
            
            ChunkedState::SizeLimitExceeded => {
                return ChunkedFeedResult::SizeLimitExceeded;
            }
        }
        ChunkedFeedResult::Continue
    }
}

// ====================
// WebSocket プロキシ処理
// ====================
//
// WebSocket アップグレードリクエストを検出し、双方向転送を行います。
//
// フロー:
// 1. クライアントから Upgrade: websocket リクエストを受信
// 2. バックエンドに接続し、アップグレードリクエストを転送
// 3. バックエンドから 101 Switching Protocols を受信
// 4. クライアントに 101 を転送
// 5. 以降は双方向でバイトストリームを透過的に転送
// 6. どちらかが接続を閉じるまで継続
// ====================

/// WebSocket プロキシ処理（双方向転送）
/// 
/// HTTP Upgrade をバックエンドに転送し、成功後は双方向のバイト転送を行う。
/// WebSocket 接続が終了するまでブロックし、終了後はクライアント接続も閉じる。
/// 
/// # Returns
/// Some((status_code, bytes_transferred)) - 成功時
/// None - エラー時
async fn handle_websocket_proxy(
    client_stream: ServerTls,
    target: &ProxyTarget,
    security: &SecurityConfig,
    method: &[u8],
    req_path: &[u8],
    prefix: &[u8],
    headers: &[(Box<[u8]>, Box<[u8]>)],
    initial_body: &[u8],
) -> Option<(u16, u64)> {
    let connect_timeout = Duration::from_secs(security.backend_connect_timeout_secs);
    
    // リクエストパス構築
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    let sub_path = if prefix.is_empty() {
        path_str.to_string()
    } else {
        let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
        if path_str.starts_with(prefix_str) {
            let remaining = &path_str[prefix_str.len()..];
            let base = target.path_prefix.trim_end_matches('/');
            
            if remaining.is_empty() {
                if base.is_empty() { "/".to_string() } else { format!("{}/", base) }
            } else if remaining.starts_with('/') {
                if base.is_empty() { remaining.to_string() } else { format!("{}{}", base, remaining) }
            } else {
                if base.is_empty() { format!("/{}", remaining) } else { format!("{}/{}", base, remaining) }
            }
        } else {
            path_str.to_string()
        }
    };
    
    let final_path = if sub_path.is_empty() { "/" } else { &sub_path };
    
    // WebSocket アップグレードリクエスト構築
    // Connection: Upgrade と Upgrade: websocket を維持
    let mut request = Vec::with_capacity(1024);
    request.extend_from_slice(method);
    request.extend_from_slice(HEADER_SPACE);
    request.extend_from_slice(final_path.as_bytes());
    request.extend_from_slice(HEADER_HTTP11_HOST);
    request.extend_from_slice(target.host.as_bytes());
    
    if !target.is_default_port() {
        request.extend_from_slice(HEADER_PORT_COLON);
        let mut port_buf = itoa::Buffer::new();
        request.extend_from_slice(port_buf.format(target.port).as_bytes());
    }
    
    request.extend_from_slice(HEADER_CRLF);
    
    // すべてのヘッダーを転送（Host 以外）
    for (name, value) in headers {
        if name.eq_ignore_ascii_case(b"host") {
            continue;
        }
        
        if !is_valid_header_name(name) || !is_valid_header_value(value) {
            continue;
        }
        
        request.extend_from_slice(name);
        request.extend_from_slice(HEADER_COLON);
        request.extend_from_slice(value);
        request.extend_from_slice(HEADER_CRLF);
    }
    request.extend_from_slice(HEADER_CRLF);
    
    // 初期ボディがあれば追加
    if !initial_body.is_empty() {
        request.extend_from_slice(initial_body);
    }
    
    // WebSocketポーリング設定を取得
    let poll_config = security.websocket_poll_config();
    
    // バックエンドに接続
    if target.use_tls {
        // HTTPS バックエンドへの WebSocket
        handle_websocket_proxy_https(client_stream, target, connect_timeout, request, &poll_config).await
    } else {
        // HTTP バックエンドへの WebSocket
        handle_websocket_proxy_http(client_stream, target, connect_timeout, request, &poll_config).await
    }
}

/// HTTP バックエンドへの WebSocket プロキシ
async fn handle_websocket_proxy_http(
    mut client_stream: ServerTls,
    target: &ProxyTarget,
    connect_timeout: Duration,
    request: Vec<u8>,
    poll_config: &WebSocketPollConfig,
) -> Option<(u16, u64)> {
    // バックエンドに接続
    let addr = format!("{}:{}", target.host, target.port);
    let connect_result = timeout(connect_timeout, TcpStream::connect(&addr)).await;
    
    let mut backend_stream = match connect_result {
        Ok(Ok(stream)) => {
            let _ = stream.set_nodelay(true);
            stream
        }
        Ok(Err(e)) => {
            error!("WebSocket proxy connect error: {}", e);
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((502, 0));
        }
        Err(_) => {
            let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((504, 0));
        }
    };
    
    // アップグレードリクエストを送信
    let (write_res, _) = backend_stream.write_all(request).await;
    if write_res.is_err() {
        let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
        let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
        return Some((502, 0));
    }
    
    // バックエンドからのレスポンスを読み取り
    let mut response_buf = Vec::with_capacity(4096);
    let status_code;
    
    loop {
        let buf = buf_get();
        let (res, mut returned_buf) = backend_stream.read(buf).await;
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                return Some((502, 0));
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                return Some((502, 0));
            }
        };
        
        returned_buf.set_valid_len(n);
        response_buf.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);
        
        // レスポンスヘッダーを解析
        if let Some(parsed) = parse_http_response(&response_buf) {
            status_code = parsed.status_code;
            
            // クライアントにレスポンスを転送
            let resp_data = response_buf.clone();
            let (write_res, _) = client_stream.write_all(resp_data).await;
            if write_res.is_err() {
                return None;
            }
            
            // 101 Switching Protocols の場合は双方向転送開始
            if status_code == 101 {
                info!("WebSocket upgrade successful, starting bidirectional transfer");
                let total = websocket_bidirectional_transfer(&mut client_stream, &mut backend_stream, poll_config).await;
                return Some((101, total));
            } else {
                // アップグレード失敗（通常の HTTP レスポンス）
                return Some((status_code, response_buf.len() as u64));
            }
        }
        
        // ヘッダーが大きすぎる
        if response_buf.len() > MAX_HEADER_SIZE {
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((502, 0));
        }
    }
}

/// HTTPS バックエンドへの WebSocket プロキシ
async fn handle_websocket_proxy_https(
    mut client_stream: ServerTls,
    target: &ProxyTarget,
    connect_timeout: Duration,
    request: Vec<u8>,
    poll_config: &WebSocketPollConfig,
) -> Option<(u16, u64)> {
    // バックエンドに TCP 接続
    let addr = format!("{}:{}", target.host, target.port);
    let connect_result = timeout(connect_timeout, TcpStream::connect(&addr)).await;
    
    let backend_tcp = match connect_result {
        Ok(Ok(stream)) => {
            let _ = stream.set_nodelay(true);
            stream
        }
        Ok(Err(e)) => {
            error!("WebSocket proxy connect error: {}", e);
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((502, 0));
        }
        Err(_) => {
            let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((504, 0));
        }
    };
    
    // TLS 接続
    let connector = TLS_CONNECTOR.with(|c| c.clone());
    let tls_result = timeout(connect_timeout, connector.connect(backend_tcp, &target.host)).await;
    
    let mut backend_stream = match tls_result {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            error!("WebSocket TLS connect error: {}", e);
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((502, 0));
        }
        Err(_) => {
            let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((504, 0));
        }
    };
    
    // アップグレードリクエストを送信
    let (write_res, _) = backend_stream.write_all(request).await;
    if write_res.is_err() {
        let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
        let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
        return Some((502, 0));
    }
    
    // バックエンドからのレスポンスを読み取り
    let mut response_buf = Vec::with_capacity(4096);
    let status_code;
    
    loop {
        let buf = buf_get();
        let (res, mut returned_buf) = backend_stream.read(buf).await;
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                return Some((502, 0));
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                return Some((502, 0));
            }
        };
        
        returned_buf.set_valid_len(n);
        response_buf.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);
        
        // レスポンスヘッダーを解析
        if let Some(parsed) = parse_http_response(&response_buf) {
            status_code = parsed.status_code;
            
            // クライアントにレスポンスを転送
            let resp_data = response_buf.clone();
            let (write_res, _) = client_stream.write_all(resp_data).await;
            if write_res.is_err() {
                return None;
            }
            
            // 101 Switching Protocols の場合は双方向転送開始
            if status_code == 101 {
                info!("WebSocket upgrade successful (TLS), starting bidirectional transfer");
                let total = websocket_bidirectional_transfer_tls(&mut client_stream, &mut backend_stream, poll_config).await;
                return Some((101, total));
            } else {
                // アップグレード失敗
                return Some((status_code, response_buf.len() as u64));
            }
        }
        
        // ヘッダーが大きすぎる
        if response_buf.len() > MAX_HEADER_SIZE {
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((502, 0));
        }
    }
}

/// WebSocket 双方向転送（HTTP バックエンド）
/// 
/// クライアント ⇔ バックエンド間でデータを双方向に転送。
/// monoio の select! 相当を手動で実装し、どちらの方向も待機。
/// 
/// ## ポーリングモード
/// 
/// ### Fixed モード
/// 設定されたタイムアウト値を固定で使用。
/// 低レイテンシが最優先の場合（リアルタイムゲームなど）に推奨。
/// 
/// ### Adaptive モード（デフォルト）
/// データ転送があればタイムアウトをリセット（初期値に戻す）。
/// アイドル時はバックオフ方式でタイムアウトを延長（最大値まで）。
/// CPU効率とレイテンシのバランスを取る場合に推奨。
/// 
/// ## 将来的な改善
/// 
/// monoio が epoll/io_uring ベースのselect風APIをサポートした場合、
/// イベント駆動型の実装に移行することで、さらなる効率化が可能。
async fn websocket_bidirectional_transfer(
    client: &mut ServerTls,
    backend: &mut TcpStream,
    poll_config: &WebSocketPollConfig,
) -> u64 {
    let mut total = 0u64;
    
    // 現在のタイムアウト値（Adaptive モードで動的に変更）
    let mut current_timeout_ms = poll_config.initial_timeout_ms;
    
    loop {
        let poll_timeout = Duration::from_millis(current_timeout_ms);
        let mut had_activity = false;
        
        // クライアント → バックエンド
        let client_buf = buf_get();
        let read_result = timeout(poll_timeout, client.read(client_buf)).await;
        
        match read_result {
            Ok((Ok(0), buf)) => {
                buf_put(buf);
                break; // クライアントが接続を閉じた
            }
            Ok((Ok(n), mut buf)) => {
                buf.set_valid_len(n);
                let write_buf = buf.into_truncated();
                let (write_res, returned) = backend.write_all(write_buf).await;
                buf_put_vec(returned);
                if write_res.is_err() {
                    break;
                }
                total += n as u64;
                had_activity = true;
            }
            Ok((Err(_), buf)) => {
                buf_put(buf);
                break;
            }
            Err(_) => {
                // タイムアウト - 反対方向をチェック
            }
        }
        
        // バックエンド → クライアント
        let backend_buf = buf_get();
        let read_result = timeout(poll_timeout, backend.read(backend_buf)).await;
        
        match read_result {
            Ok((Ok(0), buf)) => {
                buf_put(buf);
                break; // バックエンドが接続を閉じた
            }
            Ok((Ok(n), mut buf)) => {
                buf.set_valid_len(n);
                let write_buf = buf.into_truncated();
                let (write_res, returned) = client.write_all(write_buf).await;
                buf_put_vec(returned);
                if write_res.is_err() {
                    break;
                }
                total += n as u64;
                had_activity = true;
            }
            Ok((Err(_), buf)) => {
                buf_put(buf);
                break;
            }
            Err(_) => {
                // タイムアウト - ループ継続
            }
        }
        
        // Adaptive モードでのタイムアウト調整
        if poll_config.mode == WebSocketPollMode::Adaptive {
            if had_activity {
                // データ転送があった場合: タイムアウトをリセット（初期値に戻す）
                current_timeout_ms = poll_config.initial_timeout_ms;
            } else {
                // タイムアウトした場合: バックオフ（最大値まで延長）
                let new_timeout = (current_timeout_ms as f64 * poll_config.backoff_multiplier) as u64;
                current_timeout_ms = new_timeout.min(poll_config.max_timeout_ms);
            }
        }
        // Fixed モードでは current_timeout_ms は変更されない
    }
    
    total
}

/// WebSocket 双方向転送（HTTPS バックエンド）
/// 
/// HTTP版と同様のポーリングモード（Fixed/Adaptive）をサポート。
/// 詳細は `websocket_bidirectional_transfer` のドキュメントを参照。
async fn websocket_bidirectional_transfer_tls(
    client: &mut ServerTls,
    backend: &mut ClientTls,
    poll_config: &WebSocketPollConfig,
) -> u64 {
    let mut total = 0u64;
    
    // 現在のタイムアウト値（Adaptive モードで動的に変更）
    let mut current_timeout_ms = poll_config.initial_timeout_ms;
    
    loop {
        let poll_timeout = Duration::from_millis(current_timeout_ms);
        let mut had_activity = false;
        
        // クライアント → バックエンド
        let client_buf = buf_get();
        let read_result = timeout(poll_timeout, client.read(client_buf)).await;
        
        match read_result {
            Ok((Ok(0), buf)) => {
                buf_put(buf);
                break;
            }
            Ok((Ok(n), mut buf)) => {
                buf.set_valid_len(n);
                let write_buf = buf.into_truncated();
                let (write_res, returned) = backend.write_all(write_buf).await;
                buf_put_vec(returned);
                if write_res.is_err() {
                    break;
                }
                total += n as u64;
                had_activity = true;
            }
            Ok((Err(_), buf)) => {
                buf_put(buf);
                break;
            }
            Err(_) => {}
        }
        
        // バックエンド → クライアント
        let backend_buf = buf_get();
        let read_result = timeout(poll_timeout, backend.read(backend_buf)).await;
        
        match read_result {
            Ok((Ok(0), buf)) => {
                buf_put(buf);
                break;
            }
            Ok((Ok(n), mut buf)) => {
                buf.set_valid_len(n);
                let write_buf = buf.into_truncated();
                let (write_res, returned) = client.write_all(write_buf).await;
                buf_put_vec(returned);
                if write_res.is_err() {
                    break;
                }
                total += n as u64;
                had_activity = true;
            }
            Ok((Err(_), buf)) => {
                buf_put(buf);
                break;
            }
            Err(_) => {}
        }
        
        // Adaptive モードでのタイムアウト調整
        if poll_config.mode == WebSocketPollMode::Adaptive {
            if had_activity {
                // データ転送があった場合: タイムアウトをリセット（初期値に戻す）
                current_timeout_ms = poll_config.initial_timeout_ms;
            } else {
                // タイムアウトした場合: バックオフ（最大値まで延長）
                let new_timeout = (current_timeout_ms as f64 * poll_config.backoff_multiplier) as u64;
                current_timeout_ms = new_timeout.min(poll_config.max_timeout_ms);
            }
        }
        // Fixed モードでは current_timeout_ms は変更されない
    }
    
    total
}

// ====================
// プロキシ処理
// ====================
//
// バックエンドコネクションプールを使用して、接続を再利用します。
// Connection: keep-alive をバックエンドに送信し、レスポンスの
// Connection ヘッダーに基づいて接続をプールに返却します。
// ====================

async fn handle_proxy(
    mut client_stream: ServerTls,
    upstream_group: &UpstreamGroup,
    security: &SecurityConfig,
    compression: &CompressionConfig,
    buffering_config: &buffering::BufferingConfig,
    cache_config: &cache::CacheConfig,
    method: &[u8],
    req_path: &[u8],
    prefix: &[u8],
    content_length: usize,
    is_chunked: bool,
    headers: &[(Box<[u8]>, Box<[u8]>)],
    initial_body: &[u8],
    client_wants_close: bool,
    client_ip: &str,
) -> Option<(ServerTls, u16, u64, bool)> {
    // クライアントの Accept-Encoding を解析
    let client_encoding = headers.iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(b"accept-encoding"))
        .map(|(_, value)| AcceptedEncoding::parse(value))
        .unwrap_or(AcceptedEncoding::Identity);
    
    // ホスト名を取得
    let host_str = headers.iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(b"host"))
        .and_then(|(_, v)| std::str::from_utf8(v).ok())
        .unwrap_or("unknown");
    
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    
    // ===================
    // キャッシュヒット判定
    // ===================
    if cache_config.enabled {
        // キャッシュ対象かチェック
        if cache_config.is_cacheable_method(method) && !cache_config.should_bypass(path_str) {
            // キャッシュキー生成
            let query = path_str.find('?').map(|i| &path_str[i+1..]);
            let path_only = path_str.find('?').map(|i| &path_str[..i]).unwrap_or(path_str);
            
            if let Some(cache_key) = cache::CacheKey::from_request(
                method,
                host_str,
                path_only,
                query,
                cache_config.include_query,
                None, // Varyヘッダーは初回リクエストでは不明
            ) {
                // グローバルキャッシュからエントリを取得
                if let Some(cache_manager) = cache::get_global_cache() {
                    if let Some(cached_entry) = cache_manager.get(&cache_key) {
                        // キャッシュヒット！
                        debug!("Cache HIT for {} {}", host_str, path_str);
                        
                        // キャッシュからレスポンスを返す
                        if let Some(body_data) = cached_entry.memory_body() {
                            let mut response = Vec::with_capacity(512 + body_data.len());
                            
                            // ステータスライン
                            response.extend_from_slice(b"HTTP/1.1 ");
                            let mut status_buf = itoa::Buffer::new();
                            response.extend_from_slice(status_buf.format(cached_entry.status_code).as_bytes());
                            response.extend_from_slice(b" OK\r\n");
                            
                            // ヘッダー
                            for (name, value) in cached_entry.headers.iter() {
                                response.extend_from_slice(name);
                                response.extend_from_slice(b": ");
                                response.extend_from_slice(value);
                                response.extend_from_slice(b"\r\n");
                            }
                            
                            // X-Cache ヘッダー
                            response.extend_from_slice(b"X-Cache: HIT\r\n");
                            
                            // Connection ヘッダー
                            if client_wants_close {
                                response.extend_from_slice(b"Connection: close\r\n");
                            } else {
                                response.extend_from_slice(b"Connection: keep-alive\r\n");
                            }
                            
                            response.extend_from_slice(b"\r\n");
                            response.extend_from_slice(body_data);
                            
                            match timeout(WRITE_TIMEOUT, client_stream.write_all(response)).await {
                                Ok((Ok(_), _)) => {
                                    return Some((client_stream, cached_entry.status_code, body_data.len() as u64, client_wants_close));
                                }
                                _ => {
                                    return None;
                                }
                            }
                        }
                    } else {
                        debug!("Cache MISS for {} {}", host_str, path_str);
                    }
                }
            }
        }
    }
    
    // バッファリングモードのログ出力（デバッグ用）
    if buffering_config.is_enabled() {
        debug!("Buffering enabled for {} {} (mode={:?})", 
               host_str, path_str, buffering_config.mode);
    }
    
    // ロードバランシング: UpstreamGroup からサーバーを選択
    let server = match upstream_group.select(client_ip) {
        Some(s) => s,
        None => {
            // 利用可能なサーバーがない
            error!("No healthy upstream servers available");
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((client_stream, 502, 0, true));
        }
    };
    
    // 接続カウンターを増加（Least Connections 用）
    server.acquire();
    
    let target = &server.target;
    // コネクションプールキーの生成
    // HTTPS接続でSNI名が設定されている場合は、異なるSNI名は異なるプールとして扱う
    let pool_key = if target.use_tls && target.sni_name.is_some() {
        format!("{}:{}:{}", target.host, target.port, target.sni())
    } else {
        format!("{}:{}", target.host, target.port)
    };
    
    // リクエストパス構築
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    let sub_path = if prefix.is_empty() {
        path_str.to_string()
    } else {
        let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
        if path_str.starts_with(prefix_str) {
            let remaining = &path_str[prefix_str.len()..];
            let base = target.path_prefix.trim_end_matches('/');
            
            if remaining.is_empty() {
                if base.is_empty() {
                    "/".to_string()
                } else {
                    format!("{}/", base)
                }
            } else if remaining.starts_with('/') {
                if base.is_empty() {
                    remaining.to_string()
                } else {
                    format!("{}{}", base, remaining)
                }
            } else {
                if base.is_empty() {
                    format!("/{}", remaining)
                } else {
                    format!("{}/{}", base, remaining)
                }
            }
        } else {
            path_str.to_string()
        }
    };
    
    let final_path = if sub_path.is_empty() { "/" } else { &sub_path };

    // HTTPリクエスト構築（Connection: keep-alive を使用）
    // 定数バイト列を使用してアロケーションを削減
    let mut request = Vec::with_capacity(1024);
    request.extend_from_slice(method);
    request.extend_from_slice(HEADER_SPACE);
    request.extend_from_slice(final_path.as_bytes());
    request.extend_from_slice(HEADER_HTTP11_HOST);
    request.extend_from_slice(target.host.as_bytes());
    
    if !target.is_default_port() {
        request.extend_from_slice(HEADER_PORT_COLON);
        let mut port_buf = itoa::Buffer::new();
        request.extend_from_slice(port_buf.format(target.port).as_bytes());
    }
    
    request.extend_from_slice(HEADER_CRLF);
    
    // ヘッダー削除リストを小文字で保持（高速比較用）
    let remove_headers_lower: Vec<Vec<u8>> = security.remove_request_headers.iter()
        .map(|h| h.to_ascii_lowercase().into_bytes())
        .collect();
    
    for (name, value) in headers {
        // host と connection ヘッダーは別途処理済みのためスキップ
        if name.eq_ignore_ascii_case(b"host") || name.eq_ignore_ascii_case(b"connection") {
            continue;
        }
        
        // 設定で削除が指定されているヘッダーをスキップ
        let name_lower: Vec<u8> = name.iter().map(|b| b.to_ascii_lowercase()).collect();
        if remove_headers_lower.iter().any(|h| h == &name_lower) {
            continue;
        }
        
        // Header Injection防止: ヘッダー名と値の検証
        // httparseによるパース後も、多層防御として再検証を行う
        // 不正な文字（CR, LF, NUL等）を含むヘッダーは除外
        if !is_valid_header_name(name) {
            warn!("Invalid header name detected, skipping: {:?}", 
                  String::from_utf8_lossy(name));
            continue;
        }
        if !is_valid_header_value(value) {
            warn!("Invalid header value detected (possible header injection), skipping header: {:?}", 
                  String::from_utf8_lossy(name));
            continue;
        }
        
        request.extend_from_slice(name);
        request.extend_from_slice(HEADER_COLON);
        request.extend_from_slice(value);
        request.extend_from_slice(HEADER_CRLF);
    }
    
    // 設定で追加が指定されているヘッダーを追加
    // 特殊変数の置換: $client_ip, $host, $request_uri
    for (header_name, header_value) in &security.add_request_headers {
        // 特殊変数を置換
        let host_str = headers.iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(b"host"))
            .map(|(_, v)| std::str::from_utf8(v).unwrap_or("-"))
            .unwrap_or("-");
        
        let value_replaced = header_value
            .replace("$client_ip", client_ip)
            .replace("$host", host_str)
            .replace("$request_uri", path_str);
        
        // Header Injection防止チェック
        if !is_valid_header_value(value_replaced.as_bytes()) {
            warn!("Invalid add_request_header value: {}", header_name);
            continue;
        }
        
        request.extend_from_slice(header_name.as_bytes());
        request.extend_from_slice(HEADER_COLON);
        request.extend_from_slice(value_replaced.as_bytes());
        request.extend_from_slice(HEADER_CRLF);
    }
    
    // バックエンドにはKeep-Aliveを要求
    request.extend_from_slice(HEADER_CONNECTION_KEEPALIVE_END);

    let result = if target.use_tls {
        proxy_https_pooled(client_stream, target, security, compression, client_encoding, &pool_key, request, content_length, is_chunked, initial_body, client_wants_close).await
    } else if target.use_h2c {
        // H2C (HTTP/2 over cleartext) 接続
        #[cfg(feature = "http2")]
        {
            proxy_h2c(
                client_stream, 
                target, 
                security,
                method, 
                final_path.as_bytes(), 
                headers, 
                initial_body,
                client_wants_close
            ).await
        }
        #[cfg(not(feature = "http2"))]
        {
            // HTTP/2 feature が無効な場合はHTTP/1.1にフォールバック
            warn!("H2C requested but http2 feature not enabled, falling back to HTTP/1.1");
            proxy_http_pooled(client_stream, target, security, compression, client_encoding, &pool_key, request, content_length, is_chunked, initial_body, client_wants_close).await
        }
    } else {
        proxy_http_pooled(client_stream, target, security, compression, client_encoding, &pool_key, request, content_length, is_chunked, initial_body, client_wants_close).await
    };
    
    // 接続カウンターを減少（Least Connections 用）
    server.release();
    
    result
}

// ====================
// HTTP プロキシ（コネクションプール対応）
// ====================

async fn proxy_http_pooled(
    mut client_stream: ServerTls,
    target: &ProxyTarget,
    security: &SecurityConfig,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    pool_key: &str,
    request: Vec<u8>,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
    client_wants_close: bool,
) -> Option<(ServerTls, u16, u64, bool)> {
    // セキュリティ設定からタイムアウトを取得
    let connect_timeout = Duration::from_secs(security.backend_connect_timeout_secs);
    
    // プールから接続を取得、または新規作成
    let mut backend_stream = match HTTP_POOL.with(|p| p.borrow_mut().get(pool_key)) {
        Some(stream) => stream,
        None => {
            // 新規接続を作成
            let addr = format!("{}:{}", target.host, target.port);
            let connect_result = timeout(connect_timeout, TcpStream::connect(&addr)).await;
            
            match connect_result {
                Ok(Ok(stream)) => {
                    let _ = stream.set_nodelay(true);
                    stream
                }
                Ok(Err(e)) => {
                    error!("Proxy connect error to {}: {}", addr, e);
                    let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                    return Some((client_stream, 502, 0, true));
                }
                Err(_) => {
                    error!("Proxy connect timeout to {}", addr);
                    let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                    return Some((client_stream, 504, 0, true));
                }
            }
        }
    };

    // セキュリティ設定からchunked最大サイズを取得
    let max_chunked = security.max_chunked_body_size as u64;
    
    // 圧縮が有効かどうかの事前判定
    // 注意: 実際のContent-Typeはレスポンス受信後に判定するため、ここでは設定の有効/無効のみ確認
    let compression_enabled = compression.enabled && client_encoding != AcceptedEncoding::Identity;
    
    // リクエスト送信とレスポンス受信
    // kTLS 有効時は splice(2) を使用してゼロコピー転送
    // ただし、圧縮が有効な場合はkTLSを迂回（ユーザー空間で圧縮処理が必要）
    #[cfg(feature = "ktls")]
    let result = {
        // kTLS + splice 版を試みる（Content-Length の場合のみ有効、圧縮無効時のみ）
        if client_stream.is_ktls_enabled() && !is_chunked && !compression_enabled {
            let splice_result = proxy_http_request_splice(
                &client_stream,
                &backend_stream,
                &request,
                content_length,
                is_chunked,
                initial_body,
            ).await;
            
            if splice_result.is_some() {
                splice_result
            } else {
                // splice 版が失敗した場合は通常版にフォールバック
                proxy_http_request_with_compression(
                    &mut client_stream,
                    &mut backend_stream,
                    request,
                    content_length,
                    is_chunked,
                    initial_body,
                    max_chunked,
                    compression,
                    client_encoding,
                ).await
            }
        } else {
            // kTLS が無効、Chunked、または圧縮有効の場合は通常版を使用
            proxy_http_request_with_compression(
                &mut client_stream,
                &mut backend_stream,
                request,
                content_length,
                is_chunked,
                initial_body,
                max_chunked,
                compression,
                client_encoding,
            ).await
        }
    };
    
    #[cfg(not(feature = "ktls"))]
    let result = proxy_http_request_with_compression(
        &mut client_stream,
        &mut backend_stream,
        request,
        content_length,
        is_chunked,
        initial_body,
        max_chunked,
        compression,
        client_encoding,
    ).await;

    match result {
        Some((status_code, total, backend_wants_keep_alive)) => {
            // バックエンドがKeep-Aliveを許可している場合、プールに返却
            if backend_wants_keep_alive {
                let max_idle = security.max_idle_connections_per_host;
                let idle_timeout = security.idle_connection_timeout_secs;
                HTTP_POOL.with(|p| p.borrow_mut().put(pool_key.to_string(), backend_stream, max_idle, idle_timeout));
            }
            Some((client_stream, status_code, total, client_wants_close))
        }
        None => {
            // エラー発生時は接続を破棄
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            Some((client_stream, 502, 0, true))
        }
    }
}

// ====================
// H2C プロキシ (HTTP/2 over cleartext)
// ====================
//
// HTTP/2 Prior Knowledge モードでバックエンドに接続し、
// リクエストを転送します。gRPCバックエンドへの接続に適しています。
// ====================

/// H2C (HTTP/2 over cleartext) プロキシ
/// 
/// HTTP/2 Prior Knowledge モードでバックエンドに接続し、
/// リクエストを送信してレスポンスを受信します。
#[cfg(feature = "http2")]
async fn proxy_h2c(
    mut client_stream: ServerTls,
    target: &ProxyTarget,
    security: &SecurityConfig,
    method: &[u8],
    path: &[u8],
    headers: &[(Box<[u8]>, Box<[u8]>)],
    request_body: &[u8],
    client_wants_close: bool,
) -> Option<(ServerTls, u16, u64, bool)> {
    let connect_timeout = Duration::from_secs(security.backend_connect_timeout_secs);
    
    // バックエンドに接続
    let addr = format!("{}:{}", target.host, target.port);
    let connect_result = timeout(connect_timeout, TcpStream::connect(&addr)).await;
    
    let backend_stream = match connect_result {
        Ok(Ok(stream)) => {
            let _ = stream.set_nodelay(true);
            stream
        }
        Ok(Err(e)) => {
            error!("H2C connect error to {}: {}", addr, e);
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((client_stream, 502, 0, true));
        }
        Err(_) => {
            error!("H2C connect timeout to {}", addr);
            let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((client_stream, 504, 0, true));
        }
    };

    // H2Cクライアントを作成
    let settings = http2::Http2Settings::default();
    let mut h2c_client = http2::H2cClient::new(backend_stream, settings);

    // HTTP/2 ハンドシェイク
    if let Err(e) = h2c_client.handshake().await {
        error!("H2C handshake error: {}", e);
        let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
        let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
        return Some((client_stream, 502, 0, true));
    }

    // ヘッダーを変換 (Box<[u8]> -> &[u8])
    let headers_ref: Vec<(&[u8], &[u8])> = headers.iter()
        .map(|(k, v)| (k.as_ref(), v.as_ref()))
        .collect();

    // リクエストを送信
    let body = if request_body.is_empty() { None } else { Some(request_body) };
    let authority = target.host.as_bytes();
    
    let response = match h2c_client.send_request(method, path, authority, &headers_ref, body).await {
        Ok(resp) => resp,
        Err(e) => {
            error!("H2C request error: {}", e);
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((client_stream, 502, 0, true));
        }
    };

    // レスポンスをHTTP/1.1形式でクライアントに返す
    let status_code = response.status;
    let mut http11_response = Vec::with_capacity(512 + response.body.len());
    
    // ステータス行
    http11_response.extend_from_slice(b"HTTP/1.1 ");
    let mut status_buf = itoa::Buffer::new();
    http11_response.extend_from_slice(status_buf.format(status_code).as_bytes());
    http11_response.extend_from_slice(b" ");
    http11_response.extend_from_slice(status_reason_phrase(status_code).as_bytes());
    http11_response.extend_from_slice(b"\r\n");

    // レスポンスヘッダー
    for (name, value) in &response.headers {
        // ホップバイホップヘッダーはスキップ
        if name.eq_ignore_ascii_case(b"connection") 
            || name.eq_ignore_ascii_case(b"transfer-encoding")
            || name.eq_ignore_ascii_case(b"keep-alive")
        {
            continue;
        }
        http11_response.extend_from_slice(name);
        http11_response.extend_from_slice(b": ");
        http11_response.extend_from_slice(value);
        http11_response.extend_from_slice(b"\r\n");
    }

    // Content-Length
    http11_response.extend_from_slice(b"Content-Length: ");
    http11_response.extend_from_slice(status_buf.format(response.body.len()).as_bytes());
    http11_response.extend_from_slice(b"\r\n");

    // Connection ヘッダー
    if client_wants_close {
        http11_response.extend_from_slice(b"Connection: close\r\n");
    } else {
        http11_response.extend_from_slice(b"Connection: keep-alive\r\n");
    }

    http11_response.extend_from_slice(b"\r\n");

    // ボディ
    http11_response.extend_from_slice(&response.body);

    let resp_size = http11_response.len() as u64;

    // クライアントに送信
    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(http11_response)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return None;
    }

    Some((client_stream, status_code, resp_size, client_wants_close))
}

/// ステータスコードに対応する理由フレーズを返す
#[cfg(feature = "http2")]
fn status_reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

// ====================
// HTTPリクエスト送信とレスポンス受信（圧縮対応版）
// ====================
//
// 圧縮設定が有効な場合、バックエンドからのレスポンスを動的に圧縮して
// クライアントに転送します。
// 
// 圧縮判定:
// 1. compression.enabled が true
// 2. クライアントが Accept-Encoding で圧縮をサポート
// 3. Content-Type が圧縮対象
// 4. Content-Length が min_size 以上
// 5. バックエンドのレスポンスが未圧縮
// ====================

/// HTTPリクエストを送信してレスポンスを受信（圧縮対応版）
/// 戻り値: Option<(status_code, response_size, backend_wants_keep_alive)>
async fn proxy_http_request_with_compression(
    client_stream: &mut ServerTls,
    backend_stream: &mut TcpStream,
    request: Vec<u8>,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
    max_chunked_body_size: u64,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
) -> Option<(u16, u64, bool)> {
    // 1. リクエストヘッダー送信（タイムアウト付き）
    let write_result = timeout(WRITE_TIMEOUT, backend_stream.write_all(request)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return None;
    }

    // 2. リクエストボディ送信
    if !initial_body.is_empty() {
        let body_buf = initial_body.to_vec();
        let write_result = timeout(WRITE_TIMEOUT, backend_stream.write_all(body_buf)).await;
        if !matches!(write_result, Ok((Ok(_), _))) {
            return None;
        }
    }

    // 3. 残りのリクエストボディを転送
    if is_chunked {
        // Chunked転送の場合（DoS対策: ルートごとのmax_chunked_body_sizeで制限）
        match transfer_chunked_body(client_stream, backend_stream, initial_body, max_chunked_body_size).await {
            ChunkedTransferResult::Complete => {}
            ChunkedTransferResult::Failed => return None,
            ChunkedTransferResult::SizeLimitExceeded => {
                return None;
            }
        }
    } else {
        // Content-Length転送の場合
        let remaining = content_length.saturating_sub(initial_body.len());
        if remaining > 0 {
            let transferred = transfer_exact_bytes(client_stream, backend_stream, remaining).await;
            if transferred < remaining as u64 {
                return None;
            }
        }
    }

    // 4. レスポンスを受信して転送（圧縮対応）
    let (total, status_code, backend_wants_keep_alive) = 
        transfer_response_with_compression(backend_stream, client_stream, compression, client_encoding).await;

    Some((status_code, total, backend_wants_keep_alive))
}

// ====================
// レスポンス転送（圧縮対応版）
// ====================

/// レスポンスヘッダーを解析し、必要に応じて圧縮してクライアントに転送
async fn transfer_response_with_compression(
    backend_stream: &mut TcpStream,
    client_stream: &mut ServerTls,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
) -> (u64, u16, bool) {
    let mut accumulated = Vec::with_capacity(BUF_SIZE);
    let mut total = 0u64;
    let mut status_code = 502u16;
    // 初期値false: エラー時はKeep-Aliveを無効化
    let mut backend_wants_keep_alive = false;

    // ヘッダー読み取り用バッファ
    loop {
        let read_buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
        
        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => {
                return (total, status_code, backend_wants_keep_alive);
            }
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                return (total, status_code, backend_wants_keep_alive);
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                return (total, status_code, backend_wants_keep_alive);
            }
        };
        
        returned_buf.set_valid_len(n);
        accumulated.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);
        
        // ヘッダーが完全に受信されたかチェック
        if let Some(parsed) = parse_http_response(&accumulated) {
            status_code = parsed.status_code;
            backend_wants_keep_alive = !parsed.is_connection_close;
            
            let header_len = parsed.header_len;
            let body_start = &accumulated[header_len..];
            
            // Content-Type と Content-Encoding を取得
            let content_type = extract_header_value(&accumulated[..header_len], b"content-type");
            let existing_encoding = extract_header_value(&accumulated[..header_len], b"content-encoding");
            
            // 圧縮すべきか判定
            let should_compress = compression.should_compress(
                client_encoding,
                content_type,
                parsed.content_length,
                existing_encoding,
            );
            
            if let Some(encoding) = should_compress {
                // 圧縮有効: ヘッダーを書き換えて圧縮転送
                let result = transfer_compressed_response(
                    client_stream,
                    backend_stream,
                    &accumulated[..header_len],
                    body_start,
                    parsed.content_length,
                    parsed.is_chunked,
                    encoding,
                    compression,
                    backend_wants_keep_alive,
                ).await;
                
                return (result.0, status_code, result.1);
            } else {
                // 圧縮無効: そのまま転送（既存ロジック）
                // ヘッダーをそのまま送信
                let header_data = accumulated[..header_len].to_vec();
                let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(header_data)).await;
                if !matches!(write_result, Ok((Ok(_), _))) {
                    return (total, status_code, false);
                }
                total += header_len as u64;
                
                // 初期ボディを送信
                if !body_start.is_empty() {
                    let body_data = body_start.to_vec();
                    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(body_data)).await;
                    if !matches!(write_result, Ok((Ok(_), _))) {
                        return (total, status_code, false);
                    }
                    total += body_start.len() as u64;
                }
                
                // 残りのボディを転送
                let body_remaining = if let Some(cl) = parsed.content_length {
                    cl.saturating_sub(body_start.len())
                } else if parsed.is_chunked {
                    // Chunked の場合は終端まで転送
                    usize::MAX
                } else {
                    0
                };
                
                if body_remaining > 0 {
                    let transferred = transfer_response_body(
                        backend_stream,
                        client_stream,
                        parsed.content_length,
                        parsed.is_chunked,
                        body_start,
                    ).await;
                    total += transferred;
                }
                
                return (total, status_code, backend_wants_keep_alive);
            }
        }
        
        // ヘッダーが大きすぎる場合は中止
        if accumulated.len() > MAX_HEADER_SIZE {
            return (0, 502, false);
        }
    }
}

/// ヘッダーから特定のヘッダー値を抽出
fn extract_header_value<'a>(header_data: &'a [u8], header_name: &[u8]) -> Option<&'a [u8]> {
    let mut headers_storage = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers_storage);
    
    if response.parse(header_data).is_ok() {
        for header in response.headers.iter() {
            if header.name.as_bytes().eq_ignore_ascii_case(header_name) {
                return Some(header.value);
            }
        }
    }
    None
}

/// 圧縮してレスポンスを転送
/// 戻り値: (転送バイト数, backend_wants_keep_alive)
async fn transfer_compressed_response(
    client_stream: &mut ServerTls,
    backend_stream: &mut TcpStream,
    original_headers: &[u8],
    initial_body: &[u8],
    content_length: Option<usize>,
    is_chunked: bool,
    encoding: AcceptedEncoding,
    compression: &CompressionConfig,
    backend_wants_keep_alive: bool,
) -> (u64, bool) {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    
    let mut total = 0u64;
    
    // 1. まず全てのボディデータを収集（ストリーミングは将来の改善）
    let mut body_data = initial_body.to_vec();
    
    if let Some(cl) = content_length {
        let remaining = cl.saturating_sub(initial_body.len());
        if remaining > 0 {
            let mut remaining_to_read = remaining;
            while remaining_to_read > 0 {
                let read_buf = buf_get();
                let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
                
                let (res, mut returned_buf) = match read_result {
                    Ok(result) => result,
                    Err(_) => {
                        return (total, false);
                    }
                };
                
                let n = match res {
                    Ok(0) => {
                        buf_put(returned_buf);
                        break;
                    }
                    Ok(n) => n.min(remaining_to_read),
                    Err(_) => {
                        buf_put(returned_buf);
                        return (total, false);
                    }
                };
                
                returned_buf.set_valid_len(n);
                body_data.extend_from_slice(returned_buf.as_valid_slice());
                buf_put(returned_buf);
                remaining_to_read = remaining_to_read.saturating_sub(n);
            }
        }
    } else if is_chunked {
        // Chunked の場合はデコードして収集
        let mut decoder = ChunkedDecoder::new_unlimited();
        
        // 初期ボディをデコーダにフィード
        let initial_result = decoder.feed(initial_body);
        if initial_result == ChunkedFeedResult::Complete {
            // 初期ボディで完了（本来はデコード済みボディが必要だが、簡略化）
        } else {
            // 残りを読み取り
            loop {
                let read_buf = buf_get();
                let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
                
                let (res, mut returned_buf) = match read_result {
                    Ok(result) => result,
                    Err(_) => {
                        // タイムアウト
                        break;
                    }
                };
                
                let n = match res {
                    Ok(0) => {
                        buf_put(returned_buf);
                        break;
                    }
                    Ok(n) => n,
                    Err(_) => {
                        buf_put(returned_buf);
                        break;
                    }
                };
                
                returned_buf.set_valid_len(n);
                let chunk = returned_buf.as_valid_slice();
                body_data.extend_from_slice(chunk);
                let feed_result = decoder.feed(chunk);
                buf_put(returned_buf);
                
                if feed_result == ChunkedFeedResult::Complete {
                    break;
                }
            }
        }
    }
    
    // 2. ボディを圧縮
    let compressed_body = match encoding {
        AcceptedEncoding::Zstd => {
            match zstd::encode_all(std::io::Cursor::new(&body_data), compression.zstd_level) {
                Ok(compressed) => compressed,
                Err(_) => {
                    return transfer_uncompressed_fallback(
                        client_stream,
                        original_headers,
                        &body_data,
                    ).await;
                }
            }
        }
        AcceptedEncoding::Gzip => {
            let level = Compression::new(compression.gzip_level);
            let mut encoder = GzEncoder::new(Vec::new(), level);
            if encoder.write_all(&body_data).is_err() {
                // 圧縮失敗: 非圧縮で送信
                return transfer_uncompressed_fallback(
                    client_stream,
                    original_headers,
                    &body_data,
                ).await;
            }
            match encoder.finish() {
                Ok(data) => data,
                Err(_) => {
                    return transfer_uncompressed_fallback(
                        client_stream,
                        original_headers,
                        &body_data,
                    ).await;
                }
            }
        }
        AcceptedEncoding::Brotli => {
            let mut compressed = Vec::new();
            let params = brotli::enc::BrotliEncoderParams {
                quality: compression.brotli_level as i32,
                ..Default::default()
            };
            let mut input = std::io::Cursor::new(&body_data);
            if brotli::BrotliCompress(&mut input, &mut compressed, &params).is_err() {
                return transfer_uncompressed_fallback(
                    client_stream,
                    original_headers,
                    &body_data,
                ).await;
            }
            compressed
        }
        AcceptedEncoding::Deflate => {
            let level = Compression::new(compression.gzip_level);
            let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), level);
            if encoder.write_all(&body_data).is_err() {
                return transfer_uncompressed_fallback(
                    client_stream,
                    original_headers,
                    &body_data,
                ).await;
            }
            match encoder.finish() {
                Ok(data) => data,
                Err(_) => {
                    return transfer_uncompressed_fallback(
                        client_stream,
                        original_headers,
                        &body_data,
                    ).await;
                }
            }
        }
        AcceptedEncoding::Identity => {
            // 圧縮なし（ここには来ないはず）
            body_data.clone()
        }
    };
    
    // 3. 新しいヘッダーを構築
    let new_headers = build_compressed_headers(
        original_headers,
        encoding,
        compressed_body.len(),
    );
    
    // 4. ヘッダー送信
    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(new_headers.clone())).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return (total, false);
    }
    total += new_headers.len() as u64;
    
    // 5. 圧縮済みボディ送信
    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(compressed_body.clone())).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return (total, false);
    }
    total += compressed_body.len() as u64;
    
    (total, backend_wants_keep_alive)
}

/// 圧縮失敗時のフォールバック（非圧縮で送信）
async fn transfer_uncompressed_fallback(
    client_stream: &mut ServerTls,
    original_headers: &[u8],
    body_data: &[u8],
) -> (u64, bool) {
    let mut total = 0u64;
    
    // ヘッダー送信
    let headers = original_headers.to_vec();
    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(headers.clone())).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return (total, false);
    }
    total += headers.len() as u64;
    
    // ボディ送信
    let body = body_data.to_vec();
    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(body.clone())).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return (total, false);
    }
    total += body.len() as u64;
    
    (total, true)
}

/// HTTPステータスコードからリーズンフレーズを取得
fn status_code_to_reason(status_code: u16) -> &'static str {
    match status_code {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

/// 圧縮用にヘッダーを書き換え
fn build_compressed_headers(
    original_headers: &[u8],
    encoding: AcceptedEncoding,
    compressed_length: usize,
) -> Vec<u8> {
    let mut headers_storage = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers_storage);
    
    if response.parse(original_headers).is_err() {
        return original_headers.to_vec();
    }
    
    let status_code = response.code.unwrap_or(200);
    let reason = status_code_to_reason(status_code);
    
    let mut new_headers = Vec::with_capacity(original_headers.len() + 64);
    
    // ステータス行
    new_headers.extend_from_slice(b"HTTP/1.1 ");
    let mut code_buf = itoa::Buffer::new();
    new_headers.extend_from_slice(code_buf.format(status_code).as_bytes());
    new_headers.extend_from_slice(b" ");
    new_headers.extend_from_slice(reason.as_bytes());
    new_headers.extend_from_slice(b"\r\n");
    
    // 元のヘッダーをコピー（Content-Length, Content-Encoding, Transfer-Encoding を除く）
    for header in response.headers.iter() {
        let name_lower = header.name.to_ascii_lowercase();
        if name_lower == "content-length" 
            || name_lower == "content-encoding"
            || name_lower == "transfer-encoding" {
            continue;
        }
        new_headers.extend_from_slice(header.name.as_bytes());
        new_headers.extend_from_slice(b": ");
        new_headers.extend_from_slice(header.value);
        new_headers.extend_from_slice(b"\r\n");
    }
    
    // Content-Encoding を追加
    new_headers.extend_from_slice(b"Content-Encoding: ");
    new_headers.extend_from_slice(encoding.as_header_value());
    new_headers.extend_from_slice(b"\r\n");
    
    // Content-Length を追加（圧縮後のサイズ）
    new_headers.extend_from_slice(b"Content-Length: ");
    let mut len_buf = itoa::Buffer::new();
    new_headers.extend_from_slice(len_buf.format(compressed_length).as_bytes());
    new_headers.extend_from_slice(b"\r\n");
    
    // Vary ヘッダーを追加（キャッシュ制御）
    new_headers.extend_from_slice(b"Vary: Accept-Encoding\r\n");
    
    // ヘッダー終端
    new_headers.extend_from_slice(b"\r\n");
    
    new_headers
}

/// レスポンスボディを転送（圧縮なし）
async fn transfer_response_body(
    backend_stream: &mut TcpStream,
    client_stream: &mut ServerTls,
    content_length: Option<usize>,
    is_chunked: bool,
    initial_body: &[u8],
) -> u64 {
    let mut total = 0u64;
    
    if let Some(cl) = content_length {
        let remaining = cl.saturating_sub(initial_body.len());
        if remaining > 0 {
            let transferred = transfer_exact_bytes_from_backend(backend_stream, client_stream, remaining).await;
            total += transferred;
        }
    } else if is_chunked {
        // Chunked 転送
        let mut decoder = ChunkedDecoder::new_unlimited();
        decoder.feed(initial_body);
        
        if decoder.is_complete() {
            return total;
        }
        
        loop {
            let read_buf = buf_get();
            let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
            
            let (res, mut returned_buf) = match read_result {
                Ok(result) => result,
                Err(_) => break,
            };
            
            let n = match res {
                Ok(0) => {
                    buf_put(returned_buf);
                    break;
                }
                Ok(n) => n,
                Err(_) => {
                    buf_put(returned_buf);
                    break;
                }
            };
            
            returned_buf.set_valid_len(n);
            let chunk = returned_buf.as_valid_slice();
            let feed_result = decoder.feed(chunk);
            
            // クライアントに転送
            let chunk_data = chunk.to_vec();
            let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(chunk_data)).await;
            buf_put(returned_buf);
            
            if !matches!(write_result, Ok((Ok(_), _))) {
                break;
            }
            total += n as u64;
            
            if feed_result == ChunkedFeedResult::Complete {
                break;
            }
        }
    }
    
    total
}

/// バックエンドから正確なバイト数を読み取りクライアントに転送
async fn transfer_exact_bytes_from_backend(
    backend_stream: &mut TcpStream,
    client_stream: &mut ServerTls,
    mut remaining: usize,
) -> u64 {
    let mut total = 0u64;
    
    while remaining > 0 {
        let read_buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
        
        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => break,
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                break;
            }
            Ok(n) => n.min(remaining),
            Err(_) => {
                buf_put(returned_buf);
                break;
            }
        };
        
        returned_buf.set_valid_len(n);
        let chunk = returned_buf.as_valid_slice().to_vec();
        let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(chunk)).await;
        buf_put(returned_buf);
        
        if !matches!(write_result, Ok((Ok(_), _))) {
            break;
        }
        
        total += n as u64;
        remaining = remaining.saturating_sub(n);
    }
    
    total
}

// ====================
// kTLS + splice(2) によるHTTPプロキシ（高速版）
// ====================
//
// kTLS が有効な場合、splice(2) を使用してカーネル空間で直接
// データを転送します。HTTPバックエンド（平文）への接続で効果的です。
//
// 注意: Chunked 転送の場合は終端検出のためユーザー空間での
// 処理が必要なため、splice は使用しません。
// ====================

/// kTLS + splice によるボディ転送（Content-Length固定長のみ）
///
/// FD間でsplice(2)を使用してゼロコピー転送を行います。
/// 非ブロッキングソケットに対応し、WouldBlockの場合は待機します。
///
/// splice(2) によるボディ転送（固定長）
#[cfg(feature = "ktls")]
async fn splice_body_transfer(
    src_stream: &TcpStream,
    dst_stream: &TcpStream,
    pipe: &ktls_rustls::SplicePipe,
    mut remaining: usize,
) -> u64 {
    use std::os::unix::io::AsRawFd;
    
    let src_fd = src_stream.as_raw_fd();
    let dst_fd = dst_stream.as_raw_fd();
    let mut total = 0u64;
    
    const SPLICE_CHUNK_SIZE: usize = 65536;
    
    while remaining > 0 {
        let chunk_size = remaining.min(SPLICE_CHUNK_SIZE);
        
        match pipe.transfer(src_fd, dst_fd, chunk_size) {
            Ok(0) => break,
            Ok(n) => {
                total += n as u64;
                remaining = remaining.saturating_sub(n);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // 読み取り可能になるまで待機
                if let Err(_) = src_stream.readable(false).await {
                    break;
                }
            }
            Err(e) => {
                warn!("splice body transfer error: {}", e);
                break;
            }
        }
    }
    
    total
}

// ====================
// kTLS + splice(2) によるHTTPプロキシ転送
// ====================
//
// kTLS が有効な場合、以下のフローでゼロコピー転送を実現：
//
// [リクエスト] クライアント(kTLS) → splice → バックエンド(TCP)
//   1. ヘッダー: raw_read で読み取り → パース → raw_write で送信
//   2. ボディ(Content-Length): splice(2) でゼロコピー転送
//   3. ボディ(Chunked): 通常の転送（終端検出が必要）
//
// [レスポンス] バックエンド(TCP) → splice → クライアント(kTLS)
//   1. ヘッダー: raw_read で読み取り → パース → raw_write で送信
//   2. ボディ(Content-Length): splice(2) でゼロコピー転送
//   3. ボディ(Chunked): 通常の転送（終端検出が必要）
// ====================

/// kTLS + splice を使用したHTTPプロキシリクエスト処理
///
/// Content-Length が指定されている場合はボディ転送に splice を使用。
/// Chunked 転送の場合は通常の転送を使用。
#[cfg(feature = "ktls")]
async fn proxy_http_request_splice(
    client_stream: &KtlsServerStream,
    backend_stream: &TcpStream,
    request: &[u8],
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
) -> Option<(u16, u64, bool)> {
    // splice パイプを取得
    let pipe_ref = get_splice_pipe();
    let pipe = match pipe_ref.as_ref() {
        Some(p) => p,
        None => {
            warn!("splice pipe not available, falling back to normal transfer");
            return None;
        }
    };
    
    // kTLS が有効でない場合はフォールバック
    if !client_stream.is_ktls_enabled() {
        return None;
    }
    
    let client_tcp = client_stream.get_ref();
    
    // 1. リクエストヘッダーをバックエンドに送信（raw_write）
    if let Err(e) = async_raw_write_all(backend_stream, request).await {
        warn!("Failed to send request header: {}", e);
        return None;
    }
    
    // 2. 初期ボディがあれば送信
    if !initial_body.is_empty() {
        if let Err(e) = async_raw_write_all(backend_stream, initial_body).await {
            warn!("Failed to send initial body: {}", e);
            return None;
        }
    }
    
    // 3. 残りのリクエストボディを転送
    let remaining_body = content_length.saturating_sub(initial_body.len());
    if remaining_body > 0 {
        if is_chunked {
            // Chunked 転送はフォールバック（終端検出が必要）
            return None;
        }
        
        // Content-Length の場合: splice でゼロコピー転送
        // kTLS クライアント → バックエンド TCP
        let transferred = splice_body_transfer(
            client_tcp,
            backend_stream,
            pipe,
            remaining_body,
        ).await;
        
        if transferred < remaining_body as u64 {
            warn!("Request body transfer incomplete: {} < {}", transferred, remaining_body);
            return None;
        }
    }
    
    // 4. レスポンスを受信して転送（splice 使用）
    let result = splice_transfer_response_ktls(
        backend_stream,
        client_stream,
        pipe,
    ).await;
    
    Some(result)
}

/// kTLS + splice によるレスポンス転送
///
/// バックエンド(TCP) からヘッダーを読み取り、パースしてクライアント(kTLS)に送信。
/// ボディは Content-Length の場合は splice、Chunked の場合は通常転送。
#[cfg(feature = "ktls")]
async fn splice_transfer_response_ktls(
    backend_stream: &TcpStream,
    client_stream: &KtlsServerStream,
    pipe: &ktls_rustls::SplicePipe,
) -> (u16, u64, bool) {
    let client_tcp = client_stream.get_ref();
    
    let mut total = 0u64;
    let mut status_code = 502u16;
    let mut accumulated = Vec::with_capacity(4096);
    let mut backend_wants_keep_alive: bool;
    
    // ヘッダー読み取り用バッファ
    let mut header_buf = [0u8; 8192];
    
    // 1. ヘッダーを読み取り（raw_read + パース）
    loop {
        // バックエンドからヘッダーを読み取り
        let n = match async_raw_read(backend_stream, &mut header_buf).await {
            Ok(0) => {
                // EOF
                return (status_code, total, false);
            }
            Ok(n) => n,
            Err(e) => {
                warn!("Failed to read response header: {}", e);
                return (status_code, total, false);
            }
        };
        
        accumulated.extend_from_slice(&header_buf[..n]);
        
        // ヘッダーが完全に受信されたかチェック
        if let Some(parsed) = parse_http_response(&accumulated) {
            status_code = parsed.status_code;
            backend_wants_keep_alive = !parsed.is_connection_close;
            
            let header_len = parsed.header_len;
            let body_start_len = accumulated.len().saturating_sub(header_len);
            
            // ヘッダー + 初期ボディをクライアントに送信（raw_write）
            if let Err(e) = async_raw_write_all(client_tcp, &accumulated).await {
                warn!("Failed to send response header: {}", e);
                return (status_code, total, false);
            }
            total += accumulated.len() as u64;
            
            // ボディ転送
            if parsed.is_chunked {
                // Chunked 転送: 通常の方法で転送（終端検出が必要）
                // レスポンス受信時は制限なし（バックエンドを信頼）
                let mut chunked_decoder = ChunkedDecoder::new_unlimited();
                
                // 初期ボディ部分をデコーダにフィード
                if body_start_len > 0 {
                    if chunked_decoder.feed(&accumulated[header_len..]) == ChunkedFeedResult::Complete {
                        // 初期ボディで完了
                        return (status_code, total, backend_wants_keep_alive);
                    }
                }
                
                // 残りの Chunked ボディを転送
                loop {
                    let n = match async_raw_read(backend_stream, &mut header_buf).await {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => {
                            backend_wants_keep_alive = false;
                            break;
                        }
                    };
                    
                    let feed_result = chunked_decoder.feed(&header_buf[..n]);
                    
                    if let Err(_) = async_raw_write_all(client_tcp, &header_buf[..n]).await {
                        backend_wants_keep_alive = false;
                        break;
                    }
                    total += n as u64;
                    
                    if feed_result == ChunkedFeedResult::Complete {
                        break;
                    }
                }
            } else if let Some(content_length) = parsed.content_length {
                // Content-Length 転送: splice でゼロコピー
                let remaining = content_length.saturating_sub(body_start_len);
                
                if remaining > 0 {
                    let transferred = splice_body_transfer(
                        backend_stream,
                        client_tcp,
                        pipe,
                        remaining,
                    ).await;
                    
                    total += transferred;
                    
                    if transferred < remaining as u64 {
                        backend_wants_keep_alive = false;
                    }
                }
            } else {
                // Content-Length も Chunked もない場合: 接続クローズまで読み取り
                // この場合は Keep-Alive 不可
                backend_wants_keep_alive = false;
                
                loop {
                    let n = match async_raw_read(backend_stream, &mut header_buf).await {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    
                    if let Err(_) = async_raw_write_all(client_tcp, &header_buf[..n]).await {
                        break;
                    }
                    total += n as u64;
                }
            }
            
            return (status_code, total, backend_wants_keep_alive);
        }
        
        // ヘッダーが大きすぎる場合は中止
        if accumulated.len() > MAX_HEADER_SIZE {
            warn!("Response header too large");
            return (502, 0, false);
        }
    }
}

// ====================
// HTTPS プロキシ（コネクションプール対応）
// ====================

async fn proxy_https_pooled(
    mut client_stream: ServerTls,
    target: &ProxyTarget,
    security: &SecurityConfig,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    pool_key: &str,
    request: Vec<u8>,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
    client_wants_close: bool,
) -> Option<(ServerTls, u16, u64, bool)> {
    // セキュリティ設定からタイムアウトを取得
    let connect_timeout = Duration::from_secs(security.backend_connect_timeout_secs);
    
    // プールから接続を取得、または新規作成
    let mut backend_stream = match HTTPS_POOL.with(|p| p.borrow_mut().get(pool_key)) {
        Some(stream) => stream,
        None => {
            // 新規TCP接続を作成
            let addr = format!("{}:{}", target.host, target.port);
            let connect_result = timeout(connect_timeout, TcpStream::connect(&addr)).await;
            
            let backend_tcp = match connect_result {
                Ok(Ok(stream)) => {
                    let _ = stream.set_nodelay(true);
                    stream
                }
                Ok(Err(e)) => {
                    error!("Proxy connect error to {}: {}", addr, e);
                    let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                    return Some((client_stream, 502, 0, true));
                }
                Err(_) => {
                    error!("Proxy connect timeout to {}", addr);
                    let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                    return Some((client_stream, 504, 0, true));
                }
            };
            
            // TLS接続（タイムアウト付き）
            // SNI名を使用（sni_nameが設定されていればそれを使用、なければhostを使用）
            let sni = target.sni();
            let connector = TLS_CONNECTOR.with(|c| c.clone());
            let tls_result = timeout(connect_timeout, connector.connect(backend_tcp, sni)).await;
            
            match tls_result {
                Ok(Ok(stream)) => stream,
                Ok(Err(e)) => {
                    error!("TLS connect error to {} (SNI: {}): {}", target.host, sni, e);
                    let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                    return Some((client_stream, 502, 0, true));
                }
                Err(_) => {
                    error!("TLS connect timeout to {} (SNI: {})", target.host, sni);
                    let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                    return Some((client_stream, 504, 0, true));
                }
            }
        }
    };

    // セキュリティ設定からchunked最大サイズを取得
    let max_chunked = security.max_chunked_body_size as u64;
    
    // リクエスト送信とレスポンス受信（圧縮対応）
    let result = proxy_https_request_with_compression(
        &mut client_stream,
        &mut backend_stream,
        request,
        content_length,
        is_chunked,
        initial_body,
        max_chunked,
        compression,
        client_encoding,
    ).await;

    match result {
        Some((status_code, total, backend_wants_keep_alive)) => {
            // バックエンドがKeep-Aliveを許可している場合、プールに返却
            if backend_wants_keep_alive {
                let max_idle = security.max_idle_connections_per_host;
                let idle_timeout = security.idle_connection_timeout_secs;
                HTTPS_POOL.with(|p| p.borrow_mut().put(pool_key.to_string(), backend_stream, max_idle, idle_timeout));
            }
            Some((client_stream, status_code, total, client_wants_close))
        }
        None => {
            // エラー発生時は接続を破棄
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            Some((client_stream, 502, 0, true))
        }
    }
}

/// HTTPSリクエストを送信してレスポンスを受信（圧縮対応版）
/// 戻り値: Option<(status_code, response_size, backend_wants_keep_alive)>
async fn proxy_https_request_with_compression(
    client_stream: &mut ServerTls,
    backend_stream: &mut ClientTls,
    request: Vec<u8>,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
    max_chunked_body_size: u64,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
) -> Option<(u16, u64, bool)> {
    // 1. リクエストヘッダー送信
    let write_result = timeout(WRITE_TIMEOUT, backend_stream.write_all(request)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return None;
    }

    // 2. リクエストボディ送信
    if !initial_body.is_empty() {
        let body_buf = initial_body.to_vec();
        let write_result = timeout(WRITE_TIMEOUT, backend_stream.write_all(body_buf)).await;
        if !matches!(write_result, Ok((Ok(_), _))) {
            return None;
        }
    }

    // 3. 残りのリクエストボディを転送
    if is_chunked {
        match transfer_chunked_body(client_stream, backend_stream, initial_body, max_chunked_body_size).await {
            ChunkedTransferResult::Complete => {}
            ChunkedTransferResult::Failed => return None,
            ChunkedTransferResult::SizeLimitExceeded => {
                return None;
            }
        }
    } else {
        let remaining = content_length.saturating_sub(initial_body.len());
        if remaining > 0 {
            let transferred = transfer_exact_bytes(client_stream, backend_stream, remaining).await;
            if transferred < remaining as u64 {
                return None;
            }
        }
    }

    // 4. レスポンスを受信して転送（圧縮対応）
    let (total, status_code, backend_wants_keep_alive) = 
        transfer_https_response_with_compression(backend_stream, client_stream, compression, client_encoding).await;

    Some((status_code, total, backend_wants_keep_alive))
}

/// HTTPSレスポンス転送（圧縮対応版）
async fn transfer_https_response_with_compression(
    backend_stream: &mut ClientTls,
    client_stream: &mut ServerTls,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
) -> (u64, u16, bool) {
    let mut accumulated = Vec::with_capacity(BUF_SIZE);
    let mut total = 0u64;
    let mut status_code = 502u16;
    // 初期値false: エラー時はKeep-Aliveを無効化
    let mut backend_wants_keep_alive = false;

    // ヘッダー読み取り用バッファ
    loop {
        let read_buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
        
        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => {
                return (total, status_code, backend_wants_keep_alive);
            }
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                return (total, status_code, backend_wants_keep_alive);
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                return (total, status_code, backend_wants_keep_alive);
            }
        };
        
        returned_buf.set_valid_len(n);
        accumulated.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);
        
        // ヘッダーが完全に受信されたかチェック
        if let Some(parsed) = parse_http_response(&accumulated) {
            status_code = parsed.status_code;
            backend_wants_keep_alive = !parsed.is_connection_close;
            
            let header_len = parsed.header_len;
            let body_start = &accumulated[header_len..];
            
            // Content-Type と Content-Encoding を取得
            let content_type = extract_header_value(&accumulated[..header_len], b"content-type");
            let existing_encoding = extract_header_value(&accumulated[..header_len], b"content-encoding");
            
            // 圧縮すべきか判定
            let should_compress = compression.should_compress(
                client_encoding,
                content_type,
                parsed.content_length,
                existing_encoding,
            );
            
            if let Some(encoding) = should_compress {
                // 圧縮有効: ヘッダーを書き換えて圧縮転送
                let result = transfer_compressed_https_response(
                    client_stream,
                    backend_stream,
                    &accumulated[..header_len],
                    body_start,
                    parsed.content_length,
                    parsed.is_chunked,
                    encoding,
                    compression,
                    backend_wants_keep_alive,
                ).await;
                
                return (result.0, status_code, result.1);
            } else {
                // 圧縮無効: そのまま転送
                let header_data = accumulated[..header_len].to_vec();
                let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(header_data)).await;
                if !matches!(write_result, Ok((Ok(_), _))) {
                    return (total, status_code, false);
                }
                total += header_len as u64;
                
                if !body_start.is_empty() {
                    let body_data = body_start.to_vec();
                    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(body_data)).await;
                    if !matches!(write_result, Ok((Ok(_), _))) {
                        return (total, status_code, false);
                    }
                    total += body_start.len() as u64;
                }
                
                // 残りのボディを転送
                let body_remaining = if let Some(cl) = parsed.content_length {
                    cl.saturating_sub(body_start.len())
                } else if parsed.is_chunked {
                    usize::MAX
                } else {
                    0
                };
                
                if body_remaining > 0 {
                    let transferred = transfer_https_response_body(
                        backend_stream,
                        client_stream,
                        parsed.content_length,
                        parsed.is_chunked,
                        body_start,
                    ).await;
                    total += transferred;
                }
                
                return (total, status_code, backend_wants_keep_alive);
            }
        }
        
        if accumulated.len() > MAX_HEADER_SIZE {
            return (0, 502, false);
        }
    }
}

/// 圧縮してHTTPSレスポンスを転送
async fn transfer_compressed_https_response(
    client_stream: &mut ServerTls,
    backend_stream: &mut ClientTls,
    original_headers: &[u8],
    initial_body: &[u8],
    content_length: Option<usize>,
    is_chunked: bool,
    encoding: AcceptedEncoding,
    compression: &CompressionConfig,
    backend_wants_keep_alive: bool,
) -> (u64, bool) {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    
    let mut total = 0u64;
    
    // 1. まず全てのボディデータを収集
    let mut body_data = initial_body.to_vec();
    
    if let Some(cl) = content_length {
        let remaining = cl.saturating_sub(initial_body.len());
        if remaining > 0 {
            let mut remaining_to_read = remaining;
            while remaining_to_read > 0 {
                let read_buf = buf_get();
                let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
                
                let (res, mut returned_buf) = match read_result {
                    Ok(result) => result,
                    Err(_) => {
                        return (total, false);
                    }
                };
                
                let n = match res {
                    Ok(0) => {
                        buf_put(returned_buf);
                        break;
                    }
                    Ok(n) => n.min(remaining_to_read),
                    Err(_) => {
                        buf_put(returned_buf);
                        return (total, false);
                    }
                };
                
                returned_buf.set_valid_len(n);
                body_data.extend_from_slice(returned_buf.as_valid_slice());
                buf_put(returned_buf);
                remaining_to_read = remaining_to_read.saturating_sub(n);
            }
        }
    } else if is_chunked {
        let mut decoder = ChunkedDecoder::new_unlimited();
        decoder.feed(initial_body);
        
        loop {
            let read_buf = buf_get();
            let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
            
            let (res, mut returned_buf) = match read_result {
                Ok(result) => result,
                Err(_) => {
                    // タイムアウト
                    break;
                }
            };
            
            let n = match res {
                Ok(0) => {
                    buf_put(returned_buf);
                    break;
                }
                Ok(n) => n,
                Err(_) => {
                    buf_put(returned_buf);
                    break;
                }
            };
            
            returned_buf.set_valid_len(n);
            let chunk = returned_buf.as_valid_slice();
            body_data.extend_from_slice(chunk);
            let feed_result = decoder.feed(chunk);
            buf_put(returned_buf);
            
            if feed_result == ChunkedFeedResult::Complete {
                break;
            }
        }
    }
    
    // 2. ボディを圧縮
    let compressed_body = match encoding {
        AcceptedEncoding::Zstd => {
            match zstd::encode_all(std::io::Cursor::new(&body_data), compression.zstd_level) {
                Ok(compressed) => compressed,
                Err(_) => {
                    return transfer_uncompressed_fallback(client_stream, original_headers, &body_data).await;
                }
            }
        }
        AcceptedEncoding::Gzip => {
            let level = Compression::new(compression.gzip_level);
            let mut encoder = GzEncoder::new(Vec::new(), level);
            if encoder.write_all(&body_data).is_err() {
                return transfer_uncompressed_fallback(client_stream, original_headers, &body_data).await;
            }
            match encoder.finish() {
                Ok(data) => data,
                Err(_) => {
                    return transfer_uncompressed_fallback(client_stream, original_headers, &body_data).await;
                }
            }
        }
        AcceptedEncoding::Brotli => {
            let mut compressed = Vec::new();
            let params = brotli::enc::BrotliEncoderParams {
                quality: compression.brotli_level as i32,
                ..Default::default()
            };
            let mut input = std::io::Cursor::new(&body_data);
            if brotli::BrotliCompress(&mut input, &mut compressed, &params).is_err() {
                return transfer_uncompressed_fallback(client_stream, original_headers, &body_data).await;
            }
            compressed
        }
        AcceptedEncoding::Deflate => {
            let level = Compression::new(compression.gzip_level);
            let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), level);
            if encoder.write_all(&body_data).is_err() {
                return transfer_uncompressed_fallback(client_stream, original_headers, &body_data).await;
            }
            match encoder.finish() {
                Ok(data) => data,
                Err(_) => {
                    return transfer_uncompressed_fallback(client_stream, original_headers, &body_data).await;
                }
            }
        }
        AcceptedEncoding::Identity => {
            body_data.clone()
        }
    };
    
    // 3. 新しいヘッダーを構築
    let new_headers = build_compressed_headers(original_headers, encoding, compressed_body.len());
    
    // 4. ヘッダー送信
    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(new_headers.clone())).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return (total, false);
    }
    total += new_headers.len() as u64;
    
    // 5. 圧縮済みボディ送信
    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(compressed_body.clone())).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return (total, false);
    }
    total += compressed_body.len() as u64;
    
    (total, backend_wants_keep_alive)
}

/// HTTPSレスポンスボディを転送（圧縮なし）
async fn transfer_https_response_body(
    backend_stream: &mut ClientTls,
    client_stream: &mut ServerTls,
    content_length: Option<usize>,
    is_chunked: bool,
    initial_body: &[u8],
) -> u64 {
    let mut total = 0u64;
    
    if let Some(cl) = content_length {
        let remaining = cl.saturating_sub(initial_body.len());
        if remaining > 0 {
            let transferred = transfer_exact_bytes(backend_stream, client_stream, remaining).await;
            total += transferred;
        }
    } else if is_chunked {
        let mut decoder = ChunkedDecoder::new_unlimited();
        decoder.feed(initial_body);
        
        loop {
            let read_buf = buf_get();
            let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
            
            let (res, mut returned_buf) = match read_result {
                Ok(result) => result,
                Err(_) => break,
            };
            
            let n = match res {
                Ok(0) => {
                    buf_put(returned_buf);
                    break;
                }
                Ok(n) => n,
                Err(_) => {
                    buf_put(returned_buf);
                    break;
                }
            };
            
            returned_buf.set_valid_len(n);
            let chunk = returned_buf.as_valid_slice();
            let feed_result = decoder.feed(chunk);
            
            let chunk_data = chunk.to_vec();
            let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(chunk_data)).await;
            buf_put(returned_buf);
            
            if !matches!(write_result, Ok((Ok(_), _))) {
                break;
            }
            total += n as u64;
            
            if feed_result == ChunkedFeedResult::Complete {
                break;
            }
        }
    }
    
    total
}

// ====================
// 転送ヘルパー関数（ジェネリック版）
// ====================

/// 正確なバイト数を転送
async fn transfer_exact_bytes<R: AsyncReader, W: AsyncWriter>(
    reader: &mut R,
    writer: &mut W,
    mut remaining: usize,
) -> u64 {
    let mut total = 0u64;
    
    while remaining > 0 {
        let buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, reader.read_buf(buf)).await;
        
        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => return total,
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                break;
            }
            Ok(n) => n.min(remaining),
            Err(_) => {
                buf_put(returned_buf);
                break;
            }
        };
        
        // SafeReadBuffer の有効長を設定して書き込み用Vecに変換
        returned_buf.set_valid_len(n);
        let write_buf = returned_buf.into_truncated();
        
        let write_result = timeout(WRITE_TIMEOUT, writer.write_buf(write_buf)).await;
        match write_result {
            Ok((Ok(_), returned)) => {
                buf_put_vec(returned);
            }
            Ok((Err(_), returned)) => {
                buf_put_vec(returned);
                break;
            }
            Err(_) => break,
        }
        
        total += n as u64;
        remaining -= n;
    }
    
    total
}

// ====================
// kTLS + splice(2) によるゼロコピー転送（Linux 固有）
// ====================
//
// kTLS が有効な場合、splice(2) を使用してカーネル空間で直接
// データを転送します。これにより、ボディ転送時にユーザー空間への
// コピーが完全に不要になります。
//
// ## 実装状況
//
// - **ファイル送信（sendfile）**: kTLS有効時にゼロコピー対応 ✅
// - **プロキシ転送（splice）**: kTLS有効時 + Content-Length で対応 ✅
//
// ## プロキシ転送でのsplice使用
//
// libc::read/write を直接使用し、monoio の所有権ベース I/O を回避。
// 非同期待機は TcpStream::readable()/writable() を使用。
//
// ### 対応状況
// - Content-Length 転送: splice(2) でゼロコピー ✅
// - Chunked 転送: 通常転送（終端検出が必要なため）
//
// ## splice(2) の転送フロー
//
// [リクエスト] クライアント(kTLS) → splice → バックエンド(TCP)
//   1. ヘッダー: raw_read で読み取り → パース → raw_write で送信
//   2. ボディ: splice(2) でゼロコピー転送
//
// [レスポンス] バックエンド(TCP) → splice → クライアント(kTLS)
//   1. ヘッダー: raw_read で読み取り → パース → raw_write で送信
//   2. ボディ: splice(2) でゼロコピー転送
//
// 注意: splice(2) は少なくとも一方のFDがパイプである必要があります。
// ====================

/// Chunkedボディ転送の結果
#[derive(Debug, Clone, Copy, PartialEq)]
enum ChunkedTransferResult {
    /// 転送完了
    Complete,
    /// 転送失敗（I/Oエラー等）
    Failed,
    /// サイズ制限超過（DoS対策）
    SizeLimitExceeded,
}

/// Chunkedボディを転送（ステートマシンベース）
/// 
/// RFC 7230準拠のChunkedDecoderを使用して、トレーラーの有無に
/// かかわらず正確に終端を検出します。
/// 
/// DoS対策として、max_body_size を超えた場合は転送を中止します。
/// 
/// # Arguments
/// * `reader` - 読み取り元ストリーム
/// * `writer` - 書き込み先ストリーム
/// * `initial_body` - 初期ボディデータ（ヘッダー後に既に読み取り済みのデータ）
/// * `max_body_size` - 最大許容ボディサイズ（0の場合は制限なし）
async fn transfer_chunked_body<R: AsyncReader, W: AsyncWriter>(
    reader: &mut R,
    writer: &mut W,
    initial_body: &[u8],
    max_body_size: u64,
) -> ChunkedTransferResult {
    let mut decoder = ChunkedDecoder::new(max_body_size);
    
    // 初期ボディが既に終端を含んでいるかチェック
    if !initial_body.is_empty() {
        match decoder.feed(initial_body) {
            ChunkedFeedResult::Complete => return ChunkedTransferResult::Complete,
            ChunkedFeedResult::SizeLimitExceeded => return ChunkedTransferResult::SizeLimitExceeded,
            ChunkedFeedResult::Continue => {}
        }
    }
    
    loop {
        let buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, reader.read_buf(buf)).await;
        
        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => return ChunkedTransferResult::Failed,
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                break;
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                return ChunkedTransferResult::Failed;
            }
        };
        
        // SafeReadBuffer の有効長を設定
        returned_buf.set_valid_len(n);
        
        // ステートマシンにデータをフィード（型安全なアクセス）
        let feed_result = decoder.feed(returned_buf.as_valid_slice());
        
        // サイズ制限超過チェック
        if feed_result == ChunkedFeedResult::SizeLimitExceeded {
            buf_put(returned_buf);
            return ChunkedTransferResult::SizeLimitExceeded;
        }
        
        // バックエンドに転送（有効データのみを含むVecに変換）
        let write_buf = returned_buf.into_truncated();
        
        let write_result = timeout(WRITE_TIMEOUT, writer.write_buf(write_buf)).await;
        match write_result {
            Ok((Ok(_), returned)) => {
                buf_put_vec(returned);
            }
            Ok((Err(_), returned)) => {
                buf_put_vec(returned);
                return ChunkedTransferResult::Failed;
            }
            Err(_) => return ChunkedTransferResult::Failed,
        }
        
        // 終端チェック
        if feed_result == ChunkedFeedResult::Complete {
            return ChunkedTransferResult::Complete;
        }
    }
    
    ChunkedTransferResult::Failed
}

// ====================
// SendFile処理
// ====================
//
// kTLS + sendfile によるゼロコピー送信をサポートします。
//
// ## 通常の送信フロー（kTLS無効時）
//
// ファイル → ユーザー空間バッファ → TLS暗号化 → ネットワーク
// （2回のコピーが発生）
//
// ## ゼロコピー送信フロー（kTLS有効時）
//
// ファイル → カーネル空間でTLS暗号化 → NIC
// （ユーザー空間へのコピーなし）
//
// ### パフォーマンス効果
//
// - コンテキストスイッチの削減
// - メモリアクセスの削減（L3キャッシュミスの減少）
// - CPU使用率の低下（特に大きなファイル送信時）
//
// ### セキュリティ
//
// - ファイルの内容は変更されず、そのまま送信される
// - TLS暗号化はカーネル内で行われるため安全
// ====================

async fn handle_sendfile(
    mut tls_stream: ServerTls,
    base_path: &Path,
    is_dir: bool,
    index_filename: Option<&str>,
    req_path: &[u8],
    prefix: &[u8],
    client_wants_close: bool,
    security: &SecurityConfig,
) -> Option<(ServerTls, u16, u64, bool)> {
    // --- パス解決ロジック（Nginx風） ---
    // 
    // 1. ファイル指定（is_dir=false）: 完全一致のみ
    //    例: prefix="/robots.txt", path="./www/robots.txt"
    //    - リクエスト "/robots.txt" → OK（ファイルを返す）
    //    - リクエスト "/robots.txt/extra" → 404（ファイルの下には入れない）
    //
    // 2. ディレクトリ指定（is_dir=true）: プレフィックス除去後のパスを結合
    //    例: prefix="/static/", path="./www/assets/"
    //    - リクエスト "/static/css/style.css" → "./www/assets/css/style.css"
    //    - リクエスト "/static/" → "./www/assets/{index_filename}" (デフォルト: index.html)
    
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
    
    // プレフィックスを除去して「残りパス」を取得
    let remainder = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
        &path_str[prefix_str.len()..]
    } else {
        path_str
    };
    
    // パストラバーサル防止（簡易チェック）
    if remainder.contains("..") {
        let err_buf = ERR_MSG_FORBIDDEN.to_vec();
        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
        return Some((tls_stream, 403, 0, true));
    }
    
    let full_path = if is_dir {
        // ケースA: ディレクトリへのルーティング（Alias動作）
        // config: path = "./www/static/"
        // req: /static/css/style.css → remainder: css/style.css
        // result: ./www/static/css/style.css
        let sub_path = remainder.trim_start_matches('/');
        let mut p = base_path.to_path_buf();
        if !sub_path.is_empty() {
            p.push(sub_path);
        }
        p
    } else {
        // ケースB: ファイルへの直接ルーティング（完全一致）
        // config: path = "./www/robots.txt"
        // req: /robots.txt → remainder: "" (OK)
        // req: /robots.txt/extra → remainder: "/extra" (NG → 404)
        
        let clean_remainder = remainder.trim_matches('/');
        if !clean_remainder.is_empty() {
            // ファイル指定なのにさらにパスが続いている場合は404
            let err_buf = ERR_MSG_NOT_FOUND.to_vec();
            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
            return Some((tls_stream, 404, 0, true));
        }
        base_path.to_path_buf()
    };

    let full_path_canonical = match full_path.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            let err_buf = ERR_MSG_NOT_FOUND.to_vec();
            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
            return Some((tls_stream, 404, 0, true));  // エラー時は接続を閉じる
        }
    };

    if is_dir {
        if let Ok(base) = base_path.canonicalize() {
            if !full_path_canonical.starts_with(&base) {
                let err_buf = ERR_MSG_FORBIDDEN.to_vec();
                let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                return Some((tls_stream, 403, 0, true));  // エラー時は接続を閉じる
            }
        }
    }

    // ディレクトリの場合はインデックスファイルを試す
    // 設定されたファイル名、なければデフォルトの "index.html" を使用
    let final_path = if full_path_canonical.is_dir() {
        let filename = index_filename.unwrap_or("index.html");
        let index_path = full_path_canonical.join(filename);
        if index_path.exists() {
            index_path
        } else {
            // インデックスファイルが存在しない場合は403 Forbidden（ディレクトリリスティング禁止）
            let err_buf = ERR_MSG_FORBIDDEN.to_vec();
            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
            return Some((tls_stream, 403, 0, true));
        }
    } else {
        full_path_canonical
    };

    let file = match OpenOptions::new().read(true).open(&final_path).await {
        Ok(f) => f,
        Err(_) => {
            let err_buf = ERR_MSG_NOT_FOUND.to_vec();
            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
            return Some((tls_stream, 404, 0, true));  // エラー時は接続を閉じる
        }
    };

    let metadata = match file.metadata().await {
        Ok(m) => m,
        Err(_) => {
            let err_buf = ERR_MSG_NOT_FOUND.to_vec();
            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
            return Some((tls_stream, 404, 0, true));  // エラー時は接続を閉じる
        }
    };

    let file_size = metadata.len();
    let mime_type = mime_guess::from_path(&final_path).first_or_octet_stream();
    
    // ヘッダー構築（Keep-Alive対応 + カスタムレスポンスヘッダー）
    let mut header_buf = Vec::with_capacity(HEADER_BUF_CAPACITY);
    header_buf.extend_from_slice(HTTP_200_PREFIX);
    header_buf.extend_from_slice(mime_type.as_ref().as_bytes());
    header_buf.extend_from_slice(CONTENT_LENGTH_HEADER);
    
    let mut num_buf = itoa::Buffer::new();
    header_buf.extend_from_slice(num_buf.format(file_size).as_bytes());
    header_buf.extend_from_slice(b"\r\n");
    
    // 追加レスポンスヘッダー（セキュリティヘッダーなど）
    for (header_name, header_value) in &security.add_response_headers {
        header_buf.extend_from_slice(header_name.as_bytes());
        header_buf.extend_from_slice(b": ");
        header_buf.extend_from_slice(header_value.as_bytes());
        header_buf.extend_from_slice(b"\r\n");
    }
    
    if client_wants_close {
        header_buf.extend_from_slice(b"Connection: close\r\n\r\n");
    } else {
        header_buf.extend_from_slice(b"Connection: keep-alive\r\n\r\n");
    }

    // ヘッダー送信（タイムアウト付き）
    let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(header_buf)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return None;
    }

    // ファイル転送
    // kTLS が有効な場合は sendfile によるゼロコピー送信を使用
    #[cfg(feature = "ktls")]
    {
        if tls_stream.is_ktls_send_enabled() {
            return handle_sendfile_zerocopy(tls_stream, &file, file_size, client_wants_close).await;
        }
    }
    
    // kTLS が無効な場合は従来の read/write を使用
    handle_sendfile_userspace(tls_stream, &file, file_size, client_wants_close).await
}

/// kTLS + sendfile によるゼロコピーファイル送信
///
/// kTLS が有効な場合に使用されます。
/// ファイルの内容をカーネル空間で直接 TLS 暗号化して送信します。
#[cfg(feature = "ktls")]
async fn handle_sendfile_zerocopy(
    tls_stream: ServerTls,
    file: &monoio::fs::File,
    file_size: u64,
    client_wants_close: bool,
) -> Option<(ServerTls, u16, u64, bool)> {
    use std::os::unix::io::AsRawFd;
    
    let file_fd = file.as_raw_fd();
    let mut offset: i64 = 0;
    let mut total_sent = 0u64;
    
    // sendfile を使用してファイルをゼロコピー送信
    // sendfile はブロッキング呼び出しのため、大きなファイルはチャンク分割して送信
    const SENDFILE_CHUNK_SIZE: usize = 1024 * 1024; // 1MB チャンク
    
    while (offset as u64) < file_size {
        let remaining = file_size - (offset as u64);
        let chunk_size = (remaining as usize).min(SENDFILE_CHUNK_SIZE);
        
        // sendfile 実行
        match tls_stream.sendfile(file_fd, &mut offset, chunk_size) {
            Ok(0) => {
                // EOF
                break;
            }
            Ok(n) => {
                total_sent += n as u64;
            }
            Err(e) => {
                // EAGAIN/EWOULDBLOCK の場合は再試行（非同期ソケットの場合）
                if e.kind() == io::ErrorKind::WouldBlock {
                    // writable を待ってから再試行
                    if let Err(_) = tls_stream.get_ref().writable(false).await {
                        break;
                    }
                    continue;
                }
                error!("sendfile error: {}", e);
                break;
            }
        }
    }

    Some((tls_stream, 200, total_sent, client_wants_close))
}

/// 従来の read/write によるファイル送信（ユーザー空間経由）
///
/// kTLS が無効な場合、または rustls 使用時に使用されます。
async fn handle_sendfile_userspace(
    mut tls_stream: ServerTls,
    file: &monoio::fs::File,
    file_size: u64,
    client_wants_close: bool,
) -> Option<(ServerTls, u16, u64, bool)> {
    let mut total_sent = 0u64;
    let mut offset = 0u64;
    
    while offset < file_size {
        let read_buf = buf_get();
        let (res, mut returned_buf) = file.read_at(read_buf, offset).await;
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                break;
            }
            Ok(n) => n,
            Err(e) => {
                buf_put(returned_buf);
                error!("File read error: {}", e);
                break;
            }
        };
        
        // SafeReadBuffer の有効長を設定して書き込み用Vecに変換
        returned_buf.set_valid_len(n);
        let write_buf = returned_buf.into_truncated();
        
        let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(write_buf)).await;
        match write_result {
            Ok((Ok(_), returned)) => {
                buf_put_vec(returned);
                total_sent += n as u64;
                offset += n as u64;
            }
            Ok((Err(_), returned)) => {
                buf_put_vec(returned);
                break;
            }
            Err(_) => break,
        }
    }

    Some((tls_stream, 200, total_sent, client_wants_close))
}

// ====================
// ロギング
// ====================

/// アクセスログを記録 + Prometheusメトリクスを記録
/// 
/// - 処理時間: `start_instant` からの経過時間を高精度で計測（Instant使用）
/// - タイムスタンプ: Coarse Timer でキャッシュした時刻を使用（システムコール削減）
/// - メトリクス: リクエスト数、処理時間、サイズをPrometheus形式で記録
fn log_access(
    method: &[u8],
    host: &[u8],
    path: &[u8],
    ua: &[u8],
    req_body_size: u64,
    status: u16,
    resp_body_size: u64,
    start_instant: Instant,
) {
    // 処理時間は Instant で高精度計測
    let duration = start_instant.elapsed();
    let duration_ms = duration.as_millis();
    let duration_secs = duration.as_secs_f64();
    
    // タイムスタンプは Coarse Timer を使用（システムコール削減）
    let log_time = coarse_now();
    let path_str = std::str::from_utf8(path).unwrap_or("-");
    let ua_str = std::str::from_utf8(ua).unwrap_or("-");
    let method_str = std::str::from_utf8(method).unwrap_or("GET");
    let host_str = std::str::from_utf8(host).unwrap_or("-");
    
    // アクセスログ出力
    info!("Access: time={} duration={}ms method={} host={} path={} ua={} req_body_size={} status={} resp_body_size={}",
        log_time, duration_ms, method_str, host_str, path_str, ua_str, req_body_size, status, resp_body_size);
    
    // Prometheusメトリクスを記録
    record_request_metrics(method_str, host_str, status, req_body_size, resp_body_size, duration_secs);
}
