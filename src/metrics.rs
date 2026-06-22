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
// metrics feature が無効の場合、全公開 API はノーオップスタブとして提供されます。
//
// ====================

use std::sync::atomic::{AtomicBool, Ordering};

// ====================
// ランタイム有効/無効トグル（F-09）
// ====================
//
// compile-time の `metrics` feature とは独立した実行時スイッチ。
// `[metrics] enabled = false` の場合、エンドポイントは 404 を返し、
// record_* 関数はノーオップになる。
static METRICS_RUNTIME_ENABLED: AtomicBool = AtomicBool::new(true);

/// メトリクスのランタイム有効状態を設定する（設定読み込み時に呼ぶ）
#[inline]
pub fn set_metrics_runtime_enabled(enabled: bool) {
    METRICS_RUNTIME_ENABLED.store(enabled, Ordering::Relaxed);
}

/// メトリクスがランタイムで有効かどうか
#[inline]
pub fn metrics_runtime_enabled() -> bool {
    METRICS_RUNTIME_ENABLED.load(Ordering::Relaxed)
}

// ====================
// Prometheus 実装（metrics feature 有効時）
// ====================
#[cfg(feature = "metrics")]
use once_cell::sync::Lazy;

#[cfg(feature = "metrics")]
use prometheus::{
    CounterVec, Encoder, Histogram, HistogramOpts, HistogramVec, IntGaugeVec, Opts, Registry,
    TextEncoder,
};

#[cfg(feature = "metrics")]
/// Prometheusメトリクスレジストリ（グローバル）
pub(crate) static METRICS_REGISTRY: Lazy<Registry> = Lazy::new(|| Registry::new());

#[cfg(feature = "metrics")]
/// HTTPリクエスト総数カウンター（method, status, host ラベル付き）
pub(crate) static HTTP_REQUESTS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    let opts =
        Opts::new("http_requests_total", "Total number of HTTP requests").namespace("veil_proxy");
    let counter = CounterVec::new(opts, &["method", "status", "host"]).unwrap();
    METRICS_REGISTRY
        .register(Box::new(counter.clone()))
        .unwrap();
    counter
});

#[cfg(feature = "metrics")]
/// HTTPリクエスト処理時間ヒストグラム（method, host ラベル付き）
pub(crate) static HTTP_REQUEST_DURATION_SECONDS: Lazy<HistogramVec> = Lazy::new(|| {
    let opts = HistogramOpts::new(
        "http_request_duration_seconds",
        "HTTP request duration in seconds",
    )
    .namespace("veil_proxy")
    .buckets(vec![
        0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
    ]);
    let histogram = HistogramVec::new(opts, &["method", "host"]).unwrap();
    METRICS_REGISTRY
        .register(Box::new(histogram.clone()))
        .unwrap();
    histogram
});

#[cfg(feature = "metrics")]
/// HTTPリクエストボディサイズヒストグラム
pub(crate) static HTTP_REQUEST_SIZE_BYTES: Lazy<Histogram> = Lazy::new(|| {
    let opts = HistogramOpts::new("http_request_size_bytes", "HTTP request body size in bytes")
        .namespace("veil_proxy")
        .buckets(vec![
            100.0, 1000.0, 10000.0, 100000.0, 1000000.0, 10000000.0,
        ]);
    let histogram = Histogram::with_opts(opts).unwrap();
    METRICS_REGISTRY
        .register(Box::new(histogram.clone()))
        .unwrap();
    histogram
});

#[cfg(feature = "metrics")]
/// HTTPレスポンスボディサイズヒストグラム
pub(crate) static HTTP_RESPONSE_SIZE_BYTES: Lazy<Histogram> = Lazy::new(|| {
    let opts = HistogramOpts::new(
        "http_response_size_bytes",
        "HTTP response body size in bytes",
    )
    .namespace("veil_proxy")
    .buckets(vec![
        100.0,
        1000.0,
        10000.0,
        100000.0,
        1000000.0,
        10000000.0,
        100000000.0,
    ]);
    let histogram = Histogram::with_opts(opts).unwrap();
    METRICS_REGISTRY
        .register(Box::new(histogram.clone()))
        .unwrap();
    histogram
});

