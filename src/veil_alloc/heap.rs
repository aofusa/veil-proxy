// F-183
//! Thread-local heaps.
//!
//! # Structure
//!
//! - One [`Heap`] per thread. For each size class it keeps the current page (`direct`) and a
//!   doubly linked queue of pages that still have free blocks (`queue`). A page with no free
//!   blocks is taken off the queue and flagged `PAGE_FULL`, so allocation never scans full pages.
//! - The allocation fast path is only a pop from `direct[class].free` (no locks, no atomics).
//! - The free fast path pushes onto the page's free list when the segment's owning heap is the
//!   current one. Because every page has its own free list, a span can be returned as soon as
//!   its page becomes completely empty, so fragmented free space does not sit in RSS in
//!   long-running processes.
//! - Frees from other threads are pushed with CAS onto the owning heap's `remote` list (a
//!   Treiber stack), and the owner reclaims them in bulk on its slow path and when idle.
//!
//! # Lifetime
//!
//! - Heap structures are carved from a dedicated mapping and are **never freed**, so they stay
//!   valid as a place to post remote frees.
//! - When a thread exits (the pthread key destructor), its heap is cleaned up and put on the
//!   "abandoned" list, and the next thread to start takes it over as is (segment owners are
//!   not rewritten).
//! - If the same thread allocates again after that cleanup, it allocates from the shared heap,
//!   which is protected by a lock.
//!
//! # Returning memory (stable RSS for long-running processes)
//!
//! - A freed span goes back to its segment as dirty (physical pages kept) and is returned to
//!   the OS with `madvise` after `purge_delay_ms`. Reallocation prefers dirty slots, so memory
//!   is reused without page faults under load, and RSS drops after the delay once the load is gone.
//! - When the total dirty amount exceeds `dirty_max_slots` per heap, memory is returned
//!   without waiting for the delay (an upper bound).
//! - The worker idle hook ([`collect`]) returns empty pages, purges expired memory and cleans
//!   abandoned heaps, so RSS also drops for threads that have stopped allocating.

use core::cell::Cell;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU8, AtomicUsize, Ordering};

use super::lock::SpinLock;
use super::segment::{
    self, page_index, page_of, segment_of, Block, Page, Segment, PAGE_FRESH_FREE, PAGE_FULL,
    PAGE_RETAINED, PAGE_ZERO_TAIL,
};
use super::size_class::{CLASS_SIZE, CLASS_SLOTS, NCLASS};
use super::{os, stats, SLOT_SHIFT, SLOT_SIZE};

/// The list that receives frees from other threads. Placed on a different cache line from the owner's writes.
#[repr(C, align(64))]
pub(crate) struct RemoteList(pub(crate) AtomicPtr<Block>);

#[repr(C, align(64))]
pub(crate) struct Heap {
    /// The current page of each size class (`EMPTY_PAGE` if none).
    pub(crate) direct: [*mut Page; NCLASS],
    /// The head of the queue of pages with free blocks, for each size class.
    queue: [*mut Page; NCLASS],
    /// Doubly linked list of owned segments.
    segments: *mut Segment,
    /// Number of owned segments that are completely empty.
    empty_segments: u32,
    /// Per class, the number of empty pages (other than `direct`) kept on the queue.
    retained: [u8; NCLASS],
    /// Number of dirty slots in owned segments.
    dirty_slots: u32,
    /// The earliest purge deadline of the dirty slots (valid only while `dirty_slots > 0`).
    next_purge: u32,
    /// Link for the abandoned list and the list of all heaps.
    abandoned_next: *mut Heap,
    pub(crate) remote: RemoteList,
}

// SAFETY: ヒープの非アトミックな状態は所有スレッド（または HEAPS / GLOBAL_HEAP の
// ロック保持者）だけが触る。`remote` はアトミック。
unsafe impl Sync for Heap {}
unsafe impl Send for Heap {}

/// The empty page (`free` is always null; never written to).
struct SyncPage(Page);
// SAFETY: 読み取り専用。
unsafe impl Sync for SyncPage {}
static EMPTY_PAGE: SyncPage = SyncPage(Page::empty());

#[inline(always)]
fn empty_page() -> *mut Page {
    &raw const EMPTY_PAGE.0 as *mut Page
}

