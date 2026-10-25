//! 外向きの TLS の信頼アンカー（F-259）。
//!
//! 信頼アンカーの出どころは `"builtin"`（組み込みの webpki-roots）・`"system"`（OS の信頼ストア）・
//! PEM ファイルのパスのどれか（1 個、または配列の和集合）。`[tls_client] ca` が外向きの TLS 全体の既定、
//! 上流ごとの `tls_ca` がその上書き。
//!
//! 解決は設定のロード時だけで、接続ごとの経路は `Arc<ClientConfig>` の参照のままで変わらない。
//! OS の信頼ストアは、`"system"` が初めて解決されたとき（起動時はサンドボックスを掛ける前）に 1 回だけ読み、
//! プロセスの終わりまで持つ。サンドボックスを掛けた後に初めて `"system"` が現れたら読まずに拒否する。

use arc_swap::ArcSwapOption;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;
use rustls::{ClientConfig, RootCertStore};
use serde::{Deserialize, Deserializer};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// 信頼アンカーの出どころ 1 個。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CaSource {
    /// 組み込みの webpki-roots
    Builtin,
    /// OS の信頼ストア
    System,
    /// PEM ファイル
    Pem(PathBuf),
}

impl CaSource {
    /// 設定の文字列から。`builtin`・`system` の 2 語だけが特別で、それ以外はパス（`./system` はパス）。
    pub fn parse(s: &str) -> Self {
        match s {
            "builtin" => CaSource::Builtin,
            "system" => CaSource::System,
            p => CaSource::Pem(PathBuf::from(p)),
        }
    }
}

impl std::fmt::Display for CaSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaSource::Builtin => f.write_str("builtin"),
            CaSource::System => f.write_str("system"),
            CaSource::Pem(p) => write!(f, "{}", p.display()),
        }
    }
}

/// 出どころの組（設定の値。空 = 書かれていない）。`intern` の鍵に使うので `Hash`・`Eq`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct CaSources(Vec<CaSource>);

impl CaSources {
    pub fn new(v: Vec<CaSource>) -> Self {
        CaSources(v)
    }

    /// 文字列の並びから（空文字列は無視する）。
    pub fn from_strs<S: AsRef<str>>(v: &[S]) -> Self {
        CaSources(
            v.iter()
                .map(|s| s.as_ref())
                .filter(|s| !s.is_empty())
                .map(CaSource::parse)
                .collect(),
        )
    }

    /// 設定に書かれているか。
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_slice(&self) -> &[CaSource] {
        &self.0
    }

    /// PEM ファイルのパス（`intern` の鍵にファイルの中身を入れるため）。
    pub fn pem_paths(&self) -> impl Iterator<Item = &Path> {
        self.0.iter().filter_map(|s| match s {
            CaSource::Pem(p) => Some(p.as_path()),
            _ => None,
        })
    }

    /// `"system"` を含むか。
    pub fn uses_system(&self) -> bool {
        self.0.contains(&CaSource::System)
    }
}

impl std::fmt::Display for CaSources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, s) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{s}")?;
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for CaSources {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OneOrMany {
            One(String),
            Many(Vec<String>),
        }
        Ok(match OneOrMany::deserialize(d)? {
            OneOrMany::One(s) => CaSources::from_strs(&[s]),
            OneOrMany::Many(v) => CaSources::from_strs(&v),
        })
    }
}

/// `[tls_client]` セクション。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TlsClientConfig {
    /// 外向きの TLS 全体の既定の信頼アンカー（書かなければ `"builtin"`）
    pub ca: CaSources,
}

fn err(m: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, m)
}

// ====================
// 出どころごとの読み込み
// ====================

fn builtin_store() -> Arc<RootCertStore> {
    static BUILTIN: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    BUILTIN
        .get_or_init(|| {
            let mut r = RootCertStore::empty();
            r.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            Arc::new(r)
        })
        .clone()
}

/// OS の信頼ストアの証明書（DER）と、読めなかった件数。グローバルを触らないので試験できる。
struct SystemCerts {
    certs: Vec<CertificateDer<'static>>,
    errors: usize,
}

/// OS の既定の場所（Linux・BSD は `SSL_CERT_FILE` / `SSL_CERT_DIR` か既知のパス、macOS は
/// Security.framework、Windows は証明書ストア）から読む。
fn read_native() -> SystemCerts {
    let r = rustls_native_certs::load_native_certs();
    SystemCerts {
        certs: r.certs,
        errors: r.errors.len(),
    }
}

