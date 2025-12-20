//! # セキュリティ強化モジュール
//!
//! io_uringおよびプロセス全体のセキュリティ制限を提供します。
//!
//! ## 機能
//!
//! - **io_uring制限**: `IORING_REGISTER_RESTRICTIONS`によるio_uring操作の制限
//! - **seccompフィルタ**: BPF/seccompによるシステムコール制限
//! - **Landlock**: ファイルシステムアクセス制限（Linux 5.13+）
//!
//! ## 使用例
//!
//! ```rust,ignore
//! use security::{SecurityConfig, apply_security_restrictions};
//!
//! let config = SecurityConfig {
//!     enable_io_uring_restrictions: true,
//!     enable_seccomp: true,
//!     seccomp_mode: SeccompMode::Strict,
//!     ..Default::default()
//! };
//!
//! apply_security_restrictions(&config)?;
//! ```
//!
//! ## セキュリティモデル
//!
//! このモジュールは「最小権限の原則」に基づいています。
//! リバースプロキシに必要な最小限のシステムコールのみを許可し、
//! その他のシステムコールは拒否します。

use std::io;
use ftlog::{info, warn, error, debug};

// ====================
// io_uring 制限定数
// ====================
//
// Linux カーネル 5.10+ で導入された `IORING_REGISTER_RESTRICTIONS` 用の定数。
// monoio は io_uring の低レベル API を公開していないため、
// 現在は情報提供のみ。将来の拡張に備えて定義。
// ====================

/// IORING_REGISTER_RESTRICTIONS オペコード
/// Linux kernel 5.10+ で利用可能
#[allow(dead_code)]
const IORING_REGISTER_RESTRICTIONS: u32 = 11;

/// IORING_REGISTER_ENABLE_RINGS (制限適用後にリングを有効化)
#[allow(dead_code)]
const IORING_REGISTER_ENABLE_RINGS: u32 = 10;

/// 制限タイプ: SQE オペコードを許可
#[allow(dead_code)]
const IORING_RESTRICTION_REGISTER_OP: u16 = 0;

/// 制限タイプ: SQE オペコードを許可
#[allow(dead_code)]
const IORING_RESTRICTION_SQE_OP: u16 = 1;

/// 制限タイプ: SQE フラグを許可
#[allow(dead_code)]
const IORING_RESTRICTION_SQE_FLAGS_ALLOWED: u16 = 2;

/// 制限タイプ: SQE フラグを必須
#[allow(dead_code)]
const IORING_RESTRICTION_SQE_FLAGS_REQUIRED: u16 = 3;

// ====================
// io_uring オペコード定義
// ====================
//
// monoio (リバースプロキシ) で使用される io_uring オペコード。
// これらのみを許可することで、攻撃対象面を最小化できます。
// ====================

/// リバースプロキシで許可すべきオペコードのリスト
#[allow(dead_code)]
pub const ALLOWED_URING_OPCODES: &[u8] = &[
    0,  // IORING_OP_NOP
    1,  // IORING_OP_READV
    2,  // IORING_OP_WRITEV
    4,  // IORING_OP_FSYNC (ログファイル用)
    5,  // IORING_OP_READ_FIXED
    6,  // IORING_OP_WRITE_FIXED
    7,  // IORING_OP_POLL_ADD
    8,  // IORING_OP_POLL_REMOVE
    13, // IORING_OP_ACCEPT
    14, // IORING_OP_ASYNC_CANCEL
    16, // IORING_OP_CONNECT
    18, // IORING_OP_TIMEOUT
    19, // IORING_OP_TIMEOUT_REMOVE
    21, // IORING_OP_SEND
    22, // IORING_OP_RECV
    23, // IORING_OP_OPENAT
    24, // IORING_OP_CLOSE
    25, // IORING_OP_FILES_UPDATE
    26, // IORING_OP_STATX
    27, // IORING_OP_READ
    28, // IORING_OP_WRITE
    32, // IORING_OP_SENDMSG (UDP/HTTP3用)
    33, // IORING_OP_RECVMSG (UDP/HTTP3用)
    36, // IORING_OP_SPLICE (kTLS用)
    45, // IORING_OP_SOCKET
];

// ====================
// seccomp システムコール定数
// ====================
//
// リバースプロキシに必要な最小限のシステムコール許可リスト。
// systemd の SystemCallFilter と互換性があります。
//
// カテゴリ別の必要性:
// - io_uring: monoio ランタイムの基盤
// - ネットワーク: TCP/UDP ソケット操作
// - ファイルI/O: 設定ファイル、証明書、ログ
// - メモリ: mimalloc、Huge Pages、io_uring 登録バッファ
// - スレッド: ワーカースレッド、CPUアフィニティ
// - シグナル: SIGTERM/SIGHUP ハンドリング
// - 時間: タイムアウト処理
//
// systemd で使用する場合:
//   SystemCallFilter=@system-service io_uring_setup io_uring_enter io_uring_register
// ====================

