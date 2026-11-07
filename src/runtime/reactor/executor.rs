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
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::time::{Duration, Instant};

use crate::runtime::handle::RawFd;

use super::poller::{FdTable, Interest};

#[cfg(veil_poller_epoll)]
use super::epoll::{EpollPoller, ERR_HUP, READ, WRITE};

#[cfg(veil_poller_kqueue)]
use super::kqueue::{KqueuePoller, READ, WRITE};

#[cfg(veil_poller_wsapoll)]
use super::wsapoll::{WsaPollPoller, ERR_HUP, READ, WRITE};

// ====================
// reactor ドライバ（poller + fd テーブル）
// ====================

thread_local! {
    #[cfg(veil_poller_epoll)]
    static POLLER: RefCell<Option<EpollPoller>> = const { RefCell::new(None) };
    #[cfg(veil_poller_kqueue)]
    static POLLER: RefCell<Option<KqueuePoller>> = const { RefCell::new(None) };
    #[cfg(veil_poller_wsapoll)]
    static POLLER: RefCell<Option<WsaPollPoller>> = const { RefCell::new(None) };
    static FD_TABLE: RefCell<FdTable> = RefCell::new(FdTable::new());
}

/// fd 表を空にする。新しい poller は古い登録を知らないのに、「武装済み」の記録が残ると `epoll_ctl` を飛ばして
/// 永久に待つ（同じスレッドで `block_on` を繰り返すとき。F-297）。
#[cfg(any(veil_poller_epoll, veil_poller_kqueue, veil_poller_wsapoll))]
fn reset_fd_table() {
    // 残っている Waker は借用の外で落とす
    let old = FD_TABLE.with(|t| std::mem::replace(&mut *t.borrow_mut(), FdTable::new()));
    drop(old);
}

/// このスレッドの poller（epoll インスタンス）を初期化する。
#[cfg(veil_poller_epoll)]
pub fn init_reactor() -> std::io::Result<()> {
    let poller = EpollPoller::new()?;
    POLLER.with(|p| *p.borrow_mut() = Some(poller));
    reset_fd_table();
    Ok(())
}

/// このスレッドの poller（kqueue インスタンス）を初期化する。
#[cfg(veil_poller_kqueue)]
pub fn init_reactor() -> std::io::Result<()> {
    let poller = KqueuePoller::new()?;
    POLLER.with(|p| *p.borrow_mut() = Some(poller));
    reset_fd_table();
    Ok(())
}

/// このスレッドの poller（WSAPoll インスタンス）を初期化する（Windows）。
#[cfg(veil_poller_wsapoll)]
pub fn init_reactor() -> std::io::Result<()> {
    let poller = WsaPollPoller::new()?;
    POLLER.with(|p| *p.borrow_mut() = Some(poller));
    reset_fd_table();
    Ok(())
}

/// このスレッドに reactor ドライバ（poller）が初期化済みか判定する。
///
/// `runtime::offload`（F-29）が、ドライバのあるワーカースレッドでは fd readiness
/// ベースの非同期待機を、ドライバの無いコンテキスト（単体テスト等）では同期インライン
/// 実行をするための分岐に使う（uring 版の `has_ring()` に相当）。
#[cfg(any(veil_poller_epoll, veil_poller_kqueue, veil_poller_wsapoll))]
pub fn has_driver() -> bool {
    POLLER.with(|p| p.borrow().is_some())
}

/// このスレッドの kqueue fd を取得する（F-127 AIO の `SIGEV_KEVENT` 通知先設定に使う）。
/// reactor 未初期化のスレッド（単体テスト等）では `None` を返し、呼び出し側
/// （`reactor::aio`）は readiness フォールバックへ切り替える。
#[cfg(veil_aio)]
pub(crate) fn current_kqueue_fd() -> Option<RawFd> {
    POLLER.with(|p| p.borrow().as_ref().map(|poller| poller.raw_fd()))
}

