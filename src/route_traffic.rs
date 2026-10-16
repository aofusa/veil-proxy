//! ルート単位のトラフィック制御（F-219、メッシュ無しの TOML のルート）。
//!
//! `[route.traffic]` の重み付き分割（`split`）・フォルト注入の中断（`fault`）・CORS のプリフライト（`cors`）、
//! `[route.jwt]` の JWT の検証と claim の条件（F-220、`jwt` feature）と、固定応答の action（`type = "Respond"`）。どれもルートの照合（`upstream::find_backend_unified`）の直後に
//! 解決済みの [`Backend`] を選ぶだけで、プロトコル（HTTP/1.1・HTTP/2・HTTP/3）ごとの経路は変えない。
//!
//! - 設定のロード時に分割先の `Backend`・中断とプリフライトの応答を作っておき、要求のたびには `Backend` の
//!   clone（`Arc` の参照カウント）と乱数 1 個（スレッドローカルの xorshift）だけ。
//! - `[route.traffic]` の無いルートは `Route::resolved_traffic` が `None` で、分岐 1 個だけ増える。
//! - 乱数で決める判定（分割・中断）は、ストリーミングにするかの下見（`draw = false`）では引かない。
//!   下見とディスパッチで 2 回引くと、中断の割合が二乗になる（下見は中断の可能性があればバッファ経路にする）。

use crate::config::{Backend, BackendConfig, Route, SecurityConfig, UpstreamGroup};
use bytes::Bytes;
use serde::Deserialize;
use std::cell::Cell;
use std::collections::HashMap;
use std::io;
use std::sync::Arc;

/// `[route.traffic]`。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrafficConfig {
    /// 重み付き分割（カナリア）。空なら `[route.action]` のまま
    #[serde(default)]
    pub split: Vec<SplitEntry>,
    /// フォルト注入（中断）
    #[serde(default)]
    pub fault: Option<FaultConfig>,
    /// CORS のプリフライト（`OPTIONS` + `Origin` + `Access-Control-Request-Method`）への応答
    #[serde(default)]
    pub cors: Option<CorsConfig>,
}

/// 分割先の 1 つ（`upstream` か `url` のどちらか）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SplitEntry {
    /// `[upstreams.<name>]` の名前
    #[serde(default)]
    pub upstream: Option<String>,
    /// 単一の上流の URL
    #[serde(default)]
    pub url: Option<String>,
    /// 重み（0 は送らない）
    pub weight: u32,
    /// 上流を h2c で話す（`[route.action]` の `use_h2c` と同じ）
    #[serde(default)]
    pub use_h2c: bool,
}

/// フォルト注入（Istio の `fault.abort` 相当）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaultConfig {
    /// 中断の応答のステータス
    pub abort_status: u16,
    /// 中断する要求の割合（0〜100）
    #[serde(default = "default_percent")]
    pub abort_percent: f64,
}

fn default_percent() -> f64 {
    100.0
}

/// `[route.jwt]`（F-220）。検証できない要求は 401、claim の条件に合わない要求は 403 で返す。
/// 型は `jwt` feature が無くても持つ（書かれていたら設定のロードでエラーにし、黙って素通しにしない）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JwtConfig {
    /// `iss` の期待値（空なら見ない）
    #[serde(default)]
    pub issuer: String,
    /// `aud` に入っていてほしい値（どれか 1 つ。空なら見ない）
    #[serde(default)]
    pub audiences: Vec<String>,
    /// JWKS（JSON の文字列）
    #[serde(default)]
    pub jwks: String,
    /// JWKS のファイル（設定のロード時に読む。差し替えは SIGHUP）
    #[serde(default)]
    pub jwks_path: String,
    /// トークンを読むヘッダ
    #[serde(default = "default_jwt_header")]
    pub header: String,
    /// ヘッダの値の接頭辞（大文字小文字は区別しない）
    #[serde(default = "default_jwt_prefix")]
    pub prefix: String,
    /// トークンの無い要求も通す（あれば検証する）
    #[serde(default)]
    pub allow_missing: bool,
    /// `exp` の無いトークンを拒否する
    #[serde(default)]
    pub require_expiration: bool,
    #[serde(default = "default_clock_skew")]
    pub clock_skew_secs: u64,
    /// claim の条件（名前 → 許す値。全部の claim がどれかの値を持つこと。配列・空白区切りの文字列も見る）
    #[serde(default)]
    pub require_claims: std::collections::BTreeMap<String, Vec<String>>,
}