#[cfg(feature = "metrics")]
/// アクティブ接続数ゲージ（ホスト別）
pub(crate) static HTTP_ACTIVE_CONNECTIONS: Lazy<IntGaugeVec> = Lazy::new(|| {
    let opts = Opts::new(
        "http_active_connections",
        "Number of active HTTP connections",
    )
    .namespace("veil_proxy");
    let gauge = IntGaugeVec::new(opts, &["host"]).unwrap();
    METRICS_REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

/// アクティブ接続メトリクスの自動管理（Dropトレイトで自動デクリメント）
pub(crate) struct ActiveConnectionMetric {
    #[cfg(feature = "metrics")]
    host_name: Option<String>,
    #[cfg(feature = "metrics")]
    enabled: bool,
}

impl ActiveConnectionMetric {
    pub(crate) fn new(_enabled: bool) -> Self {
        Self {
            #[cfg(feature = "metrics")]
            host_name: None,
            #[cfg(feature = "metrics")]
            enabled: _enabled,
        }
    }

    pub(crate) fn set_host(&mut self, _host: String) {
        #[cfg(feature = "metrics")]
        if self.enabled && self.host_name.is_none() {
            self.host_name = Some(_host.clone());
            HTTP_ACTIVE_CONNECTIONS.with_label_values(&[&_host]).inc();
        }
    }
}

impl Drop for ActiveConnectionMetric {
    fn drop(&mut self) {
        #[cfg(feature = "metrics")]
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

#[cfg(feature = "metrics")]
/// アップストリーム健康状態ゲージ（upstream, server ラベル付き）
/// 1 = healthy, 0 = unhealthy
pub(crate) static HTTP_UPSTREAM_HEALTH: Lazy<IntGaugeVec> = Lazy::new(|| {
    let opts = Opts::new(
        "http_upstream_health",
        "Upstream server health status (1=healthy, 0=unhealthy)",
    )
    .namespace("veil_proxy");
    let gauge = IntGaugeVec::new(opts, &["upstream", "server"]).unwrap();
    METRICS_REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

// ====================
// キャッシュメトリクス（metrics feature 有効時）
// ====================

#[cfg(feature = "metrics")]
/// キャッシュヒット数カウンター（host ラベル付き）
pub(crate) static CACHE_HITS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    let opts = Opts::new("cache_hits_total", "Total number of cache hits").namespace("veil_proxy");
    let counter = CounterVec::new(opts, &["host"]).unwrap();
    METRICS_REGISTRY
        .register(Box::new(counter.clone()))
        .unwrap();
    counter
});

#[cfg(feature = "metrics")]
/// キャッシュミス数カウンター（host ラベル付き）
pub(crate) static CACHE_MISSES_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    let opts =
        Opts::new("cache_misses_total", "Total number of cache misses").namespace("veil_proxy");
    let counter = CounterVec::new(opts, &["host"]).unwrap();
    METRICS_REGISTRY
        .register(Box::new(counter.clone()))
        .unwrap();
    counter
});

#[cfg(feature = "metrics")]
/// キャッシュ保存数カウンター（host, storage ラベル付き）
/// storage: "memory" or "disk"
pub(crate) static CACHE_STORES_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    let opts =
        Opts::new("cache_stores_total", "Total number of cache stores").namespace("veil_proxy");
    let counter = CounterVec::new(opts, &["host", "storage"]).unwrap();
    METRICS_REGISTRY
        .register(Box::new(counter.clone()))
        .unwrap();
    counter
});

#[cfg(feature = "metrics")]
/// キャッシュ削除数カウンター（reason ラベル付き）
/// reason: "expired", "lru", "invalidate"
pub(crate) static CACHE_EVICTIONS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    let opts = Opts::new("cache_evictions_total", "Total number of cache evictions")
        .namespace("veil_proxy");
    let counter = CounterVec::new(opts, &["reason"]).unwrap();
    METRICS_REGISTRY
        .register(Box::new(counter.clone()))
        .unwrap();
    counter
});

