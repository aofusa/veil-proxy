// F-198（Windows は F-204）。
//! Compatible with `tokio::signal`.
//!
//! - unix: counts signals with signal-hook and wakes the waiting futures. It is independent of veil's own
//!   handling of SIGINT, SIGTERM and SIGHUP (shutdown and reload), because signal-hook allows several registrations.
//! - Windows: a `SetConsoleCtrlHandler` handler counts events per kind (Ctrl-C, Ctrl-Break, close, logoff,
//!   shutdown). It returns "handled" (TRUE) only for kinds that have a receiver; otherwise it passes the
//!   event to the next handler (veil's own shutdown handling).

use super::sync::Notify;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

struct Slot {
    count: AtomicU64,
    notify: Notify,
    /// Number of live receivers (Windows: when 0, the handler returns FALSE and the default handling runs).
    #[cfg(windows)]
    listeners: std::sync::atomic::AtomicUsize,
}

impl Slot {
    fn new() -> Slot {
        Slot {
            count: AtomicU64::new(0),
            notify: Notify::new(),
            #[cfg(windows)]
            listeners: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn deliver(&self) {
        self.count.fetch_add(1, Ordering::AcqRel);
        self.notify.notify_waiters();
    }
}

/// Receives one kind of signal (remembers a count and returns once each time it grows).
struct Receiver {
    slot: Arc<Slot>,
    seen: u64,
    pending: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + Sync>>>,
}

impl Receiver {
    fn new(slot: Arc<Slot>) -> Receiver {
        #[cfg(windows)]
        slot.listeners.fetch_add(1, Ordering::AcqRel);
        let seen = slot.count.load(Ordering::Acquire);
        Receiver {
            slot,
            seen,
            pending: None,
        }
    }

    fn poll_recv(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<()>> {
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

#[cfg(windows)]
impl Drop for Receiver {
    fn drop(&mut self) {
        self.slot.listeners.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(unix)]
fn slot(sig: libc::c_int) -> io::Result<Arc<Slot>> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static SLOTS: OnceLock<Mutex<HashMap<libc::c_int, Arc<Slot>>>> = OnceLock::new();
    let mut m = SLOTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    if let Some(s) = m.get(&sig) {
        return Ok(s.clone());
    }
    let s = Arc::new(Slot::new());
    let mut signals = signal_hook::iterator::Signals::new([sig])?;
    let s2 = s.clone();
    std::thread::Builder::new()
        .name("veil-tokio-signal".into())
        .spawn(move || {
            for _ in signals.forever() {
                s2.deliver();
            }
        })?;
    m.insert(sig, s.clone());
    Ok(s)
}

/// Waits once for Ctrl-C (SIGINT on unix, `CTRL_C_EVENT` on Windows).
pub async fn ctrl_c() -> io::Result<()> {
    #[cfg(unix)]
    let mut s = unix::signal(unix::SignalKind::interrupt())?;
    #[cfg(windows)]
    let mut s = windows::ctrl_c()?;
    s.recv().await;
    Ok(())
}

#[cfg(unix)]
pub mod unix {
    use super::*;

    /// A kind of signal.
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

    /// A stream of signals.
    pub struct Signal(Receiver);

    impl std::fmt::Debug for Signal {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Signal")
        }
    }

    /// Receives signals of the given `kind`.
    pub fn signal(kind: SignalKind) -> io::Result<Signal> {
        Ok(Signal(Receiver::new(slot(kind.0)?)))
    }

    impl Signal {
        /// Waits for the next signal (never ends with `None` in this layer).
        pub async fn recv(&mut self) -> Option<()> {
            std::future::poll_fn(|cx| self.poll_recv(cx)).await
        }

        pub fn poll_recv(
            &mut self,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<()>> {
            self.0.poll_recv(cx)
        }
    }
}

/// Equivalent of `tokio::signal::windows` (console control events).
#[cfg(windows)]
pub mod windows {
    use super::*;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{BOOL, FALSE, TRUE};
    use windows_sys::Win32::System::Console::{
        SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT,
        CTRL_SHUTDOWN_EVENT,
    };

    const KINDS: [u32; 5] = [
        CTRL_C_EVENT,
        CTRL_BREAK_EVENT,
        CTRL_CLOSE_EVENT,
        CTRL_LOGOFF_EVENT,
        CTRL_SHUTDOWN_EVENT,
    ];

    fn slots() -> &'static [Arc<Slot>; 5] {
        static SLOTS: OnceLock<[Arc<Slot>; 5]> = OnceLock::new();
        SLOTS.get_or_init(|| std::array::from_fn(|_| Arc::new(Slot::new())))
    }

    /// Console control handler (called on a thread created by the system).
    unsafe extern "system" fn handler(ctrl: u32) -> BOOL {
        let Some(i) = KINDS.iter().position(|&k| k == ctrl) else {
            return FALSE;
        };
        let slot = &slots()[i];
        if slot.listeners.load(Ordering::Acquire) == 0 {
            // 受け手がいない: 次のハンドラ（veil 本体の ctrlc、無ければ既定の終了）へ
            return FALSE;
        }
        slot.deliver();
        if matches!(
            ctrl,
            CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT
        ) {
            // これらはハンドラから戻るとプロセスが終わるので、アプリの後始末を待つ（tokio と同じ。
            // システムの猶予時間が過ぎると OS が終了させる）。
            // 理由付き allow: ランタイムの外の、コンソールの制御イベント専用のスレッド
            #[allow(clippy::disallowed_methods)]
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        }
        TRUE
    }

    fn register(kind: usize) -> io::Result<Receiver> {
        static INSTALLED: OnceLock<io::Result<()>> = OnceLock::new();
        let r = INSTALLED.get_or_init(|| {
            // SAFETY: handler は 'static の関数
            if unsafe { SetConsoleCtrlHandler(Some(handler), TRUE) } == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
        if let Err(e) = r {
            return Err(io::Error::new(e.kind(), e.to_string()));
        }
        Ok(Receiver::new(slots()[kind].clone()))
    }

    macro_rules! ctrl_kind {
        ($ty:ident, $f:ident, $idx:expr, $doc:literal) => {
            #[doc = $doc]
            pub struct $ty(Receiver);

            impl std::fmt::Debug for $ty {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str(stringify!($ty))
                }
            }

            impl $ty {
                /// Waits for the next event (never ends with `None` in this layer).
                pub async fn recv(&mut self) -> Option<()> {
                    std::future::poll_fn(|cx| self.poll_recv(cx)).await
                }

                pub fn poll_recv(
                    &mut self,
                    cx: &mut std::task::Context<'_>,
                ) -> std::task::Poll<Option<()>> {
                    self.0.poll_recv(cx)
                }
            }

            #[doc = $doc]
            pub fn $f() -> io::Result<$ty> {
                Ok($ty(register($idx)?))
            }
        };
    }

    ctrl_kind!(CtrlC, ctrl_c, 0, "Ctrl-C (`CTRL_C_EVENT`).");
    ctrl_kind!(CtrlBreak, ctrl_break, 1, "Ctrl-Break (`CTRL_BREAK_EVENT`).");
    ctrl_kind!(
        CtrlClose,
        ctrl_close,
        2,
        "The console was closed (`CTRL_CLOSE_EVENT`)."
    );
    ctrl_kind!(
        CtrlLogoff,
        ctrl_logoff,
        3,
        "Logoff (`CTRL_LOGOFF_EVENT`; only services receive it)."
    );
    ctrl_kind!(
        CtrlShutdown,
        ctrl_shutdown,
        4,
        "Shutdown (`CTRL_SHUTDOWN_EVENT`; only services receive it)."
    );
}
