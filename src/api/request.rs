// F-188
//! The request passed to handlers.
//!
//! Headers, body and path are held as `Bytes` taken from the receive buffer (reference counting only) and are
//! passed to the handler in a form independent of the protocol (HTTP/1.1, HTTP/2 or HTTP/3).

use super::pattern::{percent_decode, PathParams};
use bytes::Bytes;
use std::borrow::Cow;
use std::net::IpAddr;

/// The protocol on which the request was received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpVersion {
    /// HTTP/1.1
    Http11,
    /// HTTP/2
    Http2,
    /// HTTP/3
    Http3,
}

impl HttpVersion {
    /// One of `"HTTP/1.1"`, `"HTTP/2"` or `"HTTP/3"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            HttpVersion::Http11 => "HTTP/1.1",
            HttpVersion::Http2 => "HTTP/2",
            HttpVersion::Http3 => "HTTP/3",
        }
    }
}

/// The request passed to a handler. `clone()` only increments reference counts (body and headers are not copied).
#[derive(Debug, Clone)]
pub struct Request {
    pub(crate) method: Bytes,
    /// The path without the query.
    pub(crate) path: Bytes,
    /// What follows `?` (empty if absent).
    pub(crate) query: Bytes,
    // F-202: 要求ごとの `Arc` を作らない。`Headers` 抽出子が使われたときだけ包む。
    /// The headers as received.
    pub(crate) headers: Vec<(Bytes, Bytes)>,
    pub(crate) body: Bytes,
    pub(crate) params: PathParams,
    pub(crate) client_ip: IpAddr,
    pub(crate) version: HttpVersion,
    // F-197
    /// The body when received as a stream (for a `streaming_request` route over HTTP/2).
    pub(crate) body_stream: Option<super::stream::BodyStreamSlot>,
}

impl Request {
    /// Constructor for tests and adapters (tower, C, Python). `target` may include the query.
    pub fn new(
        method: impl Into<Bytes>,
        target: impl Into<Bytes>,
        headers: Vec<(Bytes, Bytes)>,
        body: impl Into<Bytes>,
    ) -> Request {
        let target: Bytes = target.into();
        let (path, query) = split_target(&target);
        Request {
            method: method.into(),
            path,
            query,
            headers,
            body: body.into(),
            params: Vec::new(),
            client_ip: IpAddr::from([127, 0, 0, 1]),
            version: HttpVersion::Http11,
            body_stream: None,
        }
    }

    /// A copy with the path parameters replaced (for tests).
    pub fn with_params(mut self, params: PathParams) -> Request {
        self.params = params;
        self
    }

    /// A copy with the client IP replaced.
    pub fn with_client_ip(mut self, ip: IpAddr) -> Request {
        self.client_ip = ip;
        self
    }

    /// The method (such as `GET`).
    pub fn method(&self) -> &str {
        std::str::from_utf8(&self.method).unwrap_or("")
    }

    /// The path without the query (still percent-encoded).
    pub fn path(&self) -> &str {
        std::str::from_utf8(&self.path).unwrap_or("/")
    }

    /// The query string (what follows `?`; empty if absent).
    pub fn query(&self) -> &str {
        std::str::from_utf8(&self.query).unwrap_or("")
    }

