//! バイナリエントリ（`main` から `veil::run()` で呼び出す）。

use crate::logging::*;
use crate::pool::*;
use crate::system::*;

use clap::Parser;
use ftlog::{debug, error, info, warn};
use rustls::crypto::CryptoProvider;
use std::sync::atomic::Ordering;
use std::sync::Arc;

// ktls_rustls（kTLS 対応）
#[cfg(veil_ktls)]
use crate::ktls_rustls::RustlsAcceptor;

pub use crate::config::*;
use crate::server::*;

/// OpenBSD pledge(2) の promise 集合（F-120 Phase 5）。
///
/// VM E2E での実証（`docs/backlog/features/F-120-cross-platform-epoll-kqueue-bsd.md`
/// の Phase 5 記録を参照）に基づく最小構成:
/// - `stdio`: 既に開いている fd への read/write（accept 済み接続・ログ出力）。
/// - `rpath`: TLS 証明書/鍵・静的ファイル・WASM モジュールの読み取り。
/// - `wpath`/`cpath`: アクセスログ/アプリログのローテーション・ディスクキャッシュの
///   新規ファイル作成。
/// - `inet`: TCP/UDP ソケット（bind/listen/accept/connect を含む）。
/// - `dns`: upstream ホスト名解決（`getaddrinfo`）。
/// - `flock`: ログライブラリ（ftlog）のファイルロック。
/// - `unix`: AF_UNIX ソケットの socket/connect/bind（F-164 の UDS リスナー・
///   F-170 の UDS バックエンド接続の両方に必要）。
#[cfg(target_os = "openbsd")]
const PLEDGE_PROMISES: &str = "stdio rpath wpath cpath inet dns flock unix";

// ====================
// メイン関数
// ====================

/// Runs the `veil` proxy binary: parses the command line (`-c`, `-t`, `--override`, ...), loads the
/// configuration and serves until SIGINT / SIGTERM. This is what `src/main.rs` calls; API servers use
/// [`serve`](crate::serve) instead.
pub fn run() {
    // F-237: io_uring が使えなければ（Docker の既定の seccomp・RuntimeDefault）、同じイメージの epoll 版へ
    // 切り替える（切り替え先が無ければ何もしない）。メッシュの起動（F-227）より前に行う。
    #[cfg(veil_rt_uring)]
    crate::runtime::fallback::exec_reactor_if_uring_unavailable();

    // F-182: capsicum の capability mode 用の接続ブローカーとして再実行された場合（隠し引数）。
    // 設定・ログ・ランタイムを一切初期化せずにブローカーのループへ入る。
    #[cfg(target_os = "freebsd")]
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new(crate::connect_broker::BROKER_ARG))
    {
        std::process::exit(crate::connect_broker::broker_main());
    }

    // F-222: Istio の pilot-agent から Envoy として起動された（argv[0] が envoy・`veil envoy ...`）
    #[cfg(feature = "mesh-istio")]
    if let Some(args) = crate::mesh::envoy_invocation() {
        std::process::exit(crate::mesh::envoy::main(args));
    }
    // Linkerd のプロキシ（イメージの /usr/lib/linkerd/linkerd2-proxy を veil にする。F-223）
    #[cfg(feature = "mesh-linkerd")]
    if crate::mesh::linkerd_invocation() {
        std::process::exit(crate::mesh::linkerd::main());
    }

    // コマンドライン引数を解析（--help, --version は clap が自動処理）
    let cli_args = CliArgs::parse();

    // -o/--override をパースしてグローバルへ確定させる（設定ファイルを読む前に行う）。
    // 起動時のみならずホットリロード（SIGHUP）・-t 検証も同じグローバル集合を通る
    // （src/config_override.rs の apply_to_toml_str が単一チョークポイント）。
    // F-179: 環境変数 `VEIL_OVERRIDE_<n>` も同じ書式で受ける。優先順位は
    // TOML < 環境変数 < CLI なので「環境変数（<n> の昇順）→ CLI」の順に連結する（後勝ち）。
    let mut overrides = crate::config_override::env_overrides_from(std::env::vars_os())
        .unwrap_or_else(|e| {
            eprintln!("veil: invalid environment override: {}", e);
            std::process::exit(1);
        });
    let cli_overrides = cli_args
        .overrides
        .iter()
        .map(|s| crate::config_override::ConfigOverride::parse(s))
        .collect::<Result<Vec<_>, String>>()
        .unwrap_or_else(|e| {
            eprintln!("veil: invalid --override: {}", e);
            std::process::exit(1);
        });
    overrides.extend(cli_overrides);
    crate::config_override::set_global_overrides(overrides);

    // F-247: 設定ファイルは -c → 環境変数 VEIL_CONFIG → 既定のパスの順で決める。設定は内蔵しない
    // （無ければ置き場所を示して止まる。パッケージは既定のパスに既定の設定を置く）。
    let (config_path, source) =
        crate::config::resolve_config_path(cli_args.config, std::env::var_os("VEIL_CONFIG"));
    if !config_path.exists() {
        eprintln!(
            "veil: configuration file not found: {} (from {})",
            config_path.display(),
            source.describe()
        );
        eprintln!(
            "veil: pass a file with -c FILE or set VEIL_CONFIG; packages install a default file at {}",
            crate::config::default_config_path().display()
        );
        eprintln!("veil: see {}", CONFIG_FILE_DOC_URL);
        std::process::exit(1);
    }
    let _ = CONFIG_PATH_SOURCE.set(source);

    run_server(ServerOptions {
        config_path,
        test_config: cli_args.test_config,
    });
    // F-247: 起動の途中のエラー（設定の読み込み・bind など）で run_server から戻ったときは終了コード 1 にする
    // （今まで 0 で終わり、systemd の Restart=on-failure やスクリプトが失敗に気づけなかった）
    if !STARTUP_COMPLETED.load(std::sync::atomic::Ordering::Acquire) {
        std::process::exit(1);
    }
}

