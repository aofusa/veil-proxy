//! `tokio::sync::mpsc`（有界はセマフォで容量を数える。無界は数えない）。

use super::semaphore::Semaphore;
use crate::tokio::waker::WakerSlot;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

pub mod error {
    /// 受け手が閉じていて送れなかった（値を返す）。
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

    /// `try_send` の失敗。
    #[derive(PartialEq, Eq, Clone, Copy)]
    pub enum TrySendError<T> {
        Full(T),
        Closed(T),
    }

    impl<T> TrySendError<T> {
        pub fn into_inner(self) -> T {
            match self {
                TrySendError::Full(v) | TrySendError::Closed(v) => v,
            }
        }
    }

    impl<T> std::fmt::Debug for TrySendError<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                TrySendError::Full(_) => f.write_str("Full(..)"),
                TrySendError::Closed(_) => f.write_str("Closed(..)"),
            }
        }
    }

    impl<T> std::fmt::Display for TrySendError<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                TrySendError::Full(_) => f.write_str("no available capacity"),
                TrySendError::Closed(_) => f.write_str("channel closed"),
            }
        }
    }

    impl<T> std::error::Error for TrySendError<T> {}

    impl<T> From<SendError<T>> for TrySendError<T> {
        fn from(e: SendError<T>) -> Self {
            TrySendError::Closed(e.0)
        }
    }

    /// `try_recv` の失敗。
    #[derive(PartialEq, Eq, Clone, Copy, Debug)]
    pub enum TryRecvError {
        Empty,
        Disconnected,
    }

    impl std::fmt::Display for TryRecvError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                TryRecvError::Empty => f.write_str("receiving on an empty channel"),
                TryRecvError::Disconnected => f.write_str("receiving on a closed channel"),
            }
        }
    }

    impl std::error::Error for TryRecvError {}

    /// `send_timeout` の失敗。
    #[derive(PartialEq, Eq, Clone, Copy)]
    pub enum SendTimeoutError<T> {
        Timeout(T),
        Closed(T),
    }

    impl<T> std::fmt::Debug for SendTimeoutError<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                SendTimeoutError::Timeout(_) => f.write_str("Timeout(..)"),
                SendTimeoutError::Closed(_) => f.write_str("Closed(..)"),
            }
        }
    }

    impl<T> std::fmt::Display for SendTimeoutError<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                SendTimeoutError::Timeout(_) => f.write_str("timed out waiting on send operation"),
                SendTimeoutError::Closed(_) => f.write_str("channel closed"),
            }
        }
    }

    impl<T> std::error::Error for SendTimeoutError<T> {}
}

use error::{SendError, SendTimeoutError, TryRecvError, TrySendError};

struct Queue<T> {
    items: VecDeque<T>,
    rx_closed: bool,
    /// 受け手が drop された（閉じただけなら予約済みの送信は受け付ける）
    rx_dropped: bool,
    rx_waker: WakerSlot,
    closed_wakers: Vec<WakerSlot>,
}

struct Chan<T> {
    q: Mutex<Queue<T>>,
    /// 有界なら容量のセマフォ
    sem: Option<Semaphore>,
    cap: usize,
    tx_count: AtomicUsize,
    weak_count: AtomicUsize,
}

impl<T> Chan<T> {
    fn new(cap: Option<usize>) -> Arc<Chan<T>> {
        Arc::new(Chan {
            q: Mutex::new(Queue {
                items: VecDeque::new(),
                rx_closed: false,
                rx_dropped: false,
                rx_waker: WakerSlot::default(),
                closed_wakers: Vec::new(),
            }),
            sem: cap.map(Semaphore::new),
            cap: cap.unwrap_or(usize::MAX),
            tx_count: AtomicUsize::new(1),
            weak_count: AtomicUsize::new(0),
        })
    }

    /// 値を積む（受け手が drop されていれば返す）。容量の許可は呼び出し側が取得済み。
    /// tokio と同じく、受け手が `close` しただけなら閉じる前に取った許可での送信は届く。
    fn push(&self, v: T) -> Result<(), T> {
        let w = {
            let mut q = self.q.lock().unwrap();
            if q.rx_dropped || (q.rx_closed && self.sem.is_none()) {
                return Err(v);
            }
            q.items.push_back(v);
            q.rx_waker.take()
        };
        if let Some(w) = w {
            w.wake();
        }
        Ok(())
    }

