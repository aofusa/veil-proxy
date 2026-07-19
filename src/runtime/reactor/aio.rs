//! FreeBSD POSIX AIO（`aio_read`/`aio_write` + `EVFILT_AIO`）経路（F-127、`veil_aio`）
//!
//! `veil_aio` cfg（`--features aio`、FreeBSD 専用。`build.rs` 参照）が有効な場合のみ
//! コンパイルされる。未設定時は本モジュール自体が存在せず、既存の kqueue readiness 経路
//! （`reactor::tcp` の `ReadFuture`/`WriteFuture`）が無改変・ゼロコストで維持される
//! （設計 `docs/artifacts/f127_freebsd_aio_design.md`）。
//!
//! ## アーキテクチャ
//!
//! `aio_read(2)`/`aio_write(2)` を発行する際、`aiocb.aio_sigevent` を
//! `SIGEV_KEVENT` に設定し、通知先 kqueue（このスレッドの kq fd）と udata
//! （下記スラブの token）を指定する。完了は通常の `kevent(2)` ループへ
//! `EVFILT_AIO` イベントとして届く（man 4 aio: `ident` = aiocb ポインタ、
//! `filter` = `EVFILT_AIO`、`udata` = `sigev_value` の値）。
//! そのため kevent ドレインループ側の分岐は `reactor::executor` の
//! `park`/`dispatch_event`（kqueue 版）に追加している（コメント参照。設計文書は
//! 「kqueue.rs のドレインループ」と表現しているが、実際のドレイン/dispatch は
//! `executor.rs` 側に実装されているため、そちらに `EVFILT_AIO` 分岐を追加した）。
//!
//! ## aiocb / バッファの生存管理（最重要）
//!
//! `aiocb`（`libc::aiocb`）はカーネルが完了まで参照し続けるため、submit 後は
//! **固定アドレスで生存**していなければならない（FreeBSD `aio_read(2)`
//! RESTRICTIONS 節）。本実装ではスレッドローカルなスラブ（[`AioTable`]）の各
//! スロットが `Box<libc::aiocb>` を保持し、スロット自体（`Vec` 内の要素）が
//! 再配置されても Box が指す先のヒープ領域は移動しない。
//!
//! Future（[`AioReadFuture`]/[`AioWriteFuture`]）が完了前に `Drop` された場合、
//! `aio_cancel(2)` を試みるが、FreeBSD の man ページ（aio_cancel(2)）が
//! 明記する通り **キャンセル成功・不能・既完了のいずれでも通常の完了通知
//! （SIGEV_KEVENT）が届く**。そのため cancel の戻り値による分岐は行わず、
//! 常にスロットを「Detached」状態へ移し、完了 kevent が届くまでバッファと
//! aiocb を保持してから解放する（uring バックエンドの `OpGuard`/`detach_op`
//! と同型の設計。`runtime::uring::executor` 参照）。
//!
//! `aio_return(2)` は完了後に **一度だけ**呼べる（二重呼び出しは未定義動作）。
//! 本モジュールでは [`reap`] のみが `aio_return` を呼び、スラブの状態機械
//! （`Active`→ 結果格納→ `take_outcome` で消費、または `Detached` → 即時解放）
//! がこの一回性を構造的に保証する。
//!
//! ## EAGAIN フォールバック
//!
//! `vfs.aio.max_aio_queue_per_proc` 等の上限に達すると `aio_read`/`aio_write` が
//! `EAGAIN` を返す。この場合はスラブへの登録を即座に取り消し、当該 I/O だけ
//! 既存の readiness 経路（非ブロッキング syscall → `WouldBlock` なら
//! `register_read`/`register_write` で待機）にフォールバックする
//! （`AioReadFuture`/`AioWriteFuture` の `Fallback` 状態）。

use std::cell::RefCell;
use std::future::Future;
use std::io;
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use crate::runtime::buf::{IoBuf, IoBufMut};
use crate::runtime::reactor::executor::{current_kqueue_fd, register_read, register_write};

/// `EAGAIN`/`EWOULDBLOCK` か判定する（`reactor::tcp` と同一の判定）。
#[inline]
fn is_would_block(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::WouldBlock
}