impl Heap {
    const fn sentinel() -> Self {
        Self {
            direct: [&raw const EMPTY_PAGE.0 as *mut Page; NCLASS],
            queue: [ptr::null_mut(); NCLASS],
            segments: ptr::null_mut(),
            empty_segments: 0,
            retained: [0; NCLASS],
            dirty_slots: 0,
            next_purge: 0,
            abandoned_next: ptr::null_mut(),
            remote: RemoteList(AtomicPtr::new(ptr::null_mut())),
        }
    }
}

/// Initial value for a thread that has no heap yet (every class points to the empty page, so the first allocation takes the slow path).
static EMPTY_HEAP: Heap = Heap::sentinel();
/// Marker for a thread that has finished its exit cleanup.
static DEAD_HEAP: Heap = Heap::sentinel();

#[inline(always)]
fn empty_heap() -> *mut Heap {
    &raw const EMPTY_HEAP as *mut Heap
}

#[inline(always)]
fn dead_heap() -> *mut Heap {
    &raw const DEAD_HEAP as *mut Heap
}

thread_local! {
    // Drop を持たない const 初期化の thread_local は、デストラクタ登録（＝アロケーション）を
    // 伴わない `#[thread_local]` 静的変数になる。アロケータの内側から安全に使える。
    static HEAP: Cell<*mut Heap> = const { Cell::new(&raw const EMPTY_HEAP as *mut Heap) };
}

#[inline(always)]
pub(crate) fn current() -> *mut Heap {
    HEAP.with(Cell::get)
}

// ====================
// チューニング値（起動時に環境変数で上書き可）
// ====================

/// Delay before a free span is returned to the OS (milliseconds).
pub(crate) static PURGE_DELAY_MS: AtomicUsize = AtomicUsize::new(1000);
/// Upper bound of dirty slots per heap (purged immediately when exceeded).
pub(crate) static DIRTY_MAX_SLOTS: AtomicUsize = AtomicUsize::new(256);
/// Number of completely empty segments each heap keeps.
pub(crate) static KEEP_EMPTY_SEGMENTS: AtomicUsize = AtomicUsize::new(1);

/// Number of empty pages, per class, kept on the queue besides `direct`.
const RETAIN_EMPTY_PAGES: u8 = 1;

/// Approximate number of bytes touched by one carve (so unused space does not count towards RSS).
const EXTEND_BYTES: usize = 16 * 1024;
/// Number of bytes touched by the first carve of a span.
const FIRST_EXTEND_BYTES: usize = 4096;

// ====================
// 確保
// ====================

/// Fast path: pops from the free list of the current page.
#[inline(always)]
pub(crate) unsafe fn alloc(class: usize) -> *mut u8 {
    // SAFETY: heap は EMPTY_HEAP/DEAD_HEAP か所有ヒープ。direct は常に有効なページを指す。
    unsafe {
        let heap = current();
        let page = *(*heap).direct.get_unchecked(class);
        let b = (*page).free;
        if !b.is_null() {
            (*page).free = (*b).next;
            (*page).used += 1;
            return b.cast();
        }
        alloc_slow(heap, class, 0)
    }
}

/// Zeroed allocation. Fresh blocks are already zero, so only the first 8 bytes (`next`) are cleared.
#[inline(always)]
pub(crate) unsafe fn alloc_zeroed(class: usize, size: usize) -> *mut u8 {
    // SAFETY: alloc と同じ。返すブロックは class のサイズ（>= size）以上。
    unsafe {
        let heap = current();
        let page = *(*heap).direct.get_unchecked(class);
        let b = (*page).free;
        if !b.is_null() {
            (*page).free = (*b).next;
            (*page).used += 1;
            zero_block(page, b, size);
            return b.cast();
        }
        alloc_slow(heap, class, size.max(1))
    }
}

/// Zeroes a popped block (if the page's free list holds only fresh blocks, only `next` needs clearing).
#[inline(always)]
unsafe fn zero_block(page: *mut Page, b: *mut Block, size: usize) {
    // SAFETY: b は page から pop した size バイト以上のブロック。
    unsafe {
        if (*page).flags & PAGE_FRESH_FREE != 0 {
            (*b).next = ptr::null_mut();
        } else {
            ptr::write_bytes(b.cast::<u8>(), 0, size);
        }
    }
}

