// F-267: CORS。ヘッダの値は起動時に組み立てた Bytes（要求ごとは参照カウントの clone だけ）。
// プリフライトはミドルウェアが答える（ハンドラを呼ばない）。同じパスの OPTIONS のルートも足す
// （OPTIONS を受けるルートが無いと 404 になり、ミドルウェアまで届かないため）。
//! CORS (Cross-Origin Resource Sharing) for handler routes.
//!
//! ```no_run
//! use veil::prelude::*;
//! use veil_kit::cors::Cors;
//! App::new()
//!     .with(
//!         Cors::new()
//!             .allow_origins(["https://app.example.com", "https://*.example.org"])
//!             .allow_methods(["GET", "POST"])
//!             .allow_headers(["content-type", "authorization"])
//!             .allow_credentials(true)
//!             .max_age(std::time::Duration::from_secs(600))
//!             .on("/api"),
//!     )
//!     .run()
//!     .unwrap();
//! ```
//!
//! Preflight requests (`OPTIONS` with `Access-Control-Request-Method`) are answered with 204 without calling a
//! handler; a request from an origin that is not allowed gets no CORS headers (the browser then blocks it).
//! This is separate from `[route.traffic.cors]` of proxy routes; do not configure both for the same paths.

use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;
use veil::api::{App, HandlerFn, OperationMeta, Plugin, Request, Response};

/// CORS settings (see the [module documentation](self)).
#[derive(Debug, Clone)]
pub struct Cors {
    any_origin: bool,
    origins: Vec<String>,
    methods: Vec<String>,
    any_header: bool,
    headers: Vec<String>,
    expose: Vec<String>,
    credentials: bool,
    max_age: Option<Duration>,
    prefix: String,
}

impl Default for Cors {
    fn default() -> Self {
        Cors::new()
    }
}

/// An invalid CORS setting.
#[derive(Debug, Clone)]
pub struct CorsError(String);

impl std::fmt::Display for CorsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CorsError {}

impl Cors {
    /// No origins allowed yet; methods `GET, HEAD, POST`; for all routes (`/`).
    pub fn new() -> Cors {
        Cors {
            any_origin: false,
            origins: Vec::new(),
            methods: vec!["GET".into(), "HEAD".into(), "POST".into()],
            any_header: false,
            headers: Vec::new(),
            expose: Vec::new(),
            credentials: false,
            max_age: None,
            prefix: "/".into(),
        }
    }

    /// Allowed origins: exact values (`https://app.example.com`) or one leading wildcard label
    /// (`https://*.example.com` matches subdomains, not `example.com` itself).
    pub fn allow_origins<S: Into<String>>(mut self, origins: impl IntoIterator<Item = S>) -> Cors {
        self.origins.extend(origins.into_iter().map(Into::into));
        self
    }

    /// Any origin (`Access-Control-Allow-Origin: *`; not allowed together with credentials).
    pub fn allow_any_origin(mut self) -> Cors {
        self.any_origin = true;
        self
    }

    /// Allowed methods (replaces the default `GET, HEAD, POST`).
    pub fn allow_methods<S: Into<String>>(mut self, methods: impl IntoIterator<Item = S>) -> Cors {
        self.methods = methods
            .into_iter()
            .map(|m| m.into().to_ascii_uppercase())
            .collect();
        self
    }

    /// Allowed request headers.
    pub fn allow_headers<S: Into<String>>(mut self, headers: impl IntoIterator<Item = S>) -> Cors {
        self.headers
            .extend(headers.into_iter().map(|h| h.into().to_ascii_lowercase()));
        self
    }

    /// Allows the request headers the browser asks for (echoes `Access-Control-Request-Headers`).
    pub fn allow_any_header(mut self) -> Cors {
        self.any_header = true;
        self
    }

    /// Response headers the page's script may read (`Access-Control-Expose-Headers`).
    pub fn expose_headers<S: Into<String>>(mut self, headers: impl IntoIterator<Item = S>) -> Cors {
        self.expose
            .extend(headers.into_iter().map(|h| h.into().to_ascii_lowercase()));
        self
    }

