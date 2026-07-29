//! `rustls_webpki::SignatureVerificationAlgorithm` 実装（F-142）。
//!
//! ECDSA P-256/P-384、RSA PKCS#1 v1.5、RSA PSS、Ed25519 に対応する。
//! Ed448 は LibreSSL に無いため実装しない（設計メモ・AGENTS.md 指示）。

use core::fmt;
use openssl::bn::BigNumContext;
use openssl::ec::{EcGroup, EcKey, EcPoint};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::{Id, PKey, Public};
use openssl::rsa::{Padding, Rsa};
use openssl::sign::{RsaPssSaltlen, Verifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::alg_id;
use rustls::pki_types::{AlgorithmIdentifier, InvalidSignature, SignatureVerificationAlgorithm};
use rustls::SignatureScheme;

/// このプロバイダが対応する署名検証アルゴリズム一覧。
pub static SUPPORTED_SIG_ALGS: WebPkiSupportedAlgorithms = WebPkiSupportedAlgorithms {
    all: &[
        ECDSA_P256_SHA256,
        ECDSA_P256_SHA384,
        ECDSA_P384_SHA256,
        ECDSA_P384_SHA384,
        ED25519,
        RSA_PSS_SHA512,
        RSA_PSS_SHA384,
        RSA_PSS_SHA256,
        RSA_PKCS1_SHA512,
        RSA_PKCS1_SHA384,
        RSA_PKCS1_SHA256,
    ],
    mapping: &[
        // TLS1.2 では曲線が SignatureScheme で固定されないため候補を複数挙げる。TLS1.3 は固定。
        (
            SignatureScheme::ECDSA_NISTP384_SHA384,
            &[ECDSA_P384_SHA384, ECDSA_P256_SHA384],
        ),
        (
            SignatureScheme::ECDSA_NISTP256_SHA256,
            &[ECDSA_P256_SHA256, ECDSA_P384_SHA256],
        ),
        (SignatureScheme::ED25519, &[ED25519]),
        (SignatureScheme::RSA_PSS_SHA512, &[RSA_PSS_SHA512]),
        (SignatureScheme::RSA_PSS_SHA384, &[RSA_PSS_SHA384]),
        (SignatureScheme::RSA_PSS_SHA256, &[RSA_PSS_SHA256]),
        (SignatureScheme::RSA_PKCS1_SHA512, &[RSA_PKCS1_SHA512]),
        (SignatureScheme::RSA_PKCS1_SHA384, &[RSA_PKCS1_SHA384]),
        (SignatureScheme::RSA_PKCS1_SHA256, &[RSA_PKCS1_SHA256]),
    ],
};

pub(crate) static RSA_PKCS1_SHA256: &dyn SignatureVerificationAlgorithm = &OpenSslAlgorithm {
    display_name: "RSA_PKCS1_SHA256",
    public_key_alg_id: alg_id::RSA_ENCRYPTION,
    signature_alg_id: alg_id::RSA_PKCS1_SHA256,
};
pub(crate) static RSA_PKCS1_SHA384: &dyn SignatureVerificationAlgorithm = &OpenSslAlgorithm {
    display_name: "RSA_PKCS1_SHA384",
    public_key_alg_id: alg_id::RSA_ENCRYPTION,
    signature_alg_id: alg_id::RSA_PKCS1_SHA384,
};
pub(crate) static RSA_PKCS1_SHA512: &dyn SignatureVerificationAlgorithm = &OpenSslAlgorithm {
    display_name: "RSA_PKCS1_SHA512",
    public_key_alg_id: alg_id::RSA_ENCRYPTION,
    signature_alg_id: alg_id::RSA_PKCS1_SHA512,
};
pub(crate) static RSA_PSS_SHA256: &dyn SignatureVerificationAlgorithm = &OpenSslAlgorithm {
    display_name: "RSA_PSS_SHA256",
    public_key_alg_id: alg_id::RSA_ENCRYPTION,
    signature_alg_id: alg_id::RSA_PSS_SHA256,
};
pub(crate) static RSA_PSS_SHA384: &dyn SignatureVerificationAlgorithm = &OpenSslAlgorithm {
    display_name: "RSA_PSS_SHA384",
    public_key_alg_id: alg_id::RSA_ENCRYPTION,
    signature_alg_id: alg_id::RSA_PSS_SHA384,
};
pub(crate) static RSA_PSS_SHA512: &dyn SignatureVerificationAlgorithm = &OpenSslAlgorithm {
    display_name: "RSA_PSS_SHA512",
    public_key_alg_id: alg_id::RSA_ENCRYPTION,
    signature_alg_id: alg_id::RSA_PSS_SHA512,
};
pub(crate) static ED25519: &dyn SignatureVerificationAlgorithm = &OpenSslAlgorithm {
    display_name: "ED25519",
    public_key_alg_id: alg_id::ED25519,
    signature_alg_id: alg_id::ED25519,
};
pub(crate) static ECDSA_P256_SHA256: &dyn SignatureVerificationAlgorithm = &OpenSslAlgorithm {
    display_name: "ECDSA_P256_SHA256",
    public_key_alg_id: alg_id::ECDSA_P256,
    signature_alg_id: alg_id::ECDSA_SHA256,
};
pub(crate) static ECDSA_P256_SHA384: &dyn SignatureVerificationAlgorithm = &OpenSslAlgorithm {
    display_name: "ECDSA_P256_SHA384",
    public_key_alg_id: alg_id::ECDSA_P256,
    signature_alg_id: alg_id::ECDSA_SHA384,
};
pub(crate) static ECDSA_P384_SHA256: &dyn SignatureVerificationAlgorithm = &OpenSslAlgorithm {
    display_name: "ECDSA_P384_SHA256",
    public_key_alg_id: alg_id::ECDSA_P384,
    signature_alg_id: alg_id::ECDSA_SHA256,
};
pub(crate) static ECDSA_P384_SHA384: &dyn SignatureVerificationAlgorithm = &OpenSslAlgorithm {
    display_name: "ECDSA_P384_SHA384",
    public_key_alg_id: alg_id::ECDSA_P384,
    signature_alg_id: alg_id::ECDSA_SHA384,
};

struct OpenSslAlgorithm {
    display_name: &'static str,
    public_key_alg_id: AlgorithmIdentifier,
    signature_alg_id: AlgorithmIdentifier,
}

impl fmt::Debug for OpenSslAlgorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "veil libressl provider signature algorithm: {}",
            self.display_name
        )
    }
}

