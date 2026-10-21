//! 設定の検証（Alt-Svc・ルート・リスナー・TLS の項目・平文の待ち受け）
//!
//! F-233 で `src/config.rs` から移した（中身は変えていない）。

use super::*;

/// HTTP/3 有効時に Alt-Svc 広告を初期化する（F-94、コールドパス）。
///
/// `[http3] enabled && [http3].alt_svc_enabled` のとき、
/// `[http3].alt_svc` 明示値、または listen ポートから自動生成した値を登録する。
/// Alt-Svc 関連キーはすべて `[http3]` に集約する。
pub(super) fn apply_alt_svc_from_config(config: &Config) {
    #[cfg(feature = "http3")]
    {
        let enabled = config.http3.enabled && config.http3.alt_svc_enabled;
        let value = if enabled {
            if let Some(ref explicit) = config.http3.alt_svc {
                explicit.clone()
            } else {
                let listen = config
                    .http3
                    .listen
                    .as_deref()
                    .or(config.server.listen.as_deref())
                    .unwrap_or("");
                crate::pool::build_alt_svc_value(listen, config.http3.alt_svc_ma_secs)
            }
        } else {
            String::new()
        };
        crate::pool::init_alt_svc(enabled, &value);
    }
    #[cfg(not(feature = "http3"))]
    {
        let _ = config;
        crate::pool::init_alt_svc(false, "");
    }
}

/// 上流 URL のパース失敗理由を説明するメッセージ（F-170）。
///
/// `ProxyTarget::parse` は非 unix プラットフォームで `http(s)://unix:<path>` を常に
/// `None` にする（`parse_unix` が `#[cfg(not(unix))]` で `None` を返す）ため、素の
/// "invalid url" だけでは原因（UDS 非対応）が分からない。`unix:` 表記かつ非 unix
/// ビルドの場合はその旨を明示する。
pub(super) fn proxy_url_invalid_reason(url: &str) -> String {
    #[cfg(not(unix))]
    {
        let rest = url
            .strip_prefix("http://")
            .or_else(|| url.strip_prefix("https://"));
        if rest.is_some_and(|r| r.starts_with("unix:")) {
            return format!(
                "{} (unix domain socket backends are not supported on this platform)",
                url
            );
        }
    }
    url.to_string()
}

#[cfg(test)]
pub(super) mod proxy_url_invalid_reason_tests {
    use super::proxy_url_invalid_reason;

