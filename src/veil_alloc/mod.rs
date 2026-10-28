// F-183: veil 専用アロケータ（`veil-alloc` feature）。既定値は計測で決めた（F-183 チケット参照）。
//! The veil-specific global allocator (`veil-alloc` feature).
//!
//! A thread-local, size-class allocator with page-granular free lists, designed for
//! thread-per-core workers. Key design points:
//!
//! - **The fast path is only a pop / push with no locks, atomics or hard-to-predict branches**
//!   (`heap`). `GlobalAlloc` passes the size and alignment on deallocation too, so the size
//!   class is computed directly from the `Layout`; there are no per-block headers and no
//!   pointer-to-size table.
//! - **Stable RSS for long-running processes**: free spans are found through page (span)
//!   granular free lists and returned to the OS after a delay (`purge_delay_ms`). A cap on
//!   dirty memory, a cap on cached empty segments and an immediate `munmap` for huge
//!   allocations bring RSS back down after a load peak. The worker idle hook ([`collect`])
//!   also returns memory for threads that have stopped allocating.
//! - **Robust against threads coming and going**: the heap of an exited thread is cleaned up
//!   and put on an abandoned list, and the next thread takes it over as is, so memory does
//!   not keep growing when threads (for example offload threads) are replaced.
//! - **Frees from other threads** (loggers, offload threads, hand-offs over channels) are
//!   pushed lock-free to the owning heap, and the owner reclaims them in bulk on its slow path.
//! - **fork safety**: the global lock is acquired before and released after `fork` through
//!   `pthread_atfork`.
//!
//! Memory layout: segments aligned to 4 MiB (64 slots of 64 KiB, with the header in the first
//! slot between inaccessible guard regions). Requests of up to 256 KiB use size classes (52 classes), requests of up to 1 MiB use
//! large spans made of whole slots, and anything bigger is mapped directly from the OS.
//!
//! # Tuning
//!
//! The following environment variables override the defaults at startup:
//!
//! | Environment variable | Default | Meaning |
//! |---|---|---|
//! | `VEIL_ALLOC_PURGE_DELAY_MS` | 1000 | Delay before a free span is returned to the OS |
//! | `VEIL_ALLOC_DIRTY_MAX_MB` | 16 | Upper bound on the amount each heap may keep waiting to be purged |
//! | `VEIL_ALLOC_SEGMENT_CACHE` | 8 | Process-wide number of cached empty (purged) 4 MiB segments |
//!
//! # Platform support
//!
//! Unix (Linux, FreeBSD, OpenBSD, NetBSD, macOS). It is not used on Windows, where the crate
//! falls back to another allocator.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::Ordering;

pub(crate) mod heap;
mod huge;
mod lock;
mod os;
pub(crate) mod segment;
pub(crate) mod size_class;

use size_class::{class_of, class_of_aligned, CLASS_SIZE, MAX_CLASS_SIZE, MIN_ALIGN};

/// A segment (4 MiB, aligned to its own size).
pub(crate) const SEGMENT_SIZE: usize = 1 << 22;
/// A slot (64 KiB), the unit of spans and of purging.
pub(crate) const SLOT_SHIFT: usize = 16;
pub(crate) const SLOT_SIZE: usize = 1 << SLOT_SHIFT;
pub(crate) const SLOTS: usize = SEGMENT_SIZE / SLOT_SIZE;

/// The veil-specific global allocator.
pub struct VeilAlloc;

/// Upper bound of a large allocation; anything bigger is huge. It fits in the 63 usable slots of one segment.
pub(crate) const LARGE_MAX: usize = 1 << 20;

/// The kind of an allocation. Allocation and deallocation decide it from the `Layout` with the same function.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A size class (256 KiB or less).
    Small(usize),
    /// A number of slots (more than 256 KiB up to `LARGE_MAX`, alignment of 64 KiB or less).
    Large(usize),
    /// Mapped directly from the OS.
    Huge,
}

