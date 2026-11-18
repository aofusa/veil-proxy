//! Windows: 既定の設定ファイル（`%ProgramData%\veil\config.toml`）を読む前に、所有者と DACL を確かめる。
//!
//! B-463: `%ProgramData%` は特権の無い利用者もフォルダを作れて、作った利用者がその所有者になる。MSI を入れる前に
//! `%ProgramData%\veil\config.toml` を置かれると、MSI はそれを残し（NeverOverwrite）、管理者・サービス（LocalSystem）で
//! 動く veil がその設定（任意のディレクトリを配信する File のルート・任意のファイルへ書くログ等）で動いた。
//! OpenSSH の sshd_config と同じく、所有者が Administrators・SYSTEM・TrustedInstaller・veil を動かす利用者のどれか
//! で、それ以外に書き換えを許していないファイルだけを読む。検査と読み取りは同じハンドルで行う（間で差し替えさせない）。
//! `-c` で同じファイルを別の書き方（大文字小文字・短い名前・ハードリンク）で渡しても、ファイルの同一性で既定のファイルと
//! 見なして検査する。

use std::ffi::c_void;
use std::io::{self, Read};
use std::path::Path;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, ERROR_SUCCESS, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSidToSidW, GetSecurityInfo, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    EqualSid, GetAce, GetTokenInformation, TokenUser, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
    DACL_SECURITY_INFORMATION, INHERIT_ONLY_ACE, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    PSID, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

// ACE の種類（winnt.h。windows-sys では SystemServices の feature にある）
/// `ACCESS_ALLOWED_ACE_TYPE`
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
/// `ACCESS_ALLOWED_OBJECT_ACE_TYPE`（ディレクトリサービスのオブジェクト用。ファイルには無いはずなので見つけたら断る）
const ACCESS_ALLOWED_OBJECT_ACE_TYPE: u8 = 5;
/// `ACCESS_ALLOWED_CALLBACK_ACE_TYPE`（条件付きの許可。形は ACCESS_ALLOWED_ACE と同じで、SID の後ろに条件が続く）
const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 9;
/// `ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE`
const ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE: u8 = 11;

/// `FILE_READ_ATTRIBUTES`（同一性を見るためだけに既定のパスを開く）
const FILE_READ_ATTRIBUTES: u32 = 0x80;
/// FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
const SHARE_ALL: u32 = 0x7;
/// FILE_SHARE_READ
const SHARE_READ: u32 = 0x1;

/// 中身・権限を変えられる権利: FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_WRITE_EA | WRITE_DAC | WRITE_OWNER |
/// GENERIC_ALL | GENERIC_WRITE
const WRITE_RIGHTS: u32 = 0x2 | 0x4 | 0x10 | 0x4_0000 | 0x8_0000 | 0x1000_0000 | 0x4000_0000;

/// 所有・書き込みを許す既知の SID: SYSTEM・Administrators・TrustedInstaller
const TRUSTED_SIDS: [&str; 3] = [
    "S-1-5-18",
    "S-1-5-32-544",
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
];

/// OWNER RIGHTS（所有者の暗黙の権利を置き換える ACE の SID）。所有者を確かめた後なので書き込みを許してよい
const OWNER_RIGHTS_SID: &str = "S-1-3-4";

/// LocalAlloc された領域（SID・セキュリティ記述子・文字列）を LocalFree で返す
struct LocalBox(*mut c_void);

impl Drop for LocalBox {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: self.0 は Windows の API が LocalAlloc して渡した領域で、ここで 1 回だけ返す
            unsafe { LocalFree(self.0) };
        }
    }
}

/// 設定ファイルを読む。既定の設定ファイル（`default` と同じファイル）なら、所有者と DACL を確かめてから読む
/// （同じハンドルで検査して読む）。
// 理由付き allow: 起動・リロード時の設定ファイルの読み込み（データプレーン外）
#[allow(clippy::disallowed_methods)]
pub(crate) fn read_config(path: &Path, default: &Path) -> io::Result<String> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    let spelled_default = path == default;
    // 既定のパスなら FILE_SHARE_READ だけ（検査して読み終えるまで書き換え・削除・改名させない）
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(if spelled_default {
            SHARE_READ
        } else {
            SHARE_ALL
        })
        .open(path)?;
    let handle = file.as_raw_handle() as HANDLE;
    if spelled_default || same_file_as(handle, default) {
        if let Err(why) = check_trusted(handle) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "refusing to read {}: {}. The default configuration file must be owned by Administrators, \
                     SYSTEM or the account running veil, and no other account may be allowed to change it; \
                     fix its owner and permissions, or keep the configuration somewhere else and pass it with -c",
                    path.display(),
                    why
                ),
            ));
        }
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(text)
}

