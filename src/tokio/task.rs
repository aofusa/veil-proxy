//! `tokio::task` 互換（F-198）。
//!
//! タスクは veil のランタイムのタスクとして、`spawn` したスレッドのエグゼキュータで動く（tokio の
//! work stealing のようなスレッド間の移動はしない）。ランタイムの外のスレッドから呼ばれたら、
//! 文脈の `Handle`（`Runtime::enter`・`block_on` の中）のスレッドへ受け口経由で送る。
//! タスクの中の Future にはスレッドセーフな Waker（`waker::safe_waker`）を渡すので、別スレッドから
//! 起こされても所有スレッドで再開する。

use super::waker::{safe_waker, WakerSlot};
use std::any::Any;
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

// ===================== Id =====================

/// タスクの ID。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(u64);

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static CURRENT_ID: std::cell::Cell<Option<Id>> = const { std::cell::Cell::new(None) };
}

/// 実行中のタスクの ID（タスクの外では panic）。
pub fn id() -> Id {
    try_id().expect("can't get a task id when not inside a task")
}

/// 実行中のタスクの ID。
pub fn try_id() -> Option<Id> {
    CURRENT_ID.with(|c| c.get())
}

// ===================== JoinError =====================

enum Repr {
    Cancelled,
    Panic(Mutex<Box<dyn Any + Send + 'static>>),
}

/// タスクが完了しなかった理由。
pub struct JoinError {
    repr: Repr,
    id: Id,
}

impl JoinError {
    fn cancelled(id: Id) -> JoinError {
        JoinError {
            repr: Repr::Cancelled,
            id,
        }
    }

    fn panic(id: Id, p: Box<dyn Any + Send + 'static>) -> JoinError {
        JoinError {
            repr: Repr::Panic(Mutex::new(p)),
            id,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self.repr, Repr::Cancelled)
    }

    pub fn is_panic(&self) -> bool {
        matches!(self.repr, Repr::Panic(_))
    }

    pub fn into_panic(self) -> Box<dyn Any + Send + 'static> {
        self.try_into_panic()
            .expect("`JoinError` reason is not a panic.")
    }

    pub fn try_into_panic(self) -> Result<Box<dyn Any + Send + 'static>, JoinError> {
        match self.repr {
            Repr::Panic(p) => Ok(p.into_inner().unwrap()),
            _ => Err(self),
        }
    }

    pub fn id(&self) -> Id {
        self.id
    }
}

fn panic_text(p: &(dyn Any + Send)) -> Option<&str> {
    p.downcast_ref::<&str>()
        .copied()
        .or_else(|| p.downcast_ref::<String>().map(|s| s.as_str()))
}

impl std::fmt::Display for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.repr {
            Repr::Cancelled => write!(f, "task {} was cancelled", self.id),
            Repr::Panic(p) => match panic_text(&**p.lock().unwrap()) {
                Some(m) => write!(f, "task {} panicked with message {:?}", self.id, m),
                None => write!(f, "task {} panicked", self.id),
            },
        }
    }
}

impl std::fmt::Debug for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.repr {
            Repr::Cancelled => write!(f, "JoinError::Cancelled({:?})", self.id),
            Repr::Panic(p) => match panic_text(&**p.lock().unwrap()) {
                Some(m) => write!(f, "JoinError::Panic({:?}, {:?}, ...)", self.id, m),
                None => write!(f, "JoinError::Panic({:?}, ...)", self.id),
            },
        }
    }
}

impl std::error::Error for JoinError {}

impl From<JoinError> for std::io::Error {
    fn from(e: JoinError) -> std::io::Error {
        std::io::Error::other(e.to_string())
    }
}

// ===================== 共有状態 =====================

struct Inner<T> {
    result: Option<Result<T, JoinError>>,
    finished: bool,
    aborted: bool,
    join_waker: WakerSlot,
    task_waker: WakerSlot,
}

struct State<T> {
    id: Id,
    inner: Mutex<Inner<T>>,
}

