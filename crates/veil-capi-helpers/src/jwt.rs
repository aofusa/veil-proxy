// F-291: JWT（veil_kit::auth::jwt。検査・発行の規則は Rust の kit と同じ: alg は鍵で固定、none は受けない、exp は既定で必須）。
use crate::auth::register_guard;
use crate::{put_string, tryc, VEIL_KIT_ERR_TOKEN};
use kit::auth::jwt::{encode_hmac, Algorithm, Claims, JwtKey, JwtVerifier};
use kit::auth::BearerToken;
use std::ffi::{c_char, c_int, c_void};
use std::sync::{Arc, Mutex, OnceLock};
use veil::api::Response;
use veil_capi::internal::{self, cstr, fail, opt_cstr};
use veil_capi::{
    veil_config_t, veil_request_t, veil_response_t, VEIL_ERR_INVALID_ARG, VEIL_ERR_STATE,
    VEIL_MW_CONTINUE, VEIL_MW_STOP, VEIL_OK,
};

#[derive(Default)]
struct Settings {
    keys: Vec<JwtKey>,
    issuer: Option<String>,
    audience: Option<String>,
    require_exp: Option<bool>,
    leeway: Option<u64>,
}

/// A token verifier. Configure it before the first verification or `veil_kit_jwt_authenticate` (after that only the keys can change).
#[allow(non_camel_case_types)]
pub struct veil_kit_jwt_verifier_t {
    settings: Mutex<Settings>,
    built: OnceLock<Arc<JwtVerifier>>,
}

impl veil_kit_jwt_verifier_t {
    fn verifier(&self) -> Arc<JwtVerifier> {
        self.built
            .get_or_init(|| {
                let s =
                    std::mem::take(&mut *self.settings.lock().unwrap_or_else(|e| e.into_inner()));
                let mut v = JwtVerifier::new(s.keys);
                if let Some(i) = s.issuer {
                    v = v.issuer(i);
                }
                if let Some(a) = s.audience {
                    v = v.audience(a);
                }
                if let Some(on) = s.require_exp {
                    v = v.require_exp(on);
                }
                if let Some(l) = s.leeway {
                    v = v.leeway(l);
                }
                Arc::new(v)
            })
            .clone()
    }

    /// 設定を変える（使い始めた後は `VEIL_ERR_STATE`）
    fn configure(&self, f: impl FnOnce(&mut Settings)) -> c_int {
        if self.built.get().is_some() {
            return fail(
                VEIL_ERR_STATE,
                "the verifier is already in use (only veil_kit_jwt_verifier_set_jwks may change it now)",
            );
        }
        f(&mut self.settings.lock().unwrap_or_else(|e| e.into_inner()));
        VEIL_OK
    }
}

/// A new token verifier without keys (add keys, then optionally the expected issuer and audience).
///
/// # Safety
///
/// Always callable; free it with `veil_kit_jwt_verifier_free`.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_jwt_verifier_new() -> *mut veil_kit_jwt_verifier_t {
    Box::into_raw(Box::new(veil_kit_jwt_verifier_t {
        settings: Mutex::new(Settings::default()),
        built: OnceLock::new(),
    }))
}

/// Frees a verifier (`NULL` does nothing). Guards installed with it keep their own reference.
///
/// # Safety
///
/// `v` is NULL or a verifier that has not been freed.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_jwt_verifier_free(v: *mut veil_kit_jwt_verifier_t) {
    if !v.is_null() {
        drop(Box::from_raw(v));
    }
}

unsafe fn alg(name: *const c_char) -> Result<Algorithm, c_int> {
    let n = cstr(name)?;
    Algorithm::from_name(n).ok_or_else(|| {
        fail(
            VEIL_ERR_INVALID_ARG,
            format!("unknown JWT algorithm '{}'", n),
        )
    })
}

