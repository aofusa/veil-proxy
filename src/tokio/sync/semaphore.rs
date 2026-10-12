//! `tokio::sync::Semaphore`（FIFO で公平。`Mutex`・`RwLock`・`mpsc` の土台）。

use crate::tokio::waker::WakerSlot;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

struct Node {
    needed: usize,
    granted: AtomicBool,
    waker: Mutex<WakerSlot>,
}

struct State {
    permits: usize,
    closed: bool,
    waiters: VecDeque<Arc<Node>>,
}

/// `tokio::sync::Semaphore`。
pub struct Semaphore {
    state: Mutex<State>,
}

impl std::fmt::Debug for Semaphore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Semaphore")
            .field("permits", &self.available_permits())
            .finish()
    }
}

/// 閉じたセマフォを待った。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquireError(());

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("semaphore closed")
    }
}

impl std::error::Error for AcquireError {}

/// `try_acquire` の失敗。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TryAcquireError {
    Closed,
    NoPermits,
}

impl std::fmt::Display for TryAcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TryAcquireError::Closed => f.write_str("semaphore closed"),
            TryAcquireError::NoPermits => f.write_str("no permits available"),
        }
    }
}

impl std::error::Error for TryAcquireError {}

impl Semaphore {
    /// 許可数の上限。
    pub const MAX_PERMITS: usize = usize::MAX >> 3;

    pub fn new(permits: usize) -> Semaphore {
        assert!(
            permits <= Self::MAX_PERMITS,
            "a semaphore may not have more than MAX_PERMITS permits ({})",
            Self::MAX_PERMITS
        );
        Semaphore {
            state: Mutex::new(State {
                permits,
                closed: false,
                waiters: VecDeque::new(),
            }),
        }
    }

    pub const fn const_new(permits: usize) -> Semaphore {
        Semaphore {
            state: Mutex::new(State {
                permits,
                closed: false,
                waiters: VecDeque::new(),
            }),
        }
    }

    pub fn available_permits(&self) -> usize {
        self.state.lock().unwrap().permits
    }

    /// 許可を足す（待っている先頭から順に渡す）。
    pub fn add_permits(&self, n: usize) {
        self.release(n);
    }

    /// 許可を `n` 個まで減らす（減らせた数）。
    pub fn forget_permits(&self, n: usize) -> usize {
        let mut st = self.state.lock().unwrap();
        let m = st.permits.min(n);
        st.permits -= m;
        m
    }

    pub(crate) fn release(&self, n: usize) {
        if n == 0 {
            return;
        }
        let mut wake = Vec::new();
        {
            let mut st = self.state.lock().unwrap();
            st.permits = st.permits.saturating_add(n);
            assert!(
                st.permits <= Self::MAX_PERMITS,
                "number of added permits ({}) would overflow MAX_PERMITS ({})",
                n,
                Self::MAX_PERMITS
            );
            while let Some(front) = st.waiters.front() {
                if front.needed > st.permits {
                    break;
                }
                st.permits -= front.needed;
                let node = st.waiters.pop_front().unwrap();
                node.granted.store(true, Ordering::Release);
                wake.push(node);
            }
        }
        for node in wake {
            node.waker.lock().unwrap().wake();
        }
    }

    pub fn close(&self) {
        let waiters: Vec<_> = {
            let mut st = self.state.lock().unwrap();
            st.closed = true;
            st.waiters.drain(..).collect()
        };
        for n in waiters {
            n.waker.lock().unwrap().wake();
        }
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }

    pub(crate) fn try_acquire_raw(&self, n: usize) -> Result<(), TryAcquireError> {
        let mut st = self.state.lock().unwrap();
        if st.closed {
            return Err(TryAcquireError::Closed);
        }
        if st.waiters.is_empty() && st.permits >= n {
            st.permits -= n;
            Ok(())
        } else {
            Err(TryAcquireError::NoPermits)
        }
    }

    pub(crate) fn acquire_raw(&self, n: usize) -> Acquire<'_> {
        Acquire {
            sem: self,
            n,
            node: None,
            done: false,
        }
    }

    pub async fn acquire(&self) -> Result<SemaphorePermit<'_>, AcquireError> {
        self.acquire_raw(1).await?;
        Ok(SemaphorePermit {
            sem: self,
            permits: 1,
        })
    }

    pub async fn acquire_many(&self, n: u32) -> Result<SemaphorePermit<'_>, AcquireError> {
        self.acquire_raw(n as usize).await?;
        Ok(SemaphorePermit {
            sem: self,
            permits: n as usize,
        })
    }

    pub fn try_acquire(&self) -> Result<SemaphorePermit<'_>, TryAcquireError> {
        self.try_acquire_raw(1)?;
        Ok(SemaphorePermit {
            sem: self,
            permits: 1,
        })
    }

    pub fn try_acquire_many(&self, n: u32) -> Result<SemaphorePermit<'_>, TryAcquireError> {
        self.try_acquire_raw(n as usize)?;
        Ok(SemaphorePermit {
            sem: self,
            permits: n as usize,
        })
    }

    pub async fn acquire_owned(self: Arc<Self>) -> Result<OwnedSemaphorePermit, AcquireError> {
        self.acquire_raw(1).await?;
        Ok(OwnedSemaphorePermit {
            sem: self,
            permits: 1,
        })
    }

    pub async fn acquire_many_owned(
        self: Arc<Self>,
        n: u32,
    ) -> Result<OwnedSemaphorePermit, AcquireError> {
        self.acquire_raw(n as usize).await?;
        Ok(OwnedSemaphorePermit {
            sem: self,
            permits: n as usize,
        })
    }

    pub fn try_acquire_owned(self: Arc<Self>) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        self.try_acquire_raw(1)?;
        Ok(OwnedSemaphorePermit {
            sem: self,
            permits: 1,
        })
    }

    pub fn try_acquire_many_owned(
        self: Arc<Self>,
        n: u32,
    ) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        self.try_acquire_raw(n as usize)?;
        Ok(OwnedSemaphorePermit {
            sem: self,
            permits: n as usize,
        })
    }
}

