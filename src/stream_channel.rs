//! # 単一スレッド・アクターモデル用の共有プリミティブ（HTTP/2 / HTTP/3 共通）
//!
//! HTTP/2・HTTP/3 のデータプレーンは、プロトコルコネクション（quiche / `Http2Connection`）を
//! 単一スレッドの**メインループ**が専有駆動し、per-stream の I/O（バックエンド往復・ファイル
//! 読み・WASM 実行等）を独立タスクへ切り出す **thread-per-core アクターモデル**で構成される。
//! 本モジュールは、この 2 アクターを **ロック・アトミックなしの単一スレッド非同期チャネル +
//! Notify** で接続するための共通プリミティブを提供する（元は [`crate::http3_stream`] に存在し、
//! HTTP/2 多重化（F-116）で共有するため抽出した）。
//!
//! - [`Notify`]: タスク → メインループの起床通知。メインループはソケット I/O 待機と本 Notify を
//!   同時に待つ（`select`）。タスクがレスポンスチャネルへ push した／リクエストチャネルを消費した
//!   際に [`Notify::notify`] を呼ぶと、メインループが起きて各ストリームを再駆動する。
//! - [`channel`]: 容量 `cap`（アイテム数）の有界 SPSC チャネル。[`Rc`]`<`[`RefCell`]`>` ベースで
//!   **アトミック・ロックを一切使わない**（同一スレッド内の瞬間的 borrow のみ）。有界チャネルに
//!   より、クライアント遅延 → レスポンスチャネル満杯 → バックエンド read 停止、バックエンド遅延 →
//!   リクエストチャネル満杯 → 送信停止、という**バックプレッシャ**が双方向に自然伝播する。
//!   プロセスのヒープ保持は「並行ストリーム数 × 1 ストリームあたり有界バッファ」に収まり、
//!   **RSS は総ペイロードサイズに比例しない**。

#![cfg(any(feature = "http2", feature = "http3"))]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

// ============================================================================
// Notify — メインループを起こすための単一スレッド通知
// ============================================================================

struct NotifyInner {
    /// 通知済みフラグ（消費されるまで保持されるので取りこぼさない）。
    notified: bool,
    /// メインループ（待機側）の Waker。
    waker: Option<Waker>,
}

/// タスク → メインループの起床通知。
///
/// メインループはソケット I/O（HTTP/3 は UDP 受信、HTTP/2 は `wait_readable_fd`）を待ちつつ
/// 本 Notify も `select` する。タスクがレスポンスチャネルへ push した／リクエストチャネルを
/// 消費した際に [`notify`](Notify::notify) を呼ぶと、メインループが起きて各ストリームを再駆動する。
#[derive(Clone)]
pub struct Notify {
    inner: Rc<RefCell<NotifyInner>>,
}

