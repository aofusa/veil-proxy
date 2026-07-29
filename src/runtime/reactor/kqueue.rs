//! kqueue(2)/kevent(2) の薄いラッパ（`veil_poller_kqueue`、FreeBSD/OpenBSD）
//!
//! `epoll.rs` と同じ責務分担: カーネル呼び出しのみに閉じ、fd ごとの Waker 管理は
//! `reactor::executor`（`FdTable` 経由）が担う。
//!
//! epoll と異なり、kqueue は `EVFILT_READ` / `EVFILT_WRITE` が **fd ごとに独立した
//! フィルタ**である（1 fd につき最大 2 エントリ）。そのため epoll の「1 fd = 1 interest
//! ビットマスク」という前提はそのまま持ち込めない。本ラッパでは read/write を個別の
//! `kevent` エントリとして登録・解除する API を提供し、`executor.rs` 側の armed ビット
//! 管理（`poller::FdRecord`）は「read フィルタが有効か」「write フィルタが有効か」を
//! 独立に追跡する形でそのまま再利用する（`READ`/`WRITE` ビットの意味は epoll 版と揃える）。
//!
//! `EV_ADD` は kqueue では EEXIST を返さず**冪等**（既存フィルタの再設定になる）ため、
//! epoll 版の `known_to_kernel`（ADD/MOD 判定）に相当する管理は不要で、常に `EV_ADD` を
//! 使えばよい。oneshot 意味論は `EV_ONESHOT` で表現する（発火後にカーネルが自動で
//! フィルタを削除する。次回待機時は改めて `EV_ADD|EV_ONESHOT` で再登録する）。

use std::cell::RefCell;
use std::io;
use std::os::unix::io::RawFd;

/// 読み取り可能 interest ビット（`EVFILT_READ` 相当）。
pub const READ: u32 = 0b01;
/// 書き込み可能 interest ビット（`EVFILT_WRITE` 相当）。
pub const WRITE: u32 = 0b10;

// epoll の `ERR_HUP` に相当する個別ビットは kqueue には存在しない: `EV_EOF`/`EV_ERROR`
// は発火したフィルタ（`EVFILT_READ`/`EVFILT_WRITE`）自体のイベントとして配送されるため、
// dispatch 側は READ/WRITE の発火だけを見ればよい（`executor::dispatch_event` 参照）。
// ただし `EV_ERROR`（changelist バッチ化に伴い eventlist 経由で報告される、下記 doc 参照）は
// dispatch 側で明示的に弾く必要がある。

/// changelist バッチの初期容量（1 イベントループ周回で通常発生する登録変更数を見込む。
/// 超過時は `Vec` が償却的に伸長するのみでホットパスの正しさには影響しない）。
const CHANGELIST_PREALLOC: usize = 64;

/// kqueue インスタンスのラッパ。
///
/// # changelist バッチ化（F-141）
///
/// io_uring の SQ（submission queue）と同様、`update`/`delete` は `kevent(2)` を
/// **即座には呼ばない**。代わりに変更を `changelist`（スレッドローカルではなく
/// このインスタンス自身が保持する `RefCell<Vec<kevent>>`。poller 自体が
/// スレッドローカルに 1 個だけ存在するため実質スレッドローカルと同義）に溜め、
/// 次の [`wait`](Self::wait) 呼び出しで changelist と eventlist を **同一の
/// `kevent()` 呼び出し**にまとめて渡す。
///
/// これにより「関心変更のたびに 1 回、park のたびに 1 回」だった syscall 回数が
/// 「イベントループ 1 周につき 1 回」に減る（登録変更が複数個溜まっていても
/// まとめて 1 syscall で反映される）。
///
/// 変更の適用結果（成功/エラー）は、`nevents > 0` で eventlist に余裕がある限り
/// カーネルが `EV_ERROR` フラグ付きの `kevent` として eventlist に詰めて返す
/// （`man kevent`: 変更適用中にエラーが起きても eventlist に空きがあれば
/// changelist の残りは引き続き処理される）。そのため `dispatch_event` 側で
/// `EV_ERROR` エントリを弾く必要がある（`executor::park` 参照）。
///
/// close 直前の `delete` も同様に changelist へ積むだけで即時 syscall しない。
/// fd の close(2) 自体がカーネル側フィルタを自動除去するため、次の `wait` まで
/// 反映が遅延しても実害はない（積まれた `EV_DELETE` は次の `wait` で ENOENT の
/// `EV_ERROR` として報告されるだけで無視される）。fd 番号が close 直後に別の
/// 接続へ再利用された場合も、changelist は append 順を保つため、古い
/// `EV_DELETE`（既に close 済みで実質無効）→ 新しい `EV_ADD` の順で処理され、
/// 新規登録を誤って壊すことはない。
pub(crate) struct KqueuePoller {
    kq: RawFd,
    changelist: RefCell<Vec<libc::kevent>>,
}

