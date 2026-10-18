//! 読み込み済みの設定（LoadedConfig）・設定の読み込み・ホットリロード・load_backend
//!
//! F-233 で `src/config.rs` から移した（中身は変えていない）。

use super::*;

/// 設定読み込みの戻り値型（統一）
pub struct LoadedConfig {
    /// TLS のリスナー（`[server].listen`、F-200）。無ければ平文だけのサーバ。
    pub listen_addr: Option<String>,
    /// 平文のリスナー（`[server].listen_cleartext`、F-200）
    pub listen_cleartext: Option<String>,
    /// 平文リスナーの受け入れ方（F-200）
    pub cleartext: CleartextPolicy,
    /// UDS ソケットファイルパーミッション（`[server].unix_socket_permissions`、
    /// F-164）。8 進数表記の文字列のまま保持し、bind 時に
    /// `parse_unix_socket_permissions` でパースする。
    pub unix_socket_permissions: String,
    pub tls_config: Arc<ServerConfig>,
    /// TLS証明書パス（ログ・表示用）
    pub tls_cert_path: String,
    /// TLS秘密鍵パス（ログ・表示用）
    pub tls_key_path: String,
    /// 証明書の自動リロードを有効にするか（F-03）
    pub tls_auto_reload: bool,
    /// 証明書リロードチェック間隔（秒、F-03）
    pub tls_reload_interval_secs: u64,
    /// 設定された TLS 暗号スイート（F-50、リロード時の再構築用）
    pub tls_cipher_suites: Vec<String>,
    /// TLS証明書（PEM形式、事前読み込み済み）
    ///
    /// Landlock適用前に読み込まれた証明書データ。
    /// HTTP/3ではmemfd経由でquicheに渡すことで、
    /// Landlockによるファイルシステム制限下でも動作可能。
    pub tls_cert_pem: Arc<Vec<u8>>,
    /// TLS秘密鍵（PEM形式、事前読み込み済み）
    ///
    /// Landlock適用前に読み込まれた秘密鍵データ。
    /// HTTP/3ではmemfd経由でquicheに渡す。
    pub tls_key_pem: Arc<Vec<u8>>,
    /// 統合ルーティング（唯一のルーティング方式）
    pub route: Arc<Vec<Route>>,
    /// 最適化ルーター（Phase 1-4最適化適用）
    pub optimized_router: Arc<routing::OptimizedRouter>,
    pub ktls_config: KtlsConfig,
    pub reuseport_balancing: ReuseportBalancing,
    pub num_threads: usize,
    /// Huge Pages (Large OS Pages) を有効化するかどうか
    pub huge_pages_enabled: bool,
    /// グローバルセキュリティ設定
    pub global_security: GlobalSecurityConfig,
    /// ログ設定
    pub logging: LoggingConfigSection,
    /// Prometheusメトリクス設定
    pub prometheus_config: PrometheusConfig,
    /// 管理 API 設定（F-20）
    #[cfg(feature = "admin")]
    pub admin_config: AdminConfig,
    /// 構造化アクセスログ設定（F-21）
    #[cfg(feature = "access-log")]
    pub access_log_config: crate::access_log::AccessLogConfig,
    /// OpenTelemetry 設定（F-10）
    pub opentelemetry: OpenTelemetryConfig,
    /// Upstream グループ（健康チェック用）
    pub upstream_groups: Arc<HashMap<String, Arc<UpstreamGroup>>>,
    /// HTTP/2 を有効化するかどうか
    #[cfg(feature = "http2")]
    pub http2_enabled: bool,
    /// HTTP/3 を有効化するかどうか
    #[cfg(feature = "http3")]
    pub http3_enabled: bool,
    /// HTTP/3 リスナーアドレス (UDP)
    #[cfg(feature = "http3")]
    pub http3_listen: Option<String>,
    /// HTTP/2 設定（詳細設定）
    #[cfg(feature = "http2")]
    pub http2_config: Http2ConfigSection,
    /// HTTP/3 設定（詳細設定）
    #[cfg(feature = "http3")]
    pub http3_config: Http3ConfigSection,
    /// WASM Filter Engine（WASM機能が有効な場合）
    #[cfg(feature = "wasm")]
    pub wasm_filter_engine: Option<Arc<crate::wasm::FilterEngine>>,
    /// パフォーマンス設定
    pub performance: PerformanceConfigSection,
    /// グレースフルシャットダウンタイムアウト（秒）
    pub graceful_shutdown_timeout_secs: u64,
    /// L4 プロキシリスナー設定（F-18）
    #[cfg(feature = "l4-proxy")]
    pub l4_listeners: Vec<L4ListenerConfig>,
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
pub struct RuntimeConfig {
    /// 統合ルーティング（唯一のルーティング方式）
    pub route: Arc<Vec<Route>>,
    /// 最適化ルーター（Phase 1-4最適化適用）
    pub optimized_router: Arc<routing::OptimizedRouter>,
    /// TLS設定（ホットリロード時の参照用）
    pub tls_config: Option<Arc<ServerConfig>>,
    /// kTLS設定（ホットリロード時の参照用）
    pub ktls_config: Arc<KtlsConfig>,
    /// 平文リスナー（`[server].listen_cleartext`）の受け入れ方（F-200）。ホットリロード対象。
    pub cleartext: CleartextPolicy,
    /// グローバルセキュリティ設定（ホットリロード時の参照用）
    pub global_security: Arc<GlobalSecurityConfig>,
    /// Prometheusメトリクス設定
    pub prometheus_config: Arc<PrometheusConfig>,
    /// 管理 API 設定（F-20）
    #[cfg(feature = "admin")]
    pub admin_config: Arc<AdminConfig>,
    /// 構造化アクセスログ設定（F-21）
    #[cfg(feature = "access-log")]
    pub access_log_config: Arc<crate::access_log::AccessLogConfig>,
    /// Upstream グループ（健康チェック用）
    pub upstream_groups: Arc<HashMap<String, Arc<UpstreamGroup>>>,
    /// HTTP/2 有効化フラグ
    #[cfg(feature = "http2")]
    pub http2_enabled: bool,
    /// HTTP/2 設定（詳細設定）
    #[cfg(feature = "http2")]
    pub http2_config: Http2ConfigSection,
    /// HTTP/3 設定（圧縮設定の解決に使用）
    #[cfg(feature = "http3")]
    pub http3_config: Http3ConfigSection,
    /// WASM Filter Engine（WASM機能が有効な場合）
    #[cfg(feature = "wasm")]
    pub wasm_filter_engine: Option<Arc<crate::wasm::FilterEngine>>,
    /// パフォーマンス設定（Via header, chunk size等）
    pub performance: PerformanceConfigSection,
    /// L4 プロキシリスナー設定（F-18）
    #[cfg(feature = "l4-proxy")]
    pub l4_listeners: Arc<Vec<L4ListenerConfig>>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            route: Arc::new(Vec::new()),
            optimized_router: Arc::new(routing::OptimizedRouter::new()),
            tls_config: None,
            ktls_config: Arc::new(KtlsConfig::default()),
            cleartext: CleartextPolicy::default(),
            global_security: Arc::new(GlobalSecurityConfig::default()),
            prometheus_config: Arc::new(PrometheusConfig::default()),
            #[cfg(feature = "admin")]
            admin_config: Arc::new(AdminConfig::default()),
            #[cfg(feature = "access-log")]
            access_log_config: Arc::new(crate::access_log::AccessLogConfig::default()),
            upstream_groups: Arc::new(HashMap::new()),
            #[cfg(feature = "http2")]
            http2_enabled: false,
            #[cfg(feature = "http2")]
            http2_config: Http2ConfigSection::default(),
            #[cfg(feature = "http3")]
            http3_config: Http3ConfigSection::default(),
            #[cfg(feature = "wasm")]
            wasm_filter_engine: None,
            performance: PerformanceConfigSection::default(),
            #[cfg(feature = "l4-proxy")]
            l4_listeners: Arc::new(Vec::new()),
        }
    }
}

