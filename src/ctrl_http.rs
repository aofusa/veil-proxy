//! 制御プレーン用の小さなブロッキング HTTP/1.1 クライアント（VDS・ACME が専用スレッドで使う）。
//!
//! データプレーン（ワーカーのイベントループ）からは呼ばない。名前解決・接続・読み書きはすべて
//! ブロッキングで、呼び出し元の専用スレッドだけを止める（理由付きで clippy の禁止メソッドを許可）。
//! https は rustls（webpki-roots + 任意の追加 CA、任意の mTLS クライアント証明書）。

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

/// 解析済みの URL。
#[derive(Debug, Clone, PartialEq)]
pub struct Url {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// パス + クエリ（`/` で始まる）。
    pub path: String,
}

/// `http(s)://host[:port][/path]` を解析する。
pub fn parse_url(url: &str) -> Result<Url, String> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(format!("URL must start with http:// or https:// ({})", url));
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        let end = stripped.find(']').ok_or("invalid IPv6 host")?;
        (
            stripped[..end].to_string(),
            stripped[end + 1..]
                .strip_prefix(':')
                .map(|p| p.parse::<u16>()),
        )
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p.parse::<u16>())),
            None => (authority.to_string(), None),
        }
    };
    let port = match port {
        Some(Ok(p)) => p,
        Some(Err(_)) => return Err(format!("invalid port in URL ({})", url)),
        None if tls => 443,
        None => 80,
    };
    if host.is_empty() {
        return Err(format!("missing host in URL ({})", url));
    }
    Ok(Url {
        tls,
        host,
        port,
        path,
    })
}

/// TLS の設定（追加 CA・mTLS）。
#[derive(Debug, Clone, Default)]
pub struct TlsOptions {
    pub ca_cert: Option<String>,
    pub client_cert: Option<String>,
    pub client_key: Option<String>,
}

// 理由付き allow: 制御プレーン用の専用スレッドでのみ実行するファイル読み込み（データプレーン外）。
#[allow(clippy::disallowed_methods)]
pub fn read_file(path: &str) -> std::io::Result<Vec<u8>> {
    std::fs::read(path)
}

/// rustls のクライアント設定を作る。
pub fn client_config(opts: &TlsOptions) -> Result<Arc<rustls::ClientConfig>, String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(ca) = opts.ca_cert.as_deref().filter(|s| !s.is_empty()) {
        let pem = read_file(ca).map_err(|e| format!("CA certificate {}: {}", ca, e))?;
        for c in CertificateDer::pem_slice_iter(&pem) {
            let c = c.map_err(|e| format!("CA certificate {}: {}", ca, e))?;
            roots
                .add(c)
                .map_err(|e| format!("CA certificate {}: {}", ca, e))?;
        }
    }
    let provider = Arc::new(crate::tls_provider::provider::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(rustls::DEFAULT_VERSIONS)
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots);
    let cfg = match (
        opts.client_cert.as_deref().filter(|s| !s.is_empty()),
        opts.client_key.as_deref().filter(|s| !s.is_empty()),
    ) {
        (Some(c), Some(k)) => {
            let cert_pem = read_file(c).map_err(|e| format!("client certificate {}: {}", c, e))?;
            let key_pem = read_file(k).map_err(|e| format!("client key {}: {}", k, e))?;
            let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_pem)
                .collect::<Result<_, _>>()
                .map_err(|e| format!("client certificate {}: {}", c, e))?;
            let key = PrivateKeyDer::from_pem_slice(&key_pem)
                .map_err(|e| format!("client key {}: {}", k, e))?;
            builder
                .with_client_auth_cert(certs, key)
                .map_err(|e| format!("client certificate: {}", e))?
        }
        _ => builder.with_no_client_auth(),
    };
    Ok(Arc::new(cfg))
}

/// 応答。
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

trait Stream: Read + Write {}
impl<T: Read + Write> Stream for T {}

/// ブロッキング HTTP/1.1 クライアント（1 リクエスト 1 接続、`Connection: close`）。
pub struct HttpClient {
    tls: Arc<rustls::ClientConfig>,
    /// 読み取りのタイムアウト（長輪詢の保留時間より長くする）。
    pub io_timeout: Duration,
    pub user_agent: String,
}

impl HttpClient {
    pub fn new(tls: &TlsOptions, io_timeout: Duration) -> Result<HttpClient, String> {
        Ok(HttpClient {
            tls: client_config(tls)?,
            io_timeout,
            user_agent: format!("veil/{}", env!("CARGO_PKG_VERSION")),
        })
    }

