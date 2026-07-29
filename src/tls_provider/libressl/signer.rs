//! `rustls::crypto::KeyProvider` / `SigningKey` / `Signer` 実装（F-142）。
//!
//! DER（PKCS#8 / SEC1 / PKCS#1）から `openssl::pkey::PKey` を読み、ECDSA / RSA / Ed25519 の
//! 署名を行う。Ed448 は LibreSSL に無いため実装しない（設計メモ・AGENTS.md 指示）。

use openssl::hash::MessageDigest;
use openssl::pkey::{Id, PKey, Private};
use openssl::rsa::Padding;
use openssl::sign::RsaPssSaltlen;
use rustls::pki_types::PrivateKeyDer;
use rustls::sign::SigningKey;
use rustls::{Error, SignatureAlgorithm, SignatureScheme};
use std::sync::Arc;

/// RSA スキーム（降順優先）。
pub(crate) static RSA_SCHEMES: &[SignatureScheme] = &[
    SignatureScheme::RSA_PSS_SHA512,
    SignatureScheme::RSA_PSS_SHA384,
    SignatureScheme::RSA_PSS_SHA256,
    SignatureScheme::RSA_PKCS1_SHA512,
    SignatureScheme::RSA_PKCS1_SHA384,
    SignatureScheme::RSA_PKCS1_SHA256,
];

/// ECDSA スキーム（降順優先）。Ed25519 はこのリストとは別に扱う。
pub(crate) static ECDSA_SCHEMES: &[SignatureScheme] = &[
    SignatureScheme::ECDSA_NISTP384_SHA384,
    SignatureScheme::ECDSA_NISTP256_SHA256,
];

/// `rustls::crypto::KeyProvider` 実装。
#[derive(Debug)]
pub struct KeyProvider;

#[derive(Debug)]
struct PrivateKey(Arc<PKey<Private>>);

#[derive(Debug)]
struct Signer {
    key: Arc<PKey<Private>>,
    scheme: SignatureScheme,
}

fn rsa_padding(scheme: SignatureScheme) -> Option<Padding> {
    match scheme {
        SignatureScheme::RSA_PKCS1_SHA256
        | SignatureScheme::RSA_PKCS1_SHA384
        | SignatureScheme::RSA_PKCS1_SHA512 => Some(Padding::PKCS1),
        SignatureScheme::RSA_PSS_SHA256
        | SignatureScheme::RSA_PSS_SHA384
        | SignatureScheme::RSA_PSS_SHA512 => Some(Padding::PKCS1_PSS),
        _ => None,
    }
}

fn message_digest(scheme: SignatureScheme) -> Option<MessageDigest> {
    match scheme {
        SignatureScheme::RSA_PKCS1_SHA256
        | SignatureScheme::RSA_PSS_SHA256
        | SignatureScheme::ECDSA_NISTP256_SHA256 => Some(MessageDigest::sha256()),
        SignatureScheme::RSA_PKCS1_SHA384
        | SignatureScheme::RSA_PSS_SHA384
        | SignatureScheme::ECDSA_NISTP384_SHA384 => Some(MessageDigest::sha384()),
        SignatureScheme::RSA_PKCS1_SHA512 | SignatureScheme::RSA_PSS_SHA512 => {
            Some(MessageDigest::sha512())
        }
        _ => None,
    }
}

fn mgf1(scheme: SignatureScheme) -> Option<MessageDigest> {
    match scheme {
        SignatureScheme::RSA_PSS_SHA256 => Some(MessageDigest::sha256()),
        SignatureScheme::RSA_PSS_SHA384 => Some(MessageDigest::sha384()),
        SignatureScheme::RSA_PSS_SHA512 => Some(MessageDigest::sha512()),
        _ => None,
    }
}

fn pss_salt_len(scheme: SignatureScheme) -> Option<RsaPssSaltlen> {
    match scheme {
        SignatureScheme::RSA_PSS_SHA256
        | SignatureScheme::RSA_PSS_SHA384
        | SignatureScheme::RSA_PSS_SHA512 => Some(RsaPssSaltlen::DIGEST_LENGTH),
        _ => None,
    }
}

impl rustls::crypto::KeyProvider for KeyProvider {
    fn load_private_key(
        &self,
        key_der: PrivateKeyDer<'static>,
    ) -> Result<Arc<dyn SigningKey>, Error> {
        let pkey = PKey::private_key_from_der(key_der.secret_der())
            .map_err(|e| Error::General(format!("veil libressl provider: OpenSSL error: {e}")))?;
        Ok(Arc::new(PrivateKey(Arc::new(pkey))))
    }
}

impl PrivateKey {
    fn signer(&self, scheme: SignatureScheme) -> Signer {
        Signer {
            key: Arc::clone(&self.0),
            scheme,
        }
    }
}

