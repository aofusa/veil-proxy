//! サンドボックス（UDS・OpenBSD の unveil・macOS の sandbox）の対象パスの収集
//!
//! F-233 で `src/config.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// UDS ソケットパス収集（F-164 リスナー・F-170 バックエンド、OpenBSD/macOS 共通）
// ====================

/// 生の `Config`（unveil/Seatbelt 専用の再パース結果）から UDS ソケットパスを収集する。
///
/// - `[server].listen` / `[server].listen_cleartext` の `unix:<path>`（F-164。リスナー側は
///   これまで収集漏れだった）。
/// - `[[route]]` の Proxy URL（`http(s)://unix:<path>[:<prefix>]`）・
///   `[upstreams.*.servers]` の URL（F-170、単一 URL プロキシと Upstream グループの両方）。
/// - `[[l4]].upstreams` の `addr`（`unix:<path>`、F-170、上流のみ。`[[l4]].listen` は
///   対象外）。
///
/// ソケットファイル自体は `bind`/`connect` 時点では存在するとは限らない
/// （リスナー側は自分で作成、バックエンド側は先方プロセスが作成）ため、
/// `push_unveil_parent_dir`/`push_sandbox_parent_dir` と同じ「ファイル単体を渡す」
/// 方針をそのまま踏襲する（`unveil_path`/Seatbelt 側が不存在パスを無害にスキップする、
/// または `connect(2)` 時点でのみ意味を持つため親ディレクトリ限定は不要）。
#[cfg(any(target_os = "openbsd", target_os = "macos"))]
pub(super) fn collect_uds_socket_paths(config: &Config) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    for listen in [
        config.server.listen.as_ref(),
        config.server.listen_cleartext.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(path) = listen.strip_prefix("unix:") {
            if !path.is_empty() {
                paths.push(PathBuf::from(path));
            }
        }
    }

    if let Some(routes) = &config.route {
        for route in routes {
            if let BackendConfig::Proxy { url, .. } = &route.action {
                if let Some(target) = ProxyTarget::parse(url) {
                    if let Some(p) = &target.unix_path {
                        paths.push(PathBuf::from(p.as_ref()));
                    }
                }
            }
        }
    }

    if let Some(upstreams) = &config.upstreams {
        for group in upstreams.values() {
            for server in &group.servers {
                if let Some(target) = ProxyTarget::parse(&server.url) {
                    if let Some(p) = &target.unix_path {
                        paths.push(PathBuf::from(p.as_ref()));
                    }
                }
            }
        }
    }

    #[cfg(feature = "l4-proxy")]
    if let Some(l4_listeners) = &config.l4 {
        for l4 in l4_listeners {
            for u in &l4.upstreams {
                if let Some(path) = u.addr.strip_prefix("unix:") {
                    paths.push(PathBuf::from(path));
                }
            }
        }
    }

    paths
}

/// `LoadedConfig` から UDS バックエンドのソケットパスを収集する（F-170: Linux Landlock 用）。
///
/// `collect_uds_socket_paths`（OpenBSD/macOS、生の TOML 再パース）と異なり、こちらは
/// 起動時に既に構築済みの `Route::resolved_backend` / `upstream_groups` をそのまま
/// 辿る（ロード時に一度だけ実行するコールドパスであり、`entry.rs` の Landlock 適用箇所
/// から呼ぶ）。Landlock の FS アクセス権限は UDS への `connect(2)` を仲介しないと
/// 考えられるが、将来の ABI 変更に備えた保守的措置として書き込み許可へ追加する。
pub(crate) fn collect_uds_backend_paths_from_loaded(
    routes: &[Route],
    upstream_groups: &HashMap<String, Arc<UpstreamGroup>>,
    l4_listeners: &[L4ListenerConfig],
) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    let mut push_group = |group: &UpstreamGroup| {
        for server in &group.servers {
            if let Some(p) = &server.target.unix_path {
                paths.push(PathBuf::from(p.as_ref()));
            }
        }
    };

    for route in routes {
        if let Some(Backend::Proxy(group, ..)) = &route.resolved_backend {
            push_group(group);
        }
    }
    for group in upstream_groups.values() {
        push_group(group);
    }

    for l4 in l4_listeners {
        for u in &l4.upstreams {
            if let Some(path) = u.addr.strip_prefix("unix:") {
                paths.push(PathBuf::from(path));
            }
        }
    }

    paths
}

