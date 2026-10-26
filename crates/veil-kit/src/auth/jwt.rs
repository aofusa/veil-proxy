// F-267: JWT（RFC 7519）の検証。署名は TLS のプロバイダの crate（jsonwebtoken に依存しない）。
// アルゴリズムは鍵の側で決め、トークンの alg が鍵と違えば拒む（alg の取り違え・"none" を受けない）。
//! JSON Web Tokens: verification ([`JwtVerifier`], the [`Jwt<Claims>`] extractor) and HMAC signing.
//!
//! Supported algorithms: `HS256`, `HS384`, `HS512`, `RS256`, `RS384`, `RS512`, `PS256`, `PS384`, `PS512`,
//! `ES256`, `ES384` and `EdDSA` (Ed25519). The algorithm is fixed by each key; a token whose `alg` differs from
//! the key's is rejected, and `none` is never accepted. Keys are selected by `kid` when the token has one.
//!
//! ```no_run
//! use veil::prelude::*;
//! use veil_kit::auth::jwt::{Claims, Jwt, JwtKey, JwtVerifier};
//!
//! #[veil::route(path = "/me", method = "GET")]
//! async fn me(token: Jwt<Claims>) -> String {
//!     token.0.sub.clone().unwrap_or_default()
//! }
//!
//! let verifier = JwtVerifier::new([JwtKey::hs256(b"0123456789abcdef0123456789abcdef")])
//!     .issuer("https://auth.example.com")
//!     .audience("my-api");
//! App::new().state(verifier).run().unwrap();
//! ```

use crate::crypto::provider::{hmac, signature};
use arc_swap::ArcSwap;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use veil::api::{FromRequest, Request, Response};

/// A signature algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    /// HMAC SHA-256.
    HS256,
    /// HMAC SHA-384.
    HS384,
    /// HMAC SHA-512.
    HS512,
    /// RSASSA-PKCS1-v1_5 SHA-256.
    RS256,
    /// RSASSA-PKCS1-v1_5 SHA-384.
    RS384,
    /// RSASSA-PKCS1-v1_5 SHA-512.
    RS512,
    /// RSASSA-PSS SHA-256.
    PS256,
    /// RSASSA-PSS SHA-384.
    PS384,
    /// RSASSA-PSS SHA-512.
    PS512,
    /// ECDSA P-256 SHA-256.
    ES256,
    /// ECDSA P-384 SHA-384.
    ES384,
    /// Ed25519.
    EdDSA,
}

impl Algorithm {
    /// The `alg` name.
    pub fn name(self) -> &'static str {
        match self {
            Algorithm::HS256 => "HS256",
            Algorithm::HS384 => "HS384",
            Algorithm::HS512 => "HS512",
            Algorithm::RS256 => "RS256",
            Algorithm::RS384 => "RS384",
            Algorithm::RS512 => "RS512",
            Algorithm::PS256 => "PS256",
            Algorithm::PS384 => "PS384",
            Algorithm::PS512 => "PS512",
            Algorithm::ES256 => "ES256",
            Algorithm::ES384 => "ES384",
            Algorithm::EdDSA => "EdDSA",
        }
    }

    /// The algorithm with that `alg` name.
    pub fn from_name(s: &str) -> Option<Algorithm> {
        Some(match s {
            "HS256" => Algorithm::HS256,
            "HS384" => Algorithm::HS384,
            "HS512" => Algorithm::HS512,
            "RS256" => Algorithm::RS256,
            "RS384" => Algorithm::RS384,
            "RS512" => Algorithm::RS512,
            "PS256" => Algorithm::PS256,
            "PS384" => Algorithm::PS384,
            "PS512" => Algorithm::PS512,
            "ES256" => Algorithm::ES256,
            "ES384" => Algorithm::ES384,
            "EdDSA" => Algorithm::EdDSA,
            _ => return None,
        })
    }

    fn is_hmac(self) -> bool {
        matches!(self, Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512)
    }

    fn is_rsa(self) -> bool {
        matches!(
            self,
            Algorithm::RS256
                | Algorithm::RS384
                | Algorithm::RS512
                | Algorithm::PS256
                | Algorithm::PS384
                | Algorithm::PS512
        )
    }
}

