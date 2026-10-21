//! Equivalent of `tokio::sync::broadcast`: delivers to every receiver; a receiver that falls behind gets `Lagged`.

use super::watch::WaitList;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

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

    /// The error of `recv`.
    #[derive(Debug, PartialEq, Eq, Clone)]
    pub enum RecvError {
        Closed,
        Lagged(u64),
    }

    impl std::fmt::Display for RecvError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                RecvError::Closed => f.write_str("channel closed"),
                RecvError::Lagged(n) => write!(f, "channel lagged by {}", n),
            }
        }
    }

    impl std::error::Error for RecvError {}

    /// The error of `try_recv`.
    #[derive(Debug, PartialEq, Eq, Clone)]
    pub enum TryRecvError {
        Empty,
        Closed,
        Lagged(u64),
    }

    impl std::fmt::Display for TryRecvError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                TryRecvError::Empty => f.write_str("channel empty"),
                TryRecvError::Closed => f.write_str("channel closed"),
                TryRecvError::Lagged(n) => write!(f, "channel lagged by {}", n),
            }
        }
    }

    impl std::error::Error for TryRecvError {}
}

use error::{RecvError, SendError, TryRecvError};

struct Buf<T> {
    /// (position, value, number of receivers that have not read it yet)
    items: VecDeque<(u64, T, usize)>,
    /// The next position to write.
    tail: u64,
    closed: bool,
}

struct Shared<T> {
    buf: Mutex<Buf<T>>,
    cap: usize,
    rx_count: AtomicUsize,
    tx_count: AtomicUsize,
    weak_count: AtomicUsize,
    wait: WaitList,
}

/// A broadcast channel with the given `capacity`.
#[track_caller]
pub fn channel<T: Clone>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    assert!(capacity > 0, "broadcast channel capacity cannot be zero");
    assert!(
        capacity <= usize::MAX >> 1,
        "broadcast channel capacity exceeded `usize::MAX / 2`"
    );
    let s = Arc::new(Shared {
        buf: Mutex::new(Buf {
            items: VecDeque::new(),
            tail: 0,
            closed: false,
        }),
        cap: capacity.next_power_of_two(),
        rx_count: AtomicUsize::new(1),
        tx_count: AtomicUsize::new(1),
        weak_count: AtomicUsize::new(0),
        wait: WaitList::default(),
    });
    (
        Sender { shared: s.clone() },
        Receiver { shared: s, next: 0 },
    )
}

/// The sender.
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

/// The receiver.
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
    next: u64,
}

/// A weak reference to a sender that holds no receiver.
pub struct WeakSender<T> {
    shared: Arc<Shared<T>>,
}

impl<T> std::fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("broadcast::Sender")
    }
}

impl<T> std::fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("broadcast::Receiver")
    }
}

impl<T> Sender<T> {
    pub fn new(capacity: usize) -> Sender<T>
    where
        T: Clone,
    {
        let (tx, rx) = channel(capacity);
        drop(rx);
        tx
    }

    /// Sends to every receiver. Returns the number of receivers that will get it (fails if there are none).
    pub fn send(&self, value: T) -> Result<usize, SendError<T>> {
        let n = self.shared.rx_count.load(Ordering::Acquire);
        if n == 0 {
            return Err(SendError(value));
        }
        {
            let mut b = self.shared.buf.lock().unwrap_or_else(|e| e.into_inner());
            let pos = b.tail;
            b.items.push_back((pos, value, n));
            b.tail += 1;
            while b.items.len() > self.shared.cap {
                b.items.pop_front();
            }
        }
        self.shared.wait.wake_all();
        Ok(n)
    }

    pub fn subscribe(&self) -> Receiver<T> {
        self.shared.rx_count.fetch_add(1, Ordering::AcqRel);
        let next = self
            .shared
            .buf
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .tail;
        Receiver {
            shared: self.shared.clone(),
            next,
        }
    }

    pub fn receiver_count(&self) -> usize {
        self.shared.rx_count.load(Ordering::Acquire)
    }

    pub fn len(&self) -> usize {
        let b = self.shared.buf.lock().unwrap_or_else(|e| e.into_inner());
        b.items.iter().filter(|(_, _, left)| *left > 0).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn same_channel(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }

    pub fn downgrade(&self) -> WeakSender<T> {
        self.shared.weak_count.fetch_add(1, Ordering::AcqRel);
        WeakSender {
            shared: self.shared.clone(),
        }
    }

    pub fn strong_count(&self) -> usize {
        self.shared.tx_count.load(Ordering::Acquire)
    }

    pub fn weak_count(&self) -> usize {
        self.shared.weak_count.load(Ordering::Acquire)
    }

    pub async fn closed(&self) {
        let mut key = None;
        std::future::poll_fn(|cx| {
            if self.shared.rx_count.load(Ordering::Acquire) == 0 {
                return Poll::Ready(());
            }
            self.shared.wait.register(&mut key, cx);
            if self.shared.rx_count.load(Ordering::Acquire) == 0 {
                return Poll::Ready(());
            }
            Poll::Pending
        })
        .await;
        self.shared.wait.remove(&mut key);
    }
}

impl<T> Clone for WeakSender<T> {
    fn clone(&self) -> Self {
        self.shared.weak_count.fetch_add(1, Ordering::AcqRel);
        WeakSender {
            shared: self.shared.clone(),
        }
    }
}

impl<T> Drop for WeakSender<T> {
    fn drop(&mut self) {
        self.shared.weak_count.fetch_sub(1, Ordering::AcqRel);
    }
}

impl<T> std::fmt::Debug for WeakSender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("broadcast::WeakSender")
    }
}

