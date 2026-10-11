//! アロケータのアイドル時回収（F-183）。
//!
//! どのアロケータも、確保・解放が止まったスレッドでは空き領域の OS への返却（purge /
//! decay）が進まない（mimalloc・jemalloc はアロケータ呼び出しのついでに期限を見るため）。
//! 負荷のピーク後にワーカーがアイドルになると RSS がピーク付近に張り付いたままになるので、
//! ワーカーのイベントループのアイドル点（accept の 1 秒タイムアウト、HTTP/3 の
//! タイムアウト）から [`idle_tick`] を呼び、使っているアロケータに回収させる。
//!
//! - veil-alloc: 空ページの返却・期限切れ purge・放棄ヒープの掃除（`veil_alloc::collect`）
//! - mimalloc: `mi_collect(false)`（スレッドのヒープの回収と期限切れ purge）
//! - FreeBSD のシステム malloc（libc の jemalloc）: スレッドキャッシュの flush と
//!   全アリーナの decay（`mallctl`）
//!
//! 呼び出しはスレッドごとに `MIN_INTERVAL` 以上の間隔に間引く（HTTP/3 のタイムアウトは
//! 100ms 周期で来るため）。データプレーンの要求ごとの経路からは呼ばない。

use std::cell::Cell;
use std::time::{Duration, Instant};

/// スレッドごとの最短呼び出し間隔。
const MIN_INTERVAL: Duration = Duration::from_secs(1);

thread_local! {
    static LAST_TICK: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// ワーカーのアイドル点から呼ぶ。間隔内の再呼び出しは何もしない。
pub(crate) fn idle_tick() {
    let now = Instant::now();
    let due = LAST_TICK.with(|t| match t.get() {
        Some(last) if now.duration_since(last) < MIN_INTERVAL => false,
        _ => {
            t.set(Some(now));
            true
        }
    });
    if due {
        collect_now();
    }
}

#[cfg(veil_alloc_active)]
fn collect_now() {
    crate::veil_alloc::collect(false);
}

#[cfg(all(not(veil_alloc_active), feature = "mimalloc"))]
fn collect_now() {
    // SAFETY: mi_collect はスレッドのヒープを回収するだけで、引数の制約は無い。
    unsafe { libmimalloc_sys::mi_collect(false) };
}

#[cfg(all(
    not(veil_alloc_active),
    not(feature = "mimalloc"),
    target_os = "freebsd"
))]
fn collect_now() {
    crate::system::freebsd_malloc_idle();
}

#[cfg(all(
    not(veil_alloc_active),
    not(feature = "mimalloc"),
    not(target_os = "freebsd")
))]
fn collect_now() {}
