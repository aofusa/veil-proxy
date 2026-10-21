//! Helpers behind `tokio::future`, such as `maybe_done`, which `join!` and `try_join!` use.

pub mod maybe_done {
    //! A wrapper around a future that lets you take its output after it completes.

    use std::future::{Future, IntoFuture};
    use std::pin::Pin;
    use std::task::{ready, Context, Poll};

    /// Still running, completed and holding its output, or output already taken.
    #[derive(Debug)]
    #[repr(C)]
    pub enum MaybeDone<Fut: Future> {
        Future { future: Fut },
        Done { output: Fut::Output },
        Gone,
    }

    pub fn maybe_done<F: IntoFuture>(future: F) -> MaybeDone<F::IntoFuture> {
        MaybeDone::Future {
            future: future.into_future(),
        }
    }

    impl<Fut: Future> MaybeDone<Fut> {
        pub fn output_mut(self: Pin<&mut Self>) -> Option<&mut Fut::Output> {
            // SAFETY: 出力は動かしてよい（ピン留めされるのは `future` だけ）
            match unsafe { self.get_unchecked_mut() } {
                MaybeDone::Done { output } => Some(output),
                _ => None,
            }
        }

        #[inline]
        pub fn take_output(self: Pin<&mut Self>) -> Option<Fut::Output> {
            // SAFETY: Done のときだけ置き換える（ピン留めされた future は Done には無い）
            let this = unsafe { self.get_unchecked_mut() };
            match this {
                MaybeDone::Done { .. } => {}
                _ => return None,
            }
            match std::mem::replace(this, MaybeDone::Gone) {
                MaybeDone::Done { output } => Some(output),
                _ => unreachable!(),
            }
        }
    }

    impl<Fut: Future> Future for MaybeDone<Fut> {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            // SAFETY: `future` はその場で poll し、完了後にだけ置き換える（drop は置き換えで起きる）
            let this = unsafe { self.get_unchecked_mut() };
            let output = match this {
                MaybeDone::Future { future } => {
                    ready!(unsafe { Pin::new_unchecked(future) }.poll(cx))
                }
                MaybeDone::Done { .. } => return Poll::Ready(()),
                MaybeDone::Gone => panic!("MaybeDone polled after value taken"),
            };
            *this = MaybeDone::Done { output };
            Poll::Ready(())
        }
    }
}

pub use std::future::poll_fn;