// ====================
// AIO 完了スラブ（`runtime::uring::executor::OpTable` と同型の設計）
// ====================

/// スロットの状態。
enum AioSlotState {
    /// 空きスロット（free-list に登録済み）。
    Free,
    /// Future 生存中の op。`cb` は submit 済みの `aiocb`（完了まで固定アドレスで
    /// 生存させる必要があるため `Box` で保持する。`Vec<AioSlot>` の再配置は
    /// `Box` のポインタ値を移動させるだけで、指す先のヒープ領域は動かない）。
    Active {
        cb: Box<libc::aiocb>,
        fd: RawFd,
        waker: Option<Waker>,
        /// 完了済みなら `Some`（`aio_return` 済みの結果。`take_outcome` が消費する）。
        outcome: Option<io::Result<usize>>,
    },
    /// Future が完了前に drop され、`aio_cancel` 発行済みで完了 kevent 待ちの op。
    /// `cleanup` はバッファ（と aiocb）の解放を担うクロージャで、完了 kevent 到着時
    /// （[`AioTable::complete`]）に一度だけ呼ばれる。
    Detached {
        cb: Box<libc::aiocb>,
        cleanup: Box<dyn FnOnce(io::Result<usize>)>,
    },
}

struct AioSlot {
    /// 世代カウンタ（1 始まり。解放ごとに +1、0 は「無効 token」用に予約して飛ばす）。
    generation: u32,
    state: AioSlotState,
}

/// token へ (index, generation) をパックする（`uring::executor` の `pack_op` と同一方式）。
#[inline]
fn pack(index: u32, generation: u32) -> u64 {
    ((generation as u64) << 32) | index as u64
}

#[inline]
fn unpack(token: u64) -> (u32, u32) {
    (token as u32, (token >> 32) as u32)
}

/// スレッドローカルな AIO op スラブ。
struct AioTable {
    slots: Vec<AioSlot>,
    free: Vec<u32>,
}

