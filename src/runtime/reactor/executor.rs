//! シングルスレッド非同期エグゼキュータ（reactor バックエンド）
//!
//! タスクスケジューリング機構（`Executor`/`TaskPool`/`spawn`/`yield_now`/`block_on`）は
//! `runtime::uring::executor` と等価な実装を保つ（設計ドキュメント 3.2 節: 「uring 側の
//! コードパスを変えないことを優先し、executor をバックエンド毎に持つ。共通化リファクタは
//! 行わない」）。相違点はパーキング方式のみ:
//!
//! - uring 版: io_uring `submit_and_wait(1)` で完了を待つ。
//! - reactor 版: タイマー最近接デッドラインを timeout にした `epoll_wait` でイベントを
//!   待ち、fd 起床とタイマー起床の両方を処理する。
//!
//! io_uring 固有の API（`PROXY_ALLOWED_OPCODES` / `init_ring` / `with_ring` /
//! `process_cqe` / `fuzz_op_table_sequence` 等）は reactor には存在しない。

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::time::Instant;

use super::poller::{FdTable, Interest};

#[cfg(veil_poller_epoll)]
use super::epoll::{EpollPoller, ERR_HUP, READ, WRITE};

#[cfg(veil_poller_kqueue)]
use super::kqueue::{KqueuePoller, READ, WRITE};

// ====================
// reactor ドライバ（poller + fd テーブル）
// ====================

thread_local! {
    #[cfg(veil_poller_epoll)]
    static POLLER: RefCell<Option<EpollPoller>> = const { RefCell::new(None) };
    #[cfg(veil_poller_kqueue)]
    static POLLER: RefCell<Option<KqueuePoller>> = const { RefCell::new(None) };
    static FD_TABLE: RefCell<FdTable> = RefCell::new(FdTable::new());
}

/// このスレッドの poller（epoll インスタンス）を初期化する。
#[cfg(veil_poller_epoll)]
pub fn init_reactor() -> std::io::Result<()> {
    let poller = EpollPoller::new()?;
    POLLER.with(|p| *p.borrow_mut() = Some(poller));
    Ok(())
}

/// このスレッドの poller（kqueue インスタンス）を初期化する。
#[cfg(veil_poller_kqueue)]
pub fn init_reactor() -> std::io::Result<()> {
    let poller = KqueuePoller::new()?;
    POLLER.with(|p| *p.borrow_mut() = Some(poller));
    Ok(())
}

/// このスレッドに reactor ドライバ（poller）が初期化済みか判定する。
///
/// `runtime::offload`（F-29）が、ドライバのあるワーカースレッドでは fd readiness
/// ベースの非同期待機を、ドライバの無いコンテキスト（単体テスト等）では同期インライン
/// 実行をするための分岐に使う（uring 版の `has_ring()` に相当）。
#[cfg(any(veil_poller_epoll, veil_poller_kqueue))]
pub fn has_driver() -> bool {
    POLLER.with(|p| p.borrow().is_some())
}

#[cfg(veil_poller_epoll)]
fn with_poller<R>(f: impl FnOnce(&EpollPoller) -> R) -> R {
    POLLER.with(|p| {
        let b = p.borrow();
        let poller = b
            .as_ref()
            .expect("reactor poller not initialized for this thread");
        f(poller)
    })
}

#[cfg(veil_poller_kqueue)]
fn with_poller<R>(f: impl FnOnce(&KqueuePoller) -> R) -> R {
    POLLER.with(|p| {
        let b = p.borrow();
        let poller = b
            .as_ref()
            .expect("reactor poller not initialized for this thread");
        f(poller)
    })
}

/// fd の読み取り可能待ちを（再）登録する（oneshot）。
#[cfg(any(veil_poller_epoll, veil_poller_kqueue))]
pub(crate) fn register_read(fd: RawFd, waker: Waker) {
    register(fd, Interest::Read, waker);
}

/// fd の書き込み可能待ちを（再）登録する（oneshot）。
#[cfg(any(veil_poller_epoll, veil_poller_kqueue))]
pub(crate) fn register_write(fd: RawFd, waker: Waker) {
    register(fd, Interest::Write, waker);
}

