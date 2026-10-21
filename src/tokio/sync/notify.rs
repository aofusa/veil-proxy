//! Equivalents of `tokio::sync::Notify`, `Barrier` and `OnceCell`.

use crate::tokio::waker::WakerSlot;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

struct Waiter {
    notified: AtomicBool,
    /// Woken by `notify_last` (when cycling on drop, it cycles in the same direction).
    by_last: AtomicBool,
    waker: Mutex<WakerSlot>,
}

struct NotifyState {
    permit: bool,
    generation: u64,
    waiters: VecDeque<Arc<Waiter>>,
}

/// `tokio::sync::Notify`。
pub struct Notify {
    state: Mutex<NotifyState>,
}

impl std::fmt::Debug for Notify {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Notify")
    }
}

impl Default for Notify {
    fn default() -> Self {
        Notify::new()
    }
}

impl Notify {
    pub fn new() -> Notify {
        Notify::const_new()
    }

    pub const fn const_new() -> Notify {
        Notify {
            state: Mutex::new(NotifyState {
                permit: false,
                generation: 0,
                waiters: VecDeque::new(),
            }),
        }
    }

    /// A future that waits for a notification (it also completes on a `notify_waiters` that happens after it was created).
    pub fn notified(&self) -> Notified<'_> {
        let generation = self.state.lock().unwrap().generation;
        Notified {
            notify: self,
            generation,
            waiter: None,
            done: false,
        }
    }

    /// Wakes the first waiter (if nobody is waiting, it leaves a permit for the next wait).
    pub fn notify_one(&self) {
        let w = {
            let mut st = self.state.lock().unwrap();
            match st.waiters.pop_front() {
                Some(w) => {
                    w.notified.store(true, Ordering::Release);
                    Some(w)
                }
                None => {
                    st.permit = true;
                    None
                }
            }
        };
        if let Some(w) = w {
            w.waker.lock().unwrap().wake();
        }
    }

    /// Wakes the waiter that started waiting last.
    pub fn notify_last(&self) {
        let w = {
            let mut st = self.state.lock().unwrap();
            match st.waiters.pop_back() {
                Some(w) => {
                    w.by_last.store(true, Ordering::Release);
                    w.notified.store(true, Ordering::Release);
                    Some(w)
                }
                None => {
                    st.permit = true;
                    None
                }
            }
        };
        if let Some(w) = w {
            w.waker.lock().unwrap().wake();
        }
    }

    /// Wakes every current waiter (and every `Notified` that was created but not yet polled). It does not leave a permit.
    pub fn notify_waiters(&self) {
        let ws: Vec<_> = {
            let mut st = self.state.lock().unwrap();
            st.generation += 1;
            st.waiters.drain(..).collect()
        };
        for w in ws {
            w.notified.store(true, Ordering::Release);
            w.waker.lock().unwrap().wake();
        }
    }
}

impl Notify {
    /// The owned version of `notified` (it holds an `Arc<Notify>`).
    pub fn notified_owned(self: Arc<Self>) -> OwnedNotified {
        // SAFETY: `inner` は `notify` が指す Notify を借りる。Arc を同じ構造体に持ち、inner を先に drop する
        // （フィールドの宣言順）ので、借用より先に Notify が消えることはない
        let inner: Notified<'static> = unsafe { std::mem::transmute(self.notified()) };
        OwnedNotified {
            inner,
            _notify: self,
        }
    }
}

/// The future returned by `Notify::notified_owned`.
pub struct OwnedNotified {
    inner: Notified<'static>,
    _notify: Arc<Notify>,
}

impl std::fmt::Debug for OwnedNotified {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OwnedNotified")
    }
}

impl OwnedNotified {
    pub fn enable(self: Pin<&mut Self>) -> bool {
        self.get_mut().inner.register(None)
    }
}

impl Future for OwnedNotified {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        Pin::new(&mut self.get_mut().inner).poll(cx)
    }
}

/// The future returned by `Notify::notified`.
pub struct Notified<'a> {
    notify: &'a Notify,
    generation: u64,
    waiter: Option<Arc<Waiter>>,
    done: bool,
}

impl std::fmt::Debug for Notified<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Notified")
    }
}

impl Notified<'_> {
    /// Enters the wait queue without polling (so that it can receive `notify_one`). Returns `true` if it was already notified.
    pub fn enable(self: Pin<&mut Self>) -> bool {
        let this = self.get_mut();
        this.register(None)
    }

    fn register(&mut self, cx: Option<&mut Context<'_>>) -> bool {
        if self.done {
            return true;
        }
        let mut st = self.notify.state.lock().unwrap();
        if let Some(w) = &self.waiter {
            if w.notified.load(Ordering::Acquire) {
                self.done = true;
                self.waiter = None;
                return true;
            }
            if let Some(cx) = cx {
                w.waker.lock().unwrap().register(cx.waker());
            }
            return false;
        }
        if st.generation != self.generation {
            self.done = true;
            return true;
        }
        if st.permit {
            st.permit = false;
            self.done = true;
            return true;
        }
        let w = Arc::new(Waiter {
            notified: AtomicBool::new(false),
            by_last: AtomicBool::new(false),
            waker: Mutex::new(WakerSlot::default()),
        });
        if let Some(cx) = cx {
            w.waker.lock().unwrap().register(cx.waker());
        }
        st.waiters.push_back(w.clone());
        self.waiter = Some(w);
        false
    }
}