impl AioTable {
    fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
        }
    }

    /// 新規スロットを確保し（プレースホルダの `aiocb` を保持する）、token を返す。
    /// 呼び出し側はこの token で [`raw_cb_ptr`](Self::raw_cb_ptr) を取得し、
    /// フィールドを埋めてから `aio_read`/`aio_write` を発行する。
    fn insert(&mut self, cb: Box<libc::aiocb>, fd: RawFd) -> u64 {
        let index = match self.free.pop() {
            Some(i) => i,
            None => {
                let i = self.slots.len() as u32;
                self.slots.push(AioSlot {
                    generation: 1,
                    state: AioSlotState::Free,
                });
                i
            }
        };
        let slot = &mut self.slots[index as usize];
        slot.state = AioSlotState::Active {
            cb,
            fd,
            waker: None,
            outcome: None,
        };
        pack(index, slot.generation)
    }

    #[inline]
    fn resolve(&self, token: u64) -> Option<usize> {
        let (index, generation) = unpack(token);
        let slot = self.slots.get(index as usize)?;
        if slot.generation != generation {
            return None;
        }
        Some(index as usize)
    }

    fn free_slot(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        slot.generation = slot.generation.wrapping_add(1);
        if slot.generation == 0 {
            slot.generation = 1;
        }
        slot.state = AioSlotState::Free;
        self.free.push(index as u32);
    }

    /// submit 前の `aiocb` フィールドを埋めるための生ポインタを取得する。
    /// `aio_read`/`aio_write` 発行前にのみ使う（Active スロット限定）。
    fn raw_cb_ptr(&mut self, token: u64) -> Option<*mut libc::aiocb> {
        let i = self.resolve(token)?;
        match &mut self.slots[i].state {
            AioSlotState::Active { cb, .. } => Some(cb.as_mut() as *mut libc::aiocb),
            _ => None,
        }
    }

    /// submit（`aio_read`/`aio_write`）自体が失敗した場合にスロットを即座に破棄する。
    /// カーネルはまだ aiocb を参照していないため安全に解放できる。
    fn remove_unsubmitted(&mut self, token: u64) {
        if let Some(i) = self.resolve(token) {
            self.free_slot(i);
        }
    }

    fn set_waker(&mut self, token: u64, waker: Waker) {
        if let Some(i) = self.resolve(token) {
            if let AioSlotState::Active { waker: w, .. } = &mut self.slots[i].state {
                *w = Some(waker);
            }
        }
    }

    /// 完了済みなら結果を取り出す（Future 側の poll から呼ぶ。取り出したらスロットは解放）。
    fn take_outcome(&mut self, token: u64) -> Option<io::Result<usize>> {
        let i = self.resolve(token)?;
        let has_outcome = matches!(
            &self.slots[i].state,
            AioSlotState::Active {
                outcome: Some(_),
                ..
            }
        );
        if !has_outcome {
            return None;
        }
        let outcome = match std::mem::replace(&mut self.slots[i].state, AioSlotState::Free) {
            AioSlotState::Active { outcome, .. } => outcome,
            _ => unreachable!("has_outcome checked above"),
        };
        self.free_slot(i);
        outcome
    }

    /// Future の `Drop` 時に呼ぶ。未完了なら `aio_cancel` を試み、スロットを
    /// `Detached` へ移して完了 kevent を待つ（バッファは `cleanup` へ移送する）。
    /// 既に完了済み（`take_outcome` 前に drop された）場合は即座に `cleanup` を呼ぶ。
    fn detach(&mut self, token: u64, cleanup: Box<dyn FnOnce(io::Result<usize>)>) {
        let Some(i) = self.resolve(token) else {
            // 既に take_outcome 済み（到達しないはずだが、防御的にバッファ解放だけ行う）。
            cleanup(Err(io::Error::other("aio: token already resolved")));
            return;
        };
        match std::mem::replace(&mut self.slots[i].state, AioSlotState::Free) {
            AioSlotState::Active {
                cb,
                fd: _,
                outcome: Some(outcome),
                ..
            } => {
                drop(cb);
                self.free_slot(i);
                cleanup(outcome);
            }
            AioSlotState::Active {
                cb,
                fd,
                outcome: None,
                ..
            } => {
                // 未完了: aio_cancel を試みる。戻り値（AIO_CANCELED/AIO_NOTCANCELED/
                // AIO_ALLDONE のいずれ）に関わらず、SIGEV_KEVENT 登録済みの op には
                // 必ず完了通知が届く（FreeBSD aio_cancel(2) RESTRICTIONS/DESCRIPTION 節:
                // "Normal asynchronous notification occurs for cancelled requests."）。
                // そのため cancel の成否では分岐せず、常に Detached へ移行して
                // complete() 到着を待つ。
                unsafe {
                    libc::aio_cancel(fd, cb.as_ref() as *const libc::aiocb as *mut libc::aiocb);
                }
                self.slots[i].state = AioSlotState::Detached { cb, cleanup };
            }
            AioSlotState::Detached { .. } | AioSlotState::Free => {
                // detach は Future の Drop から一度しか呼ばれない設計のため到達しない。
                unreachable!("aio: detach called on a non-Active slot");
            }
        }
    }

    /// 完了 kevent 処理（`executor::park`(kqueue 版) の `EVFILT_AIO` 分岐から呼ぶ）。
    fn complete(&mut self, token: u64) {
        let Some(i) = self.resolve(token) else {
            return; // 世代不一致（stale）。無視する。
        };
        let state = std::mem::replace(&mut self.slots[i].state, AioSlotState::Free);
        match state {
            AioSlotState::Active {
                mut cb,
                fd,
                waker,
                outcome: _,
            } => {
                let result = reap(&mut cb);
                self.slots[i].state = AioSlotState::Active {
                    cb,
                    fd,
                    waker: None,
                    outcome: Some(result),
                };
                if let Some(w) = waker {
                    w.wake();
                }
            }
            AioSlotState::Detached { mut cb, cleanup } => {
                let result = reap(&mut cb);
                self.free_slot(i);
                cleanup(result);
            }
            AioSlotState::Free => {
                // 既に解放済み(通常は resolve() の世代チェックで弾かれるはずだが、
                // 同一スロットに対する重複 complete 呼び出しに備えた防御)。
            }
        }
    }
}

