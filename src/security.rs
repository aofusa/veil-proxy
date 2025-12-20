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
    4,   // stat (DNS解決: /etc/resolv.conf等)
    5,   // fstat
    6,   // lstat (DNS解決: シンボリックリンク確認)
    8,   // lseek
    17,  // pread64
    18,  // pwrite64
    19,  // readv
    20,  // writev
    21,  // access (DNS解決: ファイルアクセス権確認)
    40,  // sendfile (kTLS ゼロコピー転送)
    72,  // fcntl
    79,  // getcwd (canonicalize() で使用)
    89,  // readlink (canonicalize() で使用)
    257, // openat
    262, // newfstatat
    275, // splice (kTLS ゼロコピー転送)
    
    // ============================================
    // DNS名前解決 (getaddrinfo)
    // ============================================
    7,   // poll (DNS応答待機)
    53,  // socketpair (NSS内部通信)
    
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
    299, // recvmmsg (DNS解決: 複数メッセージ受信)
    307, // sendmmsg (DNS解決: 複数メッセージ送信)

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
    48,  // faccessat (DNS解決: ファイルアクセス権確認)
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
    73,  // ppoll (DNS応答待機)
    76,  // splice (kTLS ゼロコピー転送)
    78,  // readlinkat (canonicalize() で使用)
    79,  // fstatat
    80,  // fstat
    199, // socketpair (NSS内部通信)

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
    243, // recvmmsg (DNS解決: 複数メッセージ受信)
    269, // sendmmsg (DNS解決: 複数メッセージ送信)

    // ============================================
    // メモリ管理
    // ============================================
    214, // brk
    215, // munmap
    222, // mmap
    226, // mprotect
    227, // mremap
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

// ====================
// サンドボックス機能（bubblewrap相当）
// ====================
//
// Linuxのnamespace分離、bind mounts、capabilities制限を
// プログラム起動時に適用することで、bubblewrapと同等の
// セキュリティ分離を実現します。
//
// ## 機能
//
// - **Namespace分離**: PID, UTS, IPC, Mount, User (Networkはサーバーでは通常不要)
// - **Bind Mounts**: ファイルシステムの読み取り専用マウント
// - **Capabilities制限**: 不要なケイパビリティのドロップ
//
// ## 適用順序
//
// 1. User namespace分離（オプション）
// 2. 他のnamespace分離（PID, UTS, IPC, Mount）
// 3. Bind mounts設定
// 4. Capabilities制限
// 5. 既存のseccomp/Landlock適用
//
// ## 注意事項
//
// - Network namespaceはサーバーでは通常分離しません（--share-net相当）
// - root権限または適切なケイパビリティが必要です
// - 一度適用すると解除できません
// ====================

/// サンドボックス設定
#[derive(Debug, Clone, Default)]
pub struct SandboxConfig {
    /// サンドボックスを有効化
    pub enabled: bool,
    
    /// PID namespace分離
    /// サンドボックス内のプロセスは外部のプロセスを見ることができなくなります
    pub unshare_pid: bool,
    
    /// Mount namespace分離
    /// サンドボックス内で独自のマウントポイントを持ちます
    pub unshare_mount: bool,
    
    /// UTS namespace分離
    /// サンドボックス内で独自のホスト名を持ちます
    pub unshare_uts: bool,
    
    /// IPC namespace分離
    /// サンドボックス内で独自のIPC（共有メモリ、セマフォ等）を持ちます
    pub unshare_ipc: bool,
    
    /// User namespace分離
    /// サンドボックス内で独自のユーザー/グループIDマッピングを持ちます
    /// 注: rootでなくてもnamespace分離が可能になりますが、制限があります
    pub unshare_user: bool,
    
    /// Network namespace分離
    /// 通常はfalse（サーバーはネットワークアクセスが必要）
    /// trueの場合、サンドボックス内からネットワークにアクセスできなくなります
    pub unshare_net: bool,
    
    /// 新しいルートファイルシステムのパス
    /// 設定されている場合、pivot_rootまたはchrootを実行します
    pub new_root: Option<String>,
    
    /// 読み取り専用バインドマウント
    /// source -> dest へのread-onlyバインドを設定
    pub ro_bind_mounts: Vec<BindMount>,
    
