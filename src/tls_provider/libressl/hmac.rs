//! `rustls::crypto::hmac::Hmac` 実装（F-142）。
//!
//! `openssl::sign::Signer` + `PKey::hmac` はクラシック API で LibreSSL でも使える。
//! TLS1.3 の HKDF は自前実装せず、この `Hmac` を `rustls::crypto::tls13::HkdfUsingHmac` に
//! 渡すことで賄う（AGENTS.md 指示・設計メモ通り）。TLS1.2 の PRF も同様に
//! `rustls::crypto::tls12::PrfUsingHmac` に委ねる。

use crate::tls_provider::libressl::hash::Algorithm as HashAlgorithm;
use openssl::pkey::{PKey, Private};
use openssl::sign::Signer as OpenSslSigner;
use rustls::crypto::hmac::{Hmac, Key, Tag};

/// HMAC-SHA256 / HMAC-SHA384 の `Hmac` 実装。
pub(crate) struct HmacUsingOpenSsl(pub(crate) HashAlgorithm);

pub(crate) static HMAC_SHA256: HmacUsingOpenSsl = HmacUsingOpenSsl(HashAlgorithm::Sha256);
pub(crate) static HMAC_SHA384: HmacUsingOpenSsl = HmacUsingOpenSsl(HashAlgorithm::Sha384);

struct HmacKey {
    key: PKey<Private>,
    hash: HashAlgorithm,
}

impl Hmac for HmacUsingOpenSsl {
    fn with_key(&self, key: &[u8]) -> Box<dyn Key> {
        Box::new(HmacKey {
            key: PKey::hmac(key).expect("veil: failed to build HMAC key from raw bytes"),
            hash: self.0,
        })
    }

    fn hash_output_len(&self) -> usize {
        self.0.message_digest().size()
    }
}

impl Key for HmacKey {
    fn sign_concat(&self, first: &[u8], middle: &[&[u8]], last: &[u8]) -> Tag {
        OpenSslSigner::new(self.hash.message_digest(), &self.key)
            .and_then(|mut signer| {
                signer.update(first)?;
                for chunk in middle {
                    signer.update(chunk)?;
                }
                signer.update(last)?;
                Ok(Tag::new(&signer.sign_to_vec()?))
            })
            .expect("veil: HMAC signing failed")
    }

    fn tag_len(&self) -> usize {
        self.hash.message_digest().size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4231 Test Case 1（HMAC-SHA256）の既知ベクタで sign() をラウンドトリップ確認する。
    #[test]
    fn hmac_sha256_rfc4231_case1() {
        let key = [0x0bu8; 20];
        let data = b"Hi There";
        let expected = [
            0xb0, 0x34, 0x4c, 0x61, 0xd8, 0xdb, 0x38, 0x53, 0x5c, 0xa8, 0xaf, 0xce, 0xaf, 0x0b,
            0xf1, 0x2b, 0x88, 0x1d, 0xc2, 0x00, 0xc9, 0x83, 0x3d, 0xa7, 0x26, 0xe9, 0x37, 0x6c,
            0x2e, 0x32, 0xcf, 0xf7,
        ];
        let key = HMAC_SHA256.with_key(&key);
        let tag = key.sign(&[data]);
        assert_eq!(tag.as_ref(), &expected[..]);
    }
}
