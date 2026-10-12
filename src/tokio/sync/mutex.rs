//! `tokio::sync::Mutex` / `RwLock`（セマフォの上に作る。FIFO で公平）。

use super::semaphore::Semaphore;
use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

/// `try_lock` の失敗。
#[derive(Debug)]
pub struct TryLockError(pub(super) ());

impl std::fmt::Display for TryLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("operation would block")
    }
}

impl std::error::Error for TryLockError {}

/// ランタイムのスレッドで同期的に待とうとした。
fn assert_not_in_runtime(what: &str) {
    if crate::runtime::has_driver() {
        panic!(
            "Cannot {} from within a runtime. This happens because a function attempted to block \
             the current thread while the thread is being used to drive asynchronous tasks.",
            what
        );
    }
}

// ===================== Mutex =====================

/// `tokio::sync::Mutex`。
pub struct Mutex<T: ?Sized> {
    s: Semaphore,
    c: UnsafeCell<T>,
}

// SAFETY: 値への参照はセマフォの許可を持つガードだけが作る（tokio と同じ条件）
unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}

impl<T: ?Sized + std::fmt::Debug> std::fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("Mutex");
        match self.try_lock() {
            Ok(g) => d.field("data", &&*g),
            Err(_) => d.field("data", &format_args!("<locked>")),
        };
        d.finish()
    }
}

impl<T: Default> Default for Mutex<T> {
    fn default() -> Self {
        Mutex::new(T::default())
    }
}

impl<T> From<T> for Mutex<T> {
    fn from(v: T) -> Self {
        Mutex::new(v)
    }
}

impl<T> Mutex<T> {
    pub fn new(t: T) -> Mutex<T> {
        Mutex {
            s: Semaphore::new(1),
            c: UnsafeCell::new(t),
        }
    }

    pub const fn const_new(t: T) -> Mutex<T> {
        Mutex {
            s: Semaphore::const_new(1),
            c: UnsafeCell::new(t),
        }
    }

    pub fn into_inner(self) -> T {
        self.c.into_inner()
    }
}

impl<T: ?Sized> Mutex<T> {
    pub async fn lock(&self) -> MutexGuard<'_, T> {
        // 閉じないセマフォなので失敗しない
        let _ = self.s.acquire_raw(1).await;
        MutexGuard { lock: self }
    }

    pub fn try_lock(&self) -> Result<MutexGuard<'_, T>, TryLockError> {
        self.s
            .try_acquire_raw(1)
            .map(|_| MutexGuard { lock: self })
            .map_err(|_| TryLockError(()))
    }

    pub fn blocking_lock(&self) -> MutexGuard<'_, T> {
        assert_not_in_runtime("block_on");
        futures::executor::block_on(self.lock())
    }

    pub async fn lock_owned(self: Arc<Self>) -> OwnedMutexGuard<T> {
        let _ = self.s.acquire_raw(1).await;
        OwnedMutexGuard { lock: self }
    }

    pub fn try_lock_owned(self: Arc<Self>) -> Result<OwnedMutexGuard<T>, TryLockError> {
        match self.s.try_acquire_raw(1) {
            Ok(()) => Ok(OwnedMutexGuard { lock: self }),
            Err(_) => Err(TryLockError(())),
        }
    }

    pub fn blocking_lock_owned(self: Arc<Self>) -> OwnedMutexGuard<T> {
        assert_not_in_runtime("block_on");
        futures::executor::block_on(self.lock_owned())
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.c.get_mut()
    }
}

/// `Mutex::lock` のガード。
#[must_use = "if unused the Mutex will immediately unlock"]
pub struct MutexGuard<'a, T: ?Sized> {
    lock: &'a Mutex<T>,
}

// SAFETY: ガードは許可を持つ間だけ値へ触れる
unsafe impl<T: ?Sized + Sync> Sync for MutexGuard<'_, T> {}

