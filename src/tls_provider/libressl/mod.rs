//! LibreSSL 対応 rustls `CryptoProvider` 自作実装（F-142）。
//!
//! `openssl` クレートの「クラシック API」（`EVP_CIPHER`/`EVP_MD`/`EVP_PKEY`/ECDH/X25519）
//! のみを使い、OpenSSL 3.x の provider/FIPS API（`rustls-openssl` が依存し LibreSSL では
//! コンパイルできなかったもの）には一切触れない。詳細は
//! `docs/artifacts/f142_libressl_provider_design.md` を参照。
//!
//! - 乱数: [`SecureRandom`]（`openssl::rand::rand_bytes`）
//! - ハッシュ/HMAC: [`hash`]/[`hmac`]（TLS1.3 HKDF・TLS1.2 PRF はそれぞれ
//!   `rustls::crypto::tls13::HkdfUsingHmac`/`rustls::crypto::tls12::PrfUsingHmac` に委譲）
//! - AEAD: [`aead`]（`openssl::cipher_ctx::CipherCtx` を再利用するホットパス実装）+
//!   [`tls12`]/[`tls13`]（cipher suite 定義）
//! - 鍵交換: [`kx`]（X25519 → secp256r1 → secp384r1）
//! - 署名検証: [`verify`]（ECDSA P-256/P-384、RSA PKCS#1/PSS、Ed25519。Ed448 は非対応）
//! - 秘密鍵: [`signer`]（`KeyProvider`/`SigningKey`/`Signer`）

pub(crate) mod aead;
pub(crate) mod hash;
pub(crate) mod hmac;
mod kx;
pub mod signer;
mod tls12;
mod tls13;
pub mod verify;

pub use signer::KeyProvider;
pub use verify::SUPPORTED_SIG_ALGS;

use rustls::crypto::{CryptoProvider, GetRandomFailed};
use rustls::SupportedCipherSuite;

/// このプロバイダが対応する全 cipher suite（優先順位順）。
///
/// TLS1.3 を先に列挙し（AES-256-GCM → AES-128-GCM → ChaCha20-Poly1305）、
/// 続けて TLS1.2（ECDSA 系 → RSA 系、各 AES-256 → AES-128 → ChaCha20-Poly1305）。
pub static ALL_CIPHER_SUITES: &[SupportedCipherSuite] = &[
    tls13::TLS13_AES_256_GCM_SHA384,
    tls13::TLS13_AES_128_GCM_SHA256,
    tls13::TLS13_CHACHA20_POLY1305_SHA256,
    tls12::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
    tls12::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
    tls12::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
    tls12::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
    tls12::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
    tls12::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
];

/// LibreSSL/OpenSSL の CSPRNG による `SecureRandom` 実装。
///
/// `openssl::rand::rand_priv_bytes` は OpenSSL 3.x 専用（LibreSSL に無い）のため使わず、
/// `rand_bytes` を使う。
#[derive(Debug)]
pub struct SecureRandom;

impl rustls::crypto::SecureRandom for SecureRandom {
    fn fill(&self, buf: &mut [u8]) -> Result<(), GetRandomFailed> {
        openssl::rand::rand_bytes(buf).map_err(|_| GetRandomFailed)
    }
}

/// LibreSSL/システム OpenSSL バックエンドの [`CryptoProvider`] を構築する。
pub fn default_provider() -> CryptoProvider {
    CryptoProvider {
        cipher_suites: ALL_CIPHER_SUITES.to_vec(),
        kx_groups: kx::SUPPORTED_KX_GROUPS.to_vec(),
        signature_verification_algorithms: SUPPORTED_SIG_ALGS,
        secure_random: &SecureRandom,
        key_provider: &KeyProvider,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_random_fills_buffer_with_nonzero_bytes() {
        use rustls::crypto::SecureRandom as _;
        let mut buf = [0u8; 32];
        SecureRandom.fill(&mut buf).unwrap();
        // 32 バイト全てが 0 になる確率は無視できるほど小さい。
        assert!(buf.iter().any(|&b| b != 0));
    }

    #[test]
    fn default_provider_has_all_suites_and_kx_groups() {
        let provider = default_provider();
        assert_eq!(provider.cipher_suites.len(), ALL_CIPHER_SUITES.len());
        assert_eq!(provider.kx_groups.len(), kx::SUPPORTED_KX_GROUPS.len());
    }
}
