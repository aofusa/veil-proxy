//! サーバの TLS 設定の構築（暗号スイート・クライアント認証・セッション再開）
//!
//! F-233 で `src/config.rs` から移した（中身は変えていない）。

use super::*;

// rustls 用の TLS 設定読み込み（統一）
/// 証明書・秘密鍵パスから ServerConfig を構築する（F-03 リローダー用の公開 API）
///
/// `load_tls_config` と同じ手順（ALPN / kTLS シークレット抽出）で再構築する。
pub fn build_server_config_from_paths(
    cert_path: &Path,
    key_path: &Path,
    ktls_enabled: bool,
    http2_enabled: bool,
    cipher_suites: &[String],
) -> anyhow::Result<Arc<ServerConfig>> {
    let section = TlsConfigSection {
        cert_path: cert_path.to_string_lossy().into_owned(),
        key_path: key_path.to_string_lossy().into_owned(),
        ktls_enabled,
        ktls_fallback_enabled: true,
        tcp_cork_enabled: true,
        cipher_suites: cipher_suites.to_vec(),
        auto_reload: false,
        reload_interval_secs: default_tls_reload_interval(),
        // 起動時の設定が `TLS_RESUMPTION` に記録済みなら、そちらが使われる。
        session_resumption: SessionResumption::default(),
        session_cache_size: default_session_cache_size(),
        tls13_tickets: default_tls13_tickets(),
        // 起動時の設定が `TLS_CLIENT_AUTH` に記録済みなら、そちらが使われる。
        client_auth: ClientAuthMode::None,
        client_ca: String::new(),
        client_allowed_sans: Vec::new(),
    };
    load_tls_config(&section, ktls_enabled, http2_enabled)
        .map_err(|e| anyhow::anyhow!("TLS reload build failed: {}", e))
}

/// 設定名（例: `TLS13_AES_256_GCM_SHA384`）から rustls の暗号スイートを解決する（F-50）。
///
/// 配列の順序を保持する（記載順 = サーバ優先度順）。不明な名前・重複はエラー。
/// 設定読み込み時（非ホットパス）にのみ呼ばれるため、ここでのアロケーションは許容される。
pub fn resolve_cipher_suites(names: &[String]) -> io::Result<Vec<rustls::SupportedCipherSuite>> {
    use crate::tls_provider::provider::ALL_CIPHER_SUITES;

    let mut out: Vec<rustls::SupportedCipherSuite> = Vec::with_capacity(names.len());
    for name in names {
        let found = ALL_CIPHER_SUITES
            .iter()
            .find(|s| format!("{:?}", s.suite()).eq_ignore_ascii_case(name));
        match found {
            Some(s) => {
                if out.iter().any(|e| e.suite() == s.suite()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("[tls] cipher_suites: duplicate cipher suite '{}'", name),
                    ));
                }
                out.push(*s);
            }
            None => {
                let valid: Vec<String> = ALL_CIPHER_SUITES
                    .iter()
                    .map(|s| format!("{:?}", s.suite()))
                    .collect();
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "[tls] cipher_suites: unknown cipher suite '{}'. Valid names: {}",
                        name,
                        valid.join(", ")
                    ),
                ));
            }
        }
    }
    Ok(out)
}