thread_local! {
    static AIO_TABLE: RefCell<AioTable> = RefCell::new(AioTable::new());
}

fn with_table<R>(f: impl FnOnce(&mut AioTable) -> R) -> R {
    AIO_TABLE.with(|t| f(&mut t.borrow_mut()))
}

/// `aio_error`/`aio_return` を用いて完了結果を回収する。**`aio_return` はここでのみ、
/// 一度だけ呼ぶ**（呼び出し側の状態機械がこの一回性を保証する。詳細はモジュール doc）。
fn reap(cb: &mut libc::aiocb) -> io::Result<usize> {
    let err = unsafe { libc::aio_error(cb as *const libc::aiocb) };
    let ret = unsafe { libc::aio_return(cb as *mut libc::aiocb) };
    if err == 0 {
        Ok(ret.max(0) as usize)
    } else {
        Err(io::Error::from_raw_os_error(err))
    }
}

/// kevent ドレインループから呼ばれる完了ハンドラ（`ev.udata` を token として渡す）。
pub(crate) fn handle_completion(token: u64) {
    with_table(|t| t.complete(token));
}

// ====================
// submit ヘルパ
// ====================

/// submit の結果（フォールバックが必要か、致命的エラーか）。
enum SubmitOutcome {
    /// カーネル上限到達（`EAGAIN`）等で AIO 経路を使えない。readiness へフォールバックする。
    Fallback,
    /// 回復不能なエラー（呼び出し元は即座に `Poll::Ready(Err(..))` を返す）。
    Error(io::Error),
}

/// 現在のスレッドの kqueue fd を取得する。reactor 未初期化ならフォールバック対象。
fn kqueue_fd_or_fallback() -> Result<RawFd, SubmitOutcome> {
    current_kqueue_fd().ok_or(SubmitOutcome::Fallback)
}

fn new_zeroed_aiocb() -> Box<libc::aiocb> {
    Box::new(unsafe { std::mem::zeroed() })
}

/// 共通の `aiocb` 初期化（`aio_fildes`/`aio_offset`/`aio_sigevent`）。
/// `aio_buf`/`aio_nbytes`/実際の submit（`aio_read`/`aio_write`）は呼び出し側が行う。
///
/// # Safety
/// `cb_ptr` は [`AioTable::insert`] 直後に取得した有効な `aiocb` へのポインタであること。
unsafe fn init_common(cb_ptr: *mut libc::aiocb, fd: RawFd, kq: RawFd, token: u64) {
    (*cb_ptr).aio_fildes = fd;
    (*cb_ptr).aio_offset = 0; // ソケットのため常に 0（設計ドキュメント参照）。
    (*cb_ptr).aio_sigevent.sigev_notify = libc::SIGEV_KEVENT;
    // FreeBSD の SIGEV_KEVENT 規約: 通知先 kqueue の fd は `sigev_signo` に格納する
    // （C ヘッダでは `sigev_notify_kqueue` という別名マクロだが、libc クレートは
    // その別名フィールドを公開していないため `sigev_signo` を直接使う。
    // `/usr/include/sys/signal.h`: `#define sigev_notify_kqueue sigev_signo`）。
    (*cb_ptr).aio_sigevent.sigev_signo = kq;
    (*cb_ptr).aio_sigevent.sigev_value.sival_ptr = token as usize as *mut libc::c_void;
}

fn try_submit_read<T: IoBufMut>(fd: RawFd, buf: &mut T) -> Result<u64, SubmitOutcome> {
    let kq = kqueue_fd_or_fallback()?;
    let cb = new_zeroed_aiocb();
    let token = with_table(|t| t.insert(cb, fd));
    let cb_ptr = with_table(|t| t.raw_cb_ptr(token)).expect("slot just inserted");
    unsafe {
        init_common(cb_ptr, fd, kq, token);
        (*cb_ptr).aio_buf = buf.write_ptr() as *mut libc::c_void;
        (*cb_ptr).aio_nbytes = buf.bytes_total();
    }
    let ret = unsafe { libc::aio_read(cb_ptr) };
    if ret == 0 {
        return Ok(token);
    }
    let e = io::Error::last_os_error();
    with_table(|t| t.remove_unsubmitted(token));
    if e.raw_os_error() == Some(libc::EAGAIN) {
        Err(SubmitOutcome::Fallback)
    } else {
        Err(SubmitOutcome::Error(e))
    }
}