    fn is_closed(&self) -> bool {
        self.q.lock().unwrap().rx_closed
    }

    /// 予約された（まだ送られていない）許可が無い。
    fn idle(&self) -> bool {
        self.sem
            .as_ref()
            .is_none_or(|s| s.available_permits() == self.cap)
    }

    /// 許可を返す。受け手が閉じていれば、最後の予約が消えたことを受け手に知らせる。
    fn release(&self, n: usize) {
        if let Some(s) = &self.sem {
            s.release(n);
        }
        let w = {
            let mut q = self.q.lock().unwrap();
            if q.rx_closed {
                q.rx_waker.take()
            } else {
                None
            }
        };
        if let Some(w) = w {
            w.wake();
        }
    }

    fn close_rx(&self) {
        let ws: Vec<_> = {
            let mut q = self.q.lock().unwrap();
            q.rx_closed = true;
            std::mem::take(&mut q.closed_wakers)
        };
        if let Some(s) = &self.sem {
            s.close();
        }
        for mut w in ws {
            w.wake();
        }
    }

    fn poll_closed(&self, cx: &mut Context<'_>) -> Poll<()> {
        let mut q = self.q.lock().unwrap();
        if q.rx_closed {
            return Poll::Ready(());
        }
        let mut slot = WakerSlot::default();
        slot.register(cx.waker());
        q.closed_wakers.push(slot);
        Poll::Pending
    }

    fn sender_dropped(&self) {
        if self.tx_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            let w = self.q.lock().unwrap().rx_waker.take();
            if let Some(w) = w {
                w.wake();
            }
        }
    }

    fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let coop = std::task::ready!(crate::tokio::task::coop::poll_proceed(cx));
        let r = self.poll_recv_inner(cx);
        if r.is_ready() {
            coop.made_progress();
        }
        r
    }

    fn poll_recv_inner(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut q = self.q.lock().unwrap();
        if let Some(v) = q.items.pop_front() {
            drop(q);
            if let Some(s) = &self.sem {
                s.release(1);
            }
            return Poll::Ready(Some(v));
        }
        if self.tx_count.load(Ordering::Acquire) == 0 || (q.rx_closed && self.idle()) {
            return Poll::Ready(None);
        }
        q.rx_waker.register(cx.waker());
        // 登録の後にもう一度（最後の送り手・予約が今消えたかもしれない）
        if self.tx_count.load(Ordering::Acquire) == 0 || (q.rx_closed && self.idle()) {
            return Poll::Ready(None);
        }
        Poll::Pending
    }

    fn try_recv(&self) -> Result<T, TryRecvError> {
        let mut q = self.q.lock().unwrap();
        if let Some(v) = q.items.pop_front() {
            drop(q);
            if let Some(s) = &self.sem {
                s.release(1);
            }
            return Ok(v);
        }
        if self.tx_count.load(Ordering::Acquire) == 0 || (q.rx_closed && self.idle()) {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    fn len(&self) -> usize {
        self.q.lock().unwrap().items.len()
    }
}

// ===================== 有界 =====================

/// 有界チャネル（`buffer` は 1 以上）。
#[track_caller]
pub fn channel<T>(buffer: usize) -> (Sender<T>, Receiver<T>) {
    assert!(buffer > 0, "mpsc bounded channel requires buffer > 0");
    let c = Chan::new(Some(buffer));
    (Sender { chan: c.clone() }, Receiver { chan: c })
}

/// 有界チャネルの送り手。
pub struct Sender<T> {
    chan: Arc<Chan<T>>,
}

/// 有界チャネルの弱い送り手（チャネルを生かさない）。
pub struct WeakSender<T> {
    chan: Arc<Chan<T>>,
}

/// 有界チャネルの受け手。
pub struct Receiver<T> {
    chan: Arc<Chan<T>>,
}

impl<T> std::fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sender")
    }
}

impl<T> std::fmt::Debug for WeakSender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WeakSender")
    }
}

impl<T> std::fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Receiver")
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.chan.tx_count.fetch_add(1, Ordering::AcqRel);
        Sender {
            chan: self.chan.clone(),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.chan.sender_dropped();
    }
}

impl<T> Clone for WeakSender<T> {
    fn clone(&self) -> Self {
        self.chan.weak_count.fetch_add(1, Ordering::AcqRel);
        WeakSender {
            chan: self.chan.clone(),
        }
    }
}

