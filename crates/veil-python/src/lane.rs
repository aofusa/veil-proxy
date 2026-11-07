// F-299: ブロッキングな Python ハンドラを動かすスレッド。GIL は同時に 1 つしか走らないので、要求ごとにスレッドを
// 起こして GIL を奪い合わせると、Python の仕事より受け渡し（futex・GIL の切り替え）の方が長くなる。
//
// 列を回す「走者」は GIL を握ったまま続けて仕事を取り、控えは 1 本だけ GIL を待つ。走者の仕事が GIL を手放す
// （I/O・`time.sleep`）と控えがすぐに GIL を取って列を引き継ぐので、ブロッキングなハンドラはこれまでどおり
// スレッド数まで同時に動く。残りのスレッドは眠ったままで、負荷の下では投入口が futex を呼ばない。
//
// 状態のロックを握ったまま GIL を待たない（GIL を握るスレッドもこのロックを取る）。
//! Threads that run blocking Python handlers.
//!
//! One thread (the runner) keeps the GIL and takes jobs back to back; one more waits for the GIL and takes over as
//! soon as a job releases it, so handlers that block still run up to the configured number of threads at a time.

use pyo3::ffi;
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use std::cell::Cell;
use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Once};
use std::thread::Thread;
use std::time::{Duration, Instant};
use veil::api::bridge::BlockingExecutor;

type Job = Box<dyn FnOnce() + Send + 'static>;

// サーバの停止で仕事の終わりを待つ長さ（止まらないハンドラがあっても `run` は戻る）
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct State {
    queue: VecDeque<Job>,
    // GIL を待っているスレッド（控え）
    followers: usize,
    // 列を回しているスレッド（仕事の中で GIL を手放しているものも数える）
    runners: usize,
    // 眠っているスレッド。最後に眠ったものから起こす（CPU とキャッシュが温まっている）
    idle: Vec<Thread>,
    // 起こしたが、まだ動き出していないスレッド
    woken: usize,
    // 列を回し始めた回数（仕事の間に別のスレッドが引き継いだかを見る）
    takeovers: u64,
    threads: usize,
    closing: bool,
}

impl State {
    // GIL へ向かうスレッドが居なければ 1 本起こす（居れば走者か控えが列を見る。負荷の下ではここで futex を呼ばない）。
    // 起こすのはロックを手放してから
    fn follower_to_wake(&mut self) -> Option<Thread> {
        if self.followers + self.woken > 0 {
            return None;
        }
        let thread = self.idle.pop()?;
        self.woken += 1;
        Some(thread)
    }
}

/// The threads of one server run.
pub(crate) struct Lane {
    state: Mutex<State>,
    stopped: Condvar,
    start: Once,
}

