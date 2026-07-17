//! BinaryHeap ベースのタイマー（reactor バックエンド）
//!
//! io_uring 版（`runtime::uring::timer`）は `IORING_OP_TIMEOUT` をカーネルへ委譲するが、
//! readiness ベースの reactor はタイマー専用 fd（timerfd 等）を使わず、スレッドローカルの
//! `BinaryHeap<Reverse<(Instant, token)>>` で最近接デッドラインを自前管理する
//! （fd 節約・実装統一。設計ドキュメント 3.3 節）。`reactor::executor::block_on` の
//! パーキングは、このヒープの最近接デッドラインを poller `wait` の timeout に変換し、
//! 起床後に期限切れタイマーを起こす。
//!
//! ## スロット/世代
//!
//! `executor.rs` の op テーブルと同様、スロット index + 世代カウンタで
//! （index, generation）をパックしたトークンを発行する。`Sleep` が完了前に drop された
//! （`timeout()` で内側 Future が勝った等）場合はスロットを即座に free-list へ返す。
//! ヒープ上には stale なエントリが残り得るが、pop 時に世代不一致で無視されるため
//! 安全である（不要エントリは「そのデッドラインに達した時」に遅延パージされる。
//! 定常的な cancel の多さは、そのデッドラインまでの間ヒープに残留するが、
//! 実際のタイムアウト値の範囲でしか残らないため無制限には増えない）。

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

/// 現在のスレッドの最近接タイマーデッドラインを返す（reactor block_on のパーキング用）。
pub(crate) fn next_deadline() -> Option<Instant> {
    TIMERS.with(|t| t.borrow_mut().next_deadline())
}

/// `now` 以前に満了したタイマーを起こす（reactor block_on の起床処理から呼ぶ）。
pub(crate) fn fire_expired(now: Instant) {
    TIMERS.with(|t| t.borrow_mut().fire_expired(now));
}

// ====================
// Sleep Future
// ====================

/// タイムアウト/スリープ Future（`runtime::uring::timer::Sleep` と同一の公開 API）。
pub struct Sleep {
    deadline: Instant,
    token: u64,
    registered: bool,
}

impl Sleep {
    /// 指定した Duration 後に完了する Sleep Future を作成する。
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

// futures::select_biased!（timeout()）で使用するため FusedFuture を実装する。
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
        if self.registered {
            TIMERS.with(|t| t.borrow_mut().cancel(self.token));
        }
    }
}

// ====================
// sleep / timeout API
// ====================

/// 指定した Duration スリープする。
pub fn sleep(duration: Duration) -> Sleep {
    Sleep::new(duration)
}

/// Future にタイムアウトを設定する。
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

/// タイムアウトエラー。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

impl std::fmt::Display for Elapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "deadline has elapsed")
    }
}

impl std::error::Error for Elapsed {}
