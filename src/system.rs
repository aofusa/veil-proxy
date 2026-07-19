// ====================
// システム機能モジュール
// ====================
//
// HugePages、権限降格、サンドボックス設定、
// パニックキャッチ、CBPFなどのシステムレベル機能

use ftlog::{error, info, warn};
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
#[allow(unused_imports)]
use std::sync::atomic::Ordering;
#[allow(unused_imports)]
use std::sync::Arc;
use std::task::{Context, Poll};

// ====================
// HugePages
// ====================

/// Huge Pages の可用性情報
#[derive(Debug)]
pub(crate) struct HugePagesInfo {
    /// Huge Pagesが利用可能かどうか
    pub(crate) available: bool,
    /// 確保済みHuge Pages総数
    pub(crate) total: u64,
    /// 空きHuge Pages数
    pub(crate) free: u64,
    /// Huge Pagesサイズ（KB）
    pub(crate) page_size_kb: u64,
}

/// /proc/meminfo から Huge Pages 情報を取得
///
/// Linux以外の環境やファイルアクセスに失敗した場合は
/// available=false を返します。
#[cfg(target_os = "linux")]
pub(crate) fn check_huge_pages_availability() -> HugePagesInfo {
    use std::fs::File as StdFile;
    use std::io::{BufRead, BufReader as StdBufReader};

    let mut info = HugePagesInfo {
        available: false,
        total: 0,
        free: 0,
        page_size_kb: 0,
    };

    let file = match StdFile::open("/proc/meminfo") {
        Ok(f) => f,
        Err(_) => return info,
    };

    let reader = StdBufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
        if line.starts_with("HugePages_Total:") {
            if let Some(val) = line.split_whitespace().nth(1) {
                info.total = val.parse().unwrap_or(0);
            }
        } else if line.starts_with("HugePages_Free:") {
            if let Some(val) = line.split_whitespace().nth(1) {
                info.free = val.parse().unwrap_or(0);
            }
        } else if line.starts_with("Hugepagesize:") {
            if let Some(val) = line.split_whitespace().nth(1) {
                info.page_size_kb = val.parse().unwrap_or(0);
            }
        }
    }

    info.available = info.total > 0;
    info
}

/// Linux以外の環境ではHuge Pagesは利用不可
#[cfg(not(target_os = "linux"))]
pub(crate) fn check_huge_pages_availability() -> HugePagesInfo {
    HugePagesInfo {
        available: false,
        total: 0,
        free: 0,
        page_size_kb: 0,
    }
}

/// mimalloc の Large OS Pages 設定を有効化し、状態をログ出力
///
/// Huge Pagesが利用可能な場合は有効化し、
/// 利用不可の場合は警告を出力して通常ページにフォールバックします。
pub(crate) fn configure_huge_pages(enabled: bool) {
    if !enabled {
        info!("Huge Pages: Disabled in configuration");
        return;
    }

    let hp_info = check_huge_pages_availability();

    if hp_info.available {
        // libmimalloc-sys を使用して Large OS Pages を有効化（mimalloc feature 有効時のみ）
        #[cfg(all(target_os = "linux", feature = "mimalloc"))]
        {
            unsafe {
                // mi_option_large_os_pages = 6 (2MiB large pages)
                libmimalloc_sys::mi_option_set(libmimalloc_sys::mi_option_large_os_pages, 1);
            }
        }

        info!(
            "Huge Pages: Enabled (Total: {}, Free: {}, Size: {}KB)",
            hp_info.total, hp_info.free, hp_info.page_size_kb
        );
        info!("Huge Pages: TLB miss reduction active, expected 5-10% performance improvement");

        // 空きページが少ない場合は警告
        if hp_info.free < hp_info.total / 2 {
            warn!(
                "Huge Pages: Free pages running low ({}/{}), consider increasing nr_hugepages",
                hp_info.free, hp_info.total
            );
        }
    } else {
        warn!("Huge Pages: Requested but not available on this system");
        #[cfg(target_os = "linux")]
        {
            warn!("Huge Pages: To enable, run: echo 128 | sudo tee /proc/sys/vm/nr_hugepages");
            warn!("Huge Pages: In containers, ensure hugepages are allocated on the host");
        }
        #[cfg(not(target_os = "linux"))]
        {
            warn!("Huge Pages: Only supported on Linux");
        }
        info!("Huge Pages: Falling back to standard 4KB pages");
    }
}

// ====================
// RLIMIT_NOFILE 引き上げ（B-44 第4段）
// ====================