    #[test]
    fn plain_invalid_url_is_unchanged() {
        assert_eq!(
            proxy_url_invalid_reason("not a url"),
            "not a url".to_string()
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn unix_notation_on_non_unix_explains_platform_reason() {
        let msg = proxy_url_invalid_reason("http://unix:/run/app.sock");
        assert!(msg.contains("not supported on this platform"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_notation_on_unix_is_unchanged_since_parse_would_succeed() {
        // unix ビルドでは `unix:` 表記自体は有効なため、ここに到達するのは
        // 別の理由（空パス等）でパースに失敗した場合のみ。特別なメッセージ拡張はしない。
        let msg = proxy_url_invalid_reason("http://unix:");
        assert_eq!(msg, "http://unix:".to_string());
    }
}

/// File ルートのパスが存在するか。
///
/// F-178: FreeBSD の capability mode 中（リロード時）は絶対パスの stat ができないため、
/// 登録済み静的ルートの dirfd 相対で確かめる（登録外のルートはリロード前に拒否済み）。
pub(super) fn file_route_path_exists(path: &Path) -> bool {
    #[cfg(target_os = "freebsd")]
    if crate::security::capsicum::is_capability_mode() {
        return matches!(crate::security::capsicum::stat_static(path), Some(Ok(_)));
    }
    path.exists()
}

/// `validate_config` の証明書ファイル存在チェックを省くか（F-178: FreeBSD の capability mode 中）。
pub(super) fn tls_file_check_suppressed() -> bool {
    #[cfg(target_os = "freebsd")]
    {
        crate::security::capsicum::is_capability_mode()
    }
    #[cfg(not(target_os = "freebsd"))]
    {
        false
    }
}

impl TlsConfigSection {
    /// 証明書・秘密鍵が設定されているか（どちらか一方でも書かれていれば「設定あり」として検査する）。
    pub fn is_configured(&self) -> bool {
        !self.cert_path.is_empty() || !self.key_path.is_empty()
    }
}

/// `[tls]` を省略したときの既定値（証明書無し = TLS 無し）。
pub(super) fn default_tls_section() -> TlsConfigSection {
    toml::from_str("").expect("TlsConfigSection must deserialize from an empty table")
}

/// `[server].listen_cleartext` の受け入れ方（F-200）。リロードで変えられる（接続ごとに読む）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleartextPolicy {
    /// 平文 HTTP/1.1 を受ける
    pub allow_http: bool,
    /// h2c（prior knowledge）を受ける
    pub allow_h2c: bool,
    /// 受けた要求を https へ 301 でリダイレクトする
    pub secure_redirect: bool,
}

impl Default for CleartextPolicy {
    fn default() -> Self {
        CleartextPolicy {
            allow_http: true,
            allow_h2c: true,
            secure_redirect: true,
        }
    }
}

impl CleartextPolicy {
    pub(super) fn from_server(server: &ServerConfigSection) -> CleartextPolicy {
        CleartextPolicy {
            allow_http: server.allow_http,
            allow_h2c: server.allow_h2c,
            secure_redirect: server.enable_secure_redirect,
        }
    }
}

/// TLS が必要な構成か（`[server].listen` か HTTP/3 がある。F-200）。
pub fn tls_required(config_server: &ServerConfigSection, http3_enabled: bool) -> bool {
    config_server.listen.is_some() || http3_enabled
}

/// F-200 で削除したキーを、移行先を示すエラーにする。
pub(super) fn validate_removed_keys(server: &ServerConfigSection) -> io::Result<()> {
    let removed: [(&str, bool, &str); 6] = [
        (
            "[server].http",
            server.removed_http.is_some(),
            "use [server].listen_cleartext (HTTPS redirect is [server].enable_secure_redirect, default true)",
        ),
        (
            "[server].tls_only",
            server.removed_tls_only.is_some(),
            "[server].listen is always TLS-only now; serve plaintext on [server].listen_cleartext",
        ),
        (
            "[server].h2c_enabled",
            server.removed_h2c_enabled.is_some(),
            "use [server].listen_cleartext with allow_h2c = true (default)",
        ),
        (
            "[server].h2c_listen",
            server.removed_h2c_listen.is_some(),
            "use [server].listen_cleartext",
        ),
        (
            "[server].h2c_http1",
            server.removed_h2c_http1.is_some(),
            "use [server].listen_cleartext with allow_http = true (default)",
        ),
        (
            "[server].http3_enabled",
            server.removed_http3_enabled.is_some(),
            "use [http3].enabled",
        ),
    ];
    // F-200 で廃止したキー。エラーの文にはチケット番号を入れず、移行先を hint で示す。
    for (key, present, hint) in removed {
        if present {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} was removed: {}", key, hint),
            ));
        }
    }
    Ok(())
}

/// 待ち受けの規則（F-200）と、TLS の有無の検査。
pub(super) fn validate_listeners(config: &Config) -> io::Result<()> {
    let invalid = |m: String| Err(io::Error::new(io::ErrorKind::InvalidInput, m));
    let server = &config.server;
    validate_removed_keys(server)?;
    if server.listen.is_none() && server.listen_cleartext.is_none() {
        return invalid(
            "no listener: set [server].listen (TLS) and/or [server].listen_cleartext (plaintext)"
                .into(),
        );
    }
    let parse = |key: &str, v: &str| {
        crate::listen_addr::ListenAddr::parse(v).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Invalid {}: {}", key, e),
            )
        })
    };
    let tls_addr = match &server.listen {
        Some(l) => Some(parse("[server].listen", l)?),
        None => None,
    };
    if let Some(c) = &server.listen_cleartext {
        let addr = parse("[server].listen_cleartext", c)?;
        if Some(&addr) == tls_addr.as_ref() {
            return invalid(format!(
                "[server].listen and [server].listen_cleartext must differ ({})",
                c
            ));
        }
        if !server.allow_http && !server.allow_h2c {
            return invalid(
                "[server].listen_cleartext accepts nothing: allow_http and allow_h2c are both false"
                    .into(),
            );
        }
        if server.enable_secure_redirect && server.listen.is_none() {
            return invalid(
                "[server].enable_secure_redirect = true needs a TLS listener ([server].listen) to \
                 redirect to; set enable_secure_redirect = false to serve plaintext"
                    .into(),
            );
        }
    }
    let http3 = config.http3.enabled;
    if http3 && server.listen.is_none() {
        return invalid("[http3].enabled = true needs [server].listen (QUIC requires TLS)".into());
    }
    if tls_required(server, http3) && !config.tls.is_configured() {
        return invalid(
            "TLS certificate is required: set [tls] cert_path and key_path \
             ([server].listen and HTTP/3 are TLS listeners; a plaintext-only server uses only \
             [server].listen_cleartext)"
                .into(),
        );
    }
    Ok(())
}

