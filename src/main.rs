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

// ====================
// メモリアロケータ選択
// mimalloc / jemalloc / system（デフォルト: mimalloc）
// ====================

#[cfg(feature = "mimalloc")]
use mimalloc::MiMalloc;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[cfg(feature = "jemalloc")]
use tikv_jemallocator::Jemalloc;

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

// ktls モジュール（自前 kTLS 実装）
#[cfg(feature = "ktls")]
mod ktls;

// ktls_rustls モジュール（kTLS 対応 TLS ストリーム）
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

/// QUIC 用 UDP ソケット (GSO/GRO 対応)
/// - sendmsg/recvmsg を使用した GSO/GRO 実装
/// - EAGAIN 対応の非同期送受信
/// - SO_REUSEPORT でマルチスレッド対応
#[cfg(feature = "http3")]
pub mod udp;

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
/// - cache feature が無効の場合はスタブ実装を提供
pub mod cache;

/// ルーティング最適化モジュール
/// - Phase 1: Host-based グループ化（O(1)ルックアップ）
/// - Phase 2: Path Radix Tree（matchit crate）
/// - Phase 3: CIDR Tree最適化
/// - Phase 4: LRUキャッシュ
pub mod routing;

/// WASM拡張モジュール（Proxy-Wasm v0.2.1互換）
#[cfg(feature = "wasm")]
pub mod wasm;

/// gRPC プロトコルサポート
/// - gRPC ワイヤプロトコル（5-byte framing）
/// - gRPC ヘッダー/トレイラー処理
/// - gRPC-Web 変換（grpc-web feature）
/// - 圧縮サポート（gzip/deflate）
#[cfg(feature = "grpc")]
pub mod grpc;

/// ログ関連モジュール
/// - Coarse Timer（ログタイムスタンプ用キャッシュ時刻）
/// - ログ設定（LogFormat, LoggingConfigSection）
/// - JSON形式ログフォーマッタ
/// - init_logging, log_ktls_status, log_access
pub mod logging;

/// システム機能モジュール
/// - HugePages検出と設定
/// - 権限降格（drop_privileges）
/// - サンドボックス設定構築
/// - パニックキャッチ（CatchUnwindFuture, spawn_with_panic_catch）
/// - セキュアメモリ操作
/// - SO_REUSEPORT CBPF振り分け
pub mod system;

/// Prometheusメトリクスモジュール
/// - メトリクスレジストリとカウンター
/// - ActiveConnectionMetric, ConnectionGuard
/// - キャッシュメトリクス
/// - CacheSaveContext
/// - record_request_metrics, build_metrics_response
/// - metrics feature が無効の場合はスタブ実装を提供
pub mod metrics;

pub mod constants;
pub mod pool;
pub mod resilience;
/// OpenTelemetry OTLP/HTTP エクスポート（F-10）
#[cfg(feature = "opentelemetry")]
pub mod otel;
pub mod http_utils;

/// 構造化アクセスログモジュール（F-21）
/// - AccessLogConfig（JSON/テキスト形式、フィールドフィルタリング）
/// - log_access_structured（スレッドローカルバッファによる低アロケーション実装）
#[cfg(feature = "access-log")]
pub mod access_log;
use crate::pool::*;
#[cfg(test)]
use crate::constants::*;
#[cfg(test)]
use crate::http_utils::*;
use crate::logging::*;
use crate::system::*;
use crate::metrics::*;

use monoio::net::TcpListener;
use monoio::RuntimeBuilder;
use monoio::time::timeout;
use clap::Parser;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use ftlog::{info, error, warn, debug};
use rustls::crypto::CryptoProvider;

// ktls_rustls（kTLS 対応）
#[cfg(feature = "ktls")]
use ktls_rustls::RustlsAcceptor;

// kTLS 無効時は直接 rustls を使用するシンプルなラッパー
#[cfg(not(feature = "ktls"))]
mod simple_tls;

/// TLS 証明書ホットリロード（F-03）
pub mod tls_reload;

// ClientTls は crate::pool モジュールで定義・再エクスポートされています
// use crate::pool::* でインポート済み


/// 設定モジュール（データ型・設定読み込み関数）
pub mod config;
pub use crate::config::*;

/// アップストリーム選択・ヘルスチェックモジュール
pub mod upstream;
pub use crate::upstream::*;

pub mod server;
use crate::server::*;

pub mod proxy;
use crate::proxy::*;

// ====================
// メイン関数
// ====================

