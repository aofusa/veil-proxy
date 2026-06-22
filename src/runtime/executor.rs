//! シングルスレッド非同期エグゼキュータ
//!
//! io_uring の CQE を処理して Future を wake するエグゼキュータ。
//! 各ワーカースレッドが独自のリング + エグゼキュータを持つ thread-per-core 設計。
//!
//! ## 設計
//!
//! - `user_data` (u64) で各 Future を識別する
//! - CQE を受け取ると対応する Waker を呼ぶ
//! - Future は `OpState` として登録され、完了時に結果を格納する

#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use crate::runtime::ring::{
    IoUring, IoUringCqe, IORING_OP_ACCEPT, IORING_OP_ASYNC_CANCEL,
    IORING_OP_CLOSE, IORING_OP_CONNECT, IORING_OP_NOP, IORING_OP_POLL_ADD, IORING_OP_POLL_REMOVE,
    IORING_OP_RECV, IORING_OP_SEND, IORING_OP_SPLICE, IORING_OP_TIMEOUT,
};

// ====================
// グローバル user_data カウンタ
// ====================

/// ユニークな user_data を生成するアトミックカウンタ
static USER_DATA_COUNTER: AtomicU64 = AtomicU64::new(1);

/// 新しいユニーク user_data を取得
pub fn next_user_data() -> u64 {
    USER_DATA_COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ====================
// 許可オペコードリスト
// ====================

/// リバースプロキシが使用する io_uring オペコード一覧
pub const PROXY_ALLOWED_OPCODES: &[u8] = &[
    IORING_OP_NOP,
    IORING_OP_POLL_ADD,
    IORING_OP_POLL_REMOVE,
    IORING_OP_TIMEOUT,
    IORING_OP_ACCEPT,
    IORING_OP_ASYNC_CANCEL,
    IORING_OP_CONNECT,
    IORING_OP_RECV,
    IORING_OP_SEND,
    IORING_OP_CLOSE,
    IORING_OP_SPLICE,
];

// ====================
// Op 完了状態
// ====================

/// io_uring 操作の完了状態
#[derive(Debug)]
pub enum OpResult {
    /// 完了していない
    Pending,
    /// 完了（res は CQE.res）
    Done(i32),
}

/// スレッドローカルな操作テーブル
struct OpTable {
    /// user_data -> (OpResult, Waker)
    ops: HashMap<u64, (OpResult, Option<Waker>)>,
}

impl OpTable {
    fn new() -> Self {
        Self {
            ops: HashMap::new(),
        }
    }

    /// 操作を登録する
    fn register(&mut self, user_data: u64) {
        self.ops.insert(user_data, (OpResult::Pending, None));
    }

    /// Waker を設定する
    fn set_waker(&mut self, user_data: u64, waker: Waker) {
        if let Some(entry) = self.ops.get_mut(&user_data) {
            entry.1 = Some(waker);
        }
    }

    /// CQE を処理して対応する Waker を wake する
    fn on_cqe(&mut self, cqe: &IoUringCqe) -> bool {
        if let Some(entry) = self.ops.get_mut(&cqe.user_data) {
            entry.0 = OpResult::Done(cqe.res);
            if let Some(waker) = entry.1.take() {
                waker.wake();
            }
            true
        } else {
            false
        }
    }

    /// 操作の結果を取得し、エントリを削除する
    fn take_result(&mut self, user_data: u64) -> Option<i32> {
        if let Some(entry) = self.ops.get(&user_data) {
            if let OpResult::Done(res) = entry.0 {
                self.ops.remove(&user_data);
                return Some(res);
            }
        }
        None
    }

    /// 操作の結果を取得する（エントリを削除しない）
    fn peek_result(&self, user_data: u64) -> Option<i32> {
        if let Some(entry) = self.ops.get(&user_data) {
            if let OpResult::Done(res) = entry.0 {
                return Some(res);
            }
        }
        None
    }

    /// 操作を削除する
    fn remove(&mut self, user_data: u64) {
        self.ops.remove(&user_data);
    }
}

// ====================
// スレッドローカルな実行コンテキスト
// ====================

thread_local! {
    /// スレッドローカルな io_uring リング
    pub(crate) static RING: RefCell<Option<IoUring>> = const { RefCell::new(None) };

    /// スレッドローカルな操作テーブル
    pub(crate) static OP_TABLE: RefCell<OpTable> = RefCell::new(OpTable::new());
}

/// スレッドローカルな io_uring リングを初期化する
pub fn init_ring(entries: u32, flags: u32) -> std::io::Result<()> {
    let ring = IoUring::new(entries, flags)?;

    // IORING_REGISTER_RESTRICTIONS でオペコードを制限
    // 制限適用に失敗しても動作は継続（カーネルバージョン依存）
    if let Err(e) = ring.apply_restrictions(PROXY_ALLOWED_OPCODES) {
        ftlog::debug!(
            "io_uring restrictions not applied (kernel may not support): {}",
            e
        );
    }

    RING.with(|r| {
        *r.borrow_mut() = Some(ring);
    });
    Ok(())
}

/// スレッドローカルな io_uring リングを取得してクロージャを実行
pub fn with_ring<F, R>(f: F) -> R
where
    F: FnOnce(&mut IoUring) -> R,
{
    RING.with(|r| {
        let mut borrow = r.borrow_mut();
        let ring = borrow
            .as_mut()
            .expect("io_uring ring not initialized for this thread");
        f(ring)
    })
}

/// 操作を登録する
pub fn register_op(user_data: u64) {
    OP_TABLE.with(|t| {
        t.borrow_mut().register(user_data);
    });
}

/// 操作の Waker を設定する
pub fn set_op_waker(user_data: u64, waker: Waker) {
    OP_TABLE.with(|t| {
        t.borrow_mut().set_waker(user_data, waker);
    });
}

/// CQE を処理して対応する Future を wake する
pub fn process_cqe(cqe: &IoUringCqe) {
    OP_TABLE.with(|t| {
        t.borrow_mut().on_cqe(cqe);
    });
}

/// 操作の結果を取得して削除する
pub fn take_op_result(user_data: u64) -> Option<i32> {
    OP_TABLE.with(|t| t.borrow_mut().take_result(user_data))
}

/// 操作の結果を参照する（削除しない）
pub fn peek_op_result(user_data: u64) -> Option<i32> {
    OP_TABLE.with(|t| t.borrow().peek_result(user_data))
}

/// 操作を削除する
pub fn remove_op(user_data: u64) {
    OP_TABLE.with(|t| t.borrow_mut().remove(user_data));
}

// ====================
// io_uring イベントループ
// ====================

/// io_uring の CQE を処理する（ノンブロッキング）
///
/// 利用可能な CQE をすべて処理して対応する Future を wake する。
pub fn poll_completions() {
    with_ring(|ring| {
        ring.consume_cqes(|cqe| {
            process_cqe(cqe);
        });
    });
}

/// io_uring の CQE を処理する（最低 1 件完了まで待機）
pub fn wait_for_completions() -> std::io::Result<()> {
    with_ring(|ring| ring.submit_and_wait(1))?;

    poll_completions();
    Ok(())
}

/// SQE を提出する
pub fn submit_sqes() -> std::io::Result<()> {
    with_ring(|ring| ring.submit())?;
    Ok(())
}

// ====================
// シングルスレッドエグゼキュータ
// ====================

/// タスクキュー（Ready な Future のリスト）
struct TaskQueue {
    ready: std::collections::VecDeque<Arc<Task>>,
}

impl TaskQueue {
    fn new() -> Self {
        Self {
            ready: std::collections::VecDeque::new(),
        }
    }
}

/// 実行タスク
struct Task {
    /// Future 本体（Mutex で Sync を付与）
    future: Mutex<Pin<Box<dyn Future<Output = ()> + 'static>>>,
    /// タスクキューへの参照（wake 時に自分自身を再キューイング）
    queue: Arc<Mutex<TaskQueue>>,
}

/// タスク用 Waker の vtable
static TASK_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    task_waker_clone,
    task_waker_wake,
    task_waker_wake_by_ref,
    task_waker_drop,
);