#[cold]
#[inline(never)]
unsafe fn alloc_slow(heap: *mut Heap, class: usize, zero_size: usize) -> *mut u8 {
    let mut heap = heap;
    if heap == empty_heap() {
        heap = thread_init();
    }
    if heap == dead_heap() {
        return alloc_global(class, zero_size);
    }
    // SAFETY: heap はこのスレッドの所有ヒープ。
    unsafe { heap_alloc(heap, class, zero_size) }
}

/// Allocation from a heap (called by the owner or by the holder of the lock). Zeroes `zero_size` bytes if it is greater than 0.
unsafe fn heap_alloc(heap: *mut Heap, class: usize, zero_size: usize) -> *mut u8 {
    // SAFETY: heap の非アトミック状態は呼び出し側が排他的に持つ。
    unsafe {
        if !(*heap).remote.0.load(Ordering::Relaxed).is_null() {
            drain_remote(heap);
        }
        let mut page = (*heap).direct[class];
        loop {
            if page != empty_page() {
                if !(*page).free.is_null() {
                    break;
                }
                if (*page).capacity < (*page).reserved {
                    extend(page);
                    break;
                }
                // 満杯: キューから外す。空きが出たら解放時に戻す。
                queue_remove(heap, class, page);
                (*page).flags |= PAGE_FULL;
            }
            page = (*heap).queue[class];
            if !page.is_null() && (*page).flags & PAGE_RETAINED != 0 {
                (*page).flags &= !PAGE_RETAINED;
                (*heap).retained[class] -= 1;
            }
            if page.is_null() {
                page = page_new(heap, class);
                if page.is_null() {
                    (*heap).direct[class] = empty_page();
                    return oom_retry(heap, class, zero_size);
                }
            }
            (*heap).direct[class] = page;
        }
        let b = (*page).free;
        (*page).free = (*b).next;
        (*page).used += 1;
        if zero_size > 0 {
            zero_block(page, b, zero_size);
        }
        b.cast()
    }
}

/// When mapping from the OS fails: cleans up the own heap and the cache, then retries once.
#[cold]
unsafe fn oom_retry(heap: *mut Heap, class: usize, zero_size: usize) -> *mut u8 {
    // SAFETY: heap は呼び出し側が排他的に持つ。
    unsafe {
        heap_collect(heap, true);
        segment::flush_cache();
        let page = page_new(heap, class);
        if page.is_null() {
            return ptr::null_mut();
        }
        (*heap).direct[class] = page;
        heap_alloc(heap, class, zero_size)
    }
}

/// Carves blocks from the uncarved area into the free list (called only when `free` is empty).
unsafe fn extend(page: *mut Page) {
    // SAFETY: page は所有ページで free は空。
    unsafe {
        debug_assert!((*page).free.is_null());
        let bsize = (*page).block_size as usize;
        let remaining = ((*page).reserved - (*page).capacity) as usize;
        // 最初の切り出しは OS ページ 1 枚分だけ触る（ほとんど使われないクラスのページが
        // 16KiB ずつ常駐しない。スレッド数 × クラス数で効く）。2 回目以降は大きく切り出す。
        let bytes = if (*page).capacity == 0 {
            FIRST_EXTEND_BYTES
        } else {
            EXTEND_BYTES
        };
        let count = (bytes / bsize).clamp(1, 512).min(remaining);
        let first = (*page).start.add((*page).capacity as usize * bsize);
        let mut b = first;
        for _ in 1..count {
            let next = b.add(bsize);
            (*(b as *mut Block)).next = next.cast();
            b = next;
        }
        (*(b as *mut Block)).next = ptr::null_mut();
        (*page).free = first.cast();
        (*page).capacity += count as u32;
        if (*page).flags & PAGE_ZERO_TAIL != 0 {
            (*page).flags |= PAGE_FRESH_FREE;
        }
    }
}

