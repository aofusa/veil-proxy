//! Equivalent of `tokio::sync::watch`: shares the latest value and notifies receivers of changes.

use crate::tokio::waker::WakerSlot;
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};
use std::task::{Context, Poll};

pub mod error {
    /// There are no receivers (the value is returned).
    #[derive(PartialEq, Eq, Clone, Copy)]
    pub struct SendError<T>(pub T);

    impl<T> std::fmt::Debug for SendError<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("SendError").finish_non_exhaustive()
        }
    }

    impl<T> std::fmt::Display for SendError<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("channel closed")
        }
    }

    impl<T> std::error::Error for SendError<T> {}

    /// There is no sender.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct RecvError(pub(super) ());

    impl std::fmt::Display for RecvError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("channel closed")
        }
    }

    impl std::error::Error for RecvError {}
}

use error::{RecvError, SendError};

/// Table of the wakers of waiting receivers (and of the sender's `closed`).
#[derive(Default)]
pub(crate) struct WaitList {
    next: AtomicU64,
    map: Mutex<HashMap<u64, WakerSlot>>,
}

impl WaitList {
    pub(crate) fn register(&self, key: &mut Option<u64>, cx: &Context<'_>) {
        let k = *key.get_or_insert_with(|| self.next.fetch_add(1, Ordering::Relaxed));
        // 入れ替えた古い Waker はロックの外で落とす（Drop が同じチャネルを触っても固まらないように）
        let old = {
            let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
            let slot = m.entry(k).or_default();
            slot.replace(cx.waker())
        };
        drop(old);
    }

    pub(crate) fn remove(&self, key: &mut Option<u64>) {
        if let Some(k) = key.take() {
            let old = self
                .map
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&k);
            drop(old);
        }
    }

    pub(crate) fn wake_all(&self) {
        let ws: Vec<WakerSlot> = self
            .map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, w)| w)
            .collect();
        for mut w in ws {
            w.wake();
        }
    }
}

struct Shared<T> {
    value: RwLock<T>,
    version: AtomicU64,
    closed: std::sync::atomic::AtomicBool,
    rx_count: AtomicUsize,
    tx_count: AtomicUsize,
    rx_wait: WaitList,
    tx_wait: WaitList,
}

/// A channel that holds a value (receivers see only the latest value).
pub fn channel<T>(init: T) -> (Sender<T>, Receiver<T>) {
    let s = Arc::new(Shared {
        value: RwLock::new(init),
        version: AtomicU64::new(0),
        closed: std::sync::atomic::AtomicBool::new(false),
        rx_count: AtomicUsize::new(1),
        tx_count: AtomicUsize::new(1),
        rx_wait: WaitList::default(),
        tx_wait: WaitList::default(),
    });
    (
        Sender { shared: s.clone() },
        Receiver {
            shared: s,
            version: 0,
        },
    )
}

/// The sender.
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

/// The receiver.
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
    version: u64,
}

impl<T> std::fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("watch::Sender")
    }
}

impl<T> std::fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("watch::Receiver")
    }
}

/// A reference to the value (holds a read lock).
pub struct Ref<'a, T> {
    inner: RwLockReadGuard<'a, T>,
    has_changed: bool,
}

impl<T> Ref<'_, T> {
    pub fn has_changed(&self) -> bool {
        self.has_changed
    }
}

impl<T> Deref for Ref<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Ref<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&*self.inner, f)
    }
}

impl<T> Sender<T> {
    pub fn new(init: T) -> Sender<T> {
        let (tx, rx) = channel(init);
        drop(rx);
        tx
    }