/// systemd SystemCallFilter 互換のシステムコール一覧
/// 
/// この定数は、systemd の SystemCallFilter ディレクティブで使用できる
/// 必要最小限のシステムコール名のリストです。
/// 
/// # 使用例 (systemd unit file)
/// ```ini
/// [Service]
/// SystemCallFilter=@system-service
/// SystemCallFilter=io_uring_setup io_uring_enter io_uring_register
/// SystemCallFilter=mlock mlock2 mlockall
/// ```
pub const SYSTEMD_SYSCALL_FILTER: &str = r#"
# ============================================
# zerocopy-server 必須システムコール一覧
# ============================================
# systemd SystemCallFilter 形式
# 
# 基本セット (@system-service に含まれる):
#   @basic-io @file-system @io-event @ipc @network-io
#   @process @signal @timer
#
# 追加で必要なシステムコール:
#   io_uring_setup io_uring_enter io_uring_register
#   mlock mlock2 mlockall (io_uring登録バッファ、Huge Pages用)
#   sched_setaffinity sched_getaffinity (CPUピンニング用)
#
# ============================================

# --- 必須: io_uring (monoio ランタイム) ---
io_uring_setup
io_uring_enter
io_uring_register

# --- 必須: メモリロック (io_uring 登録バッファ) ---
mlock
mlock2
mlockall
munlock
munlockall

# --- オプション: CPUアフィニティ ---
sched_setaffinity
sched_getaffinity

# --- オプション: kTLS ---
# splice (kTLS ゼロコピー用、kTLS feature 使用時)

# --- @system-service に含まれるため明示不要 ---
# socket, bind, listen, accept, accept4, connect
# sendto, recvfrom, sendmsg, recvmsg, setsockopt, getsockopt
# read, write, openat, close, fstat, mmap, munmap
# clone, clone3, futex, exit_group
# rt_sigaction, rt_sigprocmask
# clock_gettime, nanosleep
# prctl, ioctl, getrandom, fcntl
"#;

/// 許可するシステムコールのリスト (x86_64)
/// 
/// 最小限のシステムコールのみを含みます。
/// fork, execve, wait4 等の外部プロセス管理は不要なため除外。
#[cfg(target_arch = "x86_64")]
pub const ALLOWED_SYSCALLS: &[i64] = &[
    // ============================================
    // io_uring 関連（monoio ランタイム必須）
    // ============================================
    425, // io_uring_setup
    426, // io_uring_enter
    427, // io_uring_register

    // ============================================
    // ファイル I/O（設定、証明書、ログ）
    // ============================================
    0,   // read
    1,   // write
    3,   // close
    5,   // fstat
    8,   // lseek
    17,  // pread64
    18,  // pwrite64
    19,  // readv
    20,  // writev
    40,  // sendfile (kTLS ゼロコピー転送)
    72,  // fcntl
    79,  // getcwd (canonicalize() で使用)
    89,  // readlink (canonicalize() で使用)
    257, // openat
    262, // newfstatat
    275, // splice (kTLS ゼロコピー転送)
    
    // ============================================
    // ネットワーク（TCP/UDP ソケット）
    // ============================================
    41,  // socket
    42,  // connect
    43,  // accept
    44,  // sendto
    45,  // recvfrom
    46,  // sendmsg
    47,  // recvmsg
    48,  // shutdown
    49,  // bind
    50,  // listen
    51,  // getsockname
    52,  // getpeername
    54,  // setsockopt
    55,  // getsockopt
    288, // accept4

    // ============================================
    // メモリ管理（mimalloc、Huge Pages、io_uring）
    // ============================================
    9,   // mmap
    10,  // mprotect
    11,  // munmap
    12,  // brk
    25,  // mremap (mimalloc)
    28,  // madvise (mimalloc)
    149, // mlock (io_uring 登録バッファ)
    150, // munlock
    151, // mlockall
    152, // munlockall
    325, // mlock2 (io_uring 登録バッファ)
    319, // memfd_create (HTTP/3 TLS証明書のLandlock対応用)

    // ============================================
    // スレッド・プロセス管理
    // ============================================
    24,  // sched_yield (スレッドスケジューリング)
    56,  // clone (スレッド作成)
    60,  // exit
    186, // gettid
    202, // futex (同期プリミティブ)
    203, // sched_setaffinity (CPUピンニング)
    204, // sched_getaffinity
    218, // set_tid_address
    231, // exit_group
    234, // tgkill
    273, // set_robust_list
    309, // getcpu
    334, // rseq (Restartable Sequences)
    435, // clone3

    // ============================================
    // シグナル処理（Graceful Shutdown、Hot Reload）
    // ============================================
    13,  // rt_sigaction
    14,  // rt_sigprocmask
    15,  // rt_sigreturn
    131, // sigaltstack

    // ============================================
    // ユーザー・権限管理（権限降格用）
    // ============================================
    102, // getuid
    104, // getgid
    105, // setuid
    106, // setgid
    107, // geteuid
    108, // getegid
    110, // getppid
    116, // setgroups
    
    // ============================================
    // 時間・タイマー
    // ============================================
    35,  // nanosleep
    228, // clock_gettime
    230, // clock_nanosleep

    // ============================================
    // その他必須
    // ============================================
    16,  // ioctl (ソケット、kTLS 設定)
    39,  // getpid
    63,  // uname (カーネルバージョン検出)
    147, // prctl (PR_SET_NAME、seccomp)
    158, // arch_prctl
    302, // prlimit64
    318, // getrandom (TLS 乱数生成)
    
    // ============================================
    // epoll (monoio フォールバック、io_uring 非対応環境)
    // ============================================
    213, // epoll_create (レガシー)
    232, // epoll_wait
    233, // epoll_ctl
    281, // epoll_pwait
    291, // epoll_create1 (EPOLL_CLOEXEC対応)

    // ============================================
    // ログファイル操作
    // ============================================
    74,  // fsync
    75,  // fdatasync
    77,  // ftruncate
    285, // fallocate

    // ============================================
    // イベント・タイマー（monoio io_uring ランタイム必須）
    // ============================================
    283, // timerfd_create (monoio enable_timer() 必須)
    286, // timerfd_settime
    287, // timerfd_gettime
    290, // eventfd2 (io_uring イベント通知)
    293, // pipe2 (内部通信)
];

