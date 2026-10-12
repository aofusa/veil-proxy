//! `tokio::sync` 互換（F-198）。ランタイムに依存しない同期プリミティブ。待っている Future の Waker は
//! スレッドセーフに包んで持つ（`waker::WakerSlot`）ので、別スレッドから起こしても所有スレッドで再開する。

pub mod broadcast;
pub mod mpsc;
mod mutex;
mod notify;
pub mod oneshot;
mod semaphore;
pub mod watch;

pub use mutex::{
    MappedMutexGuard, Mutex, MutexGuard, OwnedMutexGuard, OwnedRwLockReadGuard,
    OwnedRwLockWriteGuard, RwLock, RwLockMappedWriteGuard, RwLockReadGuard, RwLockWriteGuard,
    TryLockError,
};
pub use notify::{Barrier, BarrierWaitResult, Notify, OnceCell, SetError, SetOnce, SetOnceError};
pub use semaphore::{
    AcquireError, OwnedSemaphorePermit, Semaphore, SemaphorePermit, TryAcquireError,
};

/// `tokio::sync::futures`。
pub mod futures {
    pub use super::notify::{Notified, OwnedNotified};
}
