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
    // B-398: 権限は O_NOFOLLOW で開いたディレクトリへ付ける（パスで chmod するとシンボリックリンクを辿り、置き換えた
    // 先のファイルの権限を root が変えた）。シンボリックリンクのディレクトリ（運用で置いたもの）はそのまま使う
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        if let Ok(d) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(p)
        {
            let _ = d.set_permissions(std::fs::Permissions::from_mode(0o700));
        }
    }
    // B-325: Windows は親の DACL を継承して Users が読めることがあるので、SYSTEM・Administrators・所有者だけにする
    #[cfg(windows)]
    restrict_to_owner(p)?;
    Ok(())
}

/// 継承を切った DACL（SYSTEM・Administrators・所有者のフルアクセスだけ）を付ける（B-325）。
/// unix の 0600 / 0700 に当たる。
#[cfg(windows)]
fn restrict_to_owner(p: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW,
        SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorDacl, ACL, DACL_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };
    let err = |what: &str| {
        format!(
            "restrict permissions of {} ({}): {}",
            p.display(),
            what,
            std::io::Error::last_os_error()
        )
    };
    let sddl: Vec<u16> = "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;OW)"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let path: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: sddl は NUL 終端の UTF-16。成功時の sd は LocalFree で解放する
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(err("security descriptor"));
    }
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: sd は上で得た有効な記述子。dacl は sd の中を指し、sd を解放するまでだけ使う
    let r = unsafe {
        if GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) == 0 {
            Err(err("dacl"))
        } else {
            let e = SetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null(),
            );
            if e == ERROR_SUCCESS {
                Ok(())
            } else {
                Err(format!(
                    "restrict permissions of {}: {}",
                    p.display(),
                    std::io::Error::from_raw_os_error(e as i32)
                ))
            }
        }
    };
    // SAFETY: Convert… が LocalAlloc した記述子
    unsafe { LocalFree(sd as _) };
    r
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
    let (tmp, mut f) = create_temp(dir, path, secret)?;
    // B-325: 秘密は書く前に所有者だけの DACL にする（unix は create_temp の 0600）
    #[cfg(windows)]
    if secret {
        restrict_to_owner(&tmp)?;
    }
    f.write_all(data)
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("write {}: {}", tmp.display(), e))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename to {}: {}", path.display(), e))
}

/// `path` の隣に一時ファイルを新しく作る。
///
/// B-398: 毎回新しい名前で O_EXCL（既にあれば作らない。シンボリックリンクも辿らない。unix は O_NOFOLLOW も）で
/// 作る。以前は `.<名前>.tmp<pid>` を O_TRUNC で開いていたので、ディレクトリに書き込める特権の無い利用者が置いた
/// シンボリックリンクを、権限を落とす前の root が辿って任意のファイルを書き換えた。名前が埋まっていれば別の名前で
/// やり直す。
// 理由付き allow: ACME 専用スレッド・起動時のファイル操作（データプレーン外）。
#[allow(clippy::disallowed_methods)]
fn create_temp(dir: &Path, path: &Path, secret: bool) -> Result<(PathBuf, std::fs::File), String> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("veil");
    let mut last = String::new();
    for _ in 0..16 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let tmp = dir.join(format!(
            ".{}.tmp{}.{}.{:08x}",
            name,
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            nanos
        ));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(if secret { 0o600 } else { 0o644 })
                .custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(not(unix))]
        let _ = secret;
        match opts.open(&tmp) {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last = format!("write {}: {}", tmp.display(), e);
            }
            Err(e) => return Err(format!("write {}: {}", tmp.display(), e)),
        }
    }
    Err(last)
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

#[cfg(test)]
mod tests {
    use super::*;

    // B-325: 秘密は所有者だけ（unix は 0600、Windows は継承を切った DACL）で書く
    // 理由付き allow: 試験でファイルのモードを読む（データプレーン外）
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn secret_files_are_owner_only() {
        let dir = std::env::temp_dir().join(format!("veil-acme-b325-{}", std::process::id()));
        create_dir(&dir).unwrap();
        let key = dir.join("key.pem");
        write_atomic(&key, b"secret", true).unwrap();
        assert_eq!(read(&key).as_deref(), Some(&b"secret"[..]));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&key).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let dmode = std::fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(dmode & 0o777, 0o700);
        }
        // 書き直せる（所有者が DACL で締め出されない）
        write_atomic(&key, b"secret2", true).unwrap();
        assert_eq!(read(&key).as_deref(), Some(&b"secret2"[..]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // B-398: 一時ファイルの名前に置いたシンボリックリンクを辿らない（毎回新しい名前を O_EXCL で作る）
    // 理由付き allow: 試験でファイルを作る（データプレーン外）
    #[cfg(unix)]
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn temp_files_do_not_follow_planted_symlinks() {
        let dir = std::env::temp_dir().join(format!("veil-acme-b398-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        create_dir(&dir).unwrap();
        let victim = dir.join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        // 以前の名前（.<名前>.tmp<pid>）に置いた罠
        let planted = dir.join(format!(".cert.pem.tmp{}", std::process::id()));
        std::os::unix::fs::symlink(&victim, &planted).unwrap();
        let cert = dir.join("cert.pem");
        write_atomic(&cert, b"new cert", false).unwrap();
        assert_eq!(read(&cert).as_deref(), Some(&b"new cert"[..]));
        assert_eq!(read(&victim).as_deref(), Some(&b"keep"[..]));
        // 一時ファイルは残らない（罠のリンクだけが残る）
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert_eq!(
            left,
            vec![planted.file_name().unwrap().to_string_lossy().into_owned()]
        );
        // ディレクトリへのシンボリックリンクの権限は変えない（辿らない）
        let other = dir.join("other");
        std::fs::create_dir(&other).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o755)).unwrap();
            let link = dir.join("link");
            std::os::unix::fs::symlink(&other, &link).unwrap();
            create_dir(&link).unwrap();
            let mode = std::fs::metadata(&other).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