impl SigningKey for PrivateKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn rustls::sign::Signer>> {
        match self.algorithm() {
            SignatureAlgorithm::RSA => RSA_SCHEMES
                .iter()
                .find(|scheme| offered.contains(scheme))
                .map(|scheme| Box::new(self.signer(*scheme)) as Box<dyn rustls::sign::Signer>),
            SignatureAlgorithm::ED25519 => offered.contains(&SignatureScheme::ED25519).then(|| {
                Box::new(self.signer(SignatureScheme::ED25519)) as Box<dyn rustls::sign::Signer>
            }),
            SignatureAlgorithm::ECDSA => self
                .0
                .ec_key()
                .ok()
                .and_then(|ec_key| {
                    let scheme = match ec_key.group().curve_name() {
                        Some(openssl::nid::Nid::X9_62_PRIME256V1) => {
                            SignatureScheme::ECDSA_NISTP256_SHA256
                        }
                        Some(openssl::nid::Nid::SECP384R1) => {
                            SignatureScheme::ECDSA_NISTP384_SHA384
                        }
                        _ => return None,
                    };
                    offered.contains(&scheme).then_some(scheme)
                })
                .map(|scheme| Box::new(self.signer(scheme)) as Box<dyn rustls::sign::Signer>),
            _ => None,
        }
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        match self.0.id() {
            Id::RSA => SignatureAlgorithm::RSA,
            Id::EC => SignatureAlgorithm::ECDSA,
            Id::ED25519 => SignatureAlgorithm::ED25519,
            other => SignatureAlgorithm::Unknown(other.as_raw().try_into().unwrap_or_default()),
        }
    }
}

impl rustls::sign::Signer for Signer {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        let result = if let Some(digest) = message_digest(self.scheme) {
            openssl::sign::Signer::new(digest, &self.key).and_then(|mut signer| {
                if let Some(padding) = rsa_padding(self.scheme) {
                    signer.set_rsa_padding(padding)?;
                }
                if let Some(mgf1_md) = mgf1(self.scheme) {
                    signer.set_rsa_mgf1_md(mgf1_md)?;
                }
                if let Some(len) = pss_salt_len(self.scheme) {
                    signer.set_rsa_pss_saltlen(len)?;
                }
                signer.update(message)?;
                signer.sign_to_vec()
            })
        } else {
            // Ed25519: メッセージダイジェストを指定しない one-shot 署名。
            openssl::sign::Signer::new_without_digest(&self.key)
                .and_then(|mut signer| signer.sign_oneshot_to_vec(message))
        };
        result.map_err(|e| Error::General(format!("veil libressl provider: OpenSSL error: {e}")))
    }

    fn scheme(&self) -> SignatureScheme {
        self.scheme
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::crypto::KeyProvider as _;

    fn ed25519_key_der() -> Vec<u8> {
        let pkey = PKey::generate_ed25519().unwrap();
        pkey.private_key_to_pkcs8().unwrap()
    }

    fn ecdsa_p256_key_der() -> Vec<u8> {
        let group =
            openssl::ec::EcGroup::from_curve_name(openssl::nid::Nid::X9_62_PRIME256V1).unwrap();
        let ec_key = openssl::ec::EcKey::generate(&group).unwrap();
        let pkey = PKey::from_ec_key(ec_key).unwrap();
        pkey.private_key_to_pkcs8().unwrap()
    }

    fn rsa_2048_key_der() -> Vec<u8> {
        let rsa = openssl::rsa::Rsa::generate(2048).unwrap();
        let pkey = PKey::from_rsa(rsa).unwrap();
        pkey.private_key_to_pkcs8().unwrap()
    }

    fn roundtrip(key_der: Vec<u8>, offered: &[SignatureScheme]) {
        let provider = KeyProvider;
        let signing_key = provider
            .load_private_key(PrivateKeyDer::Pkcs8(key_der.clone().into()))
            .unwrap();
        let signer = signing_key.choose_scheme(offered).expect("scheme chosen");
        let message = b"veil libressl provider signer roundtrip test message";
        let signature = signer.sign(message).unwrap();

        // 対応する openssl::sign::Verifier で検証する（自己ラウンドトリップ）。
        let scheme = signer.scheme();

        // 公開鍵で直接検証: PrivateKey から PKey を再取得できないため、
        // openssl 側で同じ DER から鍵を読み直して検証する。
        let pkey_for_verify = PKey::private_key_from_der(&key_der).unwrap();
        let digest = message_digest(scheme);
        let ok = if let Some(digest) = digest {
            openssl::sign::Verifier::new(digest, &pkey_for_verify)
                .and_then(|mut v| {
                    if let Some(padding) = rsa_padding(scheme) {
                        v.set_rsa_padding(padding)?;
                    }
                    if let Some(mgf1_md) = mgf1(scheme) {
                        v.set_rsa_mgf1_md(mgf1_md)?;
                    }
                    if let Some(len) = pss_salt_len(scheme) {
                        v.set_rsa_pss_saltlen(len)?;
                    }
                    v.update(message)?;
                    v.verify(&signature)
                })
                .unwrap()
        } else {
            openssl::sign::Verifier::new_without_digest(&pkey_for_verify)
                .and_then(|mut v| v.verify_oneshot(&signature, message))
                .unwrap()
        };
        assert!(ok, "signature failed to verify for scheme {scheme:?}");
    }

    #[test]
    fn ed25519_roundtrip() {
        roundtrip(ed25519_key_der(), &[SignatureScheme::ED25519]);
    }

    #[test]
    fn ecdsa_p256_roundtrip() {
        roundtrip(
            ecdsa_p256_key_der(),
            &[SignatureScheme::ECDSA_NISTP256_SHA256],
        );
    }

    #[test]
    fn rsa_pss_sha256_roundtrip() {
        roundtrip(rsa_2048_key_der(), &[SignatureScheme::RSA_PSS_SHA256]);
    }

    #[test]
    fn rsa_pkcs1_sha256_roundtrip() {
        roundtrip(rsa_2048_key_der(), &[SignatureScheme::RSA_PKCS1_SHA256]);
    }
}
