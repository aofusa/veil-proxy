//! 設定モジュール
//!
//! データ型、構造体、列挙型、静的変数、設定読み込み関数を提供します。

use crate::constants::*;
use crate::logging::*;
use crate::pool::*;
use crate::runtime::io::AsyncWriteRentExt;
use crate::runtime::tcp::TcpStream;
use crate::runtime::time::timeout;
use arc_swap::ArcSwap;
use clap::Parser;
use ftlog::{debug, info, warn};
use httparse::{Request, Status};
use once_cell::sync::Lazy;
use rustls::ServerConfig;
use rustls_pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::wasm_plugin_config::{ModuleRef, PluginConfiguration};

#[cfg(veil_ktls)]
use crate::ktls_rustls::{KtlsClientStream, KtlsServerStream, RustlsConnector};

#[cfg(not(veil_ktls))]
use crate::simple_tls;

#[cfg(feature = "http2")]
use crate::protocol;

use crate::buffering;
use crate::cache;
#[cfg(feature = "http2")]
use crate::http2;
#[cfg(feature = "http3")]
use crate::http3_server;
use crate::routing;

// F-233: このファイルは設定の型（[server]・[[route]]・[[l4]]・[http2]・[http3] 等）と単体テストで 3,000 行を
// 超える。設定の型は examples/config.toml の節と 1 対 1 に読めるよう、ここにまとめて置く。
// F-233: 機能ごとのファイル（子モジュールは `use super::*;` で親の名前を見る）
mod net_policy;
pub use self::net_policy::*;
mod compression;
pub use self::compression::*;
mod sandbox_paths;
pub(crate) use self::sandbox_paths::*;
mod prometheus;
pub use self::prometheus::*;
mod runtime_state;
pub use self::runtime_state::*;
mod tls_connector;
pub use self::tls_connector::*;
mod log_config;
pub use self::log_config::*;
mod backend;
pub use self::backend::*;
mod upstream_group;
pub use self::upstream_group::*;
mod async_io;
pub use self::async_io::*;
mod validate;
pub use self::validate::*;
mod tls_server;
pub use self::tls_server::*;
mod loaded;
pub use self::loaded::*;
mod cli;
pub use self::cli::*;
mod listeners;
pub use self::listeners::*;
// ====================
// WASM Response Filter Context
// ====================
// 設定構造体
// ====================

/// 上流 HTTPS で HTTP/2 を使うか（F-175）。平文（`http://`）の上流には効かない（h2c は `use_h2c`）。
///
/// - `"auto"`（既定）: TLS の ALPN で `h2, http/1.1` を提示し、上流が選んだ方で話す。HTTP/2 なら
///   多重化接続（F-174）を使う。上流が HTTP/1.1 を選んだ結果はワーカーごとに覚え、しばらくは
///   ALPN `http/1.1` だけで接続する。
/// - `"on"`: HTTP/2 を必須にする（上流が HTTP/2 を選ばなければ 502）。
/// - `"off"`: ALPN は `http/1.1` のみ（v0.7 までの挙動）。
#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UpstreamHttp2 {
    Off,
    #[default]
    Auto,
    On,
}

/// Upstream サーバーエントリ（文字列または構造体）
///
/// 以下の2つの形式をサポート:
/// - 文字列形式: "http://localhost:8080"
/// - 構造体形式: { url = "https://192.168.1.100:443", sni_name = "api.example.com" }
#[derive(Clone, Debug)]
pub struct UpstreamServerEntry {
    pub url: String,
    pub sni_name: Option<String>,
    pub use_h2c: bool,
    /// HTTPS 上流の HTTP/2（F-175）。`None` ならグループの設定（既定 `"auto"`）。
    pub http2: Option<UpstreamHttp2>,
    /// 重み（Weighted Round Robin 用、デフォルト 1）
    pub weight: u32,
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
                    use_h2c: false,
                    http2: None,
                    weight: 1,
                })
            }

            // 構造体形式: { url = "...", sni_name = "...", weight = 2 }
            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut url: Option<String> = None;
                let mut sni_name: Option<String> = None;
                let mut use_h2c: Option<bool> = None;
                let mut http2: Option<UpstreamHttp2> = None;
                let mut weight: Option<u32> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "url" => url = Some(map.next_value()?),
                        "sni_name" => sni_name = Some(map.next_value()?),
                        "use_h2c" | "h2c" => use_h2c = Some(map.next_value()?),
                        "http2" => http2 = Some(map.next_value()?),
                        "weight" => weight = Some(map.next_value()?),
                        _ => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }

                let url = url.ok_or_else(|| serde::de::Error::missing_field("url"))?;
                let use_h2c = use_h2c.unwrap_or(false);
                // weight=0 は無効なので最低1に補正
                let weight = weight.unwrap_or(1).max(1);
                Ok(UpstreamServerEntry {
                    url,
                    sni_name,
                    use_h2c,
                    http2,
                    weight,
                })
            }
        }

        deserializer.deserialize_any(UpstreamServerEntryVisitor)
    }
}

/// Upstream 設定（ロードバランシング用）
#[derive(Deserialize, Clone, Debug)]
pub struct UpstreamConfig {
    /// ロードバランシングアルゴリズム
    /// - "round_robin": ラウンドロビン（デフォルト）
    /// - "least_conn": Least Connections
    /// - "ip_hash": クライアントIPハッシュ
    #[serde(default)]
    pub algorithm: LoadBalanceAlgorithm,
    /// Consistent Hash 用のハッシュキー（algorithm = "consistent_hash" 時のみ有効）
    /// 省略時は IP ベース
    #[serde(default)]
    pub hash_key: Option<HashKey>,
    /// バックエンドサーバーエントリ一覧
    /// 文字列形式と構造体形式の両方をサポート
    pub servers: Vec<UpstreamServerEntry>,
    /// 健康チェック設定（オプション）
    #[serde(default)]
    pub health_check: Option<HealthCheckConfig>,
    /// TLS証明書検証を無効化（自己署名証明書を許可）
    /// デフォルト: false（証明書検証を有効）
    /// 注意: 本番環境では false を推奨
    #[serde(default)]
    pub tls_insecure: bool,
    /// 上流の mTLS（`tls_client_cert`・`tls_client_key`・`tls_ca`・`expected_san`。F-217）
    #[serde(flatten)]
    pub tls: crate::upstream_tls::UpstreamTlsSettings,
    /// サーキットブレーカー設定（F-06）
    #[serde(default)]
    pub circuit_breaker: CircuitBreakerConfig,
    /// 異常検知（Outlier Detection）設定（F-06）
    #[serde(default)]
    pub outlier_detection: OutlierConfig,
    /// HTTPS 上流の HTTP/2（F-175、既定 `"auto"`）。サーバー単位の `http2` が優先する。
    #[serde(default)]
    pub http2: UpstreamHttp2,
}

impl UpstreamConfig {
    /// サーバーエントリへグループの `http2` を既定値として埋めたもの（F-175）。
    fn resolved_servers(&self) -> Vec<UpstreamServerEntry> {
        self.servers
            .iter()
            .cloned()
            .map(|mut e| {
                e.http2.get_or_insert(self.http2);
                e
            })
            .collect()
    }
}

/// サーキットブレーカー設定（F-06）
///
/// アップストリームサーバー単位で適用される。連続失敗が閾値を超えると
/// Open 状態へ遷移し、一定時間経過後 HalfOpen でプローブを行う。
#[derive(Deserialize, Clone, Debug)]
pub struct CircuitBreakerConfig {
    /// 有効化フラグ
    #[serde(default)]
    pub enabled: bool,
    /// Open へ遷移する失敗回数の閾値
    #[serde(default = "default_cb_failure_threshold")]
    pub failure_threshold: u32,
    /// 失敗をカウントするスライディングウィンドウ（秒）
    #[serde(default = "default_cb_failure_window")]
    pub failure_window_secs: u64,
    /// Open 状態を維持する時間（秒）
    #[serde(default = "default_cb_open_duration")]
    pub open_duration_secs: u64,
    /// HalfOpen 時に許可するプローブ数
    #[serde(default = "default_cb_half_open_probes")]
    pub half_open_probes: u32,
    /// HalfOpen から Closed へ戻る成功回数の閾値
    #[serde(default = "default_cb_success_threshold")]
    pub success_threshold: u32,
    /// タイムアウトを失敗としてカウントするか
    #[serde(default = "default_true")]
    pub trip_on_timeout: bool,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            failure_threshold: default_cb_failure_threshold(),
            failure_window_secs: default_cb_failure_window(),
            open_duration_secs: default_cb_open_duration(),
            half_open_probes: default_cb_half_open_probes(),
            success_threshold: default_cb_success_threshold(),
            trip_on_timeout: true,
        }
    }
}

fn default_cb_failure_threshold() -> u32 {
    5
}
fn default_cb_failure_window() -> u64 {
    60
}
fn default_cb_open_duration() -> u64 {
    30
}
fn default_cb_half_open_probes() -> u32 {
    3
}
fn default_cb_success_threshold() -> u32 {
    2
}

/// 異常検知（パッシブ Outlier Detection）設定（F-06）
#[derive(Deserialize, Clone, Debug)]
pub struct OutlierConfig {
    /// 有効化フラグ
    #[serde(default)]
    pub enabled: bool,
    /// エラー率の閾値（0.0-1.0、デフォルト 0.5）
    #[serde(default = "default_outlier_error_rate")]
    pub error_rate_threshold: f64,
    /// 評価間隔（秒）
    #[serde(default = "default_outlier_interval")]
    pub interval_secs: u64,
    /// 基本排除時間（秒）
    #[serde(default = "default_outlier_base_ejection")]
    pub base_ejection_time_secs: u64,
    /// 同時に排除可能なサーバーの最大割合（%）
    #[serde(default = "default_outlier_max_eject_percent")]
    pub max_ejection_percent: u32,
}

impl Default for OutlierConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            error_rate_threshold: default_outlier_error_rate(),
            interval_secs: default_outlier_interval(),
            base_ejection_time_secs: default_outlier_base_ejection(),
            max_ejection_percent: default_outlier_max_eject_percent(),
        }
    }
}

fn default_outlier_error_rate() -> f64 {
    0.5
}
fn default_outlier_interval() -> u64 {
    10
}
fn default_outlier_base_ejection() -> u64 {
    30
}
fn default_outlier_max_eject_percent() -> u32 {
    50
}

// 注: リトライポリシー（RetryPolicy）は F-06 で構造体のみ定義されたが、リトライ機構
// 本体が未実装でどこからも参照されない dead code だったため F-51 で削除した。
// リトライを実装する際は resilience.rs と併せて再設計すること。

/// ヘルスチェックの種別（F-22）
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum HealthCheckType {
    /// HTTP/HTTPS リクエストを送信してステータスコードを確認（デフォルト）
    #[default]
    Http,
    /// TCP 接続の確立可否のみ確認
    Tcp,
    /// gRPC Health Checking Protocol (grpc.health.v1.Health/Check)
    Grpc,
}

/// 健康チェック設定
#[derive(Deserialize, Clone, Debug)]
pub struct HealthCheckConfig {
    /// チェック種別（http / tcp / grpc）。省略時は http。
    #[serde(default)]
    pub check_type: HealthCheckType,
    /// チェック間隔（秒）
    #[serde(default = "default_health_check_interval")]
    pub interval_secs: u64,
    /// チェック対象パス（HTTP チェック時のリクエストパス、gRPC チェック時のサービス名）
    #[serde(default = "default_health_check_path")]
    pub path: String,
    /// タイムアウト（秒）
    #[serde(default = "default_health_check_timeout")]
    pub timeout_secs: u64,
    /// 成功と判断するHTTPステータスコード（HTTP チェック時のみ有効、デフォルト: 200-399）
    #[serde(default = "default_healthy_statuses")]
    pub healthy_statuses: Vec<u16>,
    /// 何回連続で失敗したら unhealthy とするか
    #[serde(default = "default_unhealthy_threshold")]
    pub unhealthy_threshold: u32,
    /// 何回連続で成功したら healthy に戻すか
    #[serde(default = "default_healthy_threshold")]
    pub healthy_threshold: u32,
    /// TLS接続を使用するかどうか
    /// デフォルト: false（既存のHTTP健康チェックを使用）
    #[serde(default)]
    pub use_tls: bool,
    /// 証明書検証を有効化するかどうか（use_tls=true時のみ有効）
    /// デフォルト: true
    #[serde(default = "default_true")]
    pub verify_cert: bool,
}

fn default_health_check_interval() -> u64 {
    10
}
fn default_health_check_path() -> String {
    "/".to_string()
}
fn default_health_check_timeout() -> u64 {
    5
}
fn default_healthy_statuses() -> Vec<u16> {
    vec![200, 201, 202, 204, 301, 302, 304]
}
fn default_unhealthy_threshold() -> u32 {
    3
}
fn default_healthy_threshold() -> u32 {
    2
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            check_type: HealthCheckType::Http,
            interval_secs: default_health_check_interval(),
            path: default_health_check_path(),
            timeout_secs: default_health_check_timeout(),
            healthy_statuses: default_healthy_statuses(),
            unhealthy_threshold: default_unhealthy_threshold(),
            healthy_threshold: default_healthy_threshold(),
            use_tls: false,
            verify_cert: default_true(),
        }
    }
}

// ====================
// 統合ルーティング（AWS ALB準拠）
// ====================

/// ルーティング条件（AWS ALB準拠）
///
/// すべての条件はANDで結合されます。
/// 条件が指定されていない場合は、すべてのリクエストにマッチします（デフォルトルート）。
#[derive(Clone, Debug, Deserialize, Default)]
pub struct RouteConditions {
    /// host-header: ホスト名マッチ（ワイルドカード対応）
    /// 例: "api.example.com", "*.example.com"
    #[serde(default)]
    pub host: Option<String>,

    /// path-pattern: パスマッチ（ワイルドカード対応）
    /// 例: "/api/*", "/static/*", "/api/v2/*"
    #[serde(default)]
    pub path: Option<String>,