unsafe fn with_kid(k: JwtKey, kid: *const c_char) -> Result<JwtKey, c_int> {
    Ok(match opt_cstr(kid)? {
        Some(id) => k.with_kid(id),
        None => k,
    })
}

/// Adds an HMAC key (`alg` is `"HS256"`, `"HS384"` or `"HS512"`; `kid` may be NULL).
///
/// # Safety
///
/// `v` is a valid verifier, `secret` points to `len` readable bytes, the strings are NUL-terminated or NULL.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_jwt_verifier_add_hmac(
    v: *mut veil_kit_jwt_verifier_t,
    alg_name: *const c_char,
    secret: *const u8,
    len: usize,
    kid: *const c_char,
) -> c_int {
    let Some(v) = v.as_ref() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL verifier");
    };
    if secret.is_null() {
        return fail(VEIL_ERR_INVALID_ARG, "NULL secret");
    }
    let a = tryc!(alg(alg_name));
    let k = match JwtKey::hmac(a, std::slice::from_raw_parts(secret, len)) {
        Ok(k) => k,
        Err(e) => return fail(VEIL_ERR_INVALID_ARG, e.to_string()),
    };
    let k = tryc!(with_kid(k, kid));
    v.configure(|s| s.keys.push(k))
}

/// Adds a public key in PEM (`alg` is one of `RS256`…`PS512`, `ES256`, `ES384`, `EdDSA`; `kid` may be NULL).
///
/// # Safety
///
/// `v` is a valid verifier; the strings are NUL-terminated (`kid` may be NULL).
#[no_mangle]
pub unsafe extern "C" fn veil_kit_jwt_verifier_add_pem(
    v: *mut veil_kit_jwt_verifier_t,
    alg_name: *const c_char,
    pem: *const c_char,
    kid: *const c_char,
) -> c_int {
    let Some(v) = v.as_ref() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL verifier");
    };
    let a = tryc!(alg(alg_name));
    let k = match JwtKey::from_pem(a, tryc!(cstr(pem))) {
        Ok(k) => k,
        Err(e) => return fail(VEIL_ERR_INVALID_ARG, e.to_string()),
    };
    let k = tryc!(with_kid(k, kid));
    v.configure(|s| s.keys.push(k))
}

fn jwks_keys(json: &str) -> Result<Vec<JwtKey>, c_int> {
    let val: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| fail(VEIL_ERR_INVALID_ARG, format!("invalid JWKS JSON: {}", e)))?;
    // JwtVerifier::from_jwks と同じ規則で読む（署名用でない鍵・使えない鍵は飛ばす）
    let arr = val
        .get("keys")
        .and_then(|k| k.as_array())
        .ok_or_else(|| fail(VEIL_ERR_INVALID_ARG, "JWKS without 'keys'"))?;
    Ok(arr
        .iter()
        .filter(|k| {
            k.get("use")
                .and_then(|u| u.as_str())
                .is_none_or(|u| u == "sig")
        })
        .filter_map(|k| JwtKey::from_jwk(k).ok())
        .collect())
}

/// Adds the keys of a JWK Set (`{"keys": [...]}`; keys that cannot be used are skipped).
///
/// # Safety
///
/// `v` is a valid verifier; `jwks_json` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_jwt_verifier_add_jwks(
    v: *mut veil_kit_jwt_verifier_t,
    jwks_json: *const c_char,
) -> c_int {
    let Some(v) = v.as_ref() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL verifier");
    };
    let keys = tryc!(jwks_keys(tryc!(cstr(jwks_json))));
    v.configure(|s| s.keys.extend(keys))
}

/// Replaces the keys with those of a JWK Set while the server runs (from any thread; requests in progress finish
/// with the old keys). Use it to refresh the keys of an identity provider periodically.
///
/// # Safety
///
/// `v` is a valid verifier; `jwks_json` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_jwt_verifier_set_jwks(
    v: *mut veil_kit_jwt_verifier_t,
    jwks_json: *const c_char,
) -> c_int {
    let Some(v) = v.as_ref() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL verifier");
    };
    let keys = tryc!(jwks_keys(tryc!(cstr(jwks_json))));
    v.verifier().set_keys(keys);
    VEIL_OK
}

