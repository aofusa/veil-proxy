//! poller 共通のイベント表現・fd ごとの登録テーブル
//!
//! epoll（`veil_poller_epoll`）/ kqueue（`veil_poller_kqueue`、Phase 4）のどちらでも
//! 共有するデータ構造。poller 実装自体（syscall 発行）は `reactor::epoll` /
//! `reactor::kqueue` に閉じ込め、本モジュールは「fd → 待機中 Waker」の対応管理のみを担う。
//!
//! `Vec<Option<FdRecord>>` を fd 番号でインデックスして使う。ホットパスでは既知の fd への
//! 読み書きが大半のため、テーブル自体の再アロケーションは fd 番号の増加時のみ（償却）で
//! 発生し、リクエストごとの新規確保にはならない。

use std::task::Waker;

use crate::runtime::handle::RawFd;

/// 待機方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Interest {
    Read,
    Write,
}

/// fd の片方向（読み取り or 書き込み）の待機者を保持する小さな型（F-166 A-3）。
///
/// # なぜ `Vec<Waker>` を常用しないか
///
/// 通常のソケット read/write では 1 fd につき同時待機者は **高々 1 個**（そのタスク自身が
/// 再 poll されるまで次の待機は起こらない）。にもかかわらず旧実装は `Vec<Waker>` を
/// 常用していたため、待機登録（`push`）のたびに初回は容量確保、起床（`mem::take` して
/// drop）のたびに解放が発生し、1 fd 1 待機者という支配的なケースでも malloc/free が
/// 往復ごとに 1 回ずつ乗っていた。
///
/// # 上書き消失を防ぐ理由（Vec を完全に捨てない理由）
///
/// `runtime::offload` はワーカースレッドごとに 1 本の eventfd を共有し、複数の並行
/// `offload()` 呼び出しが同時にこの eventfd の読み取り可能待ちをする。`Option<Waker>` で
/// 最後の登録者のみを保持する設計にすると、後続の待機者登録が先行者の Waker を黙って
/// 上書き・消失させ、先行者のタスクが永久に起床しなくなる（F-120 Phase 2 で
/// `test_f62_wasm_http_call_concurrent_requests` の再現ハングとして発見した実装バグ）。
/// **後続の登録が先行の Waker を上書きしてはならない** という不変条件は本型でも維持する:
/// 1 個目は `One` にインライン格納（ヒープ確保なし）、2 個目以降だけ `Many(Vec)` へ
/// 昇格する（あふれた場合のみ確保）。
///
/// 起床（[`wake_all`](Self::wake_all)）は「キュー内の全 Waker を起床」し、各タスクが
/// 自身の non-blocking syscall を再試行する（一部は成功、残りは再度 EAGAIN で再登録する
/// レベルトリガ相当の挙動になる）。`One` の場合は `Vec` を経由せず直接 `wake()` するため
/// ヒープ操作が一切発生しない。
#[derive(Default)]
pub(crate) enum WakerSlot {
    #[default]
    Empty,
    One(Waker),
    Many(Vec<Waker>),
}

impl WakerSlot {
    /// 待機者が 1 人もいないか。
    ///
    /// epoll バックエンドでは使わない（`dispatch_event` が `mem::take` を無条件に行い、
    /// 空スロットの `wake_all()` は no-op のため呼び分け不要）。WSAPoll バックエンド
    /// （Windows 専用ビルド）でのみ使う。
    #[inline]
    // WSAPoll バックエンド（Windows 専用ビルド）と単体テストからのみ使うため、
    // それ以外のビルドではコンパイル対象から外す（`allow(dead_code)` は使わない）。
    #[cfg(any(veil_poller_wsapoll, test))]
    pub fn is_empty(&self) -> bool {
        matches!(self, WakerSlot::Empty)
    }

    /// Waker を追加する（既存の待機者を上書きしない。1 人目は `Vec` を経由せずインライン
    /// 格納するため確保が起きない。2 人目以降のみ `Vec` へ昇格する）。
    pub fn push(&mut self, waker: Waker) {
        match std::mem::replace(self, WakerSlot::Empty) {
            WakerSlot::Empty => *self = WakerSlot::One(waker),
            WakerSlot::One(prev) => *self = WakerSlot::Many(vec![prev, waker]),
            WakerSlot::Many(mut v) => {
                v.push(waker);
                *self = WakerSlot::Many(v);
            }
        }
    }

