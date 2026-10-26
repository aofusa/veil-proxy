// F-267: 認証・認可。主体（Principal）は要求の拡張領域（Request::extensions）に置き、ハンドラは Auth<T> で受ける。
//! Authentication and authorization.
//!
//! - Credential extractors: [`BasicAuth`], [`BearerToken`], [`ApiKey`], and `Jwt<Claims>` (`jwt` feature).
//! - [`authenticate`] middleware turns credentials into a principal (any type, usually [`Principal`]) stored in
//!   the request's extensions; handlers receive it with [`Auth<T>`].
//! - [`require`] middleware rejects requests without a principal (401) or whose principal fails a check (403).
//!
//! ```no_run
//! use veil::prelude::*;
//! use veil_kit::auth::{authenticate, require, Auth, BearerToken, Principal};
//!
//! #[veil::route(path = "/admin/stats", method = "GET")]
//! async fn stats(user: Auth<Principal>) -> String {
//!     format!("hello {}", user.subject)
//! }
//!
//! App::new()
//!     .wrap_on("/admin", require(|p: &Principal| p.has_scope("admin")))
//!     .wrap_on("/", authenticate(|req: &Request| {
//!         let token = BearerToken::from_headers(req)?;
//!         Ok((token.0 == "secret").then(|| Principal::new("admin").with_scopes(["admin"])))
//!     }))
//!     .run()
//!     .unwrap();
//! ```
//!
//! The middleware registered last with `wrap_on` runs first (it wraps the ones registered before it), so
//! register `require` before `authenticate`.

#[cfg(feature = "jwt")]
pub mod jwt;
#[cfg(feature = "jwt")]
pub use jwt::{Claims, Jwt, JwtKey, JwtVerifier};

use bytes::Bytes;
use std::marker::PhantomData;
use std::sync::Arc;
use veil::api::{FromRequest, HandlerFn, Request, Response};

fn unauthorized(scheme: &'static str) -> Response {
    Response::error(401, "Unauthorized").with_header("www-authenticate", scheme)
}

fn header<'a>(req: &'a Request, name: &[u8]) -> Option<&'a Bytes> {
    req.headers()
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
}

/// `Authorization: Basic` credentials (401 with `WWW-Authenticate: Basic` when missing or malformed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasicAuth {
    /// The user name.
    pub user: String,
    /// The password.
    pub password: String,
}

impl BasicAuth {
    /// Reads the credentials (for use outside an extractor, for example in [`authenticate`]).
    pub fn from_headers(req: &Request) -> Result<BasicAuth, Response> {
        let deny = || unauthorized("Basic realm=\"veil\", charset=\"UTF-8\"");
        let v = header(req, b"authorization").ok_or_else(deny)?;
        let rest = strip_scheme(v, b"basic").ok_or_else(deny)?;
        let raw = crate::b64::decode(rest.trim_ascii()).ok_or_else(deny)?;
        let s = String::from_utf8(raw).map_err(|_| deny())?;
        let (user, password) = s.split_once(':').ok_or_else(deny)?;
        Ok(BasicAuth {
            user: user.to_string(),
            password: password.to_string(),
        })
    }

    /// Compares with expected credentials in constant time.
    pub fn matches(&self, user: &str, password: &str) -> bool {
        // 両方を比べ終えてから判定する（どちらが違ったかを時間で漏らさない）
        let u = crate::crypto::ct_eq(self.user.as_bytes(), user.as_bytes());
        let p = crate::crypto::ct_eq(self.password.as_bytes(), password.as_bytes());
        u & p
    }
}

impl FromRequest for BasicAuth {
    fn from_request(req: &Request) -> Result<Self, Response> {
        BasicAuth::from_headers(req)
    }
}

fn strip_scheme<'a>(v: &'a [u8], scheme: &[u8]) -> Option<&'a [u8]> {
    if v.len() > scheme.len()
        && v[..scheme.len()].eq_ignore_ascii_case(scheme)
        && v[scheme.len()] == b' '
    {
        Some(&v[scheme.len() + 1..])
    } else {
        None
    }
}

/// `Authorization: Bearer <token>` (401 with `WWW-Authenticate: Bearer` when missing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BearerToken(pub String);

impl BearerToken {
    /// Reads the token (for use outside an extractor).
    pub fn from_headers(req: &Request) -> Result<BearerToken, Response> {
        let deny = || unauthorized("Bearer");
        let v = header(req, b"authorization").ok_or_else(deny)?;
        let t = strip_scheme(v, b"bearer").ok_or_else(deny)?.trim_ascii();
        if t.is_empty() {
            return Err(deny());
        }
        std::str::from_utf8(t)
            .map(|s| BearerToken(s.to_string()))
            .map_err(|_| deny())
    }
}