/// 起動時に RLIMIT_NOFILE の soft limit を hard limit まで引き上げる（B-44 第4段）。
///
/// nginx の `worker_rlimit_nofile` に相当する起動時処理。F-116 の HTTP/2 ストリーム
/// 多重化により、同時 1000 ストリーム（h2load `-c100 -m10` 相当）ではクライアント側
/// 100 fd + バックエンド側 ~1000 fd + ring/eventfd/リスナー等で **~1100 fd** を要求し、
/// コンテナ既定の soft limit 1024 を超過して EMFILE（Too many open files）が発生する
/// （hard は 524288 等で十分に大きいが、veil が soft を引き上げていなかった）。
/// soft を hard まで自動で引き上げることで、運用側は systemd の `LimitNOFILE` /
/// docker `--ulimit nofile` による hard の制御だけで fd 上限を管理できる。
///
/// 失敗時は warn ログを出して続行する（fail-open。権限がない環境でも起動は継続）。
/// ワーカー起動・seccomp 適用より前のコールドパスで 1 回だけ呼ぶこと
/// （`prlimit64` は seccomp 許可リストに含まれるが、起動時に完結させる）。
pub fn raise_nofile_limit() {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: lim は有効なスタック上の rlimit 構造体で、getrlimit はそこへ書き込むのみ。
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        warn!(
            "RLIMIT_NOFILE: getrlimit failed: {} (continuing with current limit)",
            io::Error::last_os_error()
        );
        return;
    }
    if lim.rlim_cur >= lim.rlim_max {
        info!(
            "RLIMIT_NOFILE: soft limit already at hard limit ({})",
            lim.rlim_cur
        );
        return;
    }
    let old_soft = lim.rlim_cur;
    lim.rlim_cur = lim.rlim_max;
    // SAFETY: lim は初期化済みの rlimit 構造体で、setrlimit はそれを読み取るのみ。
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) } != 0 {
        warn!(
            "RLIMIT_NOFILE: setrlimit failed: {} (continuing with soft limit {})",
            io::Error::last_os_error(),
            old_soft
        );
        return;
    }
    info!(
        "raised RLIMIT_NOFILE soft: {} -> {}",
        old_soft, lim.rlim_cur
    );
}

// ====================
// Task-Level Panic Recovery (Layer 1)
// ====================
//
// monoio::spawn does NOT catch panics like Tokio does.
// A panic in a monoio::spawn task kills the entire runtime thread.
// This wrapper uses std::panic::catch_unwind to prevent thread death.
//
// Note: We cannot use FutureExt::catch_unwind from futures crate since
// it's not a dependency. Instead, we use a poll-based approach with
// std::panic::catch_unwind around each poll.

/// A future wrapper that catches panics during polling
///
/// F-46: ジェネリック化して内部の `Box<dyn Future>` を排除した（型付きタスクプール
/// [`crate::runtime::TaskPool`] にインライン格納できる）。pin 投影は
/// `Pin::new_unchecked` による構造的投影で行う（下記 SAFETY 参照）。
pub(crate) struct CatchUnwindFuture<F> {
    inner: Option<F>,
}

impl<F> CatchUnwindFuture<F>
where
    F: std::future::Future<Output = ()> + 'static,
{
    pub(crate) fn new(future: F) -> Self {
        Self {
            inner: Some(future),
        }
    }
}

impl<F> std::future::Future for CatchUnwindFuture<F>
where
    F: std::future::Future<Output = ()> + 'static,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: 構造的 pin 投影。`inner` の future は完了/パニック時に in-place で
        // `None` 化（drop）する以外にムーブしない（Drop 保証を満たす）。
        let this = unsafe { self.get_unchecked_mut() };
        let inner = match this.inner.as_mut() {
            Some(f) => f,
            None => return Poll::Ready(()),
        };
        // SAFETY: 上記のとおり inner はムーブしないため再ピン留めできる。
        let pinned = unsafe { Pin::new_unchecked(inner) };

        // Wrap the poll in catch_unwind to catch panics
        let result = catch_unwind(AssertUnwindSafe(|| pinned.poll(cx)));

        match result {
            Ok(Poll::Ready(())) => {
                this.inner = None;
                Poll::Ready(())
            }
            Ok(Poll::Pending) => Poll::Pending,
            Err(panic_info) => {
                // Panic caught - log and complete the future
                error!("Task panicked during poll: {:?}", panic_info);
                this.inner = None;
                Poll::Ready(())
            }
        }
    }
}

