//! ハンドラに渡すリクエスト（F-188）。
//!
//! ヘッダ・本文・パスは受信バッファ由来の `Bytes`（参照カウントのみ）で持ち、プロトコル
//! （HTTP/1.1・HTTP/2・HTTP/3）に依存しない形でハンドラへ渡す。

use super::pattern::{percent_decode, PathParams};
use bytes::Bytes;
use std::borrow::Cow;
use std::net::IpAddr;

/// リクエストを受けたプロトコル。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpVersion {
    Http11,
    Http2,
    Http3,
}

impl HttpVersion {
    pub fn as_str(&self) -> &'static str {
        match self {
            HttpVersion::Http11 => "HTTP/1.1",
            HttpVersion::Http2 => "HTTP/2",
            HttpVersion::Http3 => "HTTP/3",
        }
    }
}

/// ハンドラに渡すリクエスト。`clone()` は参照カウントの増分だけ（本文・ヘッダはコピーしない）。
#[derive(Debug, Clone)]
pub struct Request {
    pub(crate) method: Bytes,
    /// クエリを除いたパス。
    pub(crate) path: Bytes,
    /// `?` の後ろ（無ければ空）。
    pub(crate) query: Bytes,
    pub(crate) headers: std::sync::Arc<Vec<(Bytes, Bytes)>>,
    pub(crate) body: Bytes,
    pub(crate) params: PathParams,
    pub(crate) client_ip: IpAddr,
    pub(crate) version: HttpVersion,
}

impl Request {
    /// テスト・アダプタ（tower / C / Python）用の構築関数。`target` はクエリを含んでよい。
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
            headers: std::sync::Arc::new(headers),
            body: body.into(),
            params: Vec::new(),
            client_ip: IpAddr::from([127, 0, 0, 1]),
            version: HttpVersion::Http11,
        }
    }

    /// パスパラメータを差し替えた複製（テスト用）。
    pub fn with_params(mut self, params: PathParams) -> Request {
        self.params = params;
        self
    }

    /// 送信元 IP を差し替えた複製。
    pub fn with_client_ip(mut self, ip: IpAddr) -> Request {
        self.client_ip = ip;
        self
    }

    /// メソッド（`GET` 等）。
    pub fn method(&self) -> &str {
        std::str::from_utf8(&self.method).unwrap_or("")
    }

    /// クエリを除いたパス（パーセントエンコードのまま）。
    pub fn path(&self) -> &str {
        std::str::from_utf8(&self.path).unwrap_or("/")
    }

    /// クエリ文字列（`?` の後ろ。無ければ空文字）。
    pub fn query(&self) -> &str {
        std::str::from_utf8(&self.query).unwrap_or("")
    }

    /// クエリパラメータ（最初に出現した値、`application/x-www-form-urlencoded` として復号）。
    pub fn query_param(&self, name: &str) -> Option<Cow<'_, str>> {
        form_lookup(&self.query, name)
    }

    /// ヘッダ（大文字小文字を区別しない、最初の値）。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
            .and_then(|(_, v)| std::str::from_utf8(v).ok())
    }

    /// 全ヘッダ（受信順、名前は受信したままの大文字小文字）。
    pub fn headers(&self) -> &[(Bytes, Bytes)] {
        &self.headers
    }

    /// 本文。
    pub fn body(&self) -> &Bytes {
        &self.body
    }

    /// 本文を UTF-8 として（不正なバイトは置換）。
    pub fn text(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }

    /// 本文を JSON としてデシリアライズする。
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }

    /// パスパラメータ（`{name}`、パーセント復号済み）。
    pub fn param(&self, name: &str) -> Option<Cow<'_, str>> {
        self.params
            .iter()
            .find(|(n, _)| &**n == name)
            .map(|(_, v)| percent_decode(v))
    }

    /// パスパラメータを `FromStr` で変換する。
    pub fn param_as<T: std::str::FromStr>(&self, name: &str) -> Option<Result<T, T::Err>> {
        self.param(name).map(|v| v.parse::<T>())
    }

    /// 全パスパラメータ（名前, 生の値）。
    pub fn params(&self) -> &PathParams {
        &self.params
    }

    /// 送信元 IP。
    pub fn client_ip(&self) -> IpAddr {
        self.client_ip
    }

    /// 受信プロトコル。
    pub fn version(&self) -> HttpVersion {
        self.version
    }

    /// `App::state` で登録した共有状態。
    pub fn state<T: Send + Sync + 'static>(&self) -> Option<std::sync::Arc<T>> {
        super::app::state::<T>()
    }
}

/// `target`（パス + クエリ）を分割する（`Bytes` のスライスのみ）。
pub(crate) fn split_target(target: &Bytes) -> (Bytes, Bytes) {
    match target.iter().position(|&b| b == b'?') {
        Some(i) => (target.slice(..i), target.slice(i + 1..)),
        None => (target.clone(), Bytes::new()),
    }
}

/// `a=1&b=2` 形式から `name` の最初の値を探して復号する。
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

/// `application/x-www-form-urlencoded` の 1 要素を復号する（`+` は空白）。
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
