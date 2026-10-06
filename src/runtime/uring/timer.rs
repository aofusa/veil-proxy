//! BinaryHeap ベースのタイマー（io_uring バックエンド）
//!
//! B-72: 旧実装は `Sleep` 1 本ごとに `IORING_OP_TIMEOUT` SQE を張り、
//! `timeout(READ_TIMEOUT, read)` で内側が勝つ（＝タイマーが負ける）度に
//! **キャンセルを投げず** 30 秒間そのままカーネルの `ctx->timeout_list` に
//! 居座らせていた。数千 rps では同リストが 10^5 オーダーまで肥大し、
//! 別経路（in-flight POLL_ADD の drop）が出す `IORING_OP_ASYNC_CANCEL` の
//! カーネル側フォールバック（`io_timeout_cancel` → `io_timeout_extract` →
//! `io_cancel_req_match`）がそのリストを**毎回線形走査**して CPU の 4 割超を
//! 消費していた（詳細: `docs/backlog/bugs/B-72-iouring-timeout-list-on-scan.md`）。
//!
//! 修正: `src/runtime/reactor/timer.rs` と同一の「スレッドローカルの
//! デッドライン最小ヒープ」実装へ置き換える。`Sleep::poll`/`Sleep::drop` は
//! ユーザ空間のヒープ操作のみで SQE も syscall も出さない。カーネルへの
//! `IORING_OP_TIMEOUT` は `executor::wait_for_completions` が park 直前に
//! **最近接の live デッドラインに対して 1 本だけ**アームする
//! （`next_deadline()` / `fire_expired()` は executor 側から呼ばれる）。
//! これにより `ctx->timeout_list` の長さは常に 0 か 1 に収まる。
//!
//! ## スロット/世代
//!
//! `executor.rs` の op テーブルと同様、スロット index + 世代カウンタで
//! （index, generation）をパックしたトークンを発行する。`Sleep` が完了前に drop された
//! （`timeout()` で内側 Future が勝った等）場合はスロットを即座に free-list へ返す。
//! ヒープ上には stale なエントリが残り得るが、pop 時に世代不一致で無視されるため
//! 安全である（不要エントリは「そのデッドラインに達した時」に遅延パージされる）。

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

/// タイマースロットの状態。
enum SlotState {
    /// 空きスロット（free-list に登録済み）。
    Free,
    /// 待機中（Waker は初回 poll でセットされるまで None）。
    Armed(Option<Waker>),
    /// 満了済み（`Sleep::poll` の次回呼び出しでスロットを解放する）。
    Fired,
}

struct TimerSlot {
    generation: u32,
    state: SlotState,
}

/// (index, generation) を u64 へパックする。
#[inline]
fn pack(index: u32, generation: u32) -> u64 {
    ((generation as u64) << 32) | index as u64
}

#[inline]
fn unpack(token: u64) -> (u32, u32) {
    (token as u32, (token >> 32) as u32)
}

struct TimerState {
    slots: Vec<TimerSlot>,
    free: Vec<u32>,
    heap: BinaryHeap<Reverse<(Instant, u64)>>,
}

impl TimerState {
    const PREALLOC: usize = 256;

    fn new() -> Self {
        let mut slots = Vec::with_capacity(Self::PREALLOC);
        let mut free = Vec::with_capacity(Self::PREALLOC);
        for i in 0..Self::PREALLOC as u32 {
            slots.push(TimerSlot {
                generation: 1,
                state: SlotState::Free,
            });
            free.push(i);
        }
        Self {
            slots,
            free,
            heap: BinaryHeap::with_capacity(Self::PREALLOC),
        }
    }

    fn register(&mut self, deadline: Instant) -> u64 {
        let index = match self.free.pop() {
            Some(i) => i,
            None => {
                let i = self.slots.len() as u32;
                self.slots.push(TimerSlot {
                    generation: 1,
                    state: SlotState::Free,
                });
                i
            }
        };
        let slot = &mut self.slots[index as usize];
        slot.state = SlotState::Armed(None);
        let token = pack(index, slot.generation);
        self.heap.push(Reverse((deadline, token)));
        token
    }

    fn resolve(&self, token: u64) -> Option<usize> {
        let (index, generation) = unpack(token);
        let slot = self.slots.get(index as usize)?;
        if slot.generation != generation {
            return None;
        }
        Some(index as usize)
    }

    fn set_waker(&mut self, token: u64, waker: Waker) {
        if let Some(i) = self.resolve(token) {
            if let SlotState::Armed(w) = &mut self.slots[i].state {
                *w = Some(waker);
            }
        }
    }

