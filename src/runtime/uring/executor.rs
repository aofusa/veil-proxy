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
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use crate::runtime::ring::{
    IoUring, IoUringCqe, IORING_OP_ACCEPT, IORING_OP_ASYNC_CANCEL, IORING_OP_CLOSE,
    IORING_OP_CONNECT, IORING_OP_NOP, IORING_OP_POLL_ADD, IORING_OP_POLL_REMOVE, IORING_OP_RECV,
    IORING_OP_RECVMSG, IORING_OP_SEND, IORING_OP_SENDMSG, IORING_OP_SPLICE, IORING_OP_TIMEOUT,
    IORING_SETUP_R_DISABLED,
};

// ====================
// 許可オペコードリスト
// ====================

/// リバースプロキシが使用する io_uring オペコード一覧
///
/// F-59: `IORING_OP_SENDMSG` を追加。ヘッダ + ボディの不連続バッファを 1 SQE / 1 CQE の
/// scatter-gather 送信（ゼロコピー）で送出するために使用する。セキュリティサーフェスの
/// 拡大（restriction 許可リスト +1）は、レスポンス送出ホットパスの syscall/SQE 半減の
/// 利得を優先して許容する（`docs/backlog/features/F-59` 参照）。
///
/// F-124/F-129: HTTP/3 UDP 受信に `IORING_OP_RECVMSG` を追加。
///
/// F-130: `IORING_OP_RECVMSG`（受信）/`IORING_OP_SENDMSG`（送信）を **パイプライン化**して
/// libc `recvmmsg`/`sendmmsg` をホットパスから排除した（`runtime::uring::udp_recv` /
/// `udp_send`）。真の `IORING_RECV_MULTISHOT` + provided buffers（C2、`IORING_OP_PROVIDE_BUFFERS`
/// / `REMOVE_BUFFERS` が必要）は unconnected multi-peer UDP でのアドレス安全性と ENOBUFS 耐性の
/// 課題が残るため見送り、許可オペコードからも外してある（restriction 許可リストを実使用分に
/// 限定 = セキュリティサーフェスの最小化）。将来 C2 に着手する場合はここへ追記すること。
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
    IORING_OP_SENDMSG,
    IORING_OP_RECVMSG,
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

/// ドロップされた in-flight op の後始末ガード（F-46: 確保不要の `Noop` を追加）。
///
/// io_uring に提出済みで未完了の op を持つ Future がドロップされた場合、カーネルはまだ
/// バッファ（accept の addr、read/write のデータ領域）を参照し続けている可能性があるため、
/// 即座に解放すると use-after-free になる。ガードはそれら所有リソースを保持し、op の完了
/// またはキャンセルの CQE 到着時に呼ばれて後始末（バッファ解放、accept で得た fd の
/// クローズ等）を行う。引数は CQE.res。
///
/// カーネルが参照するリソースを持たない op（TIMEOUT 等）は `Noop` を使い、
/// detach ごとのクロージャ確保を避ける。
pub enum OpGuard {
    /// 後始末不要（カーネル参照リソースなし）。
    Noop,
    /// 後始末クロージャ（バッファ・fd 等を保持）。
    Cleanup(Box<dyn FnOnce(i32)>),
}

impl OpGuard {
    /// ガードを実行する（`Noop` は何もしない）。
    #[inline]
    fn run(self, res: i32) {
        if let OpGuard::Cleanup(f) = self {
            f(res);
        }
    }
}

/// ASYNC_CANCEL op 自身の user_data に使うセンチネル。
/// パックすると index = u32::MAX となり、スロット数が u32::MAX に達しない限り
/// 実スロットと衝突せず `on_cqe` で自然に無視される。
const CANCEL_SENTINEL_USER_DATA: u64 = u64::MAX;

// ====================
// OP_TABLE（スラブ、F-46）
// ====================
//
// F-37 までは Fibonacci ハッシュの `HashMap` だったが、`user_data` に
// **スロット index（下位 32bit）+ 世代（上位 32bit）** をパックした純粋な配列インデックスへ
// 置換し、per-op のハッシュ計算・probe を排除する。detach ガードの意味論（B-07 の UAF 対策:
// キャンセル済み op の CQE を世代不一致で無視する）は世代カウンタがそのまま担う。
//
// - `user_data = 0` は無効 ID（世代は 1 始まりのため実スロットと一致しない）。Future の
//   フィールド初期値 `user_data: 0` が誤って実 op を参照する事故を型レベルでなく値レベルで防ぐ。
// - スロット解放時に世代を +1 し（0 は飛ばす）、stale な CQE / Waker / detach を弾く。

/// 事前確保する in-flight op スロット数（典型的な同時 in-flight op 数を見込む）。
const OP_TABLE_PREALLOC: usize = 256;

/// Multishot op の完了キュー（F-124: `IORING_RECV_MULTISHOT`）。
///
/// 同一 user_data に対して複数 CQE が届くため、`(res, flags)` を FIFO で溜め、
/// `F_MORE` が落ちるか負の res で終端するまでスロットを解放しない。
struct MultishotQueue {
    /// 未消費 CQE（res, flags）。
    queue: std::collections::VecDeque<(i32, u32)>,
    /// multishot アームが終了したか（再 arm が必要）。
    finished: bool,
}