#[cfg(veil_poller_epoll)]
fn register(fd: RawFd, interest: Interest, waker: Waker) {
    let (needs_add, mask) = FD_TABLE.with(|t| {
        let mut t = t.borrow_mut();
        let rec = t.get_or_insert(fd);
        // ADD/MOD の判定は「一度でも ADD 済みか」（`known_to_kernel`）だけを見る。
        // `armed == 0` は EPOLLONESHOT 発火直後にも起こり得るが、その場合でも fd 自体は
        // epoll の監視対象リストに残っているため MOD を使う必要がある（このコメントに
        // 至った実装バグの詳細は `poller::FdRecord` の doc を参照）。
        let needs_add = !rec.known_to_kernel;
        // 同一方向の複数同時待機者を許容する（キューへ追加。`poller::FdRecord` の doc
        // 参照。offload の共有 eventfd 等、1 fd に複数タスクが同時に読み取り可能待ちを
        // するケースで、先行者の Waker を上書き消失させないために必須）。
        let bit = match interest {
            Interest::Read => {
                rec.read_wakers.push(waker);
                READ
            }
            Interest::Write => {
                rec.write_wakers.push(waker);
                WRITE
            }
        };
        let new_mask = rec.armed | bit;
        rec.armed = new_mask;
        (needs_add, new_mask)
    });
    // 「既に armed 済みなら epoll_ctl を省略する」最適化はあえて行わない。
    // `register()` は個々の Future の poll ごとに（EAGAIN の度に）呼ばれるため、
    // ここでの epoll_ctl は「新規待機開始」だけでなく「まだ完了していない待機の
    // 再確認」でも起こり得る。呼び出しごとに MOD/ADD を無条件で発行することで、
    // fd ごとの armed ビット計算に依存する ADD/MOD 判定ミス（EPOLLONESHOT の
    // 再武装漏れ）の余地を構造的に排除する（EPOLL_CTL_MOD は冪等で安全に繰り返せる）。
    let res = if needs_add {
        with_poller(|p| p.add(fd, mask))
    } else {
        with_poller(|p| p.modify(fd, mask))
    };
    match res {
        Ok(()) => {
            if needs_add {
                FD_TABLE.with(|t| {
                    if let Some(rec) = t.borrow_mut().get_mut(fd) {
                        rec.known_to_kernel = true;
                    }
                });
            }
        }
        Err(e) => {
            ftlog::error!("reactor: epoll register failed for fd {}: {}", fd, e);
        }
    }
}

/// fd の interest（READ/WRITE いずれか）を oneshot で（再）登録する（kqueue 版）。
///
/// kqueue は `EVFILT_READ`/`EVFILT_WRITE` が独立フィルタなので、epoll のような
/// ADD/MOD 判定（`known_to_kernel`）は不要で `EV_ADD|EV_ONESHOT` を常に使う。
/// `armed`（登録済みとして追跡しているビット）は `KqueuePoller::update` の
/// `prev_mask` 引数として渡し、直前まで登録していてビットが落ちた方向のみ
/// `EV_DELETE` する判定に使う（`register()` 呼び出しは新規待機のみで発生し、
/// 「既に armed 済みの方向を down する」ことはないため、prev_mask == new_mask の
/// 場合は update 内で ADD が再発行されるだけで実害は無い）。
#[cfg(veil_poller_kqueue)]
fn register(fd: RawFd, interest: Interest, waker: Waker) {
    let (prev_mask, new_mask) = FD_TABLE.with(|t| {
        let mut t = t.borrow_mut();
        let rec = t.get_or_insert(fd);
        let prev = rec.armed;
        let bit = match interest {
            Interest::Read => {
                rec.read_wakers.push(waker);
                READ
            }
            Interest::Write => {
                rec.write_wakers.push(waker);
                WRITE
            }
        };
        rec.armed |= bit;
        (prev, rec.armed)
    });
    if let Err(e) = with_poller(|p| p.update(fd, new_mask, prev_mask)) {
        ftlog::error!("reactor: kqueue register failed for fd {}: {}", fd, e);
    }
}

