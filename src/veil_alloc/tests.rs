//! Unit tests for veil-alloc. They call `VeilAlloc` directly, independent of the global allocator.

use super::*;
use core::alloc::{GlobalAlloc, Layout};
use std::collections::HashSet;
use std::sync::mpsc;
use std::thread;

const A: VeilAlloc = VeilAlloc;

fn layout(size: usize, align: usize) -> Layout {
    Layout::from_size_align(size, align).unwrap()
}

/// A simple xorshift (reproducible random numbers without an external crate).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

unsafe fn fill(p: *mut u8, size: usize, tag: u8) {
    unsafe { core::ptr::write_bytes(p, tag, size) };
}

unsafe fn check(p: *const u8, size: usize, tag: u8) {
    for i in [0, size / 2, size.saturating_sub(1)] {
        assert_eq!(unsafe { *p.add(i) }, tag, "破損: offset {i}/{size}");
    }
}

#[test]
fn every_class_allocates_aligned_distinct_blocks() {
    thread::spawn(|| unsafe {
        for &class_size in CLASS_SIZE.iter() {
            let size = class_size as usize;
            let l = layout(size, 8);
            let mut seen = HashSet::new();
            let mut ptrs = Vec::new();
            for i in 0..64 {
                let p = A.alloc(l);
                assert!(!p.is_null());
                assert_eq!(p as usize % 16, 0);
                assert!(seen.insert(p as usize), "同じブロックを 2 回返した");
                fill(p, size, i as u8);
                ptrs.push(p);
            }
            for (i, p) in ptrs.iter().enumerate() {
                check(*p, size, i as u8);
                A.dealloc(*p, l);
            }
        }
    })
    .join()
    .unwrap();
}

#[test]
fn random_alloc_free_keeps_contents() {
    thread::spawn(|| unsafe {
        let mut rng = Rng(0x9e3779b97f4a7c15);
        let mut live: Vec<(*mut u8, Layout, u8)> = Vec::new();
        for i in 0..200_000u32 {
            if live.len() < 2000 && (live.is_empty() || rng.below(3) != 0) {
                let size = match rng.below(10) {
                    0..=5 => 1 + rng.below(256) as usize,
                    6..=8 => 1 + rng.below(16384) as usize,
                    _ => 1 + rng.below(300_000) as usize,
                };
                let l = layout(size, 8);
                let p = A.alloc(l);
                assert!(!p.is_null());
                let tag = i as u8;
                fill(p, size, tag);
                live.push((p, l, tag));
            } else {
                let idx = rng.below(live.len() as u64) as usize;
                let (p, l, tag) = live.swap_remove(idx);
                check(p, l.size(), tag);
                A.dealloc(p, l);
            }
        }
        for (p, l, tag) in live {
            check(p, l.size(), tag);
            A.dealloc(p, l);
        }
    })
    .join()
    .unwrap();
}

#[test]
fn alloc_zeroed_is_zero_even_after_reuse() {
    thread::spawn(|| unsafe {
        for size in [8usize, 24, 100, 1000, 5000, 70_000, 300_000] {
            let l = layout(size, 8);
            for _ in 0..4 {
                let p = A.alloc(l);
                fill(p, size, 0xAB);
                A.dealloc(p, l);
                let z = A.alloc_zeroed(l);
                for i in 0..size {
                    assert_eq!(*z.add(i), 0, "size {size} offset {i}");
                }
                A.dealloc(z, l);
            }
        }
    })
    .join()
    .unwrap();
}

#[test]
fn realloc_preserves_contents_across_classes_and_huge() {
    thread::spawn(|| unsafe {
        let mut l = layout(10, 8);
        let mut p = A.alloc(l);
        fill(p, 10, 7);
        for new_size in [20, 100, 1000, 70_000, 300_000, 2_000_000, 500_000, 50, 10] {
            let keep = l.size().min(new_size);
            check(p, keep, 7);
            let q = A.realloc(p, l, new_size);
            assert!(!q.is_null());
            check(q, keep, 7);
            fill(q, new_size, 7);
            p = q;
            l = layout(new_size, 8);
        }
        A.dealloc(p, l);
    })
    .join()
    .unwrap();
}

#[test]
fn aligned_allocations_respect_alignment() {
    thread::spawn(|| unsafe {
        for align in [32usize, 64, 128, 256, 4096, 65536, 1 << 20] {
            for size in [1usize, 31, 100, 4096, 70_000, 400_000] {
                let l = layout(size, align);
                let p = A.alloc(l);
                assert!(!p.is_null());
                assert_eq!(p as usize % align, 0, "align {align} size {size}");
                fill(p, size, 3);
                let z = A.alloc_zeroed(l);
                assert_eq!(z as usize % align, 0);
                assert_eq!(*z.add(size - 1), 0);
                A.dealloc(z, l);
                A.dealloc(p, l);
            }
        }
    })
    .join()
    .unwrap();
}

#[test]
fn cross_thread_frees_are_returned_to_owner() {
    let (tx, rx) = mpsc::channel::<Vec<usize>>();
    let (back_tx, back_rx) = mpsc::channel::<()>();
    let owner = thread::spawn(move || unsafe {
        let l = layout(64, 8);
        for round in 0..20 {
            let ptrs: Vec<usize> = (0..5000)
                .map(|i| {
                    let p = A.alloc(l);
                    fill(p, 64, (round + i) as u8);
                    p as usize
                })
                .collect();
            tx.send(ptrs).unwrap();
            back_rx.recv().unwrap();
            // 投函されたブロックは次の遅いパスで回収されて再利用される。
            collect(false);
        }
    });
    let freer = thread::spawn(move || unsafe {
        let l = layout(64, 8);
        for ptrs in rx {
            for p in ptrs {
                A.dealloc(p as *mut u8, l);
            }
            back_tx.send(()).unwrap();
        }
    });
    owner.join().unwrap();
    freer.join().unwrap();
    assert!(stats().remote_frees_total >= 100_000);
}