/// op スロットの状態。
enum OpSlotState {
    /// 空きスロット（free-list に登録済み）。
    Free,
    /// Future 生存中の op（単発）。
    Active {
        result: OpResult,
        waker: Option<Waker>,
    },
    /// Future 生存中の multishot op（複数 CQE）。
    MultishotActive {
        ms: MultishotQueue,
        waker: Option<Waker>,
    },
    /// Future がドロップされ detach された op（完了/キャンセルの CQE 待ち）。
    Detached(OpGuard),
}

/// op スラブのスロット。
struct OpSlot {
    /// 世代カウンタ（1 始まり。解放ごとに +1、0 は飛ばす）。
    generation: u32,
    state: OpSlotState,
}

/// user_data へ (index, generation) をパックする。
#[inline]
fn pack_op(index: u32, generation: u32) -> u64 {
    ((generation as u64) << 32) | index as u64
}

/// user_data から (index, generation) を取り出す。
#[inline]
fn unpack_op(user_data: u64) -> (u32, u32) {
    (user_data as u32, (user_data >> 32) as u32)
}

/// スレッドローカルな操作テーブル（スラブ + free-list）
struct OpTable {
    slots: Vec<OpSlot>,
    free: Vec<u32>,
}

impl OpTable {
    fn new() -> Self {
        let mut slots = Vec::with_capacity(OP_TABLE_PREALLOC);
        let mut free = Vec::with_capacity(OP_TABLE_PREALLOC);
        for i in 0..OP_TABLE_PREALLOC as u32 {
            slots.push(OpSlot {
                generation: 1,
                state: OpSlotState::Free,
            });
            free.push(i);
        }
        Self { slots, free }
    }

    /// 空きスロット index を確保する（状態は呼び出し側が設定）。
    fn alloc_index(&mut self) -> u32 {
        match self.free.pop() {
            Some(i) => i,
            None => {
                let i = self.slots.len() as u32;
                self.slots.push(OpSlot {
                    generation: 1,
                    state: OpSlotState::Free,
                });
                i
            }
        }
    }

    /// スロットを確保して user_data（index + 世代パック）を返す。
    fn alloc(&mut self) -> u64 {
        let index = self.alloc_index();
        let slot = &mut self.slots[index as usize];
        slot.state = OpSlotState::Active {
            result: OpResult::Pending,
            waker: None,
        };
        pack_op(index, slot.generation)
    }

    /// Multishot 用スロットを確保する（F-124）。
    fn alloc_multishot(&mut self) -> u64 {
        let index = self.alloc_index();
        let slot = &mut self.slots[index as usize];
        slot.state = OpSlotState::MultishotActive {
            ms: MultishotQueue {
                queue: std::collections::VecDeque::with_capacity(64),
                finished: false,
            },
            waker: None,
        };
        pack_op(index, slot.generation)
    }

    /// user_data を検証して現世代のスロット index を返す（不一致 = stale は None）。
    #[inline]
    fn resolve(&self, user_data: u64) -> Option<usize> {
        let (index, generation) = unpack_op(user_data);
        let slot = self.slots.get(index as usize)?;
        if slot.generation != generation {
            return None;
        }
        Some(index as usize)
    }

    /// スロットを解放する（世代 +1 で stale 参照を無効化し free-list へ返す）。
    fn free_slot(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        slot.generation = slot.generation.wrapping_add(1);
        if slot.generation == 0 {
            slot.generation = 1; // 0 は「無効 ID」用に予約
        }
        slot.state = OpSlotState::Free;
        self.free.push(index as u32);
    }

    /// Waker を設定する
    fn set_waker(&mut self, user_data: u64, waker: Waker) {
        if let Some(i) = self.resolve(user_data) {
            match &mut self.slots[i].state {
                OpSlotState::Active { waker: w, .. }
                | OpSlotState::MultishotActive { waker: w, .. } => {
                    *w = Some(waker);
                }
                _ => {}
            }
        }
    }

