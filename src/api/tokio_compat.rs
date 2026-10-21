// F-190, F-198
//! Entry points for using tokio-based libraries from handlers.
//!
//! - `tokio-compat` (the default of `api`): no tokio. `run` drives the future directly on the handler's worker
//!   (the `veil::tokio` or replaced tokio APIs are usable inside). `spawn` is `veil::tokio::spawn`.
//! - `tokio-compat-bridge`: runs `Send` futures on a lazily started tokio multi-thread runtime (the number of
//!   workers is `[api] tokio_threads`, default 2) and returns the completion to the worker thread through the
//!   same result slot plus notification fd as veil's `offload` (no waker is called across threads).

/// A panic in a task of the compatibility runtime.
#[derive(Debug, Clone)]
pub struct TokioTaskPanicked;

impl std::fmt::Display for TokioTaskPanicked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("task on the tokio compatibility runtime panicked")
    }
}

impl std::error::Error for TokioTaskPanicked {}

#[cfg(feature = "tokio-compat")]
mod imp {
    use super::TokioTaskPanicked;
    use std::future::Future;

    /// Runs `fut` (on the handler's worker). A panic gives `Err`.
    pub async fn try_run<F>(fut: F) -> Result<F::Output, TokioTaskPanicked>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(fut))
            .await
            .map_err(|_| TokioTaskPanicked)
    }

    /// Like `try_run`, but propagates a panic to the caller.
    pub async fn run<F>(fut: F) -> F::Output
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        fut.await
    }

    /// Starts a fire-and-forget task (`veil::tokio::spawn`).
    pub fn spawn<F>(fut: F) -> crate::tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        crate::tokio::spawn(fut)
    }

    /// The handle of the current runtime (the handler's worker thread).
    pub fn handle() -> crate::tokio::runtime::Handle {
        crate::tokio::runtime::Handle::current()
    }
}

// 両方有効のときは lib.rs の compile_error だけを出す
#[cfg(all(feature = "tokio-compat-bridge", not(feature = "tokio-compat")))]
mod imp {
    use super::TokioTaskPanicked;
    use std::future::Future;
    use std::sync::OnceLock;

    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

    /// The tokio runtime for compatibility (started on first call).
    pub fn runtime() -> &'static tokio::runtime::Runtime {
        RUNTIME.get_or_init(|| {
            let n = crate::api::app::TOKIO_THREADS
                .load(std::sync::atomic::Ordering::Relaxed)
                .max(1);
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(n)
                .thread_name("veil-tokio")
                .enable_all()
                .build()
                .expect("failed to start the tokio compatibility runtime")
        })
    }

    /// The handle of the tokio runtime (for when you want to call the equivalent of `tokio::spawn` yourself).
    pub fn handle() -> tokio::runtime::Handle {
        runtime().handle().clone()
    }

    /// Runs `fut` on the tokio runtime and awaits the result without blocking veil's event loop.
    ///
    /// ```ignore
    /// let body = veil::api::tokio_compat::run(async move {
    ///     reqwest::get("https://example.com").await?.text().await
    /// }).await?;
    /// ```
    pub async fn try_run<F>(fut: F) -> Result<F::Output, TokioTaskPanicked>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let h = handle();
        crate::runtime::offload::await_completion(move |c| {
            h.spawn(async move {
                let v = fut.await;
                c.complete(v);
            });
        })
        .await
        .ok_or(TokioTaskPanicked)
    }

    /// Like `try_run`, but propagates a panic to the caller (a panic inside a handler becomes a 500).
    pub async fn run<F>(fut: F) -> F::Output
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        match try_run(fut).await {
            Ok(v) => v,
            Err(e) => panic!("{}", e),
        }
    }

    /// Starts a fire-and-forget task on the tokio runtime.
    pub fn spawn<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        runtime().spawn(fut)
    }
}

pub use imp::*;