#[cfg(feature = "metrics")]
/// キャッシュサイズゲージ（storage ラベル付き）
/// storage: "memory" or "disk"
#[allow(dead_code)]
pub(crate) static CACHE_SIZE_BYTES: Lazy<IntGaugeVec> = Lazy::new(|| {
    let opts = Opts::new("cache_size_bytes", "Current cache size in bytes").namespace("veil_proxy");
    let gauge = IntGaugeVec::new(opts, &["storage"]).unwrap();
    METRICS_REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

#[cfg(feature = "metrics")]
/// キャッシュエントリ数ゲージ
#[allow(dead_code)]
pub(crate) static CACHE_ENTRIES: Lazy<IntGaugeVec> = Lazy::new(|| {
    let opts =
        Opts::new("cache_entries", "Current number of cache entries").namespace("veil_proxy");
    let gauge = IntGaugeVec::new(opts, &["storage"]).unwrap();
    METRICS_REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

#[cfg(feature = "metrics")]
/// バッファリング使用数カウンター（mode ラベル付き）
/// バッファリングが使用された回数（ホストごと）
pub(crate) static BUFFERING_USED_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    let opts = Opts::new(
        "buffering_used_total",
        "Total number of requests using buffering",
    )
    .namespace("veil_proxy");
    let counter = CounterVec::new(opts, &["host"]).unwrap();
    METRICS_REGISTRY
        .register(Box::new(counter.clone()))
        .unwrap();
    counter
});

/// メトリクス: アップストリームヘルス状態を更新 (1=healthy, 0=unhealthy)
#[inline]
pub(crate) fn update_upstream_health(_upstream: &str, _server: &str, _healthy: bool) {
    #[cfg(feature = "metrics")]
    HTTP_UPSTREAM_HEALTH
        .with_label_values(&[_upstream, _server])
        .set(if _healthy { 1 } else { 0 });
}

/// メトリクス: キャッシュヒットを記録
#[inline]
pub(crate) fn record_cache_hit(_host: &str) {
    #[cfg(feature = "metrics")]
    CACHE_HITS_TOTAL.with_label_values(&[_host]).inc();
}

/// メトリクス: キャッシュミスを記録
#[inline]
pub(crate) fn record_cache_miss(_host: &str) {
    #[cfg(feature = "metrics")]
    CACHE_MISSES_TOTAL.with_label_values(&[_host]).inc();
}

/// メトリクス: キャッシュ保存を記録
#[allow(dead_code)]
#[inline]
pub(crate) fn record_cache_store(_host: &str, _storage: &str) {
    #[cfg(feature = "metrics")]
    CACHE_STORES_TOTAL
        .with_label_values(&[_host, _storage])
        .inc();
}

/// メトリクス: キャッシュ削除を記録
#[inline]
pub(crate) fn record_cache_eviction(_reason: &str, _count: usize) {
    #[cfg(feature = "metrics")]
    CACHE_EVICTIONS_TOTAL
        .with_label_values(&[_reason])
        .inc_by(_count as f64);
}

/// メトリクス: キャッシュサイズを更新
#[inline]
pub(crate) fn update_cache_size_metrics(_stats: &crate::cache::CacheStats) {
    #[cfg(all(feature = "metrics", feature = "cache"))]
    {
        CACHE_SIZE_BYTES
            .with_label_values(&["memory"])
            .set(_stats.memory_usage as i64);
        CACHE_SIZE_BYTES
            .with_label_values(&["disk"])
            .set(_stats.disk_usage as i64);
        CACHE_ENTRIES
            .with_label_values(&["memory"])
            .set(_stats.entries as i64);
        CACHE_ENTRIES.with_label_values(&["disk"]).set(0);
    }
}

/// メトリクス: バッファリング使用を記録
#[inline]
pub(crate) fn record_buffering_used(_host: &str) {
    #[cfg(feature = "metrics")]
    BUFFERING_USED_TOTAL.with_label_values(&[_host]).inc();
}

// ====================
// F-09: 新規メトリクス（veil_ プレフィックス）
// ====================
// 既存メトリクスは後方互換のため veil_proxy_ プレフィックスを維持する。
// 新規メトリクスは veil_ プレフィックスへ標準化する。

// --- サーキットブレーカー・リトライ・異常検知（F-06 連携）---

#[cfg(feature = "metrics")]
/// サーキットブレーカーが Open になった累計回数（upstream ラベル）
pub(crate) static CIRCUIT_BREAKER_OPEN_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    let opts = Opts::new(
        "circuit_breaker_open_total",
        "Total times a circuit breaker opened",
    )
    .namespace("veil");
    let counter = CounterVec::new(opts, &["upstream"]).unwrap();
    METRICS_REGISTRY
        .register(Box::new(counter.clone()))
        .unwrap();
    counter
});

