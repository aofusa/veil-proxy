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

use once_cell::sync::Lazy;
use prometheus::{
    CounterVec, Histogram, HistogramOpts, HistogramVec, IntGaugeVec,
    Opts, Registry, TextEncoder, Encoder,
};
use std::sync::atomic::Ordering;

use crate::cache;

/// Prometheusメトリクスレジストリ（グローバル）
pub(crate) static METRICS_REGISTRY: Lazy<Registry> = Lazy::new(|| {
    Registry::new()
});

/// HTTPリクエスト総数カウンター（method, status, host ラベル付き）
pub(crate) static HTTP_REQUESTS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    let opts = Opts::new("http_requests_total", "Total number of HTTP requests")
        .namespace("veil_proxy");
    let counter = CounterVec::new(opts, &["method", "status", "host"]).unwrap();
    METRICS_REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

/// HTTPリクエスト処理時間ヒストグラム（method, host ラベル付き）
pub(crate) static HTTP_REQUEST_DURATION_SECONDS: Lazy<HistogramVec> = Lazy::new(|| {
    let opts = HistogramOpts::new("http_request_duration_seconds", "HTTP request duration in seconds")
        .namespace("veil_proxy")
        .buckets(vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]);
    let histogram = HistogramVec::new(opts, &["method", "host"]).unwrap();
    METRICS_REGISTRY.register(Box::new(histogram.clone())).unwrap();
    histogram
});

/// HTTPリクエストボディサイズヒストグラム
pub(crate) static HTTP_REQUEST_SIZE_BYTES: Lazy<Histogram> = Lazy::new(|| {
    let opts = HistogramOpts::new("http_request_size_bytes", "HTTP request body size in bytes")
        .namespace("veil_proxy")
        .buckets(vec![100.0, 1000.0, 10000.0, 100000.0, 1000000.0, 10000000.0]);
    let histogram = Histogram::with_opts(opts).unwrap();
    METRICS_REGISTRY.register(Box::new(histogram.clone())).unwrap();
    histogram
});

/// HTTPレスポンスボディサイズヒストグラム
pub(crate) static HTTP_RESPONSE_SIZE_BYTES: Lazy<Histogram> = Lazy::new(|| {
    let opts = HistogramOpts::new("http_response_size_bytes", "HTTP response body size in bytes")
        .namespace("veil_proxy")
        .buckets(vec![100.0, 1000.0, 10000.0, 100000.0, 1000000.0, 10000000.0, 100000000.0]);
    let histogram = Histogram::with_opts(opts).unwrap();
    METRICS_REGISTRY.register(Box::new(histogram.clone())).unwrap();
    histogram
});

/// アクティブ接続数ゲージ（ホスト別）
pub(crate) static HTTP_ACTIVE_CONNECTIONS: Lazy<IntGaugeVec> = Lazy::new(|| {
    let opts = Opts::new("http_active_connections", "Number of active HTTP connections")
        .namespace("veil_proxy");
    let gauge = IntGaugeVec::new(opts, &["host"]).unwrap();
    METRICS_REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

/// アクティブ接続メトリクスの自動管理（Dropトレイトで自動デクリメント）
pub(crate) struct ActiveConnectionMetric {
    host_name: Option<String>,
    enabled: bool,
}

impl ActiveConnectionMetric {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            host_name: None,
            enabled,
        }
    }

    pub(crate) fn set_host(&mut self, host: String) {
        if self.enabled && self.host_name.is_none() {
            self.host_name = Some(host.clone());
            HTTP_ACTIVE_CONNECTIONS.with_label_values(&[&host]).inc();
        }
    }
}

impl Drop for ActiveConnectionMetric {
    fn drop(&mut self) {
        if self.enabled {
            if let Some(ref host) = self.host_name {
                HTTP_ACTIVE_CONNECTIONS.with_label_values(&[host]).dec();
            }
        }
    }
}

/// 接続カウンターの自動管理（Dropトレイトで自動デクリメント）
///
/// パニック発生時もDropが呼ばれるため、接続カウンターの整合性が保証されます。
/// これにより、`max_concurrent_connections`制限が正しく機能し続けます。
pub(crate) struct ConnectionGuard;