/// fd の読み取り待機者 **全員** を起こす（readiness 通知の「横取り」再配布用）。
///
/// 共有 fd（`runtime::offload` のスレッドごと eventfd）では、複数タスクが同一 fd の
/// 読み取り可能を待つ。あるタスクの try-first `poll(2)` チェックがカウンタを drain すると、
/// **他タスク宛ての通知シグナルごと消費**してしまい、EPOLLONESHOT のイベントは
/// `epoll_wait` 時点のレベル再評価で蒸発する（drain 済み = 非 readable）。このとき
/// 待機中タスクは自分の完了フラグが立っていても永久に起床しない
/// （F-120 Phase 2 の E2E `test_f62_wasm_http_call_concurrent_requests` で発見した
/// 実装バグ。io_uring 版は `POLL_ADD` の完了が write 時点で CQE として記録されるため
/// この問題は構造的に起こらない）。
///
/// そのため「drain した者が、同じ fd の残り待機者全員を起こして再確認させる」責務を
/// 本関数で提供する。起こされたタスクは自身の状態（offload の done 等）を再確認し、
/// 未完了なら再登録して待機に戻る。
#[cfg(veil_rt_reactor)]
pub(crate) fn wake_all_readers(fd: RawFd) {
    let wakers = FD_TABLE.with(|t| {
        let mut t = t.borrow_mut();
        let Some(rec) = t.get_mut(fd) else {
            return Vec::new();
        };
        // カーネル側の armed ビットはそのままにする（counter=0 なら発火しないため無害。
        // 起こされたタスクの再登録（MOD）で上書きされる）。テーブル側の armed も
        // 対応する再登録で再計算されるためここでは触らない。
        std::mem::take(&mut rec.read_wakers)
    });
    for w in wakers {
        w.wake();
    }
}

/// fd の登録を破棄する（close 直前に呼ぶ）。
///
/// fd 番号は close 直後に OS から再利用され得るため、テーブルへ stale な Waker/armed 状態を
/// 残さないよう必ず close 前に呼ぶこと。
///
/// `TcpStream`/`TcpListener`/`Pipe` の `Drop` から呼ばれるため、スレッド終了時の
/// thread_local 破棄順序次第では `FD_TABLE`/`POLLER` が既に破棄済みのことがある
/// （例: これらを内部に保持するプール自体が別の thread_local で、そのデストラクタが
/// 本関数より後に走る保証が無い）。`try_with` で防御し、破棄済みなら黙ってスキップする
/// （fd 自体は呼び出し側が直後に `close(2)` するか、プロセス/スレッド終了時はカーネルが
/// 自動でクローズして epoll 登録も自動除去するため、明示 unregister の省略は安全）。
#[cfg(veil_poller_epoll)]
pub(crate) fn unregister(fd: RawFd) {
    let existed = FD_TABLE
        .try_with(|t| t.borrow_mut().remove(fd).is_some())
        .unwrap_or(false);
    if existed {
        let _ = POLLER.try_with(|p| {
            if let Some(poller) = p.borrow().as_ref() {
                poller.delete(fd);
            }
        });
    }
}

/// fd の登録を破棄する（close 直前に呼ぶ、kqueue 版）。
///
/// epoll 版と異なり、`delete` には現在 armed 済みのビット（どのフィルタが
/// 登録されているか）を渡す必要がある（kqueue は fd 単位ではなくフィルタ単位で
/// 個別に `EV_DELETE` するため）。
#[cfg(veil_poller_kqueue)]
pub(crate) fn unregister(fd: RawFd) {
    let armed = FD_TABLE
        .try_with(|t| t.borrow_mut().remove(fd).map(|rec| rec.armed))
        .unwrap_or(None);
    if let Some(armed) = armed {
        let _ = POLLER.try_with(|p| {
            if let Some(poller) = p.borrow().as_ref() {
                poller.delete(fd, armed);
            }
        });
    }
}

/// poller wait のイベントバッファ最大件数（事前確保しホットパスで再アロケーションしない）。
const EVENT_BATCH: usize = 256;