impl Notify {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(NotifyInner {
                notified: false,
                waker: None,
            })),
        }
    }

    /// メインループを起こす。待機していなければフラグだけ立て、次回 `wait()` で即 Ready になる。
    pub fn notify(&self) {
        let waker = {
            let mut g = self.inner.borrow_mut();
            g.notified = true;
            g.waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
    }

    /// 通知が来るまで（または既に来ていれば即座に）完了する Future を返す。
    ///
    /// 完了時にフラグを消費する。`select` の負け arm として drop されても副作用はない。
    pub async fn wait(&self) {
        poll_fn(|cx: &mut Context<'_>| {
            let mut g = self.inner.borrow_mut();
            if g.notified {
                g.notified = false;
                Poll::Ready(())
            } else {
                g.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }
}

impl Default for Notify {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// 単一スレッド SPSC 非同期チャネル（ロック・アトミックなし）
// ============================================================================

struct Shared<T> {
    queue: VecDeque<T>,
    cap: usize,
    sender_closed: bool,
    receiver_closed: bool,
    recv_waker: Option<Waker>,
    send_waker: Option<Waker>,
}

/// 送信端。
pub struct Sender<T> {
    sh: Rc<RefCell<Shared<T>>>,
}

/// 受信端。
pub struct Receiver<T> {
    sh: Rc<RefCell<Shared<T>>>,
}

/// [`Sender::try_send`] の失敗種別。
pub enum TrySendError<T> {
    /// 容量上限（バックプレッシャ）。アイテムは返却される。
    Full(T),
    /// 受信端が drop 済み。アイテムは返却される。
    Closed(#[allow(dead_code)] T),
}

/// [`Receiver::try_recv`] の結果。
pub enum TryRecv<T> {
    /// アイテムを取り出した。
    Item(T),
    /// 今は空（送信端は生存）。
    Empty,
    /// 送信端が閉じ、キューも空（= EOF）。
    Closed,
}

/// 容量 `cap`（アイテム数）の有界チャネルを作る。
pub fn channel<T>(cap: usize) -> (Sender<T>, Receiver<T>) {
    let sh = Rc::new(RefCell::new(Shared {
        queue: VecDeque::new(),
        cap,
        sender_closed: false,
        receiver_closed: false,
        recv_waker: None,
        send_waker: None,
    }));
    (Sender { sh: sh.clone() }, Receiver { sh })
}

impl<T> Sender<T> {
    /// 非ブロッキング送信。容量満杯なら [`TrySendError::Full`]。
    pub fn try_send(&self, item: T) -> Result<(), TrySendError<T>> {
        let waker = {
            let mut s = self.sh.borrow_mut();
            if s.receiver_closed {
                return Err(TrySendError::Closed(item));
            }
            if s.queue.len() >= s.cap {
                return Err(TrySendError::Full(item));
            }
            s.queue.push_back(item);
            s.recv_waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
        Ok(())
    }

    /// キューが容量上限に達しているか（バックプレッシャ判定）。
    pub fn is_full(&self) -> bool {
        let s = self.sh.borrow();
        s.queue.len() >= s.cap
    }

    /// 容量が空くまで待ってから送信する。受信端が閉じていれば `Err(())`。
    pub async fn send(&self, item: T) -> Result<(), ()> {
        let mut item = Some(item);
        poll_fn(|cx: &mut Context<'_>| {
            let waker = {
                let mut s = self.sh.borrow_mut();
                if s.receiver_closed {
                    return Poll::Ready(Err(()));
                }
                if s.queue.len() < s.cap {
                    s.queue
                        .push_back(item.take().expect("send polled after completion"));
                    s.recv_waker.take()
                } else {
                    s.send_waker = Some(cx.waker().clone());
                    return Poll::Pending;
                }
            };
            if let Some(w) = waker {
                w.wake();
            }
            Poll::Ready(Ok(()))
        })
        .await
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let waker = {
            let mut s = self.sh.borrow_mut();
            s.sender_closed = true;
            s.recv_waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
    }
}

impl<T> Receiver<T> {
    /// 非ブロッキング受信。
    pub fn try_recv(&self) -> TryRecv<T> {
        let (item, waker) = {
            let mut s = self.sh.borrow_mut();
            match s.queue.pop_front() {
                Some(x) => (Some(x), s.send_waker.take()),
                None => {
                    if s.sender_closed {
                        return TryRecv::Closed;
                    }
                    return TryRecv::Empty;
                }
            }
        };
        if let Some(w) = waker {
            w.wake();
        }
        // item は Some 確定。
        TryRecv::Item(item.unwrap())
    }

    /// アイテムが来るまで待つ。送信端が閉じてキューも空なら `None`。
    pub async fn recv(&self) -> Option<T> {
        poll_fn(|cx: &mut Context<'_>| {
            let (item, waker) = {
                let mut s = self.sh.borrow_mut();
                match s.queue.pop_front() {
                    Some(x) => (x, s.send_waker.take()),
                    None => {
                        if s.sender_closed {
                            return Poll::Ready(None);
                        }
                        s.recv_waker = Some(cx.waker().clone());
                        return Poll::Pending;
                    }
                }
            };
            if let Some(w) = waker {
                w.wake();
            }
            Poll::Ready(Some(item))
        })
        .await
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let waker = {
            let mut s = self.sh.borrow_mut();
            s.receiver_closed = true;
            s.send_waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同一スレッドで Future を 1 つ実行する最小ランタイム（テスト用）。
    fn block_on<F: std::future::Future>(mut fut: F) -> F::Output {
        use std::sync::Arc;
        use std::task::{Context, Poll, Wake, Waker};
        struct NoopWake;
        impl Wake for NoopWake {
            fn wake(self: Arc<Self>) {}
        }
        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        let mut fut = unsafe { std::pin::Pin::new_unchecked(&mut fut) };
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => {} // テストでは Pending を即リトライ（busy）しない設計のもののみ使用。
            }
        }
    }

    #[test]
    fn channel_try_send_recv_roundtrip() {
        let (tx, rx) = channel::<u32>(2);
        assert!(tx.try_send(1).is_ok());
        assert!(tx.try_send(2).is_ok());
        // 容量超過。
        assert!(matches!(tx.try_send(3), Err(TrySendError::Full(3))));
        assert!(matches!(rx.try_recv(), TryRecv::Item(1)));
        assert!(matches!(rx.try_recv(), TryRecv::Item(2)));
        assert!(matches!(rx.try_recv(), TryRecv::Empty));
        // 容量が空いたので送れる。
        assert!(tx.try_send(3).is_ok());
        assert!(matches!(rx.try_recv(), TryRecv::Item(3)));
    }

    #[test]
    fn channel_sender_drop_signals_eof() {
        let (tx, rx) = channel::<u32>(4);
        assert!(tx.try_send(7).is_ok());
        drop(tx);
        assert!(matches!(rx.try_recv(), TryRecv::Item(7)));
        // 送信端 drop 後、キュー空 → Closed（EOF）。
        assert!(matches!(rx.try_recv(), TryRecv::Closed));
    }

    #[test]
    fn channel_receiver_drop_blocks_send() {
        let (tx, rx) = channel::<u32>(1);
        drop(rx);
        assert!(matches!(tx.try_send(1), Err(TrySendError::Closed(1))));
    }

    #[test]
    fn channel_async_send_recv() {
        // 容量 1。send→recv→send→recv を単一スレッドで交互に進める。
        let (tx, rx) = channel::<u32>(1);
        block_on(async {
            tx.send(10).await.unwrap();
            assert_eq!(rx.recv().await, Some(10));
            tx.send(20).await.unwrap();
            assert_eq!(rx.recv().await, Some(20));
            drop(tx);
            assert_eq!(rx.recv().await, None);
        });
    }

    #[test]
    fn notify_sets_and_consumes_flag() {
        let n = Notify::new();
        n.notify();
        // 既に通知済みなら wait は即完了。
        block_on(n.wait());
        // 消費済み: 再 notify するまで Pending（busy ループ回避のためここでは検証のみ）。
        assert!(!n.inner.borrow().notified);
    }
}
