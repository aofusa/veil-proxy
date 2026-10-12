//! ACME（RFC 8555）による証明書の自動発行・更新（F-05、`acme` feature）。
//!
//! - 起動時（`bootstrap`）: `[acme] enabled = true` なら、証明書が無ければ自己署名の仮証明書を
//!   `[tls]` のパスに置いて TLS リスナーを起動できるようにし、HTTP-01 の応答リスナーを bind する
//!   （特権降格・サンドボックスより前）。
//! - 専用スレッド（`spawn`）: 仮証明書・期限が `renew_before_days` 日以内なら発行し、`[tls]` の
//!   パスへ原子的に書き込んで既存の証明書ホットリロード（F-03）で無停止に差し替える。
//!   以後 `check_interval_secs` ごとに確認する。データプレーンには一切触れない。

mod challenge;
mod client;
pub mod jws;
pub mod x509;

use ftlog::{error, info, warn};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::OnceLock;
use std::time::Duration;

/// `[acme]` セクション。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AcmeConfig {
    pub enabled: bool,
    pub directory_url: String,
    /// `mailto:` の連絡先。
    pub contact: Vec<String>,
    /// 証明書に入れるドメイン（1 個目が CN）。
    pub domains: Vec<String>,
    /// アカウント鍵などの保存先（書き込み可能であること）。
    pub storage_dir: String,
    /// CA の利用規約への同意（true が必須）。
    pub accept_terms_of_service: bool,
    /// HTTP-01 の応答用の平文リスナー（空なら起動しない。別の方法でポート 80 を中継する場合）。
    pub challenge_listen: String,
    pub renew_before_days: u64,
    pub check_interval_secs: u64,
    /// ACME サーバ（ディレクトリ）を検証する追加 CA（テスト用の Pebble 等）。
    pub directory_ca_cert: String,
    /// 1 回の発行の制限時間（秒）。
    pub order_timeout_secs: u64,
}

impl Default for AcmeConfig {
    fn default() -> Self {
        AcmeConfig {
            enabled: false,
            directory_url: "https://acme-v02.api.letsencrypt.org/directory".into(),
            contact: Vec::new(),
            domains: Vec::new(),
            storage_dir: "/var/lib/veil/acme".into(),
            accept_terms_of_service: false,
            challenge_listen: "0.0.0.0:80".into(),
            renew_before_days: 30,
            check_interval_secs: 43_200,
            directory_ca_cert: String::new(),
            order_timeout_secs: 300,
        }
    }
}

#[derive(Deserialize, Default)]
struct TlsPaths {
    #[serde(default)]
    cert_path: String,
    #[serde(default)]
    key_path: String,
}

#[derive(Deserialize, Default)]
struct Root {
    #[serde(default)]
    acme: AcmeConfig,
    #[serde(default)]
    tls: TlsPaths,
}

struct State {
    cfg: AcmeConfig,
    cert_path: PathBuf,
    key_path: PathBuf,
}

static STATE: OnceLock<State> = OnceLock::new();

const PLACEHOLDER: &str = "placeholder";

/// 起動時の準備。ACME が有効なら `Ok(true)`（呼び出し側は証明書ホットリロードを必ず有効にする）。
pub fn bootstrap(config_path: &Path) -> Result<bool, String> {
    let text = crate::config::read_config_text(config_path).map_err(|e| e.to_string())?;
    let root: Root = crate::config_override::apply_to_toml_str(&text)?;
    let cfg = root.acme;
    if !cfg.enabled {
        return Ok(false);
    }
    if cfg.domains.is_empty() {
        return Err("[acme] domains must not be empty".into());
    }
    if !cfg.accept_terms_of_service {
        return Err("[acme] accept_terms_of_service must be true to use ACME".into());
    }
    if root.tls.cert_path.is_empty() || root.tls.key_path.is_empty() {
        return Err("[acme] requires [tls] cert_path and key_path (the issued certificate is written there)".into());
    }
    let cert_path = PathBuf::from(&root.tls.cert_path);
    let key_path = PathBuf::from(&root.tls.key_path);
    create_dir(Path::new(&cfg.storage_dir))?;
    if !cert_path.exists() || !key_path.exists() {
        let mut params =
            rcgen::CertificateParams::new(cfg.domains.clone()).map_err(|e| e.to_string())?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, cfg.domains[0].clone());
        let key = rcgen::KeyPair::generate().map_err(|e| e.to_string())?;
        let cert = params.self_signed(&key).map_err(|e| e.to_string())?;
        write_atomic(&cert_path, cert.pem().as_bytes(), false)?;
        write_atomic(&key_path, key.serialize_pem().as_bytes(), true)?;
        write_atomic(&Path::new(&cfg.storage_dir).join(PLACEHOLDER), b"1", false)?;
        info!(
            "[ACME] wrote a temporary self-signed certificate for {:?} until the first issuance",
            cfg.domains
        );
    }
    if !cfg.challenge_listen.is_empty() {
        challenge::start(&cfg.challenge_listen)?;
    }
    let _ = STATE.set(State {
        cfg,
        cert_path,
        key_path,
    });
    Ok(true)
}

