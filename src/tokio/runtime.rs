// F-198
//! Compatible with `tokio::runtime`.
//!
//! - `Runtime::block_on` runs veil's runtime (io_uring or the reactor) on the calling thread and drives the
//!   future. It may be called any number of times on the same thread; tasks spawned with `spawn` stay on the
//!   thread's executor and keep running in the next `block_on`.
//! - A multi-thread runtime owns worker threads (each with its own veil runtime) and assigns tasks spawned
//!   with `Handle::spawn` from outside the runtime to them in turn through the entry point (`waker::Injector`).
//!   Tasks do not move between threads.
//! - Inside a worker of the API library (a handler), that worker's thread is the current runtime.

use super::task::JoinHandle;
use super::waker::{current_injector, Injector, Job};
use std::cell::RefCell;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    CurrentThread,
    MultiThread,
}

/// The kind of runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeFlavor {
    CurrentThread,
    MultiThread,
}

struct HandleInner {
    kind: Kind,
    name: Option<String>,
    id: u64,
    /// `enable_time` / `enable_io` (when disabled, creating a timer or I/O object panics, as in tokio).
    time_enabled: bool,
    io_enabled: bool,
    /// `LocalRuntime` (accepts `spawn_local`).
    local: bool,
    /// Entry points of the workers (multi-thread) or of the thread running `block_on` (current thread).
    workers: Mutex<Vec<Arc<Injector>>>,
    /// Tasks sent before an entry point existed.
    pending: Mutex<Vec<Job>>,
    next: AtomicUsize,
    shutdown: AtomicBool,
}

/// `tokio::runtime::Handle`。
#[derive(Clone)]
pub struct Handle {
    inner: Arc<HandleInner>,
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle")
            .field("kind", &self.inner.kind)
            .finish()
    }
}

thread_local! {
    static CONTEXT: RefCell<Vec<Handle>> = const { RefCell::new(Vec::new()) };
    static IN_BLOCK_ON: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// This layer initialised the driver of this thread.
    static OWN_DRIVER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    // B-118 と同じ理由。
    /// Cleans up the tasks left at thread exit (the resident entry-point task and spawned tasks). It is touched
    /// and registered after the first `block_on`: thread-local destruction runs in reverse order of registration,
    /// so this runs before the runtime's thread-locals (run queue, op table).
    static DRIVER_GUARD: DriverGuard = const { DriverGuard };
}

struct DriverGuard;

impl Drop for DriverGuard {
    fn drop(&mut self) {
        if OWN_DRIVER.try_with(|o| o.get()).unwrap_or(false) {
            crate::runtime::shutdown_thread();
        }
    }
}

/// The ID of a runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Id(u64);

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// There is no `Handle` in context.
#[derive(Debug)]
pub struct TryCurrentError {
    _p: (),
}

impl std::fmt::Display for TryCurrentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "there is no reactor running, must be called from the context of a Tokio 1.x runtime",
        )
    }
}

impl std::error::Error for TryCurrentError {}

impl Handle {
    fn new(kind: Kind) -> Handle {
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        Handle {
            inner: Arc::new(HandleInner {
                kind,
                name: None,
                id: NEXT.fetch_add(1, Ordering::Relaxed) as u64,
                time_enabled: true,
                io_enabled: true,
                local: false,
                workers: Mutex::new(Vec::new()),
                pending: Mutex::new(Vec::new()),
                next: AtomicUsize::new(0),
                shutdown: AtomicBool::new(false),
            }),
        }
    }

    /// Returns the current `Handle`. Panics outside a runtime context.
    #[track_caller]
    pub fn current() -> Handle {
        match Handle::try_current() {
            Ok(h) => h,
            Err(e) => panic!("{}", e),
        }
    }

    pub fn try_current() -> Result<Handle, TryCurrentError> {
        if let Some(h) = CONTEXT.with(|c| c.borrow().last().cloned()) {
            return Ok(h);
        }
        // veil のランタイムのスレッド（API ライブラリのワーカー等）: そのスレッド自身
        if let Some(inj) = current_injector() {
            let h = Handle::new(Kind::CurrentThread);
            h.inner.workers.lock().unwrap().push(inj);
            return Ok(h);
        }
        Err(TryCurrentError { _p: () })
    }

    /// The runtime's name (`Builder::name`).
    pub fn name(&self) -> Option<&str> {
        self.inner.name.as_deref()
    }

    /// The runtime's ID.
    pub fn id(&self) -> Id {
        Id(self.inner.id)
    }