    /// 保持している全 Waker を起床させ、スロットを空にする。
    ///
    /// 待機者が 1 人（支配的なケース）なら `Vec` を経由せず直接 `wake()` するため、
    /// このパスにヒープ確保・解放は一切発生しない。複数待機者（共有 eventfd 等）の
    /// 場合のみ `Vec` の drop が発生する（従来と同じコスト）。
    pub fn wake_all(&mut self) {
        match std::mem::replace(self, WakerSlot::Empty) {
            WakerSlot::Empty => {}
            WakerSlot::One(w) => w.wake(),
            WakerSlot::Many(v) => {
                for w in v {
                    w.wake();
                }
            }
        }
    }
}

/// fd ごとの登録状態。
///
/// `read_waker` / `write_waker` は同時に存在してよい（同一 fd への読み待ちと書き待ちの
/// 並存。L4/splice の双方向転送で必要）。`armed` はカーネル（epoll/kqueue/WSAPoll）へ現在
/// **有効化されている** interest ビット（poller 実装が解釈するビット表現。epoll では
/// `EPOLLIN`/`EPOLLOUT`）を保持する。
///
/// `known_to_kernel` は `armed` とは別に管理する: `EPOLLONESHOT` は発火後に interest を
/// 無効化するのみで、epoll インスタンスの監視対象リストからは fd を除去しない
/// （`EPOLL_CTL_DEL` を呼ばない限り fd は登録済みのまま）。そのため「現在 armed なビットが
/// 無い（`armed == 0`）」は「ADD 未実施」を意味しない。一度でも `EPOLL_CTL_ADD` に成功した
/// fd は次回以降 `armed` の値に関わらず必ず `EPOLL_CTL_MOD` を使う必要がある
/// （F-120 Phase 2 で発見した実装バグ）。
///
/// `read_waker`/`write_waker` は [`WakerSlot`] で持つ（F-166 A-3。旧実装は常に
/// `Vec<Waker>` だったため 1 fd 1 待機者という支配的ケースでも malloc/free が起床の
/// たびに発生していた。詳細は `WakerSlot` の doc 参照）。
#[derive(Default)]
pub(crate) struct FdRecord {
    pub read_waker: WakerSlot,
    pub write_waker: WakerSlot,
    /// カーネルへ現在有効化されている interest ビット（0 = 現在は無反応）。
    /// kqueue バックエンドでは「現在 `EV_ADD|EV_ONESHOT` で登録済みと認識している
    /// フィルタ（READ/WRITE）」の意味で使う（`kqueue::KqueuePoller::update`
    /// の `prev_mask` 引数に渡し、ビットが立たなくなった方向を `EV_DELETE` する判定に使う）。
    /// WSAPoll バックエンドでは「次回 `WSAPoll` 呼び出しに含める方向」の意味で使う。
    /// epoll バックエンドでは「`EPOLLONESHOT` で現在カーネルへ有効化されている方向」。
    pub armed: u32,
    /// この fd に対して `EPOLL_CTL_ADD` を一度でも実行済みか（`EPOLL_CTL_DEL` するまで
    /// true のまま。F-166 A-2 以降、ADD/MOD 判定ではなく「初回登録か否か」の唯一の正になる:
    /// true なら `register()` は epoll_ctl を一切呼ばない）。
    ///
    /// epoll 専用: kqueue の `EV_ADD` は EEXIST を返さず冪等なため、この判定自体が
    /// 不要（常に `EV_ADD` でよい）。
    #[cfg(veil_poller_epoll)]
    pub known_to_kernel: bool,
    /// 直近に届いた読み取り可能イベントのヒント（consume-once、F-141/F-166 A-1）。
    ///
    /// - kqueue: `EVFILT_READ` の `data`（読み取り可能バイト数、カーネルが kevent 発火
    ///   時点で観測した値）をそのまま保持する。
    /// - epoll: `epoll_event` にバイト数相当のフィールドが無いため、`EPOLLIN`（または
    ///   `EPOLLERR`/`EPOLLHUP`）を観測したことを示す非ゼロの番兵値
    ///   （`usize::MAX`、`executor::EPOLL_HINT_SENTINEL`）を保持する。
    ///
    /// `reactor::tcp::Readable`/`ReadableFd`（TCP・UDP 双方の `wait_readable_fd` 経路）が
    /// 「直前の起床で readable と判明済みか」を判定し、確認用の `poll(2)`
    /// syscall を省略するために使う（`runtime::executor::take_read_hint` 参照）。
    /// consume-once（取得時に 0 へリセット）のため、無関係な後続呼び出しへ古い値が
    /// 漏れることはない。
    #[cfg(veil_poller_kqueue)]
    pub read_hint: usize,
    /// 直近に届いた書き込み可能イベントのヒント（consume-once、F-155/F-166 A-1）。
    /// 値の意味は `read_hint` と対称（kqueue はバイト数、epoll は非ゼロ番兵値）。
    ///
    /// `reactor::tcp::Writable`/`WritableFd` が「直前の起床で writable と
    /// 判明済みか」を判定し、確認用の `poll(2)` syscall を省略するために使う
    /// （`runtime::executor::take_write_hint` 参照）。consume-once（取得時に 0 へ
    /// リセット）のため、無関係な後続呼び出しへ古い値が漏れることはない。
    #[cfg(veil_poller_kqueue)]
    pub write_hint: usize,
}