impl<'a, T: ?Sized> MutexGuard<'a, T> {
    /// ガードを部分への参照に写す。
    pub fn map<U: ?Sized, F: FnOnce(&mut T) -> &mut U>(
        this: Self,
        f: F,
    ) -> MappedMutexGuard<'a, U> {
        let s = &this.lock.s;
        let data = f(unsafe { &mut *this.lock.c.get() }) as *mut U;
        std::mem::forget(this);
        MappedMutexGuard {
            s,
            data,
            _p: PhantomData,
        }
    }

    pub fn try_map<U: ?Sized, F: FnOnce(&mut T) -> Option<&mut U>>(
        this: Self,
        f: F,
    ) -> Result<MappedMutexGuard<'a, U>, Self> {
        let data = match f(unsafe { &mut *this.lock.c.get() }) {
            Some(d) => d as *mut U,
            None => return Err(this),
        };
        let s = &this.lock.s;
        std::mem::forget(this);
        Ok(MappedMutexGuard {
            s,
            data,
            _p: PhantomData,
        })
    }

    pub fn mutex(this: &Self) -> &'a Mutex<T> {
        this.lock
    }
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: 許可を持っている
        unsafe { &*self.lock.c.get() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: 許可を持っている
        unsafe { &mut *self.lock.c.get() }
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.s.release(1);
    }
}

impl<T: ?Sized + std::fmt::Debug> std::fmt::Debug for MutexGuard<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized + std::fmt::Display> std::fmt::Display for MutexGuard<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&**self, f)
    }
}

/// `Mutex::lock_owned` のガード。
#[must_use = "if unused the Mutex will immediately unlock"]
pub struct OwnedMutexGuard<T: ?Sized> {
    lock: Arc<Mutex<T>>,
}

impl<T: ?Sized> OwnedMutexGuard<T> {
    pub fn mutex(this: &Self) -> &Arc<Mutex<T>> {
        &this.lock
    }
}

impl<T: ?Sized> Deref for OwnedMutexGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: 許可を持っている
        unsafe { &*self.lock.c.get() }
    }
}

impl<T: ?Sized> DerefMut for OwnedMutexGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: 許可を持っている
        unsafe { &mut *self.lock.c.get() }
    }
}

impl<T: ?Sized> Drop for OwnedMutexGuard<T> {
    fn drop(&mut self) {
        self.lock.s.release(1);
    }
}

impl<T: ?Sized + std::fmt::Debug> std::fmt::Debug for OwnedMutexGuard<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

/// `MutexGuard::map` のガード。
pub struct MappedMutexGuard<'a, T: ?Sized> {
    s: &'a Semaphore,
    data: *mut T,
    _p: PhantomData<&'a mut T>,
}

// SAFETY: 元のガードと同じ条件
unsafe impl<T: ?Sized + Sync> Sync for MappedMutexGuard<'_, T> {}
unsafe impl<'a, T: ?Sized + Send + 'a> Send for MappedMutexGuard<'a, T> {}

impl<T: ?Sized> Deref for MappedMutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: 許可を持っている
        unsafe { &*self.data }
    }
}

impl<T: ?Sized> DerefMut for MappedMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: 許可を持っている
        unsafe { &mut *self.data }
    }
}

impl<T: ?Sized> Drop for MappedMutexGuard<'_, T> {
    fn drop(&mut self) {
        self.s.release(1);
    }
}

// ===================== RwLock =====================

const MAX_READS: u32 = u32::MAX >> 3;

/// `tokio::sync::RwLock`（書き手を優先する FIFO）。
pub struct RwLock<T: ?Sized> {
    mr: u32,
    s: Semaphore,
    c: UnsafeCell<T>,
}

// SAFETY: tokio と同じ条件
unsafe impl<T: ?Sized + Send> Send for RwLock<T> {}
unsafe impl<T: ?Sized + Send + Sync> Sync for RwLock<T> {}

impl<T: ?Sized + std::fmt::Debug> std::fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("RwLock");
        match self.try_read() {
            Ok(g) => d.field("data", &&*g),
            Err(_) => d.field("data", &format_args!("<locked>")),
        };
        d.finish()
    }
}

