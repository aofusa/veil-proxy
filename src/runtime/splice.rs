//! io_uring `splice(2)` によるゼロコピー転送（F-30）
//!
//! TCP→TCP の中継を、ユーザースペースのバッファを介さず**カーネル内**で完結させる。
//! `splice` は少なくとも一方がパイプである必要があるため、`src_socket → pipe → dst_socket`
//! の 2 段 splice で中継する。パイプのページはカーネル内参照のため、ユーザースペースへの
//! コピーが発生しない。
//!
//! ## 待機方式
//!
//! ソケット/パイプを `O_NONBLOCK` にし、`SPLICE_F_NONBLOCK` 付きで splice を発行する。
//! データ/空きが無い場合は `-EAGAIN`（`WouldBlock`）が即座に返るので、呼び出し側は
//! `readable()` / `writable()`（io_uring `POLL_ADD`）で待機してから再試行する。これにより
//! アイドル接続が io_uring の非同期ワーカーを占有しない（POLL_ADD のみ登録される）。

use std::future::Future;
use std::io;
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::runtime::executor::{
    detach_op, next_user_data, register_op, remove_op, set_op_waker, submit_sqes, take_op_result,
    with_ring,
};
use crate::runtime::ring::IORING_OP_SPLICE;

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
        // SPLICE_F_NONBLOCK と組み合わせて完全ノンブロッキングにするため O_NONBLOCK を付与。
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
        unsafe {
            libc::close(self.read_fd);
            libc::close(self.write_fd);
        }
    }
}

/// `fd_in` から `fd_out` へ最大 `len` バイトを splice する Future を返す（IORING_OP_SPLICE）。
///
/// ソケット/パイプはシーク可能なオフセットを持たないため `off_in`/`off_out` は `-1`。
/// `SPLICE_F_NONBLOCK` 付きで発行するため、データ/空きが無い場合は `WouldBlock` を返す。
pub fn splice(fd_in: RawFd, fd_out: RawFd, len: usize) -> SpliceFuture {
    SpliceFuture {
        fd_in,
        fd_out,
        len: len as u32,
        user_data: 0,
        submitted: false,
    }
}

/// `IORING_OP_SPLICE` Future。
pub struct SpliceFuture {
    fd_in: RawFd,
    fd_out: RawFd,
    len: u32,
    user_data: u64,
    submitted: bool,
}

impl Future for SpliceFuture {
    type Output = io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.submitted {
            let user_data = next_user_data();
            self.user_data = user_data;
            register_op(user_data);

            let (fd_in, fd_out, len) = (self.fd_in, self.fd_out, self.len);
            with_ring(|ring| {
                if let Some(sqe) = ring.get_sqe() {
                    // SPLICE SQE: fd=fd_out, splice_fd_in=fd_in, addr=off_in(-1), off=off_out(-1),
                    // len=nbytes, op_flags=splice_flags
                    sqe.opcode = IORING_OP_SPLICE;
                    sqe.fd = fd_out;
                    sqe.splice_fd_in_or_file_index = fd_in;
                    sqe.addr_or_splice_off_in = u64::MAX; // off_in = -1（ソケット/パイプ）
                    sqe.off_or_addr2 = u64::MAX; // off_out = -1（ソケット/パイプ）
                    sqe.len = len;
                    sqe.op_flags = SPLICE_F_MOVE | SPLICE_F_NONBLOCK | SPLICE_F_MORE;
                    sqe.user_data = user_data;
                }
            });

            if let Err(e) = submit_sqes() {
                remove_op(user_data);
                return Poll::Ready(Err(e));
            }
            self.submitted = true;
        }

        match take_op_result(self.user_data) {
            Some(res) => {
                // 完了。in-flight でなくなったので Drop での detach を抑止する。
                self.submitted = false;
                if res < 0 {
                    Poll::Ready(Err(io::Error::from_raw_os_error(-res)))
                } else {
                    Poll::Ready(Ok(res as usize))
                }
            }
            None => {
                set_op_waker(self.user_data, cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl Drop for SpliceFuture {
    fn drop(&mut self) {
        // in-flight のまま drop された場合、op はソケット/パイプ fd を参照するが
        // ユーザーメモリは参照しないため、detach + ASYNC_CANCEL で除去するのみでよい
        // （fd は呼び出し側が TcpStream / Pipe として保持し続ける。B-07a と同方針）。
        if self.submitted {
            detach_op(self.user_data, Box::new(|_| {}));
        }
    }
}
