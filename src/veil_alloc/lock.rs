//! アロケータ内部のグローバルロック（F-183）。
//!
//! `std::sync::Mutex` はプラットフォームによっては遅延確保や pthread を経由するため、
//! アロケータの内側では使わない。保護する区間はどれも短い（リストの付け替え、セグメント
//! キャッシュの出し入れ）か稀（スレッド終了時の放棄、アイドル時の回収）なので、
//! 短いスピンのあと `sched_yield` で譲るロックで足りる。データプレーンの高速パス
//! （スレッドローカルなブロックの確保・解放）はロックを一切取らない。

use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, Ordering};

use super::os;

pub(crate) struct SpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

// SAFETY: data へのアクセスは locked の獲得（Acquire）〜解放（Release）の間だけ。
unsafe impl<T: Send> Sync for SpinLock<T> {}
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub(crate) const fn new(v: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(v),
        }
    }

    #[inline]
    pub(crate) fn lock(&self) -> Guard<'_, T> {
        self.raw_lock();
        Guard { lock: self }
    }

    #[inline]
    pub(crate) fn try_lock(&self) -> Option<Guard<'_, T>> {
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(Guard { lock: self })
        } else {
            None
        }
    }

    /// ガード無しで獲得する（`fork` 前ハンドラ用。対の `raw_unlock` を必ず呼ぶ）。
    pub(crate) fn raw_lock(&self) {
        let mut spins = 0u32;
        loop {
            if !self.locked.load(Ordering::Relaxed)
                && self
                    .locked
                    .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            {
                return;
            }
            spins += 1;
            if spins < 64 {
                spin_loop();
            } else {
                os::yield_now();
            }
        }
    }

    /// `raw_lock` の解放（`fork` 後ハンドラ用）。
    pub(crate) fn raw_unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }
}

pub(crate) struct Guard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> core::ops::Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: ロック保持中は排他。
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> core::ops::DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: ロック保持中は排他。
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        self.lock.raw_unlock();
    }
}