impl<T> State<T> {
    fn new() -> Arc<State<T>> {
        Arc::new(State {
            id: Id(NEXT_ID.fetch_add(1, Ordering::Relaxed)),
            inner: Mutex::new(Inner {
                result: None,
                finished: false,
                aborted: false,
                join_waker: WakerSlot::default(),
                task_waker: WakerSlot::default(),
            }),
        })
    }

    fn complete(&self, r: Result<T, JoinError>) {
        let w = {
            let mut g = self.inner.lock().unwrap();
            if g.finished {
                return;
            }
            g.finished = true;
            g.result = Some(r);
            g.join_waker.take()
        };
        if let Some(w) = w {
            w.wake();
        }
    }
}

/// 中止だけできる型消去した参照。
trait Abortable: Send + Sync {
    fn abort(&self);
    fn is_finished(&self) -> bool;
    fn id(&self) -> Id;
}

/// 中止を要求してタスクを起こす（タスクは次の poll で Future を捨てる）。
fn abort_state<T>(s: &State<T>) {
    let w = {
        let mut g = s.inner.lock().unwrap();
        if g.finished {
            return;
        }
        g.aborted = true;
        g.task_waker.take()
    };
    if let Some(w) = w {
        w.wake();
    }
}

impl<T: Send> Abortable for State<T> {
    fn abort(&self) {
        abort_state(self);
    }
    fn is_finished(&self) -> bool {
        self.inner.lock().unwrap().finished
    }
    fn id(&self) -> Id {
        self.id
    }
}

// ===================== タスク本体 =====================

/// spawn したタスクの本体（veil のタスクとして動く）。
pub(crate) struct TaskCell<F: Future> {
    future: Option<Pin<Box<F>>>,
    state: Arc<State<F::Output>>,
    waker: Option<(Waker, Waker)>,
}

impl<F: Future> TaskCell<F> {
    fn new(future: F, state: Arc<State<F::Output>>) -> TaskCell<F> {
        TaskCell {
            future: Some(Box::pin(future)),
            state,
            waker: None,
        }
    }
}

impl<F: Future> Future for TaskCell<F> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if this.future.is_none() {
            return Poll::Ready(());
        }
        {
            let mut g = this.state.inner.lock().unwrap();
            if g.aborted {
                drop(g);
                this.future = None;
                this.state
                    .complete(Err(JoinError::cancelled(this.state.id)));
                return Poll::Ready(());
            }
            g.task_waker.register(cx.waker());
        }
        let safe = match &this.waker {
            Some((orig, safe)) if orig.will_wake(cx.waker()) => safe.clone(),
            _ => {
                let s = safe_waker(cx.waker());
                this.waker = Some((cx.waker().clone(), s.clone()));
                s
            }
        };
        let mut inner_cx = Context::from_waker(&safe);
        let id = this.state.id;
        let prev = CURRENT_ID.with(|c| c.replace(Some(id)));
        let fut = this.future.as_mut().unwrap().as_mut();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fut.poll(&mut inner_cx)));
        CURRENT_ID.with(|c| c.set(prev));
        crate::tokio::waker::touch_exit_sentinel();
        match r {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(v)) => {
                this.future = None;
                this.state.complete(Ok(v));
                Poll::Ready(())
            }
            Err(p) => {
                this.future = None;
                this.state.complete(Err(JoinError::panic(id, p)));
                Poll::Ready(())
            }
        }
    }
}

impl<F: Future> Drop for TaskCell<F> {
    fn drop(&mut self) {
        if crate::tokio::waker::exiting() {
            // スレッドローカルの破棄中（process::exit 等）: 中の Future（ランタイムの待ちを持ち得る）は手放す
            if let Some(f) = self.future.take() {
                std::mem::forget(f);
            }
            return;
        }
        if self.future.take().is_some() {
            // ランタイムの停止等で完了前に捨てられた
            self.state
                .complete(Err(JoinError::cancelled(self.state.id)));
        }
    }
}

