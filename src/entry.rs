//! バイナリエントリ（`main` から `veil::run()` で呼び出す）。

use crate::logging::*;
use crate::metrics::*;
use crate::pool::*;
use crate::system::*;

use crate::runtime::tcp::TcpListener;
use crate::runtime::time::timeout;
use clap::Parser;
use ftlog::{debug, error, info, warn};
use rustls::crypto::CryptoProvider;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

// ktls_rustls（kTLS 対応）
#[cfg(feature = "ktls")]
use crate::ktls_rustls::RustlsAcceptor;

pub use crate::config::*;
use crate::proxy::*;
use crate::server::*;

// ====================
// ワーカースレッド
// ====================

/// I/O ワーカースレッド（per-core io_uring executor）を 8MB スタックで起動する。
///
/// `std::thread::spawn` の既定スタックは 2MB と小さく、接続ハンドラの async future は
/// 深いネスト（TLS read/write → HTTP/2 フレーム処理 → ストリーミング転送 → 応答リレー）で
/// 大きくなり得る。spawn 時に future はスタック上で構築されてからボックス化されるため、
/// 既定 2MB では大きな接続でスタックオーバーフローし得る。十分なスタックを確保して防ぐ。
fn spawn_worker_thread<F>(f: F) -> thread::JoinHandle<()>
where
    F: FnOnce() + Send + 'static,
{
    thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(f)
        .expect("failed to spawn I/O worker thread")
}

// ====================
// メイン関数
// ====================

