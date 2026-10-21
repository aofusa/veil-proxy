// F-183: クラスの決定は alloc / dealloc の両方で Layout から同じ関数で行う。
//! Size classes.
//!
//! - 16 to 128 bytes use 16-byte steps (8 classes).
//! - Above 128 bytes, each power-of-two range is split into 4 (160, 192, 224, 256, 320, ...,
//!   256 KiB). Internal fragmentation is at most 25%.
//! - Every power of two is a class (used to resolve alignment requests).
//! - Requests above 256 KiB have no class and are mapped directly from the OS (`huge`).
//!
//! `alloc` and `dealloc` pick the class from the `Layout` with the same function
//! (`GlobalAlloc` passes the size and alignment on deallocation too, so there is no need to
//! look the class up from the block).

use super::SLOT_SIZE;

/// Number of classes.
pub(crate) const NCLASS: usize = 52;

/// Block size of the largest class (anything bigger is huge).
pub(crate) const MAX_CLASS_SIZE: usize = 256 * 1024;

/// Minimum alignment (every block is on a 16-byte boundary).
pub(crate) const MIN_ALIGN: usize = 16;

const fn class_size_of(c: usize) -> usize {
    if c < 8 {
        (c + 1) * 16
    } else {
        let k = c - 8;
        let msb = 7 + k / 4;
        (5 + k % 4) << (msb - 2)
    }
}

/// Block size of each class.
pub(crate) static CLASS_SIZE: [u32; NCLASS] = {
    let mut t = [0u32; NCLASS];
    let mut c = 0;
    while c < NCLASS {
        t[c] = class_size_of(c) as u32;
        c += 1;
    }
    t
};

/// Number of slots in a span (contiguous slots that blocks of one class are carved from).
///
/// Chooses the smallest slot count that fits at least 8 blocks and wastes at most 1/8 of the
/// span on the remainder (at most 16 slots; classes up to 8 KiB use 1 slot, and the largest
/// class, 256 KiB, gets 4 blocks in 16 slots).
pub(crate) static CLASS_SLOTS: [u8; NCLASS] = {
    let mut t = [0u8; NCLASS];
    let mut c = 0;
    while c < NCLASS {
        let size = class_size_of(c);
        // 中サイズもスパンあたり 8 ブロック以上（上限 16 スロット = 1MiB）。ブロック数が
        // 少ないとランダムな解放でスパンが頻繁に空→再取得を繰り返す。
        let mut n = 1;
        loop {
            let span = n * SLOT_SIZE;
            let blocks = span / size;
            let waste = span - blocks * size;
            if (blocks >= 8 && waste * 8 <= span) || n == 16 {
                break;
            }
            n += 1;
        }
        assert!(n <= 16);
        t[c] = n as u8;
        c += 1;
    }
    t
};

/// Lookup table from request sizes up to 1024 bytes to classes (indexed by `(size + 15) / 16`).
static SMALL_LUT: [u8; 65] = {
    let mut t = [0u8; 65];
    let mut i = 1;
    while i <= 64 {
        let size = i * 16;
        t[i] = class_of_slow(size) as u8;
        i += 1;
    }
    t
};

const fn class_of_slow(size: usize) -> usize {
    if size <= 128 {
        if size == 0 {
            0
        } else {
            size.div_ceil(16) - 1
        }
    } else {
        let w = size - 1;
        let msb = (usize::BITS - 1 - w.leading_zeros()) as usize;
        let sub = (w >> (msb - 2)) & 3;
        8 + (msb - 7) * 4 + sub
    }
}

/// The class for a request size (`<= MAX_CLASS_SIZE`, alignment of 16 or less).
#[inline(always)]
pub(crate) fn class_of(size: usize) -> usize {
    if size <= 1024 {
        // SAFETY: size <= 1024 なので添字は 0..=64。
        unsafe { *SMALL_LUT.get_unchecked((size + 15) >> 4) as usize }
    } else {
        class_of_slow(size)
    }
}

/// The class for a request with an alignment. Blocks are laid out from the start of the span
/// (a 64 KiB boundary) in steps of `block_size`, so when `block_size` is a multiple of `align`
/// every block lies on an `align` boundary (valid for `align <= SLOT_SIZE`; the caller
/// guarantees this).
#[cold]
pub(crate) fn class_of_aligned(size: usize, align: usize) -> Option<usize> {
    let size = size.max(align);
    if size > MAX_CLASS_SIZE {
        return None;
    }
    let mut c = class_of(size);
    while c < NCLASS {
        if (CLASS_SIZE[c] as usize).is_multiple_of(align) {
            return Some(c);
        }
        c += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_sizes_are_monotonic_and_aligned() {
        for c in 0..NCLASS {
            assert_eq!(CLASS_SIZE[c] % 16, 0, "class {c}");
            if c > 0 {
                assert!(CLASS_SIZE[c] > CLASS_SIZE[c - 1]);
            }
        }
        assert_eq!(CLASS_SIZE[NCLASS - 1] as usize, MAX_CLASS_SIZE);
    }

    #[test]
    fn class_of_is_smallest_fitting_class() {
        for size in 1..=MAX_CLASS_SIZE {
            let c = class_of(size);
            assert!(CLASS_SIZE[c] as usize >= size, "size {size}");
            if c > 0 {
                assert!((CLASS_SIZE[c - 1] as usize) < size, "size {size}");
            }
        }
    }

    #[test]
    fn powers_of_two_are_classes() {
        let mut p = 16;
        while p <= MAX_CLASS_SIZE {
            assert_eq!(CLASS_SIZE[class_of(p)] as usize, p);
            p *= 2;
        }
    }

    #[test]
    fn aligned_classes_are_multiples_of_align() {
        let mut align = 32;
        while align <= SLOT_SIZE {
            for size in [1, align - 1, align, align + 1, 3 * align, 100_000] {
                if let Some(c) = class_of_aligned(size, align) {
                    assert_eq!(CLASS_SIZE[c] as usize % align, 0);
                    assert!(CLASS_SIZE[c] as usize >= size.max(align));
                }
            }
            align *= 2;
        }
    }

    #[test]
    fn spans_fit_blocks_with_little_waste() {
        for c in 0..NCLASS {
            let span = CLASS_SLOTS[c] as usize * SLOT_SIZE;
            let blocks = span / CLASS_SIZE[c] as usize;
            assert!(blocks >= 4, "class {c}");
            assert!(
                (span - blocks * CLASS_SIZE[c] as usize) * 8 <= span,
                "class {c}"
            );
        }
    }
}