fn default_jwt_header() -> String {
    "authorization".into()
}

fn default_jwt_prefix() -> String {
    "Bearer ".into()
}

fn default_clock_skew() -> u64 {
    60
}

/// 解決済みの `[route.jwt]`。
#[cfg(feature = "jwt")]
struct JwtRule {
    keys: Vec<crate::jwt_verify::Jwk>,
    issuer: String,
    audiences: Vec<String>,
    header: Box<[u8]>,
    prefix: Box<[u8]>,
    allow_missing: bool,
    require_expiration: bool,
    clock_skew_secs: u64,
    claims: Vec<(String, Vec<String>)>,
    missing: Backend,
    invalid: Backend,
    denied: Backend,
}

#[cfg(feature = "jwt")]
impl JwtRule {
    /// 通すなら `None`、拒否なら 401 / 403 の固定応答。
    fn check(&self, headers: &[(&[u8], &[u8])]) -> Option<Backend> {
        let Some(v) = header(headers, &self.header) else {
            return (!self.allow_missing).then(|| self.missing.clone());
        };
        let p = self.prefix.len();
        if v.len() < p || !v[..p].eq_ignore_ascii_case(&self.prefix) {
            return Some(self.invalid.clone());
        }
        let Ok(token) = std::str::from_utf8(v[p..].trim_ascii()) else {
            return Some(self.invalid.clone());
        };
        let c = crate::jwt_verify::Check {
            keys: &self.keys,
            issuer: &self.issuer,
            audiences: &self.audiences,
            require_expiration: self.require_expiration,
            clock_skew_secs: self.clock_skew_secs,
            now: crate::jwt_verify::unix_secs(),
        };
        let Some(payload) = crate::jwt_verify::verify(&c, token) else {
            return Some(self.invalid.clone());
        };
        let ok = self.claims.iter().all(|(name, allowed)| {
            let has = |s: &str| allowed.iter().any(|a| a == s);
            match payload.get(name) {
                Some(serde_json::Value::String(s)) => s.split_ascii_whitespace().any(has),
                Some(serde_json::Value::Array(a)) => a.iter().any(|x| x.as_str().is_some_and(has)),
                Some(serde_json::Value::Bool(b)) => has(if *b { "true" } else { "false" }),
                Some(serde_json::Value::Number(n)) => has(&n.to_string()),
                _ => false,
            }
        });
        (!ok).then(|| self.denied.clone())
    }
}

/// CORS のプリフライトの応答。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// 許可するオリジン（完全一致。`"*"` で全部）
    pub allow_origins: Vec<String>,
    #[serde(default)]
    pub allow_methods: Vec<String>,
    #[serde(default)]
    pub allow_headers: Vec<String>,
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// `Access-Control-Max-Age`（秒）
    #[serde(default)]
    pub max_age_secs: Option<u64>,
    #[serde(default)]
    pub allow_credentials: bool,
}

/// 固定応答（`type = "Respond"`・フォルトの中断・CORS のプリフライト）。ロード時に組み立てて共有する。
#[derive(Debug)]
pub struct FixedResponse {
    pub status: u16,
    /// 応答ヘッダ（小文字の名前。`content-length` は含めない）
    pub headers: Vec<(Bytes, Bytes)>,
    pub body: Bytes,
    /// HTTP/1.1 の応答全体（`Connection: keep-alive` / `close`）
    pub h1_keepalive: Bytes,
    pub h1_close: Bytes,
    /// ルートの `[route.security]`（IP 制限・許可メソッド・本文の大きさ・レート制限）。B-133 より前は
    /// `Backend::security()` が既定値を返し、`type = "Respond"` とトラフィック制御の応答（フォルト・CORS・JWT）は
    /// ルートのセキュリティ設定を無視していた
    pub security: Arc<SecurityConfig>,
}

