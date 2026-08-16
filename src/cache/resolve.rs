//! 静的配信のパス解決 — `canonicalize()` を使わないカーネル封じ込め解決（F-153）
//!
//! `std::fs::canonicalize()` はパスの全コンポーネントに `readlink(2)` を発行する
//! （深い静的ルートでは 1 リクエストあたり 7 回、しかも全件エラーになることを実測で
//! 確認済み: `docs/backlog/features/F-153-static-open-resolve-beneath.md`）。
//!
//! ここでは代わりに「解決してから検査する」ではなく「カーネルに封じ込めさせる」方式で
//! 静的ルート配下のファイルを開く。
//!
//! - **Linux 5.6+**: `openat2(2)` を `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS` 付きで
//!   直接 syscall する（`libc` に高レベルラッパが無いため `open_how` 相当の構造体を自前定義）。
//! - **FreeBSD**: F-123 で実装済みの `security::capsicum` モジュール（`O_RESOLVE_BENEATH`
//!   付き `openat`）をそのまま再利用する（capability mode 以外でも使えるようにする）。
//! - **その他 OS / 上記が使えない場合**: 従来どおり `canonicalize()` + `metadata()` +
//!   含有チェックへフォールバックする（防御水準は変えない）。
//!
//! `openat2` が `ENOSYS`（カーネル 5.6 未満）/`EPERM`（seccomp 等でブロック）を返した
//! 場合は必ずフォールバックへ落ち、一度観測したらプロセス全体で以後フォールバックする
//! （毎リクエスト失敗する syscall を撃ち続けない）。**検査なしで open する経路は作らない**
//! （フェイルオープン厳禁）。
//!
//! ホットパスからは `runtime::offload` の専用ワーカースレッド内でのみ呼び出すこと
//! （AGENTS.md ホットパス絶対規則。ここに定義する関数自体は同期関数であり、
//! イベントループから直接呼んではならない）。

