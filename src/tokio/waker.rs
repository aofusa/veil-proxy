// F-198: veil のランタイム（executor・Waker の vtable・io_uring／reactor）には手を入れない。
//! Delivers cross-thread wake-ups and spawns to the owning thread.
//!
//! A veil `Waker` may only be woken on the worker thread that owns it (see the soundness section in
//! `runtime/uring/executor.rs`). The tokio API (`sync`, `JoinHandle`, timers and so on) wakes tasks
//! from other threads, so this module wraps wakers:
//!
//! - [`safe_waker`]: wraps the `Waker` of the current thread (the thread that has the runtime driver)
//!   into a `Waker` that may be called from any thread. Called on the owning thread, it calls the
//!   original `Waker` directly; called on another thread, it queues a request on the owning thread's
//!   [`Injector`] to call that `Waker`.
//! - [`Injector`]: one per thread. A resident task on the owning thread waits through
//!   `runtime::offload::await_completion` (a result slot plus a notification fd) and runs the queued
//!   wake-ups and spawns on the owning thread.
//!
//! It uses only the existing `await_completion`, `spawn` and `has_driver`.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{RawWaker, RawWakerVTable, Waker};
use std::thread::ThreadId;

/// Work to run on the owning thread.
pub(crate) enum Job {
    /// Calls the owning thread's `Waker` (a wake-up from another thread).
    Wake(Waker),
    /// Starts a task on the owning thread (a spawn from another thread).
    Spawn(Pin<Box<dyn Future<Output = ()> + Send + 'static>>),
}

struct InjectorState {
    jobs: VecDeque<Job>,
    /// Wait of the resident task on the owning thread (completed and woken when work is queued).
    waiter: Option<crate::runtime::offload::Completer<()>>,
    /// The resident task has finished (the thread's runtime has stopped).
    closed: bool,
}

/// Per-thread entry point.
pub(crate) struct Injector {
    owner: ThreadId,
    state: Mutex<InjectorState>,
}

impl Injector {
    /// Queues work and wakes the owning thread. Returns the work if the owning thread has stopped.
    pub(crate) fn push(&self, job: Job) -> Result<(), Job> {
        let waiter = {
            let mut st = self.state.lock().unwrap();
            if st.closed {
                return Err(job);
            }
            st.jobs.push_back(job);
            st.waiter.take()
        };
        if let Some(c) = waiter {
            c.complete(());
        }
        Ok(())
    }

    pub(crate) fn owner(&self) -> ThreadId {
        self.owner
    }
}

thread_local! {
    static INJECTOR: std::cell::RefCell<Option<Arc<Injector>>> = const { std::cell::RefCell::new(None) };
    /// Thread-local destruction has started (inside `process::exit`, or at thread exit).
    static EXITING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Sentinel that signals the start of destruction. It is registered after the runtime's thread-locals
    /// (op table and so on; see [`touch_exit_sentinel`]), so destruction runs it first (reverse order of registration).
    static EXIT_SENTINEL: ExitSentinel = const { ExitSentinel };
}

struct ExitSentinel;

impl Drop for ExitSentinel {
    fn drop(&mut self) {
        let _ = EXITING.try_with(|e| e.set(true));
    }
}

/// Registers the sentinel (call it after the runtime's I/O has been used once).
pub(crate) fn touch_exit_sentinel() {
    let _ = EXIT_SENTINEL.try_with(|_| {});
}

/// Whether thread-local destruction is in progress (the runtime's tables and queues must not be touched then).
pub(crate) fn exiting() -> bool {
    EXITING.try_with(|e| e.get()).unwrap_or(true)
}

/// Closes the entry point when the resident task is dropped (the thread's runtime has stopped).
struct CloseOnDrop(Arc<Injector>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        let mut st = self.0.state.lock().unwrap();
        st.closed = true;
        st.jobs.clear();
        st.waiter = None;
    }
}

async fn drive(inj: Arc<Injector>) {
    let _guard = CloseOnDrop(inj.clone());
    loop {
        let jobs: Vec<Job> = {
            let mut st = inj.state.lock().unwrap();
            st.jobs.drain(..).collect()
        };
        for job in jobs {
            match job {
                Job::Wake(w) => w.wake(),
                Job::Spawn(f) => crate::runtime::spawn(f),
            }
        }
        touch_exit_sentinel();
        // 次の仕事を待つ（積まれていれば待たずに戻る）
        let inj2 = inj.clone();
        let _ = crate::runtime::offload::await_completion(move |c| {
            let mut st = inj2.state.lock().unwrap();
            if st.jobs.is_empty() && !st.closed {
                st.waiter = Some(c);
            }
            // 仕事があれば `c` を落とす = すぐ完了扱い（`None`）
        })
        .await;
    }
}