/// Why a token was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JwtError {
    /// Not three base64url parts with JSON header and payload.
    Malformed,
    /// No key for the token's `kid` / `alg`.
    UnknownKey,
    /// The signature does not match.
    BadSignature,
    /// `exp` has passed.
    Expired,
    /// `nbf` is in the future.
    NotYetValid,
    /// `iss` differs from the expected issuer.
    BadIssuer,
    /// `aud` does not contain the expected audience.
    BadAudience,
    /// The claims do not deserialize into the requested type.
    Claims(String),
    /// A key could not be parsed.
    Key(String),
}

impl std::fmt::Display for JwtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JwtError::Malformed => f.write_str("malformed token"),
            JwtError::UnknownKey => f.write_str("no key for the token"),
            JwtError::BadSignature => f.write_str("invalid signature"),
            JwtError::Expired => f.write_str("token expired"),
            JwtError::NotYetValid => f.write_str("token not yet valid"),
            JwtError::BadIssuer => f.write_str("unexpected issuer"),
            JwtError::BadAudience => f.write_str("unexpected audience"),
            JwtError::Claims(e) => write!(f, "invalid claims: {}", e),
            JwtError::Key(e) => write!(f, "invalid key: {}", e),
        }
    }
}

impl std::error::Error for JwtError {}

#[derive(Clone)]
enum Material {
    Hmac(Vec<u8>),
    Rsa {
        n: Vec<u8>,
        e: Vec<u8>,
    },
    /// Uncompressed EC point or the raw Ed25519 key.
    Raw(Vec<u8>),
}

/// A verification key (one algorithm, optionally a `kid`).
#[derive(Clone)]
pub struct JwtKey {
    alg: Algorithm,
    kid: Option<String>,
    material: Material,
}

impl std::fmt::Debug for JwtKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtKey")
            .field("alg", &self.alg)
            .field("kid", &self.kid)
            .finish()
    }
}

impl JwtKey {
    /// An HMAC key (`HS256`, `HS384` or `HS512`).
    pub fn hmac(alg: Algorithm, secret: &[u8]) -> Result<JwtKey, JwtError> {
        if !alg.is_hmac() {
            return Err(JwtError::Key(format!(
                "{} is not an HMAC algorithm",
                alg.name()
            )));
        }
        Ok(JwtKey {
            alg,
            kid: None,
            material: Material::Hmac(secret.to_vec()),
        })
    }

    /// An `HS256` key.
    pub fn hs256(secret: &[u8]) -> JwtKey {
        JwtKey::hmac(Algorithm::HS256, secret).expect("HS256 is an HMAC algorithm")
    }

    /// An RSA public key from its modulus and exponent (big-endian bytes).
    pub fn rsa_components(alg: Algorithm, n: &[u8], e: &[u8]) -> Result<JwtKey, JwtError> {
        if !alg.is_rsa() {
            return Err(JwtError::Key(format!(
                "{} is not an RSA algorithm",
                alg.name()
            )));
        }
        Ok(JwtKey {
            alg,
            kid: None,
            material: Material::Rsa {
                n: strip_zeros(n).to_vec(),
                e: strip_zeros(e).to_vec(),
            },
        })
    }