impl Lane {
    pub(crate) fn new() -> Arc<Lane> {
        Arc::new(Lane {
            state: Mutex::new(State::default()),
            stopped: Condvar::new(),
            start: Once::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queue a job. A job dropped without running answers its caller (see `call_blocking_via`).
    pub(crate) fn submit(self: &Arc<Self>, job: Job) {
        // スレッド数は `app.run()` が設定を読んだ後に決まるので、最初の要求で起こす
        self.start.call_once(|| self.spawn_threads());
        let mut s = self.lock();
        if s.closing && s.threads == 0 {
            return;
        }
        s.queue.push_back(job);
        let wake = s.follower_to_wake();
        drop(s);
        if let Some(thread) = wake {
            thread.unpark();
        }
    }

    fn spawn_threads(self: &Arc<Self>) {
        let n = veil::runtime::offload::handler_pool_threads();
        {
            let mut s = self.lock();
            s.threads = n;
            s.idle.reserve_exact(n);
        }
        for _ in 0..n {
            let lane = Arc::clone(self);
            std::thread::Builder::new()
                .name("veil-handler".to_string())
                .spawn(move || lane.thread_main())
                .unwrap_or_else(|e| panic!("failed to spawn veil-handler thread: {}", e));
        }
    }

    fn thread_main(self: Arc<Self>) {
        // スレッド状態はここで作る（要求の経路で作らない）
        pin_thread_state();
        let me = std::thread::current();
        let mut s = self.lock();
        loop {
            if s.queue.is_empty() || s.followers + s.woken > 0 {
                if s.closing {
                    break;
                }
                s.idle.push(me.clone());
                s = self.sleep(s, &me);
            }
            s.followers += 1;
            drop(s);
            Python::attach(|_py| self.run_queue());
            s = self.lock();
        }
        s.threads -= 1;
        if s.threads == 0 {
            self.stopped.notify_all();
        }
    }

    // 起こされるまで眠る（`idle` から取り除かれたら起こされた）
    fn sleep<'a>(&'a self, mut s: MutexGuard<'a, State>, me: &Thread) -> MutexGuard<'a, State> {
        while s.idle.iter().any(|t| t.id() == me.id()) {
            drop(s);
            std::thread::park();
            s = self.lock();
        }
        s.woken -= 1;
        s
    }

    // GIL を握って呼ばれる。列が空になるか、別のスレッドに引き継がれたら戻る（GIL は呼び出し側が手放す）
    fn run_queue(&self) {
        let mut s = self.lock();
        s.followers -= 1;
        s.runners += 1;
        s.takeovers += 1;
        let mut mine = s.takeovers;
        while let Some(job) = s.queue.pop_front() {
            // この仕事が GIL を手放したら控えが列を回す
            let wake = if s.queue.is_empty() {
                None
            } else {
                s.follower_to_wake()
            };
            drop(s);
            if let Some(thread) = wake {
                thread.unpark();
            }
            // panic でスレッドを失わない（閉包が落ちると待ち手へ応答が返る）
            let _ = catch_unwind(AssertUnwindSafe(job));
            s = self.lock();
            if s.takeovers != mine {
                if s.runners > 1 {
                    // 仕事の間に別のスレッドが引き継いだので、列はそちらに任せる
                    break;
                }
                mine = s.takeovers;
            }
        }
        s.runners -= 1;
    }

    /// Stop the threads once the server stopped (the GIL must not be held: the threads need it to finish).
    pub(crate) fn stop(&self) {
        let mut s = self.lock();
        s.closing = true;
        let sleeping = std::mem::take(&mut s.idle);
        s.woken += sleeping.len();
        drop(s);
        for thread in sleeping {
            thread.unpark();
        }
        let deadline = Instant::now() + STOP_TIMEOUT;
        let mut s = self.lock();
        while s.threads > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            s = self
                .stopped
                .wait_timeout(s, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

thread_local! {
    static PINNED: Cell<bool> = const { Cell::new(false) };
}

// B-334: Python が作っていないスレッドの `PyGILState_Ensure` は、対の `PyGILState_Release` でスレッド状態を壊す。
// 要求ごとに作り直すと `mmap`・`munmap`・`gettid` が付いてくるので、最初の 1 回を対にせず残す。
// スレッドの終わりでも消さない（veil のスレッドはプロセスと同じ長さ生き、終了処理の中の `PyGILState_Ensure` は
// 安全でない）。インタプリタの終了で片付く。
fn pin_thread_state() {
    if PINNED.get() {
        return;
    }
    // SAFETY: インタプリタは初期化済み（`run` の中でだけ呼ばれる）。
    unsafe {
        let state = ffi::PyGILState_Ensure();
        if matches!(state, ffi::PyGILState_STATE::PyGILState_UNLOCKED) {
            ffi::PyEval_SaveThread();
        }
    }
    PINNED.set(true);
}

/// `Python::attach` for a thread that calls into Python again and again: the thread state stays.
pub(crate) fn attach<F, R>(f: F) -> R
where
    F: for<'py> FnOnce(Python<'py>) -> R,
{
    pin_thread_state();
    Python::attach(f)
}

static CONTEXT: PyOnceLock<Py<PyAny>> = PyOnceLock::new();

// スレッド状態を残すと `contextvars` の文脈もスレッドに残るので、要求ごとに新しい文脈で呼ぶ（別の要求の値を見せない）
/// Call `f(arg)` in a new `contextvars.Context`, as if on a thread of its own.
pub(crate) fn call_in_new_context<'py>(
    py: Python<'py>,
    f: &Py<PyAny>,
    arg: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let context = CONTEXT
        .get_or_try_init(py, || {
            py.import("contextvars")?
                .getattr("Context")
                .map(Bound::unbind)
        })?
        .bind(py)
        .call0()?;
    context.call_method1(intern!(py, "run"), (f.bind(py), arg))
}

/// Queues a job on a [`Lane`].
pub(crate) struct OnLane(pub(crate) Arc<Lane>);

impl BlockingExecutor for OnLane {
    fn execute<J: FnOnce() + Send + 'static>(self, job: J) {
        self.0.submit(Box::new(job));
    }
}