    /// http-header: HTTPヘッダーマッチ（複数指定可能）
    /// 例: { "X-Version" = "v2" }
    #[serde(default)]
    pub header: Option<HashMap<String, String>>,

    /// http-request-method: HTTPメソッドマッチ（配列対応）
    /// 例: ["GET", "POST"]
    #[serde(default)]
    pub method: Option<Vec<String>>,

    /// query-string: クエリパラメータマッチ（複数指定可能）
    /// 例: { "key" = "value" }
    #[serde(default)]
    pub query: Option<HashMap<String, String>>,

    /// source-ip: ソースIPマッチ（CIDR表記）
    /// 例: ["192.168.0.0/16", "10.0.0.0/8"]
    #[serde(default)]
    pub source_ip: Option<Vec<String>>,
}

/// ルーティングルール
///
/// 条件に一致するリクエストに対して、指定されたバックエンドアクションを実行します。
#[derive(Clone, Debug, Deserialize)]
pub struct Route {
    /// ルーティング条件（空の場合はデフォルトルート）
    #[serde(default)]
    pub conditions: RouteConditions,

    /// バックエンドアクション
    pub action: BackendConfig,

    /// ルートレベルのセキュリティ設定（actionの設定をオーバーライド）
    #[serde(default)]
    pub security: Option<SecurityConfig>,

    /// ルートレベルの圧縮設定（actionの設定をオーバーライド）
    #[serde(default)]
    pub compression: Option<CompressionConfig>,

    /// ルートレベルのバッファリング設定（actionの設定をオーバーライド）
    #[serde(default)]
    pub buffering: Option<buffering::BufferingConfig>,

    /// ルートレベルのキャッシュ設定（actionの設定をオーバーライド）
    #[serde(default)]
    pub cache: Option<cache::CacheConfig>,

    /// ルートレベルのOpenFileCache設定（actionの設定をオーバーライド、Fileバックエンドのみ）
    #[serde(default)]
    pub open_file_cache: Option<cache::OpenFileCacheConfig>,

    /// ルートレベルの静的コンテンツキャッシュ設定（F-146、Fileバックエンドのみ、
    /// actionの設定をオーバーライド）
    #[serde(default)]
    pub static_file_cache: Option<cache::StaticContentCacheRouteConfig>,

    /// ルートレベルのWASMモジュール名のリスト（このルートに適用するWASMモジュール）
    /// 注意: modules は route 直下で設定（action配下の設定は削除）
    #[serde(default)]
    pub modules: Option<Vec<String>>,

    /// ルート単位の WASM モジュール設定上書き（モジュール名 → 設定値、F-148）。
    /// `[[wasm.modules]]` 側の `configuration`（TOML 文字列 or テーブル）を base、
    /// ここに書いた値を override として合成する（合成規則は
    /// `PluginConfiguration::merge_over` 参照）。
    #[serde(default)]
    pub module_configuration: Option<HashMap<String, PluginConfiguration>>,

    /// 設定ロード時に解決した実効モジュール参照（名前 + マージ済み設定、F-148）。
    /// `load_backend` はリクエストごとに呼ばれる（B-64）ため、ここで事前計算して
    /// Arc 共有する（ホットパスでの合成・アロケーションを避ける）。
    #[serde(skip)]
    pub resolved_modules: Option<Arc<Vec<ModuleRef>>>,

    /// 設定ロード時に解決済みの Backend（F-159）。`load_backend` はリクエストごとに
    /// 呼ばれるため、ここで 1 回だけ構築して clone（Arc 参照カウント増分のみ）で返す。
    /// 解決に失敗した場合（存在しないファイル等）は `None` のままにし、ホットパスは
    /// 従来どおり毎回構築してエラーを返す（挙動の後方互換）。
    #[serde(skip)]
    pub resolved_backend: Option<Backend>,
    /// 設定ロード時に解決済みの圧縮設定（`find_backend_unified` の戻り値用、F-159）。
    #[serde(skip)]
    pub resolved_compression: Option<Arc<CompressionConfig>>,
    /// 設定ロード時に解決済みのパスプレフィックス（`extract_path_prefix` の結果、F-159）。
    #[serde(skip)]
    pub resolved_path_prefix: Option<Arc<[u8]>>,

    /// ルート単位のトラフィック制御（重み付き分割・フォルトの中断・CORS のプリフライト。F-219）
    #[serde(default)]
    pub traffic: Option<crate::route_traffic::TrafficConfig>,
    /// JWT の検証と claim の条件（F-220。`jwt` feature）
    #[serde(default)]
    pub jwt: Option<crate::route_traffic::JwtConfig>,
    /// 設定ロード時に解決済みの `traffic`（F-219。無ければ `None` でホットパスは分岐 1 個だけ）
    #[serde(skip)]
    pub resolved_traffic: Option<Arc<crate::route_traffic::TrafficPolicy>>,
}

#[derive(Deserialize)]
struct Config {
    /// 省略可（`[[listener]]` だけで待ち受けを書く設定。F-260）
    #[serde(default = "default_server_section")]
    server: ServerConfigSection,
    /// 待ち受けの列（F-260）。`[server] listen`・`listen_cleartext` は暗黙のリスナーとして先頭に足す。
    #[serde(default)]
    listener: Option<Vec<ListenerConfigSection>>,
    /// `[acme]` のうち待ち受けの重なりの検査に使う部分（F-260。本体は `acme::bootstrap` が読む）
    #[cfg(feature = "acme")]
    #[serde(default)]
    acme: AcmeListenSection,
    /// 省略可（平文専用サーバ構成のときだけ。`validate_tls_presence` 参照）。
    #[serde(default = "default_tls_section")]
    tls: TlsConfigSection,
    #[serde(default)]
    performance: PerformanceConfigSection,
    /// 外向きの TLS 全体の既定の信頼アンカー（F-259）
    #[serde(default)]
    tls_client: crate::trust_store::TlsClientConfig,
    /// グローバルセキュリティ設定（権限降格など）
    #[serde(default)]
    security: GlobalSecurityConfig,
    /// ログ設定（非同期ログの最適化）
    #[serde(default)]
    logging: LoggingConfigSection,
    /// Prometheusメトリクス設定
    #[serde(default)]
    prometheus: PrometheusConfig,
    /// 管理 API 設定（F-20: キャッシュ Purge 等）
    #[cfg(feature = "admin")]
    #[serde(default)]
    admin: AdminConfig,
    /// 構造化アクセスログ設定（F-21）
    #[cfg(feature = "access-log")]
    #[serde(default)]
    access_log: crate::access_log::AccessLogConfig,
    /// OpenTelemetry 設定（F-10）
    #[serde(default)]
    opentelemetry: OpenTelemetryConfig,
    /// バッファプール設定（メモリ最適化）
    #[serde(default)]
    buffer_pool: BufferPoolConfig,
    /// 静的ファイル本体キャッシュ設定（F-146、グローバルデフォルト）
    ///
    /// HTTP/2・HTTP/3 静的配信のファイル本体（`bytes::Bytes`）をユーザ空間メモリに
    /// 保持し、リクエストごとの `offload(std::fs::read)` 往復を避ける。
    /// 既定は無効（[`cache::StaticContentCacheConfig::default`]）。
    #[serde(default)]
    static_file_cache: cache::StaticContentCacheConfig,
    /// HTTP/2 設定セクション
    #[cfg(feature = "http2")]
    #[serde(default)]
    http2: Http2ConfigSection,
    /// `http2` feature 無しのビルドでは `[http2]` を解釈しない。キーは拒否せずに受け取り、
    /// あれば読み込み時に無視した旨を警告する（[`warn_unbuilt_sections`]）。
    #[cfg(not(feature = "http2"))]
    #[serde(default)]
    http2: Option<toml::Value>,
    /// HTTP/3 設定セクション
    #[serde(default)]
    http3: Http3ConfigSection,
    /// Upstream グループ定義（ロードバランシング用）
    #[serde(default)]
    upstreams: Option<HashMap<String, UpstreamConfig>>,
    /// 統合ルーティング（唯一のルーティング方式）
    /// 配列の順序で評価（first-match方式）
    #[serde(default)]
    route: Option<Vec<Route>>,
    /// WASM拡張設定（feature flagで条件付きコンパイル）
    #[cfg(feature = "wasm")]
    #[serde(default)]
    wasm: Option<crate::wasm::WasmConfig>,
    /// L4 (TCP/UDP) ストリームプロキシ設定（F-18）
    #[cfg(feature = "l4-proxy")]
    #[serde(default)]
    l4: Option<Vec<L4ListenerConfig>>,
}

/// `[server]` を省略したときの既定値（F-260）
fn default_server_section() -> ServerConfigSection {
    toml::from_str("").expect("ServerConfigSection must deserialize from an empty table")
}

/// `[acme]` の待ち受けに関わるキー（F-260。既定は `acme::AcmeConfig` と同じ）
#[cfg(feature = "acme")]
#[derive(Deserialize)]
struct AcmeListenSection {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_acme_challenge_listen")]
    challenge_listen: String,
}

#[cfg(feature = "acme")]
impl Default for AcmeListenSection {
    fn default() -> Self {
        AcmeListenSection {
            enabled: false,
            challenge_listen: default_acme_challenge_listen(),
        }
    }
}

#[cfg(feature = "acme")]
fn default_acme_challenge_listen() -> String {
    crate::acme::AcmeConfig::default().challenge_listen
}

// ====================
// L4 (TCP/UDP) ストリームプロキシ設定（F-18）
// ====================

/// L4 リスナーのトランスポートプロトコル（F-124）
///
/// TOML では `protocol = "tcp"` / `"udp"`。省略時は後方互換のため `Tcp`。
#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum L4Protocol {
    /// TCP（バイダイレクショナルストリーム転送、既存動作）
    #[default]
    Tcp,
    /// UDP（セッションテーブル方式のデータグラム転送、F-124）
    Udp,
}

/// L4 TLS モード
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum L4TlsMode {
    /// TLS なし（プレーンな TCP）
    #[default]
    None,
    /// TLS パススルー（TLS を復号せず upstream にそのまま転送）
    Passthrough,
    /// TLS ターミネーション（veil で TLS を終端し、upstream にはプレーン TCP で接続）
    Terminate,
}

/// L4 ロードバランシングアルゴリズム
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum L4LbAlgorithm {
    /// ラウンドロビン（デフォルト）
    #[default]
    RoundRobin,
    /// 最小接続数
    LeastConn,
}

/// L4 upstream バックエンド
#[derive(Deserialize, Clone, Debug)]
pub struct L4UpstreamEntry {
    /// バックエンドアドレス（"host:port" 形式）
    pub addr: String,
    /// 重み（weighted round-robin 用、デフォルト: 1）
    #[serde(default = "default_l4_weight")]
    pub weight: u32,
}

fn default_l4_weight() -> u32 {
    1
}

/// L4 リスナー設定（TOML: `[[l4]]`）
#[derive(Deserialize, Clone, Debug)]
pub struct L4ListenerConfig {
    /// リスナー名（ログ・メトリクス識別用）
    pub name: String,
    /// リッスンアドレス（例: "0.0.0.0:3306"）
    pub listen: String,
    /// upstream バックエンド一覧（`handler` を使うリスナーでは省略可、F-192）
    #[cfg_attr(feature = "l4-handler", serde(default))]
    pub upstreams: Vec<L4UpstreamEntry>,
    /// トランスポートプロトコル（tcp / udp）。省略時は後方互換で tcp（F-124）。
    #[serde(default)]
    pub protocol: L4Protocol,
    /// ロードバランシングアルゴリズム
    #[serde(default)]
    pub lb: L4LbAlgorithm,
    /// TLS モード（none / passthrough / terminate）
    #[serde(default)]
    pub tls: L4TlsMode,
    /// 最大同時接続数（0 = 無制限）
    #[serde(default)]
    pub max_connections: usize,
    /// ヘルスチェック設定（省略可）
    #[serde(default)]
    pub health_check: Option<HealthCheckConfig>,
    /// バックエンド接続タイムアウト（秒）
    #[serde(default = "default_l4_connect_timeout")]
    pub connect_timeout_secs: u64,
    /// アイドルタイムアウト（秒）: この時間データ転送がなければ接続を切断（デフォルト: 600）
    #[serde(default = "default_l4_idle_timeout")]
    pub idle_timeout_secs: u64,
    /// WASM network filter モジュール名一覧（F-133）。空（デフォルト）なら WASM 無効で
    /// 従来の splice/ゼロコピー転送経路を使う（ホットパス絶対規則: 追加コストなし）。
    /// 指定すると splice を使わずユーザー空間バッファ経由の転送へ切り替わる
    /// （データを WASM に見せる必要があるため）。
    #[cfg(feature = "wasm")]
    #[serde(default)]
    pub wasm_modules: Vec<String>,
    /// L4 リスナー単位の WASM モジュール設定上書き（モジュール名 → 設定値、F-148）。
    /// HTTP ルートの `Route::module_configuration` と同じ合成規則。
    #[cfg(feature = "wasm")]
    #[serde(default)]
    pub module_configuration: Option<HashMap<String, PluginConfiguration>>,
    /// 設定ロード時に解決した実効モジュール参照（F-148）。`wasm_modules` から
    /// `resolve_l4_wasm_modules` で 1 回だけ構築し、以後は Arc 共有で持ち回る。
    #[cfg(feature = "wasm")]
    #[serde(skip)]
    pub resolved_wasm_modules: Arc<Vec<ModuleRef>>,
    /// ネイティブ L4 ハンドラ名（API ライブラリ、F-192）。指定すると上流へ転送せず、登録した
    /// Rust 関数で接続・データグラムを処理する（`upstreams` は不要）。
    #[cfg(feature = "l4-handler")]
    #[serde(default)]
    pub handler: Option<String>,
}

fn default_l4_connect_timeout() -> u64 {
    10
}