impl ConnectionGuard {
    /// 新しいガードを作成し、接続カウンターをインクリメント
    pub(crate) fn new() -> Self {
        crate::CURRENT_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        crate::CURRENT_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// アップストリーム健康状態ゲージ（upstream, server ラベル付き）
/// 1 = healthy, 0 = unhealthy
pub(crate) static HTTP_UPSTREAM_HEALTH: Lazy<IntGaugeVec> = Lazy::new(|| {
    let opts = Opts::new("http_upstream_health", "Upstream server health status (1=healthy, 0=unhealthy)")
        .namespace("veil_proxy");
    let gauge = IntGaugeVec::new(opts, &["upstream", "server"]).unwrap();
    METRICS_REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

// ====================
// キャッシュメトリクス
// ====================

/// キャッシュヒット数カウンター（host ラベル付き）
pub(crate) static CACHE_HITS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    let opts = Opts::new("cache_hits_total", "Total number of cache hits")
        .namespace("veil_proxy");
    let counter = CounterVec::new(opts, &["host"]).unwrap();
    METRICS_REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

/// キャッシュミス数カウンター（host ラベル付き）
pub(crate) static CACHE_MISSES_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    let opts = Opts::new("cache_misses_total", "Total number of cache misses")
        .namespace("veil_proxy");
    let counter = CounterVec::new(opts, &["host"]).unwrap();
    METRICS_REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

/// キャッシュ保存数カウンター（host, storage ラベル付き）
/// storage: "memory" or "disk"
pub(crate) static CACHE_STORES_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    let opts = Opts::new("cache_stores_total", "Total number of cache stores")
        .namespace("veil_proxy");
    let counter = CounterVec::new(opts, &["host", "storage"]).unwrap();
    METRICS_REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

/// キャッシュ削除数カウンター（reason ラベル付き）
/// reason: "expired", "lru", "invalidate"
pub(crate) static CACHE_EVICTIONS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    let opts = Opts::new("cache_evictions_total", "Total number of cache evictions")
        .namespace("veil_proxy");
    let counter = CounterVec::new(opts, &["reason"]).unwrap();
    METRICS_REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

/// キャッシュサイズゲージ（storage ラベル付き）
/// storage: "memory" or "disk"
pub(crate) static CACHE_SIZE_BYTES: Lazy<IntGaugeVec> = Lazy::new(|| {
    let opts = Opts::new("cache_size_bytes", "Current cache size in bytes")
        .namespace("veil_proxy");
    let gauge = IntGaugeVec::new(opts, &["storage"]).unwrap();
    METRICS_REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

/// キャッシュエントリ数ゲージ
pub(crate) static CACHE_ENTRIES: Lazy<IntGaugeVec> = Lazy::new(|| {
    let opts = Opts::new("cache_entries", "Current number of cache entries")
        .namespace("veil_proxy");
    let gauge = IntGaugeVec::new(opts, &["storage"]).unwrap();
    METRICS_REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

/// バッファリング使用数カウンター（mode ラベル付き）
/// バッファリングが使用された回数（ホストごと）
pub(crate) static BUFFERING_USED_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    let opts = Opts::new("buffering_used_total", "Total number of requests using buffering")
        .namespace("veil_proxy");
    let counter = CounterVec::new(opts, &["host"]).unwrap();
    METRICS_REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

/// メトリクス: キャッシュヒットを記録
#[inline]
pub(crate) fn record_cache_hit(host: &str) {
    CACHE_HITS_TOTAL.with_label_values(&[host]).inc();
}

/// メトリクス: キャッシュミスを記録
#[inline]
pub(crate) fn record_cache_miss(host: &str) {
    CACHE_MISSES_TOTAL.with_label_values(&[host]).inc();
}

/// メトリクス: キャッシュ保存を記録
#[inline]
pub(crate) fn record_cache_store(host: &str, storage: &str) {
    CACHE_STORES_TOTAL.with_label_values(&[host, storage]).inc();
}

/// メトリクス: キャッシュ削除を記録
#[inline]
pub(crate) fn record_cache_eviction(reason: &str, count: usize) {
    CACHE_EVICTIONS_TOTAL.with_label_values(&[reason]).inc_by(count as f64);
}

/// メトリクス: キャッシュサイズを更新
#[inline]
pub(crate) fn update_cache_size_metrics(stats: &cache::CacheStats) {
    CACHE_SIZE_BYTES.with_label_values(&["memory"]).set(stats.memory_usage as i64);
    CACHE_SIZE_BYTES.with_label_values(&["disk"]).set(stats.disk_usage as i64);
    CACHE_ENTRIES.with_label_values(&["memory"]).set(stats.entries as i64);
    CACHE_ENTRIES.with_label_values(&["disk"]).set(0); // ディスクエントリ数は別途追跡が必要
}

/// メトリクス: バッファリング使用を記録
/// バッファリングモード使用を記録
#[inline]
pub(crate) fn record_buffering_used(host: &str) {
    BUFFERING_USED_TOTAL.with_label_values(&[host]).inc();
}

// ====================
// キャッシュ保存コンテキスト
// ====================

/// キャッシュ保存コンテキスト
///
/// プロキシ処理中にレスポンスをキャプチャしてキャッシュに保存するために使用します。
/// splice転送では使用できないため、このコンテキストが存在する場合は通常転送を使用します。
pub struct CacheSaveContext {
    /// キャッシュキー
    pub key: cache::CacheKey,
    /// ホスト名（メトリクス用）
    pub host: String,
    /// キャプチャしたレスポンスヘッダー
    pub captured_headers: Vec<(Box<[u8]>, Box<[u8]>)>,
    /// キャプチャしたレスポンスボディ
    pub captured_body: Vec<u8>,
    /// ステータスコード
    pub status_code: u16,
    /// キャプチャサイズ上限（これを超えるとキャプチャを中止）
    pub max_capture_size: usize,
    /// キャプチャ中止フラグ
    pub capture_aborted: bool,
    /// レスポンスのVaryヘッダーで指定されたヘッダー名のリスト
    pub vary_headers: Option<Vec<String>>,
}

impl CacheSaveContext {
    /// 新しいキャッシュ保存コンテキストを作成
    pub fn new(key: cache::CacheKey, host: String, max_capture_size: usize) -> Self {
        Self {
            key,
            host,
            captured_headers: Vec::new(),
            captured_body: Vec::with_capacity(4096),
            status_code: 0,
            max_capture_size,
            capture_aborted: false,
            vary_headers: None,
        }
    }

    /// ヘッダーを設定
    #[inline]
    pub fn set_headers(&mut self, headers: Vec<(Box<[u8]>, Box<[u8]>)>, status_code: u16) {
        // Varyヘッダーを抽出
        self.vary_headers = cache::CachePolicy::parse_vary(&headers);
        self.captured_headers = headers;
        self.status_code = status_code;
    }

    /// ボディチャンクを追加（サイズ制限付き）
    #[inline]
    pub fn append_body(&mut self, data: &[u8]) {
        if self.capture_aborted {
            return;
        }

        let new_size = self.captured_body.len() + data.len();
        if new_size > self.max_capture_size {
            // サイズ上限を超えた場合、キャプチャを中止
            self.capture_aborted = true;
            self.captured_body.clear();
            self.captured_headers.clear();
            return;
        }

        self.captured_body.extend_from_slice(data);
    }

    /// キャッシュに保存（キャプチャ成功時のみ）
    pub fn save_to_cache(&self) -> bool {
        if self.capture_aborted || self.captured_body.is_empty() {
            return false;
        }

        if let Some(cache_manager) = cache::get_global_cache() {
            let stored = cache_manager.store_with_vary(
                self.key.clone(),
                self.status_code,
                self.captured_headers.clone(),
                self.captured_body.clone(),
                self.vary_headers.clone(),
            );

            if stored {
                record_cache_store(&self.host, "memory");
                ftlog::debug!("Cached response for {} (status={}, size={}, vary={:?})",
                       self.host, self.status_code, self.captured_body.len(), self.vary_headers);
            }

            stored
        } else {
            false
        }
    }
}

/// Prometheusメトリクスをテキストフォーマットでエンコード
pub(crate) fn encode_prometheus_metrics() -> Vec<u8> {
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
pub(crate) fn record_request_metrics(
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
pub(crate) fn build_metrics_response() -> Vec<u8> {
    let body = encode_prometheus_metrics();
    let mut response = Vec::with_capacity(256 + body.len());
    response.extend_from_slice(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: ");

    let mut num_buf = itoa::Buffer::new();
    response.extend_from_slice(num_buf.format(body.len()).as_bytes());
    response.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
    response.extend_from_slice(&body);
    response
}