fn try_submit_write<T: IoBuf>(fd: RawFd, buf: &T) -> Result<u64, SubmitOutcome> {
    let kq = kqueue_fd_or_fallback()?;
    let cb = new_zeroed_aiocb();
    let token = with_table(|t| t.insert(cb, fd));
    let cb_ptr = with_table(|t| t.raw_cb_ptr(token)).expect("slot just inserted");
    unsafe {
        init_common(cb_ptr, fd, kq, token);
        (*cb_ptr).aio_buf = buf.read_ptr() as *mut libc::c_void;
        (*cb_ptr).aio_nbytes = buf.bytes_init();
    }
    let ret = unsafe { libc::aio_write(cb_ptr) };
    if ret == 0 {
        return Ok(token);
    }
    let e = io::Error::last_os_error();
    with_table(|t| t.remove_unsubmitted(token));
    if e.raw_os_error() == Some(libc::EAGAIN) {
        Err(SubmitOutcome::Fallback)
    } else {
        Err(SubmitOutcome::Error(e))
    }
}

// ====================
// AioReadFuture
// ====================

enum OpState {
    NotSubmitted,
    Submitted(u64),
    Fallback,
    /// 完了して結果を返した後（誤って再 poll された場合に検出する）。
    Done,
}

/// AIO ベースの読み込み Future（`reactor::tcp::ReadFuture` の `veil_aio` 版）。
pub struct AioReadFuture<T: IoBufMut> {
    fd: RawFd,
    buf: Option<T>,
    state: OpState,
}

impl<T: IoBufMut> AioReadFuture<T> {
    pub(crate) fn new(fd: RawFd, buf: T) -> Self {
        Self {
            fd,
            buf: Some(buf),
            state: OpState::NotSubmitted,
        }
    }
}

impl<T: IoBufMut> Future for AioReadFuture<T> {
    type Output = (io::Result<usize>, T);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        loop {
            match this.state {
                OpState::NotSubmitted => {
                    let buf = this
                        .buf
                        .as_mut()
                        .expect("AioReadFuture: buffer missing before submit");
                    match try_submit_read(this.fd, buf) {
                        Ok(token) => this.state = OpState::Submitted(token),
                        Err(SubmitOutcome::Fallback) => this.state = OpState::Fallback,
                        Err(SubmitOutcome::Error(e)) => {
                            this.state = OpState::Done;
                            let buf = this.buf.take().expect("buffer present on submit error");
                            return Poll::Ready((Err(e), buf));
                        }
                    }
                }
                OpState::Submitted(token) => {
                    if let Some(result) = with_table(|t| t.take_outcome(token)) {
                        this.state = OpState::Done;
                        let mut buf = this.buf.take().expect("buffer present at AIO completion");
                        if let Ok(n) = result {
                            // SAFETY: aio_read がカーネル完了として n バイトを書き込んだ。
                            unsafe { buf.set_init(n) };
                        }
                        return Poll::Ready((result, buf));
                    }
                    with_table(|t| t.set_waker(token, cx.waker().clone()));
                    return Poll::Pending;
                }
                OpState::Fallback => {
                    // 既存 readiness ReadFuture と同一の try-first ループ
                    // （`reactor::tcp::ReadFuture` 参照）。
                    let buf = this
                        .buf
                        .as_mut()
                        .expect("AioReadFuture: buffer missing in fallback");
                    let ret = unsafe {
                        libc::read(
                            this.fd,
                            buf.write_ptr() as *mut libc::c_void,
                            buf.bytes_total(),
                        )
                    };
                    if ret >= 0 {
                        unsafe { buf.set_init(ret as usize) };
                        this.state = OpState::Done;
                        let buf = this.buf.take().expect("buffer present at completion");
                        return Poll::Ready((Ok(ret as usize), buf));
                    }
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    if is_would_block(&e) {
                        register_read(this.fd, cx.waker().clone());
                        return Poll::Pending;
                    }
                    this.state = OpState::Done;
                    let buf = this.buf.take().expect("buffer present on error");
                    return Poll::Ready((Err(e), buf));
                }
                OpState::Done => {
                    unreachable!("AioReadFuture polled after completion")
                }
            }
        }
    }
}