/// タスクと JoinHandle を作る。
pub(crate) fn task<F: Future>(future: F) -> (TaskCell<F>, JoinHandle<F::Output>) {
    let state = State::new();
    (TaskCell::new(future, state.clone()), JoinHandle { state })
}

// ===================== JoinHandle =====================

/// `tokio::task::JoinHandle`。drop してもタスクは止まらない（`abort` で止める）。
pub struct JoinHandle<T> {
    state: Arc<State<T>>,
}

impl<T> std::fmt::Debug for JoinHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinHandle")
            .field("id", &self.state.id)
            .finish()
    }
}

impl<T> Unpin for JoinHandle<T> {}

impl<T: Send + 'static> JoinHandle<T> {
    pub fn abort(&self) {
        Abortable::abort(&*self.state);
    }

    pub fn abort_handle(&self) -> AbortHandle {
        AbortHandle {
            inner: self.state.clone(),
        }
    }
}

impl<T> JoinHandle<T> {
    pub fn is_finished(&self) -> bool {
        self.state.inner.lock().unwrap().finished
    }

    pub fn id(&self) -> Id {
        self.state.id
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut g = self.state.inner.lock().unwrap();
        if let Some(r) = g.result.take() {
            return Poll::Ready(r);
        }
        if g.finished {
            // 2 回目の poll（結果は取り出し済み）
            return Poll::Ready(Err(JoinError::cancelled(self.state.id)));
        }
        g.join_waker.register(cx.waker());
        Poll::Pending
    }
}

/// `tokio::task::AbortHandle`。
#[derive(Clone)]
pub struct AbortHandle {
    inner: Arc<dyn Abortable>,
}

impl std::fmt::Debug for AbortHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AbortHandle")
            .field("id", &self.inner.id())
            .finish()
    }
}

impl AbortHandle {
    pub fn abort(&self) {
        self.inner.abort();
    }
    pub fn is_finished(&self) -> bool {
        self.inner.is_finished()
    }
    pub fn id(&self) -> Id {
        self.inner.id()
    }
}

// ===================== spawn =====================

/// 現在のスレッドのエグゼキュータで動かす。
pub(crate) fn spawn_here<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    let (t, h) = task(future);
    crate::runtime::spawn(t);
    h
}

/// `tokio::spawn`。
#[track_caller]
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    if crate::runtime::has_driver() {
        spawn_here(future)
    } else {
        super::runtime::Handle::current().spawn(future)
    }
}

/// `tokio::task::spawn_local`（現在のスレッドで動かす。`Send` を要求しない）。
#[track_caller]
pub fn spawn_local<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    if crate::runtime::has_driver() {
        return spawn_here(future);
    }
    LOCAL_SET.with(|ls| match ls.borrow().as_ref() {
        Some(q) => {
            let (t, h) = task(future);
            q.borrow_mut().push(Box::pin(t));
            h
        }
        None => panic!("`spawn_local` called from outside of a `task::LocalSet` or LocalRuntime"),
    })
}

/// `tokio::task::spawn_blocking`（ハンドラ用のブロッキングプールで動かす）。
#[track_caller]
pub fn spawn_blocking<F, R>(f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let state = State::<R>::new();
    let handle = JoinHandle {
        state: state.clone(),
    };
    let id = state.id;
    if crate::runtime::has_driver() {
        crate::runtime::spawn(crate::tokio::waker::ForgetOnExit::new(async move {
            let r = crate::runtime::offload::offload_handler(move || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
            })
            .await;
            match r {
                Some(Ok(v)) => state.complete(Ok(v)),
                Some(Err(p)) => state.complete(Err(JoinError::panic(id, p))),
                None => state.complete(Err(JoinError::cancelled(id))),
            }
        }));
    } else {
        // ランタイムの外（普通のスレッド）: 専用スレッドで動かす
        let spawned = std::thread::Builder::new()
            .name("veil-tokio-blocking".into())
            .spawn(
                move || match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
                    Ok(v) => state.complete(Ok(v)),
                    Err(p) => state.complete(Err(JoinError::panic(id, p))),
                },
            );
        if spawned.is_err() {
            panic!("failed to spawn a blocking thread");
        }
    }
    handle
}