impl FixedResponse {
    pub fn new(status: u16, headers: Vec<(Bytes, Bytes)>, body: Bytes) -> Self {
        let h1 = |conn: &str| {
            let mut v = Vec::with_capacity(128 + body.len());
            v.extend_from_slice(
                format!("HTTP/1.1 {} {}\r\n", status, reason_phrase(status)).as_bytes(),
            );
            for (k, val) in &headers {
                v.extend_from_slice(k);
                v.extend_from_slice(b": ");
                v.extend_from_slice(val);
                v.extend_from_slice(b"\r\n");
            }
            v.extend_from_slice(
                format!(
                    "Content-Length: {}\r\nConnection: {}\r\n\r\n",
                    body.len(),
                    conn
                )
                .as_bytes(),
            );
            v.extend_from_slice(&body);
            Bytes::from(v)
        };
        let h1_keepalive = h1("keep-alive");
        let h1_close = h1("close");
        FixedResponse {
            status,
            headers,
            body,
            h1_keepalive,
            h1_close,
            security: default_security(),
        }
    }

    /// ルートのセキュリティ設定を付ける（ロード時に 1 回）
    pub fn with_security(mut self, security: Arc<SecurityConfig>) -> Self {
        self.security = security;
        self
    }
}

/// セキュリティ設定の無い固定応答（単体テスト・ルートの外で作る応答）が共有する既定値
fn default_security() -> Arc<SecurityConfig> {
    static DEFAULT: std::sync::OnceLock<Arc<SecurityConfig>> = std::sync::OnceLock::new();
    DEFAULT
        .get_or_init(|| Arc::new(SecurityConfig::default()))
        .clone()
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        418 => "I'm a teapot",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

/// 解決済みのトラフィック制御（`Route::resolved_traffic`）。
pub struct TrafficPolicy {
    /// (累積の重み, 分割先)
    split: Vec<(u32, Backend)>,
    split_total: u32,
    /// (100 万分率, 中断の応答)
    abort: Option<(u32, Backend)>,
    cors: Option<CorsPreflight>,
    #[cfg(feature = "jwt")]
    jwt: Option<JwtRule>,
}

struct CorsPreflight {
    /// `allow_origins` に `"*"` があるときの応答
    any: Option<Backend>,
    /// オリジンごとの応答（`Access-Control-Allow-Origin` にそのオリジンを返す）
    exact: Vec<(Box<[u8]>, Backend)>,
}

impl std::fmt::Debug for TrafficPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrafficPolicy")
            .field("split", &self.split.len())
            .field("abort", &self.abort.as_ref().map(|a| a.0))
            .field("cors", &self.cors.is_some())
            .finish()
    }
}

thread_local! {
    static RNG: Cell<u64> = const { Cell::new(0) };
}

/// スレッドローカルの xorshift64*（初回はスレッドのアドレスと時刻で種を作る）。
#[inline]
fn next_u64() -> u64 {
    RNG.with(|c| {
        let mut x = c.get();
        if x == 0 {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            x = (c as *const Cell<u64> as u64) ^ t ^ 0x9E37_79B9_7F4A_7C15;
            if x == 0 {
                x = 1;
            }
        }
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        c.set(x);
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    })
}

#[inline]
fn header<'a>(headers: &[(&[u8], &'a [u8])], name: &[u8]) -> Option<&'a [u8]> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| *v)
}