/// Requires `iss` to equal `issuer`.
///
/// # Safety
///
/// `v` is a valid verifier; `issuer` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_jwt_verifier_issuer(
    v: *mut veil_kit_jwt_verifier_t,
    issuer: *const c_char,
) -> c_int {
    let Some(v) = v.as_ref() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL verifier");
    };
    let i = tryc!(cstr(issuer)).to_string();
    v.configure(|s| s.issuer = Some(i))
}

/// Requires `aud` to contain `audience`.
///
/// # Safety
///
/// `v` is a valid verifier; `audience` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_jwt_verifier_audience(
    v: *mut veil_kit_jwt_verifier_t,
    audience: *const c_char,
) -> c_int {
    let Some(v) = v.as_ref() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL verifier");
    };
    let a = tryc!(cstr(audience)).to_string();
    v.configure(|s| s.audience = Some(a))
}

/// Whether tokens without `exp` are rejected (default 1).
///
/// # Safety
///
/// `v` is a valid verifier.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_jwt_verifier_require_exp(
    v: *mut veil_kit_jwt_verifier_t,
    on: c_int,
) -> c_int {
    let Some(v) = v.as_ref() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL verifier");
    };
    v.configure(|s| s.require_exp = Some(on != 0))
}

/// Clock leeway in seconds for `exp` and `nbf` (default 60).
///
/// # Safety
///
/// `v` is a valid verifier.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_jwt_verifier_leeway(
    v: *mut veil_kit_jwt_verifier_t,
    secs: u64,
) -> c_int {
    let Some(v) = v.as_ref() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL verifier");
    };
    v.configure(|s| s.leeway = Some(secs))
}

/// Verifies `token` (`len` bytes). `VEIL_OK` with the claims as JSON in `*claims_json` (NULL to skip; free it with
/// `veil_kit_free`), or `VEIL_KIT_ERR_TOKEN` with the reason in `veil_last_error`.
///
/// # Safety
///
/// `v` is a valid verifier, `token` points to `len` readable bytes, `claims_json` is NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_jwt_verify(
    v: *mut veil_kit_jwt_verifier_t,
    token: *const c_char,
    len: usize,
    claims_json: *mut *mut c_char,
) -> c_int {
    let Some(v) = v.as_ref() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL verifier");
    };
    if token.is_null() {
        return fail(VEIL_ERR_INVALID_ARG, "NULL token");
    }
    let Ok(t) = std::str::from_utf8(std::slice::from_raw_parts(token as *const u8, len)) else {
        return fail(VEIL_KIT_ERR_TOKEN, "the token is not UTF-8");
    };
    match v.verifier().verify::<serde_json::Value>(t) {
        Ok(c) => {
            if claims_json.is_null() {
                VEIL_OK
            } else {
                put_string(claims_json, c.to_string())
            }
        }
        Err(e) => fail(VEIL_KIT_ERR_TOKEN, e.to_string()),
    }
}

