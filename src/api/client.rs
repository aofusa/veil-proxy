// F-267: API ライブラリの HTTP クライアント。本体の上流の部品（接続・TLS・HTTP/2 の多重化接続・名前解決）を使う。
//! An asynchronous HTTP client for handlers and background tasks, running on veil's own runtime.
//!
//! It reuses the proxy's upstream building blocks: TCP and Unix-socket connections, the upstream TLS client
//! (trust anchors from `[tls_client] ca`, or per client with [`ClientBuilder::ca`]), name resolution on the
//! offload pool, and the multiplexed HTTP/2 upstream connections (`h2c` with prior knowledge, or `h2` chosen by
//! ALPN). HTTP/1.1 connections are pooled per worker thread.
//!
//! ```no_run
//! # async fn f() -> Result<(), veil::api::client::ClientError> {
//! use veil::api::client::{Client, Http2};
//! let client = Client::builder().http2(Http2::Auto).timeout(std::time::Duration::from_secs(5)).build()?;
//! let res = client.post("https://api.example.com/v1/items").json(&serde_json::json!({"a": 1})).send().await?;
//! let v: serde_json::Value = res.json()?;
//! # Ok(()) }
//! ```
//!
//! A [`Client`] is cheap to clone (`Arc`) and can be kept in `App::state`. Requests must run on a veil worker or
//! background task (they use the worker's runtime and connection pools). Build clients at startup: a custom CA
//! or client certificate is read from disk when the client is built.

use crate::config::{ProxyTarget, UpstreamHttp2};
use crate::http2::upstream_mux;
use crate::runtime::io::{AsyncReadRent, AsyncWriteRentExt};
use crate::runtime::TcpStream;
use crate::upstream_tls::TlsProfile;
use bytes::{Buf, Bytes, BytesMut};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How the client uses HTTP/2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Http2 {
    /// HTTPS: offer `h2` and `http/1.1` with ALPN and use what the server picks; plain HTTP: HTTP/1.1 (default).
    #[default]
    Auto,
    /// HTTP/1.1 only.
    Off,
    /// HTTP/2 only: ALPN `h2` over TLS, prior knowledge (`h2c`) over plain TCP.
    On,
}

/// What went wrong with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The URL could not be parsed or contains a space or a control character (only `http://`, `https://` and,
    /// with [`ClientBuilder::unix_sockets`], `http://unix:/path:/uri` are accepted).
    Url,
    /// Connecting or the TLS handshake failed.
    Connect,
    /// The connect timeout or the request timeout expired.
    Timeout,
    /// The server sent something that is not valid HTTP, or reset the stream.
    Protocol,
    /// The request could not be encoded: an invalid method or header (a control character such as CR or LF, or a
    /// name that is not an HTTP token), or a JSON or form body.
    Encode,
    /// The response body could not be decoded (JSON).
    Decode,
    /// The response is larger than [`ClientBuilder::max_response_size`].
    TooLarge,
    /// The response status is not 2xx (from [`ClientResponse::error_for_status`]).
    Status,
    /// The client configuration is invalid (for example an unreadable CA file).
    Config,
}

/// An error of [`Client`].
#[derive(Debug, Clone)]
pub struct ClientError {
    kind: ErrorKind,
    message: String,
    status: Option<u16>,
}

impl ClientError {
    fn new(kind: ErrorKind, message: impl Into<String>) -> ClientError {
        ClientError {
            kind,
            message: message.into(),
            status: None,
        }
    }

    /// The kind of error.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The status for [`ErrorKind::Status`] errors.
    pub fn status(&self) -> Option<u16> {
        self.status
    }

    /// Whether this is a timeout.
    pub fn is_timeout(&self) -> bool {
        self.kind == ErrorKind::Timeout
    }

    /// Whether the request may be retried on a new connection (connection failures, resets and timeouts).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::Connect | ErrorKind::Protocol | ErrorKind::Timeout
        )
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ClientError {}

impl From<ClientError> for super::response::Response {
    fn from(e: ClientError) -> Self {
        let status = match e.kind {
            ErrorKind::Timeout => 504,
            _ => 502,
        };
        super::response::Response::error(status, &e.message)
    }
}

/// Settings of a [`Client`].
#[derive(Debug, Clone)]
pub struct ClientBuilder {
    http2: Http2,
    timeout: Duration,
    connect_timeout: Duration,
    pool_idle_timeout: Duration,
    max_response_size: usize,
    insecure: bool,
    ca: Vec<String>,
    client_cert: Option<(String, String)>,
    user_agent: Option<String>,
    default_headers: Vec<(Bytes, Bytes)>,
    unix_sockets: bool,
}

impl Default for ClientBuilder {
    fn default() -> Self {
        ClientBuilder {
            http2: Http2::Auto,
            timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(5),
            pool_idle_timeout: Duration::from_secs(60),
            max_response_size: 64 * 1024 * 1024,
            insecure: false,
            ca: Vec::new(),
            client_cert: None,
            user_agent: None,
            default_headers: Vec::new(),
            unix_sockets: false,
        }
    }
}

impl ClientBuilder {
    /// How HTTP/2 is used (default [`Http2::Auto`]).
    pub fn http2(mut self, mode: Http2) -> Self {
        self.http2 = mode;
        self
    }

    /// Time limit of a whole request, from connecting to the last byte of the response (default 30 s).
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    /// Time limit of connecting and the TLS handshake (default 5 s).
    pub fn connect_timeout(mut self, d: Duration) -> Self {
        self.connect_timeout = d;
        self
    }

    /// How long an idle pooled connection is kept (default 60 s; zero disables pooling).
    pub fn pool_idle_timeout(mut self, d: Duration) -> Self {
        self.pool_idle_timeout = d;
        self
    }

