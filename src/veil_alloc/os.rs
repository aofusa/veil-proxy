// F-183: seccomp の許可リスト（security::ALLOWED_SYSCALLS）には mmap/munmap/madvise/mremap/
// clock_gettime/sched_yield が元から含まれている（mimalloc と同じ syscall 集合）。
// F-282 AL-01: セグメントのガードの mprotect(PROT_NONE) も許可リストにある（PROT_EXEC だけ拒否）。
//! OS page management.
//!
//! Uses only `mmap` / `munmap` / `madvise` / `mremap` / `mprotect` and a coarse monotonic clock.
//! Everything calls libc directly and never performs a Rust allocation, because the allocator
//! itself calls into this module.

use core::ptr;

/// Whether a purged (`madvise`) region reads as zeroes the next time it is touched.
///
/// Linux `MADV_DONTNEED` releases an anonymous mapping immediately and hands out zero pages on
/// the next access (RSS drops at once). BSD and macOS use `MADV_FREE` (lazy release; the
/// contents are undefined).
pub(crate) const PURGE_ZEROES: bool = cfg!(any(target_os = "linux", target_os = "android"));

/// Creates a readable and writable anonymous mapping. Returns null on failure.
#[inline]
pub(crate) fn map(size: usize) -> *mut u8 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let flags = libc::MAP_PRIVATE | libc::MAP_ANON;
    // SAFETY: 新規の匿名マッピング。既存のメモリには触れない。
    let p = unsafe {
        libc::mmap(
            ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            flags,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        ptr::null_mut()
    } else {
        p.cast()
    }
}

/// Creates a mapping aligned to `align` (a power of two, at least the page size).
///
/// Maps `size + align` bytes and then `munmap`s the surplus before and after.
pub(crate) fn map_aligned(size: usize, align: usize) -> *mut u8 {
    let Some(total) = size.checked_add(align) else {
        return ptr::null_mut();
    };
    let raw = map(total);
    if raw.is_null() {
        return raw;
    }
    let start = raw as usize;
    let aligned = (start + align - 1) & !(align - 1);
    let head = aligned - start;
    let tail = total - head - size;
    if head > 0 {
        unmap(raw, head);
    }
    if tail > 0 {
        unmap((aligned + size) as *mut u8, tail);
    }
    aligned as *mut u8
}

/// Makes a range inaccessible (`PROT_NONE`). Failure is ignored (used only for guard regions).
pub(crate) fn protect_none(p: *mut u8, size: usize) {
    // SAFETY: 呼び出し側が自分でマップした範囲（ページ境界）だけを渡す。
    unsafe { libc::mprotect(p.cast(), size, libc::PROT_NONE) };
}

/// The page size (cached after the first call; 0 if unknown).
pub(crate) fn page_size() -> usize {
    use core::sync::atomic::{AtomicUsize, Ordering};
    static PAGE: AtomicUsize = AtomicUsize::new(0);
    let p = PAGE.load(Ordering::Relaxed);
    if p != 0 {
        return p;
    }
    // SAFETY: 引数の無い問い合わせ。アロケーションしない。
    let p = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let p = if p > 0 { p as usize } else { 0 };
    PAGE.store(p, Ordering::Relaxed);
    p
}

/// Releases a mapping.
#[inline]
pub(crate) fn unmap(p: *mut u8, size: usize) {
    // SAFETY: 呼び出し側が自分でマップした範囲だけを渡す。
    unsafe {
        libc::munmap(p.cast(), size);
    }
}

/// Returns the physical pages of a range to the OS (the virtual addresses are kept).
#[inline]
pub(crate) fn purge(p: *mut u8, size: usize) {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let advice = libc::MADV_DONTNEED;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let advice = libc::MADV_FREE;
    // SAFETY: 呼び出し側が所有する、どのブロックにも使われていない範囲だけを渡す。
    unsafe {
        libc::madvise(p.cast(), size, advice);
    }
}

/// Resizes a huge region (Linux `mremap(MREMAP_MAYMOVE)`). Returns null on failure.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[inline]
pub(crate) fn remap(p: *mut u8, old: usize, new: usize) -> *mut u8 {
    // SAFETY: p..p+old は自分でマップした huge 領域。
    let q = unsafe { libc::mremap(p.cast(), old, new, libc::MREMAP_MAYMOVE) };
    if q == libc::MAP_FAILED {
        ptr::null_mut()
    } else {
        q.cast()
    }
}

/// A coarse monotonic clock (milliseconds, wrapping at 32 bits). Used only for purge decisions on the slow path.
#[inline]
pub(crate) fn now_ms() -> u32 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let clock = libc::CLOCK_MONOTONIC_COARSE;
    #[cfg(target_os = "freebsd")]
    let clock = libc::CLOCK_MONOTONIC_FAST;
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
    let clock = libc::CLOCK_MONOTONIC;
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts は有効な書き込み先。
    unsafe {
        libc::clock_gettime(clock, &mut ts);
    }
    (ts.tv_sec as u64)
        .wrapping_mul(1000)
        .wrapping_add(ts.tv_nsec as u64 / 1_000_000) as u32
}

/// Whether `a` is at or after `b` (comparison of wrapping 32-bit milliseconds).
#[inline]
pub(crate) fn time_reached(now: u32, deadline: u32) -> bool {
    (now.wrapping_sub(deadline) as i32) >= 0
}

/// Yields the CPU while waiting on a spin lock.
#[inline]
pub(crate) fn yield_now() {
    // SAFETY: 引数なしの syscall。
    unsafe {
        libc::sched_yield();
    }
}
