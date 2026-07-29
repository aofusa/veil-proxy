//! `rustls::crypto::hash::Hash` 実装（F-142）。
//!
//! `openssl::sha`（SHA-256/384 のワンショット・逐次計算 API）はクラシック API であり
//! LibreSSL でもそのまま使える。OpenSSL 3.x の `EVP_MD_fetch`/provider は使わない。

use openssl::hash::MessageDigest;
use openssl::sha;
use rustls::crypto::hash::{Context, Hash, HashAlgorithm, Output};

/// SHA-256 / SHA-384 の `Hash` 実装。
#[derive(Clone, Copy, Debug)]
pub(crate) enum Algorithm {
    Sha256,
    Sha384,
}

pub(crate) static SHA256: Algorithm = Algorithm::Sha256;
pub(crate) static SHA384: Algorithm = Algorithm::Sha384;

impl Algorithm {
    /// HMAC 署名用の `MessageDigest`（`openssl::sign::Signer` に渡す）。
    pub(crate) fn message_digest(self) -> MessageDigest {
        match self {
            Algorithm::Sha256 => MessageDigest::sha256(),
            Algorithm::Sha384 => MessageDigest::sha384(),
        }
    }
}

/// 逐次ハッシュ計算のコンテキスト。
#[derive(Clone)]
enum HashContext {
    Sha256(sha::Sha256),
    Sha384(sha::Sha384),
}

impl Hash for Algorithm {
    fn start(&self) -> Box<dyn Context> {
        match self {
            Algorithm::Sha256 => Box::new(HashContext::Sha256(sha::Sha256::new())),
            Algorithm::Sha384 => Box::new(HashContext::Sha384(sha::Sha384::new())),
        }
    }

    fn hash(&self, data: &[u8]) -> Output {
        match self {
            Algorithm::Sha256 => Output::new(&sha::sha256(data)),
            Algorithm::Sha384 => Output::new(&sha::sha384(data)),
        }
    }

    fn output_len(&self) -> usize {
        self.message_digest().size()
    }

    fn algorithm(&self) -> HashAlgorithm {
        match self {
            Algorithm::Sha256 => HashAlgorithm::SHA256,
            Algorithm::Sha384 => HashAlgorithm::SHA384,
        }
    }
}

impl HashContext {
    fn finish_inner(self) -> Output {
        match self {
            Self::Sha256(ctx) => Output::new(&ctx.finish()),
            Self::Sha384(ctx) => Output::new(&ctx.finish()),
        }
    }
}

impl Context for HashContext {
    fn fork_finish(&self) -> Output {
        self.clone().finish_inner()
    }

    fn fork(&self) -> Box<dyn Context> {
        Box::new(self.clone())
    }

    fn finish(self: Box<Self>) -> Output {
        (*self).finish_inner()
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Self::Sha256(ctx) => ctx.update(data),
            Self::Sha384(ctx) => ctx.update(data),
        }
    }
}
