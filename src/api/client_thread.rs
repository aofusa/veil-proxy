// F-291: 外向きの呼び出し（HTTP・gRPC のクライアント）を、ランタイムを持たないスレッド（C・Python のハンドラ用プール、
// 利用者のスレッド）から使うための専用スレッド。veil のランタイム（runtime::block_on）を 1 本のスレッドで回し、
// 仕事はキューと WakeFlag で渡す（スレッドを越えて Waker を呼ばない）。結果は Mutex + Condvar（同期の待ち）か、
// ハンドラ用プールで呼ぶ閉包で返す。最初の利用で起動する。veil-python の gRPC クライアント（F-268）から切り出した。
//! A dedicated thread running veil's runtime for outgoing calls (the HTTP client and gRPC channels) made from
//! threads without a runtime: the handler pool of C and Python handlers, or the application's own threads.
//!
//! Work is handed over through a queue and a [`WakeFlag`] (no `Waker` is called across threads). Results come back
//! through [`run_blocking`] (the calling thread waits) or [`run_then`] (a function called on the handler pool).
//! The thread starts with the first call.

use crate::runtime::offload::WakeFlag;
use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

/// Work run on the client thread (it typically spawns a task with `veil::runtime::spawn`).
type Job = Box<dyn FnOnce() + Send>;

struct ClientRt {
    queue: Mutex<VecDeque<Job>>,
    wake: Arc<WakeFlag>,
}

static RT: OnceLock<Result<ClientRt, String>> = OnceLock::new();

fn rt() -> Result<&'static ClientRt, String> {
    RT.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Arc<WakeFlag>>();
        std::thread::Builder::new()
            .name("veil-client".into())
            .spawn(move || {
                crate::runtime::block_on(async move {
                    // 旗はこのスレッドで作る（このスレッドの通知 fd を覚える）
                    let wake = Arc::new(WakeFlag::new());
                    let _ = tx.send(Arc::clone(&wake));
                    let rt = loop {
                        if let Some(Ok(rt)) = RT.get() {
                            break rt;
                        }
                        crate::runtime::sleep(Duration::from_millis(1)).await;
                    };
                    loop {
                        wake.reset();
                        let jobs: Vec<Job> = rt.queue.lock().unwrap().drain(..).collect();
                        if jobs.is_empty() {
                            wake.wait().await;
                            continue;
                        }
                        for job in jobs {
                            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err()
                            {
                                ftlog::error!("[api] a client job panicked");
                            }
                        }
                    }
                })
            })
            .map_err(|e| e.to_string())?;
        let wake = rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "the client thread did not start".to_string())?;
        Ok(ClientRt {
            queue: Mutex::new(VecDeque::new()),
            wake,
        })
    })
    .as_ref()
    .map_err(|e| e.clone())
}

/// Starts the client thread now (otherwise it starts with the first call).
pub fn start() -> Result<(), String> {
    rt().map(|_| ())
}

/// Runs `job` on the client thread (inside its runtime, so it may call `veil::runtime::spawn`).
pub fn submit(job: impl FnOnce() + Send + 'static) -> Result<(), String> {
    let rt = rt()?;
    rt.queue.lock().unwrap().push_back(Box::new(job));
    rt.wake.notify();
    Ok(())
}

/// Spawns the future made by `make` on the client thread.
pub fn spawn<F, Fut>(make: F) -> Result<(), String>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    submit(move || {
        let fut = make();
        crate::runtime::spawn(async move {
            // 1 つの呼び出しの panic でクライアントのスレッドを止めない
            let r = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(fut)).await;
            if r.is_err() {
                ftlog::error!("[api] a client task panicked");
            }
        });
    })
}

/// Whether the current thread runs a veil event loop (a server worker or the client thread), where waiting for a
/// result would stall every task of the thread.
pub fn on_event_loop() -> bool {
    crate::runtime::has_driver()
}

/// Runs the future made by `make` on the client thread and waits for its result. Refused on a thread that runs
/// an event loop (see [`on_event_loop`]).
pub fn run_blocking<F, Fut, R>(make: F) -> Result<R, String>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = R> + 'static,
    R: Send + 'static,
{
    if on_event_loop() {
        return Err(
            "blocking client calls are not allowed on an event-loop thread (use the asynchronous form)"
                .into(),
        );
    }
    let shared = Arc::new((Mutex::new(None::<Option<R>>), Condvar::new()));
    let s2 = Arc::clone(&shared);
    spawn(move || async move {
        // 閉包・Future の panic でも待ち手を起こす（None）
        let guard = Finish(Some(Arc::clone(&s2)));
        let r = make().await;
        *s2.0.lock().unwrap() = Some(Some(r));
        s2.1.notify_all();
        drop(guard);
    })?;
    let mut g = shared.0.lock().unwrap();
    loop {
        if let Some(v) = g.take() {
            return v.ok_or_else(|| "the client call panicked".to_string());
        }
        g = shared.1.wait(g).unwrap();
    }
}

/// Wakes the waiter of [`run_blocking`] with "no result" when the task is dropped before it finished.
struct Finish<R>(Option<Arc<(Mutex<Option<Option<R>>>, Condvar)>>);

impl<R> Drop for Finish<R> {
    fn drop(&mut self) {
        if let Some(s) = self.0.take() {
            let mut g = s.0.lock().unwrap();
            if g.is_none() {
                *g = Some(None);
            }
            s.1.notify_all();
        }
    }
}

/// Runs the future made by `make` on the client thread and calls `then` with its result on the handler pool.
pub fn run_then<F, Fut, R, T>(make: F, then: T) -> Result<(), String>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = R> + 'static,
    R: Send + 'static,
    T: FnOnce(R) + Send + 'static,
{
    spawn(move || async move {
        let r = make().await;
        crate::runtime::offload::spawn_handler(move || then(r));
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn blocking_and_callback_results() {
        // ビルドのサンドボックスは io_uring を拒否する（使えなければ飛ばす）
        #[cfg(all(veil_rt_uring, target_os = "linux"))]
        if crate::runtime::ring::IoUring::new(8, 0).is_err() {
            eprintln!("io_uring unavailable; skipping");
            return;
        }
        assert_eq!(super::run_blocking(|| async { 40 + 2 }).unwrap(), 42);
        let (tx, rx) = std::sync::mpsc::channel();
        super::run_then(
            || async {
                crate::runtime::sleep(std::time::Duration::from_millis(5)).await;
                "late"
            },
            move |v| tx.send(v).unwrap(),
        )
        .unwrap();
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap(),
            "late"
        );
        // panic した呼び出しは待ち手に誤りとして返る
        let r: Result<u8, String> = super::run_blocking(|| async { panic!("boom on purpose") });
        assert!(r.is_err());
    }
}