/// Signs `claims_json` (a JSON object) with an HMAC key and writes the token to `*out` (free it with
/// `veil_kit_free`). `alg` is `"HS256"`, `"HS384"` or `"HS512"`; `kid` may be NULL.
///
/// # Safety
///
/// `secret` points to `len` readable bytes; the strings are NUL-terminated (`kid` may be NULL); `out` is writable.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_jwt_encode_hmac(
    alg_name: *const c_char,
    secret: *const u8,
    len: usize,
    kid: *const c_char,
    claims_json: *const c_char,
    out: *mut *mut c_char,
) -> c_int {
    if secret.is_null() {
        return fail(VEIL_ERR_INVALID_ARG, "NULL secret");
    }
    let a = tryc!(alg(alg_name));
    let k = match JwtKey::hmac(a, std::slice::from_raw_parts(secret, len)) {
        Ok(k) => k,
        Err(e) => return fail(VEIL_ERR_INVALID_ARG, e.to_string()),
    };
    let k = tryc!(with_kid(k, kid));
    let claims: serde_json::Value = match serde_json::from_str(tryc!(cstr(claims_json))) {
        Ok(v @ serde_json::Value::Object(_)) => v,
        Ok(_) => return fail(VEIL_ERR_INVALID_ARG, "the claims must be a JSON object"),
        Err(e) => return fail(VEIL_ERR_INVALID_ARG, format!("invalid claims JSON: {}", e)),
    };
    match encode_hmac(&k, &claims) {
        Ok(t) => put_string(out, t),
        Err(e) => fail(VEIL_ERR_INVALID_ARG, e.to_string()),
    }
}

// ===================== Bearer の JWT から主体を作るガード =====================

/// Requests without a token continue anonymously (`veil_kit_jwt_authenticate`).
pub const VEIL_KIT_JWT_OPTIONAL: u32 = 1;

struct JwtGuard {
    verifier: Arc<JwtVerifier>,
    optional: bool,
}

unsafe extern "C" fn jwt_before(
    req: *const veil_request_t,
    resp: *mut veil_response_t,
    ud: *mut c_void,
) -> c_int {
    let g = &*(ud as *const JwtGuard);
    let Some(r) = internal::request(req) else {
        return VEIL_MW_STOP;
    };
    let has_auth = r
        .headers()
        .iter()
        .any(|(n, _)| n.eq_ignore_ascii_case(b"authorization"));
    let deny = match BearerToken::from_headers(r) {
        Err(_) if !has_auth && g.optional => return VEIL_MW_CONTINUE,
        Err(e) => e,
        Ok(t) => match g.verifier.verify::<Claims>(&t.0) {
            Ok(c) => {
                r.extensions().insert(c.to_principal());
                return VEIL_MW_CONTINUE;
            }
            // Rust の kit の Jwt 抽出子と同じ 401
            Err(e) => Response::error(401, "Unauthorized").with_header(
                "www-authenticate",
                format!(
                    "Bearer error=\"invalid_token\", error_description=\"{}\"",
                    e
                ),
            ),
        },
    };
    internal::update_response(resp, |_| deny);
    VEIL_MW_STOP
}