impl FromRequest for BearerToken {
    fn from_request(req: &Request) -> Result<Self, Response> {
        BearerToken::from_headers(req)
    }
}

/// The header an [`ApiKey`] is read from.
pub trait KeyHeader: Send + Sync + 'static {
    /// The header name (lower case).
    const NAME: &'static str;
}

/// `x-api-key` (the default header of [`ApiKey`]).
#[derive(Debug, Clone, Copy)]
pub struct XApiKey;

impl KeyHeader for XApiKey {
    const NAME: &'static str = "x-api-key";
}

/// An API key from a header (`x-api-key` by default; define a [`KeyHeader`] for another). 401 when missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKey<H: KeyHeader = XApiKey>(pub String, PhantomData<H>);

impl<H: KeyHeader> ApiKey<H> {
    /// The key.
    pub fn value(&self) -> &str {
        &self.0
    }

    /// Whether the key is one of `keys` (constant time for each candidate).
    pub fn is_one_of(&self, keys: &[&str]) -> bool {
        keys.iter().fold(false, |acc, k| {
            acc | crate::crypto::ct_eq(self.0.as_bytes(), k.as_bytes())
        })
    }
}

impl<H: KeyHeader> FromRequest for ApiKey<H> {
    fn from_request(req: &Request) -> Result<Self, Response> {
        match header(req, H::NAME.as_bytes()).map(|v| std::str::from_utf8(v)) {
            Some(Ok(v)) if !v.is_empty() => Ok(ApiKey(v.to_string(), PhantomData)),
            _ => Err(Response::error(401, "Unauthorized")),
        }
    }
}

/// An authenticated subject with scopes and roles.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Principal {
    /// The subject (user id, client id).
    pub subject: String,
    /// OAuth scopes.
    pub scopes: Vec<String>,
    /// Roles.
    pub roles: Vec<String>,
    /// Extra attributes (for example the JWT claims).
    pub attributes: serde_json::Value,
}

impl Principal {
    /// A principal without scopes or roles.
    pub fn new(subject: impl Into<String>) -> Principal {
        Principal {
            subject: subject.into(),
            ..Default::default()
        }
    }

    /// Adds scopes.
    pub fn with_scopes<S: Into<String>>(
        mut self,
        scopes: impl IntoIterator<Item = S>,
    ) -> Principal {
        self.scopes.extend(scopes.into_iter().map(Into::into));
        self
    }

    /// Adds roles.
    pub fn with_roles<S: Into<String>>(mut self, roles: impl IntoIterator<Item = S>) -> Principal {
        self.roles.extend(roles.into_iter().map(Into::into));
        self
    }

    /// Whether the principal has the scope.
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }

    /// Whether the principal has the role.
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }
}

/// The principal stored by [`authenticate`] (401 when the request has none).
#[derive(Debug)]
pub struct Auth<T: Send + Sync + 'static = Principal>(pub Arc<T>);

impl<T: Send + Sync + 'static> Clone for Auth<T> {
    fn clone(&self) -> Self {
        Auth(self.0.clone())
    }
}

impl<T: Send + Sync + 'static> std::ops::Deref for Auth<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: Send + Sync + 'static> FromRequest for Auth<T> {
    fn from_request(req: &Request) -> Result<Self, Response> {
        req.extensions()
            .get::<T>()
            .map(Auth)
            .ok_or_else(|| Response::error(401, "Unauthorized"))
    }
}

/// An optional principal (`None` for anonymous requests).
#[derive(Debug)]
pub struct MaybeAuth<T: Send + Sync + 'static = Principal>(pub Option<Arc<T>>);

impl<T: Send + Sync + 'static> FromRequest for MaybeAuth<T> {
    fn from_request(req: &Request) -> Result<Self, Response> {
        Ok(MaybeAuth(req.extensions().get::<T>()))
    }
}

/// Middleware that runs `f` on each request: `Ok(Some(p))` stores the principal, `Ok(None)` continues anonymously
/// (a later [`require`] or [`Auth`] rejects it), and `Err(response)` answers at once.
pub fn authenticate<T, F>(f: F) -> impl Fn(HandlerFn) -> HandlerFn + Send + Sync + 'static
where
    T: Send + Sync + 'static,
    F: Fn(&Request) -> Result<Option<T>, Response> + Send + Sync + 'static,
{
    let f = Arc::new(f);
    move |next: HandlerFn| -> HandlerFn {
        let f = f.clone();
        Arc::new(move |req: Request| {
            match f(&req) {
                Ok(Some(p)) => req.extensions().insert(p),
                Ok(None) => {}
                Err(r) => return Box::pin(std::future::ready(r)),
            }
            next(req)
        })
    }
}