/// Takes a span of `n` contiguous slots from an owned segment (or from a new segment if none fits).
/// Returns (the page descriptor, whether the contents are zero).
unsafe fn span_take(heap: *mut Heap, n: usize) -> Option<(*mut Page, bool)> {
    // SAFETY: heap の非アトミック状態は呼び出し側が排他的に持つ。
    unsafe {
        let mut found: *mut Segment = ptr::null_mut();
        let mut first = 0;
        // 使用中のセグメントを優先する（空のセグメントを手放せるように）。
        for pass in 0..2 {
            let mut s = (*heap).segments;
            while !s.is_null() {
                if pass == 1 || !(*s).is_empty() {
                    if let Some(f) = (*s).find_run(n) {
                        found = s;
                        first = f;
                        break;
                    }
                }
                s = (*s).next;
            }
            if !found.is_null() {
                break;
            }
        }
        if found.is_null() {
            found = segment::acquire(heap);
            if found.is_null() {
                return None;
            }
            segment_link(heap, found);
            (*heap).empty_segments += 1;
            first = (*found).find_run(n).unwrap_or(1);
        }
        let seg = found;
        if (*seg).is_empty() {
            (*heap).empty_segments -= 1;
        }
        let was_dirty = ((*seg).dirty_mask >> first) & ((1u64 << n) - 1);
        let zero = (*seg).take_run(first, n);
        (*heap).dirty_slots -= was_dirty.count_ones();
        let page = (*seg).pages.as_mut_ptr().add(first);
        (*page).start = segment::segment_base(seg).add(first << SLOT_SHIFT);
        (*page).slots = n as u8;
        (*page).prev = ptr::null_mut();
        (*page).next = ptr::null_mut();
        Some((page, zero))
    }
}

/// Allocates a new span and puts it at the head of the queue.
unsafe fn page_new(heap: *mut Heap, class: usize) -> *mut Page {
    let n = CLASS_SLOTS[class] as usize;
    // SAFETY: heap の非アトミック状態は呼び出し側が排他的に持つ。
    unsafe {
        let Some((page, zero)) = span_take(heap, n) else {
            return ptr::null_mut();
        };
        let bsize = CLASS_SIZE[class];
        (*page).free = ptr::null_mut();
        (*page).used = 0;
        (*page).flags = if zero { PAGE_ZERO_TAIL } else { 0 };
        (*page).class = class as u8;
        (*page).capacity = 0;
        let reserved = ((n * SLOT_SIZE) / bsize as usize) as u32;
        (*page).reserved = reserved;
        (*page).requeue_at = reserved - (reserved / 8).max(1);
        (*page).block_size = bsize;
        queue_push_front(heap, class, page);
        page
    }
}

// ====================
// large（256KiB 超〜LARGE_MAX）: スロット単位のスパンを 1 ブロックとして使う
// ====================

/// Marker of a large span (stored in `Page::class` instead of a size class).
pub(crate) const LARGE_CLASS: u8 = 255;

/// Large allocation. Returns a span of `slots` slots as is (it reuses dirty slots, and after
/// being freed it follows the same delayed purge as small classes).
pub(crate) unsafe fn alloc_large(slots: usize, zero_size: usize) -> *mut u8 {
    let mut heap = current();
    if heap == empty_heap() {
        heap = thread_init();
    }
    if heap == dead_heap() {
        let mut g = GLOBAL_HEAP.lock();
        if g.0.is_null() {
            let mut h = HEAPS.lock();
            g.0 = heap_new(&mut h);
            if g.0.is_null() {
                return ptr::null_mut();
            }
        }
        // SAFETY: 共有ヒープはロック下で排他的に使う。
        return unsafe { heap_alloc_large(g.0, slots, zero_size) };
    }
    // SAFETY: 自スレッドのヒープ。
    unsafe { heap_alloc_large(heap, slots, zero_size) }
}

unsafe fn heap_alloc_large(heap: *mut Heap, slots: usize, zero_size: usize) -> *mut u8 {
    // SAFETY: heap は呼び出し側が排他的に持つ。
    unsafe {
        if !(*heap).remote.0.load(Ordering::Relaxed).is_null() {
            drain_remote(heap);
        }
        let mut taken = span_take(heap, slots);
        if taken.is_none() {
            heap_collect(heap, true);
            segment::flush_cache();
            taken = span_take(heap, slots);
        }
        let Some((page, zero)) = taken else {
            return ptr::null_mut();
        };
        (*page).free = ptr::null_mut();
        (*page).used = 1;
        (*page).flags = 0;
        (*page).class = LARGE_CLASS;
        (*page).capacity = 1;
        (*page).reserved = 1;
        (*page).requeue_at = 0;
        (*page).block_size = (slots * SLOT_SIZE) as u32;
        let p = (*page).start;
        if zero_size > 0 && !zero {
            ptr::write_bytes(p, 0, zero_size);
        }
        p
    }
}