    /// A public key in PEM: `PUBLIC KEY` (SubjectPublicKeyInfo; RSA, P-256, P-384, Ed25519) or `RSA PUBLIC KEY`.
    pub fn from_pem(alg: Algorithm, pem: &str) -> Result<JwtKey, JwtError> {
        let (label, der) =
            pem_decode(pem).ok_or_else(|| JwtError::Key("not a PEM block".into()))?;
        match label.as_str() {
            "RSA PUBLIC KEY" => {
                let (n, e) = rsa_pkcs1(&der)
                    .ok_or_else(|| JwtError::Key("invalid RSA public key".into()))?;
                JwtKey::rsa_components(alg, n, e)
            }
            "PUBLIC KEY" => {
                let key = spki_key(&der)
                    .ok_or_else(|| JwtError::Key("invalid SubjectPublicKeyInfo".into()))?;
                if alg.is_rsa() {
                    let (n, e) = rsa_pkcs1(key)
                        .ok_or_else(|| JwtError::Key("invalid RSA public key".into()))?;
                    JwtKey::rsa_components(alg, n, e)
                } else if alg.is_hmac() {
                    Err(JwtError::Key("an HMAC key is not a public key".into()))
                } else {
                    Ok(JwtKey {
                        alg,
                        kid: None,
                        material: Material::Raw(key.to_vec()),
                    })
                }
            }
            other => Err(JwtError::Key(format!("unsupported PEM label '{}'", other))),
        }
    }

    /// A key from a JWK (`kty` `RSA`, `EC`, `OKP` or `oct`; `alg` is required except for `EC` / `OKP`).
    pub fn from_jwk(jwk: &serde_json::Value) -> Result<JwtKey, JwtError> {
        let s = |k: &str| jwk.get(k).and_then(|v| v.as_str());
        let b = |k: &str| {
            s(k).and_then(|v| crate::b64::decode(v.as_bytes()))
                .ok_or_else(|| {
                    JwtError::Key(format!("JWK member '{}' is missing or not base64url", k))
                })
        };
        let alg_of = |default: Option<Algorithm>| {
            s("alg")
                .map(|a| {
                    Algorithm::from_name(a)
                        .ok_or_else(|| JwtError::Key(format!("unsupported alg '{}'", a)))
                })
                .transpose()?
                .or(default)
                .ok_or_else(|| JwtError::Key("JWK without alg".into()))
        };
        let mut key = match s("kty") {
            Some("RSA") => {
                JwtKey::rsa_components(alg_of(Some(Algorithm::RS256))?, &b("n")?, &b("e")?)?
            }
            Some("EC") => {
                let alg = match s("crv") {
                    Some("P-256") => Algorithm::ES256,
                    Some("P-384") => Algorithm::ES384,
                    c => return Err(JwtError::Key(format!("unsupported EC curve {:?}", c))),
                };
                let mut point = vec![4u8];
                point.extend(b("x")?);
                point.extend(b("y")?);
                JwtKey {
                    alg: alg_of(Some(alg))?,
                    kid: None,
                    material: Material::Raw(point),
                }
            }
            Some("OKP") if s("crv") == Some("Ed25519") => JwtKey {
                alg: Algorithm::EdDSA,
                kid: None,
                material: Material::Raw(b("x")?),
            },
            Some("oct") => JwtKey::hmac(alg_of(None)?, &b("k")?)?,
            k => return Err(JwtError::Key(format!("unsupported JWK kty {:?}", k))),
        };
        key.kid = s("kid").map(str::to_string);
        Ok(key)
    }

    /// Sets the `kid`.
    pub fn with_kid(mut self, kid: impl Into<String>) -> JwtKey {
        self.kid = Some(kid.into());
        self
    }

    /// The algorithm.
    pub fn algorithm(&self) -> Algorithm {
        self.alg
    }