/// 設定ファイルの決まり方の文書（F-247。エラーの文に出す）
const CONFIG_FILE_DOC_URL: &str =
    "https://github.com/aofusa/veil-proxy/blob/main/docs/guide/running.md#configuration-file";

/// 設定ファイルのパスがどこから決まったか（F-247。起動ログに出す。ライブラリからの起動では未設定）
static CONFIG_PATH_SOURCE: std::sync::OnceLock<crate::config::ConfigPathSource> =
    std::sync::OnceLock::new();

/// 全リスナー・ワーカーの起動を終えたら true（F-247。run_server が起動の途中で戻ったかの判定）
static STARTUP_COMPLETED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Startup options of the server (independent of CLI parsing; also used by the API library).
pub struct ServerOptions {
    /// Path of the configuration file (the same path is read again on a SIGHUP reload).
    pub config_path: std::path::PathBuf,
    /// Equivalent of `-t`: validate the configuration and exit.
    pub test_config: bool,
}

/// Loads the configuration file, starts the server and blocks until it stops (SIGINT / SIGTERM or
/// `server::request_shutdown`). The caller must settle `--override` and environment-variable
/// overrides beforehand with `config_override::set_global_overrides`.
pub fn run_server(opts: ServerOptions) {
    // 設定ファイルパスをグローバル変数に保存（ホットリロード用）
    CONFIG_PATH.store(Arc::new(opts.config_path.clone()));
    let config_path = opts.config_path;

    // -t オプション: 設定ファイルのテストのみ
    if opts.test_config {
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
    // F-122: OpenBSD は ring、他は aws_lc_rs（`crate::tls_provider` で選択）。
    // ライブラリとして組み込まれた場合はホスト側が先に入れていることがあるため、未設定のときだけ入れる。
    if CryptoProvider::get_default().is_none() {
        CryptoProvider::install_default(crate::tls_provider::provider::default_provider())
            .expect("Failed to install rustls crypto provider");
    }

    // ログ設定を先に読み込む（ログ初期化前）
    // 設定ファイルが読めない場合はデフォルト設定を使用
    let logging_config =
        load_logging_config(&config_path).unwrap_or_else(|_| LoggingConfigSection::default());

    // ftlogを設定に基づいて初期化
    // ftlogは内部でバックグラウンドスレッドとチャネルを使用した非同期ログライブラリ
    // 追加の非同期化層（tokio::sync::mpsc等）は不要
    let _guard = init_logging(&logging_config);

    // B-44 第4段: RLIMIT_NOFILE の soft limit を hard limit まで引き上げる
    // （nginx の worker_rlimit_nofile 相当。F-116 多重化後の同時 fd 需要 ~1100 が
    //   コンテナ既定 soft 1024 を超えるため。ワーカー起動・seccomp 適用より前に実行）
    crate::system::raise_nofile_limit();

    // F-05: ACME。証明書が無ければ仮の自己署名証明書を置き（設定の検証より前）、HTTP-01 の
    // 応答リスナーを bind する（特権降格・サンドボックスより前）。
    #[cfg(feature = "acme")]
    let acme_enabled = match crate::acme::bootstrap(&config_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("ACME setup error: {}", e);
            return;
        }
    };
    #[cfg(not(feature = "acme"))]
    let acme_enabled = false;

    // http3: 証明書世代の初期化で、NetBSD: chroot 後のルート再解決で書き換える。
    #[cfg(any(feature = "http3", target_os = "netbsd"))]
    let mut loaded_config = match load_config(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Config load error: {}", e);
            return;
        }
    };
    #[cfg(not(any(feature = "http3", target_os = "netbsd")))]
    let loaded_config = match load_config(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Config load error: {}", e);
            return;
        }
    };

    // F-259: OS の信頼ストア（`"system"`）は load_config で読み終えた。以後（リロード）に初めて
    // `"system"` が現れても、サンドボックスの下では読めないので拒否する
    {
        let sec = &loaded_config.global_security;
        crate::trust_store::set_sandboxed(
            sec.enable_seccomp
                || sec.enable_landlock
                || sec.enable_capsicum
                || sec.capsicum_capability_mode
                || sec.enable_unveil
                || sec.enable_pledge
                || sec.enable_sandbox_macos
                || sec.chroot_dir.is_some(),
        );
    }

    // F-153: 静的配信のパス解決をカーネル封じ込め（Linux: openat2 RESOLVE_BENEATH、
    // FreeBSD: F-123 の capsicum O_RESOLVE_BENEATH）へ切り替えるため、File ルートの
    // ルートディレクトリを設定ロード直後（**FreeBSD の `cap_enter`・OpenBSD の
    // `unveil` ロックより前**）に登録する。絶対パス open/canonicalize がまだ許可されて
    // いるうちに行う必要があるため、この位置（他の OS 別セキュリティ機構より前）で
    // 1 回だけ呼ぶ。FreeBSD の capability mode（`cap_enter`）はこの後さらに listener
    // の bind 完了を待ってから発動するため、ここで先に登録しておいても順序は安全
    // （登録 → listener bind → cap_enter の順が常に保たれる）。
    // 登録に失敗しても静的配信自体は canonicalize フォールバックで動作し続けるため、
    // 起動を中断しない（`register_static_roots` 内で警告ログのみ出す）。
    let static_roots: Vec<std::path::PathBuf> = loaded_config
        .route
        .iter()
        .filter_map(|r| match &r.action {
            crate::config::BackendConfig::File { path, .. } => Some(std::path::PathBuf::from(path)),
            _ => None,
        })
        .collect();
    crate::cache::resolve::register_static_roots(&static_roots);

    // FreeBSD: jail_name が設定されていれば起動最初期に jail_attach する（F-120 Phase 4）。
    // root 前提・失敗は明確なエラーで起動中止する（サンドボックス/権限降格より前に行う:
    // jail 内へ移った後でこそ以降のセキュリティ処理が正しいコンテキストで動く）。
    #[cfg(target_os = "freebsd")]
    if let Some(jail_name) = loaded_config.global_security.jail_name.as_deref() {
        match crate::security::capsicum::attach_jail(jail_name) {
            Ok(()) => info!("attached to jail \"{}\"", jail_name),
            Err(e) => {
                error!("failed to attach to jail \"{}\": {}", jail_name, e);
                return;
            }
        }
    }
    #[cfg(not(target_os = "freebsd"))]
    if loaded_config.global_security.jail_name.is_some() {
        warn!("jail_name is set but this build does not target FreeBSD; ignoring");
    }

    // 降格先の利用者・グループ名は chroot・サンドボックスの前に ID へ解決しておく
    // （chroot 後は /etc/passwd・/etc/group を読めない。F-176）。
    let privilege_ids = match crate::system::resolve_privilege_ids(&loaded_config.global_security) {
        Ok(ids) => ids,
        Err(e) => {
            error!("Failed to drop privileges: {}", e);
            return;
        }
    };

    // NetBSD: chroot(2)（F-140、オプトイン、root 前提）。
    //
    // NetBSD には pledge/unveil 相当のランタイム API が無いため
    // （`crate::security::netbsd` のモジュールコメント参照）、まず起動時ログで
    // 実際にサポートできる範囲（chroot + 特権降格 + rlimit）を正直に報告する。
    // chroot 自体は `drop_privileges`（setuid/setgid、entry.rs 後段）より
    // **前**に適用すること（先に setuid すると多くの実装で chroot(2) 自体が
    // `EPERM` になる）。
    #[cfg(target_os = "netbsd")]
    {
        crate::security::netbsd::report_security_support();
        if let Some(dir) = loaded_config.global_security.chroot_dir.as_deref() {
            match crate::security::netbsd::chroot_to(std::path::Path::new(dir)) {
                Ok(()) => {
                    info!("netbsd: chroot applied to \"{}\"", dir);
                    // 静的配信のベースパスを新しいルート基準で解決し直す（F-176）。
                    loaded_config.route = crate::config::reresolve_routes_after_chroot(
                        &loaded_config.route,
                        &loaded_config.upstream_groups,
                    );
                }
                Err(e) => {
                    error!("netbsd: chroot(\"{}\") failed: {}", dir, e);
                    if !loaded_config.global_security.allow_security_failures {
                        return;
                    }
                }
            }
        }
    }
    #[cfg(not(target_os = "netbsd"))]
    if loaded_config.global_security.chroot_dir.is_some() {
        warn!("chroot_dir is set but this build does not target NetBSD; ignoring");
    }

    // OpenBSD: unveil(2)（F-120 Phase 5）。
    //
    // 設定パスが確定した直後（TLS 証明書/鍵・WASM モジュールは load_config 内で既に
    // 読み込み済み・静的ファイルルートやログ/キャッシュディレクトリはこれから使う）に、
    // 設定から導出したパス集合のみへアクセスを制限する。unveil は最初の呼び出し以降
    // 明示的に unveil していないパスを ENOENT で拒否するため、最後に必ず
    // `unveil(NULL, NULL)` でロックする（ロードパスに漏れがあると以降の全アクセスが
    // 失敗し得るため、`allow_security_failures` に応じて起動可否を判断する）。
    #[cfg(target_os = "openbsd")]
    if loaded_config.global_security.enable_unveil {
        match crate::config::collect_unveil_paths(&config_path) {
            Ok(paths) => {
                // F-266: ライブラリのアップロードの一時ファイルの置き場
                #[cfg(feature = "handler")]
                let paths = {
                    let mut p = paths;
                    p.read_write_create
                        .push(crate::api::multipart::upload_tmp_dir());
                    p
                };
                let mut all_ok = true;
                for p in &paths.read_only {
                    if let Err(e) = crate::security::pledge_unveil::unveil_path(p, "r") {
                        warn!("unveil: failed to unveil '{}' (r): {}", p.display(), e);
                        all_ok = false;
                    }
                }
                for p in &paths.read_write_create {
                    // ディスクキャッシュ/ログディレクトリは初回書き込み時に自動作成される
                    // 実装だが、unveil は不存在パスに対する効果を保証しないため、
                    // ここで事前に作成しておく（起動時コールドパスの同期 fs 操作）。
                    if let Err(e) = std::fs::create_dir_all(p) {
                        warn!(
                            "unveil: failed to create directory '{}': {}",
                            p.display(),
                            e
                        );
                    }
                    if let Err(e) = crate::security::pledge_unveil::unveil_path(p, "rwc") {
                        warn!("unveil: failed to unveil '{}' (rwc): {}", p.display(), e);
                        all_ok = false;
                    }
                }
                match crate::security::pledge_unveil::lock_unveil() {
                    Ok(()) => info!(
                        "unveil: locked ({} read-only, {} read-write-create paths)",
                        paths.read_only.len(),
                        paths.read_write_create.len()
                    ),
                    Err(e) => {
                        error!("unveil: failed to lock: {}", e);
                        all_ok = false;
                    }
                }
                if !all_ok && !loaded_config.global_security.allow_security_failures {
                    error!("unveil: one or more steps failed; aborting startup (set allow_security_failures = true to continue)");
                    return;
                }
            }
            Err(e) => {
                error!("unveil: failed to collect paths from config: {}", e);
                if !loaded_config.global_security.allow_security_failures {
                    return;
                }
            }
        }
    }
    #[cfg(not(target_os = "openbsd"))]
    if loaded_config.global_security.enable_unveil {
        warn!("enable_unveil is set but this build does not target OpenBSD; ignoring");
    }

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

    // TLS アクセプターを作成（TLS のリスナーごとに、そのリスナーの ALPN のプロファイルへ `with_profile` する。F-260）
    #[cfg(veil_ktls)]
    let acceptor = RustlsAcceptor::new(loaded_config.tls_config.clone())
        .with_ktls(loaded_config.ktls_config.enabled)
        .with_fallback(loaded_config.ktls_config.fallback_enabled)
        .with_tcp_cork(loaded_config.ktls_config.tcp_cork_enabled);

    #[cfg(not(veil_ktls))]
    let acceptor = crate::simple_tls::SimpleTlsAcceptor::new(loaded_config.tls_config.clone())
        .with_ktls(loaded_config.ktls_config.enabled);

    // F-03: グローバル TLS 設定を初期化（アクセプタが毎ハンドシェイク参照）。F-260: 作った ALPN のプロファイルごと。
    let tls_profiles: Vec<crate::tls_reload::AlpnProfile> = crate::tls_reload::AlpnProfile::ALL
        .into_iter()
        .filter(|p| loaded_config.tls_profiles[p.index()].is_some())
        .collect();
    for p in &tls_profiles {
        if let Some(c) = &loaded_config.tls_profiles[p.index()] {
            crate::tls_reload::init_global_tls_config(*p, c.clone());
        }
    }

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
    // ACME が証明書を差し替えるので、ACME 有効時は auto_reload の設定に関わらずリローダーを動かす。
    if loaded_config.tls_auto_reload || acme_enabled {
        let cert_path = std::path::PathBuf::from(&loaded_config.tls_cert_path);
        let key_path = std::path::PathBuf::from(&loaded_config.tls_key_path);
        let interval = loaded_config.tls_reload_interval_secs.max(1);
        let ktls_enabled = loaded_config.ktls_config.enabled;
        // F-50: リロード時も設定された暗号スイートを維持する
        let cipher_suites = loaded_config.tls_cipher_suites.clone();
        // F-260: 起動時に作った ALPN のプロファイルを全部作り直す（ACME の差し替えも同じ経路）
        let builder: crate::tls_reload::ServerConfigBuilder = Box::new(move |c, k, profile| {
            crate::config::build_server_config_from_paths(
                c,
                k,
                ktls_enabled,
                profile.http2(),
                &cipher_suites,
            )
        });
        match crate::tls_reload::TlsCertReloader::new_global(
            cert_path,
            key_path,
            builder,
            tls_profiles.clone(),
        ) {
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

    let ktls_config = Arc::new(loaded_config.ktls_config.clone());

    // CURRENT_CONFIG を初期化（ホットリロード対応）
    // ワーカースレッドは CURRENT_CONFIG.load() を使用して最新の設定を取得
    let runtime_config = RuntimeConfig {
        route: loaded_config.route.clone(),
        optimized_router: loaded_config.optimized_router.clone(),
        tls_config: Some(loaded_config.tls_config.clone()),
        ktls_config: ktls_config.clone(),
        // F-260: 平文の方針の表（リスナーの id で引く。リダイレクト先のポートも各リスナーが持つ）
        cleartext: loaded_config
            .listeners
            .iter()
            .map(|l| l.cleartext)
            .collect(),
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
        #[cfg(feature = "wasm")]
        wasm_filter_engine: loaded_config.wasm_filter_engine.clone(),
        performance: loaded_config.performance.clone(),
        #[cfg(feature = "l4-proxy")]
        l4_listeners: Arc::new(loaded_config.l4_listeners.clone()),
    };
    // `[prometheus] enabled` を記録側の実行時スイッチへ反映する（config::reload と同じ）。
    crate::metrics::set_metrics_runtime_enabled(runtime_config.prometheus_config.enabled);
    crate::l7::set_max_buffered_request_body(&runtime_config.route);
    crate::metrics::set_host_labels(&runtime_config.route);
    crate::path_norm::set_request_mode(runtime_config.global_security.path_normalization);
    CURRENT_CONFIG.store(Arc::new(runtime_config));
    info!("Runtime configuration initialized (hot reload enabled via SIGHUP)");

    // F-04: VDS（制御プレーンからの動的設定配信）。`[vds] enabled = true` のときだけ専用スレッドを起動する。
    #[cfg(feature = "vds")]
    crate::vds::spawn_if_enabled(config_path.clone());

    // F-05: ACME の発行・更新スレッド。
    #[cfg(feature = "acme")]
    if acme_enabled {
        crate::acme::spawn();
    }

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

    let hostname = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "unknown".to_string());

    let num_threads = loaded_config.num_threads;

    info!("============================================");
    info!("High-Performance Reverse Proxy Server");
    match CONFIG_PATH_SOURCE.get() {
        Some(src) => info!(
            "Config File: {} (from {})",
            config_path.display(),
            src.describe()
        ),
        None => info!("Config File: {}", config_path.display()),
    }
    info!("Hostname: {}", hostname);
    info!("Threads: {} (CPU cores: {})", num_threads, num_cpus::get());
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

    // F-139: WASM gRPC 呼び出し専用の実行スレッドを起動（tick スレッドとは分離）。
    #[cfg(all(feature = "wasm", feature = "grpc"))]
    spawn_wasm_grpc_thread();

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
    if let Err(e) = drop_privileges(&privilege_ids) {
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

    // F-170: UDS バックエンド（HTTP 上流・L4 上流）のソケットパスを Landlock 書き込み
    // 許可へ追加する（Landlock の FS 権限は UDS connect を仲介しないと考えられるが、
    // 将来の ABI 変更に備えた保守的措置）。
    #[cfg(feature = "l4-proxy")]
    let l4_listeners_for_uds = loaded_config.l4_listeners.as_slice();
    #[cfg(not(feature = "l4-proxy"))]
    let l4_listeners_for_uds: &[crate::config::L4ListenerConfig] = &[];
    for p in crate::config::collect_uds_backend_paths_from_loaded(
        &loaded_config.route,
        &loaded_config.upstream_groups,
        l4_listeners_for_uds,
    ) {
        let s = p.to_string_lossy().into_owned();
        if !landlock_write_paths.contains(&s) {
            landlock_write_paths.push(s);
        }
    }
    // F-266: ライブラリのアップロードの一時ファイルの置き場（[api] upload_tmp_dir）
    #[cfg(feature = "handler")]
    {
        let d = crate::api::multipart::upload_tmp_dir()
            .to_string_lossy()
            .into_owned();
        if !landlock_write_paths.contains(&d) {
            landlock_write_paths.push(d);
        }
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

    // FreeBSD: capsicum（F-120 Phase 4）。
    //
    // リスナー fd の rights 制限（`enable_capsicum`）は `server::create_listener`
    // （各ワーカーの listener 作成経路）で個別に適用済み。
    //
    // capability mode（`capsicum_capability_mode`、オプトイン）はここでは適用**しない**:
    // `cap_enter(2)` はプロセス全体へ即時に効き、capability mode 中は `bind(2)` が
    // 禁止されるため、各ワーカースレッドが SO_REUSEPORT_LB listener を bind し終える
    // **前に** 入ると全ワーカーが Bind error で死ぬ（F-120 Phase 4 の VM 検証で発見）。
    // 適用は全 TLS ワーカーの listener bind 完了をバリアで待ってから行う（後段参照）。
    #[cfg(target_os = "freebsd")]
    let capsicum_capability_mode_requested = {
        let sec = &loaded_config.global_security;
        if sec.capsicum_capability_mode {
            #[cfg(feature = "l4-proxy")]
            let has_l4 = !loaded_config.l4_listeners.is_empty();
            #[cfg(not(feature = "l4-proxy"))]
            let has_l4 = false;
            #[cfg(feature = "http3")]
            let http3 = loaded_config.listeners.iter().any(|l| l.http3.is_some());
            #[cfg(not(feature = "http3"))]
            let http3 = false;
            // metrics（Prometheus）はメインリスナーのルートとして配信されるため
            // 追加リスナーは無く、capability mode の妨げにならない。
            // HTTP→HTTPS リダイレクトリスナーは cap_enter 後の bind になるため不可。
            // 判定はリロード時（F-178）と共用する。上流への接続は接続ブローカー（F-182）が
            // 代行するので、プロキシ・upstream は阻害要因ではない。
            let facts = crate::config::CapModeFacts { has_l4, http3 };
            // F-181: 要求したのに入れない構成は、既定で起動を中止する（fail-closed）。
            use crate::config::CapModeDecision;
            match crate::config::capability_mode_decision(
                true,
                crate::config::capability_mode_blocker(&facts),
                sec.allow_security_failures,
            ) {
                // F-182: 上流があれば、cap_enter より前（ここ。特権降格の後）に接続ブローカーを
                // 起動する。起動できなければ他の失敗と同じく既定で起動を中止する。
                CapModeDecision::Enter => {
                    let allow = crate::config::startup_connect_allowlist(&loaded_config);
                    if allow.is_empty() {
                        true
                    } else {
                        match crate::connect_broker::start(allow) {
                            Ok(()) => true,
                            Err(e) if sec.allow_security_failures => {
                                warn!(
                                    "capsicum: failed to start the connect broker ({}); continuing in \
                                     rights-limited mode because allow_security_failures = true",
                                    e
                                );
                                false
                            }
                            Err(e) => {
                                let msg = format!(
                                    "capsicum: failed to start the connect broker ({}); aborting startup \
                                     (set allow_security_failures = true to continue in rights-limited mode)",
                                    e
                                );
                                error!("{}", msg);
                                eprintln!("{}", msg);
                                std::process::exit(1);
                            }
                        }
                    }
                }
                CapModeDecision::NotRequested => false,
                CapModeDecision::RightsLimitedOnly(b) => {
                    warn!(
                        "capsicum: capability mode requested but {}; continuing in \
                         rights-limited mode because allow_security_failures = true",
                        b.as_str()
                    );
                    false
                }
                CapModeDecision::Abort(b) => {
                    let msg = format!(
                        "capsicum: capability mode requested but {}; aborting startup \
                         (set allow_security_failures = true to continue in rights-limited mode)",
                        b.as_str()
                    );
                    error!("{}", msg);
                    eprintln!("{}", msg);
                    std::process::exit(1);
                }
            }
        } else {
            false
        }
    };
    #[cfg(target_os = "freebsd")]
    if loaded_config.global_security.enable_capsicum {
        info!("capsicum: cap_rights_limit enforcement enabled for long-lived fds");
    }
    #[cfg(not(target_os = "freebsd"))]
    if loaded_config.global_security.enable_capsicum
        || loaded_config.global_security.capsicum_capability_mode
    {
        warn!("enable_capsicum / capsicum_capability_mode is set but this build does not target FreeBSD; ignoring");
    }
    #[cfg(not(target_os = "openbsd"))]
    if loaded_config.global_security.enable_pledge {
        warn!("enable_pledge is set but this build does not target OpenBSD; ignoring");
    }
    #[cfg(not(target_os = "macos"))]
    if loaded_config.global_security.enable_sandbox_macos {
        warn!("enable_sandbox_macos is set but this build does not target macOS; ignoring");
    }
    #[cfg(not(windows))]
    if loaded_config.global_security.enable_job_object_windows {
        warn!("enable_job_object_windows is set but this build does not target Windows; ignoring");
    }
    // Windows Job Object（best-effort、v0.6.0/F-125）: pledge/macOS sandbox と異なり
    // listener bind のバリアを必要としない（プロセス数/道連れ終了の粗粒度制限のため、
    // 早期に適用しても以降の bind/connect を妨げない）。起動直後のコールドパスで適用する。
    #[cfg(windows)]
    if loaded_config.global_security.enable_job_object_windows {
        match crate::security::windows_security::apply() {
            Ok(()) => {}
            Err(e) => {
                error!("windows job object: failed to apply: {}", e);
                if !loaded_config.global_security.allow_security_failures {
                    std::process::exit(1);
                }
            }
        }
    }

    // 同時接続数制限
    let max_connections = loaded_config.global_security.max_concurrent_connections;

    // FreeBSD capability mode 用の bind 完了バリア。
    // 各 TLS ワーカーが listener bind に成功したらカウントを進め、全ワーカー分
    // 揃ってから `cap_enter(2)` する（capability mode 中は bind 不可のため、
    // 先に入ると全ワーカーが Bind error で死ぬ）。
    #[cfg(target_os = "freebsd")]
    let listeners_ready = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // OpenBSD pledge(2) 用の bind 完了バリア。
    // pledge の "inet" promise は bind(2)/connect(2)/accept(2) を許すため FreeBSD の
    // capability mode ほど厳密なバリアは不要という見立てだが、適用順序を保守的に
    // 揃えるため FreeBSD と同じ「全ワーカー bind 完了後」に pledge する
    // （設計ドキュメント 4.3 節、E2E で実証の上 Phase 5 メモに記録）。
    #[cfg(target_os = "openbsd")]
    let listeners_ready = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // macOS sandbox_init(3) 用の bind 完了バリア（F-125）。プロファイルはネットワークを
    // 無条件許可するため FreeBSD/OpenBSD ほど bind 順序への依存は無いが、適用順序を
    // 保守的に揃えるため同じく「全ワーカー bind 完了後」に適用する。
    #[cfg(target_os = "macos")]
    let listeners_ready = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // 上の 3 つのバリアが待つ bind の数。TCP の全リスナー（TLS・平文）のワーカーを数える
    // （以前は TLS のワーカーだけで、待つスレッド自体も TLS のリスナーがあるときだけ起動していた。平文だけの設定
    // ＝パッケージの既定の設定では capability mode・pledge・sandbox_init が掛からず、FreeBSD は起動を中止していた。B-163）。
    // F-260: リスナーごとの `threads` の和（HTTP/3 は今までどおり数えない）。
    #[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "macos"))]
    let barrier_expected: usize = loaded_config
        .listeners
        .iter()
        .map(|l| l.worker_count(num_threads))
        .sum();

    // TCP のリスナー（TLS・平文）を `[server] listen` → `listen_cleartext` → `[[listener]]` の順に起動する（F-260）。
    let worker_env = crate::listener_workers::WorkerEnv {
        core_ids: core_ids.clone(),
        balancing: reuseport_balancing,
        max_conn: max_connections,
        #[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "macos"))]
        listeners_ready: std::sync::Arc::clone(&listeners_ready),
    };
    // veil 自身が bind した UDS のソケットファイル（グレースフルシャットダウン時に unlink する。F-164）
    #[cfg(unix)]
    let mut unix_socket_paths: Vec<std::path::PathBuf> = Vec::new();
    for (id, spec) in loaded_config.listeners.iter().enumerate() {
        let workers = spec.worker_count(num_threads);
        let listener_acceptor = spec.alpn_profile().map(|p| {
            let config = loaded_config.tls_profiles[p.index()]
                .clone()
                .unwrap_or_else(|| loaded_config.tls_config.clone());
            acceptor.clone().with_profile(p, config)
        });
        match crate::listener_workers::spawn_tcp_listener(
            id,
            spec,
            workers,
            listener_acceptor,
            &worker_env,
            &mut handles,
        ) {
            #[cfg(unix)]
            Ok(path) => unix_socket_paths.extend(path),
            #[cfg(not(unix))]
            Ok(_) => {}
            Err(e) => {
                error!(
                    "Bind error on listener \"{}\" {}: {}",
                    spec.name, spec.addr, e
                );
                return;
            }
        }
    }
    if !loaded_config.listeners.iter().any(|l| l.tls) {
        info!("No TLS listener: plaintext-only server");
    }

    // FreeBSD capability mode（オプトイン）: 全リスナー（TLS・平文）のワーカーの bind 完了を待って
    // `cap_enter(2)` する。cap_enter はプロセス全体に即時に効くため、bind が
    // 終わる前に入ると全ワーカーが Bind error で死ぬ（バリアの根拠は前段コメント）。
    #[cfg(target_os = "freebsd")]
    if capsicum_capability_mode_requested {
        let listeners_ready = std::sync::Arc::clone(&listeners_ready);
        let expected = barrier_expected;
        // F-123/F-153: 静的ルート dirfd の登録は設定ロード直後（この関数の先頭付近、
        // `cache::resolve::register_static_roots` 呼び出し）で **cap_enter よりずっと前**
        // に完了済み（`capsicum::init_static_dirfds` 経由。canonicalize/絶対パス open が
        // まだ許可されているタイミングで実行される）。ここで二重に登録し直すと
        // `capsicum::STATIC_DIRS`（`OnceLock`）への 2 回目の `set` が黙って無視され、
        // 1 回目に open した fd がリークするため、ここでは登録し直さない。
        // F-136: capability mode 下でも TLS 証明書ホットリロードを動作させるため、
        // cert/key の親ディレクトリ fd を **cap_enter 前** に登録する。auto_reload が
        // 無効なら TLS リロードスレッド自体が起動しないため登録は不要。
        let allow_failures = loaded_config.global_security.allow_security_failures;
        let tls_auto_reload = loaded_config.tls_auto_reload;
        let tls_cert_path = std::path::PathBuf::from(&loaded_config.tls_cert_path);
        let tls_key_path = std::path::PathBuf::from(&loaded_config.tls_key_path);
        // F-178: 設定ファイルとアクセスログの親ディレクトリ fd も cap_enter 前に登録し、
        // capability mode 下の SIGHUP でも設定を読み直し・ログを開き直せるようにする。
        let config_path = (**crate::config::CONFIG_PATH.load()).clone();
        #[cfg(feature = "access-log")]
        let access_log_path = {
            let cfg = &loaded_config.access_log_config;
            if cfg.enabled {
                cfg.file_path.clone().map(std::path::PathBuf::from)
            } else {
                None
            }
        };
        std::thread::Builder::new()
            .name("veil-cap-enter".to_string())
            .spawn(move || {
                use std::sync::atomic::Ordering;
                // 起動時コールドパスの専用監視スレッド（イベントループ外）のため
                // ポーリング sleep でよい。最大 ~30 秒待つ。
                for _ in 0..3000 {
                    if listeners_ready.load(Ordering::Acquire) >= expected {
                        // cap_enter 後の初回オフロードでワーカースレッドを遅延生成
                        // すると、capability mode 下のスレッド生成がスタックガード設定
                        // 等で失敗し得る。cap_enter 前にプールを暖機しておく（F-123）。
                        crate::runtime::offload::warmup();
                        // F-153: 静的ルート dirfd は既に `register_static_roots`
                        // （関数冒頭）で登録済み。
                        // F-136: TLS 証明書 dirfd の登録（auto_reload 有効時のみ）。
                        if tls_auto_reload {
                            match crate::security::capsicum::init_tls_cert_dirfds(
                                &tls_cert_path,
                                &tls_key_path,
                            ) {
                                Ok(()) => info!(
                                    "capsicum: TLS certificate hot-reload will remain functional under capability mode (dirfd relative openat/fstatat)"
                                ),
                                Err(e) => error!(
                                    "capsicum: failed to register the TLS certificate directory fds; \
                                     certificate hot reload may not work in capability mode: {}",
                                    e
                                ),
                            }
                        }
                        match crate::security::capsicum::init_config_dirfd(&config_path) {
                            Ok(()) => {}
                            Err(e) => error!(
                                "capsicum: failed to register the config file directory fd; \
                                 SIGHUP config reload will not work in capability mode: {}",
                                e
                            ),
                        }
                        #[cfg(feature = "access-log")]
                        if let Some(p) = &access_log_path {
                            if let Err(e) =
                                crate::security::capsicum::init_access_log_dirfd(p)
                            {
                                error!(
                                    "capsicum: failed to register the access log directory fd; \
                                     reopening the log on SIGHUP will not work in capability mode: {}",
                                    e
                                );
                            }
                        }
                        match crate::security::capsicum::enter_capability_mode() {
                            Ok(()) => info!(
                                "capsicum: capability mode active ({} listeners bound)",
                                expected
                            ),
                            Err(e) => {
                                error!("capsicum: cap_enter failed: {}", e);
                                // F-181: 要求した防御が効かないまま動かさない。
                                if !allow_failures {
                                    eprintln!("capsicum: cap_enter failed: {}; aborting", e);
                                    std::process::exit(1);
                                }
                            }
                        }
                        return;
                    }
                    // 理由付き allow: 起動時コールドパスの専用スレッド。イベントループを
                    // ブロックしない。
                    #[allow(clippy::disallowed_methods)]
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                error!(
                    "capsicum: timed out waiting for {} listeners; capability mode NOT applied",
                    expected
                );
                // F-181: 要求した防御が効かないまま動かさない。
                if !allow_failures {
                    eprintln!(
                        "capsicum: timed out waiting for {} listeners; aborting",
                        expected
                    );
                    std::process::exit(1);
                }
            })
            .expect("failed to spawn veil-cap-enter thread");
    }

    // OpenBSD pledge（オプトイン）: 全リスナー（TLS・平文）のワーカーの bind 完了を待って pledge する
    // （バリアの根拠は前段コメント。promise 集合は E2E で実証した最小構成
    //   — docs/backlog/features/F-120 の Phase 5 記録を参照）。
    #[cfg(target_os = "openbsd")]
    if loaded_config.global_security.enable_pledge {
        let listeners_ready = std::sync::Arc::clone(&listeners_ready);
        let expected = barrier_expected;
        let allow_failures = loaded_config.global_security.allow_security_failures;
        std::thread::Builder::new()
            .name("veil-pledge".to_string())
            .spawn(move || {
                use std::sync::atomic::Ordering;
                // 起動時コールドパスの専用監視スレッド（イベントループ外）のため
                // ポーリング sleep でよい。最大 ~30 秒待つ。
                for _ in 0..3000 {
                    if listeners_ready.load(Ordering::Acquire) >= expected {
                        match crate::security::pledge_unveil::pledge_promises(PLEDGE_PROMISES) {
                            Ok(()) => {
                                info!("pledge: promises restricted ({} listeners bound)", expected)
                            }
                            Err(e) => {
                                error!("pledge: failed to apply: {}", e);
                                if !allow_failures {
                                    std::process::exit(1);
                                }
                            }
                        }
                        return;
                    }
                    // 理由付き allow: 起動時コールドパスの専用スレッド。イベントループを
                    // ブロックしない。
                    #[allow(clippy::disallowed_methods)]
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                error!(
                    "pledge: timed out waiting for {} listeners; pledge NOT applied",
                    expected
                );
                // B-163: 要求した防御が効かないまま動かさない（FreeBSD の capability mode と同じく fail-closed）
                if !allow_failures {
                    eprintln!(
                        "pledge: timed out waiting for {} listeners; aborting",
                        expected
                    );
                    std::process::exit(1);
                }
            })
            .expect("failed to spawn veil-pledge thread");
    }

    // macOS sandbox_init（オプトイン）: 全リスナー（TLS・平文）のワーカーの bind 完了を待って適用する
    // （バリアの根拠は前段コメント。プロファイル生成は `crate::security::macos_sandbox`、
    // 設定から導出するパス集合は `crate::config::collect_macos_sandbox_paths`）。
    #[cfg(target_os = "macos")]
    if loaded_config.global_security.enable_sandbox_macos {
        let listeners_ready = std::sync::Arc::clone(&listeners_ready);
        let expected = barrier_expected;
        let allow_failures = loaded_config.global_security.allow_security_failures;
        let config_path_for_sandbox = config_path.clone();
        std::thread::Builder::new()
            .name("veil-macos-sandbox".to_string())
            .spawn(move || {
                use std::sync::atomic::Ordering;
                // 起動時コールドパスの専用監視スレッド（イベントループ外）のため
                // ポーリング sleep でよい。最大 ~30 秒待つ。
                for _ in 0..3000 {
                    if listeners_ready.load(Ordering::Acquire) >= expected {
                        let result =
                            crate::config::collect_macos_sandbox_paths(&config_path_for_sandbox)
                                .and_then(|paths| {
                                    let profile =
                                        crate::security::macos_sandbox::build_profile(&paths);
                                    crate::security::macos_sandbox::apply(&profile)
                                });
                        match result {
                            Ok(()) => info!(
                                "macos sandbox: sandbox_init applied ({} listeners bound)",
                                expected
                            ),
                            Err(e) => {
                                error!("macos sandbox: failed to apply: {}", e);
                                if !allow_failures {
                                    std::process::exit(1);
                                }
                            }
                        }
                        return;
                    }
                    // 理由付き allow: 起動時コールドパスの専用スレッド。イベントループを
                    // ブロックしない。
                    #[allow(clippy::disallowed_methods)]
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                error!(
                    "macos sandbox: timed out waiting for {} listeners; sandbox_init NOT applied",
                    expected
                );
                // B-163: 要求した防御が効かないまま動かさない（fail-closed）
                if !allow_failures {
                    eprintln!(
                        "macos sandbox: timed out waiting for {} listeners; aborting",
                        expected
                    );
                    std::process::exit(1);
                }
            })
            .expect("failed to spawn veil-macos-sandbox thread");
    }

    // HTTP/3 (QUIC/UDP) サーバー（`http3` のリスナーごと。F-260）
    // TCP側と同様に複数スレッドで並列起動し、CPUコアにピンニング
    //
    // 注意: quicheはファイルパスからの証明書読み込みのみサポートしているため、
    // 証明書は事前に読んだ PEM を memfd 経由で渡す（Landlock の許可パスに証明書を足さなくてよい）。
    // 証明書の PEM を写し終えた HTTP/3 のワーカーの数（全員が写してから元の PEM を消す）
    #[cfg(feature = "http3")]
    let http3_pem_copied = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    #[cfg(feature = "http3")]
    let http3_workers: usize = {
        let mut total = 0usize;
        let h3_listeners: Vec<(&crate::config::ListenerSpec, std::net::SocketAddr)> = loaded_config
            .listeners
            .iter()
            .filter_map(|l| l.http3.map(|a| (l, a)))
            .collect();
        if !h3_listeners.is_empty() {
            if loaded_config.global_security.enable_landlock {
                info!("[HTTP/3] Landlock enabled - using memfd for certificate loading");
                info!("[HTTP/3] No need to add certificate paths to landlock_read_paths");
            }
            info!(
                "[HTTP/3] TLS Cert: {} (pre-loaded, {} bytes), TLS Key: {} (pre-loaded, {} bytes)",
                loaded_config.tls_cert_path,
                loaded_config.tls_cert_pem.len(),
                loaded_config.tls_key_path,
                loaded_config.tls_key_pem.len()
            );
            // [http3] セクションの quiche 輸送パラメータ・GSO/GRO・バッチ幅などをワーカーへ渡す（全リスナー共通）。
            // 証明書は memfd 経由で渡すため cert_path/key_path は空のまま、PEM バイトを差し込む。
            let http3_server_base = loaded_config
                .http3_config
                .to_http3_config(&loaded_config.tls_cert_path, &loaded_config.tls_key_path);
            for (spec, addr) in h3_listeners {
                let workers = spec.worker_count(num_threads);
                crate::listener_workers::spawn_http3_listener(
                    spec,
                    addr,
                    workers,
                    &http3_server_base,
                    &loaded_config.tls_cert_pem,
                    &loaded_config.tls_key_pem,
                    &http3_pem_copied,
                    &worker_env,
                    &mut handles,
                );
                total += workers;
            }
        }
        total
    };
    #[cfg(not(feature = "http3"))]
    if loaded_config.listeners.iter().any(|l| l.http3.is_some()) {
        warn!("HTTP/3 is configured but this build does not include the http3 feature; ignoring");
    }

    // L4 (TCP/UDP) ストリームプロキシリスナーを起動（F-18）
    #[cfg(feature = "l4-proxy")]
    {
        if !loaded_config.l4_listeners.is_empty() {
            info!("============================================");
            info!("L4 Stream Proxy");
            info!("Listeners: {}", loaded_config.l4_listeners.len());
            info!("============================================");
            crate::l4::server::spawn_l4_listeners(
                &loaded_config.l4_listeners,
                num_threads,
                loaded_config.reuseport_balancing,
            );
        }
    }

    // HTTP/3 の全ワーカーが証明書データを写し終えるのを数えてから、LoadedConfig の証明書データを
    // セキュアにゼロ化する（F-260: 複数の HTTP/3 リスナーの全員が写すまで消さない）
    #[cfg(feature = "http3")]
    if http3_workers > 0 {
        // 起動直後に一度だけの待ち（ワーカーの PEM の複製と Arc の解放待ち）。最大 ~5 秒。
        for _ in 0..500 {
            if http3_pem_copied.load(Ordering::Acquire) >= http3_workers {
                break;
            }
            // 理由付き allow: メインスレッドで一度だけ実行する起動後の待機（ワーカーの Arc ドロップ待ち）。イベントループ外。
            #[allow(clippy::disallowed_methods)]
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // LoadedConfig の証明書データをセキュアにゼロ化
        secure_clear_arc_vec(
            &mut loaded_config.tls_cert_pem,
            "TLS certificate (LoadedConfig)",
        );
        secure_clear_arc_vec(
            &mut loaded_config.tls_key_pem,
            "TLS private key (LoadedConfig)",
        );
    }

    STARTUP_COMPLETED.store(true, std::sync::atomic::Ordering::Release);

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

    // F-164: veil 自身が bind した UDS ソケットファイルを unlink する
    // （全ワーカー join 後 = accept ループが抜けて fd が close された後）。
    #[cfg(unix)]
    for path in &unix_socket_paths {
        crate::server::unlink_unix_socket(path);
    }

    // F-10: OpenTelemetry エクスポータをクリーンに停止（最終 flush 込み）
    #[cfg(feature = "opentelemetry")]
    crate::otel::shutdown_global();

    info!("Server shutdown complete");
}
