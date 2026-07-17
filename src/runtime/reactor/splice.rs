//! `splice(2)` によるゼロコピー転送（reactor バックエンド、Linux 専用）
//!
//! `runtime::uring::splice` と同一の公開 API（`Pipe`/`splice`/`splice_more`）を提供する。
//! io_uring 版はカーネルへ `IORING_OP_SPLICE` を発行して完了を待つが、reactor 版は
//! `SPLICE_F_NONBLOCK` 付きの非ブロッキング `splice(2)` を直接呼び、`EAGAIN` の場合のみ
//! readiness を待つ（try-first）。`fd_in`（読み出し側）と `fd_out`（書き込み側）の
//! どちらが詰まっているか事前には分からないため、両方の readiness を同一 Waker で
//! 登録する（どちらか一方でも起床すれば再試行する）。
//!
//! 設計ドキュメント 3.3 節のとおり Linux 専用（BSD は Phase 4 でバッファプール経由の
//! read/write フォールバックへ切り替える予定）。

use std::future::Future;
use std::io;
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::runtime::executor::{register_read, register_write, unregister};

// splice(2) フラグ
const SPLICE_F_MOVE: u32 = 0x01;
const SPLICE_F_NONBLOCK: u32 = 0x02;
const SPLICE_F_MORE: u32 = 0x04;

/// 匿名パイプ（splice の中継バッファ）。両端 fd を Drop でクローズする。
pub struct Pipe {
    pub read_fd: RawFd,
    pub write_fd: RawFd,
}

impl Pipe {
    /// `O_NONBLOCK | O_CLOEXEC` の匿名パイプを作成する。
    pub fn new() -> io::Result<Self> {
        let mut fds = [0 as RawFd; 2];
        let ret = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            read_fd: fds[0],
            write_fd: fds[1],
        })
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        unregister(self.read_fd);
        unregister(self.write_fd);
        unsafe {
            libc::close(self.read_fd);
            libc::close(self.write_fd);
        }
    }
}

/// `fd_in` から `fd_out` へ最大 `len` バイトを splice する Future を返す。
///
/// `SPLICE_F_MORE` は付与しない（B-25 と同方針。kTLS 出力先の最終部分レコードが
/// フラッシュされない問題を避ける）。
pub fn splice(fd_in: RawFd, fd_out: RawFd, len: usize) -> SpliceFuture {
    SpliceFuture {
        fd_in,
        fd_out,
        len: len as u32,
        flags: SPLICE_F_MOVE | SPLICE_F_NONBLOCK,
    }
}

/// `splice` の `SPLICE_F_MORE` 付き版（後続データが確実に続く中間チャンク専用）。
pub fn splice_more(fd_in: RawFd, fd_out: RawFd, len: usize) -> SpliceFuture {
    SpliceFuture {
        fd_in,
        fd_out,
        len: len as u32,
        flags: SPLICE_F_MOVE | SPLICE_F_NONBLOCK | SPLICE_F_MORE,
    }
}

/// splice Future（非ブロッキング `splice(2)` の try-first ラッパ）。
pub struct SpliceFuture {
    fd_in: RawFd,
    fd_out: RawFd,
    len: u32,
    flags: u32,
}

impl Future for SpliceFuture {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            let ret = unsafe {
                libc::splice(
                    self.fd_in,
                    std::ptr::null_mut(),
                    self.fd_out,
                    std::ptr::null_mut(),
                    self.len as usize,
                    self.flags,
                )
            };
            if ret >= 0 {
                return Poll::Ready(Ok(ret as usize));
            }
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if e.kind() == io::ErrorKind::WouldBlock {
                // 詰まっている側が読み出し・書き込みどちらか事前に分からないため、
                // 両方の readiness を同一 Waker で登録する。
                register_read(self.fd_in, cx.waker().clone());
                register_write(self.fd_out, cx.waker().clone());
                return Poll::Pending;
            }
            return Poll::Ready(Err(e));
        }
    }
}
