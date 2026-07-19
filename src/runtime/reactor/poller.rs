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

/// fd ごとの登録状態。
///
/// `read_waker` / `write_waker` は同時に存在してよい（同一 fd への読み待ちと書き待ちの
/// 並存。L4/splice の双方向転送で必要）。`armed` はカーネル（epoll/kqueue）へ現在
/// **有効化されている** interest ビット（poller 実装が解釈するビット表現。epoll では
/// `EPOLLIN`/`EPOLLOUT`）を保持する。
///
/// `known_to_kernel` は `armed` とは別に管理する: `EPOLLONESHOT` は発火後に interest を
/// 無効化するのみで、epoll インスタンスの監視対象リストからは fd を除去しない
/// （`EPOLL_CTL_DEL` を呼ばない限り fd は登録済みのまま）。そのため「現在armedなビットが
/// 無い（`armed == 0`）」は「ADD 未実施」を意味しない。一度でも `EPOLL_CTL_ADD` に成功した
/// fd は次回以降 `armed` の値に関わらず必ず `EPOLL_CTL_MOD` を使う必要があり、これを
/// 誤って ADD すると `EEXIST` で失敗し、再武装できないまま readiness 待ちがハングする
/// （F-120 Phase 2 で発見した実装バグ。`armed == 0` を ADD/MOD 判定に使ってはならない）。
///
/// `read_waker`/`write_waker` は **`Vec<Waker>`**（同方向の複数同時待機者を保持できる
/// キュー）で持つ。io_uring 版は `wait_readable_fd`/`wait_writable_fd` の呼び出しごとに
/// 独立した `POLL_ADD` op を発行するため、同一 fd に対する複数同時待機が自然にサポート
/// される（`runtime::offload` はワーカースレッドごとに 1 本の eventfd を共有し、
/// 複数の並行 `offload()` 呼び出しが同時にこの eventfd の読み取り可能待ちをする）。
/// epoll の oneshot 登録は fd あたり 1 つの interest しか持てないため、`Option<Waker>`
/// で最後の登録者のみを保持すると、後続の待機者登録が先行者の Waker を黙って
/// 上書き・消失させ、先行者のタスクが永久に起床しなくなる（F-120 Phase 2 で
/// `test_f62_wasm_http_call_concurrent_requests` の再現ハングとして発見した実装バグ）。
/// fd がイベント発火時は「キュー内の全 Waker を起床」し、各タスクが自身の non-blocking
/// syscall を再試行する（一部は成功、残りは再度 EAGAIN で再登録するレベルトリガ相当の
/// 挙動になる。ソケット read/write の通常経路では 1 fd につき 1 待機者が大半のため
/// キューは通常 0〜1 要素で、複数待機は eventfd 共有等の限られたケースのみ）。
#[derive(Default)]
pub(crate) struct FdRecord {
    pub read_wakers: Vec<Waker>,
    pub write_wakers: Vec<Waker>,
    /// カーネルへ現在有効化されている interest ビット（0 = 現在は無反応。fd 自体は
    /// `known_to_kernel` が true なら epoll 監視対象リストに残っている）。
    /// kqueue バックエンドでは「現在 `EV_ADD|EV_ONESHOT` で登録済みと認識している
    /// フィルタ（READ/WRITE）」の意味で同じフィールドを使う（`kqueue::KqueuePoller::update`
    /// の `prev_mask` 引数に渡し、ビットが立たなくなった方向を `EV_DELETE` する判定に使う）。
    pub armed: u32,
    /// この fd に対して `EPOLL_CTL_ADD` を一度でも実行済みか（`EPOLL_CTL_DEL` するまで
    /// true のまま。ADD/MOD の選択に使う唯一の正）。
    ///
    /// epoll 専用: kqueue の `EV_ADD` は EEXIST を返さず冪等なため、この判定自体が
    /// 不要（常に `EV_ADD` でよい）。
    #[cfg(veil_poller_epoll)]
    pub known_to_kernel: bool,
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
