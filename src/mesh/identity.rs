//! ワークロード ID の mTLS（F-217）。
//!
//! - 証明書・鍵・信頼アンカーは名前（Istio の SDS の `default`・`ROOTCA`、ゲートウェイの `credentialName`）ごとに
//!   `ArcSwap` で持ち、制御プレーン（SDS・Linkerd の Identity）から専用スレッドで差し替える。rustls の設定は
//!   作り直さない（証明書はリゾルバ、信頼アンカーは検証器がハンドシェイクのたびに現在の値を読む）。
//!   鍵はメモリのみ（ファイルに書かない）。
//! - 相手の ID はクライアント証明書・サーバ証明書の SAN（Istio は SPIFFE の URI、Linkerd は DNS）。
//!   ホスト名（SNI）ではなく SAN の条件で検証する（Istio の `match_subject_alt_names`）。

use super::model::StringMatcher;
use arc_swap::ArcSwap;
use once_cell::sync::Lazy;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::sign::CertifiedKey;
use rustls::{DigitallySignedStruct, DistinguishedName, RootCertStore, SignatureScheme};
use std::collections::HashMap;
use std::sync::Arc;

/// rustls の暗号プロバイダ（veil の TLS と同じ。`tls_provider`）。
pub static PROVIDER: Lazy<Arc<CryptoProvider>> =
    Lazy::new(|| Arc::new(crate::tls_provider::provider::default_provider()));

/// 信頼アンカー。
pub struct Roots {
    pub store: Arc<RootCertStore>,
    pub client_verifier: Arc<dyn ClientCertVerifier>,
    /// 信頼アンカーの PEM（管理 API 用）
    pub pem: Vec<u8>,
}

impl std::fmt::Debug for Roots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Roots")
            .field("anchors", &self.store.len())
            .finish()
    }
}

/// 自分の証明書（名前つき）と、その SAN。
pub struct OwnCert {
    pub key: Arc<CertifiedKey>,
    /// SAN の URI（Istio の SPIFFE ID。XFCC の `By=`）
    pub uri: String,
    /// 有効期限（UNIX 秒）
    pub not_after: i64,
    /// 証明書チェーンの PEM（管理 API の `config_dump` に出す。鍵は持たない）
    pub chain_pem: Vec<u8>,
}

#[derive(Default)]
struct Store {
    certs: HashMap<String, Arc<OwnCert>>,
    roots: HashMap<String, Arc<Roots>>,
}

static STORE: Lazy<ArcSwap<Store>> = Lazy::new(|| ArcSwap::from_pointee(Store::default()));

/// 名前つきの証明書を差し替える（PEM の証明書チェーンと秘密鍵）。
pub fn set_cert_pem(name: &str, chain_pem: &[u8], key_pem: &[u8]) -> Result<(), String> {
    use rustls::pki_types::pem::PemObject;
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(chain_pem)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("certificate chain: {e:?}"))?;
    if chain.is_empty() {
        return Err("empty certificate chain".into());
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| format!("private key: {e:?}"))?;
    set_cert_der(name, chain, key)
}