/// F-141/F-166 A-1: fd の直近の read readiness ヒント（`poller::FdRecord::read_hint`
/// 参照。kqueue はバイト数のスナップショット、epoll は `EPOLL_HINT_SENTINEL`）を
/// **消費**（0 にリセット）しつつ取得する。
///
/// consume-once（take）にする理由: `dispatch_event` が Waker を起こすのと同一
/// スレッド・同一イベントループ周回内で、起こされたタスクが直後に再 poll される
/// （シングルスレッド executor のため、この間に他タスクが同じ fd を読み進めることは
/// ない）。そのため「起こされた直後の 1 回だけヒントを信頼して `poll(2)` の
/// 確認 syscall を省略し、以降は 0 に戻す」ことで、無関係な後続の poll 呼び出しが
/// 古いヒントを誤って読み取って spurious な readiness を報告することを防ぐ
/// （`reactor::tcp::ReadableFd`/`Readable` 参照）。
///
/// ヒントが不正確だった場合（真に readable でなかった場合）も、呼び出し側は
/// 元々 `Poll::Ready` 後に非ブロッキング read/recv を試して `WouldBlock` を
/// 処理できる設計（try-first パターン）のため安全側（誤検知しても実害は
/// 「1 回余分な read syscall」のみで、既存の epoll 版のレベルトリガ相当の
/// spurious wake と同程度）。
///
/// ヒントは「確認の `poll(2)` を省略してよい」ことの十分条件であって必要条件ではない。
/// ヒントを消費した側は必ず直後に非ブロッキング I/O を試し、ヒントが立っていないときは
/// `poll(2)` で確かめる（F-141。epoll は B-75 以降レベルトリガなので、この関数は kqueue 専用）。
///
/// reactor 未初期化のスレッドや、その fd に対する read イベントがまだ一度も
/// 届いていない場合は `0` を返す。
#[cfg(veil_poller_kqueue)]
pub(crate) fn take_read_hint(fd: RawFd) -> usize {
    FD_TABLE
        .try_with(|t| {
            t.borrow_mut()
                .get_mut(fd)
                .map(|r| std::mem::take(&mut r.read_hint))
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// F-155: fd の直近の `EVFILT_WRITE` readiness ヒント（送信可能バイト数の
/// スナップショット、`poller::FdRecord::write_hint` 参照）を **消費**（0 にリセット）
/// しつつ取得する。
///
/// consume-once（take）にする理由は `take_read_hint` と同一（`dispatch_event` が
/// Waker を起こすのと同一スレッド・同一イベントループ周回内で、起こされたタスクが
/// 直後に再 poll されるため、「起こされた直後の 1 回だけヒントを信頼して `poll(2)` の
/// 確認 syscall を省略し、以降は 0 に戻す」ことで、無関係な後続の poll 呼び出しが
/// 古いヒントを誤って読み取って spurious な readiness を報告することを防ぐ）。
///
/// reactor 未初期化のスレッドや、その fd に対する write イベントがまだ一度も
/// 届いていない場合は `0` を返す。
#[cfg(veil_poller_kqueue)]
pub(crate) fn take_write_hint(fd: RawFd) -> usize {
    FD_TABLE
        .try_with(|t| {
            t.borrow_mut()
                .get_mut(fd)
                .map(|r| std::mem::take(&mut r.write_hint))
                .unwrap_or(0)
        })
        .unwrap_or(0)
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

/// fd の読み取り可能待ちを（再）登録する。
#[cfg(any(veil_poller_epoll, veil_poller_kqueue, veil_poller_wsapoll))]
pub(crate) fn register_read(fd: RawFd, waker: Waker) {
    register(fd, Interest::Read, waker);
}

/// fd の書き込み可能待ちを（再）登録する。
#[cfg(any(veil_poller_epoll, veil_poller_kqueue, veil_poller_wsapoll))]
pub(crate) fn register_write(fd: RawFd, waker: Waker) {
    register(fd, Interest::Write, waker);
}

/// fd の interest（READ/WRITE いずれか）を登録する（epoll 版）。
///
/// レベルトリガで、**既にその方向が武装済みなら `epoll_ctl` を呼ばない**（F-297。キープアライブの待ちごとの
/// `EPOLL_CTL_MOD` が 1 要求あたり 1 本だった）。武装の下げは `dispatch_event` が行う。読み取りの方向は
/// 待機者がいる間ずっと武装のままで、レベルが高ければ次の `epoll_wait` が報告するので取りこぼさない。
/// 下げた方向を上げ直すときはカーネルがその時点のレベルも評価する。
#[cfg(veil_poller_epoll)]
fn register(fd: RawFd, interest: Interest, waker: Waker) {
    let change = FD_TABLE.with(|t| {
        let mut t = t.borrow_mut();
        let rec = t.get_or_insert(fd);
        // 同一方向の複数同時待機者を許容する（キューへ追加。`poller::WakerSlot` の doc
        // 参照。offload の共有 eventfd 等、1 fd に複数タスクが同時に読み取り可能待ちを
        // するケースで、先行者の Waker を上書き消失させないために必須）。
        let bit = match interest {
            Interest::Read => {
                rec.read_waker.push(waker);
                READ
            }
            Interest::Write => {
                rec.write_waker.push(waker);
                WRITE
            }
        };
        if rec.known_to_kernel && rec.armed & bit != 0 {
            return None;
        }
        rec.armed |= bit;
        Some((rec.known_to_kernel, rec.armed))
    });
    let Some((known, mask)) = change else {
        return;
    };
    let (res, added) = if known {
        match with_poller(|p| p.modify(fd, mask)) {
            // B-294: 表では「カーネルが知っている」となっているのに、別の経路で close された同じ番号の fd が
            // 再利用されていると MOD は ENOENT になる（epoll は close で登録を外す）。ADD でやり直す
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
                (with_poller(|p| p.add(fd, mask)), true)
            }
            r => (r, false),
        }
    } else {
        match with_poller(|p| p.add(fd, mask)) {
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {
                (with_poller(|p| p.modify(fd, mask)), true)
            }
            r => (r, true),
        }
    };
    match res {
        Ok(()) => {
            if added {
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

/// `ReadableLazy` の待ち（epoll。F-297）。この fd のイベントで起きたのなら `true`（確認なしで読みにいってよい）。
///
/// それ以外は待ちを登録して `false`。`true` のあとの読み取りが `EAGAIN` でも `ReadFuture` が待ち直すので実害は無い
/// （[`ReadableLazy`](super::tcp::ReadableLazy) はそのように使うところだけで使う）。
#[cfg(veil_poller_epoll)]
pub(crate) fn lazy_read_ready(fd: RawFd, waker: &Waker) -> bool {
    use super::poller::LazyRead;
    let fired = FD_TABLE.with(|t| {
        let mut t = t.borrow_mut();
        let rec = t.get_or_insert(fd);
        if rec.lazy_read == LazyRead::Fired {
            rec.lazy_read = LazyRead::None;
            return true;
        }
        rec.lazy_read = LazyRead::Waiting;
        false
    });
    if !fired {
        register(fd, Interest::Read, waker.clone());
    }
    fired
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
                rec.read_waker.push(waker);
                READ
            }
            Interest::Write => {
                rec.write_waker.push(waker);
                WRITE
            }
        };
        rec.armed |= bit;
        (prev, rec.armed)
    });
    // F-141: `update` は changelist へ積むだけで syscall しない（次の `park` の
    // `kevent()` でまとめて反映される）ため、ここでは同期エラーが発生しない。
    // 適用時のエラー（ENOENT 等）は `park` 側で `EV_ERROR` エントリとして観測され、
    // 実害の無いものは黙って無視される（`kqueue::KqueuePoller` 型 doc 参照）。
    with_poller(|p| p.update(fd, new_mask, prev_mask));
}

/// fd の interest（READ/WRITE いずれか）を登録する（WSAPoll 版、Windows）。
///
/// `WSAPoll` はカーネル側に登録状態を持たない（`super::wsapoll` のドキュメント参照）ため、
/// ここでは `FD_TABLE` の armed ビットと Waker キューを更新するのみで、poller への
/// syscall は発行しない。実際の `WSAPoll` 呼び出しは次回の `park()` が `FD_TABLE` から
/// 全 armed エントリを毎回組み立てて行う。
#[cfg(veil_poller_wsapoll)]
fn register(fd: RawFd, interest: Interest, waker: Waker) {
    FD_TABLE.with(|t| {
        let mut t = t.borrow_mut();
        let rec = t.get_or_insert(fd);
        let bit = match interest {
            Interest::Read => {
                rec.read_waker.push(waker);
                READ
            }
            Interest::Write => {
                rec.write_waker.push(waker);
                WRITE
            }
        };
        rec.armed |= bit;
    });
}

/// fd の読み取り待機者 **全員** を起こす（readiness 通知の「横取り」再配布用）。
///
/// 共有 fd（`runtime::offload` のスレッドごと eventfd）では、複数タスクが同一 fd の
/// 読み取り可能を待つ。あるタスクの try-first `poll(2)` チェックがカウンタを drain すると、
/// **他タスク宛ての通知シグナルごと消費**してしまい、epoll のイベントは
/// `epoll_wait` 時点のレベルの評価で蒸発する（drain 済み = 非 readable）。このとき
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
    // A-3: 単一待機者（支配的なケース）なら `WakerSlot::wake_all()` はヒープ操作なしで
    // 直接 wake する（`poller::WakerSlot` の doc 参照）。
    //
    let mut wakers = FD_TABLE.with(|t| {
        let mut t = t.borrow_mut();
        let Some(rec) = t.get_mut(fd) else {
            return super::poller::WakerSlot::Empty;
        };
        // カーネル側の armed ビット（kqueue/WSAPoll のみ持つフィールド。epoll は
        // 保持しない）はそのままにする（counter=0 なら発火しないため無害。
        // 起こされたタスクの再登録で上書きされる）。
        std::mem::take(&mut rec.read_waker)
    });
    wakers.wake_all();
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
    let known = FD_TABLE
        .try_with(|t| {
            t.borrow_mut()
                .remove(fd)
                .is_some_and(|rec| rec.known_to_kernel)
        })
        .unwrap_or(false);
    if known {
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

/// fd の登録を破棄する（WSAPoll 版、Windows）。
///
/// カーネル側に登録状態を持たないため `FD_TABLE` からエントリを除去するのみでよい。
#[cfg(veil_poller_wsapoll)]
pub(crate) fn unregister(fd: RawFd) {
    let _ = FD_TABLE.try_with(|t| t.borrow_mut().remove(fd));
}

/// 新しく作った fd に残っている同じ番号の古い記録を捨てる（F-297）。
///
/// epoll の武装の記録は「カーネルが知っている」前提でシステムコールを省くので、`unregister` を通らずに
/// 閉じられた fd の記録が残っていると、同じ番号の新しい fd の待ちが武装されない。
#[cfg(veil_poller_epoll)]
pub(crate) fn forget_stale_fd(fd: RawFd) {
    let _ = FD_TABLE.try_with(|t| t.borrow_mut().remove(fd));
}

/// poller wait のイベントバッファ最大件数（事前確保しホットパスで再アロケーションしない）。
const EVENT_BATCH: usize = 256;

/// タスクの 1 回の `poll` の中で続けて進められる I/O の回数（B-332）。
///
/// reactor の読み書きは try-first なので、相手がすぐ応える接続のタスクは `Pending` を返さず処理を続け、同じ
/// ワーカーの他の接続を飢えさせる。回数を使い切ったタスクは自分を起こして `Pending` を返す。
const COOP_BUDGET: u32 = 16;

thread_local! {
    static COOP: std::cell::Cell<u32> = const { std::cell::Cell::new(COOP_BUDGET) };
}

/// I/O の協調的な予算を 1 回分使う（B-332）。`false` なら呼び出し側は `Poll::Pending` を返す（再度起こす登録は済み）。
#[inline]
pub(crate) fn coop_proceed(cx: &mut std::task::Context<'_>) -> bool {
    COOP.with(|c| {
        let n = c.get();
        if n == 0 {
            cx.waker().wake_by_ref();
            false
        } else {
            c.set(n - 1);
            true
        }
    })
}

/// ready キューにタスクが残っている間に、待たずにイベントとタイマーを取り込む間隔（タスクの `poll` の回数）。
const EVENT_INTERVAL: u32 = 61;

/// 1 回分の poller wait + イベント/タイマー処理。
#[cfg(veil_poller_epoll)]
fn park(timeout: Option<Duration>) {
    thread_local! {
        static EVENT_BUF: RefCell<Vec<libc::epoll_event>> =
            RefCell::new(vec![unsafe { std::mem::zeroed() }; EVENT_BATCH]);
    }
    let n = EVENT_BUF.with(|buf| {
        let mut buf = buf.borrow_mut();
        match with_poller(|p| p.wait(&mut buf, timeout_ms_ceil(timeout))) {
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
    super::timer::fire_expired(super::timer::refresh_now());
}

/// epoll 版イベント配送（F-297）。
///
/// レベルトリガなので、発火しても武装はカーネルに残る。次の 3 通りだけ武装を下げる。
///
/// - 書き込みの方向: 送信バッファが空いている間は鳴り続けるので、発火したら必ず下げる（待機は `EAGAIN` の後だけ）。
/// - 読み取りの方向で起こす相手がいない: 読まれないデータがある間は鳴り続けるので下げる。次に待つときに上げ直す
///   （カーネルがその時点のレベルも評価するので取りこぼさない）。
/// - 全方向が下りたら監視対象から外す（`EPOLLHUP`・`EPOLLERR` はマスクできない）。
#[cfg(veil_poller_epoll)]
fn dispatch_event(fd: RawFd, flags: u32) {
    use super::poller::{LazyRead, WakerSlot};
    // ヒント設定と Waker の取り出しは FD_TABLE を借用したまま行うが、実際に
    // `wake()` するのは借用を外した後にする（Waker::wake() が再帰的に
    // register()/FD_TABLE を触るタスクを起こし得るため、二重借用パニックを避ける）。
    // `mem::take`（`WakerSlot::default()` = `Empty`）で取り出すのは単一待機者
    // ならヒープ操作なしの `WakerSlot::One`（ムーブのみ）で、`Vec` を経由しない。
    let (mut read_waker, mut write_waker, change) = FD_TABLE.with(|t| {
        let mut t = t.borrow_mut();
        let Some(rec) = t.get_mut(fd) else {
            // 記録が無い fd がレベルで鳴り続けないようにする（`unregister` は記録と登録を一緒に消すので通常は来ない）
            return (WakerSlot::Empty, WakerSlot::Empty, Some(0));
        };
        let hup = flags & ERR_HUP != 0;
        let mut lower = 0;
        let rw = if flags & READ != 0 || hup {
            if rec.read_waker.is_empty() {
                lower |= READ;
            } else if rec.lazy_read == LazyRead::Waiting {
                rec.lazy_read = LazyRead::Fired;
            }
            std::mem::take(&mut rec.read_waker)
        } else {
            WakerSlot::Empty
        };
        let ww = if flags & WRITE != 0 || hup {
            lower |= WRITE;
            std::mem::take(&mut rec.write_waker)
        } else {
            WakerSlot::Empty
        };
        let armed = rec.armed & !lower;
        let change = (armed != rec.armed || armed == 0).then_some(armed);
        rec.armed = armed;
        if armed == 0 {
            rec.known_to_kernel = false;
        }
        (rw, ww, change)
    });
    read_waker.wake_all();
    write_waker.wake_all();
    match change {
        None => {}
        Some(0) => with_poller(|p| p.delete(fd)),
        Some(mask) => {
            let _ = with_poller(|p| p.modify(fd, mask));
        }
    }
}

/// kqueue バージョンの 1 回分の poller wait + イベント/タイマー処理。
#[cfg(veil_poller_kqueue)]
fn park(timeout: Option<Duration>) {
    thread_local! {
        static EVENT_BUF: RefCell<Vec<libc::kevent>> =
            RefCell::new(vec![unsafe { std::mem::zeroed() }; EVENT_BATCH]);
    }
    let n = EVENT_BUF.with(|buf| {
        let mut buf = buf.borrow_mut();
        match with_poller(|p| p.wait(&mut buf, timeout)) {
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
                // F-127: AIO 完了通知（`SIGEV_KEVENT`）は `EVFILT_AIO` として届く。
                // `ident` は aiocb ポインタ（本実装では未使用）、`udata` に
                // `aio_sigevent.sigev_value` で埋め込んだ op token が入る
                // （`reactor::aio` 参照）。`veil_aio` 未設定時はこの分岐自体が
                // 存在せず、既存の READ/WRITE 分岐のみがコンパイルされる。
                #[cfg(veil_aio)]
                if ev.filter == libc::EVFILT_AIO {
                    super::aio::handle_completion(ev.udata as u64);
                    continue;
                }
                // F-141: changelist バッチ化により、適用に失敗した change
                // （例: 既に close 済みの fd への `EV_DELETE` が ENOENT になる）は
                // 通常のイベントではなく `EV_ERROR` フラグ付きの kevent として
                // eventlist に混ざって返る（`kqueue::KqueuePoller::wait` の doc 参照）。
                // 実際の read/write readiness ではないため読み飛ばす。
                if (ev.flags as u32) & (libc::EV_ERROR as u32) != 0 {
                    continue;
                }
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
                // F-141: EVFILT_READ の `data`（読み取り可能バイト数のカーネル観測値）を
                // fd ごとのヒントとして保持する（`poller::FdRecord::read_hint` 参照）。
                // `reactor::tcp::Readable`/`ReadableFd`（UDP の `wait_readable_fd` を含む）が
                // 「起こされた直後は確認用 poll(2) を省略してよい」判定に使う
                // （`executor::take_read_hint` 参照）。ヒントを読まない経路には副作用が無い。
                let data_hint = ev.data;
                dispatch_event(fd, bit, data_hint);
            }
        });
    }
    super::timer::fire_expired(super::timer::refresh_now());
}

#[cfg(veil_poller_kqueue)]
fn dispatch_event(fd: RawFd, flags: u32, data_hint: impl TryInto<i64>) {
    // kqueue は EV_ONESHOT 発火時にカーネル側フィルタを自動削除するため、epoll のように
    // 「片方だけ起きたらもう片方を再武装する」再武装処理（epoll_ctl(MOD)）は不要。
    // 起きた方向のビットを armed から落とすだけでよい（次回 register 時に改めて
    // EV_ADD|EV_ONESHOT される）。
    let data_hint = data_hint.try_into().unwrap_or(0).max(0) as usize;
    // A-3: `mem::take` は `WakerSlot::default()`（= `Empty`）と交換するだけで、単一
    // 待機者なら `WakerSlot::One` のムーブのみ（ヒープ操作なし）。`wake_all()` は
    // FD_TABLE の借用を外した後に呼ぶ（`epoll::dispatch_event` と同じ理由）。
    let (mut read_waker, mut write_waker) = FD_TABLE.with(|t| {
        let mut t = t.borrow_mut();
        let Some(rec) = t.get_mut(fd) else {
            return (
                super::poller::WakerSlot::Empty,
                super::poller::WakerSlot::Empty,
            );
        };
        if flags & READ != 0 {
            rec.read_hint = data_hint;
        }
        // F-155: read_hint と対称に、waker が空でも（futures 側が readiness を
        // まだ待っていない場合でも）ヒントだけは保存する。次回 `Writable::poll` 等が
        // 確認用 poll(2) を省略できるようにするため。
        if flags & WRITE != 0 {
            rec.write_hint = data_hint;
        }
        let rw = if flags & READ != 0 {
            rec.armed &= !READ;
            std::mem::take(&mut rec.read_waker)
        } else {
            super::poller::WakerSlot::Empty
        };
        let ww = if flags & WRITE != 0 {
            rec.armed &= !WRITE;
            std::mem::take(&mut rec.write_waker)
        } else {
            super::poller::WakerSlot::Empty
        };
        (rw, ww)
    });
    read_waker.wake_all();
    write_waker.wake_all();
}

#[cfg(veil_poller_wsapoll)]
fn with_poller<R>(f: impl FnOnce(&WsaPollPoller) -> R) -> R {
    POLLER.with(|p| {
        let b = p.borrow();
        let poller = b
            .as_ref()
            .expect("reactor poller not initialized for this thread");
        f(poller)
    })
}

/// WSAPoll バージョンの 1 回分の poller wait + イベント/タイマー処理（Windows）。
///
/// `FD_TABLE` から現在 armed な全エントリを毎回列挙して `WSAPoll` へ渡す
/// （`super::wsapoll` のドキュメント参照。カーネル側に登録状態を持たないため
/// epoll/kqueue のような差分登録ができない）。
#[cfg(veil_poller_wsapoll)]
fn park(timeout: Option<Duration>) {
    thread_local! {
        static EVENT_BUF: RefCell<Vec<(RawFd, u32)>> = RefCell::new(Vec::with_capacity(EVENT_BATCH));
    }
    let entries = FD_TABLE.with(|t| t.borrow().armed_entries());
    let n = EVENT_BUF.with(|buf| {
        let mut buf = buf.borrow_mut();
        match with_poller(|p| p.wait(&entries, &mut buf, timeout_ms_ceil(timeout))) {
            Ok(n) => n,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::Interrupted {
                    ftlog::error!("reactor: WSAPoll error: {}", e);
                }
                0
            }
        }
    });
    if n > 0 {
        EVENT_BUF.with(|buf| {
            let buf = buf.borrow();
            for (fd, bits) in buf.iter() {
                dispatch_event(*fd, *bits);
            }
        });
    }
    super::timer::fire_expired(super::timer::refresh_now());
}

#[cfg(veil_poller_wsapoll)]
fn dispatch_event(fd: RawFd, flags: u32) {
    // WSAPoll はレベルトリガのため、次回 park() で armed なエントリのみ再度渡す形で
    // oneshot 相当を表現する。ここでは発火した方向の armed ビットを落とし、対応する
    // Waker を起こすのみでよい（次回の待機は register() による再武装で行われる）。
    let (mut read_waker, mut write_waker) = FD_TABLE.with(|t| {
        let mut t = t.borrow_mut();
        let Some(rec) = t.get_mut(fd) else {
            return (
                super::poller::WakerSlot::Empty,
                super::poller::WakerSlot::Empty,
            );
        };
        let rw = if flags & (READ | ERR_HUP) != 0 && !rec.read_waker.is_empty() {
            rec.armed &= !READ;
            std::mem::take(&mut rec.read_waker)
        } else {
            super::poller::WakerSlot::Empty
        };
        let ww = if flags & (WRITE | ERR_HUP) != 0 && !rec.write_waker.is_empty() {
            rec.armed &= !WRITE;
            std::mem::take(&mut rec.write_waker)
        } else {
            super::poller::WakerSlot::Empty
        };
        (rw, ww)
    });
    read_waker.wake_all();
    write_waker.wake_all();
}

/// poller wait のタイムアウト（ミリ秒）を最近接タイマーデッドラインから計算する。
/// タイマーが無ければ無期限待機（-1）。
/// 次のタイマーまでの時間（`None` は無期限）。期限を過ぎていれば 0。
fn next_timeout() -> Option<Duration> {
    super::timer::next_deadline().map(|deadline| deadline.saturating_duration_since(Instant::now()))
}

/// ミリ秒の単位の poller（epoll・WSAPoll）へ渡す値。**切り上げる**: 以前は `as_millis` で切り捨てていたため、
/// 1ms 未満の先のタイマーが 0（待たない）になり、期限まで poller を空回りさせていた（FreeBSD の HTTP/3 で
/// 空の kevent が 1 スレッドで 50 万回/秒。v0.8.0。kqueue はナノ秒で渡す）。
#[cfg(any(veil_poller_epoll, veil_poller_wsapoll))]
fn timeout_ms_ceil(timeout: Option<Duration>) -> i32 {
    match timeout {
        None => -1,
        Some(d) if d.is_zero() => 0,
        Some(d) => {
            let ms = d.as_nanos().div_ceil(1_000_000);
            ms.min(i32::MAX as u128) as i32
        }
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

    /// タスク本体を新しいスロットへ格納するが、ready キューへは積まない（A-1'）。
    ///
    /// `spawn_body` と同じスロット確保規律（free-list 優先、無ければ push）を使うが、
    /// 呼び出し側（`spawn_body_and_poll`）がこの直後に自前で 1 回 poll するため、
    /// 通常の実行キュー経由の起動とは別経路を取る。戻り値は確保したスロットの
    /// (index, generation)。uring 版 `runtime::uring::executor::ExecutorState::reserve_slot`
    /// と等価な実装。
    fn reserve_slot(&mut self, body: TaskBody) -> (usize, u32) {
        let index = if let Some(i) = self.free.pop() {
            let slot = &mut self.slots[i];
            slot.body = Some(body);
            slot.scheduled = false;
            i
        } else {
            let i = self.slots.len();
            self.slots.push(TaskSlot {
                body: Some(body),
                generation: 0,
                scheduled: false,
            });
            i
        };
        (index, self.slots[index].generation)
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

    /// ready キューの今の長さの分だけタスクを進める（途中で起きたタスクは次の周回。B-332: ずっと ready のタスクが
    /// いても poller へ戻れるように）。実行した回数と、キューが残っているかを返す。
    fn run_ready_tasks(&self) -> (u32, bool) {
        let batch = EXEC_STATE.with(|s| s.borrow().ready.len());
        let mut ran = 0;
        for _ in 0..batch {
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
            COOP.with(|c| c.set(COOP_BUDGET));
            ran += 1;
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
        (ran, EXEC_STATE.with(|s| !s.borrow().ready.is_empty()))
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

        let mut since_park = 0u32;
        loop {
            let (ran, more) = self.run_ready_tasks();

            if result.borrow().is_some() {
                break;
            }

            // タスクが残っているときは待たない。残っている間もときどきイベントとタイマーを見る（B-332）
            since_park = since_park.saturating_add(ran);
            if more && since_park < EVENT_INTERVAL {
                continue;
            }
            since_park = 0;
            let timeout = if more {
                Some(Duration::ZERO)
            } else {
                next_timeout()
            };
            #[cfg(any(veil_poller_epoll, veil_poller_kqueue, veil_poller_wsapoll))]
            park(timeout);
            #[cfg(not(any(veil_poller_epoll, veil_poller_kqueue, veil_poller_wsapoll)))]
            {
                // veil_rt_reactor は必ずどちらかの poller cfg を伴う（build.rs）ため、
                // ここへ到達することは cfg 上あり得ない。
                let _ = timeout;
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

/// B-118: スレッドを終える前に、残っているタスクを破棄する（ワーカーの `block_on` の後に呼ぶ）。
///
/// スレッドローカルの破棄（`EXEC_STATE` のデストラクタ）の中でタスクを drop すると、タスク内の
/// I/O Future の Drop が破棄済みのスレッドローカル（poller・fd 表）に触れ得るので、生きているうちに
/// drop する。readiness 型なのでカーネル側に完了待ちの操作は残らない（`drain` は io_uring 版用）。
pub fn shutdown_tasks(_drain: std::time::Duration) {
    loop {
        let bodies: Vec<TaskBody> = EXEC_STATE.with(|s| {
            let mut st = s.borrow_mut();
            st.ready.clear();
            st.slots
                .iter_mut()
                .filter_map(|slot| slot.body.take())
                .collect()
        });
        if bodies.is_empty() {
            break;
        }
        drop(bodies);
    }
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

/// タスクをスラブへ登録し、その場でタスク自身の実 Waker を使って 1 回だけ poll する（A-1'）。
///
/// uring 版 `runtime::uring::executor::spawn_body_and_poll` と等価な実装
/// （AGENTS.md の「reactor 追加でも uring 生成コードを等価に保つ」方針に沿い、
/// reactor 側も対称的に実装する）。健全性の根拠（Waker・借用規律）は
/// uring 版の doc コメントを参照。
///
/// `Poll::Ready` まで進めば `true`（ready キューに一度も積まれない = エグゼキュータ往復・
/// fd readiness の確認 poll(2)・Notify 起床が消える）、`Poll::Pending` なら `false`
/// （通常の `spawn()` と全く同じ状態でエグゼキュータに残る）を返す。
fn spawn_body_and_poll(body: TaskBody) -> bool {
    // スロットを確保する（ready キューへは積まない）。
    let (index, generation) = EXEC_STATE.with(|s| s.borrow_mut().reserve_slot(body));

    // poll 対象の body を取り出す（EXEC_STATE 借用外で poll するため）。
    let mut body = EXEC_STATE
        .with(|s| s.borrow_mut().slots[index].body.take())
        .expect("slot has no body right after reserve_slot");

    let waker = make_waker(index, generation);
    let mut cx = Context::from_waker(&waker);
    let poll = match &mut body {
        TaskBody::Boxed(f) => f.as_mut().poll(&mut cx),
        TaskBody::Pooled { pool, slot } => pool.poll_slot(*slot, &mut cx),
    };

    EXEC_STATE.with(|s| {
        let mut st = s.borrow_mut();
        match st.slots.get_mut(index) {
            Some(slot) if slot.generation == generation => match poll {
                Poll::Pending => {
                    slot.body = Some(body);
                    false
                }
                Poll::Ready(()) => {
                    slot.generation = slot.generation.wrapping_add(1);
                    slot.scheduled = false;
                    st.free.push(index);
                    true
                }
            },
            // 単一スレッド・直列 poll のため通常起き得ない（run_ready_tasks と同じ前提）。
            _ => true,
        }
    })
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

    /// 空きスロットを 1 つ取り出す（足りなければチャンクを足す）。
    fn alloc_slot(&self) -> u32 {
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
    }

    /// future を空きスロットへ格納し、スロット index を返す（`spawn`/`spawn_inline` 共有）。
    fn store(&self, future: F) -> u32 {
        self.store_with(move || future)
    }

    /// `make` が作る future を空きスロットへ直接書き込み、スロット index を返す。
    ///
    /// 要求ごとの future は数 KB〜十数 KB あり（状態機械の大きさは一番大きい分岐で決まる）、値で
    /// 受け渡すと `memcpy` が要求ごとに何回も走る（FreeBSD の h2c 3B でユーザ空間の CPU の 28%。B-240）。
    /// 作る関数を呼び出しとして残し、戻り値の領域をスロットそのものにする。
    fn store_with(&self, make: impl FnOnce() -> F) -> u32 {
        let slot = self.alloc_slot();
        {
            let chunks = self.inner.chunks.borrow();
            let cell = &chunks[slot as usize / POOL_CHUNK][slot as usize % POOL_CHUNK];
            write_empty_slot(&mut cell.borrow_mut(), make);
        }
        slot
    }

    pub fn spawn(&self, future: F) {
        let slot = self.store(future);
        let pool: Rc<dyn PoolPoll> = self.inner.clone();
        EXEC_STATE.with(|s| s.borrow_mut().spawn_body(TaskBody::Pooled { pool, slot }));
    }

    /// future をプールのスロットへ格納し、その場で 1 回だけ poll する（A-1'）。
    /// uring 版 `TaskPool::spawn_inline` と等価。健全性の根拠はそちらの doc コメント参照。
    pub fn spawn_inline(&self, future: F) -> bool {
        let slot = self.store(future);
        let pool: Rc<dyn PoolPoll> = self.inner.clone();
        spawn_body_and_poll(TaskBody::Pooled { pool, slot })
    }

    /// [`spawn_inline`](Self::spawn_inline) の、future をスロットの中で作る版（B-240）。
    pub fn spawn_inline_with(&self, make: impl FnOnce() -> F) -> bool {
        let slot = self.store_with(make);
        let pool: Rc<dyn PoolPoll> = self.inner.clone();
        spawn_body_and_poll(TaskBody::Pooled { pool, slot })
    }
}

/// 空きスロット（常に `None`）へ `make()` の結果を書く。
///
/// `construct` を呼び出しのまま残すと、その戻り値の一時領域から `dst` への `memcpy` を LLVM が消して
/// `dst` へ直接作らせる（call slot optimization）。`dst` は `&mut` の引数なので書ける大きさと
/// 別名が無いことが分かる。古い値の drop を挟むと消えないので `ptr::write` で書く。
#[inline(never)]
fn write_empty_slot<F, M: FnOnce() -> F>(dst: &mut Option<F>, make: M) {
    debug_assert!(dst.is_none(), "pool slot reused before release");
    // SAFETY: 空きスロットは解放時（`poll_slot`・`drop_slot`）と作成時に `None` にしてあり、
    // 上書きで drop し損なう値は無い。`dst` は有効な `&mut`。
    unsafe { std::ptr::write(dst, Some(construct(make))) }
}

#[inline(never)]
fn construct<F, M: FnOnce() -> F>(make: M) -> F {
    make()
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
        init_reactor().unwrap();
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
        init_reactor().unwrap();
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
