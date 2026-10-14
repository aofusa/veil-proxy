//! スレッド越しの wake と spawn を所有スレッドへ届ける（F-198。veil のランタイムは変えない）。
//!
//! veil の Waker は「所有ワーカースレッドでだけ wake してよい」（`runtime/uring/executor.rs` の健全性の節）。
//! tokio の API（`sync`・`JoinHandle`・タイマー等）は別スレッドから wake するので、ここで包む:
//!
//! - [`safe_waker`]: 現在のスレッド（ランタイムのドライバがあるスレッド）の Waker を、どのスレッドから
//!   呼んでもよい Waker に包む。所有スレッドから呼ばれたら元の Waker をそのまま呼び、別スレッドから
//!   呼ばれたら所有スレッドの [`Injector`] に「この Waker を呼んで」と積む。
//! - [`Injector`]: スレッドごとに 1 つ。所有スレッドで常駐タスクが `runtime::offload::await_completion`
//!   （結果スロット + 通知 fd）で待ち、積まれた wake・spawn を所有スレッドで実行する。
//!
//! 使うのは既存の `await_completion`・`spawn`・`has_driver` だけで、executor・Waker の vtable・
//! io_uring／reactor の処理には手を入れない。

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{RawWaker, RawWakerVTable, Waker};
use std::thread::ThreadId;

/// 所有スレッドで実行する仕事。
pub(crate) enum Job {
    /// 所有スレッドの Waker を呼ぶ（別スレッドからの wake）。
    Wake(Waker),
    /// 所有スレッドでタスクを起こす（別スレッドからの spawn）。
    Spawn(Pin<Box<dyn Future<Output = ()> + Send + 'static>>),
}

struct InjectorState {
    jobs: VecDeque<Job>,
    /// 所有スレッドの常駐タスクの待ち（積んだら完了させて起こす）。
    waiter: Option<crate::runtime::offload::Completer<()>>,
    /// 常駐タスクが終わった（スレッドのランタイムが止まった）。
    closed: bool,
}

/// スレッドごとの受け口。
pub(crate) struct Injector {
    owner: ThreadId,
    state: Mutex<InjectorState>,
}

impl Injector {
    /// 仕事を積んで所有スレッドを起こす。所有スレッドが止まっていたら仕事を返す。
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
    /// スレッドローカルの破棄が始まった（`process::exit` の中・スレッドの終わり）。
    static EXITING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// 破棄の始まりを知らせる番兵。ランタイムのスレッドローカル（op 表等）より後に登録する（[`touch_exit_sentinel`]）
    /// ので、破棄は登録の逆順により番兵が先に走る。
    static EXIT_SENTINEL: ExitSentinel = const { ExitSentinel };
}

struct ExitSentinel;

impl Drop for ExitSentinel {
    fn drop(&mut self) {
        let _ = EXITING.try_with(|e| e.set(true));
    }
}

/// 番兵を登録する（ランタイムの I/O を一度使った後に呼ぶ）。
pub(crate) fn touch_exit_sentinel() {
    let _ = EXIT_SENTINEL.try_with(|_| {});
}

/// スレッドローカルの破棄の最中か（このときランタイムの表・キューに触れてはいけない）。
pub(crate) fn exiting() -> bool {
    EXITING.try_with(|e| e.get()).unwrap_or(true)
}

/// 常駐タスクが drop されたら受け口を閉じる（スレッドのランタイムが止まった）。
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

/// 現在のスレッドの受け口（ランタイムのドライバが無いスレッドでは `None`）。初回に常駐タスクを起こす。
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

/// Waker を「ランタイムが次に I/O の完了を刈り取った後」に呼ぶ（tokio の yield_now と同じく park まで遅らせる）。
///
/// veil の実行キューは空になるまで回るので、すぐ wake すると yield を繰り返すタスクが I/O の完了の刈り取りを
/// 止めてしまう。所有スレッドの受け口（通知 fd）を経由させると、起こされるのは完了の刈り取りの後になる。
/// ドライバの無いスレッドではそのまま起こす。
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

/// `w` がこのモジュールの Waker か（包み直さない）。
pub(crate) fn is_safe(w: &Waker) -> bool {
    std::ptr::eq(w.vtable(), &VTABLE)
}

/// どのスレッドから呼んでもよい Waker にする。ランタイムのドライバが無いスレッドの Waker（普通の
/// スレッドセーフな Waker）と、すでに包んだものはそのまま返す。
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

/// Waker の置き場（同じ Waker なら包み直さない）。sync・JoinHandle・タイマーが使う。
#[derive(Default)]
pub(crate) struct WakerSlot {
    /// 登録した Waker（ドライバのあるスレッドの Waker は包んだもの）。1 つだけ持つ（参照数を増やさない）
    stored: Option<Waker>,
}

/// `stored`（包んだかもしれない Waker）が `w` と同じタスクを起こすか。
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
    /// `w` を登録する。
    pub(crate) fn register(&mut self, w: &Waker) {
        if let Some(stored) = &self.stored {
            if same_task(stored, w) {
                return;
            }
        }
        self.stored = Some(safe_waker(w));
    }

    /// 登録して、入れ替えた古い Waker を返す（呼び出し側がロックの外で落とす）。
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

/// スレッドローカルの破棄の最中に drop されたら、中身を drop せずに手放す包み（中のランタイムの待ちが、
/// 破棄済みの op 表に触れないように。`process::exit` やスレッドの終わりに残ったタスク用）。
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

/// 中の Future にスレッドセーフな Waker を渡す包み（ハンドラの Future を、別スレッドから起こされても
/// 安全にする。`api::dispatch::call` が使う）。
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

/// 包んだ Waker の置き場（`(元の Waker, 包んだ Waker)`）。同じ Waker なら包み直さない。
pub(crate) type SafeWakerCache = Option<(Waker, Waker)>;

/// [`WithSafeWaker`] の置き場を外から借りる版。API ライブラリの HTTP/1.1 の接続が持ち、キープアライブの
/// 要求をまたいで包んだ Waker を使い回す（要求ごとに `Arc` を作らない。F-202）。
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
