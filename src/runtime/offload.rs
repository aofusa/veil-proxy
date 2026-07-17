//! ブロッキング FS 操作の非同期オフロード（F-29）
//!
//! `canonicalize`（シンボリックリンク解決を含むパス正規化）は io_uring に対応オペコードが
//! 無く（`readlink` 系が存在しない）、純 io_uring では非同期化できない。また thread-per-core
//! エグゼキュータのタスクは `Send` でないため、ワーカースレッドからタスク Waker を直接
//! 呼ぶのは不健全である。
//!
//! そこで「ブロッキング処理を専用スレッドプールにオフロードし、完了をスレッドごとの
//! `eventfd`（`POLL_ADD` で待機）でイベントループへ通知する」非同期オフロード機構を
//! **一から自作** する。これにより、キャッシュミス時の `canonicalize`/`metadata` を
//! **イベントループをブロックせず** に実行できる。新規 io_uring オペコードは追加しない
//! （`POLL_ADD` は許可済み）ため、セキュリティサーフェスは変わらない。
//!
//! ## 健全性
//!
//! - 待機は起点スレッド自身の `wait_readable_fd(eventfd)`（POLL_ADD）で行う＝同一スレッドで
//!   完結し Waker のクロススレッド呼び出しは発生しない。
//! - ワーカースレッドは結果スロットに値を格納し `done` を `Release` で立て、起点スレッドの
//!   `eventfd` を `write(2)` で叩く（fd への write はスレッド安全）。完了ごとに必ず
//!   eventfd を叩くため取りこぼしは無い。
//! - ランタイムドライバ未初期化（単体テスト等）では同期インライン実行にフォールバックする。

use std::cell::Cell;
use std::collections::VecDeque;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use once_cell::sync::Lazy;

use crate::runtime::has_driver;
#[cfg(veil_rt_uring)]
use crate::runtime::tcp::wait_readable_fd;

type Job = Box<dyn FnOnce() + Send + 'static>;

/// ブロッキング FS 用スレッドプール（MPMC: Mutex<VecDeque> + Condvar）。
struct OffloadPool {
    queue: Mutex<VecDeque<Job>>,
    cv: Condvar,
}

impl OffloadPool {
    fn submit(&self, job: Job) {
        {
            let mut q = self.queue.lock().unwrap();
            q.push_back(job);
        }
        self.cv.notify_one();
    }
}

static POOL: Lazy<Arc<OffloadPool>> = Lazy::new(|| {
    let pool = Arc::new(OffloadPool {
        queue: Mutex::new(VecDeque::new()),
        cv: Condvar::new(),
    });
    // FS の syscall は短時間なので少数のワーカーで十分。最低 2、最大 8。
    let workers = num_cpus::get().clamp(2, 8);
    for _ in 0..workers {
        let pool = Arc::clone(&pool);
        std::thread::Builder::new()
            .name("veil-fs-offload".to_string())
            .spawn(move || loop {
                let job = {
                    let mut q = pool.queue.lock().unwrap();
                    while q.is_empty() {
                        q = pool.cv.wait(q).unwrap();
                    }
                    q.pop_front()
                };
                if let Some(job) = job {
                    job();
                }
            })
            .expect("failed to spawn veil-fs-offload thread");
    }
    pool
});

thread_local! {
    /// 起点スレッド（io_uring ワーカー）の完了通知用 eventfd。初回オフロード時に遅延生成。
    static OFFLOAD_EVENTFD: Cell<RawFd> = const { Cell::new(-1) };
}

/// 現在のスレッドのオフロード用 eventfd を取得する（ランタイムドライバがある場合のみ）。
fn current_thread_eventfd() -> Option<RawFd> {
    if !has_driver() {
        return None;
    }
    OFFLOAD_EVENTFD.with(|cell| {
        let fd = cell.get();
        if fd >= 0 {
            return Some(fd);
        }
        // EFD_NONBLOCK: ドレイン read が即時に返る。EFD_CLOEXEC: exec で漏らさない。
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        cell.set(fd);
        Some(fd)
    })
}