/// 許可するシステムコールのリスト (aarch64)
#[cfg(target_arch = "aarch64")]
pub const ALLOWED_SYSCALLS: &[i64] = &[
    // ============================================
    // io_uring 関連（monoio ランタイム必須）
    // ============================================
    425, // io_uring_setup
    426, // io_uring_enter  
    427, // io_uring_register

    // ============================================
    // ファイル I/O
    // ============================================
    17,  // getcwd (canonicalize() で使用)
    56,  // openat
    57,  // close
    62,  // lseek
    63,  // read
    64,  // write
    65,  // readv
    66,  // writev
    67,  // pread64
    68,  // pwrite64
    71,  // sendfile (kTLS ゼロコピー転送)
    76,  // splice (kTLS ゼロコピー転送)
    78,  // readlinkat (canonicalize() で使用)
    79,  // fstatat
    80,  // fstat

    // ============================================
    // ネットワーク
    // ============================================
    198, // socket
    200, // bind
    201, // listen
    202, // accept
    203, // connect
    204, // getsockname
    205, // getpeername
    206, // sendto
    207, // recvfrom
    208, // setsockopt
    209, // getsockopt
    210, // shutdown
    211, // sendmsg
    212, // recvmsg
    242, // accept4

    // ============================================
    // メモリ管理
    // ============================================
    214, // brk
    215, // munmap
    222, // mmap
    226, // mprotect
    228, // mremap
    233, // madvise
    228, // mlock
    229, // munlock
    230, // mlockall
    231, // munlockall
    279, // memfd_create (HTTP/3 TLS証明書のLandlock対応用)

    // ============================================
    // スレッド・プロセス管理
    // ============================================
    93,  // exit
    94,  // exit_group
    96,  // set_tid_address
    98,  // futex
    99,  // set_robust_list
    122, // sched_setaffinity
    123, // sched_getaffinity
    131, // tgkill
    178, // gettid
    220, // clone
    435, // clone3

    // ============================================
    // シグナル処理
    // ============================================
    132, // sigaltstack
    134, // rt_sigaction
    135, // rt_sigprocmask
    139, // rt_sigreturn

    // ============================================
    // ユーザー・権限管理
    // ============================================
    146, // setresuid
    147, // getresuid
    148, // setresgid
    149, // getresgid
    172, // getpid
    173, // getppid
    174, // getuid
    175, // geteuid
    176, // getgid
    177, // getegid
    159, // setgroups

    // ============================================
    // 時間・タイマー
    // ============================================
    101, // nanosleep
    113, // clock_gettime
    115, // clock_nanosleep

    // ============================================
    // その他必須
    // ============================================
    25,  // fcntl
    29,  // ioctl
    63,  // uname
    160, // uname (alias)
    167, // prctl
    261, // prlimit64
    278, // getrandom

    // ============================================
    // epoll
    // ============================================
    20,  // epoll_create1
    21,  // epoll_ctl
    22,  // epoll_pwait

    // ============================================
    // ログファイル操作
    // ============================================
    82,  // fsync
    83,  // fdatasync
    46,  // ftruncate
    47,  // fallocate

    // ============================================
    // イベント・タイマー（monoio io_uring ランタイム必須）
    // ============================================
    85,  // timerfd_create (monoio enable_timer() 必須)
    86,  // timerfd_settime
    87,  // timerfd_gettime
    19,  // eventfd2 (io_uring イベント通知)
    59,  // pipe2 (内部通信)
];