    // 理由付き allow: 制御プレーン用の専用スレッドでのみ実行するブロッキングな名前解決・接続。
    #[allow(clippy::disallowed_methods)]
    fn connect(&self, url: &Url) -> Result<Box<dyn Stream>, String> {
        use std::net::ToSocketAddrs;
        let addrs: Vec<_> = (url.host.as_str(), url.port)
            .to_socket_addrs()
            .map_err(|e| format!("resolve {}: {}", url.host, e))?
            .collect();
        let mut last = String::from("no address");
        for a in addrs {
            match std::net::TcpStream::connect_timeout(&a, Duration::from_secs(5)) {
                Ok(s) => {
                    let _ = s.set_read_timeout(Some(self.io_timeout));
                    let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
                    let _ = s.set_nodelay(true);
                    if url.tls {
                        let name = rustls::pki_types::ServerName::try_from(url.host.clone())
                            .map_err(|e| e.to_string())?;
                        let conn = rustls::ClientConnection::new(self.tls.clone(), name)
                            .map_err(|e| e.to_string())?;
                        return Ok(Box::new(rustls::StreamOwned::new(conn, s)));
                    }
                    return Ok(Box::new(s));
                }
                Err(e) => last = format!("connect {}: {}", a, e),
            }
        }
        Err(last)
    }

    /// リクエストを送り、応答をまとめて読む（本文が `limit` を超えたらエラー）。
    pub fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        limit: usize,
    ) -> Result<HttpResponse, String> {
        let u = parse_url(url)?;
        let mut s = self.connect(&u)?;
        let host = if u.host.contains(':') {
            format!("[{}]:{}", u.host, u.port)
        } else {
            format!("{}:{}", u.host, u.port)
        };
        let mut req = format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {}\r\nConnection: close\r\n",
            method, u.path, host, self.user_agent
        );
        if !body.is_empty() || method == "POST" || method == "PUT" {
            req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        for (n, v) in headers {
            req.push_str(n);
            req.push_str(": ");
            req.push_str(v);
            req.push_str("\r\n");
        }
        req.push_str("\r\n");
        s.write_all(req.as_bytes())
            .map_err(|e| format!("write: {}", e))?;
        s.write_all(body).map_err(|e| format!("write: {}", e))?;
        s.flush().map_err(|e| format!("write: {}", e))?;
        let mut raw = Vec::new();
        let mut buf = [0u8; 16 * 1024];
        loop {
            match s.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    raw.extend_from_slice(&buf[..n]);
                    if raw.len() > limit + 64 * 1024 {
                        return Err(format!("response exceeds the limit ({} bytes)", limit));
                    }
                }
                // TLS の close_notify 無しの切断は、ヘッダを読めていれば受け入れる
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && !raw.is_empty() => break,
                Err(e) => return Err(format!("read: {}", e)),
            }
        }
        parse_response(&raw, limit, method == "HEAD")
    }
}

/// HTTP/1.1 の応答を解析する（Content-Length / chunked / close 区切り）。
pub fn parse_response(raw: &[u8], limit: usize, head: bool) -> Result<HttpResponse, String> {
    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("malformed response (no header terminator)")?;
    let text = std::str::from_utf8(&raw[..head_end]).map_err(|_| "non-UTF-8 response header")?;
    let mut lines = text.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or("malformed status line")?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| {
            let (n, v) = l.split_once(':')?;
            Some((n.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    if head || status == 204 || status == 304 {
        return Ok(HttpResponse {
            status,
            headers,
            body: Vec::new(),
        });
    }
    let chunked = headers.iter().any(|(n, v)| {
        n.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked")
    });
    let length = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok());
    let rest = &raw[head_end + 4..];
    let body = if chunked {
        crate::http_utils::decode_chunked_body(rest)
    } else if let Some(n) = length {
        rest.get(..n).ok_or("truncated response body")?.to_vec()
    } else {
        rest.to_vec()
    };
    if body.len() > limit {
        return Err(format!("response exceeds the limit ({} bytes)", limit));
    }
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_urls() {
        let u = parse_url("http://cp:18000").unwrap();
        assert_eq!(
            (u.tls, u.host.as_str(), u.port, u.path.as_str()),
            (false, "cp", 18000, "/")
        );
        let u = parse_url("https://acme.example/dir?x=1").unwrap();
        assert_eq!((u.tls, u.port, u.path.as_str()), (true, 443, "/dir?x=1"));
        let u = parse_url("http://[::1]:9/a").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("::1", 9));
        assert!(parse_url("ftp://x").is_err());
        assert!(parse_url("http://x:bad").is_err());
    }

    #[test]
    fn parse_responses() {
        let r = parse_response(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nX-A: b\r\n\r\nok",
            10,
            false,
        )
        .unwrap();
        assert_eq!(
            (r.status, r.body.as_slice(), r.header("x-a")),
            (200, &b"ok"[..], Some("b"))
        );
        let r = parse_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\n\r\n",
            10,
            false,
        )
        .unwrap();
        assert_eq!(r.body, b"ok");
        let r = parse_response(
            b"HTTP/1.1 200 OK\r\nReplay-Nonce: n\r\nContent-Length: 9\r\n\r\n",
            10,
            true,
        )
        .unwrap();
        assert_eq!(r.header("replay-nonce"), Some("n"));
        assert!(parse_response(
            b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n0123456789012345678",
            10,
            false
        )
        .is_err());
    }
}