// ====================
// OpenBSD: unveil 対象パス収集（F-120 Phase 5）
// ====================

/// OpenBSD unveil(2) に渡すパス集合。
#[cfg(target_os = "openbsd")]
pub struct UnveilPaths {
    /// 読み取り専用で unveil するパス（"r"）: 設定ファイル・TLS証明書/鍵・静的ファイル
    /// ルート・WASM モジュール。
    pub read_only: Vec<PathBuf>,
    /// 読み書き・作成を伴う unveil するパス（"rwc"）: アクセスログ/アプリログの
    /// ディレクトリ・ディスクキャッシュディレクトリ。
    pub read_write_create: Vec<PathBuf>,
}

/// 設定ファイルを軽量に再パースし、unveil(2) に必要なパス集合を導出する（OpenBSD 専用）。
///
/// `load_config` が返す `LoadedConfig` は WASM エンジン構築・TLS 証明書読み込み等で
/// 個々のパス文字列を消費済みで再取得できないフィールドがあるため、unveil 専用に
/// 設定ファイルをもう一度パースする（起動時コールドパスで 1 回のみ・ホットパス無関係）。
#[cfg(target_os = "openbsd")]
// 起動時コールドパス（unveil 前に 1 回だけ設定を読み直す）。データプレーン非経由。
#[allow(clippy::disallowed_methods)]
pub fn collect_unveil_paths(config_path: &Path) -> io::Result<UnveilPaths> {
    let config_str = read_config_text(config_path)?;
    let config: Config = crate::config_override::apply_to_toml_str(&config_str)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let mut read_only = vec![config_path.to_path_buf()];
    let mut read_write_create = Vec::new();

    if config.tls.is_configured() {
        read_only.push(PathBuf::from(&config.tls.cert_path));
        read_only.push(PathBuf::from(&config.tls.key_path));
    }

    // プロキシ経路の名前解決（getaddrinfo）と upstream TLS 検証に必要なシステムファイル。
    // 静的配信のみの構成では未使用だが、存在すれば読み取り許可しておく（unveil_path は
    // 不存在パスをスキップするため、無害）。Landlock のデフォルト読み取りパスと同方針。
    for sys_path in [
        "/etc/resolv.conf",
        "/etc/hosts",
        "/etc/ssl/cert.pem", // OpenBSD のシステム信頼ストア
        "/etc/ssl",
    ] {
        read_only.push(PathBuf::from(sys_path));
    }

    if let Some(routes) = &config.route {
        for route in routes {
            if let BackendConfig::File { path, .. } = &route.action {
                read_only.push(PathBuf::from(path));
            }
            if let Some(cache_cfg) = &route.cache {
                if let Some(disk_path) = &cache_cfg.disk_path {
                    read_write_create.push(disk_path.clone());
                }
            }
        }
    }

    #[cfg(feature = "wasm")]
    if let Some(wasm_cfg) = &config.wasm {
        for module in &wasm_cfg.modules {
            read_only.push(PathBuf::from(&module.path));
        }
    }

    #[cfg(feature = "access-log")]
    if let Some(file_path) = &config.access_log.file_path {
        push_unveil_parent_dir(&mut read_write_create, file_path);
    }

    if let Some(file_path) = &config.logging.app_file_path {
        push_unveil_parent_dir(&mut read_write_create, file_path);
    }
    if let Some(file_path) = &config.logging.error_file_path {
        push_unveil_parent_dir(&mut read_write_create, file_path);
    }

    // F-164/F-170: UDS リスナー・UDS バックエンドのソケットパス（`unix` promise で
    // socket/connect/bind が必要）。socket/connect/bind はいずれもパス自体を対象と
    // するため（親ディレクトリではなく）、`read_write_create` にそのまま追加する。
    read_write_create.extend(collect_uds_socket_paths(&config));

    Ok(UnveilPaths {
        read_only,
        read_write_create,
    })
}

// ====================
// macOS: sandbox_init 対象パス収集（F-125）
// ====================

