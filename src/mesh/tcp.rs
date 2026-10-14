//! L4 の転送（F-216。Envoy の `tcp_proxy`）。
//!
//! 片方向ずつのタスクに分けられない（TLS のストリームは読み書きで状態を共有する）ので、1 つのタスクで
//! 両側の可読（`wait_readable_fd`。TLS の復号済みの平文は `BufferedReadState`）を待ち、読めた側から
//! もう一方へ書く。読み取りを取り消さない（io_uring の読み取りを途中で捨てるとデータを失う。F-116）。
//! 片側が閉じたら相手側へ送信の終わり（FIN / close_notify）を伝え、もう一方が閉じるまで続ける（半クローズ）。

use crate::runtime::handle::AsRawFd;
use crate::runtime::io::{AsyncReadRent, AsyncWriteRentExt, BufferedReadState};
use std::future::Future;
use std::future::poll_fn;
use std::task::Poll;
use std::time::Duration;

const BUF: usize = 64 * 1024;

enum Ready {
    A,
    B,
    Idle,
}

async fn wait_either(a_fd: i32, b_fd: i32, wait_a: bool, wait_b: bool, idle: Duration) -> Ready {
    let ra = crate::runtime::tcp::wait_readable_fd(a_fd as _);
    let rb = crate::runtime::tcp::wait_readable_fd(b_fd as _);
    let sleep = crate::runtime::time::sleep(idle);
    let mut ra = std::pin::pin!(ra);
    let mut rb = std::pin::pin!(rb);
    let mut sleep = std::pin::pin!(sleep);
    poll_fn(move |cx| {
        if wait_a && ra.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Ready::A);
        }
        if wait_b && rb.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Ready::B);
        }
        if sleep.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Ready::Idle);
        }
        Poll::Pending
    })
    .await
}

/// 送信側だけ閉じる（FIN。TLS は close_notify を送らない＝Envoy の tcp_proxy の半クローズと同じ扱い）。
fn half_close(fd: i32) {
    // SAFETY: 有効なソケット fd の送信側を閉じるだけ
    unsafe {
        libc::shutdown(fd, libc::SHUT_WR);
    }
}

/// 1 回読んで相手へ書く。`Ok(0)` は EOF。
async fn pump<R, W>(r: &mut R, w: &mut W) -> std::io::Result<usize>
where
    R: AsyncReadRent,
    W: AsyncWriteRentExt,
{
    let buf: Vec<u8> = Vec::with_capacity(BUF);
    let (res, mut buf) = r.read(buf).await;
    let n = res?;
    if n == 0 {
        return Ok(0);
    }
    buf.truncate(n);
    let (wr, _) = w.write_all(buf).await;
    wr?;
    Ok(n)
}

/// 両方向に転送する。戻り値は (a→b のバイト数, b→a のバイト数)。
pub async fn relay<A, B>(a: &mut A, b: &mut B, idle_timeout: Duration) -> (u64, u64)
where
    A: AsyncReadRent + AsyncWriteRentExt + AsRawFd + BufferedReadState,
    B: AsyncReadRent + AsyncWriteRentExt + AsRawFd + BufferedReadState,
{
    let (mut ab, mut ba) = (0u64, 0u64);
    let (mut a_open, mut b_open) = (true, true);
    let a_fd = a.as_raw_fd() as i32;
    let b_fd = b.as_raw_fd() as i32;
    let mut last_activity = std::time::Instant::now();
    while a_open || b_open {
        if super::state::state()
            .draining
            .load(std::sync::atomic::Ordering::Relaxed)
            && crate::config::SHUTDOWN_FLAG.load(std::sync::atomic::Ordering::Relaxed)
        {
            break;
        }
        let which = if a_open && a.has_buffered_read_data() {
            Ready::A
        } else if b_open && b.has_buffered_read_data() {
            Ready::B
        } else {
            wait_either(a_fd, b_fd, a_open, b_open, Duration::from_secs(1)).await
        };
        match which {
            Ready::A => match pump(a, b).await {
                Ok(0) | Err(_) => {
                    a_open = false;
                    half_close(b_fd);
                }
                Ok(n) => {
                    ab += n as u64;
                    last_activity = std::time::Instant::now();
                }
            },
            Ready::B => match pump(b, a).await {
                Ok(0) | Err(_) => {
                    b_open = false;
                    half_close(a_fd);
                }
                Ok(n) => {
                    ba += n as u64;
                    last_activity = std::time::Instant::now();
                }
            },
            Ready::Idle => {
                if last_activity.elapsed() >= idle_timeout {
                    break;
                }
            }
        }
    }
    (ab, ba)
}