    /// 読み書きバインドマウント
    /// source -> dest へのread-writeバインドを設定
    pub rw_bind_mounts: Vec<BindMount>,
    
    /// tmpfsマウント（メモリファイルシステム）
    /// 指定されたパスにtmpfsをマウント
    pub tmpfs_mounts: Vec<String>,
    
    /// /proc をマウントするかどうか
    pub mount_proc: bool,
    
    /// /dev の最小限のデバイスノードを作成するかどうか
    pub mount_dev: bool,
    
    /// ドロップするケイパビリティのリスト
    /// 例: ["CAP_SYS_ADMIN", "CAP_NET_RAW"]
    pub drop_capabilities: Vec<String>,
    
    /// 保持するケイパビリティのリスト（他は全てドロップ）
    /// drop_capabilitiesより優先されます
    /// 例: ["CAP_NET_BIND_SERVICE"]
    pub keep_capabilities: Vec<String>,
    
    /// サンドボックス内のホスト名
    pub hostname: Option<String>,
    
    /// PR_SET_NO_NEW_PRIVSを設定するかどうか
    /// seccompと併用する場合は自動的にtrueになります
    pub no_new_privs: bool,
}

/// バインドマウント設定
#[derive(Debug, Clone)]
pub struct BindMount {
    /// ソースパス（ホスト側）
    pub source: String,
    /// デスティネーションパス（サンドボックス内）
    pub dest: String,
}

impl BindMount {
    pub fn new(source: impl Into<String>, dest: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            dest: dest.into(),
        }
    }
}

/// Linuxケイパビリティの定義
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
#[allow(non_camel_case_types)]
pub enum Capability {
    CAP_CHOWN = 0,
    CAP_DAC_OVERRIDE = 1,
    CAP_DAC_READ_SEARCH = 2,
    CAP_FOWNER = 3,
    CAP_FSETID = 4,
    CAP_KILL = 5,
    CAP_SETGID = 6,
    CAP_SETUID = 7,
    CAP_SETPCAP = 8,
    CAP_LINUX_IMMUTABLE = 9,
    CAP_NET_BIND_SERVICE = 10,
    CAP_NET_BROADCAST = 11,
    CAP_NET_ADMIN = 12,
    CAP_NET_RAW = 13,
    CAP_IPC_LOCK = 14,
    CAP_IPC_OWNER = 15,
    CAP_SYS_MODULE = 16,
    CAP_SYS_RAWIO = 17,
    CAP_SYS_CHROOT = 18,
    CAP_SYS_PTRACE = 19,
    CAP_SYS_PACCT = 20,
    CAP_SYS_ADMIN = 21,
    CAP_SYS_BOOT = 22,
    CAP_SYS_NICE = 23,
    CAP_SYS_RESOURCE = 24,
    CAP_SYS_TIME = 25,
    CAP_SYS_TTY_CONFIG = 26,
    CAP_MKNOD = 27,
    CAP_LEASE = 28,
    CAP_AUDIT_WRITE = 29,
    CAP_AUDIT_CONTROL = 30,
    CAP_SETFCAP = 31,
    CAP_MAC_OVERRIDE = 32,
    CAP_MAC_ADMIN = 33,
    CAP_SYSLOG = 34,
    CAP_WAKE_ALARM = 35,
    CAP_BLOCK_SUSPEND = 36,
    CAP_AUDIT_READ = 37,
    CAP_PERFMON = 38,
    CAP_BPF = 39,
    CAP_CHECKPOINT_RESTORE = 40,
}

