//! io_uring リング管理
//!
//! io_uring_setup / io_uring_enter / io_uring_register の raw syscall ラッパーと
//! SQE/CQE の低レベル操作を提供する。
//!
//! ## 参考
//! - Linux kernel docs: Documentation/block/io-uring.rst
//! - man io_uring_setup(2), io_uring_enter(2), io_uring_register(2)

#![allow(dead_code)]

use std::io;
use std::sync::atomic::{AtomicU32, Ordering};

// ====================
// io_uring syscall 番号
// ====================

#[cfg(target_arch = "x86_64")]
pub const SYS_IO_URING_SETUP: i64 = 425;
#[cfg(target_arch = "x86_64")]
pub const SYS_IO_URING_ENTER: i64 = 426;
#[cfg(target_arch = "x86_64")]
pub const SYS_IO_URING_REGISTER: i64 = 427;

#[cfg(target_arch = "aarch64")]
pub const SYS_IO_URING_SETUP: i64 = 425;
#[cfg(target_arch = "aarch64")]
pub const SYS_IO_URING_ENTER: i64 = 426;
#[cfg(target_arch = "aarch64")]
pub const SYS_IO_URING_REGISTER: i64 = 427;

// ====================
// io_uring_setup フラグ
// ====================

/// IOPOLL: Busyポーリングモード（NVMe等の高速ストレージ向け）
pub const IORING_SETUP_IOPOLL: u32 = 1 << 0;
/// SQPOLL: SQ ポーリングスレッドを使用（カーネルがSQEを自動消費）
pub const IORING_SETUP_SQPOLL: u32 = 1 << 1;
/// SQ_AFF: SQPOLLスレッドをCPUに固定
pub const IORING_SETUP_SQ_AFF: u32 = 1 << 2;
/// CQSIZE: CQサイズを手動指定
pub const IORING_SETUP_CQSIZE: u32 = 1 << 3;
/// CLAMP: エントリ数を最大値にクランプ
pub const IORING_SETUP_CLAMP: u32 = 1 << 4;
/// ATTACH_WQ: 既存のio_uringの作業キューに接続
pub const IORING_SETUP_ATTACH_WQ: u32 = 1 << 5;
/// R_DISABLED: 制限適用まで無効状態で作成
pub const IORING_SETUP_R_DISABLED: u32 = 1 << 6;

// ====================
// io_uring_enter フラグ
// ====================

pub const IORING_ENTER_GETEVENTS: u32 = 1 << 0;
pub const IORING_ENTER_SQ_WAKEUP: u32 = 1 << 1;
pub const IORING_ENTER_SQ_WAIT: u32 = 1 << 2;
pub const IORING_ENTER_EXT_ARG: u32 = 1 << 3;

// ====================
// io_uring_register オペコード
// ====================

pub const IORING_REGISTER_BUFFERS: u32 = 0;
pub const IORING_UNREGISTER_BUFFERS: u32 = 1;
pub const IORING_REGISTER_FILES: u32 = 2;
pub const IORING_UNREGISTER_FILES: u32 = 3;
pub const IORING_REGISTER_EVENTFD: u32 = 4;
pub const IORING_UNREGISTER_EVENTFD: u32 = 5;
pub const IORING_REGISTER_FILES_UPDATE: u32 = 6;
pub const IORING_REGISTER_EVENTFD_ASYNC: u32 = 7;
pub const IORING_REGISTER_PROBE: u32 = 8;
pub const IORING_REGISTER_PERSONALITY: u32 = 9;
pub const IORING_UNREGISTER_PERSONALITY: u32 = 10;
pub const IORING_REGISTER_RESTRICTIONS: u32 = 11;
pub const IORING_REGISTER_ENABLE_RINGS: u32 = 12;

// ====================
// SQE オペコード
// ====================