impl<T> Drop for WeakSender<T> {
    fn drop(&mut self) {
        self.chan.weak_count.fetch_sub(1, Ordering::AcqRel);
    }
}

impl<T> WeakSender<T> {
    pub fn upgrade(&self) -> Option<Sender<T>> {
        let mut n = self.chan.tx_count.load(Ordering::Acquire);
        loop {
            if n == 0 {
                return None;
            }
            match self.chan.tx_count.compare_exchange_weak(
                n,
                n + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(Sender {
                        chan: self.chan.clone(),
                    })
                }
                Err(cur) => n = cur,
            }
        }
    }

    pub fn strong_count(&self) -> usize {
        self.chan.tx_count.load(Ordering::Acquire)
    }

    pub fn weak_count(&self) -> usize {
        self.chan.weak_count.load(Ordering::Acquire)
    }
}

impl<T> Sender<T> {
    fn sem(&self) -> &Semaphore {
        self.chan.sem.as_ref().unwrap()
    }

    pub async fn send(&self, value: T) -> Result<(), SendError<T>> {
        if self.sem().acquire_raw(1).await.is_err() {
            return Err(SendError(value));
        }
        self.chan.push(value).map_err(|v| {
            self.sem().release(1);
            SendError(v)
        })
    }

    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        match self.sem().try_acquire_raw(1) {
            Ok(()) => self.chan.push(value).map_err(|v| {
                self.sem().release(1);
                TrySendError::Closed(v)
            }),
            Err(super::TryAcquireError::Closed) => Err(TrySendError::Closed(value)),
            Err(super::TryAcquireError::NoPermits) => Err(TrySendError::Full(value)),
        }
    }

    pub async fn send_timeout(
        &self,
        value: T,
        timeout: std::time::Duration,
    ) -> Result<(), SendTimeoutError<T>> {
        match crate::tokio::time::timeout(timeout, self.sem().acquire_raw(1)).await {
            Err(_) => Err(SendTimeoutError::Timeout(value)),
            Ok(Err(_)) => Err(SendTimeoutError::Closed(value)),
            Ok(Ok(())) => self.chan.push(value).map_err(|v| {
                self.sem().release(1);
                SendTimeoutError::Closed(v)
            }),
        }
    }

    #[track_caller]
    pub fn blocking_send(&self, value: T) -> Result<(), SendError<T>> {
        if crate::runtime::has_driver() {
            panic!("Cannot block the current thread from within a runtime.");
        }
        futures::executor::block_on(self.send(value))
    }

    /// 容量を 1 つ予約する。
    pub async fn reserve(&self) -> Result<Permit<'_, T>, SendError<()>> {
        self.sem().acquire_raw(1).await.map_err(|_| SendError(()))?;
        Ok(Permit { chan: &self.chan })
    }

    pub async fn reserve_many(&self, n: usize) -> Result<PermitIterator<'_, T>, SendError<()>> {
        // 容量を超える予約は決して満たせない（tokio と同じくすぐ失敗）
        if n > self.chan.cap {
            return Err(SendError(()));
        }
        self.sem().acquire_raw(n).await.map_err(|_| SendError(()))?;
        Ok(PermitIterator {
            chan: &self.chan,
            n,
        })
    }

    pub fn try_reserve_many(&self, n: usize) -> Result<PermitIterator<'_, T>, TrySendError<()>> {
        if n > self.chan.cap {
            return Err(TrySendError::Full(()));
        }
        match self.sem().try_acquire_raw(n) {
            Ok(()) => Ok(PermitIterator {
                chan: &self.chan,
                n,
            }),
            Err(super::TryAcquireError::Closed) => Err(TrySendError::Closed(())),
            Err(super::TryAcquireError::NoPermits) => Err(TrySendError::Full(())),
        }
    }

    pub fn try_reserve(&self) -> Result<Permit<'_, T>, TrySendError<()>> {
        match self.sem().try_acquire_raw(1) {
            Ok(()) => Ok(Permit { chan: &self.chan }),
            Err(super::TryAcquireError::Closed) => Err(TrySendError::Closed(())),
            Err(super::TryAcquireError::NoPermits) => Err(TrySendError::Full(())),
        }
    }

    pub async fn reserve_owned(self) -> Result<OwnedPermit<T>, SendError<()>> {
        self.sem().acquire_raw(1).await.map_err(|_| SendError(()))?;
        Ok(OwnedPermit { tx: Some(self) })
    }

    pub fn try_reserve_owned(self) -> Result<OwnedPermit<T>, TrySendError<Self>> {
        match self.sem().try_acquire_raw(1) {
            Ok(()) => Ok(OwnedPermit { tx: Some(self) }),
            Err(super::TryAcquireError::Closed) => Err(TrySendError::Closed(self)),
            Err(super::TryAcquireError::NoPermits) => Err(TrySendError::Full(self)),
        }
    }

    pub async fn closed(&self) {
        std::future::poll_fn(|cx| self.chan.poll_closed(cx)).await
    }

    pub fn is_closed(&self) -> bool {
        self.chan.is_closed()
    }

    pub fn same_channel(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.chan, &other.chan)
    }

    pub fn capacity(&self) -> usize {
        self.sem().available_permits()
    }

    pub fn max_capacity(&self) -> usize {
        self.chan.cap
    }

    pub fn downgrade(&self) -> WeakSender<T> {
        self.chan.weak_count.fetch_add(1, Ordering::AcqRel);
        WeakSender {
            chan: self.chan.clone(),
        }
    }

    pub fn strong_count(&self) -> usize {
        self.chan.tx_count.load(Ordering::Acquire)
    }

    pub fn weak_count(&self) -> usize {
        self.chan.weak_count.load(Ordering::Acquire)
    }
}

