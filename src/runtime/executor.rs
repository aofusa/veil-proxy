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

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use crate::runtime::ring::{
    IoUring, IoUringCqe, IORING_OP_ACCEPT, IORING_OP_ASYNC_CANCEL, IORING_OP_CLOSE,
    IORING_OP_CONNECT, IORING_OP_NOP, IORING_OP_POLL_ADD, IORING_OP_POLL_REMOVE, IORING_OP_RECV,
    IORING_OP_SEND, IORING_OP_SPLICE, IORING_OP_TIMEOUT, IORING_SETUP_R_DISABLED,
};

// ====================
// スレッドローカル user_data カウンタ
// ====================

thread_local! {
    /// スレッドローカルな user_data カウンタ。
    ///
    /// user_data は **同一ワーカースレッド内** でのみ意味を持つ（リング `RING` と操作テーブル
    /// `OP_TABLE` はともにスレッドローカルで、SQE を提出したスレッドへ CQE が戻り同スレッドで
    /// 処理される。`offload.rs` の完了通知も eventfd 経由で起点スレッドへ戻る）。よって一意性は
    /// スレッド内で足りる。旧実装のグローバル `AtomicU64` は **全ワーカーコアが毎 io_uring op で
    /// 同一キャッシュラインを奪い合う**（thread-per-core のスケールを阻害する偽共有）。スレッド
    /// ローカル化して **毎 op のアトミック競合（同期コスト）を排除** する。別スレッドが同じ値を
    /// 持っても別テーブル・別リングのため衝突しない。1 から単調増加し、`CANCEL_SENTINEL_USER_DATA`
    /// (= u64::MAX) には実時間で到達しない。
    static USER_DATA_COUNTER: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}