/// ビルドに含まれていない feature の設定セクションが書かれていたら、無視することを警告する
/// （キーを拒否すると、同じ設定ファイルを feature の違うビルドで使えなくなる）。
pub(super) fn warn_unbuilt_sections(_config: &Config) {
    #[cfg(not(feature = "http2"))]
    if _config.http2.is_some() {
        warn!("[config] [http2] is ignored: this build does not include the http2 feature");
    }
}

pub(super) fn validate_config(config: &Config) -> io::Result<()> {
    validate_listeners(config)?;

    // F-220: SAN の許可リストはクライアント証明書を要求するときだけ意味がある（黙って無視しない）
    if !config.tls.client_allowed_sans.is_empty() && config.tls.client_auth == ClientAuthMode::None
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "[tls] client_allowed_sans requires client_auth = \"optional\" or \"require\"",
        ));
    }
    // F-217: クライアント証明書は TCP の TLS（HTTP/1.1・HTTP/2）だけ。HTTP/3 を併用すると mTLS を迂回できるので拒否する
    if config.tls.client_auth != ClientAuthMode::None {
        if config.tls.client_ca.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "[tls] client_auth requires client_ca",
            ));
        }
        if config.http3.enabled {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "[tls] client_auth is not supported with [http3] enabled (HTTP/3 clients would bypass it)",
            ));
        }
    }

    // TLS証明書ファイルの存在チェック
    //
    // F-178: capability mode 中（＝リロード時）は絶対パスの stat ができず常に「無い」と判定
    // されるため省く。リロードは TLS 設定を引き継ぐので証明書の存在は結果に影響しない
    // （証明書の差し替えは `[tls] auto_reload` の dirfd 経路が扱う。F-136）。
    if !tls_file_check_suppressed() && config.tls.is_configured() {
        let cert_path = Path::new(&config.tls.cert_path);
        if !cert_path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("TLS certificate file not found: {}", config.tls.cert_path),
            ));
        }

        let key_path = Path::new(&config.tls.key_path);
        if !key_path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("TLS key file not found: {}", config.tls.key_path),
            ));
        }
    }

    // UDS ソケットパーミッションの妥当性チェック（F-164）
    if let Err(e) = parse_unix_socket_permissions(&config.server.unix_socket_permissions) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, e));
    }

    // F-164: HTTP/3（QUIC/UDP）は UDS に載せられない。`[server].listen` が
    // `unix:<path>` のとき `[http3].listen` が未指定だと、HTTP/3 リスナーが
    // `[server].listen` を継承して起動時に失敗するため、設定エラーとして先に弾く。
    #[cfg(feature = "http3")]
    if config.http3.enabled
        && config.http3.listen.is_none()
        && config
            .server
            .listen
            .as_deref()
            .and_then(|l| crate::listen_addr::ListenAddr::parse(l).ok())
            .map(|a| a.is_unix())
            .unwrap_or(false)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "HTTP/3 cannot listen on a unix socket: set [http3].listen to a host:port address \
             when [server].listen is a unix:<path> address",
        ));
    }

    // Upstream設定の妥当性チェック
    if let Some(ref upstreams) = config.upstreams {
        for (name, upstream) in upstreams {
            if upstream.servers.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Upstream '{}' has no servers configured", name),
                ));
            }

            for entry in &upstream.servers {
                if ProxyTarget::parse(&entry.url).is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "Invalid server URL in upstream '{}': {}",
                            name,
                            proxy_url_invalid_reason(&entry.url)
                        ),
                    ));
                }
            }
        }
    }

    // F-192: ネイティブ L4 ハンドラの検査（登録済み・プロトコル一致・併用できない設定）。
    #[cfg(feature = "l4-handler")]
    for l4 in config.l4.iter().flatten() {
        if let Some(name) = &l4.handler {
            let proto = match l4.protocol {
                L4Protocol::Tcp => "tcp",
                L4Protocol::Udp => "udp",
            };
            match crate::api::l4::protocol_of(name) {
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("L4 listener '{}': unknown handler '{}'", l4.name, name),
                    ))
                }
                Some(p) if p != proto => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "L4 listener '{}': handler '{}' is a {} handler but protocol = \"{}\"",
                            l4.name, name, p, proto
                        ),
                    ))
                }
                Some(_) => {}
            }
            if l4.tls != L4TlsMode::None {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "L4 listener '{}': a native handler requires tls = \"none\"",
                        l4.name
                    ),
                ));
            }
            #[cfg(feature = "wasm")]
            if !l4.wasm_modules.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "L4 listener '{}': wasm_modules cannot be combined with a native handler",
                        l4.name
                    ),
                ));
            }
        }
    }

    // L4 上流の妥当性チェック（F-170: `unix:<path>` の非対応プラットフォームでの
    // 使用を明確なエラーにする）。
    #[cfg(feature = "l4-proxy")]
    for l4 in config.l4.iter().flatten() {
        #[cfg(unix)]
        for u in &l4.upstreams {
            if let Some(path) = u.addr.strip_prefix("unix:") {
                if path.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "L4 listener '{}' upstream '{}': empty unix socket path",
                            l4.name, u.addr
                        ),
                    ));
                }
            }
        }
        #[cfg(not(unix))]
        for u in &l4.upstreams {
            if u.addr.starts_with("unix:") {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "L4 listener '{}' upstream '{}': unix domain socket backends are not supported on this platform",
                        l4.name, u.addr
                    ),
                ));
            }
        }
        // F-170: UDP 上流は UDS 非対応（QUIC/UDP は AF_UNIX に載らない）。
        if l4.protocol == L4Protocol::Udp {
            for u in &l4.upstreams {
                if u.addr.starts_with("unix:") {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "L4 listener '{}' upstream '{}': unix domain socket backends are not supported for UDP",
                            l4.name, u.addr
                        ),
                    ));
                }
            }
        }
    }

    // 統合ルーティング（[[route]]）の妥当性チェック
    if let Some(ref routes) = config.route {
        for (i, route) in routes.iter().enumerate() {
            let route_name = format!("route[{}]", i);
            validate_route_config(
                route,
                &route_name,
                #[cfg(feature = "wasm")]
                config.wasm.as_ref(),
            )?;
        }
    }

    // WASM設定のバリデーション
    #[cfg(feature = "wasm")]
    if let Some(wasm_cfg) = &config.wasm {
        if wasm_cfg.enabled {
            wasm_cfg.validate().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("WASM config validation failed: {}", e),
                )
            })?;
        }
    }

    // L4 リスナーの WASM モジュール参照チェック（F-133）
    #[cfg(all(feature = "wasm", feature = "l4-proxy"))]
    if let Some(ref wasm_cfg) = config.wasm {
        for l4 in config.l4.iter().flatten() {
            for module_name in &l4.wasm_modules {
                if !wasm_cfg.modules.iter().any(|m| &m.name == module_name) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "L4 listener '{}' references unknown WASM module: {}",
                            l4.name, module_name
                        ),
                    ));
                }
            }

            // F-148: module_configuration のキーが wasm_modules に含まれているかチェック
            if let Some(ref module_configuration) = l4.module_configuration {
                for key in module_configuration.keys() {
                    if !l4.wasm_modules.iter().any(|m| m == key) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "L4 listener '{}' has module_configuration for module '{}' which is not listed in its wasm_modules",
                                l4.name, key
                            ),
                        ));
                    }
                }
            }
        }
    }

    Ok(())
}