/// Spawn a task with panic catching
///
/// Unlike Tokio, monoio does not catch panics in spawned tasks.
/// A panic in a monoio::spawn task kills the entire runtime thread,
/// causing all other connections on that worker to be lost.
///
/// This wrapper catches panics and logs them, preventing thread death.
/// The ConnectionGuard's Drop trait still runs on panic, ensuring
/// connection counters remain accurate.
pub(crate) fn spawn_with_panic_catch<F>(future: F)
where
    F: std::future::Future<Output = ()> + 'static,
{
    crate::runtime::spawn(CatchUnwindFuture::new(future));
}

/// F-46: 型付きタスクプールへパニックキャッチ付きで spawn する。
///
/// `spawn_with_panic_catch` と異なり、`Box<dyn Future>` 確保を伴わない
/// （プールのスラブスロットへインライン格納。ウォームアップ後は malloc ゼロ）。
/// プールは spawn 呼び出しサイト（accept ループ等）で `TaskPool::new()` して使い回す。
pub(crate) fn spawn_pooled_with_panic_catch<F>(
    pool: &crate::runtime::TaskPool<CatchUnwindFuture<F>>,
    future: F,
) where
    F: std::future::Future<Output = ()> + 'static,
{
    pool.spawn(CatchUnwindFuture::new(future));
}

// ====================
// 権限降格機能
// ====================
//
// root権限で起動した後、非特権ユーザーに降格することで
// セキュリティを向上させます。
//
// 注意: 特権ポート（1024未満）を使用する場合は、
// リスナー作成後に権限降格を行う必要があります。
// 現在のSO_REUSEPORT設計では、各スレッドがリスナーを作成するため、
// CAP_NET_BIND_SERVICEケイパビリティを付与するか、
// 非特権ポート（1024以上）を使用することを推奨します。
//
// ケイパビリティ付与例:
//   sudo setcap 'cap_net_bind_service=+ep' ./target/release/veil
// ====================

/// ユーザー名からUIDを取得
///
/// `getpwnam(3)` は POSIX のため Linux/FreeBSD/OpenBSD 共通で使える（F-120 Phase 4 で
/// Linux 限定 cfg を撤去）。
pub(crate) fn get_uid_by_name(username: &str) -> Option<u32> {
    use std::ffi::CString;

    let username_cstr = CString::new(username).ok()?;

    unsafe {
        let pwd = libc::getpwnam(username_cstr.as_ptr());
        if pwd.is_null() {
            None
        } else {
            Some((*pwd).pw_uid)
        }
    }
}

/// グループ名からGIDを取得（POSIX、全対応 OS 共通）
pub(crate) fn get_gid_by_name(groupname: &str) -> Option<u32> {
    use std::ffi::CString;

    let groupname_cstr = CString::new(groupname).ok()?;

    unsafe {
        let grp = libc::getgrnam(groupname_cstr.as_ptr());
        if grp.is_null() {
            None
        } else {
            Some((*grp).gr_gid)
        }
    }
}

/// 権限降格を実行
///
/// グループ→ユーザーの順で降格する（逆順では失敗する可能性あり）。
/// `setgid`/`setgroups`/`setuid` は POSIX のため Linux/FreeBSD/OpenBSD 共通で動作する
/// （F-120 Phase 4 で Linux 限定 cfg とスタブを撤去）。
pub(crate) fn drop_privileges(security: &crate::GlobalSecurityConfig) -> io::Result<()> {
    // rootでない場合は何もしない
    if unsafe { libc::getuid() } != 0 {
        info!("Not running as root, skipping privilege drop");
        return Ok(());
    }

    // グループ降格（先に行う）
    if let Some(ref group_name) = security.drop_privileges_group {
        let gid = get_gid_by_name(group_name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("Group '{}' not found", group_name),
            )
        })?;

        if unsafe { libc::setgid(gid) } != 0 {
            return Err(io::Error::last_os_error());
        }

        // 補助グループをクリア
        if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
            warn!(
                "Failed to clear supplementary groups: {}",
                io::Error::last_os_error()
            );
        }

        info!("Dropped group privileges to '{}' (gid={})", group_name, gid);
    }

    // ユーザー降格
    if let Some(ref user_name) = security.drop_privileges_user {
        let uid = get_uid_by_name(user_name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("User '{}' not found", user_name),
            )
        })?;

        if unsafe { libc::setuid(uid) } != 0 {
            return Err(io::Error::last_os_error());
        }

        info!("Dropped user privileges to '{}' (uid={})", user_name, uid);
    }

    // 降格成功の確認
    if security.drop_privileges_user.is_some() || security.drop_privileges_group.is_some() {
        let current_uid = unsafe { libc::getuid() };
        let current_gid = unsafe { libc::getgid() };
        info!(
            "Current privileges: uid={}, gid={}",
            current_uid, current_gid
        );

        // rootに戻れないことを確認
        if security.drop_privileges_user.is_some() && unsafe { libc::setuid(0) } == 0 {
            warn!("WARNING: Process can still regain root privileges!");
        }
    }

    Ok(())
}