/// 設定ファイルを軽量に再パースし、`sandbox_init(3)`（Seatbelt）の SBPL プロファイル生成に
/// 必要なパス集合を導出する（macOS 専用）。`collect_unveil_paths`（OpenBSD）と同じ理由
/// （`LoadedConfig` はパス文字列を消費済みで再取得できないフィールドがある）で、起動時
/// コールドパスにおいて設定ファイルをもう一度パースする。
#[cfg(target_os = "macos")]
// 起動時コールドパス（sandbox_init 前に 1 回だけ設定を読み直す）。データプレーン非経由。
#[allow(clippy::disallowed_methods)]
pub fn collect_macos_sandbox_paths(
    config_path: &Path,
) -> io::Result<crate::security::macos_sandbox::SandboxPaths> {
    use crate::security::macos_sandbox::SandboxPaths;

    let config_str = read_config_text(config_path)?;
    let config: Config = crate::config_override::apply_to_toml_str(&config_str)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let mut static_roots = Vec::new();
    // `read_only` は wasm feature 有効時のみ変更されるため、mut 修飾を feature で分岐する
    // （wasm 無効ビルドで `unused_mut` 警告を出さないため。allow は使わない）。
    // 平文専用サーバ（`[tls]` 無し）では証明書パスが空なので含めない。
    let tls_paths = [&config.tls.cert_path, &config.tls.key_path]
        .into_iter()
        .filter(|p| !p.is_empty())
        .map(PathBuf::from);
    #[cfg(feature = "wasm")]
    let mut read_only: Vec<PathBuf> = tls_paths.collect();
    #[cfg(not(feature = "wasm"))]
    let read_only: Vec<PathBuf> = tls_paths.collect();
    let mut read_write = Vec::new();

    if let Some(routes) = &config.route {
        for route in routes {
            if let BackendConfig::File { path, .. } = &route.action {
                static_roots.push(PathBuf::from(path));
            }
            if let Some(cache_cfg) = &route.cache {
                if let Some(disk_path) = &cache_cfg.disk_path {
                    read_write.push(disk_path.clone());
                }
            }
        }
    }

    #[cfg(feature = "wasm")]
    if let Some(wasm_cfg) = &config.wasm {
        for module in &wasm_cfg.modules {
            read_only.push(PathBuf::from(&module.path));
        }
    }

    #[cfg(feature = "access-log")]
    if let Some(file_path) = &config.access_log.file_path {
        push_sandbox_parent_dir(&mut read_write, file_path);
    }

    if let Some(file_path) = &config.logging.app_file_path {
        push_sandbox_parent_dir(&mut read_write, file_path);
    }
    if let Some(file_path) = &config.logging.error_file_path {
        push_sandbox_parent_dir(&mut read_write, file_path);
    }

    // F-164/F-170: UDS リスナー・UDS バックエンドのソケットパス。既存プロファイルは
    // `(allow system-socket)` 済みだが、パス指定の `connect`/`bind` にはファイルシステム
    // パーミッションも必要なため読み書き許可へ追加する。
    read_write.extend(collect_uds_socket_paths(&config));

    Ok(SandboxPaths {
        static_roots,
        read_only,
        read_write,
    })
}

/// ログ/アクセスログファイルパスの親ディレクトリ（ローテーション・新規作成に必要）を
/// 書き込み許可対象へ追加する。親ディレクトリが取得できない場合はファイルパス自体を
/// 追加する（`push_unveil_parent_dir` と同じロジック。OpenBSD/macOS で cfg が異なるため
/// 個別に定義する）。
#[cfg(target_os = "macos")]
pub(super) fn push_sandbox_parent_dir(paths: &mut Vec<PathBuf>, file_path: &str) {
    let p = Path::new(file_path);
    match p.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => paths.push(parent.to_path_buf()),
        _ => paths.push(p.to_path_buf()),
    }
}

/// ログ/アクセスログファイルパスの親ディレクトリ（ローテーション・新規作成に必要）を
/// rwc 対象へ追加する。親ディレクトリが取得できない場合はファイルパス自体を追加する。
#[cfg(target_os = "openbsd")]
pub(super) fn push_unveil_parent_dir(paths: &mut Vec<PathBuf>, file_path: &str) {
    let p = Path::new(file_path);
    match p.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => paths.push(parent.to_path_buf()),
        _ => paths.push(p.to_path_buf()),
    }
}