fn default_l4_idle_timeout() -> u64 {
    600
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
    /// デフォルト: 65536 (64KB)。これは **受信側の上限として相手へ広告する値** であり、
    /// 送信フレームは相手の SETTINGS_MAX_FRAME_SIZE（既定 16384）に従って分割される。
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

    // ====================
    // DoS 対策設定
    // ====================
    /// RST_STREAM レート制限 (1秒あたりの最大数)
    /// Rapid Reset 対策 (CVE-2023-44487)
    /// デフォルト: 100
    #[serde(default = "default_h2_max_rst_stream_per_second")]
    pub max_rst_stream_per_second: u32,

    /// 制御フレームレート制限 (1秒あたりの最大数)
    /// PING/SETTINGS フラッド対策
    /// デフォルト: 500
    #[serde(default = "default_h2_max_control_frames_per_second")]
    pub max_control_frames_per_second: u32,

    /// CONTINUATION フレーム制限 (ヘッダーブロックあたりの最大数)
    /// CONTINUATION Flood 対策 (CVE-2024-24786)
    /// デフォルト: 10
    #[serde(default = "default_h2_max_continuation_frames")]
    pub max_continuation_frames: u32,

    /// 最大ヘッダーブロックサイズ (bytes)
    /// HPACK Bomb 対策
    /// デフォルト: 65536 (64KB)
    #[serde(default = "default_h2_max_header_block_size")]
    pub max_header_block_size: usize,

    /// ストリームアイドルタイムアウト (秒)
    /// Slow Loris 対策
    /// デフォルト: 60
    #[serde(default = "default_h2_stream_idle_timeout_secs")]
    pub stream_idle_timeout_secs: u64,
}

// HTTP/2 設定のデフォルト値（high_performance と同等）
// HPACK動的テーブルサイズを大きくすることで、ヘッダー圧縮効率が向上
fn default_h2_header_table_size() -> u32 {
    65536
} // 64KB (より多くのヘッダーをキャッシュ)
fn default_h2_max_concurrent_streams() -> u32 {
    256
} // より多くの同時ストリーム
fn default_h2_initial_window_size() -> u32 {
    1048576
} // 1MB (より大きなウィンドウ)
fn default_h2_max_frame_size() -> u32 {
    65536
} // 64KB (より大きなフレーム)
fn default_h2_max_header_list_size() -> u32 {
    65536
} // 64KB
fn default_h2_connection_window_size() -> u32 {
    1048576
} // 1MB

// DoS 対策のデフォルト値
fn default_h2_max_rst_stream_per_second() -> u32 {
    100
}
fn default_h2_max_control_frames_per_second() -> u32 {
    500
}
fn default_h2_max_continuation_frames() -> u32 {
    10
}
fn default_h2_max_header_block_size() -> usize {
    65536
}
fn default_h2_stream_idle_timeout_secs() -> u64 {
    60
}

impl Default for Http2ConfigSection {
    fn default() -> Self {
        Self {
            header_table_size: default_h2_header_table_size(),
            max_concurrent_streams: default_h2_max_concurrent_streams(),
            initial_window_size: default_h2_initial_window_size(),
            max_frame_size: default_h2_max_frame_size(),
            max_header_list_size: default_h2_max_header_list_size(),
            connection_window_size: default_h2_connection_window_size(),
            // DoS 対策
            max_rst_stream_per_second: default_h2_max_rst_stream_per_second(),
            max_control_frames_per_second: default_h2_max_control_frames_per_second(),
            max_continuation_frames: default_h2_max_continuation_frames(),
            max_header_block_size: default_h2_max_header_block_size(),
            stream_idle_timeout_secs: default_h2_stream_idle_timeout_secs(),
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
            // DoS 対策
            max_rst_stream_per_second: self.max_rst_stream_per_second,
            max_control_frames_per_second: self.max_control_frames_per_second,
            max_continuation_frames: self.max_continuation_frames,
            max_header_block_size: self.max_header_block_size,
            stream_idle_timeout_secs: self.stream_idle_timeout_secs,
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
            if !(1..=9).contains(&level) {
                return Err(format!(
                    "http3.compression.gzip_level: {} (must be 1-9)",
                    level
                ));
            }
        }
        if let Some(level) = self.brotli_level {
            if level > 11 {
                return Err(format!(
                    "http3.compression.brotli_level: {} (must be 0-11)",
                    level
                ));
            }
        }
        if let Some(level) = self.zstd_level {
            if !(1..=22).contains(&level) {
                return Err(format!(
                    "http3.compression.zstd_level: {} (must be 1-22)",
                    level
                ));
            }
        }
        if let Some(ref encodings) = self.preferred_encodings {
            for enc in encodings {
                if !["zstd", "br", "gzip", "deflate"].contains(&enc.as_str()) {
                    return Err(format!(
                        "http3.compression.preferred_encodings: unknown encoding '{}'",
                        enc
                    ));
                }
            }
        }
        Ok(())
    }
}

/// HTTP/3 詳細設定
///
/// HTTP/3 (QUIC) プロトコルのパラメータを設定します。
/// 有効化は `[http3] enabled` で行います。
#[derive(Deserialize, Clone)]
pub struct Http3ConfigSection {
    /// HTTP/3 を起動するか（F-200。旧 `[server].http3_enabled`）。`[server].listen`（TLS）が必要。
    #[serde(default)]
    pub enabled: bool,
    /// HTTP/3リッスンアドレス（UDP）
    /// 未指定の場合は server.listen と同じアドレスを使用
    #[serde(default)]
    pub listen: Option<String>,

    /// 最大アイドルタイムアウト（ミリ秒）
    /// デフォルト: 30000 (30秒)
    #[serde(default = "default_h3_max_idle_timeout")]
    pub max_idle_timeout: u64,

    /// 最大UDPペイロードサイズ（経路 MTU の探索の上限）
    ///
    /// デフォルト: 1452（v0.8.0 で 1350 から変更。MTU 1500 の IPv6 で載る最大）。`discover_pmtu` が有効なら quiche は
    /// 1200 から始めて、探索で確かめた大きさまでしか使わないので、MTU の小さい経路でも届かなくはならない。
    /// 1350 固定だと 54KB の応答に 42 パケット（nginx 39）で、Linux の Docker で HTTP/3 が 6.7% 遅かった。
    #[serde(default = "default_h3_max_udp_payload_size")]
    pub max_udp_payload_size: u64,

    /// 経路 MTU の探索（quiche `discover_pmtu`、RFC 8899 の DPLPMTUD）。UDP のソケットには DF を立てる。
    ///
    /// デフォルト: `true`（v0.8.0）
    #[serde(default = "default_true")]
    pub discover_pmtu: bool,

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

    /// GSO/GRO を有効化するかどうか
    ///
    /// GSO (Generic Segmentation Offload) / GRO (Generic Receive Offload) は
    /// カーネルレベルでUDPパケットの送受信を効率化する機能です。
    ///
    /// 効果:
    /// - 複数の小さなUDPパケットを一度に送受信
    /// - システムコール回数の削減
    /// - CPU使用率の低減
    ///
    /// 注意:
    /// - Linux 5.0+ でサポート
    /// - 一部の仮想環境やDockerでは期待通りに動作しない場合あり
    /// - 問題が発生した場合は false に設定してください
    ///
    /// デフォルト: false
    #[serde(default)]
    pub gso_gro_enabled: bool,

    // ====================
    // quiche::Config 輻輳制御 / Pacing / バッチ（F-124）
    // ====================
    /// QUIC 輻輳制御アルゴリズム（quiche `set_cc_algorithm_name`）
    ///
    /// 受け付ける値（大小無視）:
    /// - `"reno"` / `"cubic"` / `"bbr"` / `"bbr2"` / `"bbr2_gcongestion"`
    ///
    /// 既定は `"cubic"`（v0.8.0 で `"bbr"` から変更）。veil は quiche の pacing の送出時刻を守らずに送るので、
    /// BBR は送出が一気になり、受け手のソケットのバッファで捨てられて再送が増えていた（FreeBSD のループバックで
    /// 6 秒に 70 万パケットの破棄、54KB の応答 1 つに 61 パケット。CUBIC は 46）。CUBIC で HTTP/3 は FreeBSD の
    /// 54KB が +8〜20%、Linux の Docker が静的 +5.6%・中継 +4.1%。未知の値は起動時に警告して `"cubic"` にする。
    ///
    /// デフォルト: `"cubic"`
    #[serde(default = "default_h3_cc_algorithm")]
    pub cc_algorithm: String,

    /// Packet Pacing を有効にするか（quiche `enable_pacing`）
    ///
    /// 有効時は送信間隔をならしパケットロスを減らす。quiche 自体の既定も true。
    /// デフォルト: `true`
    #[serde(default = "default_true")]
    pub pacing: bool,

    /// 最大 pacing レート（バイト/秒）。未指定時は制限なし（quiche `set_max_pacing_rate`）
    ///
    /// デフォルト: なし（`None`）
    #[serde(default)]
    pub max_pacing_rate: Option<u64>,

    /// HyStart++ を有効にするか（quiche `enable_hystart`）
    ///
    /// デフォルト: `true`
    #[serde(default = "default_true")]
    pub hystart: bool,

    /// UDP データグラム送受信バッチ幅（`recvmmsg` / `sendmmsg` / multishot 提供バッファ数）
    ///
    /// 1 回の syscall / 1 本の multishot アームで扱う最大データグラム数。
    /// 範囲: 1..=128。範囲外はクランプする。
    ///
    /// デフォルト: `64`
    #[serde(default = "default_h3_mmsg_batch_size")]
    pub mmsg_batch_size: usize,

    /// 1 イテレーションあたりに掻き出す UDP データグラム数の上限（reactor バックエンド専用、F-152）
    ///
    /// **readiness reactor（FreeBSD / OpenBSD / NetBSD / macOS、および Linux の
    /// `--features epoll`）だけで使う。** Linux 既定の io_uring バックエンドは
    /// `mmsg_batch_size` 本の `IORING_OP_RECVMSG` を常時 in-flight に保つ
    /// パイプライン方式（F-130）なので本設定は参照しない。
    ///
    /// reactor 経路は「読み込み可能になったら `recv_mmsg_sync` を繰り返して掻き出す」
    /// 形をとる。本設定はその 1 イテレーションあたりの合計データグラム数の上限で、
    /// 大きくするほど select/タイマー往復と接続スイープの固定費を多くのデータグラムへ
    /// 償却できる（F-151 の実験では、1 イテレーションで扱うデータグラム数を増やすと
    /// スループットが 3.2 倍になった）。ただし大きすぎると送信・タイムアウト・
    /// バックエンド通知の処理が遅れるため、レイテンシとのトレードオフになる。
    ///
    /// 範囲: 1..=4096。範囲外はクランプする。
    ///
    /// デフォルト: `1024`（v0.8.0 で 64 から変更。FreeBSD の HTTP/3 3B で +37%）
    #[serde(default = "default_h3_recv_drain_max")]
    pub recv_drain_max: usize,

    // ====================
    // Alt-Svc（HTTP/3 広告、F-94）— すべて [http3] に集約
    // ====================
    /// HTTP/1.1・HTTP/2 応答へ `Alt-Svc` ヘッダーを付与するか
    ///
    /// デフォルト: `true`（`[http3] enabled = true` のときのみ実際に広告される）
    /// `false` にすると広告を抑制する。
    #[serde(default = "default_true")]
    pub alt_svc_enabled: bool,

    /// Alt-Svc ヘッダー値の上書き
    ///
    /// 未指定時はリッスンポートから `h3=":PORT"; ma=86400` を自動生成する。
    /// 明示指定例: `h3=":443"; ma=86400, h3=":443"; ma=2592000`
    #[serde(default)]
    pub alt_svc: Option<String>,

    /// Alt-Svc の max-age（秒）。`alt_svc` 未指定時の自動生成に使用。デフォルト: 86400
    #[serde(default = "default_h3_alt_svc_ma")]
    pub alt_svc_ma_secs: u64,
}

fn default_h3_max_idle_timeout() -> u64 {
    30000
}
fn default_h3_max_udp_payload_size() -> u64 {
    1452
}
fn default_h3_initial_max_data() -> u64 {
    10_000_000
}
fn default_h3_initial_max_stream_data() -> u64 {
    1_000_000
}
fn default_h3_max_streams() -> u64 {
    100
}
fn default_h3_alt_svc_ma() -> u64 {
    86400
}
fn default_h3_cc_algorithm() -> String {
    "cubic".to_string()
}
fn default_h3_mmsg_batch_size() -> usize {
    64
}
/// F-152: reactor 経路の 1 イテレーションあたり drain 上限の既定値
/// （`http3_server::H3_RECV_DRAIN_MAX_DEFAULT` と同値。v0.8.0 で 64 → 1024）。
fn default_h3_recv_drain_max() -> usize {
    1024
}

impl Default for Http3ConfigSection {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: None,
            max_idle_timeout: default_h3_max_idle_timeout(),
            max_udp_payload_size: default_h3_max_udp_payload_size(),
            discover_pmtu: true,
            initial_max_data: default_h3_initial_max_data(),
            initial_max_stream_data_bidi_local: default_h3_initial_max_stream_data(),
            initial_max_stream_data_bidi_remote: default_h3_initial_max_stream_data(),
            initial_max_stream_data_uni: default_h3_initial_max_stream_data(),
            initial_max_streams_bidi: default_h3_max_streams(),
            initial_max_streams_uni: default_h3_max_streams(),
            compression_enabled: false,
            compression: Http3CompressionConfig::default(),
            gso_gro_enabled: false,
            cc_algorithm: default_h3_cc_algorithm(),
            pacing: true,
            max_pacing_rate: None,
            hystart: true,
            mmsg_batch_size: default_h3_mmsg_batch_size(),
            recv_drain_max: default_h3_recv_drain_max(),
            alt_svc_enabled: true,
            alt_svc: None,
            alt_svc_ma_secs: default_h3_alt_svc_ma(),
        }
    }
}