pub const IORING_OP_NOP: u8 = 0;
pub const IORING_OP_READV: u8 = 1;
pub const IORING_OP_WRITEV: u8 = 2;
pub const IORING_OP_FSYNC: u8 = 3;
pub const IORING_OP_READ_FIXED: u8 = 4;
pub const IORING_OP_WRITE_FIXED: u8 = 5;
pub const IORING_OP_POLL_ADD: u8 = 6;
pub const IORING_OP_POLL_REMOVE: u8 = 7;
pub const IORING_OP_SYNC_FILE_RANGE: u8 = 8;
pub const IORING_OP_SENDMSG: u8 = 9;
pub const IORING_OP_RECVMSG: u8 = 10;
pub const IORING_OP_TIMEOUT: u8 = 11;
pub const IORING_OP_TIMEOUT_REMOVE: u8 = 12;
pub const IORING_OP_ACCEPT: u8 = 13;
pub const IORING_OP_ASYNC_CANCEL: u8 = 14;
pub const IORING_OP_LINK_TIMEOUT: u8 = 15;
pub const IORING_OP_CONNECT: u8 = 16;
pub const IORING_OP_FALLOCATE: u8 = 17;
pub const IORING_OP_OPENAT: u8 = 18;
pub const IORING_OP_CLOSE: u8 = 19;
pub const IORING_OP_FILES_UPDATE: u8 = 20;
pub const IORING_OP_STATX: u8 = 21;
pub const IORING_OP_READ: u8 = 22;
pub const IORING_OP_WRITE: u8 = 23;
pub const IORING_OP_FADVISE: u8 = 24;
pub const IORING_OP_MADVISE: u8 = 25;
pub const IORING_OP_SEND: u8 = 26;
pub const IORING_OP_RECV: u8 = 27;
pub const IORING_OP_OPENAT2: u8 = 28;
pub const IORING_OP_EPOLL_CTL: u8 = 29;
pub const IORING_OP_SPLICE: u8 = 30;
pub const IORING_OP_PROVIDE_BUFFERS: u8 = 31;
pub const IORING_OP_REMOVE_BUFFERS: u8 = 32;
pub const IORING_OP_TEE: u8 = 33;
pub const IORING_OP_SHUTDOWN: u8 = 34;
pub const IORING_OP_RENAMEAT: u8 = 35;
pub const IORING_OP_UNLINKAT: u8 = 36;
pub const IORING_OP_MKDIRAT: u8 = 37;
pub const IORING_OP_SYMLINKAT: u8 = 38;
pub const IORING_OP_LINKAT: u8 = 39;
pub const IORING_OP_SOCKET: u8 = 41;

// ====================
// 制限タイプ（IORING_REGISTER_RESTRICTIONS 用）
// ====================

pub const IORING_RESTRICTION_REGISTER_OP: u16 = 0;
pub const IORING_RESTRICTION_SQE_OP: u16 = 1;
pub const IORING_RESTRICTION_SQE_FLAGS_ALLOWED: u16 = 2;
pub const IORING_RESTRICTION_SQE_FLAGS_REQUIRED: u16 = 3;

// ====================
// mmap オフセット定数
// ====================

pub const IORING_OFF_SQ_RING: i64 = 0;
pub const IORING_OFF_CQ_RING: i64 = 0x0800_0000;
pub const IORING_OFF_SQES: i64 = 0x1000_0000;

// ====================
// SQE フラグ
// ====================

pub const IOSQE_FIXED_FILE: u8 = 1 << 0;
pub const IOSQE_IO_DRAIN: u8 = 1 << 1;
pub const IOSQE_IO_LINK: u8 = 1 << 2;
pub const IOSQE_IO_HARDLINK: u8 = 1 << 3;
pub const IOSQE_ASYNC: u8 = 1 << 4;
pub const IOSQE_BUFFER_SELECT: u8 = 1 << 5;

// ====================
// CQE フラグ
// ====================

pub const IORING_CQE_F_BUFFER: u32 = 1 << 0;
pub const IORING_CQE_F_MORE: u32 = 1 << 1;

// ====================
// io_uring_params 構造体
// ====================

/// io_uring_setup(2) に渡すパラメータ構造体
#[repr(C)]
#[derive(Debug, Default)]
pub struct IoUringParams {
    pub sq_entries: u32,
    pub cq_entries: u32,
    pub flags: u32,
    pub sq_thread_cpu: u32,
    pub sq_thread_idle: u32,
    pub features: u32,
    pub wq_fd: u32,
    pub resv: [u32; 3],
    pub sq_off: SqRingOffsets,
    pub cq_off: CqRingOffsets,
}

/// SQ リングのオフセット
#[repr(C)]
#[derive(Debug, Default)]
pub struct SqRingOffsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub flags: u32,
    pub dropped: u32,
    pub array: u32,
    pub resv1: u32,
    pub resv2: u64,
}

/// CQ リングのオフセット
#[repr(C)]
#[derive(Debug, Default)]
pub struct CqRingOffsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub overflow: u32,
    pub cqes: u32,
    pub flags: u32,
    pub resv1: u32,
    pub resv2: u64,
}

