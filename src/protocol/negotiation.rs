//! # ALPN プロトコルネゴシエーション
//!
//! TLS ハンドシェイク時に ALPN (Application-Layer Protocol Negotiation) を使用して
//! HTTP/1.1 と HTTP/2 のプロトコルを選択します。
//!
//! ## サポートするプロトコル
//!
//! - `h2`: HTTP/2 over TLS (RFC 7540)
//! - `http/1.1`: HTTP/1.1 over TLS (フォールバック)

use rustls::{ServerConfig, ClientConfig};

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
    b"h2",        // HTTP/2
    b"http/1.1",  // HTTP/1.1 フォールバック
];

/// HTTP/2 のみの ALPN リスト
pub const ALPN_H2_ONLY: &[&[u8]] = &[
    b"h2",
];

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
    
    config.alpn_protocols = protocols.iter()
        .map(|p| p.to_vec())
        .collect();
    
    config
}

/// rustls ClientConfig に HTTP/2 対応の ALPN を設定
pub fn configure_alpn_h2_client(mut config: ClientConfig, http2_only: bool) -> ClientConfig {
    let protocols = if http2_only {
        ALPN_H2_ONLY
    } else {
        ALPN_H2_HTTP11
    };
    
    config.alpn_protocols = protocols.iter()
        .map(|p| p.to_vec())
        .collect();
    
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
    }
}