fn ecdsa_public_key(curve: Nid, public_key: &[u8]) -> Result<PKey<Public>, InvalidSignature> {
    EcGroup::from_curve_name(curve)
        .and_then(|group| {
            let mut ctx = BigNumContext::new()?;
            let point = EcPoint::from_bytes(&group, public_key, &mut ctx)?;
            let key = EcKey::from_public_key(&group, &point)?;
            key.try_into()
        })
        .map_err(|_| InvalidSignature)
}

impl OpenSslAlgorithm {
    fn public_key(&self, public_key: &[u8]) -> Result<PKey<Public>, InvalidSignature> {
        match self.public_key_alg_id {
            alg_id::RSA_ENCRYPTION => Rsa::public_key_from_der_pkcs1(public_key)
                .and_then(TryInto::try_into)
                .map_err(|_| InvalidSignature),
            alg_id::ECDSA_P384 => ecdsa_public_key(Nid::SECP384R1, public_key),
            alg_id::ECDSA_P256 => ecdsa_public_key(Nid::X9_62_PRIME256V1, public_key),
            alg_id::ED25519 => PKey::public_key_from_raw_bytes(public_key, Id::ED25519)
                .map_err(|_| InvalidSignature),
            _ => Err(InvalidSignature),
        }
    }

    fn message_digest(&self) -> Option<MessageDigest> {
        match self.signature_alg_id {
            alg_id::RSA_PKCS1_SHA256 | alg_id::ECDSA_SHA256 | alg_id::RSA_PSS_SHA256 => {
                Some(MessageDigest::sha256())
            }
            alg_id::RSA_PKCS1_SHA384 | alg_id::ECDSA_SHA384 | alg_id::RSA_PSS_SHA384 => {
                Some(MessageDigest::sha384())
            }
            alg_id::RSA_PKCS1_SHA512 | alg_id::RSA_PSS_SHA512 => Some(MessageDigest::sha512()),
            _ => None,
        }
    }

    fn mgf1(&self) -> Option<MessageDigest> {
        match self.signature_alg_id {
            alg_id::RSA_PSS_SHA256 => Some(MessageDigest::sha256()),
            alg_id::RSA_PSS_SHA384 => Some(MessageDigest::sha384()),
            alg_id::RSA_PSS_SHA512 => Some(MessageDigest::sha512()),
            _ => None,
        }
    }

    fn pss_salt_len(&self) -> Option<RsaPssSaltlen> {
        match self.signature_alg_id {
            alg_id::RSA_PSS_SHA256 | alg_id::RSA_PSS_SHA384 | alg_id::RSA_PSS_SHA512 => {
                Some(RsaPssSaltlen::DIGEST_LENGTH)
            }
            _ => None,
        }
    }