    /// `Access-Control-Allow-Credentials: true` (cookies and `Authorization`).
    pub fn allow_credentials(mut self, on: bool) -> Cors {
        self.credentials = on;
        self
    }

    /// How long the browser may cache a preflight answer (`Access-Control-Max-Age`).
    pub fn max_age(mut self, d: Duration) -> Cors {
        self.max_age = Some(d);
        self
    }

    /// Applies only to routes under `prefix` (default `/`, all routes).
    pub fn on(mut self, prefix: &str) -> Cors {
        self.prefix = prefix.to_string();
        self
    }

    /// Checks the settings and prepares the header values.
    pub fn build(self) -> Result<CorsPolicy, CorsError> {
        if self.any_origin && self.credentials {
            return Err(CorsError(
                "CORS: allow_any_origin cannot be combined with allow_credentials (browsers reject it)".into(),
            ));
        }
        if !self.any_origin && self.origins.is_empty() {
            return Err(CorsError(
                "CORS: no origin is allowed (use allow_origins or allow_any_origin)".into(),
            ));
        }
        let mut exact = Vec::new();
        let mut wildcard = Vec::new();
        for o in &self.origins {
            if o == "*" {
                return Err(CorsError(
                    "CORS: use allow_any_origin instead of \"*\"".into(),
                ));
            }
            let (scheme, host) = o
                .split_once("://")
                .ok_or_else(|| CorsError(format!("CORS: origin '{}' needs a scheme", o)))?;
            if o.ends_with('/') {
                return Err(CorsError(format!(
                    "CORS: origin '{}' must not end with '/'",
                    o
                )));
            }
            if let Some(rest) = host.strip_prefix("*.") {
                wildcard.push((format!("{}://", scheme), format!(".{}", rest)));
            } else if host.contains('*') {
                return Err(CorsError(format!(
                    "CORS: origin '{}': only a leading '*.' label is supported",
                    o
                )));
            } else {
                exact.push(Bytes::from(o.to_ascii_lowercase()));
            }
        }
        for m in &self.methods {
            if m.is_empty() || !m.bytes().all(|b| b.is_ascii_alphabetic()) {
                return Err(CorsError(format!("CORS: invalid method '{}'", m)));
            }
        }
        let join = |v: &[String]| Bytes::from(v.join(", "));
        Ok(CorsPolicy {
            inner: Arc::new(PolicyInner {
                any_origin: self.any_origin,
                exact,
                wildcard,
                methods: join(&self.methods),
                headers: (!self.headers.is_empty()).then(|| join(&self.headers)),
                any_header: self.any_header,
                expose: (!self.expose.is_empty()).then(|| join(&self.expose)),
                credentials: self.credentials,
                max_age: self.max_age.map(|d| Bytes::from(d.as_secs().to_string())),
            }),
            prefix: self.prefix,
        })
    }
}

impl Plugin for Cors {
    fn apply(self, app: App) -> App {
        // 設定の誤りは起動時に止める（要求の時点で黙って CORS を外さない）
        match self.build() {
            Ok(p) => p.apply(app),
            Err(e) => panic!("{}", e),
        }
    }
}

struct PolicyInner {
    any_origin: bool,
    exact: Vec<Bytes>,
    wildcard: Vec<(String, String)>,
    methods: Bytes,
    headers: Option<Bytes>,
    any_header: bool,
    expose: Option<Bytes>,
    credentials: bool,
    max_age: Option<Bytes>,
}

/// Checked CORS settings (from [`Cors::build`]); a [`Plugin`].
#[derive(Clone)]
pub struct CorsPolicy {
    inner: Arc<PolicyInner>,
    prefix: String,
}