unsafe fn task_waker_clone(data: *const ()) -> RawWaker {
    let arc = Arc::from_raw(data as *const Task);
    let cloned = arc.clone();
    std::mem::forget(arc);
    RawWaker::new(Arc::into_raw(cloned) as *const (), &TASK_WAKER_VTABLE)
}

unsafe fn task_waker_wake(data: *const ()) {
    let arc = Arc::from_raw(data as *const Task);
    arc.schedule();
    // arc はここでドロップ（Arc::into_raw の逆）
}

unsafe fn task_waker_wake_by_ref(data: *const ()) {
    let arc = Arc::from_raw(data as *const Task);
    arc.schedule();
    std::mem::forget(arc); // 参照カウントを減らさない
}

unsafe fn task_waker_drop(data: *const ()) {
    drop(Arc::from_raw(data as *const Task));
}

impl Task {
    /// タスクをキューに追加する
    fn schedule(self: &Arc<Self>) {
        if let Ok(mut q) = self.queue.lock() {
            q.ready.push_back(self.clone());
        }
    }

    /// タスクを1回ポーリングする
    fn poll_once(self: &Arc<Self>) -> Poll<()> {
        let raw = Arc::as_ptr(self) as *const ();
        let raw_waker = RawWaker::new(raw, &TASK_WAKER_VTABLE);
        // SAFETY: vtable は有効な関数ポインタを持つ
        let waker = unsafe { Waker::from_raw(raw_waker) };
        // Arc の参照カウントを1増やす（Waker が保持）
        std::mem::forget(self.clone());

        let mut cx = Context::from_waker(&waker);
        let mut future = self.future.lock().unwrap();
        future.as_mut().poll(&mut cx)
    }
}