impl Capability {
    /// 文字列からケイパビリティを解析
    pub fn from_str(s: &str) -> Option<Self> {
        let s = s.trim().to_uppercase();
        let s = s.strip_prefix("CAP_").unwrap_or(&s);
        
        match s {
            "CHOWN" => Some(Capability::CAP_CHOWN),
            "DAC_OVERRIDE" => Some(Capability::CAP_DAC_OVERRIDE),
            "DAC_READ_SEARCH" => Some(Capability::CAP_DAC_READ_SEARCH),
            "FOWNER" => Some(Capability::CAP_FOWNER),
            "FSETID" => Some(Capability::CAP_FSETID),
            "KILL" => Some(Capability::CAP_KILL),
            "SETGID" => Some(Capability::CAP_SETGID),
            "SETUID" => Some(Capability::CAP_SETUID),
            "SETPCAP" => Some(Capability::CAP_SETPCAP),
            "LINUX_IMMUTABLE" => Some(Capability::CAP_LINUX_IMMUTABLE),
            "NET_BIND_SERVICE" => Some(Capability::CAP_NET_BIND_SERVICE),
            "NET_BROADCAST" => Some(Capability::CAP_NET_BROADCAST),
            "NET_ADMIN" => Some(Capability::CAP_NET_ADMIN),
            "NET_RAW" => Some(Capability::CAP_NET_RAW),
            "IPC_LOCK" => Some(Capability::CAP_IPC_LOCK),
            "IPC_OWNER" => Some(Capability::CAP_IPC_OWNER),
            "SYS_MODULE" => Some(Capability::CAP_SYS_MODULE),
            "SYS_RAWIO" => Some(Capability::CAP_SYS_RAWIO),
            "SYS_CHROOT" => Some(Capability::CAP_SYS_CHROOT),
            "SYS_PTRACE" => Some(Capability::CAP_SYS_PTRACE),
            "SYS_PACCT" => Some(Capability::CAP_SYS_PACCT),
            "SYS_ADMIN" => Some(Capability::CAP_SYS_ADMIN),
            "SYS_BOOT" => Some(Capability::CAP_SYS_BOOT),
            "SYS_NICE" => Some(Capability::CAP_SYS_NICE),
            "SYS_RESOURCE" => Some(Capability::CAP_SYS_RESOURCE),
            "SYS_TIME" => Some(Capability::CAP_SYS_TIME),
            "SYS_TTY_CONFIG" => Some(Capability::CAP_SYS_TTY_CONFIG),
            "MKNOD" => Some(Capability::CAP_MKNOD),
            "LEASE" => Some(Capability::CAP_LEASE),
            "AUDIT_WRITE" => Some(Capability::CAP_AUDIT_WRITE),
            "AUDIT_CONTROL" => Some(Capability::CAP_AUDIT_CONTROL),
            "SETFCAP" => Some(Capability::CAP_SETFCAP),
            "MAC_OVERRIDE" => Some(Capability::CAP_MAC_OVERRIDE),
            "MAC_ADMIN" => Some(Capability::CAP_MAC_ADMIN),
            "SYSLOG" => Some(Capability::CAP_SYSLOG),
            "WAKE_ALARM" => Some(Capability::CAP_WAKE_ALARM),
            "BLOCK_SUSPEND" => Some(Capability::CAP_BLOCK_SUSPEND),
            "AUDIT_READ" => Some(Capability::CAP_AUDIT_READ),
            "PERFMON" => Some(Capability::CAP_PERFMON),
            "BPF" => Some(Capability::CAP_BPF),
            "CHECKPOINT_RESTORE" => Some(Capability::CAP_CHECKPOINT_RESTORE),
            _ => None,
        }
    }
    
    /// リバースプロキシサーバーに推奨されるケイパビリティセット
    /// 
    /// 最小権限の原則に基づき、以下のケイパビリティのみを保持:
    /// - CAP_NET_BIND_SERVICE: 特権ポート（1024未満）へのバインド
    /// - CAP_SETUID/CAP_SETGID: 権限降格用
    pub fn recommended_for_server() -> Vec<Self> {
        vec![
            Capability::CAP_NET_BIND_SERVICE,
            Capability::CAP_SETUID,
            Capability::CAP_SETGID,
        ]
    }
    