    fn rsa_padding(&self) -> Option<Padding> {
        match self.signature_alg_id {
            alg_id::RSA_PSS_SHA256 | alg_id::RSA_PSS_SHA384 | alg_id::RSA_PSS_SHA512 => {
                Some(Padding::PKCS1_PSS)
            }
            alg_id::RSA_PKCS1_SHA256 | alg_id::RSA_PKCS1_SHA384 | alg_id::RSA_PKCS1_SHA512 => {
                Some(Padding::PKCS1)
            }
            _ => None,
        }
    }
}

impl SignatureVerificationAlgorithm for OpenSslAlgorithm {
    fn public_key_alg_id(&self) -> AlgorithmIdentifier {
        self.public_key_alg_id
    }

    fn signature_alg_id(&self) -> AlgorithmIdentifier {
        self.signature_alg_id
    }

    fn verify_signature(
        &self,
        public_key: &[u8],
        message: &[u8],
        signature: &[u8],
    ) -> Result<(), InvalidSignature> {
        if matches!(
            self.public_key_alg_id,
            alg_id::ECDSA_P256 | alg_id::ECDSA_P384
        ) {
            // 非圧縮形式以外の EC 公開鍵は拒否する（RFC 5480 §2.2）。
            match public_key.first() {
                Some(0x02..=0x04) => {}
                _ => return Err(InvalidSignature),
            }
        }

        let pkey = self.public_key(public_key)?;

        let verified = if let Some(digest) = self.message_digest() {
            Verifier::new(digest, &pkey).and_then(|mut verifier| {
                if let Some(padding) = self.rsa_padding() {
                    verifier.set_rsa_padding(padding)?;
                }
                if let Some(mgf1_md) = self.mgf1() {
                    verifier.set_rsa_mgf1_md(mgf1_md)?;
                }
                if let Some(salt_len) = self.pss_salt_len() {
                    verifier.set_rsa_pss_saltlen(salt_len)?;
                }
                verifier.update(message)?;
                verifier.verify(signature)
            })
        } else {
            Verifier::new_without_digest(&pkey)
                .and_then(|mut verifier| verifier.verify_oneshot(signature, message))
        };

        match verified {
            Ok(true) => Ok(()),
            _ => Err(InvalidSignature),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_format() {
        assert_eq!(
            format!("{:?}", ECDSA_P256_SHA256),
            "veil libressl provider signature algorithm: ECDSA_P256_SHA256"
        );
    }

    /// 生成した鍵で署名し、`verify_signature` でラウンドトリップ検証する。
    #[test]
    fn ecdsa_p256_sha256_verify_roundtrip() {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        let ec_key = EcKey::generate(&group).unwrap();
        let mut ctx = BigNumContext::new().unwrap();
        let pub_key_bytes = ec_key
            .public_key()
            .to_bytes(
                &group,
                openssl::ec::PointConversionForm::UNCOMPRESSED,
                &mut ctx,
            )
            .unwrap();
        let pkey = PKey::from_ec_key(ec_key).unwrap();

        let message = b"veil libressl provider verify roundtrip";
        let signature = openssl::sign::Signer::new(MessageDigest::sha256(), &pkey)
            .and_then(|mut signer| {
                signer.update(message)?;
                signer.sign_to_vec()
            })
            .unwrap();

        ECDSA_P256_SHA256
            .verify_signature(&pub_key_bytes, message, &signature)
            .expect("valid signature must verify");

        let mut tampered = signature.clone();
        tampered[0] ^= 0xff;
        assert!(ECDSA_P256_SHA256
            .verify_signature(&pub_key_bytes, message, &tampered)
            .is_err());
    }

    #[test]
    fn ed25519_verify_roundtrip() {
        let pkey = PKey::generate_ed25519().unwrap();
        let pub_key_bytes = pkey.raw_public_key().unwrap();
        let message = b"ed25519 roundtrip message";
        let signature = openssl::sign::Signer::new_without_digest(&pkey)
            .and_then(|mut signer| signer.sign_oneshot_to_vec(message))
            .unwrap();

        ED25519
            .verify_signature(&pub_key_bytes, message, &signature)
            .expect("valid ed25519 signature must verify");
    }

    #[test]
    fn rsa_pkcs1_sha256_verify_roundtrip() {
        let rsa = Rsa::generate(2048).unwrap();
        let pub_key_bytes = rsa.public_key_to_der_pkcs1().unwrap();
        let pkey = PKey::from_rsa(rsa).unwrap();
        let message = b"rsa pkcs1 roundtrip message";
        let signature = openssl::sign::Signer::new(MessageDigest::sha256(), &pkey)
            .and_then(|mut signer| {
                signer.set_rsa_padding(Padding::PKCS1)?;
                signer.update(message)?;
                signer.sign_to_vec()
            })
            .unwrap();

        RSA_PKCS1_SHA256
            .verify_signature(&pub_key_bytes, message, &signature)
            .expect("valid rsa pkcs1 signature must verify");
    }
}