    /// Largest response body accepted (default 64 MiB).
    pub fn max_response_size(mut self, n: usize) -> Self {
        self.max_response_size = n;
        self
    }

    /// Trust anchors for HTTPS: `"system"`, `"builtin"` or PEM file paths (default: `[tls_client] ca`).
    pub fn ca<S: Into<String>>(mut self, sources: impl IntoIterator<Item = S>) -> Self {
        self.ca = sources.into_iter().map(Into::into).collect();
        self
    }

    /// A client certificate and key (PEM files) for mutual TLS.
    pub fn client_cert(mut self, cert: impl Into<String>, key: impl Into<String>) -> Self {
        self.client_cert = Some((cert.into(), key.into()));
        self
    }

    /// Accept any server certificate (testing only).
    pub fn danger_accept_invalid_certs(mut self, on: bool) -> Self {
        self.insecure = on;
        self
    }

    /// The `user-agent` header (default `veil/<version>`).
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = Some(ua.into());
        self
    }

    /// A header sent with every request.
    pub fn default_header(mut self, name: impl Into<Bytes>, value: impl Into<Bytes>) -> Self {
        self.default_headers.push((name.into(), value.into()));
        self
    }

    // B-198: URL 文字列だけでローカルのソケットへ届かないよう、既定では受けない
    /// Accept `http://unix:/path.sock:/uri` URLs, which connect to a Unix socket (default `false`, so a URL that
    /// comes from a user cannot reach a local socket).
    pub fn unix_sockets(mut self, on: bool) -> Self {
        self.unix_sockets = on;
        self
    }

    /// Builds the client (reads the CA and client certificate files now).
    pub fn build(self) -> Result<Client, ClientError> {
        if let Some((n, _)) = self
            .default_headers
            .iter()
            .find(|(n, v)| !is_token(n) || !valid_header_value(v))
        {
            return Err(ClientError::new(
                ErrorKind::Config,
                format!(
                    "invalid default header '{}'",
                    String::from_utf8_lossy(n).escape_debug()
                ),
            ));
        }
        let tls =
            if self.insecure {
                TlsProfile::Insecure
            } else if self.ca.is_empty() && self.client_cert.is_none() {
                TlsProfile::Verify
            } else {
                let (cert, key) = self.client_cert.clone().unwrap_or_default();
                let settings = crate::upstream_tls::UpstreamTlsSettings {
                    tls_client_cert: cert,
                    tls_client_key: key,
                    tls_ca: crate::trust_store::CaSources::from_strs(&self.ca),
                    expected_san: Vec::new(),
                };
                TlsProfile::Custom(crate::upstream_tls::intern(&settings, false).map_err(|e| {
                    ClientError::new(ErrorKind::Config, format!("client TLS: {}", e))
                })?)
            };
        let ua = self
            .user_agent
            .clone()
            .unwrap_or_else(|| format!("veil/{}", env!("CARGO_PKG_VERSION")));
        Ok(Client {
            inner: Arc::new(Inner {
                id: NEXT_CLIENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                cfg: self,
                tls,
                user_agent: Bytes::from(ua),
                origins: Mutex::new(HashMap::new()),
            }),
        })
    }
}

static NEXT_CLIENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

struct Inner {
    id: u64,
    cfg: ClientBuilder,
    tls: TlsProfile,
    user_agent: Bytes,
    /// `scheme://authority` → the parsed origin (parsed once per origin, not per request).
    origins: Mutex<HashMap<String, Arc<Origin>>>,
}

/// One server the client talks to.
pub(crate) struct Origin {
    pub(crate) target: ProxyTarget,
    /// The `host` / `:authority` value.
    pub(crate) authority: Bytes,
    /// Key of the connection pools (distinct per client: the TLS settings and HTTP/2 mode differ).
    pub(crate) pool_key: String,
}

impl std::fmt::Debug for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Origin")
            .field("authority", &self.authority)
            .field("pool_key", &self.pool_key)
            .finish()
    }
}

/// An HTTP client (see the [module documentation](self)).
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("http2", &self.inner.cfg.http2)
            .finish()
    }
}

impl Default for Client {
    fn default() -> Self {
        Client::new()
    }
}

impl Client {
    /// A client with the default settings.
    pub fn new() -> Client {
        ClientBuilder::default()
            .build()
            .expect("the default client settings are valid")
    }

    /// Settings for a new client.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// A request with any method (an HTTP token; anything else makes `send` fail with [`ErrorKind::Encode`]).
    pub fn request(&self, method: &str, url: &str) -> RequestBuilder {
        let error = (!is_token(method.as_bytes())).then(|| {
            ClientError::new(
                ErrorKind::Encode,
                format!("invalid request method '{}'", method.escape_debug()),
            )
        });
        RequestBuilder {
            client: self.clone(),
            method: Bytes::copy_from_slice(method.as_bytes()),
            url: url.to_string(),
            headers: Vec::new(),
            body: Bytes::new(),
            timeout: None,
            error,
        }
    }

    /// A `GET` request.
    pub fn get(&self, url: &str) -> RequestBuilder {
        self.request("GET", url)
    }

    /// A `POST` request.
    pub fn post(&self, url: &str) -> RequestBuilder {
        self.request("POST", url)
    }

    /// A `PUT` request.
    pub fn put(&self, url: &str) -> RequestBuilder {
        self.request("PUT", url)
    }

    /// A `PATCH` request.
    pub fn patch(&self, url: &str) -> RequestBuilder {
        self.request("PATCH", url)
    }

    /// A `DELETE` request.
    pub fn delete(&self, url: &str) -> RequestBuilder {
        self.request("DELETE", url)
    }

    /// A `HEAD` request.
    pub fn head(&self, url: &str) -> RequestBuilder {
        self.request("HEAD", url)
    }