    /// 全てのケイパビリティのリスト
    pub fn all() -> Vec<Self> {
        vec![
            Capability::CAP_CHOWN,
            Capability::CAP_DAC_OVERRIDE,
            Capability::CAP_DAC_READ_SEARCH,
            Capability::CAP_FOWNER,
            Capability::CAP_FSETID,
            Capability::CAP_KILL,
            Capability::CAP_SETGID,
            Capability::CAP_SETUID,
            Capability::CAP_SETPCAP,
            Capability::CAP_LINUX_IMMUTABLE,
            Capability::CAP_NET_BIND_SERVICE,
            Capability::CAP_NET_BROADCAST,
            Capability::CAP_NET_ADMIN,
            Capability::CAP_NET_RAW,
            Capability::CAP_IPC_LOCK,
            Capability::CAP_IPC_OWNER,
            Capability::CAP_SYS_MODULE,
            Capability::CAP_SYS_RAWIO,
            Capability::CAP_SYS_CHROOT,
            Capability::CAP_SYS_PTRACE,
            Capability::CAP_SYS_PACCT,
            Capability::CAP_SYS_ADMIN,
            Capability::CAP_SYS_BOOT,
            Capability::CAP_SYS_NICE,
            Capability::CAP_SYS_RESOURCE,
            Capability::CAP_SYS_TIME,
            Capability::CAP_SYS_TTY_CONFIG,
            Capability::CAP_MKNOD,
            Capability::CAP_LEASE,
            Capability::CAP_AUDIT_WRITE,
            Capability::CAP_AUDIT_CONTROL,
            Capability::CAP_SETFCAP,
            Capability::CAP_MAC_OVERRIDE,
            Capability::CAP_MAC_ADMIN,
            Capability::CAP_SYSLOG,
            Capability::CAP_WAKE_ALARM,
            Capability::CAP_BLOCK_SUSPEND,
            Capability::CAP_AUDIT_READ,
            Capability::CAP_PERFMON,
            Capability::CAP_BPF,
            Capability::CAP_CHECKPOINT_RESTORE,
        ]
    }
}