/// The current thread's entry point (`None` on a thread without a runtime driver). The first call starts the resident task.
pub(crate) fn current_injector() -> Option<Arc<Injector>> {
    if !crate::runtime::has_driver() {
        return None;
    }
    INJECTOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        if let Some(inj) = slot.as_ref() {
            if !inj.state.lock().unwrap().closed {
                return Some(inj.clone());
            }
        }
        let inj = Arc::new(Injector {
            owner: std::thread::current().id(),
            state: Mutex::new(InjectorState {
                jobs: VecDeque::new(),
                waiter: None,
                closed: false,
            }),
        });
        crate::runtime::spawn(ForgetOnExit::new(drive(inj.clone())));
        *slot = Some(inj.clone());
        Some(inj)
    })
}

/// Wakes a `Waker` after the runtime next reaps I/O completions (deferred until park, like tokio's `yield_now`).
///
/// veil's run queue is drained until it is empty, so an immediate wake-up would let a task that keeps
/// yielding starve the reaping of I/O completions. Going through the owning thread's entry point
/// (notification fd) makes the wake-up happen after completions are reaped. On a thread without a
/// driver the waker is woken directly.
pub(crate) fn defer_wake(w: &Waker) {
    match current_injector() {
        Some(inj) => {
            if let Err(Job::Wake(w)) = inj.push(Job::Wake(w.clone())) {
                w.wake();
            }
        }
        None => w.wake_by_ref(),
    }
}

// ===================== スレッドセーフな Waker =====================

struct SafeWakerInner {
    owner: ThreadId,
    waker: Waker,
    injector: Arc<Injector>,
}

impl SafeWakerInner {
    fn wake_by_ref(&self) {
        if std::thread::current().id() == self.owner {
            if exiting() {
                // スレッドの終わり: 実行キューは破棄中
                return;
            }
            self.waker.wake_by_ref();
        } else {
            let _ = self.injector.push(Job::Wake(self.waker.clone()));
        }
    }
}

static VTABLE: RawWakerVTable = RawWakerVTable::new(clone_raw, wake_raw, wake_by_ref_raw, drop_raw);

unsafe fn clone_raw(p: *const ()) -> RawWaker {
    // SAFETY: `p` は `Arc::into_raw` した `SafeWakerInner`
    unsafe { Arc::increment_strong_count(p as *const SafeWakerInner) };
    RawWaker::new(p, &VTABLE)
}

unsafe fn wake_raw(p: *const ()) {
    // SAFETY: 所有権ごと受け取る（clone_raw / into_raw の 1 参照）
    let inner = unsafe { Arc::from_raw(p as *const SafeWakerInner) };
    inner.wake_by_ref();
}

unsafe fn wake_by_ref_raw(p: *const ()) {
    // SAFETY: 参照だけ借りる
    let inner = unsafe { &*(p as *const SafeWakerInner) };
    inner.wake_by_ref();
}

unsafe fn drop_raw(p: *const ()) {
    // SAFETY: 1 参照を手放す
    drop(unsafe { Arc::from_raw(p as *const SafeWakerInner) });
}

/// Whether `w` is a waker created by this module (it is not wrapped again).
pub(crate) fn is_safe(w: &Waker) -> bool {
    std::ptr::eq(w.vtable(), &VTABLE)
}

/// Turns a waker into one that may be called from any thread. A waker from a thread without a runtime
/// driver (an ordinary thread-safe waker) and one that is already wrapped are returned as they are.
pub(crate) fn safe_waker(w: &Waker) -> Waker {
    if is_safe(w) {
        return w.clone();
    }
    let Some(injector) = current_injector() else {
        return w.clone();
    };
    let inner = Arc::new(SafeWakerInner {
        owner: injector.owner(),
        waker: w.clone(),
        injector,
    });
    let raw = RawWaker::new(Arc::into_raw(inner) as *const (), &VTABLE);
    // SAFETY: vtable の契約（参照カウントの Arc）を守っている
    unsafe { Waker::from_raw(raw) }
}

/// Storage for a waker (the same waker is not wrapped again). Used by `sync`, `JoinHandle` and timers.
#[derive(Default)]
pub(crate) struct WakerSlot {
    /// The registered waker (wrapped if it came from a thread with a driver). Holds only one, so the reference count does not grow.
    stored: Option<Waker>,
}

/// Whether `stored` (possibly a wrapped waker) wakes the same task as `w`.
fn same_task(stored: &Waker, w: &Waker) -> bool {
    if stored.will_wake(w) {
        return true;
    }
    if is_safe(stored) && !is_safe(w) {
        // SAFETY: 包んだ Waker の data は SafeWakerInner を指す
        let inner = unsafe { &*(stored.data() as *const SafeWakerInner) };
        return inner.waker.will_wake(w);
    }
    false
}

impl WakerSlot {
    /// Registers `w`.
    pub(crate) fn register(&mut self, w: &Waker) {
        if let Some(stored) = &self.stored {
            if same_task(stored, w) {
                return;
            }
        }
        self.stored = Some(safe_waker(w));
    }