/// ルート設定の妥当性チェック
pub(super) fn validate_route_config(
    route: &Route,
    route_name: &str,
    #[cfg(feature = "wasm")] wasm_config: Option<&crate::wasm::WasmConfig>,
) -> io::Result<()> {
    match &route.action {
        BackendConfig::Proxy { url, .. } => {
            if ProxyTarget::parse(url).is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "Invalid proxy URL for route '{}': {}",
                        route_name,
                        proxy_url_invalid_reason(url)
                    ),
                ));
            }
        }
        BackendConfig::ProxyUpstream { upstream, .. } => {
            // upstream の存在は load_backend でチェックされる
            if upstream.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Empty upstream name for route '{}'", route_name),
                ));
            }
        }
        BackendConfig::File { path, mode, .. } => {
            if !file_route_path_exists(Path::new(path)) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "File/directory not found for route '{}': {}",
                        route_name, path
                    ),
                ));
            }

            if !["sendfile", "memory", ""].contains(&mode.as_str()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "Invalid mode for route '{}': {} (expected 'sendfile' or 'memory')",
                        route_name, mode
                    ),
                ));
            }
        }
        BackendConfig::Respond { .. } => {}
        #[cfg(feature = "handler")]
        BackendConfig::Handler { handler } => {
            if crate::api::handler::lookup(handler).is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "Unknown handler '{}' for route '{}' (register it with #[veil::route] or App::route)",
                        handler, route_name
                    ),
                ));
            }
        }
        BackendConfig::Redirect {
            redirect_url,
            redirect_status,
            ..
        } => {
            if redirect_url.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Empty redirect_url for route '{}'", route_name),
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

    // WASMモジュールの参照チェック（route直下のmodulesを使用）
    #[cfg(feature = "wasm")]
    if let Some(wasm_cfg) = wasm_config {
        if let Some(ref modules) = route.modules {
            for module_name in modules {
                if !wasm_cfg.modules.iter().any(|m| &m.name == module_name) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "Route '{}' references unknown WASM module: {}",
                            route_name, module_name
                        ),
                    ));
                }
            }
        }
    }

    // F-148: module_configuration のキーが route.modules に含まれているかチェック
    // （modules に無いモジュールへ設定を書いても絶対に適用されないため、設定ミスとして拒否）。
    #[cfg(feature = "wasm")]
    if let Some(ref module_configuration) = route.module_configuration {
        let modules = route.modules.as_deref().unwrap_or(&[]);
        for key in module_configuration.keys() {
            if !modules.iter().any(|m| m == key) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "Route '{}' has module_configuration for module '{}' which is not listed in its modules",
                        route_name, key
                    ),
                ));
            }
        }
    }

    Ok(())
}