impl<T> WeakSender<T> {
    pub fn strong_count(&self) -> usize {
        self.shared.tx_count.load(Ordering::Acquire)
    }

    pub fn weak_count(&self) -> usize {
        self.shared.weak_count.load(Ordering::Acquire)
    }

    pub fn upgrade(&self) -> Option<Sender<T>> {
        let mut n = self.shared.tx_count.load(Ordering::Acquire);
        loop {
            if n == 0 {
                return None;
            }
            match self.shared.tx_count.compare_exchange_weak(
                n,
                n + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(Sender {
                        shared: self.shared.clone(),
                    })
                }
                Err(cur) => n = cur,
            }
        }
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

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.shared.tx_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shared
                .buf
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .closed = true;
            self.shared.wait.wake_all();
        }
    }
}

impl<T: Clone> Receiver<T> {
    fn try_take(&mut self) -> Result<T, TryRecvError> {
        let mut b = self.shared.buf.lock().unwrap_or_else(|e| e.into_inner());
        let oldest = b.items.front().map(|(p, _, _)| *p).unwrap_or(b.tail);
        if self.next < oldest {
            let missed = oldest - self.next;
            self.next = oldest;
            return Err(TryRecvError::Lagged(missed));
        }
        if self.next >= b.tail {
            return if b.closed {
                Err(TryRecvError::Closed)
            } else {
                Err(TryRecvError::Empty)
            };
        }
        let idx = (self.next - oldest) as usize;
        // clone が panic しても読んだことにする（tokio と同じく次は後ろの値）
        self.next += 1;
        let item = &mut b.items[idx];
        item.2 = item.2.saturating_sub(1);
        let v = item.1.clone();
        let drained = drain_consumed(&mut b);
        drop(b);
        drop(drained);
        Ok(v)
    }

    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.try_take()
    }

    pub async fn recv(&mut self) -> Result<T, RecvError> {
        let mut key = None;
        let shared = self.shared.clone();
        let r = std::future::poll_fn(|cx| {
            std::task::ready!(crate::tokio::task::coop::poll_proceed_and_make_progress(cx));
            for _ in 0..2 {
                match self.try_take() {
                    Ok(v) => return Poll::Ready(Ok(v)),
                    Err(TryRecvError::Closed) => return Poll::Ready(Err(RecvError::Closed)),
                    Err(TryRecvError::Lagged(n)) => return Poll::Ready(Err(RecvError::Lagged(n))),
                    Err(TryRecvError::Empty) => shared.wait.register(&mut key, cx),
                }
            }
            Poll::Pending
        })
        .await;
        shared.wait.remove(&mut key);
        r
    }

    pub fn blocking_recv(&mut self) -> Result<T, RecvError> {
        if crate::runtime::has_driver() {
            panic!("Cannot block the current thread from within a runtime.");
        }
        futures::executor::block_on(self.recv())
    }
}

impl<T> Receiver<T> {
    pub fn resubscribe(&self) -> Receiver<T> {
        self.shared.rx_count.fetch_add(1, Ordering::AcqRel);
        let next = self
            .shared
            .buf
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .tail;
        Receiver {
            shared: self.shared.clone(),
            next,
        }
    }

    pub fn len(&self) -> usize {
        let b = self.shared.buf.lock().unwrap_or_else(|e| e.into_inner());
        (b.tail.saturating_sub(self.next)) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_closed(&self) -> bool {
        self.shared
            .buf
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closed
    }

    pub fn sender_strong_count(&self) -> usize {
        self.shared.tx_count.load(Ordering::Acquire)
    }

    pub fn sender_weak_count(&self) -> usize {
        self.shared.weak_count.load(Ordering::Acquire)
    }

    pub fn same_channel(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

/// Drops values that everyone has read, from the front (the caller drops the removed values outside the lock).
fn drain_consumed<T>(b: &mut Buf<T>) -> Vec<T> {
    let mut out = Vec::new();
    while b.items.front().is_some_and(|(_, _, left)| *left == 0) {
        out.push(b.items.pop_front().unwrap().1);
    }
    out
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        // まだ読んでいない値の「残りの受け手」から自分を外す
        let drained = {
            let mut b = self.shared.buf.lock().unwrap_or_else(|e| e.into_inner());
            let next = self.next;
            for item in b.items.iter_mut() {
                if item.0 >= next {
                    item.2 = item.2.saturating_sub(1);
                }
            }
            drain_consumed(&mut b)
        };
        let last = self.shared.rx_count.fetch_sub(1, Ordering::AcqRel) == 1;
        drop(drained);
        if last {
            self.shared.wait.wake_all();
        }
    }
}
