//! AEAD（AES-GCM / ChaCha20-Poly1305）の EVP レベル実装（F-142）。
//!
//! `openssl::cipher_ctx::CipherCtx` はクラシック API（`EVP_CipherInit_ex`/`EVP_CipherUpdate`/
//! `EVP_CipherFinal_ex`）のラッパーで LibreSSL でも動く。
//!
//! **ホットパス注意**: TLS レコードごとに呼ばれるため、`CipherCtx` はレコードごとに
//! `CipherCtx::new()` せず、`Tls13Crypter`/TLS1.2 の各 Encrypter/Decrypter が生成時
//! （ハンドシェイク完了時、コネクションごとに一度だけ）に確保したものを使い回す。
//! `encrypt_init`/`decrypt_init` は同一 `CipherCtx` に対して繰り返し呼べる
//! （`EVP_CipherInit_ex` は鍵/IV の再設定のみで内部バッファを再アロケートしない）。

use openssl::cipher::{Cipher, CipherRef};
use openssl::cipher_ctx::CipherCtx;
use rustls::crypto::cipher::NONCE_LEN;
use rustls::Error;

/// タグ長は対応する全アルゴリズムで 16 バイト固定。
pub(crate) const TAG_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Algorithm {
    Aes128Gcm,
    Aes256Gcm,
    Chacha20Poly1305,
}

impl Algorithm {
    fn openssl_cipher(self) -> &'static CipherRef {
        match self {
            Self::Aes128Gcm => Cipher::aes_128_gcm(),
            Self::Aes256Gcm => Cipher::aes_256_gcm(),
            Self::Chacha20Poly1305 => Cipher::chacha20_poly1305(),
        }
    }

    pub(crate) fn key_size(self) -> usize {
        self.openssl_cipher().key_length()
    }

    /// 新しい再利用可能な `CipherCtx` を確保する（コネクション確立時に一度だけ呼ぶ）。
    pub(crate) fn new_ctx(self) -> CipherCtx {
        CipherCtx::new().expect("veil: failed to allocate EVP_CIPHER_CTX")
    }

    /// `data` をインプレースで暗号化しタグを返す。`ctx` は呼び出し元が保持し続ける
    /// 再利用可能なコンテキスト（ホットパスでの新規確保を避けるため）。
    pub(crate) fn encrypt_in_place(
        self,
        ctx: &mut CipherCtx,
        key: &[u8],
        nonce: &[u8; NONCE_LEN],
        aad: &[u8],
        data: &mut [u8],
    ) -> Result<[u8; TAG_LEN], Error> {
        (|| -> Result<[u8; TAG_LEN], openssl::error::ErrorStack> {
            ctx.encrypt_init(Some(self.openssl_cipher()), Some(key), Some(nonce))?;
            // 出力バッファを渡さないと AAD として扱われる。
            ctx.cipher_update(aad, None)?;
            let count = ctx.cipher_update_inplace(data, data.len())?;
            debug_assert_eq!(count, data.len());
            let rest = ctx.cipher_final(&mut [])?;
            debug_assert_eq!(rest, 0);
            let mut tag = [0u8; TAG_LEN];
            ctx.tag(&mut tag)?;
            Ok(tag)
        })()
        .map_err(|e| Error::General(format!("veil libressl provider: OpenSSL error: {e}")))
    }

    /// `data`（末尾にタグ付き）をインプレースで復号する。平文長を返す。
    pub(crate) fn decrypt_in_place(
        self,
        ctx: &mut CipherCtx,
        key: &[u8],
        nonce: &[u8; NONCE_LEN],
        aad: &[u8],
        data: &mut [u8],
    ) -> Result<usize, Error> {
        let payload_len = data.len();
        if payload_len < TAG_LEN {
            return Err(Error::DecryptError);
        }
        let (ciphertext, tag) = data.split_at_mut(payload_len - TAG_LEN);

        (|| -> Result<usize, openssl::error::ErrorStack> {
            ctx.decrypt_init(Some(self.openssl_cipher()), Some(key), Some(nonce))?;
            ctx.cipher_update(aad, None)?;
            ctx.set_tag(tag)?;
            let count = ctx.cipher_update_inplace(ciphertext, ciphertext.len())?;
            debug_assert_eq!(count, ciphertext.len());
            let rest = ctx.cipher_final(&mut [])?;
            debug_assert_eq!(rest, 0);
            Ok(count + rest)
        })()
        .map_err(|_| Error::DecryptError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(algo: Algorithm) {
        let key = vec![0x42u8; algo.key_size()];
        let nonce = [0x24u8; NONCE_LEN];
        let aad = b"aad-data";
        let plaintext = b"hello, libressl aead roundtrip! this spans more than one block.".to_vec();

        let mut ctx = algo.new_ctx();
        let mut buf = plaintext.clone();
        let tag = algo
            .encrypt_in_place(&mut ctx, &key, &nonce, aad, &mut buf)
            .unwrap();

        // 暗号文は平文と異なること（無変換バグの検出）。
        assert_ne!(buf, plaintext);

        buf.extend_from_slice(&tag);
        // 同じ ctx を再利用して復号する（ホットパスの再利用パスを検証）。
        let plaintext_len = algo
            .decrypt_in_place(&mut ctx, &key, &nonce, aad, &mut buf)
            .unwrap();
        buf.truncate(plaintext_len);
        assert_eq!(buf, plaintext);

        // タグ改竄は検出されること。
        let mut tampered = plaintext.clone();
        let mut tag2 = algo
            .encrypt_in_place(&mut ctx, &key, &nonce, aad, &mut tampered)
            .unwrap();
        tag2[0] ^= 0xff;
        tampered.extend_from_slice(&tag2);
        assert!(algo
            .decrypt_in_place(&mut ctx, &key, &nonce, aad, &mut tampered)
            .is_err());
    }

    #[test]
    fn aes_128_gcm_roundtrip() {
        roundtrip(Algorithm::Aes128Gcm);
    }

    #[test]
    fn aes_256_gcm_roundtrip() {
        roundtrip(Algorithm::Aes256Gcm);
    }

    #[test]
    fn chacha20_poly1305_roundtrip() {
        roundtrip(Algorithm::Chacha20Poly1305);
    }

    /// 同一 `CipherCtx` を暗号化→復号→暗号化と繰り返し使い回せること
    /// （ホットパスで `CipherCtx::new()` を毎回呼ばない設計の検証）。
    #[test]
    fn ctx_reused_across_multiple_records() {
        let algo = Algorithm::Aes128Gcm;
        let key = vec![0x11u8; algo.key_size()];
        let aad = b"aad";
        let mut ctx = algo.new_ctx();

        for i in 0..8u8 {
            let nonce = [i; NONCE_LEN];
            let plaintext = format!("record number {i}").into_bytes();
            let mut buf = plaintext.clone();
            let tag = algo
                .encrypt_in_place(&mut ctx, &key, &nonce, aad, &mut buf)
                .unwrap();
            buf.extend_from_slice(&tag);
            let len = algo
                .decrypt_in_place(&mut ctx, &key, &nonce, aad, &mut buf)
                .unwrap();
            buf.truncate(len);
            assert_eq!(buf, plaintext);
        }
    }
}
