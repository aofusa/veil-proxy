//! JWT Cryptographic Operations
//!
//! Supports HS256 and RS256 signature verification

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::jwks::Jwk;

type HmacSha256 = Hmac<Sha256>;

/// Verification result
#[derive(Debug)]
pub enum VerifyResult {
    Valid,
    InvalidSignature,
    // UnsupportedAlgorithm,  // 削除: アルゴリズム検証はlib.rsで行われる
    InvalidKey,
    InvalidFormat,
}

/// Verify HS256 signature
pub fn verify_hs256(
    header_payload: &str,
    signature_b64: &str,
    secret: &[u8],
) -> VerifyResult {
    // Decode signature
    let signature = match URL_SAFE_NO_PAD.decode(signature_b64) {
        Ok(s) => s,
        Err(_) => return VerifyResult::InvalidFormat,
    };
    
    // Compute HMAC-SHA256
    let mut mac = match HmacSha256::new_from_slice(secret) {
        Ok(m) => m,
        Err(_) => return VerifyResult::InvalidKey,
    };
    mac.update(header_payload.as_bytes());
    
    // Verify
    match mac.verify_slice(&signature) {
        Ok(_) => VerifyResult::Valid,
        Err(_) => VerifyResult::InvalidSignature,
    }
}

/// SHA-256 の DigestInfo（DER）。RFC 8017 §9.2 の注 1
const SHA256_DIGEST_INFO: [u8; 19] = [
    0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05,
    0x00, 0x04, 0x20,
];
/// RFC 7518 §3.3: RS256 の鍵は 2048 ビット以上
const MIN_MODULUS_BITS: u64 = 2048;
/// 以前の `rsa` クレートの既定の上限と同じ（大きな鍵での CPU の浪費を防ぐ）
const MAX_MODULUS_BITS: u64 = 4096;