/// Large free (the owner returns the span immediately; other threads post it to the owner).
pub(crate) unsafe fn free_large(p: *mut u8) {
    // SAFETY: p は alloc_large が返した生きているブロック。
    unsafe {
        let seg = segment_of(p);
        let heap = current();
        if (*seg).owner.load(Ordering::Relaxed) == heap {
            let page = page_of(seg, p);
            (*page).used = 0;
            span_free(heap, page, os::now_ms());
        } else {
            free_remote(seg, p);
        }
    }
}

// ====================
// 解放
// ====================

/// Fast path: returns the block to its page if it belongs to this heap; posts it to the owner otherwise.
#[inline(always)]
pub(crate) unsafe fn free(p: *mut u8) {
    // SAFETY: p はこのアロケータが返した生きているブロック（huge ではない）。
    unsafe {
        let seg = segment_of(p);
        let heap = current();
        if (*seg).owner.load(Ordering::Relaxed) == heap {
            free_local(heap, seg, p);
        } else {
            free_remote(seg, p);
        }
    }
}

#[inline(always)]
unsafe fn free_local(heap: *mut Heap, seg: *mut Segment, p: *mut u8) {
    // SAFETY: seg は heap の所有セグメントで、p はその中の使用中ブロック。
    unsafe {
        let page = page_of(seg, p);
        let b = p as *mut Block;
        (*b).next = (*page).free;
        (*page).free = b;
        let used = (*page).used - 1;
        (*page).used = used;
        let flags = (*page).flags;
        (*page).flags = flags & !PAGE_FRESH_FREE;
        // 満杯ページは空きが 1/8 溜まるまでキューへ戻さない（1 つ空くたびに戻すと、
        // 次の確保で再び満杯になり遅いパスを往復する）。
        if used == 0 || (flags & PAGE_FULL != 0 && used <= (*page).requeue_at) {
            page_freed_slow(heap, page);
        }
    }
}

#[cold]
#[inline(never)]
unsafe fn free_remote(seg: *mut Segment, p: *mut u8) {
    // SAFETY: ブロックが生きている間、seg の所有者は変わらない（所有者はセグメントを
    // 全ブロック解放＝remote の回収後まで手放さない）。ヒープ構造体は解放されない。
    unsafe {
        let owner = (*seg).owner.load(Ordering::Acquire);
        let list = &(*owner).remote.0;
        let b = p as *mut Block;
        let mut head = list.load(Ordering::Relaxed);
        loop {
            (*b).next = head;
            match list.compare_exchange_weak(head, b, Ordering::Release, Ordering::Relaxed) {
                Ok(_) => break,
                Err(h) => head = h,
            }
        }
    }
    stats::REMOTE_FREES.fetch_add(1, Ordering::Relaxed);
}

/// Returns the posted frees to this heap in bulk.
unsafe fn drain_remote(heap: *mut Heap) {
    // SAFETY: heap は呼び出し側が排他的に持つ。投函されたブロックは heap の所有セグメント内。
    unsafe {
        let mut b = (*heap).remote.0.swap(ptr::null_mut(), Ordering::Acquire);
        while !b.is_null() {
            let next = (*b).next;
            let seg = segment_of(b);
            free_local(heap, seg, b.cast());
            b = next;
        }
    }
}

/// Called when a page became empty or when a full page got free blocks.
#[cold]
#[inline(never)]
unsafe fn page_freed_slow(heap: *mut Heap, page: *mut Page) {
    // SAFETY: page は heap の所有ページ。
    unsafe {
        let class = (*page).class as usize;
        if class == LARGE_CLASS as usize {
            // large スパン（1 ブロック、キューに入らない）。
            span_free(heap, page, os::now_ms());
            return;
        }
        if (*page).flags & PAGE_FULL != 0 {
            (*page).flags &= !PAGE_FULL;
            queue_push_front(heap, class, page);
        }
        if (*page).used == 0 && (*heap).direct[class] != page {
            // 空ページを 1 枚だけ（direct とは別に）キューに残し、確保・解放が境界を
            // 行き来するときのスパン返却・再取得の往復を避ける。残りは返す。
            if (*heap).retained[class] < RETAIN_EMPTY_PAGES {
                (*heap).retained[class] += 1;
                (*page).flags |= PAGE_RETAINED;
            } else {
                queue_remove(heap, class, page);
                span_free(heap, page, os::now_ms());
            }
        }
    }
}