    /// 満了済みなら true を返しスロットを解放する。
    fn take_fired(&mut self, token: u64) -> bool {
        let Some(i) = self.resolve(token) else {
            return false;
        };
        if matches!(self.slots[i].state, SlotState::Fired) {
            self.free_slot(i);
            true
        } else {
            false
        }
    }

    /// 完了を待たずスロットを解放する（Future drop 時。ヒープのエントリは stale として残る）。
    fn cancel(&mut self, token: u64) {
        if let Some(i) = self.resolve(token) {
            self.free_slot(i);
        }
    }

    fn free_slot(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        slot.generation = slot.generation.wrapping_add(1);
        if slot.generation == 0 {
            slot.generation = 1;
        }
        slot.state = SlotState::Free;
        self.free.push(index as u32);
    }

    /// 次のタイマーデッドラインを返す（stale なヒープ先頭は遅延パージする）。
    fn next_deadline(&mut self) -> Option<Instant> {
        loop {
            let Reverse((deadline, token)) = *self.heap.peek()?;
            match self.resolve(token) {
                Some(i) if matches!(self.slots[i].state, SlotState::Armed(_)) => {
                    return Some(deadline);
                }
                _ => {
                    // stale（cancel 済み、または既に fire 済みで別サイクルに積まれた同一 index
                    // の新規エントリの可能性は世代不一致で弾かれる）。パージして次を見る。
                    self.heap.pop();
                }
            }
        }
    }

    /// `now` 以前に満了したタイマーを起こす。
    fn fire_expired(&mut self, now: Instant) {
        while let Some(&Reverse((deadline, token))) = self.heap.peek() {
            if deadline > now {
                break;
            }
            self.heap.pop();
            let Some(i) = self.resolve(token) else {
                continue; // stale
            };
            if let SlotState::Armed(Some(w)) =
                std::mem::replace(&mut self.slots[i].state, SlotState::Fired)
            {
                w.wake();
            }
        }
    }
}

thread_local! {
    static TIMERS: RefCell<TimerState> = RefCell::new(TimerState::new());
}

/// 現在のスレッドの最近接タイマーデッドラインを返す（`executor::wait_for_completions` の
/// park 直前に呼ばれ、アームすべきカーネル `IORING_OP_TIMEOUT` の期限を決めるために使う）。
pub(crate) fn next_deadline() -> Option<Instant> {
    TIMERS.with(|t| t.borrow_mut().next_deadline())
}

/// `now` 以前に満了したタイマーを起こす（park からの起床処理、および busy ループ中の
/// `block_on` から毎周呼ばれる）。
pub(crate) fn fire_expired(now: Instant) {
    TIMERS.with(|t| t.borrow_mut().fire_expired(now));
}

/// ヒープが空でないかを返す。`Instant::now()` すら呼ばずに「タイマーが 1 本もない」
/// 定常状態を判定するためのショートカット（B-72: `fire_expired` を毎周呼ぶ際、
/// 空なら `Instant::now()` の呼び出し自体を避ける）。
pub(crate) fn has_timers() -> bool {
    TIMERS.with(|t| !t.borrow().heap.is_empty())
}

// ====================
// Sleep Future
// ====================

/// タイムアウト/スリープ Future
///
/// カーネルの `IORING_OP_TIMEOUT` には触れず、スレッドローカルのデッドラインヒープに
/// 登録するのみ（SQE も syscall も出さない）。実際にカーネルへタイムアウトをアームするのは
/// `executor::wait_for_completions` が park 直前に最近接デッドラインに対して行う。
pub struct Sleep {
    deadline: Instant,
    token: u64,
    registered: bool,
}

impl Sleep {
    /// 指定した Duration 後に完了する Sleep Future を作成する
    pub fn new(duration: Duration) -> Self {
        Self {
            deadline: Instant::now() + duration,
            token: 0,
            registered: false,
        }
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.registered {
            let token = TIMERS.with(|t| t.borrow_mut().register(self.deadline));
            self.token = token;
            self.registered = true;
        }

        if TIMERS.with(|t| t.borrow_mut().take_fired(self.token)) {
            return Poll::Ready(());
        }

        TIMERS.with(|t| t.borrow_mut().set_waker(self.token, cx.waker().clone()));
        Poll::Pending
    }
}

// Sleep に FusedFuture を実装（futures::select_biased! で使用するため）
impl futures::future::FusedFuture for Sleep {
    fn is_terminated(&self) -> bool {
        if !self.registered {
            return false;
        }
        TIMERS.with(|t| {
            let t = t.borrow();
            match t.resolve(self.token) {
                Some(i) => matches!(t.slots[i].state, SlotState::Fired),
                None => false,
            }
        })
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        // B-72: カーネルへ SQE を出していないため、キャンセルの syscall も不要。
        // スロットを即座に解放する（世代が進み、ヒープ上の stale エントリは
        // 次回 next_deadline()/fire_expired() で無視される）。
        if self.registered {
            TIMERS.with(|t| t.borrow_mut().cancel(self.token));
        }
    }
}

