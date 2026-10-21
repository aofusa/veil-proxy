// F-183: スピンロックで足りる理由 — 保護区間が短いか稀で、高速パスはロックを取らない。
//! The allocator's internal global lock.
//!
//! `std::sync::Mutex` may allocate lazily or go through pthread on some platforms, so it is not
//! used inside the allocator. Every protected section is either short (relinking lists, putting
//! segments into or taking them out of the cache) or rare (abandoning a heap at thread exit,
//! idle-time collection), so a lock that spins briefly and then yields with `sched_yield` is
//! enough. The data-plane fast path (allocating and freeing thread-local blocks) never takes
//! a lock.

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

    /// Acquires the lock without a guard (for the pre-`fork` handler; always call the matching `raw_unlock`).
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

    /// Releases the lock taken by `raw_lock` (for the post-`fork` handler).
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