impl PolicyInner {
    fn origin_allowed(&self, origin: &[u8]) -> bool {
        if self.any_origin {
            return true;
        }
        if self.exact.iter().any(|o| o.eq_ignore_ascii_case(origin)) {
            return true;
        }
        let Ok(o) = std::str::from_utf8(origin) else {
            return false;
        };
        let o = o.to_ascii_lowercase();
        self.wildcard.iter().any(|(scheme, suffix)| {
            o.strip_prefix(scheme.as_str()).is_some_and(|host| {
                // ポートを除いた名前で比べる
                let name = host.split(':').next().unwrap_or(host);
                name.len() > suffix.len() && name.ends_with(suffix.as_str())
            })
        })
    }

    fn allow_origin_value(&self, origin: &Bytes) -> Bytes {
        if self.any_origin {
            Bytes::from_static(b"*")
        } else {
            origin.clone()
        }
    }

    fn preflight(&self, req: &Request, origin: &Bytes) -> Response {
        let mut r = Response::new(204);
        r = r
            .with_header(
                "access-control-allow-origin",
                self.allow_origin_value(origin),
            )
            .with_header("access-control-allow-methods", self.methods.clone());
        if self.any_header {
            if let Some(h) = header_bytes(req, b"access-control-request-headers") {
                r = r.with_header("access-control-allow-headers", h);
            }
        } else if let Some(h) = &self.headers {
            r = r.with_header("access-control-allow-headers", h.clone());
        }
        if self.credentials {
            r = r.with_header("access-control-allow-credentials", "true");
        }
        if let Some(m) = &self.max_age {
            r = r.with_header("access-control-max-age", m.clone());
        }
        if !self.any_origin {
            r = r.with_header(
                "vary",
                "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
            );
        }
        r
    }

    fn decorate(&self, mut r: Response, origin: &Bytes) -> Response {
        r = r.with_header(
            "access-control-allow-origin",
            self.allow_origin_value(origin),
        );
        if self.credentials {
            r = r.with_header("access-control-allow-credentials", "true");
        }
        if let Some(e) = &self.expose {
            r = r.with_header("access-control-expose-headers", e.clone());
        }
        if !self.any_origin {
            r = r.with_header("vary", "Origin");
        }
        r
    }
}

fn header_bytes(req: &Request, name: &[u8]) -> Option<Bytes> {
    req.headers()
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

impl CorsPolicy {
    /// The middleware function (for `App::wrap_on` when the route prefix is chosen elsewhere).
    pub fn middleware(&self) -> impl Fn(HandlerFn) -> HandlerFn + Send + Sync + 'static {
        let p = self.inner.clone();
        move |next: HandlerFn| -> HandlerFn {
            let p = p.clone();
            Arc::new(move |req: Request| {
                let origin = match header_bytes(&req, b"origin") {
                    Some(o) if p.origin_allowed(&o) => o,
                    // Origin の無い要求（同一オリジン・サーバ間）と許可外の Origin はそのまま通す
                    _ => return next(req),
                };
                if req.method() == "OPTIONS"
                    && req
                        .headers()
                        .iter()
                        .any(|(n, _)| n.eq_ignore_ascii_case(b"access-control-request-method"))
                {
                    let r = p.preflight(&req, &origin);
                    return Box::pin(std::future::ready(r));
                }
                let fut = next(req);
                let p = p.clone();
                Box::pin(async move { p.decorate(fut.await, &origin) })
            })
        }
    }
}