pub(super) fn load_tls_config(
    tls_config: &TlsConfigSection,
    ktls_enabled: bool,
    #[allow(unused_variables)] http2_enabled: bool,
) -> io::Result<Arc<ServerConfig>> {
    // 平文専用サーバ（`[tls]` 無し）: TLS リスナーは起動しないが、アクセプタの型を満たすために
    // 証明書を持たない ServerConfig を返す（万一ハンドシェイクが来ても証明書が無く失敗する）。
    if !tls_config.is_configured() {
        return Ok(no_certificate_server_config());
    }
    // F-136: cert/key 読み取りは単一チョークポイント `tls_reload::read_pem` を経由する。
    // 通常（Linux/macOS/Windows/OpenBSD）は `std::fs::read` と完全に等価。FreeBSD
    // capability mode 下で `security::capsicum::init_tls_cert_dirfds` が該当パスを
    // 登録済みの場合のみ dirfd 相対の `openat`（`O_RESOLVE_BENEATH`）へ切り替わる。
    // これにより H1/H2（rustls）は「PEM をバイト列として読み、ライブラリにパスを渡さない」
    // という既存の設計のまま、capability mode 下でも証明書リロードが継続動作する。
    let cert_pem = crate::tls_reload::read_pem(Path::new(&tls_config.cert_path))?;
    let key_pem = crate::tls_reload::read_pem(Path::new(&tls_config.key_path))?;

    let cert_reader = BufReader::new(cert_pem.as_slice());
    let cert_chain: Vec<CertificateDer<'static>> = CertificateDer::pem_reader_iter(cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Certificate parse error: {}", e),
            )
        })?;

    let key_reader = BufReader::new(key_pem.as_slice());
    let keys: PrivateKeyDer<'static> = PrivateKeyDer::from_pem_reader(key_reader).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Private key parse error: {}", e),
        )
    })?;

    // F-50: [tls] cipher_suites による暗号スイートの取捨選択・優先度指定
    //
    // 指定がある場合は CryptoProvider の cipher_suites を設定順（= サーバ優先度順）で
    // 差し替える。不正な名前はここでエラーになる（起動時 / リロード時の検証）。
    let custom_suites = if !tls_config.cipher_suites.is_empty() {
        let suites = resolve_cipher_suites(&tls_config.cipher_suites)?;
        info!(
            "TLS cipher suites restricted by config (priority order): {:?}",
            tls_config.cipher_suites
        );
        Some(suites)
    } else {
        None
    };

    // kTLS 互換性チェック: kTLS は AES-GCM 系のみオフロード可能。
    // 非互換スイートが指定されている場合は警告する（該当接続は rustls フォールバック、
    // ktls_fallback_enabled = false ならハンドシェイク後に拒否される）。
    #[cfg(veil_ktls)]
    if ktls_enabled {
        if let Some(ref suites) = custom_suites {
            let compat = crate::ktls::ktls_compatible_cipher_suites();
            let incompatible: Vec<String> = suites
                .iter()
                .filter(|s| !compat.contains(s))
                .map(|s| format!("{:?}", s.suite()))
                .collect();
            if !incompatible.is_empty() {
                warn!(
                    "kTLS enabled but non-kTLS-compatible cipher suites configured: {}. \
                     Connections negotiating these will not be offloaded to kTLS",
                    incompatible.join(", ")
                );
            }
        } else {
            info!("kTLS enabled: kTLS offload is available for AES-GCM cipher suites");
        }
    }

    // kTLS 有効時のみ config を変更するため、条件付きで mut を使用
    #[allow(unused_mut)]
    let mut config = {
        #[cfg(not(veil_ktls))]
        let _ = ktls_enabled;

        let mut provider = crate::tls_provider::provider::default_provider();
        if let Some(suites) = custom_suites {
            provider.cipher_suites = suites;
        }

        let provider: Arc<rustls::crypto::CryptoProvider> = provider.into();
        let builder = ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(rustls::DEFAULT_VERSIONS)
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("TLS version error: {}", e),
                )
            })?;
        // F-217: クライアント証明書（mTLS の受け側）
        match client_cert_verifier(tls_config, provider)? {
            Some(v) => builder.with_client_cert_verifier(v),
            None => builder.with_no_client_auth(),
        }
        .with_single_cert(cert_chain, keys)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
    };

    // kTLS が有効な場合のみシークレット抽出を有効化
    // これにより dangerous_extract_secrets() が使用可能になる
    #[cfg(veil_ktls)]
    if ktls_enabled {
        config.enable_secret_extraction = true;
        info!("TLS secret extraction enabled for kTLS support");
    }

    apply_session_resumption(&mut config, tls_config)?;

    // HTTP/2 有効時は ALPN を設定
    #[cfg(feature = "http2")]
    if http2_enabled {
        config = protocol::configure_alpn_h2(config, false);
        info!("HTTP/2 enabled via ALPN negotiation (h2, http/1.1)");
    }

    Ok(Arc::new(config))
}