/// Verify RS256 (RSASSA-PKCS1-v1_5 with SHA-256) signature using JWK
///
/// 検証は公開鍵の演算（s^e mod n）だけなので、秘密鍵の演算の時間差を突く攻撃（`rsa` クレートの
/// RUSTSEC-2023-0071、修正版なし）とは無関係だが、その依存ごと外した（B-158）。復号した値は解析せず、
/// 期待する EMSA-PKCS1-v1_5 の符号化を組み立ててバイト列ごと比べる（緩い解析による署名偽造を避ける。RFC 8017 §8.2.2）。
pub fn verify_rs256(
    header_payload: &str,
    signature_b64: &str,
    jwk: &Jwk,
) -> VerifyResult {
    use num_bigint::BigUint;
    use sha2::Digest;

    // Get n and e from JWK
    let n_b64 = match &jwk.n {
        Some(n) => n,
        None => return VerifyResult::InvalidKey,
    };
    let e_b64 = match &jwk.e {
        Some(e) => e,
        None => return VerifyResult::InvalidKey,
    };

    // Decode base64url
    let n_bytes = match URL_SAFE_NO_PAD.decode(n_b64) {
        Ok(b) => b,
        Err(_) => return VerifyResult::InvalidKey,
    };
    let e_bytes = match URL_SAFE_NO_PAD.decode(e_b64) {
        Ok(b) => b,
        Err(_) => return VerifyResult::InvalidKey,
    };

    let n = BigUint::from_bytes_be(&n_bytes);
    let e = BigUint::from_bytes_be(&e_bytes);
    // 公開鍵の検査（以前の `rsa::RsaPublicKey::new` と同じ e の範囲。n は RFC 7518 の下限を課す）
    if n.bits() < MIN_MODULUS_BITS || n.bits() > MAX_MODULUS_BITS {
        return VerifyResult::InvalidKey;
    }
    if e.bits() < 2 || e.bits() > 33 || !e.bit(0) {
        return VerifyResult::InvalidKey;
    }
    let k = n.bits().div_ceil(8) as usize;

    // Decode signature
    let signature_bytes = match URL_SAFE_NO_PAD.decode(signature_b64) {
        Ok(s) => s,
        Err(_) => return VerifyResult::InvalidFormat,
    };
    // RFC 8017 §8.2.2 step 1: 署名の長さは k バイト
    if signature_bytes.len() != k {
        return VerifyResult::InvalidSignature;
    }
    let s = BigUint::from_bytes_be(&signature_bytes);
    if s >= n {
        return VerifyResult::InvalidSignature;
    }

    // RSAVP1: m = s^e mod n を k バイトの大端に
    let m = s.modpow(&e, &n).to_bytes_be();
    let mut em = vec![0u8; k];
    em[k - m.len()..].copy_from_slice(&m);

    // EMSA-PKCS1-v1_5: 0x00 0x01 FF..FF 0x00 DigestInfo H（k は 2048 ビット以上なので PS は 8 バイト以上ある）
    let hash = Sha256::digest(header_payload.as_bytes());
    let t_len = SHA256_DIGEST_INFO.len() + hash.len();
    let mut expected = vec![0xffu8; k];
    expected[0] = 0x00;
    expected[1] = 0x01;
    expected[k - t_len - 1] = 0x00;
    expected[k - t_len..k - hash.len()].copy_from_slice(&SHA256_DIGEST_INFO);
    expected[k - hash.len()..].copy_from_slice(&hash);

    if em == expected {
        VerifyResult::Valid
    } else {
        VerifyResult::InvalidSignature
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jwks::Jwk;
    
    #[test]
    fn test_hs256_valid() {
        // Test vector from jwt.io
        let header_payload = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ";
        let signature = "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let secret = b"your-256-bit-secret";
        
        match verify_hs256(header_payload, signature, secret) {
            VerifyResult::Valid => {}
            other => panic!("Expected Valid, got {:?}", other),
        }
    }
    
    #[test]
    fn test_hs256_invalid() {
        let header_payload = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0";
        let signature = "invalid_signature";
        let secret = b"secret";
        
        match verify_hs256(header_payload, signature, secret) {
            VerifyResult::InvalidFormat | VerifyResult::InvalidSignature => {}
            other => panic!("Expected InvalidFormat or InvalidSignature, got {:?}", other),
        }
    }
    
    // openssl genrsa 2048 + `openssl dgst -sha256 -sign` で作った RS256 の検証用ベクタ
    const RS256_HP: &str = "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ2ZWlsIiwiaWF0IjoxNzAwMDAwMDAwfQ";
    const RS256_N: &str = "z-VmEatvDeG25rpejZzWTeHT4ArHxjx8d_KYlTmiLgqroxndGK-_v3rarJYXNTo2rLuQhxHcWArivvsR9OPNdi2lsqqAilIVfrHcCJw53cykwnpByAvJWelYsegpp1Een4ZGlkICtIgha_PJsJK3abtMJ8fVSHkHw9rjBr0vgDqHXsVCHyJLZKzFRjhhb3WqGBmSdg1HtybVE1A9s6mqgg-1t9qL0Ed0eKScW7mGjWmsPw4hFJK7HBVd4fX3zRrkBXkjngYrGXuMBi0ax4OFmWOzcc9hJPGcaafb616k9enef_Srs5zNUcHIjc0TfHfg-bB42cWpF4J15c0cDe0npQ";
    const RS256_SIG: &str = "WT00U1ZH3hVq_3eqMQ8kWnvPw3-21HMJL46Rk0fnleu3nAnJILzfuxRVwaY9PuTlm8PlM63_ozrOn35Dc57KWohPpm5r5dv63N_UBTKrkOJEm5a2rmG2tgUFqbjLNYzW0H0LBUBigzGO0nMSsBnNCA2UTDu_c3sq6H3tW0JnZcefK-xHrgCWN4T_Lm65fp-pr50o4ymf3w2s0_FVFpxmi346Xm3JgdQiJdE_75mXlB3nWnapuspFTG8f4CUBxjNuXJPEY9PVFK7RJx1z_0U9adpRDP4nU1coRveMp0nFc0M5s4jaSG3s9D-rHYq_8_vBL9W6vp7I3xThgwLCNxNn6Q";

    fn rs256_jwk() -> Jwk {
        Jwk {
            kty: "RSA".to_string(),
            kid: None,
            alg: Some("RS256".to_string()),
            use_type: None,
            n: Some(RS256_N.to_string()),
            e: Some("AQAB".to_string()),
        }
    }

    #[test]
    fn test_rs256_valid() {
        match verify_rs256(RS256_HP, RS256_SIG, &rs256_jwk()) {
            VerifyResult::Valid => {}
            other => panic!("Expected Valid, got {:?}", other),
        }
    }

    #[test]
    fn test_rs256_tampered_payload() {
        let hp = format!("{}x", RS256_HP);
        match verify_rs256(&hp, RS256_SIG, &rs256_jwk()) {
            VerifyResult::InvalidSignature => {}
            other => panic!("Expected InvalidSignature, got {:?}", other),
        }
    }

    #[test]
    fn test_rs256_tampered_signature() {
        let mut sig = URL_SAFE_NO_PAD.decode(RS256_SIG).unwrap();
        sig[100] ^= 1;
        let sig = URL_SAFE_NO_PAD.encode(&sig);
        match verify_rs256(RS256_HP, &sig, &rs256_jwk()) {
            VerifyResult::InvalidSignature => {}
            other => panic!("Expected InvalidSignature, got {:?}", other),
        }
        // 長さが k バイトでない署名
        let short = URL_SAFE_NO_PAD.encode(&URL_SAFE_NO_PAD.decode(RS256_SIG).unwrap()[1..]);
        match verify_rs256(RS256_HP, &short, &rs256_jwk()) {
            VerifyResult::InvalidSignature => {}
            other => panic!("Expected InvalidSignature, got {:?}", other),
        }
    }

    #[test]
    fn test_rs256_rejects_weak_keys() {
        // 1024 ビットの鍵（RFC 7518 の下限未満）
        let mut jwk = rs256_jwk();
        let n = URL_SAFE_NO_PAD.decode(RS256_N).unwrap();
        jwk.n = Some(URL_SAFE_NO_PAD.encode(&n[..128]));
        match verify_rs256(RS256_HP, RS256_SIG, &jwk) {
            VerifyResult::InvalidKey => {}
            other => panic!("Expected InvalidKey, got {:?}", other),
        }
        // 偶数の e・e = 1
        for e in ["AQAA", "AQ"] {
            let mut jwk = rs256_jwk();
            jwk.e = Some(e.to_string());
            match verify_rs256(RS256_HP, RS256_SIG, &jwk) {
                VerifyResult::InvalidKey => {}
                other => panic!("Expected InvalidKey for e={}, got {:?}", e, other),
            }
        }
    }

    #[test]
    fn test_rs256_invalid_key_missing_n() {
        let header_payload = "test";
        let signature = "test";
        let jwk = Jwk {
            kty: "RSA".to_string(),
            kid: None,
            alg: None,
            use_type: None,
            n: None,  // Missing n
            e: Some("AQAB".to_string()),
        };
        
        match verify_rs256(header_payload, signature, &jwk) {
            VerifyResult::InvalidKey => {}
            other => panic!("Expected InvalidKey, got {:?}", other),
        }
    }
    
    #[test]
    fn test_rs256_invalid_key_missing_e() {
        let header_payload = "test";
        let signature = "test";
        let jwk = Jwk {
            kty: "RSA".to_string(),
            kid: None,
            alg: None,
            use_type: None,
            n: Some("test".to_string()),
            e: None,  // Missing e
        };
        
        match verify_rs256(header_payload, signature, &jwk) {
            VerifyResult::InvalidKey => {}
            other => panic!("Expected InvalidKey, got {:?}", other),
        }
    }
    
    #[test]
    fn test_rs256_invalid_format_signature() {
        let header_payload = "test";
        let signature = "invalid_base64!!!";  // Invalid base64url
        let jwk = Jwk {
            kty: "RSA".to_string(),
            kid: None,
            alg: None,
            use_type: None,
            n: Some("AQAB".to_string()),
            e: Some("AQAB".to_string()),
        };
        
        match verify_rs256(header_payload, signature, &jwk) {
            VerifyResult::InvalidFormat | VerifyResult::InvalidKey => {}
            other => panic!("Expected InvalidFormat or InvalidKey, got {:?}", other),
        }
    }
    
    #[test]
    fn test_rs256_invalid_format_key() {
        let header_payload = "test";
        let signature = "dGVzdA";  // Valid base64url
        let jwk = Jwk {
            kty: "RSA".to_string(),
            kid: None,
            alg: None,
            use_type: None,
            n: Some("invalid_base64!!!".to_string()),  // Invalid base64url
            e: Some("AQAB".to_string()),
        };
        
        match verify_rs256(header_payload, signature, &jwk) {
            VerifyResult::InvalidKey => {}
            other => panic!("Expected InvalidKey, got {:?}", other),
        }
    }
}