#[test]
fn exited_threads_heaps_are_reused() {
    // 先にヒープを持つスレッドを 1 本作って終わらせ、放棄ヒープを用意する。
    thread::spawn(|| unsafe {
        let l = layout(32, 8);
        let p = A.alloc(l);
        A.dealloc(p, l);
    })
    .join()
    .unwrap();
    let before = stats().heaps;
    // 逐次にスレッドを作っては終わらせても、ヒープは引き継がれて増え続けない。
    for _ in 0..50 {
        thread::spawn(|| unsafe {
            let l = layout(128, 8);
            let ptrs: Vec<_> = (0..100).map(|_| A.alloc(l)).collect();
            for p in ptrs {
                A.dealloc(p, l);
            }
        })
        .join()
        .unwrap();
    }
    let after = stats().heaps;
    // 並行して走る他のテストのスレッドが作るヒープの分だけ余裕を見る。
    assert!(after <= before + 16, "heaps {before} -> {after}");
}

#[test]
fn blocks_outliving_their_thread_can_be_freed_anywhere() {
    let ptrs = thread::spawn(|| unsafe {
        let l = layout(256, 8);
        (0..1000)
            .map(|i| {
                let p = A.alloc(l);
                fill(p, 256, i as u8);
                p as usize
            })
            .collect::<Vec<_>>()
    })
    .join()
    .unwrap();
    // 確保したスレッドは終了済み（ヒープは放棄 → 引き継ぎ）。
    unsafe {
        let l = layout(256, 8);
        for (i, p) in ptrs.iter().enumerate() {
            check(*p as *const u8, 256, i as u8);
            A.dealloc(*p as *mut u8, l);
        }
    }
    collect(true);
}

#[test]
fn freed_memory_is_purged_after_collect() {
    thread::spawn(|| unsafe {
        let l = layout(1024, 8);
        let ptrs: Vec<_> = (0..20_000)
            .map(|_| {
                let p = A.alloc(l);
                fill(p, 1024, 1);
                p
            })
            .collect();
        let purged_before = stats().purged_bytes_total;
        for p in ptrs {
            A.dealloc(p, l);
        }
        collect(true);
        let purged = stats().purged_bytes_total - purged_before;
        // 20MB 分のスパンが空いた。ほぼ全部が OS へ返る（直前ページ 1 枚と空セグメント 1 個を除く）。
        assert!(purged >= 16 << 20, "purged {purged}");
    })
    .join()
    .unwrap();
}

#[test]
fn many_threads_hammer_concurrently() {
    let handles: Vec<_> = (0..8)
        .map(|t| {
            thread::spawn(move || unsafe {
                let mut rng = Rng(0x1234_5678 + t);
                let mut live: Vec<(*mut u8, Layout, u8)> = Vec::new();
                for i in 0..50_000u32 {
                    if live.len() < 500 && rng.below(2) == 0 {
                        let l = layout(1 + rng.below(4096) as usize, 8);
                        let p = A.alloc(l);
                        fill(p, l.size(), i as u8);
                        live.push((p, l, i as u8));
                    } else if !live.is_empty() {
                        let (p, l, tag) = live.swap_remove(rng.below(live.len() as u64) as usize);
                        check(p, l.size(), tag);
                        A.dealloc(p, l);
                    }
                }
                // 残りは他スレッドへ渡して解放させる。
                live.into_iter()
                    .map(|(p, l, tag)| (p as usize, l, tag))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let leftovers: Vec<_> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    thread::spawn(move || unsafe {
        for (p, l, tag) in leftovers {
            check(p as *const u8, l.size(), tag);
            A.dealloc(p as *mut u8, l);
        }
    })
    .join()
    .unwrap();
}

#[test]
fn usable_size_matches_class() {
    assert_eq!(usable_size(layout(1, 1)), 16);
    assert_eq!(usable_size(layout(129, 8)), 160);
    assert_eq!(usable_size(layout(65536, 8)), 65536);
    assert_eq!(usable_size(layout(300_000, 8)), 5 * 65536);
    assert_eq!(usable_size(layout(2_000_000, 8)), 2_000_000);
}

#[test]
fn large_blocks_reuse_spans_and_cross_threads() {
    let ptrs = thread::spawn(|| unsafe {
        let mut out = Vec::new();
        for i in 0..64usize {
            let size = 300_000 + i * 10_000;
            let l = layout(size, 8);
            let p = A.alloc(l);
            assert_eq!(p as usize % SLOT_SIZE, 0);
            fill(p, size, i as u8);
            if i % 2 == 0 {
                check(p, size, i as u8);
                A.dealloc(p, l);
            } else {
                out.push((p as usize, size, i as u8));
            }
        }
        // ゼロ埋めの large は再利用スパンでもゼロ。
        let l = layout(400_000, 8);
        let z = A.alloc_zeroed(l);
        assert!((0..400_000).step_by(997).all(|i| *z.add(i) == 0));
        A.dealloc(z, l);
        out
    })
    .join()
    .unwrap();
    unsafe {
        for (p, size, tag) in ptrs {
            check(p as *const u8, size, tag);
            A.dealloc(p as *mut u8, layout(size, 8));
        }
    }
}
