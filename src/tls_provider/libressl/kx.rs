//! 鍵交換グループ（X25519 / secp256r1 / secp384r1）実装（F-142）。
//!
//! `openssl::pkey::PKey` + `openssl::derive::Deriver` はクラシック API で LibreSSL でも
//! 使える（X25519 は `PKey::generate_x25519`、EC は `openssl::ec` + `Deriver`）。

use openssl::bn::BigNumContext;
use openssl::derive::Deriver;
use openssl::ec::{EcGroup, EcKey, EcPoint, PointConversionForm};
use openssl::nid::Nid;
use openssl::pkey::{Id, PKey, Private, Public};
use rustls::crypto::{ActiveKeyExchange, SharedSecret, SupportedKxGroup};
use rustls::{Error, NamedGroup};

/// このプロバイダが対応する鍵交換グループ（優先順位順）: X25519 → secp256r1 → secp384r1。
pub(crate) static SUPPORTED_KX_GROUPS: &[&dyn SupportedKxGroup] =
    &[&X25519KxGroup, &EcKxGroup::SECP256R1, &EcKxGroup::SECP384R1];

#[derive(Debug)]
struct X25519KxGroup;

struct X25519KeyExchange {
    private_key: PKey<Private>,
    public_key: Vec<u8>,
}

impl SupportedKxGroup for X25519KxGroup {
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
        PKey::generate_x25519()
            .and_then(|private_key| {
                let public_key = private_key.raw_public_key()?;
                Ok(Box::new(X25519KeyExchange {
                    private_key,
                    public_key,
                }) as Box<dyn ActiveKeyExchange>)
            })
            .map_err(|e| Error::General(format!("veil libressl provider: OpenSSL error: {e}")))
    }

    fn name(&self) -> NamedGroup {
        NamedGroup::X25519
    }
}

impl ActiveKeyExchange for X25519KeyExchange {
    fn complete(self: Box<Self>, peer_pub_key: &[u8]) -> Result<SharedSecret, Error> {
        PKey::public_key_from_raw_bytes(peer_pub_key, Id::X25519)
            .and_then(|peer_key| {
                let mut deriver = Deriver::new(&self.private_key)?;
                deriver.set_peer(&peer_key)?;
                let secret = deriver.derive_to_vec()?;
                Ok(SharedSecret::from(secret.as_slice()))
            })
            .map_err(|e| Error::General(format!("veil libressl provider: OpenSSL error: {e}")))
    }

    fn pub_key(&self) -> &[u8] {
        &self.public_key
    }

    fn group(&self) -> NamedGroup {
        NamedGroup::X25519
    }
}

#[derive(Debug)]
struct EcKxGroup {
    name: NamedGroup,
    nid: Nid,
}

impl EcKxGroup {
    const SECP256R1: EcKxGroup = EcKxGroup {
        name: NamedGroup::secp256r1,
        nid: Nid::X9_62_PRIME256V1,
    };
    const SECP384R1: EcKxGroup = EcKxGroup {
        name: NamedGroup::secp384r1,
        nid: Nid::SECP384R1,
    };
}

struct EcKeyExchange {
    priv_key: EcKey<Private>,
    group: EcGroup,
    name: NamedGroup,
    pub_key: Vec<u8>,
}

impl SupportedKxGroup for EcKxGroup {
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
        EcGroup::from_curve_name(self.nid)
            .and_then(|group| {
                let priv_key = EcKey::generate(&group)?;
                let mut ctx = BigNumContext::new()?;
                let pub_key = priv_key.public_key().to_bytes(
                    &group,
                    PointConversionForm::UNCOMPRESSED,
                    &mut ctx,
                )?;
                Ok(Box::new(EcKeyExchange {
                    priv_key,
                    group,
                    name: self.name,
                    pub_key,
                }) as Box<dyn ActiveKeyExchange>)
            })
            .map_err(|e| Error::General(format!("veil libressl provider: OpenSSL error: {e}")))
    }

    fn name(&self) -> NamedGroup {
        self.name
    }
}

impl EcKeyExchange {
    fn load_peer_key(
        &self,
        peer_pub_key: &[u8],
    ) -> Result<PKey<Public>, openssl::error::ErrorStack> {
        let mut ctx = BigNumContext::new()?;
        let point = EcPoint::from_bytes(&self.group, peer_pub_key, &mut ctx)?;
        let peer_key = EcKey::from_public_key(&self.group, &point)?;
        peer_key.check_key()?;
        peer_key.try_into()
    }
}

impl ActiveKeyExchange for EcKeyExchange {
    fn complete(self: Box<Self>, peer_pub_key: &[u8]) -> Result<SharedSecret, Error> {
        // 非圧縮形式以外の公開鍵は拒否する（RFC 5480 §2.2）。
        if peer_pub_key.first() != Some(&0x04) {
            return Err(Error::PeerMisbehaved(
                rustls::PeerMisbehaved::InvalidKeyShare,
            ));
        }

        self.load_peer_key(peer_pub_key)
            .and_then(|peer_key| {
                let key: PKey<_> = self.priv_key.try_into()?;
                let mut deriver = Deriver::new(&key)?;
                deriver.set_peer(&peer_key)?;
                let secret = deriver.derive_to_vec()?;
                Ok(SharedSecret::from(secret.as_slice()))
            })
            .map_err(|e| Error::General(format!("veil libressl provider: OpenSSL error: {e}")))
    }

    fn pub_key(&self) -> &[u8] {
        &self.pub_key
    }

    fn group(&self) -> NamedGroup {
        self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// X25519: 両者が `start()` した鍵交換を互いの公開鍵で `complete()` し、
    /// 共有秘密が一致することを確認する。
    #[test]
    fn x25519_shared_secret_matches() {
        let a = X25519KxGroup.start().unwrap();
        let b = X25519KxGroup.start().unwrap();
        let a_pub = a.pub_key().to_vec();
        let b_pub = b.pub_key().to_vec();
        let secret_a = a.complete(&b_pub).unwrap();
        let secret_b = b.complete(&a_pub).unwrap();
        assert_eq!(secret_a.secret_bytes(), secret_b.secret_bytes());
    }

    #[test]
    fn secp256r1_shared_secret_matches() {
        let group = EcKxGroup::SECP256R1;
        let a = group.start().unwrap();
        let b = group.start().unwrap();
        let a_pub = a.pub_key().to_vec();
        let b_pub = b.pub_key().to_vec();
        let secret_a = a.complete(&b_pub).unwrap();
        let secret_b = b.complete(&a_pub).unwrap();
        assert_eq!(secret_a.secret_bytes(), secret_b.secret_bytes());
    }

    #[test]
    fn secp384r1_shared_secret_matches() {
        let group = EcKxGroup::SECP384R1;
        let a = group.start().unwrap();
        let b = group.start().unwrap();
        let a_pub = a.pub_key().to_vec();
        let b_pub = b.pub_key().to_vec();
        let secret_a = a.complete(&b_pub).unwrap();
        let secret_b = b.complete(&a_pub).unwrap();
        assert_eq!(secret_a.secret_bytes(), secret_b.secret_bytes());
    }
}
