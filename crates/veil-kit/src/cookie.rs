// F-267: Cookie（RFC 6265bis）。要求の Cookie ヘッダを 1 回走査し、値は Bytes の切り出し（コピーしない）。
//! Cookies: the [`CookieJar`] extractor, the [`Cookie`] builder for `Set-Cookie`, and signed
//! ([`SignedCookieJar`], HMAC-SHA256) and private ([`PrivateCookieJar`], AES-256-GCM) cookies.
//!
//! ```no_run
//! use veil::prelude::*;
//! use veil_kit::cookie::{Cookie, CookieJar, ResponseCookieExt, SameSite};
//!
//! #[veil::route(path = "/visit", method = "GET")]
//! async fn visit(jar: CookieJar) -> Response {
//!     let n: u32 = jar.get("visits").and_then(|v| v.parse().ok()).unwrap_or(0);
//!     Response::text(format!("visit {}", n + 1))
//!         .with_cookie(Cookie::new("visits", (n + 1).to_string()).http_only(true).same_site(SameSite::Lax))
//! }
//! ```
//!
//! Signed and private jars need a [`Key`] registered with `App::state(key)`.

use bytes::Bytes;
use std::borrow::Cow;
use std::time::Duration;
use veil::api::{FromRequest, Request, Response};

/// The `SameSite` attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    /// `SameSite=Strict`.
    Strict,
    /// `SameSite=Lax`.
    Lax,
    /// `SameSite=None` (requires `Secure`; [`Cookie::same_site`] sets it).
    None,
}

/// A cookie to send in `Set-Cookie`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cookie {
    name: String,
    value: String,
    path: Option<String>,
    domain: Option<String>,
    max_age: Option<i64>,
    secure: bool,
    http_only: bool,
    same_site: Option<SameSite>,
    partitioned: bool,
}

/// Why a cookie was rejected (RFC 6265bis section 4.1.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookieError(String);

impl std::fmt::Display for CookieError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CookieError {}

/// `token` of RFC 9110 (the cookie name).
fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// `cookie-octet` (the cookie value; a pair of surrounding double quotes is allowed).
fn is_cookie_value(s: &str) -> bool {
    let s = s
        .strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .unwrap_or(s);
    s.bytes().all(|b| {
        b == 0x21
            || (0x23..=0x2b).contains(&b)
            || (0x2d..=0x3a).contains(&b)
            || (0x3c..=0x5b).contains(&b)
            || (0x5d..=0x7e).contains(&b)
    })
}

/// An attribute value (no control characters and no `;`).
fn is_attr_value(s: &str) -> bool {
    s.bytes().all(|b| (0x20..0x7f).contains(&b) && b != b';')
}

impl Cookie {
    /// A cookie with `Path=/` (checked when it is turned into a header; see [`Cookie::to_header`]).
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Cookie {
        Cookie {
            name: name.into(),
            value: value.into(),
            path: Some("/".into()),
            domain: None,
            max_age: None,
            secure: false,
            http_only: false,
            same_site: None,
            partitioned: false,
        }
    }

    /// A cookie that deletes `name` (empty value, `Max-Age=0`, `Path=/`).
    pub fn removal(name: impl Into<String>) -> Cookie {
        Cookie::new(name, "").max_age(Duration::ZERO)
    }

    /// The name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The value.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// `Path` (`None` removes it).
    pub fn path(mut self, p: Option<&str>) -> Cookie {
        self.path = p.map(str::to_string);
        self
    }

    /// `Domain`.
    pub fn domain(mut self, d: impl Into<String>) -> Cookie {
        self.domain = Some(d.into());
        self
    }

    /// `Max-Age` (seconds; zero deletes the cookie).
    pub fn max_age(mut self, d: Duration) -> Cookie {
        self.max_age = Some(d.as_secs().min(400 * 24 * 3600) as i64);
        self
    }

    /// `Secure`.
    pub fn secure(mut self, on: bool) -> Cookie {
        self.secure = on;
        self
    }

    /// `HttpOnly`.
    pub fn http_only(mut self, on: bool) -> Cookie {
        self.http_only = on;
        self
    }

