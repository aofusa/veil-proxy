//! epoll(7) の薄いラッパ（`veil_poller_epoll`）
//!
//! `epoll_create1` / `epoll_ctl` / `epoll_wait` を直接呼ぶだけの最小レイヤ。
//! oneshot（`EPOLLONESHOT`）を前提とし、fd ごとの Waker 管理は
//! `reactor::executor`（`FdTable` 経由）が担う。本モジュールはカーネル呼び出しのみに
//! 責務を絞り、poller の差し替え（kqueue 等）を local に閉じ込める。

use std::io;
use std::os::unix::io::RawFd;

/// 読み取り可能 interest ビット（`EPOLLIN` 相当）。
pub const READ: u32 = libc::EPOLLIN as u32;
/// 書き込み可能 interest ビット（`EPOLLOUT` 相当）。
pub const WRITE: u32 = libc::EPOLLOUT as u32;
/// エラー/ハングアップ通知ビット（要求せずとも常に配送される）。
pub const ERR_HUP: u32 = (libc::EPOLLERR | libc::EPOLLHUP) as u32;

const ONESHOT: u32 = libc::EPOLLONESHOT as u32;

/// epoll インスタンスのラッパ。
pub(crate) struct EpollPoller {
    epfd: RawFd,
}

impl EpollPoller {
    /// 新しい epoll インスタンスを作成する（`EPOLL_CLOEXEC` 付き）。
    pub fn new() -> io::Result<Self> {
        let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epfd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { epfd })
    }

    /// fd を interest ビット付きで新規登録する（`EPOLLONESHOT` を常に付与する）。
    pub fn add(&self, fd: RawFd, interest: u32) -> io::Result<()> {
        let mut ev = libc::epoll_event {
            events: interest | ONESHOT,
            u64: fd as u64,
        };
        let ret = unsafe { libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_ADD, fd, &mut ev) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// 既存登録の interest ビットを差し替える（oneshot 再武装）。
    pub fn modify(&self, fd: RawFd, interest: u32) -> io::Result<()> {
        let mut ev = libc::epoll_event {
            events: interest | ONESHOT,
            u64: fd as u64,
        };
        let ret = unsafe { libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_MOD, fd, &mut ev) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// fd の登録を除去する（close 直前に呼ぶ。close 自体でもカーネル側は自動除去されるが、
    /// close 前に明示除去することで fd 再利用時の競合ウィンドウを狭める）。
    pub fn delete(&self, fd: RawFd) {
        // SAFETY: EPOLL_CTL_DEL は event 引数を無視する（NULL 可、glibc 実装依存の
        // 過去のカーネルバグ回避のためダミー値を渡す実装もあるが、現行カーネルは無視する）。
        unsafe {
            libc::epoll_ctl(
                self.epfd,
                libc::EPOLL_CTL_DEL,
                fd,
                std::ptr::null_mut::<libc::epoll_event>(),
            );
        }
    }

    /// イベントを待つ。`timeout_ms` は負値で無期限待機。
    pub fn wait(&self, events: &mut [libc::epoll_event], timeout_ms: i32) -> io::Result<usize> {
        let n = unsafe {
            libc::epoll_wait(
                self.epfd,
                events.as_mut_ptr(),
                events.len() as i32,
                timeout_ms,
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

impl Drop for EpollPoller {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.epfd);
        }
    }
}