/// ブロッキングなクロージャ `f` を専用スレッドで実行し、結果を非同期に受け取る。
///
/// ドライバのあるワーカースレッドでは eventfd で待機して**イベントループをブロックしない**。
/// ドライバが無い（単体テスト等）場合は同期インライン実行にフォールバックする。
pub async fn offload<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let eventfd = match current_thread_eventfd() {
        Some(fd) => fd,
        // ドライバ無し: 同期実行（このパスは単体テスト等のみ）
        None => return f(),
    };

    let slot: Arc<Mutex<Option<R>>> = Arc::new(Mutex::new(None));
    let done = Arc::new(AtomicBool::new(false));
    let slot_w = Arc::clone(&slot);
    let done_w = Arc::clone(&done);

    POOL.submit(Box::new(move || {
        let result = f();
        *slot_w.lock().unwrap() = Some(result);
        // 結果格納を Release で公開してから eventfd を叩く（起点スレッドは Acquire で読む）。
        done_w.store(true, Ordering::Release);
        let val: u64 = 1;
        // SAFETY: eventfd は 8 バイト書き込み。fd は起点スレッドが保持し続けている。
        unsafe {
            libc::write(eventfd, &val as *const u64 as *const libc::c_void, 8);
        }
    }));

    // ===== io_uring バックエンド: POLL_ADD 待機 =====
    //
    // POLL_ADD の完了は eventfd への write 時点で CQE として **各待機者に個別に** 記録される
    // （後から他タスクがカウンタを drain しても、既に発行された完了は失われない）。
    // そのため「wait → drain → done 再確認」の素朴なループで取りこぼしが無い。
    #[cfg(veil_rt_uring)]
    loop {
        if done.load(Ordering::Acquire) {
            return slot
                .lock()
                .unwrap()
                .take()
                .expect("offload result must be present once done");
        }
        // eventfd が読み取り可能（=どれかのオフロードが完了）になるまで待つ。
        let _ = wait_readable_fd(eventfd).await;
        // カウンタをドレイン（複数オフロードで共有するため best-effort、EAGAIN は無視）。
        let mut buf = [0u8; 8];
        unsafe {
            libc::read(eventfd, buf.as_mut_ptr() as *mut libc::c_void, 8);
        }
    }

    // ===== reactor（readiness）バックエンド: done 検査つき専用 Future で待機 =====
    //
    // eventfd カウンタは全 completions を合算した 1 本のレベル信号のため、あるタスクの
    // drain が **他の待機タスク宛ての通知シグナルごと消費** し得る。消費されたシグナルの
    // 持ち主は「readable 待ち」のままでは二度と起床できない（EPOLLONESHOT のイベントは
    // epoll_wait 時点のレベル再評価で蒸発する）。そのため:
    //
    // 1. 待機 Future の poll で **毎回 `done` を最初に検査**する（どんな起床でも、
    //    readiness の有無に関わらず完了を観測できる）。
    // 2. drain した者は同じ eventfd の残り待機者を全員起こす
    //    （`reactor::executor::wake_all_readers`）。起こされた各タスクは (1) により
    //    自身の done を再確認し、未完了なら再登録して待機に戻る。
    //
    // この 2 つの組で「シグナル横取り + 起床消失」を構造的に排除する
    // （F-120 Phase 2 の E2E `test_f62_wasm_http_call_concurrent_requests` で発見）。
    #[cfg(veil_rt_reactor)]
    {
        struct OffloadWait {
            eventfd: RawFd,
            done: Arc<AtomicBool>,
        }

        impl std::future::Future for OffloadWait {
            type Output = ();

            fn poll(
                self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<()> {
                // (1) どんな起床経路でも、まず完了フラグを観測する。
                if self.done.load(Ordering::Acquire) {
                    return std::task::Poll::Ready(());
                }
                // try-first: 既に readable なら drain して他の待機者へ再配布する。
                let mut pfd = libc::pollfd {
                    fd: self.eventfd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
                if ret > 0 && pfd.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
                    let mut buf = [0u8; 8];
                    let rret = unsafe {
                        libc::read(self.eventfd, buf.as_mut_ptr() as *mut libc::c_void, 8)
                    };
                    // (2) drain したシグナルは他タスク宛てかもしれない。全読者を起こして
                    // 各自の done を再確認させる（起こされた側は本 poll の (1) で観測する）。
                    if rret > 0 {
                        crate::runtime::executor::wake_all_readers(self.eventfd);
                    }
                    // 呼び出し元ループで done を再確認させる（自分宛てだったかは分からない）。
                    return std::task::Poll::Ready(());
                }
                crate::runtime::executor::register_read(self.eventfd, cx.waker().clone());
                std::task::Poll::Pending
            }
        }

        loop {
            if done.load(Ordering::Acquire) {
                return slot
                    .lock()
                    .unwrap()
                    .take()
                    .expect("offload result must be present once done");
            }
            OffloadWait {
                eventfd,
                done: Arc::clone(&done),
            }
            .await;
        }
    }
}