/// グローバルな設定保持用（ホットリロード対応）
/// 読み込みはロックフリーで非常に高速
pub static CURRENT_CONFIG: Lazy<ArcSwap<RuntimeConfig>> =
    Lazy::new(|| ArcSwap::from_pointee(RuntimeConfig::default()));

/// デフォルトの設定ファイルパス
pub(super) const DEFAULT_CONFIG_PATH: &str = "/etc/veil/config.toml";

/// グローバルな設定ファイルパス（ホットリロード用）
/// コマンドライン引数で指定されたパス、またはデフォルトパスを保持
pub static CONFIG_PATH: Lazy<ArcSwap<PathBuf>> =
    Lazy::new(|| ArcSwap::from_pointee(PathBuf::from(DEFAULT_CONFIG_PATH)));

/// HTTPSリダイレクト先のポート（listen設定から抽出）
///
/// HTTP→HTTPSリダイレクト時に使用するポート番号。
/// デフォルトは443（HTTPSの標準ポート）。
/// main関数で`[server].listen`の値から初期化される。
pub static HTTPS_REDIRECT_PORT: std::sync::atomic::AtomicU16 =
    std::sync::atomic::AtomicU16::new(443);

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
pub fn reload_config(path: &Path) -> io::Result<()> {
    let loaded = load_config_without_tls(path)?;

    // 現在のTLS設定を維持（ホットリロード対象外）
    let current = CURRENT_CONFIG.load();

    // キャッシュをクリア（設定変更時）
    loaded.optimized_router.clear_cache();

    let runtime_config = RuntimeConfig {
        route: loaded.route,
        optimized_router: loaded.optimized_router,
        // TLS設定は起動時のものを維持（セキュリティ上の理由）
        tls_config: current.tls_config.clone(),
        ktls_config: current.ktls_config.clone(),
        cleartext: loaded.cleartext,
        global_security: Arc::new(loaded.global_security),
        prometheus_config: Arc::new(loaded.prometheus_config),
        #[cfg(feature = "admin")]
        admin_config: Arc::new(loaded.admin_config),
        #[cfg(feature = "access-log")]
        access_log_config: Arc::new(loaded.access_log_config),
        upstream_groups: loaded.upstream_groups,
        #[cfg(feature = "http2")]
        http2_enabled: loaded.http2_enabled,
        #[cfg(feature = "http2")]
        http2_config: loaded.http2_config,
        #[cfg(feature = "http3")]
        http3_config: loaded.http3_config,
        #[cfg(feature = "wasm")]
        wasm_filter_engine: current.wasm_filter_engine.clone(),
        performance: loaded.performance.clone(),
        // L4 リスナーはホットリロード対象外（再起動が必要）
        #[cfg(feature = "l4-proxy")]
        l4_listeners: current.l4_listeners.clone(),
    };

    // アトミックに設定を入れ替え
    // `[prometheus] enabled` を記録側の実行時スイッチへ反映する（以前は設定ロード時に
    // 一度も呼ばれておらず、無効設定・既定値でも毎リクエストのメトリクス記録が走っていた）。
    crate::metrics::set_metrics_runtime_enabled(runtime_config.prometheus_config.enabled);
    crate::l7::set_max_buffered_request_body(&runtime_config.route);
    crate::path_norm::set_request_mode(runtime_config.global_security.path_normalization);
    CURRENT_CONFIG.store(Arc::new(runtime_config));

    info!("Configuration reloaded successfully (TLS certificates unchanged - restart required for TLS updates)");
    Ok(())
}

/// ホットリロード用の設定（TLS証明書を除く）
///
/// Landlock適用後はTLS証明書ファイルへのアクセスが制限されるため、
/// ホットリロード時はルーティング設定等のみを更新します。
pub struct LoadedConfigWithoutTls {
    /// 統合ルーティング（唯一のルーティング方式）
    pub route: Arc<Vec<Route>>,
    /// 最適化ルーター（Phase 1-4最適化適用）
    pub optimized_router: Arc<routing::OptimizedRouter>,
    pub global_security: GlobalSecurityConfig,
    pub prometheus_config: PrometheusConfig,
    #[cfg(feature = "admin")]
    pub admin_config: AdminConfig,
    /// 構造化アクセスログ設定（F-21）
    #[cfg(feature = "access-log")]
    pub access_log_config: crate::access_log::AccessLogConfig,
    pub upstream_groups: Arc<HashMap<String, Arc<UpstreamGroup>>>,
    #[cfg(feature = "http2")]
    pub http2_enabled: bool,
    #[cfg(feature = "http2")]
    pub http2_config: Http2ConfigSection,
    #[cfg(feature = "http3")]
    pub http3_config: Http3ConfigSection,
    /// 平文リスナーの受け入れ方（F-200）
    pub cleartext: CleartextPolicy,
    pub performance: PerformanceConfigSection,
}

/// リロード時に設定ファイルを読む唯一の入口（F-178）。
///
/// FreeBSD の capability mode では絶対パスの `open` が `ECAPMODE` になるため、`cap_enter` 前に
/// 登録した親ディレクトリ fd から `openat` で読む（`security::capsicum::read_config_file`）。
/// 未登録（capability mode を使わない構成・他 OS）は従来どおり `fs::read_to_string`。
// 理由付き allow: リロード・設定検証時のみ実行されるコールドパス（データプレーン非経由）。
#[allow(clippy::disallowed_methods)]
pub(super) fn read_config_source(path: &Path) -> io::Result<String> {
    #[cfg(target_os = "freebsd")]
    if let Some(res) = crate::security::capsicum::read_config_file(path) {
        return res;
    }
    read_config_text(path)
}

/// 設定ファイルの本文を読む（起動・リロード・検証の全経路の入口）。
///
/// API ライブラリのモード（`handler` feature でフック設定済み）では、ファイルが無ければ
/// 空文字（= ライブラリ既定設定とコード側の上書きだけで起動する）を返す。
// 理由付き allow: 起動・リロード・設定検証時のみ実行されるコールドパス（データプレーン非経由）。
#[allow(clippy::disallowed_methods)]
pub(crate) fn read_config_text(path: &Path) -> io::Result<String> {
    #[cfg(feature = "handler")]
    if crate::config_override::library_mode() && !path.exists() {
        return Ok(String::new());
    }
    fs::read_to_string(path)
}

