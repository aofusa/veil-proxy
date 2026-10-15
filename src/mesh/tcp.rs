//! L4 の転送（F-216。Envoy の `tcp_proxy`）。
//!
//! 片方向ずつのタスクに分けられない（TLS のストリームは読み書きで状態を共有する）ので、1 つのタスクで
//! 両側の可読（`wait_readable_fd`。TLS の復号済みの平文は `BufferedReadState`）を待ち、読めた側から
//! もう一方へ書く。読み取りを取り消さない（io_uring の読み取りを途中で捨てるとデータを失う。F-116）。
//! 片側が閉じたら相手側へ送信の終わり（FIN / close_notify）を伝え、もう一方が閉じるまで続ける（半クローズ）。

use crate::runtime::handle::AsRawFd;
use crate::runtime::io::{AsyncReadRent, AsyncWriteRentExt};
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

/// 読める平文を既に手元に持っているか（TLS は rustls の中の復号済みの分も含める）。
pub trait PendingRead {
    fn pending_read(&mut self) -> bool;
    /// ソケットが可読になった後に呼ぶ。`read` が待たずに返る（平文がある・閉じた）なら `true`。
    /// TLS は可読でも制御レコード（NewSessionTicket 等）だけのことがあり、そこで `read` すると
    /// 平文が来るまで待ってしまい、反対方向の転送が止まる。
    fn fill_ready(&mut self) -> bool {
        true
    }
}

impl PendingRead for super::server::DownStream {
    fn pending_read(&mut self) -> bool {
        self.take_pending_plaintext()
    }
    fn fill_ready(&mut self) -> bool {
        self.fill_plaintext_nonblocking().unwrap_or(true)
    }
}

impl PendingRead for super::upstream::UpIo {
    fn pending_read(&mut self) -> bool {
        match self {
            super::upstream::UpIo::Plain(_) => false,
            super::upstream::UpIo::Tls(s) => s.take_pending_plaintext(),
            #[cfg(feature = "mesh-linkerd")]
            super::upstream::UpIo::Tracked(s, _) => s.take_pending_plaintext(),
        }
    }
    fn fill_ready(&mut self) -> bool {
        match self {
            super::upstream::UpIo::Plain(_) => true,
            super::upstream::UpIo::Tls(s) => s.fill_plaintext_nonblocking().unwrap_or(true),
            #[cfg(feature = "mesh-linkerd")]
            super::upstream::UpIo::Tracked(s, _) => s.fill_plaintext_nonblocking().unwrap_or(true),
        }
    }
}

/// 両方向に転送する。戻り値は (a→b のバイト数, b→a のバイト数)。
pub async fn relay<A, B>(a: &mut A, b: &mut B, idle_timeout: Duration) -> (u64, u64)
where
    A: AsyncReadRent + AsyncWriteRentExt + AsRawFd + PendingRead,
    B: AsyncReadRent + AsyncWriteRentExt + AsRawFd + PendingRead,
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
        let pa = a_open && a.pending_read();
        let pb = !pa && b_open && b.pending_read();
        let which = if pa {
            Ready::A
        } else if pb {
            Ready::B
        } else {
            match wait_either(a_fd, b_fd, a_open, b_open, Duration::from_secs(1)).await {
                Ready::A if !a.fill_ready() => continue,
                Ready::B if !b.fill_ready() => continue,
                r => r,
            }
        };
        match which {
            Ready::A => match pump(a, b).await {
                Ok(0) | Err(_) => {
                    ftlog::debug!("[mesh] tcp relay: downstream closed after {} bytes", ab);
                    a_open = false;
                    half_close(b_fd);
                }
                Ok(n) => {
                    ftlog::debug!("[mesh] tcp relay: {} bytes downstream -> upstream", n);
                    ab += n as u64;
                    last_activity = std::time::Instant::now();
                }
            },
            Ready::B => match pump(b, a).await {
                Ok(0) | Err(_) => {
                    ftlog::debug!("[mesh] tcp relay: upstream closed after {} bytes", ba);
                    b_open = false;
                    half_close(a_fd);
                }
                Ok(n) => {
                    ftlog::debug!("[mesh] tcp relay: {} bytes upstream -> downstream", n);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::tcp::TcpStream;
    use std::io::{Read, Write};

    impl PendingRead for TcpStream {
        fn pending_read(&mut self) -> bool {
            false
        }
    }

    fn pair() -> (std::net::TcpStream, std::net::TcpStream) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let c = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (s, _) = l.accept().unwrap();
        (c, s)
    }

    fn to_rt(s: std::net::TcpStream) -> TcpStream {
        use std::os::fd::IntoRawFd;
        s.set_nonblocking(true).unwrap();
        // SAFETY: 所有権を移す
        unsafe { TcpStream::from_raw_fd(s.into_raw_fd()) }
    }

    /// 待ちに入ってから届いたデータも両方向に転送する（データが後から来る場合の起床）。
    #[test]
    fn relays_late_data_both_ways() {
        #[cfg(veil_rt_uring)]
        if crate::runtime::IoUring::new(8, 0).is_err() {
            return;
        }
        let (mut app, a_srv) = pair();
        let (b_cli, mut echo) = pair();
        let echo_t = std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            let n = echo.read(&mut buf).unwrap();
            echo.write_all(&buf[..n]).unwrap();
            // 相手が送信側を閉じるまで待つ
            let _ = echo.read(&mut buf);
        });
        let app_t = std::thread::spawn(move || {
            // 理由付き allow: 試験のスレッド
            #[allow(clippy::disallowed_methods)]
            std::thread::sleep(Duration::from_millis(300));
            app.write_all(b"ping-tcp").unwrap();
            let mut buf = [0u8; 8];
            app.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            app.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"ping-tcp");
            app.shutdown(std::net::Shutdown::Write).unwrap();
        });
        crate::runtime::block_on(async move {
            let mut a = to_rt(a_srv);
            let mut b = to_rt(b_cli);
            let (ab, ba) = relay(&mut a, &mut b, Duration::from_secs(10)).await;
            assert_eq!((ab, ba), (8, 8));
        });
        app_t.join().unwrap();
        echo_t.join().unwrap();
    }
}