/// `tokio::task::block_in_place`（そのまま呼ぶ。veil のワーカーでは他のタスクを止める点に注意）。
pub fn block_in_place<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    f()
}

/// 1 回だけ他のタスクに譲る。
pub async fn yield_now() {
    let mut yielded = false;
    std::future::poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await
}

/// 協調的スケジューリングの予算（このレイヤーでは 1 回譲るだけ）。
pub async fn consume_budget() {
    yield_now().await
}

/// 予算の制限を外す（このレイヤーでは何もしない）。
pub fn unconstrained<F: Future>(inner: F) -> F {
    inner
}

// ===================== JoinSet =====================

/// `tokio::task::JoinSet`（drop で残りを abort する）。
pub struct JoinSet<T> {
    handles: Vec<JoinHandle<T>>,
}

impl<T> std::fmt::Debug for JoinSet<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinSet").field("len", &self.len()).finish()
    }
}

impl<T> Default for JoinSet<T> {
    fn default() -> Self {
        JoinSet {
            handles: Vec::new(),
        }
    }
}

impl<T> JoinSet<T> {
    pub fn new() -> JoinSet<T> {
        JoinSet::default()
    }

    pub fn len(&self) -> usize {
        self.handles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }
}

impl<T: 'static> JoinSet<T> {
    pub fn spawn<F>(&mut self, task: F) -> AbortHandle
    where
        F: Future<Output = T> + Send + 'static,
        T: Send,
    {
        self.insert(spawn(task))
    }

    pub fn spawn_on<F>(&mut self, task: F, handle: &super::runtime::Handle) -> AbortHandle
    where
        F: Future<Output = T> + Send + 'static,
        T: Send,
    {
        self.insert(handle.spawn(task))
    }

    pub fn spawn_local<F>(&mut self, task: F) -> AbortHandle
    where
        F: Future<Output = T> + 'static,
        T: Send,
    {
        self.insert(spawn_local(task))
    }

    pub fn spawn_blocking<F>(&mut self, f: F) -> AbortHandle
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send,
    {
        self.insert(spawn_blocking(f))
    }

    fn insert(&mut self, h: JoinHandle<T>) -> AbortHandle
    where
        T: Send,
    {
        let a = h.abort_handle();
        self.handles.push(h);
        a
    }

    /// どれか 1 つの完了を待つ（空なら `None`）。
    pub async fn join_next(&mut self) -> Option<Result<T, JoinError>> {
        std::future::poll_fn(|cx| self.poll_join_next(cx)).await
    }

    /// `join_next` と同じで、タスク ID も返す。
    pub async fn join_next_with_id(&mut self) -> Option<Result<(Id, T), JoinError>> {
        std::future::poll_fn(|cx| self.poll_join_next_with_id(cx)).await
    }

    pub fn poll_join_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<T, JoinError>>> {
        self.poll_join_next_with_id(cx)
            .map(|o| o.map(|r| r.map(|(_, v)| v)))
    }

    pub fn poll_join_next_with_id(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<(Id, T), JoinError>>> {
        if self.handles.is_empty() {
            return Poll::Ready(None);
        }
        for i in 0..self.handles.len() {
            let id = self.handles[i].id();
            if let Poll::Ready(r) = Pin::new(&mut self.handles[i]).poll(cx) {
                self.handles.swap_remove(i);
                return Poll::Ready(Some(r.map(|v| (id, v))));
            }
        }
        Poll::Pending
    }

    /// 完了しているものを 1 つ取る（待たない）。
    pub fn try_join_next(&mut self) -> Option<Result<T, JoinError>> {
        let i = self.handles.iter().position(|h| h.is_finished())?;
        let mut h = self.handles.swap_remove(i);
        let waker = futures::task::noop_waker();
        match Pin::new(&mut h).poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(r) => Some(r),
            Poll::Pending => None,
        }
    }

    pub fn abort_all(&mut self)
    where
        T: Send,
    {
        for h in &self.handles {
            h.abort();
        }
    }

    pub fn detach_all(&mut self) {
        self.handles.clear();
    }

    /// すべて abort して終わるのを待つ。
    pub async fn shutdown(&mut self)
    where
        T: Send,
    {
        self.abort_all();
        while self.join_next().await.is_some() {}
    }

    /// すべての完了を待って結果を返す（panic したタスクがあれば再 panic）。
    pub async fn join_all(mut self) -> Vec<T> {
        let mut out = Vec::with_capacity(self.len());
        while let Some(r) = self.join_next().await {
            match r {
                Ok(v) => out.push(v),
                Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                Err(e) => panic!("{}", e),
            }
        }
        out
    }
}