/// TLS証明書を除いた設定をロード（ホットリロード用）
///
/// Landlock適用後は証明書ファイルへのアクセスが制限されるため、
/// この関数ではTLS関連の読み込みをスキップします。
// 理由付き allow: 起動・リロード・設定検証時のみ実行されるコールドパス（データプレーン非経由）。
#[allow(clippy::disallowed_methods)]
pub(super) fn load_config_without_tls(path: &Path) -> io::Result<LoadedConfigWithoutTls> {
    let config_str = read_config_source(path)?;
    // ホットリロード時も --override は同じグローバル集合が適用される
    // （src/config_override.rs の単一チョークポイント経由）。
    let config: Config = crate::config_override::apply_to_toml_str(&config_str)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // F-178: capability mode 中のリロードは、cap_enter 後でも動く変更だけを受け入れる
    // （グローバルな状態を書き換える処理より前に判定し、拒否時は何も変えずに前の設定を保つ）。
    // `validate_config` のパス存在チェックより先に行い、拒否の理由をそのままログに出す。
    #[cfg(target_os = "freebsd")]
    if crate::security::capsicum::is_capability_mode() {
        check_capability_mode_reload(&config)?;
    }

    // 設定ファイルのバリデーション
    validate_config(&config)?;
    warn_unbuilt_sections(&config);

    // HTTP/2・HTTP/3・H2C 設定を読み込み
    #[cfg(feature = "http2")]
    let http2_enabled = config.server.http2_enabled;
    #[cfg(feature = "http2")]
    let http2_config = config.http2.clone();
    #[cfg(feature = "http3")]
    let http3_config = config.http3.clone();
    let cleartext = CleartextPolicy::from_server(&config.server);

    // Serverヘッダー設定を更新（リロード対応）
    init_server_header(
        config.server.server_header_enabled,
        &config.server.server_header_value,
    );

    // Alt-Svc（HTTP/3 広告、F-94）— リロードでも同期
    apply_alt_svc_from_config(&config);

    // Upstream グループを構築（ロードバランシング用）
    let mut upstream_groups: HashMap<String, Arc<UpstreamGroup>> = HashMap::new();
    if let Some(upstreams) = &config.upstreams {
        for (name, cfg) in upstreams {
            let algorithm = resolve_algorithm(&cfg.algorithm, &cfg.hash_key);
            if let Some(group) = UpstreamGroup::new(
                name.clone(),
                cfg.resolved_servers(),
                algorithm.clone(),
                cfg.health_check.clone(),
                cfg.tls_insecure,
            ) {
                let mut group = group.with_resilience(&cfg.circuit_breaker, &cfg.outlier_detection);
                if cfg.tls.is_set() {
                    group = group.with_tls_client(crate::upstream_tls::intern(
                        &cfg.tls,
                        config.tls.ktls_enabled,
                    )?);
                }
                info!(
                    "Reloaded upstream '{}' with {} servers ({:?})",
                    name,
                    group.len(),
                    algorithm
                );
                upstream_groups.insert(name.clone(), Arc::new(group));
            } else {
                warn!("Failed to reload upstream '{}': no valid servers", name);
            }
        }
    }

    // 統合ルーティング（[[route]]）の読み込み
    let routes = if let Some(routes_config) = config.route {
        let mut routes_vec = Vec::with_capacity(routes_config.len());
        for mut route in routes_config {
            // F-148: モジュール設定の合成は設定ロード時に 1 回だけ行い、`resolved_modules`
            // へ Arc 共有で持たせる（load_backend はリクエストごとに呼ばれるため）。
            resolve_route_modules(
                &mut route,
                #[cfg(feature = "wasm")]
                config.wasm.as_ref(),
            );
            // F-159: Backend / 圧縮設定 / パスプレフィックスを設定ロード時に 1 回だけ
            // 構築し Route へ持たせる。ホットパス（load_backend / find_backend_unified）は
            // Arc clone のみになる。失敗しても設定ロードは失敗させず（`.ok()`）、
            // 従来どおりリクエスト時に構築して同じエラーを返す（後方互換）。
            route.resolved_backend = build_backend(&route, &upstream_groups).ok();
            // F-219: ルート単位のトラフィック制御（分割先の Backend・固定応答をここで作る）
            route.resolved_traffic =
                crate::route_traffic::resolve(&route, &upstream_groups, build_backend)?;
            route.resolved_compression =
                Some(Arc::new(route.compression.clone().unwrap_or_default()));
            route.resolved_path_prefix = Some(crate::upstream::extract_path_prefix_arc(&route));
            routes_vec.push(route);
        }
        Arc::new(routes_vec)
    } else {
        Arc::new(Vec::new())
    };

    // OptimizedRouter を構築（Phase 1-4 最適化）
    let optimized_router = build_optimized_router(&routes);

    // グローバルOpenFileCache設定を適用
    let performance_config = &config.performance;
    cache::configure_global_open_file_cache(
        performance_config.open_file_cache_enabled,
        performance_config.open_file_cache_valid_duration_secs,
        performance_config.open_file_cache_max_entries,
    );

    // F-146: グローバル静的コンテンツキャッシュ設定を適用
    cache::configure_global_static_content_cache(&config.static_file_cache);

    // F-35: グローバル IP ブロックリストを適用（起動時・SIGHUP リロード時の両方で本関数が
    // 呼ばれるためここで一元的に適用する）。CIDR はパース済みで保持され accept ホットパスでは
    // 文字列解析を行わない。
    set_global_blocked_ips(&config.security.blocked_ips);

    // AdminConfig の事前計算フィールドを補完
    #[cfg(feature = "admin")]
    let admin_config = {
        let mut admin = config.admin;
        admin.compute_derived();
        admin
    };

    Ok(LoadedConfigWithoutTls {
        route: routes,
        optimized_router,
        global_security: config.security,
        prometheus_config: config.prometheus,
        #[cfg(feature = "admin")]
        admin_config,
        #[cfg(feature = "access-log")]
        access_log_config: config.access_log,
        upstream_groups: Arc::new(upstream_groups),
        #[cfg(feature = "http2")]
        http2_enabled,
        #[cfg(feature = "http2")]
        http2_config,
        #[cfg(feature = "http3")]
        http3_config,
        cleartext,
        performance: config.performance.clone(),
    })
}

/// ルート配列から OptimizedRouter を構築
///
/// Phase 1: Host-based グループ化
/// Phase 2: Path Radix Tree (matchit)
/// Phase 3: CIDR Tree 最適化
/// Phase 4: LRU キャッシュ（構築時に初期化）
pub(super) fn build_optimized_router(routes: &[Route]) -> Arc<routing::OptimizedRouter> {
    let mut router = routing::OptimizedRouter::with_cache_capacity(10000);

    for (idx, route) in routes.iter().enumerate() {
        let conditions = &route.conditions;

        // host条件
        let host = conditions.host.as_deref();
        // path条件
        let path = conditions.path.as_deref();
        // source_ip条件
        let source_ip = conditions.source_ip.as_deref();

        router.add_route(idx, host, path, source_ip);
    }

    // CIDRマッチャーを最適化（ソート）
    router.finalize();

    info!(
        "Built OptimizedRouter: {} routes indexed (host groups: {} exact + {} wildcard, path patterns: {})",
        routes.len(),
        router.host_router.exact_count(),
        router.host_router.wildcard_count(),
        router.path_router.patterns_count()
    );

    Arc::new(router)
}

