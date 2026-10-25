//! 上流の TLS コネクタ（スレッドローカル）とアクセサ
//!
//! F-233 で `src/config.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// TLSコネクタ（スレッドローカル）
// ====================

// rustls 用の TLS コネクター（kTLS 有効時は ktls_rustls を使用）
// kTLSフィーチャー有効時はシークレット抽出を有効化し、kTLS利用可能な状態にする
#[cfg(all(veil_ktls, feature = "http2"))]
thread_local! {
    static TLS_CONNECTOR: RustlsConnector = {
        // 設定ファイルから kTLS 有効化、ktls_fallback_enabled, tcp_cork_enabled を読み込み
        let config_guard = CURRENT_CONFIG.load();
        let ktls_enabled = config_guard.ktls_config.enabled;
        let fallback_enabled = config_guard.ktls_config.fallback_enabled;
        let tcp_cork_enabled = config_guard.ktls_config.tcp_cork_enabled;

        // kTLS が有効な場合のみシークレット抽出を有効化した設定を使用
        let config = (*crate::ktls_rustls::client_config(ktls_enabled)).clone();
        let config = crate::protocol::configure_alpn_http11_client(config);

        RustlsConnector::new(Arc::new(config))
            .with_ktls(ktls_enabled)        // 設定に基づいて kTLS を有効化
            .with_fallback(fallback_enabled)    // kTLS 失敗時のフォールバック設定
            .with_tcp_cork(tcp_cork_enabled)    // TCP_CORK 設定
    };
}

// kTLS あり・HTTP/2 なしの場合のコネクター
#[cfg(all(veil_ktls, not(feature = "http2")))]
thread_local! {
    static TLS_CONNECTOR: RustlsConnector = {
        let config_guard = CURRENT_CONFIG.load();
        let ktls_enabled = config_guard.ktls_config.enabled;
        let fallback_enabled = config_guard.ktls_config.fallback_enabled;
        let tcp_cork_enabled = config_guard.ktls_config.tcp_cork_enabled;

        let config = (*crate::ktls_rustls::client_config(ktls_enabled)).clone();

        RustlsConnector::new(Arc::new(config))
            .with_ktls(ktls_enabled)
            .with_fallback(fallback_enabled)
            .with_tcp_cork(tcp_cork_enabled)
    };
}

// 証明書検証をスキップする TLS コネクター（kTLS 有効時・自己署名証明書用）
#[cfg(all(veil_ktls, feature = "http2"))]
thread_local! {
    static TLS_CONNECTOR_INSECURE: RustlsConnector = {
        // 証明書検証をスキップするクライアント設定
        // 設定ファイルから kTLS 有効化、ktls_fallback_enabled, tcp_cork_enabled を読み込み
        let config_guard = CURRENT_CONFIG.load();
        let ktls_enabled = config_guard.ktls_config.enabled;
        let fallback_enabled = config_guard.ktls_config.fallback_enabled;
        let tcp_cork_enabled = config_guard.ktls_config.tcp_cork_enabled;

        let config = (*crate::ktls_rustls::insecure_client_config()).clone();
        let config = crate::protocol::configure_alpn_http11_client(config);

        RustlsConnector::new(Arc::new(config))
            .with_ktls(ktls_enabled)
            .with_fallback(fallback_enabled)
            .with_tcp_cork(tcp_cork_enabled)
    };
}

// kTLS あり・HTTP/2 なしの場合の insecure コネクター
#[cfg(all(veil_ktls, not(feature = "http2")))]
thread_local! {
    static TLS_CONNECTOR_INSECURE: RustlsConnector = {
        let config_guard = CURRENT_CONFIG.load();
        let ktls_enabled = config_guard.ktls_config.enabled;
        let fallback_enabled = config_guard.ktls_config.fallback_enabled;
        let tcp_cork_enabled = config_guard.ktls_config.tcp_cork_enabled;

        let config = (*crate::ktls_rustls::insecure_client_config()).clone();

        RustlsConnector::new(Arc::new(config))
            .with_ktls(ktls_enabled)
            .with_fallback(fallback_enabled)
            .with_tcp_cork(tcp_cork_enabled)
    };
}

// rustls 用の TLS コネクター（kTLS 無効時は simple_tls を使用）
#[cfg(all(not(veil_ktls), feature = "http2"))]
thread_local! {
    static TLS_CONNECTOR: simple_tls::SimpleTlsConnector = {
        let config = (*simple_tls::default_client_config()).clone();
        let config = protocol::configure_alpn_http11_client(config);
        simple_tls::SimpleTlsConnector::new(Arc::new(config))
    };
}

// HTTP/2 なし・kTLS なしの場合のコネクター
#[cfg(all(not(veil_ktls), not(feature = "http2")))]
thread_local! {
    static TLS_CONNECTOR: simple_tls::SimpleTlsConnector = {
        let config = (*simple_tls::default_client_config()).clone();
        simple_tls::SimpleTlsConnector::new(Arc::new(config))
    };
}