/// fd 番号でインデックスする登録テーブル（Unix: `Vec` ベース）。
///
/// Windows は `RawFd`（`isize` に再解釈した `SOCKET`）が小さい連番であることを
/// 保証されないため、同じ `Vec` インデックス方式を使うとハンドル値次第で巨大な
/// アロケーションが発生し得る。そのため Windows のみ `HashMap` ベースの実装に
/// 切り替える（公開 API・呼び出し側は不変。`veil_poller_wsapoll` 参照）。
#[cfg(not(windows))]
pub(crate) struct FdTable {
    slots: Vec<Option<FdRecord>>,
}

#[cfg(not(windows))]
impl FdTable {
    /// 典型的な同時接続数を見込んで事前確保する（成長はコールドパスの償却のみ）。
    const PREALLOC: usize = 1024;

    pub fn new() -> Self {
        Self {
            slots: Vec::with_capacity(Self::PREALLOC),
        }
    }

    /// fd スロットを確保する（未到達の index までベクタを伸長する。伸長はコールドパスの
    /// 償却のみで、定常状態の fd 番号レンジに収まればアロケーションは発生しない）。
    fn ensure(&mut self, fd: RawFd) -> &mut Option<FdRecord> {
        let idx = fd as usize;
        if idx >= self.slots.len() {
            self.slots.resize_with(idx + 1, || None);
        }
        &mut self.slots[idx]
    }

    /// fd のレコードを取得する（無ければ新規作成する）。
    pub fn get_or_insert(&mut self, fd: RawFd) -> &mut FdRecord {
        self.ensure(fd).get_or_insert_with(FdRecord::default)
    }

    /// fd のレコードを取得する（存在しなければ `None`）。
    pub fn get_mut(&mut self, fd: RawFd) -> Option<&mut FdRecord> {
        self.slots.get_mut(fd as usize).and_then(|s| s.as_mut())
    }

    /// fd のレコードを破棄する。
    ///
    /// ソケット/パイプの close 直前に必ず呼ぶこと。fd 番号は OS により即座に再利用される
    /// ため、呼ばないと新しい fd が古い Waker・armed 状態を誤って引き継ぐ（stale wake の
    /// 原因になる）。
    pub fn remove(&mut self, fd: RawFd) -> Option<FdRecord> {
        self.slots.get_mut(fd as usize).and_then(|s| s.take())
    }
}

/// fd 番号でインデックスする登録テーブル（Windows: `HashMap` ベース）。
///
/// API は Unix 版（`Vec` ベース）と同一。ソケットハンドル値が小さい連番である
/// 保証が無いため `HashMap` を使う。
#[cfg(windows)]
pub(crate) struct FdTable {
    slots: std::collections::HashMap<RawFd, FdRecord>,
}

#[cfg(windows)]
impl FdTable {
    pub fn new() -> Self {
        Self {
            slots: std::collections::HashMap::new(),
        }
    }

