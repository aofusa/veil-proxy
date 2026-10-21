//! `tokio::sync::oneshot`。

use crate::tokio::waker::WakerSlot;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

struct State<T> {
    value: Option<T>,
    /// A value was sent (including the case where it has already been received).
    complete: bool,
    tx_dropped: bool,
    rx_closed: bool,
    rx_waker: WakerSlot,
    tx_waker: WakerSlot,
}

type Shared<T> = Arc<Mutex<State<T>>>;

/// The sending half.
pub struct Sender<T> {
    inner: Option<Shared<T>>,
}

/// The receiving half (`await` it to wait for the value).
pub struct Receiver<T> {
    inner: Option<Shared<T>>,
}

impl<T> std::fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("oneshot::Sender")
    }
}

impl<T> std::fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("oneshot::Receiver")
    }
}

pub mod error {
    /// The sender went away without sending a value.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct RecvError(pub(super) ());

    impl std::fmt::Display for RecvError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("channel closed")
        }
    }

    impl std::error::Error for RecvError {}

    /// The error of `try_recv`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum TryRecvError {
        Empty,
        Closed,
    }

    impl std::fmt::Display for TryRecvError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                TryRecvError::Empty => f.write_str("channel empty"),
                TryRecvError::Closed => f.write_str("channel closed"),
            }
        }
    }

    impl std::error::Error for TryRecvError {}
}

use error::{RecvError, TryRecvError};

/// A channel that can send a value only once.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let s = Arc::new(Mutex::new(State {
        value: None,
        complete: false,
        tx_dropped: false,
        rx_closed: false,
        rx_waker: WakerSlot::default(),
        tx_waker: WakerSlot::default(),
    }));
    (
        Sender {
            inner: Some(s.clone()),
        },
        Receiver { inner: Some(s) },
    )
}

impl<T> Sender<T> {
    /// Sends a value. Returns the value if the receiver has been closed.
    pub fn send(mut self, t: T) -> Result<(), T> {
        let inner = self.inner.take().unwrap();
        let w = {
            let mut st = inner.lock().unwrap();
            if st.rx_closed {
                return Err(t);
            }
            st.value = Some(t);
            st.complete = true;
            st.rx_waker.take()
        };
        if let Some(w) = w {
            w.wake();
        }
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.inner
            .as_ref()
            .is_none_or(|i| i.lock().unwrap().rx_closed)
    }

    /// Waits until the receiver is closed (dropped or `close`d).
    pub async fn closed(&mut self) {
        std::future::poll_fn(|cx| self.poll_closed(cx)).await
    }

    pub fn poll_closed(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let Some(inner) = &self.inner else {
            return Poll::Ready(());
        };
        let mut st = inner.lock().unwrap();
        if st.rx_closed {
            return Poll::Ready(());
        }
        st.tx_waker.register(cx.waker());
        Poll::Pending
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            let w = {
                let mut st = inner.lock().unwrap();
                st.tx_dropped = true;
                st.rx_waker.take()
            };
            if let Some(w) = w {
                w.wake();
            }
        }
    }
}

impl<T> Receiver<T> {
    /// Stops receiving (later `send`s fail; a value that was already sent can be taken with `try_recv`).
    pub fn close(&mut self) {
        if let Some(inner) = &self.inner {
            let w = {
                let mut st = inner.lock().unwrap();
                st.rx_closed = true;
                st.tx_waker.take()
            };
            if let Some(w) = w {
                w.wake();
            }
        }
    }

    pub fn is_terminated(&self) -> bool {
        self.inner.is_none()
    }

    pub fn is_empty(&self) -> bool {
        self.inner
            .as_ref()
            .is_none_or(|i| i.lock().unwrap().value.is_none())
    }

    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let Some(inner) = &self.inner else {
            return Err(TryRecvError::Closed);
        };
        let r = {
            let mut st = inner.lock().unwrap();
            match st.value.take() {
                Some(v) => Ok(v),
                None if st.tx_dropped || st.complete || st.rx_closed => Err(TryRecvError::Closed),
                None => Err(TryRecvError::Empty),
            }
        };
        if !matches!(r, Err(TryRecvError::Empty)) {
            self.inner = None;
        }
        r
    }

    #[track_caller]
    pub fn blocking_recv(self) -> Result<T, RecvError> {
        if crate::runtime::has_driver() {
            panic!("Cannot block the current thread from within a runtime.");
        }
        futures::executor::block_on(self)
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.close();
    }
}

impl<T> Future for Receiver<T> {
    type Output = Result<T, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let Some(inner) = &this.inner else {
            // 完了後（値・閉じたことを返した後）の poll は tokio と同じく panic
            panic!("called after complete");
        };
        let coop = std::task::ready!(crate::tokio::task::coop::poll_proceed(cx));
        let r = {
            let mut st = inner.lock().unwrap();
            match st.value.take() {
                Some(v) => Some(Ok(v)),
                None if st.tx_dropped || st.complete || st.rx_closed => Some(Err(RecvError(()))),
                None => {
                    st.rx_waker.register(cx.waker());
                    None
                }
            }
        };
        match r {
            Some(r) => {
                coop.made_progress();
                this.inner = None;
                Poll::Ready(r)
            }
            None => Poll::Pending,
        }
    }
}