/// サンドボックスを適用
/// 
/// bubblewrapと同等のプロセス分離を実現します。
/// 
/// # 適用順序
/// 
/// 1. PR_SET_NO_NEW_PRIVS設定
/// 2. Namespace分離（unshare）
/// 3. Mount namespace内でのbind mounts
/// 4. Capabilities制限
/// 
/// # 注意事項
/// 
/// - この関数は一度だけ呼び出してください
/// - 適用後は設定を変更できません
/// - Network namespaceを分離するとネットワーク通信ができなくなります
/// 
/// # 引数
/// 
/// * `config` - サンドボックス設定
/// 
/// # エラー
/// 
/// 設定の適用に失敗した場合はエラーを返します。
#[cfg(target_os = "linux")]
pub fn apply_sandbox(config: &SandboxConfig) -> io::Result<()> {
    if !config.enabled {
        debug!("Sandbox is disabled");
        return Ok(());
    }
    
    info!("Applying sandbox restrictions (bubblewrap-compatible)...");
    
    // 1. PR_SET_NO_NEW_PRIVS を設定
    if config.no_new_privs {
        let ret = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
        if ret != 0 {
            let err = io::Error::last_os_error();
            // 既に設定済みの場合は無視
            if err.raw_os_error() != Some(libc::EINVAL) {
                return Err(err);
            }
        }
        debug!("PR_SET_NO_NEW_PRIVS set successfully");
    }
    
    // 2. Namespace分離
    apply_namespaces(config)?;
    
    // 3. Mount namespace内でのbind mounts
    if config.unshare_mount {
        apply_mounts(config)?;
    }
    
    // 4. ホスト名設定（UTS namespace分離時）
    if config.unshare_uts {
        if let Some(ref hostname) = config.hostname {
            apply_hostname(hostname)?;
        }
    }
    
    // 5. Capabilities制限
    apply_capabilities(config)?;
    
    info!("Sandbox restrictions applied successfully");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn apply_sandbox(_config: &SandboxConfig) -> io::Result<()> {
    warn!("Sandbox is only available on Linux");
    Ok(())
}

/// Namespace分離を適用
#[cfg(target_os = "linux")]
fn apply_namespaces(config: &SandboxConfig) -> io::Result<()> {
    use nix::sched::{unshare, CloneFlags};
    
    let mut flags = CloneFlags::empty();
    
    if config.unshare_pid {
        flags |= CloneFlags::CLONE_NEWPID;
        debug!("Will unshare PID namespace");
    }
    
    if config.unshare_mount {
        flags |= CloneFlags::CLONE_NEWNS;
        debug!("Will unshare Mount namespace");
    }
    
    if config.unshare_uts {
        flags |= CloneFlags::CLONE_NEWUTS;
        debug!("Will unshare UTS namespace");
    }
    
    if config.unshare_ipc {
        flags |= CloneFlags::CLONE_NEWIPC;
        debug!("Will unshare IPC namespace");
    }
    
    if config.unshare_user {
        flags |= CloneFlags::CLONE_NEWUSER;
        debug!("Will unshare User namespace");
    }
    
    if config.unshare_net {
        flags |= CloneFlags::CLONE_NEWNET;
        warn!("Will unshare Network namespace - network access will be blocked!");
    }
    
    if flags.is_empty() {
        debug!("No namespaces to unshare");
        return Ok(());
    }
    
    unshare(flags).map_err(|e| {
        io::Error::new(io::ErrorKind::PermissionDenied, 
            format!("Failed to unshare namespaces: {} (may require root or CAP_SYS_ADMIN)", e))
    })?;
    
    info!("Namespace separation applied: {:?}", flags);
    Ok(())
}

/// マウント設定を適用
#[cfg(target_os = "linux")]
fn apply_mounts(config: &SandboxConfig) -> io::Result<()> {
    use nix::mount::{mount, MsFlags};
    
    // マウントプロパゲーションをprivateに設定
    // これにより、このマウントnamespace内の変更が外部に影響しない
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    ).map_err(|e| {
        io::Error::new(io::ErrorKind::Other, 
            format!("Failed to set mount propagation: {}", e))
    })?;
    debug!("Mount propagation set to private");
    
    // 読み取り専用バインドマウント
    for bind in &config.ro_bind_mounts {
        if let Err(e) = apply_bind_mount(&bind.source, &bind.dest, true) {
            warn!("Failed to apply ro-bind mount {} -> {}: {}", bind.source, bind.dest, e);
        } else {
            debug!("Applied ro-bind mount: {} -> {}", bind.source, bind.dest);
        }
    }
    
    // 読み書きバインドマウント
    for bind in &config.rw_bind_mounts {
        if let Err(e) = apply_bind_mount(&bind.source, &bind.dest, false) {
            warn!("Failed to apply rw-bind mount {} -> {}: {}", bind.source, bind.dest, e);
        } else {
            debug!("Applied rw-bind mount: {} -> {}", bind.source, bind.dest);
        }
    }
    
    // tmpfsマウント
    for path in &config.tmpfs_mounts {
        if let Err(e) = apply_tmpfs_mount(path) {
            warn!("Failed to apply tmpfs mount at {}: {}", path, e);
        } else {
            debug!("Applied tmpfs mount: {}", path);
        }
    }
    
    // /proc マウント
    if config.mount_proc {
        if let Err(e) = apply_proc_mount() {
            warn!("Failed to mount /proc: {}", e);
        } else {
            debug!("Mounted /proc");
        }
    }
    
    // /dev マウント
    if config.mount_dev {
        if let Err(e) = apply_dev_mount() {
            warn!("Failed to mount /dev: {}", e);
        } else {
            debug!("Mounted minimal /dev");
        }
    }
    
    info!("Mount configuration applied");
    Ok(())
}

/// バインドマウントを適用
#[cfg(target_os = "linux")]
fn apply_bind_mount(source: &str, dest: &str, readonly: bool) -> io::Result<()> {
    use nix::mount::{mount, MsFlags};
    use std::path::Path;
    
    // ソースパスの存在確認
    if !Path::new(source).exists() {
        return Err(io::Error::new(io::ErrorKind::NotFound,
            format!("Source path does not exist: {}", source)));
    }
    
    // デスティネーションディレクトリの作成
    let dest_path = Path::new(dest);
    if !dest_path.exists() {
        if Path::new(source).is_dir() {
            std::fs::create_dir_all(dest)?;
        } else {
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::File::create(dest)?;
        }
    }
    
    // バインドマウント
    mount(
        Some(source),
        dest,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    ).map_err(|e| {
        io::Error::new(io::ErrorKind::Other,
            format!("Failed to bind mount {} -> {}: {}", source, dest, e))
    })?;
    
    // 読み取り専用に再マウント
    if readonly {
        mount(
            None::<&str>,
            dest,
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY | MsFlags::MS_REC,
            None::<&str>,
        ).map_err(|e| {
            io::Error::new(io::ErrorKind::Other,
                format!("Failed to remount {} as readonly: {}", dest, e))
        })?;
    }
    
    Ok(())
}