    fn verify(&self, msg: &[u8], sig: &[u8]) -> bool {
        match &self.material {
            Material::Hmac(secret) => {
                let a = match self.alg {
                    Algorithm::HS256 => hmac::HMAC_SHA256,
                    Algorithm::HS384 => hmac::HMAC_SHA384,
                    _ => hmac::HMAC_SHA512,
                };
                hmac::verify(&hmac::Key::new(a, secret), msg, sig).is_ok()
            }
            Material::Rsa { n, e } => {
                let params: &signature::RsaParameters = match self.alg {
                    Algorithm::RS256 => &signature::RSA_PKCS1_2048_8192_SHA256,
                    Algorithm::RS384 => &signature::RSA_PKCS1_2048_8192_SHA384,
                    Algorithm::RS512 => &signature::RSA_PKCS1_2048_8192_SHA512,
                    Algorithm::PS256 => &signature::RSA_PSS_2048_8192_SHA256,
                    Algorithm::PS384 => &signature::RSA_PSS_2048_8192_SHA384,
                    _ => &signature::RSA_PSS_2048_8192_SHA512,
                };
                signature::RsaPublicKeyComponents {
                    n: &n[..],
                    e: &e[..],
                }
                .verify(params, msg, sig)
                .is_ok()
            }
            Material::Raw(k) => {
                let a: &dyn signature::VerificationAlgorithm = match self.alg {
                    Algorithm::ES256 => &signature::ECDSA_P256_SHA256_FIXED,
                    Algorithm::ES384 => &signature::ECDSA_P384_SHA384_FIXED,
                    _ => &signature::ED25519,
                };
                signature::UnparsedPublicKey::new(a, k)
                    .verify(msg, sig)
                    .is_ok()
            }
        }
    }
}

fn strip_zeros(b: &[u8]) -> &[u8] {
    let i = b
        .iter()
        .position(|&x| x != 0)
        .unwrap_or(b.len().saturating_sub(1));
    &b[i..]
}

// ---- DER / PEM（公開鍵を取り出すのに要る分だけ） ----

/// One DER element: (tag, contents, rest).
fn der(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&l, rest) = rest.split_first()?;
    let (len, rest) = if l < 0x80 {
        (l as usize, rest)
    } else {
        let n = (l & 0x7f) as usize;
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let len = rest[..n].iter().fold(0usize, |a, &b| a << 8 | b as usize);
        (len, &rest[n..])
    };
    if rest.len() < len {
        return None;
    }
    Some((tag, &rest[..len], &rest[len..]))
}

/// The key bits of a SubjectPublicKeyInfo.
fn spki_key(d: &[u8]) -> Option<&[u8]> {
    let (t, seq, _) = der(d)?;
    if t != 0x30 {
        return None;
    }
    let (t, _alg, rest) = der(seq)?;
    if t != 0x30 {
        return None;
    }
    let (t, bits, _) = der(rest)?;
    if t != 0x03 || bits.first() != Some(&0) {
        return None;
    }
    Some(&bits[1..])
}

/// `(n, e)` of a PKCS#1 RSAPublicKey.
fn rsa_pkcs1(d: &[u8]) -> Option<(&[u8], &[u8])> {
    let (t, seq, _) = der(d)?;
    if t != 0x30 {
        return None;
    }
    let (t, n, rest) = der(seq)?;
    if t != 0x02 {
        return None;
    }
    let (t, e, _) = der(rest)?;
    if t != 0x02 {
        return None;
    }
    Some((n, e))
}

fn pem_decode(pem: &str) -> Option<(String, Vec<u8>)> {
    let start = pem.find("-----BEGIN ")?;
    let after = &pem[start + 11..];
    let label_end = after.find("-----")?;
    let label = after[..label_end].to_string();
    let body_start = &after[label_end + 5..];
    let end = body_start.find("-----END ")?;
    let b64: Vec<u8> = body_start[..end]
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    Some((label, crate::b64::decode(&b64)?))
}

// ---- 検証 ----

/// Verifies tokens with a set of keys and the expected `iss` / `aud`. Register it with `App::state(verifier)` for
/// the [`Jwt`] extractor. The keys can be replaced at run time ([`JwtVerifier::set_keys`], [`JwtVerifier::refresh_jwks`]).
pub struct JwtVerifier {
    keys: ArcSwap<Vec<JwtKey>>,
    issuer: Option<String>,
    audience: Option<String>,
    leeway: u64,
}

impl std::fmt::Debug for JwtVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtVerifier")
            .field("keys", &self.keys.load().len())
            .field("issuer", &self.issuer)
            .field("audience", &self.audience)
            .finish()
    }
}

impl JwtVerifier {
    /// A verifier with these keys (60 s of clock leeway for `exp` and `nbf`).
    pub fn new(keys: impl IntoIterator<Item = JwtKey>) -> JwtVerifier {
        JwtVerifier {
            keys: ArcSwap::from_pointee(keys.into_iter().collect()),
            issuer: None,
            audience: None,
            leeway: 60,
        }
    }