/// `route.modules` + `route.module_configuration` から `resolved_modules` を構築する
/// （F-148、設定ロード時のみ実行するコールドパス。合成結果は `Arc` で共有し、
/// `load_backend`（リクエストごとに呼ばれる、B-64）は clone のみで済ませる）。
///
/// - `wasm` feature 無効ビルドではモジュール定義（`WasmConfig`）自体が存在しないため、
///   `route.module_configuration` の上書きは適用できず、名前のみの `ModuleRef` を作る
///   （既存の「モジュール名だけ持ち回る」挙動と 1 バイトも変わらない）。
/// - `wasm` feature 有効ビルドでは `[[wasm.modules]]` の `configuration` を base、
///   `route.module_configuration[name]` を override として `PluginConfiguration::merge_over`
///   で合成し、`to_bytes()`（TOML→JSON エンコード、コールドパス専用）した結果を持つ。
pub(super) fn resolve_route_modules(
    route: &mut Route,
    #[cfg(feature = "wasm")] wasm_config: Option<&crate::wasm::WasmConfig>,
) {
    let Some(names) = route.modules.clone() else {
        route.resolved_modules = None;
        return;
    };

    #[cfg(feature = "wasm")]
    let resolved: Vec<ModuleRef> = names
        .iter()
        .map(|name| {
            let base = wasm_config
                .and_then(|w| w.modules.iter().find(|m| &m.name == name))
                .map(|m| &m.configuration);
            let over = route
                .module_configuration
                .as_ref()
                .and_then(|mc| mc.get(name));
            match PluginConfiguration::merge_over(base, over) {
                Some(cfg) => ModuleRef::new(name.clone(), Some(cfg.to_bytes())),
                None => ModuleRef::from_name(name),
            }
        })
        .collect();

    #[cfg(not(feature = "wasm"))]
    let resolved: Vec<ModuleRef> = names.iter().map(|n| ModuleRef::from_name(n)).collect();

    route.resolved_modules = Some(Arc::new(resolved));
}
