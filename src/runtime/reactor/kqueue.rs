//! kqueue(2)/kevent(2) の薄いラッパ（`veil_poller_kqueue`、FreeBSD/OpenBSD）
//!
//! `epoll.rs` と同じ責務分担: カーネル呼び出しのみに閉じ、fd ごとの Waker 管理は
//! `reactor::executor`（`FdTable` 経由）が担う。
//!
//! epoll と異なり、kqueue は `EVFILT_READ` / `EVFILT_WRITE` が **fd ごとに独立した
//! フィルタ**である（1 fd につき最大 2 エントリ）。そのため epoll の「1 fd = 1 interest
//! ビットマスク」という前提はそのまま持ち込めない。本ラッパでは read/write を個別の
//! `kevent` エントリとして登録・解除する API を提供し、`executor.rs` 側の armed ビット
//! 管理（`poller::FdRecord`）は「read フィルタが有効か」「write フィルタが有効か」を
//! 独立に追跡する形でそのまま再利用する（`READ`/`WRITE` ビットの意味は epoll 版と揃える）。
//!
//! `EV_ADD` は kqueue では EEXIST を返さず**冪等**（既存フィルタの再設定になる）ため、
//! epoll 版の `known_to_kernel`（ADD/MOD 判定）に相当する管理は不要で、常に `EV_ADD` を
//! 使えばよい。oneshot 意味論は `EV_ONESHOT` で表現する（発火後にカーネルが自動で
//! フィルタを削除する。次回待機時は改めて `EV_ADD|EV_ONESHOT` で再登録する）。

use std::io;
use std::os::unix::io::RawFd;

/// 読み取り可能 interest ビット（`EVFILT_READ` 相当）。
pub const READ: u32 = 0b01;
/// 書き込み可能 interest ビット（`EVFILT_WRITE` 相当）。
pub const WRITE: u32 = 0b10;

// epoll の `ERR_HUP` に相当する個別ビットは kqueue には存在しない: `EV_EOF`/`EV_ERROR`
// は発火したフィルタ（`EVFILT_READ`/`EVFILT_WRITE`）自体のイベントとして配送されるため、
// dispatch 側は READ/WRITE の発火だけを見ればよい（`executor::dispatch_event` 参照）。

/// kqueue インスタンスのラッパ。
pub(crate) struct KqueuePoller {
    kq: RawFd,
}

impl KqueuePoller {
    /// 新しい kqueue インスタンスを作成する。
    pub fn new() -> io::Result<Self> {
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        // FD_CLOEXEC を明示付与する（kqueue() 自体には CLOEXEC 版の生成関数が無い）。
        unsafe {
            libc::fcntl(kq, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        Ok(Self { kq })
    }

    /// fd の interest ビット（`READ`/`WRITE` の組み合わせ）を oneshot で（再）登録する。
    ///
    /// 立っているビットは `EV_ADD|EV_ONESHOT` で登録し、立っていないビットのうち
    /// 直前まで登録されていたもの（`prev_mask` で渡す）は `EV_DELETE` で明示的に外す。
    /// kqueue は fd ごとに read/write が独立フィルタのため、epoll の 1 回の
    /// `epoll_ctl(MOD)` に相当する処理を changelist にまとめて `kevent` 1 回で発行する。
    pub fn update(&self, fd: RawFd, mask: u32, prev_mask: u32) -> io::Result<()> {
        let mut changes: [libc::kevent; 2] = unsafe { std::mem::zeroed() };
        let mut n = 0usize;

        if mask & READ != 0 {
            changes[n] = make_kevent(fd, libc::EVFILT_READ, libc::EV_ADD | libc::EV_ONESHOT);
            n += 1;
        } else if prev_mask & READ != 0 {
            changes[n] = make_kevent(fd, libc::EVFILT_READ, libc::EV_DELETE);
            n += 1;
        }
        if mask & WRITE != 0 {
            changes[n] = make_kevent(fd, libc::EVFILT_WRITE, libc::EV_ADD | libc::EV_ONESHOT);
            n += 1;
        } else if prev_mask & WRITE != 0 {
            changes[n] = make_kevent(fd, libc::EVFILT_WRITE, libc::EV_DELETE);
            n += 1;
        }
        if n == 0 {
            return Ok(());
        }

        let ret = unsafe {
            libc::kevent(
                self.kq,
                changes.as_ptr(),
                n as i32,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if ret < 0 {
            let e = io::Error::last_os_error();
            // EV_DELETE を「登録されていないフィルタ」に対して発行した場合 ENOENT が
            // 返ることがある（oneshot 発火直後に prev_mask 側の delete が競合するケース）。
            // 実害が無いため無視する。
            if e.raw_os_error() != Some(libc::ENOENT) {
                return Err(e);
            }
        }
        Ok(())
    }

    /// fd の READ/WRITE フィルタを両方削除する（close 直前に呼ぶ）。
    pub fn delete(&self, fd: RawFd, mask: u32) {
        let mut changes: [libc::kevent; 2] = unsafe { std::mem::zeroed() };
        let mut n = 0usize;
        if mask & READ != 0 {
            changes[n] = make_kevent(fd, libc::EVFILT_READ, libc::EV_DELETE);
            n += 1;
        }
        if mask & WRITE != 0 {
            changes[n] = make_kevent(fd, libc::EVFILT_WRITE, libc::EV_DELETE);
            n += 1;
        }
        if n == 0 {
            return;
        }
        unsafe {
            libc::kevent(
                self.kq,
                changes.as_ptr(),
                n as i32,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            );
        }
    }

    /// イベントを待つ。`timeout_ms` が負値なら無期限待機。changelist は使わず
    /// eventlist のみを渡す（登録は `update`/`delete` で都度発行済みのため、
    /// wait 呼び出しと同一 syscall にまとめる最適化は行わない: 登録の発生タイミングが
    /// wait ループの外（Future::poll 内）であり、まとめてもバッチ効果が薄いため）。
    pub fn wait(&self, events: &mut [libc::kevent], timeout_ms: i32) -> io::Result<usize> {
        let ts;
        let ts_ptr = if timeout_ms < 0 {
            std::ptr::null()
        } else {
            ts = libc::timespec {
                tv_sec: (timeout_ms / 1000) as libc::time_t,
                tv_nsec: ((timeout_ms % 1000) * 1_000_000) as libc::c_long,
            };
            &ts as *const libc::timespec
        };
        let n = unsafe {
            libc::kevent(
                self.kq,
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                events.len() as i32,
                ts_ptr,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(0);
            }
            return Err(e);
        }
        Ok(n as usize)
    }
}

// filter/flags は FreeBSD/OpenBSD で型が異なり得る（libc クレートが target 別に
// `libc::kevent` のフィールド型を定義する）ため、呼び出し側の `libc::EVFILT_*`/
// `libc::EV_*` 定数をそのまま `as _` でフィールド型へキャストする（ハードコードした
// 具象型を引数に取らない）。
fn make_kevent(
    fd: RawFd,
    filter: impl TryInto<i16> + Copy,
    flags: impl TryInto<u16> + Copy,
) -> libc::kevent {
    let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
    ev.ident = fd as libc::uintptr_t;
    ev.filter = match filter.try_into() {
        Ok(v) => v,
        Err(_) => 0,
    };
    ev.flags = match flags.try_into() {
        Ok(v) => v,
        Err(_) => 0,
    };
    ev
}

impl Drop for KqueuePoller {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.kq);
        }
    }
}
