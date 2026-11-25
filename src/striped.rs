//! スレッドごとに行を分けた計数器（F-321）。
//!
//! 全ワーカーが要求ごとに同じ `AtomicU64` へ `fetch_add` すると、そのキャッシュラインの持ち主がコアの間を往復し、
//! 多コアでは 1 回が 100 ns を超え、ライン 1 本が捌ける書き込みの数で全体の要求の数が頭打ちになる。
//! [`StripedCounter`] はスレッドごとに別の行（スロット）へ足し、読むときに合計する。読むのは試験・診断・
//! メトリクスの読み出しだけで、足すより桁違いに少ない。
//!
//! スロットはスレッドが最初に足したときに順に割り当てる（[`STRIPES`] 本を超えるスレッドは巡回して相乗りする）。

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// スロットの数（ワーカーの数がこれ以下なら、ワーカーどうしが同じ行に書かない）
pub(crate) const STRIPES: usize = 64;

/// 1 スロット。隣のラインまで取りにくるプリフェッチ（隣接ラインの空間プリフェッチ）でも隣のスロットと
/// 混ざらないよう 128 バイトにそろえる。
#[repr(align(128))]
struct Slot(AtomicU64);

/// スロットの割り当ての次の番号
static NEXT_STRIPE: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// このスレッドのスロットの番号（`usize::MAX` = まだ割り当てていない）
    static STRIPE: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// このスレッドのスロットの番号
#[inline]
fn stripe() -> usize {
    STRIPE.with(|s| {
        let v = s.get();
        if v != usize::MAX {
            return v;
        }
        let n = NEXT_STRIPE.fetch_add(1, Ordering::Relaxed) % STRIPES;
        s.set(n);
        n
    })
}

/// スレッドごとに行を分けた計数器（足すのは速く、読むのは [`STRIPES`] 本の合計）
pub(crate) struct StripedCounter {
    slots: Box<[Slot; STRIPES]>,
}

impl StripedCounter {
    pub(crate) fn new() -> Self {
        Self {
            slots: Box::new(std::array::from_fn(|_| Slot(AtomicU64::new(0)))),
        }
    }

    /// 1 足す
    #[inline]
    pub(crate) fn incr(&self) {
        self.slots[stripe()].0.fetch_add(1, Ordering::Relaxed);
    }

    /// 全スロットの合計
    pub(crate) fn get(&self) -> u64 {
        self.slots
            .iter()
            .fold(0u64, |a, s| a.wrapping_add(s.0.load(Ordering::Relaxed)))
    }

    /// 0 に戻す（読み出しと同時に足すスレッドの分は残ることがある）
    pub(crate) fn reset(&self) {
        for s in self.slots.iter() {
            s.0.store(0, Ordering::Relaxed);
        }
    }
}

impl Default for StripedCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for StripedCounter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn sums_over_threads_and_resets() {
        let c = Arc::new(StripedCounter::new());
        let hs: Vec<_> = (0..8)
            .map(|_| {
                let c = Arc::clone(&c);
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        c.incr();
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        c.incr();
        assert_eq!(c.get(), 8001);
        c.reset();
        assert_eq!(c.get(), 0);
    }

    #[test]
    fn a_thread_keeps_its_stripe() {
        let a = stripe();
        assert_eq!(stripe(), a);
        assert!(a < STRIPES);
    }
}