/// 発行・更新の専用スレッドを起動する（`bootstrap` が `Ok(true)` のときだけ呼ぶ）。
pub fn spawn() {
    if STATE.get().is_none() {
        return;
    }
    let r = std::thread::Builder::new()
        .name("veil-acme".into())
        .spawn(run_loop);
    if let Err(e) = r {
        error!("[ACME] failed to spawn the ACME thread: {}", e);
    }
}

// 理由付き allow: ACME 専用スレッド・起動時のファイル操作（データプレーン外）。
#[allow(clippy::disallowed_methods)]
fn create_dir(p: &Path) -> Result<(), String> {
    std::fs::create_dir_all(p).map_err(|e| format!("create {}: {}", p.display(), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// 同じディレクトリの一時ファイルに書いて rename する（読み手が半端な内容を見ないように）。
// 理由付き allow: ACME 専用スレッド・起動時のファイル操作（データプレーン外）。
#[allow(clippy::disallowed_methods)]
fn write_atomic(path: &Path, data: &[u8], secret: bool) -> Result<(), String> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    if !dir.as_os_str().is_empty() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {}", dir.display(), e))?;
    }
    let tmp = dir.join(format!(
        ".{}.tmp{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("veil"),
        std::process::id()
    ));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(if secret { 0o600 } else { 0o644 });
    }
    #[cfg(not(unix))]
    let _ = secret;
    let mut f = opts
        .open(&tmp)
        .map_err(|e| format!("write {}: {}", tmp.display(), e))?;
    f.write_all(data)
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("write {}: {}", tmp.display(), e))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename to {}: {}", path.display(), e))
}

// 理由付き allow: ACME 専用スレッドのファイル読み込み（データプレーン外）。
#[allow(clippy::disallowed_methods)]
fn read(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 発行が必要か（仮証明書・証明書が読めない・期限が近い）。
fn needs_issue(st: &State) -> bool {
    let placeholder = Path::new(&st.cfg.storage_dir).join(PLACEHOLDER);
    if placeholder.exists() {
        return true;
    }
    match read(&st.cert_path).and_then(|p| x509::pem_not_after(&p)) {
        Some(na) => na - now_secs() < (st.cfg.renew_before_days as i64) * 86_400,
        None => true,
    }
}

fn account_key(st: &State) -> Result<jws::AccountKey, String> {
    let path = Path::new(&st.cfg.storage_dir).join("account.key");
    if let Some(der) = read(&path) {
        return jws::AccountKey::from_pkcs8(&der);
    }
    let k = jws::AccountKey::generate()?;
    write_atomic(&path, k.pkcs8(), true)?;
    Ok(k)
}

fn issue_once(st: &State) -> Result<(), String> {
    let http = crate::ctrl_http::HttpClient::new(
        &crate::ctrl_http::TlsOptions {
            ca_cert: Some(st.cfg.directory_ca_cert.clone()),
            ..Default::default()
        },
        Duration::from_secs(30),
    )?;
    let key = account_key(st)?;
    let mut c = client::AcmeClient::new(&http, &key, &st.cfg.directory_url)?;
    c.account(&st.cfg.contact, st.cfg.accept_terms_of_service)?;
    let (chain, key_pem) = c.issue(
        &st.cfg.domains,
        Duration::from_secs(st.cfg.order_timeout_secs.max(30)),
    )?;
    // 鍵 → 証明書の順に書く（リローダーは両方の変化を見て読み直す）
    write_atomic(&st.key_path, key_pem.as_bytes(), true)?;
    write_atomic(&st.cert_path, chain.as_bytes(), false)?;
    let _ = remove(&Path::new(&st.cfg.storage_dir).join(PLACEHOLDER));
    // 即座に差し替える（定期確認を待たない）
    crate::config::TLS_RELOAD_FLAG.store(true, Ordering::SeqCst);
    Ok(())
}

// 理由付き allow: ACME 専用スレッドのファイル操作（データプレーン外）。
#[allow(clippy::disallowed_methods)]
fn remove(p: &Path) -> std::io::Result<()> {
    std::fs::remove_file(p)
}

fn sleep_interruptible(d: Duration) {
    let mut left = d.as_millis() as u64;
    while left > 0 && !crate::config::SHUTDOWN_FLAG.load(Ordering::Relaxed) {
        let step = left.min(500);
        crate::server::cap_safe_sleep(Duration::from_millis(step));
        left -= step;
    }
}

fn run_loop() {
    let Some(st) = STATE.get() else { return };
    info!(
        "[ACME] managing a certificate for {:?} via {}",
        st.cfg.domains, st.cfg.directory_url
    );
    let mut retry = Duration::from_secs(5);
    while !crate::config::SHUTDOWN_FLAG.load(Ordering::Relaxed) {
        if needs_issue(st) {
            match issue_once(st) {
                Ok(()) => {
                    info!("[ACME] certificate installed: {}", st.cert_path.display());
                    retry = Duration::from_secs(5);
                }
                Err(e) => {
                    warn!("[ACME] issuance failed (retry in {:?}): {}", retry, e);
                    sleep_interruptible(retry);
                    retry = (retry * 2).min(Duration::from_secs(3600));
                    continue;
                }
            }
        }
        sleep_interruptible(Duration::from_secs(st.cfg.check_interval_secs.max(60)));
    }
}