    /// The request timeout.
    #[cfg(feature = "grpc-handler")]
    pub(crate) fn request_timeout(&self) -> Duration {
        self.inner.cfg.timeout
    }

    /// Parses `url` into its origin (cached) and the request target (path and query).
    pub(crate) fn resolve(&self, url: &str) -> Result<(Arc<Origin>, String), ClientError> {
        let bad = || {
            ClientError::new(
                ErrorKind::Url,
                format!("unsupported URL '{}'", url.escape_debug()),
            )
        };
        // B-198: 空白・制御文字（CR・LF 等）は要求行に書けない
        if !url.bytes().all(|c| c > 0x20 && c != 0x7f) {
            return Err(bad());
        }
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .ok_or_else(bad)?;
        // オリジンの終わり（unix: は `:/` の手前まで）
        let origin_len = if let Some(u) = rest.strip_prefix("unix:") {
            if !self.inner.cfg.unix_sockets {
                return Err(ClientError::new(
                    ErrorKind::Url,
                    "Unix socket URLs are disabled (enable them with ClientBuilder::unix_sockets)",
                ));
            }
            let end = u.find(":/").map(|i| i + 5).unwrap_or(rest.len());
            url.len() - rest.len() + end
        } else {
            url.len() - rest.len() + rest.find(['/', '?']).unwrap_or(rest.len())
        };
        let origin_str = &url[..origin_len];
        let target_part = &url[origin_len..];
        let target = if rest.starts_with("unix:") {
            target_part.strip_prefix(':').unwrap_or(target_part)
        } else {
            target_part
        };
        let target = if target.is_empty() {
            "/".to_string()
        } else if target.starts_with('?') {
            format!("/{}", target)
        } else {
            target.to_string()
        };
        if let Some(o) = self.inner.origins.lock().unwrap().get(origin_str) {
            return Ok((o.clone(), target));
        }
        let mut t = ProxyTarget::parse(origin_str).ok_or_else(bad)?;
        t.path_prefix = "/".into();
        t.http2 = match self.inner.cfg.http2 {
            Http2::Auto => UpstreamHttp2::Auto,
            Http2::Off => UpstreamHttp2::Off,
            Http2::On => UpstreamHttp2::On,
        };
        if !t.use_tls && self.inner.cfg.http2 == Http2::On {
            t.use_h2c = true;
        }
        let default_port = if t.use_tls { 443 } else { 80 };
        let authority = if t.is_unix() || t.port == default_port {
            t.host.clone()
        } else {
            format!("{}:{}", t.host, t.port)
        };
        let pool_key = format!(
            "veil-client#{}|{}|{}|{}",
            self.inner.id,
            t.conn_addr().as_str(),
            t.sni(),
            self.inner.tls.tag()
        );
        let o = Arc::new(Origin {
            authority: Bytes::from(authority),
            target: t,
            pool_key,
        });
        self.inner
            .origins
            .lock()
            .unwrap()
            .insert(origin_str.to_string(), o.clone());
        Ok((o, target))
    }

    /// Opens an HTTP/2 stream (for the gRPC channel). `headers` exclude the pseudo-headers.
    #[cfg(feature = "grpc-handler")]
    pub(crate) async fn open_h2_stream(
        &self,
        origin: &Origin,
        method: &[u8],
        path: &[u8],
        headers: &[(Bytes, Bytes)],
        end_stream: bool,
    ) -> Result<upstream_mux::UpStream, ClientError> {
        let hs = headers.iter().map(|(n, v)| (n.as_ref(), v.as_ref()));
        let idle = self.inner.cfg.pool_idle_timeout;
        let ct = self.inner.cfg.connect_timeout;
        let r = if origin.target.use_tls {
            let mut target = origin.target.clone();
            target.http2 = UpstreamHttp2::On;
            self.prepare_h2(origin, &target).await?;
            match upstream_mux::open_https(
                &target,
                &origin.pool_key,
                self.inner.tls,
                ct,
                idle,
                method,
                path,
                &origin.authority,
                hs,
                end_stream,
            )
            .await
            {
                Ok(upstream_mux::HttpsOpen::H2(s)) => Ok(s),
                Ok(upstream_mux::HttpsOpen::Http1(_)) => Err(502),
                Err(e) => Err(e),
            }
        } else {
            self.prepare_h2(origin, &origin.target).await?;
            upstream_mux::open_h2c(
                &origin.target,
                &origin.pool_key,
                ct,
                idle,
                method,
                path,
                &origin.authority,
                hs,
                end_stream,
            )
            .await
        };
        r.map_err(status_error)
    }

    async fn send(&self, req: RequestBuilder) -> Result<ClientResponse, ClientError> {
        if let Some(e) = req.error {
            return Err(e);
        }
        let (origin, target) = self.resolve(&req.url)?;
        let limit = req.timeout.unwrap_or(self.inner.cfg.timeout);
        match crate::runtime::time::timeout(
            limit,
            self.send_to(&origin, &target, &req.method, &req.headers, req.body),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => Err(ClientError::new(
                ErrorKind::Timeout,
                format!("request to {} timed out", req.url),
            )),
        }
    }