/// 1 回分の poller wait + イベント/タイマー処理。
#[cfg(veil_poller_epoll)]
fn park(timeout_ms: i32) {
    thread_local! {
        static EVENT_BUF: RefCell<Vec<libc::epoll_event>> =
            RefCell::new(vec![unsafe { std::mem::zeroed() }; EVENT_BATCH]);
    }
    let n = EVENT_BUF.with(|buf| {
        let mut buf = buf.borrow_mut();
        match with_poller(|p| p.wait(&mut buf, timeout_ms)) {
            Ok(n) => n,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::Interrupted {
                    ftlog::error!("reactor: epoll_wait error: {}", e);
                }
                0
            }
        }
    });
    if n > 0 {
        EVENT_BUF.with(|buf| {
            let buf = buf.borrow();
            for ev in buf.iter().take(n) {
                dispatch_event(ev.u64 as RawFd, ev.events);
            }
        });
    }
    super::timer::fire_expired(Instant::now());
}

#[cfg(veil_poller_epoll)]
fn dispatch_event(fd: RawFd, flags: u32) {
    // キュー内の **全** Waker を起床する（`poller::FdRecord` の doc 参照）。各タスクは
    // 自身の非ブロッキング syscall を再試行し、成功できなかったものは再度 register する
    // （レベルトリガ相当。eventfd のように「複数読者が同時に readable を観測できる」fd
    // では全員が正しく起床する必要がある）。
    let (read_wakers, write_wakers, remaining) = FD_TABLE.with(|t| {
        let mut t = t.borrow_mut();
        let Some(rec) = t.get_mut(fd) else {
            return (Vec::new(), Vec::new(), 0);
        };
        let mut rw = Vec::new();
        let mut ww = Vec::new();
        if flags & (READ | ERR_HUP) != 0 && !rec.read_wakers.is_empty() {
            rw = std::mem::take(&mut rec.read_wakers);
            rec.armed &= !READ;
        }
        if flags & (WRITE | ERR_HUP) != 0 && !rec.write_wakers.is_empty() {
            ww = std::mem::take(&mut rec.write_wakers);
            rec.armed &= !WRITE;
        }
        (rw, ww, rec.armed)
    });
    for w in read_wakers {
        w.wake();
    }
    for w in write_wakers {
        w.wake();
    }
    if remaining != 0 {
        // EPOLLONESHOT により fd 全体の interest が disarm されているため、まだ待ち手が
        // 残っている方向（読み書きどちらか一方のみ起床した場合）を再武装する。
        let _ = with_poller(|p| p.modify(fd, remaining));
    }
}

/// kqueue バージョンの 1 回分の poller wait + イベント/タイマー処理。
#[cfg(veil_poller_kqueue)]
fn park(timeout_ms: i32) {
    thread_local! {
        static EVENT_BUF: RefCell<Vec<libc::kevent>> =
            RefCell::new(vec![unsafe { std::mem::zeroed() }; EVENT_BATCH]);
    }
    let n = EVENT_BUF.with(|buf| {
        let mut buf = buf.borrow_mut();
        match with_poller(|p| p.wait(&mut buf, timeout_ms)) {
            Ok(n) => n,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::Interrupted {
                    ftlog::error!("reactor: kevent wait error: {}", e);
                }
                0
            }
        }
    });
    if n > 0 {
        EVENT_BUF.with(|buf| {
            let buf = buf.borrow();
            for ev in buf.iter().take(n) {
                let fd = ev.ident as RawFd;
                // EVFILT_READ/EVFILT_WRITE はフィルタごとに独立したイベントとして届く
                // （epoll のように 1 fd 1 イベントへ両方向がまとめられない）ため、
                // フィルタ種別を READ/WRITE ビットへ変換して dispatch_event へ渡す。
                let bit = if ev.filter == libc::EVFILT_READ {
                    READ
                } else if ev.filter == libc::EVFILT_WRITE {
                    WRITE
                } else {
                    continue;
                };
                dispatch_event(fd, bit);
            }
        });
    }
    super::timer::fire_expired(Instant::now());
}

