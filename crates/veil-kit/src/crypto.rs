// F-267: 暗号は veil の TLS と同じプロバイダの crate（OpenBSD・NetBSD は ring、他は aws-lc-rs）。API はほぼ同じ。
//! Thin wrappers over the crypto provider (HMAC, AES-256-GCM, random bytes, signature verification).

#[cfg(all(
    any(feature = "cookie", feature = "jwt", feature = "tasks"),
    not(any(target_os = "openbsd", target_os = "netbsd"))
))]
pub(crate) use aws_lc_rs as provider;
#[cfg(all(
    any(feature = "cookie", feature = "jwt", feature = "tasks"),
    any(target_os = "openbsd", target_os = "netbsd")
))]
pub(crate) use ring as provider;

/// Fills `buf` with random bytes from the OS.
#[cfg(any(feature = "cookie", feature = "tasks"))]
pub(crate) fn random(buf: &mut [u8]) {
    use provider::rand::SecureRandom as _;
    provider::rand::SystemRandom::new()
        .fill(buf)
        .expect("the system random number generator failed");
}

/// HMAC-SHA256 of `parts` (concatenated) with `key`.
#[cfg(feature = "cookie")]
pub(crate) fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let k = provider::hmac::Key::new(provider::hmac::HMAC_SHA256, key);
    let mut ctx = provider::hmac::Context::with_key(&k);
    for p in parts {
        ctx.update(p);
    }
    let tag = ctx.sign();
    let mut out = [0u8; 32];
    out.copy_from_slice(tag.as_ref());
    out
}

/// Constant-time comparison.
#[cfg(any(feature = "cookie", feature = "auth"))]
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for (x, y) in a.iter().zip(b) {
        d |= x ^ y;
    }
    d == 0
}

/// AES-256-GCM: `nonce (12) || ciphertext || tag (16)`, with `aad` authenticated.
#[cfg(feature = "cookie")]
pub(crate) fn seal(key: &[u8; 32], aad: &[u8], plain: &[u8]) -> Vec<u8> {
    use provider::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
    let k = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).expect("32-byte key"));
    let mut nonce = [0u8; 12];
    random(&mut nonce);
    let mut out = Vec::with_capacity(12 + plain.len() + 16);
    out.extend_from_slice(&nonce);
    let mut buf = plain.to_vec();
    k.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(aad),
        &mut buf,
    )
    .expect("AES-GCM seal");
    out.extend_from_slice(&buf);
    out
}

/// Opens what [`seal`] produced (`None` when it was altered or the key is wrong).
#[cfg(feature = "cookie")]
pub(crate) fn open(key: &[u8; 32], aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
    use provider::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
    if sealed.len() < 12 + 16 {
        return None;
    }
    let k = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).ok()?);
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&sealed[..12]);
    let mut buf = sealed[12..].to_vec();
    let plain = k
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad),
            &mut buf,
        )
        .ok()?;
    Some(plain.to_vec())
}