// 証明書検証をスキップする TLS コネクター（自己署名証明書用）
#[cfg(all(not(veil_ktls), feature = "http2"))]
thread_local! {
    static TLS_CONNECTOR_INSECURE: simple_tls::SimpleTlsConnector = {
        let config = (*simple_tls::insecure_client_config()).clone();
        let config = protocol::configure_alpn_http11_client(config);
        simple_tls::SimpleTlsConnector::new(Arc::new(config))
    };
}

// HTTP/2 なし・kTLS なしの場合の insecure コネクター
#[cfg(all(not(veil_ktls), not(feature = "http2")))]
thread_local! {
    static TLS_CONNECTOR_INSECURE: simple_tls::SimpleTlsConnector = {
        let config = (*simple_tls::insecure_client_config()).clone();
        simple_tls::SimpleTlsConnector::new(Arc::new(config))
    };
}

// F-175: 上流 HTTPS の HTTP/2（ALPN `h2, http/1.1`）用コネクター。証明書検証・kTLS の設定は
// 上の HTTP/1.1 用と同じで、ALPN だけが違う（ワーカースレッドごとに 1 回だけ作る）。
#[cfg(all(veil_ktls, feature = "http2"))]
thread_local! {
    static TLS_CONNECTOR_H2: RustlsConnector = {
        let config_guard = CURRENT_CONFIG.load();
        let ktls = &config_guard.ktls_config;
        let config = (*crate::ktls_rustls::client_config(ktls.enabled)).clone();
        let config = crate::protocol::configure_alpn_h2_client(config);
        RustlsConnector::new(Arc::new(config))
            .with_ktls(ktls.enabled)
            .with_fallback(ktls.fallback_enabled)
            .with_tcp_cork(ktls.tcp_cork_enabled)
    };
    static TLS_CONNECTOR_H2_INSECURE: RustlsConnector = {
        let config_guard = CURRENT_CONFIG.load();
        let ktls = &config_guard.ktls_config;
        let config = (*crate::ktls_rustls::insecure_client_config()).clone();
        let config = crate::protocol::configure_alpn_h2_client(config);
        RustlsConnector::new(Arc::new(config))
            .with_ktls(ktls.enabled)
            .with_fallback(ktls.fallback_enabled)
            .with_tcp_cork(ktls.tcp_cork_enabled)
    };
}

#[cfg(all(not(veil_ktls), feature = "http2"))]
thread_local! {
    static TLS_CONNECTOR_H2: simple_tls::SimpleTlsConnector = {
        let config = (*simple_tls::default_client_config()).clone();
        let config = protocol::configure_alpn_h2_client(config);
        simple_tls::SimpleTlsConnector::new(Arc::new(config))
    };
    static TLS_CONNECTOR_H2_INSECURE: simple_tls::SimpleTlsConnector = {
        let config = (*simple_tls::insecure_client_config()).clone();
        let config = protocol::configure_alpn_h2_client(config);
        simple_tls::SimpleTlsConnector::new(Arc::new(config))
    };
}

// ====================
// TLS コネクタアクセサ関数
// ====================

/// TLS コネクタを取得（通常接続用）
#[cfg(veil_ktls)]
pub fn get_tls_connector() -> RustlsConnector {
    TLS_CONNECTOR.with(|c| c.clone())
}

/// TLS コネクタを取得（証明書検証スキップ用）
#[cfg(veil_ktls)]
pub fn get_tls_connector_insecure() -> RustlsConnector {
    TLS_CONNECTOR_INSECURE.with(|c| c.clone())
}

/// F-175: ALPN `h2, http/1.1` の TLS コネクタを取得する。
#[cfg(all(veil_ktls, feature = "http2"))]
pub fn get_tls_connector_h2(insecure: bool) -> RustlsConnector {
    if insecure {
        TLS_CONNECTOR_H2_INSECURE.with(|c| c.clone())
    } else {
        TLS_CONNECTOR_H2.with(|c| c.clone())
    }
}

/// F-175: ALPN `h2, http/1.1` の TLS コネクタを取得する。
#[cfg(all(not(veil_ktls), feature = "http2"))]
pub fn get_tls_connector_h2(insecure: bool) -> crate::simple_tls::SimpleTlsConnector {
    if insecure {
        TLS_CONNECTOR_H2_INSECURE.with(|c| c.clone())
    } else {
        TLS_CONNECTOR_H2.with(|c| c.clone())
    }
}

/// TLS コネクタを取得（通常接続用）
#[cfg(not(veil_ktls))]
pub fn get_tls_connector() -> crate::simple_tls::SimpleTlsConnector {
    TLS_CONNECTOR.with(|c| c.clone())
}

/// TLS コネクタを取得（証明書検証スキップ用）
#[cfg(not(veil_ktls))]
pub fn get_tls_connector_insecure() -> crate::simple_tls::SimpleTlsConnector {
    TLS_CONNECTOR_INSECURE.with(|c| c.clone())
}

// F-259: `[tls_client] ca` がリロードで変わったあと（世代が 1 以上）の既定のコネクタ。
// 世代が 0 の間は上のスレッドローカルをそのまま使う（ホットパスは世代の読み出し 1 回だけ増える）。
// 変わった後はスレッドごとに世代が合わなくなった時点で作り直す。
#[cfg(veil_ktls)]
type VerifyConnector = RustlsConnector;
#[cfg(not(veil_ktls))]
type VerifyConnector = simple_tls::SimpleTlsConnector;