use std::fs::{File, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Once;

use ftlog::{info, warn};

/// 起動時ログ（どちらの解決経路を使っているか）は 1 回だけ出す。
static STARTUP_LOG_ONCE: Once = Once::new();
/// 実行時に openat2 が使えないと判明した場合の警告も 1 回だけ出す
/// （毎リクエスト ENOSYS を撃ち続けるのを防ぐための memoization と対になっている）。
///
/// `openat2` の実行時フォールバック判定は Linux 経路にしか存在しない
/// （FreeBSD は `O_RESOLVE_BENEATH` が使えるかを起動時に確定させる）ため、
/// Linux 以外ではこの静的変数もロガーも参照されない。`#[allow(dead_code)]` は
/// AGENTS.md で禁止されているので cfg で存在自体を絞る。
#[cfg(target_os = "linux")]
static RUNTIME_FALLBACK_LOG_ONCE: Once = Once::new();

fn log_registration_result(beneath_active: bool) {
    STARTUP_LOG_ONCE.call_once(|| {
        if beneath_active {
            info!(
                "F-153: 静的配信のパス解決にカーネル封じ込め（openat2 RESOLVE_BENEATH / \
                 capsicum O_RESOLVE_BENEATH）を使用します（canonicalize の readlink 連発を回避）"
            );
        } else {
            info!(
                "F-153: 静的配信のパス解決はフォールバック canonicalize() を使用します \
                 （静的ルート未登録、または本プラットフォーム未対応）"
            );
        }
    });
}

/// Linux 経路専用（上記 `RUNTIME_FALLBACK_LOG_ONCE` のコメント参照）。
#[cfg(target_os = "linux")]
fn log_runtime_fallback_engaged(reason: &str) {
    RUNTIME_FALLBACK_LOG_ONCE.call_once(|| {
        warn!(
            "F-153: openat2(RESOLVE_BENEATH) が利用できないため（{}）、以後 canonicalize \
             フォールバックへ切り替えます（プロセス全体で 1 回だけ判定・以後は再試行しません）",
            reason
        );
    });
}

/// 静的ルートを登録する（設定ロード時に 1 回だけ呼ぶコールドパス）。
///
/// - Linux: 各ルートを `O_DIRECTORY` で open し、dirfd を保持する。
/// - FreeBSD: F-123 の `security::capsicum::init_static_dirfds` を再利用する
///   （capability mode の `cap_enter` 前後どちらでも安全に呼べる。`cap_enter` していなくても
///   dirfd 相対化自体は有効化される）。
/// - その他 OS: 何もしない（常にフォールバック）。
///
/// 登録に失敗したルート（ディレクトリでない・open 不可等）は警告を出して個別にスキップし、
/// 他のルートの登録は継続する（1 件の失敗で全体を諦めない）。
pub fn register_static_roots(roots: &[PathBuf]) {
    #[cfg(target_os = "linux")]
    {
        linux_impl::register(roots);
        log_registration_result(linux_impl::is_active());
    }
    #[cfg(target_os = "freebsd")]
    {
        match crate::security::capsicum::init_static_dirfds(roots) {
            Ok(()) => log_registration_result(crate::security::capsicum::static_serving_active()),
            Err(e) => {
                warn!(
                    "F-153: capsicum 静的ルート dirfd 登録に失敗（フォールバック canonicalize \
                     を使用します）: {}",
                    e
                );
                log_registration_result(false);
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        let _ = roots;
        log_registration_result(false);
    }
}

/// リクエスト処理中（`runtime::offload` のワーカースレッド内）から呼ぶ統合エントリ。
///
/// `full_path` が登録済み静的ルート配下と判定できた場合のみ `Some` を返す。
/// - `Some(Ok((file, meta)))`: カーネル封じ込め経由で開けた（`readlink` ゼロ）。
/// - `Some(Err(e))`: 登録済みルート配下だが open/stat が失敗した（404 相当）。
///   **この場合は呼び出し側は canonicalize フォールバックへ再試行しないこと**
///   （openat2 が既に権威ある判定を返しているため、二重に緩い経路を試すと
///   フェイルオープンの入り口になる）。ただし `EXDEV`/`ELOOP`（絶対シンボリックリンク等、
///   `openat2` が該当パスだけを安全側に拒否したケース）は、ここへ到達する前に
///   `linux_impl` 内部で `fallback_open_beneath` により 1 回だけ再判定済みであり、
///   その結果（許可 or 拒否）がそのまま `Some` に入る。呼び出し側から見れば
///   常に権威ある最終判定として扱ってよい。
/// - `None`: 対象外（未登録ルート・本プラットフォーム未対応・openat2 が恒久的に
///   利用不可と判明済み）。呼び出し側は従来どおり `canonicalize()` ベースの
///   フォールバックへ進むこと。
///
/// **Linux 専用**（FreeBSD は F-123 の `security::capsicum::stat_static`/`open_static_ro`
/// が既に fstatat オンリー・open オンリーの最小コストな専用経路を持っており、ここで
/// 「まず open して fstat する」形に統一すると FreeBSD の方が却って高コストになる
/// （F-123 の既存最適化を退化させてしまう）。そのため FreeBSD の呼び出し側は引き続き
/// 個別に `security::capsicum` を直接使う。その他 OS は常にフォールバック）。
pub fn open_beneath_for_request(full_path: &Path) -> Option<io::Result<(File, Metadata)>> {
    #[cfg(target_os = "linux")]
    {
        linux_impl::open_for_request(full_path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = full_path;
        None
    }
}

/// 「このルートの」dirfd に対して相対 open する（F-154）。
///
/// `open_beneath_for_request` と異なり、探索対象を **呼び出し側が指定した `root` 一つ**
/// に限定する。`root` が指す dirfd に対して `openat2(RESOLVE_BENEATH)` を発行するため、
/// **カーネルがそのまま per-route の封じ込めになる**（`root` 以外のルート配下へは
/// 構造的に出られない）。呼び出し側は結果を得た後に「実際にどのルート配下へ解決されたか」
/// を `readlink` 等で再検査する必要が一切ない（B-65 が抱えていた `readlink` 1 回/リクエスト
/// を丸ごと削除できる）。
///
/// - `root` が登録済みルートで**ない**場合、または `full_path` が `root` の配下でない
///   場合（`strip_prefix` 失敗）は `None`（呼び出し側は `canonicalize` フォールバックへ）。
/// - `Some(Ok(..))`/`Some(Err(..))` の意味・EXDEV/ELOOP 時の 1 リクエスト限りの
///   フォールバック挙動は `open_beneath_for_request` と同じ（`fallback_open_beneath` を
///   `root`/`rel` に対して呼ぶため、これも per-route のまま維持される）。
///
/// Linux 専用（FreeBSD は `security::capsicum` が「どれかの登録済みルート」方式のまま
/// だが、`cache::sendfile_base_contains` が cap_enter 下で常に `true` を返す設計のため
/// そもそも `readlink` 相当の検査を行っておらず、本チケットが解消しようとしている問題が
/// 存在しない。FreeBSD 側の per-route 化は別途の検証環境が必要なため本チケットの範囲外
/// とする）。
pub fn open_beneath_in_root(root: &Path, full_path: &Path) -> Option<io::Result<(File, Metadata)>> {
    #[cfg(target_os = "linux")]
    {
        linux_impl::open_in_root(root, full_path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (root, full_path);
        None
    }
}

/// `open_beneath_in_root` が実際に高速経路（`root` が登録済み＋openat2 利用可）を
/// 使えるかどうかを **syscall を一切発行せず** 事前判定する（F-154、`has_fast_path` の
/// ルート指定版）。
pub fn has_fast_path_in_root(root: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        linux_impl::is_registered_root(root)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = root;
        false
    }
}

/// `open_beneath_for_request` が実際に高速経路（登録済みルート＋openat2 利用可）を
/// 使えるかどうかを **syscall を一切発行せず** 事前判定する（B-65）。
///
/// 静的配信のメタデータ+本体を 1 回の offload で取得する複合 API
/// （`cache::get_static_file_with_content`）が「1 回の offload で済む高速経路」か
/// 「従来どおり 2 回 offload するフォールバック経路」かを、offload を起動する **前**に
/// 決めるために使う（該当しない場合に offload を 1 回無駄撃ちしてからフォールバックすると
/// 往復が 3 回に増えてしまうため、この事前判定が必須）。
///
/// Linux 専用（FreeBSD は `security::capsicum::is_registered_static_root` を使う）。
pub fn has_fast_path(full_path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        linux_impl::is_registered(full_path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = full_path;
        false
    }
}

/// 単発の封じ込め付き open（ルート dirfd を毎回新規 open する汎用版）。
///
/// `register_static_roots` による事前登録を必要としないため、テストや
/// アドホックな呼び出しに向く。ホットパスでは `open_beneath_for_request`
/// （登録済み dirfd を再利用する高速経路）を使うこと — こちらは呼び出しごとに
/// ルートディレクトリを開き直す分コストが高い。
///
/// 実装の分岐:
/// 1. Linux 5.6+: `openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)`。
/// 2. FreeBSD 13+: `openat(O_RESOLVE_BENEATH)`。
/// 3. 上記が使えない（古いカーネル・他 OS・ENOSYS/EPERM 観測後）:
///    `canonicalize()` + 含有チェック（従来どおりの防御）。
pub fn open_beneath(root: &Path, rel: &Path) -> io::Result<(File, Metadata)> {
    #[cfg(target_os = "linux")]
    if let Some(result) = linux_impl::open_beneath_fresh(root, rel) {
        return result;
    }
    #[cfg(target_os = "freebsd")]
    if let Some(result) = freebsd_impl::open_beneath_fresh(root, rel) {
        return result;
    }
    fallback_open_beneath(root, rel)
}

/// フォールバック実装: `canonicalize()` + 含有チェック（従来の静的配信と同じ防御）。
///
/// `root` 自体も `canonicalize()` する（シンボリックリンクを含むルート設定でも
/// 正しく比較できるようにするため。cf. `cache::sendfile_base_contains` の
/// `canonical_base` と同じ考え方）。
fn fallback_open_beneath(root: &Path, rel: &Path) -> io::Result<(File, Metadata)> {
    let full = if rel.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let canonical_root = root.canonicalize()?;
    let canonical_full = full.canonicalize()?;
    if !canonical_full.starts_with(&canonical_root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "path resolves outside of the configured static root",
        ));
    }
    // 理由付き allow: フォールバック経路は offload の専用ワーカースレッド内から
    // 呼ばれる同期 FS 操作（`register_static_roots`/`open_beneath_for_request` の
    // 呼び出し元がいずれもホットパス外・offload 内であることを保証している）。
    #[allow(clippy::disallowed_methods)]
    let file = File::open(&canonical_full)?;
    let meta = file.metadata()?;
    Ok((file, meta))
}

/// テスト専用: openat2/capsicum を経由せず、必ずフォールバック経路を通す。
/// 実装が openat2 経路と同じ判定を返すことを検証するために使う。
#[cfg(test)]
pub(crate) fn open_beneath_force_fallback(root: &Path, rel: &Path) -> io::Result<(File, Metadata)> {
    fallback_open_beneath(root, rel)
}

// ============================================================================
// Linux: openat2(2) 実装
// ============================================================================
#[cfg(target_os = "linux")]
mod linux_impl {
    use super::{fallback_open_beneath, info, log_runtime_fallback_engaged, warn};
    use std::ffi::CString;
    use std::fs::File;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::{FromRawFd, RawFd};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::OnceLock;

    /// `<linux/openat2.h>` の `struct open_how`。glibc/musl の `libc` クレートに
    /// 非 `#[non_exhaustive]` な形での公開が無いため自前定義する（ABI は安定済み）。
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }

    /// 登録済み静的ルート（config に書かれたパス原形 → ディレクトリ fd）。
    ///
    /// キーは **canonicalize しない生のパス**（capsicum::STATIC_DIRS と同じ設計）。
    /// 配信側の `full_path` は常にこの生の base_path を起点に join して構築されるため、
    /// `strip_prefix` による前方一致がここでは常に成立する。dirfd を開くための
    /// `libc::open` はプロセスの CWD 相対でも絶対でもどちらでも正しく動く。
    struct LinuxRoot {
        root: PathBuf,
        dirfd: RawFd,
    }

    static LINUX_ROOTS: OnceLock<Vec<LinuxRoot>> = OnceLock::new();
    /// openat2 が恒久的に使えないと判明したか（ENOSYS/EPERM を一度でも観測したら true）。
    static OPENAT2_UNAVAILABLE: AtomicBool = AtomicBool::new(false);

    pub(super) fn is_active() -> bool {
        LINUX_ROOTS.get().is_some_and(|v| !v.is_empty()) && !openat2_unavailable()
    }

    fn openat2_unavailable() -> bool {
        OPENAT2_UNAVAILABLE.load(Ordering::Relaxed)
    }

    fn mark_openat2_unavailable(reason: &str) {
        OPENAT2_UNAVAILABLE.store(true, Ordering::Relaxed);
        log_runtime_fallback_engaged(reason);
    }

    pub(super) fn register(roots: &[std::path::PathBuf]) {
        let mut dirs: Vec<LinuxRoot> = Vec::new();
        for root in roots {
            if dirs.iter().any(|d| &d.root == root) {
                continue;
            }
            let cpath = match CString::new(root.as_os_str().as_bytes()) {
                Ok(c) => c,
                Err(_) => {
                    warn_skip(root, "path contains NUL byte");
                    continue;
                }
            };
            // 理由付き allow: 起動時コールドパス（設定ロード時に 1 回だけ）。
            #[allow(clippy::disallowed_methods)]
            let fd = unsafe {
                libc::open(
                    cpath.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                warn_skip(root, &io::Error::last_os_error().to_string());
                continue;
            }
            info!(
                "F-153: 静的ルート dirfd={} を登録（{:?}、openat2 RESOLVE_BENEATH 相対化対象）",
                fd, root
            );
            dirs.push(LinuxRoot {
                root: root.clone(),
                dirfd: fd,
            });
        }
        // 複数回呼ばれても 2 回目以降は無視される（設定ロードは通常起動時 1 回のみ）。
        let _ = LINUX_ROOTS.set(dirs);
    }

    fn warn_skip(root: &Path, reason: &str) {
        warn!(
            "F-153: 静的ルート {:?} の dirfd 登録をスキップ（このルートは canonicalize \
             フォールバックのまま動作します）: {}",
            root, reason
        );
    }

    /// `open_how` を用いた raw `openat2(2)` 呼び出し。
    ///
    /// SAFETY: `dirfd` は呼び出し元が所有する有効なディレクトリ fd（登録済み dirfd、
    /// または呼び出し元が直前に open した一時 dirfd）であること。`rel`/`how` は
    /// syscall 実行中のみ有効なスタック上の値。
    fn raw_openat2(dirfd: RawFd, rel: &std::ffi::CStr) -> io::Result<RawFd> {
        let how = OpenHow {
            flags: (libc::O_RDONLY | libc::O_CLOEXEC) as u64,
            mode: 0,
            resolve: libc::RESOLVE_BENEATH | libc::RESOLVE_NO_MAGICLINKS,
        };
        let ret = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                dirfd,
                rel.as_ptr(),
                &how as *const OpenHow as *const libc::c_void,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret as RawFd)
        }
    }

    /// `openat2` 自体が恒久的に使えない（カーネルが対応していない・seccomp 等で
    /// ブロックされている）ことを示すエラー。これを観測したら `OPENAT2_UNAVAILABLE`
    /// をメモ化し、以後は毎リクエスト失敗する syscall を撃たずフォールバックへ回る。
    fn is_unsupported(e: &io::Error) -> bool {
        matches!(
            e.raw_os_error(),
            Some(libc::ENOSYS) | Some(libc::EPERM) | Some(libc::EOPNOTSUPP)
        )
    }

    /// `openat2` は使えるが、**この特定のパスだけ** `RESOLVE_BENEATH` が安全側に倒して
    /// 拒否したことを示すエラー。恒久フォールバックへは倒さず、このリクエスト限りで
    /// `fallback_open_beneath`（`canonicalize()` + 含有チェック）にもう一度だけ判定させる。
    ///
    /// - `EXDEV`: `RESOLVE_BENEATH` は**絶対パスを指すシンボリックリンク**を、
    ///   リンク先がルート内かどうかに関わらず一律拒否する仕様
    ///   （`openat2(2)`: "This causes absolute symbolic links ... to be rejected"）。
    ///   `/var/www/current -> /var/www/releases/vNNN` のような、デプロイで極めて一般的な
    ///   絶対シンボリックリンクが `canonicalize()` では従来問題なく配信できていたため、
    ///   ここで一律 404 にすると回帰になる。フォールバックの含有チェックが最終防御を担う
    ///   （「速い経路で弾かれたものだけ安全な遅い経路で確認し直す」構造であり、
    ///   フェイルオープンではない）。
    /// - `ELOOP`: `RESOLVE_NO_MAGICLINKS` がマジックリンク（`/proc/self/fd/N` 等の
    ///   非通常シンボリックリンク）や過度なシンボリックリンクの入れ子を拒否した際にも
    ///   返り得る。`openat2` そのものが使えないわけではなく該当パス固有の事情のため、
    ///   `EXDEV` と同様に扱う（防御的措置。実際に観測された報告は無いが、
    ///   フォールバックの含有チェックが最終判定を行うため安全側に倒しても害はない）。
    fn is_retry_fallback(e: &io::Error) -> bool {
        matches!(e.raw_os_error(), Some(libc::EXDEV) | Some(libc::ELOOP))
    }

    fn path_to_rel_cstring(rel: &Path) -> io::Result<CString> {
        if rel.as_os_str().is_empty() {
            Ok(CString::new(".").unwrap())
        } else {
            CString::new(rel.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL byte"))
        }
    }

    fn finish(fd: RawFd) -> io::Result<(File, std::fs::Metadata)> {
        // SAFETY: openat2 が返した所有権のある有効な fd。
        let file = unsafe { File::from_raw_fd(fd) };
        let meta = file.metadata()?;
        Ok((file, meta))
    }

    /// `full_path` が登録済み静的ルート配下かどうかを syscall なしで判定する（B-65）。
    pub(super) fn is_registered(full_path: &Path) -> bool {
        if openat2_unavailable() {
            return false;
        }
        match LINUX_ROOTS.get() {
            Some(roots) => roots
                .iter()
                .any(|r| full_path.strip_prefix(&r.root).is_ok()),
            None => false,
        }
    }

    /// `root` が登録済みルートかどうかを syscall なしで判定する（F-154）。
    pub(super) fn is_registered_root(root: &Path) -> bool {
        if openat2_unavailable() {
            return false;
        }
        match LINUX_ROOTS.get() {
            Some(roots) => roots.iter().any(|r| r.root == root),
            None => false,
        }
    }

    /// `root` の登録済み dirfd に対してのみ相対 open する高速経路（F-154）。
    /// `open_for_request` と異なり、`root` 以外のルートは一切探索しない
    /// （呼び出し側が「このリクエストを処理しているルート」を明示するため、
    /// カーネルの `RESOLVE_BENEATH` がそのまま per-route の封じ込めになる）。
    pub(super) fn open_in_root(
        root: &Path,
        full_path: &Path,
    ) -> Option<io::Result<(File, std::fs::Metadata)>> {
        if openat2_unavailable() {
            return None;
        }
        let roots = LINUX_ROOTS.get()?;
        let r = roots.iter().find(|r| r.root == root)?;
        let rel = full_path.strip_prefix(root).ok()?;
        let rel_c = match path_to_rel_cstring(rel) {
            Ok(c) => c,
            Err(e) => return Some(Err(e)),
        };
        Some(match raw_openat2(r.dirfd, &rel_c) {
            Ok(fd) => finish(fd),
            Err(e) if is_unsupported(&e) => {
                mark_openat2_unavailable(&e.to_string());
                return None;
            }
            // EXDEV/ELOOP: このパスだけ openat2 が安全側に拒否した。1 リクエスト限りで
            // canonicalize + 含有チェックへ再判定させる（`root`/`rel` を渡すため、
            // この再判定も per-route のまま維持される）。
            Err(e) if is_retry_fallback(&e) => fallback_open_beneath(root, rel),
            Err(e) => Err(e),
        })
    }

    /// 登録済み dirfd を使う高速経路（ホットパス用。ルート open をやり直さない）。
    pub(super) fn open_for_request(
        full_path: &Path,
    ) -> Option<io::Result<(File, std::fs::Metadata)>> {
        if openat2_unavailable() {
            return None;
        }
        let roots = LINUX_ROOTS.get()?;
        for r in roots {
            if let Ok(rel) = full_path.strip_prefix(&r.root) {
                let rel_c = match path_to_rel_cstring(rel) {
                    Ok(c) => c,
                    Err(e) => return Some(Err(e)),
                };
                return Some(match raw_openat2(r.dirfd, &rel_c) {
                    Ok(fd) => finish(fd),
                    Err(e) if is_unsupported(&e) => {
                        mark_openat2_unavailable(&e.to_string());
                        return None;
                    }
                    // EXDEV/ELOOP: このパスだけ openat2 が安全側に拒否した（絶対
                    // シンボリックリンク等）。恒久フォールバックへは倒さず、1 リクエスト
                    // 限りで canonicalize + 含有チェックにもう一度だけ判定させる
                    // （`is_retry_fallback` のコメント参照。フェイルオープンではない）。
                    Err(e) if is_retry_fallback(&e) => fallback_open_beneath(&r.root, rel),
                    Err(e) => Err(e),
                });
            }
        }
        None
    }

    /// ルートを毎回新規 open する汎用版（テスト・アドホック呼び出し向け）。
    pub(super) fn open_beneath_fresh(
        root: &Path,
        rel: &Path,
    ) -> Option<io::Result<(File, std::fs::Metadata)>> {
        if openat2_unavailable() {
            return None;
        }
        let root_c = match CString::new(root.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => {
                return Some(Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "root path contains NUL byte",
                )))
            }
        };
        // 理由付き allow: テスト・アドホック呼び出し専用の低頻度経路（ホットパスは
        // `open_for_request` の登録済み dirfd を使う）。
        #[allow(clippy::disallowed_methods)]
        let dirfd = unsafe {
            libc::open(
                root_c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if dirfd < 0 {
            return Some(Err(io::Error::last_os_error()));
        }
        let rel_c = match path_to_rel_cstring(rel) {
            Ok(c) => c,
            Err(e) => {
                unsafe { libc::close(dirfd) };
                return Some(Err(e));
            }
        };
        let result = match raw_openat2(dirfd, &rel_c) {
            Ok(fd) => Some(finish(fd)),
            Err(e) if is_unsupported(&e) => {
                mark_openat2_unavailable(&e.to_string());
                None
            }
            // EXDEV/ELOOP: このパスだけ openat2 が安全側に拒否した。dirfd はここで
            // 不要になった（フォールバックは root を自前で canonicalize して開き直す）
            // ので先に close してから 1 リクエスト限りの再判定を行う。
            Err(e) if is_retry_fallback(&e) => {
                unsafe { libc::close(dirfd) };
                return Some(fallback_open_beneath(root, rel));
            }
            Err(e) => Some(Err(e)),
        };
        unsafe { libc::close(dirfd) };
        result
    }

    #[cfg(test)]
    pub(super) fn reset_for_test() {
        OPENAT2_UNAVAILABLE.store(false, Ordering::Relaxed);
    }
}

// ============================================================================
// FreeBSD: capsicum（F-123）の O_RESOLVE_BENEATH を使った汎用版
// ============================================================================
#[cfg(target_os = "freebsd")]
mod freebsd_impl {
    use std::ffi::CString;
    use std::fs::File;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::FromRawFd;
    use std::path::Path;

    /// ルートを毎回新規 open する汎用版（テスト・アドホック呼び出し向け）。
    /// 登録済み dirfd を使うホットパスは `security::capsicum::open_static_ro`
    /// （`open_beneath_for_request` から直接呼ぶ）を使う。
    pub(super) fn open_beneath_fresh(
        root: &Path,
        rel: &Path,
    ) -> Option<io::Result<(File, std::fs::Metadata)>> {
        let root_c = CString::new(root.as_os_str().as_bytes()).ok()?;
        // 理由付き allow: テスト・アドホック呼び出し専用の低頻度経路。
        #[allow(clippy::disallowed_methods)]
        let dirfd = unsafe {
            libc::open(
                root_c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if dirfd < 0 {
            return Some(Err(io::Error::last_os_error()));
        }
        let rel_c = if rel.as_os_str().is_empty() {
            CString::new(".").unwrap()
        } else {
            match CString::new(rel.as_os_str().as_bytes()) {
                Ok(c) => c,
                Err(_) => {
                    unsafe { libc::close(dirfd) };
                    return Some(Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "rel path contains NUL byte",
                    )));
                }
            }
        };
        let fd = unsafe {
            libc::openat(
                dirfd,
                rel_c.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_RESOLVE_BENEATH,
            )
        };
        let result = if fd < 0 {
            Some(Err(io::Error::last_os_error()))
        } else {
            // SAFETY: openat が返した所有権のある有効な fd。
            let file = unsafe { File::from_raw_fd(fd) };
            Some(file.metadata().map(|meta| (file, meta)))
        };
        unsafe { libc::close(dirfd) };
        result
    }
}

// 理由付き allow: テストのフィクスチャ作成（一時ファイル書き込み）であり、データプレーン
// ではない（AGENTS.md がホットパス外の正当な同期 FS 利用として許可する用途）。
#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::tempdir;

    #[cfg(target_os = "linux")]
    fn reset_openat2_memoization() {
        linux_impl::reset_for_test();
    }
    #[cfg(not(target_os = "linux"))]
    fn reset_openat2_memoization() {}

    /// 1〜5 のシナリオを openat2/capsicum 経路とフォールバック経路の両方で実行し、
    /// 「成功/失敗」の判定が一致することを検証する共通ヘルパ。
    fn assert_same_verdict(root: &Path, rel: &Path) {
        reset_openat2_memoization();
        let primary = open_beneath(root, rel);
        let fallback = open_beneath_force_fallback(root, rel);
        assert_eq!(
            primary.is_ok(),
            fallback.is_ok(),
            "openat2/capsicum 経路とフォールバック経路で判定が食い違った: root={:?} rel={:?} \
             primary={:?} fallback={:?}",
            root,
            rel,
            primary.as_ref().err(),
            fallback.as_ref().err()
        );
    }

    #[test]
    fn opens_regular_file_under_root() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("hello.txt");
        {
            let mut f = fs::File::create(&file_path).unwrap();
            f.write_all(b"hello, world").unwrap();
        }

        let rel = Path::new("hello.txt");
        let (mut file, meta) = open_beneath(dir.path(), rel).expect("should open");
        assert_eq!(meta.len(), 12);
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut file, &mut buf).unwrap();
        assert_eq!(buf, "hello, world");

        assert_same_verdict(dir.path(), rel);
    }

    #[test]
    fn rejects_dot_dot_escape() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        // ルート外の秘密ファイル（tempdir の親、つまり root の外）。
        let outside_dir = tempdir().unwrap();
        let secret = outside_dir.path().join("secret.txt");
        fs::write(&secret, b"top secret").unwrap();

        // sub/../../<outside>/secret.txt 相当のトラバーサル。
        let escape_count = dir.path().components().count() + 2;
        let mut rel = PathBuf::new();
        rel.push("sub");
        for _ in 0..escape_count {
            rel.push("..");
        }
        for comp in secret.strip_prefix("/").unwrap_or(&secret).components() {
            rel.push(comp);
        }

        let result = open_beneath(dir.path(), &rel);
        assert!(result.is_err(), "'..' でルート外へ出るパスは拒否されるべき");

        assert_same_verdict(dir.path(), &rel);
    }

    /// 最重要テスト: ルート外を指す**絶対**シンボリックリンクが拒否されること。
    ///
    /// `openat2(RESOLVE_BENEATH)` は絶対パスを指すシンボリックリンクをリンク先に
    /// 関わらず一律 `EXDEV` で拒否する。`open_beneath`/`open_for_request` はこの
    /// `EXDEV` を観測すると（`OPENAT2_UNAVAILABLE` をメモ化せず）このリクエスト限りで
    /// `fallback_open_beneath`（`canonicalize()` + 含有チェック）に再判定させる設計
    /// （`is_retry_fallback` のコメント参照）。このテストはその再判定経路が実際に
    /// 「ルート外を指す絶対シンボリックリンクを拒否する」という結論に正しく到達する
    /// ことを検証する（EXDEV → フォールバック → 含有チェックで拒否、を実地で通す）。
    #[test]
    fn rejects_absolute_symlink_escaping_root() {
        let dir = tempdir().unwrap();
        let outside_dir = tempdir().unwrap();
        let secret = outside_dir.path().join("secret.txt");
        fs::write(&secret, b"top secret").unwrap();

        // secret は tempdir() が返す絶対パスなので、これは絶対シンボリックリンクになる。
        let link_path = dir.path().join("escape_link");
        std::os::unix::fs::symlink(&secret, &link_path).unwrap();
        assert!(secret.is_absolute());

        let rel = Path::new("escape_link");
        let result = open_beneath(dir.path(), rel);
        assert!(
            result.is_err(),
            "ルート外を指す絶対シンボリックリンクは拒否されるべき（EXDEV → フォールバック \
             含有チェックで最終的に拒否されること）"
        );

        assert_same_verdict(dir.path(), rel);
    }

    /// 絶対シンボリックリンク（デプロイでの `current -> releases/vNNN` のような形）が
    /// リンク先がルート**内**であれば従来どおり配信できること（回帰防止）。
    ///
    /// `openat2` はこのケースでも `EXDEV` を返す（絶対リンクは一律拒否のため）が、
    /// 1 リクエスト限りのフォールバック（`canonicalize()` + 含有チェック）がリンク先を
    /// 正しく解決し、ルート内であることを確認した上で許可する。
    #[test]
    fn allows_symlink_within_root() {
        let dir = tempdir().unwrap();
        let target_path = dir.path().join("real.txt");
        fs::write(&target_path, b"inside root").unwrap();
        let link_path = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target_path, &link_path).unwrap();

        let rel = Path::new("link.txt");
        let (_file, meta) = open_beneath(dir.path(), rel)
            .expect("ルート内で完結するシンボリックリンクは許可されるべき");
        assert_eq!(meta.len(), 11);

        assert_same_verdict(dir.path(), rel);
    }

    /// 相対シンボリックリンク（`symlink("real.txt", link)`）はルート内で完結する限り
    /// `openat2` の高速経路（`RESOLVE_NO_MAGICLINKS` のみに抵触、`EXDEV` にはならない）
    /// でそのまま解決されるべきこと。絶対リンクの `allows_symlink_within_root`
    /// （フォールバック経由）と対になる、高速経路そのものの回帰防止テスト。
    #[test]
    fn allows_relative_symlink_within_root() {
        let dir = tempdir().unwrap();
        let target_path = dir.path().join("real.txt");
        fs::write(&target_path, b"inside root").unwrap();
        let link_path = dir.path().join("rel_link.txt");
        // ターゲットは相対パスで指定する（絶対パスにしない）。
        std::os::unix::fs::symlink("real.txt", &link_path).unwrap();

        let rel = Path::new("rel_link.txt");
        let (_file, meta) = open_beneath(dir.path(), rel)
            .expect("ルート内で完結する相対シンボリックリンクは許可されるべき");
        assert_eq!(meta.len(), 11);

        assert_same_verdict(dir.path(), rel);
    }

    #[test]
    fn nonexistent_file_is_not_found() {
        let dir = tempdir().unwrap();
        let rel = Path::new("does_not_exist.txt");
        let err = open_beneath(dir.path(), rel).expect_err("存在しないファイルは Err");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);

        let fallback_err = open_beneath_force_fallback(dir.path(), rel)
            .expect_err("フォールバック経路も同様に Err");
        assert_eq!(fallback_err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn empty_rel_opens_root_itself() {
        let dir = tempdir().unwrap();
        let (_file, meta) = open_beneath(dir.path(), Path::new("")).expect("root 自身を開ける");
        assert!(meta.is_dir());
    }
}
