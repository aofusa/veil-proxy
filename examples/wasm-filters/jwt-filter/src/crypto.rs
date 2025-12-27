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

/// Verify RS256 signature using JWK
pub fn verify_rs256(
    header_payload: &str,
    signature_b64: &str,
    jwk: &Jwk,
) -> VerifyResult {
    use rsa::{RsaPublicKey, pkcs1v15::Signature, signature::Verifier};
    use rsa::pkcs1v15::VerifyingKey;
    use sha2::Sha256;
    use rsa::BigUint;
    
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
    
    // Convert to BigUint
    let n = BigUint::from_bytes_be(&n_bytes);
    let e = BigUint::from_bytes_be(&e_bytes);
    
    // Create RSA public key
    let public_key = match RsaPublicKey::new(n, e) {
        Ok(k) => k,
        Err(_) => return VerifyResult::InvalidKey,
    };
    
    // Decode signature
    let signature_bytes = match URL_SAFE_NO_PAD.decode(signature_b64) {
        Ok(s) => s,
        Err(_) => return VerifyResult::InvalidFormat,
    };
    
    // Create verifying key
    let verifying_key = VerifyingKey::<Sha256>::new(public_key);
    
    // Create signature object
    let signature = match Signature::try_from(signature_bytes.as_slice()) {
        Ok(s) => s,
        Err(_) => return VerifyResult::InvalidFormat,
    };
    
    // Verify
    match verifying_key.verify(header_payload.as_bytes(), &signature) {
        Ok(_) => VerifyResult::Valid,
        Err(_) => VerifyResult::InvalidSignature,
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