/// `handle` のファイルが `other` のパスのファイルと同じか（ボリュームとファイルの番号で比べる）。`other` が無ければ false
// 理由付き allow: 起動・リロード時の設定ファイルの読み込み（データプレーン外）
#[allow(clippy::disallowed_methods)]
fn same_file_as(handle: HANDLE, other: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    let Ok(o) = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(SHARE_ALL)
        .open(other)
    else {
        return false;
    };
    match (file_id(handle), file_id(o.as_raw_handle() as HANDLE)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// (ボリュームのシリアル番号, ファイルの番号)
fn file_id(handle: HANDLE) -> Option<(u32, u32, u32)> {
    // SAFETY: BY_HANDLE_FILE_INFORMATION は整数だけの POD で、全ビット 0 が有効な初期値
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: handle は開いているファイルのハンドル。info は書き込める出力の領域
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
        return None;
    }
    Some((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}

/// ハンドルのファイルの所有者と、書き換えを許された相手を確かめる。
fn check_trusted(handle: HANDLE) -> Result<(), String> {
    let mut owner: PSID = null_mut();
    let mut dacl: *mut ACL = null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: handle は読み取りで開いた（READ_CONTROL を含む）有効なファイルのハンドル。owner・dacl は sd の中を指し、
    // sd（LocalAlloc）を返すまでだけ使う
    let e = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        )
    };
    if e != ERROR_SUCCESS {
        return Err(format!(
            "cannot read its security descriptor ({})",
            io::Error::from_raw_os_error(e as i32)
        ));
    }
    let _sd = LocalBox(sd);
    let mut trusted = Vec::with_capacity(TRUSTED_SIDS.len());
    for s in TRUSTED_SIDS {
        trusted.push(sid_from_string(s).map_err(|e| format!("cannot build the SID {s} ({e})"))?);
    }
    let me = CurrentUser::query().map_err(|e| format!("cannot read the current account ({e})"))?;
    let owner_rights = sid_from_string(OWNER_RIGHTS_SID)
        .map_err(|e| format!("cannot build the SID {OWNER_RIGHTS_SID} ({e})"))?;
    // SAFETY: 比べる SID はどれも生きている領域（sd・trusted・me）を指す
    let is_trusted = |sid: PSID| unsafe {
        EqualSid(sid, me.sid()) != 0 || trusted.iter().any(|t| EqualSid(sid, t.0) != 0)
    };
    if owner.is_null() || !is_trusted(owner) {
        return Err(format!("it is owned by {}", sid_string(owner)));
    }
    // 所有者は確かめたので、OWNER RIGHTS への許可は所有者への許可と同じ
    // SAFETY: owner_rights は生きている SID
    let may_write = |sid: PSID| is_trusted(sid) || unsafe { EqualSid(sid, owner_rights.0) != 0 };
    if dacl.is_null() {
        return Err("it has no DACL, so every account may change it".to_string());
    }
    // SAFETY: dacl は sd の中の有効な ACL
    let count = unsafe { (*dacl).AceCount };
    for i in 0..u32::from(count) {
        let mut ace: *mut c_void = null_mut();
        // SAFETY: i は ACL の ACE の数より小さい。ace は ACL の中を指す
        if unsafe { GetAce(dacl, i, &mut ace) } == 0 {
            return Err("cannot read its DACL".to_string());
        }
        // SAFETY: GetAce が返した ACE は ACE_HEADER で始まる
        let header = unsafe { &*(ace as *const ACE_HEADER) };
        // 子にだけ継承する ACE はこのファイルに効かない
        if u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 {
            continue;
        }
        match header.AceType {
            ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE => {}
            // 形の違う許可（オブジェクトの ACE）はファイルには付かない。読み解かずに断る
            ACCESS_ALLOWED_OBJECT_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE => {
                return Err("its DACL has an object access-allowed entry".to_string());
            }
            // 拒否・監査の ACE は書き込みを許さない
            _ => continue,
        }
        // SAFETY: 種類が ACCESS_ALLOWED(_CALLBACK)_ACE_TYPE の ACE は ACCESS_ALLOWED_ACE の形で始まり、SID は SidStart
        // から続く（条件付きの ACE は SID の後ろに条件が続く）
        let allowed = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
        let sid = std::ptr::addr_of!(allowed.SidStart) as PSID;
        if allowed.Mask & WRITE_RIGHTS != 0 && !may_write(sid) {
            return Err(format!("{} is allowed to change it", sid_string(sid)));
        }
    }
    Ok(())
}

fn sid_from_string(s: &str) -> io::Result<LocalBox> {
    let wide: Vec<u16> = s.encode_utf16().chain(Some(0)).collect();
    let mut sid: PSID = null_mut();
    // SAFETY: wide は NUL 終端の UTF-16。成功時の sid は LocalFree で返す（LocalBox）
    if unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut sid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(LocalBox(sid))
}

/// SID の文字列（S-1-5-…）。エラーの文に使う
fn sid_string(sid: PSID) -> String {
    if sid.is_null() {
        return "no account".to_string();
    }
    let mut p: *mut u16 = null_mut();
    // SAFETY: sid は有効な SID。成功時の p は NUL 終端の UTF-16 で、LocalFree で返す
    if unsafe { ConvertSidToStringSidW(sid, &mut p) } == 0 || p.is_null() {
        return "an unknown account".to_string();
    }
    let _free = LocalBox(p as *mut c_void);
    let mut n = 0;
    // SAFETY: p は NUL 終端の文字列
    while unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    // SAFETY: p から n 文字は上で数えた範囲
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) })
}