    /// `SameSite` (`None` also sets `Secure`, which browsers require).
    pub fn same_site(mut self, s: SameSite) -> Cookie {
        if s == SameSite::None {
            self.secure = true;
        }
        self.same_site = Some(s);
        self
    }

    /// `Partitioned` (CHIPS; also sets `Secure`).
    pub fn partitioned(mut self, on: bool) -> Cookie {
        self.partitioned = on;
        if on {
            self.secure = true;
        }
        self
    }

    /// The `Set-Cookie` value, after checking the name, value and attributes (RFC 6265bis section 4.1.1, and the
    /// `__Secure-` / `__Host-` prefixes).
    pub fn to_header(&self) -> Result<String, CookieError> {
        if !is_token(&self.name) {
            return Err(CookieError(format!("invalid cookie name '{}'", self.name)));
        }
        if !is_cookie_value(&self.value) {
            return Err(CookieError(format!(
                "invalid value for cookie '{}' (use only printable ASCII without spaces, '\"', ',', ';' and '\\')",
                self.name
            )));
        }
        if self.name.len() + self.value.len() > 4096 {
            return Err(CookieError(format!(
                "cookie '{}' is larger than 4096 bytes",
                self.name
            )));
        }
        let lower = self.name.to_ascii_lowercase();
        if lower.starts_with("__secure-") && !self.secure {
            return Err(CookieError(format!("cookie '{}' needs Secure", self.name)));
        }
        if lower.starts_with("__host-")
            && (!self.secure || self.domain.is_some() || self.path.as_deref() != Some("/"))
        {
            return Err(CookieError(format!(
                "cookie '{}' needs Secure, Path=/ and no Domain",
                self.name
            )));
        }
        let mut s = String::with_capacity(self.name.len() + self.value.len() + 64);
        s.push_str(&self.name);
        s.push('=');
        s.push_str(&self.value);
        if let Some(p) = &self.path {
            if !is_attr_value(p) {
                return Err(CookieError("invalid Path attribute".into()));
            }
            s.push_str("; Path=");
            s.push_str(p);
        }
        if let Some(d) = &self.domain {
            if !is_attr_value(d) || d.is_empty() {
                return Err(CookieError("invalid Domain attribute".into()));
            }
            s.push_str("; Domain=");
            s.push_str(d);
        }
        if let Some(m) = self.max_age {
            s.push_str("; Max-Age=");
            s.push_str(&m.to_string());
        }
        if self.secure {
            s.push_str("; Secure");
        }
        if self.http_only {
            s.push_str("; HttpOnly");
        }
        match self.same_site {
            Some(SameSite::Strict) => s.push_str("; SameSite=Strict"),
            Some(SameSite::Lax) => s.push_str("; SameSite=Lax"),
            Some(SameSite::None) => s.push_str("; SameSite=None"),
            None => {}
        }
        if self.partitioned {
            s.push_str("; Partitioned");
        }
        Ok(s)
    }
}

/// Adds `Set-Cookie` to a [`Response`].
pub trait ResponseCookieExt {
    /// Adds the cookie. An invalid cookie is not sent (it is logged as an error).
    fn with_cookie(self, cookie: Cookie) -> Response;
    /// Deletes a cookie in the browser (`Max-Age=0`, `Path=/`).
    fn remove_cookie(self, name: &str) -> Response;
}

impl ResponseCookieExt for Response {
    fn with_cookie(self, cookie: Cookie) -> Response {
        match cookie.to_header() {
            Ok(h) => self.with_header("set-cookie", h),
            Err(e) => {
                ftlog::error!("not sending a cookie: {}", e);
                self
            }
        }
    }

    fn remove_cookie(self, name: &str) -> Response {
        self.with_cookie(Cookie::removal(name))
    }
}

// ===================== 要求の Cookie =====================

/// The cookies of a request (from every `Cookie` header; the values are slices of the header, not copies).
#[derive(Debug, Clone, Default)]
pub struct CookieJar {
    pairs: Vec<(Bytes, Bytes)>,
}