#[inline(always)]
fn classify(layout: &Layout) -> Kind {
    let size = layout.size();
    let align = layout.align();
    if align <= MIN_ALIGN && size <= MAX_CLASS_SIZE {
        return Kind::Small(class_of(size));
    }
    classify_slow(size, align)
}

#[cold]
fn classify_slow(size: usize, align: usize) -> Kind {
    if align > SLOT_SIZE {
        return Kind::Huge;
    }
    if align > MIN_ALIGN {
        if let Some(c) = class_of_aligned(size, align) {
            return Kind::Small(c);
        }
    }
    if size <= LARGE_MAX {
        Kind::Large(size.div_ceil(SLOT_SIZE))
    } else {
        Kind::Huge
    }
}

// SAFETY: 返すブロックは layout のサイズ以上・アライメントを満たし、解放されるまで
// 他の確保と重ならない。解放は同じ layout で同じ分類を通る。
unsafe impl GlobalAlloc for VeilAlloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match classify(&layout) {
            // SAFETY: class は有効なサイズクラス。
            Kind::Small(c) => unsafe { heap::alloc(c) },
            // SAFETY: slots は 1..=16。
            Kind::Large(n) => unsafe { heap::alloc_large(n, 0) },
            Kind::Huge => huge::alloc(layout.size(), layout.align()),
        }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        match classify(&layout) {
            // SAFETY: class は有効なサイズクラス。
            Kind::Small(c) => unsafe { heap::alloc_zeroed(c, layout.size()) },
            // SAFETY: slots は 1..=16。
            Kind::Large(n) => unsafe { heap::alloc_large(n, layout.size().max(1)) },
            // 新規マッピングはゼロ。
            Kind::Huge => huge::alloc(layout.size(), layout.align()),
        }
    }

    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        match classify(&layout) {
            // SAFETY: p は同じ layout で確保されたブロック。
            Kind::Small(_) => unsafe { heap::free(p) },
            // SAFETY: p は同じ layout で確保された large ブロック。
            Kind::Large(_) => unsafe { heap::free_large(p) },
            Kind::Huge => huge::free(p, layout.size()),
        }
    }

    #[inline]
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: GlobalAlloc::realloc の契約により new_size は align で丸めても isize::MAX 以下。
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        let old_kind = classify(&layout);
        let new_kind = classify(&new_layout);
        if old_kind == new_kind {
            match old_kind {
                Kind::Huge => {
                    let q = huge::realloc(p, layout.size(), new_size, layout.align());
                    if !q.is_null() {
                        return q;
                    }
                }
                _ => return p,
            }
        }
        // SAFETY: 新しいブロックを確保して内容を移し、古いブロックを解放する。
        unsafe {
            let q = self.alloc(new_layout);
            if !q.is_null() {
                ptr::copy_nonoverlapping(p, q, layout.size().min(new_size));
                self.dealloc(p, layout);
            }
            q
        }
    }
}

/// The block size of the class that serves `layout` (for tests and diagnostics).
pub fn usable_size(layout: Layout) -> usize {
    match classify(&layout) {
        Kind::Small(c) => CLASS_SIZE[c] as usize,
        Kind::Large(n) => n * SLOT_SIZE,
        Kind::Huge => layout.size(),
    }
}

/// Idle-time reclamation.
///
/// For the current thread's heap, this collects frees from other threads, returns empty pages,
/// purges expired memory (everything if `force` is set) and returns surplus empty segments.
/// Abandoned heaps and the shared heap are cleaned the same way when their lock can be taken.
/// Call it from a worker's idle point. It is also correct when called under load; its cost is
/// proportional to the number of free spans and owned segments, not the number of blocks.
pub fn collect(force: bool) {
    heap::collect(force);
}

