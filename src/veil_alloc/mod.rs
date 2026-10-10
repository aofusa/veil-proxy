//! veil 専用アロケータ（F-183、`veil-alloc` feature）。
//!
//! thread-per-core のワーカーに合わせた、スレッドローカルなサイズクラス・ページ単位
//! フリーリストのアロケータ。設計の要点:
//!
//! - **高速パスはロック・アトミック・分岐の少ない pop / push だけ**（[`heap`]）。
//!   `GlobalAlloc` は解放時にもサイズとアライメントを渡すので、サイズクラスを `Layout`
//!   から直接計算し、ブロックごとのヘッダや「ポインタ → サイズ」の表を持たない。
//! - **24 時間 365 日の RSS 安定性**: ページ（スパン）単位のフリーリストで空いたスパンを
//!   特定し、遅延付きで OS へ返す（`purge_delay_ms`）。dirty の上限、空セグメントの
//!   キャッシュ上限、huge の即時 `munmap` で、負荷のピーク後に RSS が元へ戻る。
//!   ワーカーのアイドル時フック（[`collect`]）で、確保が止まったスレッドも返却する。
//! - **スレッドの増減に強い**: 終了したスレッドのヒープは掃除して放棄リストに置き、
//!   次のスレッドがそのまま引き継ぐ（オフロード等のスレッドが入れ替わっても増え続けない）。
//! - **他スレッドの解放**（ロガー・オフロード・チャネル越しの受け渡し）は所有ヒープへ
//!   ロックフリーに投函し、所有者が遅いパスで一括回収する。
//! - **fork 安全**: グローバルロックを `pthread_atfork` で fork の前後に獲得・解放する。
//!
//! メモリ配置: 4MiB 境界のセグメント（64KiB スロット × 64、先頭スロットにヘッダ）。
//! 256KiB 以下はサイズクラス（52 クラス）、それを超えるものは OS から直接マップする。
//!
//! チューニング（起動時に環境変数で上書き可。既定値は計測で決めた。F-183 チケット参照）:
//!
//! | 環境変数 | 既定 | 意味 |
//! |---|---|---|
//! | `VEIL_ALLOC_PURGE_DELAY_MS` | 1000 | 空いたスパンを OS へ返すまでの遅延 |
//! | `VEIL_ALLOC_DIRTY_MAX_MB` | 16 | ヒープあたり purge 待ちにできる量の上限 |
//! | `VEIL_ALLOC_SEGMENT_CACHE` | 8 | 空セグメント（4MiB、purge 済み）のプロセス全体キャッシュ数 |
//!
//! 対応 OS は unix（Linux / FreeBSD / OpenBSD / NetBSD / macOS）。Windows では使わない
//! （`src/lib.rs` で他のアロケータへフォールバックする）。

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

/// セグメント（4MiB、同じ境界に揃える）。
pub(crate) const SEGMENT_SIZE: usize = 1 << 22;
/// スロット（64KiB）。スパンと purge の単位。
pub(crate) const SLOT_SHIFT: usize = 16;
pub(crate) const SLOT_SIZE: usize = 1 << SLOT_SHIFT;
pub(crate) const SLOTS: usize = SEGMENT_SIZE / SLOT_SIZE;

/// veil 専用のグローバルアロケータ。
pub struct VeilAlloc;

/// `Layout` のサイズクラス。`None` は huge（OS 直接）。確保と解放で同じ関数を使う。
#[inline(always)]
fn classify(layout: &Layout) -> Option<usize> {
    let size = layout.size();
    let align = layout.align();
    if align <= MIN_ALIGN {
        if size <= MAX_CLASS_SIZE {
            Some(class_of(size))
        } else {
            None
        }
    } else if align <= SLOT_SIZE {
        class_of_aligned(size, align)
    } else {
        None
    }
}

// SAFETY: 返すブロックは layout のサイズ以上・アライメントを満たし、解放されるまで
// 他の確保と重ならない。解放は同じ layout で同じ分類を通る。
unsafe impl GlobalAlloc for VeilAlloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match classify(&layout) {
            // SAFETY: class は有効なサイズクラス。
            Some(c) => unsafe { heap::alloc(c) },
            None => huge::alloc(layout.size(), layout.align()),
        }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        match classify(&layout) {
            // SAFETY: class は有効なサイズクラス。
            Some(c) => unsafe { heap::alloc_zeroed(c, layout.size()) },
            // 新規マッピングはゼロ。
            None => huge::alloc(layout.size(), layout.align()),
        }
    }

    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        match classify(&layout) {
            // SAFETY: p は同じ layout で確保されたブロック。
            Some(_) => unsafe { heap::free(p) },
            None => huge::free(p, layout.size()),
        }
    }

    #[inline]
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: GlobalAlloc::realloc の契約により new_size は align で丸めても isize::MAX 以下。
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        let old_class = classify(&layout);
        let new_class = classify(&new_layout);
        match (old_class, new_class) {
            (Some(a), Some(b)) if a == b => return p,
            (None, None) => {
                let q = huge::realloc(p, layout.size(), new_size, layout.align());
                if !q.is_null() {
                    return q;
                }
            }
            _ => {}
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

/// クラスのブロックサイズ（テスト・診断用）。
pub fn usable_size(layout: Layout) -> usize {
    match classify(&layout) {
        Some(c) => CLASS_SIZE[c] as usize,
        None => layout.size(),
    }
}

/// アイドル時の回収。
///
/// 現在のスレッドのヒープについて、他スレッドからの解放の回収・空ページの返却・
/// 期限切れ（`force` なら全部）の purge・余分な空セグメントの返却を行い、
/// 放棄されたヒープと共有ヒープも（ロックが取れれば）同様に掃除する。
/// ワーカーのアイドル時（accept の 1 秒タイムアウト）から呼ぶ。負荷中に呼ばれても
/// 正しく、コストは空いたスパン数と所有セグメント数に比例する（ブロック数ではない）。
pub fn collect(force: bool) {
    heap::collect(force);
}

/// アロケータの状態（プロセス全体、遅いパスで更新する近似値）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// OS からマップ中のセグメント数（キャッシュ分を含む）。
    pub segments: usize,
    /// 再利用キャッシュ中の空セグメント数。
    pub cached_segments: usize,
    /// スパンに使われているバイト数。
    pub active_bytes: usize,
    /// 空きだが OS へ返していない（purge 待ちの）バイト数。
    pub dirty_bytes: usize,
    /// huge（OS 直接）のバイト数。
    pub huge_bytes: usize,
    /// これまでに purge したバイト数の累計。
    pub purged_bytes_total: usize,
    /// ヒープ数（放棄中を含む）。
    pub heaps: usize,
    /// 放棄中のヒープ数。
    pub abandoned_heaps: usize,
    /// 他スレッドからの解放の累計回数。
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

/// 環境変数のチューニング値を読む（最初のヒープ生成時に 1 回。アロケーションしない）。
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