// ====================
// SQE 構造体（64バイト固定長）
// ====================

/// Submission Queue Entry（SQE）
///
/// io_uring に送信する I/O 操作の記述子。64バイト固定長。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct IoUringSqe {
    /// オペコード（IORING_OP_*）
    pub opcode: u8,
    /// フラグ（IOSQE_*）
    pub flags: u8,
    /// ioprio
    pub ioprio: u16,
    /// ファイルディスクリプタ
    pub fd: i32,
    /// オフセット（ファイル位置、タイムアウト等）
    pub off_or_addr2: u64,
    /// バッファアドレス（またはポインタ）
    pub addr_or_splice_off_in: u64,
    /// バッファ長
    pub len: u32,
    /// op固有フィールド（rw_flags, fsync_flags等）
    pub op_flags: u32,
    /// user_data: CQEで返ってくる識別子
    pub user_data: u64,
    /// 予備フィールド
    pub buf_index_or_buf_group: u16,
    pub personality: u16,
    pub splice_fd_in_or_file_index: i32,
    pub addr3_or_cmd: [u64; 2],
}

// ====================
// CQE 構造体（16バイト固定長）
// ====================

/// Completion Queue Entry（CQE）
///
/// io_uring から受け取る I/O 完了通知。16バイト固定長。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct IoUringCqe {
    /// user_data: SQEで設定した識別子
    pub user_data: u64,
    /// 完了結果（負の値はエラーコード）
    pub res: i32,
    /// フラグ（IORING_CQE_F_*）
    pub flags: u32,
}

// ====================
// 制限エントリ（IORING_REGISTER_RESTRICTIONS 用）
// ====================

/// io_uring 制限エントリ
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct IoUringRestriction {
    /// 制限タイプ（IORING_RESTRICTION_*）
    pub opcode: u16,
    /// 操作コード（SQE_OP または REGISTER_OP）
    pub register_opcode_or_sqe_op: u8,
    pub sqe_flags: u8,
    pub resv: u32,
    pub resv2: [u32; 3],
}

// ====================
// タイムアウト構造体
// ====================

/// io_uring タイムアウト用 timespec
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct KernelTimespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

// ====================
// io_uring リング管理構造体
// ====================

/// io_uring リング
///
/// mmap によりマップされた SQ/CQ リングと SQE 配列を管理する。
pub struct IoUring {
    /// io_uring ファイルディスクリプタ
    pub fd: i32,
    /// リングパラメータ
    pub params: IoUringParams,

    // SQ リング
    sq_ring_ptr: *mut u8,
    sq_ring_size: usize,
    sq_head: *const AtomicU32,
    sq_tail: *mut AtomicU32,
    sq_ring_mask: *const u32,
    sq_ring_entries: *const u32,
    sq_flags: *mut u32,
    sq_array: *mut u32,

    // SQE 配列
    sqes_ptr: *mut IoUringSqe,
    sqes_size: usize,

    // CQ リング
    cq_ring_ptr: *mut u8,
    cq_ring_size: usize,
    cq_head: *mut AtomicU32,
    cq_tail: *const AtomicU32,
    cq_ring_mask: *const u32,
    cq_ring_entries: *const u32,
    cqes_ptr: *mut IoUringCqe,
}

// SAFETY: IoUring は各スレッドが専有して使用するため Send を実装
unsafe impl Send for IoUring {}