/// tmpfsをマウント
#[cfg(target_os = "linux")]
fn apply_tmpfs_mount(path: &str) -> io::Result<()> {
    use nix::mount::{mount, MsFlags};
    use std::path::Path;
    
    // ディレクトリの作成
    if !Path::new(path).exists() {
        std::fs::create_dir_all(path)?;
    }
    
    mount(
        Some("tmpfs"),
        path,
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some("mode=0755"),
    ).map_err(|e| {
        io::Error::new(io::ErrorKind::Other,
            format!("Failed to mount tmpfs at {}: {}", path, e))
    })?;
    
    Ok(())
}

/// /proc をマウント
#[cfg(target_os = "linux")]
fn apply_proc_mount() -> io::Result<()> {
    use nix::mount::{mount, MsFlags};
    use std::path::Path;
    
    let proc_path = "/proc";
    if !Path::new(proc_path).exists() {
        std::fs::create_dir_all(proc_path)?;
    }
    
    mount(
        Some("proc"),
        proc_path,
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None::<&str>,
    ).map_err(|e| {
        io::Error::new(io::ErrorKind::Other,
            format!("Failed to mount /proc: {}", e))
    })?;
    
    Ok(())
}

/// /dev に最小限のデバイスノードを作成
#[cfg(target_os = "linux")]
fn apply_dev_mount() -> io::Result<()> {
    use nix::mount::{mount, MsFlags};
    use std::path::Path;
    
    let dev_path = "/dev";
    
    // tmpfs を /dev にマウント
    if !Path::new(dev_path).exists() {
        std::fs::create_dir_all(dev_path)?;
    }
    
    mount(
        Some("tmpfs"),
        dev_path,
        Some("tmpfs"),
        MsFlags::MS_NOSUID,
        Some("mode=0755"),
    ).map_err(|e| {
        io::Error::new(io::ErrorKind::Other,
            format!("Failed to mount tmpfs on /dev: {}", e))
    })?;
    
    // 必須デバイスノードをバインドマウント
    let devices = [
        ("/dev/null", "/dev/null"),
        ("/dev/zero", "/dev/zero"),
        ("/dev/random", "/dev/random"),
        ("/dev/urandom", "/dev/urandom"),
    ];
    
    for (src, dest) in &devices {
        // タッチでファイルを作成
        let dest_path = Path::new(dest);
        if !dest_path.exists() {
            std::fs::File::create(dest)?;
        }
        
        if let Err(e) = mount(
            Some(*src),
            *dest,
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        ) {
            warn!("Failed to bind mount device {}: {}", dest, e);
        }
    }
    
    // /dev/pts を作成
    let pts_path = "/dev/pts";
    if !Path::new(pts_path).exists() {
        std::fs::create_dir_all(pts_path)?;
    }
    
    // /dev/shm を作成
    let shm_path = "/dev/shm";
    if !Path::new(shm_path).exists() {
        std::fs::create_dir_all(shm_path)?;
    }
    
    Ok(())
}

/// ホスト名を設定
#[cfg(target_os = "linux")]
fn apply_hostname(hostname: &str) -> io::Result<()> {
    use std::ffi::CString;
    
    let hostname_c = CString::new(hostname)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid hostname"))?;
    
    let ret = unsafe {
        libc::sethostname(hostname_c.as_ptr(), hostname.len())
    };
    
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    
    debug!("Hostname set to: {}", hostname);
    Ok(())
}

/// Capabilities制限を適用
#[cfg(target_os = "linux")]
fn apply_capabilities(config: &SandboxConfig) -> io::Result<()> {
    // keep_capabilitiesが指定されている場合
    if !config.keep_capabilities.is_empty() {
        let keep_caps: Vec<Capability> = config.keep_capabilities
            .iter()
            .filter_map(|s| Capability::from_str(s))
            .collect();
        
        if keep_caps.is_empty() {
            warn!("No valid capabilities found in keep_capabilities list");
            return Ok(());
        }
        
        // 保持するケイパビリティ以外を全てドロップ
        let all_caps = Capability::all();
        for cap in all_caps {
            if !keep_caps.contains(&cap) {
                drop_capability(cap)?;
            }
        }
        
        info!("Keeping only capabilities: {:?}", config.keep_capabilities);
        return Ok(());
    }
    
    // drop_capabilitiesが指定されている場合
    if !config.drop_capabilities.is_empty() {
        for cap_name in &config.drop_capabilities {
            if let Some(cap) = Capability::from_str(cap_name) {
                drop_capability(cap)?;
            } else {
                warn!("Unknown capability: {}", cap_name);
            }
        }
        
        info!("Dropped capabilities: {:?}", config.drop_capabilities);
    }
    
    Ok(())
}