impl CookieJar {
    /// Parses the `Cookie` headers of `req`.
    pub fn from_request_headers(req: &Request) -> CookieJar {
        let mut pairs = Vec::new();
        for (n, v) in req.headers() {
            if !n.eq_ignore_ascii_case(b"cookie") {
                continue;
            }
            let mut start = 0;
            let len = v.len();
            while start < len {
                let end = v[start..]
                    .iter()
                    .position(|&b| b == b';')
                    .map(|i| start + i)
                    .unwrap_or(len);
                let part = trim_range(v, start, end);
                if let Some(eq) = v[part.0..part.1].iter().position(|&b| b == b'=') {
                    let (ns, ne) = trim_range(v, part.0, part.0 + eq);
                    let (vs, mut ve) = trim_range(v, part.0 + eq + 1, part.1);
                    let mut vs = vs;
                    if ve - vs >= 2 && v[vs] == b'"' && v[ve - 1] == b'"' {
                        vs += 1;
                        ve -= 1;
                    }
                    if ne > ns {
                        pairs.push((v.slice(ns..ne), v.slice(vs..ve)));
                    }
                }
                start = end + 1;
            }
        }
        CookieJar { pairs }
    }

    /// The value of the first cookie named `name`.
    pub fn get(&self, name: &str) -> Option<Cow<'_, str>> {
        self.get_bytes(name).map(|v| String::from_utf8_lossy(v))
    }

    /// The raw value of the first cookie named `name`.
    pub fn get_bytes(&self, name: &str) -> Option<&Bytes> {
        self.pairs
            .iter()
            .find(|(n, _)| &n[..] == name.as_bytes())
            .map(|(_, v)| v)
    }

    /// All cookies (name, value) in order.
    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        self.pairs.iter().map(|(n, v)| (&n[..], &v[..]))
    }

    /// The number of cookies.
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    /// Whether the request has no cookies.
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }
}

fn trim_range(v: &[u8], mut s: usize, mut e: usize) -> (usize, usize) {
    while s < e && (v[s] == b' ' || v[s] == b'\t') {
        s += 1;
    }
    while e > s && (v[e - 1] == b' ' || v[e - 1] == b'\t') {
        e -= 1;
    }
    (s, e)
}

impl FromRequest for CookieJar {
    fn from_request(req: &Request) -> Result<Self, Response> {
        Ok(CookieJar::from_request_headers(req))
    }
}

// ===================== 署名・暗号化 =====================

/// The secret of signed and private cookies: 32 bytes for HMAC-SHA256 and 32 bytes for AES-256-GCM. Old keys
/// added with [`Key::with_fallback`] are still accepted when reading (key rotation); new cookies use the current key.
#[derive(Clone)]
pub struct Key {
    sign: [u8; 32],
    enc: [u8; 32],
    fallback: Vec<([u8; 32], [u8; 32])>,
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(..)")
    }
}

impl Key {
    /// A key from 64 bytes of secret material (the first half signs, the second half encrypts).
    pub fn from(material: &[u8; 64]) -> Key {
        let mut sign = [0u8; 32];
        let mut enc = [0u8; 32];
        sign.copy_from_slice(&material[..32]);
        enc.copy_from_slice(&material[32..]);
        Key {
            sign,
            enc,
            fallback: Vec::new(),
        }
    }

    /// Derives a key from a secret of any length (at least 32 bytes, for example from an environment variable).
    pub fn derive_from(secret: &[u8]) -> Result<Key, CookieError> {
        if secret.len() < 32 {
            return Err(CookieError(
                "the cookie secret must be at least 32 bytes".into(),
            ));
        }
        let sign = crate::crypto::hmac_sha256(secret, &[b"veil-kit cookie signing"]);
        let enc = crate::crypto::hmac_sha256(secret, &[b"veil-kit cookie encryption"]);
        Ok(Key {
            sign,
            enc,
            fallback: Vec::new(),
        })
    }

    /// A random key (cookies do not survive a restart; use [`Key::from`] in production).
    pub fn generate() -> Key {
        let mut m = [0u8; 64];
        crate::crypto::random(&mut m);
        Key::from(&m)
    }

    /// Also accepts cookies made with `old` (rotation).
    pub fn with_fallback(mut self, old: &Key) -> Key {
        self.fallback.push((old.sign, old.enc));
        self
    }