#[cfg(veil_poller_kqueue)]
fn dispatch_event(fd: RawFd, flags: u32) {
    // kqueue は EV_ONESHOT 発火時にカーネル側フィルタを自動削除するため、epoll のように
    // 「片方だけ起きたらもう片方を再武装する」再武装処理（epoll_ctl(MOD)）は不要。
    // 起きた方向のビットを armed から落とすだけでよい（次回 register 時に改めて
    // EV_ADD|EV_ONESHOT される）。
    let (read_wakers, write_wakers) = FD_TABLE.with(|t| {
        let mut t = t.borrow_mut();
        let Some(rec) = t.get_mut(fd) else {
            return (Vec::new(), Vec::new());
        };
        let mut rw = Vec::new();
        let mut ww = Vec::new();
        if flags & READ != 0 && !rec.read_wakers.is_empty() {
            rw = std::mem::take(&mut rec.read_wakers);
            rec.armed &= !READ;
        }
        if flags & WRITE != 0 && !rec.write_wakers.is_empty() {
            ww = std::mem::take(&mut rec.write_wakers);
            rec.armed &= !WRITE;
        }
        (rw, ww)
    });
    for w in read_wakers {
        w.wake();
    }
    for w in write_wakers {
        w.wake();
    }
}

/// poller wait のタイムアウト（ミリ秒）を最近接タイマーデッドラインから計算する。
/// タイマーが無ければ無期限待機（-1）。
fn next_timeout_ms() -> i32 {
    match super::timer::next_deadline() {
        Some(deadline) => {
            let now = Instant::now();
            if deadline <= now {
                0
            } else {
                let ms = (deadline - now).as_millis();
                ms.min(i32::MAX as u128) as i32
            }
        }
        None => -1,
    }
}

// ====================
// シングルスレッドエグゼキュータ（uring 版 `runtime::uring::executor` と等価）
// ====================
//
// thread-per-core 前提の単一スレッドエグゼキュータ。タスクをスレッドローカルのスラブ
// （free-list 付き Vec）で管理し、Waker は「スロット index + 世代」をポインタ幅へパックして
// 持つ。設計・不変条件は uring 版と同一（コメントは уring 版を参照）。

/// プールされたタスクの poll フック。
pub(crate) trait PoolPoll {
    fn poll_slot(&self, slot: u32, cx: &mut Context<'_>) -> Poll<()>;
    fn drop_slot(&self, slot: u32);
}

enum TaskBody {
    Boxed(Pin<Box<dyn Future<Output = ()> + 'static>>),
    Pooled { pool: Rc<dyn PoolPoll>, slot: u32 },
}

impl Drop for TaskBody {
    fn drop(&mut self) {
        if let TaskBody::Pooled { pool, slot } = self {
            pool.drop_slot(*slot);
        }
    }
}

struct TaskSlot {
    body: Option<TaskBody>,
    generation: u32,
    scheduled: bool,
}

struct ExecutorState {
    slots: Vec<TaskSlot>,
    free: Vec<usize>,
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

    fn spawn_body(&mut self, body: TaskBody) {
        let index = if let Some(i) = self.free.pop() {
            let slot = &mut self.slots[i];
            slot.body = Some(body);
            slot.scheduled = true;
            i
        } else {
            let i = self.slots.len();
            self.slots.push(TaskSlot {
                body: Some(body),
                generation: 0,
                scheduled: true,
            });
            i
        };
        let generation = self.slots[index].generation;
        self.ready.push_back((index, generation));
    }

    fn schedule(&mut self, index: usize, generation: u32) {
        if let Some(slot) = self.slots.get_mut(index) {
            if slot.generation != generation || slot.scheduled {
                return;
            }
            slot.scheduled = true;
            self.ready.push_back((index, generation));
        }
    }
}

thread_local! {
    static EXEC_STATE: RefCell<ExecutorState> = RefCell::new(ExecutorState::new());
}

#[inline]
fn pack_waker(index: usize, generation: u32) -> *const () {
    (((index as u64) << 32) | (generation as u64)) as *const ()
}

#[inline]
fn unpack_waker(data: *const ()) -> (usize, u32) {
    let v = data as u64;
    ((v >> 32) as usize, (v & 0xFFFF_FFFF) as u32)
}

static TASK_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    task_waker_clone,
    task_waker_wake,
    task_waker_wake_by_ref,
    task_waker_drop,
);

// SAFETY: data は (index, generation) を埋め込んだ非ポインタ値。参照カウントを持たないため
// clone はビットコピー、drop は no-op。wake は所有スレッド上でのみ呼ばれる前提
// （uring 版と同一の健全性: すべての wake が所有ワーカースレッド上で発生する）。
unsafe fn task_waker_clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &TASK_WAKER_VTABLE)
}