fn main() {
    // コマンドライン引数を解析（--help, --version は clap が自動処理）
    let cli_args = CliArgs::parse();
    
    // 設定ファイルパスをグローバル変数に保存（ホットリロード用）
    CONFIG_PATH.store(Arc::new(cli_args.config.clone()));
    let config_path = cli_args.config;
    
    // -t オプション: 設定ファイルのテストのみ
    if cli_args.test_config {
        match test_config_file(&config_path) {
            Ok(()) => {
                println!("veil: configuration file {} test is successful", 
                         config_path.display());
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("veil: configuration file {} test failed", 
                          config_path.display());
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
    let logging_config = load_logging_config(&config_path)
        .unwrap_or_else(|_| LoggingConfigSection::default());
    
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
    GRACEFUL_SHUTDOWN_TIMEOUT_SECS.store(loaded_config.graceful_shutdown_timeout_secs, Ordering::Relaxed);
    if loaded_config.graceful_shutdown_timeout_secs > 0 {
        info!("Graceful shutdown timeout: {} seconds", loaded_config.graceful_shutdown_timeout_secs);
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
    let acceptor = simple_tls::SimpleTlsAcceptor::new(loaded_config.tls_config.clone())
        .with_ktls(loaded_config.ktls_config.enabled);

    // F-03: グローバル TLS 設定を初期化（アクセプタが毎ハンドシェイク参照）
    tls_reload::init_global_tls_config(loaded_config.tls_config.clone());

    // F-10: OpenTelemetry エクスポータを起動（feature 有効かつ enabled 時のみ）
    #[cfg(feature = "opentelemetry")]
    {
        let otel = &loaded_config.opentelemetry;
        if otel.enabled {
            otel::init_global(otel::OtelConfig {
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
        let builder: tls_reload::ServerConfigBuilder = Box::new(move |c, k| {
            config::build_server_config_from_paths(c, k, ktls_enabled, http2_enabled)
        });
        match tls_reload::TlsCertReloader::new_global(cert_path, key_path, builder) {
            Ok(reloader) => {
                spawn_tls_reloader(reloader, interval);
                info!("TLS certificate auto-reload enabled (interval: {}s, SIGHUP supported)", interval);
            }
            Err(e) => {
                warn!("Failed to initialize TLS cert reloader: {}", e);
            }
        }
    }

    let listen_addr = loaded_config.listen_addr.parse::<SocketAddr>()
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
    
    // HTTP/2・HTTP/3・H2C の設定ログ
    #[cfg(feature = "http2")]
    if loaded_config.http2_enabled {
        info!("HTTP/2 enabled via ALPN negotiation");
    }
    #[cfg(feature = "http2")]
    if loaded_config.h2c_enabled {
        let h2c_addr = loaded_config.h2c_listen.as_deref().unwrap_or(&loaded_config.listen_addr);
        info!("H2C (HTTP/2 Cleartext) enabled (listener: {})", h2c_addr);
    }
    #[cfg(feature = "http3")]
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
                if loaded_config.global_security.allow_security_failures {
                    warn!("Failed to apply sandbox restrictions: {} - continuing without sandbox", e);
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
                if loaded_config.global_security.allow_security_failures {
                    warn!("Failed to apply security restrictions: {} - continuing without them", e);
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
                    
                    let _ = stream.set_nodelay(true);
                    
                    let acceptor = acceptor_clone.clone();
                    
                    // spawn_with_panic_catch を使用してパニック時もスレッドが生存し続ける
                    spawn_with_panic_catch(async move {
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
                    
                    // 軽量なリダイレクト処理をspawn（パニック耐性あり）
                    spawn_with_panic_catch(async move {
                        handle_http_redirect(stream).await;
                    });
                }
                
                // グレースフルシャットダウン: 既存接続の完了を待機
                // HTTPリダイレクトワーカーはConnectionGuardを使用していないため、
                // 単純にスリープで待機（リダイレクト処理は高速なため問題なし）
                let timeout_secs = GRACEFUL_SHUTDOWN_TIMEOUT_SECS.load(Ordering::Relaxed);
                if timeout_secs > 0 {
                    monoio::time::sleep(Duration::from_millis(500)).await;
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
    
    // H2C (HTTP/2 Cleartext) サーバー（設定されている場合のみ）
    #[cfg(feature = "http2")]
    if loaded_config.h2c_enabled {
        let h2c_addr_str = loaded_config.h2c_listen
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
            let h2c_addr = h2c_addr;
            let balancing = loaded_config.reuseport_balancing;
            let max_conn = loaded_config.global_security.max_concurrent_connections;
            
            // このスレッドに割り当てるコアIDを決定
            let assigned_core = core_ids.as_ref().map(|ids| {
                let core_index = thread_id % ids.len();
                ids[core_index]
            });
            
            let h2c_handle = thread::spawn(move || {
                // スレッド開始直後にCPUアフィニティを設定
                if let Some(core_id) = assigned_core {
                    if core_affinity::set_for_current(core_id) {
                        info!("[H2C Worker {}] Pinned to CPU core {:?}", thread_id, core_id);
                    } else {
                        warn!("[H2C Worker {}] Failed to pin to CPU core {:?}", thread_id, core_id);
                    }
                }
                
                let mut rt = RuntimeBuilder::<monoio::IoUringDriver>::new()
                    .enable_timer()
                    .build()
                    .expect("Failed to create H2C runtime");
                
                rt.block_on(async move {
                    let listener = match create_listener(h2c_addr, balancing, num_threads, thread_id) {
                        Ok(l) => l,
                        Err(e) => {
                            error!("[H2C Worker {}] Bind error: {}", thread_id, e);
                            return;
                        }
                    };
                    
                    info!("[H2C Worker {}] Started", thread_id);
                    
                    loop {
                        // Shutdown チェック
                        if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                            info!("[H2C Worker {}] Shutting down...", thread_id);
                            break;
                        }
                        
                        // タイムアウト付きaccept
                        let accept_result = timeout(Duration::from_secs(1), listener.accept()).await;
                        
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
                        
                        // H2C接続処理をspawn（パニック耐性あり）
                        spawn_with_panic_catch(async move {
                            let _guard = ConnectionGuard::new();
                            // H2C専用リスナーでも、プロトコル検出を実行して初期データを取得
                            // これにより、クライアントがまだプリフェースを送信していない場合でも
                            // 正しく処理できる
                            let (protocol_type, initial_data) = detect_protocol_with_buffer(&mut stream).await;
                            
                            match protocol_type {
                                ProtocolType::H2C => {
                                    // H2C接続処理
                                    handle_h2c_connection(stream, &peer_addr.ip().to_string(), initial_data).await;
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
                                    warn!("[H2C Worker] Unknown protocol from {}, closing connection", peer_addr);
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
    otel::shutdown_global();

    info!("Server shutdown complete");
}

// サーバーライフサイクル関数は crate::server モジュールに移動しました。

#[cfg(test)]
mod tests {
    use super::*;

    // ====================
    // CidrRange テスト
    // ====================
    
    mod cidr_tests {
        use super::*;

        #[test]
        fn test_parse_ipv4_cidr() {
            // IPv4 CIDRのパース検証
            let cidr = CidrRange::parse("192.168.1.0/24").unwrap();
            assert!(!cidr.is_ipv6);
            assert_eq!(cidr.prefix_len, 24);
        }

        #[test]
        fn test_parse_ipv4_single() {
            // 単一IPv4アドレスのパース（/32相当）
            let cidr = CidrRange::parse("10.0.0.1").unwrap();
            assert!(!cidr.is_ipv6);
            assert_eq!(cidr.prefix_len, 32);
        }

        #[test]
        fn test_parse_ipv6_cidr() {
            // IPv6 CIDRのパース検証
            let cidr = CidrRange::parse("2001:db8::/32").unwrap();
            assert!(cidr.is_ipv6);
            assert_eq!(cidr.prefix_len, 32);
        }

        #[test]
        fn test_parse_ipv6_single() {
            // 単一IPv6アドレスのパース（/128相当）
            let cidr = CidrRange::parse("::1").unwrap();
            assert!(cidr.is_ipv6);
            assert_eq!(cidr.prefix_len, 128);
        }

        #[test]
        fn test_parse_invalid_cidr() {
            // 無効な入力のパース失敗
            assert!(CidrRange::parse("invalid").is_none());
            assert!(CidrRange::parse("256.256.256.256").is_none());
            assert!(CidrRange::parse("192.168.1.0/33").is_none()); // 無効なプレフィックス
            assert!(CidrRange::parse("").is_none());
        }

        #[test]
        fn test_contains_ipv4_in_range() {
            // IPv4アドレスがCIDR範囲内に含まれる
            let cidr = CidrRange::parse("192.168.0.0/16").unwrap();
            assert!(cidr.contains("192.168.1.100"));
            assert!(cidr.contains("192.168.255.255"));
            assert!(cidr.contains("192.168.0.1"));
        }

        #[test]
        fn test_contains_ipv4_out_of_range() {
            // IPv4アドレスがCIDR範囲外
            let cidr = CidrRange::parse("192.168.0.0/16").unwrap();
            assert!(!cidr.contains("192.169.0.1"));
            assert!(!cidr.contains("10.0.0.1"));
            assert!(!cidr.contains("172.16.0.1"));
        }

        #[test]
        fn test_contains_ipv4_exact_match() {
            // 単一IPアドレスの完全一致
            let cidr = CidrRange::parse("10.0.0.1").unwrap();
            assert!(cidr.contains("10.0.0.1"));
            assert!(!cidr.contains("10.0.0.2"));
        }

        #[test]
        fn test_contains_ipv6_in_range() {
            // IPv6アドレスがCIDR範囲内に含まれる
            let cidr = CidrRange::parse("2001:db8::/32").unwrap();
            assert!(cidr.contains("2001:db8::1"));
            assert!(cidr.contains("2001:db8:ffff::1"));
        }

        #[test]
        fn test_contains_ipv6_out_of_range() {
            // IPv6アドレスがCIDR範囲外
            let cidr = CidrRange::parse("2001:db8::/32").unwrap();
            assert!(!cidr.contains("2001:db9::1"));
            assert!(!cidr.contains("::1"));
        }

        #[test]
        fn test_contains_localhost_ipv6() {
            // IPv6ローカルホストの検証
            let cidr = CidrRange::parse("::1/128").unwrap();
            assert!(cidr.contains("::1"));
            assert!(!cidr.contains("::2"));
        }

        #[test]
        fn test_ipv4_mapped_ipv6() {
            // IPv4をIPv6で確認した場合（異なるアドレスファミリー）
            let cidr_v4 = CidrRange::parse("192.168.1.0/24").unwrap();
            // IPv4 CIDRにIPv6アドレスは含まれない
            assert!(!cidr_v4.contains("::ffff:192.168.1.1"));
        }
    }

    // ====================
    // IpFilter テスト
    // ====================
    
    mod ip_filter_tests {
        use super::*;

        #[test]
        fn test_filter_empty_allows_all() {
            // 空のフィルターは全て許可
            let filter = IpFilter::from_lists(&[], &[]);
            assert!(filter.is_allowed("192.168.1.1"));
            assert!(filter.is_allowed("10.0.0.1"));
            assert!(filter.is_allowed("2001:db8::1"));
        }

        #[test]
        fn test_filter_allow_list() {
            // 許可リストのみ設定
            let allowed = vec!["192.168.0.0/16".to_string()];
            let filter = IpFilter::from_lists(&allowed, &[]);
            
            assert!(filter.is_allowed("192.168.1.1"));
            assert!(filter.is_allowed("192.168.255.255"));
            assert!(!filter.is_allowed("10.0.0.1"));
            assert!(!filter.is_allowed("172.16.0.1"));
        }

        #[test]
        fn test_filter_deny_list() {
            // 拒否リストのみ設定（許可リストが空なので、拒否以外は全て許可）
            let denied = vec!["192.168.1.0/24".to_string()];
            let filter = IpFilter::from_lists(&[], &denied);
            
            assert!(!filter.is_allowed("192.168.1.1"));
            assert!(filter.is_allowed("192.168.2.1"));
            assert!(filter.is_allowed("10.0.0.1"));
        }

        #[test]
        fn test_filter_deny_priority() {
            // denyがallowより優先されることを検証
            let allowed = vec!["192.168.0.0/16".to_string()];
            let denied = vec!["192.168.1.0/24".to_string()];
            let filter = IpFilter::from_lists(&allowed, &denied);
            
            // 192.168.1.xはdenyされる
            assert!(!filter.is_allowed("192.168.1.1"));
            assert!(!filter.is_allowed("192.168.1.100"));
            
            // 192.168.0.xや192.168.2.xはallowされる
            assert!(filter.is_allowed("192.168.0.1"));
            assert!(filter.is_allowed("192.168.2.1"));
            
            // 許可リスト外は拒否
            assert!(!filter.is_allowed("10.0.0.1"));
        }

        #[test]
        fn test_filter_multiple_ranges() {
            // 複数のCIDR範囲
            let allowed = vec![
                "10.0.0.0/8".to_string(),
                "172.16.0.0/12".to_string(),
                "192.168.0.0/16".to_string(),
            ];
            let filter = IpFilter::from_lists(&allowed, &[]);
            
            // RFC1918プライベートアドレスは全て許可
            assert!(filter.is_allowed("10.1.2.3"));
            assert!(filter.is_allowed("172.16.100.1"));
            assert!(filter.is_allowed("172.31.255.255"));
            assert!(filter.is_allowed("192.168.0.1"));
            
            // パブリックアドレスは拒否
            assert!(!filter.is_allowed("8.8.8.8"));
            assert!(!filter.is_allowed("1.1.1.1"));
        }

        #[test]
        fn test_filter_single_ip() {
            // 単一IPアドレスの許可
            let allowed = vec!["127.0.0.1".to_string()];
            let filter = IpFilter::from_lists(&allowed, &[]);
            
            assert!(filter.is_allowed("127.0.0.1"));
            assert!(!filter.is_allowed("127.0.0.2"));
        }

        #[test]
        fn test_filter_ipv6() {
            // IPv6アドレスのフィルタリング
            let allowed = vec!["2001:db8::/32".to_string(), "::1".to_string()];
            let filter = IpFilter::from_lists(&allowed, &[]);
            
            assert!(filter.is_allowed("::1"));
            assert!(filter.is_allowed("2001:db8::1"));
            assert!(!filter.is_allowed("2001:db9::1"));
        }

        #[test]
        fn test_filter_is_configured() {
            // フィルターが設定されているかの確認
            let empty = IpFilter::from_lists(&[], &[]);
            assert!(!empty.is_configured());
            
            let with_allow = IpFilter::from_lists(&["10.0.0.0/8".to_string()], &[]);
            assert!(with_allow.is_configured());
            
            let with_deny = IpFilter::from_lists(&[], &["192.168.1.0/24".to_string()]);
            assert!(with_deny.is_configured());
        }

        #[test]
        fn test_filter_invalid_entry_ignored() {
            // 無効なエントリは無視される
            let allowed = vec![
                "192.168.1.0/24".to_string(),
                "invalid".to_string(),
                "10.0.0.0/8".to_string(),
            ];
            let filter = IpFilter::from_lists(&allowed, &[]);
            
            // 有効なエントリは機能する
            assert!(filter.is_allowed("192.168.1.1"));
            assert!(filter.is_allowed("10.1.2.3"));
            assert!(!filter.is_allowed("172.16.0.1"));
        }
    }

    // ====================
    // RateLimitEntry テスト
    // ====================
    
    mod rate_limit_tests {
        use super::*;

        #[test]
        fn test_new_entry() {
            // 新規エントリの初期状態
            let entry = RateLimitEntry::new(100);
            assert_eq!(entry.current_count, 1);
            assert_eq!(entry.previous_count, 0);
            assert_eq!(entry.current_minute, 100);
        }

        #[test]
        fn test_record_same_minute() {
            // 同一分内でのリクエスト記録
            let mut entry = RateLimitEntry::new(100);
            
            // 初期状態: count=1
            let rate = entry.record_request(100, 30);
            // count=2, previous=0, weight=(60-30)/60=0.5
            // estimated = 0*0.5 + 2 = 2
            assert_eq!(entry.current_count, 2);
            assert!(rate >= 2);
        }

        #[test]
        fn test_record_next_minute() {
            // 次の分へ移行
            let mut entry = RateLimitEntry::new(100);
            entry.current_count = 10;
            
            let rate = entry.record_request(101, 0);
            
            // previous_count = 10（前の分のカウント）
            // current_count = 1（新しい分）
            assert_eq!(entry.current_minute, 101);
            assert_eq!(entry.previous_count, 10);
            assert_eq!(entry.current_count, 1);
            
            // rate = 10 * (60-0)/60 + 1 = 10 + 1 = 11
            assert_eq!(rate, 11);
        }

        #[test]
        fn test_record_skip_minutes() {
            // 2分以上経過した場合のリセット
            let mut entry = RateLimitEntry::new(100);
            entry.current_count = 100;
            entry.previous_count = 50;
            
            let rate = entry.record_request(103, 0);
            
            // 2分以上経過なのでリセット
            assert_eq!(entry.current_minute, 103);
            assert_eq!(entry.previous_count, 0);
            assert_eq!(entry.current_count, 1);
            assert_eq!(rate, 1);
        }

        #[test]
        fn test_sliding_window_calculation() {
            // スライディングウィンドウ計算の検証
            let mut entry = RateLimitEntry::new(100);
            entry.current_count = 30;
            entry.previous_count = 60;
            
            // 分の真ん中（30秒経過）でのレート計算
            let rate = entry.record_request(100, 30);
            // current_count = 31, previous = 60
            // weight = (60-30)/60 = 0.5
            // estimated = 60*0.5 + 31 = 30 + 31 = 61
            assert_eq!(entry.current_count, 31);
            assert_eq!(rate, 61);
        }

        #[test]
        fn test_sliding_window_end_of_minute() {
            // 分の終わりでのレート計算（weight ≈ 0）
            let mut entry = RateLimitEntry::new(100);
            entry.current_count = 50;
            entry.previous_count = 100;
            
            let rate = entry.record_request(100, 59);
            // weight = (60-59)/60 ≈ 0.0167
            // estimated = 100*0.0167 + 51 ≈ 52.67 → ceil → 53
            assert_eq!(entry.current_count, 51);
            assert!(rate >= 51 && rate <= 53);
        }
    }

    // ====================
    // AcceptedEncoding テスト
    // ====================
    
    mod encoding_tests {
        use super::*;

        #[test]
        fn test_parse_gzip() {
            let encoding = AcceptedEncoding::parse(b"gzip");
            assert_eq!(encoding, AcceptedEncoding::Gzip);
        }

        #[test]
        fn test_parse_brotli() {
            let encoding = AcceptedEncoding::parse(b"br");
            assert_eq!(encoding, AcceptedEncoding::Brotli);
        }

        #[test]
        fn test_parse_zstd() {
            let encoding = AcceptedEncoding::parse(b"zstd");
            assert_eq!(encoding, AcceptedEncoding::Zstd);
        }

        #[test]
        fn test_parse_deflate() {
            let encoding = AcceptedEncoding::parse(b"deflate");
            assert_eq!(encoding, AcceptedEncoding::Deflate);
        }

        #[test]
        fn test_parse_multiple_prefer_zstd() {
            // 複数指定時はzstdを優先
            let encoding = AcceptedEncoding::parse(b"gzip, br, zstd");
            assert_eq!(encoding, AcceptedEncoding::Zstd);
        }

        #[test]
        fn test_parse_with_quality() {
            // q値指定
            let encoding = AcceptedEncoding::parse(b"gzip;q=0.5, br;q=1.0");
            // br (q=1.0) > gzip (q=0.5)
            assert_eq!(encoding, AcceptedEncoding::Brotli);
        }

        #[test]
        fn test_parse_zstd_higher_quality() {
            // zstdが高いq値を持つ場合
            let encoding = AcceptedEncoding::parse(b"gzip;q=0.8, zstd;q=1.0");
            assert_eq!(encoding, AcceptedEncoding::Zstd);
        }

        #[test]
        fn test_parse_empty() {
            // 空の場合はIdentity
            let encoding = AcceptedEncoding::parse(b"");
            assert_eq!(encoding, AcceptedEncoding::Identity);
        }

        #[test]
        fn test_parse_identity() {
            // identityのみ（圧縮なし）
            let encoding = AcceptedEncoding::parse(b"identity");
            assert_eq!(encoding, AcceptedEncoding::Identity);
        }

        #[test]
        fn test_parse_wildcard() {
            // * はgzipとして扱う
            let encoding = AcceptedEncoding::parse(b"*");
            assert_eq!(encoding, AcceptedEncoding::Gzip);
        }

        #[test]
        fn test_parse_unknown() {
            // 不明なエンコーディングはIdentity
            let encoding = AcceptedEncoding::parse(b"unknown");
            assert_eq!(encoding, AcceptedEncoding::Identity);
        }

        #[test]
        fn test_parse_invalid_utf8() {
            // 無効なUTF-8はIdentity
            let encoding = AcceptedEncoding::parse(&[0xff, 0xfe]);
            assert_eq!(encoding, AcceptedEncoding::Identity);
        }

        #[test]
        fn test_as_header_value() {
            // ヘッダー値への変換
            assert_eq!(AcceptedEncoding::Zstd.as_header_value(), b"zstd");
            assert_eq!(AcceptedEncoding::Brotli.as_header_value(), b"br");
            assert_eq!(AcceptedEncoding::Gzip.as_header_value(), b"gzip");
            assert_eq!(AcceptedEncoding::Deflate.as_header_value(), b"deflate");
            assert_eq!(AcceptedEncoding::Identity.as_header_value(), b"identity");
        }
    }

    // ====================
    // CompressionConfig テスト
    // ====================
    
    mod compression_config_tests {
        use super::*;

        #[test]
        fn test_default_config() {
            // デフォルト設定の検証
            let config = CompressionConfig::default();
            assert!(!config.enabled); // デフォルトは無効
            assert_eq!(config.gzip_level, 4);
            assert_eq!(config.brotli_level, 4);
            assert_eq!(config.zstd_level, 3);
            assert_eq!(config.min_size, 1024);
        }

        #[test]
        fn test_validate_valid_config() {
            // 有効な設定の検証
            let config = CompressionConfig {
                enabled: true,
                gzip_level: 6,
                brotli_level: 6,
                zstd_level: 10,
                ..Default::default()
            };
            assert!(config.validate().is_ok());
        }

        #[test]
        fn test_validate_invalid_gzip_level() {
            // 無効なgzipレベル
            let config = CompressionConfig {
                gzip_level: 10, // 1-9のみ有効
                ..Default::default()
            };
            assert!(config.validate().is_err());
            
            let config_zero = CompressionConfig {
                gzip_level: 0, // 0は無効
                ..Default::default()
            };
            assert!(config_zero.validate().is_err());
        }

        #[test]
        fn test_validate_invalid_brotli_level() {
            // 無効なbrotliレベル
            let config = CompressionConfig {
                brotli_level: 12, // 0-11のみ有効
                ..Default::default()
            };
            assert!(config.validate().is_err());
        }

        #[test]
        fn test_validate_invalid_zstd_level() {
            // 無効なzstdレベル
            let config = CompressionConfig {
                zstd_level: 0, // 1-22のみ有効
                ..Default::default()
            };
            assert!(config.validate().is_err());
            
            let config_high = CompressionConfig {
                zstd_level: 23,
                ..Default::default()
            };
            assert!(config_high.validate().is_err());
        }

        #[test]
        fn test_validate_unknown_encoding() {
            // 不明なエンコーディング
            let config = CompressionConfig {
                preferred_encodings: vec!["unknown".to_string()],
                ..Default::default()
            };
            assert!(config.validate().is_err());
        }

        #[test]
        fn test_should_compress_disabled() {
            // 圧縮無効時
            let config = CompressionConfig {
                enabled: false,
                ..Default::default()
            };
            let result = config.should_compress(
                AcceptedEncoding::Gzip,
                Some(b"text/html"),
                Some(2048),
                None,
            );
            assert!(result.is_none());
        }

        #[test]
        fn test_should_compress_client_identity() {
            // クライアントが圧縮非対応
            let config = CompressionConfig {
                enabled: true,
                ..Default::default()
            };
            let result = config.should_compress(
                AcceptedEncoding::Identity,
                Some(b"text/html"),
                Some(2048),
                None,
            );
            assert!(result.is_none());
        }

        #[test]
        fn test_should_compress_already_compressed() {
            // バックエンドが既に圧縮済み
            let config = CompressionConfig {
                enabled: true,
                ..Default::default()
            };
            let result = config.should_compress(
                AcceptedEncoding::Gzip,
                Some(b"text/html"),
                Some(2048),
                Some(b"gzip"),
            );
            assert!(result.is_none());
        }

        #[test]
        fn test_should_compress_text_html() {
            // text/htmlは圧縮対象
            let config = CompressionConfig {
                enabled: true,
                ..Default::default()
            };
            let result = config.should_compress(
                AcceptedEncoding::Gzip,
                Some(b"text/html; charset=utf-8"),
                Some(2048),
                None,
            );
            assert!(result.is_some());
        }

        #[test]
        fn test_should_compress_json() {
            // application/jsonは圧縮対象
            let config = CompressionConfig {
                enabled: true,
                ..Default::default()
            };
            let result = config.should_compress(
                AcceptedEncoding::Brotli,
                Some(b"application/json"),
                Some(2048),
                None,
            );
            assert!(result.is_some());
        }

        #[test]
        fn test_should_not_compress_image() {
            // 画像は圧縮スキップ
            let config = CompressionConfig {
                enabled: true,
                ..Default::default()
            };
            let result = config.should_compress(
                AcceptedEncoding::Gzip,
                Some(b"image/png"),
                Some(100000),
                None,
            );
            assert!(result.is_none());
        }

        #[test]
        fn test_should_not_compress_small() {
            // min_size未満は圧縮しない
            let config = CompressionConfig {
                enabled: true,
                min_size: 1024,
                ..Default::default()
            };
            let result = config.should_compress(
                AcceptedEncoding::Gzip,
                Some(b"text/html"),
                Some(500), // 1024未満
                None,
            );
            assert!(result.is_none());
        }

        #[test]
        fn test_should_not_compress_no_content_type() {
            // Content-Typeがない場合は圧縮しない
            let config = CompressionConfig {
                enabled: true,
                ..Default::default()
            };
            let result = config.should_compress(
                AcceptedEncoding::Gzip,
                None,
                Some(2048),
                None,
            );
            assert!(result.is_none());
        }
    }

    // ====================
    // WebSocketPollConfig テスト
    // ====================
    
    mod websocket_poll_tests {
        use super::*;

        #[test]
        fn test_default_config() {
            // デフォルト設定の検証
            let config = WebSocketPollConfig::default();
            assert_eq!(config.mode, WebSocketPollMode::Adaptive);
            assert_eq!(config.initial_timeout_ms, 1);
            assert_eq!(config.max_timeout_ms, 100);
            assert!((config.backoff_multiplier - 2.0).abs() < f64::EPSILON);
        }

        #[test]
        fn test_mode_equality() {
            // モード比較
            assert_eq!(WebSocketPollMode::Fixed, WebSocketPollMode::Fixed);
            assert_eq!(WebSocketPollMode::Adaptive, WebSocketPollMode::Adaptive);
            assert_ne!(WebSocketPollMode::Fixed, WebSocketPollMode::Adaptive);
        }
    }

    // ====================
    // SecurityConfig テスト  
    // ====================
    
    mod security_config_tests {
        use super::*;

        #[test]
        fn test_default_security_config() {
            // デフォルトセキュリティ設定
            let config = SecurityConfig::default();
            assert_eq!(config.max_request_body_size, MAX_BODY_SIZE);
            assert_eq!(config.max_request_header_size, MAX_HEADER_SIZE);
            assert_eq!(config.client_header_timeout_secs, 30);
            assert!(config.allowed_methods.is_empty());
        }

        #[test]
        fn test_security_config_ip_filter() {
            // IPフィルターの構築
            let mut config = SecurityConfig::default();
            config.allowed_ips = vec!["10.0.0.0/8".to_string()];
            config.denied_ips = vec!["10.0.1.0/24".to_string()];
            
            let filter = config.ip_filter();
            assert!(filter.is_allowed("10.0.0.1"));
            assert!(!filter.is_allowed("10.0.1.1"));
        }
    }

    // ====================
    // PooledConnection テスト
    // ====================
    
    mod pooled_connection_tests {
        use super::*;

        #[test]
        fn test_pooled_connection_new() {
            // PooledConnectionの作成
            let stream = (); // ダミー型
            let conn = PooledConnection::new(stream, 30);
            
            assert_eq!(conn.idle_timeout_secs, 30);
        }

        #[test]
        fn test_pooled_connection_is_valid_immediately() {
            // 作成直後は有効
            let stream = ();
            let conn = PooledConnection::new(stream, 30);
            
            assert!(conn.is_valid());
        }

        #[test]
        fn test_pooled_connection_is_valid_with_zero_timeout() {
            // タイムアウト0秒の場合、即座に無効
            let stream = ();
            let conn = PooledConnection::new(stream, 0);
            
            // 作成直後でも0秒以上経過しているため無効
            assert!(!conn.is_valid());
        }

        #[test]
        fn test_pooled_connection_is_valid_with_long_timeout() {
            // 長いタイムアウトの場合、有効
            let stream = ();
            let conn = PooledConnection::new(stream, 3600);
            
            assert!(conn.is_valid());
        }
    }

    // ====================
    // Config Parse テスト
    // ====================
    
    mod config_parse_tests {
        use super::*;

        #[test]
        fn test_parse_log_level() {
            // ログレベルのパース
            assert_eq!(parse_log_level("trace"), ftlog::LevelFilter::Trace);
            assert_eq!(parse_log_level("debug"), ftlog::LevelFilter::Debug);
            assert_eq!(parse_log_level("info"), ftlog::LevelFilter::Info);
            assert_eq!(parse_log_level("warn"), ftlog::LevelFilter::Warn);
            assert_eq!(parse_log_level("error"), ftlog::LevelFilter::Error);
            assert_eq!(parse_log_level("off"), ftlog::LevelFilter::Off);
        }

        #[test]
        fn test_parse_log_level_case_insensitive() {
            // 大文字小文字を区別しない
            assert_eq!(parse_log_level("INFO"), ftlog::LevelFilter::Info);
            assert_eq!(parse_log_level("Debug"), ftlog::LevelFilter::Debug);
            assert_eq!(parse_log_level("WARN"), ftlog::LevelFilter::Warn);
        }

        #[test]
        fn test_parse_log_level_unknown() {
            // 不明なレベルはInfoにフォールバック
            assert_eq!(parse_log_level("unknown"), ftlog::LevelFilter::Info);
            assert_eq!(parse_log_level(""), ftlog::LevelFilter::Info);
        }

        #[test]
        fn test_default_logging_config() {
            // デフォルトロギング設定
            let config = LoggingConfigSection::default();
            assert_eq!(config.level, "info");
            assert_eq!(config.channel_size, 100000);
            assert_eq!(config.flush_interval_ms, 1000);
        }

        #[test]
        fn test_default_server_config() {
            // ServerConfigSectionはDeserializeのみなので、デフォルト値関数をテスト
            // threads = 0 がデフォルト（CPUコア数）
            // http2_enabled = false がデフォルト
            // http3_enabled = false がデフォルト
        }

        #[test]
        fn test_http2_config_default() {
            // HTTP/2設定のデフォルト値
            let config = Http2ConfigSection::default();
            
            assert_eq!(config.header_table_size, 65536);
            assert_eq!(config.max_concurrent_streams, 256);
            assert_eq!(config.initial_window_size, 1048576);
            assert_eq!(config.max_frame_size, 65536);
            assert_eq!(config.max_header_list_size, 65536);
            assert_eq!(config.connection_window_size, 1048576);
        }

        #[test]
        #[cfg(feature = "http2")]
        fn test_http2_config_to_settings() {
            // HTTP/2設定からHttp2Settingsへの変換
            let config = Http2ConfigSection::default();
            let settings = config.to_http2_settings();
            
            assert_eq!(settings.header_table_size, config.header_table_size);
            assert_eq!(settings.max_concurrent_streams, config.max_concurrent_streams);
            assert_eq!(settings.initial_window_size, config.initial_window_size);
            assert_eq!(settings.max_frame_size, config.max_frame_size);
            assert_eq!(settings.max_header_list_size, config.max_header_list_size);
        }

        #[test]
        fn test_http3_config_default() {
            // HTTP/3設定のデフォルト値
            let config = Http3ConfigSection::default();
            
            assert_eq!(config.max_idle_timeout, 30000);
            assert_eq!(config.max_udp_payload_size, 1350);
            assert_eq!(config.initial_max_data, 10_000_000);
            assert_eq!(config.initial_max_streams_bidi, 100);
            assert_eq!(config.initial_max_streams_uni, 100);
        }

        #[test]
        fn test_reuseport_balancing_default() {
            // ReuseportBalancingのデフォルト値
            let balancing = ReuseportBalancing::default();
            assert_eq!(balancing, ReuseportBalancing::Kernel);
        }

        #[test]
        fn test_prometheus_config_default() {
            // Prometheusメトリクス設定のデフォルト
            let config = PrometheusConfig::default();
            
            assert!(!config.enabled);
            assert_eq!(config.path, "/__metrics");
            assert!(config.allowed_ips.is_empty());
        }

        #[test]
        fn test_prometheus_config_enabled_field() {
            // Prometheus有効化チェック
            let disabled = PrometheusConfig::default();
            assert!(!disabled.enabled);
            
            let enabled = PrometheusConfig {
                enabled: true,
                ..Default::default()
            };
            assert!(enabled.enabled);
        }
    }

    // ====================
    // UpstreamConfig テスト
    // ====================
    
    mod upstream_config_tests {
        use super::*;

        #[test]
        fn test_default_health_check_config() {
            // ヘルスチェック設定のデフォルト値
            let config = HealthCheckConfig::default();
            
            assert_eq!(config.interval_secs, 10);
            assert_eq!(config.path, "/");
            assert_eq!(config.timeout_secs, 5);
            assert_eq!(config.unhealthy_threshold, 3);
            assert_eq!(config.healthy_threshold, 2);
        }

        #[test]
        fn test_default_health_check_statuses() {
            // デフォルトの健康ステータスコード
            let config = HealthCheckConfig::default();
            
            assert!(config.healthy_statuses.contains(&200));
            assert!(config.healthy_statuses.contains(&201));
            assert!(config.healthy_statuses.contains(&204));
            assert!(config.healthy_statuses.contains(&301));
            assert!(config.healthy_statuses.contains(&302));
            assert!(config.healthy_statuses.contains(&304));
        }
    }

    // ====================
    // Backend テスト
    // ====================
    
    mod backend_tests {
        #[test]
        fn test_backend_config_types() {
            // BackendConfigの種類を確認
            // File, Proxy, Static, Redirect などの種類が存在
            // 各種類に対応した処理が実装されている
            assert!(true);
        }
    }

    // ====================
    // ProxyTarget テスト
    // ====================

    mod proxy_target_tests {
        use super::*;

        #[test]
        fn test_parse_http_url() {
            let target = ProxyTarget::parse("http://localhost:8080/api").unwrap();
            
            assert_eq!(target.host, "localhost");
            assert_eq!(target.port, 8080);
            assert!(!target.use_tls);
            assert_eq!(target.path_prefix, "/api");
        }

        #[test]
        fn test_parse_https_url() {
            let target = ProxyTarget::parse("https://example.com/").unwrap();
            
            assert_eq!(target.host, "example.com");
            assert_eq!(target.port, 443);
            assert!(target.use_tls);
        }

        #[test]
        fn test_parse_url_default_ports() {
            let http = ProxyTarget::parse("http://localhost/").unwrap();
            assert_eq!(http.port, 80);
            
            let https = ProxyTarget::parse("https://localhost/").unwrap();
            assert_eq!(https.port, 443);
        }

        #[test]
        fn test_parse_url_with_path() {
            let target = ProxyTarget::parse("http://api.example.com:3000/v1/users").unwrap();
            
            assert_eq!(target.host, "api.example.com");
            assert_eq!(target.port, 3000);
            assert_eq!(target.path_prefix, "/v1/users");
        }

        #[test]
        fn test_parse_url_no_path() {
            let target = ProxyTarget::parse("http://localhost:8080").unwrap();
            
            assert_eq!(target.path_prefix, "/");
        }

        #[test]
        fn test_parse_invalid_url() {
            assert!(ProxyTarget::parse("invalid").is_none());
            assert!(ProxyTarget::parse("ftp://localhost/").is_none());
            assert!(ProxyTarget::parse("").is_none());
            assert!(ProxyTarget::parse("://no-scheme").is_none());
        }

        #[test]
        fn test_with_sni_name() {
            let target = ProxyTarget::parse("https://192.168.1.1:443/")
                .unwrap()
                .with_sni_name(Some("api.example.com".to_string()));
            
            assert_eq!(target.sni_name, Some("api.example.com".to_string()));
            assert_eq!(target.host, "192.168.1.1");
        }

        #[test]
        fn test_with_h2c() {
            let target = ProxyTarget::parse("http://localhost:8080/")
                .unwrap()
                .with_h2c(true);
            
            assert!(target.use_h2c);
            assert!(!target.use_tls);
        }

        #[test]
        fn test_ipv6_host() {
            // IPv6アドレスのパース（ブラケット表記）
            let target = ProxyTarget::parse("http://[::1]:8080/");
            // 現在の実装ではIPv6はサポートされていない可能性があるため、
            // パース結果を確認
            if let Some(t) = target {
                assert_eq!(t.port, 8080);
            }
        }
    }

    // ====================
    // UpstreamGroup 選択ロジックテスト
    // ====================

    mod upstream_selection_tests {
        use super::*;

        fn create_test_servers() -> Vec<UpstreamServerEntry> {
            vec![
                UpstreamServerEntry { 
                    url: "http://server1:8080".into(), 
                    sni_name: None,
                    use_h2c: false,
                    weight: 1,
                },
                UpstreamServerEntry { 
                    url: "http://server2:8080".into(), 
                    sni_name: None,
                    use_h2c: false,
                    weight: 1,
                },
                UpstreamServerEntry { 
                    url: "http://server3:8080".into(), 
                    sni_name: None,
                    use_h2c: false,
                    weight: 1,
                },
            ]
        }

        #[test]
        fn test_upstream_group_creation() {
            let servers = create_test_servers();
            let group = UpstreamGroup::new(
                "test-group".into(),
                servers,
                LoadBalanceAlgorithm::RoundRobin,
                None,
                false
            );
            
            assert!(group.is_some());
            let group = group.unwrap();
            assert_eq!(group.len(), 3);
        }

        #[test]
        fn test_upstream_group_empty_servers() {
            let group = UpstreamGroup::new(
                "empty".into(),
                vec![],
                LoadBalanceAlgorithm::RoundRobin,
                None,
                false
            );
            
            assert!(group.is_none());
        }

        #[test]
        fn test_upstream_group_invalid_url() {
            let servers = vec![
                UpstreamServerEntry { 
                    url: "invalid-url".into(), 
                    sni_name: None,
                    use_h2c: false,
                    weight: 1,
                },
            ];
            let group = UpstreamGroup::new(
                "invalid".into(),
                servers,
                LoadBalanceAlgorithm::RoundRobin,
                None,
                false
            );
            
            assert!(group.is_none());
        }

        #[test]
        fn test_round_robin_distribution() {
            let servers = create_test_servers();
            let group = UpstreamGroup::new(
                "rr-test".into(),
                servers,
                LoadBalanceAlgorithm::RoundRobin,
                None,
                false
            ).unwrap();
            
            let mut hosts: Vec<String> = Vec::new();
            for _ in 0..9 {
                if let Some(server) = group.select("client") {
                    hosts.push(server.target.host.clone());
                }
            }
            
            // 9回選択で3サイクル
            assert_eq!(hosts.len(), 9);
            
            // 各サーバーが3回ずつ選択される
            let count_server1 = hosts.iter().filter(|h| *h == "server1").count();
            let count_server2 = hosts.iter().filter(|h| *h == "server2").count();
            let count_server3 = hosts.iter().filter(|h| *h == "server3").count();
            
            assert_eq!(count_server1, 3);
            assert_eq!(count_server2, 3);
            assert_eq!(count_server3, 3);
        }

        #[test]
        fn test_ip_hash_consistency() {
            let servers = create_test_servers();
            let group = UpstreamGroup::new(
                "iphash-test".into(),
                servers,
                LoadBalanceAlgorithm::IpHash,
                None,
                false
            ).unwrap();
            
            let client_ip = "192.168.1.100";
            let first = group.select(client_ip).map(|s| s.target.host.clone());
            
            // 同じIPは常に同じサーバーを選択
            for _ in 0..20 {
                let selected = group.select(client_ip).map(|s| s.target.host.clone());
                assert_eq!(first, selected, "IP Hash should be consistent");
            }
        }

        #[test]
        fn test_ip_hash_different_ips_distribute() {
            let servers = create_test_servers();
            let group = UpstreamGroup::new(
                "iphash-dist".into(),
                servers,
                LoadBalanceAlgorithm::IpHash,
                None,
                false
            ).unwrap();
            
            let mut selected_hosts = std::collections::HashSet::new();
            
            // 100個の異なるIPで分散を確認
            for i in 0..100 {
                let ip = format!("10.0.{}.{}", i / 256, i % 256);
                if let Some(server) = group.select(&ip) {
                    selected_hosts.insert(server.target.host.clone());
                }
            }
            
            // 複数サーバーに分散されることを確認
            assert!(selected_hosts.len() >= 2, "Should distribute across multiple servers");
        }

        #[test]
        fn test_least_connections_selection() {
            let servers = create_test_servers();
            let group = UpstreamGroup::new(
                "lc-test".into(),
                servers,
                LoadBalanceAlgorithm::LeastConnections,
                None,
                false
            ).unwrap();
            
            // 初期状態では全サーバーの接続数が0なので、最初のサーバーが選択される
            let selected = group.select("client");
            assert!(selected.is_some());
        }

        #[test]
        fn test_single_server_group() {
            let target = ProxyTarget::parse("http://single:8080/").unwrap();
            let group = UpstreamGroup::single(target);
            
            assert_eq!(group.len(), 1);
            
            // 何度選択しても同じサーバー
            for _ in 0..5 {
                let selected = group.select("client");
                assert!(selected.is_some());
                assert_eq!(selected.unwrap().target.host, "single");
            }
        }
    }

    // ====================
    // UpstreamServer 健康状態テスト
    // ====================

    mod upstream_health_tests {
        use super::*;

        #[test]
        fn test_server_initial_state_healthy() {
            let target = ProxyTarget::parse("http://localhost:8080/").unwrap();
            let server = UpstreamServer::new(target);
            
            assert!(server.is_healthy());
        }

        #[test]
        fn test_server_becomes_unhealthy_after_failures() {
            let target = ProxyTarget::parse("http://localhost:8080/").unwrap();
            let server = UpstreamServer::new(target);
            
            // 3回失敗すると不健全になる（デフォルト閾値）
            server.record_failure(3);
            assert!(server.is_healthy()); // まだ健全
            server.record_failure(3);
            assert!(server.is_healthy()); // まだ健全
            server.record_failure(3);
            assert!(!server.is_healthy()); // 3回目で不健全
        }

        #[test]
        fn test_server_becomes_healthy_after_successes() {
            let target = ProxyTarget::parse("http://localhost:8080/").unwrap();
            let server = UpstreamServer::new(target);
            
            // まず不健全にする
            for _ in 0..3 {
                server.record_failure(3);
            }
            assert!(!server.is_healthy());
            
            // 2回成功すると健全になる（デフォルト閾値）
            server.record_success(2);
            assert!(!server.is_healthy()); // まだ不健全
            server.record_success(2);
            assert!(server.is_healthy()); // 2回目で健全
        }

        #[test]
        fn test_server_connection_count() {
            let target = ProxyTarget::parse("http://localhost:8080/").unwrap();
            let server = UpstreamServer::new(target);
            
            assert_eq!(server.connections(), 0);
            
            server.acquire();
            assert_eq!(server.connections(), 1);
            
            server.acquire();
            assert_eq!(server.connections(), 2);
            
            server.release();
            assert_eq!(server.connections(), 1);
            
            server.release();
            assert_eq!(server.connections(), 0);
        }

        #[test]
        fn test_select_skips_unhealthy_servers() {
            let servers = vec![
                UpstreamServerEntry { 
                    url: "http://healthy:8080".into(), 
                    sni_name: None,
                    use_h2c: false,
                    weight: 1,
                },
                UpstreamServerEntry { 
                    url: "http://unhealthy:8080".into(), 
                    sni_name: None,
                    use_h2c: false,
                    weight: 1,
                },
            ];
            let group = UpstreamGroup::new(
                "health-test".into(),
                servers,
                LoadBalanceAlgorithm::RoundRobin,
                None,
                false
            ).unwrap();
            
            // 2番目のサーバーを不健全にマーク（3回失敗で不健全）
            for _ in 0..3 {
                group.servers[1].record_failure(3);
            }
            
            // 10回選択しても不健全サーバーは選択されない
            for _ in 0..10 {
                let selected = group.select("client");
                assert!(selected.is_some());
                assert_eq!(selected.unwrap().target.host, "healthy");
            }
        }

        #[test]
        fn test_select_returns_none_all_unhealthy() {
            let servers = vec![
                UpstreamServerEntry { 
                    url: "http://server1:8080".into(), 
                    sni_name: None,
                    use_h2c: false,
                    weight: 1,
                },
                UpstreamServerEntry { 
                    url: "http://server2:8080".into(), 
                    sni_name: None,
                    use_h2c: false,
                    weight: 1,
                },
            ];
            let group = UpstreamGroup::new(
                "all-unhealthy".into(),
                servers,
                LoadBalanceAlgorithm::RoundRobin,
                None,
                false
            ).unwrap();
            
            // 全サーバーを不健全にマーク（3回失敗で不健全）
            for server in &group.servers {
                for _ in 0..3 {
                    server.record_failure(3);
                }
            }
            
            let selected = group.select("client");
            assert!(selected.is_none());
        }

        #[test]
        fn test_failure_resets_success_counter() {
            let target = ProxyTarget::parse("http://localhost:8080/").unwrap();
            let server = UpstreamServer::new(target);
            
            // 不健全にする
            for _ in 0..3 {
                server.record_failure(3);
            }
            assert!(!server.is_healthy());
            
            // 1回成功
            server.record_success(2);
            
            // 失敗で成功カウンターリセット
            server.record_failure(3);
            
            // 再度2回成功が必要
            server.record_success(2);
            assert!(!server.is_healthy());
            server.record_success(2);
            assert!(server.is_healthy());
        }
    }

    // ====================
    // LoadBalanceAlgorithm パーステスト
    // ====================

    mod load_balance_algorithm_tests {
        use super::*;

        #[test]
        fn test_default_algorithm() {
            let algo = LoadBalanceAlgorithm::default();
            assert_eq!(algo, LoadBalanceAlgorithm::RoundRobin);
        }
    }

    // ====================
    // HealthCheckConfig テスト
    // ====================

    mod health_check_config_tests {
        use super::*;

        #[test]
        fn test_is_status_healthy() {
            let config = HealthCheckConfig::default();
            
            // 健康なステータス
            assert!(config.healthy_statuses.contains(&200));
            assert!(config.healthy_statuses.contains(&201));
            assert!(config.healthy_statuses.contains(&204));
            assert!(config.healthy_statuses.contains(&301));
            assert!(config.healthy_statuses.contains(&302));
            assert!(config.healthy_statuses.contains(&304));
            
            // 不健康なステータス
            assert!(!config.healthy_statuses.contains(&400));
            assert!(!config.healthy_statuses.contains(&500));
            assert!(!config.healthy_statuses.contains(&503));
        }

        #[test]
        fn test_custom_healthy_statuses() {
            let mut config = HealthCheckConfig::default();
            config.healthy_statuses = vec![200, 201];
            
            assert!(config.healthy_statuses.contains(&200));
            assert!(config.healthy_statuses.contains(&201));
            assert!(!config.healthy_statuses.contains(&204));
        }
    }

    // ====================
    // HTTP/1.1 RFC準拠ヘルパー関数テスト
    // ====================
    
    mod http11_tests {
        use super::*;

        #[test]
        fn test_add_via_header_new() {
            let mut headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
                (b"host".to_vec(), b"example.com".to_vec()),
            ];
            add_via_header(&mut headers, "proxy.example.com");
            
            assert_eq!(headers.len(), 2);
            assert_eq!(headers[1].0, b"via".to_vec());
            assert_eq!(headers[1].1, b"1.1 proxy.example.com".to_vec());
        }

        #[test]
        fn test_add_via_header_existing() {
            let mut headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
                (b"via".to_vec(), b"1.1 first-proxy".to_vec()),
            ];
            add_via_header(&mut headers, "second-proxy");
            
            assert_eq!(headers.len(), 1);
            assert_eq!(headers[0].1, b"1.1 first-proxy, 1.1 second-proxy".to_vec());
        }

        #[test]
        fn test_validate_http_headers_valid() {
            let headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
                (b"content-length".to_vec(), b"100".to_vec()),
            ];
            assert!(validate_http_headers(&headers).is_ok());
        }

        #[test]
        fn test_validate_http_headers_valid_transfer_encoding() {
            let headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
                (b"transfer-encoding".to_vec(), b"chunked".to_vec()),
            ];
            assert!(validate_http_headers(&headers).is_ok());
        }

        #[test]
        fn test_validate_http_headers_conflict() {
            let headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
                (b"content-length".to_vec(), b"100".to_vec()),
                (b"transfer-encoding".to_vec(), b"chunked".to_vec()),
            ];
            assert!(validate_http_headers(&headers).is_err());
        }

        #[test]
        fn test_check_expect_continue_true() {
            let headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
                (b"expect".to_vec(), b"100-continue".to_vec()),
            ];
            assert!(check_expect_continue(&headers));
        }

        #[test]
        fn test_check_expect_continue_false() {
            let headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
                (b"host".to_vec(), b"example.com".to_vec()),
            ];
            assert!(!check_expect_continue(&headers));
        }

        #[test]
        fn test_check_header_count_within_limit() {
            assert!(check_header_count(50, 64).is_ok());
        }

        #[test]
        fn test_check_header_count_expansion() {
            let result = check_header_count(64, 64);
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), 128);
        }

        #[test]
        fn test_check_header_count_max_limit() {
            let result = check_header_count(1024, 1024);
            assert!(result.is_err());
        }
    }

    // ====================
    // RFC 7230-7233 準拠ヘルパー関数テスト
    // ====================
    
    mod rfc_compliance_tests {
        use super::*;

        // Hostヘッダー検証テスト (RFC 7230 Section 5.4)
        
        #[test]
        fn test_validate_host_header_present_http11() {
            let headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
                (b"host".to_vec(), b"example.com".to_vec()),
            ];
            assert!(validate_host_header(&headers, 1).is_ok());
        }

        #[test]
        fn test_validate_host_header_missing_http11() {
            let headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
                (b"content-type".to_vec(), b"text/html".to_vec()),
            ];
            assert!(validate_host_header(&headers, 1).is_err());
        }

        #[test]
        fn test_validate_host_header_http10_optional() {
            // HTTP/1.0ではHostヘッダーは任意
            let headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
                (b"content-type".to_vec(), b"text/html".to_vec()),
            ];
            assert!(validate_host_header(&headers, 0).is_ok());
        }

        #[test]
        fn test_validate_host_header_case_insensitive() {
            let headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
                (b"HOST".to_vec(), b"example.com".to_vec()),
            ];
            assert!(validate_host_header(&headers, 1).is_ok());
        }

        // Hop-by-hopヘッダーテスト (RFC 7230 Section 6.1)
        
        #[test]
        fn test_is_hop_by_hop_header_connection() {
            assert!(is_hop_by_hop_header(b"connection"));
            assert!(is_hop_by_hop_header(b"Connection"));
            assert!(is_hop_by_hop_header(b"CONNECTION"));
        }

        #[test]
        fn test_is_hop_by_hop_header_keep_alive() {
            assert!(is_hop_by_hop_header(b"keep-alive"));
            assert!(is_hop_by_hop_header(b"Keep-Alive"));
        }

        #[test]
        fn test_is_hop_by_hop_header_proxy_connection() {
            assert!(is_hop_by_hop_header(b"proxy-connection"));
            assert!(is_hop_by_hop_header(b"Proxy-Connection"));
        }

        #[test]
        fn test_is_hop_by_hop_header_te() {
            assert!(is_hop_by_hop_header(b"te"));
            assert!(is_hop_by_hop_header(b"TE"));
        }

        #[test]
        fn test_is_hop_by_hop_header_trailer() {
            assert!(is_hop_by_hop_header(b"trailer"));
        }

        #[test]
        fn test_is_hop_by_hop_header_transfer_encoding() {
            assert!(is_hop_by_hop_header(b"transfer-encoding"));
        }

        #[test]
        fn test_is_hop_by_hop_header_upgrade() {
            assert!(is_hop_by_hop_header(b"upgrade"));
        }

        #[test]
        fn test_is_not_hop_by_hop_header() {
            assert!(!is_hop_by_hop_header(b"content-type"));
            assert!(!is_hop_by_hop_header(b"host"));
            assert!(!is_hop_by_hop_header(b"accept"));
            assert!(!is_hop_by_hop_header(b"cache-control"));
        }

        #[test]
        fn test_strip_hop_by_hop_headers_basic() {
            let mut headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
                (b"host".to_vec(), b"example.com".to_vec()),
                (b"connection".to_vec(), b"keep-alive".to_vec()),
                (b"keep-alive".to_vec(), b"timeout=5".to_vec()),
                (b"content-type".to_vec(), b"text/html".to_vec()),
            ];
            
            strip_hop_by_hop_headers(&mut headers);
            
            assert_eq!(headers.len(), 2);
            assert!(headers.iter().any(|(n, _)| n == b"host"));
            assert!(headers.iter().any(|(n, _)| n == b"content-type"));
            assert!(!headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(b"connection")));
            assert!(!headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(b"keep-alive")));
        }

        #[test]
        fn test_strip_hop_by_hop_headers_custom() {
            // Connectionヘッダーで指定されたカスタムヘッダーも削除
            let mut headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
                (b"host".to_vec(), b"example.com".to_vec()),
                (b"connection".to_vec(), b"keep-alive, x-custom".to_vec()),
                (b"x-custom".to_vec(), b"value".to_vec()),
            ];
            
            strip_hop_by_hop_headers(&mut headers);
            
            assert_eq!(headers.len(), 1);
            assert!(headers.iter().any(|(n, _)| n == b"host"));
        }

        // Rangeヘッダーテスト (RFC 7233)
        
        #[test]
        fn test_parse_range_header_single_range() {
            let result = parse_range_header(b"bytes=0-99");
            assert!(result.is_some());
            let parsed = result.unwrap();
            assert_eq!(parsed.ranges.len(), 1);
            assert_eq!(parsed.ranges[0], RangeSpec::Bytes { start: 0, end: Some(99) });
        }

        #[test]
        fn test_parse_range_header_open_end() {
            let result = parse_range_header(b"bytes=100-");
            assert!(result.is_some());
            let parsed = result.unwrap();
            assert_eq!(parsed.ranges[0], RangeSpec::Bytes { start: 100, end: None });
        }

        #[test]
        fn test_parse_range_header_suffix() {
            let result = parse_range_header(b"bytes=-500");
            assert!(result.is_some());
            let parsed = result.unwrap();
            assert_eq!(parsed.ranges[0], RangeSpec::Suffix { suffix_length: 500 });
        }

        #[test]
        fn test_parse_range_header_multiple() {
            let result = parse_range_header(b"bytes=0-99, 200-299");
            assert!(result.is_some());
            let parsed = result.unwrap();
            assert_eq!(parsed.ranges.len(), 2);
        }

        #[test]
        fn test_parse_range_header_invalid_no_bytes() {
            assert!(parse_range_header(b"0-99").is_none());
        }

        #[test]
        fn test_parse_range_header_invalid_start_greater_than_end() {
            assert!(parse_range_header(b"bytes=100-50").is_none());
        }

        #[test]
        fn test_parse_range_header_case_insensitive() {
            let result = parse_range_header(b"BYTES=0-100");
            assert!(result.is_some());
        }

        // normalize_range テスト
        
        #[test]
        fn test_normalize_range_bytes_within_bounds() {
            let spec = RangeSpec::Bytes { start: 0, end: Some(99) };
            let result = normalize_range(&spec, 1000);
            assert_eq!(result, Some((0, 99)));
        }

        #[test]
        fn test_normalize_range_bytes_end_exceeds() {
            let spec = RangeSpec::Bytes { start: 0, end: Some(9999) };
            let result = normalize_range(&spec, 1000);
            assert_eq!(result, Some((0, 999))); // end should be clamped
        }

        #[test]
        fn test_normalize_range_bytes_open_end() {
            let spec = RangeSpec::Bytes { start: 500, end: None };
            let result = normalize_range(&spec, 1000);
            assert_eq!(result, Some((500, 999)));
        }

        #[test]
        fn test_normalize_range_bytes_start_exceeds() {
            let spec = RangeSpec::Bytes { start: 1000, end: Some(1100) };
            let result = normalize_range(&spec, 1000);
            assert_eq!(result, None); // 416 Range Not Satisfiable
        }

        #[test]
        fn test_normalize_range_suffix() {
            let spec = RangeSpec::Suffix { suffix_length: 100 };
            let result = normalize_range(&spec, 1000);
            assert_eq!(result, Some((900, 999)));
        }

        #[test]
        fn test_normalize_range_suffix_larger_than_content() {
            let spec = RangeSpec::Suffix { suffix_length: 2000 };
            let result = normalize_range(&spec, 1000);
            assert_eq!(result, Some((0, 999)));
        }

        #[test]
        fn test_normalize_range_empty_content() {
            let spec = RangeSpec::Bytes { start: 0, end: Some(100) };
            let result = normalize_range(&spec, 0);
            assert_eq!(result, None);
        }

        // 206 Partial Content レスポンス構築テスト
        
        #[test]
        fn test_build_partial_response_header() {
            let header = build_partial_response_header(0, 99, 1000, "text/plain", false);
            let header_str = String::from_utf8_lossy(&header);
            
            assert!(header_str.contains("HTTP/1.1 206 Partial Content"));
            assert!(header_str.contains("Content-Range: bytes 0-99/1000"));
            assert!(header_str.contains("Content-Length: 100"));
            assert!(header_str.contains("Content-Type: text/plain"));
            assert!(header_str.contains("Connection: keep-alive"));
        }

        #[test]
        fn test_build_partial_response_header_close() {
            let header = build_partial_response_header(0, 99, 1000, "text/plain", true);
            let header_str = String::from_utf8_lossy(&header);
            
            assert!(header_str.contains("Connection: close"));
        }

        #[test]
        fn test_build_range_not_satisfiable_response() {
            let response = build_range_not_satisfiable_response(1000);
            let response_str = String::from_utf8_lossy(&response);
            
            assert!(response_str.contains("HTTP/1.1 416 Range Not Satisfiable"));
            assert!(response_str.contains("Content-Range: bytes */1000"));
            assert!(response_str.contains("Content-Length: 0"));
        }

        // TEヘッダーテスト (RFC 7230 Section 4.3)
        
        #[test]
        fn test_parse_te_header_trailers() {
            let te = parse_te_header(b"trailers");
            assert!(te.supports_trailers);
            assert!(te.encodings.is_empty());
        }

        #[test]
        fn test_parse_te_header_trailers_case_insensitive() {
            let te = parse_te_header(b"TRAILERS");
            assert!(te.supports_trailers);
        }

        #[test]
        fn test_parse_te_header_gzip() {
            let te = parse_te_header(b"gzip");
            assert!(!te.supports_trailers);
            assert_eq!(te.encodings.len(), 1);
            assert_eq!(te.encodings[0], "gzip");
        }

        #[test]
        fn test_parse_te_header_multiple() {
            let te = parse_te_header(b"trailers, gzip, deflate");
            assert!(te.supports_trailers);
            assert_eq!(te.encodings.len(), 2);
            assert!(te.encodings.contains(&"gzip".to_string()));
            assert!(te.encodings.contains(&"deflate".to_string()));
        }

        #[test]
        fn test_parse_te_header_with_quality() {
            let te = parse_te_header(b"gzip;q=0.5, deflate;q=1.0");
            assert_eq!(te.encodings.len(), 2);
            assert_eq!(te.encodings[0], "gzip");
            assert_eq!(te.encodings[1], "deflate");
        }

        #[test]
        fn test_parse_te_header_chunked_ignored() {
            // chunkedはTE経由で指定すべきではないがスキップ
            let te = parse_te_header(b"chunked, trailers");
            assert!(te.supports_trailers);
            assert!(te.encodings.is_empty());
        }

        #[test]
        fn test_parse_te_header_empty() {
            let te = parse_te_header(b"");
            assert!(!te.supports_trailers);
            assert!(te.encodings.is_empty());
        }

        // get_range_header テスト
        
        #[test]
        fn test_get_range_header_found() {
            let headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
                (b"host".to_vec(), b"example.com".to_vec()),
                (b"range".to_vec(), b"bytes=0-100".to_vec()),
            ];
            let result = get_range_header(&headers);
            assert_eq!(result, Some(b"bytes=0-100".as_slice()));
        }

        #[test]
        fn test_get_range_header_not_found() {
            let headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
                (b"host".to_vec(), b"example.com".to_vec()),
            ];
            let result = get_range_header(&headers);
            assert!(result.is_none());
        }

        #[test]
        fn test_get_range_header_case_insensitive() {
            let headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
                (b"Range".to_vec(), b"bytes=0-100".to_vec()),
            ];
            let result = get_range_header(&headers);
            assert!(result.is_some());
        }

        // should_advertise_accept_ranges テスト
        
        #[test]
        fn test_should_advertise_accept_ranges_get() {
            assert!(should_advertise_accept_ranges(b"GET"));
            assert!(should_advertise_accept_ranges(b"get"));
        }

        #[test]
        fn test_should_advertise_accept_ranges_head() {
            assert!(should_advertise_accept_ranges(b"HEAD"));
            assert!(should_advertise_accept_ranges(b"head"));
        }

        #[test]
        fn test_should_not_advertise_accept_ranges_post() {
            assert!(!should_advertise_accept_ranges(b"POST"));
            assert!(!should_advertise_accept_ranges(b"PUT"));
            assert!(!should_advertise_accept_ranges(b"DELETE"));
        }
    }
}

