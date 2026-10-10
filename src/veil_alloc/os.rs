//! OS ページ管理（F-183）。`mmap` / `munmap` / `madvise` / `mremap` と粗い単調時計だけを使う。
//!
//! すべて libc を直接呼び、Rust のアロケーションを一切しない（アロケータ自身から呼ばれる）。
//! seccomp の許可リスト（`security::ALLOWED_SYSCALLS`）には mmap/munmap/madvise/mremap/
//! clock_gettime/sched_yield が元から含まれている（mimalloc と同じ syscall 集合）。

use core::ptr;

/// purge（`madvise`）した領域を次に触ったときゼロで埋まっているか。
///
/// Linux の `MADV_DONTNEED` は匿名マッピングを即座に解放し、次のアクセスでゼロページを
/// 割り当てる（RSS が即座に下がる）。BSD/macOS は `MADV_FREE`（遅延解放。内容は不定）。
pub(crate) const PURGE_ZEROES: bool = cfg!(any(target_os = "linux", target_os = "android"));

/// 読み書き可能な匿名マッピングを作る。失敗時は null。
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

/// `align`（2 の冪、ページサイズ以上）境界に揃えたマッピングを作る。
///
/// `size + align` をマップしてから前後の余りを `munmap` する。
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

/// マッピングを解放する。
#[inline]
pub(crate) fn unmap(p: *mut u8, size: usize) {
    // SAFETY: 呼び出し側が自分でマップした範囲だけを渡す。
    unsafe {
        libc::munmap(p.cast(), size);
    }
}

/// 範囲の物理ページを OS へ返す（仮想アドレスは保持する）。
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

/// huge 領域の伸縮（Linux の `mremap(MREMAP_MAYMOVE)`）。失敗時は null。
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

/// 粗い単調時計（ミリ秒、32bit で巡回）。遅いパスの purge 判定にだけ使う。
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

/// `a` が `b` 以降か（巡回する 32bit ミリ秒の比較）。
#[inline]
pub(crate) fn time_reached(now: u32, deadline: u32) -> bool {
    (now.wrapping_sub(deadline) as i32) >= 0
}

/// スピンロックの待ちで CPU を譲る。
#[inline]
pub(crate) fn yield_now() {
    // SAFETY: 引数なしの syscall。
    unsafe {
        libc::sched_yield();
    }
}
