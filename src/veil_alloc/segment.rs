// F-183
//! Segments and spans.
//!
//! A segment is a 4 MiB mapping aligned to `SEGMENT_SIZE` (4 MiB) and divided into 64 slots of
//! 64 KiB. The header ([`Segment`]) sits at the start of slot 0, and the remaining 63 slots
//! are assigned to spans (contiguous slots that blocks of one size class are carved from).
//! The segment a pointer `p` belongs to is found with `p & !(SEGMENT_SIZE - 1)`.
//!
//! A segment is owned by exactly one heap ([`Heap`]) at any time, and only the owner touches the
//! mutable state of the header (except `owner`). Frees from other threads are pushed onto the
//! owning heap's `remote` list.
//!
//! State of the free slots (bit masks):
//! - `free_mask`: not used by any span
//! - `dirty_mask`: free, but physical pages may remain (waiting to be purged)
//! - `zero_mask`: known to be zero-filled (never used, or purged on Linux)

use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use super::heap::Heap;
use super::lock::SpinLock;
use super::{os, stats, SEGMENT_SIZE, SLOTS, SLOT_SHIFT, SLOT_SIZE};

/// A free-list node (an intrusive list stored in the first 8 bytes of the free block itself).
#[repr(C)]
pub(crate) struct Block {
    pub(crate) next: *mut Block,
}

/// Page flag: the page has no free blocks and is therefore off the heap queue.
pub(crate) const PAGE_FULL: u8 = 1;
/// Page flag: every block on the free list is zero (except the 8-byte `next` at its start).
pub(crate) const PAGE_FRESH_FREE: u8 = 2;
/// Page flag: everything from `capacity` on (the part not carved yet) is zero.
pub(crate) const PAGE_ZERO_TAIL: u8 = 4;
/// Page flag: the page is kept on the queue while empty (counted in `Heap::retained`).
pub(crate) const PAGE_RETAINED: u8 = 8;

/// A span descriptor. There are as many as there are slots in the segment header, and only the
/// one of the span's first slot is used.
///
/// The fast path touches only `free`, `used` and `flags` (the first 16 bytes).
#[repr(C)]
pub(crate) struct Page {
    pub(crate) free: *mut Block,
    /// Number of blocks in use (including blocks freed by other threads that are pending in `remote`).
    pub(crate) used: u32,
    pub(crate) flags: u8,
    pub(crate) class: u8,
    /// Number of slots in the span (0 means this is not the first slot of a span).
    pub(crate) slots: u8,
    _pad: u8,
    /// Number of blocks carved so far.
    pub(crate) capacity: u32,
    /// Number of blocks that fit in the span.
    pub(crate) reserved: u32,
    pub(crate) block_size: u32,
    /// Threshold of used blocks at which a full page returns to the queue (when the free blocks reach 1/8 of the block count).
    pub(crate) requeue_at: u32,
    pub(crate) start: *mut u8,
    pub(crate) prev: *mut Page,
    pub(crate) next: *mut Page,
}

impl Page {
    pub(crate) const fn empty() -> Self {
        Self {
            free: ptr::null_mut(),
            used: 0,
            flags: 0,
            class: 0,
            slots: 0,
            _pad: 0,
            capacity: 0,
            reserved: 0,
            block_size: 0,
            requeue_at: 0,
            start: ptr::null_mut(),
            prev: ptr::null_mut(),
            next: ptr::null_mut(),
        }
    }
}

/// Mask of slots 1..63 (slot 0 holds the header).
pub(crate) const ALL_SLOTS: u64 = !1u64;

#[repr(C)]
pub(crate) struct Segment {
    /// The owning heap. Read by other threads' frees to decide where to post (constant while a block is live).
    pub(crate) owner: AtomicPtr<Heap>,
    pub(crate) prev: *mut Segment,
    pub(crate) next: *mut Segment,
    pub(crate) free_mask: u64,
    pub(crate) dirty_mask: u64,
    pub(crate) zero_mask: u64,
    /// Time at which a slot became dirty (`os::now_ms`).
    pub(crate) dirty_time: [u32; SLOTS],
    /// Slot to first slot of the span it belongs to.
    pub(crate) slot_head: [u8; SLOTS],
    pub(crate) pages: [Page; SLOTS],
}

const _: () = assert!(core::mem::size_of::<Segment>() <= 4096);
const _: () = assert!(core::mem::size_of::<Page>() == 56);

/// The segment a pointer belongs to.
#[inline(always)]
pub(crate) fn segment_of<T>(p: *const T) -> *mut Segment {
    (p as usize & !(SEGMENT_SIZE - 1)) as *mut Segment
}