    /// A query parameter (the first occurrence, decoded as `application/x-www-form-urlencoded`).
    pub fn query_param(&self, name: &str) -> Option<Cow<'_, str>> {
        form_lookup(&self.query, name)
    }

    /// A header (case-insensitive; the first value).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
            .and_then(|(_, v)| std::str::from_utf8(v).ok())
    }

    /// All headers, in the order received, with names in the case received.
    pub fn headers(&self) -> &[(Bytes, Bytes)] {
        &self.headers
    }

    // F-197
    /// Receives the body as chunks in arrival order (can be taken only once).
    ///
    /// On a route with streaming reception (HTTP/2), the received DATA is returned in order. Otherwise the whole body
    /// is returned as one chunk (zero chunks if it is empty).
    pub fn take_body_stream(&self) -> super::stream::RequestBodyStream {
        if let Some(slot) = &self.body_stream {
            if let Some(s) = slot.lock().unwrap().take() {
                return s;
            }
        }
        let (mut tx, slot) = super::stream::request_body_channel(1);
        if !self.body.is_empty() {
            let _ = tx.try_send(self.body.clone());
        }
        drop(tx);
        let s = slot.lock().unwrap().take().expect("fresh slot");
        s
    }

    /// Whether this is a streaming-reception route (the body arrives through `take_body_stream`).
    pub fn is_body_streaming(&self) -> bool {
        self.body_stream.is_some()
    }

    /// The body.
    pub fn body(&self) -> &Bytes {
        &self.body
    }

    /// The body as UTF-8 (invalid bytes are replaced).
    pub fn text(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }

    /// Deserializes the body as JSON.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }

    /// A path parameter (`{name}`, percent-decoded).
    pub fn param(&self, name: &str) -> Option<Cow<'_, str>> {
        self.params
            .iter()
            .find(|(n, _)| &**n == name)
            .map(|(_, v)| percent_decode(v))
    }

    /// Converts a path parameter with `FromStr`.
    pub fn param_as<T: std::str::FromStr>(&self, name: &str) -> Option<Result<T, T::Err>> {
        self.param(name).map(|v| v.parse::<T>())
    }

    /// All path parameters (name, raw value).
    pub fn params(&self) -> &PathParams {
        &self.params
    }

    /// The client IP.
    pub fn client_ip(&self) -> IpAddr {
        self.client_ip
    }

    /// The protocol of the request.
    pub fn version(&self) -> HttpVersion {
        self.version
    }

    // F-261
    /// The `Upgrade` header when the request asks for a protocol upgrade (`Connection` lists `upgrade` and
    /// `Upgrade` is present). Always `None` on HTTP/2 and HTTP/3, which have no upgrades.
    pub fn upgrade_protocol(&self) -> Option<&[u8]> {
        if self.version != HttpVersion::Http11 {
            return None;
        }
        let mut wants = false;
        let mut proto = None;
        for (n, v) in &self.headers {
            if n.eq_ignore_ascii_case(b"connection") {
                wants |= v
                    .split(|&b| b == b',')
                    .any(|t| t.trim_ascii().eq_ignore_ascii_case(b"upgrade"));
            } else if n.eq_ignore_ascii_case(b"upgrade") && proto.is_none() {
                proto = Some(v.as_ref());
            }
        }
        if wants {
            proto
        } else {
            None
        }
    }

    /// Shared state registered with `App::state`.
    pub fn state<T: Send + Sync + 'static>(&self) -> Option<std::sync::Arc<T>> {
        super::app::state::<T>()
    }
}

/// Splits `target` (path plus query) into its two parts (only slices of `Bytes`).
pub(crate) fn split_target(target: &Bytes) -> (Bytes, Bytes) {
    match target.iter().position(|&b| b == b'?') {
        Some(i) => (target.slice(..i), target.slice(i + 1..)),
        None => (target.clone(), Bytes::new()),
    }
}

/// Finds the first value of `name` in the `a=1&b=2` form and decodes it.
pub(crate) fn form_lookup<'a>(data: &'a [u8], name: &str) -> Option<Cow<'a, str>> {
    for pair in data.split(|&b| b == b'&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.iter().position(|&b| b == b'=') {
            Some(i) => (&pair[..i], &pair[i + 1..]),
            None => (pair, &b""[..]),
        };
        if form_decode(k) == name {
            return Some(form_decode(v));
        }
    }
    None
}

/// Decodes one `application/x-www-form-urlencoded` element (`+` is a space).
pub(crate) fn form_decode(input: &[u8]) -> Cow<'_, str> {
    if !input.contains(&b'+') {
        return percent_decode(input);
    }
    let replaced: Vec<u8> = input
        .iter()
        .map(|&b| if b == b'+' { b' ' } else { b })
        .collect();
    Cow::Owned(percent_decode(&replaced).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessors() {
        let req = Request::new(
            "POST",
            "/users/42?name=a+b&x=%41&flag",
            vec![(
                Bytes::from_static(b"Content-Type"),
                Bytes::from_static(b"application/json"),
            )],
            "{\"a\":1}",
        )
        .with_params(vec![(
            std::sync::Arc::from("id"),
            Bytes::from_static(b"4%202"),
        )]);
        assert_eq!(req.method(), "POST");
        assert_eq!(req.path(), "/users/42");
        assert_eq!(req.query(), "name=a+b&x=%41&flag");
        assert_eq!(req.query_param("name").as_deref(), Some("a b"));
        assert_eq!(req.query_param("x").as_deref(), Some("A"));
        assert_eq!(req.query_param("flag").as_deref(), Some(""));
        assert_eq!(req.query_param("missing"), None);
        assert_eq!(req.header("content-type"), Some("application/json"));
        assert_eq!(req.param("id").as_deref(), Some("4 2"));
        let v: serde_json::Value = req.json().unwrap();
        assert_eq!(v["a"], 1);
    }
}