    async fn send_to(
        &self,
        origin: &Origin,
        target: &str,
        method: &Bytes,
        headers: &[(Bytes, Bytes)],
        body: Bytes,
    ) -> Result<ClientResponse, ClientError> {
        let idempotent = matches!(
            &method[..],
            b"GET" | b"HEAD" | b"OPTIONS" | b"PUT" | b"DELETE" | b"TRACE"
        );
        let mut all: Vec<(Bytes, Bytes)> =
            Vec::with_capacity(headers.len() + self.inner.cfg.default_headers.len() + 1);
        all.extend(self.inner.cfg.default_headers.iter().cloned());
        all.extend(headers.iter().cloned());
        if !all
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(b"user-agent"))
        {
            all.push((
                Bytes::from_static(b"user-agent"),
                self.inner.user_agent.clone(),
            ));
        }
        let t = &origin.target;
        // HTTP/2: h2c（On の平文）か TLS の上の h2（Auto・On）
        if t.use_h2c || t.h2_over_tls() {
            let read_timeout = self.inner.cfg.timeout;
            let hs = all.iter().map(|(n, v)| (n.as_ref(), v.as_ref()));
            let body_opt = (!body.is_empty()).then(|| body.clone());
            if let Some(conn) = self.prepare_h2(origin, t).await? {
                return self
                    .send_h1(origin, target, method, &all, body, idempotent, Some(conn))
                    .await;
            }
            let r = if t.use_tls {
                upstream_mux::request_https_buffered(
                    t,
                    &origin.pool_key,
                    self.inner.tls,
                    self.inner.cfg.connect_timeout,
                    self.inner.cfg.pool_idle_timeout,
                    read_timeout,
                    method,
                    target.as_bytes(),
                    &origin.authority,
                    hs,
                    body_opt,
                    idempotent,
                )
                .await
                .map_err(status_error)?
            } else {
                let r = upstream_mux::request_h2c_buffered(
                    t,
                    &origin.pool_key,
                    self.inner.cfg.connect_timeout,
                    self.inner.cfg.pool_idle_timeout,
                    read_timeout,
                    method,
                    target.as_bytes(),
                    &origin.authority,
                    hs,
                    body_opt,
                    idempotent,
                )
                .await
                .map_err(status_error)?;
                upstream_mux::HttpsBuffered::H2(r)
            };
            match r {
                upstream_mux::HttpsBuffered::H2(r) => {
                    if r.body.len() > self.inner.cfg.max_response_size {
                        return Err(too_large(self.inner.cfg.max_response_size));
                    }
                    return Ok(ClientResponse {
                        status: r.status,
                        headers: r.headers,
                        body: r.body,
                        trailers: r.trailers,
                        version: super::request::HttpVersion::Http2,
                    });
                }
                upstream_mux::HttpsBuffered::Http1(conn) => {
                    // 上流は HTTP/1.1 を選んだ（ALPN）。張った接続があればそれで送る
                    let conn = conn.map(Conn::Tls);
                    return self
                        .send_h1(origin, target, method, &all, body, idempotent, conn)
                        .await;
                }
            }
        }
        self.send_h1(origin, target, method, &all, body, idempotent, None)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_h1(
        &self,
        origin: &Origin,
        target: &str,
        method: &Bytes,
        headers: &[(Bytes, Bytes)],
        body: Bytes,
        idempotent: bool,
        fresh: Option<Conn>,
    ) -> Result<ClientResponse, ClientError> {
        let mut head = Vec::with_capacity(256 + target.len());
        head.extend_from_slice(method);
        head.push(b' ');
        head.extend_from_slice(target.as_bytes());
        head.extend_from_slice(b" HTTP/1.1\r\nhost: ");
        head.extend_from_slice(&origin.authority);
        head.extend_from_slice(b"\r\n");
        for (n, v) in headers {
            if n.eq_ignore_ascii_case(b"host")
                || n.eq_ignore_ascii_case(b"content-length")
                || n.eq_ignore_ascii_case(b"transfer-encoding")
            {
                continue;
            }
            head.extend_from_slice(n);
            head.extend_from_slice(b": ");
            head.extend_from_slice(v);
            head.extend_from_slice(b"\r\n");
        }
        if !body.is_empty() || matches!(&method[..], b"POST" | b"PUT" | b"PATCH") {
            head.extend_from_slice(b"content-length: ");
            head.extend_from_slice(itoa::Buffer::new().format(body.len()).as_bytes());
            head.extend_from_slice(b"\r\n");
        }
        head.extend_from_slice(b"\r\n");
        let head = Bytes::from(head);
        let is_head = &method[..] == b"HEAD";
        let mut fresh = fresh;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let (conn, pooled) = match fresh.take() {
                Some(c) => (c, false),
                None => match pool_take(&origin.pool_key) {
                    Some(c) => (c, true),
                    None => (self.connect_h1(origin).await?, false),
                },
            };
            match exchange(
                conn,
                &head,
                &body,
                is_head,
                self.inner.cfg.max_response_size,
            )
            .await
            {
                Ok((resp, keep)) => {
                    if let Some(c) = keep {
                        if !self.inner.cfg.pool_idle_timeout.is_zero() {
                            pool_put(&origin.pool_key, c, self.inner.cfg.pool_idle_timeout);
                        }
                    }
                    return Ok(resp);
                }
                // プールの接続が応答の前に切れていた: 冪等な要求だけ新しい接続で 1 回送り直す（F-177 と同じ規則）
                Err((e, nothing_received)) => {
                    if pooled && nothing_received && idempotent && attempt == 1 {
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    async fn connect_h1(&self, origin: &Origin) -> Result<Conn, ClientError> {
        self.dial(&origin.target, false).await
    }

    /// HTTP/2 の要求の前に、プールに接続が無ければ自分で張って登録する。接続・TLS の失敗を
    /// `Connect` / `Timeout` として理由付きで返すため（多重化接続の層は状態コードしか返さない）。
    /// 上流が ALPN で HTTP/1.1 を選んだら、その接続を返す（呼び出し側は HTTP/1.1 で送る）。
    async fn prepare_h2(
        &self,
        origin: &Origin,
        t: &ProxyTarget,
    ) -> Result<Option<Conn>, ClientError> {
        let key = origin.pool_key.as_str();
        let idle = self.inner.cfg.pool_idle_timeout;
        if idle.is_zero() || upstream_mux::pool_has_live(key) {
            return Ok(None);
        }
        if t.use_tls && t.http2 == UpstreamHttp2::Auto && upstream_mux::https_known_h1(key) {
            return Ok(None);
        }
        match self.dial(t, true).await? {
            Conn::Tcp(tcp) => {
                upstream_mux::pool_insert(key, upstream_mux::H2Mux::start(tcp, false, idle));
                Ok(None)
            }
            Conn::Tls(tls) => {
                if tls.negotiated_h2() {
                    upstream_mux::pool_insert(key, upstream_mux::H2Mux::start(*tls, true, idle));
                    Ok(None)
                } else if t.http2 == UpstreamHttp2::On {
                    Err(ClientError::new(
                        ErrorKind::Protocol,
                        format!("{} did not negotiate h2", t.conn_addr().as_str()),
                    ))
                } else {
                    upstream_mux::remember_h1(key);
                    Ok(Some(Conn::Tls(tls)))
                }
            }
        }
    }

    /// TCP（と TLS）の接続を張る。`h2` なら ALPN で h2 も申し出る。
    async fn dial(&self, t: &ProxyTarget, h2: bool) -> Result<Conn, ClientError> {
        let addr = t.conn_addr();
        let ct = self.inner.cfg.connect_timeout;
        let tcp =
            match crate::runtime::time::timeout(ct, crate::proxy::connect_target(t, addr.as_str()))
                .await
            {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    return Err(ClientError::new(
                        ErrorKind::Connect,
                        format!("connect to {} failed: {}", addr.as_str(), e),
                    ))
                }
                Err(_) => {
                    return Err(ClientError::new(
                        ErrorKind::Timeout,
                        format!("connect to {} timed out", addr.as_str()),
                    ))
                }
            };
        let _ = tcp.set_nodelay(true);
        if !t.use_tls {
            return Ok(Conn::Tcp(tcp));
        }
        let connector = if h2 {
            crate::config::tls_connector_h2(self.inner.tls)
        } else {
            crate::config::tls_connector(self.inner.tls)
        };
        match crate::runtime::time::timeout(ct, connector.connect(tcp, t.sni())).await {
            Ok(Ok(s)) => Ok(Conn::Tls(Box::new(s))),
            Ok(Err(e)) => Err(ClientError::new(
                ErrorKind::Connect,
                format!("TLS handshake with {} failed: {}", addr.as_str(), e),
            )),
            Err(_) => Err(ClientError::new(
                ErrorKind::Timeout,
                format!("TLS handshake with {} timed out", addr.as_str()),
            )),
        }
    }
}

fn status_error(code: u16) -> ClientError {
    if code == 504 {
        ClientError::new(ErrorKind::Timeout, "the server did not answer in time")
    } else {
        ClientError::new(ErrorKind::Protocol, "the connection or stream failed")
    }
}

fn too_large(max: usize) -> ClientError {
    ClientError::new(
        ErrorKind::TooLarge,
        format!("response body larger than max_response_size ({})", max),
    )
}

// ===================== HTTP/1.1 の接続 =====================

enum Conn {
    Tcp(TcpStream),
    Tls(Box<crate::pool::ClientTls>),
}

impl Conn {
    async fn read(&mut self, buf: BytesMut) -> (std::io::Result<usize>, BytesMut) {
        match self {
            Conn::Tcp(s) => s.read(buf).await,
            Conn::Tls(s) => s.read(buf).await,
        }
    }

    async fn write_all(&mut self, buf: Bytes) -> std::io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        match self {
            Conn::Tcp(s) => s.write_all(buf).await.0.map(|_| ()),
            Conn::Tls(s) => s.write_all(buf).await.0.map(|_| ()),
        }
    }
}

thread_local! {
    /// HTTP/1.1 の空き接続（ワーカーごと。鍵はクライアントとオリジン）
    static H1_POOL: RefCell<HashMap<String, Vec<(Conn, Instant, Duration)>>> = RefCell::new(HashMap::new());
}

fn pool_take(key: &str) -> Option<Conn> {
    H1_POOL.with(|p| {
        let mut p = p.borrow_mut();
        let v = p.get_mut(key)?;
        while let Some((c, at, idle)) = v.pop() {
            if at.elapsed() < idle {
                return Some(c);
            }
        }
        None
    })
}

fn pool_put(key: &str, c: Conn, idle: Duration) {
    H1_POOL.with(|p| {
        let mut p = p.borrow_mut();
        let v = match p.get_mut(key) {
            Some(v) => v,
            None => p.entry(key.to_string()).or_default(),
        };
        // 古い接続を捨て、オリジンあたりの上限（64）を超えない
        v.retain(|(_, at, idle)| at.elapsed() < *idle);
        if v.len() < 64 {
            v.push((c, Instant::now(), idle));
        }
    });
}

type ExchangeError = (ClientError, bool);

/// Writes one request and reads its response. Returns the connection when it can be reused. On error the flag says
/// whether no response byte was received (so an idempotent request may be retried).
async fn exchange(
    mut conn: Conn,
    head: &Bytes,
    body: &Bytes,
    is_head: bool,
    max: usize,
) -> Result<(ClientResponse, Option<Conn>), ExchangeError> {
    let io_err = |e: std::io::Error, nothing: bool| {
        (
            ClientError::new(ErrorKind::Protocol, format!("connection error: {}", e)),
            nothing,
        )
    };
    conn.write_all(head.clone())
        .await
        .map_err(|e| io_err(e, true))?;
    conn.write_all(body.clone())
        .await
        .map_err(|e| io_err(e, true))?;
    let mut buf = BytesMut::with_capacity(8 * 1024);
    let mut received = false;
    // ヘッダ（1xx は読み飛ばす）
    let (status, headers) = loop {
        let mut hs = [httparse::EMPTY_HEADER; 64];
        let mut r = httparse::Response::new(&mut hs);
        match r.parse(&buf) {
            Ok(httparse::Status::Complete(n)) => {
                let status = r.code.unwrap_or(0);
                let off = |s: &[u8]| s.as_ptr() as usize - buf.as_ptr() as usize;
                let spans: Vec<((usize, usize), (usize, usize))> = r
                    .headers
                    .iter()
                    .map(|h| {
                        (
                            (off(h.name.as_bytes()), h.name.len()),
                            (off(h.value), h.value.len()),
                        )
                    })
                    .collect();
                let head_bytes = buf.split_to(n).freeze();
                if (100..200).contains(&status) && status != 101 {
                    continue;
                }
                let headers: Vec<(Bytes, Bytes)> = spans
                    .into_iter()
                    .map(|((no, nl), (vo, vl))| {
                        (head_bytes.slice(no..no + nl), head_bytes.slice(vo..vo + vl))
                    })
                    .collect();
                break (status, headers);
            }
            Ok(httparse::Status::Partial) => {
                if buf.len() > 64 * 1024 {
                    return Err((
                        ClientError::new(ErrorKind::Protocol, "response headers too large"),
                        false,
                    ));
                }
            }
            Err(e) => {
                return Err((
                    ClientError::new(ErrorKind::Protocol, format!("invalid response: {}", e)),
                    !received,
                ))
            }
        }
        buf.reserve(8 * 1024);
        let (r, b) = conn.read(buf).await;
        buf = b;
        match r {
            Ok(0) => {
                return Err((
                    ClientError::new(ErrorKind::Protocol, "connection closed before the response"),
                    !received,
                ))
            }
            Ok(_) => received = true,
            Err(e) => return Err(io_err(e, !received)),
        }
    };
    let hval = |name: &[u8]| {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let close = hval(b"connection").is_some_and(|v| {
        v.split(|&b| b == b',')
            .any(|t| t.trim_ascii().eq_ignore_ascii_case(b"close"))
    });
    let chunked = hval(b"transfer-encoding").is_some_and(|v| {
        v.split(|&b| b == b',')
            .any(|t| t.trim_ascii().eq_ignore_ascii_case(b"chunked"))
    });
    let length = hval(b"content-length")
        .and_then(|v| std::str::from_utf8(&v).ok()?.trim().parse::<usize>().ok());
    let no_body = is_head || status == 204 || status == 304;
    let mut trailers = Vec::new();
    let (body, reusable) = if no_body {
        (Bytes::new(), !close)
    } else if chunked {
        let (b, t) = read_chunked(&mut conn, &mut buf, max).await?;
        trailers = t;
        (b, !close)
    } else if let Some(len) = length {
        if len > max {
            return Err((too_large(max), false));
        }
        while buf.len() < len {
            buf.reserve(len - buf.len());
            let (r, b) = conn.read(buf).await;
            buf = b;
            match r {
                Ok(0) => {
                    return Err((
                        ClientError::new(
                            ErrorKind::Protocol,
                            "connection closed in the middle of the body",
                        ),
                        false,
                    ))
                }
                Ok(_) => {}
                Err(e) => return Err(io_err(e, false)),
            }
        }
        (buf.split_to(len).freeze(), !close)
    } else {
        // 長さの無い本文: 接続が閉じるまで（再利用しない）
        loop {
            if buf.len() > max {
                return Err((too_large(max), false));
            }
            buf.reserve(16 * 1024);
            let (r, b) = conn.read(buf).await;
            buf = b;
            match r {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => return Err(io_err(e, false)),
            }
        }
        (std::mem::take(&mut buf).freeze(), false)
    };
    let resp = ClientResponse {
        status,
        headers,
        body,
        trailers,
        version: super::request::HttpVersion::Http11,
    };
    // 次の応答の断片が残っている接続は使い回さない
    Ok((resp, (reusable && buf.is_empty()).then_some(conn)))
}

/// Reads a chunked body (and its trailers).
// B-198: 16 進の数字だけを受ける（from_str_radix は先頭の `+` も受ける）
/// The size of a chunk from its size line (before any extension).
fn parse_chunk_size(hex: &[u8]) -> Option<usize> {
    if hex.is_empty() || !hex.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    usize::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()
}

// B-198: 巨大なチャンクの大きさで加算があふれて上限の検査を素通りしない
/// The bytes to buffer for a chunk of `size` (data and CRLF), or `None` when the body would exceed `max`.
fn chunk_need(have: usize, size: usize, max: usize) -> Option<usize> {
    have.checked_add(size).filter(|&n| n <= max)?;
    size.checked_add(2)
}

async fn read_chunked(
    conn: &mut Conn,
    buf: &mut BytesMut,
    max: usize,
) -> Result<(Bytes, Vec<(Bytes, Bytes)>), ExchangeError> {
    let mut out = BytesMut::new();
    let proto = |m: &str| (ClientError::new(ErrorKind::Protocol, m.to_string()), false);
    loop {
        // 断片の大きさの行
        let line_end = loop {
            if let Some(i) = buf.windows(2).position(|w| w == b"\r\n") {
                break i;
            }
            if buf.len() > 4096 {
                return Err(proto("invalid chunk size line"));
            }
            fill(conn, buf).await?;
        };
        let line = buf.split_to(line_end + 2);
        let hex = line[..line_end]
            .split(|&b| b == b';')
            .next()
            .unwrap_or(b"")
            .trim_ascii();
        let size = parse_chunk_size(hex).ok_or_else(|| proto("invalid chunk size"))?;
        if size == 0 {
            // トレーラ（空行まで）
            let mut trailers = Vec::new();
            loop {
                let end = loop {
                    if let Some(i) = buf.windows(2).position(|w| w == b"\r\n") {
                        break i;
                    }
                    if buf.len() > 64 * 1024 {
                        return Err(proto("trailers too large"));
                    }
                    fill(conn, buf).await?;
                };
                let l = buf.split_to(end + 2).freeze();
                if end == 0 {
                    break;
                }
                if let Some(c) = l[..end].iter().position(|&b| b == b':') {
                    let v = l.slice(c + 1..end);
                    let v = v.slice(v.len() - v.trim_ascii_start().len()..);
                    trailers.push((l.slice(..c), v));
                }
            }
            return Ok((out.freeze(), trailers));
        }
        let need = chunk_need(out.len(), size, max).ok_or_else(|| (too_large(max), false))?;
        while buf.len() < need {
            fill(conn, buf).await?;
        }
        out.extend_from_slice(&buf[..size]);
        buf.advance(need);
    }
}

async fn fill(conn: &mut Conn, buf: &mut BytesMut) -> Result<(), ExchangeError> {
    buf.reserve(16 * 1024);
    let b = std::mem::take(buf);
    let (r, b) = conn.read(b).await;
    *buf = b;
    match r {
        Ok(0) => Err((
            ClientError::new(
                ErrorKind::Protocol,
                "connection closed in the middle of the body",
            ),
            false,
        )),
        Ok(_) => Ok(()),
        Err(e) => Err((
            ClientError::new(ErrorKind::Protocol, format!("connection error: {}", e)),
            false,
        )),
    }
}

// ===================== 要求・応答 =====================

/// A request being built (see [`Client::get`] and friends).
pub struct RequestBuilder {
    client: Client,
    method: Bytes,
    url: String,
    headers: Vec<(Bytes, Bytes)>,
    body: Bytes,
    timeout: Option<Duration>,
    error: Option<ClientError>,
}

impl RequestBuilder {
    /// Adds a header. A name that is not an HTTP token or a value with a control character (CR, LF, NUL, ...)
    /// makes `send` fail with [`ErrorKind::Encode`].
    pub fn header(mut self, name: impl Into<Bytes>, value: impl Into<Bytes>) -> Self {
        let (name, value) = (name.into(), value.into());
        if self.error.is_none() && (!is_token(&name) || !valid_header_value(&value)) {
            self.error = Some(ClientError::new(
                ErrorKind::Encode,
                format!(
                    "invalid header '{}'",
                    String::from_utf8_lossy(&name).escape_debug()
                ),
            ));
        }
        self.headers.push((name, value));
        self
    }

    /// `authorization: Bearer <token>`.
    pub fn bearer_auth(self, token: &str) -> Self {
        self.header("authorization", format!("Bearer {}", token))
    }

    /// `authorization: Basic ...`.
    pub fn basic_auth(self, user: &str, password: Option<&str>) -> Self {
        let raw = format!("{}:{}", user, password.unwrap_or(""));
        self.header(
            "authorization",
            format!("Basic {}", base64_std(raw.as_bytes())),
        )
    }

    /// Appends `?k=v&...` to the URL (form-encoded).
    pub fn query<T: serde::Serialize + ?Sized>(mut self, q: &T) -> Self {
        match serde_urlencoded::to_string(q) {
            Ok(s) if !s.is_empty() => {
                self.url
                    .push(if self.url.contains('?') { '&' } else { '?' });
                self.url.push_str(&s);
            }
            Ok(_) => {}
            Err(e) => self.error = Some(ClientError::new(ErrorKind::Encode, e.to_string())),
        }
        self
    }

    /// The raw body.
    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

    /// A JSON body (`content-type: application/json`).
    pub fn json<T: serde::Serialize + ?Sized>(mut self, v: &T) -> Self {
        match serde_json::to_vec(v) {
            Ok(b) => {
                self.body = Bytes::from(b);
                self.headers.push((
                    Bytes::from_static(b"content-type"),
                    Bytes::from_static(b"application/json"),
                ));
            }
            Err(e) => self.error = Some(ClientError::new(ErrorKind::Encode, e.to_string())),
        }
        self
    }

    /// A form body (`application/x-www-form-urlencoded`).
    pub fn form<T: serde::Serialize + ?Sized>(mut self, v: &T) -> Self {
        match serde_urlencoded::to_string(v) {
            Ok(s) => {
                self.body = Bytes::from(s);
                self.headers.push((
                    Bytes::from_static(b"content-type"),
                    Bytes::from_static(b"application/x-www-form-urlencoded"),
                ));
            }
            Err(e) => self.error = Some(ClientError::new(ErrorKind::Encode, e.to_string())),
        }
        self
    }

    /// Time limit of this request (overrides [`ClientBuilder::timeout`]).
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }

    /// Sends the request and reads the whole response.
    pub async fn send(self) -> Result<ClientResponse, ClientError> {
        let c = self.client.clone();
        c.send(self).await
    }
}

// B-198: 要求行・ヘッダへ CR・LF 等を書かない（ヘッダの注入・上流への要求のスマグリング）
/// Whether `b` is an HTTP token (RFC 9110 section 5.6.2): a method or a header name.
fn is_token(b: &[u8]) -> bool {
    !b.is_empty()
        && b.iter()
            .all(|&c| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c))
}

/// Whether `v` can be sent as a header value (no control characters other than HTAB).
fn valid_header_value(v: &[u8]) -> bool {
    v.iter().all(|&c| c == b'\t' || (c >= 0x20 && c != 0x7f))
}

/// Standard base64 with padding (for `basic_auth`).
pub(crate) fn base64_std(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for c in input.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// A received response (the body is read completely).
#[derive(Debug, Clone)]
pub struct ClientResponse {
    status: u16,
    headers: Vec<(Bytes, Bytes)>,
    body: Bytes,
    trailers: Vec<(Bytes, Bytes)>,
    version: super::request::HttpVersion,
}

impl ClientResponse {
    /// The status code.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Whether the status is 2xx.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The protocol the response came over.
    pub fn version(&self) -> super::request::HttpVersion {
        self.version
    }

    /// A header (case-insensitive; the first value).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
            .and_then(|(_, v)| std::str::from_utf8(v).ok())
    }

    /// All headers.
    pub fn headers(&self) -> &[(Bytes, Bytes)] {
        &self.headers
    }

    /// The trailers (HTTP/2 and chunked HTTP/1.1).
    pub fn trailers(&self) -> &[(Bytes, Bytes)] {
        &self.trailers
    }

    /// The body.
    pub fn bytes(&self) -> &Bytes {
        &self.body
    }

    /// The body, consuming the response.
    pub fn into_bytes(self) -> Bytes {
        self.body
    }

    /// The body as UTF-8 (invalid bytes replaced).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The body decoded as JSON.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, ClientError> {
        serde_json::from_slice(&self.body).map_err(|e| {
            ClientError::new(ErrorKind::Decode, format!("invalid JSON response: {}", e))
        })
    }