// ====================
// sleep / timeout API
// ====================

/// 指定した Duration スリープする
/// reactor 版（起床ごとに 1 回読む粗い時計）と同じ公開 API。io_uring 版は既存の
/// ロジックを変えないため、その場で正確な時刻を返す。
#[inline]
pub fn coarse_now() -> Instant {
    Instant::now()
}

pub fn sleep(duration: Duration) -> Sleep {
    Sleep::new(duration)
}

/// Future にタイムアウトを設定する
///
/// タイムアウト前に Future が完了すれば `Ok(R)` を返す。
/// タイムアウトした場合は `Err(Elapsed)` を返す。
pub async fn timeout<F, R>(duration: Duration, future: F) -> Result<R, Elapsed>
where
    F: Future<Output = R>,
{
    futures::select_biased! {
        result = futures::FutureExt::fuse(future) => Ok(result),
        _ = futures::FutureExt::fuse(sleep(duration)) => Err(Elapsed),
    }
}

/// タイムアウトエラー
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

impl std::fmt::Display for Elapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "deadline has elapsed")
    }
}

impl std::error::Error for Elapsed {}

// ====================
// テスト
// ====================

#[cfg(test)]
mod tests {
    use super::*;

    /// io_uring が利用可能か（生成できるか）を検査する。
    /// io_uring を許可しない環境（Docker ビルドサンドボックス・古いカーネル・
    /// seccomp 制限下など）ではリング生成が失敗するため、実行を伴うテストはスキップする。
    fn io_uring_available() -> bool {
        crate::runtime::ring::IoUring::new(8, 0).is_ok()
    }

    #[test]
    fn sleep_completes_after_duration() {
        if !io_uring_available() {
            eprintln!("io_uring unavailable; skipping sleep_completes_after_duration");
            return;
        }
        crate::runtime::block_on(async {
            let start = Instant::now();
            sleep(Duration::from_millis(20)).await;
            assert!(start.elapsed() >= Duration::from_millis(20));
        });
    }

    #[test]
    fn timeout_ok_when_inner_finishes_first() {
        if !io_uring_available() {
            eprintln!("io_uring unavailable; skipping timeout_ok_when_inner_finishes_first");
            return;
        }
        crate::runtime::block_on(async {
            let result = timeout(Duration::from_secs(5), async { 42 }).await;
            assert_eq!(result, Ok(42));
        });
    }

    #[test]
    fn timeout_elapsed_when_inner_never_finishes() {
        if !io_uring_available() {
            eprintln!("io_uring unavailable; skipping timeout_elapsed_when_inner_never_finishes");
            return;
        }
        crate::runtime::block_on(async {
            let result = timeout(Duration::from_millis(20), std::future::pending::<()>()).await;
            assert_eq!(result, Err(Elapsed));
        });
    }

    /// B-72: Sleep を満了前に drop すると、スロットが即座に解放され（ヒープに
    /// stale エントリのみが残り）、カーネルへタイマーがアームされたままにならないこと。
    #[test]
    fn dropping_sleep_frees_slot_without_arming_kernel_timer() {
        if !io_uring_available() {
            eprintln!(
                "io_uring unavailable; skipping dropping_sleep_frees_slot_without_arming_kernel_timer"
            );
            return;
        }
        crate::runtime::block_on(async {
            {
                let mut s = sleep(Duration::from_secs(30));
                // 1 回 poll して登録させる（token 発行・ヒープへ push）。30 秒先の
                // デッドラインなので確実に Pending が返る。
                let waker = noop_waker();
                let mut cx = Context::from_waker(&waker);
                assert_eq!(Pin::new(&mut s).poll(&mut cx), Poll::Pending);
            }
            // drop 後は live なデッドラインが存在しない（stale エントリは
            // next_deadline() が遅延パージして無視する）。
            assert_eq!(
                next_deadline(),
                None,
                "dropped Sleep must not leave a live deadline"
            );
        });
    }

    /// 何もしない Waker（poll 1 回だけを目的とする手動 poll 用）。
    fn noop_waker() -> Waker {
        use std::task::{RawWaker, RawWakerVTable};
        const VTABLE: RawWakerVTable = RawWakerVTable::new(
            |_| RawWaker::new(std::ptr::null(), &VTABLE),
            |_| {},
            |_| {},
            |_| {},
        );
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
    }
}