/// 読んだ証明書からストアを作る。1 個も使えなければエラー。
fn store_from_system(c: SystemCerts) -> io::Result<RootCertStore> {
    let mut r = RootCertStore::empty();
    let (added, ignored) = r.add_parsable_certificates(c.certs);
    let bad = c.errors + ignored;
    if bad > 0 {
        ftlog::warn!(
            "system trust store: {} certificate(s) could not be parsed and were skipped",
            bad
        );
    }
    if added == 0 {
        return Err(err(
            "the OS trust store has no usable certificates (\"system\" in tls_ca / [tls_client] ca); \
             install the OS CA bundle (for example the ca-certificates package) or set SSL_CERT_FILE"
                .into(),
        ));
    }
    Ok(r)
}

fn source_description() -> String {
    #[cfg(target_os = "macos")]
    {
        "Security.framework".to_string()
    }
    #[cfg(windows)]
    {
        "Windows certificate store".to_string()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        if let Some(f) = std::env::var_os("SSL_CERT_FILE").filter(|v| !v.is_empty()) {
            format!("SSL_CERT_FILE={}", f.to_string_lossy())
        } else if let Some(d) = std::env::var_os("SSL_CERT_DIR").filter(|v| !v.is_empty()) {
            format!("SSL_CERT_DIR={}", d.to_string_lossy())
        } else {
            "the OS default location".to_string()
        }
    }
}

static SYSTEM: OnceLock<Arc<RootCertStore>> = OnceLock::new();
static SANDBOXED: AtomicBool = AtomicBool::new(false);

/// サンドボックス（seccomp・Landlock・capsicum・pledge・Seatbelt・chroot）を掛ける設定かを記録する。
/// 起動時の最初の設定のロード後、サンドボックスを掛ける前に呼ぶ。
pub fn set_sandboxed(on: bool) {
    SANDBOXED.store(on, Ordering::Relaxed);
}

/// OS の信頼ストア（プロセスで 1 回だけ読む）。
fn system_store() -> io::Result<Arc<RootCertStore>> {
    if let Some(s) = SYSTEM.get() {
        return Ok(s.clone());
    }
    if SANDBOXED.load(Ordering::Relaxed) {
        return Err(err(
            "\"system\" (OS trust store) was not used at startup and cannot be read after the \
             sandbox is applied; restart veil to use it"
                .into(),
        ));
    }
    let store = Arc::new(store_from_system(read_native())?);
    // 同時に 2 か所から読んでも害はない（先に入ったほうを使う）
    let s = SYSTEM.get_or_init(|| store.clone()).clone();
    ftlog::info!(
        "system trust store: {} certificates from {}",
        s.roots.len(),
        source_description()
    );
    Ok(s)
}

fn pem_store(path: &Path, pem: &[u8]) -> io::Result<RootCertStore> {
    let mut r = RootCertStore::empty();
    for c in CertificateDer::pem_slice_iter(pem) {
        let c = c.map_err(|e| err(format!("tls_ca {}: {:?}", path.display(), e)))?;
        r.add(c)
            .map_err(|e| err(format!("tls_ca {}: {}", path.display(), e)))?;
    }
    if r.is_empty() {
        return Err(err(format!("tls_ca {}: no certificates", path.display())));
    }
    Ok(r)
}

/// 出どころの組から `RootCertStore` を作る（設定のロード時）。空は `"builtin"`。
/// 1 個だけの `builtin` / `system` はプロセス共有のストアをそのまま返す。
pub fn roots(sources: &[CaSource]) -> io::Result<Arc<RootCertStore>> {
    match sources {
        [] | [CaSource::Builtin] => return Ok(builtin_store()),
        [CaSource::System] => return system_store(),
        _ => {}
    }
    let mut out = RootCertStore::empty();
    for s in sources {
        match s {
            CaSource::Builtin => out.extend(builtin_store().roots.iter().cloned()),
            CaSource::System => out.extend(system_store()?.roots.iter().cloned()),
            CaSource::Pem(p) => {
                let pem = crate::tls_reload::read_pem(p)
                    .map_err(|e| err(format!("tls_ca {}: {}", p.display(), e)))?;
                out.extend(pem_store(p, &pem)?.roots);
            }
        }
    }
    Ok(Arc::new(out))
}

// ====================
// 既定（[tls_client] ca）
// ====================

struct DefaultState {
    roots: Arc<RootCertStore>,
    /// 出どころ + PEM の中身（変わったかの判定用）
    key: Vec<u8>,
    /// プールの鍵のタグ（世代つき）
    tag: &'static str,
    /// ヘルスチェックの同期プローブ用（最初に使うときに作る）
    probe: OnceLock<Arc<ClientConfig>>,
}