// ====================
// サンドボックス設定構築
// ====================

/// GlobalSecurityConfigからSandboxConfigを構築
///
/// 設定ファイルのsandbox_*フィールドをsecurity::SandboxConfigに変換します。
pub(crate) fn build_sandbox_config(
    global_security: &crate::GlobalSecurityConfig,
) -> crate::security::SandboxConfig {
    // 読み取り専用バインドマウントのパース
    let ro_bind_mounts: Vec<crate::security::BindMount> = global_security
        .sandbox_ro_bind_mounts
        .iter()
        .filter_map(|s| {
            let parts: Vec<&str> = s.splitn(2, ':').collect();
            if parts.len() == 2 {
                Some(crate::security::BindMount::new(parts[0], parts[1]))
            } else if parts.len() == 1 && !parts[0].is_empty() {
                // source:dest が同じ場合は source のみでも可
                Some(crate::security::BindMount::new(parts[0], parts[0]))
            } else {
                warn!(
                    "Invalid ro-bind mount format: '{}' (expected 'source:dest')",
                    s
                );
                None
            }
        })
        .collect();

    // 読み書きバインドマウントのパース
    let rw_bind_mounts: Vec<crate::security::BindMount> = global_security
        .sandbox_rw_bind_mounts
        .iter()
        .filter_map(|s| {
            let parts: Vec<&str> = s.splitn(2, ':').collect();
            if parts.len() == 2 {
                Some(crate::security::BindMount::new(parts[0], parts[1]))
            } else if parts.len() == 1 && !parts[0].is_empty() {
                Some(crate::security::BindMount::new(parts[0], parts[0]))
            } else {
                warn!(
                    "Invalid rw-bind mount format: '{}' (expected 'source:dest')",
                    s
                );
                None
            }
        })
        .collect();

    crate::security::SandboxConfig {
        enabled: global_security.enable_sandbox,
        unshare_pid: global_security.sandbox_unshare_pid,
        unshare_mount: global_security.sandbox_unshare_mount,
        unshare_uts: global_security.sandbox_unshare_uts,
        unshare_ipc: global_security.sandbox_unshare_ipc,
        unshare_user: global_security.sandbox_unshare_user,
        unshare_net: global_security.sandbox_unshare_net,
        new_root: None,
        ro_bind_mounts,
        rw_bind_mounts,
        tmpfs_mounts: global_security.sandbox_tmpfs_mounts.clone(),
        mount_proc: global_security.sandbox_mount_proc,
        mount_dev: global_security.sandbox_mount_dev,
        drop_capabilities: global_security.sandbox_drop_capabilities.clone(),
        keep_capabilities: global_security.sandbox_keep_capabilities.clone(),
        hostname: global_security.sandbox_hostname.clone(),
        no_new_privs: global_security.sandbox_no_new_privs,
    }
}

// ====================
// セキュアなメモリ操作
// ====================

/// セキュアなバイト配列のゼロ化
///
/// メモリ上の機密データを安全にゼロ化します。
/// コンパイラによる最適化（デッドストア削除）を防ぐため、
/// volatile 書き込みを使用します。
#[cfg(any(feature = "http3", feature = "http3-quiche"))]
pub(crate) fn secure_zero(data: &mut [u8]) {
    // volatile 書き込みで最適化を防止
    for byte in data.iter_mut() {
        unsafe {
            std::ptr::write_volatile(byte, 0);
        }
    }
    // メモリバリアで確実に書き込みを完了
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
}

/// Arc<Vec<u8>> をセキュアにゼロ化して解放
///
/// Arc の参照カウントが 1（自分だけ）の場合、
/// 内部の Vec をゼロ化してからドロップします。
/// 参照カウントが 2 以上の場合は警告を出力します。
#[cfg(any(feature = "http3", feature = "http3-quiche"))]
pub(crate) fn secure_clear_arc_vec(arc: &mut Arc<Vec<u8>>, name: &str) {
    match Arc::get_mut(arc) {
        Some(vec) => {
            let len = vec.len();
            secure_zero(vec);
            vec.clear();
            vec.shrink_to_fit();
            info!("[Security] {} securely zeroed ({} bytes)", name, len);
        }
        None => {
            // 他の参照が存在する場合（通常は発生しない）
            warn!(
                "[Security] {} cannot be zeroed: other references exist",
                name
            );
        }
    }
}