    fn sign_keys(&self) -> impl Iterator<Item = &[u8; 32]> {
        std::iter::once(&self.sign).chain(self.fallback.iter().map(|k| &k.0))
    }

    fn enc_keys(&self) -> impl Iterator<Item = &[u8; 32]> {
        std::iter::once(&self.enc).chain(self.fallback.iter().map(|k| &k.1))
    }

    /// `base64url(HMAC(name=value)).value`.
    pub(crate) fn sign_value(&self, name: &str, value: &str) -> String {
        let mac =
            crate::crypto::hmac_sha256(&self.sign, &[name.as_bytes(), b"=", value.as_bytes()]);
        let mut s = crate::b64::encode(&mac);
        s.push('.');
        s.push_str(value);
        s
    }

    pub(crate) fn verify_value(&self, name: &str, signed: &[u8]) -> Option<String> {
        let dot = signed.iter().position(|&b| b == b'.')?;
        let mac = crate::b64::decode(&signed[..dot])?;
        let value = &signed[dot + 1..];
        let ok = self.sign_keys().any(|k| {
            let m = crate::crypto::hmac_sha256(k, &[name.as_bytes(), b"=", value]);
            crate::crypto::ct_eq(&m, &mac)
        });
        if !ok {
            return None;
        }
        String::from_utf8(value.to_vec()).ok()
    }

    /// `base64url(nonce || AES-256-GCM(value))`, with the name as associated data.
    pub(crate) fn encrypt_value(&self, name: &str, value: &[u8]) -> String {
        crate::b64::encode(&crate::crypto::seal(&self.enc, name.as_bytes(), value))
    }

    pub(crate) fn decrypt_value(&self, name: &str, sealed: &[u8]) -> Option<Vec<u8>> {
        let raw = crate::b64::decode(sealed)?;
        self.enc_keys()
            .find_map(|k| crate::crypto::open(k, name.as_bytes(), &raw))
    }
}

fn key_of(req: &Request) -> Result<std::sync::Arc<Key>, Response> {
    req.state::<Key>().ok_or_else(|| {
        ftlog::error!(
            "signed or private cookies need a veil_kit::cookie::Key registered with App::state"
        );
        Response::error(500, "Internal Server Error")
    })
}

/// Cookies whose values are signed (the browser can read them but not change them).
#[derive(Debug, Clone)]
pub struct SignedCookieJar {
    jar: CookieJar,
    key: std::sync::Arc<Key>,
}

impl SignedCookieJar {
    /// The verified value (`None` when the cookie is missing or its signature does not match).
    pub fn get(&self, name: &str) -> Option<String> {
        self.key.verify_value(name, self.jar.get_bytes(name)?)
    }

    /// A signed cookie to send with [`ResponseCookieExt::with_cookie`].
    pub fn cookie(&self, name: &str, value: &str) -> Cookie {
        Cookie::new(name, self.key.sign_value(name, value))
    }
}

impl FromRequest for SignedCookieJar {
    fn from_request(req: &Request) -> Result<Self, Response> {
        Ok(SignedCookieJar {
            jar: CookieJar::from_request_headers(req),
            key: key_of(req)?,
        })
    }
}

/// Cookies whose values are encrypted and authenticated (the browser can neither read nor change them).
#[derive(Debug, Clone)]
pub struct PrivateCookieJar {
    jar: CookieJar,
    key: std::sync::Arc<Key>,
}

impl PrivateCookieJar {
    /// The decrypted value (`None` when the cookie is missing, altered or not UTF-8).
    pub fn get(&self, name: &str) -> Option<String> {
        String::from_utf8(self.key.decrypt_value(name, self.jar.get_bytes(name)?)?).ok()
    }

    /// An encrypted cookie to send with [`ResponseCookieExt::with_cookie`].
    pub fn cookie(&self, name: &str, value: &str) -> Cookie {
        Cookie::new(name, self.key.encrypt_value(name, value.as_bytes()))
    }
}