    /// CQE を処理して対応する Waker を wake する
    fn on_cqe(&mut self, cqe: &IoUringCqe) -> bool {
        use crate::runtime::ring::IORING_CQE_F_MORE;

        let Some(i) = self.resolve(cqe.user_data) else {
            // 未知/stale な user_data（ASYNC_CANCEL 自身の CQE 等）→ 無視。
            return false;
        };
        match &mut self.slots[i].state {
            OpSlotState::Active { result, waker } => {
                *result = OpResult::Done(cqe.res);
                if let Some(w) = waker.take() {
                    w.wake();
                }
                true
            }
            OpSlotState::MultishotActive { ms, waker } => {
                // Multishot: キューへ積み、F_MORE 無し or エラーで終端。スロットは take まで残す。
                ms.queue.push_back((cqe.res, cqe.flags));
                if cqe.res < 0 || (cqe.flags & IORING_CQE_F_MORE) == 0 {
                    ms.finished = true;
                }
                if let Some(w) = waker.take() {
                    w.wake();
                }
                true
            }
            OpSlotState::Detached(_) => {
                // detach 済み op が完了/キャンセルした。ここで初めてバッファ解放・fd クローズを行う。
                // Multishot で F_MORE 付きの中間 CQE が来ても Detached では毎回ガードを走らせない
                // （最終 CQE / cancel で 1 回）。F_MORE 中間はスロットを残す。
                if (cqe.flags & IORING_CQE_F_MORE) != 0 && cqe.res >= 0 {
                    return true;
                }
                let state = std::mem::replace(&mut self.slots[i].state, OpSlotState::Free);
                self.free_slot(i);
                if let OpSlotState::Detached(guard) = state {
                    guard.run(cqe.res);
                }
                true
            }
            OpSlotState::Free => false,
        }
    }

    /// in-flight op を detach する。
    ///
    /// 戻り値が true の場合、呼び出し側は ASYNC_CANCEL を投げてカーネルに早期キャンセルを
    /// 依頼する（accept のように放置すると次の接続を奪う op のため）。
    fn detach(&mut self, user_data: u64, guard: OpGuard) -> bool {
        let Some(i) = self.resolve(user_data) else {
            // 既に take 済み（Future が正常完了して結果を取り出した）等。カーネルはもう
            // バッファを触らないので、ガードは呼ばずに破棄する（accept fd は引き取り済み）。
            return false;
        };
        match &self.slots[i].state {
            OpSlotState::Active {
                result: OpResult::Done(res),
                ..
            } => {
                // 既に完了済み（CQE 到着済みだが take されていない）。即座に後始末。
                let res = *res;
                self.free_slot(i);
                guard.run(res);
                false
            }
            OpSlotState::Active {
                result: OpResult::Pending,
                ..
            } => {
                // 未完了。ガードを保持して完了/キャンセルの CQE を待つ。
                self.slots[i].state = OpSlotState::Detached(guard);
                true
            }
            OpSlotState::MultishotActive { ms, .. } => {
                if ms.finished && ms.queue.is_empty() {
                    // 既に終端済みで未消費なし → 即後始末。
                    self.free_slot(i);
                    guard.run(0);
                    false
                } else {
                    // 進行中 or 未消費あり。キャンセルしてガードで延命。
                    self.slots[i].state = OpSlotState::Detached(guard);
                    true
                }
            }
            _ => false,
        }
    }

    /// 操作の結果を取得し、スロットを解放する
    fn take_result(&mut self, user_data: u64) -> Option<i32> {
        let i = self.resolve(user_data)?;
        if let OpSlotState::Active {
            result: OpResult::Done(res),
            ..
        } = self.slots[i].state
        {
            self.free_slot(i);
            return Some(res);
        }
        None
    }

    /// Multishot CQE を 1 件取り出す（スロットは finished かつ空になるまで残す）。
    ///
    /// 戻り値: `Some((res, flags))` / キュー空で未終端なら `None`（Pending）/
    /// 終端かつ空なら `Some` ではなくスロット解放して呼び出し側へ終端を示すため
    /// `take_multishot` は `(Option<(i32,u32)>, finished)` を返す。
    fn take_multishot(&mut self, user_data: u64) -> (Option<(i32, u32)>, bool) {
        let Some(i) = self.resolve(user_data) else {
            return (None, true);
        };
        match &mut self.slots[i].state {
            OpSlotState::MultishotActive { ms, .. } => {
                let item = ms.queue.pop_front();
                let finished = ms.finished;
                if finished && ms.queue.is_empty() {
                    self.free_slot(i);
                }
                (item, finished)
            }
            _ => (None, true),
        }
    }

    /// 操作の結果を取得する（スロットを解放しない）
    fn peek_result(&self, user_data: u64) -> Option<i32> {
        let i = self.resolve(user_data)?;
        if let OpSlotState::Active {
            result: OpResult::Done(res),
            ..
        } = self.slots[i].state
        {
            return Some(res);
        }
        None
    }

    /// 操作を削除する（結果を待たずスロットを解放する。SQE 提出失敗時等）
    fn remove(&mut self, user_data: u64) {
        if let Some(i) = self.resolve(user_data) {
            self.free_slot(i);
        }
    }

    /// 全スロットが解放済みか（F-84 ファジング・テストの不変条件検査用）。
    fn all_slots_free(&self) -> bool {
        self.free.len() == self.slots.len()
            && self
                .slots
                .iter()
                .all(|s| matches!(s.state, OpSlotState::Free))
    }
}

// ====================
// F-84: 擬似 CQE 注入ファジングドライバ（ホットパス外）
// ====================

/// ファジング駆動用の no-op Waker（wake 経路を通すことのみが目的）。
fn fuzz_noop_waker() -> Waker {
    fn clone(data: *const ()) -> RawWaker {
        RawWaker::new(data, &NOOP_VTABLE)
    }
    fn noop(_data: *const ()) {}
    static NOOP_VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
    // SAFETY: data は未使用の null。全 vtable 関数が副作用なしで契約を満たす。
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &NOOP_VTABLE)) }
}

