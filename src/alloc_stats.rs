//! アロケーション計測（F-165 Phase 1: `alloc-stats` feature）。
//!
//! 「1 リクエストあたりのヒープアロケーション回数」を**実測**するための診断専用
//! ビルドを提供する。内側のアロケータ（mimalloc / jemalloc / システムアロケータ）
//! を [`CountingAllocator`] で包み、`alloc` / `dealloc` / `realloc` の呼び出し回数と
//! 確保バイト数を [`AtomicU64`] でカウントする。
//!
//! この feature はデフォルトビルドに含まれない（`default` 不変・opt-in）。
//! 無効時はモジュール自体がコンパイルされないため、通常ビルドへの影響はゼロ
//! （コード生成・実行時オーバーヘッドとも無し）。
//!
//! ## なぜ `Ordering::Relaxed` で良いか
//!
//! カウンタは負荷試験時に「大まかな傾向」を掴むための**参考値**であり、他のメモリ
//! 操作との happens-before 関係を必要としない。アロケータ呼び出し自体は各スレッドが
//! 独立して行うため、カウンタ間の順序性・可視性の即時性を保証する必要はなく、
//! `SeqCst`/`AcqRel` によるメモリバリアコストを払う理由がない。合計値は
//! `snapshot()` 呼び出し時点までに各 CPU から見えているインクリメントを集計した
//! 近似値になるが、計測用途では十分な精度である。
//!
//! ## 使い方
//!
//! ```ignore
//! use veil::alloc_stats;
//!
//! // ウォームアップ（ワーカー起動・設定ロードなどのコールドパス分を除外）後に
//! // カウンタをゼロクリアしてから負荷をかける
//! alloc_stats::reset();
//!
//! // 例: h2load -n 10000 ... で 10000 リクエストを打つ
//!
//! let snap = alloc_stats::snapshot();
//! println!("allocs/req = {}", snap.allocs as f64 / 10000.0);
//! ```
//!
//! `metrics` feature も同時に有効な場合は `veil_alloc_allocs_total` 等の
//! Prometheus ゲージとしてスクレイプ時点のスナップショットが公開される
//! （[`crate::metrics`] を参照）。

use std::alloc::{GlobalAlloc, Layout};
use std::sync::atomic::{AtomicU64, Ordering};

/// アロケーション回数（`alloc` + `alloc_zeroed`）
static ALLOCS: AtomicU64 = AtomicU64::new(0);
/// 解放回数（`dealloc`）
static DEALLOCS: AtomicU64 = AtomicU64::new(0);
/// 再確保回数（`realloc`）
static REALLOCS: AtomicU64 = AtomicU64::new(0);
/// 確保した合計バイト数（`alloc`/`alloc_zeroed` は要求サイズ、`realloc` は新サイズを加算）
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

/// 内側のアロケータ `A` に処理を委譲しつつ、呼び出し回数・バイト数を計測する
/// `#[global_allocator]`。
///
/// カウンタ自体はモジュールレベルの `static AtomicU64`（本プロセス内で唯一の
/// グローバルアロケータを想定）で保持するため、`CountingAllocator` 自体は
/// ゼロサイズラッパーとして振る舞う。
pub struct CountingAllocator<A>(pub A);

// SAFETY: `alloc`/`dealloc`/`realloc`/`alloc_zeroed` は全て内側の `A` の実装へ
// そのまま委譲しており、カウンタ更新（`Relaxed` の `AtomicU64::fetch_add`）は
// メモリ安全性に影響しない副作用のみを持つ。`Layout` はそのまま渡すため、
// `A: GlobalAlloc` の安全性契約をそのまま引き継ぐ。
unsafe impl<A: GlobalAlloc> GlobalAlloc for CountingAllocator<A> {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { self.0.alloc(layout) }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        DEALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { self.0.dealloc(ptr, layout) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { self.0.alloc_zeroed(layout) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        REALLOCS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        unsafe { self.0.realloc(ptr, layout, new_size) }
    }
}

/// ある時点でのアロケーションカウンタのスナップショット。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocSnapshot {
    /// `alloc` + `alloc_zeroed` の呼び出し回数
    pub allocs: u64,
    /// `dealloc` の呼び出し回数
    pub deallocs: u64,
    /// `realloc` の呼び出し回数
    pub reallocs: u64,
    /// 確保した合計バイト数
    pub alloc_bytes: u64,
}

/// 現在のアロケーションカウンタを読み取る。
///
/// 負荷試験では `reset()` 直後から `snapshot()` までの間に処理したリクエスト数で
/// `allocs` を割ることで「1 リクエストあたりのアロケーション回数」を求める。
pub fn snapshot() -> AllocSnapshot {
    AllocSnapshot {
        allocs: ALLOCS.load(Ordering::Relaxed),
        deallocs: DEALLOCS.load(Ordering::Relaxed),
        reallocs: REALLOCS.load(Ordering::Relaxed),
        alloc_bytes: ALLOC_BYTES.load(Ordering::Relaxed),
    }
}

/// 全カウンタをゼロクリアする。
///
/// ワーカー起動・設定ロードなど計測対象外のコールドパスで発生したアロケーションを
/// 除外するため、負荷試験のウォームアップ完了後に呼び出す想定。
pub fn reset() {
    ALLOCS.store(0, Ordering::Relaxed);
    DEALLOCS.store(0, Ordering::Relaxed);
    REALLOCS.store(0, Ordering::Relaxed);
    ALLOC_BYTES.store(0, Ordering::Relaxed);
}

/// 現在のスナップショットを `tag` 付きでログ出力する。
///
/// テストやツールから任意のタイミングで手軽に呼べる補助関数。定期的な
/// バックグラウンドポーリングは行わない（呼び出し側が明示的にトリガーする）。
pub fn log_snapshot(tag: &str) {
    let snap = snapshot();
    ftlog::info!(
        "alloc-stats[{}]: allocs={} deallocs={} reallocs={} alloc_bytes={}",
        tag,
        snap.allocs,
        snap.deallocs,
        snap.reallocs,
        snap.alloc_bytes,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // グローバルカウンタを共有するため、テストを直列化する。
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn counters_increase_across_vec_allocation() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset();
        let before = snapshot();

        let mut v: Vec<u64> = Vec::with_capacity(4);
        for i in 0..1024u64 {
            v.push(i);
        }
        // ドロップして dealloc を発生させる
        drop(v);

        let after = snapshot();
        assert!(after.allocs > before.allocs, "alloc count should increase");
        assert!(
            after.deallocs > before.deallocs,
            "dealloc count should increase"
        );
        assert!(
            after.alloc_bytes > before.alloc_bytes,
            "alloc bytes should increase"
        );
    }

    #[test]
    fn reset_zeroes_all_counters() {
        let _guard = TEST_LOCK.lock().unwrap();

        // 何かしらアロケーションを発生させてからリセットする
        let v: Vec<u8> = vec![1, 2, 3, 4];
        drop(v);

        reset();
        let snap = snapshot();
        assert_eq!(
            snap,
            AllocSnapshot {
                allocs: 0,
                deallocs: 0,
                reallocs: 0,
                alloc_bytes: 0,
            }
        );
    }
}