/// Index of a page descriptor (equal to the number of the span's first slot).
#[inline]
pub(crate) fn page_index(seg: *mut Segment, page: *mut Page) -> usize {
    // SAFETY: page は seg.pages の要素。
    (page as usize - unsafe { (*seg).pages.as_ptr() } as usize) / core::mem::size_of::<Page>()
}

/// The page descriptor of the span a pointer belongs to.
#[inline(always)]
pub(crate) unsafe fn page_of(seg: *mut Segment, p: *const u8) -> *mut Page {
    let idx = (p as usize - seg as usize) >> SLOT_SHIFT;
    // SAFETY: idx < SLOTS（セグメント内のポインタ）。slot_head は使用中スロットでは有効。
    unsafe {
        let head = *(*seg).slot_head.get_unchecked(idx) as usize;
        (*seg).pages.as_mut_ptr().add(head)
    }
}

#[inline]
const fn run_mask(first: usize, n: usize) -> u64 {
    (if n >= 64 { u64::MAX } else { (1u64 << n) - 1 }) << first
}

/// The set of start positions of runs of `n` consecutive set bits in `mask`.
#[inline]
fn runs_of(mask: u64, n: usize) -> u64 {
    let mut m = mask;
    let mut k = 1;
    while k < n && m != 0 {
        m &= mask >> k;
        k += 1;
    }
    m
}

impl Segment {
    /// Finds a run of `n` consecutive free slots. Runs whose physical pages remain (dirty) are
    /// preferred, which avoids page faults and keeps RSS on pages that are already resident.
    pub(crate) fn find_run(&self, n: usize) -> Option<usize> {
        let dirty = runs_of(self.free_mask & self.dirty_mask, n);
        if dirty != 0 {
            return Some(dirty.trailing_zeros() as usize);
        }
        let any = runs_of(self.free_mask, n);
        if any != 0 {
            Some(any.trailing_zeros() as usize)
        } else {
            None
        }
    }

    /// Marks slots `first..first+n` as in use by a span. Returns true if their contents are zero.
    pub(crate) fn take_run(&mut self, first: usize, n: usize) -> bool {
        let m = run_mask(first, n);
        debug_assert_eq!(self.free_mask & m, m);
        let zero = self.zero_mask & m == m;
        let was_dirty = (self.dirty_mask & m).count_ones() as usize;
        self.free_mask &= !m;
        self.dirty_mask &= !m;
        self.zero_mask &= !m;
        stats::DIRTY_SLOTS.fetch_sub(was_dirty, Ordering::Relaxed);
        stats::ACTIVE_SLOTS.fetch_add(n, Ordering::Relaxed);
        for i in first..first + n {
            self.slot_head[i] = first as u8;
        }
        zero
    }

    /// Returns a span to the free state (as dirty, waiting to be purged).
    pub(crate) fn give_run(&mut self, first: usize, n: usize, now: u32) {
        let m = run_mask(first, n);
        debug_assert_eq!(self.free_mask & m, 0);
        self.free_mask |= m;
        self.dirty_mask |= m;
        for t in &mut self.dirty_time[first..first + n] {
            *t = now;
        }
        stats::ACTIVE_SLOTS.fetch_sub(n, Ordering::Relaxed);
        stats::DIRTY_SLOTS.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.free_mask == ALL_SLOTS
    }

    /// Purges slots that have been dirty for at least `delay_ms` (all dirty slots if `force` is set).
    /// Returns (the number of slots purged, the earliest deadline among the dirty slots left).
    pub(crate) fn purge(&mut self, now: u32, delay_ms: u32, force: bool) -> (usize, Option<u32>) {
        let mut todo = 0u64;
        let mut earliest: Option<u32> = None;
        let mut m = self.dirty_mask;
        while m != 0 {
            let i = m.trailing_zeros() as usize;
            m &= m - 1;
            let deadline = self.dirty_time[i].wrapping_add(delay_ms);
            if force || os::time_reached(now, deadline) {
                todo |= 1 << i;
            } else {
                earliest = Some(match earliest {
                    Some(e) if !os::time_reached(deadline, e) => deadline,
                    Some(e) => e,
                    None => deadline,
                });
            }
        }
        if todo == 0 {
            return (0, earliest);
        }
        // 連続する並びごとに 1 回の madvise。
        let base = self as *mut Segment as usize;
        let mut t = todo;
        while t != 0 {
            let first = t.trailing_zeros() as usize;
            let len = (!(t >> first)).trailing_zeros() as usize;
            os::purge((base + (first << SLOT_SHIFT)) as *mut u8, len * SLOT_SIZE);
            t &= !run_mask(first, len);
        }
        let n = todo.count_ones() as usize;
        self.dirty_mask &= !todo;
        if os::PURGE_ZEROES {
            self.zero_mask |= todo;
        }
        stats::DIRTY_SLOTS.fetch_sub(n, Ordering::Relaxed);
        stats::PURGED_SLOTS.fetch_add(n, Ordering::Relaxed);
        (n, earliest)
    }
}