// ====================
// セキュリティ設定
// ====================

/// seccomp モード
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SeccompMode {
    /// 無効
    #[default]
    Disabled,
    /// ログのみ（違反をログに記録、ブロックしない）
    Log,
    /// 厳格（違反したプロセスをSIGKILL）
    Strict,
    /// フィルタ（違反をEPERMで拒否）
    Filter,
}

impl SeccompMode {
    /// 文字列からSeccompModeを解析
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "disabled" | "off" | "none" => SeccompMode::Disabled,
            "log" | "audit" => SeccompMode::Log,
            "strict" | "kill" => SeccompMode::Strict,
            "filter" | "errno" | "deny" => SeccompMode::Filter,
            _ => {
                warn!("Unknown seccomp mode '{}', defaulting to disabled", s);
                SeccompMode::Disabled
            }
        }
    }
}

/// セキュリティ設定
#[derive(Debug, Clone)]
pub struct SecurityConfig {
    /// io_uring の操作制限を有効化
    /// 注意: monoio が低レベル API を公開していないため、現在は効果なし
    pub enable_io_uring_restrictions: bool,

    /// seccomp フィルタを有効化
    pub enable_seccomp: bool,

    /// seccomp モード
    pub seccomp_mode: SeccompMode,

    /// Landlock を有効化（Linux 5.13+）
    pub enable_landlock: bool,

    /// 読み取り専用ファイルシステムパス（Landlock用）
    pub landlock_read_paths: Vec<String>,

    /// 読み書き可能ファイルシステムパス（Landlock用）  
    pub landlock_write_paths: Vec<String>,
}

impl Default for SecurityConfig {
    /// デフォルトのセキュリティ設定
    /// 
    /// # 本番環境での推奨設定
    /// 
    /// ```toml
    /// [security]
    /// enable_seccomp = true
    /// seccomp_mode = "filter"
    /// enable_landlock = true
    /// landlock_read_paths = ["/etc/zerocopy-server", "/usr", "/lib", "/lib64"]
    /// landlock_write_paths = ["/var/log/zerocopy-server"]
    /// ```
    /// 
    /// # 導入時の推奨手順
    /// 
    /// 1. まず `seccomp_mode = "log"` で動作確認
    /// 2. `journalctl | grep seccomp` でブロックされるシステムコールを確認
    /// 3. 問題がなければ `seccomp_mode = "filter"` に変更
    fn default() -> Self {
        Self {
            enable_io_uring_restrictions: false,
            enable_seccomp: false,
            seccomp_mode: SeccompMode::Disabled,
            enable_landlock: false,
            landlock_read_paths: vec![
                "/etc".to_string(),
                "/usr".to_string(),
                "/lib".to_string(),
                "/lib64".to_string(),
            ],
            landlock_write_paths: vec![
                "/var/log".to_string(),
                "/tmp".to_string(),
            ],
        }
    }
}

// ====================
// カーネルバージョン検出
// ====================