impl<T> Drop for JoinSet<T> {
    fn drop(&mut self) {
        for h in &self.handles {
            abort_state(&h.state);
        }
    }
}

impl<T, F> FromIterator<F> for JoinSet<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    fn from_iter<I: IntoIterator<Item = F>>(iter: I) -> Self {
        let mut set = JoinSet::new();
        for f in iter {
            set.spawn(f);
        }
        set
    }
}

// ===================== LocalSet =====================

type LocalQueue = Rc<RefCell<Vec<Pin<Box<dyn Future<Output = ()>>>>>>;

thread_local! {
    static LOCAL_SET: RefCell<Option<LocalQueue>> = const { RefCell::new(None) };
}

/// `tokio::task::LocalSet`（`Send` でないタスクを現在のスレッドで動かす）。
pub struct LocalSet {
    queue: LocalQueue,
}

impl std::fmt::Debug for LocalSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalSet")
    }
}

impl Default for LocalSet {
    fn default() -> Self {
        LocalSet::new()
    }
}

impl LocalSet {
    pub fn new() -> LocalSet {
        LocalSet {
            queue: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// タスクを積む（`run_until`・`block_on` の中、またはランタイムのスレッドではすぐ動く）。
    pub fn spawn_local<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let (t, h) = task(future);
        if crate::runtime::has_driver() {
            crate::runtime::spawn(t);
        } else {
            self.queue.borrow_mut().push(Box::pin(t));
        }
        h
    }

    fn flush(&self) {
        let pending: Vec<_> = self.queue.borrow_mut().drain(..).collect();
        for t in pending {
            crate::runtime::spawn(t);
        }
    }

    /// `future` が終わるまで、このセットのタスクと一緒に動かす。
    pub async fn run_until<F: Future>(&self, future: F) -> F::Output {
        self.flush();
        let prev = LOCAL_SET.with(|l| l.replace(Some(self.queue.clone())));
        let r = future.await;
        LOCAL_SET.with(|l| *l.borrow_mut() = prev);
        self.flush();
        r
    }

    /// `rt.block_on(local.run_until(future))`。
    pub fn block_on<F>(&self, rt: &super::runtime::Runtime, future: F) -> F::Output
    where
        F: Future,
    {
        rt.block_on(self.run_until(future))
    }

    /// 現在のスレッドの文脈に入る（`spawn_local` が積めるようになる）。
    pub fn enter(&self) -> LocalEnterGuard {
        let prev = LOCAL_SET.with(|l| l.replace(Some(self.queue.clone())));
        LocalEnterGuard { prev }
    }
}

/// `LocalSet::enter` のガード。
pub struct LocalEnterGuard {
    prev: Option<LocalQueue>,
}

impl Drop for LocalEnterGuard {
    fn drop(&mut self) {
        let prev = self.prev.take();
        LOCAL_SET.with(|l| *l.borrow_mut() = prev);
    }
}

impl Future for LocalSet {
    type Output = ();
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        self.flush();
        Poll::Ready(())
    }
}

// ===================== task_local =====================

/// `tokio::task_local!` の鍵。
pub struct LocalKey<T: 'static> {
    #[doc(hidden)]
    pub inner: std::thread::LocalKey<RefCell<Option<T>>>,
}