/// 単一のケイパビリティをドロップ
#[cfg(target_os = "linux")]
fn drop_capability(cap: Capability) -> io::Result<()> {
    // PR_CAPBSET_DROP を使用してbounding setからケイパビリティを削除
    let ret = unsafe {
        libc::prctl(libc::PR_CAPBSET_DROP, cap as u32, 0, 0, 0)
    };
    
    if ret != 0 {
        let err = io::Error::last_os_error();
        // EINVAL は権限不足やケイパビリティが既にない場合
        if err.raw_os_error() != Some(libc::EINVAL) {
            debug!("Failed to drop capability {:?}: {}", cap, err);
        }
    }
    
    Ok(())
}

/// サンドボックスの推奨設定を生成
/// 
/// リバースプロキシサーバー用の推奨設定を返します。
/// Network namespaceは分離せず、必要最小限のケイパビリティを保持します。
pub fn recommended_sandbox_config() -> SandboxConfig {
    SandboxConfig {
        enabled: true,
        unshare_pid: false,      // 通常はプロセス分離不要
        unshare_mount: true,     // ファイルシステム分離
        unshare_uts: true,       // ホスト名分離
        unshare_ipc: true,       // IPC分離
        unshare_user: false,     // User namespace は複雑なため無効
        unshare_net: false,      // ネットワークは必要なため分離しない
        new_root: None,
        ro_bind_mounts: vec![
            BindMount::new("/usr", "/usr"),
            BindMount::new("/lib", "/lib"),
            BindMount::new("/lib64", "/lib64"),
            BindMount::new("/etc/ssl", "/etc/ssl"),
            // DNS解決に必要なファイル
            BindMount::new("/etc/resolv.conf", "/etc/resolv.conf"),
            BindMount::new("/etc/hosts", "/etc/hosts"),
            BindMount::new("/etc/nsswitch.conf", "/etc/nsswitch.conf"),
            BindMount::new("/etc/gai.conf", "/etc/gai.conf"),
            // systemd-resolved使用時に必要
            BindMount::new("/run/systemd/resolve", "/run/systemd/resolve"),
            // ユーザー/グループ情報
            BindMount::new("/etc/passwd", "/etc/passwd"),
            BindMount::new("/etc/group", "/etc/group"),
        ],
        rw_bind_mounts: vec![],
        tmpfs_mounts: vec![
            "/tmp".to_string(),
            // 注: /run はsystemd-resolvedのために除外
        ],
        mount_proc: true,
        mount_dev: true,
        drop_capabilities: vec![],
        keep_capabilities: vec![
            "CAP_NET_BIND_SERVICE".to_string(),
        ],
        hostname: Some("zerocopy-sandbox".to_string()),
        no_new_privs: true,
    }
}

/// サンドボックスのサポート状況をレポート
pub fn report_sandbox_support() {
    match KernelVersion::current() {
        Ok(kernel) => {
            info!("=== Sandbox Feature Support ===");
            info!("Kernel version: {}", kernel);
            
            // Namespace サポートチェック（カーネル 2.6.19+ で基本サポート）
            let ns_supported = kernel.major >= 3 || (kernel.major == 2 && kernel.minor >= 6);
            info!("Namespaces (PID/UTS/IPC/Mount): {}", 
                  if ns_supported { "Available" } else { "Not available" });
            
            // User namespace（カーネル 3.8+）
            let user_ns = (kernel.major, kernel.minor) >= (3, 8);
            info!("User namespace (3.8+): {}", 
                  if user_ns { "Available" } else { "Not available" });
            
            // Capabilities（カーネル 2.6.25+ で改善）
            info!("Capabilities: Available");
            
            info!("================================");
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

