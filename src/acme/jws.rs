//! ACME の JWS（RFC 7515 / RFC 8555 §6.2）: ES256（ECDSA P-256 + SHA-256）と JWK サムプリント（RFC 7638）。
//!
//! 暗号は rustls と同じプロバイダ（OpenBSD/NetBSD は ring、それ以外は aws-lc-rs）を使う。

#[cfg(not(any(target_os = "openbsd", target_os = "netbsd")))]
use aws_lc_rs as crypto;
#[cfg(any(target_os = "openbsd", target_os = "netbsd"))]
use ring as crypto;

use crypto::rand::SystemRandom;
use crypto::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde_json::{json, Value};

/// base64url（パディング無し、RFC 4648 §5）。
pub fn b64url(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(T[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(T[n as usize & 63] as char);
        }
    }
    out
}

/// SHA-256。
pub fn sha256(data: &[u8]) -> Vec<u8> {
    crypto::digest::digest(&crypto::digest::SHA256, data)
        .as_ref()
        .to_vec()
}

/// ACME アカウント鍵（ECDSA P-256）。
pub struct AccountKey {
    pair: EcdsaKeyPair,
    pkcs8: Vec<u8>,
    rng: SystemRandom,
}

impl AccountKey {
    /// 新しい鍵を作る。
    pub fn generate() -> Result<AccountKey, String> {
        let rng = SystemRandom::new();
        let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .map_err(|_| "failed to generate the ACME account key".to_string())?;
        AccountKey::from_pkcs8(doc.as_ref())
    }

    /// PKCS#8（DER）から読み込む。
    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<AccountKey, String> {
        let rng = SystemRandom::new();
        #[cfg(not(any(target_os = "openbsd", target_os = "netbsd")))]
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8);
        #[cfg(any(target_os = "openbsd", target_os = "netbsd"))]
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8, &rng);
        let pair =
            pair.map_err(|_| "invalid ACME account key (expected PKCS#8 P-256)".to_string())?;
        Ok(AccountKey {
            pair,
            pkcs8: pkcs8.to_vec(),
            rng,
        })
    }

    /// 保存用の PKCS#8（DER）。
    pub fn pkcs8(&self) -> &[u8] {
        &self.pkcs8
    }

    /// 公開鍵の JWK（`{"crv","kty","x","y"}`、サムプリント用に辞書順）。
    pub fn jwk(&self) -> Value {
        let p = self.pair.public_key().as_ref(); // 0x04 || X(32) || Y(32)
        json!({
            "crv": "P-256",
            "kty": "EC",
            "x": b64url(&p[1..33]),
            "y": b64url(&p[33..65]),
        })
    }

    /// JWK サムプリント（RFC 7638、base64url）。HTTP-01 の key authorization に使う。
    pub fn thumbprint(&self) -> String {
        let jwk = self.jwk();
        // 必須メンバーを辞書順・空白無しで並べた JSON の SHA-256
        let canon = format!(
            r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
            jwk["x"].as_str().unwrap_or(""),
            jwk["y"].as_str().unwrap_or("")
        );
        b64url(&sha256(canon.as_bytes()))
    }

    /// JWS（Flattened JSON Serialization）を作る。`kid` があれば kid、無ければ jwk をヘッダに入れる。
    /// `payload` が `None` なら POST-as-GET（空のペイロード）。
    pub fn sign(
        &self,
        url: &str,
        nonce: &str,
        kid: Option<&str>,
        payload: Option<&Value>,
    ) -> Result<Vec<u8>, String> {
        let mut protected = json!({ "alg": "ES256", "nonce": nonce, "url": url });
        match kid {
            Some(k) => protected["kid"] = Value::String(k.to_string()),
            None => protected["jwk"] = self.jwk(),
        }
        let protected = b64url(protected.to_string().as_bytes());
        let payload = match payload {
            Some(p) => b64url(p.to_string().as_bytes()),
            None => String::new(),
        };
        let signing_input = format!("{}.{}", protected, payload);
        let sig = self
            .pair
            .sign(&self.rng, signing_input.as_bytes())
            .map_err(|_| "ES256 signing failed".to_string())?;
        let body = json!({
            "protected": protected,
            "payload": payload,
            "signature": b64url(sig.as_ref()),
        });
        Ok(body.to_string().into_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_vectors() {
        // RFC 4648 §10 のテストベクタ（パディング無し・URL セーフ）
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(b"foobar"), "Zm9vYmFy");
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn key_roundtrip_and_jws_shape() {
        let k = AccountKey::generate().unwrap();
        let k2 = AccountKey::from_pkcs8(k.pkcs8()).unwrap();
        assert_eq!(k.thumbprint(), k2.thumbprint());
        assert_eq!(k.thumbprint().len(), 43); // SHA-256 の base64url
        let jws: Value = serde_json::from_slice(
            &k.sign("https://x/acct", "n1", None, Some(&json!({"a": 1})))
                .unwrap(),
        )
        .unwrap();
        let sig = jws["signature"].as_str().unwrap();
        assert_eq!(sig.len(), 86); // r||s 64 バイト
        let post_as_get: Value =
            serde_json::from_slice(&k2.sign("https://x/o", "n2", Some("kid1"), None).unwrap())
                .unwrap();
        assert_eq!(post_as_get["payload"], "");
    }
}