/// Returns a span to its segment (as dirty, waiting to be purged).
unsafe fn span_free(heap: *mut Heap, page: *mut Page, now: u32) {
    // SAFETY: page は heap の所有ページで、キューから外れ、使用中ブロックが無い。
    unsafe {
        debug_assert_eq!((*page).used, 0);
        let seg = segment_of(page);
        let first = page_index(seg, page);
        let n = (*page).slots as usize;
        (*page).slots = 0;
        (*page).free = ptr::null_mut();
        (*page).flags = 0;
        (*seg).give_run(first, n, now);
        if (*heap).dirty_slots == 0 {
            (*heap).next_purge = now.wrapping_add(PURGE_DELAY_MS.load(Ordering::Relaxed) as u32);
        }
        (*heap).dirty_slots += n as u32;
        if (*seg).is_empty() {
            (*heap).empty_segments += 1;
        }
        maybe_purge(heap, now);
    }
}

/// Purges if the deadline has passed (or if dirty memory exceeds the limit).
unsafe fn maybe_purge(heap: *mut Heap, now: u32) {
    // SAFETY: heap は呼び出し側が排他的に持つ。
    unsafe {
        if (*heap).dirty_slots == 0 {
            return;
        }
        let over = (*heap).dirty_slots as usize > DIRTY_MAX_SLOTS.load(Ordering::Relaxed);
        if over || os::time_reached(now, (*heap).next_purge) {
            heap_purge(heap, now, over);
        }
    }
}

/// Purges expired dirty slots of owned segments and gives up surplus empty segments.
unsafe fn heap_purge(heap: *mut Heap, now: u32, force: bool) {
    let delay = PURGE_DELAY_MS.load(Ordering::Relaxed) as u32;
    let keep = KEEP_EMPTY_SEGMENTS.load(Ordering::Relaxed) as u32;
    // SAFETY: heap の非アトミック状態は呼び出し側が排他的に持つ。
    unsafe {
        let mut earliest: Option<u32> = None;
        let mut s = (*heap).segments;
        while !s.is_null() {
            let next = (*s).next;
            if (*heap).empty_segments > keep && (*s).is_empty() {
                let dirty = (*s).dirty_mask.count_ones();
                segment_unlink(heap, s);
                (*heap).empty_segments -= 1;
                (*heap).dirty_slots -= dirty;
                segment::release(s);
            } else if (*s).dirty_mask != 0 {
                let (purged, e) = (*s).purge(now, delay, force);
                (*heap).dirty_slots -= purged as u32;
                if let Some(e) = e {
                    earliest = Some(match earliest {
                        Some(x) if os::time_reached(e, x) => x,
                        _ => e,
                    });
                }
            }
            s = next;
        }
        if let Some(e) = earliest {
            (*heap).next_purge = e;
        }
    }
}

/// Cleans a heap: reclaims posted frees, returns empty pages, purges expired memory
/// (everything if `force` is set) and returns surplus empty segments.
pub(crate) unsafe fn heap_collect(heap: *mut Heap, force: bool) {
    // SAFETY: heap は呼び出し側が排他的に持つ。
    unsafe {
        if !(*heap).remote.0.load(Ordering::Relaxed).is_null() {
            drain_remote(heap);
        }
        let now = os::now_ms();
        for class in 0..NCLASS {
            let mut p = (*heap).queue[class];
            while !p.is_null() {
                let next = (*p).next;
                if (*p).used == 0 {
                    if (*heap).direct[class] == p {
                        (*heap).direct[class] = empty_page();
                    }
                    if (*p).flags & PAGE_RETAINED != 0 {
                        (*heap).retained[class] -= 1;
                    }
                    queue_remove(heap, class, p);
                    span_free(heap, p, now);
                }
                p = next;
            }
        }
        heap_purge(heap, now, force);
    }
}

// ====================
// キュー・セグメントリスト
// ====================