// ====================
// グローバルセグメントキャッシュ
// ====================

/// Reuse cache of segments that became empty (every slot purged).
/// Segments beyond the limit are unmapped, which keeps the virtual address space and the number of VMAs bounded.
pub(crate) struct SegCache {
    head: *mut Segment,
    count: usize,
}

// SAFETY: ロック下でだけ触る。
unsafe impl Send for SegCache {}

pub(crate) static SEG_CACHE: SpinLock<SegCache> = SpinLock::new(SegCache {
    head: ptr::null_mut(),
    count: 0,
});

/// Upper bound on the number of segments kept in the cache (override with `VEIL_ALLOC_SEGMENT_CACHE`).
pub(crate) static SEG_CACHE_MAX: AtomicUsize = AtomicUsize::new(8);

/// Obtains a segment (from the cache first, otherwise from the OS). The owner is `owner`.
pub(crate) fn acquire(owner: *mut Heap) -> *mut Segment {
    let cached = {
        let mut c = SEG_CACHE.lock();
        let s = c.head;
        if !s.is_null() {
            // SAFETY: キャッシュ中のセグメントはロック下でだけ触る。
            c.head = unsafe { (*s).next };
            c.count -= 1;
            stats::CACHED_SEGMENTS.fetch_sub(1, Ordering::Relaxed);
        }
        s
    };
    let seg = if !cached.is_null() {
        cached
    } else {
        let mut raw = os::map_aligned(SEGMENT_SIZE, SEGMENT_SIZE);
        if raw.is_null() {
            // マップ数・コミット上限で失敗した可能性。キャッシュを OS へ返して 1 回だけ再試行する。
            flush_cache();
            raw = os::map_aligned(SEGMENT_SIZE, SEGMENT_SIZE);
            if raw.is_null() {
                return ptr::null_mut();
            }
        }
        let seg = raw as *mut Segment;
        // 新規マッピングはゼロ埋め済み。ゼロでないフィールドだけ書く。
        // SAFETY: seg はヘッダを収める大きさの新規マッピング。
        unsafe {
            (*seg).free_mask = ALL_SLOTS;
            (*seg).zero_mask = ALL_SLOTS;
        }
        stats::SEGMENTS.fetch_add(1, Ordering::Relaxed);
        seg
    };
    // SAFETY: seg は誰にも所有されていない。
    unsafe {
        (*seg).prev = ptr::null_mut();
        (*seg).next = ptr::null_mut();
        (*seg).owner.store(owner, Ordering::Release);
    }
    seg
}

/// Gives up a segment that became empty (purges every dirty slot and caches it, or unmaps it if the cache is full).
pub(crate) fn release(seg: *mut Segment) {
    // SAFETY: 呼び出し側は seg の所有者で、全スロットが空き。
    unsafe {
        debug_assert!((*seg).is_empty());
        (*seg).purge(0, 0, true);
        (*seg).owner.store(ptr::null_mut(), Ordering::Release);
    }
    let mut c = SEG_CACHE.lock();
    if c.count < SEG_CACHE_MAX.load(Ordering::Relaxed) {
        // SAFETY: ロック下。
        unsafe { (*seg).next = c.head };
        c.head = seg;
        c.count += 1;
        stats::CACHED_SEGMENTS.fetch_add(1, Ordering::Relaxed);
    } else {
        drop(c);
        os::unmap(seg.cast(), SEGMENT_SIZE);
        stats::SEGMENTS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Returns every cached segment to the OS.
pub(crate) fn flush_cache() {
    let mut head = {
        let mut c = SEG_CACHE.lock();
        let h = c.head;
        c.head = ptr::null_mut();
        stats::CACHED_SEGMENTS.fetch_sub(c.count, Ordering::Relaxed);
        c.count = 0;
        h
    };
    while !head.is_null() {
        // SAFETY: リストから外したセグメントは誰も参照しない。
        let next = unsafe { (*head).next };
        os::unmap(head.cast(), SEGMENT_SIZE);
        stats::SEGMENTS.fetch_sub(1, Ordering::Relaxed);
        head = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_of_finds_consecutive_bits() {
        assert_eq!(runs_of(0b1110_1110, 3), 0b0010_0010);
        assert_eq!(runs_of(0b1110_1110, 4), 0);
        assert_eq!(runs_of(ALL_SLOTS, 63), 0b10);
        assert_eq!(runs_of(ALL_SLOTS, 1), ALL_SLOTS);
    }

    #[test]
    fn run_mask_covers_range() {
        assert_eq!(run_mask(1, 3), 0b1110);
        assert_eq!(run_mask(60, 4), 0xF000_0000_0000_0000);
    }
}