#[cfg(feature = "http3")]
impl Http3ConfigSection {
    /// HTTP/3 設定を Http3ServerConfig に変換
    pub fn to_http3_config(
        &self,
        cert_path: &str,
        key_path: &str,
    ) -> http3_server::Http3ServerConfig {
        http3_server::Http3ServerConfig {
            cert_path: cert_path.to_string(),
            key_path: key_path.to_string(),
            cert_pem: None, // quicheはファイルパスからの読み込みのみサポート
            key_pem: None,
            max_idle_timeout: self.max_idle_timeout,
            max_udp_payload_size: self.max_udp_payload_size,
            discover_pmtu: self.discover_pmtu,
            initial_max_data: self.initial_max_data,
            initial_max_stream_data_bidi_local: self.initial_max_stream_data_bidi_local,
            initial_max_stream_data_bidi_remote: self.initial_max_stream_data_bidi_remote,
            initial_max_stream_data_uni: self.initial_max_stream_data_uni,
            initial_max_streams_bidi: self.initial_max_streams_bidi,
            initial_max_streams_uni: self.initial_max_streams_uni,
            gso_gro_enabled: self.gso_gro_enabled,
            cc_algorithm: self.cc_algorithm.clone(),
            pacing: self.pacing,
            max_pacing_rate: self.max_pacing_rate,
            hystart: self.hystart,
            mmsg_batch_size: self
                .mmsg_batch_size
                .clamp(1, crate::udp::socket::MMSG_BATCH_MAX),
            recv_drain_max: self
                .recv_drain_max
                .clamp(1, http3_server::H3_RECV_DRAIN_MAX_LIMIT),
        }
    }
}

#[derive(Deserialize)]
pub struct ServerConfigSection {
    /// TLS のリスナー（HTTP/1.1・h2（ALPN）。`[http3] enabled = true` なら同じアドレスの UDP で HTTP/3）。
    /// 省略可（平文だけのサーバ）。`unix:<path>` も受理する（F-164）。指定時は `[tls]` が必須（F-200）。
    #[serde(default)]
    pub listen: Option<String>,
    /// 平文のリスナー（HTTP/1.1・h2c（prior knowledge）、F-200）。省略可。
    /// プロトコル検出で HTTP/1.1 か h2c かを判別し、`allow_http`・`allow_h2c` で受けるかを決める。
    #[serde(default)]
    pub listen_cleartext: Option<String>,
    /// `listen_cleartext` で平文 HTTP/1.1 を受けるか（既定 `true`）。
    #[serde(default = "default_true")]
    pub allow_http: bool,
    /// `listen_cleartext` で h2c を受けるか（既定 `true`、要 `http2` feature）。
    #[serde(default = "default_true")]
    pub allow_h2c: bool,
    /// `listen_cleartext` に来た要求（HTTP/1.1・h2c とも）を `https://<Host>[:<listen のポート>]` へ
    /// 301 でリダイレクトするか（既定 `true`）。平文で配信するときは `false`。`listen` が無いのに
    /// `true` は設定エラー（リダイレクト先が無い）。
    #[serde(default = "default_true")]
    pub enable_secure_redirect: bool,
    /// 削除したキー（F-200）。書かれていたら移行先を示す設定エラーにする（黙って無視しない）。
    #[serde(default, rename = "http")]
    removed_http: Option<toml::Value>,
    #[serde(default, rename = "tls_only")]
    removed_tls_only: Option<toml::Value>,
    #[serde(default, rename = "h2c_enabled")]
    removed_h2c_enabled: Option<toml::Value>,
    #[serde(default, rename = "h2c_listen")]
    removed_h2c_listen: Option<toml::Value>,
    #[serde(default, rename = "h2c_http1")]
    removed_h2c_http1: Option<toml::Value>,
    #[serde(default, rename = "http3_enabled")]
    removed_http3_enabled: Option<toml::Value>,
    /// ワーカースレッド数
    /// 未指定または0の場合はCPUコア数と同じスレッド数を使用
    #[serde(default)]
    pub threads: Option<usize>,

    // ====================
    // Serverヘッダー設定
    // ====================
    /// Serverヘッダーを有効化（デフォルト: false）
    ///
    /// セキュリティ考慮事項:
    /// - Serverヘッダーはサーバーソフトウェア情報を公開します
    /// - 攻撃者がバージョン別の脆弱性を狙う手がかりになり得ます
    /// - 本番環境では無効化を推奨
    #[serde(default)]
    pub server_header_enabled: bool,

    /// Serverヘッダーの値（server_header_enabled = true時のみ有効）
    ///
    /// デフォルト: "veil"
    /// 空文字の場合: ヘッダー自体を送信しない
    #[serde(default = "default_server_header_value")]
    pub server_header_value: String,

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
    pub http2_enabled: bool,

    // ====================
    // Unix ドメインソケット（UDS）設定（F-164）
    // ====================
    /// `[server].listen` / `[server].listen_cleartext` に `unix:<path>` を指定した場合の
    /// ソケットファイルパーミッション（8 進数表記の文字列、既定 `"0660"`）。
    ///
    /// bind の前後で `umask(2)` と `chmod(2)` を用いて適用する（Linux では AF_UNIX
    /// ソケット fd への `fchmod(2)` がパスのパーミッションへ反映されないため）。
    /// 不正な値（8 進数として解釈できない、
    /// または `0` 〜 `0777` の範囲外）は設定エラーとして起動を拒否する。
    ///
    /// `cfg(unix)` のみで意味を持つ（TCP リスナーには影響しない）。Windows では
    /// UDS 自体が非対応のため、この設定値も無視される。
    #[serde(default = "default_unix_socket_permissions")]
    pub unix_socket_permissions: String,

    // ====================
    // グレースフルシャットダウン設定
    // ====================
    /// グレースフルシャットダウンのドレインタイムアウト（秒）
    ///
    /// SIGTERMまたはSIGINT受信時に、既存の接続が完了するまで待機する最大時間。
    /// タイムアウト後は残りの接続を強制終了します。
    ///
    /// デフォルト: 30秒
    /// 0: 待機せずに即座に終了（既存の動作）
    #[serde(default = "default_graceful_shutdown_timeout")]
    pub graceful_shutdown_timeout_secs: u64,
}

/// グレースフルシャットダウンタイムアウトのデフォルト値（30秒）
fn default_graceful_shutdown_timeout() -> u64 {
    30
}

/// `[server].unix_socket_permissions` のデフォルト値（`"0660"`）。
fn default_unix_socket_permissions() -> String {
    "0660".to_string()
}

/// `[server].unix_socket_permissions` の文字列表現をパースする（F-164）。
///
/// 8 進数表記（`"0660"` のような先頭 `0` あり、`"660"` のような先頭 `0` なしの
/// いずれも受理する）。`0` 〜 `0777`（10 進で 511）の範囲外や、8 進数として
/// 解釈できない文字列はエラーにする。
pub fn parse_unix_socket_permissions(s: &str) -> Result<u32, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(format!("invalid unix_socket_permissions '{}': empty", s));
    }
    // `0` は有効な 8 進数字でもあるため、先頭の "0" を特別扱いせず
    // そのまま基数 8 でパースできる（"0660" と "660" は同じ結果になる）。
    let mode = u32::from_str_radix(trimmed, 8)
        .map_err(|e| format!("invalid unix_socket_permissions '{}': {}", s, e))?;
    if mode > 0o777 {
        return Err(format!(
            "invalid unix_socket_permissions '{}': must be between 0 and 0777",
            s
        ));
    }
    Ok(mode)
}

#[cfg(test)]
mod unix_socket_permissions_tests {
    use super::parse_unix_socket_permissions;

    #[test]
    fn test_parse_default() {
        assert_eq!(parse_unix_socket_permissions("0660").unwrap(), 0o660);
    }

    #[test]
    fn test_parse_without_leading_zero() {
        assert_eq!(parse_unix_socket_permissions("660").unwrap(), 0o660);
    }

    #[test]
    fn test_parse_full_permissions() {
        assert_eq!(parse_unix_socket_permissions("0777").unwrap(), 0o777);
    }

    #[test]
    fn test_parse_zero() {
        assert_eq!(parse_unix_socket_permissions("0").unwrap(), 0);
        assert_eq!(parse_unix_socket_permissions("0000").unwrap(), 0);
    }

    #[test]
    fn test_parse_invalid_digit() {
        assert!(parse_unix_socket_permissions("0888").is_err());
        assert!(parse_unix_socket_permissions("abc").is_err());
        assert!(parse_unix_socket_permissions("").is_err());
    }

    #[test]
    fn test_parse_out_of_range() {
        // 0o1000 は 0777 を超える
        assert!(parse_unix_socket_permissions("1000").is_err());
    }
}

/// Serverヘッダーのデフォルト値
fn default_server_header_value() -> String {
    "veil".to_string()
}

#[derive(Deserialize)]
pub struct TlsConfigSection {
    /// 証明書のパス。`[tls]` 自体を省略した場合・空文字の場合は「TLS 無し」。
    /// TLS 無しが許されるのは平文専用サーバ構成（`plaintext_only_server`）だけ。
    #[serde(default)]
    pub cert_path: String,
    #[serde(default)]
    pub key_path: String,
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
    pub ktls_enabled: bool,
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
    pub ktls_fallback_enabled: bool,
    /// kTLS有効時にTCP_CORKを使用するかどうか
    ///
    /// TCP_CORKはkTLS設定中に小さなTCPパケットの送信を遅延し、
    /// パケット結合により効率的なネットワーク転送を実現します。
    ///
    /// - true: TCP_CORK有効（デフォルト、推奨）
    /// - false: TCP_CORK無効（特定の低遅延要件がある場合）
    #[serde(default = "default_tcp_cork")]
    pub tcp_cork_enabled: bool,
    /// 利用を許可する TLS 暗号スイート（nginx の `ssl_ciphers` 相当、F-50）
    ///
    /// **記載順 = サーバ優先度順**（rustls はサーバ選好でネゴシエートする）。
    /// 未指定（空）の場合は rustls 既定（DEFAULT_CIPHER_SUITES）を使用する。
    /// 不正なスイート名は起動時（設定検証時）にエラーとなる。
    ///
    /// 指定可能な名前（aws-lc-rs プロバイダ）:
    /// - TLS 1.3: `TLS13_AES_256_GCM_SHA384`, `TLS13_AES_128_GCM_SHA256`,
    ///   `TLS13_CHACHA20_POLY1305_SHA256`
    /// - TLS 1.2: `TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384`,
    ///   `TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256`,
    ///   `TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256`,
    ///   `TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384`,
    ///   `TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256`,
    ///   `TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256`
    ///
    /// 注意: kTLS は AES-GCM 系のみ対応。kTLS 有効時に CHACHA20 系を含めると、
    /// そのスイートでネゴシエートした接続は kTLS へオフロードされず rustls で
    /// 処理される（`ktls_fallback_enabled = false` の場合は接続拒否）。
    #[serde(default)]
    pub cipher_suites: Vec<String>,
    /// 証明書ファイルの変更を自動検知してリロードするか（F-03）
    /// デフォルト: false
    #[serde(default)]
    pub auto_reload: bool,
    /// 証明書変更チェックの間隔（秒、F-03）
    /// デフォルト: 60
    #[serde(default = "default_tls_reload_interval")]
    pub reload_interval_secs: u64,
    /// TLS セッション再開の方式（F-183）。
    ///
    /// - `"tickets"`（既定）: ステートレスなセッションチケット。鍵はプロセス共有で定期的に
    ///   ローテーションされる。TLS 1.3 の再開はサーバ側の状態を持たない（ワーカー間の共有
    ///   ロック無し）。チケット非対応の TLS 1.2 クライアント（session ID）向けに
    ///   `session_cache_size` 件のキャッシュも併用する。
    /// - `"cache"`: サーバ側のセッションキャッシュ（`session_cache_size` 件、全ワーカー共有）。
    /// - `"off"`: セッション再開をしない（チケットを送らない）。
    #[serde(default)]
    pub session_resumption: SessionResumption,
    /// セッションキャッシュの件数（`"cache"`、および `"tickets"` の TLS 1.2 session ID 用。F-183）。既定 4096。
    #[serde(default = "default_session_cache_size")]
    pub session_cache_size: usize,
    /// TLS 1.3 のハンドシェイクごとに送るセッションチケット数（F-183）。既定 1。
    /// `session_resumption = "off"` のときは 0 になる。
    #[serde(default = "default_tls13_tickets")]
    pub tls13_tickets: usize,
    /// クライアント証明書の要求（mTLS の受け側、F-217）。
    ///
    /// - `"none"`（既定）: 要求しない
    /// - `"optional"`: 要求するが、無くても受ける（あれば `client_ca` で検証する）
    /// - `"require"`: 必須（無い・検証できないクライアントはハンドシェイクで拒否）
    #[serde(default)]
    pub client_auth: ClientAuthMode,
    /// クライアント証明書の信頼アンカー（PEM、`client_auth` が `none` 以外のとき必須。F-217）。
    #[serde(default)]
    pub client_ca: String,
    /// 受けるクライアント証明書の SAN（DNS 名・URI・IP のどれかが一致すれば受ける。空なら `client_ca` が
    /// 発行したもの全部）。ハンドシェイクで 1 回だけ照合する（F-220 の ID による認可）。
    #[serde(default)]
    pub client_allowed_sans: Vec<String>,
}

/// クライアント証明書の要求（`[tls] client_auth`、F-217）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientAuthMode {
    #[default]
    None,
    Optional,
    Require,
}

/// TLS セッション再開の方式（`[tls] session_resumption`、F-183）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionResumption {
    #[default]
    Tickets,
    Cache,
    Off,
}

fn default_session_cache_size() -> usize {
    4096
}

fn default_tls13_tickets() -> usize {
    1
}

/// 証明書リロードチェック間隔のデフォルト値（秒）
fn default_tls_reload_interval() -> u64 {
    60
}

