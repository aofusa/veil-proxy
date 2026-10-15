//! JWT の検証（F-220。Envoy の `jwt_authn` フィルタ＝Istio の RequestAuthentication）。
//!
//! istiod は JWKS を取得してインラインで渡す（`local_jwks.inline_string`）ので、ここでは取りに行かない。
//! 検証できたトークンの payload は RBAC のメタデータ（`envoy.filters.http.jwt_authn` の `payload`）になる。
//! 署名の検証は aws-lc（RS256/384/512・PS256/384/512・ES256/384）。

use super::xds::proto::{self as p};
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// proto（envoy.extensions.filters.http.jwt_authn.v3）
// ---------------------------------------------------------------------------

pub const T_JWT: &str = "type.googleapis.com/envoy.extensions.filters.http.jwt_authn.v3.JwtAuthentication";

#[derive(Clone, PartialEq, prost::Message)]
pub struct JwtAuthentication {
    #[prost(map = "string, message", tag = "1")]
    pub providers: HashMap<String, JwtProviderProto>,
    #[prost(message, repeated, tag = "2")]
    pub rules: Vec<RequirementRule>,
    #[prost(bool, tag = "4")]
    pub bypass_cors_preflight: bool,
    #[prost(map = "string, message", tag = "5")]
    pub requirement_map: HashMap<String, JwtRequirement>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct JwtProviderProto {
    #[prost(string, tag = "1")]
    pub issuer: String,
    #[prost(string, repeated, tag = "2")]
    pub audiences: Vec<String>,
    #[prost(bool, tag = "20")]
    pub require_expiration: bool,
    #[prost(bool, tag = "5")]
    pub forward: bool,
    #[prost(message, repeated, tag = "6")]
    pub from_headers: Vec<JwtHeader>,
    #[prost(string, repeated, tag = "7")]
    pub from_params: Vec<String>,
    #[prost(string, repeated, tag = "13")]
    pub from_cookies: Vec<String>,
    #[prost(string, tag = "8")]
    pub forward_payload_header: String,
    #[prost(string, tag = "9")]
    pub payload_in_metadata: String,
    #[prost(message, optional, tag = "18")]
    pub normalize_payload_in_metadata: Option<NormalizePayload>,
    #[prost(uint32, tag = "10")]
    pub clock_skew_seconds: u32,
    #[prost(message, repeated, tag = "15")]
    pub claim_to_headers: Vec<JwtClaimToHeader>,
    #[prost(message, optional, tag = "4")]
    pub local_jwks: Option<p::DataSource>,
    #[prost(message, optional, tag = "3")]
    pub remote_jwks: Option<p::Empty>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct NormalizePayload {
    #[prost(string, repeated, tag = "1")]
    pub space_delimited_claims: Vec<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct JwtHeader {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub value_prefix: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct JwtClaimToHeader {
    #[prost(string, tag = "1")]
    pub header_name: String,
    #[prost(string, tag = "2")]
    pub claim_name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RequirementRule {
    #[prost(message, optional, tag = "1")]
    pub r#match: Option<p::RouteMatch>,
    #[prost(message, optional, tag = "2")]
    pub requires: Option<JwtRequirement>,
    #[prost(string, tag = "3")]
    pub requirement_name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct JwtRequirement {
    #[prost(oneof = "jwt_requirement::Type", tags = "1, 2, 3, 4, 5, 6")]
    pub r#type: Option<jwt_requirement::Type>,
}

pub mod jwt_requirement {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ProviderWithAudiences {
        #[prost(string, tag = "1")]
        pub provider_name: String,
        #[prost(string, repeated, tag = "2")]
        pub audiences: Vec<String>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct List {
        #[prost(message, repeated, tag = "1")]
        pub requirements: Vec<super::JwtRequirement>,
    }
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Type {
        #[prost(string, tag = "1")]
        ProviderName(String),
        #[prost(message, tag = "2")]
        ProviderAndAudiences(ProviderWithAudiences),
        #[prost(message, tag = "3")]
        RequiresAny(List),
        #[prost(message, tag = "4")]
        RequiresAll(List),
        #[prost(message, tag = "5")]
        AllowMissingOrFailed(super::p::Empty),
        #[prost(message, tag = "6")]
        AllowMissing(super::p::Empty),
    }
}

// ---------------------------------------------------------------------------
// モデル
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum Key {
    Rsa { n: Vec<u8>, e: Vec<u8> },
    Ec256(Vec<u8>),
    Ec384(Vec<u8>),
}

#[derive(Debug)]
pub struct Jwk {
    pub kid: Option<String>,
    pub alg: Option<String>,
    pub key: Key,
}

#[derive(Debug)]
pub struct Provider {
    pub name: String,
    pub issuer: String,
    pub audiences: Vec<String>,
    pub require_expiration: bool,
    pub forward: bool,
    /// (小文字のヘッダ名, 接頭辞)
    pub from_headers: Vec<(String, String)>,
    pub from_params: Vec<String>,
    pub payload_in_metadata: String,
    pub space_delimited: Vec<String>,
    pub clock_skew: u64,
    pub claim_to_headers: Vec<(Bytes, String)>,
    pub keys: Vec<Jwk>,
}

#[derive(Debug, Clone)]
pub enum Requirement {
    Provider(String, Vec<String>),
    Any(Vec<Requirement>),
    All(Vec<Requirement>),
    AllowMissing,
    AllowMissingOrFailed,
}

#[derive(Debug)]
pub struct JwtCfg {
    pub providers: HashMap<String, Arc<Provider>>,
    /// (パスの接頭辞, 要件)。先に一致したものを使う
    pub rules: Vec<(super::model::PathMatch, Requirement)>,
    pub bypass_cors_preflight: bool,
}

fn b64url(s: &str) -> Option<Vec<u8>> {
    super::httpcore::base64_decode(s.as_bytes())
}

fn parse_jwks(text: &str) -> Vec<Jwk> {
    let v: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let keys = v.get("keys").and_then(|k| k.as_array()).cloned().unwrap_or_default();
    keys.iter()
        .filter_map(|k| {
            let kid = k.get("kid").and_then(|x| x.as_str()).map(|s| s.to_string());
            let alg = k.get("alg").and_then(|x| x.as_str()).map(|s| s.to_string());
            let key = match k.get("kty")?.as_str()? {
                "RSA" => Key::Rsa {
                    n: b64url(k.get("n")?.as_str()?)?,
                    e: b64url(k.get("e")?.as_str()?)?,
                },
                "EC" => {
                    let x = b64url(k.get("x")?.as_str()?)?;
                    let y = b64url(k.get("y")?.as_str()?)?;
                    let mut pt = vec![4u8];
                    pt.extend_from_slice(&x);
                    pt.extend_from_slice(&y);
                    match k.get("crv")?.as_str()? {
                        "P-256" => Key::Ec256(pt),
                        "P-384" => Key::Ec384(pt),
                        _ => return None,
                    }
                }
                _ => return None,
            };
            Some(Jwk { kid, alg, key })
        })
        .collect()
}

fn requirement(r: &JwtRequirement, map: &HashMap<String, JwtRequirement>) -> Result<Requirement, String> {
    use jwt_requirement::Type as T;
    Ok(match &r.r#type {
        Some(T::ProviderName(n)) => Requirement::Provider(n.clone(), Vec::new()),
        Some(T::ProviderAndAudiences(pa)) => Requirement::Provider(pa.provider_name.clone(), pa.audiences.clone()),
        Some(T::RequiresAny(l)) => Requirement::Any(l.requirements.iter().map(|x| requirement(x, map)).collect::<Result<_, _>>()?),
        Some(T::RequiresAll(l)) => Requirement::All(l.requirements.iter().map(|x| requirement(x, map)).collect::<Result<_, _>>()?),
        Some(T::AllowMissingOrFailed(_)) => Requirement::AllowMissingOrFailed,
        Some(T::AllowMissing(_)) => Requirement::AllowMissing,
        None => Requirement::AllowMissingOrFailed,
    })
}

/// xDS の `JwtAuthentication` を変換する。
pub fn translate(any: &p::Any) -> Result<JwtCfg, String> {
    let ja: JwtAuthentication = p::decode_any(any, T_JWT).ok_or("invalid JwtAuthentication")?;
    let mut providers = HashMap::new();
    for (name, pp) in &ja.providers {
        if pp.remote_jwks.is_some() {
            return Err(format!("jwt provider {}: remote_jwks is not supported (istiod inlines JWKS)", name));
        }
        let jwks = pp
            .local_jwks
            .as_ref()
            .and_then(|d| d.inline())
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();
        let mut from_headers: Vec<(String, String)> = pp
            .from_headers
            .iter()
            .map(|h| (h.name.to_ascii_lowercase(), h.value_prefix.clone()))
            .collect();
        let mut from_params = pp.from_params.clone();
        if from_headers.is_empty() && from_params.is_empty() && pp.from_cookies.is_empty() {
            from_headers.push(("authorization".into(), "Bearer ".into()));
            from_params.push("access_token".into());
        }
        providers.insert(
            name.clone(),
            Arc::new(Provider {
                name: name.clone(),
                issuer: pp.issuer.clone(),
                audiences: pp.audiences.clone(),
                require_expiration: pp.require_expiration,
                forward: pp.forward,
                from_headers,
                from_params,
                payload_in_metadata: pp.payload_in_metadata.clone(),
                space_delimited: pp
                    .normalize_payload_in_metadata
                    .as_ref()
                    .map(|n| n.space_delimited_claims.clone())
                    .unwrap_or_default(),
                clock_skew: if pp.clock_skew_seconds == 0 { 60 } else { pp.clock_skew_seconds as u64 },
                claim_to_headers: pp
                    .claim_to_headers
                    .iter()
                    .map(|c| (Bytes::from(c.header_name.to_ascii_lowercase()), c.claim_name.clone()))
                    .collect(),
                keys: parse_jwks(&jwks),
            }),
        );
    }
    let mut rules = Vec::new();
    for r in &ja.rules {
        let path = match r.r#match.as_ref().and_then(|m| m.path_specifier.as_ref()) {
            Some(p::route_match::PathSpecifier::Prefix(s)) if s.is_empty() || s == "/" => super::model::PathMatch::Any,
            Some(p::route_match::PathSpecifier::Prefix(s)) => super::model::PathMatch::Prefix(s.clone()),
            Some(p::route_match::PathSpecifier::Path(s)) => super::model::PathMatch::Exact(s.clone()),
            Some(p::route_match::PathSpecifier::SafeRegex(re)) => super::model::PathMatch::Regex(
                regex::Regex::new(&re.regex).map_err(|e| format!("jwt rule regex: {e}"))?,
            ),
            _ => super::model::PathMatch::Any,
        };
        let req = match (&r.requires, r.requirement_name.as_str()) {
            (Some(rq), _) => requirement(rq, &ja.requirement_map)?,
            (None, "") => Requirement::AllowMissingOrFailed,
            (None, name) => requirement(
                ja.requirement_map.get(name).ok_or_else(|| format!("unknown requirement {}", name))?,
                &ja.requirement_map,
            )?,
        };
        rules.push((path, req));
    }
    Ok(JwtCfg {
        providers,
        rules,
        bypass_cors_preflight: ja.bypass_cors_preflight,
    })
}

// ---------------------------------------------------------------------------
// 検証
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fail {
    Missing,
    Invalid,
}

/// 検証の結果（RBAC のメタデータと、上流へのヘッダの変更）。
#[derive(Default)]
pub struct JwtOutcome {
    /// (メタデータのキー, payload)
    pub payloads: Vec<(String, serde_json::Value)>,
    pub remove_headers: Vec<Bytes>,
    pub add_headers: Vec<(Bytes, Bytes)>,
}

fn token_from(prov: &Provider, header: &dyn Fn(&str) -> Option<Bytes>, query: &[u8]) -> Option<(String, Option<Bytes>)> {
    for (name, prefix) in &prov.from_headers {
        if let Some(v) = header(name) {
            let s = String::from_utf8_lossy(&v).into_owned();
            let t = if prefix.is_empty() {
                Some(s.trim().to_string())
            } else if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
                Some(s[prefix.len()..].trim().to_string())
            } else {
                None
            };
            if let Some(t) = t.filter(|t| !t.is_empty()) {
                return Some((t, Some(Bytes::from(name.clone()))));
            }
        }
    }
    for param in &prov.from_params {
        if let Some(v) = super::model::query_value(query, param.as_bytes()) {
            if !v.is_empty() {
                return Some((String::from_utf8_lossy(v).into_owned(), None));
            }
        }
    }
    None
}

fn verify_sig(alg: &str, key: &Key, msg: &[u8], sig: &[u8]) -> bool {
    use aws_lc_rs::signature as s;
    match (alg, key) {
        ("RS256" | "RS384" | "RS512" | "PS256" | "PS384" | "PS512", Key::Rsa { n, e }) => {
            let params: &s::RsaParameters = match alg {
                "RS256" => &s::RSA_PKCS1_2048_8192_SHA256,
                "RS384" => &s::RSA_PKCS1_2048_8192_SHA384,
                "RS512" => &s::RSA_PKCS1_2048_8192_SHA512,
                "PS256" => &s::RSA_PSS_2048_8192_SHA256,
                "PS384" => &s::RSA_PSS_2048_8192_SHA384,
                _ => &s::RSA_PSS_2048_8192_SHA512,
            };
            s::RsaPublicKeyComponents { n: n.as_slice(), e: e.as_slice() }
                .verify(params, msg, sig)
                .is_ok()
        }
        ("ES256", Key::Ec256(pt)) => s::UnparsedPublicKey::new(&s::ECDSA_P256_SHA256_FIXED, pt)
            .verify(msg, sig)
            .is_ok(),
        ("ES384", Key::Ec384(pt)) => s::UnparsedPublicKey::new(&s::ECDSA_P384_SHA384_FIXED, pt)
            .verify(msg, sig)
            .is_ok(),
        _ => false,
    }
}

/// トークンを検証して payload を返す。
pub fn verify_token(prov: &Provider, token: &str, audiences: &[String], now: u64) -> Option<serde_json::Value> {
    let mut parts = token.split('.');
    let (h, pl, sg) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let header: serde_json::Value = serde_json::from_slice(&b64url(h)?).ok()?;
    let mut payload: serde_json::Value = serde_json::from_slice(&b64url(pl)?).ok()?;
    let sig = b64url(sg)?;
    let alg = header.get("alg")?.as_str()?;
    let kid = header.get("kid").and_then(|k| k.as_str());
    let msg = &token.as_bytes()[..h.len() + 1 + pl.len()];
    let ok = prov.keys.iter().any(|k| {
        let kid_ok = match (kid, &k.kid) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        };
        let alg_ok = k.alg.as_deref().is_none_or(|a| a == alg);
        kid_ok && alg_ok && verify_sig(alg, &k.key, msg, &sig)
    });
    if !ok {
        return None;
    }
    if !prov.issuer.is_empty() && payload.get("iss").and_then(|i| i.as_str()) != Some(prov.issuer.as_str()) {
        return None;
    }
    let skew = prov.clock_skew;
    match payload.get("exp").and_then(|e| e.as_u64()) {
        Some(exp) if exp + skew < now => return None,
        None if prov.require_expiration => return None,
        _ => {}
    }
    if let Some(nbf) = payload.get("nbf").and_then(|e| e.as_u64()) {
        if nbf > now + skew {
            return None;
        }
    }
    let want: &[String] = if audiences.is_empty() { &prov.audiences } else { audiences };
    if !want.is_empty() {
        let aud_ok = match payload.get("aud") {
            Some(serde_json::Value::String(a)) => want.iter().any(|w| w == a),
            Some(serde_json::Value::Array(a)) => a.iter().any(|x| x.as_str().is_some_and(|s| want.iter().any(|w| w == s))),
            _ => false,
        };
        if !aud_ok {
            return None;
        }
    }
    for c in &prov.space_delimited {
        if let Some(serde_json::Value::String(s)) = payload.get(c).cloned() {
            payload[c] = serde_json::Value::Array(
                s.split(' ').filter(|x| !x.is_empty()).map(|x| serde_json::Value::String(x.into())).collect(),
            );
        }
    }
    Some(payload)
}

fn eval(
    cfg: &JwtCfg,
    r: &Requirement,
    header: &dyn Fn(&str) -> Option<Bytes>,
    query: &[u8],
    now: u64,
    out: &mut JwtOutcome,
) -> Result<(), Fail> {
    match r {
        Requirement::AllowMissingOrFailed => {
            // 検証できるものは検証してメタデータにする（失敗は無視）
            for prov in cfg.providers.values() {
                let _ = eval(cfg, &Requirement::Provider(prov.name.clone(), Vec::new()), header, query, now, out);
            }
            Ok(())
        }
        Requirement::AllowMissing => {
            let any_token = cfg.providers.values().any(|p| token_from(p, header, query).is_some());
            if any_token {
                Err(Fail::Invalid)
            } else {
                Ok(())
            }
        }
        Requirement::Provider(name, auds) => {
            let prov = cfg.providers.get(name).ok_or(Fail::Invalid)?;
            let (tok, hdr) = token_from(prov, header, query).ok_or(Fail::Missing)?;
            let payload = verify_token(prov, &tok, auds, now).ok_or(Fail::Invalid)?;
            if !prov.forward {
                if let Some(h) = hdr {
                    out.remove_headers.push(h);
                }
            }
            for (h, claim) in &prov.claim_to_headers {
                if let Some(v) = payload.get(claim) {
                    let s = match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    out.add_headers.push((h.clone(), Bytes::from(s)));
                }
            }
            let key = if prov.payload_in_metadata.is_empty() {
                prov.issuer.clone()
            } else {
                prov.payload_in_metadata.clone()
            };
            out.payloads.push((key, payload));
            Ok(())
        }
        Requirement::Any(list) => {
            let mut last = Fail::Missing;
            for x in list {
                match eval(cfg, x, header, query, now, out) {
                    Ok(()) => return Ok(()),
                    Err(Fail::Invalid) => last = Fail::Invalid,
                    Err(Fail::Missing) => {}
                }
            }
            Err(last)
        }
        Requirement::All(list) => {
            for x in list {
                eval(cfg, x, header, query, now, out)?;
            }
            Ok(())
        }
    }
}

/// 要求を検証する。失敗は (ステータス, 本文)。
pub fn authenticate(
    cfg: &JwtCfg,
    method: &[u8],
    path: &[u8],
    header: &dyn Fn(&str) -> Option<Bytes>,
) -> Result<JwtOutcome, (u16, &'static str)> {
    let mut out = JwtOutcome::default();
    if cfg.bypass_cors_preflight && method == b"OPTIONS" && header("origin").is_some() {
        return Ok(out);
    }
    let (p, q) = match memchr::memchr(b'?', path) {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => (path, &b""[..]),
    };
    let rule = cfg.rules.iter().find(|(m, _)| match m {
        super::model::PathMatch::Any => true,
        super::model::PathMatch::Prefix(x) => p.starts_with(x.as_bytes()),
        super::model::PathMatch::Exact(x) => p == x.as_bytes(),
        super::model::PathMatch::Separated(x) => p.starts_with(x.as_bytes()),
        super::model::PathMatch::Regex(re) => std::str::from_utf8(p).is_ok_and(|s| re.is_match(s)),
        super::model::PathMatch::Connect => method == b"CONNECT",
    });
    let Some((_, req)) = rule else { return Ok(out) };
    let now = super::httpcore::unix_secs();
    match eval(cfg, req, header, q, now, &mut out) {
        Ok(()) => Ok(out),
        Err(Fail::Missing) => Err((401, "Jwt is missing")),
        Err(Fail::Invalid) => Err((401, "Jwt verification fails")),
    }
}

/// payload から RBAC のメタデータの値を取り出す（`path` は ["payload", "iss"] のような列）。
pub fn lookup<'a>(payloads: &'a [(String, serde_json::Value)], path: &[String]) -> Option<&'a serde_json::Value> {
    let (first, rest) = path.split_first()?;
    let mut v = &payloads.iter().find(|(k, _)| k == first)?.1;
    for k in rest {
        v = v.get(k)?;
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Istio の security/tools/jwt/samples の鍵（jwks.json）と groups-scope.jwt（groups: [group1, group2]）
    const JWKS: &str = r#"{ "keys":[ {"e":"AQAB","kid":"DHFbpoIUqrY8t2zpA2qXfCmr5VO5ZEr4RzHU_-envvQ","kty":"RSA","n":"xAE7eB6qugXyCAG3yhh7pkDkT65pHymX-P7KfIupjf59vsdo91bSP9C8H07pSAGQO1MV_xFj9VswgsCg4R6otmg5PV2He95lZdHtOcU5DXIg_pbhLdKXbi66GlVeK6ABZOUW3WYtnNHD-91gVuoeJT_DwtGGcp4ignkgXfkiEm4sw-4sfb4qdt5oLbyVpmW6x9cfa7vs2WTfURiCrBoUqgBo_-4WTiULmmHSGZHOjzwa8WtrtOQGsAFjIbno85jp6MnGGGZPYZbDAa_b3y5u-YpW7ypZrvD8BgtKVjgtQgZhLAGezMt0ua3DRrWnKqTZ0BJ_EyxOGuHJrLsn00fnMQ"}]}"#;
    const TOKEN_GROUP: &str = "eyJhbGciOiJSUzI1NiIsImtpZCI6IkRIRmJwb0lVcXJZOHQyenBBMnFYZkNtcjVWTzVaRXI0UnpIVV8tZW52dlEiLCJ0eXAiOiJKV1QifQ.eyJleHAiOjM1MzczOTExMDQsImdyb3VwcyI6WyJncm91cDEiLCJncm91cDIiXSwiaWF0IjoxNTM3MzkxMTA0LCJpc3MiOiJ0ZXN0aW5nQHNlY3VyZS5pc3Rpby5pbyIsInNjb3BlIjpbInNjb3BlMSIsInNjb3BlMiJdLCJzdWIiOiJ0ZXN0aW5nQHNlY3VyZS5pc3Rpby5pbyJ9.EdJnEZSH6X8hcyEii7c8H5lnhgjB5dwo07M5oheC8Xz8mOllyg--AHCFWHybM48reunF--oGaG6IXVngCEpVF0_P5DwsUoBgpPmK1JOaKN6_pe9sh0ZwTtdgK_RP01PuI7kUdbOTlkuUi2AO-qUyOm7Art2POzo36DLQlUXv8Ad7NBOqfQaKjE9ndaPWT7aexUsBHxmgiGbz1SyLH879f7uHYPbPKlpHU6P9S-DaKnGLaEchnoKnov7ajhrEhGXAQRukhDPKUHO9L30oPIr5IJllEQfHYtt6IZvlNUGeLUcif3wpry1R5tBXRicx2sXMQ7LyuDremDbcNy_iE76Upg";

    fn cfg() -> JwtCfg {
        let prov = Provider {
            name: "origins-0".into(),
            issuer: "testing@secure.istio.io".into(),
            audiences: vec![],
            require_expiration: false,
            forward: true,
            from_headers: vec![("authorization".into(), "Bearer ".into())],
            from_params: vec!["access_token".into()],
            payload_in_metadata: "payload".into(),
            space_delimited: vec!["scope".into()],
            clock_skew: 60,
            claim_to_headers: vec![],
            keys: parse_jwks(JWKS),
        };
        let mut providers = HashMap::new();
        providers.insert("origins-0".into(), Arc::new(prov));
        JwtCfg {
            providers,
            rules: vec![(
                super::super::model::PathMatch::Any,
                Requirement::Any(vec![
                    Requirement::Provider("origins-0".into(), vec![]),
                    Requirement::AllowMissing,
                ]),
            )],
            bypass_cors_preflight: true,
        }
    }

    #[test]
    fn verifies_istio_sample_token() {
        let c = cfg();
        assert_eq!(c.providers["origins-0"].keys.len(), 1);
        let auth = format!("Bearer {}", TOKEN_GROUP);
        let h = |n: &str| (n == "authorization").then(|| Bytes::from(auth.clone()));
        let out = authenticate(&c, b"GET", b"/", &h).unwrap();
        let groups = lookup(&out.payloads, &["payload".into(), "groups".into()]).unwrap();
        assert_eq!(groups, &serde_json::json!(["group1", "group2"]));
        // 無し: allow_missing で通る
        let none = |_: &str| None;
        assert!(authenticate(&c, b"GET", b"/", &none).is_ok());
        // 改ざん: 401
        let bad = format!("Bearer {}x", TOKEN_GROUP);
        let hb = |n: &str| (n == "authorization").then(|| Bytes::from(bad.clone()));
        assert_eq!(authenticate(&c, b"GET", b"/", &hb).err(), Some((401, "Jwt verification fails")));
    }
}