impl TrafficPolicy {
    /// 要求の `Backend` を選ぶ。`None` は `[route.action]` のまま（`load_backend`）。
    /// `draw = false` は下見（乱数を引かない。中断の可能性があれば中断の応答を返してバッファ経路にさせる）。
    #[inline]
    pub fn select(&self, method: &[u8], headers: &[(&[u8], &[u8])], draw: bool) -> Option<Backend> {
        if let Some(c) = &self.cors {
            if method == b"OPTIONS" && header(headers, b"access-control-request-method").is_some() {
                if let Some(origin) = header(headers, b"origin") {
                    if let Some((_, b)) = c.exact.iter().find(|(o, _)| o.as_ref() == origin) {
                        return Some(b.clone());
                    }
                    if let Some(b) = &c.any {
                        return Some(b.clone());
                    }
                }
            }
        }
        // JWT（F-220）は乱数を使わないので下見でも判定する（HTTP/3 のストリーミングは下見だけでディスパッチする）
        #[cfg(feature = "jwt")]
        if let Some(j) = &self.jwt {
            if let Some(b) = j.check(headers) {
                return Some(b);
            }
        }
        if let Some((ppm, b)) = &self.abort {
            if !draw || (next_u64() % 1_000_000) < u64::from(*ppm) {
                return Some(b.clone());
            }
        }
        if self.split_total > 0 {
            if !draw {
                return self.split.first().map(|(_, b)| b.clone());
            }
            let r = (next_u64() % u64::from(self.split_total)) as u32;
            let i = self.split.partition_point(|(cum, _)| *cum <= r);
            return self.split.get(i).map(|(_, b)| b.clone());
        }
        None
    }
}

fn err(m: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, m)
}

fn respond_backend(
    security: &Arc<SecurityConfig>,
    status: u16,
    headers: Vec<(Bytes, Bytes)>,
    body: Bytes,
) -> Backend {
    Backend::Respond(Arc::new(
        FixedResponse::new(status, headers, body).with_security(security.clone()),
    ))
}

/// `[route.traffic]` を解決する（設定のロード時）。
pub fn resolve(
    route: &Route,
    upstream_groups: &HashMap<String, Arc<UpstreamGroup>>,
    build: impl Fn(&Route, &HashMap<String, Arc<UpstreamGroup>>) -> io::Result<Backend>,
) -> io::Result<Option<Arc<TrafficPolicy>>> {
    if route.traffic.is_none() && route.jwt.is_none() {
        return Ok(None);
    }
    let default_traffic = TrafficConfig::default();
    let t = route.traffic.as_ref().unwrap_or(&default_traffic);
    // トラフィック制御が返す応答（フォルト・CORS・JWT）にもルートのセキュリティ設定を付ける（B-133）
    let sec = Arc::new(route.security.clone().unwrap_or_default());
    #[cfg(feature = "jwt")]
    let jwt = route
        .jwt
        .as_ref()
        .map(|j| resolve_jwt(j, &sec))
        .transpose()?;
    #[cfg(not(feature = "jwt"))]
    if route.jwt.is_some() {
        return Err(err(
            "[route.jwt] requires a build with the `jwt` feature (included in `full`)".into(),
        ));
    }
    let mut split = Vec::with_capacity(t.split.len());
    let mut total: u32 = 0;
    for (i, e) in t.split.iter().enumerate() {
        let action = match (&e.upstream, &e.url) {
            (Some(u), None) => BackendConfig::ProxyUpstream {
                upstream: u.clone(),
                use_h2c: e.use_h2c,
            },
            (None, Some(url)) => BackendConfig::Proxy {
                url: url.clone(),
                sni_name: None,
                use_h2c: e.use_h2c,
                http2: Default::default(),
                tls: None,
            },
            _ => {
                return Err(err(format!(
                    "[route.traffic] split[{}]: set exactly one of upstream or url",
                    i
                )));
            }
        };
        if e.weight == 0 {
            continue;
        }
        let mut r = route.clone();
        r.action = action;
        r.traffic = None;
        r.jwt = None;
        let b = build(&r, upstream_groups)
            .map_err(|e2| err(format!("[route.traffic] split[{}]: {}", i, e2)))?;
        total = total
            .checked_add(e.weight)
            .ok_or_else(|| err("[route.traffic] split: total weight overflows".into()))?;
        split.push((total, b));
    }
    if !t.split.is_empty() && total == 0 {
        return Err(err("[route.traffic] split: all weights are 0".into()));
    }
    let abort = match &t.fault {
        None => None,
        Some(f) => {
            if !(100..=599).contains(&f.abort_status) {
                return Err(err(format!(
                    "[route.traffic] fault.abort_status {} is not an HTTP status",
                    f.abort_status
                )));
            }
            if !(0.0..=100.0).contains(&f.abort_percent) {
                return Err(err(
                    "[route.traffic] fault.abort_percent must be 0..=100".into()
                ));
            }
            let ppm = (f.abort_percent * 10_000.0).round() as u32;
            Some((
                ppm,
                respond_backend(
                    &sec,
                    f.abort_status,
                    vec![(
                        Bytes::from_static(b"content-type"),
                        Bytes::from_static(b"text/plain"),
                    )],
                    Bytes::from_static(b"fault filter abort"),
                ),
            ))
        }
    };
    let cors = match &t.cors {
        None => None,
        Some(c) => {
            if c.allow_origins.is_empty() {
                return Err(err("[route.traffic] cors.allow_origins is empty".into()));
            }
            let make = |origin: &str| {
                let mut h: Vec<(Bytes, Bytes)> = vec![(
                    Bytes::from_static(b"access-control-allow-origin"),
                    Bytes::from(origin.to_string()),
                )];
                if origin != "*" {
                    h.push((Bytes::from_static(b"vary"), Bytes::from_static(b"Origin")));
                }
                if !c.allow_methods.is_empty() {
                    h.push((
                        Bytes::from_static(b"access-control-allow-methods"),
                        Bytes::from(c.allow_methods.join(",")),
                    ));
                }
                if !c.allow_headers.is_empty() {
                    h.push((
                        Bytes::from_static(b"access-control-allow-headers"),
                        Bytes::from(c.allow_headers.join(",")),
                    ));
                }
                if !c.expose_headers.is_empty() {
                    h.push((
                        Bytes::from_static(b"access-control-expose-headers"),
                        Bytes::from(c.expose_headers.join(",")),
                    ));
                }
                if let Some(age) = c.max_age_secs {
                    h.push((
                        Bytes::from_static(b"access-control-max-age"),
                        Bytes::from(age.to_string()),
                    ));
                }
                if c.allow_credentials {
                    h.push((
                        Bytes::from_static(b"access-control-allow-credentials"),
                        Bytes::from_static(b"true"),
                    ));
                }
                respond_backend(&sec, 200, h, Bytes::new())
            };
            let mut any = None;
            let mut exact = Vec::new();
            for o in &c.allow_origins {
                if o == "*" {
                    if c.allow_credentials {
                        return Err(err(
                            "[route.traffic] cors: allow_credentials cannot be used with \"*\""
                                .into(),
                        ));
                    }
                    any = Some(make("*"));
                } else {
                    exact.push((o.as_bytes().to_vec().into_boxed_slice(), make(o)));
                }
            }
            Some(CorsPreflight { any, exact })
        }
    };
    Ok(Some(Arc::new(TrafficPolicy {
        split,
        split_total: total,
        abort,
        cors,
        #[cfg(feature = "jwt")]
        jwt,
    })))
}