/// kTLSフォールバックのデフォルト値（true = フォールバック有効）
fn default_ktls_fallback() -> bool {
    true
}

/// TCP_CORKのデフォルト値（true = 有効）
fn default_tcp_cork() -> bool {
    true
}

/// パフォーマンス設定
#[derive(Deserialize, Clone, Default)]
pub struct PerformanceConfigSection {
    /// SO_REUSEPORTの振り分け方式
    /// - "kernel": カーネルデフォルト（3元タプルハッシュ）
    /// - "cbpf": フローハッシュ（4タプル）ベースのCBPF振り分け（Linux 4.6+必須）
    #[serde(default)]
    pub reuseport_balancing: ReuseportBalancing,
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
    pub huge_pages_enabled: bool,

    // ====================
    // Viaヘッダー設定
    // ====================
    /// Viaヘッダーを追加するかどうか
    ///
    /// RFC 7230 Section 5.7.1 に従い、プロキシ経由のリクエスト/レスポンスに
    /// Via ヘッダーを追加します。
    ///
    /// 形式: Via: 1.1 `<hostname>`
    ///
    /// - true: Viaヘッダーを追加
    /// - false: Viaヘッダーを追加しない（デフォルト）
    #[serde(default)]
    pub via_header_enabled: bool,

    /// Viaヘッダーに使用するホスト名
    ///
    /// via_header_enabled = true 時に使用します。
    /// 未指定の場合はシステムホスト名または "veil" を使用します。
    #[serde(default)]
    pub via_header_hostname: Option<String>,

    // ====================
    // sendfile/splice チャンクサイズ設定
    // ====================
    /// チャンクサイズ調整モード
    ///
    /// - "dynamic": ファイルサイズに応じて動的にチャンクサイズを決定（デフォルト）
    ///   - 0-64KB: 64KB
    ///   - 64KB-1MB: 256KB
    ///   - 1MB超: 1MB
    /// - "manual": 固定チャンクサイズを使用
    #[serde(default = "default_chunk_size_mode")]
    pub chunk_size_mode: ChunkSizeMode,

    /// 手動チャンクサイズ（バイト）
    ///
    /// chunk_size_mode = "manual" 時に使用します。
    /// デフォルト: 1048576 (1MB)
    #[serde(default = "default_manual_chunk_size")]
    pub manual_chunk_size: usize,

    // ====================
    // パイプ割当設定
    // ====================
    /// ストリーム毎にパイプを割り当てるかどうか
    ///
    /// kTLS splice 使用時のパイプバッファ管理方式を設定します。
    ///
    /// - false: スレッドローカルパイプを再利用（デフォルト、メモリ効率重視）
    /// - true: ストリーム毎にパイプを割り当て（高並行性環境向け）
    ///
    /// 高並行性環境（同時接続数1000+）ではtrueを推奨します。
    #[serde(default)]
    pub per_stream_pipe_enabled: bool,

    /// HTTP/1.1 のキープアライブで、応答を返した直後に次の要求を蓄積バッファへ直接読みにいく時間（ミリ秒、F-205）。
    ///
    /// 未指定は 20。この間に届けば「待つ → 読む」の 2 段を io_uring の RECV 1 回にでき、届かなければ読みを
    /// 取り消してバッファを返し、従来どおりバッファを持たない待ち方に戻る（B-101）。0 で無効（常に従来の待ち方）。
    /// 平文の接続・io_uring だけが対象（reactor は先に待つ）。
    #[serde(default)]
    pub keepalive_direct_read_ms: Option<u64>,

    // ====================
    // OpenFileCache設定
    // ====================
    /// OpenFileCache（ファイルメタデータキャッシュ）のグローバルデフォルト設定
    ///
    /// 効果:
    ///   - canonicalize、metadata、mime_guessのシステムコールをキャッシュ
    ///   - 1リクエストあたり5〜6回のシステムコールを2回に削減（キャッシュヒット時）
    ///   - パフォーマンス向上: 60〜67%のシステムコール削減
    ///
    /// 注意事項:
    ///   - ファイル変更の検出が最大60秒（デフォルト）遅延する可能性
    ///   - シンボリックリンク変更の検出が遅延する可能性
    ///   - 静的ファイル配信に最適（動的に変更されるファイルには不向き）
    ///
    /// ルーティングごとの設定:
    ///   - 各ルーティング（`[[route]]`）で`open_file_cache`セクションを指定可能
    ///   - ルーティング設定がない場合は、このグローバル設定が使用される
    ///
    /// デフォルト: false（無効）
    #[serde(default)]
    pub open_file_cache_enabled: bool,

    /// OpenFileCacheの有効期間（秒、グローバルデフォルト）
    /// キャッシュされたファイル情報が有効とみなされる期間
    /// デフォルト: 60秒
    #[serde(default = "default_open_file_cache_valid_duration")]
    pub open_file_cache_valid_duration_secs: u64,

    /// OpenFileCacheの最大エントリ数（グローバルデフォルト）
    /// キャッシュに保持する最大ファイル情報数
    /// デフォルト: 10000
    #[serde(default = "default_open_file_cache_max_entries")]
    pub open_file_cache_max_entries: usize,

    /// OpenFileCache がディスクの変更を通知で追従するか（グローバルデフォルト、F-272）
    /// ルートの `[route.open_file_cache] watch` が無いルートに効く。
    /// デフォルト: false（有効期限だけで追従する）
    #[serde(default)]
    pub open_file_cache_watch: bool,
}

impl PerformanceConfigSection {
    /// `keepalive_direct_read_ms`（未指定は 20ms。F-205）。
    pub fn keepalive_direct_read(&self) -> Duration {
        Duration::from_millis(self.keepalive_direct_read_ms.unwrap_or(20))
    }
}

fn default_open_file_cache_valid_duration() -> u64 {
    60
}

fn default_open_file_cache_max_entries() -> usize {
    10000
}

// ====================
// ロードバランシングのテスト（F-19）
// ====================
#[cfg(test)]
mod blocklist_tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn test_cidr_contains_addr_ipv4() {
        let cidr = CidrRange::parse("192.168.1.0/24").unwrap();
        assert!(cidr.contains_addr(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50))));
        assert!(cidr.contains_addr(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 255))));
        assert!(!cidr.contains_addr(IpAddr::V4(Ipv4Addr::new(192, 168, 2, 1))));
        // IPv6 は IPv4 CIDR にマッチしない
        assert!(!cidr.contains_addr(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn test_cidr_contains_addr_single_ipv4() {
        let cidr = CidrRange::parse("10.0.0.5").unwrap();
        assert!(cidr.contains_addr(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))));
        assert!(!cidr.contains_addr(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 6))));
    }

    #[test]
    fn test_cidr_contains_addr_ipv6() {
        let cidr = CidrRange::parse("2001:db8::/32").unwrap();
        assert!(cidr.contains_addr("2001:db8::1".parse::<IpAddr>().unwrap()));
        assert!(!cidr.contains_addr("2001:db9::1".parse::<IpAddr>().unwrap()));
        // IPv4 は IPv6 CIDR にマッチしない
        assert!(!cidr.contains_addr(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }

    #[test]
    fn test_global_blocklist_set_and_check() {
        // 空のときは常に false（accept ホットパスの早期 return）
        set_global_blocked_ips(&[]);
        assert!(!is_ip_blocked(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5))));

        set_global_blocked_ips(&["203.0.113.0/24".to_string(), "198.51.100.7".to_string()]);
        assert!(is_ip_blocked(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5))));
        assert!(is_ip_blocked(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7))));
        assert!(!is_ip_blocked(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 8))));
        assert!(!is_ip_blocked(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));

        // グローバル状態を空に戻す（他テストへの影響を避ける）
        set_global_blocked_ips(&[]);
    }
}

// ====================
// F-200: 待ち受け設定（listen / listen_cleartext）のテスト
// ====================

#[cfg(test)]
mod f200_listener_tests {
    use super::*;

    fn check(toml_text: &str) -> Result<(), String> {
        let cfg: Config = toml::from_str(toml_text).map_err(|e| e.to_string())?;
        validate_listeners(&cfg).map_err(|e| e.to_string())
    }

    const TLS: &str = "[tls]\ncert_path = \"/c.pem\"\nkey_path = \"/k.pem\"\n";

    #[test]
    fn cleartext_defaults_are_all_true() {
        let cfg: Config =
            toml::from_str("[server]\nlisten_cleartext = \"127.0.0.1:80\"\n").unwrap();
        assert!(cfg.server.allow_http && cfg.server.allow_h2c && cfg.server.enable_secure_redirect);
        assert!(!cfg.http3.enabled);
    }

    #[test]
    fn needs_at_least_one_listener() {
        assert!(check("[server]\n").unwrap_err().contains("no listener"));
    }

    #[test]
    fn tls_listener_needs_certificate() {
        let e = check("[server]\nlisten = \"127.0.0.1:443\"\n").unwrap_err();
        assert!(e.contains("TLS certificate is required"), "{}", e);
        check(&format!("[server]\nlisten = \"127.0.0.1:443\"\n{}", TLS)).unwrap();
    }

    #[test]
    fn plaintext_only_server_needs_no_tls_but_no_redirect() {
        // 既定の enable_secure_redirect = true はリダイレクト先が無いのでエラー
        let e = check("[server]\nlisten_cleartext = \"127.0.0.1:80\"\n").unwrap_err();
        assert!(e.contains("enable_secure_redirect"), "{}", e);
        check("[server]\nlisten_cleartext = \"127.0.0.1:80\"\nenable_secure_redirect = false\n")
            .unwrap();
    }

    #[test]
    fn listeners_must_differ_and_accept_something() {
        let both = format!(
            "[server]\nlisten = \"127.0.0.1:443\"\nlisten_cleartext = \"127.0.0.1:443\"\n{}",
            TLS
        );
        assert!(check(&both).unwrap_err().contains("must differ"));
        let none = "[server]\nlisten_cleartext = \"127.0.0.1:80\"\nallow_http = false\nallow_h2c = false\nenable_secure_redirect = false\n";
        assert!(check(none).unwrap_err().contains("accepts nothing"));
    }

    #[test]
    fn http3_needs_tls_listener() {
        let e = check(
            "[server]\nlisten_cleartext = \"127.0.0.1:80\"\nenable_secure_redirect = false\n[http3]\nenabled = true\n",
        )
        .unwrap_err();
        assert!(e.contains("[http3].enabled"), "{}", e);
        check(&format!(
            "[server]\nlisten = \"127.0.0.1:443\"\n[http3]\nenabled = true\n{}",
            TLS
        ))
        .unwrap();
    }

    #[test]
    fn removed_keys_point_to_the_replacement() {
        for (key, hint) in [
            ("http = \"0.0.0.0:80\"", "listen_cleartext"),
            ("tls_only = false", "listen_cleartext"),
            ("h2c_enabled = true", "allow_h2c"),
            ("h2c_listen = \"0.0.0.0:8080\"", "listen_cleartext"),
            ("h2c_http1 = true", "allow_http"),
            ("http3_enabled = true", "[http3].enabled"),
        ] {
            let e = check(&format!(
                "[server]\nlisten = \"127.0.0.1:443\"\n{}\n{}",
                key, TLS
            ))
            .unwrap_err();
            assert!(
                e.contains("was removed") && e.contains(hint),
                "{} -> {}",
                key,
                e
            );
        }
    }

    #[test]
    fn redirect_url_uses_listen_port() {
        assert_eq!(
            https_redirect_url("a.example:80", "/x?y", 443),
            "https://a.example/x?y"
        );
        assert_eq!(https_redirect_url("", "/", 443), "https://localhost/");
        assert_eq!(https_redirect_url("[::1]:80", "/", 443), "https://[::1]/");
        // F-260: リダイレクト先のリスナーのポート
        assert_eq!(
            https_redirect_url("a.example:8080", "/", 8443),
            "https://a.example:8443/"
        );
    }
}

#[cfg(test)]
mod load_balancing_tests {
    use super::*;

    fn entry(url: &str, weight: u32) -> UpstreamServerEntry {
        UpstreamServerEntry {
            url: url.to_string(),
            sni_name: None,
            use_h2c: false,
            http2: None,
            weight,
        }
    }

    #[test]
    fn weighted_distribution_roughly_2_to_1() {
        let entries = vec![
            entry("http://10.0.0.1:80", 2),
            entry("http://10.0.0.2:80", 1),
        ];
        let group = UpstreamGroup::new(
            "wrr".into(),
            entries,
            LoadBalanceAlgorithm::Weighted,
            None,
            false,
        )
        .unwrap();

        let mut counts = [0usize; 2];
        for _ in 0..3000 {
            let s = group.select("1.2.3.4").unwrap();
            if s.target.host == "10.0.0.1" {
                counts[0] += 1;
            } else {
                counts[1] += 1;
            }
        }
        // 重み 2:1 なので server1 はおよそ server2 の2倍
        let ratio = counts[0] as f64 / counts[1] as f64;
        assert!(
            ratio > 1.7 && ratio < 2.3,
            "weighted ratio out of range: {} (counts={:?})",
            ratio,
            counts
        );
    }

    #[test]
    fn consistent_hash_same_key_same_server() {
        let entries = vec![
            entry("http://10.0.0.1:80", 1),
            entry("http://10.0.0.2:80", 1),
            entry("http://10.0.0.3:80", 1),
        ];
        let group = UpstreamGroup::new(
            "ch".into(),
            entries,
            LoadBalanceAlgorithm::ConsistentHash {
                hash_key: HashKey::Ip,
            },
            None,
            false,
        )
        .unwrap();

        // 同じキーは常に同じサーバーへ
        let first = group.select("192.168.1.50").unwrap().target.host.clone();
        for _ in 0..100 {
            let host = group.select("192.168.1.50").unwrap().target.host.clone();
            assert_eq!(host, first, "consistent hash not stable");
        }

        // 別のキーでも安定していること
        let second = group.select("8.8.8.8").unwrap().target.host.clone();
        for _ in 0..100 {
            let host = group.select("8.8.8.8").unwrap().target.host.clone();
            assert_eq!(host, second);
        }
    }

