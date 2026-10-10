//! セグメントとスパン（F-183）。
//!
//! セグメントは `SEGMENT_SIZE`（4MiB）境界に揃えた 4MiB のマッピングで、64KiB の
//! スロット 64 個に分かれる。スロット 0 の先頭にヘッダ（[`Segment`]）を置き、残り 63 スロットを
//! スパン（同じサイズクラスのブロックを切り出す連続スロット）に割り当てる。
//! ポインタ `p` の属するセグメントは `p & !(SEGMENT_SIZE - 1)` で引ける。
//!
//! セグメントは常にちょうど 1 つのヒープ（[`Heap`]）が所有し、ヘッダの可変状態は所有者だけが
//! 触る（`owner` を除く）。他スレッドからの解放は所有ヒープの `remote` リストへ投函される。
//!
//! 空きスロットの状態（ビットマスク）:
//! - `free_mask`: スパンに使われていない
//! - `dirty_mask`: 空きだが物理ページが残っている可能性がある（purge 待ち）
//! - `zero_mask`: 内容がゼロと分かっている（未使用、または Linux で purge 済み）

use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use super::heap::Heap;
use super::lock::SpinLock;
use super::{os, stats, SEGMENT_SIZE, SLOTS, SLOT_SHIFT, SLOT_SIZE};

/// フリーリストのノード（空きブロック自身の先頭 8 バイトに置く侵入型リスト）。
#[repr(C)]
pub(crate) struct Block {
    pub(crate) next: *mut Block,
}

/// ページフラグ: 空きが無いのでヒープのキューから外れている。
pub(crate) const PAGE_FULL: u8 = 1;
/// ページフラグ: フリーリストの全ブロックがゼロ（先頭 8 バイトの next を除く）。
pub(crate) const PAGE_FRESH_FREE: u8 = 2;
/// ページフラグ: `capacity` 以降（まだ切り出していない部分）がゼロ。
pub(crate) const PAGE_ZERO_TAIL: u8 = 4;

/// スパンの記述子。セグメントヘッダ内にスロットと同数並び、スパン先頭スロットのものだけを使う。
///
/// 高速パスが触るのは `free`・`used`・`flags` だけ（先頭 16 バイト）。
#[repr(C)]
pub(crate) struct Page {
    pub(crate) free: *mut Block,
    /// 使用中ブロック数（他スレッドが解放して `remote` に溜まっている分を含む）。
    pub(crate) used: u32,
    pub(crate) flags: u8,
    pub(crate) class: u8,
    /// スパンのスロット数（0 = スパン先頭ではない）。
    pub(crate) slots: u8,
    _pad: u8,
    /// 切り出し済みブロック数。
    pub(crate) capacity: u32,
    /// スパンに入るブロック数。
    pub(crate) reserved: u32,
    pub(crate) block_size: u32,
    _pad2: u32,
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
            _pad2: 0,
            start: ptr::null_mut(),
            prev: ptr::null_mut(),
            next: ptr::null_mut(),
        }
    }
}

/// スロット 1..63 のマスク（スロット 0 はヘッダ）。
pub(crate) const ALL_SLOTS: u64 = !1u64;

#[repr(C)]
pub(crate) struct Segment {
    /// 所有ヒープ。他スレッドの解放が投函先を決めるために読む（ブロックが生きている間は不変）。
    pub(crate) owner: AtomicPtr<Heap>,
    pub(crate) prev: *mut Segment,
    pub(crate) next: *mut Segment,
    pub(crate) free_mask: u64,
    pub(crate) dirty_mask: u64,
    pub(crate) zero_mask: u64,
    /// スロットが dirty になった時刻（`os::now_ms`）。
    pub(crate) dirty_time: [u32; SLOTS],
    /// スロット → 属するスパンの先頭スロット。
    pub(crate) slot_head: [u8; SLOTS],
    pub(crate) pages: [Page; SLOTS],
}

const _: () = assert!(core::mem::size_of::<Segment>() <= 4096);
const _: () = assert!(core::mem::size_of::<Page>() == 56);

/// ポインタの属するセグメント。
#[inline(always)]
pub(crate) fn segment_of<T>(p: *const T) -> *mut Segment {
    (p as usize & !(SEGMENT_SIZE - 1)) as *mut Segment
}

/// ページ記述子の添字（= スパン先頭スロット番号）。
#[inline]
pub(crate) fn page_index(seg: *mut Segment, page: *mut Page) -> usize {
    // SAFETY: page は seg.pages の要素。
    (page as usize - unsafe { (*seg).pages.as_ptr() } as usize) / core::mem::size_of::<Page>()
}

/// ポインタが属するスパンのページ記述子。
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

/// `mask` 中で連続 `n` ビットが立つ位置の先頭集合。
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
    /// 連続 `n` スロットの空きを探す。物理ページが残っている（dirty）並びを優先する
    /// （ページフォルトを避け、RSS を既存の常駐ページへ寄せる）。
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

    /// スロット `first..first+n` をスパンとして使用中にする。内容がゼロなら true。
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

    /// スパンを空きに戻す（dirty として purge 待ちにする）。
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

    /// `delay_ms` 以上 dirty のスロット（`force` なら全 dirty スロット）を purge する。
    /// 戻り値は（purge したスロット数, 残った dirty スロットの最早期限）。
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

/// 空になったセグメント（全スロット purge 済み）の再利用キャッシュ。
/// 上限を超えた分は `munmap` する（仮想アドレスと VMA 数を有界に保つ）。
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

/// キャッシュに保持するセグメント数の上限（`VEIL_ALLOC_SEGMENT_CACHE` で上書き可）。
pub(crate) static SEG_CACHE_MAX: AtomicUsize = AtomicUsize::new(8);

/// セグメントを 1 つ得る（キャッシュ優先、無ければ OS から）。所有者は `owner`。
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

/// 空になったセグメントを手放す（全 dirty スロットを purge してキャッシュへ、溢れたら unmap）。
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

/// キャッシュ中のセグメントをすべて OS へ返す。
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