impl KqueuePoller {
    /// 新しい kqueue インスタンスを作成する。
    pub fn new() -> io::Result<Self> {
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        // FD_CLOEXEC を明示付与する（kqueue() 自体には CLOEXEC 版の生成関数が無い）。
        unsafe {
            libc::fcntl(kq, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        Ok(Self {
            kq,
            changelist: RefCell::new(Vec::with_capacity(CHANGELIST_PREALLOC)),
        })
    }

    /// 生の kq fd を取得する（F-127 AIO の `SIGEV_KEVENT` 通知先設定に使う。
    /// AIO 経路のみが必要とするため `veil_aio` でガードする）。
    #[cfg(veil_aio)]
    pub(crate) fn raw_fd(&self) -> RawFd {
        self.kq
    }

    /// fd の interest ビット（`READ`/`WRITE` の組み合わせ）を oneshot で（再）登録する。
    ///
    /// 立っているビットは `EV_ADD|EV_ONESHOT` で登録し、立っていないビットのうち
    /// 直前まで登録されていたもの（`prev_mask` で渡す）は `EV_DELETE` で明示的に外す。
    /// **F-141: 即座に `kevent()` を呼ばず、changelist（`self.changelist`）へ積むだけ**
    /// （型 doc 参照）。次の [`wait`](Self::wait) 呼び出しでまとめて 1 syscall で反映される。
    pub fn update(&self, fd: RawFd, mask: u32, prev_mask: u32) {
        let mut changes = self.changelist.borrow_mut();

        if mask & READ != 0 {
            changes.push(make_kevent(
                fd,
                libc::EVFILT_READ,
                libc::EV_ADD | libc::EV_ONESHOT,
            ));
        } else if prev_mask & READ != 0 {
            changes.push(make_kevent(fd, libc::EVFILT_READ, libc::EV_DELETE));
        }
        if mask & WRITE != 0 {
            changes.push(make_kevent(
                fd,
                libc::EVFILT_WRITE,
                libc::EV_ADD | libc::EV_ONESHOT,
            ));
        } else if prev_mask & WRITE != 0 {
            changes.push(make_kevent(fd, libc::EVFILT_WRITE, libc::EV_DELETE));
        }
    }

    /// fd の READ/WRITE フィルタを両方削除する（close 直前に呼ぶ）。
    ///
    /// F-141: `update` と同様、即座に syscall せず changelist へ積むだけ。close(2) 自体が
    /// カーネル側フィルタを自動除去するため、次の `wait` まで反映が遅延しても実害はない
    /// （型 doc 参照）。
    pub fn delete(&self, fd: RawFd, mask: u32) {
        let mut changes = self.changelist.borrow_mut();
        if mask & READ != 0 {
            changes.push(make_kevent(fd, libc::EVFILT_READ, libc::EV_DELETE));
        }
        if mask & WRITE != 0 {
            changes.push(make_kevent(fd, libc::EVFILT_WRITE, libc::EV_DELETE));
        }
    }

    /// イベントを待つ。`timeout_ms` が負値なら無期限待機。
    ///
    /// F-141: `update`/`delete` が溜めた changelist を **eventlist と同一の `kevent()`
    /// 呼び出し**で渡す（io_uring の `submit_and_wait` 相当。型 doc 参照）。changelist は
    /// 呼び出し後に空にする（次周回分は `update`/`delete` が新たに積む）。
    pub fn wait(&self, events: &mut [libc::kevent], timeout_ms: i32) -> io::Result<usize> {
        let ts;
        let ts_ptr = if timeout_ms < 0 {
            std::ptr::null()
        } else {
            ts = libc::timespec {
                tv_sec: (timeout_ms / 1000) as libc::time_t,
                tv_nsec: ((timeout_ms % 1000) * 1_000_000) as libc::c_long,
            };
            &ts as *const libc::timespec
        };
        let mut changes = self.changelist.borrow_mut();
        // F-140: `kevent(2)` の個数引数は **NetBSD だけ `size_t`**（他 BSD/macOS は `c_int`）。
        // libc クレートの束縛も target 別に型が変わるため、`as _` ではなく明示的に
        // ターゲット別のエイリアスへキャストする（`as _` は推論できず E0308 になる）。
        #[cfg(target_os = "netbsd")]
        type KeventCount = usize;
        #[cfg(not(target_os = "netbsd"))]
        type KeventCount = libc::c_int;
        let n = unsafe {
            libc::kevent(
                self.kq,
                changes.as_ptr(),
                changes.len() as KeventCount,
                events.as_mut_ptr(),
                events.len() as KeventCount,
                ts_ptr,
            )
        };
        // 反映済み（または EV_ERROR として eventlist へ報告済み）なので changelist を
        // クリアする。容量は保持するため次周回の push で再アロケーションは起きない。
        //
        // EINTR（`n < 0` かつ Interrupted）でも changelist を清算してよい: kqueue の
        // changelist はイベント待機（ブロック）フェーズに入る **前**に適用される
        // （libevent 等の kqueue バックエンドが同様に changelist+eventlist を 1 回の
        // `kevent()` にまとめる際に依拠している、広く確立された挙動）。そのため
        // シグナル割り込みは「待機」部分のみに影響し、直前に積んだ登録変更は
        // 既に適用済みである。
        changes.clear();
        drop(changes);
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(0);
            }
            return Err(e);
        }
        Ok(n as usize)
    }
}