/// veil を動かしている利用者（TOKEN_USER の領域を持ち、SID はその中を指す）
struct CurrentUser {
    buf: Vec<u64>,
}

impl CurrentUser {
    fn query() -> io::Result<Self> {
        let mut token: HANDLE = null_mut();
        // SAFETY: 自分のプロセスの擬似ハンドル。成功時の token は CloseHandle で閉じる
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut len = 0u32;
        // SAFETY: 大きさを問い合わせるだけ（出力の領域なし）
        unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len) };
        // TOKEN_USER はポインタを含むので 8 バイトに揃えた領域に受ける
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
        // SAFETY: buf は len バイト以上の書き込める領域
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 8) as u32,
                &mut len,
            )
        };
        let err = io::Error::last_os_error();
        // SAFETY: OpenProcessToken が開いたハンドルを 1 回だけ閉じる
        unsafe { CloseHandle(token) };
        if ok == 0 {
            return Err(err);
        }
        Ok(Self { buf })
    }

    fn sid(&self) -> PSID {
        // SAFETY: buf は GetTokenInformation(TokenUser) が満たした TOKEN_USER
        unsafe { (*(self.buf.as_ptr() as *const TOKEN_USER)).User.Sid }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 自分で作ったファイル（所有者は自分か Administrators、継承した DACL）は読める
    // 理由付き allow: 試験でファイルを作る（データプレーン外）
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn own_file_is_trusted() {
        let dir = std::env::temp_dir().join(format!("veil-b463-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(&p, "[server]\n").unwrap();
        assert_eq!(read_config(&p, &p).unwrap(), "[server]\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Users（S-1-5-32-545）に書き込みを許したファイルは読まない
    // 理由付き allow: 試験でファイルを作る・icacls で権限を足す（データプレーン外）
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn file_writable_by_users_is_refused() {
        let dir = std::env::temp_dir().join(format!("veil-b463w-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(&p, "[server]\n").unwrap();
        let st = std::process::Command::new("icacls")
            .arg(&p)
            .args(["/grant", "*S-1-5-32-545:(M)"])
            .output()
            .unwrap();
        assert!(st.status.success(), "{st:?}");
        let e = read_config(&p, &p).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
        assert!(e.to_string().contains("S-1-5-32-545"), "{e}");
        // 別の書き方（大文字）でも同じファイルなら検査する
        let upper = std::path::PathBuf::from(p.to_string_lossy().to_uppercase());
        assert_eq!(
            read_config(&upper, &p).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        // 既定のファイルでなければ検査しない
        let other = dir.join("other.toml");
        assert_eq!(read_config(&p, &other).unwrap(), "[server]\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