/// Allocator state (process-wide approximate values, updated on the slow path).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Number of segments mapped from the OS, including cached ones.
    pub segments: usize,
    /// Number of empty segments in the reuse cache.
    pub cached_segments: usize,
    /// Bytes used by spans.
    pub active_bytes: usize,
    /// Bytes that are free but not yet returned to the OS (waiting to be purged).
    pub dirty_bytes: usize,
    /// Bytes of huge allocations (mapped directly from the OS).
    pub huge_bytes: usize,
    /// Total bytes purged so far.
    pub purged_bytes_total: usize,
    /// Number of heaps, including abandoned ones.
    pub heaps: usize,
    /// Number of abandoned heaps.
    pub abandoned_heaps: usize,
    /// Total number of frees from other threads.
    pub remote_frees_total: usize,
}

pub fn stats() -> Stats {
    Stats {
        segments: stats::SEGMENTS.load(Ordering::Relaxed),
        cached_segments: stats::CACHED_SEGMENTS.load(Ordering::Relaxed),
        active_bytes: stats::ACTIVE_SLOTS.load(Ordering::Relaxed) * SLOT_SIZE,
        dirty_bytes: stats::DIRTY_SLOTS.load(Ordering::Relaxed) * SLOT_SIZE,
        huge_bytes: stats::HUGE_BYTES.load(Ordering::Relaxed),
        purged_bytes_total: stats::PURGED_SLOTS.load(Ordering::Relaxed) * SLOT_SIZE,
        heaps: stats::HEAPS.load(Ordering::Relaxed),
        abandoned_heaps: stats::ABANDONED_HEAPS.load(Ordering::Relaxed),
        remote_frees_total: stats::REMOTE_FREES.load(Ordering::Relaxed),
    }
}

pub(crate) mod stats {
    use core::sync::atomic::AtomicUsize;

    pub(crate) static SEGMENTS: AtomicUsize = AtomicUsize::new(0);
    pub(crate) static CACHED_SEGMENTS: AtomicUsize = AtomicUsize::new(0);
    pub(crate) static ACTIVE_SLOTS: AtomicUsize = AtomicUsize::new(0);
    pub(crate) static DIRTY_SLOTS: AtomicUsize = AtomicUsize::new(0);
    pub(crate) static HUGE_BYTES: AtomicUsize = AtomicUsize::new(0);
    pub(crate) static PURGED_SLOTS: AtomicUsize = AtomicUsize::new(0);
    pub(crate) static HEAPS: AtomicUsize = AtomicUsize::new(0);
    pub(crate) static ABANDONED_HEAPS: AtomicUsize = AtomicUsize::new(0);
    pub(crate) static REMOTE_FREES: AtomicUsize = AtomicUsize::new(0);
}

/// Reads the tuning values from the environment (once, when the first heap is created; does not allocate).
pub(crate) fn read_env_tuning() {
    fn env_usize(name: &core::ffi::CStr) -> Option<usize> {
        // SAFETY: NUL 終端の静的文字列。getenv はアロケーションしない。
        let v = unsafe { libc::getenv(name.as_ptr()) };
        if v.is_null() {
            return None;
        }
        // SAFETY: getenv の戻り値は NUL 終端。
        let bytes = unsafe { core::ffi::CStr::from_ptr(v) }.to_bytes();
        let mut n: usize = 0;
        if bytes.is_empty() {
            return None;
        }
        for &b in bytes {
            if !b.is_ascii_digit() {
                return None;
            }
            n = n.checked_mul(10)?.checked_add((b - b'0') as usize)?;
        }
        Some(n)
    }
    if let Some(v) = env_usize(c"VEIL_ALLOC_PURGE_DELAY_MS") {
        heap::PURGE_DELAY_MS.store(v.min(3_600_000), Ordering::Relaxed);
    }
    if let Some(v) = env_usize(c"VEIL_ALLOC_DIRTY_MAX_MB") {
        heap::DIRTY_MAX_SLOTS.store(v.saturating_mul(16), Ordering::Relaxed);
    }
    if let Some(v) = env_usize(c"VEIL_ALLOC_SEGMENT_CACHE") {
        segment::SEG_CACHE_MAX.store(v, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests;