/// 予約した容量（`send` で使う。drop で返す）。
pub struct Permit<'a, T> {
    chan: &'a Arc<Chan<T>>,
}

impl<T> Permit<'_, T> {
    pub fn send(self, value: T) {
        if self.chan.push(value).is_err() {
            self.chan.release(1);
        }
        std::mem::forget(self);
    }
}

impl<T> Drop for Permit<'_, T> {
    fn drop(&mut self) {
        self.chan.release(1);
    }
}

/// `reserve_many` の予約。
pub struct PermitIterator<'a, T> {
    chan: &'a Arc<Chan<T>>,
    n: usize,
}

impl<T> ExactSizeIterator for PermitIterator<'_, T> {}

impl<'a, T> Iterator for PermitIterator<'a, T> {
    type Item = Permit<'a, T>;

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.n, Some(self.n))
    }

    fn next(&mut self) -> Option<Permit<'a, T>> {
        if self.n == 0 {
            return None;
        }
        self.n -= 1;
        Some(Permit { chan: self.chan })
    }
}

impl<T> Drop for PermitIterator<'_, T> {
    fn drop(&mut self) {
        if self.n > 0 {
            self.chan.release(self.n);
        }
    }
}

/// 所有の予約。
pub struct OwnedPermit<T> {
    tx: Option<Sender<T>>,
}

impl<T> OwnedPermit<T> {
    pub fn send(mut self, value: T) -> Sender<T> {
        let tx = self.tx.take().unwrap();
        if tx.chan.push(value).is_err() {
            tx.chan.release(1);
        }
        tx
    }

    pub fn release(mut self) -> Sender<T> {
        let tx = self.tx.take().unwrap();
        tx.chan.release(1);
        tx
    }
}

impl<T> Drop for OwnedPermit<T> {
    fn drop(&mut self) {
        if let Some(tx) = &self.tx {
            tx.chan.release(1);
        }
    }
}

impl<T> Receiver<T> {
    pub async fn recv(&mut self) -> Option<T> {
        std::future::poll_fn(|cx| self.chan.poll_recv(cx)).await
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.chan.poll_recv(cx)
    }

    /// 届いている分を `limit` 個まで `buffer` に足す（無ければ 1 つ届くまで待つ）。足した数（0 は終わり）。
    pub async fn recv_many(&mut self, buffer: &mut Vec<T>, limit: usize) -> usize {
        recv_many(&self.chan, buffer, limit).await
    }