// 理由付き allow: 起動・リロード・設定検証時のみ実行されるコールドパス（データプレーン非経由）。
#[allow(clippy::disallowed_methods)]
pub fn load_config(path: &Path) -> io::Result<LoadedConfig> {
    let config_str = read_config_text(path)?;
    let config: Config = crate::config_override::apply_to_toml_str(&config_str)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // 設定ファイルのバリデーション
    validate_config(&config)?;
    warn_unbuilt_sections(&config);

    // kTLS設定（TLS設定より先に読み込む）
    let ktls_config = KtlsConfig {
        enabled: config.tls.ktls_enabled,
        enable_tx: config.tls.ktls_enabled,
        enable_rx: config.tls.ktls_enabled,
        fallback_enabled: config.tls.ktls_fallback_enabled,
        tcp_cork_enabled: config.tls.tcp_cork_enabled,
    };

    // HTTP/2・HTTP/3・H2C 設定を読み込み
    // 有効化フラグは server セクションで管理、詳細設定は [http2]/[http3] セクション
    #[cfg(feature = "http2")]
    let http2_enabled = config.server.http2_enabled;
    #[cfg(feature = "http3")]
    let http3_enabled = config.http3.enabled;
    #[cfg(feature = "http2")]
    let http2_config = config.http2.clone();
    #[cfg(feature = "http3")]
    let http3_config = config.http3.clone();
    #[cfg(feature = "http3")]
    let http3_listen = http3_config.listen.clone();
    let cleartext = CleartextPolicy::from_server(&config.server);

    // Serverヘッダー設定を初期化
    init_server_header(
        config.server.server_header_enabled,
        &config.server.server_header_value,
    );

    // Alt-Svc（HTTP/3 広告、F-94）
    apply_alt_svc_from_config(&config);

    // バッファプール設定を初期化
    init_buffer_pool_config(config.buffer_pool.clone());

    // TLS設定（kTLS有効時はシークレット抽出を有効化、HTTP/2有効時はALPN設定）
    #[cfg(feature = "http2")]
    let tls_config = load_tls_config(&config.tls, ktls_config.enabled, http2_enabled)?;
    #[cfg(not(feature = "http2"))]
    let tls_config = load_tls_config(&config.tls, ktls_config.enabled, false)?;

    // Upstream グループを構築（ロードバランシング用）
    let mut upstream_groups: HashMap<String, Arc<UpstreamGroup>> = HashMap::new();
    if let Some(upstreams) = &config.upstreams {
        for (name, cfg) in upstreams {
            let algorithm = resolve_algorithm(&cfg.algorithm, &cfg.hash_key);
            if let Some(group) = UpstreamGroup::new(
                name.clone(),
                cfg.resolved_servers(),
                algorithm.clone(),
                cfg.health_check.clone(),
                cfg.tls_insecure,
            ) {
                let mut group = group.with_resilience(&cfg.circuit_breaker, &cfg.outlier_detection);
                if cfg.tls.is_set() {
                    group = group.with_tls_client(crate::upstream_tls::intern(
                        &cfg.tls,
                        config.tls.ktls_enabled,
                    )?);
                }
                info!(
                    "Loaded upstream '{}' with {} servers ({:?})",
                    name,
                    group.len(),
                    algorithm
                );
                if cfg.health_check.is_some() {
                    info!("  Health check enabled for '{}'", name);
                }
                upstream_groups.insert(name.clone(), Arc::new(group));
            } else {
                warn!("Failed to load upstream '{}': no valid servers", name);
            }
        }
    }

    // 統合ルーティング（[[route]]）の読み込み
    let routes = if let Some(routes_config) = config.route {
        let mut routes_vec = Vec::with_capacity(routes_config.len());
        for mut route in routes_config {
            // F-148: モジュール設定の合成は設定ロード時に 1 回だけ行い、`resolved_modules`
            // へ Arc 共有で持たせる（load_backend はリクエストごとに呼ばれるため）。
            resolve_route_modules(
                &mut route,
                #[cfg(feature = "wasm")]
                config.wasm.as_ref(),
            );
            // F-159: Backend / 圧縮設定 / パスプレフィックスを設定ロード時に 1 回だけ
            // 構築し Route へ持たせる。ホットパス（load_backend / find_backend_unified）は
            // Arc clone のみになる。失敗しても設定ロードは失敗させず（`.ok()`）、
            // 従来どおりリクエスト時に構築して同じエラーを返す（後方互換）。
            route.resolved_backend = build_backend(&route, &upstream_groups).ok();
            // F-219: ルート単位のトラフィック制御（分割先の Backend・固定応答をここで作る）
            route.resolved_traffic =
                crate::route_traffic::resolve(&route, &upstream_groups, build_backend)?;
            route.resolved_compression =
                Some(Arc::new(route.compression.clone().unwrap_or_default()));
            route.resolved_path_prefix = Some(crate::upstream::extract_path_prefix_arc(&route));
            routes_vec.push(route);
        }
        Arc::new(routes_vec)
    } else {
        Arc::new(Vec::new())
    };

    // OptimizedRouter を構築（Phase 1-4 最適化）
    let optimized_router = build_optimized_router(&routes);

    // グローバルOpenFileCache設定を適用
    let performance_config = &config.performance;
    cache::configure_global_open_file_cache(
        performance_config.open_file_cache_enabled,
        performance_config.open_file_cache_valid_duration_secs,
        performance_config.open_file_cache_max_entries,
    );

    // F-146: グローバル静的コンテンツキャッシュ設定を適用
    cache::configure_global_static_content_cache(&config.static_file_cache);

    // F-35: グローバル IP ブロックリストを適用（起動時・SIGHUP リロード時の両方で本関数が
    // 呼ばれるためここで一元的に適用する）。CIDR はパース済みで保持され accept ホットパスでは
    // 文字列解析を行わない。
    set_global_blocked_ips(&config.security.blocked_ips);

    // スレッド数の決定: 未指定または0の場合はCPUコア数を使用
    let num_threads = match config.server.threads {
        Some(n) if n > 0 => n,
        _ => num_cpus::get(),
    };

    // Prometheusメトリクス設定をログ出力
    // F-09: ランタイムトグルを設定（enabled=false で record_* がノーオップ・endpoint が 404）
    crate::metrics::set_metrics_runtime_enabled(config.prometheus.enabled);
    if config.prometheus.enabled {
        info!(
            "Prometheus metrics enabled at path: {}",
            config.prometheus.path
        );
        if !config.prometheus.allowed_ips.is_empty() {
            info!("  Allowed IPs: {:?}", config.prometheus.allowed_ips);
        }
    } else {
        info!("Prometheus metrics disabled");
    }

    // TLS証明書をバイト列として読み込み（HTTP/3用、Landlock適用前に読み込み）
    // これによりLandlock適用後も証明書ファイルへのアクセスなしで動作可能
    // 平文専用サーバ（`[tls]` 無し）では読まない（HTTP/3 は TLS 必須なので無効が保証されている）。
    let tls_cert_pem = if !config.tls.is_configured() {
        Vec::new()
    } else {
        fs::read(&config.tls.cert_path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("Failed to read TLS cert '{}': {}", config.tls.cert_path, e),
            )
        })?
    };
    let tls_key_pem = if !config.tls.is_configured() {
        Vec::new()
    } else {
        fs::read(&config.tls.key_path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("Failed to read TLS key '{}': {}", config.tls.key_path, e),
            )
        })?
    };

    info!(
        "TLS certificates pre-loaded for Landlock compatibility (cert: {} bytes, key: {} bytes)",
        tls_cert_pem.len(),
        tls_key_pem.len()
    );

    // WASM Filter Engineの初期化
    #[cfg(feature = "wasm")]
    let wasm_filter_engine = if let Some(wasm_config) = &config.wasm {
        if wasm_config.enabled {
            // バリデーション（validate_configで既に実行されているが、念のため）
            if let Err(e) = wasm_config.validate() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("WASM config validation failed: {}", e),
                ));
            }

            // FilterEngine初期化
            info!("Initializing WASM Filter Engine...");
            match crate::wasm::init(wasm_config) {
                Ok(engine) => {
                    info!("WASM Filter Engine initialized successfully");
                    Some(Arc::new(engine))
                }
                Err(e) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("Failed to initialize WASM Filter Engine: {}", e),
                    ));
                }
            }
        } else {
            info!("WASM extensions disabled");
            None
        }
    } else {
        None
    };

    Ok(LoadedConfig {
        listen_addr: config.server.listen.clone(),
        listen_cleartext: config.server.listen_cleartext.clone(),
        cleartext,
        unix_socket_permissions: config.server.unix_socket_permissions.clone(),
        tls_config,
        tls_cert_path: config.tls.cert_path.clone(),
        tls_key_path: config.tls.key_path.clone(),
        // 証明書が無い（平文専用サーバ）ならリロード対象も無い。
        tls_auto_reload: config.tls.auto_reload && config.tls.is_configured(),
        tls_reload_interval_secs: config.tls.reload_interval_secs,
        tls_cipher_suites: config.tls.cipher_suites.clone(),
        tls_cert_pem: Arc::new(tls_cert_pem),
        tls_key_pem: Arc::new(tls_key_pem),
        route: routes,
        optimized_router,
        ktls_config,
        reuseport_balancing: config.performance.reuseport_balancing,
        num_threads,
        huge_pages_enabled: config.performance.huge_pages_enabled,
        global_security: config.security,
        logging: config.logging,
        prometheus_config: config.prometheus,
        #[cfg(feature = "admin")]
        admin_config: {
            let mut a = config.admin;
            a.compute_derived();
            a
        },
        #[cfg(feature = "access-log")]
        access_log_config: config.access_log,
        opentelemetry: config.opentelemetry,
        upstream_groups: Arc::new(upstream_groups),
        #[cfg(feature = "http2")]
        http2_enabled,
        #[cfg(feature = "http3")]
        http3_enabled,
        #[cfg(feature = "http3")]
        http3_listen,
        #[cfg(feature = "http2")]
        http2_config,
        #[cfg(feature = "http3")]
        http3_config,
        #[cfg(feature = "wasm")]
        wasm_filter_engine,
        performance: config.performance.clone(),
        graceful_shutdown_timeout_secs: config.server.graceful_shutdown_timeout_secs,
        #[cfg(feature = "l4-proxy")]
        l4_listeners: {
            let listeners = config.l4.unwrap_or_default();
            // F-148: L4 リスナーの WASM モジュール設定も HTTP ルートと同様、設定ロード時に
            // 1 回だけ合成する（コネクションごとの合成を避ける）。
            // `wasm` 無効ビルドでは合成対象が無く `mut` も不要になるため、
            // `mut` を伴うシャドーイングごと cfg の内側に閉じ込める（`#[allow(unused_mut)]`
            // を足さずに警告ゼロを保つ）。
            #[cfg(feature = "wasm")]
            let listeners = {
                let mut listeners = listeners;
                for l4 in &mut listeners {
                    resolve_l4_wasm_modules(l4, config.wasm.as_ref());
                }
                listeners
            };
            listeners
        },
    })
}