#[cfg(feature = "metrics")]
/// サーキットブレーカーの状態ゲージ（0=closed,1=open,2=half_open）
pub(crate) static CIRCUIT_BREAKER_STATE: Lazy<IntGaugeVec> = Lazy::new(|| {
    let opts = Opts::new(
        "circuit_breaker_state",
        "Circuit breaker state (0=closed,1=open,2=half_open)",
    )
    .namespace("veil");
    let gauge = IntGaugeVec::new(opts, &["upstream"]).unwrap();
    METRICS_REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

#[cfg(feature = "metrics")]
/// リトライ回数カウンター（upstream, result ラベル）
pub(crate) static RETRY_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    let opts = Opts::new("retry_total", "Total number of upstream retries").namespace("veil");
    let counter = CounterVec::new(opts, &["upstream", "result"]).unwrap();
    METRICS_REGISTRY
        .register(Box::new(counter.clone()))
        .unwrap();
    counter
});

#[cfg(feature = "metrics")]
/// 異常検知による排除状態ゲージ（upstream, server ラベル、1=排除中）
pub(crate) static OUTLIER_EJECTED: Lazy<IntGaugeVec> = Lazy::new(|| {
    let opts = Opts::new(
        "outlier_ejected",
        "Outlier-ejected upstream servers (1=ejected)",
    )
    .namespace("veil");
    let gauge = IntGaugeVec::new(opts, &["upstream", "server"]).unwrap();
    METRICS_REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

/// メトリクス: サーキットブレーカー Open を記録
#[inline]
pub fn record_circuit_breaker_open(_upstream: &str) {
    #[cfg(feature = "metrics")]
    if metrics_runtime_enabled() {
        CIRCUIT_BREAKER_OPEN_TOTAL
            .with_label_values(&[_upstream])
            .inc();
    }
}

/// メトリクス: サーキットブレーカー状態を更新
#[inline]
pub fn set_circuit_breaker_state(_upstream: &str, _state: u64) {
    #[cfg(feature = "metrics")]
    if metrics_runtime_enabled() {
        CIRCUIT_BREAKER_STATE
            .with_label_values(&[_upstream])
            .set(_state as i64);
    }
}

/// メトリクス: リトライを記録（result は "success" / "exhausted" 等）
#[inline]
pub fn record_retry(_upstream: &str, _result: &str) {
    #[cfg(feature = "metrics")]
    if metrics_runtime_enabled() {
        RETRY_TOTAL.with_label_values(&[_upstream, _result]).inc();
    }
}

/// メトリクス: 異常検知による排除状態を更新
#[inline]
pub fn set_outlier_ejected(_upstream: &str, _server: &str, _ejected: bool) {
    #[cfg(feature = "metrics")]
    if metrics_runtime_enabled() {
        OUTLIER_EJECTED
            .with_label_values(&[_upstream, _server])
            .set(if _ejected { 1 } else { 0 });
    }
}

// --- コネクションプール（F-09）---

#[cfg(feature = "metrics")]
/// コネクションプールのサイズゲージ（upstream ラベル）
pub(crate) static CONNECTION_POOL_SIZE: Lazy<IntGaugeVec> = Lazy::new(|| {
    let opts = Opts::new("connection_pool_size", "Current connection pool size").namespace("veil");
    let gauge = IntGaugeVec::new(opts, &["upstream"]).unwrap();
    METRICS_REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

#[cfg(feature = "metrics")]
/// コネクションプールのヒット数カウンター（upstream ラベル）
pub(crate) static CONNECTION_POOL_HITS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    let opts =
        Opts::new("connection_pool_hits_total", "Total connection pool hits").namespace("veil");
    let counter = CounterVec::new(opts, &["upstream"]).unwrap();
    METRICS_REGISTRY
        .register(Box::new(counter.clone()))
        .unwrap();
    counter
});

#[cfg(feature = "metrics")]
/// コネクションプールのミス数カウンター（upstream ラベル）
pub(crate) static CONNECTION_POOL_MISSES_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    let opts = Opts::new(
        "connection_pool_misses_total",
        "Total connection pool misses",
    )
    .namespace("veil");
    let counter = CounterVec::new(opts, &["upstream"]).unwrap();
    METRICS_REGISTRY
        .register(Box::new(counter.clone()))
        .unwrap();
    counter
});