    /// A verifier with the keys of a JWK Set (`{"keys": [...]}`; keys it cannot use are skipped).
    pub fn from_jwks(jwks: &serde_json::Value) -> Result<JwtVerifier, JwtError> {
        Ok(JwtVerifier::new(parse_jwks(jwks)?))
    }

    /// Requires `iss` to equal `issuer`.
    pub fn issuer(mut self, issuer: impl Into<String>) -> JwtVerifier {
        self.issuer = Some(issuer.into());
        self
    }

    /// Requires `aud` to contain `audience`.
    pub fn audience(mut self, audience: impl Into<String>) -> JwtVerifier {
        self.audience = Some(audience.into());
        self
    }

    /// Clock leeway in seconds (default 60).
    pub fn leeway(mut self, secs: u64) -> JwtVerifier {
        self.leeway = secs;
        self
    }

    /// Replaces the keys (requests in progress finish with the old ones).
    pub fn set_keys(&self, keys: Vec<JwtKey>) {
        self.keys.store(Arc::new(keys));
    }

    /// Fetches a JWK Set from `url` with `client` and replaces the keys (run it at startup and periodically, for
    /// example with `veil_kit::tasks::Tasks::every`).
    pub async fn refresh_jwks(
        &self,
        client: &veil::api::client::Client,
        url: &str,
    ) -> Result<usize, JwtError> {
        let res = client
            .get(url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| JwtError::Key(format!("fetching {}: {}", url, e)))?;
        let v: serde_json::Value = res
            .json()
            .map_err(|e| JwtError::Key(format!("JWKS from {}: {}", url, e)))?;
        let keys = parse_jwks(&v)?;
        let n = keys.len();
        self.set_keys(keys);
        Ok(n)
    }

    /// Verifies `token` and deserializes its claims.
    pub fn verify<C: DeserializeOwned>(&self, token: &str) -> Result<C, JwtError> {
        let mut parts = token.splitn(3, '.');
        let (h, p, s) = match (parts.next(), parts.next(), parts.next()) {
            (Some(h), Some(p), Some(s)) if !s.contains('.') => (h, p, s),
            _ => return Err(JwtError::Malformed),
        };
        let header: Header = crate::b64::decode(h.as_bytes())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .ok_or(JwtError::Malformed)?;
        let sig = crate::b64::decode(s.as_bytes()).ok_or(JwtError::Malformed)?;
        let alg = Algorithm::from_name(&header.alg).ok_or(JwtError::UnknownKey)?;
        let keys = self.keys.load();
        let signed = &token[..h.len() + 1 + p.len()];
        let mut candidates = keys
            .iter()
            .filter(|k| k.alg == alg && (header.kid.is_none() || k.kid == header.kid))
            .peekable();
        if candidates.peek().is_none() {
            return Err(JwtError::UnknownKey);
        }
        if !candidates.any(|k| k.verify(signed.as_bytes(), &sig)) {
            return Err(JwtError::BadSignature);
        }
        let payload = crate::b64::decode(p.as_bytes()).ok_or(JwtError::Malformed)?;
        let claims: serde_json::Value =
            serde_json::from_slice(&payload).map_err(|_| JwtError::Malformed)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Some(exp) = claims.get("exp").and_then(|v| v.as_f64()) {
            if (now as f64) > exp + self.leeway as f64 {
                return Err(JwtError::Expired);
            }
        }
        if let Some(nbf) = claims.get("nbf").and_then(|v| v.as_f64()) {
            if (now as f64) + (self.leeway as f64) < nbf {
                return Err(JwtError::NotYetValid);
            }
        }
        if let Some(iss) = &self.issuer {
            if claims.get("iss").and_then(|v| v.as_str()) != Some(iss.as_str()) {
                return Err(JwtError::BadIssuer);
            }
        }
        if let Some(aud) = &self.audience {
            let ok = match claims.get("aud") {
                Some(serde_json::Value::String(a)) => a == aud,
                Some(serde_json::Value::Array(a)) => {
                    a.iter().any(|x| x.as_str() == Some(aud.as_str()))
                }
                _ => false,
            };
            if !ok {
                return Err(JwtError::BadAudience);
            }
        }
        serde_json::from_value(claims).map_err(|e| JwtError::Claims(e.to_string()))
    }
}