/// カーネルバージョン情報
#[derive(Debug, Clone)]
pub struct KernelVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl KernelVersion {
    /// 現在のカーネルバージョンを取得
    pub fn current() -> io::Result<Self> {
        let uname = nix::sys::utsname::uname()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let release = uname.release().to_string_lossy();
        
        // パース: "5.15.0-generic" -> (5, 15, 0)
        let parts: Vec<&str> = release.split(|c: char| c == '.' || c == '-').collect();
        
        let major = parts.get(0)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let minor = parts.get(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let patch = parts.get(2)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        
        Ok(Self { major, minor, patch })
    }

    /// io_uring制限がサポートされているか (5.10+)
    pub fn supports_uring_restrictions(&self) -> bool {
        (self.major, self.minor) >= (5, 10)
    }

    /// seccompがサポートされているか (3.17+)
    pub fn supports_seccomp(&self) -> bool {
        (self.major, self.minor) >= (3, 17)
    }

    /// Landlockがサポートされているか (5.13+)
    pub fn supports_landlock(&self) -> bool {
        (self.major, self.minor) >= (5, 13)
    }
}

impl std::fmt::Display for KernelVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

// ====================
// セキュリティ制限の適用
// ====================

/// セキュリティ制限を適用
///
/// 権限降格後、ワーカースレッド起動前に呼び出すことを推奨。
///
/// # 適用順序
///
/// 1. io_uring制限（現在は未サポート）
/// 2. Landlockファイルシステム制限
/// 3. seccompシステムコール制限（最後に適用）
///
/// # エラー
///
/// 制限の適用に失敗した場合はエラーを返します。
/// ただし、機能がサポートされていない場合は警告を出力して続行します。
pub fn apply_security_restrictions(config: &SecurityConfig) -> io::Result<()> {
    let kernel = KernelVersion::current()?;
    info!("Kernel version: {} - Checking security feature support", kernel);

    // 1. io_uring制限
    if config.enable_io_uring_restrictions {
        if kernel.supports_uring_restrictions() {
            warn!("io_uring restrictions: Currently not supported with monoio");
            warn!("monoio does not expose low-level io_uring API (uring_fd/submitter)");
            warn!("Alternative: Use seccomp to limit system calls including io_uring operations");
        } else {
            warn!("io_uring restrictions require Linux 5.10+ (current: {})", kernel);
        }
    }

    // 2. Landlock
    if config.enable_landlock {
        if kernel.supports_landlock() {
            match apply_landlock(config) {
                Ok(()) => info!("Landlock filesystem restrictions applied"),
                Err(e) => {
                    warn!("Failed to apply Landlock: {} - continuing without it", e);
                }
            }
        } else {
            warn!("Landlock requires Linux 5.13+ (current: {})", kernel);
        }
    }

    // 3. seccomp（最後に適用 - 不可逆）
    if config.enable_seccomp && config.seccomp_mode != SeccompMode::Disabled {
        if kernel.supports_seccomp() {
            apply_seccomp(config)?;
            info!("seccomp filter applied (mode: {:?})", config.seccomp_mode);
        } else {
            warn!("seccomp requires Linux 3.17+ (current: {})", kernel);
        }
    }

    Ok(())
}

// ====================
// seccomp 実装
// ====================

/// seccompフィルタを適用
#[cfg(target_os = "linux")]
fn apply_seccomp(config: &SecurityConfig) -> io::Result<()> {
    // BPF プログラムを構築
    let filter = build_seccomp_filter(config.seccomp_mode)?;

    // PR_SET_NO_NEW_PRIVS を設定（seccomp前に必須）
    let ret = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    debug!("PR_SET_NO_NEW_PRIVS set successfully");

    // seccomp フィルタを適用
    let prog = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr() as *mut libc::sock_filter,
    };

    let ret = unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER,
            &prog as *const libc::sock_fprog,
            0,
            0,
        )
    };

    if ret != 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn apply_seccomp(_config: &SecurityConfig) -> io::Result<()> {
    warn!("seccomp is only available on Linux");
    Ok(())
}

/// BPF seccompフィルタを構築
#[cfg(target_os = "linux")]
fn build_seccomp_filter(mode: SeccompMode) -> io::Result<Vec<libc::sock_filter>> {
    // BPF 命令定数
    const BPF_LD: u16 = 0x00;
    const BPF_JMP: u16 = 0x05;
    const BPF_RET: u16 = 0x06;
    const BPF_W: u16 = 0x00;
    const BPF_ABS: u16 = 0x20;
    const BPF_JEQ: u16 = 0x10;
    const BPF_K: u16 = 0x00;

    // seccomp データオフセット
    const OFFSET_ARCH: u32 = 4;
    const OFFSET_NR: u32 = 0;

    // アーキテクチャ
    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xc000003e; // AUDIT_ARCH_X86_64

    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xc00000b7; // AUDIT_ARCH_AARCH64

    // アクション
    let action_allow = 0x7fff0000u32; // SECCOMP_RET_ALLOW
    let action_deny = match mode {
        SeccompMode::Log => 0x7ffc0000u32,     // SECCOMP_RET_LOG
        SeccompMode::Strict => 0x00000000u32,  // SECCOMP_RET_KILL_PROCESS
        SeccompMode::Filter => 0x00050001u32,  // SECCOMP_RET_ERRNO | EPERM
        SeccompMode::Disabled => 0x7fff0000u32, // SECCOMP_RET_ALLOW (no-op)
    };

    let mut filter = Vec::new();

    // 1. アーキテクチャチェック
    filter.push(libc::sock_filter {
        code: BPF_LD | BPF_W | BPF_ABS,
        jt: 0,
        jf: 0,
        k: OFFSET_ARCH,
    });
    filter.push(libc::sock_filter {
        code: BPF_JMP | BPF_JEQ | BPF_K,
        jt: 1, // 次の命令へ
        jf: 0, // 拒否へ
        k: AUDIT_ARCH,
    });
    // アーキテクチャ不一致 -> 拒否
    filter.push(libc::sock_filter {
        code: BPF_RET | BPF_K,
        jt: 0,
        jf: 0,
        k: action_deny,
    });

    // 2. システムコール番号を読み込み
    filter.push(libc::sock_filter {
        code: BPF_LD | BPF_W | BPF_ABS,
        jt: 0,
        jf: 0,
        k: OFFSET_NR,
    });

    // 3. 許可リストにあるシステムコールをチェック
    let syscall_count = ALLOWED_SYSCALLS.len();
    for (i, &syscall) in ALLOWED_SYSCALLS.iter().enumerate() {
        let remaining = syscall_count - i - 1;
        filter.push(libc::sock_filter {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: (remaining + 1) as u8, // 許可へジャンプ
            jf: 0, // 次のチェックへ
            k: syscall as u32,
        });
    }

    // 4. リストにない -> 拒否
    filter.push(libc::sock_filter {
        code: BPF_RET | BPF_K,
        jt: 0,
        jf: 0,
        k: action_deny,
    });

    // 5. 許可
    filter.push(libc::sock_filter {
        code: BPF_RET | BPF_K,
        jt: 0,
        jf: 0,
        k: action_allow,
    });

    debug!("Built seccomp filter with {} instructions for {} syscalls", 
           filter.len(), syscall_count);

    Ok(filter)
}