static DEFAULT: ArcSwapOption<DefaultState> = ArcSwapOption::const_empty();
/// 既定が変わるたびに足して、プールの鍵のタグ（`"verify"`）と接続のコネクタの作り直しの判定に使う。
static GENERATION: AtomicU32 = AtomicU32::new(0);
/// 反映（`commit_default`）を直列にする。
static COMMIT_LOCK: Mutex<()> = Mutex::new(());

/// 解決済みで、まだ反映していない既定。
pub struct PendingDefault(Option<DefaultState>);

fn default_key(sources: &CaSources) -> io::Result<Vec<u8>> {
    let mut key = format!("{:?}", sources.as_slice()).into_bytes();
    for p in sources.pem_paths() {
        let b = crate::tls_reload::read_pem(p)
            .map_err(|e| err(format!("tls_ca {}: {}", p.display(), e)))?;
        key.extend_from_slice(&(b.len() as u64).to_le_bytes());
        key.extend_from_slice(&b);
    }
    Ok(key)
}

/// `[tls_client] ca` を解決する。現在の既定と同じなら何も変えない（`PendingDefault` は空）。
pub fn resolve_default(sources: &CaSources) -> io::Result<PendingDefault> {
    let key = default_key(sources)?;
    let current = DEFAULT.load();
    if let Some(c) = current.as_ref() {
        if c.key == key {
            return Ok(PendingDefault(None));
        }
    }
    let roots = roots(sources.as_slice())?;
    let gen = GENERATION.load(Ordering::Relaxed) + u32::from(current.is_some());
    let tag: &'static str = if gen == 0 {
        "verify"
    } else {
        // 8 文字まで。上流の mTLS の `mtls<n>` とは別の接頭辞
        Box::leak(format!("vf{}", gen % 10_000).into_boxed_str())
    };
    Ok(PendingDefault(Some(DefaultState {
        roots,
        key,
        tag,
        probe: OnceLock::new(),
    })))
}

/// 解決した既定を反映する。
pub fn commit_default(p: PendingDefault) {
    let Some(state) = p.0 else { return };
    let _g = COMMIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let first = DEFAULT.load().is_none();
    DEFAULT.store(Some(Arc::new(state)));
    if !first {
        GENERATION.fetch_add(1, Ordering::Release);
    }
}

/// 解決して反映する（起動時）。
pub fn set_default(sources: &CaSources) -> io::Result<()> {
    commit_default(resolve_default(sources)?);
    Ok(())
}

fn default_state() -> Arc<DefaultState> {
    if let Some(s) = DEFAULT.load_full() {
        return s;
    }
    // 設定のロード前（テスト・ライブラリ用途）は組み込み
    let _ = set_default(&CaSources::default());
    DEFAULT
        .load_full()
        .expect("default trust store is set by set_default")
}

/// `[tls_client] ca` を解決した既定の信頼アンカー。
pub fn default_roots() -> Arc<RootCertStore> {
    default_state().roots.clone()
}

/// 既定が変わった回数（0 = 起動時のまま）。
#[inline]
pub fn generation() -> u32 {
    GENERATION.load(Ordering::Acquire)
}

/// 既定の接続（`TlsProfile::Verify`）のプールの鍵のタグ。
#[inline]
pub fn verify_tag() -> &'static str {
    match DEFAULT.load().as_ref() {
        Some(s) => s.tag,
        None => "verify",
    }
}

/// ヘルスチェックの同期プローブ用の `ClientConfig`（既定の信頼アンカー。作り直しはリロードで既定が変わったときだけ）。
pub fn probe_client_config() -> Arc<ClientConfig> {
    let s = default_state();
    s.probe
        .get_or_init(|| {
            Arc::new(
                ClientConfig::builder()
                    .with_root_certificates(s.roots.clone())
                    .with_no_client_auth(),
            )
        })
        .clone()
}