/// 許可を待つ Future（drop で列から抜ける。渡された後なら返す）。
pub(crate) struct Acquire<'a> {
    sem: &'a Semaphore,
    n: usize,
    node: Option<Arc<Node>>,
    done: bool,
}

impl Future for Acquire<'_> {
    type Output = Result<(), AcquireError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(node) = &this.node {
            if node.granted.load(Ordering::Acquire) {
                this.done = true;
                return Poll::Ready(Ok(()));
            }
            let st = this.sem.state.lock().unwrap();
            if node.granted.load(Ordering::Acquire) {
                this.done = true;
                return Poll::Ready(Ok(()));
            }
            if st.closed {
                this.done = true;
                return Poll::Ready(Err(AcquireError(())));
            }
            node.waker.lock().unwrap().register(cx.waker());
            return Poll::Pending;
        }
        let mut st = this.sem.state.lock().unwrap();
        if st.closed {
            this.done = true;
            return Poll::Ready(Err(AcquireError(())));
        }
        if st.waiters.is_empty() && st.permits >= this.n {
            st.permits -= this.n;
            this.done = true;
            return Poll::Ready(Ok(()));
        }
        let node = Arc::new(Node {
            needed: this.n,
            granted: AtomicBool::new(false),
            waker: Mutex::new(WakerSlot::default()),
        });
        node.waker.lock().unwrap().register(cx.waker());
        st.waiters.push_back(node.clone());
        this.node = Some(node);
        Poll::Pending
    }
}

impl Drop for Acquire<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let Some(node) = self.node.take() else {
            return;
        };
        let granted = {
            let mut st = self.sem.state.lock().unwrap();
            if node.granted.load(Ordering::Acquire) {
                true
            } else {
                st.waiters.retain(|n| !Arc::ptr_eq(n, &node));
                false
            }
        };
        if granted {
            // 渡された許可を受け取らずに去る: 返す
            self.sem.release(self.n);
        } else {
            // 先頭が抜けたことで後ろの待ちが通れるかもしれない
            let wake = {
                let mut st = self.sem.state.lock().unwrap();
                let mut out = Vec::new();
                while let Some(front) = st.waiters.front() {
                    if front.needed > st.permits {
                        break;
                    }
                    st.permits -= front.needed;
                    let n = st.waiters.pop_front().unwrap();
                    n.granted.store(true, Ordering::Release);
                    out.push(n);
                }
                out
            };
            for n in wake {
                n.waker.lock().unwrap().wake();
            }
        }
    }
}

/// 借用の許可（drop で返す）。
#[must_use]
pub struct SemaphorePermit<'a> {
    sem: &'a Semaphore,
    permits: usize,
}

impl std::fmt::Debug for SemaphorePermit<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SemaphorePermit")
            .field("permits", &self.permits)
            .finish()
    }
}

impl SemaphorePermit<'_> {
    /// 返さずに捨てる。
    pub fn forget(mut self) {
        self.permits = 0;
    }

    pub fn num_permits(&self) -> usize {
        self.permits
    }

    pub fn merge(&mut self, mut other: Self) {
        assert!(
            std::ptr::eq(self.sem, other.sem),
            "merging permits from different semaphore instances"
        );
        self.permits += other.permits;
        other.permits = 0;
    }

    pub fn split(&mut self, n: usize) -> Option<Self> {
        if n > self.permits {
            return None;
        }
        self.permits -= n;
        Some(SemaphorePermit {
            sem: self.sem,
            permits: n,
        })
    }
}

impl Drop for SemaphorePermit<'_> {
    fn drop(&mut self) {
        self.sem.release(self.permits);
    }
}

/// 所有の許可（drop で返す）。
#[must_use]
pub struct OwnedSemaphorePermit {
    sem: Arc<Semaphore>,
    permits: usize,
}

impl std::fmt::Debug for OwnedSemaphorePermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedSemaphorePermit")
            .field("permits", &self.permits)
            .finish()
    }
}

impl OwnedSemaphorePermit {
    pub fn forget(mut self) {
        self.permits = 0;
    }

    pub fn num_permits(&self) -> usize {
        self.permits
    }

    pub fn semaphore(&self) -> &Arc<Semaphore> {
        &self.sem
    }

    pub fn merge(&mut self, mut other: Self) {
        assert!(
            Arc::ptr_eq(&self.sem, &other.sem),
            "merging permits from different semaphore instances"
        );
        self.permits += other.permits;
        other.permits = 0;
    }

    pub fn split(&mut self, n: usize) -> Option<Self> {
        if n > self.permits {
            return None;
        }
        self.permits -= n;
        Some(OwnedSemaphorePermit {
            sem: self.sem.clone(),
            permits: n,
        })
    }
}

impl Drop for OwnedSemaphorePermit {
    fn drop(&mut self) {
        self.sem.release(self.permits);
    }
}