    #[test]
    fn consistent_hash_distributes_keys() {
        let entries = vec![
            entry("http://10.0.0.1:80", 1),
            entry("http://10.0.0.2:80", 1),
            entry("http://10.0.0.3:80", 1),
        ];
        let group = UpstreamGroup::new(
            "ch".into(),
            entries,
            LoadBalanceAlgorithm::ConsistentHash {
                hash_key: HashKey::Ip,
            },
            None,
            false,
        )
        .unwrap();

        let mut seen = std::collections::HashSet::new();
        for i in 0..200 {
            let key = format!("10.20.30.{}", i % 256);
            let host = group.select(&key).unwrap().target.host.clone();
            seen.insert(host);
        }
        // 150 vnodes/サーバーで十分に分散され、全サーバーが使われるはず
        assert_eq!(
            seen.len(),
            3,
            "keys not distributed across all servers: {:?}",
            seen
        );
    }

    #[test]
    fn hash_key_parse() {
        assert_eq!(HashKey::parse("ip").unwrap(), HashKey::Ip);
        assert_eq!(
            HashKey::parse("header:X-User-Id").unwrap(),
            HashKey::Header("X-User-Id".into())
        );
        assert_eq!(
            HashKey::parse("cookie:session_id").unwrap(),
            HashKey::Cookie("session_id".into())
        );
        assert!(HashKey::parse("garbage").is_err());
    }

    /// F-97: header キーの Consistent Hash が select_with_header_fn で安定
    #[test]
    fn consistent_hash_header_key_via_select_with_header_fn() {
        let entries = vec![
            entry("http://10.0.0.1:80", 1),
            entry("http://10.0.0.2:80", 1),
        ];
        let group = UpstreamGroup::new(
            "ch-hdr".into(),
            entries,
            LoadBalanceAlgorithm::ConsistentHash {
                hash_key: HashKey::Header("x-user-id".into()),
            },
            None,
            false,
        )
        .unwrap();

        let headers: Vec<(&[u8], &[u8])> = vec![(b"x-user-id", b"user-stable-1")];
        let first = group
            .select_with_header_fn("1.2.3.4", |name| {
                headers
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case(name))
                    .map(|(_, v)| *v)
            })
            .unwrap()
            .target
            .host
            .clone();
        for _ in 0..20 {
            let host = group
                .select_with_header_fn("9.9.9.9", |name| {
                    headers
                        .iter()
                        .find(|(n, _)| n.eq_ignore_ascii_case(name))
                        .map(|(_, v)| *v)
                })
                .unwrap()
                .target
                .host
                .clone();
            assert_eq!(
                host, first,
                "same header value must stick to same backend regardless of client IP"
            );
        }

        // Cookie キー
        assert_eq!(
            extract_cookie_value("a=1; session_id=abc; b=2", "session_id"),
            Some("abc")
        );
    }

    #[test]
    fn weighted_entry_default_weight_is_one() {
        let s = entry("http://x:80", 1);
        assert_eq!(s.weight, 1);
    }

    #[test]
    fn round_robin_still_cycles() {
        let entries = vec![
            entry("http://10.0.0.1:80", 1),
            entry("http://10.0.0.2:80", 1),
        ];
        let group = UpstreamGroup::new(
            "rr".into(),
            entries,
            LoadBalanceAlgorithm::RoundRobin,
            None,
            false,
        )
        .unwrap();
        let a = group.select("x").unwrap().target.host.clone();
        let b = group.select("x").unwrap().target.host.clone();
        assert_ne!(a, b, "round robin should alternate");
    }
}

// ====================
// 管理 API 設定のテスト（F-20）
// ====================
#[cfg(all(test, feature = "admin"))]
mod admin_config_tests {
    use super::*;

    #[test]
    fn admin_auth_disabled_rejects() {
        let cfg = AdminConfig::default();
        assert!(!cfg.check_auth(Some("Bearer x")));
    }

    #[test]
    fn admin_auth_bearer_and_raw() {
        let cfg = AdminConfig {
            enabled: true,
            path_prefix: "/__admin".into(),
            secret: "topsecret".into(),
            allowed_ips: Vec::new(),
            cache_purge_prefix: "/__admin/cache/purge".into(),
        };
        assert!(cfg.check_auth(Some("Bearer topsecret")));
        assert!(cfg.check_auth(Some("topsecret")));
        assert!(!cfg.check_auth(Some("wrong")));
        assert!(!cfg.check_auth(None));
    }

    #[test]
    fn admin_empty_secret_rejects() {
        let cfg = AdminConfig {
            enabled: true,
            path_prefix: "/__admin".into(),
            secret: String::new(),
            allowed_ips: Vec::new(),
            cache_purge_prefix: "/__admin/cache/purge".into(),
        };
        assert!(!cfg.check_auth(Some("Bearer ")));
    }

    #[test]
    fn admin_ip_allowed_empty_allows_all() {
        let cfg = AdminConfig {
            enabled: true,
            path_prefix: "/__admin".into(),
            secret: "s".into(),
            allowed_ips: Vec::new(),
            cache_purge_prefix: "/__admin/cache/purge".into(),
        };
        assert!(cfg.is_ip_allowed("1.2.3.4"));
        assert!(cfg.is_ip_allowed("::1"));
    }

    #[test]
    fn admin_ip_allowed_single_ip() {
        let cfg = AdminConfig {
            enabled: true,
            path_prefix: "/__admin".into(),
            secret: "s".into(),
            allowed_ips: vec!["127.0.0.1".into()],
            cache_purge_prefix: "/__admin/cache/purge".into(),
        };
        assert!(cfg.is_ip_allowed("127.0.0.1"));
        assert!(!cfg.is_ip_allowed("192.168.0.1"));
    }

    #[test]
    fn admin_ip_allowed_cidr() {
        let cfg = AdminConfig {
            enabled: true,
            path_prefix: "/__admin".into(),
            secret: "s".into(),
            allowed_ips: vec!["10.0.0.0/8".into()],
            cache_purge_prefix: "/__admin/cache/purge".into(),
        };
        assert!(cfg.is_ip_allowed("10.1.2.3"));
        assert!(cfg.is_ip_allowed("10.255.255.255"));
        assert!(!cfg.is_ip_allowed("192.168.0.1"));
    }

    #[test]
    fn admin_ip_allowed_ipv6() {
        let cfg = AdminConfig {
            enabled: true,
            path_prefix: "/__admin".into(),
            secret: "s".into(),
            allowed_ips: vec!["::1".into(), "fe80::/10".into()],
            cache_purge_prefix: "/__admin/cache/purge".into(),
        };
        assert!(cfg.is_ip_allowed("::1"));
        assert!(cfg.is_ip_allowed("fe80::1"));
        assert!(!cfg.is_ip_allowed("2001:db8::1"));
    }
}

// ====================
// F-06: サーキットブレーカーと UpstreamGroup 統合テスト
// ====================
#[cfg(test)]
mod circuit_breaker_upstream_tests {
    use super::*;
    use crate::resilience::CircuitBreaker;

    fn make_entry(url: &str) -> UpstreamServerEntry {
        UpstreamServerEntry {
            url: url.to_string(),
            sni_name: None,
            use_h2c: false,
            http2: None,
            weight: 1,
        }
    }

    fn trip_config() -> super::CircuitBreakerConfig {
        super::CircuitBreakerConfig {
            enabled: true,
            failure_threshold: 2,
            failure_window_secs: 60,
            open_duration_secs: 300,
            half_open_probes: 1,
            success_threshold: 1,
            trip_on_timeout: true,
        }
    }

    /// CBがOpenのサーバーはselect()でスキップされる（2台構成の場合）
    #[test]
    fn tripped_circuit_breaker_skips_server_with_two_servers() {
        let entries = vec![
            make_entry("http://10.0.0.1:80"),
            make_entry("http://10.0.0.2:80"),
        ];
        let mut group = UpstreamGroup::new(
            "cb-test".into(),
            entries,
            LoadBalanceAlgorithm::RoundRobin,
            None,
            false,
        )
        .unwrap();

        // サーバー0にCBを付与してトリップさせる
        let cb = CircuitBreaker::new(trip_config());
        cb.record_failure();
        cb.record_failure();
        assert!(cb.is_open(), "CB should be open after exceeding threshold");
        group.servers[0] = group.servers[0].clone().with_circuit_breaker(Some(cb));

        // 50回 select() して、トリップしたサーバー0は選ばれないことを確認
        // （2台構成なので candidates() の healthy fallback は不要）
        for _ in 0..50 {
            let s = group.select("1.2.3.4");
            if let Some(s) = s {
                assert_ne!(
                    s.target.host, "10.0.0.1",
                    "Tripped server (10.0.0.1) should not be selected when healthy fallback exists"
                );
            }
        }
    }

    /// 全サーバーのCBがOpenの場合は healthy fallback が動作する
    ///
    /// candidates() の設計: avail が空なら healthy なサーバーにフォールバック（完全停止回避）。
    /// よって1台構成でCBがOpenでも、サーバー自体は healthy なら選択される。
    #[test]
    fn all_servers_tripped_falls_back_to_healthy() {
        let entries = vec![make_entry("http://10.0.0.1:80")];
        let mut group = UpstreamGroup::new(
            "cb-all-tripped".into(),
            entries,
            LoadBalanceAlgorithm::RoundRobin,
            None,
            false,
        )
        .unwrap();

        let cb = CircuitBreaker::new(trip_config());
        cb.record_failure();
        cb.record_failure();
        assert!(cb.is_open(), "CB should be open");
        group.servers[0] = group.servers[0].clone().with_circuit_breaker(Some(cb));

        // 設計上: 全 CB が Open でもサーバー自体が healthy なら fallback で選択される
        // （完全停止（None）を避けるための安全策）
        let result = group.select("1.2.3.4");
        // fallback が動作するため Some が返る（設計通り）
        assert!(
            result.is_some(),
            "Should fallback to healthy server even if all CBs are open (by design)"
        );
    }

    /// record_outcome で失敗を記録すると CB の状態が更新される
    #[test]
    fn record_outcome_updates_circuit_breaker_state() {
        let entries = vec![
            make_entry("http://10.0.0.1:80"),
            make_entry("http://10.0.0.2:80"),
        ];
        let mut group = UpstreamGroup::new(
            "cb-outcome".into(),
            entries,
            LoadBalanceAlgorithm::RoundRobin,
            None,
            false,
        )
        .unwrap();

        let cb = CircuitBreaker::new(trip_config());
        group.servers[0] = group.servers[0].clone().with_circuit_breaker(Some(cb));

        // トリップ前は サーバー0 が選ばれることがある
        let before_trip = (0..10)
            .filter_map(|_| group.select("1.1.1.1"))
            .any(|s| s.target.host == "10.0.0.1");
        assert!(before_trip, "Server 0 should be reachable before tripping");

        // 失敗を記録してトリップ閾値（2回）に達する
        group.record_outcome(0, false, 100);
        group.record_outcome(0, false, 100);

        // トリップ後は2台のうちサーバー1のみが選ばれるはず
        // （サーバー0は avail から除かれ、candidates が [server1] になる）
        let after_trip_server0 = (0..20)
            .filter_map(|_| group.select("1.1.1.1"))
            .filter(|s| s.target.host == "10.0.0.1")
            .count();
        assert_eq!(
            after_trip_server0, 0,
            "Tripped server (10.0.0.1) should not be selected when alternative exists"
        );
    }

    /// スライディングウィンドウ内の失敗率が閾値未満なら CB は開かない
    #[test]
    fn below_threshold_does_not_trip() {
        let entries = vec![make_entry("http://10.0.0.1:80")];
        let mut group = UpstreamGroup::new(
            "cb-below".into(),
            entries,
            LoadBalanceAlgorithm::RoundRobin,
            None,
            false,
        )
        .unwrap();

        let cb = CircuitBreaker::new(trip_config());
        group.servers[0] = group.servers[0].clone().with_circuit_breaker(Some(cb));

        // 閾値-1回の失敗では開かない
        group.record_outcome(0, false, 10);

        assert!(
            group.select("1.1.1.1").is_some(),
            "Should still select server when below failure threshold"
        );
    }
}

// ====================
// F-19: 追加ロードバランシング統合テスト
// ====================
#[cfg(test)]
mod advanced_lb_integration_tests {
    use super::*;

    fn w_entry(url: &str, weight: u32) -> UpstreamServerEntry {
        UpstreamServerEntry {
            url: url.to_string(),
            sni_name: None,
            use_h2c: false,
            http2: None,
            weight,
        }
    }

    /// Weighted RR: weight=0 は weight=1 として扱われる（最小重みは1）
    ///
    /// 設計上、`weight.max(1)` により 0 は 1 に切り上げられる。
    #[test]
    fn zero_weight_treated_as_one() {
        let entries = vec![
            w_entry("http://10.0.0.1:80", 100), // 重み 100
            w_entry("http://10.0.0.2:80", 0),   // weight=0 → 内部で 1 として扱われる
        ];
        let group = UpstreamGroup::new(
            "wt-zero".into(),
            entries,
            LoadBalanceAlgorithm::Weighted,
            None,
            false,
        )
        .unwrap();

        let mut count1 = 0usize;
        let mut count2 = 0usize;
        for _ in 0..200 {
            if let Some(s) = group.select("1.2.3.4") {
                if s.target.host == "10.0.0.1" {
                    count1 += 1;
                } else {
                    count2 += 1;
                }
            }
        }
        // weight=100 vs weight=0(→1) → server1 が圧倒的多数
        assert!(
            count1 > count2 * 10,
            "weight=100 server should dominate weight=0(→1): {count1} vs {count2}"
        );
    }

