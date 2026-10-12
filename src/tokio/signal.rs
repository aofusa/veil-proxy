//! `tokio::signal` 互換（F-198、unix）。signal-hook でシグナルを数え、待っている Future を起こす。
//! veil 本体の SIGINT / SIGTERM / SIGHUP の扱い（終了・リロード）とは独立に受け取れる（signal-hook は複数登録できる）。

use super::sync::Notify;
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

struct Slot {
    count: AtomicU64,
    notify: Notify,
}

fn slot(sig: libc::c_int) -> io::Result<Arc<Slot>> {
    static SLOTS: OnceLock<Mutex<HashMap<libc::c_int, Arc<Slot>>>> = OnceLock::new();
    let mut m = SLOTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    if let Some(s) = m.get(&sig) {
        return Ok(s.clone());
    }
    let s = Arc::new(Slot {
        count: AtomicU64::new(0),
        notify: Notify::new(),
    });
    let mut signals = signal_hook::iterator::Signals::new([sig])?;
    let s2 = s.clone();
    std::thread::Builder::new()
        .name("veil-tokio-signal".into())
        .spawn(move || {
            for _ in signals.forever() {
                s2.count.fetch_add(1, Ordering::AcqRel);
                s2.notify.notify_waiters();
            }
        })?;
    m.insert(sig, s.clone());
    Ok(s)
}

/// Ctrl-C（SIGINT）を 1 回待つ。
pub async fn ctrl_c() -> io::Result<()> {
    let mut s = unix::signal(unix::SignalKind::interrupt())?;
    s.recv().await;
    Ok(())
}

pub mod unix {
    use super::*;

    /// シグナルの種類。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct SignalKind(libc::c_int);

    impl SignalKind {
        pub const fn from_raw(n: libc::c_int) -> SignalKind {
            SignalKind(n)
        }
        pub const fn as_raw_value(&self) -> libc::c_int {
            self.0
        }
        pub const fn alarm() -> SignalKind {
            SignalKind(libc::SIGALRM)
        }
        pub const fn child() -> SignalKind {
            SignalKind(libc::SIGCHLD)
        }
        pub const fn hangup() -> SignalKind {
            SignalKind(libc::SIGHUP)
        }
        pub const fn interrupt() -> SignalKind {
            SignalKind(libc::SIGINT)
        }
        pub const fn io() -> SignalKind {
            SignalKind(libc::SIGIO)
        }
        pub const fn pipe() -> SignalKind {
            SignalKind(libc::SIGPIPE)
        }
        pub const fn quit() -> SignalKind {
            SignalKind(libc::SIGQUIT)
        }
        pub const fn terminate() -> SignalKind {
            SignalKind(libc::SIGTERM)
        }
        pub const fn user_defined1() -> SignalKind {
            SignalKind(libc::SIGUSR1)
        }
        pub const fn user_defined2() -> SignalKind {
            SignalKind(libc::SIGUSR2)
        }
        pub const fn window_change() -> SignalKind {
            SignalKind(libc::SIGWINCH)
        }
    }

    impl From<libc::c_int> for SignalKind {
        fn from(n: libc::c_int) -> SignalKind {
            SignalKind(n)
        }
    }

    /// シグナルの流れ。
    pub struct Signal {
        slot: Arc<Slot>,
        seen: u64,
        pending: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + Sync>>>,
    }

    impl std::fmt::Debug for Signal {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Signal")
        }
    }

    /// `kind` のシグナルを受け取る。
    pub fn signal(kind: SignalKind) -> io::Result<Signal> {
        let slot = slot(kind.0)?;
        let seen = slot.count.load(Ordering::Acquire);
        Ok(Signal {
            slot,
            seen,
            pending: None,
        })
    }

    impl Signal {
        /// 次のシグナルを待つ（このレイヤーでは終わらない）。
        pub async fn recv(&mut self) -> Option<()> {
            std::future::poll_fn(|cx| self.poll_recv(cx)).await
        }

        pub fn poll_recv(
            &mut self,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<()>> {
            use std::task::Poll;
            loop {
                let c = self.slot.count.load(Ordering::Acquire);
                if c != self.seen {
                    self.seen = c;
                    self.pending = None;
                    return Poll::Ready(Some(()));
                }
                let slot = self.slot.clone();
                let fut = self
                    .pending
                    .get_or_insert_with(|| Box::pin(async move { slot.notify.notified().await }));
                if fut.as_mut().poll(cx).is_ready() {
                    self.pending = None;
                    continue;
                }
                // 待ちに入った後にもう一度（入る前に届いた分を取りこぼさない）
                if self.slot.count.load(Ordering::Acquire) != self.seen {
                    continue;
                }
                return Poll::Pending;
            }
        }
    }
}