unsafe fn queue_push_front(heap: *mut Heap, class: usize, page: *mut Page) {
    // SAFETY: 所有者だけが呼ぶ。
    unsafe {
        let head = (*heap).queue[class];
        (*page).prev = ptr::null_mut();
        (*page).next = head;
        if !head.is_null() {
            (*head).prev = page;
        }
        (*heap).queue[class] = page;
    }
}

unsafe fn queue_remove(heap: *mut Heap, class: usize, page: *mut Page) {
    // SAFETY: page は queue[class] に入っている。
    unsafe {
        let prev = (*page).prev;
        let next = (*page).next;
        if prev.is_null() {
            (*heap).queue[class] = next;
        } else {
            (*prev).next = next;
        }
        if !next.is_null() {
            (*next).prev = prev;
        }
        (*page).prev = ptr::null_mut();
        (*page).next = ptr::null_mut();
    }
}

unsafe fn segment_link(heap: *mut Heap, seg: *mut Segment) {
    // SAFETY: 所有者だけが呼ぶ。
    unsafe {
        let head = (*heap).segments;
        (*seg).prev = ptr::null_mut();
        (*seg).next = head;
        if !head.is_null() {
            (*head).prev = seg;
        }
        (*heap).segments = seg;
    }
}

unsafe fn segment_unlink(heap: *mut Heap, seg: *mut Segment) {
    // SAFETY: seg は heap のリストに入っている。
    unsafe {
        let prev = (*seg).prev;
        let next = (*seg).next;
        if prev.is_null() {
            (*heap).segments = next;
        } else {
            (*prev).next = next;
        }
        if !next.is_null() {
            (*next).prev = prev;
        }
        (*seg).prev = ptr::null_mut();
        (*seg).next = ptr::null_mut();
    }
}

// ====================
// ヒープの生成・放棄・引き継ぎ
// ====================

struct Heaps {
    /// Abandoned heaps (the next thread takes them over).
    abandoned: *mut Heap,
    /// The memory heap structures are carved from.
    arena: *mut u8,
    arena_left: usize,
}

// SAFETY: ロック下でだけ触る。
unsafe impl Send for Heaps {}

static HEAPS: SpinLock<Heaps> = SpinLock::new(Heaps {
    abandoned: ptr::null_mut(),
    arena: ptr::null_mut(),
    arena_left: 0,
});

/// The shared heap used by threads after their exit cleanup (protected by a lock, created lazily).
struct GlobalHeap(*mut Heap);
// SAFETY: ロック下でだけ触る。
unsafe impl Send for GlobalHeap {}
static GLOBAL_HEAP: SpinLock<GlobalHeap> = SpinLock::new(GlobalHeap(ptr::null_mut()));

/// Carves out a new heap structure (under the `HEAPS` lock).
fn heap_new(h: &mut Heaps) -> *mut Heap {
    let size = core::mem::size_of::<Heap>();
    if h.arena_left < size {
        let a = os::map(SLOT_SIZE);
        if a.is_null() {
            return ptr::null_mut();
        }
        h.arena = a;
        h.arena_left = SLOT_SIZE;
    }
    let heap = h.arena as *mut Heap;
    h.arena = h.arena.wrapping_add(size);
    h.arena_left -= size;
    // SAFETY: heap は新規マッピング内の、境界の揃った未使用領域。
    unsafe { heap.write(Heap::sentinel()) };
    stats::HEAPS.fetch_add(1, Ordering::Relaxed);
    heap
}

/// The pthread key (detects thread exit). 0 = not created, 1 = being created, 2 = created.
static INIT_STATE: AtomicU8 = AtomicU8::new(0);
static mut THREAD_KEY: libc::pthread_key_t = 0;

fn global_init() {
    if INIT_STATE.load(Ordering::Acquire) == 2 {
        return;
    }
    if INIT_STATE
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Acquire)
        .is_ok()
    {
        super::read_env_tuning();
        // SAFETY: 1 回だけ実行される（INIT_STATE で排他）。pthread_key_create・pthread_atfork は
        // Rust のアロケータを呼ばない。
        unsafe {
            let mut key: libc::pthread_key_t = 0;
            libc::pthread_key_create(&mut key, Some(thread_done));
            THREAD_KEY = key;
            libc::pthread_atfork(Some(fork_prepare), Some(fork_parent), Some(fork_child));
        }
        INIT_STATE.store(2, Ordering::Release);
    } else {
        while INIT_STATE.load(Ordering::Acquire) != 2 {
            os::yield_now();
        }
    }
}