/// シングルスレッドエグゼキュータ
pub struct Executor {
    queue: Arc<Mutex<TaskQueue>>,
}

impl Executor {
    /// 新しいエグゼキュータを作成する
    pub fn new() -> Self {
        Self {
            queue: Arc::new(Mutex::new(TaskQueue::new())),
        }
    }

    /// Future をスポーンする
    pub fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + 'static,
    {
        let task = Arc::new(Task {
            future: Mutex::new(Box::pin(future)),
            queue: self.queue.clone(),
        });
        if let Ok(mut q) = self.queue.lock() {
            q.ready.push_back(task);
        }
    }

    /// Ready なタスクをすべてポーリングする
    fn run_ready_tasks(&self) {
        loop {
            let task = {
                let mut q = self.queue.lock().unwrap();
                q.ready.pop_front()
            };

            match task {
                Some(t) => {
                    t.poll_once();
                }
                None => break,
            }
        }
    }

    /// メインの実行ループ
    ///
    /// 与えられた Future が完了するまでイベントループを回す。
    pub fn block_on<F, R>(&self, future: F) -> R
    where
        F: Future<Output = R> + 'static,
        R: 'static,
    {
        use std::sync::atomic::AtomicBool;

        let done = Arc::new(AtomicBool::new(false));
        let result: Arc<Mutex<Option<R>>> = Arc::new(Mutex::new(None));

        let done_clone = done.clone();
        let result_clone = result.clone();

        self.spawn(async move {
            let r = future.await;
            *result_clone.lock().unwrap() = Some(r);
            done_clone.store(true, Ordering::Release);
        });

        loop {
            // Ready なタスクを実行
            self.run_ready_tasks();

            if done.load(Ordering::Acquire) {
                break;
            }

            // io_uring の完了を待つ
            if let Err(e) = wait_for_completions() {
                // EINTR は無視
                if e.kind() != std::io::ErrorKind::Interrupted {
                    ftlog::error!("io_uring wait error: {}", e);
                    break;
                }
            }
        }

        let value = result
            .lock()
            .unwrap()
            .take()
            .expect("future completed but no result");
        value
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::new()
    }
}

// ====================
// グローバルエグゼキュータ（スレッドローカル）
// ====================

thread_local! {
    static EXECUTOR: RefCell<Option<Executor>> = const { RefCell::new(None) };
}

/// スレッドローカルなエグゼキュータを初期化する
pub fn init_executor() {
    EXECUTOR.with(|e| {
        *e.borrow_mut() = Some(Executor::new());
    });
}

/// Future をスポーンする（現在のスレッドのエグゼキュータに）
pub fn spawn<F>(future: F)
where
    F: Future<Output = ()> + 'static,
{
    EXECUTOR.with(|e| {
        let borrow = e.borrow();
        let exec = borrow
            .as_ref()
            .expect("executor not initialized for this thread");
        exec.spawn(future);
    });
}
