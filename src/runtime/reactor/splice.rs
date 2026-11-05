//! `splice(2)` によるゼロコピー転送（reactor バックエンド、Linux 専用）
//!
//! `runtime::uring::splice` と同一の公開 API（`Pipe`/`splice`/`splice_more`）を提供する。
//! io_uring 版はカーネルへ `IORING_OP_SPLICE` を発行して完了を待つが、reactor 版は
//! `SPLICE_F_NONBLOCK` 付きの非ブロッキング `splice(2)` を直接呼ぶ。`EAGAIN` は io_uring 版と同じく
//! `WouldBlock` で返し、待ち方（どちらのソケットの readiness を待つか・時間切れ）は呼び出し側が決める（B-294）。
//!
//! 設計ドキュメント 3.3 節のとおり Linux 専用（BSD は Phase 4 でバッファプール経由の
//! read/write フォールバックへ切り替える予定）。

use std::future::Future;
use std::io;
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::runtime::executor::unregister;

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

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
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
            // B-294: `EAGAIN` は `WouldBlock` として返す（io_uring 版と同じ意味）。呼び出し側はソケットの
            // `readable()`/`writable()` を時間切れ付きで待ってからやり直す。以前は両端の readiness を登録して
            // `Pending` を返していたが、片方は必ずパイプで常に準備できている（空なら書ける・データがあれば読める）ため
            // 即座に起床して空回りし、呼び出し側の時間切れも効かず、Waker が積もり続けた
            return Poll::Ready(Err(e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// B-294: データの無いソケットからの splice は待たずに `WouldBlock` を返す（以前はパイプ側の readiness で
    /// 即座に起床し続ける `Pending` を返していた）。待ち方は呼び出し側が決める。
    #[test]
    fn splice_without_data_returns_would_block() {
        let mut sv = [0 as RawFd; 2];
        // SAFETY: sv は 2 要素の配列で、socketpair が両端の fd を書き込む。
        let r = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
                sv.as_mut_ptr(),
            )
        };
        assert_eq!(r, 0);
        let pipe = Pipe::new().unwrap();
        let res = futures::executor::block_on(splice(sv[0], pipe.write_fd, 4096));
        assert_eq!(res.map_err(|e| e.kind()), Err(io::ErrorKind::WouldBlock));
        // データを入れると進む
        // SAFETY: sv[1] は有効なソケットで、バッファは 3 バイト。
        let w = unsafe { libc::write(sv[1], b"abc".as_ptr().cast(), 3) };
        assert_eq!(w, 3);
        let res = futures::executor::block_on(splice(sv[0], pipe.write_fd, 4096));
        assert_eq!(res.unwrap(), 3);
        // SAFETY: 自分で開いた fd を閉じる。
        unsafe {
            libc::close(sv[0]);
            libc::close(sv[1]);
        }
    }
}
