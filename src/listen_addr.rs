//! リッスンアドレス表現（F-164: Unix ドメインソケット（UDS）リスナー対応）。
//!
//! `[server].listen` / `[server].h2c_listen` は従来 `host:port` の TCP アドレスのみを
//! 受理していたが、`unix:<path>` 形式で AF_UNIX ソケットパスも指定できるようにする。
//! 対応プラットフォームは `cfg(unix)` のみ（Windows では設定エラーとして起動を拒否する）。
//!
//! 対象は `[server].listen` / `[server].h2c_listen` のみ（`[[l4]].listen` ・
//! 上流バックエンドへの UDS 接続・`[server].http` ・ `[http3].listen` は対象外）。

use std::fmt;
use std::net::SocketAddr;
#[cfg(unix)]
use std::path::PathBuf;

/// `unix:` 接頭辞。
const UNIX_PREFIX: &str = "unix:";

/// リッスンアドレス（TCP または、cfg(unix) では Unix ドメインソケット）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenAddr {
    /// TCP（`host:port`）。
    Tcp(SocketAddr),
    /// Unix ドメインソケット（`unix:<path>`）。`cfg(unix)` のみ。
    #[cfg(unix)]
    Unix(PathBuf),
}

impl ListenAddr {
    /// 文字列表記からパースする。
    ///
    /// - `unix:` 接頭辞: `cfg(unix)` なら Unix ドメインソケットとしてパースする
    ///   （接頭辞除去後が空文字ならエラー）。`cfg(unix)` でなければ、この環境では
    ///   UDS が非対応であることを明示するエラーを返す。
    /// - それ以外: `SocketAddr` としてパースする（ipv4/ipv6 の `host:port`）。
    pub fn parse(s: &str) -> Result<Self, String> {
        if let Some(path) = s.strip_prefix(UNIX_PREFIX) {
            #[cfg(unix)]
            {
                if path.is_empty() {
                    return Err(format!(
                        "invalid unix socket address (empty path after 'unix:'): {}",
                        s
                    ));
                }
                return Ok(ListenAddr::Unix(PathBuf::from(path)));
            }
            #[cfg(not(unix))]
            {
                let _ = path;
                return Err(format!(
                    "unix domain socket listeners are not supported on this platform: {}",
                    s
                ));
            }
        }

        s.parse::<SocketAddr>()
            .map(ListenAddr::Tcp)
            .map_err(|e| format!("invalid listen address '{}': {}", s, e))
    }

    /// Unix ドメインソケットかどうか。
    pub fn is_unix(&self) -> bool {
        match self {
            ListenAddr::Tcp(_) => false,
            #[cfg(unix)]
            ListenAddr::Unix(_) => true,
        }
    }

    /// TCP アドレスであれば `SocketAddr` を返す。
    pub fn tcp_addr(&self) -> Option<SocketAddr> {
        match self {
            ListenAddr::Tcp(addr) => Some(*addr),
            #[cfg(unix)]
            ListenAddr::Unix(_) => None,
        }
    }

    /// Unix ドメインソケットであればパスを返す。
    #[cfg(unix)]
    pub fn unix_path(&self) -> Option<&std::path::Path> {
        match self {
            ListenAddr::Tcp(_) => None,
            ListenAddr::Unix(path) => Some(path.as_path()),
        }
    }
}

impl fmt::Display for ListenAddr {
    /// 元の表記を保つ（ログ・メトリクスの見た目を壊さない）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ListenAddr::Tcp(addr) => write!(f, "{}", addr),
            #[cfg(unix)]
            ListenAddr::Unix(path) => write!(f, "unix:{}", path.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_tcp_ipv4() {
        let addr = ListenAddr::parse("0.0.0.0:8443").expect("parse");
        assert!(!addr.is_unix());
        assert_eq!(
            addr.tcp_addr(),
            Some("0.0.0.0:8443".parse::<SocketAddr>().unwrap())
        );
        assert_eq!(addr.to_string(), "0.0.0.0:8443");
    }

    #[test]
    fn test_parse_tcp_ipv6() {
        let addr = ListenAddr::parse("[::1]:8443").expect("parse");
        assert!(!addr.is_unix());
        assert_eq!(
            addr.tcp_addr(),
            Some("[::1]:8443".parse::<SocketAddr>().unwrap())
        );
        assert_eq!(addr.to_string(), "[::1]:8443");
    }

    #[test]
    fn test_parse_invalid() {
        assert!(ListenAddr::parse("not-an-address").is_err());
        assert!(ListenAddr::parse("").is_err());
        assert!(ListenAddr::parse("256.256.256.256:80").is_err());
        assert!(ListenAddr::parse("0.0.0.0").is_err()); // ポート無し
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_unix_valid() {
        let addr = ListenAddr::parse("unix:/run/veil/https.sock").expect("parse");
        assert!(addr.is_unix());
        assert_eq!(addr.tcp_addr(), None);
        assert_eq!(
            addr.unix_path(),
            Some(std::path::Path::new("/run/veil/https.sock"))
        );
        assert_eq!(addr.to_string(), "unix:/run/veil/https.sock");
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_unix_relative_path() {
        let addr = ListenAddr::parse("unix:relative/path.sock").expect("parse");
        assert!(addr.is_unix());
        assert_eq!(
            addr.unix_path(),
            Some(std::path::Path::new("relative/path.sock"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_unix_empty_path() {
        let err = ListenAddr::parse("unix:").expect_err("empty path must error");
        assert!(err.contains("empty path"));
    }

    #[cfg(not(unix))]
    #[test]
    fn test_parse_unix_unsupported_on_non_unix() {
        let err = ListenAddr::parse("unix:/run/veil/https.sock").expect_err("must error");
        assert!(err.contains("not supported"));
    }
}