/// 起動時の `[tls] client_auth`・`client_ca`（証明書のリロードでも同じものを使う。F-217）。
pub(super) static TLS_CLIENT_AUTH: once_cell::sync::OnceCell<(
    ClientAuthMode,
    String,
    Vec<String>,
)> = once_cell::sync::OnceCell::new();

/// クライアント証明書の検証器（`client_auth = "none"` なら `None`）。
pub(super) fn client_cert_verifier(
    tls_config: &TlsConfigSection,
    provider: Arc<rustls::crypto::CryptoProvider>,
) -> io::Result<Option<Arc<dyn rustls::server::danger::ClientCertVerifier>>> {
    let (mode, ca, allowed) = TLS_CLIENT_AUTH
        .get_or_init(|| {
            (
                tls_config.client_auth,
                tls_config.client_ca.clone(),
                tls_config.client_allowed_sans.clone(),
            )
        })
        .clone();
    if mode == ClientAuthMode::None {
        return Ok(None);
    }
    if ca.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "[tls] client_auth requires client_ca",
        ));
    }
    let pem = crate::tls_reload::read_pem(Path::new(&ca))?;
    let mut roots = rustls::RootCertStore::empty();
    for c in CertificateDer::pem_reader_iter(BufReader::new(pem.as_slice())) {
        let c = c.map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("[tls] client_ca: {}", e),
            )
        })?;
        roots.add(c).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("[tls] client_ca: {}", e),
            )
        })?;
    }
    if roots.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "[tls] client_ca: no certificates",
        ));
    }
    let b = rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider);
    let b = if mode == ClientAuthMode::Optional {
        b.allow_unauthenticated()
    } else {
        b
    };
    let v = b.build().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("[tls] client_ca: {}", e),
        )
    })?;
    info!(
        "TLS client certificates: {:?} (CA {}, allowed SANs {:?})",
        mode, ca, allowed
    );
    if allowed.is_empty() {
        return Ok(Some(v));
    }
    Ok(Some(Arc::new(ClientSanAllowList { inner: v, allowed })))
}

/// クライアント証明書の SAN を許可リストで照合する検証器（`[tls] client_allowed_sans`、F-220）。
/// チェーンの検証は `WebPkiClientVerifier` に任せ、通ったものの SAN だけを見る（ハンドシェイクで 1 回）。
#[derive(Debug)]
pub(super) struct ClientSanAllowList {
    inner: Arc<dyn rustls::server::danger::ClientCertVerifier>,
    allowed: Vec<String>,
}

impl rustls::server::danger::ClientCertVerifier for ClientSanAllowList {
    fn offer_client_auth(&self) -> bool {
        self.inner.offer_client_auth()
    }