unsafe fn task_waker_wake(data: *const ()) {
    let (index, generation) = unpack_waker(data);
    let _ = EXEC_STATE.try_with(|s| s.borrow_mut().schedule(index, generation));
}

unsafe fn task_waker_wake_by_ref(data: *const ()) {
    let (index, generation) = unpack_waker(data);
    let _ = EXEC_STATE.try_with(|s| s.borrow_mut().schedule(index, generation));
}

unsafe fn task_waker_drop(_data: *const ()) {}

fn make_waker(index: usize, generation: u32) -> Waker {
    let raw = RawWaker::new(pack_waker(index, generation), &TASK_WAKER_VTABLE);
    // SAFETY: vtable は有効な関数ポインタを持ち、clone/wake/drop の契約を満たす。
    unsafe { Waker::from_raw(raw) }
}

/// シングルスレッドエグゼキュータのハンドル（状態はスレッドローカル `EXEC_STATE`）。
#[derive(Clone, Default)]
pub struct Executor {
    _private: (),
}

impl Executor {
    pub fn new() -> Self {
        Executor { _private: () }
    }

    pub fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + 'static,
    {
        spawn(future);
    }

    fn run_ready_tasks(&self) {
        loop {
            let next = EXEC_STATE.with(|s| s.borrow_mut().ready.pop_front());
            let (index, generation) = match next {
                Some(v) => v,
                None => break,
            };

            let taken = EXEC_STATE.with(|s| {
                let mut st = s.borrow_mut();
                match st.slots.get_mut(index) {
                    Some(slot) if slot.generation == generation => {
                        slot.scheduled = false;
                        slot.body.take()
                    }
                    _ => None,
                }
            });
            let mut body = match taken {
                Some(b) => b,
                None => continue,
            };

            let waker = make_waker(index, generation);
            let mut cx = Context::from_waker(&waker);
            let poll = match &mut body {
                TaskBody::Boxed(f) => f.as_mut().poll(&mut cx),
                TaskBody::Pooled { pool, slot } => pool.poll_slot(*slot, &mut cx),
            };

            EXEC_STATE.with(|s| {
                let mut st = s.borrow_mut();
                let ready_done = match st.slots.get_mut(index) {
                    Some(slot) if slot.generation == generation => match poll {
                        Poll::Pending => {
                            slot.body = Some(body);
                            false
                        }
                        Poll::Ready(()) => {
                            slot.generation = slot.generation.wrapping_add(1);
                            slot.scheduled = false;
                            true
                        }
                    },
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
    /// io_uring 版と異なり、CQE 待機の代わりに poller wait（epoll_wait）で
    /// パーキングする。timeout はタイマーの最近接デッドラインから計算する。
    pub fn block_on<F, R>(&self, future: F) -> R
    where
        F: Future<Output = R> + 'static,
        R: 'static,
    {
        let result: Rc<RefCell<Option<R>>> = Rc::new(RefCell::new(None));
        let setter = result.clone();

        spawn(async move {
            let r = future.await;
            *setter.borrow_mut() = Some(r);
        });

        loop {
            self.run_ready_tasks();

            if result.borrow().is_some() {
                break;
            }

            let timeout_ms = next_timeout_ms();
            #[cfg(any(veil_poller_epoll, veil_poller_kqueue))]
            park(timeout_ms);
            #[cfg(not(any(veil_poller_epoll, veil_poller_kqueue)))]
            {
                // veil_rt_reactor は必ずどちらかの poller cfg を伴う（build.rs）ため、
                // ここへ到達することは cfg 上あり得ない。
                let _ = timeout_ms;
                unreachable!("reactor backend without a poller cfg");
            }
        }

        let value = result
            .borrow_mut()
            .take()
            .expect("future completed but no result");
        value
    }
}

/// スレッドローカルなエグゼキュータ状態を初期化する（タスクスラブを空に準備する）。
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
    let boxed: Pin<Box<dyn Future<Output = ()> + 'static>> = Box::pin(future);
    EXEC_STATE.with(|s| s.borrow_mut().spawn_body(TaskBody::Boxed(boxed)));
}

/// 現在のスレッドのエグゼキュータハンドルを取得する。
pub fn current_executor() -> Executor {
    Executor::new()
}

// ====================
// 型付きタスクプール（uring 版と同一実装）
// ====================

const POOL_CHUNK: usize = 16;

/// 型付きタスクプール（spawn ごとの `Box<dyn Future>` ヒープ確保を排除）。
pub struct TaskPool<F: Future<Output = ()> + 'static> {
    inner: Rc<PoolInner<F>>,
}

impl<F: Future<Output = ()> + 'static> Clone for TaskPool<F> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

struct PoolInner<F> {
    chunks: RefCell<Vec<Box<[RefCell<Option<F>>]>>>,
    free: RefCell<Vec<u32>>,
}

impl<F: Future<Output = ()> + 'static> TaskPool<F> {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(PoolInner {
                chunks: RefCell::new(Vec::new()),
                free: RefCell::new(Vec::new()),
            }),
        }
    }

    pub fn spawn(&self, future: F) {
        let slot = {
            let mut free = self.inner.free.borrow_mut();
            match free.pop() {
                Some(s) => s,
                None => {
                    let mut chunks = self.inner.chunks.borrow_mut();
                    let base = (chunks.len() * POOL_CHUNK) as u32;
                    let chunk: Box<[RefCell<Option<F>>]> = (0..POOL_CHUNK)
                        .map(|_| RefCell::new(None))
                        .collect::<Vec<_>>()
                        .into_boxed_slice();
                    chunks.push(chunk);
                    for i in (1..POOL_CHUNK as u32).rev() {
                        free.push(base + i);
                    }
                    base
                }
            }
        };
        {
            let chunks = self.inner.chunks.borrow();
            let cell = &chunks[slot as usize / POOL_CHUNK][slot as usize % POOL_CHUNK];
            *cell.borrow_mut() = Some(future);
        }
        let pool: Rc<dyn PoolPoll> = self.inner.clone();
        EXEC_STATE.with(|s| s.borrow_mut().spawn_body(TaskBody::Pooled { pool, slot }));
    }
}