    pub fn poll_recv_many(
        &mut self,
        cx: &mut Context<'_>,
        buffer: &mut Vec<T>,
        limit: usize,
    ) -> Poll<usize> {
        poll_recv_many(&self.chan, cx, buffer, limit)
    }

    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.chan.try_recv()
    }

    #[track_caller]
    pub fn blocking_recv(&mut self) -> Option<T> {
        if crate::runtime::has_driver() {
            panic!("Cannot block the current thread from within a runtime.");
        }
        futures::executor::block_on(self.recv())
    }

    #[track_caller]
    pub fn blocking_recv_many(&mut self, buffer: &mut Vec<T>, limit: usize) -> usize {
        if crate::runtime::has_driver() {
            panic!("Cannot block the current thread from within a runtime.");
        }
        futures::executor::block_on(self.recv_many(buffer, limit))
    }

    pub fn close(&mut self) {
        self.chan.close_rx();
    }

    pub fn is_closed(&self) -> bool {
        self.chan.is_closed() || self.chan.tx_count.load(Ordering::Acquire) == 0
    }

    pub fn is_empty(&self) -> bool {
        self.chan.len() == 0
    }

    pub fn len(&self) -> usize {
        self.chan.len()
    }

    pub fn capacity(&self) -> usize {
        self.chan
            .sem
            .as_ref()
            .map(|s| s.available_permits())
            .unwrap_or(usize::MAX)
    }

    pub fn max_capacity(&self) -> usize {
        self.chan.cap
    }

    pub fn sender_strong_count(&self) -> usize {
        self.chan.tx_count.load(Ordering::Acquire)
    }

    pub fn sender_weak_count(&self) -> usize {
        self.chan.weak_count.load(Ordering::Acquire)
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.chan.close_rx();
        // 残った値と Waker を捨てる（ロックの外で drop する）
        let (items, w) = {
            let mut q = self.chan.q.lock().unwrap();
            q.rx_dropped = true;
            let items: Vec<T> = q.items.drain(..).collect();
            (items, q.rx_waker.take())
        };
        drop(items);
        drop(w);
    }
}

impl<T> Unpin for Receiver<T> {}

fn poll_recv_many<T>(
    chan: &Chan<T>,
    cx: &mut Context<'_>,
    buffer: &mut Vec<T>,
    limit: usize,
) -> Poll<usize> {
    if limit == 0 {
        return Poll::Ready(0);
    }
    match chan.poll_recv(cx) {
        Poll::Pending => Poll::Pending,
        Poll::Ready(None) => Poll::Ready(0),
        Poll::Ready(Some(v)) => {
            buffer.push(v);
            let mut n = 1;
            while n < limit {
                match chan.try_recv() {
                    Ok(v) => {
                        buffer.push(v);
                        n += 1;
                    }
                    Err(_) => break,
                }
            }
            Poll::Ready(n)
        }
    }
}

async fn recv_many<T>(chan: &Chan<T>, buffer: &mut Vec<T>, limit: usize) -> usize {
    std::future::poll_fn(|cx| poll_recv_many(chan, cx, buffer, limit)).await
}

// ===================== 無界 =====================

/// 無界チャネル。
pub fn unbounded_channel<T>() -> (UnboundedSender<T>, UnboundedReceiver<T>) {
    let c = Chan::new(None);
    (
        UnboundedSender { chan: c.clone() },
        UnboundedReceiver { chan: c },
    )
}

/// 無界チャネルの送り手。
pub struct UnboundedSender<T> {
    chan: Arc<Chan<T>>,
}

/// 無界チャネルの弱い送り手。
pub struct WeakUnboundedSender<T> {
    chan: Arc<Chan<T>>,
}

/// 無界チャネルの受け手。
pub struct UnboundedReceiver<T> {
    chan: Arc<Chan<T>>,
}

impl<T> std::fmt::Debug for UnboundedSender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UnboundedSender")
    }
}

impl<T> std::fmt::Debug for WeakUnboundedSender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WeakUnboundedSender")
    }
}

impl<T> std::fmt::Debug for UnboundedReceiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UnboundedReceiver")
    }
}

impl<T> Clone for UnboundedSender<T> {
    fn clone(&self) -> Self {
        self.chan.tx_count.fetch_add(1, Ordering::AcqRel);
        UnboundedSender {
            chan: self.chan.clone(),
        }
    }
}

impl<T> Drop for UnboundedSender<T> {
    fn drop(&mut self) {
        self.chan.sender_dropped();
    }
}

impl<T> Clone for WeakUnboundedSender<T> {
    fn clone(&self) -> Self {
        self.chan.weak_count.fetch_add(1, Ordering::AcqRel);
        WeakUnboundedSender {
            chan: self.chan.clone(),
        }
    }
}

impl<T> Drop for WeakUnboundedSender<T> {
    fn drop(&mut self) {
        self.chan.weak_count.fetch_sub(1, Ordering::AcqRel);
    }
}