// ====================
// SO_REUSEPORT CBPF 振り分け
// ====================

/// CBPFプログラムがアタッチ済みかどうかを追跡するグローバルカウンター
/// 最初のワーカーのみがCBPFプログラムをアタッチする
#[cfg(target_os = "linux")]
pub(crate) static CBPF_ATTACHED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// クライアントIPハッシュに基づく振り分けCBPFプログラムを生成
///
/// このBPFプログラムは、accept()時に呼び出され、
/// クライアントのソースIPアドレスをハッシュしてワーカーインデックスを返す
///
/// # 引数
/// * `num_workers` - ワーカースレッド数
///
/// # 戻り値
/// BPF命令列（sock_filter配列）
#[cfg(target_os = "linux")]
pub(crate) fn create_reuseport_cbpf_program(num_workers: u32) -> Vec<libc::sock_filter> {
    // BPF命令セット:
    // 1. ソースIPアドレスを取得（sk_reuseport_mdからオフセット0でソースIPを読み取り）
    // 2. ワーカー数でmod演算
    // 3. 結果をソケットインデックスとして返す
    //
    // BPF_LD + BPF_W + BPF_ABS: 32ビットワードをパケットから絶対オフセットで読み込み
    // BPF_ALU + BPF_MOD + BPF_K: 即値でmod演算
    // BPF_RET + BPF_A: Aレジスタの値を返す
    vec![
        // LD A, [0]: ソースIPアドレスを読み込み（sk_reuseport_md構造体のオフセット0）
        libc::sock_filter {
            code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            jt: 0,
            jf: 0,
            k: 0, // remote_ip4 のオフセット
        },
        // ALU MOD #num_workers: A = A % num_workers
        libc::sock_filter {
            code: (libc::BPF_ALU | libc::BPF_MOD | libc::BPF_K) as u16,
            jt: 0,
            jf: 0,
            k: num_workers,
        },
        // RET A: Aレジスタの値（ソケットインデックス）を返す
        libc::sock_filter {
            code: (libc::BPF_RET | libc::BPF_A) as u16,
            jt: 0,
            jf: 0,
            k: 0,
        },
    ]
}

/// CBPFプログラムをソケットにアタッチする
///
/// SO_ATTACH_REUSEPORT_CBPF を使用して、クライアントIPベースの
/// 振り分けロジックをカーネルに設定する
///
/// # 引数
/// * `fd` - ソケットファイルディスクリプタ
/// * `num_workers` - ワーカースレッド数
///
/// # 戻り値
/// 成功時はOk(()), 失敗時はエラー
#[cfg(target_os = "linux")]
pub(crate) fn attach_reuseport_cbpf(fd: i32, num_workers: usize) -> io::Result<()> {
    let program = create_reuseport_cbpf_program(num_workers as u32);

    #[repr(C)]
    struct SockFprog {
        len: u16,
        filter: *const libc::sock_filter,
    }

    let prog = SockFprog {
        len: program.len() as u16,
        filter: program.as_ptr(),
    };

    // SO_ATTACH_REUSEPORT_CBPF の値（Linux 4.5+）
    // include/uapi/asm-generic/socket.h: #define SO_ATTACH_REUSEPORT_CBPF 51
    const SO_ATTACH_REUSEPORT_CBPF: libc::c_int = 51;

    let result = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            SO_ATTACH_REUSEPORT_CBPF,
            &prog as *const _ as *const libc::c_void,
            std::mem::size_of::<SockFprog>() as libc::socklen_t,
        )
    };

    if result < 0 {
        let err = io::Error::last_os_error();
        warn!(
            "Failed to attach CBPF program: {} (errno: {})",
            err,
            err.raw_os_error().unwrap_or(-1)
        );
        return Err(err);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// raise_nofile_limit が panic せず、soft limit が呼び出し前以上・hard 以下に
    /// なること（B-44 第4段）。到達値は環境（コンテナ/権限）依存のため、
    /// 非減少と上限内のみを検証する。
    #[test]
    fn test_raise_nofile_limit_non_decreasing() {
        let mut before = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: 有効なスタック上の rlimit 構造体への書き込みのみ。
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut before) },
            0
        );

        raise_nofile_limit();

        let mut after = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: 同上。
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut after) },
            0
        );
        assert!(
            after.rlim_cur >= before.rlim_cur,
            "soft limit must not decrease"
        );
        assert!(after.rlim_cur <= after.rlim_max);
    }
}