impl IoUring {
    /// 新しい io_uring リングを作成
    ///
    /// # Arguments
    /// * `entries` - キューエントリ数（2の冪数）
    /// * `flags` - IORING_SETUP_* フラグ
    pub fn new(entries: u32, flags: u32) -> io::Result<Self> {
        let mut params = IoUringParams {
            flags,
            ..Default::default()
        };

        // io_uring_setup(2) syscall
        let fd = unsafe {
            libc::syscall(
                SYS_IO_URING_SETUP,
                entries as libc::c_long,
                &mut params as *mut IoUringParams as libc::c_long,
            )
        };

        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = fd as i32;

        // SQ リングをマップ
        let sq_ring_size = (params.sq_off.array as usize)
            + (params.sq_entries as usize) * std::mem::size_of::<u32>();

        let sq_ring_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                sq_ring_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                fd,
                IORING_OFF_SQ_RING as libc::off_t,
            )
        };
        if sq_ring_ptr == libc::MAP_FAILED {
            unsafe { libc::close(fd) };
            return Err(io::Error::last_os_error());
        }
        let sq_ring_ptr = sq_ring_ptr as *mut u8;

        // SQE 配列をマップ
        let sqes_size = (params.sq_entries as usize) * std::mem::size_of::<IoUringSqe>();
        let sqes_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                sqes_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                fd,
                IORING_OFF_SQES as libc::off_t,
            )
        };
        if sqes_ptr == libc::MAP_FAILED {
            unsafe {
                libc::munmap(sq_ring_ptr as *mut libc::c_void, sq_ring_size);
                libc::close(fd);
            }
            return Err(io::Error::last_os_error());
        }
        let sqes_ptr = sqes_ptr as *mut IoUringSqe;

        // CQ リングをマップ（SQ と同じ mmap ページの場合もある）
        let cq_ring_size = (params.cq_off.cqes as usize)
            + (params.cq_entries as usize) * std::mem::size_of::<IoUringCqe>();

        let cq_ring_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                cq_ring_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                fd,
                IORING_OFF_CQ_RING as libc::off_t,
            )
        };
        if cq_ring_ptr == libc::MAP_FAILED {
            unsafe {
                libc::munmap(sq_ring_ptr as *mut libc::c_void, sq_ring_size);
                libc::munmap(sqes_ptr as *mut libc::c_void, sqes_size);
                libc::close(fd);
            }
            return Err(io::Error::last_os_error());
        }
        let cq_ring_ptr = cq_ring_ptr as *mut u8;

        // オフセットからポインタを計算
        let sq_head = unsafe {
            sq_ring_ptr.add(params.sq_off.head as usize) as *const AtomicU32
        };
        let sq_tail = unsafe {
            sq_ring_ptr.add(params.sq_off.tail as usize) as *mut AtomicU32
        };
        let sq_ring_mask = unsafe {
            sq_ring_ptr.add(params.sq_off.ring_mask as usize) as *const u32
        };
        let sq_ring_entries = unsafe {
            sq_ring_ptr.add(params.sq_off.ring_entries as usize) as *const u32
        };
        let sq_flags = unsafe {
            sq_ring_ptr.add(params.sq_off.flags as usize) as *mut u32
        };
        let sq_array = unsafe {
            sq_ring_ptr.add(params.sq_off.array as usize) as *mut u32
        };

        let cq_head = unsafe {
            cq_ring_ptr.add(params.cq_off.head as usize) as *mut AtomicU32
        };
        let cq_tail = unsafe {
            cq_ring_ptr.add(params.cq_off.tail as usize) as *const AtomicU32
        };
        let cq_ring_mask = unsafe {
            cq_ring_ptr.add(params.cq_off.ring_mask as usize) as *const u32
        };
        let cq_ring_entries = unsafe {
            cq_ring_ptr.add(params.cq_off.ring_entries as usize) as *const u32
        };
        let cqes_ptr = unsafe {
            cq_ring_ptr.add(params.cq_off.cqes as usize) as *mut IoUringCqe
        };

        Ok(Self {
            fd,
            params,
            sq_ring_ptr,
            sq_ring_size,
            sq_head,
            sq_tail,
            sq_ring_mask,
            sq_ring_entries,
            sq_flags,
            sq_array,
            sqes_ptr,
            sqes_size,
            cq_ring_ptr,
            cq_ring_size,
            cq_head,
            cq_tail,
            cq_ring_mask,
            cq_ring_entries,
            cqes_ptr,
        })
    }

    /// IORING_REGISTER_RESTRICTIONS を適用して許可オペコードを制限する
    ///
    /// R_DISABLED フラグ付きで作成後、制限を適用してから ENABLE_RINGS を呼ぶこと。
    pub fn apply_restrictions(&self, opcodes: &[u8]) -> io::Result<()> {
        let mut restrictions: Vec<IoUringRestriction> = opcodes
            .iter()
            .map(|&op| IoUringRestriction {
                opcode: IORING_RESTRICTION_SQE_OP,
                register_opcode_or_sqe_op: op,
                ..Default::default()
            })
            .collect();

        let ret = unsafe {
            libc::syscall(
                SYS_IO_URING_REGISTER,
                self.fd as libc::c_long,
                IORING_REGISTER_RESTRICTIONS as libc::c_long,
                restrictions.as_mut_ptr() as libc::c_long,
                restrictions.len() as libc::c_long,
            )
        };

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// IORING_REGISTER_ENABLE_RINGS でリングを有効化
    pub fn enable_rings(&self) -> io::Result<()> {
        let ret = unsafe {
            libc::syscall(
                SYS_IO_URING_REGISTER,
                self.fd as libc::c_long,
                IORING_REGISTER_ENABLE_RINGS as libc::c_long,
                0i64,
                0i64,
            )
        };

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// SQE スロットを取得（None = SQ フル）
    ///
    /// # Safety
    /// 返されたポインタは次の `submit()` 呼び出しまでに初期化すること
    pub fn get_sqe(&mut self) -> Option<&mut IoUringSqe> {
        unsafe {
            let head = (*self.sq_head).load(Ordering::Acquire);
            let tail = (*self.sq_tail).load(Ordering::Relaxed);
            let mask = *self.sq_ring_mask;
            let ring_entries = *self.sq_ring_entries;

            if tail.wrapping_sub(head) >= ring_entries {
                // SQ フル
                return None;
            }

            let idx = (tail & mask) as usize;
            *self.sq_array.add(idx) = idx as u32;

            let sqe = &mut *self.sqes_ptr.add(idx);
            // SQE をゼロ初期化
            std::ptr::write_bytes(sqe as *mut IoUringSqe, 0, 1);

            (*self.sq_tail).store(tail.wrapping_add(1), Ordering::Release);

            Some(sqe)
        }
    }

    /// io_uring_enter(2) で SQE をカーネルに送信し、CQE を待つ
    ///
    /// # Arguments
    /// * `to_submit` - 送信する SQE 数
    /// * `min_complete` - 最低限待つ完了数（0 = 待たない）
    /// * `flags` - IORING_ENTER_* フラグ
    pub fn enter(&self, to_submit: u32, min_complete: u32, flags: u32) -> io::Result<i32> {
        let ret = unsafe {
            libc::syscall(
                SYS_IO_URING_ENTER,
                self.fd as libc::c_long,
                to_submit as libc::c_long,
                min_complete as libc::c_long,
                flags as libc::c_long,
                std::ptr::null::<libc::sigset_t>() as libc::c_long,
                std::mem::size_of::<libc::sigset_t>() as libc::c_long,
            )
        };

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ret as i32)
    }

    /// CQE を消費する（イテレータとしてコールバック）
    ///
    /// 利用可能な CQE をすべて処理し、`f` コールバックを呼ぶ。
    pub fn consume_cqes<F>(&mut self, mut f: F)
    where
        F: FnMut(&IoUringCqe),
    {
        unsafe {
            let mut head = (*self.cq_head).load(Ordering::Acquire);
            let tail = (*self.cq_tail).load(Ordering::Acquire);
            let mask = *self.cq_ring_mask;

            while head != tail {
                let cqe = &*self.cqes_ptr.add((head & mask) as usize);
                f(cqe);
                head = head.wrapping_add(1);
            }

            (*self.cq_head).store(head, Ordering::Release);
        }
    }

    /// 保留中の SQE を提出する
    pub fn submit(&self) -> io::Result<i32> {
        self.enter(self.sq_pending(), 0, 0)
    }

    /// 提出して最低 `n` 件の完了を待つ
    pub fn submit_and_wait(&self, n: u32) -> io::Result<i32> {
        let pending = self.sq_pending();
        if pending == 0 && n == 0 {
            return Ok(0);
        }
        self.enter(pending, n, if n > 0 { IORING_ENTER_GETEVENTS } else { 0 })
    }

    /// 未提出の SQE 数を返す
    fn sq_pending(&self) -> u32 {
        unsafe {
            let head = (*self.sq_head).load(Ordering::Acquire);
            let tail = (*self.sq_tail).load(Ordering::Relaxed);
            tail.wrapping_sub(head)
        }
    }

    /// 利用可能な CQE 数を返す
    pub fn cq_ready(&self) -> u32 {
        unsafe {
            let head = (*self.cq_head).load(Ordering::Relaxed);
            let tail = (*self.cq_tail).load(Ordering::Acquire);
            tail.wrapping_sub(head)
        }
    }
}

impl Drop for IoUring {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.sq_ring_ptr as *mut libc::c_void, self.sq_ring_size);
            libc::munmap(self.sqes_ptr as *mut libc::c_void, self.sqes_size);
            libc::munmap(self.cq_ring_ptr as *mut libc::c_void, self.cq_ring_size);
            libc::close(self.fd);
        }
    }
}
