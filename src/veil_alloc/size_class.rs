//! サイズクラス（F-183）。
//!
//! - 16〜128B は 16B 刻み（8 クラス）。
//! - 128B 超は 2 倍ごとに 4 分割（160, 192, 224, 256, 320, …, 256KiB）。内部断片化は最大 25%。
//! - 2 の冪はすべてクラスに含まれる（アライメント要求の解決に使う）。
//! - 256KiB を超える要求はクラスを持たず、OS から直接マップする（`huge`）。
//!
//! クラスの決定は `alloc` と `dealloc` の両方で `Layout` から同じ関数で行う（`GlobalAlloc`
//! は解放時にもサイズとアライメントを渡すので、ブロックからクラスを引く必要がない）。

use super::{SLOTS, SLOT_SIZE};

/// クラス数。
pub(crate) const NCLASS: usize = 52;

/// 最大クラスのブロックサイズ（これを超えると huge）。
pub(crate) const MAX_CLASS_SIZE: usize = 256 * 1024;

/// 最小アライメント（すべてのブロックは 16B 境界）。
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

/// クラスごとのブロックサイズ。
pub(crate) static CLASS_SIZE: [u32; NCLASS] = {
    let mut t = [0u32; NCLASS];
    let mut c = 0;
    while c < NCLASS {
        t[c] = class_size_of(c) as u32;
        c += 1;
    }
    t
};

/// スパン（同じクラスのブロックを切り出す連続スロット）のスロット数。
///
/// ブロックが 8 個以上入り、端数の無駄がスパンの 1/8 以下になる最小のスロット数を選ぶ
/// （上限 16 スロット。8KiB 以下は 1 スロット、最大クラス 256KiB は 16 スロットに 4 個）。
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
        assert!(n <= 16 && n < SLOTS);
        t[c] = n as u8;
        c += 1;
    }
    t
};

/// 1024B 以下の要求サイズ → クラスの表（`(size + 15) / 16` で引く）。
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
            (size + 15) / 16 - 1
        }
    } else {
        let w = size - 1;
        let msb = (usize::BITS - 1 - w.leading_zeros()) as usize;
        let sub = (w >> (msb - 2)) & 3;
        8 + (msb - 7) * 4 + sub
    }
}

/// 要求サイズ（`<= MAX_CLASS_SIZE`、アライメント 16 以下）のクラス。
#[inline(always)]
pub(crate) fn class_of(size: usize) -> usize {
    if size <= 1024 {
        // SAFETY: size <= 1024 なので添字は 0..=64。
        unsafe { *SMALL_LUT.get_unchecked((size + 15) >> 4) as usize }
    } else {
        class_of_slow(size)
    }
}

/// アライメント要求付きのクラス。ブロックはスパン先頭（64KiB 境界）から `block_size` 刻みで
/// 並ぶので、`block_size` が `align` の倍数なら全ブロックが `align` 境界に乗る
/// （`align <= SLOT_SIZE` の範囲。呼び出し側で保証する）。
#[cold]
pub(crate) fn class_of_aligned(size: usize, align: usize) -> Option<usize> {
    let size = size.max(align);
    if size > MAX_CLASS_SIZE {
        return None;
    }
    let mut c = class_of(size);
    while c < NCLASS {
        if CLASS_SIZE[c] as usize % align == 0 {
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