impl<F: Future<Output = ()> + 'static> Default for TaskPool<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: Future<Output = ()> + 'static> PoolPoll for PoolInner<F> {
    fn poll_slot(&self, slot: u32, cx: &mut Context<'_>) -> Poll<()> {
        let cell: *const RefCell<Option<F>> = {
            let chunks = self.chunks.borrow();
            &chunks[slot as usize / POOL_CHUNK][slot as usize % POOL_CHUNK] as *const _
        };
        let cell = unsafe { &*cell };
        let mut guard = cell.borrow_mut();
        let fut = guard.as_mut().expect("pooled task polled after completion");
        // SAFETY: future は格納後、解放（in-place drop）まで一切ムーブしない。
        let pinned = unsafe { Pin::new_unchecked(fut) };
        match pinned.poll(cx) {
            Poll::Ready(()) => {
                *guard = None;
                drop(guard);
                self.free.borrow_mut().push(slot);
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn drop_slot(&self, slot: u32) {
        let cell: *const RefCell<Option<F>> = {
            let chunks = self.chunks.borrow();
            &chunks[slot as usize / POOL_CHUNK][slot as usize % POOL_CHUNK] as *const _
        };
        let cell = unsafe { &*cell };
        let had_future = {
            let mut guard = cell.borrow_mut();
            guard.take().is_some()
        };
        if had_future {
            self.free.borrow_mut().push(slot);
        }
    }
}

/// 現在のタスクを一度だけ実行キューの末尾へ譲る（協調的 yield）。
pub async fn yield_now() {
    struct YieldNow(bool);
    impl Future for YieldNow {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
    YieldNow(false).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

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
    fn task_pool_spawn_and_complete() {
        init_executor();
        let exec = current_executor();
        let counter = Rc::new(Cell::new(0usize));
        let got = exec.block_on({
            let counter = counter.clone();
            async move {
                let pool = TaskPool::new();
                for _ in 0..100 {
                    let c = counter.clone();
                    pool.spawn(async move {
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
}