/// L4 リスナーの `wasm_modules` + `module_configuration` から
/// `resolved_wasm_modules` を構築する（F-148、設定ロード時のみのコールドパス）。
///
/// 呼び出し元は `load_config` の `l4_listeners` 構築部（`l4-proxy` feature 有効時のみ存在）
/// なので、`wasm` だけを有効にしたビルドで未使用関数にならないよう両方の feature で
/// ゲートする。
#[cfg(all(feature = "wasm", feature = "l4-proxy"))]
pub(super) fn resolve_l4_wasm_modules(
    l4: &mut L4ListenerConfig,
    wasm_config: Option<&crate::wasm::WasmConfig>,
) {
    let resolved: Vec<ModuleRef> = l4
        .wasm_modules
        .iter()
        .map(|name| {
            let base = wasm_config
                .and_then(|w| w.modules.iter().find(|m| &m.name == name))
                .map(|m| &m.configuration);
            let over = l4.module_configuration.as_ref().and_then(|mc| mc.get(name));
            match PluginConfiguration::merge_over(base, over) {
                Some(cfg) => ModuleRef::new(name.clone(), Some(cfg.to_bytes())),
                None => ModuleRef::from_name(name),
            }
        })
        .collect();
    l4.resolved_wasm_modules = Arc::new(resolved);
}

/// 設定ファイルからログ設定のみを読み込む（ログ初期化前用）
// 理由付き allow: 起動・リロード・設定検証時のみ実行されるコールドパス（データプレーン非経由）。
#[allow(clippy::disallowed_methods)]
pub fn load_logging_config(path: &Path) -> io::Result<LoggingConfigSection> {
    let config_str = read_config_text(path)?;
    let config: Config = crate::config_override::apply_to_toml_str(&config_str)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(config.logging)
}

// JSON形式ログフォーマッタ（JsonLogFormat 等）と init_logging は
// crate::logging モジュールに移動しました。

/// `File` ルートの `is_dir` 判定をメモ化して返す（B-64）。
///
/// `canonical_base_memoized` と同じ理由。`load_backend` がリクエストごとに呼ばれるため、
/// `fs::metadata` を毎回叩くと 1 リクエスト 1 回の `fstatat` になる（DTrace 実測で
/// `__realpathat` と同数＝最多の syscall だった）。設定に対して不変な値なので
/// 設定パスごとに 1 回だけ解決する。
///
/// エラーはメモしない（起動時にパスが存在しない運用を壊さないため、次回再試行する）。
/// 成功した時点でメモされ、以降 syscall は発生しない。
pub(super) fn is_dir_memoized<F>(path: &str, fetch: F) -> io::Result<bool>
where
    F: FnOnce() -> io::Result<bool>,
{
    if let Some(hit) = IS_DIR_MEMO.with(|m| m.borrow().get(path).copied()) {
        return Ok(hit);
    }
    let resolved = fetch()?;
    IS_DIR_MEMO.with(|m| m.borrow_mut().insert(path.to_string(), resolved));
    Ok(resolved)
}

