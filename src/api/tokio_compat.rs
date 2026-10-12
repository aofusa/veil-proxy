//! tokio インターフェース互換レイヤー（F-190、`tokio-interface` feature）。
//!
//! データプレーン（受付・プロトコル処理・ハンドラの実行）は veil の独自ランタイムのまま、tokio 前提の
//! ライブラリ（sqlx・reqwest・tower-http の Timeout 等）をハンドラから使うための橋渡し。
//! 遅延起動する tokio マルチスレッドランタイム（ワーカー数は `[api] tokio_threads`、既定 2）で
//! `Send` な Future を実行し、完了を veil の `offload` と同じ「結果スロット + 通知 fd」で
//! ワーカースレッドへ返す（スレッド越しに Waker を呼ばない）。

use std::future::Future;
use std::sync::OnceLock;

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// 互換用の tokio ランタイム（初回呼び出しで起動）。
pub fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        let n = super::app::TOKIO_THREADS
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

/// tokio ランタイムのハンドル（`tokio::spawn` 相当を自分で呼びたい場合）。
pub fn handle() -> tokio::runtime::Handle {
    runtime().handle().clone()
}

/// tokio のタスクの panic。
#[derive(Debug, Clone)]
pub struct TokioTaskPanicked;

impl std::fmt::Display for TokioTaskPanicked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("task on the tokio compatibility runtime panicked")
    }
}

impl std::error::Error for TokioTaskPanicked {}

/// `fut` を tokio ランタイムで実行し、結果を（veil のイベントループをブロックせずに）待つ。
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

/// `try_run` の panic を呼び出し側へ伝える版（ハンドラ内の panic は 500 になる）。
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

/// tokio ランタイムで投げっぱなしのタスクを起動する。
pub fn spawn<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    runtime().spawn(fut)
}