/// メトリクス: コネクションプールサイズを更新
#[inline]
pub fn set_connection_pool_size(_upstream: &str, _size: usize) {
    #[cfg(feature = "metrics")]
    if metrics_runtime_enabled() {
        CONNECTION_POOL_SIZE
            .with_label_values(&[_upstream])
            .set(_size as i64);
    }
}

/// メトリクス: コネクションプールヒットを記録
#[inline]
pub fn record_connection_pool_hit(_upstream: &str) {
    #[cfg(feature = "metrics")]
    if metrics_runtime_enabled() {
        CONNECTION_POOL_HITS_TOTAL
            .with_label_values(&[_upstream])
            .inc();
    }
}

/// メトリクス: コネクションプールミスを記録
#[inline]
pub fn record_connection_pool_miss(_upstream: &str) {
    #[cfg(feature = "metrics")]
    if metrics_runtime_enabled() {
        CONNECTION_POOL_MISSES_TOTAL
            .with_label_values(&[_upstream])
            .inc();
    }
}

// --- gRPC（F-09、metrics + grpc 両方有効時）---

#[cfg(all(feature = "metrics", feature = "grpc"))]
/// gRPC リクエスト総数（method, status_code, upstream）
pub(crate) static GRPC_REQUESTS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    let opts = Opts::new("grpc_requests_total", "Total number of gRPC requests").namespace("veil");
    let counter = CounterVec::new(opts, &["method", "status_code", "upstream"]).unwrap();
    METRICS_REGISTRY
        .register(Box::new(counter.clone()))
        .unwrap();
    counter
});

#[cfg(all(feature = "metrics", feature = "grpc"))]
/// gRPC ストリーム継続時間ヒストグラム（method）
pub(crate) static GRPC_STREAM_DURATION_SECONDS: Lazy<HistogramVec> = Lazy::new(|| {
    let opts = HistogramOpts::new(
        "grpc_stream_duration_seconds",
        "gRPC stream duration in seconds",
    )
    .namespace("veil")
    .buckets(vec![
        0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0,
    ]);
    let histogram = HistogramVec::new(opts, &["method"]).unwrap();
    METRICS_REGISTRY
        .register(Box::new(histogram.clone()))
        .unwrap();
    histogram
});

/// メトリクス: gRPC リクエストを記録
#[inline]
pub fn record_grpc_request(_method: &str, _status_code: &str, _upstream: &str) {
    #[cfg(all(feature = "metrics", feature = "grpc"))]
    if metrics_runtime_enabled() {
        GRPC_REQUESTS_TOTAL
            .with_label_values(&[_method, _status_code, _upstream])
            .inc();
    }
}

/// メトリクス: gRPC ストリーム継続時間を記録
#[inline]
pub fn observe_grpc_stream_duration(_method: &str, _duration_secs: f64) {
    #[cfg(all(feature = "metrics", feature = "grpc"))]
    if metrics_runtime_enabled() {
        GRPC_STREAM_DURATION_SECONDS
            .with_label_values(&[_method])
            .observe(_duration_secs);
    }
}

// --- WASM（F-09、metrics + wasm 両方有効時）---

#[cfg(all(feature = "metrics", feature = "wasm"))]
/// WASM フィルタ実行時間ヒストグラム（filter, phase）
pub(crate) static WASM_FILTER_DURATION_SECONDS: Lazy<HistogramVec> = Lazy::new(|| {
    let opts = HistogramOpts::new(
        "wasm_filter_duration_seconds",
        "WASM filter execution time in seconds",
    )
    .namespace("veil")
    .buckets(vec![
        0.00001, 0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5,
    ]);
    let histogram = HistogramVec::new(opts, &["filter", "phase"]).unwrap();
    METRICS_REGISTRY
        .register(Box::new(histogram.clone()))
        .unwrap();
    histogram
});