impl<T: Default> Default for RwLock<T> {
    fn default() -> Self {
        RwLock::new(T::default())
    }
}

impl<T> From<T> for RwLock<T> {
    fn from(v: T) -> Self {
        RwLock::new(v)
    }
}

impl<T> RwLock<T> {
    pub fn new(value: T) -> RwLock<T> {
        RwLock {
            mr: MAX_READS,
            s: Semaphore::new(MAX_READS as usize),
            c: UnsafeCell::new(value),
        }
    }

    pub const fn const_new(value: T) -> RwLock<T> {
        RwLock {
            mr: MAX_READS,
            s: Semaphore::const_new(MAX_READS as usize),
            c: UnsafeCell::new(value),
        }
    }

    pub const fn const_with_max_readers(value: T, max_reads: u32) -> RwLock<T> {
        assert!(max_reads <= MAX_READS);
        RwLock {
            mr: max_reads,
            s: Semaphore::const_new(max_reads as usize),
            c: UnsafeCell::new(value),
        }
    }

    pub fn with_max_readers(value: T, max_reads: u32) -> RwLock<T> {
        assert!(
            max_reads <= MAX_READS,
            "a RwLock may not be created with more than {} readers",
            MAX_READS
        );
        RwLock {
            mr: max_reads,
            s: Semaphore::new(max_reads as usize),
            c: UnsafeCell::new(value),
        }
    }

    pub fn into_inner(self) -> T {
        self.c.into_inner()
    }
}

impl<T: ?Sized> RwLock<T> {
    pub async fn read(&self) -> RwLockReadGuard<'_, T> {
        let _ = self.s.acquire_raw(1).await;
        RwLockReadGuard {
            s: &self.s,
            data: self.c.get(),
            _p: PhantomData,
        }
    }

    pub fn try_read(&self) -> Result<RwLockReadGuard<'_, T>, TryLockError> {
        self.s.try_acquire_raw(1).map_err(|_| TryLockError(()))?;
        Ok(RwLockReadGuard {
            s: &self.s,
            data: self.c.get(),
            _p: PhantomData,
        })
    }

    pub fn blocking_read(&self) -> RwLockReadGuard<'_, T> {
        assert_not_in_runtime("block_on");
        futures::executor::block_on(self.read())
    }

    pub async fn write(&self) -> RwLockWriteGuard<'_, T> {
        let _ = self.s.acquire_raw(self.mr as usize).await;
        RwLockWriteGuard {
            s: &self.s,
            permits: self.mr,
            data: self.c.get(),
            _p: PhantomData,
        }
    }

    pub fn try_write(&self) -> Result<RwLockWriteGuard<'_, T>, TryLockError> {
        self.s
            .try_acquire_raw(self.mr as usize)
            .map_err(|_| TryLockError(()))?;
        Ok(RwLockWriteGuard {
            s: &self.s,
            permits: self.mr,
            data: self.c.get(),
            _p: PhantomData,
        })
    }

    pub fn blocking_write(&self) -> RwLockWriteGuard<'_, T> {
        assert_not_in_runtime("block_on");
        futures::executor::block_on(self.write())
    }

    pub async fn read_owned(self: Arc<Self>) -> OwnedRwLockReadGuard<T> {
        let _ = self.s.acquire_raw(1).await;
        OwnedRwLockReadGuard { lock: self }
    }

    pub fn try_read_owned(self: Arc<Self>) -> Result<OwnedRwLockReadGuard<T>, TryLockError> {
        match self.s.try_acquire_raw(1) {
            Ok(()) => Ok(OwnedRwLockReadGuard { lock: self }),
            Err(_) => Err(TryLockError(())),
        }
    }

    pub async fn write_owned(self: Arc<Self>) -> OwnedRwLockWriteGuard<T> {
        let _ = self.s.acquire_raw(self.mr as usize).await;
        OwnedRwLockWriteGuard { lock: self }
    }

    pub fn try_write_owned(self: Arc<Self>) -> Result<OwnedRwLockWriteGuard<T>, TryLockError> {
        match self.s.try_acquire_raw(self.mr as usize) {
            Ok(()) => Ok(OwnedRwLockWriteGuard { lock: self }),
            Err(_) => Err(TryLockError(())),
        }
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.c.get_mut()
    }
}

