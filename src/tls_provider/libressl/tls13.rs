//! TLS1.3 cipher suites（F-142）。
//!
//! HKDF は自前実装せず `rustls::crypto::tls13::HkdfUsingHmac` に
//! `src/tls_provider/libressl/hmac.rs` の `Hmac` 実装を渡すだけで賄う。

use crate::tls_provider::libressl::aead::{self, TAG_LEN};
use crate::tls_provider::libressl::hash::{SHA256, SHA384};
use crate::tls_provider::libressl::hmac::{HMAC_SHA256, HMAC_SHA384};
use openssl::cipher_ctx::CipherCtx;
use rustls::crypto::cipher::{
    make_tls13_aad, AeadKey, InboundOpaqueMessage, InboundPlainMessage, Iv, MessageDecrypter,
    MessageEncrypter, Nonce, OutboundOpaqueMessage, OutboundPlainMessage, PrefixedPayload,
    Tls13AeadAlgorithm, UnsupportedOperationError,
};
use rustls::crypto::tls13::HkdfUsingHmac;
use rustls::crypto::CipherSuiteCommon;
use rustls::{
    CipherSuite, ConnectionTrafficSecrets, Error, SupportedCipherSuite, Tls13CipherSuite,
};

pub(crate) static TLS13_AES_128_GCM_SHA256: SupportedCipherSuite =
    SupportedCipherSuite::Tls13(&Tls13CipherSuite {
        common: CipherSuiteCommon {
            suite: CipherSuite::TLS13_AES_128_GCM_SHA256,
            hash_provider: &SHA256,
            confidentiality_limit: 1 << 23,
        },
        hkdf_provider: &HkdfUsingHmac(&HMAC_SHA256),
        aead_alg: &aead::Algorithm::Aes128Gcm,
        quic: None,
    });

pub(crate) static TLS13_AES_256_GCM_SHA384: SupportedCipherSuite =
    SupportedCipherSuite::Tls13(&Tls13CipherSuite {
        common: CipherSuiteCommon {
            suite: CipherSuite::TLS13_AES_256_GCM_SHA384,
            hash_provider: &SHA384,
            confidentiality_limit: 1 << 23,
        },
        hkdf_provider: &HkdfUsingHmac(&HMAC_SHA384),
        aead_alg: &aead::Algorithm::Aes256Gcm,
        quic: None,
    });

pub(crate) static TLS13_CHACHA20_POLY1305_SHA256: SupportedCipherSuite =
    SupportedCipherSuite::Tls13(&Tls13CipherSuite {
        common: CipherSuiteCommon {
            suite: CipherSuite::TLS13_CHACHA20_POLY1305_SHA256,
            hash_provider: &SHA256,
            confidentiality_limit: u64::MAX,
        },
        hkdf_provider: &HkdfUsingHmac(&HMAC_SHA256),
        aead_alg: &aead::Algorithm::Chacha20Poly1305,
        quic: None,
    });

/// 暗号化・復号どちらの向きでも使う。`ctx` はコネクション確立時に一度だけ確保し、
/// レコードごとに使い回す（ホットパスでの `CipherCtx::new()` 禁止）。
struct Tls13Crypter {
    algo: aead::Algorithm,
    key: AeadKey,
    iv: Iv,
    ctx: CipherCtx,
}

impl Tls13AeadAlgorithm for aead::Algorithm {
    fn encrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageEncrypter> {
        Box::new(Tls13Crypter {
            algo: *self,
            key,
            iv,
            ctx: self.new_ctx(),
        })
    }

    fn decrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageDecrypter> {
        Box::new(Tls13Crypter {
            algo: *self,
            key,
            iv,
            ctx: self.new_ctx(),
        })
    }

    fn key_len(&self) -> usize {
        self.key_size()
    }

    fn extract_keys(
        &self,
        key: AeadKey,
        iv: Iv,
    ) -> Result<ConnectionTrafficSecrets, UnsupportedOperationError> {
        Ok(match self {
            aead::Algorithm::Aes128Gcm => ConnectionTrafficSecrets::Aes128Gcm { key, iv },
            aead::Algorithm::Aes256Gcm => ConnectionTrafficSecrets::Aes256Gcm { key, iv },
            aead::Algorithm::Chacha20Poly1305 => {
                ConnectionTrafficSecrets::Chacha20Poly1305 { key, iv }
            }
        })
    }
}

impl MessageEncrypter for Tls13Crypter {
    fn encrypt(
        &mut self,
        msg: OutboundPlainMessage,
        seq: u64,
    ) -> Result<OutboundOpaqueMessage, Error> {
        let total_len = self.encrypted_payload_len(msg.payload.len());
        let mut payload = PrefixedPayload::with_capacity(total_len);
        let aad = make_tls13_aad(total_len);
        payload.extend_from_chunks(&msg.payload);
        payload.extend_from_slice(&msg.typ.to_array());
        let tag = self.algo.encrypt_in_place(
            &mut self.ctx,
            self.key.as_ref(),
            &Nonce::new(&self.iv, seq).0,
            &aad,
            payload.as_mut(),
        )?;
        payload.extend_from_slice(&tag);
        Ok(OutboundOpaqueMessage::new(
            rustls::ContentType::ApplicationData,
            // TLS1.3 のアプリケーションデータレコードは legacy record version として
            // 常に TLSv1_2 (0x0303) を使う（RFC 8446 §5.1）。
            rustls::ProtocolVersion::TLSv1_2,
            payload,
        ))
    }

    fn encrypted_payload_len(&self, payload_len: usize) -> usize {
        payload_len + 1 + TAG_LEN
    }
}

impl MessageDecrypter for Tls13Crypter {
    fn decrypt<'a>(
        &mut self,
        mut msg: InboundOpaqueMessage<'a>,
        seq: u64,
    ) -> Result<InboundPlainMessage<'a>, Error> {
        let payload = &mut msg.payload;
        let aad = make_tls13_aad(payload.len());
        let plaintext_len = self.algo.decrypt_in_place(
            &mut self.ctx,
            self.key.as_ref(),
            &Nonce::new(&self.iv, seq).0,
            &aad,
            payload.as_mut(),
        )?;
        payload.truncate(plaintext_len);
        msg.into_tls13_unpadded_message()
    }
}