// ====================
// Landlock 実装
// ====================

/// Landlockファイルシステム制限を適用
/// 
/// Landlock ABI バージョン対応:
/// - ABI v1 (Linux 5.13+): 基本的なファイルシステムアクセス制御
/// - ABI v2 (Linux 5.19+): ファイル参照権限
/// - ABI v3 (Linux 6.2+):  TRUNCATE権限
/// - ABI v4 (Linux 6.7+):  ioctl権限
/// 
/// 利用可能な最高のABIバージョンを自動検出して使用します。
#[cfg(target_os = "linux")]
fn apply_landlock(config: &SecurityConfig) -> io::Result<()> {
    use std::os::unix::io::RawFd;

    // Landlock システムコール番号
    const LANDLOCK_CREATE_RULESET: i64 = 444;
    const LANDLOCK_ADD_RULE: i64 = 445;
    const LANDLOCK_RESTRICT_SELF: i64 = 446;

    // ============================================
    // Landlock アクセス権限 (ABI v1-v4)
    // ============================================
    
    // ABI v1 (Linux 5.13+) - 基本権限
    // 注: EXECUTE (1 << 0) はサーバーでは不要のため定義しない
    const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
    const LANDLOCK_ACCESS_FS_READ_FILE: u64 = 1 << 2;
    const LANDLOCK_ACCESS_FS_READ_DIR: u64 = 1 << 3;
    const LANDLOCK_ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
    const LANDLOCK_ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
    const LANDLOCK_ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
    const LANDLOCK_ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
    const LANDLOCK_ACCESS_FS_MAKE_REG: u64 = 1 << 8;
    const LANDLOCK_ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
    const LANDLOCK_ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
    const LANDLOCK_ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
    const LANDLOCK_ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
    
    // ABI v2 (Linux 5.19+) - ファイル参照権限
    const LANDLOCK_ACCESS_FS_REFER: u64 = 1 << 13;
    
    // ABI v3 (Linux 6.2+) - TRUNCATE権限
    const LANDLOCK_ACCESS_FS_TRUNCATE: u64 = 1 << 14;
    
    // ABI v4 (Linux 6.7+) - ioctl権限
    const LANDLOCK_ACCESS_FS_IOCTL_DEV: u64 = 1 << 15;

    // 作成/削除関連の全権限
    #[allow(dead_code)]
    const LANDLOCK_ACCESS_FS_MAKE_ALL: u64 = 
        LANDLOCK_ACCESS_FS_MAKE_CHAR | LANDLOCK_ACCESS_FS_MAKE_DIR |
        LANDLOCK_ACCESS_FS_MAKE_REG | LANDLOCK_ACCESS_FS_MAKE_SOCK |
        LANDLOCK_ACCESS_FS_MAKE_FIFO | LANDLOCK_ACCESS_FS_MAKE_BLOCK |
        LANDLOCK_ACCESS_FS_MAKE_SYM;

    // ruleset_attr 構造体
    #[repr(C)]
    struct LandlockRulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
    }

    // path_beneath_attr 構造体
    #[repr(C)]
    struct LandlockPathBeneathAttr {
        allowed_access: u64,
        parent_fd: RawFd,
    }

    const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;
    const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1 << 0;

    // ============================================
    // Landlock ABI バージョン検出
    // ============================================
    let abi_version: i32 = unsafe {
        libc::syscall(
            LANDLOCK_CREATE_RULESET,
            std::ptr::null::<LandlockRulesetAttr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        ) as i32
    };
    
    if abi_version < 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Landlock is not supported on this kernel"
        ));
    }
    
    info!("Landlock: ABI version {} detected", abi_version);

    // ABIバージョンに応じたアクセス権限を設定
    // 注意: ABIバージョンと権限の対応
    //   - ABI v1 (5.13+): 基本ファイルシステム権限
    //   - ABI v2 (5.19+): REFER権限追加
    //   - ABI v3 (6.2+):  TRUNCATE権限追加
    //   - ABI v4 (6.7+):  ネットワーク制限サポート（FSは変更なし）
    //   - ABI v5 (6.10+): IOCTL_DEV権限追加
    let (read_access, write_access) = match abi_version {
        1 => {
            // ABI v1: 基本権限のみ
            let read = LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_READ_DIR;
            let write = read | LANDLOCK_ACCESS_FS_WRITE_FILE | 
                        LANDLOCK_ACCESS_FS_REMOVE_FILE | LANDLOCK_ACCESS_FS_REMOVE_DIR |
                        LANDLOCK_ACCESS_FS_MAKE_REG | LANDLOCK_ACCESS_FS_MAKE_DIR;
            (read, write)
        }
        2 => {
            // ABI v2: REFER権限追加
            let read = LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_READ_DIR;
            let write = read | LANDLOCK_ACCESS_FS_WRITE_FILE | 
                        LANDLOCK_ACCESS_FS_REMOVE_FILE | LANDLOCK_ACCESS_FS_REMOVE_DIR |
                        LANDLOCK_ACCESS_FS_MAKE_REG | LANDLOCK_ACCESS_FS_MAKE_DIR |
                        LANDLOCK_ACCESS_FS_REFER;
            (read, write)
        }
        3 | 4 => {
            // ABI v3-v4: TRUNCATE権限追加
            // 注: ABI v4はネットワーク制限を追加するが、FS権限は変更なし
            let read = LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_READ_DIR;
            let write = read | LANDLOCK_ACCESS_FS_WRITE_FILE | 
                        LANDLOCK_ACCESS_FS_REMOVE_FILE | LANDLOCK_ACCESS_FS_REMOVE_DIR |
                        LANDLOCK_ACCESS_FS_MAKE_REG | LANDLOCK_ACCESS_FS_MAKE_DIR |
                        LANDLOCK_ACCESS_FS_REFER | LANDLOCK_ACCESS_FS_TRUNCATE;
            (read, write)
        }
        _ => {
            // ABI v5+: IOCTL_DEV権限追加（Linux 6.10+）
            let read = LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_READ_DIR;
            let write = read | LANDLOCK_ACCESS_FS_WRITE_FILE | 
                        LANDLOCK_ACCESS_FS_REMOVE_FILE | LANDLOCK_ACCESS_FS_REMOVE_DIR |
                        LANDLOCK_ACCESS_FS_MAKE_REG | LANDLOCK_ACCESS_FS_MAKE_DIR |
                        LANDLOCK_ACCESS_FS_REFER | LANDLOCK_ACCESS_FS_TRUNCATE |
                        LANDLOCK_ACCESS_FS_IOCTL_DEV;
            (read, write)
        }
    };

    // ルールセット作成
    let attr = LandlockRulesetAttr {
        handled_access_fs: write_access,
        handled_access_net: 0,
    };

    let ruleset_fd: RawFd = unsafe {
        libc::syscall(
            LANDLOCK_CREATE_RULESET,
            &attr as *const LandlockRulesetAttr,
            std::mem::size_of::<LandlockRulesetAttr>(),
            0u32,
        ) as RawFd
    };

    if ruleset_fd < 0 {
        return Err(io::Error::last_os_error());
    }

    // 読み取り専用パスのルール追加
    for path in &config.landlock_read_paths {
        // 空パスをスキップ
        if path.is_empty() {
            continue;
        }
        if let Ok(fd) = std::fs::File::open(path) {
            use std::os::unix::io::AsRawFd;
            let path_attr = LandlockPathBeneathAttr {
                allowed_access: read_access,
                parent_fd: fd.as_raw_fd(),
            };
            unsafe {
                libc::syscall(
                    LANDLOCK_ADD_RULE,
                    ruleset_fd,
                    LANDLOCK_RULE_PATH_BENEATH,
                    &path_attr as *const LandlockPathBeneathAttr,
                    0u32,
                );
            }
            debug!("Landlock: Added read-only rule for {}", path);
        }
    }

    // 読み書きパスのルール追加
    for path in &config.landlock_write_paths {
        // 空パスをスキップ
        if path.is_empty() {
            continue;
        }
        if let Ok(fd) = std::fs::File::open(path) {
            use std::os::unix::io::AsRawFd;
            let path_attr = LandlockPathBeneathAttr {
                allowed_access: write_access,
                parent_fd: fd.as_raw_fd(),
            };
            unsafe {
                libc::syscall(
                    LANDLOCK_ADD_RULE,
                    ruleset_fd,
                    LANDLOCK_RULE_PATH_BENEATH,
                    &path_attr as *const LandlockPathBeneathAttr,
                    0u32,
                );
            }
            debug!("Landlock: Added read-write rule for {}", path);
        }
    }

    // ルールセットを適用
    let ret = unsafe {
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)
    };
    if ret != 0 && unsafe { *libc::__errno_location() } != libc::EINVAL {
        // 既に設定済みの場合は無視
    }

    let ret = unsafe {
        libc::syscall(LANDLOCK_RESTRICT_SELF, ruleset_fd, 0u32)
    };

    unsafe { libc::close(ruleset_fd) };

    if ret < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn apply_landlock(_config: &SecurityConfig) -> io::Result<()> {
    warn!("Landlock is only available on Linux 5.13+");
    Ok(())
}