/// 読み手のガード。
#[must_use = "if unused the RwLock will immediately unlock"]
pub struct RwLockReadGuard<'a, T: ?Sized> {
    s: &'a Semaphore,
    data: *const T,
    _p: PhantomData<&'a T>,
}

// SAFETY: 共有参照と同じ条件
unsafe impl<T: ?Sized + Sync> Send for RwLockReadGuard<'_, T> {}
unsafe impl<T: ?Sized + Sync> Sync for RwLockReadGuard<'_, T> {}

impl<'a, T: ?Sized> RwLockReadGuard<'a, T> {
    pub fn map<U: ?Sized, F: FnOnce(&T) -> &U>(this: Self, f: F) -> RwLockReadGuard<'a, U> {
        let data = f(unsafe { &*this.data }) as *const U;
        let s = this.s;
        std::mem::forget(this);
        RwLockReadGuard {
            s,
            data,
            _p: PhantomData,
        }
    }

    pub fn try_map<U: ?Sized, F: FnOnce(&T) -> Option<&U>>(
        this: Self,
        f: F,
    ) -> Result<RwLockReadGuard<'a, U>, Self> {
        let data = match f(unsafe { &*this.data }) {
            Some(d) => d as *const U,
            None => return Err(this),
        };
        let s = this.s;
        std::mem::forget(this);
        Ok(RwLockReadGuard {
            s,
            data,
            _p: PhantomData,
        })
    }
}

impl<T: ?Sized> Deref for RwLockReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: 読みの許可を持っている
        unsafe { &*self.data }
    }
}

impl<T: ?Sized> Drop for RwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        self.s.release(1);
    }
}

impl<T: ?Sized + std::fmt::Debug> std::fmt::Debug for RwLockReadGuard<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

/// 書き手のガード。
#[must_use = "if unused the RwLock will immediately unlock"]
pub struct RwLockWriteGuard<'a, T: ?Sized> {
    s: &'a Semaphore,
    permits: u32,
    data: *mut T,
    _p: PhantomData<&'a mut T>,
}

// SAFETY: 可変参照と同じ条件
unsafe impl<T: ?Sized + Send + Sync> Send for RwLockWriteGuard<'_, T> {}
unsafe impl<T: ?Sized + Send + Sync> Sync for RwLockWriteGuard<'_, T> {}

impl<'a, T: ?Sized> RwLockWriteGuard<'a, T> {
    /// 書きの許可を読み 1 つに落とす（他の読み手を通す）。
    pub fn downgrade(self) -> RwLockReadGuard<'a, T> {
        let s = self.s;
        let data = self.data as *const T;
        let release = self.permits as usize - 1;
        std::mem::forget(self);
        s.release(release);
        RwLockReadGuard {
            s,
            data,
            _p: PhantomData,
        }
    }

    /// 部分への参照に写して読みに落とす。
    pub fn downgrade_map<U: ?Sized, F: FnOnce(&T) -> &U>(
        this: Self,
        f: F,
    ) -> RwLockReadGuard<'a, U> {
        let data = f(unsafe { &*this.data }) as *const U;
        let (s, permits) = (this.s, this.permits);
        std::mem::forget(this);
        s.release(permits as usize - 1);
        RwLockReadGuard {
            s,
            data,
            _p: PhantomData,
        }
    }

    /// `f` が `Some` を返したら写して読みに落とす（`None` ならガードを返す）。
    pub fn try_downgrade_map<U: ?Sized, F: FnOnce(&T) -> Option<&U>>(
        this: Self,
        f: F,
    ) -> Result<RwLockReadGuard<'a, U>, Self> {
        let data = match f(unsafe { &*this.data }) {
            Some(d) => d as *const U,
            None => return Err(this),
        };
        let (s, permits) = (this.s, this.permits);
        std::mem::forget(this);
        s.release(permits as usize - 1);
        Ok(RwLockReadGuard {
            s,
            data,
            _p: PhantomData,
        })
    }

    pub fn map<U: ?Sized, F: FnOnce(&mut T) -> &mut U>(
        mut this: Self,
        f: F,
    ) -> RwLockMappedWriteGuard<'a, U> {
        let data = f(&mut *this) as *mut U;
        let (s, permits) = (this.s, this.permits);
        std::mem::forget(this);
        RwLockMappedWriteGuard {
            s,
            permits,
            data,
            _p: PhantomData,
        }
    }
}