    pub fn runtime_flavor(&self) -> RuntimeFlavor {
        match self.inner.kind {
            Kind::CurrentThread => RuntimeFlavor::CurrentThread,
            Kind::MultiThread => RuntimeFlavor::MultiThread,
        }
    }

    /// Enters the context; while the guard lives, `Handle::current` and `spawn` use this `Handle`.
    pub fn enter(&self) -> EnterGuard<'_> {
        CONTEXT.with(|c| c.borrow_mut().push(self.clone()));
        EnterGuard {
            _p: std::marker::PhantomData,
        }
    }

    fn is_own_thread(&self) -> bool {
        if !crate::runtime::has_driver() {
            return false;
        }
        let me = std::thread::current().id();
        self.inner
            .workers
            .lock()
            .unwrap()
            .iter()
            .any(|w| w.owner() == me)
            || IN_BLOCK_ON.with(|b| b.get())
                && CONTEXT.with(|c| {
                    c.borrow()
                        .last()
                        .is_some_and(|h| Arc::ptr_eq(&h.inner, &self.inner))
                })
    }

    fn submit(&self, job: Job) {
        let mut job = job;
        let workers = self.inner.workers.lock().unwrap().clone();
        let n = workers.len();
        let start = self.inner.next.fetch_add(1, Ordering::Relaxed);
        for i in 0..n {
            match workers[(start + i) % n].push(job) {
                Ok(()) => return,
                // 閉じた受け口（スレッドが止まった）: 次へ
                Err(j) => job = j,
            }
        }
        // まだ受け口が無い（カレントスレッドのランタイムで block_on 前）: 次の block_on で動かす
        self.inner.pending.lock().unwrap().push(job);
    }

    /// `tokio::runtime::Handle::spawn`。
    #[track_caller]
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        if self.is_own_thread() {
            return super::task::spawn_here(future);
        }
        let (t, h) = super::task::task(future);
        self.submit(Job::Spawn(Box::pin(t)));
        h
    }

    pub fn spawn_blocking<F, R>(&self, f: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        super::task::spawn_blocking(f)
    }

    /// Runs `future` to completion on the calling thread. Panics when called from inside a runtime thread.
    #[track_caller]
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        run_block_on(self, future)
    }
}

/// Guard returned by `Handle::enter` / `Runtime::enter`.
pub struct EnterGuard<'a> {
    _p: std::marker::PhantomData<&'a Handle>,
}

impl Drop for EnterGuard<'_> {
    fn drop(&mut self) {
        CONTEXT.with(|c| {
            c.borrow_mut().pop();
        });
    }
}

/// Whether the context is a current-thread runtime (`block_in_place` is not allowed there).
pub(crate) fn in_current_thread_runtime() -> bool {
    CONTEXT
        .try_with(|c| {
            c.borrow()
                .last()
                .map(|h| h.inner.kind == Kind::CurrentThread)
        })
        .ok()
        .flatten()
        .unwrap_or(false)
}

/// Whether `spawn_local` is allowed in this context: in a tokio runtime context only a `LocalRuntime` allows it;
/// a veil worker with no context (a handler of the API library) allows it.
pub(crate) fn in_local_capable_context() -> bool {
    CONTEXT
        .try_with(|c| c.borrow().last().map(|h| h.inner.local))
        .ok()
        .flatten()
        .unwrap_or_else(crate::runtime::has_driver)
}

/// Whether timers can be used in the context's runtime (they can when there is no context, such as on a veil worker).
pub(crate) fn time_enabled_in_context() -> bool {
    CONTEXT
        .try_with(|c| c.borrow().last().map(|h| h.inner.time_enabled))
        .ok()
        .flatten()
        .unwrap_or(true)
}

/// Whether I/O can be used in the context's runtime (used by `net`).
pub(crate) fn io_enabled_in_context() -> bool {
    CONTEXT
        .try_with(|c| c.borrow().last().map(|h| h.inner.io_enabled))
        .ok()
        .flatten()
        .unwrap_or(true)
}

/// Makes veil's runtime usable on this thread (initialised once).
fn ensure_driver() {
    if crate::runtime::has_driver() {
        return;
    }
    #[cfg(veil_rt_uring)]
    crate::runtime::executor::init_ring(crate::runtime::DEFAULT_RING_ENTRIES, 0)
        .expect("failed to initialize io_uring for the tokio compatibility runtime");
    #[cfg(veil_rt_reactor)]
    crate::runtime::executor::init_reactor()
        .expect("failed to initialize the reactor for the tokio compatibility runtime");
    crate::runtime::executor::init_executor();
    OWN_DRIVER.with(|o| o.set(true));
}