/// Middleware that requires a principal of type `T` (401) for which `check` is true (403).
pub fn require<T, F>(check: F) -> impl Fn(HandlerFn) -> HandlerFn + Send + Sync + 'static
where
    T: Send + Sync + 'static,
    F: Fn(&T) -> bool + Send + Sync + 'static,
{
    let check = Arc::new(check);
    move |next: HandlerFn| -> HandlerFn {
        let check = check.clone();
        Arc::new(move |req: Request| match req.extensions().get::<T>() {
            None => Box::pin(std::future::ready(unauthorized("Bearer"))),
            Some(p) if !check(&p) => {
                Box::pin(std::future::ready(Response::error(403, "Forbidden")))
            }
            Some(_) => next(req),
        })
    }
}

/// The OAuth 2.0 password flow's bearer token (FastAPI's `OAuth2PasswordBearer`): the token from
/// `Authorization: Bearer`, plus the OpenAPI security scheme that documents where to get it.
#[derive(Debug, Clone)]
pub struct OAuth2PasswordBearer {
    token_url: String,
}

impl OAuth2PasswordBearer {
    /// `token_url` is the endpoint that issues tokens (documented in OpenAPI).
    pub fn new(token_url: &str) -> OAuth2PasswordBearer {
        OAuth2PasswordBearer {
            token_url: token_url.to_string(),
        }
    }

    /// The token of the request (401 with `WWW-Authenticate: Bearer` when missing).
    pub fn token(&self, req: &Request) -> Result<String, Response> {
        BearerToken::from_headers(req).map(|t| t.0)
    }

    /// The `components.securitySchemes` entry (merge it into the OpenAPI document you publish).
    pub fn security_scheme(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "oauth2",
            "flows": { "password": { "tokenUrl": self.token_url, "scopes": {} } }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(h: &[(&'static str, &'static str)]) -> Request {
        Request::new(
            "GET",
            "/",
            h.iter()
                .map(|(n, v)| {
                    (
                        Bytes::from_static(n.as_bytes()),
                        Bytes::from_static(v.as_bytes()),
                    )
                })
                .collect(),
            Bytes::new(),
        )
    }

    #[test]
    fn credentials() {
        let b =
            BasicAuth::from_headers(&req(&[("authorization", "Basic dXNlcjpwYTpzcw==")])).unwrap();
        assert_eq!((b.user.as_str(), b.password.as_str()), ("user", "pa:ss"));
        assert!(b.matches("user", "pa:ss"));
        assert!(!b.matches("user", "x"));
        let e = BasicAuth::from_headers(&req(&[])).unwrap_err();
        assert_eq!(e.status_code(), 401);
        assert!(e.header("www-authenticate").unwrap().starts_with(b"Basic"));
        assert_eq!(
            BearerToken::from_headers(&req(&[("authorization", "bearer abc")]))
                .unwrap()
                .0,
            "abc"
        );
        assert!(BearerToken::from_headers(&req(&[("authorization", "Basic abc")])).is_err());
        let k = ApiKey::<XApiKey>::from_request(&req(&[("x-api-key", "k1")])).unwrap();
        assert!(k.is_one_of(&["k0", "k1"]));
        assert!(!k.is_one_of(&["k0"]));
    }

    #[test]
    fn authenticate_and_require() {
        let ok: HandlerFn = Arc::new(|r: Request| {
            let who = Auth::<Principal>::from_request(&r).map(|p| p.subject.clone());
            Box::pin(std::future::ready(match who {
                Ok(s) => Response::text(s),
                Err(e) => e,
            }))
        });
        let authn = authenticate(|r: &Request| {
            Ok(BearerToken::from_headers(r).ok().map(|t| {
                Principal::new(t.0.clone()).with_scopes(if t.0 == "root" {
                    vec!["admin"]
                } else {
                    vec![]
                })
            }))
        });
        let f = authn(require(|p: &Principal| p.has_scope("admin"))(ok));
        let run = |r| futures::executor::block_on(f(r));
        assert_eq!(run(req(&[])).status_code(), 401);
        assert_eq!(
            run(req(&[("authorization", "Bearer bob")])).status_code(),
            403
        );
        let r = run(req(&[("authorization", "Bearer root")]));
        assert_eq!(r.status_code(), 200);
        assert_eq!(r.body(), &Bytes::from_static(b"root"));
    }
}