impl<T: IoBufMut> Drop for AioReadFuture<T> {
    fn drop(&mut self) {
        if let OpState::Submitted(token) = self.state {
            if let Some(buf) = self.buf.take() {
                with_table(|t| t.detach(token, Box::new(move |_result| drop(buf))));
            }
        }
    }
}

// ====================
// AioWriteFuture
// ====================

/// AIO ベースの書き込み Future（`reactor::tcp::WriteFuture` の `veil_aio` 版）。
pub struct AioWriteFuture<T: IoBuf> {
    fd: RawFd,
    buf: Option<T>,
    state: OpState,
}

impl<T: IoBuf> AioWriteFuture<T> {
    pub(crate) fn new(fd: RawFd, buf: T) -> Self {
        Self {
            fd,
            buf: Some(buf),
            state: OpState::NotSubmitted,
        }
    }
}

impl<T: IoBuf> Future for AioWriteFuture<T> {
    type Output = (io::Result<usize>, T);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        loop {
            match this.state {
                OpState::NotSubmitted => {
                    let buf = this
                        .buf
                        .as_ref()
                        .expect("AioWriteFuture: buffer missing before submit");
                    match try_submit_write(this.fd, buf) {
                        Ok(token) => this.state = OpState::Submitted(token),
                        Err(SubmitOutcome::Fallback) => this.state = OpState::Fallback,
                        Err(SubmitOutcome::Error(e)) => {
                            this.state = OpState::Done;
                            let buf = this.buf.take().expect("buffer present on submit error");
                            return Poll::Ready((Err(e), buf));
                        }
                    }
                }
                OpState::Submitted(token) => {
                    if let Some(result) = with_table(|t| t.take_outcome(token)) {
                        this.state = OpState::Done;
                        let buf = this.buf.take().expect("buffer present at AIO completion");
                        return Poll::Ready((result, buf));
                    }
                    with_table(|t| t.set_waker(token, cx.waker().clone()));
                    return Poll::Pending;
                }
                OpState::Fallback => {
                    // 既存 readiness WriteFuture と同一の try-first ループ
                    // （`reactor::tcp::WriteFuture` 参照。macOS の MSG_NOSIGNAL 分岐は
                    // FreeBSD 専用モジュールのため不要）。
                    let buf = this
                        .buf
                        .as_ref()
                        .expect("AioWriteFuture: buffer missing in fallback");
                    let ret = unsafe {
                        libc::send(
                            this.fd,
                            buf.read_ptr() as *const libc::c_void,
                            buf.bytes_init(),
                            libc::MSG_NOSIGNAL,
                        )
                    };
                    if ret >= 0 {
                        this.state = OpState::Done;
                        let buf = this.buf.take().expect("buffer present at completion");
                        return Poll::Ready((Ok(ret as usize), buf));
                    }
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    if is_would_block(&e) {
                        register_write(this.fd, cx.waker().clone());
                        return Poll::Pending;
                    }
                    this.state = OpState::Done;
                    let buf = this.buf.take().expect("buffer present on error");
                    return Poll::Ready((Err(e), buf));
                }
                OpState::Done => {
                    unreachable!("AioWriteFuture polled after completion")
                }
            }
        }
    }
}

impl<T: IoBuf> Drop for AioWriteFuture<T> {
    fn drop(&mut self) {
        if let OpState::Submitted(token) = self.state {
            if let Some(buf) = self.buf.take() {
                with_table(|t| t.detach(token, Box::new(move |_result| drop(buf))));
            }
        }
    }
}