// `is_dir_memoized` のメモ本体（`CANONICAL_BASE_MEMO` と同じくスレッドローカル。
// 理由はそちらのコメントを参照）。
thread_local! {
    static IS_DIR_MEMO: std::cell::RefCell<std::collections::HashMap<String, bool>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

// `File` ルートの base_path を canonical 化した結果のメモ（B-64）。
//
// `load_backend` はリクエストごとに呼ばれるため、ここで毎回 `canonicalize()` すると
// 1 リクエスト 1 回の `__realpathat` になる（実測でホットパス最多の syscall だった）。
// base_path は設定に対して不変なので、設定文字列をキーにプロセス全体で 1 回だけ解決する。
//
// 値が `None`（解決失敗）の場合もキャッシュする。起動時にディレクトリが存在しない運用を
// 壊さないための挙動（`cache::sendfile_base_contains` が生パスへフォールバックする）だが、
// 失敗を毎回再試行すると結局 syscall が走ってしまうため、結果ごとキャッシュする。
// **スレッドローカルで持つ**。当初はプロセス共有の `RwLock<HashMap>` にしたが、
// `load_backend` はリクエストごとに呼ばれるため**毎リクエスト 1 回のロック取得**が
// 発生し、実測でスループットが改善しなかった（syscall は消えたのに 234k → 201k rps）。
// ウォームな VFS キャッシュ上の `realpath`/`fstat` は 1μs 程度と安く、
// ワーカースレッド間で共有する RwLock の読み取り競合の方が高くつく。
// veil は thread-per-core なので、ワーカーごとに 1 回解決すれば以降は
// **syscall もアトミック操作も無し**で済む。
// （`dashmap` は `cache` feature 依存なので `--no-default-features` では使えない、
//  という制約もあり std のみで完結させている。）
thread_local! {
    static CANONICAL_BASE_MEMO: std::cell::RefCell<
        std::collections::HashMap<String, Option<Arc<Path>>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// `path` の canonical 形をメモ化して返す（B-64。詳細は [`CANONICAL_BASE_MEMO`]）。
pub(super) fn canonical_base_memoized(path: &str) -> Option<Arc<Path>> {
    if let Some(hit) = CANONICAL_BASE_MEMO.with(|m| m.borrow().get(path).cloned()) {
        return hit;
    }
    // F-178: capability mode では `canonicalize()` が `ECAPMODE` になる（リロードスレッドの
    // メモは空）。cap_enter 前の登録時に解決した canonical 形を使う。
    #[cfg(target_os = "freebsd")]
    if crate::security::capsicum::is_capability_mode() {
        let resolved = crate::security::capsicum::registered_canonical_root(Path::new(path))
            .map(|c| Arc::from(c.as_path()));
        if resolved.is_some() {
            CANONICAL_BASE_MEMO.with(|m| m.borrow_mut().insert(path.to_string(), resolved.clone()));
        }
        return resolved;
    }
    // 理由付き allow: 設定パスごとに 1 回だけ実行されるコールドパス（以降はメモから返る）。
    #[allow(clippy::disallowed_methods)]
    let resolved = match Path::new(path).canonicalize() {
        Ok(c) => Some(Arc::from(c.as_path())),
        Err(e) => {
            warn!(
                "File ルート {:?} の canonicalize に失敗（リクエスト時は生パスへフォールバック）: {}",
                path, e
            );
            None
        }
    };
    // **失敗（None）はメモしない。** 起動時点でディレクトリが存在しなくても、後から
    // 作成されれば動作し続ける、という既存の運用挙動を壊さないため
    // （`cache::sendfile_base_contains` が生パスへフォールバックする設計）。
    // 失敗し続ける構成では毎回 `canonicalize` が走るが、それは元々の挙動と同じであり、
    // 正常な構成（＝解決できる）では初回のみで以降 syscall は発生しない。
    if resolved.is_some() {
        CANONICAL_BASE_MEMO.with(|m| m.borrow_mut().insert(path.to_string(), resolved.clone()));
    }
    resolved
}

/// chroot(2) 後にルートの `resolved_backend` を作り直す（NetBSD の `chroot_dir`。F-140/F-176）。
///
/// 設定ロードは chroot 前に行うため、`canonical_base_memoized` は chroot 外の実体パスを
/// 返す（証明書を chroot 前後の両方で読めるよう chroot 外の同じパスにシンボリックリンクを
/// 張る構成では、リンクを辿った chroot 外のパスになる）。chroot 後のファイル解決結果は
/// 新しいルート基準になるので、`sendfile_base_contains` の包含判定が一致せず静的配信が
/// すべて 403 になっていた。メモを捨てて新しいルート基準で解決し直す（起動時に 1 回だけ）。
#[cfg(any(target_os = "netbsd", test))]
pub fn reresolve_routes_after_chroot(
    routes: &[Route],
    upstream_groups: &HashMap<String, Arc<UpstreamGroup>>,
) -> Arc<Vec<Route>> {
    CANONICAL_BASE_MEMO.with(|m| m.borrow_mut().clear());
    IS_DIR_MEMO.with(|m| m.borrow_mut().clear());
    Arc::new(
        routes
            .iter()
            .map(|route| {
                let mut route = route.clone();
                route.resolved_backend = build_backend(&route, upstream_groups).ok();
                route
            })
            .collect(),
    )
}

/// FreeBSD capsicum の capability mode（`cap_enter`）に入れない理由（F-178）。
///
/// capability mode では `connect(2)`・`bind(2)`・絶対パスの `open(2)` ができない。
/// `cap_enter` 後にリスナーの追加（`bind(2)`）を要する構成は対象外とし、起動時（入るかどうか）と
/// リロード時（受け入れるかどうか）で同じ判定を使う。上流への接続は接続ブローカー（F-182）が
/// 代行するので、プロキシ・`[upstreams]` は阻害要因ではない。
#[cfg(any(target_os = "freebsd", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapModeBlocker {
    /// `[[l4]]` リスナーがある
    L4,
    /// 平文のリスナー（`[server].listen_cleartext`、F-200）がある
    Cleartext,
    /// HTTP/3 が有効
    Http3,
}

#[cfg(any(target_os = "freebsd", test))]
impl CapModeBlocker {
    pub fn as_str(self) -> &'static str {
        match self {
            CapModeBlocker::L4 => "[[l4]] listeners require bind(2)/connect(2)",
            CapModeBlocker::Cleartext => "[server].listen_cleartext requires an extra listener",
            CapModeBlocker::Http3 => "HTTP/3 requires an extra UDP listener",
        }
    }
}

/// capability mode を要求されたときの起動時の扱い（F-181）。
#[cfg(any(target_os = "freebsd", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapModeDecision {
    /// 要求されていない
    NotRequested,
    /// capability mode に入る
    Enter,
    /// 条件を満たさないが `allow_security_failures = true` なので rights 制限のみで続行する
    RightsLimitedOnly(CapModeBlocker),
    /// 条件を満たさないので起動を中止する（既定。fail-closed）
    Abort(CapModeBlocker),
}

/// capability mode の起動時の扱いを決める（F-181）。
///
/// 要求したのに入れない構成は、他のサンドボックスと同じく既定で起動を中止する
/// （黙って弱いサンドボックスのまま動かさない）。`allow_security_failures = true` のときだけ続行する。
#[cfg(any(target_os = "freebsd", test))]
pub fn capability_mode_decision(
    requested: bool,
    blocker: Option<CapModeBlocker>,
    allow_security_failures: bool,
) -> CapModeDecision {
    match (requested, blocker) {
        (false, _) => CapModeDecision::NotRequested,
        (true, None) => CapModeDecision::Enter,
        (true, Some(b)) if allow_security_failures => CapModeDecision::RightsLimitedOnly(b),
        (true, Some(b)) => CapModeDecision::Abort(b),
    }
}

/// capability mode の適格判定の入力（ルート以外）。
#[cfg(any(target_os = "freebsd", test))]
pub struct CapModeFacts {
    pub has_l4: bool,
    /// `[server].listen_cleartext` がある（F-200）
    pub cleartext: bool,
    pub http3: bool,
}

/// capability mode に入れない最初の理由を返す（入れるなら `None`）。
#[cfg(any(target_os = "freebsd", test))]
pub fn capability_mode_blocker(facts: &CapModeFacts) -> Option<CapModeBlocker> {
    if facts.has_l4 {
        return Some(CapModeBlocker::L4);
    }
    if facts.cleartext {
        return Some(CapModeBlocker::Cleartext);
    }
    if facts.http3 {
        return Some(CapModeBlocker::Http3);
    }
    None
}

/// 上流ターゲットを接続ブローカーの許可リストのエントリにする（F-182）。
#[cfg(target_os = "freebsd")]
pub(super) fn connect_target_of(t: &ProxyTarget) -> crate::connect_broker::Target {
    use crate::connect_broker::Target;
    if let Some(p) = &t.unix_path {
        return Target::Unix(PathBuf::from(p.as_ref()));
    }
    match t.socket_addr {
        Some(a) => Target::Addr(a),
        None => Target::Host(t.conn_addr().as_str().to_string()),
    }
}

/// 読み込み済みの設定から、接続ブローカーの許可リストを作る（F-182）。
///
/// `[upstreams]` の全サーバ、`Proxy` ルートの URL（ルートごとの上流グループ）、OpenTelemetry の
/// エンドポイント。データプレーン・ヘルスチェック・WASM の外部呼び出しが接続するのはこの範囲だけ。
#[cfg(target_os = "freebsd")]
pub fn startup_connect_allowlist(cfg: &LoadedConfig) -> crate::connect_broker::Allowlist {
    let mut allow = crate::connect_broker::Allowlist::new();
    for group in cfg.upstream_groups.values() {
        for s in &group.servers {
            allow.insert(connect_target_of(&s.target));
        }
    }
    for route in cfg.route.iter() {
        if let Some(Backend::Proxy(group, ..)) = &route.resolved_backend {
            for s in &group.servers {
                allow.insert(connect_target_of(&s.target));
            }
        }
    }
    #[cfg(feature = "opentelemetry")]
    if cfg.opentelemetry.enabled {
        if let Some(a) = crate::otel::connect_addr(&cfg.opentelemetry.endpoint) {
            allow.insert(crate::connect_broker::Target::from_host_port(&a));
        }
    }
    allow
}