// ====================
// io_uring 制限（将来実装用）
// ====================

/// io_uring制限を適用する関数（現在は未実装）
/// 
/// monoioが以下のいずれかをサポートした場合に実装可能:
/// 1. io_uringのファイルディスクリプタへのアクセス
/// 2. RuntimeBuilderでの制限設定オプション
/// 3. io-uringクレートの直接使用オプション
///
/// # 将来の実装例
///
/// ```rust,ignore
/// use io_uring::{IoUring, register::Restriction};
///
/// fn apply_uring_restrictions(ring: &IoUring) -> io::Result<()> {
///     let restrictions = [
///         Restriction::allow_opcode(io_uring::opcode::Read::CODE),
///         Restriction::allow_opcode(io_uring::opcode::Write::CODE),
///         Restriction::allow_opcode(io_uring::opcode::Accept::CODE),
///         Restriction::allow_opcode(io_uring::opcode::Connect::CODE),
///         Restriction::allow_opcode(io_uring::opcode::Send::CODE),
///         Restriction::allow_opcode(io_uring::opcode::Recv::CODE),
///         Restriction::allow_opcode(io_uring::opcode::Close::CODE),
///     ];
///     ring.submitter().register_restrictions(&restrictions)?;
///     Ok(())
/// }
/// ```
#[allow(dead_code)]
pub fn apply_io_uring_restrictions() -> io::Result<()> {
    error!("io_uring restrictions cannot be applied directly with monoio");
    error!("monoio abstracts io_uring and does not expose the underlying uring_fd");
    error!("Use seccomp instead to restrict io_uring operations at the syscall level");
    
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "io_uring restrictions not supported with monoio - use seccomp instead",
    ))
}