/// F-84: Fuzzer が生成したバイト列を「op テーブル操作列 + 擬似 CQE」として解釈し、
/// 実リング（カーネル）を介さずに executor の完了ディスパッチ経路を駆動する。
///
/// 異常な `res` 値（負のエラーコード・想定外サイズ）、`user_data` の不整合（偽造・
/// stale 世代・センチネル衝突）、完了順序の逆転・重複到着を大量生成し、次の不変条件を
/// 検査する（違反は assert 失敗 = ファザーがクラッシュとして報告）:
///
/// 1. 任意の操作列で panic しない。
/// 2. detach ガードは **高々 1 回** しか実行されない（2 回 = 二重解放相当）。
/// 3. detach 時点で未完了（pending）だったガードは、遅延 CQE の到着で **ちょうど 1 回**
///    実行される（0 回 = カーネル参照バッファのリーク相当、B-07 の意味論）。
/// 4. 全 CQE 処理後にスロットがリークしない（alloc された op は必ず解放経路に到達する）。
///
/// 実カーネルの SQE 提出・restriction 検証は対象外（F-86 の syscall フォールト注入と
/// F-87 の実リング統合テストが補完する）。
pub(crate) fn fuzz_op_table_sequence(data: &[u8]) {
    /// 操作列の上限（スロット数爆発とファジングの時間超過を防ぐ）。
    const MAX_OPS: usize = 1024;
    /// 同時追跡する op の上限。
    const MAX_TRACKED: usize = 256;

    let mut table = OpTable::new();
    // alloc 済み op の user_data と「detach 済みか」（実コードでは Future ドロップ後に
    // 所有者が take/remove を呼ぶ経路は存在しないため、detach 済み id への remove は
    // 発行しない = 実際の呼び出し契約をモデル化する）。
    let mut tracked: Vec<(u64, bool)> = Vec::new();
    // detach ガードの実行回数カウンタと「detach 時点で pending だったか」。
    let mut guards: Vec<(Rc<std::cell::Cell<u32>>, bool)> = Vec::new();

    let mut cursor = 0usize;
    let mut next = |n: usize| -> Option<&[u8]> {
        let end = cursor.checked_add(n)?;
        let s = data.get(cursor..end)?;
        cursor = end;
        Some(s)
    };

    for _ in 0..MAX_OPS {
        let Some(op) = next(1) else { break };
        match op[0] % 8 {
            // alloc: 新しい op スロットを確保
            0 => {
                if tracked.len() < MAX_TRACKED {
                    tracked.push((table.alloc(), false));
                }
            }
            // 追跡中 id への CQE 配送（res は任意の i32 = 負のエラーコード・巨大サイズ等）
            1 => {
                let Some(b) = next(5) else { break };
                if !tracked.is_empty() {
                    let (ud, _) = tracked[b[0] as usize % tracked.len()];
                    let res = i32::from_le_bytes([b[1], b[2], b[3], b[4]]);
                    table.on_cqe(&IoUringCqe {
                        user_data: ud,
                        res,
                        flags: 0,
                    });
                }
            }
            // 偽造 user_data への CQE 配送（無効 index・stale 世代・センチネル等）
            2 => {
                let Some(b) = next(8) else { break };
                let ud = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
                table.on_cqe(&IoUringCqe {
                    user_data: ud,
                    res: -22, // EINVAL
                    flags: 0,
                });
            }
            // detach（Future ドロップの模擬。ガード実行回数を数える）
            3 => {
                let Some(b) = next(1) else { break };
                if !tracked.is_empty() {
                    let i = b[0] as usize % tracked.len();
                    let (ud, _) = tracked[i];
                    let counter = Rc::new(std::cell::Cell::new(0u32));
                    let c = counter.clone();
                    let was_pending = table.detach(
                        ud,
                        OpGuard::Cleanup(Box::new(move |_res| c.set(c.get() + 1))),
                    );
                    assert!(
                        counter.get() <= 1,
                        "detach guard ran {} times synchronously",
                        counter.get()
                    );
                    guards.push((counter, was_pending));
                    tracked[i].1 = true;
                }
            }
            // take_result（完了結果の取り出し = 正常完了経路）
            4 => {
                let Some(b) = next(1) else { break };
                if !tracked.is_empty() {
                    let (ud, _) = tracked[b[0] as usize % tracked.len()];
                    let _ = table.take_result(ud);
                }
            }
            // peek_result（非破壊参照）
            5 => {
                let Some(b) = next(1) else { break };
                if !tracked.is_empty() {
                    let (ud, _) = tracked[b[0] as usize % tracked.len()];
                    let _ = table.peek_result(ud);
                }
            }
            // remove（SQE 提出失敗の模擬。detach 済み id には所有者がいないため発行しない）
            6 => {
                let Some(b) = next(1) else { break };
                if !tracked.is_empty() {
                    let i = b[0] as usize % tracked.len();
                    let (ud, detached) = tracked[i];
                    if !detached {
                        table.remove(ud);
                    }
                }
            }
            // set_waker（wake 経路の駆動）
            7 => {
                let Some(b) = next(1) else { break };
                if !tracked.is_empty() {
                    let (ud, _) = tracked[b[0] as usize % tracked.len()];
                    table.set_waker(ud, fuzz_noop_waker());
                }
            }
            _ => unreachable!(),
        }
    }

    // 遅延 CQE フラッシュ: カーネルから遅れて到着する完了/キャンセル CQE を全 id へ配送する
    // （解放済み id へは stale として無視されることも同時に検査される）。
    for &(ud, _) in &tracked {
        table.on_cqe(&IoUringCqe {
            user_data: ud,
            res: -125, // ECANCELED
            flags: 0,
        });
    }
    // 未回収の完了結果を取り出してスロットを解放する（正常系の後始末）。
    for &(ud, _) in &tracked {
        let _ = table.take_result(ud);
    }

    // 不変条件 2・3: ガードは高々 1 回、pending detach はちょうど 1 回。
    for (counter, was_pending) in &guards {
        let runs = counter.get();
        assert!(runs <= 1, "detach guard must run at most once, ran {runs}");
        if *was_pending {
            assert_eq!(
                runs, 1,
                "pending detach guard must run exactly once after late CQE (leak otherwise)"
            );
        }
    }

    // 不変条件 4: 全スロット解放済み（リークなし）。
    assert!(
        table.all_slots_free(),
        "op table slot leak: {} slots, {} free",
        table.slots.len(),
        table.free.len()
    );
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

/// 操作スロットを確保し、SQE に設定する user_data を返す（F-46: スラブ）。
///
/// 返り値は「スロット index + 世代」のパック。スレッドローカルのため一意性は
/// スレッド内で足りる（リング `RING` と `OP_TABLE` はともにスレッドローカルで、
/// SQE を提出したスレッドへ CQE が戻る）。`0` は無効 ID（どのスロットとも一致しない）。
#[inline]
pub fn alloc_op() -> u64 {
    OP_TABLE.with(|t| t.borrow_mut().alloc())
}

/// Multishot 用スロットを確保する（F-124 の試作: `IORING_RECV_MULTISHOT`）。
///
/// 同一 user_data に複数 CQE が届き、`take_multishot_cqe` で 1 件ずつ取り出す。
///
/// **現状未使用**（F-130 時点）。F-129/F-130 は真の `IORING_RECV_MULTISHOT` + provided
/// buffers ではなく、独立した複数 `IORING_OP_RECVMSG` を in-flight に保つソフトウェア
/// パイプライン（`runtime::uring::udp_recv::PipelinedUdpRecv`）を採用したため出番がない。
/// 将来 C2（真 multishot + buffer ring）に着手する際の土台として残す。
#[inline]
#[allow(dead_code)]
pub fn alloc_multishot_op() -> u64 {
    OP_TABLE.with(|t| t.borrow_mut().alloc_multishot())
}

/// Multishot CQE を 1 件取り出す。
///
/// 戻り値: `(Some((res, flags)), finished)` / キュー空なら `(None, finished)`。
/// `finished && item.is_none()` のときスロットは解放済みで、呼び出し側は再 arm する。
///
/// **現状未使用**（`alloc_multishot_op` と同じ理由。F-130 C2 用に温存）。
#[inline]
#[allow(dead_code)]
pub fn take_multishot_cqe(user_data: u64) -> (Option<(i32, u32)>, bool) {
    OP_TABLE.with(|t| t.borrow_mut().take_multishot(user_data))
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

/// in-flight op を detach する（ASYNC_CANCEL は投げない）。
///
/// TIMEOUT のように放置しても自然完了し、副作用（接続の横取り等）がない op 用。
/// キャンセル SQE + 即時 submit のシステムコールを節約する（`timeout()` で内側 Future が
/// 勝つたびに発生するホットパス）。スロットは自然完了の CQE 到着時に解放される。
pub fn detach_op_no_cancel(user_data: u64, guard: OpGuard) {
    let _ = OP_TABLE.with(|t| t.borrow_mut().detach(user_data, guard));
}

/// 指定した user_data の in-flight op に ASYNC_CANCEL を投げる（ベストエフォート）。
fn submit_cancel(target_user_data: u64) {
    with_ring(|ring| {
        // B-24: SQ 満杯なら提出してスロットを確保してからキャンセル SQE を積む
        //（ベストエフォート。取得できなくても副作用はない）。
        if let Some(sqe) = ring.get_sqe_or_submit() {
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

/// プールされたタスクの poll フック（F-46）。
///
/// モノモルフィックな [`TaskPool`]`<F>` ごとに 1 つの vtable（`Rc<dyn PoolPoll>`）を共有し、
/// タスクごとの `Box<dyn Future>` 確保を排除する。ディスパッチコストは従来の
/// `dyn Future::poll` と同等（間接呼び出し 1 回）。
pub(crate) trait PoolPoll {
    /// スロットの future を poll する。`Ready` 時はスロットを解放済みで返す。
    fn poll_slot(&self, slot: u32, cx: &mut Context<'_>) -> Poll<()>;
    /// スロットの future を drop して解放する（未完了タスクの破棄時）。
    /// 既に解放済み（`Ready` 返却後）の場合は何もしない。
    fn drop_slot(&self, slot: u32);
}

/// タスク本体（F-46: Box 型消去 or 型付きプールのスロット参照）。
enum TaskBody {
    /// 型消去された Future（汎用 `spawn()`）。
    Boxed(Pin<Box<dyn Future<Output = ()> + 'static>>),
    /// 型付きプールに格納された Future（`TaskPool::spawn()`。spawn ごとのヒープ確保なし）。
    Pooled { pool: Rc<dyn PoolPoll>, slot: u32 },
}

impl Drop for TaskBody {
    fn drop(&mut self) {
        if let TaskBody::Pooled { pool, slot } = self {
            // 未完了のまま破棄された場合に future を解放する。正常完了時は
            // `poll_slot` が解放済みで、`drop_slot` は no-op（二重解放しない）。
            pool.drop_slot(*slot);
        }
    }
}

/// スラブのスロット。poll 時に `body` を take するため `Option` で保持する。
struct TaskSlot {
    /// タスク本体（poll 中は take して None になる）
    body: Option<TaskBody>,
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

    /// タスク本体を新しいスロットへ格納し、ready キューへ積む。
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

            // スロットを検証してタスク本体を take する（poll 中は borrow しない）。
            let taken = EXEC_STATE.with(|s| {
                let mut st = s.borrow_mut();
                match st.slots.get_mut(index) {
                    Some(slot) if slot.generation == generation => {
                        slot.scheduled = false;
                        slot.body.take()
                    }
                    // 世代不一致 = 解放済みの stale エントリ。スキップ。
                    _ => None,
                }
            });
            let mut body = match taken {
                Some(b) => b,
                None => continue,
            };

            // poll（EXEC_STATE 非借用）。
            let waker = make_waker(index, generation);
            let mut cx = Context::from_waker(&waker);
            let poll = match &mut body {
                TaskBody::Boxed(f) => f.as_mut().poll(&mut cx),
                TaskBody::Pooled { pool, slot } => pool.poll_slot(*slot, &mut cx),
            };

            // 結果を反映する。
            EXEC_STATE.with(|s| {
                let mut st = s.borrow_mut();
                let ready_done = match st.slots.get_mut(index) {
                    Some(slot) if slot.generation == generation => match poll {
                        Poll::Pending => {
                            // タスク本体をスロットへ戻す。
                            slot.body = Some(body);
                            false
                        }
                        Poll::Ready(()) => {
                            // スロットを解放: 世代 +1、scheduled クリア。body(ローカル) は drop
                            //（Pooled の場合、pool スロットは poll_slot が解放済み）。
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
    EXEC_STATE.with(|s| s.borrow_mut().spawn_body(TaskBody::Boxed(boxed)));
}

/// 現在のスレッドのエグゼキュータハンドルを取得する。
///
/// 状態はスレッドローカルのため、ハンドル経由でも `spawn()` 経由でも同一の
/// イベントループでポーリングされる。
pub fn current_executor() -> Executor {
    Executor::new()
}

// ====================
// 型付きタスクプール（F-46）
// ====================

/// 1 チャンクあたりのスロット数。
///
/// 接続ハンドラの async fn Future は非常に大きい（全ローカル変数を await 跨ぎで内包する）
/// ため、チャンク粒度を小さめにして未使用スロットの RSS を抑える。
const POOL_CHUNK: usize = 16;

/// 型付きタスクプール（F-46: spawn ごとの `Box<dyn Future>` ヒープ確保を排除）。
///
/// 同一の具象 Future 型 `F`（接続ハンドラ・HTTP/3 バックエンドタスク等、spawn
/// 呼び出しサイトごとに 1 つ定まる `async` ブロック型）をチャンク化スラブに
/// インライン格納する。spawn の定常コストは「スラブスロット再利用 + `Rc` クローン +
/// エグゼキュータスロット再利用」のみで、**ウォームアップ後は malloc ゼロ**になる。
///
/// ## Pin 健全性
///
/// future は `Box<[RefCell<Option<F>>; POOL_CHUNK]>`（ヒープ上の固定長配列）に格納され、
/// チャンク列（`Vec<Box<..>>`）が成長しても **チャンク自体は移動しない**。poll は
/// in-place（take しない）で行い、解放（`Option` を `None` 化して drop）まで一切
/// ムーブしないため、`Pin::new_unchecked` の要件を満たす。
///
/// ## 借用規律（単一スレッド）
///
/// - `poll_slot` はチャンク列の借用を **poll 前に解放**する（poll 中の future が同じ
///   プールへ `spawn` して `chunks` を可変借用しても安全）。
/// - スロットセルの `RefCell` 借用は poll 中保持されるが、同一スロットを再入的に
///   poll する経路は存在しない（エグゼキュータはスロットごとに 1 タスクとして直列に
///   poll する）。万一の再入は `RefCell` が panic で検出する。
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
    /// チャンク列。チャンクは Box によりヒープ上で位置固定（Pin 健全性の要）。
    chunks: RefCell<Vec<Box<[RefCell<Option<F>>]>>>,
    /// 空きスロット index の free-list（LIFO）。
    free: RefCell<Vec<u32>>,
}

impl<F: Future<Output = ()> + 'static> TaskPool<F> {
    /// 空のプールを作成する。`F` は最初の `spawn` 呼び出しから推論される。
    pub fn new() -> Self {
        Self {
            inner: Rc::new(PoolInner {
                chunks: RefCell::new(Vec::new()),
                free: RefCell::new(Vec::new()),
            }),
        }
    }

    /// future をプールのスロットへ格納し、エグゼキュータのタスクとして起動する。
    pub fn spawn(&self, future: F) {
        let slot = {
            let mut free = self.inner.free.borrow_mut();
            match free.pop() {
                Some(s) => s,
                None => {
                    // 新チャンクを確保し、先頭以外を free-list へ積む。
                    // 注意: 巨大な async fn Future ではチャンク（POOL_CHUNK × size_of::<F>()）が
                    // 数 MB になり得るため、スタック経由の配列構築（Box::new([..; N])）は
                    // スタックオーバーフローを起こす。Vec 経由でヒープ上に直接構築する。
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
        // チャンク列の借用を poll 前に解放するため、セルへの生ポインタを取る。
        // SAFETY: チャンクは Box で位置固定・縮小しない。スロットの解放者は本 poll
        // のみ（単一スレッド・エグゼキュータのタスク直列 poll）なので、poll 中に
        // セルが解放・再利用されることはない。
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
                *guard = None; // future を in-place drop
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
        // SAFETY: poll_slot と同じ固定位置保証。
        let cell = unsafe { &*cell };
        let had_future = {
            let mut guard = cell.borrow_mut();
            guard.take().is_some() // in-place drop
        };
        if had_future {
            // 正常完了時（poll_slot が解放済み）は free へ二重 push しない。
            self.free.borrow_mut().push(slot);
        }
    }
}

/// 現在のタスクを一度だけ実行キューの末尾へ譲る（協調的 yield）。
///
/// 長時間 Ready であり続けるループ（例: HTTP/3 メインループがパケットフラッドを
/// 処理し続けるケース）が同一スレッドの他タスク（バックエンド I/O タスク等）を
/// 飢餓させないために使う。`wake_by_ref` で即座に再スケジュールされるため、
/// 他の Ready タスクを 1 巡実行した後に制御が戻る。
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
            st.spawn_body(TaskBody::Boxed(Box::pin(async {})));
            let (idx, gen0) = st.ready.pop_front().unwrap();
            // 解放（poll で Ready 相当）: 世代 +1 して free へ。
            st.slots[idx].body = None;
            st.slots[idx].generation = st.slots[idx].generation.wrapping_add(1);
            st.slots[idx].scheduled = false;
            st.free.push(idx);
            // 古い世代の Waker による schedule は弾かれる。
            st.schedule(idx, gen0);
            assert!(st.ready.is_empty(), "stale generation must be ignored");
            // 再確保すると同じスロットが世代 +1 で払い出される。
            st.spawn_body(TaskBody::Boxed(Box::pin(async {})));
            let (idx2, gen2) = st.ready.pop_front().unwrap();
            assert_eq!(idx2, idx, "freed slot should be reused");
            assert_ne!(gen2, gen0, "generation must advance on reuse");
        });
    }

    /// OP_TABLE 専用ハッシャ（NoHashU64）が u64 キーで衝突なく機能し、HashMap の登録／
    /// 取得／削除が正しく動くことを検証する（SipHash 排除後のリグレッションガード）。
    // ==================== OP_TABLE スラブ（F-46） ====================

    #[test]
    fn op_table_slab_roundtrip() {
        let mut table = OpTable::new();
        let ud = table.alloc();
        assert_ne!(ud, 0, "user_data 0 は無効 ID として予約");
        // 未完了はまだ取り出せない。
        assert_eq!(table.take_result(ud), None);
        assert_eq!(table.peek_result(ud), None);
        let cqe = IoUringCqe {
            user_data: ud,
            res: 42,
            flags: 0,
        };
        assert!(table.on_cqe(&cqe));
        assert_eq!(table.peek_result(ud), Some(42));
        assert_eq!(table.take_result(ud), Some(42));
        // take 後は世代が進み、同じ user_data は無効。
        assert_eq!(table.take_result(ud), None);
        assert!(!table.on_cqe(&cqe), "stale CQE は無視される");
    }

    #[test]
    fn op_table_slab_reuses_slots_with_new_generation() {
        let mut table = OpTable::new();
        let ud1 = table.alloc();
        assert_eq!(table.take_result(ud1), None);
        table.remove(ud1);
        let ud2 = table.alloc();
        // 同じスロットが再利用されるが世代が異なる（旧 ID は無効）。
        assert_eq!(
            unpack_op(ud1).0,
            unpack_op(ud2).0,
            "LIFO free-list で同一スロット"
        );
        assert_ne!(ud1, ud2, "世代が進んで別 ID");
        let stale = IoUringCqe {
            user_data: ud1,
            res: 7,
            flags: 0,
        };
        assert!(
            !table.on_cqe(&stale),
            "旧世代の CQE は新 op に影響しない (B-07)"
        );
        assert_eq!(table.peek_result(ud2), None);
    }

    #[test]
    fn op_table_slab_detach_pending_then_cqe_runs_guard() {
        let mut table = OpTable::new();
        let ud = table.alloc();
        let ran = Rc::new(Cell::new(-1i32));
        let ran2 = ran.clone();
        let should_cancel = table.detach(ud, OpGuard::Cleanup(Box::new(move |res| ran2.set(res))));
        assert!(should_cancel, "未完了 op の detach はキャンセル要求");
        assert_eq!(ran.get(), -1, "ガードは CQE 到着まで実行されない");
        let cqe = IoUringCqe {
            user_data: ud,
            res: -125, // ECANCELED
            flags: 0,
        };
        assert!(table.on_cqe(&cqe));
        assert_eq!(ran.get(), -125, "CQE でガードが実行される");
        // スロットは解放済み → 同 ID は無効。
        assert!(!table.on_cqe(&cqe));
    }

    #[test]
    fn op_table_slab_detach_done_runs_guard_immediately() {
        let mut table = OpTable::new();
        let ud = table.alloc();
        let cqe = IoUringCqe {
            user_data: ud,
            res: 5,
            flags: 0,
        };
        assert!(table.on_cqe(&cqe));
        let ran = Rc::new(Cell::new(-1i32));
        let ran2 = ran.clone();
        let should_cancel = table.detach(ud, OpGuard::Cleanup(Box::new(move |res| ran2.set(res))));
        assert!(!should_cancel, "完了済み op の detach はキャンセル不要");
        assert_eq!(ran.get(), 5, "完了済みなら即座にガード実行");
    }

    #[test]
    fn op_table_slab_detach_after_take_drops_guard() {
        let mut table = OpTable::new();
        let ud = table.alloc();
        let cqe = IoUringCqe {
            user_data: ud,
            res: 1,
            flags: 0,
        };
        table.on_cqe(&cqe);
        assert_eq!(table.take_result(ud), Some(1));
        let ran = Rc::new(Cell::new(false));
        let ran2 = ran.clone();
        let should_cancel = table.detach(ud, OpGuard::Cleanup(Box::new(move |_| ran2.set(true))));
        assert!(!should_cancel);
        assert!(!ran.get(), "take 済み op の detach はガードを実行せず破棄");
    }

    #[test]
    fn op_table_slab_invalid_ids_ignored() {
        let mut table = OpTable::new();
        let _ud = table.alloc();
        // 0（無効 ID）と CANCEL_SENTINEL は常に無視される。
        assert!(!table.on_cqe(&IoUringCqe {
            user_data: 0,
            res: 0,
            flags: 0
        }));
        assert!(!table.on_cqe(&IoUringCqe {
            user_data: CANCEL_SENTINEL_USER_DATA,
            res: 0,
            flags: 0
        }));
        assert_eq!(table.take_result(0), None);
        assert_eq!(table.peek_result(0), None);
    }

    // ==================== 型付きタスクプール（F-46） ====================

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

    #[test]
    fn task_pool_reuses_slots() {
        // 逐次 spawn→完了を繰り返してもチャンクが増えない（スロット再利用）。
        init_executor();
        let exec = current_executor();
        exec.block_on(async move {
            let pool = TaskPool::new();
            for _ in 0..500 {
                let done = Rc::new(Cell::new(false));
                let d = done.clone();
                pool.spawn(async move {
                    yield_once().await;
                    d.set(true);
                });
                while !done.get() {
                    yield_once().await;
                }
            }
            let chunks = pool.inner.chunks.borrow().len();
            assert_eq!(chunks, 1, "逐次実行では 1 チャンクで足りる");
        });
    }

    #[test]
    fn task_pool_grows_chunks_under_concurrency() {
        // 同時 100 タスクは 2 チャンク（128 スロット）に収まる。
        init_executor();
        let exec = current_executor();
        let counter = Rc::new(Cell::new(0usize));
        exec.block_on({
            let counter = counter.clone();
            async move {
                let pool = TaskPool::new();
                for _ in 0..100 {
                    let c = counter.clone();
                    pool.spawn(async move {
                        yield_once().await;
                        yield_once().await;
                        c.set(c.get() + 1);
                    });
                }
                while counter.get() < 100 {
                    yield_once().await;
                }
                let need = 100usize.div_ceil(POOL_CHUNK);
                assert_eq!(pool.inner.chunks.borrow().len(), need);
                assert_eq!(pool.inner.free.borrow().len(), need * POOL_CHUNK);
            }
        });
    }
}