fn parse_jwks(v: &serde_json::Value) -> Result<Vec<JwtKey>, JwtError> {
    let arr = v
        .get("keys")
        .and_then(|k| k.as_array())
        .ok_or_else(|| JwtError::Key("JWKS without 'keys'".into()))?;
    Ok(arr
        .iter()
        .filter(|k| {
            k.get("use")
                .and_then(|u| u.as_str())
                .is_none_or(|u| u == "sig")
        })
        .filter_map(|k| match JwtKey::from_jwk(k) {
            Ok(k) => Some(k),
            Err(e) => {
                ftlog::warn!("skipping a JWK: {}", e);
                None
            }
        })
        .collect())
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    #[serde(default)]
    kid: Option<String>,
}

/// Signs `claims` with an HMAC key (for issuing tokens, for example from a `/token` endpoint).
pub fn encode_hmac<C: Serialize>(key: &JwtKey, claims: &C) -> Result<String, JwtError> {
    let Material::Hmac(secret) = &key.material else {
        return Err(JwtError::Key("encode_hmac needs an HMAC key".into()));
    };
    let mut header = serde_json::json!({ "alg": key.alg.name(), "typ": "JWT" });
    if let Some(kid) = &key.kid {
        header["kid"] = serde_json::Value::String(kid.clone());
    }
    let mut out = crate::b64::encode(header.to_string().as_bytes());
    out.push('.');
    out.push_str(&crate::b64::encode(
        &serde_json::to_vec(claims).map_err(|e| JwtError::Claims(e.to_string()))?,
    ));
    let a = match key.alg {
        Algorithm::HS256 => hmac::HMAC_SHA256,
        Algorithm::HS384 => hmac::HMAC_SHA384,
        _ => hmac::HMAC_SHA512,
    };
    let tag = hmac::sign(&hmac::Key::new(a, secret), out.as_bytes());
    out.push('.');
    out.push_str(&crate::b64::encode(tag.as_ref()));
    Ok(out)
}