impl<T> WeakUnboundedSender<T> {
    pub fn upgrade(&self) -> Option<UnboundedSender<T>> {
        let mut n = self.chan.tx_count.load(Ordering::Acquire);
        loop {
            if n == 0 {
                return None;
            }
            match self.chan.tx_count.compare_exchange_weak(
                n,
                n + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(UnboundedSender {
                        chan: self.chan.clone(),
                    })
                }
                Err(cur) => n = cur,
            }
        }
    }

    pub fn strong_count(&self) -> usize {
        self.chan.tx_count.load(Ordering::Acquire)
    }

    pub fn weak_count(&self) -> usize {
        self.chan.weak_count.load(Ordering::Acquire)
    }
}

impl<T> UnboundedSender<T> {
    pub fn send(&self, message: T) -> Result<(), SendError<T>> {
        self.chan.push(message).map_err(SendError)
    }

    pub async fn closed(&self) {
        std::future::poll_fn(|cx| self.chan.poll_closed(cx)).await
    }

    pub fn is_closed(&self) -> bool {
        self.chan.is_closed()
    }

    pub fn same_channel(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.chan, &other.chan)
    }

    pub fn downgrade(&self) -> WeakUnboundedSender<T> {
        self.chan.weak_count.fetch_add(1, Ordering::AcqRel);
        WeakUnboundedSender {
            chan: self.chan.clone(),
        }
    }

    pub fn strong_count(&self) -> usize {
        self.chan.tx_count.load(Ordering::Acquire)
    }

    pub fn weak_count(&self) -> usize {
        self.chan.weak_count.load(Ordering::Acquire)
    }
}

impl<T> UnboundedReceiver<T> {
    pub async fn recv(&mut self) -> Option<T> {
        std::future::poll_fn(|cx| self.chan.poll_recv(cx)).await
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.chan.poll_recv(cx)
    }

    pub async fn recv_many(&mut self, buffer: &mut Vec<T>, limit: usize) -> usize {
        recv_many(&self.chan, buffer, limit).await
    }

    pub fn poll_recv_many(
        &mut self,
        cx: &mut Context<'_>,
        buffer: &mut Vec<T>,
        limit: usize,
    ) -> Poll<usize> {
        poll_recv_many(&self.chan, cx, buffer, limit)
    }

    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.chan.try_recv()
    }

    #[track_caller]
    pub fn blocking_recv(&mut self) -> Option<T> {
        if crate::runtime::has_driver() {
            panic!("Cannot block the current thread from within a runtime.");
        }
        futures::executor::block_on(self.recv())
    }

    pub fn close(&mut self) {
        self.chan.close_rx();
    }

    pub fn is_closed(&self) -> bool {
        self.chan.is_closed() || self.chan.tx_count.load(Ordering::Acquire) == 0
    }

    pub fn is_empty(&self) -> bool {
        self.chan.len() == 0
    }

    pub fn len(&self) -> usize {
        self.chan.len()
    }

    pub fn sender_strong_count(&self) -> usize {
        self.chan.tx_count.load(Ordering::Acquire)
    }

    pub fn sender_weak_count(&self) -> usize {
        self.chan.weak_count.load(Ordering::Acquire)
    }
}

impl<T> UnboundedReceiver<T> {
    #[track_caller]
    pub fn blocking_recv_many(&mut self, buffer: &mut Vec<T>, limit: usize) -> usize {
        if crate::runtime::has_driver() {
            panic!("Cannot block the current thread from within a runtime.");
        }
        futures::executor::block_on(self.recv_many(buffer, limit))
    }
}

impl<T> Drop for UnboundedReceiver<T> {
    fn drop(&mut self) {
        self.chan.close_rx();
        let (items, w) = {
            let mut q = self.chan.q.lock().unwrap();
            q.rx_dropped = true;
            let items: Vec<T> = q.items.drain(..).collect();
            (items, q.rx_waker.take())
        };
        drop(items);
        drop(w);
    }
}

impl<T> Unpin for UnboundedReceiver<T> {}

impl<T> std::fmt::Debug for Permit<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Permit")
    }
}

impl<T> std::fmt::Debug for PermitIterator<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermitIterator")
            .field("n", &self.n)
            .finish()
    }
}

impl<T> std::fmt::Debug for OwnedPermit<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OwnedPermit")
    }
}
