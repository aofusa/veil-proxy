//! 上流の mTLS（F-217、メッシュ無しでも使える一般機能）。
//!
//! `[upstreams.<name>]`・Proxy の action の `tls_client_cert` / `tls_client_key`（自分のクライアント証明書）、
//! `tls_ca`（上流の証明書の信頼アンカー。省略時は `[tls_client] ca`、その既定は組み込みの公開 CA。F-259）、`expected_san`（上流の証明書の SAN の期待値。
//! 指定するとホスト名の代わりにこれで照合する。SPIFFE の URI SAN 等）。
//!
//! 設定のロード時に rustls の設定を作って `UpstreamTlsClient` として `ProxyTarget` に持たせ、接続のたびに
//! 作らない（ホットパスは `Arc` の参照だけ）。接続のプールの鍵には設定ごとのタグを足し、別の証明書の接続を
//! 共有しない。

use crate::trust_store::CaSources;
use crate::x509_san::{parse_sans, San};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::ClientConfig;
use rustls::{DigitallySignedStruct, RootCertStore, SignatureScheme};
use serde::Deserialize;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// TOML の上流の TLS の設定（未設定なら従来どおり既定のコネクタ）。
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq, Hash)]
pub struct UpstreamTlsSettings {
    /// 上流へ出すクライアント証明書（PEM。`tls_client_key` と組）
    #[serde(default)]
    pub tls_client_cert: String,
    #[serde(default)]
    pub tls_client_key: String,
    /// 上流の証明書の信頼アンカー（`"builtin"`・`"system"`・PEM のパス、またはその配列。F-259）。
    /// 書かなければ `[tls_client] ca`
    #[serde(default)]
    pub tls_ca: CaSources,
    /// 上流の証明書の SAN の期待値（DNS 名・URI・IP のどれかが一致すれば受ける）
    #[serde(default)]
    pub expected_san: Vec<String>,
}

impl UpstreamTlsSettings {
    pub fn is_set(&self) -> bool {
        !self.tls_client_cert.is_empty() || !self.tls_ca.is_empty() || !self.expected_san.is_empty()
    }
}

/// 読み込み済みの上流の TLS の設定。
#[derive(Debug)]
pub struct UpstreamTlsClient {
    /// HTTP/1.1 用（ALPN `http/1.1`）
    pub h1: Arc<ClientConfig>,
    /// HTTP/2 も選べる（ALPN `h2, http/1.1`。F-175 の `http2 = "auto" | "on"`）
    #[cfg(feature = "http2")]
    pub h2: Arc<ClientConfig>,
    /// 接続のプールの鍵のタグ（設定ごとに一意。8 文字まで）
    pub pool_tag: &'static str,
}

/// 上流の TLS の種類（接続の作り方とプールの鍵の分け方）。`Copy` で持ち回る（ホットパスで参照を数えない）。
#[derive(Clone, Copy, Debug)]
pub enum TlsProfile {
    /// 既定（公開 CA でホスト名を検証）
    Verify,
    /// 検証しない（`tls_insecure = true`）
    Insecure,
    /// 上流の mTLS の設定（[`intern`] で作ったもの。プロセスの終わりまで生きる）
    Custom(&'static UpstreamTlsClient),
}

impl PartialEq for TlsProfile {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (TlsProfile::Verify, TlsProfile::Verify) => true,
            (TlsProfile::Insecure, TlsProfile::Insecure) => true,
            (TlsProfile::Custom(a), TlsProfile::Custom(b)) => std::ptr::eq(*a, *b),
            _ => false,
        }
    }
}

impl Eq for TlsProfile {}

impl TlsProfile {
    pub fn from_insecure(insecure: bool) -> Self {
        if insecure {
            TlsProfile::Insecure
        } else {
            TlsProfile::Verify
        }
    }

    /// 証明書を検証しないか
    pub fn is_insecure(self) -> bool {
        matches!(self, TlsProfile::Insecure)
    }