/// First allocation of a thread: takes over an abandoned heap or creates a new one.
#[cold]
fn thread_init() -> *mut Heap {
    global_init();
    let heap = {
        let mut h = HEAPS.lock();
        let a = h.abandoned;
        if !a.is_null() {
            // SAFETY: 放棄リストはロック下でだけ触る。
            h.abandoned = unsafe { (*a).abandoned_next };
            stats::ABANDONED_HEAPS.fetch_sub(1, Ordering::Relaxed);
            a
        } else {
            heap_new(&mut h)
        }
    };
    if heap.is_null() {
        // ヒープ構造体すら確保できない: 共有ヒープへ（そちらも失敗すれば null が返る）。
        return dead_heap();
    }
    HEAP.with(|c| c.set(heap));
    // SAFETY: THREAD_KEY は global_init で作成済み。
    unsafe {
        libc::pthread_setspecific(THREAD_KEY, heap.cast());
    }
    heap
}

/// Destructor of the pthread key: cleans the heap and puts it on the abandoned list.
unsafe extern "C" fn thread_done(p: *mut libc::c_void) {
    let heap = p as *mut Heap;
    if heap.is_null() {
        return;
    }
    // 以後このスレッドの確保は共有ヒープ、解放は投函になる。
    HEAP.with(|c| c.set(dead_heap()));
    // SAFETY: heap はこのスレッドが所有していた。もう他の誰も所有者として触らない。
    unsafe { heap_collect(heap, true) };
    let mut h = HEAPS.lock();
    // SAFETY: ロック下。
    unsafe { (*heap).abandoned_next = h.abandoned };
    h.abandoned = heap;
    stats::ABANDONED_HEAPS.fetch_add(1, Ordering::Relaxed);
}

/// Allocation by a thread after its exit cleanup (shared heap, with the lock).
#[cold]
#[inline(never)]
fn alloc_global(class: usize, zero_size: usize) -> *mut u8 {
    let mut g = GLOBAL_HEAP.lock();
    if g.0.is_null() {
        let mut h = HEAPS.lock();
        g.0 = heap_new(&mut h);
        if g.0.is_null() {
            return ptr::null_mut();
        }
    }
    // SAFETY: 共有ヒープはロック下で排他的に使う。
    unsafe { heap_alloc(g.0, class, zero_size) }
}

// ====================
// アイドル時の回収
// ====================

/// Cleans the current thread's heap and, when the locks can be taken, the abandoned and shared heaps too.
pub(crate) fn collect(force: bool) {
    let heap = current();
    if heap != empty_heap() && heap != dead_heap() {
        // SAFETY: 自スレッドのヒープ。
        unsafe { heap_collect(heap, force) };
    }
    if let Some(h) = HEAPS.try_lock() {
        let mut a = h.abandoned;
        while !a.is_null() {
            // SAFETY: 放棄ヒープは HEAPS のロック保持者だけが触る。
            unsafe {
                heap_collect(a, force);
                a = (*a).abandoned_next;
            }
        }
    }
    if let Some(g) = GLOBAL_HEAP.try_lock() {
        if !g.0.is_null() {
            // SAFETY: 共有ヒープはロック下で排他的に使う。
            unsafe { heap_collect(g.0, force) };
        }
    }
}

// ====================
// fork
// ====================

// fork の瞬間に他スレッドがロックを持っていると、子プロセスでは永遠に解放されない。
// 獲得順（GLOBAL_HEAP → HEAPS → SEG_CACHE）で全部取ってから fork し、親子とも解放する。
extern "C" fn fork_prepare() {
    GLOBAL_HEAP.raw_lock();
    HEAPS.raw_lock();
    segment::SEG_CACHE.raw_lock();
}

extern "C" fn fork_parent() {
    segment::SEG_CACHE.raw_unlock();
    HEAPS.raw_unlock();
    GLOBAL_HEAP.raw_unlock();
}

extern "C" fn fork_child() {
    segment::SEG_CACHE.raw_unlock();
    HEAPS.raw_unlock();
    GLOBAL_HEAP.raw_unlock();
}