    fn client_auth_mandatory(&self) -> bool {
        self.inner.client_auth_mandatory()
    }

    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        self.inner.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        let v = self
            .inner
            .verify_client_cert(end_entity, intermediates, now)?;
        use crate::x509_san::San;
        let ok = crate::x509_san::parse_sans(end_entity).iter().any(|s| {
            self.allowed.iter().any(|a| match s {
                San::Dns(d) => d.eq_ignore_ascii_case(a),
                San::Uri(u) => u == a,
                San::Ip(ip) => ip.to_string() == *a,
            })
        });
        if ok {
            Ok(v)
        } else {
            debug!("TLS client certificate rejected: SAN not in client_allowed_sans");
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// 証明書を返さない rustls のサーバ証明書リゾルバ（平文専用サーバ用）。
#[derive(Debug)]
pub(super) struct NoServerCertificate;

impl rustls::server::ResolvesServerCert for NoServerCertificate {
    fn resolve(
        &self,
        _client_hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        None
    }
}

/// 証明書を持たない ServerConfig（平文専用サーバ用。TLS リスナーは起動しない）。
pub(super) fn no_certificate_server_config() -> Arc<ServerConfig> {
    Arc::new(
        ServerConfig::builder_with_provider(
            crate::tls_provider::provider::default_provider().into(),
        )
        .with_protocol_versions(rustls::DEFAULT_VERSIONS)
        .expect("default TLS versions are always valid")
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(NoServerCertificate)),
    )
}

/// プロセス共有のセッションチケット発行器（F-183）。証明書のリロードで `ServerConfig` を
/// 作り直しても同じ鍵（とローテーション状態）を使い続け、発行済みチケットを無効にしない。
pub(super) static TLS_TICKETER: once_cell::sync::OnceCell<
    Arc<dyn rustls::server::ProducesTickets>,
> = once_cell::sync::OnceCell::new();
/// 起動時の `[tls]` のセッション再開設定（方式, キャッシュ件数, TLS 1.3 チケット数）。
pub(super) static TLS_RESUMPTION: once_cell::sync::OnceCell<(SessionResumption, usize, usize)> =
    once_cell::sync::OnceCell::new();
/// プロセス共有のサーバ側セッションキャッシュ（`session_resumption = "cache"`、F-183）。
pub(super) static TLS_SESSION_CACHE: once_cell::sync::OnceCell<
    Arc<dyn rustls::server::StoresServerSessions>,
> = once_cell::sync::OnceCell::new();

pub(super) fn session_cache(size: usize) -> &'static Arc<dyn rustls::server::StoresServerSessions> {
    TLS_SESSION_CACHE.get_or_init(|| {
        rustls::server::ServerSessionMemoryCache::new(size.max(1))
            as Arc<dyn rustls::server::StoresServerSessions>
    })
}

/// `[tls] session_resumption` を `ServerConfig` へ反映する（F-183）。
///
/// rustls の既定（全ワーカー共有の Mutex 付きキャッシュ 256 件 + TLS 1.3 チケット 2 枚）は、
/// フルハンドシェイクのたびに共有ロック下で 2 件挿入し、再開しないクライアントでも
/// キャッシュを掃き出し続ける。既定はステートレスチケット 1 枚にする。
pub(super) fn apply_session_resumption(
    config: &mut ServerConfig,
    tls: &TlsConfigSection,
) -> io::Result<()> {
    // 最初の構築（起動時の `[tls]`）の値を記録し、証明書リロード（合成した `[tls]` で
    // 呼ばれる）でも同じ方式を使う。変更の反映には再起動が要る。
    let &(mode, cache_size, tickets) = TLS_RESUMPTION.get_or_init(|| {
        (
            tls.session_resumption,
            tls.session_cache_size,
            tls.tls13_tickets,
        )
    });
    apply_session_resumption_mode(config, mode, cache_size, tickets)
}

pub(super) fn apply_session_resumption_mode(
    config: &mut ServerConfig,
    mode: SessionResumption,
    cache_size: usize,
    tickets: usize,
) -> io::Result<()> {
    match mode {
        SessionResumption::Tickets => {
            let ticketer = TLS_TICKETER
                .get_or_try_init(crate::tls_provider::provider::Ticketer::new)
                .map_err(|e| io::Error::other(format!("TLS ticketer: {e}")))?;
            config.ticketer = Arc::clone(ticketer);
            // TLS 1.3 と、チケットを使う TLS 1.2 クライアントはステートレスに再開する
            // （キャッシュに触れない）。チケット非対応で session ID を使う TLS 1.2 クライアント
            // （LibreSSL の wrk 等）向けにキャッシュも持つ。
            config.session_storage = Arc::clone(session_cache(cache_size));
            config.send_tls13_tickets = tickets;
        }
        SessionResumption::Cache => {
            config.session_storage = Arc::clone(session_cache(cache_size));
            config.send_tls13_tickets = tickets;
        }
        SessionResumption::Off => {
            config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
            config.send_tls13_tickets = 0;
        }
    }
    Ok(())
}