impl FromRequest for PrivateCookieJar {
    fn from_request(req: &Request) -> Result<Self, Response> {
        Ok(PrivateCookieJar {
            jar: CookieJar::from_request_headers(req),
            key: key_of(req)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(cookie: &str) -> Request {
        Request::new(
            "GET",
            "/",
            vec![(
                Bytes::from_static(b"cookie"),
                Bytes::from(cookie.to_string()),
            )],
            Bytes::new(),
        )
    }

    #[test]
    fn parses_the_cookie_header() {
        let jar = CookieJar::from_request_headers(&req("a=1; b = two ;c=\"q\"; empty=; =x; d=x=y"));
        assert_eq!(jar.get("a").as_deref(), Some("1"));
        assert_eq!(jar.get("b").as_deref(), Some("two"));
        assert_eq!(jar.get("c").as_deref(), Some("q"));
        assert_eq!(jar.get("empty").as_deref(), Some(""));
        assert_eq!(jar.get("d").as_deref(), Some("x=y"));
        assert_eq!(jar.len(), 5);
        assert!(jar.get("missing").is_none());
    }

    #[test]
    fn set_cookie_follows_rfc6265bis() {
        let c = Cookie::new("sid", "abc")
            .http_only(true)
            .same_site(SameSite::Lax)
            .max_age(Duration::from_secs(60));
        assert_eq!(
            c.to_header().unwrap(),
            "sid=abc; Path=/; Max-Age=60; HttpOnly; SameSite=Lax"
        );
        let n = Cookie::new("x", "1").same_site(SameSite::None);
        assert!(n.to_header().unwrap().contains("; Secure"));
        assert!(Cookie::new("bad name", "v").to_header().is_err());
        assert!(Cookie::new("n", "a;b").to_header().is_err());
        assert!(Cookie::new("n", "a b").to_header().is_err());
        assert!(Cookie::new("n", "a\r\nSet-Cookie: x").to_header().is_err());
        assert!(Cookie::new("__Secure-a", "1").to_header().is_err());
        assert!(Cookie::new("__Host-a", "1")
            .secure(true)
            .domain("example.com")
            .to_header()
            .is_err());
        assert!(Cookie::new("__Host-a", "1")
            .secure(true)
            .to_header()
            .is_ok());
        assert_eq!(
            Cookie::removal("sid").to_header().unwrap(),
            "sid=; Path=/; Max-Age=0"
        );
        // Max-Age は 400 日で頭打ち（RFC 6265bis）
        let long = Cookie::new("a", "1").max_age(Duration::from_secs(10 * 365 * 24 * 3600));
        assert!(long.to_header().unwrap().contains("Max-Age=34560000"));
    }

    #[test]
    fn signed_cookies_detect_tampering() {
        let key = Key::from(&[7u8; 64]);
        let v = key.sign_value("user", "alice");
        assert_eq!(
            key.verify_value("user", v.as_bytes()).as_deref(),
            Some("alice")
        );
        let tampered = v.replace("alice", "admin");
        assert!(key.verify_value("user", tampered.as_bytes()).is_none());
        // 名前を変えて別の Cookie として使い回せない
        assert!(key.verify_value("other", v.as_bytes()).is_none());
        // 鍵の入れ替え: 古い鍵の Cookie も読める
        let new = Key::from(&[9u8; 64]).with_fallback(&key);
        assert_eq!(
            new.verify_value("user", v.as_bytes()).as_deref(),
            Some("alice")
        );
        assert!(Key::from(&[9u8; 64])
            .verify_value("user", v.as_bytes())
            .is_none());
    }

    #[test]
    fn private_cookies_roundtrip_and_reject_changes() {
        let key = Key::generate();
        let sealed = key.encrypt_value("s", b"secret");
        assert!(!sealed.contains("secret"));
        assert_eq!(
            key.decrypt_value("s", sealed.as_bytes()).unwrap(),
            b"secret"
        );
        assert!(
            key.decrypt_value("t", sealed.as_bytes()).is_none(),
            "the name is authenticated"
        );
        let mut raw = crate::b64::decode(sealed.as_bytes()).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 1;
        assert!(key
            .decrypt_value("s", crate::b64::encode(&raw).as_bytes())
            .is_none());
        assert!(Key::generate()
            .decrypt_value("s", sealed.as_bytes())
            .is_none());
        assert!(Key::derive_from(b"short").is_err());
        assert!(Key::derive_from(&[1u8; 32]).is_ok());
    }
}