pub fn set_cert_der(
    name: &str,
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<(), String> {
    let signing = PROVIDER
        .key_provider
        .load_private_key(key)
        .map_err(|e| format!("private key: {e}"))?;
    let leaf = chain[0].clone();
    let sans = parse_sans(&leaf);
    let uri = sans
        .iter()
        .find_map(|s| match s {
            San::Uri(u) => Some(u.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let not_after = parse_not_after(&leaf).unwrap_or(0);
    let chain_pem = chain.iter().map(|c| der_to_pem(c)).collect::<String>().into_bytes();
    let own = Arc::new(OwnCert {
        key: Arc::new(CertifiedKey::new(chain, signing)),
        uri,
        not_after,
        chain_pem,
    });
    STORE.rcu(|cur| {
        let mut certs = cur.certs.clone();
        certs.insert(name.to_string(), own.clone());
        Store {
            certs,
            roots: cur.roots.clone(),
        }
    });
    Ok(())
}

/// 名前つきの信頼アンカーを差し替える（PEM）。
pub fn set_roots_pem(name: &str, pem: &[u8]) -> Result<(), String> {
    use rustls::pki_types::pem::PemObject;
    let mut store = RootCertStore::empty();
    for c in CertificateDer::pem_slice_iter(pem) {
        let c = c.map_err(|e| format!("root certificate: {e:?}"))?;
        store.add(c).map_err(|e| format!("root certificate: {e}"))?;
    }
    if store.is_empty() {
        return Err("no trust anchors".into());
    }
    let store = Arc::new(store);
    let client_verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        store.clone(),
        PROVIDER.clone(),
    )
    .allow_unauthenticated()
    .build()
    .map_err(|e| format!("client verifier: {e}"))?;
    let roots = Arc::new(Roots {
        store,
        client_verifier,
        pem: pem.to_vec(),
    });
    STORE.rcu(|cur| {
        let mut r = cur.roots.clone();
        r.insert(name.to_string(), roots.clone());
        Store {
            certs: cur.certs.clone(),
            roots: r,
        }
    });
    Ok(())
}

/// DER の証明書を PEM にする。
pub fn der_to_pem(der: &[u8]) -> String {
    let b64 = super::httpcore::base64_encode(der);
    let mut s = String::with_capacity(b64.len() + 64);
    s.push_str("-----BEGIN CERTIFICATE-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        s.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        s.push('\n');
    }
    s.push_str("-----END CERTIFICATE-----\n");
    s
}

/// 証明書・信頼アンカーの PEM（管理 API の config_dump。名前 → (証明書チェーン, 信頼アンカー)）。
pub fn pems() -> Vec<(String, Option<Vec<u8>>, Option<Vec<u8>>)> {
    let s = STORE.load();
    let mut v: Vec<_> = s
        .certs
        .iter()
        .map(|(k, c)| (k.clone(), Some(c.chain_pem.clone()), None))
        .collect();
    for (k, r) in &s.roots {
        v.push((k.clone(), None, Some(r.pem.clone())));
    }
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

pub fn cert(name: &str) -> Option<Arc<OwnCert>> {
    STORE.load().certs.get(name).cloned()
}

pub fn roots(name: &str) -> Option<Arc<Roots>> {
    STORE.load().roots.get(name).cloned()
}

/// 証明書・信頼アンカーの一覧（管理 API の `config_dump` 用。鍵は出さない）。
pub fn summary() -> Vec<(String, String, i64)> {
    let s = STORE.load();
    let mut v: Vec<_> = s
        .certs
        .iter()
        .map(|(k, c)| (k.clone(), c.uri.clone(), c.not_after))
        .collect();
    for (k, r) in &s.roots {
        v.push((k.clone(), format!("trust anchors: {}", r.store.len()), 0));
    }
    v
}

// ---------------------------------------------------------------------------
// SAN
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum San {
    Uri(String),
    Dns(String),
    Ip(std::net::IpAddr),
}

/// DER の 1 要素を読む（タグ・中身・残り）。長さは 4 バイトまで。
fn der_next(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&l0, mut rest) = rest.split_first()?;
    let len = if l0 & 0x80 == 0 {
        l0 as usize
    } else {
        let n = (l0 & 0x7f) as usize;
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let mut len = 0usize;
        for &b in &rest[..n] {
            len = (len << 8) | b as usize;
        }
        rest = &rest[n..];
        len
    };
    if rest.len() < len {
        return None;
    }
    Some((tag, &rest[..len], &rest[len..]))
}

/// 証明書の TBSCertificate の要素を順に返す。
fn tbs_fields(cert: &[u8]) -> Option<Vec<(u8, &[u8])>> {
    let (tag, cert_body, _) = der_next(cert)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, tbs, _) = der_next(cert_body)?;
    if tag != 0x30 {
        return None;
    }
    let mut out = Vec::new();
    let mut rest = tbs;
    while !rest.is_empty() {
        let (t, body, r) = der_next(rest)?;
        out.push((t, body));
        rest = r;
    }
    Some(out)
}

/// 証明書の SAN（URI・DNS・IP）を取り出す。解析できなければ空。
pub fn parse_sans(cert: &[u8]) -> Vec<San> {
    let mut out = Vec::new();
    let Some(fields) = tbs_fields(cert) else {
        return out;
    };
    // extensions は [3] EXPLICIT
    let Some((_, ext_wrap)) = fields.iter().find(|(t, _)| *t == 0xa3) else {
        return out;
    };
    let Some((0x30, exts, _)) = der_next(ext_wrap) else {
        return out;
    };
    let mut rest = exts;
    while let Some((0x30, ext, r)) = der_next(rest) {
        rest = r;
        let Some((0x06, oid, after_oid)) = der_next(ext) else {
            continue;
        };
        if oid != [0x55, 0x1d, 0x11] {
            continue;
        }
        // critical（BOOLEAN）を飛ばす
        let mut after = after_oid;
        if let Some((0x01, _, r2)) = der_next(after) {
            after = r2;
        }
        let Some((0x04, octets, _)) = der_next(after) else {
            continue;
        };
        let Some((0x30, names, _)) = der_next(octets) else {
            continue;
        };
        let mut nrest = names;
        while let Some((tag, v, r3)) = der_next(nrest) {
            nrest = r3;
            match tag {
                0x86 => {
                    if let Ok(s) = std::str::from_utf8(v) {
                        out.push(San::Uri(s.to_string()));
                    }
                }
                0x82 => {
                    if let Ok(s) = std::str::from_utf8(v) {
                        out.push(San::Dns(s.to_string()));
                    }
                }
                0x87 => match v.len() {
                    4 => out.push(San::Ip(std::net::IpAddr::from([v[0], v[1], v[2], v[3]]))),
                    16 => {
                        let mut a = [0u8; 16];
                        a.copy_from_slice(v);
                        out.push(San::Ip(std::net::IpAddr::from(a)));
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
    out
}

/// 証明書の notAfter（UNIX 秒）。
pub fn parse_not_after(cert: &[u8]) -> Option<i64> {
    let fields = tbs_fields(cert)?;
    // version [0] があればずれる。validity は SEQUENCE で 2 つの時刻を持つ最初の要素
    let start = if fields.first().map(|f| f.0) == Some(0xa0) {
        1
    } else {
        0
    };
    let (_, validity) = fields.get(start + 3)?;
    let (_, _nb, rest) = der_next(validity)?;
    let (tag, na, _) = der_next(rest)?;
    let s = std::str::from_utf8(na).ok()?;
    // UTCTime YYMMDDHHMMSSZ / GeneralizedTime YYYYMMDDHHMMSSZ
    let (year, rest) = match tag {
        0x17 => {
            let y: i64 = s.get(0..2)?.parse().ok()?;
            (if y < 50 { 2000 + y } else { 1900 + y }, s.get(2..)?)
        }
        0x18 => (s.get(0..4)?.parse().ok()?, s.get(4..)?),
        _ => return None,
    };
    let p = |a: usize| -> Option<i64> { rest.get(a..a + 2)?.parse().ok() };
    let (mo, d, h, mi, se) = (p(0)?, p(2)?, p(4)?, p(6)?, p(8)?);
    // 日付 → UNIX 日数（civil_from_days の逆）
    let y = if mo <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + h * 3600 + mi * 60 + se)
}

fn san_matches(sans: &[San], matchers: &[StringMatcher]) -> bool {
    if matchers.is_empty() {
        return true;
    }
    sans.iter().any(|s| {
        let v: &[u8] = match s {
            San::Uri(u) => u.as_bytes(),
            San::Dns(d) => d.as_bytes(),
            San::Ip(_) => return false,
        };
        matchers.iter().any(|m| m.matches(v))
    })
}

/// 証明書の SHA-256（XFCC の `Hash=`）を 16 進で。
pub fn cert_sha256_hex(der: &[u8]) -> String {
    let d = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, der);
    let mut s = String::with_capacity(64);
    for b in d.as_ref() {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        s.push(char::from_digit((b & 15) as u32, 16).unwrap_or('0'));
    }
    s
}

// ---------------------------------------------------------------------------
// rustls のリゾルバ・検証器
// ---------------------------------------------------------------------------

/// サーバ証明書を名前で引く（ハンドシェイクのたびに現在の値）。
#[derive(Debug)]
pub struct NamedServerCert(pub String);

impl rustls::server::ResolvesServerCert for NamedServerCert {
    fn resolve(&self, _hello: rustls::server::ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        cert(&self.0).map(|c| c.key.clone())
    }
}

/// クライアント証明書を名前で引く。
#[derive(Debug)]
pub struct NamedClientCert(pub String);

impl rustls::client::ResolvesClientCert for NamedClientCert {
    fn resolve(
        &self,
        _root_hint_subjects: &[&[u8]],
        _sigschemes: &[SignatureScheme],
    ) -> Option<Arc<CertifiedKey>> {
        cert(&self.0).map(|c| c.key.clone())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

/// クライアント証明書の検証（信頼アンカーを名前で引き、SAN の条件を見る）。
#[derive(Debug)]
pub struct MeshClientVerifier {
    pub roots_name: String,
    pub san: Vec<StringMatcher>,
    pub mandatory: bool,
}

impl ClientCertVerifier for MeshClientVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        self.mandatory
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let roots = roots(&self.roots_name).ok_or(rustls::Error::InvalidCertificate(
            rustls::CertificateError::UnknownIssuer,
        ))?;
        roots
            .client_verifier
            .verify_client_cert(end_entity, intermediates, now)?;
        if !san_matches(&parse_sans(end_entity), &self.san) {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::NotValidForName,
            ));
        }
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &PROVIDER.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &PROVIDER.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        PROVIDER
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// サーバ証明書の検証（ホスト名ではなく SAN の条件。Istio の `match_subject_alt_names`）。
#[derive(Debug)]
pub struct MeshServerVerifier {
    /// 信頼アンカーの名前。`None` は検証しない（`insecure_skip_verify`）
    pub roots_name: Option<String>,
    pub san: Vec<StringMatcher>,
}

impl ServerCertVerifier for MeshServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let Some(name) = &self.roots_name else {
            return Ok(ServerCertVerified::assertion());
        };
        let roots = roots(name).ok_or(rustls::Error::InvalidCertificate(
            rustls::CertificateError::UnknownIssuer,
        ))?;
        let parsed = rustls::server::ParsedCertificate::try_from(end_entity)?;
        rustls::client::verify_server_cert_signed_by_trust_anchor(
            &parsed,
            &roots.store,
            intermediates,
            now,
            PROVIDER.signature_verification_algorithms.all,
        )?;
        if !san_matches(&parse_sans(end_entity), &self.san) {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::NotValidForName,
            ));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &PROVIDER.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &PROVIDER.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        PROVIDER
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// メッシュの受け側（mTLS のサーバ）の rustls 設定。
pub fn server_config(
    cert_name: &str,
    roots_name: Option<&str>,
    san: Vec<StringMatcher>,
    require_client_cert: bool,
    alpn: &[Vec<u8>],
) -> Result<Arc<rustls::ServerConfig>, String> {
    let builder = rustls::ServerConfig::builder_with_provider(PROVIDER.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    let builder = match roots_name {
        Some(r) => builder.with_client_cert_verifier(Arc::new(MeshClientVerifier {
            roots_name: r.to_string(),
            san,
            mandatory: require_client_cert,
        })),
        None => builder.with_no_client_auth(),
    };
    let mut cfg = builder.with_cert_resolver(Arc::new(NamedServerCert(cert_name.to_string())));
    cfg.alpn_protocols = alpn.to_vec();
    #[cfg(veil_ktls)]
    {
        cfg.enable_secret_extraction = true;
    }
    Ok(Arc::new(cfg))
}

/// メッシュの送り側（mTLS のクライアント）の rustls 設定。
pub fn client_config(
    cert_name: Option<&str>,
    roots_name: Option<&str>,
    san: Vec<StringMatcher>,
    alpn: &[&[u8]],
) -> Result<Arc<rustls::ClientConfig>, String> {
    let builder = rustls::ClientConfig::builder_with_provider(PROVIDER.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(MeshServerVerifier {
            roots_name: roots_name.map(|s| s.to_string()),
            san,
        }));
    let mut cfg = match cert_name {
        Some(c) => builder.with_client_cert_resolver(Arc::new(NamedClientCert(c.to_string()))),
        None => builder.with_no_client_auth(),
    };
    cfg.alpn_protocols = alpn.iter().map(|a| a.to_vec()).collect();
    #[cfg(veil_ktls)]
    {
        cfg.enable_secret_extraction = true;
    }
    Ok(Arc::new(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gen_cert(uri: &str) -> (Vec<u8>, String, String) {
        let mut params = rcgen::CertificateParams::new(vec![]).unwrap();
        params.subject_alt_names.push(rcgen::SanType::URI(
            rcgen::string::Ia5String::try_from(uri).unwrap(),
        ));
        params.subject_alt_names.push(rcgen::SanType::DnsName(
            rcgen::string::Ia5String::try_from("a.example").unwrap(),
        ));
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert.der().to_vec(), cert.pem(), key.serialize_pem())
    }

    #[test]
    fn sans_and_expiry_are_parsed() {
        let (der, _, _) = gen_cert("spiffe://cluster.local/ns/demo/sa/client");
        let sans = parse_sans(&der);
        assert!(sans.contains(&San::Uri("spiffe://cluster.local/ns/demo/sa/client".into())));
        assert!(sans.contains(&San::Dns("a.example".into())));
        let na = parse_not_after(&der).unwrap();
        // rcgen の既定は 4096 年。2000 年以降であればよい
        assert!(na > 946684800, "{na}");
    }

    #[test]
    fn named_cert_and_roots_roundtrip() {
        let (_, pem, key) = gen_cert("spiffe://cluster.local/ns/demo/sa/x");
        set_cert_pem("test-default", pem.as_bytes(), key.as_bytes()).unwrap();
        set_roots_pem("test-root", pem.as_bytes()).unwrap();
        assert_eq!(
            cert("test-default").unwrap().uri,
            "spiffe://cluster.local/ns/demo/sa/x"
        );
        assert!(roots("test-root").is_some());
        assert_eq!(cert_sha256_hex(b"").len(), 64);
    }
}