/// The registered claims (RFC 7519 section 4.1), `scope` and the remaining claims.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Claims {
    /// `sub`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub: Option<String>,
    /// `iss`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iss: Option<String>,
    /// `aud` (a string or an array).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aud: Option<serde_json::Value>,
    /// `exp` (seconds since the epoch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exp: Option<u64>,
    /// `nbf`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nbf: Option<u64>,
    /// `iat`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iat: Option<u64>,
    /// `jti`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jti: Option<String>,
    /// `scope` (space-separated, RFC 8693).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Every other claim.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Claims {
    /// A [`Principal`](super::Principal): `sub`, the scopes of `scope`, and `roles` (an array of strings).
    pub fn to_principal(&self) -> super::Principal {
        let roles = self
            .extra
            .get("roles")
            .and_then(|r| r.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        super::Principal {
            subject: self.sub.clone().unwrap_or_default(),
            scopes: self
                .scope
                .as_deref()
                .map(|s| s.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default(),
            roles,
            attributes: serde_json::to_value(self).unwrap_or_default(),
        }
    }
}

/// A verified token from `Authorization: Bearer` (needs a [`JwtVerifier`] in `App::state`). 401 with
/// `WWW-Authenticate: Bearer error="invalid_token"` when it is missing or invalid.
#[derive(Debug, Clone)]
pub struct Jwt<C = Claims>(pub C);

impl<C: DeserializeOwned> FromRequest for Jwt<C> {
    fn from_request(req: &Request) -> Result<Self, Response> {
        let token = super::BearerToken::from_headers(req)?;
        let v = req.state::<JwtVerifier>().ok_or_else(|| {
            ftlog::error!("the Jwt extractor needs a veil_kit::auth::jwt::JwtVerifier registered with App::state");
            Response::error(500, "Internal Server Error")
        })?;
        v.verify::<C>(&token.0).map(Jwt).map_err(|e| {
            Response::error(401, "Unauthorized").with_header(
                "www-authenticate",
                format!(
                    "Bearer error=\"invalid_token\", error_description=\"{}\"",
                    e
                ),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn claims(exp: u64) -> Claims {
        Claims {
            sub: Some("alice".into()),
            iss: Some("https://iss".into()),
            aud: Some(serde_json::json!(["api", "other"])),
            exp: Some(exp),
            scope: Some("read write".into()),
            ..Default::default()
        }
    }

    #[test]
    fn hmac_roundtrip_and_checks() {
        let key = JwtKey::hs256(b"0123456789abcdef0123456789abcdef").with_kid("k1");
        let t = encode_hmac(&key, &claims(now() + 300)).unwrap();
        let v = JwtVerifier::new([key.clone()])
            .issuer("https://iss")
            .audience("api");
        let c: Claims = v.verify(&t).unwrap();
        assert_eq!(c.sub.as_deref(), Some("alice"));
        let p = c.to_principal();
        assert!(p.has_scope("write"));
        // 期限切れ・発行者・受け手・改ざん・別の鍵
        let old = encode_hmac(&key, &claims(now() - 3600)).unwrap();
        assert_eq!(v.verify::<Claims>(&old).unwrap_err(), JwtError::Expired);
        let v2 = JwtVerifier::new([key.clone()]).issuer("https://other");
        assert_eq!(v2.verify::<Claims>(&t).unwrap_err(), JwtError::BadIssuer);
        let v3 = JwtVerifier::new([key.clone()]).audience("nope");
        assert_eq!(v3.verify::<Claims>(&t).unwrap_err(), JwtError::BadAudience);
        let mut parts: Vec<&str> = t.split('.').collect();
        let forged = crate::b64::encode(br#"{"sub":"admin","exp":99999999999}"#);
        parts[1] = &forged;
        assert_eq!(
            v.verify::<Claims>(&parts.join(".")).unwrap_err(),
            JwtError::BadSignature
        );
        let other =
            JwtVerifier::new([JwtKey::hs256(b"another-secret-another-secret-xx").with_kid("k1")]);
        assert_eq!(
            other.verify::<Claims>(&t).unwrap_err(),
            JwtError::BadSignature
        );
        let wrong_kid =
            JwtVerifier::new([JwtKey::hs256(b"0123456789abcdef0123456789abcdef").with_kid("k2")]);
        assert_eq!(
            wrong_kid.verify::<Claims>(&t).unwrap_err(),
            JwtError::UnknownKey
        );
        assert_eq!(v.verify::<Claims>("a.b").unwrap_err(), JwtError::Malformed);
    }

    #[test]
    fn alg_none_and_confusion_are_rejected() {
        let key = JwtKey::hs256(b"0123456789abcdef0123456789abcdef");
        let v = JwtVerifier::new([key]);
        let h = crate::b64::encode(br#"{"alg":"none"}"#);
        let p = crate::b64::encode(br#"{"sub":"x"}"#);
        assert_eq!(
            v.verify::<Claims>(&format!("{}.{}.", h, p)).unwrap_err(),
            JwtError::UnknownKey
        );
        let h = crate::b64::encode(br#"{"alg":"RS256"}"#);
        assert_eq!(
            v.verify::<Claims>(&format!("{}.{}.AAAA", h, p))
                .unwrap_err(),
            JwtError::UnknownKey
        );
    }

    #[cfg(not(any(target_os = "openbsd", target_os = "netbsd")))]
    fn sign_with(alg: &str, sign: impl Fn(&[u8]) -> Vec<u8>, kid: &str) -> String {
        let h = crate::b64::encode(format!(r#"{{"alg":"{}","kid":"{}"}}"#, alg, kid).as_bytes());
        let p = crate::b64::encode(
            serde_json::to_string(&claims(now() + 60))
                .unwrap()
                .as_bytes(),
        );
        let msg = format!("{}.{}", h, p);
        let sig = sign(msg.as_bytes());
        format!("{}.{}", msg, crate::b64::encode(&sig))
    }

    #[cfg(not(any(target_os = "openbsd", target_os = "netbsd")))]
    #[test]
    fn asymmetric_algorithms() {
        use aws_lc_rs::rand::SystemRandom;
        use aws_lc_rs::signature::{self as sig, KeyPair};
        let rng = SystemRandom::new();

        // ES256（JWK と SPKI の PEM の両方から）
        let pk =
            sig::EcdsaKeyPair::generate_pkcs8(&sig::ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let kp = sig::EcdsaKeyPair::from_pkcs8(&sig::ECDSA_P256_SHA256_FIXED_SIGNING, pk.as_ref())
            .unwrap();
        let point = kp.public_key().as_ref().to_vec();
        let t = sign_with(
            "ES256",
            |m| kp.sign(&rng, m).unwrap().as_ref().to_vec(),
            "ec",
        );
        let jwk = serde_json::json!({"kty":"EC","crv":"P-256","kid":"ec",
            "x": crate::b64::encode(&point[1..33]), "y": crate::b64::encode(&point[33..])});
        let v = JwtVerifier::from_jwks(&serde_json::json!({"keys":[jwk]})).unwrap();
        assert_eq!(
            v.verify::<Claims>(&t).unwrap().sub.as_deref(),
            Some("alice")
        );
        // SPKI: P-256 の AlgorithmIdentifier + BIT STRING
        let mut spki = vec![
            0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06,
            0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
        ];
        spki.extend(&point);
        let pem = format!(
            "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
            crate::b64::encode_std(&spki)
        );
        let k = JwtKey::from_pem(Algorithm::ES256, &pem)
            .unwrap()
            .with_kid("ec");
        assert_eq!(
            JwtVerifier::new([k])
                .verify::<Claims>(&t)
                .unwrap()
                .sub
                .as_deref(),
            Some("alice")
        );

        // EdDSA
        let pk = sig::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let kp = sig::Ed25519KeyPair::from_pkcs8(pk.as_ref()).unwrap();
        let t = sign_with("EdDSA", |m| kp.sign(m).as_ref().to_vec(), "ed");
        let jwk = serde_json::json!({"kty":"OKP","crv":"Ed25519","kid":"ed","x": crate::b64::encode(kp.public_key().as_ref())});
        let v = JwtVerifier::from_jwks(&serde_json::json!({"keys":[jwk]})).unwrap();
        assert!(v.verify::<Claims>(&t).is_ok());

        // RS256・PS256（PKCS#1 の DER から n・e を取り出す）
        let kp = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
        let der = kp.public_key().as_ref().to_vec();
        let (n, e) = rsa_pkcs1(&der).unwrap();
        for (alg, pad) in [
            ("RS256", &sig::RSA_PKCS1_SHA256 as &dyn sig::RsaEncoding),
            ("PS256", &sig::RSA_PSS_SHA256),
        ] {
            let t = sign_with(
                alg,
                |m| {
                    let mut s = vec![0u8; kp.public_modulus_len()];
                    kp.sign(pad, &rng, m, &mut s).unwrap();
                    s
                },
                "rsa",
            );
            let k = JwtKey::rsa_components(Algorithm::from_name(alg).unwrap(), n, e)
                .unwrap()
                .with_kid("rsa");
            assert!(
                JwtVerifier::new([k]).verify::<Claims>(&t).is_ok(),
                "{}",
                alg
            );
        }
        let pem = format!(
            "-----BEGIN RSA PUBLIC KEY-----\n{}\n-----END RSA PUBLIC KEY-----\n",
            crate::b64::encode_std(&der)
        );
        assert!(JwtKey::from_pem(Algorithm::RS256, &pem).is_ok());
    }
}