#[track_caller]
fn run_block_on<F: Future>(handle: &Handle, future: F) -> F::Output {
    if IN_BLOCK_ON.with(|b| b.get())
        || (crate::runtime::has_driver() && !OWN_DRIVER.with(|o| o.get()))
    {
        panic!(
            "Cannot start a runtime from within a runtime. This happens because a function (like \
             `block_on`) attempted to block the current thread while the thread is being used to \
             drive asynchronous tasks."
        );
    }
    ensure_driver();
    // カレントスレッドのランタイム: このスレッドが受け口になる
    if handle.inner.kind == Kind::CurrentThread {
        if let Some(inj) = current_injector() {
            let mut ws = handle.inner.workers.lock().unwrap();
            ws.retain(|w| w.owner() != inj.owner() || Arc::ptr_eq(w, &inj));
            if !ws.iter().any(|w| Arc::ptr_eq(w, &inj)) {
                ws.push(inj.clone());
            }
            drop(ws);
            for job in std::mem::take(&mut *handle.inner.pending.lock().unwrap()) {
                if let Job::Spawn(f) = job {
                    crate::runtime::spawn(f);
                }
            }
        }
    }
    let _ctx = handle.enter();
    IN_BLOCK_ON.with(|b| b.set(true));
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            IN_BLOCK_ON.with(|b| b.set(false));
        }
    }
    let _reset = Reset;

    // veil の block_on は 'static を要求する。`future` は block_on が戻る前に必ず完了か drop される
    // （panic も中で受け止めてから外へ投げ直す）ので、借用の寿命を延ばしてよい。
    // 別スレッドから起こされても所有スレッドで再開するように Waker を包む（他のクレートのチャネル等）
    let future = super::waker::WithSafeWaker::new(future);
    let fut =
        async move { futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(future)).await };
    let boxed: std::pin::Pin<Box<dyn Future<Output = std::thread::Result<F::Output>> + '_>> =
        Box::pin(fut);
    // SAFETY: 上記のとおり、このスタックフレームの中で完了・drop される
    let boxed: std::pin::Pin<Box<dyn Future<Output = std::thread::Result<F::Output>> + 'static>> =
        unsafe { std::mem::transmute(boxed) };
    let slot: std::rc::Rc<RefCell<Option<std::thread::Result<F::Output>>>> =
        std::rc::Rc::new(RefCell::new(None));
    let slot2 = slot.clone();
    // 出力型も 'static でないことがあるので、結果は外側のスロットへ書く
    let runner: std::pin::Pin<Box<dyn Future<Output = ()> + '_>> = Box::pin(async move {
        let r = boxed.await;
        *slot2.borrow_mut() = Some(r);
    });
    // SAFETY: 同上（runner は block_on の中で完了する）
    let runner: std::pin::Pin<Box<dyn Future<Output = ()> + 'static>> =
        unsafe { std::mem::transmute(runner) };
    crate::runtime::executor::current_executor().block_on(runner);
    DRIVER_GUARD.with(|_| {});
    let r = slot
        .borrow_mut()
        .take()
        .expect("block_on finished without a result");
    match r {
        Ok(v) => v,
        Err(p) => std::panic::resume_unwind(p),
    }
}

// ===================== Runtime =====================

/// `tokio::runtime::Runtime`。
pub struct Runtime {
    handle: Handle,
    // Mutex に入れて RefUnwindSafe にする（catch_unwind の中で &Runtime を使えるように）
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    stop: Arc<Mutex<Vec<crate::runtime::offload::Completer<()>>>>,
    owner: std::thread::ThreadId,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("flavor", &self.handle.runtime_flavor())
            .finish()
    }
}

impl Runtime {
    /// A multi-thread runtime, equivalent to `Builder::new_multi_thread().enable_all().build()`.
    pub fn new() -> std::io::Result<Runtime> {
        Builder::new_multi_thread().enable_all().build()
    }

    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    #[track_caller]
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        run_block_on(&self.handle, future)
    }

    #[track_caller]
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.handle.spawn(future)
    }

    pub fn spawn_blocking<F, R>(&self, f: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        super::task::spawn_blocking(f)
    }

    pub fn enter(&self) -> EnterGuard<'_> {
        self.handle.enter()
    }

    pub fn shutdown_timeout(self, _d: Duration) {
        drop(self);
    }

    pub fn shutdown_background(self) {
        drop(self);
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.handle.inner.shutdown.store(true, Ordering::Relaxed);
        for c in std::mem::take(&mut *self.stop.lock().unwrap()) {
            c.complete(());
        }
        for t in self.threads.get_mut().unwrap().drain(..) {
            let _ = t.join();
        }
        // ランタイムを作ったスレッド（block_on したスレッド）で drop したら、そこに残ったタスクを片付ける
        // （tokio と同じく取り消し）
        if std::thread::current().id() == self.owner
            && OWN_DRIVER.with(|o| o.get())
            && !IN_BLOCK_ON.with(|b| b.get())
        {
            crate::runtime::shutdown_thread();
        }
        self.handle.inner.workers.lock().unwrap().clear();
    }
}

