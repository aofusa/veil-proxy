//! # ALPN プロトコルネゴシエーション
//!
//! TLS ハンドシェイク時に ALPN (Application-Layer Protocol Negotiation) を使用して
//! HTTP/1.1 と HTTP/2 のプロトコルを選択します。
//!
//! ## サポートするプロトコル
//!
//! - `h2`: HTTP/2 over TLS (RFC 7540)
//! - `http/1.1`: HTTP/1.1 over TLS (フォールバック)

use rustls::{ClientConfig, ServerConfig};

/// サポートする HTTP プロトコル
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpProtocol {
    /// HTTP/1.1
    Http1_1,
    /// HTTP/2
    Http2,
}

impl std::fmt::Display for HttpProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpProtocol::Http1_1 => write!(f, "HTTP/1.1"),
            HttpProtocol::Http2 => write!(f, "HTTP/2"),
        }
    }
}

/// ALPN プロトコルリスト
/// HTTP/2 を優先し、HTTP/1.1 にフォールバック
pub const ALPN_H2_HTTP11: &[&[u8]] = &[
    b"h2",       // HTTP/2
    b"http/1.1", // HTTP/1.1 フォールバック
];

/// HTTP/2 のみの ALPN リスト
pub const ALPN_H2_ONLY: &[&[u8]] = &[b"h2"];

/// 上流（バックエンド）向け ALPN リスト: HTTP/1.1 のみ
///
/// veil は TLS 上の HTTP/2 上流（`https://` + h2）を実装していない
/// （`proxy_https_pooled` / `connect_https_backend_fresh` / `h2_proxy_https` /
/// `http3_stream::run_backend_task` はいずれも HTTP/1.1 を書き込む）。
/// そのため上流向けクライアントの ALPN に `h2` を含めてはならない
/// （含めると h2 対応の HTTPS バックエンド（nginx/Envoy の既定）が h2 を選択し、
/// veil は HTTP/1.1 のバイト列を送りつけて中継が壊れる。B-83）。
/// 平文の HTTP/2 上流は `use_h2c`（ALPN を経由しない h2c プール）で対応する。
pub const ALPN_HTTP11_ONLY: &[&[u8]] = &[b"http/1.1"];

/// rustls ServerConfig に HTTP/2 対応の ALPN を設定
///
/// # Arguments
///
/// * `config` - rustls ServerConfig ビルダー
/// * `http2_only` - true の場合 HTTP/2 のみ、false の場合 HTTP/1.1 フォールバックあり
///
/// # Returns
///
/// ALPN が設定された ServerConfig
pub fn configure_alpn_h2(mut config: ServerConfig, http2_only: bool) -> ServerConfig {
    let protocols = if http2_only {
        ALPN_H2_ONLY
    } else {
        ALPN_H2_HTTP11
    };

    config.alpn_protocols = protocols.iter().map(|p| p.to_vec()).collect();

    config
}

/// rustls ClientConfig に上流（バックエンド）向けの ALPN を設定
///
/// `http/1.1` のみを提示する。veil は TLS 上の HTTP/2 上流を実装しておらず、
/// `h2` を提示すると h2 対応の HTTPS バックエンド（nginx/Envoy の既定）が h2 を
/// 選択して中継が壊れる（B-83）。平文の HTTP/2 上流は `use_h2c` を使う
/// （ALPN を経由しない）。
pub fn configure_alpn_http11_client(mut config: ClientConfig) -> ClientConfig {
    config.alpn_protocols = ALPN_HTTP11_ONLY.iter().map(|p| p.to_vec()).collect();

    config
}

/// ネゴシエートされたプロトコルを取得
///
/// TLS ハンドシェイク完了後に呼び出し、選択されたプロトコルを返します。
///
/// # Arguments
///
/// * `conn` - rustls ServerConnection への参照
///
/// # Returns
///
/// ネゴシエートされた HttpProtocol（ALPN 未設定または不明な場合は HTTP/1.1）
#[inline]
pub fn get_negotiated_protocol(conn: &rustls::ServerConnection) -> HttpProtocol {
    match conn.alpn_protocol() {
        Some(proto) if proto == b"h2" => HttpProtocol::Http2,
        Some(proto) if proto == b"http/1.1" => HttpProtocol::Http1_1,
        Some(_) => HttpProtocol::Http1_1, // 未知のプロトコルは HTTP/1.1 扱い
        None => HttpProtocol::Http1_1,    // ALPN なしは HTTP/1.1
    }
}

/// クライアント接続用: ネゴシエートされたプロトコルを取得
#[inline]
pub fn get_negotiated_protocol_client(conn: &rustls::ClientConnection) -> HttpProtocol {
    match conn.alpn_protocol() {
        Some(proto) if proto == b"h2" => HttpProtocol::Http2,
        Some(proto) if proto == b"http/1.1" => HttpProtocol::Http1_1,
        Some(_) => HttpProtocol::Http1_1,
        None => HttpProtocol::Http1_1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protocol_display() {
        assert_eq!(format!("{}", HttpProtocol::Http1_1), "HTTP/1.1");
        assert_eq!(format!("{}", HttpProtocol::Http2), "HTTP/2");
    }

    #[test]
    fn test_alpn_lists() {
        assert_eq!(ALPN_H2_HTTP11.len(), 2);
        assert_eq!(ALPN_H2_HTTP11[0], b"h2");
        assert_eq!(ALPN_H2_HTTP11[1], b"http/1.1");

        assert_eq!(ALPN_H2_ONLY.len(), 1);
        assert_eq!(ALPN_H2_ONLY[0], b"h2");

        assert_eq!(ALPN_HTTP11_ONLY.len(), 1);
        assert_eq!(ALPN_HTTP11_ONLY[0], b"http/1.1");
    }

    /// CryptoProvider をプロセスに一度だけインストールする（テスト用）
    fn ensure_provider() {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let _ = crate::tls_provider::provider::default_provider().install_default();
        });
    }

    /// B-83: 上流向けクライアント設定は `h2` を一切提示しないこと
    #[test]
    fn test_configure_alpn_http11_client_excludes_h2() {
        // simple_tls::default_client_config は veil_ktls cfg で有無が変わるため、
        // ここでは ClientConfig を直接組み立てて alpn_protocols のみを検証する。
        ensure_provider();
        let root_store = rustls::RootCertStore::empty();
        let config = ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        let config = configure_alpn_http11_client(config);

        assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
        assert!(!config.alpn_protocols.contains(&b"h2".to_vec()));
    }
}