    /// `Err` with [`ErrorKind::Status`] unless the status is 2xx.
    pub fn error_for_status(self) -> Result<ClientResponse, ClientError> {
        if self.is_success() {
            return Ok(self);
        }
        let mut e = ClientError::new(
            ErrorKind::Status,
            format!("the server answered {}", self.status),
        );
        e.status = Some(self.status);
        Err(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_split_into_origin_and_target() {
        let c = Client::new();
        let (o, t) = c.resolve("http://example.com:8080/a/b?x=1").unwrap();
        assert_eq!(&o.authority[..], b"example.com:8080");
        assert_eq!(t, "/a/b?x=1");
        let (o2, t) = c.resolve("http://example.com:8080").unwrap();
        assert!(Arc::ptr_eq(&o, &o2), "origins are cached");
        assert_eq!(t, "/");
        let (o, t) = c.resolve("https://example.com?q").unwrap();
        assert_eq!(&o.authority[..], b"example.com");
        assert!(o.target.use_tls);
        assert_eq!(t, "/?q");
        // B-198: Unix ソケットの URL は明示的に許したクライアントだけ
        assert_eq!(
            c.resolve("http://unix:/run/app.sock:/v1/x")
                .unwrap_err()
                .kind(),
            ErrorKind::Url
        );
        #[cfg(unix)]
        {
            let u = Client::builder().unix_sockets(true).build().unwrap();
            let (o, t) = u.resolve("http://unix:/run/app.sock:/v1/x").unwrap();
            assert!(o.target.is_unix());
            assert_eq!(t, "/v1/x");
        }
        assert_eq!(c.resolve("ftp://x").unwrap_err().kind(), ErrorKind::Url);
    }

    // B-198: 制御文字・空白を要求行・ヘッダに書かない
    #[test]
    fn control_characters_are_rejected() {
        let c = Client::new();
        for u in [
            "http://example.com/a\r\nx-injected: 1",
            "http://example.com/a b",
            "http://exa\nmple.com/",
            "http://example.com/\u{7f}",
        ] {
            assert_eq!(c.resolve(u).unwrap_err().kind(), ErrorKind::Url, "{u:?}");
        }
        let kind = |r: RequestBuilder| r.error.map(|e| e.kind());
        assert_eq!(kind(c.get("http://x/").header("a", "b\tc é")), None);
        assert_eq!(
            kind(c.get("http://x/").header("a", "b\r\nx: y")),
            Some(ErrorKind::Encode)
        );
        assert_eq!(
            kind(c.get("http://x/").header("a b", "c")),
            Some(ErrorKind::Encode)
        );
        assert_eq!(
            kind(c.get("http://x/").header("", "c")),
            Some(ErrorKind::Encode)
        );
        assert_eq!(
            kind(c.get("http://x/").header("a", "\0")),
            Some(ErrorKind::Encode)
        );
        assert_eq!(
            kind(c.request("GET /x HTTP/1.1\r\n", "http://x/")),
            Some(ErrorKind::Encode)
        );
        assert_eq!(kind(c.request("PROPFIND", "http://x/")), None);
        assert_eq!(
            Client::builder()
                .default_header("x", "a\nb")
                .build()
                .unwrap_err()
                .kind(),
            ErrorKind::Config
        );
    }

    // B-198: チャンクの大きさのあふれで panic しない
    #[test]
    fn chunk_sizes_do_not_overflow() {
        assert_eq!(parse_chunk_size(b"1a"), Some(26));
        assert_eq!(parse_chunk_size(b"+1"), None);
        assert_eq!(parse_chunk_size(b""), None);
        assert_eq!(parse_chunk_size(b"-1"), None);
        assert_eq!(parse_chunk_size(b"ffffffffffffffff"), Some(usize::MAX));
        assert_eq!(parse_chunk_size(b"1ffffffffffffffff"), None);
        assert_eq!(chunk_need(0, 10, 100), Some(12));
        assert_eq!(chunk_need(1, usize::MAX, 100), None);
        assert_eq!(chunk_need(0, usize::MAX, usize::MAX), None);
        assert_eq!(chunk_need(0, usize::MAX - 1, usize::MAX), None);
        assert_eq!(chunk_need(90, 11, 100), None);
    }

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64_std(b""), "");
        assert_eq!(base64_std(b"f"), "Zg==");
        assert_eq!(base64_std(b"fo"), "Zm8=");
        assert_eq!(base64_std(b"foo"), "Zm9v");
        assert_eq!(base64_std(b"user:pass"), "dXNlcjpwYXNz");
    }

    #[test]
    fn h2c_only_with_http2_on() {
        let c = Client::builder().http2(Http2::On).build().unwrap();
        let (o, _) = c.resolve("http://127.0.0.1:1/").unwrap();
        assert!(o.target.use_h2c);
        let c = Client::new();
        let (o, _) = c.resolve("http://127.0.0.1:1/").unwrap();
        assert!(!o.target.use_h2c);
    }
}