    /// Consistent Hash: 同一キーは unhealthy サーバーが除かれても次のサーバーに転送される
    #[test]
    fn consistent_hash_falls_back_on_unhealthy() {
        let entries = vec![
            w_entry("http://10.0.0.1:80", 1),
            w_entry("http://10.0.0.2:80", 1),
            w_entry("http://10.0.0.3:80", 1),
        ];
        let group = UpstreamGroup::new(
            "ch-fallback".into(),
            entries,
            LoadBalanceAlgorithm::ConsistentHash {
                hash_key: HashKey::Ip,
            },
            None,
            false,
        )
        .unwrap();

        // 正常状態で選択できること
        let first = group.select("192.168.1.100");
        assert!(first.is_some(), "Should select a server in normal state");

        // 1台をunhealthyにしても選択できること
        group.servers[0]
            .healthy
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let after = group.select("192.168.1.100");
        assert!(
            after.is_some(),
            "Should still select a server with one unhealthy"
        );
    }

    /// IpHash: 同じIPは常に同じサーバーへ（既存動作の確認）
    #[test]
    fn ip_hash_consistent_routing() {
        let entries = vec![
            w_entry("http://10.0.0.1:80", 1),
            w_entry("http://10.0.0.2:80", 1),
            w_entry("http://10.0.0.3:80", 1),
        ];
        let group = UpstreamGroup::new(
            "iphash".into(),
            entries,
            LoadBalanceAlgorithm::IpHash,
            None,
            false,
        )
        .unwrap();

        let ip = "203.0.113.42";
        let first = group.select(ip).unwrap().target.host.clone();
        for _ in 0..20 {
            let host = group.select(ip).unwrap().target.host.clone();
            assert_eq!(
                host, first,
                "IP hash should always route same IP to same server"
            );
        }
    }
}

// ====================
// F-22: HealthCheckType / HealthCheckConfig serde テスト
// ====================
#[cfg(test)]
mod health_check_type_tests {
    use super::*;
    use serde::Deserialize;

    // TOML はトップレベルにスカラー値を置けないため、ラッパーを使う
    #[derive(Deserialize)]
    struct Wrapper {
        t: HealthCheckType,
    }

    #[test]
    fn test_health_check_type_deser_http() {
        let w: Wrapper = toml::from_str("t = \"http\"").unwrap();
        assert_eq!(w.t, HealthCheckType::Http);
    }

    #[test]
    fn test_health_check_type_deser_tcp() {
        let w: Wrapper = toml::from_str("t = \"tcp\"").unwrap();
        assert_eq!(w.t, HealthCheckType::Tcp);
    }

    #[test]
    fn test_health_check_type_deser_grpc() {
        let w: Wrapper = toml::from_str("t = \"grpc\"").unwrap();
        assert_eq!(w.t, HealthCheckType::Grpc);
    }

    #[test]
    fn test_health_check_type_default() {
        assert_eq!(HealthCheckType::default(), HealthCheckType::Http);
    }

    #[test]
    fn test_health_check_config_deser_tcp() {
        let toml = r#"
check_type = "tcp"
interval_secs = 15
timeout_secs = 3
"#;
        let cfg: HealthCheckConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.check_type, HealthCheckType::Tcp);
        assert_eq!(cfg.interval_secs, 15);
        assert_eq!(cfg.timeout_secs, 3);
    }

    #[test]
    fn test_health_check_config_deser_grpc() {
        let toml = r#"
check_type = "grpc"
path = "my.service.Health"
timeout_secs = 5
use_tls = true
verify_cert = false
"#;
        let cfg: HealthCheckConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.check_type, HealthCheckType::Grpc);
        assert_eq!(cfg.path, "my.service.Health");
        assert!(cfg.use_tls);
        assert!(!cfg.verify_cert);
    }

    #[test]
    fn test_health_check_config_deser_defaults() {
        // check_type を省略すると Http になる
        let cfg: HealthCheckConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.check_type, HealthCheckType::Http);
    }
}

// ====================
// F-18: L4ListenerConfig serde テスト
// ====================
#[cfg(all(test, feature = "l4-proxy"))]
mod l4_config_tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct TlsWrapper {
        t: L4TlsMode,
    }
    #[derive(Deserialize)]
    struct LbWrapper {
        t: L4LbAlgorithm,
    }

    #[test]
    fn test_l4_tls_mode_deser() {
        let none: TlsWrapper = toml::from_str("t = \"none\"").unwrap();
        let passthrough: TlsWrapper = toml::from_str("t = \"passthrough\"").unwrap();
        let terminate: TlsWrapper = toml::from_str("t = \"terminate\"").unwrap();
        assert_eq!(none.t, L4TlsMode::None);
        assert_eq!(passthrough.t, L4TlsMode::Passthrough);
        assert_eq!(terminate.t, L4TlsMode::Terminate);
    }

    #[test]
    fn test_l4_lb_algorithm_deser() {
        let rr: LbWrapper = toml::from_str("t = \"round_robin\"").unwrap();
        let lc: LbWrapper = toml::from_str("t = \"least_conn\"").unwrap();
        assert_eq!(rr.t, L4LbAlgorithm::RoundRobin);
        assert_eq!(lc.t, L4LbAlgorithm::LeastConn);
    }

    #[test]
    fn test_l4_listener_config_full_deser() {
        let toml = r#"
name = "db-proxy"
listen = "0.0.0.0:5432"
lb = "least_conn"
tls = "passthrough"
max_connections = 200
connect_timeout_secs = 5

[[upstreams]]
addr = "10.0.0.1:5432"
weight = 2

[[upstreams]]
addr = "10.0.0.2:5432"
weight = 1
"#;
        let cfg: L4ListenerConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.name, "db-proxy");
        assert_eq!(cfg.listen, "0.0.0.0:5432");
        assert_eq!(cfg.lb, L4LbAlgorithm::LeastConn);
        assert_eq!(cfg.tls, L4TlsMode::Passthrough);
        assert_eq!(cfg.max_connections, 200);
        assert_eq!(cfg.connect_timeout_secs, 5);
        assert_eq!(cfg.upstreams.len(), 2);
        assert_eq!(cfg.upstreams[0].addr, "10.0.0.1:5432");
        assert_eq!(cfg.upstreams[0].weight, 2);
        assert_eq!(cfg.upstreams[1].weight, 1);
    }

    #[test]
    fn test_l4_listener_config_defaults() {
        // lb, tls, max_connections, connect_timeout_secs, idle_timeout_secs を省略したときデフォルト値になる
        let toml = r#"
name = "minimal"
listen = "0.0.0.0:9000"

[[upstreams]]
addr = "127.0.0.1:9001"
"#;
        let cfg: L4ListenerConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.protocol, L4Protocol::Tcp);
        assert_eq!(cfg.lb, L4LbAlgorithm::RoundRobin);
        assert_eq!(cfg.tls, L4TlsMode::None);
        assert_eq!(cfg.max_connections, 0);
        assert_eq!(cfg.connect_timeout_secs, 10);
        assert_eq!(cfg.idle_timeout_secs, 600);
        assert_eq!(cfg.upstreams[0].weight, 1);
    }

    // ====================
    // F-124: L4Protocol（UDP 対応）
    // ====================

    #[test]
    fn test_l4_protocol_default_is_tcp() {
        assert_eq!(L4Protocol::default(), L4Protocol::Tcp);
    }

    #[test]
    fn test_l4_protocol_deser_tcp_and_udp() {
        #[derive(Deserialize)]
        struct ProtoWrapper {
            t: L4Protocol,
        }
        let tcp: ProtoWrapper = toml::from_str("t = \"tcp\"").unwrap();
        let udp: ProtoWrapper = toml::from_str("t = \"udp\"").unwrap();
        assert_eq!(tcp.t, L4Protocol::Tcp);
        assert_eq!(udp.t, L4Protocol::Udp);
    }

    #[test]
    fn test_l4_listener_config_protocol_udp_deser() {
        let toml = r#"
name = "dns-proxy"
listen = "0.0.0.0:53"
protocol = "udp"

[[upstreams]]
addr = "10.0.0.1:53"
"#;
        let cfg: L4ListenerConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.protocol, L4Protocol::Udp);
    }

    #[test]
    fn test_l4_listener_config_protocol_omitted_is_tcp() {
        // 既存 TCP 構成の後方互換: protocol 省略時は Tcp のまま。
        let toml = r#"
name = "legacy"
listen = "0.0.0.0:9000"

[[upstreams]]
addr = "127.0.0.1:9001"
"#;
        let cfg: L4ListenerConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.protocol, L4Protocol::Tcp);
    }

    #[test]
    fn test_l4_listener_config_idle_timeout() {
        let toml = r#"
name = "fast-idle"
listen = "0.0.0.0:8000"
idle_timeout_secs = 30

[[upstreams]]
addr = "127.0.0.1:8001"
"#;
        let cfg: L4ListenerConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.idle_timeout_secs, 30);
    }

    #[test]
    fn test_l4_listener_config_with_health_check() {
        let toml = r#"
name = "mysql"
listen = "0.0.0.0:3306"

[[upstreams]]
addr = "10.0.0.1:3306"

[health_check]
check_type = "tcp"
interval_secs = 10
timeout_secs = 3
"#;
        let cfg: L4ListenerConfig = toml::from_str(toml).unwrap();
        let hc = cfg.health_check.unwrap();
        assert_eq!(hc.check_type, HealthCheckType::Tcp);
        assert_eq!(hc.interval_secs, 10);
        assert_eq!(hc.timeout_secs, 3);
    }
}

// ====================
// TLS 暗号スイート設定（F-50）のテスト
// ====================

#[cfg(test)]
mod cipher_suites_tests {
    // 理由付き allow: テストコードは同期 I/O・sleep を使用してよい（データプレーン非経由）。
    #![allow(clippy::disallowed_methods)]
    use super::*;

    #[test]
    fn resolve_known_suites_preserves_order() {
        let names = vec![
            "TLS13_AES_256_GCM_SHA384".to_string(),
            "TLS13_AES_128_GCM_SHA256".to_string(),
            "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384".to_string(),
        ];
        let suites = resolve_cipher_suites(&names).unwrap();
        assert_eq!(suites.len(), 3);
        // 設定順 = 優先度順が保持される
        for (name, suite) in names.iter().zip(suites.iter()) {
            assert_eq!(&format!("{:?}", suite.suite()), name);
        }
    }

    #[test]
    fn resolve_is_case_insensitive() {
        let names = vec!["tls13_aes_128_gcm_sha256".to_string()];
        let suites = resolve_cipher_suites(&names).unwrap();
        assert_eq!(
            format!("{:?}", suites[0].suite()),
            "TLS13_AES_128_GCM_SHA256"
        );
    }

    #[test]
    fn resolve_unknown_suite_is_error() {
        let names = vec!["TLS_RSA_WITH_RC4_128_MD5".to_string()];
        let err = resolve_cipher_suites(&names).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        // エラーメッセージに有効な候補一覧が含まれる
        assert!(err.to_string().contains("TLS13_AES_256_GCM_SHA384"));
    }

    #[test]
    fn resolve_duplicate_suite_is_error() {
        let names = vec![
            "TLS13_AES_128_GCM_SHA256".to_string(),
            "TLS13_AES_128_GCM_SHA256".to_string(),
        ];
        let err = resolve_cipher_suites(&names).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn resolve_empty_returns_empty() {
        assert!(resolve_cipher_suites(&[]).unwrap().is_empty());
    }

    #[test]
    fn tls_section_deserializes_cipher_suites() {
        let toml = r#"
cert_path = "/tmp/cert.pem"
key_path = "/tmp/key.pem"
cipher_suites = ["TLS13_AES_256_GCM_SHA384", "TLS13_AES_128_GCM_SHA256"]
"#;
        let section: TlsConfigSection = toml::from_str(toml).unwrap();
        assert_eq!(
            section.cipher_suites,
            vec!["TLS13_AES_256_GCM_SHA384", "TLS13_AES_128_GCM_SHA256"]
        );
    }

    #[test]
    fn tls_section_cipher_suites_defaults_to_empty() {
        let toml = r#"
cert_path = "/tmp/cert.pem"
key_path = "/tmp/key.pem"
"#;
        let section: TlsConfigSection = toml::from_str(toml).unwrap();
        assert!(section.cipher_suites.is_empty());
    }

    /// build_server_config_from_paths が cipher_suites を反映した ServerConfig を
    /// 構築できることを確認する（自己署名証明書使用）。
    #[test]
    fn build_server_config_with_custom_suites() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.signing_key.serialize_pem()).unwrap();

        let suites = vec!["TLS13_AES_256_GCM_SHA384".to_string()];
        let config =
            build_server_config_from_paths(&cert_path, &key_path, false, false, &suites).unwrap();
        // ServerConfig 構築成功 = スイート解決と provider 差し替えが機能
        assert!(Arc::strong_count(&config) >= 1);

        // 不正なスイート名はエラー
        let bad = vec!["NOT_A_SUITE".to_string()];
        assert!(build_server_config_from_paths(&cert_path, &key_path, false, false, &bad).is_err());
    }
}

// ====================
// 同梱 examples/config.toml の同期検証（F-51）
// ====================

#[cfg(test)]
mod shipped_config_tests {
    // 理由付き allow: テストコードは同期 I/O・sleep を使用してよい（データプレーン非経由）。
    #![allow(clippy::disallowed_methods)]
    use super::*;

    /// リポジトリ同梱の examples/config.toml が常にパース・バリデーション可能であることを保証する。
    ///
    /// config.rs の設定構造とドキュメント（examples/config.toml）の乖離を CI で検出する。
    /// cert_path / key_path のプレースホルダーのみ、実在する自己署名証明書に差し替える。
    #[test]
    fn shipped_config_toml_parses_and_validates() {
        // 同梱リファレンス examples/config.toml は開発／CI 環境（リポジトリツリー）でのみ存在する。
        // コンテナビルド等、ビルドコンテキストに含めない環境ではファイルが無いためスキップする
        // （コンテナ実行時の設定は docker/assets/conf.d/config.toml を使用する）。
        let shipped = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/config.toml");
        let content = match std::fs::read_to_string(&shipped) {
            Ok(c) => c,
            Err(_) => {
                eprintln!(
                    "skip shipped_config_toml_parses_and_validates: {} が存在しない環境のためスキップ",
                    shipped.display()
                );
                return;
            }
        };

        // プレースホルダー証明書を実在ファイルに差し替え
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.signing_key.serialize_pem()).unwrap();