/// Verifies `Authorization: Bearer` on the C routes under `prefix` (NULL is `/`) and makes the token's claims the
/// request's principal (`sub`, the scopes of `scope`, `roles`; the claims are its attributes). A missing or
/// invalid token is answered with 401 (`WWW-Authenticate: Bearer error="invalid_token"` for an invalid one);
/// with `VEIL_KIT_JWT_OPTIONAL` a request without a token continues anonymously. It is a C middleware, so register
/// `veil_kit_require_scope` before it. The configuration keeps its own reference to the verifier.
///
/// # Safety
///
/// `cfg` is a valid configuration handle, `prefix` NULL or a NUL-terminated string, `v` a valid verifier.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_jwt_authenticate(
    cfg: *mut veil_config_t,
    prefix: *const c_char,
    v: *mut veil_kit_jwt_verifier_t,
    flags: u32,
) -> c_int {
    let Some(v) = v.as_ref() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL verifier");
    };
    if flags & !VEIL_KIT_JWT_OPTIONAL != 0 {
        return fail(VEIL_ERR_INVALID_ARG, "unknown flags");
    }
    register_guard(
        cfg,
        prefix,
        jwt_before,
        JwtGuard {
            verifier: v.verifier(),
            optional: flags & VEIL_KIT_JWT_OPTIONAL != 0,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    const SECRET: &[u8] = b"kit-example-secret-0123456789abcdef";

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    // Rust の kit が発行したトークンを C が検証し、C が発行したトークンを Rust の kit が検証する
    #[test]
    fn tokens_interoperate_with_the_rust_kit() {
        let claims = Claims {
            sub: Some("bob".into()),
            iss: Some("veil-kit-example".into()),
            exp: Some(now() + 300),
            scope: Some("read admin".into()),
            ..Default::default()
        };
        let rust_token = encode_hmac(&JwtKey::hs256(SECRET), &claims).unwrap();
        unsafe {
            let v = veil_kit_jwt_verifier_new();
            assert_eq!(
                veil_kit_jwt_verifier_add_hmac(
                    v,
                    c("HS256").as_ptr(),
                    SECRET.as_ptr(),
                    SECRET.len(),
                    std::ptr::null()
                ),
                VEIL_OK
            );
            assert_eq!(
                veil_kit_jwt_verifier_issuer(v, c("veil-kit-example").as_ptr()),
                VEIL_OK
            );
            let mut out = std::ptr::null_mut();
            assert_eq!(
                veil_kit_jwt_verify(
                    v,
                    rust_token.as_ptr() as *const c_char,
                    rust_token.len(),
                    &mut out
                ),
                VEIL_OK
            );
            let got: serde_json::Value = serde_json::from_str(&owned(out)).unwrap();
            assert_eq!(got["sub"], "bob");
            // 使い始めた後の設定の変更は断る
            assert_eq!(
                veil_kit_jwt_verifier_audience(v, c("x").as_ptr()),
                VEIL_ERR_STATE
            );
            // 改ざん・期限切れ・alg の取り違え
            let bad = format!("{}x", rust_token);
            assert_eq!(
                veil_kit_jwt_verify(
                    v,
                    bad.as_ptr() as *const c_char,
                    bad.len(),
                    std::ptr::null_mut()
                ),
                VEIL_KIT_ERR_TOKEN
            );
            let expired = encode_hmac(
                &JwtKey::hs256(SECRET),
                &Claims {
                    exp: Some(now() - 3600),
                    ..claims.clone()
                },
            )
            .unwrap();
            assert_eq!(
                veil_kit_jwt_verify(
                    v,
                    expired.as_ptr() as *const c_char,
                    expired.len(),
                    std::ptr::null_mut()
                ),
                VEIL_KIT_ERR_TOKEN
            );
            let hs512 =
                encode_hmac(&JwtKey::hmac(Algorithm::HS512, SECRET).unwrap(), &claims).unwrap();
            assert_eq!(
                veil_kit_jwt_verify(
                    v,
                    hs512.as_ptr() as *const c_char,
                    hs512.len(),
                    std::ptr::null_mut()
                ),
                VEIL_KIT_ERR_TOKEN
            );
            // C が発行 → Rust の kit が検証
            let body = format!(
                "{{\"sub\":\"carol\",\"iss\":\"veil-kit-example\",\"exp\":{}}}",
                now() + 60
            );
            let mut tok = std::ptr::null_mut();
            assert_eq!(
                veil_kit_jwt_encode_hmac(
                    c("HS256").as_ptr(),
                    SECRET.as_ptr(),
                    SECRET.len(),
                    std::ptr::null(),
                    c(&body).as_ptr(),
                    &mut tok
                ),
                VEIL_OK
            );
            let tok = owned(tok);
            let rv = JwtVerifier::new([JwtKey::hs256(SECRET)]).issuer("veil-kit-example");
            assert_eq!(
                rv.verify::<Claims>(&tok).unwrap().sub.as_deref(),
                Some("carol")
            );
            assert_eq!(
                veil_kit_jwt_encode_hmac(
                    c("RS256").as_ptr(),
                    SECRET.as_ptr(),
                    SECRET.len(),
                    std::ptr::null(),
                    c(&body).as_ptr(),
                    &mut std::ptr::null_mut()
                ),
                VEIL_ERR_INVALID_ARG
            );
            assert_eq!(
                veil_kit_jwt_encode_hmac(
                    c("HS256").as_ptr(),
                    SECRET.as_ptr(),
                    SECRET.len(),
                    std::ptr::null(),
                    c("[1]").as_ptr(),
                    &mut std::ptr::null_mut()
                ),
                VEIL_ERR_INVALID_ARG
            );
            // ガード: 主体が入る・無効なトークンは 401・トークン無しは OPTIONAL なら続ける
            let g = JwtGuard {
                verifier: (*v).verifier(),
                optional: true,
            };
            let gp = &g as *const JwtGuard as *mut c_void;
            let auth = format!("Bearer {}", rust_token);
            let (rc, _) = with_handles(request(&[("authorization", &auth)], ""), |rq, rs| {
                let rc = jwt_before(rq, rs, gp);
                let p = crate::auth::veil_kit_principal(rq);
                assert_eq!(
                    crate::auth::veil_kit_principal_has_scope(p, c("admin").as_ptr()),
                    1
                );
                rc
            });
            assert_eq!(rc, VEIL_MW_CONTINUE);
            let (rc, resp) = with_handles(
                request(&[("authorization", "Bearer nope")], ""),
                |rq, rs| jwt_before(rq, rs, gp),
            );
            assert_eq!((rc, resp.status_code()), (VEIL_MW_STOP, 401));
            assert!(
                std::str::from_utf8(resp.header("www-authenticate").unwrap())
                    .unwrap()
                    .contains("invalid_token")
            );
            let (rc, _) = with_handles(request(&[], ""), |rq, rs| jwt_before(rq, rs, gp));
            assert_eq!(rc, VEIL_MW_CONTINUE);
            let g2 = JwtGuard {
                verifier: (*v).verifier(),
                optional: false,
            };
            let (rc, resp) = with_handles(request(&[], ""), |rq, rs| {
                jwt_before(rq, rs, &g2 as *const JwtGuard as *mut c_void)
            });
            assert_eq!((rc, resp.status_code()), (VEIL_MW_STOP, 401));
            veil_kit_jwt_verifier_free(v);
        }
    }

    #[test]
    fn jwks_keys_can_be_replaced() {
        // HS256 の JWK（k は base64url の秘密）
        let jwks = r#"{"keys":[{"kty":"oct","alg":"HS256","kid":"a","k":"a2l0LWV4YW1wbGUtc2VjcmV0LTAxMjM0NTY3ODlhYmNkZWY"}]}"#;
        let tok = encode_hmac(
            &JwtKey::hs256(SECRET).with_kid("a"),
            &Claims {
                sub: Some("x".into()),
                exp: Some(now() + 60),
                ..Default::default()
            },
        )
        .unwrap();
        unsafe {
            let v = veil_kit_jwt_verifier_new();
            assert_eq!(veil_kit_jwt_verifier_add_jwks(v, c(jwks).as_ptr()), VEIL_OK);
            assert_eq!(
                veil_kit_jwt_verify(
                    v,
                    tok.as_ptr() as *const c_char,
                    tok.len(),
                    std::ptr::null_mut()
                ),
                VEIL_OK
            );
            assert_eq!(
                veil_kit_jwt_verifier_set_jwks(v, c(r#"{"keys":[]}"#).as_ptr()),
                VEIL_OK
            );
            assert_eq!(
                veil_kit_jwt_verify(
                    v,
                    tok.as_ptr() as *const c_char,
                    tok.len(),
                    std::ptr::null_mut()
                ),
                VEIL_KIT_ERR_TOKEN
            );
            assert_eq!(
                veil_kit_jwt_verifier_set_jwks(v, c("{").as_ptr()),
                VEIL_ERR_INVALID_ARG
            );
            veil_kit_jwt_verifier_free(v);
        }
    }
}