fn build_verify_connector(h2: bool) -> VerifyConnector {
    #[cfg(veil_ktls)]
    {
        let config_guard = CURRENT_CONFIG.load();
        let k = &config_guard.ktls_config;
        let config = (*crate::ktls_rustls::client_config(k.enabled)).clone();
        let config = alpn_for(config, h2);
        RustlsConnector::new(Arc::new(config))
            .with_ktls(k.enabled)
            .with_fallback(k.fallback_enabled)
            .with_tcp_cork(k.tcp_cork_enabled)
    }
    #[cfg(not(veil_ktls))]
    {
        let config = (*simple_tls::default_client_config()).clone();
        simple_tls::SimpleTlsConnector::new(Arc::new(alpn_for(config, h2)))
    }
}

#[cfg(feature = "http2")]
fn alpn_for(config: rustls::ClientConfig, h2: bool) -> rustls::ClientConfig {
    if h2 {
        protocol::configure_alpn_h2_client(config)
    } else {
        protocol::configure_alpn_http11_client(config)
    }
}

#[cfg(not(feature = "http2"))]
fn alpn_for(config: rustls::ClientConfig, _h2: bool) -> rustls::ClientConfig {
    config
}

thread_local! {
    static VERIFY_REBUILT: std::cell::RefCell<[Option<(u32, VerifyConnector)>; 2]> =
        const { std::cell::RefCell::new([None, None]) };
}

/// 既定（`TlsProfile::Verify`）のコネクタ。`h2` は ALPN `h2, http/1.1`。
fn verify_connector(h2: bool) -> VerifyConnector {
    let gen = crate::trust_store::generation();
    if gen == 0 {
        #[cfg(feature = "http2")]
        if h2 {
            return get_tls_connector_h2(false);
        }
        return get_tls_connector();
    }
    VERIFY_REBUILT.with(|c| {
        let mut c = c.borrow_mut();
        let slot = &mut c[usize::from(h2)];
        match slot {
            Some((g, conn)) if *g == gen => conn.clone(),
            _ => {
                let conn = build_verify_connector(h2);
                *slot = Some((gen, conn.clone()));
                conn
            }
        }
    })
}

/// 上流の TLS の種類に合ったコネクタ（HTTP/1.1。F-217）。
#[cfg(veil_ktls)]
pub fn tls_connector(p: crate::upstream_tls::TlsProfile) -> RustlsConnector {
    use crate::upstream_tls::TlsProfile;
    match p {
        TlsProfile::Verify => verify_connector(false),
        TlsProfile::Insecure => get_tls_connector_insecure(),
        TlsProfile::Custom(c) => {
            let k = &CURRENT_CONFIG.load().ktls_config;
            RustlsConnector::new(c.h1.clone())
                .with_ktls(k.enabled)
                .with_fallback(k.fallback_enabled)
                .with_tcp_cork(k.tcp_cork_enabled)
        }
    }
}

/// 上流の TLS の種類に合ったコネクタ（HTTP/1.1。F-217）。
#[cfg(not(veil_ktls))]
pub fn tls_connector(p: crate::upstream_tls::TlsProfile) -> crate::simple_tls::SimpleTlsConnector {
    use crate::upstream_tls::TlsProfile;
    match p {
        TlsProfile::Verify => verify_connector(false),
        TlsProfile::Insecure => get_tls_connector_insecure(),
        TlsProfile::Custom(c) => crate::simple_tls::SimpleTlsConnector::new(c.h1.clone()),
    }
}

/// 上流の TLS の種類に合った ALPN `h2, http/1.1` のコネクタ（F-175・F-217）。
#[cfg(all(veil_ktls, feature = "http2"))]
pub fn tls_connector_h2(p: crate::upstream_tls::TlsProfile) -> RustlsConnector {
    use crate::upstream_tls::TlsProfile;
    match p {
        TlsProfile::Verify => verify_connector(true),
        TlsProfile::Insecure => get_tls_connector_h2(true),
        TlsProfile::Custom(c) => {
            let k = &CURRENT_CONFIG.load().ktls_config;
            RustlsConnector::new(c.h2.clone())
                .with_ktls(k.enabled)
                .with_fallback(k.fallback_enabled)
                .with_tcp_cork(k.tcp_cork_enabled)
        }
    }
}

/// 上流の TLS の種類に合った ALPN `h2, http/1.1` のコネクタ（F-175・F-217）。
#[cfg(all(not(veil_ktls), feature = "http2"))]
pub fn tls_connector_h2(
    p: crate::upstream_tls::TlsProfile,
) -> crate::simple_tls::SimpleTlsConnector {
    use crate::upstream_tls::TlsProfile;
    match p {
        TlsProfile::Verify => verify_connector(true),
        TlsProfile::Insecure => get_tls_connector_h2(true),
        TlsProfile::Custom(c) => crate::simple_tls::SimpleTlsConnector::new(c.h2.clone()),
    }
}