/// メトリクス: WASM フィルタ実行時間を記録
#[inline]
pub fn observe_wasm_filter_duration(_filter: &str, _phase: &str, _duration_secs: f64) {
    #[cfg(all(feature = "metrics", feature = "wasm"))]
    if metrics_runtime_enabled() {
        WASM_FILTER_DURATION_SECONDS
            .with_label_values(&[_filter, _phase])
            .observe(_duration_secs);
    }
}

// ====================
// キャッシュ保存コンテキスト
// ====================

/// キャッシュ保存コンテキスト
///
/// プロキシ処理中にレスポンスをキャプチャしてキャッシュに保存するために使用します。
/// splice転送では使用できないため、このコンテキストが存在する場合は通常転送を使用します。
/// cache feature が無効の場合はノーオップスタブとして機能します。
pub struct CacheSaveContext {
    /// キャッシュキー
    #[cfg(feature = "cache")]
    pub key: crate::cache::CacheKey,
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
    #[cfg(feature = "cache")]
    pub fn new(key: crate::cache::CacheKey, host: String, max_capture_size: usize) -> Self {
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

    /// cache feature 無効時の new（引数は無視、常にノーオップ）
    #[cfg(not(feature = "cache"))]
    pub fn new(_key: crate::cache::CacheKey, host: String, _max_capture_size: usize) -> Self {
        Self {
            host,
            captured_headers: Vec::new(),
            captured_body: Vec::new(),
            status_code: 0,
            max_capture_size: 0,
            capture_aborted: true,
            vary_headers: None,
        }
    }

    /// ヘッダーを設定
    #[inline]
    pub fn set_headers(&mut self, headers: Vec<(Box<[u8]>, Box<[u8]>)>, status_code: u16) {
        #[cfg(feature = "cache")]
        {
            // Varyヘッダーを抽出
            self.vary_headers = crate::cache::CachePolicy::parse_vary(&headers);
        }
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
            self.capture_aborted = true;
            self.captured_body.clear();
            self.captured_headers.clear();
            return;
        }

        self.captured_body.extend_from_slice(data);
    }

    /// キャッシュに保存（キャプチャ成功時のみ）
    pub fn save_to_cache(&self) -> bool {
        #[cfg(feature = "cache")]
        {
            if self.capture_aborted || self.captured_body.is_empty() {
                return false;
            }

            if let Some(cache_manager) = crate::cache::get_global_cache() {
                let stored = cache_manager.store_with_vary(
                    self.key.clone(),
                    self.status_code,
                    self.captured_headers.clone(),
                    self.captured_body.clone(),
                    self.vary_headers.clone(),
                );

                if stored {
                    record_cache_store(&self.host, "memory");
                    ftlog::debug!(
                        "Cached response for {} (status={}, size={}, vary={:?})",
                        self.host,
                        self.status_code,
                        self.captured_body.len(),
                        self.vary_headers
                    );
                }

                return stored;
            }
        }
        false
    }
}

/// Prometheusメトリクスをテキストフォーマットでエンコード
#[cfg(feature = "metrics")]
pub(crate) fn encode_prometheus_metrics() -> Vec<u8> {
    let encoder = TextEncoder::new();
    let metric_families = METRICS_REGISTRY.gather();
    let mut buffer = Vec::new();
    encoder
        .encode(&metric_families, &mut buffer)
        .unwrap_or_default();
    buffer
}

/// metrics feature 無効時のスタブ
#[cfg(not(feature = "metrics"))]
#[allow(dead_code)]
#[inline]
pub(crate) fn encode_prometheus_metrics() -> Vec<u8> {
    Vec::new()
}