    /// fd のレコードを取得する（無ければ新規作成する）。
    pub fn get_or_insert(&mut self, fd: RawFd) -> &mut FdRecord {
        self.slots.entry(fd).or_default()
    }

    /// fd のレコードを取得する（存在しなければ `None`）。
    pub fn get_mut(&mut self, fd: RawFd) -> Option<&mut FdRecord> {
        self.slots.get_mut(&fd)
    }

    /// fd のレコードを破棄する。
    pub fn remove(&mut self, fd: RawFd) -> Option<FdRecord> {
        self.slots.remove(&fd)
    }

    /// 現在 armed（待機中）なエントリの `(fd, armed)` 一覧を返す（WSAPoll 用: poll
    /// 対象配列は毎回このテーブルから構築するため、Vec ベース版と異なりカーネル側の
    /// 監視対象リストを別途持たない）。
    pub(crate) fn armed_entries(&self) -> Vec<(RawFd, u32)> {
        self.slots
            .iter()
            .filter(|(_, rec)| rec.armed != 0)
            .map(|(fd, rec)| (*fd, rec.armed))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{RawWaker, RawWakerVTable};

    /// wake() された回数を数えるだけのテスト用 Waker（自身は起こされたことを記録するのみで
    /// エグゼキュータには一切触れない）。
    fn counting_waker(counter: Rc<AtomicUsize>) -> Waker {
        // Rc<AtomicUsize> を生ポインタへ変換して RawWaker data として持ち回る。
        fn clone(data: *const ()) -> RawWaker {
            let rc = unsafe { Rc::from_raw(data as *const AtomicUsize) };
            let cloned = rc.clone();
            std::mem::forget(rc);
            RawWaker::new(Rc::into_raw(cloned) as *const (), &VTABLE)
        }
        fn wake(data: *const ()) {
            let rc = unsafe { Rc::from_raw(data as *const AtomicUsize) };
            rc.fetch_add(1, Ordering::SeqCst);
        }
        fn wake_by_ref(data: *const ()) {
            let rc = unsafe { Rc::from_raw(data as *const AtomicUsize) };
            rc.fetch_add(1, Ordering::SeqCst);
            std::mem::forget(rc);
        }
        fn drop_fn(data: *const ()) {
            unsafe { drop(Rc::from_raw(data as *const AtomicUsize)) };
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_fn);
        let raw = RawWaker::new(Rc::into_raw(counter) as *const (), &VTABLE);
        unsafe { Waker::from_raw(raw) }
    }

    /// F-166 A-3: 単一待機者（支配的なケース）は `WakerSlot::One` へインライン格納され、
    /// `push`/`wake_all` のどちらも `Vec` を経由しない。ここでは `Vec` を経由するかどうか
    /// を直接は観測できないため、代わりに「1 個だけ push したら 1 回だけ wake される」
    /// という観測可能な振る舞いを検証する（`Many` へ誤って昇格していないかの間接検証は
    /// 下の 2 待機者テストで行う）。
    #[test]
    fn waker_slot_single_waiter_wakes_once() {
        let counter = Rc::new(AtomicUsize::new(0));
        let mut slot = WakerSlot::default();
        assert!(slot.is_empty());

        slot.push(counting_waker(counter.clone()));
        assert!(!slot.is_empty());
        assert!(matches!(slot, WakerSlot::One(_)));

        slot.wake_all();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        // wake_all 後はスロットが空に戻る（consume-once）。
        assert!(slot.is_empty());
    }

    /// 同一 fd・同一方向へ 2 人待機者が登録された場合（共有 eventfd 等）、
    /// `Many` へ昇格し、両者とも `wake_all()` で起床する（上書き消失しない）。
    #[test]
    fn waker_slot_two_waiters_both_wake() {
        let counter_a = Rc::new(AtomicUsize::new(0));
        let counter_b = Rc::new(AtomicUsize::new(0));
        let mut slot = WakerSlot::default();

        slot.push(counting_waker(counter_a.clone()));
        slot.push(counting_waker(counter_b.clone()));
        assert!(matches!(slot, WakerSlot::Many(_)));

        slot.wake_all();
        assert_eq!(counter_a.load(Ordering::SeqCst), 1);
        assert_eq!(counter_b.load(Ordering::SeqCst), 1);
        assert!(slot.is_empty());
    }
}