/// 値が設定されていない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessError(());

impl std::fmt::Display for AccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("task-local value not set")
    }
}

impl std::error::Error for AccessError {}

impl<T: 'static> LocalKey<T> {
    /// `value` を設定して `f` を動かす（poll のたびに設定し、抜けるときに戻す）。
    pub fn scope<F: Future>(&'static self, value: T, f: F) -> TaskLocalFuture<T, F> {
        TaskLocalFuture {
            key: self,
            slot: Some(value),
            future: Some(f),
        }
    }

    /// 同期版の `scope`。
    pub fn sync_scope<F: FnOnce() -> R, R>(&'static self, value: T, f: F) -> R {
        let prev = self.inner.with(|c| c.replace(Some(value)));
        struct Restore<T: 'static> {
            key: &'static std::thread::LocalKey<RefCell<Option<T>>>,
            prev: Option<Option<T>>,
        }
        impl<T> Drop for Restore<T> {
            fn drop(&mut self) {
                let p = self.prev.take().unwrap();
                self.key.with(|c| *c.borrow_mut() = p);
            }
        }
        let _r = Restore {
            key: &self.inner,
            prev: Some(prev),
        };
        f()
    }

    pub fn with<F: FnOnce(&T) -> R, R>(&'static self, f: F) -> R {
        self.try_with(f).expect(
            "cannot access a task-local storage value without setting it via `LocalKey::set`",
        )
    }

    pub fn try_with<F: FnOnce(&T) -> R, R>(&'static self, f: F) -> Result<R, AccessError> {
        self.inner.with(|c| match c.borrow().as_ref() {
            Some(v) => Ok(f(v)),
            None => Err(AccessError(())),
        })
    }
}

impl<T: Clone + 'static> LocalKey<T> {
    pub fn get(&'static self) -> T {
        self.with(|v| v.clone())
    }

    pub fn try_get(&'static self) -> Result<T, AccessError> {
        self.try_with(|v| v.clone())
    }
}

/// `LocalKey::scope` の Future。
pub struct TaskLocalFuture<T: 'static, F> {
    key: &'static LocalKey<T>,
    slot: Option<T>,
    future: Option<F>,
}

impl<T: 'static, F: Future> Future for TaskLocalFuture<T, F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        // SAFETY: `future` は動かさない（構造的ピン留め）。`slot` は Unpin として扱う
        let this = unsafe { self.get_unchecked_mut() };
        let value = this.slot.take();
        let prev = this.key.inner.with(|c| c.replace(value));
        let r = match this.future.as_mut() {
            Some(f) => unsafe { Pin::new_unchecked(f) }.poll(cx),
            None => panic!("`TaskLocalFuture` polled after completion"),
        };
        this.slot = this
            .key
            .inner
            .with(|c| std::mem::replace(&mut *c.borrow_mut(), prev));
        if r.is_ready() {
            // SAFETY: 完了後は動かさずに捨てる
            this.future = None;
        }
        r
    }
}

/// `tokio::task_local!`。
#[macro_export]
#[doc(hidden)]
macro_rules! __veil_tokio_task_local {
    () => {};
    ($(#[$attr:meta])* $vis:vis static $name:ident: $t:ty; $($rest:tt)*) => {
        $(#[$attr])*
        $vis static $name: $crate::tokio::task::LocalKey<$t> = {
            ::std::thread_local! {
                static __KEY: ::std::cell::RefCell<::std::option::Option<$t>> =
                    const { ::std::cell::RefCell::new(::std::option::Option::None) };
            }
            $crate::tokio::task::LocalKey { inner: __KEY }
        };
        $crate::__veil_tokio_task_local!($($rest)*);
    };
    ($(#[$attr:meta])* $vis:vis static $name:ident: $t:ty) => {
        $crate::__veil_tokio_task_local!($(#[$attr])* $vis static $name: $t;);
    };
}

pub use crate::__veil_tokio_task_local as task_local;