/// Equivalent of `tokio::runtime::LocalRuntime`: a current-thread runtime that can also spawn tasks that are not `Send`.
pub struct LocalRuntime {
    rt: Runtime,
    queue: RefCell<Vec<std::pin::Pin<Box<dyn Future<Output = ()>>>>>,
}

impl std::fmt::Debug for LocalRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalRuntime")
    }
}

/// Settings for `Builder::build_local` (for compatibility).
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct LocalOptions {}

impl LocalRuntime {
    pub fn new() -> std::io::Result<LocalRuntime> {
        Builder::new_current_thread()
            .enable_all()
            .build_local(LocalOptions::default())
    }

    pub fn handle(&self) -> &Handle {
        self.rt.handle()
    }

    pub fn spawn_local<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        if IN_BLOCK_ON.with(|b| b.get()) && crate::runtime::has_driver() {
            return super::task::spawn_here(future);
        }
        let (t, h) = super::task::task(future);
        self.queue.borrow_mut().push(Box::pin(t));
        h
    }

    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.spawn_local(future)
    }

    pub fn spawn_blocking<F, R>(&self, f: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        super::task::spawn_blocking(f)
    }

    #[track_caller]
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        let queued: Vec<_> = self.queue.borrow_mut().drain(..).collect();
        self.rt.block_on(async move {
            for t in queued {
                crate::runtime::spawn(t);
            }
            future.await
        })
    }

    pub fn enter(&self) -> EnterGuard<'_> {
        self.rt.enter()
    }
}

/// Equivalent of `tokio::runtime::Builder`. Settings such as enabling I/O or the clock are accepted but have no effect (both are always enabled).
pub struct Builder {
    kind: Kind,
    worker_threads: Option<usize>,
    thread_name: String,
    start_paused: bool,
    name: Option<String>,
    enable_time: bool,
    enable_io: bool,
}

impl std::fmt::Debug for Builder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Builder").field("kind", &self.kind).finish()
    }
}

macro_rules! accept {
    ($($name:ident($($a:ident: $t:ty),*)),* $(,)?) => {
        $(
            #[doc = concat!("Accepted for compatibility; `", stringify!($name), "` has no effect.")]
            pub fn $name(&mut self, $(_ : $t),*) -> &mut Self { $(let _ = stringify!($a);)* self }
        )*
    };
}

impl Builder {
    pub fn new_current_thread() -> Builder {
        Builder {
            kind: Kind::CurrentThread,
            worker_threads: None,
            thread_name: "tokio-runtime-worker".into(),
            start_paused: false,
            name: None,
            enable_time: false,
            enable_io: false,
        }
    }

    pub fn new_multi_thread() -> Builder {
        Builder {
            kind: Kind::MultiThread,
            worker_threads: None,
            thread_name: "tokio-runtime-worker".into(),
            start_paused: false,
            name: None,
            enable_time: false,
            enable_io: false,
        }
    }

    pub fn worker_threads(&mut self, n: usize) -> &mut Self {
        assert!(n > 0, "Worker threads cannot be set to 0");
        self.worker_threads = Some(n);
        self
    }

    /// The runtime's name (readable through `Handle::name`).
    pub fn name(&mut self, name: impl Into<String>) -> &mut Self {
        self.name = Some(name.into());
        self
    }

    pub fn thread_name(&mut self, name: impl Into<String>) -> &mut Self {
        self.thread_name = name.into();
        self
    }

    /// Starts the runtime with the clock paused (the test-util behaviour). Requires the time driver: `build`
    /// panics when `enable_time` is not set.
    pub fn start_paused(&mut self, paused: bool) -> &mut Self {
        self.start_paused = paused;
        self
    }

    pub fn enable_all(&mut self) -> &mut Self {
        self.enable_time = true;
        self.enable_io = true;
        self
    }

    pub fn enable_io(&mut self) -> &mut Self {
        self.enable_io = true;
        self
    }

    pub fn enable_time(&mut self) -> &mut Self {
        self.enable_time = true;
        self
    }