/// リロードする設定が接続する上流（F-182。許可リストに入っているかを確かめる）。
#[cfg(target_os = "freebsd")]
pub(super) fn config_connect_targets(config: &Config) -> Vec<crate::connect_broker::Target> {
    let mut out = Vec::new();
    if let Some(ups) = &config.upstreams {
        for u in ups.values() {
            for e in u.resolved_servers() {
                if let Some(t) = ProxyTarget::parse(&e.url) {
                    out.push(connect_target_of(&t));
                }
            }
        }
    }
    for route in config.route.as_deref().unwrap_or(&[]) {
        if let BackendConfig::Proxy { url, .. } = &route.action {
            if let Some(t) = ProxyTarget::parse(url) {
                out.push(connect_target_of(&t));
            }
        }
    }
    out
}

/// capability mode 中のリロードで、新しい設定が cap_enter 後でも動くかを確かめる（F-178）。
///
/// 拒否した場合はエラーを返し、呼び出し元（`reload_config`）は前の設定を保つ。
/// - 追加リスナーを要する構成（`capability_mode_blocker`）
/// - 接続ブローカーの許可リスト（起動時に確定）に無い上流（F-182）
/// - 起動時に登録していない静的ルート（cap_enter 後は新しいディレクトリを開けない）
/// - アクセスログの出力先の変更・新規有効化（新しいパスを開けず、スレッドも作り直さない）
#[cfg(target_os = "freebsd")]
pub(super) fn check_capability_mode_reload(config: &Config) -> io::Result<()> {
    let reject = |why: String| {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "capability mode: {}; restart veil to apply this change",
                why
            ),
        ))
    };
    let routes: &[Route] = config.route.as_deref().unwrap_or(&[]);
    #[cfg(feature = "l4-proxy")]
    let has_l4 = config.l4.as_ref().is_some_and(|v| !v.is_empty());
    #[cfg(not(feature = "l4-proxy"))]
    let has_l4 = false;
    #[cfg(feature = "http3")]
    let http3 = config.http3.enabled;
    #[cfg(not(feature = "http3"))]
    let http3 = false;
    let facts = CapModeFacts {
        has_l4,
        cleartext: config.server.listen_cleartext.is_some(),
        http3,
    };
    if let Some(b) = capability_mode_blocker(&facts) {
        return reject(b.as_str().to_string());
    }
    // F-182: 上流は起動時に確定した接続ブローカーの許可リストに入っているものだけ。
    let allow = crate::connect_broker::client().map(|c| c.allowlist());
    for t in config_connect_targets(config) {
        let ok = allow.is_some_and(|a| match &t {
            crate::connect_broker::Target::Addr(x) => a.index_of_addr(x).is_some(),
            crate::connect_broker::Target::Host(h) => a.index_of_host(h).is_some(),
            crate::connect_broker::Target::Unix(p) => a.index_of_unix(p).is_some(),
        });
        if !ok {
            return reject(format!(
                "upstream {:?} is not in the connect allowlist fixed at startup",
                t
            ));
        }
    }
    for route in routes {
        if let BackendConfig::File { path, .. } = &route.action {
            if !crate::security::capsicum::is_registered_static_root(Path::new(path)) {
                return reject(format!(
                    "static root {:?} was not registered before cap_enter",
                    path
                ));
            }
        }
    }
    #[cfg(feature = "access-log")]
    {
        let current = CURRENT_CONFIG.load();
        let cur = &current.access_log_config;
        let new = &config.access_log;
        if new.enabled && (!cur.enabled || new.file_path != cur.file_path) {
            return reject(format!(
                "access log destination changed ({:?} -> {:?})",
                cur.enabled.then_some(&cur.file_path),
                new.file_path
            ));
        }
    }
    Ok(())
}

pub fn load_backend(
    route: &Route,
    upstream_groups: &HashMap<String, Arc<UpstreamGroup>>,
) -> io::Result<Backend> {
    // F-159: 設定ロード時に解決済みの Backend があれば Arc clone のみ（malloc なし）で
    // 返す。未解決（安全網、または旧経路からの呼び出し）の場合のみ従来どおり構築する。
    if let Some(b) = &route.resolved_backend {
        return Ok(b.clone());
    }
    build_backend(route, upstream_groups)
}

