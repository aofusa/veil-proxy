//! 256KiB を超える確保（F-183）。OS から直接マップし、解放で即座に `munmap` する
//! （大きな領域を手元に残さない＝ピーク後の RSS がすぐ下がる）。
//!
//! `GlobalAlloc` は解放時にもサイズを渡すので、ヘッダや管理表を持たない。
//! Linux の `realloc` は `mremap` でコピー無しに伸縮する。

use core::ptr;
use core::sync::atomic::Ordering;

use super::{os, stats, SLOT_SIZE};

/// mmap が保証する最小アライメント（すべての対応プラットフォームのページサイズ以下）。
const MAP_ALIGN: usize = 4096;

#[inline]
fn mapped_size(size: usize) -> Option<usize> {
    size.checked_add(SLOT_SIZE - 1)
        .map(|s| s & !(SLOT_SIZE - 1))
}

pub(crate) fn alloc(size: usize, align: usize) -> *mut u8 {
    let Some(len) = mapped_size(size.max(1)) else {
        return ptr::null_mut();
    };
    let p = if align <= MAP_ALIGN {
        os::map(len)
    } else {
        os::map_aligned(len, align)
    };
    if !p.is_null() {
        stats::HUGE_BYTES.fetch_add(len, Ordering::Relaxed);
    }
    p
}

pub(crate) fn free(p: *mut u8, size: usize) {
    if let Some(len) = mapped_size(size.max(1)) {
        os::unmap(p, len);
        stats::HUGE_BYTES.fetch_sub(len, Ordering::Relaxed);
    }
}

/// huge → huge の伸縮。マップ長が同じなら同じポインタ、Linux は `mremap`。
/// それ以外（他 OS の伸長）は null を返し、呼び出し側が確保・コピー・解放する。
pub(crate) fn realloc(p: *mut u8, old: usize, new: usize, align: usize) -> *mut u8 {
    let (Some(old_len), Some(new_len)) = (mapped_size(old.max(1)), mapped_size(new.max(1))) else {
        return ptr::null_mut();
    };
    if old_len == new_len {
        return p;
    }
    if new_len < old_len {
        // 縮小は末尾を返すだけ（アドレス・アライメントは変わらない）。
        os::unmap(p.wrapping_add(new_len), old_len - new_len);
        stats::HUGE_BYTES.fetch_sub(old_len - new_len, Ordering::Relaxed);
        return p;
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if align <= MAP_ALIGN {
        let q = os::remap(p, old_len, new_len);
        if !q.is_null() {
            stats::HUGE_BYTES.fetch_add(new_len - old_len, Ordering::Relaxed);
        }
        return q;
    }
    let _ = align;
    ptr::null_mut()
}
