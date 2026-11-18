//! 静的ファイルを丸ごと読んでいる量の上限（B-321）。`cache` feature の有無で同じ上限を使う（B-427: 以前は `cache` の
//! 無いビルドの静的配信が数えておらず、HTTP/2・HTTP/3 のストリームごとに大きなファイルを丸ごと読み込めた）

use bytes::Bytes;
use std::sync::atomic::{AtomicU64, Ordering};

/// 丸ごと読んでいる量（`Busy` の判定）
static UNCACHED_INFLIGHT: AtomicU64 = AtomicU64::new(0);
/// その上限。1 つも読んでいなければ上限を超える大きさのファイルも 1 つは読む
const UNCACHED_INFLIGHT_MAX: u64 = 256 * 1024 * 1024;

/// 読む量を予約する。返した値を落とすと戻す
pub(super) struct UncachedRead {
    counter: &'static AtomicU64,
    len: u64,
}

impl UncachedRead {
    pub(super) fn reserve_in(counter: &'static AtomicU64, max: u64, len: u64) -> Option<Self> {
        let mut cur = counter.load(Ordering::Relaxed);
        loop {
            if cur != 0 && cur.saturating_add(len) > max {
                return None;
            }
            match counter.compare_exchange_weak(
                cur,
                cur + len,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(Self { counter, len }),
                Err(c) => cur = c,
            }
        }
    }
}

impl Drop for UncachedRead {
    fn drop(&mut self) {
        self.counter.fetch_sub(self.len, Ordering::Relaxed);
    }
}

/// プロセス全体の上限に対して `len` バイトを予約する。`None` は上限に達した
pub(super) fn reserve_whole(len: u64) -> Option<UncachedRead> {
    UncachedRead::reserve_in(&UNCACHED_INFLIGHT, UNCACHED_INFLIGHT_MAX, len)
}

/// 予約を本文の寿命（送り終えて落とすまで）に結び付ける
struct ChargedBytes {
    data: Bytes,
    _reserved: UncachedRead,
}

impl AsRef<[u8]> for ChargedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

pub(super) fn charge(data: Bytes, reserved: Option<UncachedRead>) -> Bytes {
    match reserved {
        Some(r) => Bytes::from_owner(ChargedBytes { data, _reserved: r }),
        None => data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// B-321: 読み込みは上限まで。予約は本文を落とすと戻る
    #[test]
    fn uncached_read_budget() {
        static C: AtomicU64 = AtomicU64::new(0);
        let first = UncachedRead::reserve_in(&C, 100, 101).expect("one read is always allowed");
        let data = charge(Bytes::from_static(b"x"), Some(first));
        assert!(UncachedRead::reserve_in(&C, 100, 1).is_none());
        let copy = data.clone();
        drop(data);
        assert!(UncachedRead::reserve_in(&C, 100, 1).is_none());
        drop(copy);
        let a = UncachedRead::reserve_in(&C, 100, 60).expect("released");
        assert!(UncachedRead::reserve_in(&C, 100, 50).is_none());
        let b = UncachedRead::reserve_in(&C, 100, 40).expect("fits");
        drop((a, b));
        assert_eq!(C.load(Ordering::Relaxed), 0);
    }
}