/// `load_backend` の実体。ログ出力・エラーメッセージ・メモ化を含め従来の
/// `load_backend` と同一の処理を行う。
///
/// F-159 以降、**通常は設定ロード時（`load_config` / `load_config_without_tls` の
/// ルート読み込みループ）から 1 回だけ呼ばれる**。ホットパスから呼ばれるのは
/// `resolved_backend` の解決に失敗したルート（存在しないファイル等）の安全網のみで、
/// その場合は従来と同じエラーを毎回返す。
///
/// 理由付き allow: `fs::metadata`（is_dir 判定）と `fs::read`（MemoryFile モードの
/// 事前読み込み）を使う。上記のとおり設定ロード時のコールドパスであり、安全網経路でも
/// B-64 で判明したコストの大きい `canonicalize`/`metadata` は `canonical_base_memoized` /
/// `is_dir_memoized` で設定パスごとに 1 回だけ実行するようメモ化してある（2 回目以降は
/// syscall なし）。
#[allow(clippy::disallowed_methods)]
pub(super) fn build_backend(
    route: &Route,
    upstream_groups: &HashMap<String, Arc<UpstreamGroup>>,
) -> io::Result<Backend> {
    // Routeレベルの設定を取得（route直下の設定のみを使用）
    let security = route.security.clone().unwrap_or_default();
    let compression = route.compression.clone().unwrap_or_default();
    let buffering = route.buffering.clone().unwrap_or_default();
    let cache = route.cache.clone().unwrap_or_default();
    // F-148: 設定ロード時に解決済みの `resolved_modules` を Arc clone するだけ
    // （従来あった `Arc::new(m.clone())` = リクエストごとの `Vec<String>` ディープコピーを排除）。
    // `resolved_modules` が未設定（安全網）の場合のみ、名前のみの `ModuleRef` を都度組み立てる。
    let modules_arc = route.resolved_modules.clone().or_else(|| {
        route.modules.as_ref().map(|names| {
            Arc::new(
                names
                    .iter()
                    .map(|n| ModuleRef::from_name(n))
                    .collect::<Vec<_>>(),
            )
        })
    });

    match &route.action {
        BackendConfig::Proxy {
            url,
            sni_name,
            use_h2c,
            http2,
            tls,
        } => {
            // 単一URLの場合は UpstreamGroup::single で単一サーバーのグループを作成
            let target = ProxyTarget::parse(url)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid proxy URL"))?
                .with_sni_name(sni_name.clone())
                .with_h2c(*use_h2c)
                .with_http2(*http2);

            if *use_h2c && !target.use_tls {
                info!("H2C (HTTP/2 over cleartext) enabled for backend: {}", url);
            }

            // 圧縮設定のログ出力
            if compression.enabled {
                info!(
                    "Response compression enabled for backend: {} (gzip_level={}, brotli_level={})",
                    url, compression.gzip_level, compression.brotli_level
                );
            }

            // バッファリング設定のログ出力
            if buffering.is_enabled() {
                info!(
                    "Response buffering enabled for backend: {} (mode={:?}, max_memory={})",
                    url, buffering.mode, buffering.max_memory_buffer
                );
            }

            // キャッシュ設定のログ出力
            if cache.enabled {
                info!(
                    "Proxy cache enabled for backend: {} (max_memory={}, ttl={}s)",
                    url, cache.max_memory_size, cache.default_ttl_secs
                );
            }

            let mut group = UpstreamGroup::single(target);
            if let Some(t) = tls {
                // 上流の mTLS（F-217）。同じ設定は作り直さない（設定のロード時にだけ作る）
                group = group.with_tls_client(crate::upstream_tls::intern(
                    t,
                    CURRENT_CONFIG.load().ktls_config.enabled,
                )?);
            }
            Ok(Backend::Proxy(
                Arc::new(group),
                Arc::new(security.clone()),
                Arc::new(compression.clone()),
                Arc::new(buffering.clone()),
                Arc::new(cache.clone()),
                modules_arc.clone(),
            ))
        }
        BackendConfig::ProxyUpstream { upstream, use_h2c } => {
            // Upstream グループ参照
            let group = upstream_groups.get(upstream).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Upstream '{}' not found", upstream),
                )
            })?;

            // ルート設定で use_h2c が指定されている場合はオーバーライド
            let group = if *use_h2c {
                Arc::new(group.with_h2c(true))
            } else {
                group.clone()
            };

            // 圧縮設定のログ出力
            if compression.enabled {
                info!("Response compression enabled for upstream: {} (gzip_level={}, brotli_level={})", 
                      upstream, compression.gzip_level, compression.brotli_level);
            }

            // バッファリング設定のログ出力
            if buffering.is_enabled() {
                info!(
                    "Response buffering enabled for upstream: {} (mode={:?}, max_memory={})",
                    upstream, buffering.mode, buffering.max_memory_buffer
                );
            }

            // キャッシュ設定のログ出力
            if cache.enabled {
                info!(
                    "Proxy cache enabled for upstream: {} (max_memory={}, ttl={}s)",
                    upstream, cache.max_memory_size, cache.default_ttl_secs
                );
            }

            Ok(Backend::Proxy(
                group.clone(),
                Arc::new(security.clone()),
                Arc::new(compression.clone()),
                Arc::new(buffering.clone()),
                Arc::new(cache.clone()),
                modules_arc.clone(),
            ))
        }
        BackendConfig::File { path, mode, index } => {
            // Routeレベルの設定のみを使用
            let security = route.security.clone().unwrap_or_default();
            let cache = route.cache.clone().unwrap_or_default();
            let access_err = |e: io::Error| {
                let error_msg = format!(
                    "Failed to access file '{}': {} (error code: {})",
                    path,
                    e,
                    e.raw_os_error().unwrap_or(-1)
                );
                io::Error::new(e.kind(), error_msg)
            };
            // F-123: FreeBSD capability mode 下では絶対パス `fs::metadata` が禁止される
            // ため、登録済みルート dirfd 相対の `fstatat` で is_dir を判定する
            // （File ルート自身は登録ルートなので rel="." で解決される）。
            #[cfg(target_os = "freebsd")]
            let is_dir = is_dir_memoized(path, || {
                if let Some(res) =
                    crate::security::capsicum::stat_static(std::path::Path::new(path))
                {
                    Ok(res.map_err(access_err)?.is_dir)
                } else {
                    Ok(fs::metadata(path).map_err(access_err)?.is_dir())
                }
            })?;
            #[cfg(not(target_os = "freebsd"))]
            let is_dir = is_dir_memoized(path, || {
                Ok(fs::metadata(path).map_err(access_err)?.is_dir())
            })?;
            // インデックスファイル名を Arc<str> に変換（None = デフォルトで "index.html"）
            let index_file: Option<Arc<str>> = index.as_ref().map(|s| Arc::from(s.as_str()));
            let security = Arc::new(security.clone());
            let cache = Arc::new(cache.clone());
            let open_file_cache_arc = route.open_file_cache.as_ref().map(|c| Arc::new(c.clone()));
            let static_file_cache_arc = route
                .static_file_cache
                .as_ref()
                .map(|c| Arc::new(c.clone()));

            // キャッシュ設定のログ出力
            if cache.enabled {
                info!(
                    "File cache enabled for path: {} (max_memory={}, ttl={}s)",
                    path, cache.max_memory_size, cache.default_ttl_secs
                );
            }

            match mode.as_str() {
                "memory" => {
                    if is_dir {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "Memory mode not supported for directories",
                        ));
                    }
                    // F-178: FreeBSD の capability mode 中（リロード）は絶対パスの read が
                    // できないため、登録済み静的ルート配下なら dirfd 相対で読む。
                    #[cfg(target_os = "freebsd")]
                    let data = match crate::security::capsicum::open_static_ro(Path::new(path)) {
                        Some(f) => {
                            use std::io::Read;
                            let mut v = Vec::new();
                            f?.read_to_end(&mut v)?;
                            v
                        }
                        None => fs::read(path)?,
                    };
                    #[cfg(not(target_os = "freebsd"))]
                    let data = fs::read(path)?;
                    let mime_type = mime_guess::from_path(path).first_or_octet_stream();

                    Ok(Backend::MemoryFile(
                        Arc::new(data),
                        Arc::from(mime_type.as_ref()),
                        security,
                        modules_arc.clone(),
                    ))
                }
                "sendfile" | "" => {
                    // F-145 / B-64: SendFile の封じ込め検査に使う base_path の canonical 形。
                    //
                    // **`load_backend` はリクエストごとに呼ばれる**（`upstream.rs` の
                    // `find_backend_unified` がルート照合のたびに Backend を組み立てる）。
                    // F-145 の初版はここで直接 `canonicalize()` していたため、
                    // **1 リクエストあたり 1 回の `__realpathat` を発生させていた**
                    // （DTrace 実測で最多の syscall だった。B-64）。`realpath` はパスの
                    // 構成要素を 1 つずつ辿るので `stat` より高価で、小さなレスポンスほど
                    // 相対コストが大きい。
                    //
                    // base_path は設定に対して不変なので、**設定文字列をキーにメモ化**して
                    // プロセス全体で 1 回だけ解決する。ホットパスでは DashMap のルックアップ
                    // （syscall なし）だけになる。ホットリロードで設定が変わっても、
                    // 新しいパス文字列は別キーになるので取り違えは起きない。
                    let canonical_base: Option<Arc<Path>> = if is_dir {
                        canonical_base_memoized(path)
                    } else {
                        None
                    };
                    Ok(Backend::SendFile(
                        Arc::new(PathBuf::from(path)),
                        is_dir,
                        index_file,
                        security,
                        cache,
                        open_file_cache_arc,
                        canonical_base,
                        static_file_cache_arc,
                        modules_arc.clone(),
                    ))
                }
                _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid mode")),
            }
        }
        BackendConfig::Respond {
            status,
            body,
            content_type,
        } => {
            let mut headers = Vec::with_capacity(1);
            if let Some(ct) = content_type {
                headers.push((
                    bytes::Bytes::from_static(b"content-type"),
                    bytes::Bytes::from(ct.clone()),
                ));
            }
            Ok(Backend::Respond(Arc::new(
                crate::route_traffic::FixedResponse::new(
                    *status,
                    headers,
                    bytes::Bytes::from(body.clone()),
                )
                .with_security(Arc::new(security)),
            )))
        }
        BackendConfig::Redirect {
            redirect_url,
            redirect_status,
            preserve_path,
        } => Ok(Backend::Redirect(
            Arc::from(redirect_url.as_str()),
            *redirect_status,
            *preserve_path,
            modules_arc.clone(),
            Arc::new(security),
        )),
        #[cfg(feature = "handler")]
        BackendConfig::Handler { handler } => match crate::api::handler::lookup(handler) {
            Some(entry) => Ok(Backend::Handler(
                entry,
                Arc::new(security),
                modules_arc.clone(),
            )),
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown handler '{}' (not registered)", handler),
            )),
        },
    }
}