impl Plugin for CorsPolicy {
    fn apply(self, app: App) -> App {
        let p = self.prefix.trim_end_matches('/');
        let pattern = format!("{}/{{*__veil_kit_cors}}", p);
        let name = format!(
            "veil_kit_cors_preflight{}",
            p.replace(['/', '{', '}', '*'], "_")
        );
        // プリフライトはミドルウェアが答える。ここに来るのは CORS でない OPTIONS（許可外の Origin を含む）
        let func: HandlerFn =
            Arc::new(|_req: Request| Box::pin(std::future::ready(Response::not_found())));
        let meta = OperationMeta {
            operation_id: name.clone(),
            hidden: true,
            ..Default::default()
        };
        let mw = self.middleware();
        let prefix = if p.is_empty() {
            "/".to_string()
        } else {
            p.to_string()
        };
        app.route_fn(&name, &["OPTIONS"], &pattern, func, meta)
            .wrap_on(&prefix, mw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, headers: &[(&'static str, &'static str)]) -> Request {
        Request::new(
            method.to_string(),
            "/api/x",
            headers
                .iter()
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

    fn run(p: &CorsPolicy, r: Request) -> Response {
        let next: HandlerFn =
            Arc::new(|_r: Request| Box::pin(std::future::ready(Response::text("ok"))));
        let f = (p.middleware())(next);
        futures::executor::block_on(f(r))
    }

    #[test]
    fn invalid_settings_fail_at_startup() {
        assert!(Cors::new()
            .allow_any_origin()
            .allow_credentials(true)
            .build()
            .is_err());
        assert!(Cors::new().build().is_err());
        assert!(Cors::new().allow_origins(["example.com"]).build().is_err());
        assert!(Cors::new()
            .allow_origins(["https://a.*.com"])
            .build()
            .is_err());
        assert!(Cors::new()
            .allow_origins(["https://a.com/"])
            .build()
            .is_err());
    }

    #[test]
    fn preflight_and_actual_requests() {
        let p = Cors::new()
            .allow_origins(["https://app.example.com", "https://*.example.org"])
            .allow_methods(["GET", "PUT"])
            .allow_headers(["Content-Type"])
            .allow_credentials(true)
            .max_age(Duration::from_secs(600))
            .build()
            .unwrap();
        let r = run(
            &p,
            req(
                "OPTIONS",
                &[
                    ("origin", "https://app.example.com"),
                    ("access-control-request-method", "PUT"),
                ],
            ),
        );
        assert_eq!(r.status_code(), 204);
        assert_eq!(
            r.header("access-control-allow-origin"),
            Some(&b"https://app.example.com"[..])
        );
        assert_eq!(
            r.header("access-control-allow-methods"),
            Some(&b"GET, PUT"[..])
        );
        assert_eq!(
            r.header("access-control-allow-headers"),
            Some(&b"content-type"[..])
        );
        assert_eq!(r.header("access-control-max-age"), Some(&b"600"[..]));
        assert_eq!(
            r.header("access-control-allow-credentials"),
            Some(&b"true"[..])
        );

        let r = run(&p, req("GET", &[("origin", "https://x.example.org:8443")]));
        assert_eq!(r.body(), &Bytes::from_static(b"ok"));
        assert_eq!(
            r.header("access-control-allow-origin"),
            Some(&b"https://x.example.org:8443"[..])
        );
        assert_eq!(r.header("vary"), Some(&b"Origin"[..]));

        // 許可外・ワイルドカードの親ドメイン自身・Origin 無しは CORS ヘッダを付けない
        for o in [
            "https://evil.example",
            "https://example.org",
            "http://x.example.org",
        ] {
            let h: &'static str = Box::leak(o.to_string().into_boxed_str());
            let r = run(&p, req("GET", &[("origin", h)]));
            assert!(r.header("access-control-allow-origin").is_none(), "{}", o);
        }
        let r = run(&p, req("GET", &[]));
        assert!(r.header("access-control-allow-origin").is_none());
    }

    #[test]
    fn any_origin_uses_star() {
        let p = Cors::new()
            .allow_any_origin()
            .allow_any_header()
            .build()
            .unwrap();
        let r = run(
            &p,
            req(
                "OPTIONS",
                &[
                    ("origin", "https://a.test"),
                    ("access-control-request-method", "GET"),
                    ("access-control-request-headers", "x-a, x-b"),
                ],
            ),
        );
        assert_eq!(r.header("access-control-allow-origin"), Some(&b"*"[..]));
        assert_eq!(
            r.header("access-control-allow-headers"),
            Some(&b"x-a, x-b"[..])
        );
        assert!(r.header("vary").is_none());
    }
}