/// メトリクスファミリーを (メトリクス名, [(ラベル集合, 値)]) 形式で収集する（F-10）
///
/// OpenTelemetry エクスポータが Prometheus レジストリの値をブリッジする際に使用する。
/// metrics feature 無効時は空配列を返す。
#[allow(clippy::type_complexity)]
pub fn gather_metric_families() -> Vec<(String, Vec<(Vec<(String, String)>, f64)>)> {
    #[cfg(feature = "metrics")]
    {
        let mut result = Vec::new();
        for mf in METRICS_REGISTRY.gather() {
            let name = mf.name().to_string();
            let mut samples = Vec::new();
            for m in mf.get_metric() {
                let labels: Vec<(String, String)> = m
                    .get_label()
                    .iter()
                    .map(|l| (l.name().to_string(), l.value().to_string()))
                    .collect();
                // カウンタ/ゲージ/ヒストグラムの代表値を抽出
                // MessageField は Option ライクなので as_ref で内部を取り出す
                let value = if let Some(c) = m.get_counter().as_ref() {
                    c.value()
                } else if let Some(g) = m.get_gauge().as_ref() {
                    g.value()
                } else if let Some(h) = m.get_histogram().as_ref() {
                    h.get_sample_sum()
                } else {
                    continue;
                };
                samples.push((labels, value));
            }
            if !samples.is_empty() {
                result.push((name, samples));
            }
        }
        result
    }

    #[cfg(not(feature = "metrics"))]
    {
        Vec::new()
    }
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
    _method: &str,
    _host: &str,
    _status: u16,
    _req_body_size: u64,
    _resp_body_size: u64,
    _duration_secs: f64,
) {
    #[cfg(feature = "metrics")]
    {
        // ランタイムで無効化されている場合は記録しない
        if !metrics_runtime_enabled() {
            return;
        }
        // ステータスコードを事前割り当てバッファで文字列化（アロケーション回避）
        let mut status_buf = itoa::Buffer::new();
        let status_str = status_buf.format(_status);

        HTTP_REQUESTS_TOTAL
            .with_label_values(&[_method, status_str, _host])
            .inc();

        HTTP_REQUEST_DURATION_SECONDS
            .with_label_values(&[_method, _host])
            .observe(_duration_secs);

        HTTP_REQUEST_SIZE_BYTES.observe(_req_body_size as f64);
        HTTP_RESPONSE_SIZE_BYTES.observe(_resp_body_size as f64);
    }
}

/// メトリクスエンドポイント用のHTTPレスポンスを生成
pub(crate) fn build_metrics_response() -> Vec<u8> {
    // ランタイムで無効化されている場合は 404 を返す（F-09）
    if !metrics_runtime_enabled() {
        return b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_vec();
    }
    #[cfg(feature = "metrics")]
    {
        let body = encode_prometheus_metrics();
        let mut response = Vec::with_capacity(256 + body.len());
        response.extend_from_slice(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: ");

        let mut num_buf = itoa::Buffer::new();
        response.extend_from_slice(num_buf.format(body.len()).as_bytes());
        response.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
        response.extend_from_slice(&body);
        return response;
    }

    #[cfg(not(feature = "metrics"))]
    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
}

// ====================
// メトリクスのテスト（F-09）
// ====================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_toggle_controls_endpoint() {
        // デフォルトは有効
        set_metrics_runtime_enabled(true);
        assert!(metrics_runtime_enabled());
        let resp = build_metrics_response();
        // 有効時は 200（metrics feature 有効時）または 404（無効時）
        let s = String::from_utf8_lossy(&resp);
        #[cfg(feature = "metrics")]
        assert!(
            s.starts_with("HTTP/1.1 200"),
            "expected 200 when enabled: {}",
            s
        );

        // 無効化すると 404 を返す
        set_metrics_runtime_enabled(false);
        assert!(!metrics_runtime_enabled());
        let resp = build_metrics_response();
        let s = String::from_utf8_lossy(&resp);
        assert!(
            s.starts_with("HTTP/1.1 404"),
            "expected 404 when disabled: {}",
            s
        );

        // 後続テストに影響しないよう元に戻す
        set_metrics_runtime_enabled(true);
    }

    #[test]
    fn record_functions_are_noop_when_disabled() {
        // 無効時に record_* がパニックしないことを確認
        set_metrics_runtime_enabled(false);
        record_circuit_breaker_open("up");
        set_circuit_breaker_state("up", 1);
        record_retry("up", "success");
        set_outlier_ejected("up", "s1", true);
        record_connection_pool_hit("up");
        record_connection_pool_miss("up");
        set_connection_pool_size("up", 3);
        record_grpc_request("/svc/Method", "0", "up");
        observe_grpc_stream_duration("/svc/Method", 0.1);
        observe_wasm_filter_duration("auth", "request", 0.001);
        set_metrics_runtime_enabled(true);
    }
}