    /// 接続のプールの鍵のタグ（検証の設定・クライアント証明書の違う接続を共有しない。B-30）。
    pub fn tag(self) -> &'static str {
        match self {
            TlsProfile::Verify => crate::trust_store::verify_tag(),
            TlsProfile::Insecure => "insecure",
            TlsProfile::Custom(c) => c.pool_tag,
        }
    }
}

/// 同じ設定・同じファイルの中身なら同じものを使う（設定のリロードのたびに作り直してプールを分けない）。
/// ファイルの中身も鍵に入れるので、証明書を差し替えて SIGHUP すれば新しい証明書で作り直す。
/// 作ったものはプロセスの終わりまで持つ（古い設定の接続がプールに残っていてもよいように）。
pub fn intern(s: &UpstreamTlsSettings, ktls: bool) -> io::Result<&'static UpstreamTlsClient> {
    // 末尾は `[tls_client] ca` の中身の識別子（`tls_ca` を書かない設定は既定の変更で作り直す。
    // 世代でなく中身にするのは、失敗したリロードの未反映の既定と混ざらないため）
    type Key = (UpstreamTlsSettings, bool, Vec<u8>, Vec<u8>);
    static INTERNED: std::sync::Mutex<
        Option<std::collections::HashMap<Key, &'static UpstreamTlsClient>>,
    > = std::sync::Mutex::new(None);
    let mut contents = Vec::new();
    let files = s
        .tls_ca
        .pem_paths()
        .chain([&s.tls_client_cert, &s.tls_client_key].map(Path::new))
        .filter(|f| !f.as_os_str().is_empty());
    for f in files {
        let b = crate::tls_reload::read_pem(f)?;
        contents.extend_from_slice(&(b.len() as u64).to_le_bytes());
        contents.extend_from_slice(&b);
    }
    let key = (
        s.clone(),
        ktls,
        contents,
        crate::trust_store::default_key_id(),
    );
    let mut g = INTERNED.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(std::collections::HashMap::new);
    if let Some(c) = m.get(&key) {
        return Ok(c);
    }
    let c: &'static UpstreamTlsClient = Box::leak(Box::new(build(s, ktls)?));
    m.insert(key, c);
    Ok(c)
}

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

fn err(m: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, m)
}