// filter/flags は FreeBSD/OpenBSD/macOS で型が異なり得る（libc クレートが target 別に
// `libc::kevent` のフィールド型を定義する）ため、呼び出し側の `libc::EVFILT_*`/
// `libc::EV_*` 定数をそのまま `as _` でフィールド型へキャストする（ハードコードした
// 具象型を引数に取らない）。
//
// F-140: NetBSD の `struct kevent` は歴史的な BSD 定義（FreeBSD/OpenBSD/macOS の
// `filter: i16` / `flags: u16` / `udata: intptr_t`）から拡張されており、
// `filter`/`flags` とも `uint32_t`、`udata` は `void *` である（NetBSD
// `<sys/event.h>` 参照）。本関数は `udata` を一切設定しない（`mem::zeroed()` の
// ままゼロ/NULL）ため `udata` の型差はここでは影響しないが、`filter`/`flags` の
// 代入先フィールド型が変わるため `TryInto<i16>`/`TryInto<u16>` 固定の下の実装は
// NetBSD では型不一致でコンパイルできない。`TryInto<u32>` 版を別途用意して吸収する
// （FreeBSD/OpenBSD/macOS 側のこの関数は 1 行も変更していない）。
#[cfg(not(target_os = "netbsd"))]
fn make_kevent(
    fd: RawFd,
    filter: impl TryInto<i16> + Copy,
    flags: impl TryInto<u16> + Copy,
) -> libc::kevent {
    let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
    ev.ident = fd as libc::uintptr_t;
    ev.filter = match filter.try_into() {
        Ok(v) => v,
        Err(_) => 0,
    };
    ev.flags = match flags.try_into() {
        Ok(v) => v,
        Err(_) => 0,
    };
    ev
}

/// NetBSD 版 `make_kevent`（`filter`/`flags` が `uint32_t` のため `TryInto<u32>`
/// で受ける。ロジックは非 NetBSD 版と同一）。
#[cfg(target_os = "netbsd")]
fn make_kevent(
    fd: RawFd,
    filter: impl TryInto<u32> + Copy,
    flags: impl TryInto<u32> + Copy,
) -> libc::kevent {
    let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
    ev.ident = fd as libc::uintptr_t;
    ev.filter = match filter.try_into() {
        Ok(v) => v,
        Err(_) => 0,
    };
    ev.flags = match flags.try_into() {
        Ok(v) => v,
        Err(_) => 0,
    };
    ev
}

impl Drop for KqueuePoller {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.kq);
        }
    }
}