pub fn run() {
    // コマンドライン引数を解析（--help, --version は clap が自動処理）
    let cli_args = CliArgs::parse();

    // 設定ファイルパスをグローバル変数に保存（ホットリロード用）
    CONFIG_PATH.store(Arc::new(cli_args.config.clone()));
    let config_path = cli_args.config;

    // -t オプション: 設定ファイルのテストのみ
    if cli_args.test_config {
        match test_config_file(&config_path) {
            Ok(()) => {
                println!(
                    "veil: configuration file {} test is successful",
                    config_path.display()
                );
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!(
                    "veil: configuration file {} test failed",
                    config_path.display()
                );
                eprintln!("veil: {}", e);
                std::process::exit(1);
            }
        }
    }

    // プロセスレベルで暗号プロバイダーをインストール（aws-lc-rs使用）
    CryptoProvider::install_default(rustls::crypto::aws_lc_rs::default_provider())
        .expect("Failed to install rustls crypto provider");

    // ログ設定を先に読み込む（ログ初期化前）
    // 設定ファイルが読めない場合はデフォルト設定を使用
    let logging_config =
        load_logging_config(&config_path).unwrap_or_else(|_| LoggingConfigSection::default());

    // ftlogを設定に基づいて初期化
    // ftlogは内部でバックグラウンドスレッドとチャネルを使用した非同期ログライブラリ
    // 追加の非同期化層（tokio::sync::mpsc等）は不要
    let _guard = init_logging(&logging_config);

    #[cfg(feature = "http3")]
    let mut loaded_config = match load_config(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Config load error: {}", e);
            return;
        }
    };
    #[cfg(not(feature = "http3"))]
    let loaded_config = match load_config(&config_path) {
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

    // グレースフルシャットダウンタイムアウトを設定
    GRACEFUL_SHUTDOWN_TIMEOUT_SECS.store(
        loaded_config.graceful_shutdown_timeout_secs,
        Ordering::Relaxed,
    );

    // F-35: グローバル IP ブロックリスト（accept 段の最前線 DDoS 防御）は config の
    // ローダ内（build_loaded_config）で起動時・リロード時の両方に適用済み。
    if !loaded_config.global_security.blocked_ips.is_empty() {
        info!(
            "Front-line IP blocklist active: {} CIDR entr(ies)",
            loaded_config.global_security.blocked_ips.len()
        );
    }

    if loaded_config.graceful_shutdown_timeout_secs > 0 {
        info!(
            "Graceful shutdown timeout: {} seconds",
            loaded_config.graceful_shutdown_timeout_secs
        );
    } else {
        info!("Graceful shutdown timeout: disabled (immediate shutdown)");
    }

    // TLS アクセプターを作成
    #[cfg(feature = "ktls")]
    let acceptor = RustlsAcceptor::new(loaded_config.tls_config.clone())
        .with_ktls(loaded_config.ktls_config.enabled)
        .with_fallback(loaded_config.ktls_config.fallback_enabled)
        .with_tcp_cork(loaded_config.ktls_config.tcp_cork_enabled);

    #[cfg(not(feature = "ktls"))]
    let acceptor = crate::simple_tls::SimpleTlsAcceptor::new(loaded_config.tls_config.clone())
        .with_ktls(loaded_config.ktls_config.enabled);

    // F-03: グローバル TLS 設定を初期化（アクセプタが毎ハンドシェイク参照）
    crate::tls_reload::init_global_tls_config(loaded_config.tls_config.clone());

    // F-10: OpenTelemetry エクスポータを起動（feature 有効かつ enabled 時のみ）
    #[cfg(feature = "opentelemetry")]
    {
        let otel = &loaded_config.opentelemetry;
        if otel.enabled {
            crate::otel::init_global(crate::otel::OtelConfig {
                enabled: true,
                endpoint: otel.endpoint.clone(),
                service_name: otel.service_name.clone(),
                batch_interval_secs: otel.batch_interval_secs,
            });
            info!(
                "OpenTelemetry enabled: exporting to {} (interval: {}s)",
                otel.endpoint, otel.batch_interval_secs
            );
        }
    }

    // F-03: 証明書ホットリロードスレッドを起動（auto_reload 有効時のみ）
    //
    // 注意: Landlock サンドボックス適用後は証明書ファイルへの read が
    // 制限される。auto_reload を使う場合は、Landlock の許可パスに証明書
    // ディレクトリを含めるか、サンドボックスを無効化すること。
    if loaded_config.tls_auto_reload {
        let cert_path = std::path::PathBuf::from(&loaded_config.tls_cert_path);
        let key_path = std::path::PathBuf::from(&loaded_config.tls_key_path);
        let interval = loaded_config.tls_reload_interval_secs.max(1);
        let ktls_enabled = loaded_config.ktls_config.enabled;
        #[cfg(feature = "http2")]
        let http2_enabled = loaded_config.http2_enabled;
        #[cfg(not(feature = "http2"))]
        let http2_enabled = false;
        // F-50: リロード時も設定された暗号スイートを維持する
        let cipher_suites = loaded_config.tls_cipher_suites.clone();
        let builder: crate::tls_reload::ServerConfigBuilder = Box::new(move |c, k| {
            crate::config::build_server_config_from_paths(
                c,
                k,
                ktls_enabled,
                http2_enabled,
                &cipher_suites,
            )
        });
        match crate::tls_reload::TlsCertReloader::new_global(cert_path, key_path, builder) {
            Ok(reloader) => {
                spawn_tls_reloader(reloader, interval);
                info!(
                    "TLS certificate auto-reload enabled (interval: {}s, SIGHUP supported)",
                    interval
                );
            }
            Err(e) => {
                warn!("Failed to initialize TLS cert reloader: {}", e);
            }
        }
    }

    let listen_addr = loaded_config
        .listen_addr
        .parse::<SocketAddr>()
        .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 443)));

    // HTTPSリダイレクト用ポートを保存（HTTP→HTTPSリダイレクト時に使用）
    HTTPS_REDIRECT_PORT.store(listen_addr.port(), std::sync::atomic::Ordering::Relaxed);

    let ktls_config = Arc::new(loaded_config.ktls_config.clone());

    // CURRENT_CONFIG を初期化（ホットリロード対応）
    // ワーカースレッドは CURRENT_CONFIG.load() を使用して最新の設定を取得
    let runtime_config = RuntimeConfig {
        route: loaded_config.route.clone(),
        optimized_router: loaded_config.optimized_router.clone(),
        tls_config: Some(loaded_config.tls_config.clone()),
        ktls_config: ktls_config.clone(),
        global_security: Arc::new(loaded_config.global_security.clone()),
        prometheus_config: Arc::new(loaded_config.prometheus_config.clone()),
        #[cfg(feature = "admin")]
        admin_config: Arc::new(loaded_config.admin_config.clone()),
        #[cfg(feature = "access-log")]
        access_log_config: Arc::new(loaded_config.access_log_config.clone()),
        upstream_groups: loaded_config.upstream_groups.clone(),
        #[cfg(feature = "http2")]
        http2_enabled: loaded_config.http2_enabled,
        #[cfg(feature = "http2")]
        http2_config: loaded_config.http2_config.clone(),
        #[cfg(feature = "http3")]
        http3_config: loaded_config.http3_config.clone(),
        #[cfg(feature = "http2")]
        h2c_enabled: loaded_config.h2c_enabled,
        #[cfg(feature = "http2")]
        h2c_listen: loaded_config.h2c_listen.clone(),
        #[cfg(feature = "wasm")]
        wasm_filter_engine: loaded_config.wasm_filter_engine.clone(),
        performance: loaded_config.performance.clone(),
        #[cfg(feature = "l4-proxy")]
        l4_listeners: Arc::new(loaded_config.l4_listeners.clone()),
    };
    CURRENT_CONFIG.store(Arc::new(runtime_config));
    info!("Runtime configuration initialized (hot reload enabled via SIGHUP)");

    // グローバルプロキシキャッシュの初期化
    // デフォルト設定でグローバルキャッシュを初期化（各ルートのcache設定で有効化される）
    let global_cache_config = crate::cache::CacheConfig {
        enabled: true,
        max_memory_size: 100 * 1024 * 1024, // 100MB
        disk_path: None,
        max_disk_size: 1024 * 1024 * 1024, // 1GB
        memory_threshold: 64 * 1024,       // 64KB
        default_ttl_secs: 300,             // 5分
        ..Default::default()
    };

    match crate::cache::init_global_cache(global_cache_config) {
        Ok(()) => {
            info!("Global proxy cache initialized (max_memory=100MB, default_ttl=300s)");
        }
        Err(e) => {
            warn!("Failed to initialize global cache: {}", e);
        }
    }

    // HTTP/2・HTTP/3・H2C の設定ログ
    #[cfg(feature = "http2")]
    if loaded_config.http2_enabled {
        info!("HTTP/2 enabled via ALPN negotiation");
    }
    #[cfg(feature = "http2")]
    if loaded_config.h2c_enabled {
        let h2c_addr = loaded_config
            .h2c_listen
            .as_deref()
            .unwrap_or(&loaded_config.listen_addr);
        info!("H2C (HTTP/2 Cleartext) enabled (listener: {})", h2c_addr);
    }
    #[cfg(feature = "http3")]
    if loaded_config.http3_enabled {
        info!(
            "HTTP/3 enabled (UDP listener: {})",
            loaded_config
                .http3_listen
                .as_deref()
                .unwrap_or(&loaded_config.listen_addr)
        );
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
    info!(
        "Reuseport Balancing: {:?}",
        loaded_config.reuseport_balancing
    );
    info!("Read Timeout: {:?}", READ_TIMEOUT);
    info!("Write Timeout: {:?}", WRITE_TIMEOUT);
    info!("Connect Timeout: {:?}", CONNECT_TIMEOUT);
    info!("Idle Timeout: {:?}", IDLE_TIMEOUT);

    // ログ設定のログ出力
    info!(
        "Logging: level={}, channel_size={}, flush_interval={}ms",
        loaded_config.logging.level,
        loaded_config.logging.channel_size,
        loaded_config.logging.flush_interval_ms
    );
    match loaded_config.logging.resolved_app_path() {
        Some(p) => info!("Logging: app log (INFO/WARN/DEBUG) -> file '{}'", p),
        None => info!("Logging: app log (INFO/WARN/DEBUG) -> stdout"),
    }
    match loaded_config.logging.resolved_error_path() {
        Some(p) => info!("Logging: error log (ERROR) -> file '{}'", p),
        None => info!("Logging: error log (ERROR) -> stderr"),
    }

    // kTLS設定のログ出力
    log_ktls_status(&ktls_config);

    info!("============================================");

    // 構造化アクセスログライタースレッドを起動（access-log feature が有効な場合のみ）
    // ホットパスの stderr グローバルロックを排除し、専用スレッドで非同期書き込みする
    #[cfg(feature = "access-log")]
    {
        let cfg = CURRENT_CONFIG.load();
        crate::access_log::init_access_log_writer(&cfg.access_log_config);
    }

    // Graceful Shutdown用のシグナルハンドラを設定
    setup_signal_handler();

    // 設定リロードスレッドを起動（SIGHUP で設定を動的更新）
    spawn_reload_thread();

    // 健康チェックスレッドを起動（Upstream の健康状態を監視）
    spawn_health_check_thread();

    // キャッシュクリーンアップスレッドを起動（期限切れエントリの削除、LRU eviction）
    spawn_cache_cleanup_thread();

    // WASMタイマースレッドを起動（on_tick コールバック処理）
    #[cfg(feature = "wasm")]
    spawn_wasm_tick_thread();

    let mut handles = Vec::with_capacity(num_threads);

    // CPUアフィニティ設定のためのコアID取得
    let core_ids = core_affinity::get_core_ids();
    let core_ids_available = core_ids.as_ref().map(|ids| ids.len()).unwrap_or(0);

    if core_ids.is_some() && core_ids_available > 0 {
        info!(
            "CPU Affinity: {} cores available, pinning {} worker threads",
            core_ids_available, num_threads
        );
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
    // 1. サンドボックス setup（namespace分離、bind mounts）
    // 2. 権限降格（setuid/setgid）
    // 3. サンドボックス lockdown（PR_SET_NO_NEW_PRIVS、ケイパビリティ制限）
    // 4. Landlock（ファイルシステム制限）
    // 5. seccomp（システムコール制限）
    // ====================

    let sandbox_config = if loaded_config.global_security.enable_sandbox {
        // サンドボックスサポート状況をレポート
        crate::security::report_sandbox_support();
        Some(build_sandbox_config(&loaded_config.global_security))
    } else {
        None
    };

    if let Some(ref sandbox_config) = sandbox_config {
        match crate::security::apply_sandbox_setup(sandbox_config) {
            Ok(()) => {
                info!("Sandbox restrictions applied successfully");
                if sandbox_config.unshare_mount {
                    info!("Sandbox: Mount namespace isolated");
                }
                if sandbox_config.unshare_uts {
                    info!(
                        "Sandbox: UTS namespace isolated (hostname: {})",
                        sandbox_config.hostname.as_deref().unwrap_or("default")
                    );
                }
                if sandbox_config.unshare_ipc {
                    info!("Sandbox: IPC namespace isolated");
                }
                if sandbox_config.unshare_pid {
                    info!("Sandbox: PID namespace isolated");
                }
            }
            Err(e) => {
                if loaded_config.global_security.allow_security_failures {
                    warn!(
                        "Failed to apply sandbox restrictions: {} - continuing without sandbox",
                        e
                    );
                    warn!("Hint: Sandbox may require root privileges or CAP_SYS_ADMIN");
                } else {
                    error!("Failed to apply sandbox restrictions: {}", e);
                    error!("Server startup aborted. To allow failures, set allow_security_failures = true in config.toml");
                    error!("Hint: Sandbox may require root privileges or CAP_SYS_ADMIN");
                    return;
                }
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

    if let Some(ref sandbox_config) = sandbox_config {
        match crate::security::apply_sandbox_lockdown(sandbox_config) {
            Ok(()) => {
                if !sandbox_config.keep_capabilities.is_empty() {
                    info!(
                        "Sandbox: Keeping only capabilities: {:?}",
                        sandbox_config.keep_capabilities
                    );
                } else if !sandbox_config.drop_capabilities.is_empty() {
                    info!(
                        "Sandbox: Dropped capabilities: {:?}",
                        sandbox_config.drop_capabilities
                    );
                }
            }
            Err(e) => {
                if loaded_config.global_security.allow_security_failures {
                    warn!(
                        "Failed to apply sandbox lockdown: {} - continuing without capability restriction",
                        e
                    );
                } else {
                    error!("Failed to apply sandbox lockdown: {}", e);
                    error!("Server startup aborted. To allow failures, set allow_security_failures = true in config.toml");
                    return;
                }
            }
        }
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
    crate::security::report_security_status();

    // ログ出力先ファイルの親ディレクトリを Landlock 書き込み許可へ自動追加する。
    // （Landlock 有効時にログ書き込みが拒否されるのを防ぐ。日次ローテーションで生成される
    //   ファイルも同ディレクトリ配下のため、ファイル単体でなく親ディレクトリを許可する。）
    let mut landlock_write_paths = loaded_config.global_security.landlock_write_paths.clone();
    {
        let mut add_parent_dir = |file_path: Option<&str>| {
            if let Some(path) = file_path {
                let dir = std::path::Path::new(path)
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| ".".to_string());
                if !landlock_write_paths.contains(&dir) {
                    landlock_write_paths.push(dir);
                }
            }
        };
        add_parent_dir(loaded_config.logging.resolved_app_path());
        add_parent_dir(loaded_config.logging.resolved_error_path());
        #[cfg(feature = "access-log")]
        add_parent_dir(loaded_config.access_log_config.file_path.as_deref());
    }

    // セキュリティ設定を構築
    let security_config = crate::security::SecurityConfig {
        enable_io_uring_restrictions: true, // カスタム io_uring 実装で IORING_REGISTER_RESTRICTIONS を適用
        enable_seccomp: loaded_config.global_security.enable_seccomp,
        seccomp_mode: crate::security::SeccompMode::parse_str(
            &loaded_config.global_security.seccomp_mode,
        ),
        enable_landlock: loaded_config.global_security.enable_landlock,
        landlock_read_paths: loaded_config.global_security.landlock_read_paths.clone(),
        landlock_write_paths,
    };

    // セキュリティ制限を適用
    if security_config.enable_seccomp || security_config.enable_landlock {
        match crate::security::apply_security_restrictions(&security_config) {
            Ok(()) => {
                info!("Security restrictions applied successfully");
                if security_config.enable_seccomp {
                    info!("seccomp: mode={:?}", security_config.seccomp_mode);
                }
                if security_config.enable_landlock {
                    info!(
                        "Landlock: read_paths={:?}, write_paths={:?}",
                        security_config.landlock_read_paths, security_config.landlock_write_paths
                    );
                }
            }
            Err(e) => {
                if loaded_config.global_security.allow_security_failures {
                    warn!(
                        "Failed to apply security restrictions: {} - continuing without them",
                        e
                    );
                } else {
                    error!("Failed to apply security restrictions: {}", e);
                    error!("Server startup aborted. To allow failures, set allow_security_failures = true in config.toml");

                    // より詳細なエラーメッセージ
                    if security_config.enable_seccomp {
                        error!("seccomp was enabled but failed to apply");
                    }
                    if security_config.enable_landlock {
                        error!("Landlock was enabled but failed to apply");
                    }
                    error!("Hint: Check kernel version requirements (seccomp: Linux 3.17+, Landlock: Linux 5.13+)");
                    error!("Hint: Ensure required privileges are available");

                    return;
                }
            }
        }
    }

    // 同時接続数制限
    let max_connections = loaded_config.global_security.max_concurrent_connections;

    // H2C専用サーバーの判定
    // H2Cが有効で、h2c_listenが未指定またはlistenと同じ場合、通常のTLSリスナーは不要
    #[cfg(feature = "http2")]
    let is_h2c_only_server = loaded_config.h2c_enabled
        && (loaded_config.h2c_listen.is_none()
            || loaded_config.h2c_listen.as_ref().unwrap() == &loaded_config.listen_addr);

    #[cfg(not(feature = "http2"))]
    let is_h2c_only_server = false;

    // 通常のTLSリスナーを起動（H2C専用サーバーの場合はスキップ）
    if !is_h2c_only_server {
        info!("============================================");
        info!("HTTPS Server");
        info!("Listen Address: {}", listen_addr);
        info!("Workers: {} (SO_REUSEPORT enabled)", num_threads);
        info!("============================================");

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

            let handle = spawn_worker_thread(move || {
                // スレッド開始直後にCPUアフィニティを設定
                // これによりL1/L2キャッシュミスを削減し、レイテンシのジッターを安定化
                if let Some(core_id) = assigned_core {
                    if core_affinity::set_for_current(core_id) {
                        info!("[Thread {}] Pinned to CPU core {:?}", thread_id, core_id);
                    } else {
                        warn!(
                            "[Thread {}] Failed to pin to CPU core {:?}, running unpinned",
                            thread_id, core_id
                        );
                    }
                }

                crate::runtime::block_on(async move {
                    let listener = match create_listener(addr, balancing, workers, thread_id) {
                        Ok(l) => l,
                        Err(e) => {
                            error!("[Thread {}] Bind error: {}", thread_id, e);
                            return;
                        }
                    };

                    info!("[Thread {}] Worker started", thread_id);

                    // F-46: 接続ハンドラの型付きタスクプール（spawn ごとの Box 確保を排除）
                    let conn_pool = crate::runtime::TaskPool::new();

                    loop {
                        // Shutdown チェック
                        if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                            info!("[Thread {}] Shutting down...", thread_id);
                            break;
                        }

                        // タイムアウト付きaccept（Graceful Shutdown対応）
                        let accept_result =
                            timeout(Duration::from_secs(1), listener.accept()).await;

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

                        // F-35: 最前線 IP ブロックリスト。ブロック対象 IP は TLS ハンドシェイク
                        // 前・ハンドラ spawn 前に切断する（stream は drop で閉じられる）。
                        if crate::config::is_ip_blocked(peer_addr.ip()) {
                            continue;
                        }

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

                        let _ = stream.set_nodelay(true);

                        let acceptor = acceptor_clone.clone();

                        // パニックキャッチ + 型付きプール（F-46）でスレッド生存とゼロ確保を両立
                        spawn_pooled_with_panic_catch(&conn_pool, async move {
                            // ConnectionGuard がスコープ内で生存している間、接続がカウントされる
                            // パニック時も Drop が呼ばれるため、カウンターの整合性が保証される
                            let _guard = ConnectionGuard::new();
                            // handle_connection 内で CURRENT_CONFIG から最新の設定を取得
                            // これによりホットリロード時に新しい設定が即座に反映される
                            handle_connection(stream, acceptor, peer_addr).await;
                        });
                    }

                    // グレースフルシャットダウン: 既存接続の完了を待機
                    drain_connections("Thread", thread_id).await;

                    info!("[Thread {}] Worker stopped", thread_id);
                });
            });
            handles.push(handle);
        }
    } else {
        info!("Skipping TLS listener (H2C-only server detected)");
    }

    // HTTP to HTTPS リダイレクトワーカー（設定されている場合のみ）
    if let Some(http_addr) = loaded_config.listen_http_addr {
        info!("============================================");
        info!("HTTP to HTTPS Redirect Server");
        info!("HTTP Listen Address: {}", http_addr);
        info!("All HTTP requests will be redirected to HTTPS (301)");
        info!("============================================");

        let http_handle = spawn_worker_thread(move || {
            crate::runtime::block_on(async move {
                // HTTPリスナーを作成（SO_REUSEADDRを有効化）
                let listener = match TcpListener::bind(http_addr) {
                    Ok(l) => l,
                    Err(e) => {
                        error!("[HTTP] Bind error on {}: {}", http_addr, e);
                        return;
                    }
                };

                info!("[HTTP] Redirect worker started");

                // F-46: リダイレクトハンドラの型付きタスクプール
                let redirect_pool = crate::runtime::TaskPool::new();

                loop {
                    // Shutdown チェック
                    if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                        info!("[HTTP] Shutting down...");
                        break;
                    }

                    // タイムアウト付きaccept
                    let accept_result = timeout(Duration::from_secs(1), listener.accept()).await;

                    let (stream, peer_addr) = match accept_result {
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

                    // F-35: 最前線 IP ブロックリスト（リダイレクト処理前に弾く）
                    if crate::config::is_ip_blocked(peer_addr.ip()) {
                        continue;
                    }

                    let _ = stream.set_nodelay(true);

                    // 軽量なリダイレクト処理をspawn（パニック耐性あり・型付きプール）
                    spawn_pooled_with_panic_catch(&redirect_pool, async move {
                        handle_http_redirect(stream).await;
                    });
                }

                // グレースフルシャットダウン: 既存接続の完了を待機
                // HTTPリダイレクトワーカーはConnectionGuardを使用していないため、
                // 単純にスリープで待機（リダイレクト処理は高速なため問題なし）
                let timeout_secs = GRACEFUL_SHUTDOWN_TIMEOUT_SECS.load(Ordering::Relaxed);
                if timeout_secs > 0 {
                    crate::runtime::time::sleep(Duration::from_millis(500)).await;
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
        let http3_addr_str = loaded_config
            .http3_listen
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
        info!(
            "TLS Cert: {} (pre-loaded, {} bytes)",
            tls_cert_path,
            tls_cert_pem.len()
        );
        info!(
            "TLS Key: {} (pre-loaded, {} bytes)",
            tls_key_path,
            tls_key_pem.len()
        );
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

            let http3_handle = spawn_worker_thread(move || {
                // スレッド開始直後にCPUアフィニティを設定
                if let Some(core_id) = assigned_core {
                    if core_affinity::set_for_current(core_id) {
                        info!(
                            "[HTTP/3 Worker {}] Pinned to CPU core {:?}",
                            thread_id, core_id
                        );
                    } else {
                        warn!(
                            "[HTTP/3 Worker {}] Failed to pin to CPU core {:?}",
                            thread_id, core_id
                        );
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

                let config = crate::http3_server::Http3ServerConfig {
                    cert_path: String::new(), // memfd使用時は不要
                    key_path: String::new(),  // memfd使用時は不要
                    cert_pem: Some(cert_data),
                    key_pem: Some(key_data),
                    ..Default::default()
                };

                info!("[HTTP/3 Worker {}] Starting...", thread_id);

                if let Err(e) = crate::http3_server::run_http3_server(addr, config) {
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

    // H2C (HTTP/2 Cleartext) サーバー（設定されている場合のみ）
    #[cfg(feature = "http2")]
    if loaded_config.h2c_enabled {
        let h2c_addr_str = loaded_config
            .h2c_listen
            .clone()
            .unwrap_or_else(|| loaded_config.listen_addr.clone());

        let h2c_addr: SocketAddr = match h2c_addr_str.parse() {
            Ok(addr) => addr,
            Err(e) => {
                error!("Invalid H2C listen address '{}': {}", h2c_addr_str, e);
                return;
            }
        };

        info!("============================================");
        info!("H2C (HTTP/2 Cleartext) Server");
        info!("H2C Listen Address: {}", h2c_addr);
        info!("H2C Workers: {} (SO_REUSEPORT enabled)", num_threads);
        info!("============================================");

        // 各ワーカースレッドでH2Cリスナーを起動
        let core_ids = core_ids.clone();
        for thread_id in 0..num_threads {
            let balancing = loaded_config.reuseport_balancing;
            let max_conn = loaded_config.global_security.max_concurrent_connections;

            // このスレッドに割り当てるコアIDを決定
            let assigned_core = core_ids.as_ref().map(|ids| {
                let core_index = thread_id % ids.len();
                ids[core_index]
            });

            let h2c_handle = spawn_worker_thread(move || {
                // スレッド開始直後にCPUアフィニティを設定
                if let Some(core_id) = assigned_core {
                    if core_affinity::set_for_current(core_id) {
                        info!(
                            "[H2C Worker {}] Pinned to CPU core {:?}",
                            thread_id, core_id
                        );
                    } else {
                        warn!(
                            "[H2C Worker {}] Failed to pin to CPU core {:?}",
                            thread_id, core_id
                        );
                    }
                }

                crate::runtime::block_on(async move {
                    let listener =
                        match create_listener(h2c_addr, balancing, num_threads, thread_id) {
                            Ok(l) => l,
                            Err(e) => {
                                error!("[H2C Worker {}] Bind error: {}", thread_id, e);
                                return;
                            }
                        };

                    info!("[H2C Worker {}] Started", thread_id);

                    // F-46: H2C 接続ハンドラの型付きタスクプール
                    let conn_pool = crate::runtime::TaskPool::new();

                    loop {
                        // Shutdown チェック
                        if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                            info!("[H2C Worker {}] Shutting down...", thread_id);
                            break;
                        }

                        // タイムアウト付きaccept
                        let accept_result =
                            timeout(Duration::from_secs(1), listener.accept()).await;

                        let (mut stream, peer_addr) = match accept_result {
                            Ok(Ok(s)) => s,
                            Ok(Err(e)) => {
                                error!("[H2C Worker {}] Accept error: {}", thread_id, e);
                                continue;
                            }
                            Err(_) => {
                                // タイムアウト - ループを継続してshutdownチェック
                                continue;
                            }
                        };

                        // F-35: 最前線 IP ブロックリスト（プロトコル検出前に弾く）
                        if crate::config::is_ip_blocked(peer_addr.ip()) {
                            continue;
                        }

                        // 同時接続数制限チェック
                        if max_conn > 0 {
                            let current = CURRENT_CONNECTIONS.load(Ordering::Relaxed);
                            if current >= max_conn {
                                warn!("[H2C Worker {}] Connection limit reached ({}/{}), rejecting connection from {}", 
                                      thread_id, current, max_conn, peer_addr);
                                drop(stream);
                                continue;
                            }
                        }

                        let _ = stream.set_nodelay(true);

                        // H2C接続処理をspawn（パニック耐性あり・型付きプール）
                        spawn_pooled_with_panic_catch(&conn_pool, async move {
                            let _guard = ConnectionGuard::new();
                            // H2C専用リスナーでも、プロトコル検出を実行して初期データを取得
                            // これにより、クライアントがまだプリフェースを送信していない場合でも
                            // 正しく処理できる
                            let (protocol_type, initial_data) =
                                detect_protocol_with_buffer(&mut stream).await;

                            match protocol_type {
                                ProtocolType::H2C => {
                                    // H2C接続処理
                                    handle_h2c_connection(
                                        stream,
                                        &peer_addr.ip().to_string(),
                                        initial_data,
                                    )
                                    .await;
                                }
                                ProtocolType::Http11 => {
                                    // HTTP/1.1はH2C専用サーバーではサポートしない
                                    warn!("[H2C Worker] Plain HTTP/1.1 not supported on H2C-only server, closing connection from {}", peer_addr);
                                }
                                ProtocolType::TLS => {
                                    // TLSはH2C専用サーバーではサポートしない
                                    warn!("[H2C Worker] TLS not supported on H2C-only server, closing connection from {}", peer_addr);
                                }
                                ProtocolType::Unknown => {
                                    warn!(
                                        "[H2C Worker] Unknown protocol from {}, closing connection",
                                        peer_addr
                                    );
                                }
                            }
                        });
                    }

                    // グレースフルシャットダウン: 既存接続の完了を待機
                    drain_connections("H2C Worker", thread_id).await;

                    info!("[H2C Worker {}] Stopped", thread_id);
                });
            });
            handles.push(h2c_handle);
        }
    }

    // L4 (TCP/UDP) ストリームプロキシリスナーを起動（F-18）
    #[cfg(feature = "l4-proxy")]
    {
        if !loaded_config.l4_listeners.is_empty() {
            info!("============================================");
            info!("L4 Stream Proxy");
            info!("Listeners: {}", loaded_config.l4_listeners.len());
            info!("============================================");
            crate::l4::server::spawn_l4_listeners(&loaded_config.l4_listeners);
        }
    }

    // HTTP/3 ワーカーが証明書データをクローンするまで短時間待機
    // その後、LoadedConfig の証明書データをセキュアにゼロ化
    #[cfg(feature = "http3")]
    if loaded_config.http3_enabled {
        // ワーカースレッドが Arc 参照をドロップするまで少し待機
        // 理由付き allow: メインスレッドで一度だけ実行する起動後の待機（ワーカーの Arc ドロップ待ち）。イベントループ外。
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(std::time::Duration::from_millis(100));

        // LoadedConfig の証明書データをセキュアにゼロ化
        secure_clear_arc_vec(
            &mut loaded_config.tls_cert_pem,
            "TLS certificate (LoadedConfig)",
        );
        secure_clear_arc_vec(
            &mut loaded_config.tls_key_pem,
            "TLS private key (LoadedConfig)",
        );

        info!("[Security] Pre-loaded TLS credentials have been securely cleared from memory");
    }

    for (index, handle) in handles.into_iter().enumerate() {
        match handle.join() {
            Ok(()) => {
                debug!("Worker thread {} exited normally", index);
            }
            Err(e) => {
                error!("Worker thread {} panicked: {:?}", index, e);
            }
        }
    }

    // F-10: OpenTelemetry エクスポータをクリーンに停止（最終 flush 込み）
    #[cfg(feature = "opentelemetry")]
    crate::otel::shutdown_global();

    info!("Server shutdown complete");
}
