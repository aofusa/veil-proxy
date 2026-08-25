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
use std::time::Instant;

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

/// このスレッドの poller（WSAPoll インスタンス）を初期化する（Windows）。
#[cfg(veil_poller_wsapoll)]
pub fn init_reactor() -> std::io::Result<()> {
    let poller = WsaPollPoller::new()?;
    POLLER.with(|p| *p.borrow_mut() = Some(poller));
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

/// F-166 A-1: epoll バックエンドで `EPOLLIN`/`EPOLLERR`/`EPOLLHUP` 発火を示す非ゼロ番兵値。
/// epoll には kqueue の `data`（読み取り/書き込み可能バイト数）に相当するフィールドが
/// 無いため、バイト数の代わりに「非ゼロ = 直前の起床でこの方向のイベントを観測した」
/// という真偽相当の意味でこの値を `read_hint`/`write_hint` へ格納する
/// （`poller::FdRecord` の doc 参照）。
#[cfg(veil_poller_epoll)]
pub(crate) const EPOLL_HINT_SENTINEL: usize = usize::MAX;

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
/// **epoll の `EPOLLET`（F-166 A-2）とヒントの関係**: ET はエッジ検出のみを担い、
/// 「readable かどうか」の意味論は本ヒント + `poll(2)` フォールバックが担う。
/// ヒントを消費した側は必ず直後に非ブロッキング I/O を試すため、たとえエッジを
/// 取りこぼしても「データは残っているが `read_hint` が立っていない」状態は
/// 次の `poll(2)` フォールバックで検出できる（ヒントは「省略してよい」ことの
/// 十分条件であって必要条件ではない）。
///
/// reactor 未初期化のスレッドや、その fd に対する read イベントがまだ一度も
/// 届いていない場合は `0` を返す。
#[cfg(any(veil_poller_kqueue, veil_poller_epoll))]
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
#[cfg(any(veil_poller_kqueue, veil_poller_epoll))]
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

/// fd の読み取り可能待ちを（再）登録する（oneshot）。
#[cfg(any(veil_poller_epoll, veil_poller_kqueue, veil_poller_wsapoll))]
pub(crate) fn register_read(fd: RawFd, waker: Waker) {
    register(fd, Interest::Read, waker);
}

/// fd の書き込み可能待ちを（再）登録する（oneshot）。
#[cfg(any(veil_poller_epoll, veil_poller_kqueue, veil_poller_wsapoll))]
pub(crate) fn register_write(fd: RawFd, waker: Waker) {
    register(fd, Interest::Write, waker);
}

/// fd の interest（READ/WRITE いずれか）を登録する（epoll 版、F-166 A-2）。
///
/// **fd あたり `epoll_ctl` は生涯 1 回だけ**（初回登録時の `EPOLL_CTL_ADD`）。
/// `EPOLLONESHOT` + 毎回 `EPOLL_CTL_MOD` だった旧実装をやめ、`EPOLLIN|EPOLLOUT|
/// EPOLLRDHUP|EPOLLET`（エッジトリガ、読み書き両方向を常時）で 1 回 ADD したら、
/// 以降の `register()` 呼び出しは **syscall を一切発行せず** `FD_TABLE` へ Waker を
/// 積むだけになる（`known_to_kernel` が唯一の ADD/MOD ならぬ「ADD 済みか否か」判定軸）。
///
/// 読み書き両方向を常時 armed にすることの安全性: 待機者がいない方向の
/// `EPOLLOUT`/`EPOLLIN` 起床は `dispatch_event` が Waker を探しても見つからず
/// 何もしない（`WakerSlot::is_empty()` なら wake 対象なしで即終了）ため実害が無い
/// （F-166 詳細設計の「正しさの根拠」4 参照）。
///
/// ET のエッジ取りこぼしを起こさない理由は `take_read_hint`/`take_write_hint` の
/// doc と `poller::FdRecord` の doc を参照（ヒント consume-once + `poll(2)`
/// フォールバックの組で塞ぐ）。
#[cfg(veil_poller_epoll)]
fn register(fd: RawFd, interest: Interest, waker: Waker) {
    let needs_add = FD_TABLE.with(|t| {
        let mut t = t.borrow_mut();
        let rec = t.get_or_insert(fd);
        // 同一方向の複数同時待機者を許容する（キューへ追加。`poller::WakerSlot` の doc
        // 参照。offload の共有 eventfd 等、1 fd に複数タスクが同時に読み取り可能待ちを
        // するケースで、先行者の Waker を上書き消失させないために必須）。
        match interest {
            Interest::Read => rec.read_waker.push(waker),
            Interest::Write => rec.write_waker.push(waker),
        }
        !rec.known_to_kernel
    });
    if !needs_add {
        // 既にカーネルへ ADD 済み（ET・読み書き両方向を常時 armed）のため、
        // ここでの epoll_ctl は不要（syscall ゼロ）。
        return;
    }
    // EPOLLRDHUP: 相手が半クローズ（shutdown(SHUT_WR) 相当）したことを検出するため、
    // 従来から要求していたビットをそのまま引き継ぐ（`ERR_HUP` を要求しなくても
    // EPOLLERR/EPOLLHUP は常に配送されるが、EPOLLRDHUP は明示要求が必要）。
    let mask = READ | WRITE | libc::EPOLLRDHUP as u32;
    match with_poller(|p| p.add(fd, mask)) {
        Ok(()) => {
            FD_TABLE.with(|t| {
                if let Some(rec) = t.borrow_mut().get_mut(fd) {
                    rec.known_to_kernel = true;
                }
            });
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
    // A-3: 単一待機者（支配的なケース）なら `WakerSlot::wake_all()` はヒープ操作なしで
    // 直接 wake する（`poller::WakerSlot` の doc 参照）。
    //
    // ET（F-166 A-2、epoll のみ）でもこの経路は安全: eventfd への次の `write(2)` が
    // 新しいエッジを生成するため、ここで一旦全読者を起こして再登録させても
    // 取りこぼしは発生しない（`offload.rs` の `OffloadWait::poll` が try-first で
    // 自前の `poll(2)` を行うため、`read_hint`/ET の armed 状態に依存しない）。
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

/// fd の登録を破棄する（WSAPoll 版、Windows）。
///
/// カーネル側に登録状態を持たないため `FD_TABLE` からエントリを除去するのみでよい。
#[cfg(veil_poller_wsapoll)]
pub(crate) fn unregister(fd: RawFd) {
    let _ = FD_TABLE.try_with(|t| t.borrow_mut().remove(fd));
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

/// epoll 版イベント配送（F-166 A-1/A-2/A-3）。
///
/// A-2（ET 常時登録）により、ここでは **epoll_ctl を一切呼ばない**（旧実装の
/// 「片方だけ起きたらもう片方を再武装する」`modify` 呼び出しは、fd が生涯 ADD
/// されたまま・両方向常時 armed のため不要になった）。
///
/// A-1: 起床させる Waker の有無に関わらず、観測したイベント方向の `read_hint`/
/// `write_hint` へ非ゼロ番兵値（`EPOLL_HINT_SENTINEL`）を立てる（`take_read_hint`/
/// `take_write_hint` の doc 参照）。待機者がいない方向にヒントだけ立てても、
/// 対応する `Readable`/`Writable` 系 Future が次に poll されたときに consume-once で
/// 読み取って `poll(2)` を省略するだけで実害は無い（詳細設計の「正しさの根拠」4）。
#[cfg(veil_poller_epoll)]
fn dispatch_event(fd: RawFd, flags: u32) {
    // ヒント設定と Waker の取り出しは FD_TABLE を借用したまま行うが、実際に
    // `wake()` するのは借用を外した後にする（Waker::wake() が再帰的に
    // register()/FD_TABLE を触るタスクを起こし得るため、二重借用パニックを避ける）。
    // A-3: `mem::take`（`WakerSlot::default()` = `Empty`）で取り出すのは単一待機者
    // ならヒープ操作なしの `WakerSlot::One`（ムーブのみ）で、`Vec` を経由しない。
    let (mut read_waker, mut write_waker) = FD_TABLE.with(|t| {
        let mut t = t.borrow_mut();
        let Some(rec) = t.get_mut(fd) else {
            return (
                super::poller::WakerSlot::Empty,
                super::poller::WakerSlot::Empty,
            );
        };
        // A-1: 起床させる Waker の有無に関わらず、観測したイベント方向のヒントを
        // 立てる（`take_read_hint`/`take_write_hint` の doc 参照。待機者がいない
        // 方向にヒントだけ立てても実害は無い＝詳細設計の「正しさの根拠」4）。
        if flags & (READ | ERR_HUP) != 0 {
            rec.read_hint = EPOLL_HINT_SENTINEL;
        }
        if flags & (WRITE | ERR_HUP) != 0 {
            rec.write_hint = EPOLL_HINT_SENTINEL;
        }
        let rw = if flags & (READ | ERR_HUP) != 0 {
            std::mem::take(&mut rec.read_waker)
        } else {
            super::poller::WakerSlot::Empty
        };
        let ww = if flags & (WRITE | ERR_HUP) != 0 {
            std::mem::take(&mut rec.write_waker)
        } else {
            super::poller::WakerSlot::Empty
        };
        (rw, ww)
    });
    // A-2: ET 常時登録のため、ここでの再武装（旧: 片方だけ起きたらもう片方を
    // 再武装する `epoll_ctl(MOD)`）は不要。fd は生涯 ADD 済みのまま・両方向常時
    // armed であり、epoll_ctl は一切呼ばない。
    read_waker.wake_all();
    write_waker.wake_all();
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
    super::timer::fire_expired(Instant::now());
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
fn park(timeout_ms: i32) {
    thread_local! {
        static EVENT_BUF: RefCell<Vec<(RawFd, u32)>> = RefCell::new(Vec::with_capacity(EVENT_BATCH));
    }
    let entries = FD_TABLE.with(|t| t.borrow().armed_entries());
    let n = EVENT_BUF.with(|buf| {
        let mut buf = buf.borrow_mut();
        match with_poller(|p| p.wait(&entries, &mut buf, timeout_ms)) {
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
    super::timer::fire_expired(Instant::now());
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
            #[cfg(any(veil_poller_epoll, veil_poller_kqueue, veil_poller_wsapoll))]
            park(timeout_ms);
            #[cfg(not(any(veil_poller_epoll, veil_poller_kqueue, veil_poller_wsapoll)))]
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
        .expect("reserve_slot 直後のスロットに body が無い");

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

    /// future を空きスロットへ格納し、スロット index を返す（`spawn`/`spawn_inline` 共有）。
    fn store(&self, future: F) -> u32 {
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