impl Future for Notified<'_> {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.get_mut().register(Some(cx)) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Drop for Notified<'_> {
    fn drop(&mut self) {
        let Some(w) = self.waiter.take() else {
            return;
        };
        let mut st = self.notify.state.lock().unwrap();
        if w.notified.load(Ordering::Acquire) {
            // notify_one を受け取ったまま去る: 次の待ちへ回す（notify_waiters の通知は回さない）
            if st.generation == self.generation {
                drop(st);
                if w.by_last.load(Ordering::Acquire) {
                    self.notify.notify_last();
                } else {
                    self.notify.notify_one();
                }
            }
        } else {
            st.waiters.retain(|x| !Arc::ptr_eq(x, &w));
        }
    }
}

// ===================== Barrier =====================

/// `tokio::sync::Barrier`。
#[derive(Debug)]
pub struct Barrier {
    n: usize,
    state: Mutex<BarrierStateDbg>,
    notify: Notify,
}

/// (generation, number of arrivals)
#[derive(Debug)]
struct BarrierStateDbg(u64, usize);

/// The result of `Barrier::wait` (exactly one waiter is the leader).
#[derive(Debug, Clone)]
pub struct BarrierWaitResult(bool);

impl BarrierWaitResult {
    pub fn is_leader(&self) -> bool {
        self.0
    }
}

impl Barrier {
    pub fn new(n: usize) -> Barrier {
        Barrier {
            n: n.max(1),
            state: Mutex::new(BarrierStateDbg(0, 0)),
            notify: Notify::new(),
        }
    }

    pub async fn wait(&self) -> BarrierWaitResult {
        let (generation, leader, notified) = {
            let mut st = self.state.lock().unwrap();
            let generation = st.0;
            st.1 += 1;
            if st.1 == self.n {
                st.1 = 0;
                st.0 += 1;
                (generation, true, None)
            } else {
                (generation, false, Some(self.notify.notified()))
            }
        };
        if leader {
            self.notify.notify_waiters();
            return BarrierWaitResult(true);
        }
        let mut notified = std::pin::pin!(notified.unwrap());
        loop {
            notified.as_mut().await;
            if self.state.lock().unwrap().0 != generation {
                return BarrierWaitResult(false);
            }
            notified.set(self.notify.notified());
        }
    }
}

// ===================== OnceCell =====================

/// The error of `OnceCell::set`.
#[derive(Debug, PartialEq, Eq)]
pub enum SetError<T> {
    AlreadyInitializedError(T),
    InitializingError(T),
}

impl<T> SetError<T> {
    pub fn is_already_init_err(&self) -> bool {
        matches!(self, SetError::AlreadyInitializedError(_))
    }
    pub fn is_initializing_err(&self) -> bool {
        matches!(self, SetError::InitializingError(_))
    }
}

impl<T> std::fmt::Display for SetError<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetError::AlreadyInitializedError(_) => f.write_str("AlreadyInitializedError"),
            SetError::InitializingError(_) => f.write_str("InitializingError"),
        }
    }
}

impl<T: std::fmt::Debug> std::error::Error for SetError<T> {}

/// Equivalent of `tokio::sync::OnceCell`: only one future runs the initialisation.
pub struct OnceCell<T> {
    value: std::sync::OnceLock<T>,
    init: super::Semaphore,
}

impl<T: std::fmt::Debug> std::fmt::Debug for OnceCell<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnceCell")
            .field("value", &self.get())
            .finish()
    }
}

impl<T> Default for OnceCell<T> {
    fn default() -> Self {
        OnceCell::new()
    }
}

impl<T: Clone> Clone for OnceCell<T> {
    fn clone(&self) -> Self {
        OnceCell::new_with(self.get().cloned())
    }
}

impl<T> From<T> for OnceCell<T> {
    fn from(v: T) -> Self {
        OnceCell::new_with(Some(v))
    }
}

impl<T> OnceCell<T> {
    pub fn new() -> OnceCell<T> {
        OnceCell::const_new()
    }

    pub const fn const_new() -> OnceCell<T> {
        OnceCell {
            value: std::sync::OnceLock::new(),
            init: super::Semaphore::const_new(1),
        }
    }