// ====================
// セキュリティ状態レポート
// ====================

/// セキュリティ機能の状態をレポート
pub fn report_security_status() {
    match KernelVersion::current() {
        Ok(kernel) => {
            info!("=== Security Feature Support ===");
            info!("Kernel version: {}", kernel);
            info!("io_uring restrictions (5.10+): {}", 
                  if kernel.supports_uring_restrictions() { "Available" } else { "Not available" });
            info!("seccomp (3.17+): {}", 
                  if kernel.supports_seccomp() { "Available" } else { "Not available" });
            info!("Landlock (5.13+): {}", 
                  if kernel.supports_landlock() { "Available" } else { "Not available" });
            info!("=================================");
        }
        Err(e) => {
            warn!("Failed to detect kernel version: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kernel_version_parse() {
        let kernel = KernelVersion {
            major: 5,
            minor: 15,
            patch: 0,
        };
        assert!(kernel.supports_uring_restrictions());
        assert!(kernel.supports_seccomp());
        assert!(kernel.supports_landlock());
    }

    #[test]
    fn test_seccomp_mode_parse() {
        assert_eq!(SeccompMode::from_str("disabled"), SeccompMode::Disabled);
        assert_eq!(SeccompMode::from_str("log"), SeccompMode::Log);
        assert_eq!(SeccompMode::from_str("strict"), SeccompMode::Strict);
        assert_eq!(SeccompMode::from_str("filter"), SeccompMode::Filter);
        assert_eq!(SeccompMode::from_str("unknown"), SeccompMode::Disabled);
    }

    #[test]
    fn test_default_security_config() {
        let config = SecurityConfig::default();
        assert!(!config.enable_io_uring_restrictions);
        assert!(!config.enable_seccomp);
        assert_eq!(config.seccomp_mode, SeccompMode::Disabled);
    }
}