/// 新しい（スレッド内で）ユニークな user_data を取得する。アトミック不要・ロックフリー。
#[inline]
pub fn next_user_data() -> u64 {
    USER_DATA_COUNTER.with(|c| {
        let v = c.get();
        c.set(v.wrapping_add(1));
        v
    })
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

/// ドロップされた in-flight op の後始末ガード。
///
/// io_uring に提出済みで未完了の op を持つ Future がドロップされた場合、カーネルはまだ
/// バッファ（accept の addr、read/write のデータ領域）を参照し続けている可能性があるため、
/// 即座に解放すると use-after-free になる。ガードはそれら所有リソースを保持し、op の完了
/// またはキャンセルの CQE 到着時に呼ばれて後始末（バッファ解放、accept で得た fd の
/// クローズ等）を行う。引数は CQE.res。
pub type OpGuard = Box<dyn FnOnce(i32)>;

/// ASYNC_CANCEL op 自身の user_data に使うセンチネル（テーブルに登録されないため無視される）。
/// USER_DATA_COUNTER は 1 から単調増加するため衝突しない。
const CANCEL_SENTINEL_USER_DATA: u64 = u64::MAX;

/// `OP_TABLE` のキー（`user_data`）専用の超軽量ハッシャ。
///
/// キーはスレッドローカルな単調増加カウンタ由来（`next_user_data`）で、外部入力ではない
/// （ハッシュ衝突攻撃の対象にならない）。`OP_TABLE` は **io_uring の全オペレーション
/// （recv/send/accept/splice/timeout/...）の登録・Waker 設定・完了取り出し**で引かれる
/// プロキシ最ホットパスのため、標準 `HashMap` の SipHash（暗号学的ハッシュ）は純粋な無駄。
/// 黄金比由来の奇数定数を 1 回掛ける Fibonacci ハッシュで全 64bit に拡散し
/// （SwissTable は上位 7bit を制御バイトに使うため連番キーでも上位を 0 にしない）、
/// per-op の SipHash 計算を排除する。乗算 1 回のみ。
#[derive(Default)]
struct NoHashU64(u64);

impl std::hash::Hasher for NoHashU64 {
    #[inline]
    fn write_u64(&mut self, n: u64) {
        // Fibonacci ハッシング（FxHash 同系）。連番キーを全ビットへ拡散する。
        self.0 = n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    #[inline]
    fn write(&mut self, _bytes: &[u8]) {
        // user_data(u64) キーは常に write_u64 を通る。到達しない想定。
        debug_assert!(false, "NoHashU64 supports only u64 keys");
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

type NoHashBuilder = std::hash::BuildHasherDefault<NoHashU64>;
type OpMap<V> = HashMap<u64, V, NoHashBuilder>;

/// 事前確保する in-flight op スロット数（典型的な同時 in-flight op 数を見込む）。
/// ホットパスでの HashMap 再確保（成長時 malloc）を抑える。
const OP_TABLE_PREALLOC: usize = 256;

/// スレッドローカルな操作テーブル
struct OpTable {
    /// user_data -> (OpResult, Waker)（Future が生存している op）
    ops: OpMap<(OpResult, Option<Waker>)>,
    /// user_data -> ガード（Future がドロップされ detach された op）
    detached: OpMap<OpGuard>,
}

impl OpTable {
    fn new() -> Self {
        Self {
            ops: HashMap::with_capacity_and_hasher(OP_TABLE_PREALLOC, NoHashBuilder::default()),
            detached: HashMap::with_capacity_and_hasher(16, NoHashBuilder::default()),
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
        } else if let Some(guard) = self.detached.remove(&cqe.user_data) {
            // detach 済み op が完了/キャンセルした。ここで初めてバッファ解放・fd クローズを行う。
            guard(cqe.res);
            true
        } else {
            // 未知の user_data（ASYNC_CANCEL 自身の CQE 等）→ 無視。
            false
        }
    }

    /// in-flight op を detach する。
    ///
    /// 戻り値が true の場合、呼び出し側は ASYNC_CANCEL を投げてカーネルに早期キャンセルを
    /// 依頼する（accept のように放置すると次の接続を奪う op のため）。
    fn detach(&mut self, user_data: u64, guard: OpGuard) -> bool {
        match self.ops.get(&user_data) {
            Some((OpResult::Done(res), _)) => {
                // 既に完了済み（CQE 到着済みだが take されていない）。即座に後始末。
                let res = *res;
                self.ops.remove(&user_data);
                guard(res);
                false
            }
            Some((OpResult::Pending, _)) => {
                // 未完了。ガードを保持して完了/キャンセルの CQE を待つ。
                self.ops.remove(&user_data);
                self.detached.insert(user_data, guard);
                true
            }
            None => {
                // 既に take 済み（Future が正常完了して結果を取り出した）。カーネルはもう
                // バッファを触らないので、ガードは呼ばずに破棄する（accept fd は引き取り済み）。
                drop(guard);
                false
            }
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
    // IORING_REGISTER_RESTRICTIONS は IORING_SETUP_R_DISABLED 付きで生成した無効状態の
    // リングにのみ適用できる。「生成（R_DISABLED）→ 制限登録 → ENABLE_RINGS で有効化」の
    // 順で行う。R_DISABLED 自体が未対応の古いカーネル（<5.10）では制限なしの通常リングへ
    // フォールバックする。
    let ring = match IoUring::new(entries, flags | IORING_SETUP_R_DISABLED) {
        Ok(ring) => {
            match ring.apply_restrictions(PROXY_ALLOWED_OPCODES) {
                Ok(()) => {
                    // 制限適用後は ENABLE_RINGS を呼ばないと SQE が一切処理されない。
                    ring.enable_rings()?;
                    ftlog::debug!(
                        "io_uring restrictions applied ({} opcodes allowed)",
                        PROXY_ALLOWED_OPCODES.len()
                    );
                }
                Err(e) => {
                    // 制限が未対応でも R_DISABLED で生成した以上は有効化が必須。
                    ftlog::debug!("io_uring restrictions not applied: {}", e);
                    ring.enable_rings()?;
                }
            }
            ring
        }
        Err(e) => {
            // R_DISABLED 未対応カーネル: 制限なしで通常生成にフォールバック。
            ftlog::debug!(
                "io_uring R_DISABLED unsupported ({}); creating ring without restrictions",
                e
            );
            IoUring::new(entries, flags)?
        }
    };

    RING.with(|r| {
        *r.borrow_mut() = Some(ring);
    });
    Ok(())
}

/// このスレッドに io_uring リングが初期化済みか判定する。
///
/// FS オフロード（F-29）が、リングのあるワーカースレッドでは eventfd ベースの非同期待機を、
/// リングの無いコンテキスト（単体テスト等）では同期インライン実行をするための分岐に使う。
pub fn has_ring() -> bool {
    RING.with(|r| r.borrow().is_some())
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

/// in-flight op を detach し、必要なら ASYNC_CANCEL を投げる。
///
/// 提出済み・未完了の op を持つ Future がドロップされたときに呼ぶ。`guard` はカーネルが
/// 参照中のバッファ等を保持し、op の完了/キャンセル時に後始末（バッファ解放・accept fd の
/// クローズ等）を行う。これにより「タイムアウト等で in-flight Future を drop した際に
/// カーネルが参照中のメモリを解放してしまう use-after-free」や「孤立した accept が後続の
/// 接続を奪って捨ててしまう問題」を防ぐ。
pub fn detach_op(user_data: u64, guard: OpGuard) {
    let should_cancel = OP_TABLE.with(|t| t.borrow_mut().detach(user_data, guard));
    if should_cancel {
        submit_cancel(user_data);
    }
}

/// 指定した user_data の in-flight op に ASYNC_CANCEL を投げる（ベストエフォート）。
fn submit_cancel(target_user_data: u64) {
    with_ring(|ring| {
        if let Some(sqe) = ring.get_sqe() {
            sqe.opcode = IORING_OP_ASYNC_CANCEL;
            sqe.fd = -1;
            // ASYNC_CANCEL は addr フィールドにキャンセル対象 op の user_data を入れる。
            sqe.addr_or_splice_off_in = target_user_data;
            // キャンセル op 自身の CQE はテーブル未登録のため on_cqe で無視される。
            sqe.user_data = CANCEL_SENTINEL_USER_DATA;
        }
    });
    let _ = submit_sqes();
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

// thread-per-core 前提の単一スレッドエグゼキュータ。タスクをスレッドローカルのスラブ
// （free-list 付き Vec）で管理し、Waker は「スロット index + 世代」をポインタ幅へパックして
// 持つ。これにより:
//   - 旧実装の接続ごと `Arc<Task>` 確保を排除（スラブのスロット再利用）。
//   - `Mutex<Pin<Box<dyn Future>>>` と `Arc<Mutex<TaskQueue>>` の 2 ロックを排除（単一スレッド
//     のため RefCell で十分）。
//   - wake/schedule ごとの `Arc` クローン（参照カウント atomic）を排除（index の push のみ）。
//
// 残る確保は型消去のための `Box<dyn Future>` 1 本のみ（spawn ごと）。異種 Future を一様に保持
// するため不可避（全スロットを最大サイズでインライン化するのは非現実的）。
//
// ## 健全性（Waker のスレッド前提）
//
// 本ランタイムでは **すべての wake が所有ワーカースレッド上で発生する**。I/O 完了は同スレッドの
// io_uring CQE 処理（`on_cqe`）で wake され、ブロッキングオフロード（`offload.rs`）も完了を
// eventfd + POLL_ADD で **起点スレッドへ** 通知して同スレッドで wake する（Waker をクロス
// スレッドで呼ばない）。`std::task::Waker` の `Send + Sync` 契約上クロススレッド送信は型的に
// 可能だが、本ランタイムは上記不変条件を満たすため index ベースの軽量 Waker が
// スレッドローカル状態へアクセスしても健全である（monoio / glommio と同方針）。Waker は
// ワーカースレッドより長生きしない（OP_TABLE もスレッドローカルで同時に破棄される）。

/// スラブのスロット。poll 時に `future` を take するため `Option` で保持する。
struct TaskSlot {
    /// Future 本体（型消去のため Box。poll 中は take して None になる）
    future: Option<Pin<Box<dyn Future<Output = ()> + 'static>>>,
    /// 世代カウンタ。スロット再利用時に +1 し、古い Waker からの wake を弾く。
    generation: u32,
    /// 既に ready キューに積まれているか（重複 push 抑止）。
    scheduled: bool,
}

/// スレッドローカルなエグゼキュータ状態（スラブ + free-list + ready キュー）。
struct ExecutorState {
    /// タスクスロット（index で参照、index は Vec の realloc を跨いで安定）
    slots: Vec<TaskSlot>,
    /// 空きスロット index の free-list（LIFO）
    free: Vec<usize>,
    /// 実行可能タスクの (index, generation) キュー（FIFO）
    ready: VecDeque<(usize, u32)>,
}

impl ExecutorState {
    fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            ready: VecDeque::new(),
        }
    }

    /// Box 化済み Future を新しいスロットへ格納し、ready キューへ積む。
    fn spawn_boxed(&mut self, future: Pin<Box<dyn Future<Output = ()> + 'static>>) {
        let index = if let Some(i) = self.free.pop() {
            let slot = &mut self.slots[i];
            slot.future = Some(future);
            slot.scheduled = true;
            i
        } else {
            let i = self.slots.len();
            self.slots.push(TaskSlot {
                future: Some(future),
                generation: 0,
                scheduled: true,
            });
            i
        };
        let generation = self.slots[index].generation;
        self.ready.push_back((index, generation));
    }

    /// index/generation のタスクを ready キューへ積む（既に積まれていれば何もしない）。
    fn schedule(&mut self, index: usize, generation: u32) {
        if let Some(slot) = self.slots.get_mut(index) {
            // 世代不一致 = 既に解放/再利用された古い Waker。無視。
            if slot.generation != generation || slot.scheduled {
                return;
            }
            slot.scheduled = true;
            self.ready.push_back((index, generation));
        }
    }
}

thread_local! {
    /// スレッドローカルなエグゼキュータ状態。thread-per-core のためロック不要。
    static EXEC_STATE: RefCell<ExecutorState> = RefCell::new(ExecutorState::new());
}

// ── index ベース Waker ──────────────────────────────────────────────

/// (index, generation) をポインタ幅へパックして Waker の data とする。
#[inline]
fn pack_waker(index: usize, generation: u32) -> *const () {
    (((index as u64) << 32) | (generation as u64)) as *const ()
}

/// `pack_waker` の逆。
#[inline]
fn unpack_waker(data: *const ()) -> (usize, u32) {
    let v = data as u64;
    ((v >> 32) as usize, (v & 0xFFFF_FFFF) as u32)
}

/// タスク用 Waker の vtable（index ベース、参照カウントなし）
static TASK_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    task_waker_clone,
    task_waker_wake,
    task_waker_wake_by_ref,
    task_waker_drop,
);

// SAFETY: data は (index, generation) を埋め込んだ非ポインタ値。参照カウントを持たないため
// clone はビットコピー、drop は no-op。wake は所有スレッド上でのみ呼ばれる前提（上記健全性参照）。
unsafe fn task_waker_clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &TASK_WAKER_VTABLE)
}

unsafe fn task_waker_wake(data: *const ()) {
    let (index, generation) = unpack_waker(data);
    // スレッド破棄中は try_with が Err になり得るため握りつぶす（その場合 wake は不要）。
    let _ = EXEC_STATE.try_with(|s| s.borrow_mut().schedule(index, generation));
}

unsafe fn task_waker_wake_by_ref(data: *const ()) {
    let (index, generation) = unpack_waker(data);
    let _ = EXEC_STATE.try_with(|s| s.borrow_mut().schedule(index, generation));
}

unsafe fn task_waker_drop(_data: *const ()) {}

/// index/generation から Waker を構築する。
fn make_waker(index: usize, generation: u32) -> Waker {
    let raw = RawWaker::new(pack_waker(index, generation), &TASK_WAKER_VTABLE);
    // SAFETY: vtable は有効な関数ポインタを持ち、clone/wake/drop の契約を満たす。
    unsafe { Waker::from_raw(raw) }
}

/// シングルスレッドエグゼキュータのハンドル（状態はスレッドローカル `EXEC_STATE`）。
///
/// 旧実装の `Arc<Mutex<TaskQueue>>` 共有は不要になり、ハンドルは ZST。`spawn()` も
/// `block_on()` も同一スレッドローカル状態を参照するため、ポーリング対象が一致する。
#[derive(Clone, Default)]
pub struct Executor {
    _private: (),
}

impl Executor {
    /// 新しいエグゼキュータハンドルを作成する。
    pub fn new() -> Self {
        Executor { _private: () }
    }

    /// Future をスポーンする。
    pub fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + 'static,
    {
        spawn(future);
    }

    /// Ready なタスクを空になるまで poll する。
    ///
    /// poll 中は `EXEC_STATE` を borrow しない（future を一旦 take して保持する）ため、
    /// future 内からの `spawn()` や自身の `wake()`（いずれも `EXEC_STATE` を再 borrow）が
    /// 安全に行える。
    fn run_ready_tasks(&self) {
        loop {
            // 次の (index, generation) を取り出す（borrow は最小限）。
            let next = EXEC_STATE.with(|s| s.borrow_mut().ready.pop_front());
            let (index, generation) = match next {
                Some(v) => v,
                None => break,
            };

            // スロットを検証して future を take する（poll 中は borrow しない）。
            let taken = EXEC_STATE.with(|s| {
                let mut st = s.borrow_mut();
                match st.slots.get_mut(index) {
                    Some(slot) if slot.generation == generation => {
                        slot.scheduled = false;
                        slot.future.take()
                    }
                    // 世代不一致 = 解放済みの stale エントリ。スキップ。
                    _ => None,
                }
            });
            let mut future = match taken {
                Some(f) => f,
                None => continue,
            };

            // poll（EXEC_STATE 非借用）。
            let waker = make_waker(index, generation);
            let mut cx = Context::from_waker(&waker);
            let poll = future.as_mut().poll(&mut cx);

            // 結果を反映する。
            EXEC_STATE.with(|s| {
                let mut st = s.borrow_mut();
                let ready_done = match st.slots.get_mut(index) {
                    Some(slot) if slot.generation == generation => match poll {
                        Poll::Pending => {
                            // future をスロットへ戻す。
                            slot.future = Some(future);
                            false
                        }
                        Poll::Ready(()) => {
                            // スロットを解放: 世代 +1、scheduled クリア。future(ローカル) は drop。
                            slot.generation = slot.generation.wrapping_add(1);
                            slot.scheduled = false;
                            true
                        }
                    },
                    // 通常起き得ない（同一スレッドのため poll 中に解放されない）。
                    _ => false,
                };
                if ready_done {
                    st.free.push(index);
                }
            });
        }
    }

    /// メインの実行ループ。
    ///
    /// 与えられた Future が完了するまでイベントループを回す。
    pub fn block_on<F, R>(&self, future: F) -> R
    where
        F: Future<Output = R> + 'static,
        R: 'static,
    {
        // 単一スレッドのため Rc<RefCell> で十分（Arc/Mutex/AtomicBool 不要）。
        let result: Rc<RefCell<Option<R>>> = Rc::new(RefCell::new(None));
        let setter = result.clone();

        spawn(async move {
            let r = future.await;
            *setter.borrow_mut() = Some(r);
        });

        loop {
            // Ready なタスクを実行
            self.run_ready_tasks();

            if result.borrow().is_some() {
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

        // RefMut の一時値を result より先にドロップするため一旦ローカルに束ねる。
        let value = result
            .borrow_mut()
            .take()
            .expect("future completed but no result");
        value
    }
}

// ====================
// グローバルエグゼキュータ（スレッドローカル）
// ====================

/// スレッドローカルなエグゼキュータ状態を初期化する（スラブを空に準備する）。
pub fn init_executor() {
    EXEC_STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.slots.clear();
        st.free.clear();
        st.ready.clear();
    });
}

/// Future をスポーンする（現在のスレッドのエグゼキュータに）。
pub fn spawn<F>(future: F)
where
    F: Future<Output = ()> + 'static,
{
    // Box 化（確保）はスレッドローカル borrow の外で行う。
    let boxed: Pin<Box<dyn Future<Output = ()> + 'static>> = Box::pin(future);
    EXEC_STATE.with(|s| s.borrow_mut().spawn_boxed(boxed));
}

/// 現在のスレッドのエグゼキュータハンドルを取得する。
///
/// 状態はスレッドローカルのため、ハンドル経由でも `spawn()` 経由でも同一の
/// イベントループでポーリングされる。
pub fn current_executor() -> Executor {
    Executor::new()
}

#[cfg(test)]
mod executor_tests {
    use super::*;
    use std::cell::Cell;

    /// 自分自身を 1 回 wake して Pending→Ready する Future（協調的 yield）。
    /// 自己 wake のみで進行するため io_uring リング無しでも block_on が完走する
    /// （ready キューが空になる前に必ず完了し、wait_for_completions に到達しない）。
    struct YieldOnce {
        yielded: bool,
    }

    impl Future for YieldOnce {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.yielded {
                Poll::Ready(())
            } else {
                self.yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    fn yield_once() -> YieldOnce {
        YieldOnce { yielded: false }
    }

    #[test]
    fn block_on_immediate() {
        init_executor();
        let exec = current_executor();
        assert_eq!(exec.block_on(async { 40 + 2 }), 42);
    }

    #[test]
    fn block_on_with_self_wake_yield() {
        init_executor();
        let exec = current_executor();
        let r = exec.block_on(async {
            yield_once().await;
            yield_once().await;
            7
        });
        assert_eq!(r, 7);
    }

    #[test]
    fn spawn_children_and_join() {
        init_executor();
        let exec = current_executor();
        let counter = Rc::new(Cell::new(0usize));
        let got = exec.block_on({
            let counter = counter.clone();
            async move {
                for _ in 0..100 {
                    let c = counter.clone();
                    spawn(async move {
                        yield_once().await;
                        c.set(c.get() + 1);
                    });
                }
                while counter.get() < 100 {
                    yield_once().await;
                }
                counter.get()
            }
        });
        assert_eq!(got, 100);
    }

    #[test]
    fn slab_reuses_slots() {
        // 多数の spawn→完了を逐次繰り返し、スロットが再利用される（slots.len が小さい）
        // ことを確認する。再利用が無ければ slots は 50 を超える。
        init_executor();
        let exec = current_executor();
        let counter = Rc::new(Cell::new(0usize));
        exec.block_on({
            let counter = counter.clone();
            async move {
                for round in 0..50 {
                    let c = counter.clone();
                    spawn(async move {
                        yield_once().await;
                        c.set(c.get() + 1);
                    });
                    let target = round + 1;
                    while counter.get() < target {
                        yield_once().await;
                    }
                }
            }
        });
        assert_eq!(counter.get(), 50);
        let slots = EXEC_STATE.with(|s| s.borrow().slots.len());
        assert!(slots <= 8, "slab should reuse slots, got {slots}");
    }

    #[test]
    fn stale_waker_is_ignored() {
        // 解放済みスロットの古い Waker が、再利用後のタスクへ誤って割り込まないことを確認。
        init_executor();
        // 手動でスロットを 1 つ確保→解放→再確保し、世代が進むことを検証する。
        EXEC_STATE.with(|s| {
            let mut st = s.borrow_mut();
            st.spawn_boxed(Box::pin(async {}));
            let (idx, gen0) = st.ready.pop_front().unwrap();
            // 解放（poll で Ready 相当）: 世代 +1 して free へ。
            st.slots[idx].future = None;
            st.slots[idx].generation = st.slots[idx].generation.wrapping_add(1);
            st.slots[idx].scheduled = false;
            st.free.push(idx);
            // 古い世代の Waker による schedule は弾かれる。
            st.schedule(idx, gen0);
            assert!(st.ready.is_empty(), "stale generation must be ignored");
            // 再確保すると同じスロットが世代 +1 で払い出される。
            st.spawn_boxed(Box::pin(async {}));
            let (idx2, gen2) = st.ready.pop_front().unwrap();
            assert_eq!(idx2, idx, "freed slot should be reused");
            assert_ne!(gen2, gen0, "generation must advance on reuse");
        });
    }

    /// OP_TABLE 専用ハッシャ（NoHashU64）が u64 キーで衝突なく機能し、HashMap の登録／
    /// 取得／削除が正しく動くことを検証する（SipHash 排除後のリグレッションガード）。
    #[test]
    fn op_table_nohash_roundtrip() {
        use std::hash::{Hash, Hasher};

        // 異なる連番キーは異なるハッシュへ拡散される（上位ビットも 0 でない）。
        let mut h1 = NoHashU64::default();
        1u64.hash(&mut h1);
        let mut h2 = NoHashU64::default();
        2u64.hash(&mut h2);
        assert_ne!(h1.finish(), h2.finish());
        assert_ne!(
            h1.finish() >> 57,
            0,
            "upper bits must be spread for SwissTable"
        );

        // OpTable の基本ライフサイクル（register → on_cqe → take）が新ハッシャ上で動作する。
        let mut table = OpTable::new();
        for ud in [1u64, 2, 3, 1_000_000, u64::MAX - 1] {
            table.register(ud);
        }
        let cqe = IoUringCqe {
            user_data: 1_000_000,
            res: 42,
            flags: 0,
        };
        assert!(table.on_cqe(&cqe));
        assert_eq!(table.take_result(1_000_000), Some(42));
        // 未完了キーはまだ取り出せない。
        assert_eq!(table.take_result(2), None);
    }
}