    pub fn new_with(value: Option<T>) -> OnceCell<T> {
        let c = OnceCell::new();
        if let Some(v) = value {
            let _ = c.value.set(v);
        }
        c
    }

    pub fn initialized(&self) -> bool {
        self.value.get().is_some()
    }

    pub fn get(&self) -> Option<&T> {
        self.value.get()
    }

    pub fn get_mut(&mut self) -> Option<&mut T> {
        self.value.get_mut()
    }

    pub fn set(&self, value: T) -> Result<(), SetError<T>> {
        if self.initialized() {
            return Err(SetError::AlreadyInitializedError(value));
        }
        match self.init.try_acquire_raw(1) {
            Ok(()) => {
                let r = self
                    .value
                    .set(value)
                    .map_err(SetError::AlreadyInitializedError);
                self.init.release(1);
                r
            }
            Err(_) => Err(SetError::InitializingError(value)),
        }
    }

    pub async fn get_or_init<F, Fut>(&self, f: F) -> &T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        if let Some(v) = self.get() {
            return v;
        }
        let _ = self.init.acquire_raw(1).await;
        struct Release<'a>(&'a super::Semaphore);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.release(1);
            }
        }
        let _r = Release(&self.init);
        if let Some(v) = self.get() {
            return v;
        }
        let v = f().await;
        let _ = self.value.set(v);
        self.get().unwrap()
    }

    pub async fn get_or_try_init<E, F, Fut>(&self, f: F) -> Result<&T, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        if let Some(v) = self.get() {
            return Ok(v);
        }
        let _ = self.init.acquire_raw(1).await;
        struct Release<'a>(&'a super::Semaphore);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.release(1);
            }
        }
        let _r = Release(&self.init);
        if let Some(v) = self.get() {
            return Ok(v);
        }
        let v = f().await?;
        let _ = self.value.set(v);
        Ok(self.get().unwrap())
    }

    pub async fn wait(&self) -> &T {
        loop {
            if let Some(v) = self.get() {
                return v;
            }
            // 初期化中の Future が終わるのを待つ
            let _ = self.init.acquire_raw(1).await;
            self.init.release(1);
            if self.get().is_none() {
                crate::tokio::task::yield_now().await;
            }
        }
    }

    pub fn into_inner(self) -> Option<T> {
        self.value.into_inner()
    }

    pub fn take(&mut self) -> Option<T> {
        self.value.take()
    }
}

// ===================== SetOnce =====================

/// The error of `SetOnce::set` (it gives back the value).
#[derive(Debug, PartialEq, Eq)]
pub struct SetOnceError<T>(pub T);

impl<T> std::fmt::Display for SetOnceError<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SetOnceError")
    }
}

impl<T: std::fmt::Debug> std::error::Error for SetOnceError<T> {}

/// Equivalent of `tokio::sync::SetOnce`: the value can be set once, and `wait` waits until it is set.
pub struct SetOnce<T> {
    value: std::sync::OnceLock<T>,
    notify: Notify,
}

impl<T: std::fmt::Debug> std::fmt::Debug for SetOnce<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetOnce")
            .field("value", &self.get())
            .finish()
    }
}

impl<T> Default for SetOnce<T> {
    fn default() -> Self {
        SetOnce::new()
    }
}

impl<T: Clone> Clone for SetOnce<T> {
    fn clone(&self) -> Self {
        SetOnce::new_with(self.get().cloned())
    }
}

impl<T: PartialEq> PartialEq for SetOnce<T> {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

impl<T: Eq> Eq for SetOnce<T> {}

impl<T> From<T> for SetOnce<T> {
    fn from(v: T) -> Self {
        SetOnce::new_with(Some(v))
    }
}

impl<T> SetOnce<T> {
    pub fn new() -> SetOnce<T> {
        SetOnce::const_new()
    }

    pub const fn const_new() -> SetOnce<T> {
        SetOnce {
            value: std::sync::OnceLock::new(),
            notify: Notify::const_new(),
        }
    }

    pub fn new_with(value: Option<T>) -> SetOnce<T> {
        let s = SetOnce::new();
        if let Some(v) = value {
            let _ = s.value.set(v);
        }
        s
    }

    pub fn initialized(&self) -> bool {
        self.value.get().is_some()
    }

    pub fn get(&self) -> Option<&T> {
        self.value.get()
    }

    pub fn set(&self, value: T) -> Result<(), SetOnceError<T>> {
        self.value.set(value).map_err(SetOnceError)?;
        self.notify.notify_waiters();
        Ok(())
    }

    pub fn into_inner(self) -> Option<T> {
        self.value.into_inner()
    }

    pub async fn wait(&self) -> &T {
        loop {
            let notified = self.notify.notified();
            if let Some(v) = self.get() {
                return v;
            }
            notified.await;
        }
    }
}
