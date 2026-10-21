// F-198
//! Compatible with `tokio::sync`: runtime-independent synchronisation primitives. The wakers of waiting futures
//! are stored wrapped to be thread-safe (`waker::WakerSlot`), so a wake-up from another thread still resumes the
//! future on its owning thread.

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