        // File ルート例のプレースホルダー（/var/www/...）も実在ディレクトリへ差し替え
        // （バリデーションはファイル/ディレクトリの実在を確認するため）
        let www = dir.path().join("www");
        for sub in ["assets", "user", "app", "docs"] {
            std::fs::create_dir_all(www.join(sub)).unwrap();
        }
        std::fs::write(www.join("robots.txt"), "User-agent: *\n").unwrap();
        std::fs::write(www.join("index.html"), "<html></html>").unwrap();

        // TOML の基本文字列ではバックスラッシュがエスケープ扱いになるため、
        // Windows のパス（`C:\Users\...`）は区切りを `/` にしてから埋め込む。
        let toml_path = |p: &std::path::Path| p.to_string_lossy().replace('\\', "/");
        let content = content
            .replace("/path/to/cert.pem", &toml_path(&cert_path))
            .replace("/path/to/key.pem", &toml_path(&key_path))
            .replace("/var/www", &toml_path(&www));

        let test_path = dir.path().join("config.toml");
        std::fs::write(&test_path, content).unwrap();

        // nginx -t 相当の検証（TOML パース + バリデーション + 証明書存在確認）
        test_config_file(&test_path).expect("shipped config.toml must parse and validate");
    }
}

/// F-159: `Route::resolved_backend` が設定ロード時に構築される前提で、
/// `load_backend` がホットパスで Arc clone のみを行うことを確認するテスト。
/// F-178: capability mode の適格判定（起動時とリロード時で共用）。
#[cfg(test)]
mod f178_capability_mode_blocker_tests {
    use super::*;

    fn no_facts() -> CapModeFacts {
        CapModeFacts {
            has_l4: false,
            http3: false,
        }
    }

    /// 追加リスナーが無ければ入れる（プロキシ・upstream は接続ブローカーが代行するので阻害要因ではない。F-182）。
    #[test]
    fn eligible_without_extra_listeners() {
        assert_eq!(capability_mode_blocker(&no_facts()), None);
    }

    /// F-181: 要求したのに入れない構成は既定で起動を中止し、`allow_security_failures` のときだけ続行する。
    #[test]
    fn decision_is_fail_closed_by_default() {
        let b = Some(CapModeBlocker::L4);
        assert_eq!(
            capability_mode_decision(false, b, false),
            CapModeDecision::NotRequested
        );
        assert_eq!(
            capability_mode_decision(false, None, true),
            CapModeDecision::NotRequested
        );
        assert_eq!(
            capability_mode_decision(true, None, false),
            CapModeDecision::Enter
        );
        assert_eq!(
            capability_mode_decision(true, None, true),
            CapModeDecision::Enter
        );
        assert_eq!(
            capability_mode_decision(true, b, false),
            CapModeDecision::Abort(CapModeBlocker::L4)
        );
        assert_eq!(
            capability_mode_decision(true, b, true),
            CapModeDecision::RightsLimitedOnly(CapModeBlocker::L4)
        );
    }

    #[test]
    fn extra_listeners_are_rejected() {
        let cases = [
            (
                CapModeFacts {
                    has_l4: true,
                    ..no_facts()
                },
                CapModeBlocker::L4,
            ),
            (
                CapModeFacts {
                    http3: true,
                    ..no_facts()
                },
                CapModeBlocker::Http3,
            ),
        ];
        for (facts, want) in cases {
            assert_eq!(capability_mode_blocker(&facts), Some(want));
            assert!(!want.as_str().is_empty());
        }
    }
}

#[cfg(test)]
mod f159_resolved_backend_tests {
    use super::*;

    /// `resolved_backend` が埋まっている場合、`load_backend` はそれを clone して
    /// 返すだけであること（＝設定ロード時に構築した Backend と同一であること）を確認する。
    #[test]
    fn load_backend_returns_resolved_backend_when_present() {
        let toml = r#"
            action = { type = "Redirect", redirect_url = "https://example.com/", redirect_status = 301, preserve_path = false }
        "#;
        let mut route: Route = toml::from_str(toml).unwrap();
        let upstream_groups: HashMap<String, Arc<UpstreamGroup>> = HashMap::new();

        // 設定ロード時と同様に resolved_backend を構築する。
        let built = build_backend(&route, &upstream_groups).expect("build_backend should succeed");
        route.resolved_backend = Some(built);

        let loaded = load_backend(&route, &upstream_groups).expect("load_backend should succeed");

        // Redirect ならリダイレクト先 URL が resolved_backend 由来のものと一致することを確認
        // （clone のみで再構築していないことの間接確認）。
        match loaded {
            Backend::Redirect(url, status, preserve_path, _, _) => {
                assert_eq!(&*url, "https://example.com/");
                assert_eq!(status, 301);
                assert!(!preserve_path);
            }
            _ => panic!("expected Backend::Redirect"),
        }
    }

    /// 解決に失敗するルート（存在しない path の File バックエンド）では、
    /// `resolved_backend` が `None` のままとなり、`load_backend` は従来どおり
    /// エラーを返すこと（挙動の後方互換）を確認する。
    #[test]
    fn load_backend_errors_when_file_path_does_not_exist() {
        let toml = r#"
            action = { type = "File", path = "/nonexistent/path/for/f159/test", mode = "sendfile" }
        "#;
        let mut route: Route = toml::from_str(toml).unwrap();
        let upstream_groups: HashMap<String, Arc<UpstreamGroup>> = HashMap::new();

        // 設定ロード時の解決は失敗するため resolved_backend は None のまま
        // （build_backend(...).ok() 相当の挙動）。
        route.resolved_backend = build_backend(&route, &upstream_groups).ok();
        assert!(route.resolved_backend.is_none());

        // ホットパスの安全網として、従来どおり load_backend を呼んでもエラーになること。
        let result = load_backend(&route, &upstream_groups);
        assert!(result.is_err());
    }

    /// F-176: chroot でファイルシステムの見え方が変わった後、`reresolve_routes_after_chroot`
    /// が古い canonical 形（メモ）を捨てて解決し直すこと。chroot はテストで行えないので、
    /// 同じ設定パスが指すシンボリックリンクの先を差し替えて「見え方の変化」を再現する。
    #[cfg(unix)]
    #[test]
    // 理由付き allow: テストコードは同期 I/O・sleep を使用してよい（データプレーン非経由）。
    #[allow(clippy::disallowed_methods)]
    fn reresolve_routes_after_chroot_drops_stale_canonical_base() {
        let dir = tempfile::tempdir().unwrap();
        let before = dir.path().join("before");
        let after = dir.path().join("after");
        std::fs::create_dir_all(before.join("www")).unwrap();
        std::fs::create_dir_all(after.join("www")).unwrap();
        let link = dir.path().join("data");
        std::os::unix::fs::symlink(&before, &link).unwrap();

        let www = link.join("www");
        let toml = format!(
            "action = {{ type = \"File\", path = {:?}, mode = \"sendfile\" }}",
            www.to_str().unwrap()
        );
        let mut route: Route = toml::from_str(&toml).unwrap();
        let upstream_groups: HashMap<String, Arc<UpstreamGroup>> = HashMap::new();
        route.resolved_backend = build_backend(&route, &upstream_groups).ok();

        fn canonical_base(route: &Route) -> PathBuf {
            match route.resolved_backend.as_ref() {
                Some(Backend::SendFile(_, true, _, _, _, _, Some(base), _, _)) => {
                    base.to_path_buf()
                }
                other => panic!("expected a directory SendFile backend, got {:?}", other),
            }
        }
        assert_eq!(
            canonical_base(&route),
            before.join("www").canonicalize().unwrap()
        );

        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&after, &link).unwrap();

        let routes = reresolve_routes_after_chroot(std::slice::from_ref(&route), &upstream_groups);
        assert_eq!(
            canonical_base(&routes[0]),
            after.join("www").canonicalize().unwrap()
        );
    }
}

#[cfg(test)]
mod f175_upstream_http2_tests {
    use super::*;

    fn group_from_toml(toml_src: &str) -> UpstreamGroup {
        let cfg: UpstreamConfig = toml::from_str(toml_src).expect("parse upstream");
        UpstreamGroup::new(
            "g".to_string(),
            cfg.resolved_servers(),
            LoadBalanceAlgorithm::RoundRobin,
            None,
            cfg.tls_insecure,
        )
        .expect("group")
    }

    /// 既定は "auto"。HTTPS の上流だけが HTTP/2 を試す（平文は h2c 設定に従う）。
    #[test]
    fn default_is_auto_and_only_applies_to_tls() {
        let g = group_from_toml(r#"servers = ["https://127.0.0.1:8443", "http://127.0.0.1:8080"]"#);
        assert_eq!(g.servers[0].target.http2, UpstreamHttp2::Auto);
        assert!(g.servers[0].target.h2_over_tls());
        assert!(!g.servers[1].target.h2_over_tls());
    }

    /// グループの `http2` がサーバーの既定になり、サーバー単位の指定が優先する。
    #[test]
    fn group_default_and_server_override() {
        let g = group_from_toml(
            r#"
http2 = "off"
servers = [
    "https://127.0.0.1:8443",
    { url = "https://127.0.0.1:9443", http2 = "on" },
]
"#,
        );
        assert_eq!(g.servers[0].target.http2, UpstreamHttp2::Off);
        assert!(!g.servers[0].target.h2_over_tls());
        assert_eq!(g.servers[1].target.http2, UpstreamHttp2::On);
    }

    /// ルートの `url` 指定でも `http2` を受け付ける。不正な値はエラー。
    #[test]
    fn route_level_http2_and_invalid_value() {
        let action: BackendConfig = toml::from_str(
            r#"type = "Proxy"
url = "https://127.0.0.1:8443"
http2 = "on""#,
        )
        .expect("route action");
        match action {
            BackendConfig::Proxy { http2, .. } => assert_eq!(http2, UpstreamHttp2::On),
            _ => panic!("expected Proxy"),
        }
        assert!(toml::from_str::<UpstreamConfig>(
            r#"
http2 = "maybe"
servers = ["https://127.0.0.1:8443"]
"#
        )
        .is_err());
    }
}

#[cfg(test)]
mod session_resumption_tests {
    use super::{apply_session_resumption_mode, SessionResumption};
    use std::sync::Arc;

    fn pump(client: &mut rustls::ClientConnection, server: &mut rustls::ServerConnection) {
        for _ in 0..16 {
            let mut buf = Vec::new();
            while client.wants_write() {
                client.write_tls(&mut buf).unwrap();
            }
            if !buf.is_empty() {
                crate::tls_writev::read_tls_all(server, &buf).unwrap();
            }
            let mut back = Vec::new();
            while server.wants_write() {
                server.write_tls(&mut back).unwrap();
            }
            if !back.is_empty() {
                crate::tls_writev::read_tls_all(client, &back).unwrap();
            }
            if buf.is_empty() && back.is_empty() && !client.is_handshaking() {
                return;
            }
        }
    }

    /// 同じクライアント設定で 2 回接続し、2 回目がセッション再開になるか。
    fn second_handshake_kind(
        mode: SessionResumption,
        version: &'static rustls::SupportedProtocolVersion,
        client_tickets: bool,
    ) -> rustls::HandshakeKind {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let provider = Arc::new(crate::tls_provider::provider::default_provider());
        let cert_der = ck.cert.der().clone();
        let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()),
        );
        let mut server_cfg = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[version])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .unwrap();
        apply_session_resumption_mode(&mut server_cfg, mode, 64, 1).unwrap();
        let server_cfg = Arc::new(server_cfg);
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert_der).unwrap();
        let mut client_cfg = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[version])
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        if !client_tickets {
            // TLS 1.2 でチケットを使わず session ID で再開するクライアント（LibreSSL の wrk 等）。
            client_cfg.resumption = client_cfg
                .resumption
                .tls12_resumption(rustls::client::Tls12Resumption::SessionIdOnly);
        }
        let client_cfg = Arc::new(client_cfg);
        let mut kind = rustls::HandshakeKind::Full;
        for _ in 0..2 {
            let mut client = rustls::ClientConnection::new(
                Arc::clone(&client_cfg),
                rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            )
            .unwrap();
            let mut server = rustls::ServerConnection::new(Arc::clone(&server_cfg)).unwrap();
            pump(&mut client, &mut server);
            assert!(!client.is_handshaking() && !server.is_handshaking());
            kind = client.handshake_kind().unwrap();
        }
        kind
    }

    #[test]
    fn tickets_resume_tls13_and_both_tls12_styles() {
        use rustls::version::{TLS12, TLS13};
        use rustls::HandshakeKind::Resumed;
        assert_eq!(
            second_handshake_kind(SessionResumption::Tickets, &TLS13, true),
            Resumed
        );
        assert_eq!(
            second_handshake_kind(SessionResumption::Tickets, &TLS12, true),
            Resumed
        );
        assert_eq!(
            second_handshake_kind(SessionResumption::Tickets, &TLS12, false),
            Resumed
        );
    }

    #[test]
    fn cache_resumes_and_off_does_not() {
        use rustls::version::{TLS12, TLS13};
        use rustls::HandshakeKind::{Full, Resumed};
        assert_eq!(
            second_handshake_kind(SessionResumption::Cache, &TLS13, true),
            Resumed
        );
        assert_eq!(
            second_handshake_kind(SessionResumption::Cache, &TLS12, false),
            Resumed
        );
        assert_eq!(
            second_handshake_kind(SessionResumption::Off, &TLS13, true),
            Full
        );
        assert_eq!(
            second_handshake_kind(SessionResumption::Off, &TLS12, false),
            Full
        );
    }
}
