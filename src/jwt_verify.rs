//! JWT（JWS のコンパクト形式）の検証（F-220。TOML の `[route.jwt]` とメッシュの `jwt_authn` が共有する）。
//!
//! 鍵は JWKS（RSA・EC P-256 / P-384）。署名は RS256/384/512・PS256/384/512・ES256/384。暗号は rustls と同じ
//! プロバイダ（OpenBSD / NetBSD は ring、それ以外は aws-lc-rs。`src/tls_provider.rs` と同じ分け方）。

#[cfg(not(any(target_os = "openbsd", target_os = "netbsd")))]
use aws_lc_rs::signature as sig;
#[cfg(any(target_os = "openbsd", target_os = "netbsd"))]
use ring::signature as sig;

/// JWKS の公開鍵。
#[derive(Debug)]
pub enum Key {
    Rsa {
        n: Vec<u8>,
        e: Vec<u8>,
    },
    /// 非圧縮の点（0x04 || x || y）
    Ec256(Vec<u8>),
    Ec384(Vec<u8>),
}

#[derive(Debug)]
pub struct Jwk {
    pub kid: Option<String>,
    pub alg: Option<String>,
    pub key: Key,
}

/// base64url（パディング・空白は無視。標準の base64 の `+` `/` も受ける）。
pub fn b64url(input: &[u8]) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        })
    }
    let mut out = Vec::with_capacity(input.len() * 3 / 4 + 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in input {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// JWKS（`{"keys": [...]}`）を読む。使えない鍵は飛ばす。
pub fn parse_jwks(text: &str) -> Vec<Jwk> {
    let v: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(keys) = v.get("keys").and_then(|k| k.as_array()) else {
        return Vec::new();
    };
    keys.iter()
        .filter_map(|k| {
            let kid = k.get("kid").and_then(|x| x.as_str()).map(|s| s.to_string());
            let alg = k.get("alg").and_then(|x| x.as_str()).map(|s| s.to_string());
            let s = |name: &str| k.get(name).and_then(|x| x.as_str()).map(str::as_bytes);
            let key = match k.get("kty")?.as_str()? {
                "RSA" => Key::Rsa {
                    n: b64url(s("n")?)?,
                    e: b64url(s("e")?)?,
                },
                "EC" => {
                    let x = b64url(s("x")?)?;
                    let y = b64url(s("y")?)?;
                    let mut pt = Vec::with_capacity(1 + x.len() + y.len());
                    pt.push(4u8);
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

/// 署名を検証する。
pub fn verify_sig(alg: &str, key: &Key, msg: &[u8], signature: &[u8]) -> bool {
    match (alg, key) {
        ("RS256" | "RS384" | "RS512" | "PS256" | "PS384" | "PS512", Key::Rsa { n, e }) => {
            let params: &sig::RsaParameters = match alg {
                "RS256" => &sig::RSA_PKCS1_2048_8192_SHA256,
                "RS384" => &sig::RSA_PKCS1_2048_8192_SHA384,
                "RS512" => &sig::RSA_PKCS1_2048_8192_SHA512,
                "PS256" => &sig::RSA_PSS_2048_8192_SHA256,
                "PS384" => &sig::RSA_PSS_2048_8192_SHA384,
                _ => &sig::RSA_PSS_2048_8192_SHA512,
            };
            sig::RsaPublicKeyComponents {
                n: n.as_slice(),
                e: e.as_slice(),
            }
            .verify(params, msg, signature)
            .is_ok()
        }
        ("ES256", Key::Ec256(pt)) => sig::UnparsedPublicKey::new(&sig::ECDSA_P256_SHA256_FIXED, pt)
            .verify(msg, signature)
            .is_ok(),
        ("ES384", Key::Ec384(pt)) => sig::UnparsedPublicKey::new(&sig::ECDSA_P384_SHA384_FIXED, pt)
            .verify(msg, signature)
            .is_ok(),
        _ => false,
    }
}

/// 検証の条件。
#[derive(Debug)]
pub struct Check<'a> {
    pub keys: &'a [Jwk],
    /// 空なら見ない
    pub issuer: &'a str,
    /// 空なら見ない（どれかが `aud` に入っていればよい）
    pub audiences: &'a [String],
    pub require_expiration: bool,
    pub clock_skew_secs: u64,
    pub now: u64,
}

/// トークンを検証して payload を返す（署名・`iss`・`exp`・`nbf`・`aud`）。
pub fn verify(c: &Check<'_>, token: &str) -> Option<serde_json::Value> {
    let mut parts = token.split('.');
    let (h, pl, sg) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let header: serde_json::Value = serde_json::from_slice(&b64url(h.as_bytes())?).ok()?;
    let payload: serde_json::Value = serde_json::from_slice(&b64url(pl.as_bytes())?).ok()?;
    let signature = b64url(sg.as_bytes())?;
    let alg = header.get("alg")?.as_str()?;
    let kid = header.get("kid").and_then(|k| k.as_str());
    let msg = &token.as_bytes()[..h.len() + 1 + pl.len()];
    let ok = c.keys.iter().any(|k| {
        let kid_ok = match (kid, &k.kid) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        };
        let alg_ok = k.alg.as_deref().is_none_or(|a| a == alg);
        kid_ok && alg_ok && verify_sig(alg, &k.key, msg, &signature)
    });
    if !ok {
        return None;
    }
    if !c.issuer.is_empty() && payload.get("iss").and_then(|i| i.as_str()) != Some(c.issuer) {
        return None;
    }
    let skew = c.clock_skew_secs;
    match payload.get("exp").and_then(|e| e.as_u64()) {
        Some(exp) if exp.saturating_add(skew) < c.now => return None,
        None if c.require_expiration => return None,
        _ => {}
    }
    if let Some(nbf) = payload.get("nbf").and_then(|e| e.as_u64()) {
        if nbf > c.now.saturating_add(skew) {
            return None;
        }
    }
    if !c.audiences.is_empty() {
        let aud_ok = match payload.get("aud") {
            Some(serde_json::Value::String(a)) => c.audiences.iter().any(|w| w == a),
            Some(serde_json::Value::Array(a)) => a.iter().any(|x| {
                x.as_str()
                    .is_some_and(|s| c.audiences.iter().any(|w| w == s))
            }),
            _ => false,
        };
        if !aud_ok {
            return None;
        }
    }
    Some(payload)
}

/// 今の UNIX 時刻（秒）。
pub fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// テスト用: EC P-256 の鍵で署名したトークンと、その JWKS。
    pub(crate) fn es256_token(payload: &str) -> (String, String) {
        #[cfg(not(any(target_os = "openbsd", target_os = "netbsd")))]
        use aws_lc_rs::{rand::SystemRandom, signature::EcdsaKeyPair, signature::KeyPair};
        #[cfg(any(target_os = "openbsd", target_os = "netbsd"))]
        use ring::{rand::SystemRandom, signature::EcdsaKeyPair, signature::KeyPair};
        let rng = SystemRandom::new();
        let pkcs8 =
            EcdsaKeyPair::generate_pkcs8(&sig::ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        #[cfg(not(any(target_os = "openbsd", target_os = "netbsd")))]
        let kp = EcdsaKeyPair::from_pkcs8(&sig::ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref())
            .unwrap();
        #[cfg(any(target_os = "openbsd", target_os = "netbsd"))]
        let kp =
            EcdsaKeyPair::from_pkcs8(&sig::ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
                .unwrap();
        let pt = kp.public_key().as_ref();
        let enc = |b: &[u8]| b64url_encode(b);
        let jwks = format!(
            r#"{{"keys":[{{"kty":"EC","crv":"P-256","kid":"k1","x":"{}","y":"{}"}}]}}"#,
            enc(&pt[1..33]),
            enc(&pt[33..65])
        );
        let signing_input = format!(
            "{}.{}",
            enc(br#"{"alg":"ES256","kid":"k1","typ":"JWT"}"#),
            enc(payload.as_bytes())
        );
        let s = kp.sign(&rng, signing_input.as_bytes()).unwrap();
        (format!("{}.{}", signing_input, enc(s.as_ref())), jwks)
    }

    pub(crate) fn b64url_encode(b: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for ch in b.chunks(3) {
            let n = (ch[0] as u32) << 16
                | (*ch.get(1).unwrap_or(&0) as u32) << 8
                | *ch.get(2).unwrap_or(&0) as u32;
            for i in 0..(ch.len() + 1) {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            }
        }
        out
    }

    #[test]
    fn es256_issuer_audience_and_expiry() {
        let (tok, jwks) =
            es256_token(r#"{"iss":"https://issuer","aud":["api"],"exp":2000000000,"sub":"alice"}"#);
        let keys = parse_jwks(&jwks);
        assert_eq!(keys.len(), 1);
        let aud = vec!["api".to_string()];
        let mut c = Check {
            keys: &keys,
            issuer: "https://issuer",
            audiences: &aud,
            require_expiration: true,
            clock_skew_secs: 60,
            now: 1_900_000_000,
        };
        let p = verify(&c, &tok).expect("valid");
        assert_eq!(p["sub"], "alice");
        // 期限切れ・別の発行者・別の audience・改ざん
        c.now = 2_000_000_100;
        assert!(verify(&c, &tok).is_none());
        c.now = 1_900_000_000;
        c.issuer = "https://other";
        assert!(verify(&c, &tok).is_none());
        c.issuer = "https://issuer";
        let other = vec!["other".to_string()];
        c.audiences = &other;
        assert!(verify(&c, &tok).is_none());
        c.audiences = &aud;
        // 署名の中ほどの 1 文字を変える（末尾の文字は使われないビットだけのことがある）
        let mut bad = tok.clone().into_bytes();
        let i = bad.len() - 10;
        bad[i] = if bad[i] == b'A' { b'B' } else { b'A' };
        assert!(verify(&c, std::str::from_utf8(&bad).unwrap()).is_none());
    }
}