/// 期待する SAN で照合する検証器（チェーンは信頼アンカーで検証し、ホスト名の代わりに SAN を見る）。
#[derive(Debug)]
struct ExpectedSanVerifier {
    roots: Arc<RootCertStore>,
    expected: Vec<String>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for ExpectedSanVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let parsed = rustls::server::ParsedCertificate::try_from(end_entity)?;
        rustls::client::verify_server_cert_signed_by_trust_anchor(
            &parsed,
            &self.roots,
            intermediates,
            now,
            self.provider.signature_verification_algorithms.all,
        )?;
        let sans = parse_sans(end_entity);
        let ok = self.expected.iter().any(|e| {
            sans.iter().any(|s| match s {
                San::Dns(d) => d.eq_ignore_ascii_case(e),
                San::Uri(u) => u == e,
                San::Ip(ip) => ip.to_string() == *e,
            })
        });
        if ok {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::NotValidForName,
            ))
        }
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
            &self.provider.signature_verification_algorithms,
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
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// 設定から読み込む（設定のロード時。`ktls` は kTLS へ渡すために鍵の取り出しを許すか）。
fn build(s: &UpstreamTlsSettings, ktls: bool) -> io::Result<UpstreamTlsClient> {
    let provider = Arc::new(crate::tls_provider::provider::default_provider());
    let roots = if s.tls_ca.is_empty() {
        crate::trust_store::default_roots()
    } else {
        crate::trust_store::roots(s.tls_ca.as_slice())?
    };
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| err(e.to_string()))?;
    let builder = if s.expected_san.is_empty() {
        builder.with_root_certificates(roots)
    } else {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(ExpectedSanVerifier {
                roots,
                expected: s.expected_san.clone(),
                provider,
            }))
    };
    let mut cfg = match (s.tls_client_cert.is_empty(), s.tls_client_key.is_empty()) {
        (true, true) => builder.with_no_client_auth(),
        (false, false) => {
            let cert_pem = crate::tls_reload::read_pem(Path::new(&s.tls_client_cert))?;
            let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_pem)
                .collect::<Result<_, _>>()
                .map_err(|e| err(format!("tls_client_cert {}: {:?}", s.tls_client_cert, e)))?;
            let key_pem = crate::tls_reload::read_pem(Path::new(&s.tls_client_key))?;
            let key = PrivateKeyDer::from_pem_slice(&key_pem)
                .map_err(|e| err(format!("tls_client_key {}: {:?}", s.tls_client_key, e)))?;
            builder
                .with_client_auth_cert(chain, key)
                .map_err(|e| err(format!("tls_client_cert: {}", e)))?
        }
        _ => {
            return Err(err(
                "tls_client_cert and tls_client_key must be set together".into(),
            ))
        }
    };
    cfg.enable_secret_extraction = ktls;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    #[cfg(feature = "http2")]
    let h2 = Arc::new(crate::protocol::configure_alpn_h2_client(cfg.clone()));
    #[cfg(feature = "http2")]
    let h1 = Arc::new(crate::protocol::configure_alpn_http11_client(cfg));
    #[cfg(not(feature = "http2"))]
    let h1 = Arc::new(cfg);
    Ok(UpstreamTlsClient {
        h1,
        #[cfg(feature = "http2")]
        h2,
        pool_tag: Box::leak(format!("mtls{}", id % 10_000).into_boxed_str()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 自前の CA と、それが署名した URI SAN 付きの証明書（PEM の CA と、証明書の DER）。
    fn ca_and_leaf(san_uri: &str) -> (String, CertificateDer<'static>) {
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(vec![]).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca = rcgen::CertifiedIssuer::self_signed(ca_params, ca_key).unwrap();
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let mut lp = rcgen::CertificateParams::new(vec!["backend.local".to_string()]).unwrap();
        lp.subject_alt_names.push(rcgen::SanType::URI(
            rcgen::string::Ia5String::try_from(san_uri).unwrap(),
        ));
        let leaf = lp.signed_by(&leaf_key, &ca).unwrap();
        (ca.pem(), leaf.der().clone())
    }

    fn verify(v: &dyn ServerCertVerifier, der: &CertificateDer<'_>) -> bool {
        v.verify_server_cert(
            der,
            &[],
            &ServerName::try_from("ignored.example").unwrap(),
            &[],
            UnixTime::now(),
        )
        .is_ok()
    }

    #[test]
    fn expected_san_matches_uri_and_rejects_others() {
        let (ca_pem, der) = ca_and_leaf("spiffe://cluster.local/ns/a/sa/backend");
        let mut roots = RootCertStore::empty();
        for c in CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
            roots.add(c.unwrap()).unwrap();
        }
        let roots = Arc::new(roots);
        let provider = Arc::new(crate::tls_provider::provider::default_provider());
        let ok = ExpectedSanVerifier {
            roots: roots.clone(),
            expected: vec!["spiffe://cluster.local/ns/a/sa/backend".into()],
            provider: provider.clone(),
        };
        assert!(verify(&ok, &der));
        let wrong = ExpectedSanVerifier {
            roots,
            expected: vec!["spiffe://cluster.local/ns/a/sa/other".into()],
            provider: provider.clone(),
        };
        assert!(!verify(&wrong, &der));
        // 信頼アンカー違い
        let (other_ca, _) = ca_and_leaf("spiffe://x");
        let mut r2 = RootCertStore::empty();
        for c in CertificateDer::pem_slice_iter(other_ca.as_bytes()) {
            r2.add(c.unwrap()).unwrap();
        }
        let other = ExpectedSanVerifier {
            roots: Arc::new(r2),
            expected: vec!["spiffe://cluster.local/ns/a/sa/backend".into()],
            provider,
        };
        assert!(!verify(&other, &der));
    }
}