#[cfg(feature = "jwt")]
fn resolve_jwt(j: &JwtConfig, sec: &Arc<SecurityConfig>) -> io::Result<JwtRule> {
    let text = match (j.jwks.is_empty(), j.jwks_path.is_empty()) {
        (false, true) => j.jwks.clone(),
        (true, false) => {
            let b = crate::tls_reload::read_pem(std::path::Path::new(&j.jwks_path))
                .map_err(|e| err(format!("[route.jwt] jwks_path {}: {}", j.jwks_path, e)))?;
            String::from_utf8(b)
                .map_err(|_| err(format!("[route.jwt] jwks_path {}: not UTF-8", j.jwks_path)))?
        }
        _ => {
            return Err(err(
                "[route.jwt] set exactly one of jwks or jwks_path".into()
            ))
        }
    };
    let keys = crate::jwt_verify::parse_jwks(&text);
    if keys.is_empty() {
        return Err(err("[route.jwt] the JWKS has no usable keys".into()));
    }
    let text_resp = |status: u16, body: &'static [u8], www: Option<&'static [u8]>| {
        let mut h = vec![(
            Bytes::from_static(b"content-type"),
            Bytes::from_static(b"text/plain"),
        )];
        if let Some(w) = www {
            h.push((
                Bytes::from_static(b"www-authenticate"),
                Bytes::from_static(w),
            ));
        }
        respond_backend(sec, status, h, Bytes::from_static(body))
    };
    Ok(JwtRule {
        keys,
        issuer: j.issuer.clone(),
        audiences: j.audiences.clone(),
        header: j
            .header
            .to_ascii_lowercase()
            .into_bytes()
            .into_boxed_slice(),
        prefix: j.prefix.as_bytes().to_vec().into_boxed_slice(),
        allow_missing: j.allow_missing,
        require_expiration: j.require_expiration,
        clock_skew_secs: j.clock_skew_secs,
        claims: j
            .require_claims
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        missing: text_resp(401, b"Jwt is missing", Some(b"Bearer realm=\"veil\"")),
        invalid: text_resp(
            401,
            b"Jwt verification fails",
            Some(b"Bearer realm=\"veil\", error=\"invalid_token\""),
        ),
        denied: text_resp(403, b"RBAC: access denied", None),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(b: &Backend) -> u16 {
        match b {
            Backend::Respond(r) => r.status,
            _ => 0,
        }
    }

    fn policy(abort_percent: f64) -> TrafficPolicy {
        TrafficPolicy {
            split: vec![
                (
                    90,
                    respond_backend(&default_security(), 201, vec![], Bytes::new()),
                ),
                (
                    100,
                    respond_backend(&default_security(), 202, vec![], Bytes::new()),
                ),
            ],
            split_total: 100,
            abort: (abort_percent > 0.0).then(|| {
                (
                    (abort_percent * 10_000.0) as u32,
                    respond_backend(&default_security(), 503, vec![], Bytes::new()),
                )
            }),
            cors: None,
            #[cfg(feature = "jwt")]
            jwt: None,
        }
    }

    #[test]
    fn split_follows_weights() {
        let p = policy(0.0);
        let mut n201 = 0;
        for _ in 0..10_000 {
            if status(&p.select(b"GET", &[], true).unwrap()) == 201 {
                n201 += 1;
            }
        }
        assert!((8_700..=9_300).contains(&n201), "{n201}");
    }

    #[test]
    fn abort_percent_and_preview_does_not_draw() {
        let p = policy(25.0);
        let mut aborted = 0;
        for _ in 0..10_000 {
            if status(&p.select(b"GET", &[], true).unwrap()) == 503 {
                aborted += 1;
            }
        }
        assert!((2_200..=2_800).contains(&aborted), "{aborted}");
        // 下見は乱数を引かず、中断の可能性があれば中断の応答（バッファ経路にさせる）
        assert_eq!(status(&p.select(b"GET", &[], false).unwrap()), 503);
    }

    #[test]
    fn cors_preflight_echoes_listed_origin() {
        let cfg = CorsConfig {
            allow_origins: vec!["https://a.example".into()],
            allow_methods: vec!["GET".into(), "POST".into()],
            max_age_secs: Some(600),
            ..Default::default()
        };
        let mut route: Route = toml::from_str(
            r#"
[conditions]
path = "/"
[action]
type = "Respond"
status = 200
"#,
        )
        .unwrap();
        route.traffic = Some(TrafficConfig {
            cors: Some(cfg),
            ..Default::default()
        });
        let p = resolve(&route, &HashMap::new(), |_, _| unreachable!())
            .unwrap()
            .unwrap();
        let hs: [(&[u8], &[u8]); 2] = [
            (b"Origin", b"https://a.example"),
            (b"access-control-request-method", b"POST"),
        ];
        let Some(Backend::Respond(r)) = p.select(b"OPTIONS", &hs, true) else {
            panic!("expected a preflight response");
        };
        assert_eq!(r.status, 200);
        let text = String::from_utf8_lossy(&r.h1_close).to_string();
        assert!(
            text.contains("access-control-allow-origin: https://a.example"),
            "{text}"
        );
        assert!(text.contains("access-control-max-age: 600"), "{text}");
        // 一覧に無いオリジン・プリフライトでない要求は action のまま
        let other: [(&[u8], &[u8]); 2] = [
            (b"origin", b"https://b.example"),
            (b"access-control-request-method", b"POST"),
        ];
        assert!(p.select(b"OPTIONS", &other, true).is_none());
        assert!(p.select(b"GET", &hs, true).is_none());
    }
}