    /// Registers `w` and returns the old waker it replaced (the caller drops it outside the lock).
    pub(crate) fn replace(&mut self, w: &Waker) -> Option<Waker> {
        if let Some(stored) = &self.stored {
            if same_task(stored, w) {
                return None;
            }
        }
        self.stored.replace(safe_waker(w))
    }

    pub(crate) fn take(&mut self) -> Option<Waker> {
        self.stored.take()
    }

    pub(crate) fn wake(&mut self) {
        if let Some(w) = self.take() {
            w.wake();
        }
    }
}

/// Wrapper that, if dropped during thread-local destruction, leaks its contents instead of dropping them,
/// so a wait inside the runtime does not touch an already destroyed op table (for tasks left over at `process::exit` or thread exit).
pub(crate) struct ForgetOnExit<F> {
    inner: std::mem::ManuallyDrop<F>,
}

impl<F> ForgetOnExit<F> {
    pub(crate) fn new(inner: F) -> ForgetOnExit<F> {
        ForgetOnExit {
            inner: std::mem::ManuallyDrop::new(inner),
        }
    }
}

impl<F: Future> Future for ForgetOnExit<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<F::Output> {
        // SAFETY: inner は構造的にピン留めされ、動かさない
        let r = unsafe { self.map_unchecked_mut(|s| &mut *s.inner) }.poll(cx);
        touch_exit_sentinel();
        r
    }
}

impl<F> Drop for ForgetOnExit<F> {
    fn drop(&mut self) {
        if !exiting() {
            // SAFETY: 一度だけ drop する
            unsafe { std::mem::ManuallyDrop::drop(&mut self.inner) };
        }
    }
}

/// Wrapper that gives the inner future a thread-safe waker, so a handler future can be woken safely
/// from another thread (used by `api::dispatch::call`).
pub(crate) struct WithSafeWaker<F> {
    fut: F,
    cache: SafeWakerCache,
}

impl<F> WithSafeWaker<F> {
    pub(crate) fn new(fut: F) -> WithSafeWaker<F> {
        WithSafeWaker { fut, cache: None }
    }
}

impl<F: Future> Future for WithSafeWaker<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<F::Output> {
        // SAFETY: `fut` は構造的にピン留めされ、動かさない
        let this = unsafe { self.get_unchecked_mut() };
        poll_with_safe_waker(
            unsafe { Pin::new_unchecked(&mut this.fut) },
            &mut this.cache,
            cx,
        )
    }
}

/// Cache for the wrapped waker: `(original waker, wrapped waker)`. The same waker is not wrapped again.
pub(crate) type SafeWakerCache = Option<(Waker, Waker)>;

// F-202: キープアライブの要求ごとに `Arc` を作らない。
/// A variant of [`WithSafeWaker`] that borrows its cache from outside. The API library's HTTP/1.1
/// connection owns it and reuses the wrapped waker across keep-alive requests (no `Arc` per request).
#[cfg(feature = "handler")]
pub(crate) struct WithSafeWakerIn<'a, F> {
    fut: F,
    cache: &'a mut SafeWakerCache,
}

#[cfg(feature = "handler")]
impl<'a, F> WithSafeWakerIn<'a, F> {
    pub(crate) fn new(fut: F, cache: &'a mut SafeWakerCache) -> WithSafeWakerIn<'a, F> {
        WithSafeWakerIn { fut, cache }
    }
}

#[cfg(feature = "handler")]
impl<F: Future> Future for WithSafeWakerIn<'_, F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<F::Output> {
        // SAFETY: `fut` は構造的にピン留めされ、動かさない（`cache` は参照で、ピン留めと無関係）
        let this = unsafe { self.get_unchecked_mut() };
        poll_with_safe_waker(unsafe { Pin::new_unchecked(&mut this.fut) }, this.cache, cx)
    }
}

fn poll_with_safe_waker<F: Future>(
    fut: Pin<&mut F>,
    cache: &mut SafeWakerCache,
    cx: &mut std::task::Context<'_>,
) -> std::task::Poll<F::Output> {
    let w = match cache {
        Some((orig, safe)) if orig.will_wake(cx.waker()) => safe.clone(),
        _ => {
            let s = safe_waker(cx.waker());
            *cache = Some((cx.waker().clone(), s.clone()));
            s
        }
    };
    let mut inner = std::task::Context::from_waker(&w);
    crate::tokio::time::note_activity();
    let r = crate::tokio::task::coop::with_budget(|| fut.poll(&mut inner));
    touch_exit_sentinel();
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Count(AtomicUsize);
    impl std::task::Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn without_driver_wakers_pass_through() {
        let c = Arc::new(Count(AtomicUsize::new(0)));
        let w = Waker::from(c.clone());
        let s = safe_waker(&w);
        assert!(!is_safe(&s));
        s.wake();
        assert_eq!(c.0.load(Ordering::SeqCst), 1);
    }
}