impl<T: ?Sized> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: 書きの許可を持っている
        unsafe { &*self.data }
    }
}

impl<T: ?Sized> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: 書きの許可を持っている
        unsafe { &mut *self.data }
    }
}

impl<T: ?Sized> Drop for RwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        self.s.release(self.permits as usize);
    }
}

impl<T: ?Sized + std::fmt::Debug> std::fmt::Debug for RwLockWriteGuard<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

/// `RwLockWriteGuard::map` のガード。
pub struct RwLockMappedWriteGuard<'a, T: ?Sized> {
    s: &'a Semaphore,
    permits: u32,
    data: *mut T,
    _p: PhantomData<&'a mut T>,
}

// SAFETY: 可変参照と同じ条件
unsafe impl<T: ?Sized + Send + Sync> Send for RwLockMappedWriteGuard<'_, T> {}
unsafe impl<T: ?Sized + Send + Sync> Sync for RwLockMappedWriteGuard<'_, T> {}

impl<T: ?Sized> Deref for RwLockMappedWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: 書きの許可を持っている
        unsafe { &*self.data }
    }
}

impl<T: ?Sized> DerefMut for RwLockMappedWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: 書きの許可を持っている
        unsafe { &mut *self.data }
    }
}

impl<T: ?Sized> Drop for RwLockMappedWriteGuard<'_, T> {
    fn drop(&mut self) {
        self.s.release(self.permits as usize);
    }
}

/// 所有の読み手のガード。
pub struct OwnedRwLockReadGuard<T: ?Sized> {
    lock: Arc<RwLock<T>>,
}

impl<T: ?Sized> Deref for OwnedRwLockReadGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: 読みの許可を持っている
        unsafe { &*self.lock.c.get() }
    }
}

impl<T: ?Sized> Drop for OwnedRwLockReadGuard<T> {
    fn drop(&mut self) {
        self.lock.s.release(1);
    }
}

/// 所有の書き手のガード。
pub struct OwnedRwLockWriteGuard<T: ?Sized> {
    lock: Arc<RwLock<T>>,
}

impl<T: ?Sized> OwnedRwLockWriteGuard<T> {
    pub fn downgrade(self) -> OwnedRwLockReadGuard<T> {
        let this = std::mem::ManuallyDrop::new(self);
        // SAFETY: Drop を走らせずに Arc を取り出す（this はこの後使わない）
        let lock = unsafe { std::ptr::read(&this.lock) };
        let release = lock.mr as usize - 1;
        lock.s.release(release);
        OwnedRwLockReadGuard { lock }
    }
}

impl<T: ?Sized> Deref for OwnedRwLockWriteGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: 書きの許可を持っている
        unsafe { &*self.lock.c.get() }
    }
}

impl<T: ?Sized> DerefMut for OwnedRwLockWriteGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: 書きの許可を持っている
        unsafe { &mut *self.lock.c.get() }
    }
}

impl<T: ?Sized> Drop for OwnedRwLockWriteGuard<T> {
    fn drop(&mut self) {
        self.lock.s.release(self.lock.mr as usize);
    }
}