    /// Changes the value (fails if there are no receivers, and then the value is unchanged).
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        if self.shared.rx_count.load(Ordering::Acquire) == 0 {
            return Err(SendError(value));
        }
        self.send_replace(value);
        Ok(())
    }

    /// Changes the value whether or not there are receivers, and returns the previous value.
    pub fn send_replace(&self, value: T) -> T {
        let old = {
            let mut g = self.shared.value.write().unwrap_or_else(|e| e.into_inner());
            std::mem::replace(&mut *g, value)
        };
        self.shared.version.fetch_add(1, Ordering::AcqRel);
        self.shared.rx_wait.wake_all();
        old
    }

    pub fn send_modify<F: FnOnce(&mut T)>(&self, modify: F) {
        self.send_if_modified(|v| {
            modify(v);
            true
        });
    }

    pub fn send_if_modified<F: FnOnce(&mut T) -> bool>(&self, modify: F) -> bool {
        let modified = {
            let mut g = self.shared.value.write().unwrap_or_else(|e| e.into_inner());
            modify(&mut g)
        };
        if modified {
            self.shared.version.fetch_add(1, Ordering::AcqRel);
            self.shared.rx_wait.wake_all();
        }
        modified
    }

    pub fn borrow(&self) -> Ref<'_, T> {
        Ref {
            inner: self.shared.value.read().unwrap_or_else(|e| e.into_inner()),
            has_changed: false,
        }
    }

    pub fn is_closed(&self) -> bool {
        self.shared.rx_count.load(Ordering::Acquire) == 0
    }

    pub async fn closed(&self) {
        let mut key = None;
        std::future::poll_fn(|cx| {
            std::task::ready!(crate::tokio::task::coop::poll_proceed_and_make_progress(cx));
            if self.is_closed() {
                return Poll::Ready(());
            }
            self.shared.tx_wait.register(&mut key, cx);
            if self.is_closed() {
                return Poll::Ready(());
            }
            Poll::Pending
        })
        .await;
        self.shared.tx_wait.remove(&mut key);
    }

    pub fn subscribe(&self) -> Receiver<T> {
        self.shared.rx_count.fetch_add(1, Ordering::AcqRel);
        Receiver {
            shared: self.shared.clone(),
            version: self.shared.version.load(Ordering::Acquire),
        }
    }

    pub fn receiver_count(&self) -> usize {
        self.shared.rx_count.load(Ordering::Acquire)
    }

    pub fn sender_count(&self) -> usize {
        self.shared.tx_count.load(Ordering::Acquire)
    }

    pub fn same_channel(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.shared.tx_count.fetch_add(1, Ordering::AcqRel);
        Sender {
            shared: self.shared.clone(),
        }
    }
}

impl<T: Default> Default for Sender<T> {
    fn default() -> Self {
        Sender::new(T::default())
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.shared.tx_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shared.closed.store(true, Ordering::Release);
            self.shared.rx_wait.wake_all();
        }
    }
}

impl<T> Receiver<T> {
    pub fn borrow(&self) -> Ref<'_, T> {
        let has_changed = self.shared.version.load(Ordering::Acquire) != self.version;
        Ref {
            inner: self.shared.value.read().unwrap_or_else(|e| e.into_inner()),
            has_changed,
        }
    }

    pub fn borrow_and_update(&mut self) -> Ref<'_, T> {
        let inner = self.shared.value.read().unwrap_or_else(|e| e.into_inner());
        let v = self.shared.version.load(Ordering::Acquire);
        let has_changed = v != self.version;
        self.version = v;
        Ref { inner, has_changed }
    }

    pub fn has_changed(&self) -> Result<bool, RecvError> {
        // tokio と同じく、送り手がいなければ未読の値があってもエラー
        if self.shared.closed.load(Ordering::Acquire) {
            return Err(RecvError(()));
        }
        Ok(self.shared.version.load(Ordering::Acquire) != self.version)
    }

    pub fn mark_changed(&mut self) {
        self.version = self.version.wrapping_sub(1);
    }

    pub fn mark_unchanged(&mut self) {
        self.version = self.shared.version.load(Ordering::Acquire);
    }

    /// Waits until the value changes from the version last seen (returns `Err` when all senders are gone).
    pub async fn changed(&mut self) -> Result<(), RecvError> {
        let mut key = None;
        let shared = self.shared.clone();
        let seen = &mut self.version;
        let r = std::future::poll_fn(|cx| {
            let coop = std::task::ready!(crate::tokio::task::coop::poll_proceed(cx));
            coop.made_progress();
            let v = shared.version.load(Ordering::Acquire);
            if v != *seen {
                *seen = v;
                return Poll::Ready(Ok(()));
            }
            if shared.closed.load(Ordering::Acquire) {
                return Poll::Ready(Err(RecvError(())));
            }
            shared.rx_wait.register(&mut key, cx);
            let v = shared.version.load(Ordering::Acquire);
            if v != *seen {
                *seen = v;
                return Poll::Ready(Ok(()));
            }
            if shared.closed.load(Ordering::Acquire) {
                return Poll::Ready(Err(RecvError(())));
            }
            Poll::Pending
        })
        .await;
        shared.rx_wait.remove(&mut key);
        r
    }

    /// Waits until the value satisfies the condition.
    pub async fn wait_for<F: FnMut(&T) -> bool>(
        &mut self,
        mut f: F,
    ) -> Result<Ref<'_, T>, RecvError> {
        loop {
            // 条件をすぐ満たしても予算を使う（tokio と同じく協調的）
            std::future::poll_fn(crate::tokio::task::coop::poll_proceed_and_make_progress).await;
            {
                let r = self.borrow_and_update();
                if f(&r) {
                    break;
                }
            }
            self.changed().await?;
        }
        Ok(self.borrow())
    }

    pub fn same_channel(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        self.shared.rx_count.fetch_add(1, Ordering::AcqRel);
        Receiver {
            shared: self.shared.clone(),
            version: self.version,
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        if self.shared.rx_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shared.tx_wait.wake_all();
        }
    }
}