#[cfg(test)]
// 理由付き allow: 一時ディレクトリへ試験用の PEM を書く（データプレーン外のテスト）。
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn gen_ca(cn: &str) -> (String, CertificateDer<'static>) {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut p = rcgen::CertificateParams::new(vec![]).unwrap();
        p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        p.distinguished_name
            .push(rcgen::DnType::CommonName, cn.to_string());
        let c = rcgen::CertifiedIssuer::self_signed(p, key).unwrap();
        (c.pem(), c.der().clone())
    }

    #[derive(Deserialize)]
    struct W {
        #[serde(default)]
        tls_ca: CaSources,
    }

    fn parse(toml_src: &str) -> CaSources {
        toml::from_str::<W>(toml_src).unwrap().tls_ca
    }

    #[test]
    fn parses_string_and_array() {
        assert!(parse("").is_empty());
        assert_eq!(parse("tls_ca = \"system\"").as_slice(), &[CaSource::System]);
        assert_eq!(
            parse("tls_ca = \"builtin\"").as_slice(),
            &[CaSource::Builtin]
        );
        assert_eq!(
            parse("tls_ca = [\"system\", \"/etc/x.pem\"]").as_slice(),
            &[CaSource::System, CaSource::Pem("/etc/x.pem".into())]
        );
        // 空文字列は「書かれていない」
        assert!(parse("tls_ca = \"\"").is_empty());
    }

    #[test]
    fn dot_slash_system_is_a_path() {
        assert_eq!(
            parse("tls_ca = \"./system\"").as_slice(),
            &[CaSource::Pem("./system".into())]
        );
        assert!(!parse("tls_ca = \"./system\"").uses_system());
        assert!(parse("tls_ca = [\"builtin\", \"system\"]").uses_system());
    }

    #[test]
    fn tls_client_section_defaults_to_unset() {
        let c: TlsClientConfig = toml::from_str("").unwrap();
        assert!(c.ca.is_empty());
        let c: TlsClientConfig = toml::from_str("ca = \"system\"").unwrap();
        assert!(c.ca.uses_system());
    }

    #[test]
    fn builtin_is_shared_and_empty_means_builtin() {
        let a = roots(&[]).unwrap();
        let b = roots(&[CaSource::Builtin]).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!a.is_empty());
    }

    #[test]
    fn union_of_builtin_and_pem() {
        let (pem, _) = gen_ca("union-ca");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.pem");
        std::fs::write(&path, pem).unwrap();
        let builtin = roots(&[CaSource::Builtin]).unwrap();
        let both = roots(&[CaSource::Builtin, CaSource::Pem(path.clone())]).unwrap();
        assert_eq!(both.roots.len(), builtin.roots.len() + 1);
        let only = roots(&[CaSource::Pem(path)]).unwrap();
        assert_eq!(only.roots.len(), 1);
    }

    #[test]
    fn pem_without_certificates_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.pem");
        std::fs::write(&path, "not a certificate").unwrap();
        assert!(roots(&[CaSource::Pem(path)]).is_err());
        assert!(roots(&[CaSource::Pem("/nonexistent/veil-ca.pem".into())]).is_err());
    }

    #[test]
    fn zero_system_certificates_is_an_error() {
        let e = store_from_system(SystemCerts {
            certs: vec![],
            errors: 0,
        })
        .unwrap_err();
        assert!(e.to_string().contains("no usable certificates"), "{e}");
        // 解析できない証明書だけでも 0 個
        let e = store_from_system(SystemCerts {
            certs: vec![CertificateDer::from(vec![1u8, 2, 3])],
            errors: 1,
        });
        assert!(e.is_err());
    }

    #[test]
    fn system_store_skips_unparsable_certificates() {
        let (_, der) = gen_ca("ok-ca");
        let s = store_from_system(SystemCerts {
            certs: vec![der, CertificateDer::from(vec![9u8, 9, 9])],
            errors: 0,
        })
        .unwrap();
        assert_eq!(s.roots.len(), 1);
    }

    /// 子プロセスで `SSL_CERT_FILE` を設定して OS の信頼ストアの読み込みを確かめる
    /// （環境変数をテストのプロセスで書き換えない）。
    #[test]
    #[cfg(not(any(target_os = "macos", windows)))]
    fn ssl_cert_file_is_loaded_as_system_store() {
        let (pem, _) = gen_ca("ssl-cert-file-ca");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bundle.pem");
        std::fs::write(&path, pem).unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "trust_store::tests::child_reads_ssl_cert_file",
                "--nocapture",
            ])
            .env("SSL_CERT_FILE", &path)
            .env("SSL_CERT_DIR", dir.path().join("none"))
            .env("VEIL_TRUST_STORE_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("child checked"));
    }

    #[test]
    #[cfg(not(any(target_os = "macos", windows)))]
    fn child_reads_ssl_cert_file() {
        if std::env::var_os("VEIL_TRUST_STORE_CHILD").is_none() {
            return;
        }
        let s = store_from_system(read_native()).unwrap();
        assert_eq!(s.roots.len(), 1, "only the SSL_CERT_FILE bundle is read");
        println!("child checked");
    }
}