    accept! {
        thread_stack_size(v: usize),
        max_blocking_threads(v: usize),
        event_interval(v: u32),
        global_queue_interval(v: u32),
        max_io_events_per_tick(v: usize),
        thread_keep_alive(v: Duration),
        disable_lifo_slot(),
    }

    pub fn thread_name_fn<F>(&mut self, _f: F) -> &mut Self
    where
        F: Fn() -> String + Send + Sync + 'static,
    {
        self
    }

    pub fn on_thread_start<F>(&mut self, _f: F) -> &mut Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        self
    }

    pub fn on_thread_stop<F>(&mut self, _f: F) -> &mut Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        self
    }

    /// Builds a `LocalRuntime` (always on the current thread, regardless of the runtime kind).
    pub fn build_local(&mut self, _options: LocalOptions) -> std::io::Result<LocalRuntime> {
        self.kind = Kind::CurrentThread;
        let mut rt = self.build()?;
        Arc::get_mut(&mut rt.handle.inner)
            .expect("fresh handle")
            .local = true;
        Ok(LocalRuntime {
            rt,
            queue: RefCell::new(Vec::new()),
        })
    }

    pub fn build(&mut self) -> std::io::Result<Runtime> {
        if self.start_paused {
            if !self.enable_time {
                panic!(
                    "start_paused requires the time driver (call `enable_time` or `enable_all`)"
                );
            }
            super::time::pause_if_running();
        }
        let mut handle = Handle::new(self.kind);
        {
            let inner = Arc::get_mut(&mut handle.inner).expect("fresh handle");
            inner.name = self.name.clone();
            inner.time_enabled = self.enable_time;
            inner.io_enabled = self.enable_io;
        }
        let stop: Arc<Mutex<Vec<crate::runtime::offload::Completer<()>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let mut threads = Vec::new();
        if self.kind == Kind::MultiThread {
            let n = self
                .worker_threads
                .unwrap_or_else(|| num_cpus::get().max(1));
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            for i in 0..n {
                let h = handle.clone();
                let stop = stop.clone();
                let tx = tx.clone();
                let t = std::thread::Builder::new()
                    .name(format!("{}-{}", self.thread_name, i))
                    .spawn(move || worker_main(h, stop, tx))?;
                threads.push(t);
            }
            drop(tx);
            // 全ワーカーの受け口ができるまで待つ（その前の spawn は pending に積まれる）
            for _ in 0..n {
                let _ = rx.recv();
            }
        }
        Ok(Runtime {
            handle,
            threads: Mutex::new(threads),
            stop,
            owner: std::thread::current().id(),
        })
    }
}

fn worker_main(
    handle: Handle,
    stop: Arc<Mutex<Vec<crate::runtime::offload::Completer<()>>>>,
    ready: std::sync::mpsc::Sender<()>,
) {
    crate::runtime::block_on(async move {
        let inj = current_injector().expect("driver initialized by block_on");
        handle.inner.workers.lock().unwrap().push(inj);
        let pending = std::mem::take(&mut *handle.inner.pending.lock().unwrap());
        for job in pending {
            if let Job::Spawn(f) = job {
                crate::runtime::spawn(f);
            }
        }
        CONTEXT.with(|c| c.borrow_mut().push(handle.clone()));
        let _ = ready.send(());
        if !handle.inner.shutdown.load(Ordering::Relaxed) {
            let _ =
                crate::runtime::offload::await_completion(|c| stop.lock().unwrap().push(c)).await;
        }
        CONTEXT.with(|c| c.borrow_mut().clear());
    });
    crate::runtime::shutdown_thread();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_thread_block_on_and_spawn() {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        let v = rt.block_on(async {
            let h = super::super::task::spawn(async { 40 + 2 });
            h.await.unwrap()
        });
        assert_eq!(v, 42);
        // 2 回目の block_on（同じスレッド）
        let s = String::from("borrowed");
        let n = rt.block_on(async { s.len() });
        assert_eq!(n, 8);
    }

    #[test]
    fn multi_thread_spawn_from_outside_and_cross_thread_wake() {
        let rt = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let h = rt.spawn(async {
            super::super::time::sleep(Duration::from_millis(10)).await;
            std::thread::current().name().map(|s| s.to_string())
        });
        let name = rt.block_on(h).unwrap();
        assert!(name.unwrap().starts_with("tokio-runtime-worker"));
    }

    #[test]
    fn panics_propagate_through_join_handle() {
        let rt = Builder::new_current_thread().build().unwrap();
        let r = rt.block_on(async { super::super::task::spawn(async { panic!("boom") }).await });
        let e = r.unwrap_err();
        assert!(e.is_panic());
        assert!(e.to_string().contains("boom"));
    }
}
