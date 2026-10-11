//! アロケータのマイクロベンチ（F-183）。veil のデータプレーンに近い確保パターンで
//! veil-alloc / mimalloc / システムアロケータを `GlobalAlloc` 経由で直接呼んで比べる。
//!
//! 各シナリオを全アロケータで交互に `ROUNDS` 回ずつ走らせ、ns/op の中央値を出す。

#[path = "../../../../src/veil_alloc/mod.rs"]
mod veil_alloc;

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

const ROUNDS: usize = 5;

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

/// veil のデータプレーンに近いサイズ分布: 小物（ヘッダ・状態・Arc）が大半、
/// 一部が TLS レコード・HTTP/2 フレーム級、まれに 64KB バッファ・それ以上。
fn veil_size(rng: &mut Rng) -> usize {
    match rng.below(1000) {
        0..=699 => 8 + rng.below(248) as usize,
        700..=899 => 256 + rng.below(1792) as usize,
        900..=979 => 2048 + rng.below(14336) as usize,
        980..=996 => 16384 + rng.below(49152) as usize,
        _ => 65536 + rng.below(262144) as usize,
    }
}

fn l(size: usize) -> Layout {
    Layout::from_size_align(size, 8).unwrap()
}

/// 確保直後の即解放（LIFO、高速パスそのもの）。
fn lifo_small<A: GlobalAlloc>(a: &A) -> (u64, u64) {
    let n = 5_000_000;
    let t = Instant::now();
    for i in 0..n {
        let lay = l(16 + (i & 7) * 16);
        unsafe {
            let p = a.alloc(lay);
            black_box(p);
            *p = 1;
            a.dealloc(p, lay);
        }
    }
    (t.elapsed().as_nanos() as u64, n as u64 * 2)
}

/// 要求処理 1 回分のバッチ: 数十個を確保して触り、ばらばらの順で解放する。
fn request_batch<A: GlobalAlloc>(a: &A) -> (u64, u64) {
    let mut rng = Rng(42);
    let mut v: Vec<(*mut u8, Layout)> = Vec::with_capacity(64);
    let iters = 200_000;
    let mut ops = 0u64;
    let t = Instant::now();
    for _ in 0..iters {
        let k = 8 + rng.below(40) as usize;
        for _ in 0..k {
            let lay = l(veil_size(&mut rng).min(70_000));
            unsafe {
                let p = a.alloc(lay);
                *p = 1;
                v.push((p, lay));
            }
        }
        while !v.is_empty() {
            let i = rng.below(v.len() as u64) as usize;
            let (p, lay) = v.swap_remove(i);
            unsafe { a.dealloc(p, lay) };
        }
        ops += 2 * k as u64;
    }
    (t.elapsed().as_nanos() as u64, ops)
}

/// 長寿命の生存集合（キープアライブ接続の状態）を保ったままの置き換え。
fn live_set<A: GlobalAlloc>(a: &A) -> (u64, u64) {
    let mut rng = Rng(7);
    let live = 50_000;
    let mut v: Vec<(*mut u8, Layout)> = (0..live)
        .map(|_| {
            let lay = l(veil_size(&mut rng));
            (unsafe { a.alloc(lay) }, lay)
        })
        .collect();
    let n = 2_000_000;
    let t = Instant::now();
    for _ in 0..n {
        let i = rng.below(live as u64) as usize;
        let lay = l(veil_size(&mut rng));
        unsafe {
            a.dealloc(v[i].0, v[i].1);
            let p = a.alloc(lay);
            *p = 1;
            v[i] = (p, lay);
        }
    }
    let ns = t.elapsed().as_nanos() as u64;
    for (p, lay) in v {
        unsafe { a.dealloc(p, lay) };
    }
    (ns, n as u64 * 2)
}

/// 生存集合の置き換えをサイズ帯ごとに（live_set の内訳の切り分け用）。
fn live_band<A: GlobalAlloc>(a: &A, lo: usize, hi: usize) -> (u64, u64) {
    let mut rng = Rng(11);
    let live = if hi > 65536 { 500 } else if hi > 16384 { 3000 } else { 50_000 };
    let size = |rng: &mut Rng| lo + rng.below((hi - lo) as u64) as usize;
    let mut v: Vec<(*mut u8, Layout)> = (0..live)
        .map(|_| {
            let lay = l(size(&mut rng));
            (unsafe { a.alloc(lay) }, lay)
        })
        .collect();
    let n = 2_000_000;
    let t = Instant::now();
    for _ in 0..n {
        let i = rng.below(live as u64) as usize;
        let lay = l(size(&mut rng));
        unsafe {
            a.dealloc(v[i].0, v[i].1);
            let p = a.alloc(lay);
            *p = 1;
            v[i] = (p, lay);
        }
    }
    let ns = t.elapsed().as_nanos() as u64;
    for (p, lay) in v {
        unsafe { a.dealloc(p, lay) };
    }
    (ns, n as u64 * 2)
}
fn live_small<A: GlobalAlloc>(a: &A) -> (u64, u64) {
    live_band(a, 8, 256)
}
fn live_mid<A: GlobalAlloc>(a: &A) -> (u64, u64) {
    live_band(a, 256, 2048)
}
fn live_big<A: GlobalAlloc>(a: &A) -> (u64, u64) {
    live_band(a, 2048, 16384)
}
fn live_xl<A: GlobalAlloc>(a: &A) -> (u64, u64) {
    live_band(a, 16384, 65536)
}
fn live_large<A: GlobalAlloc>(a: &A) -> (u64, u64) {
    live_band(a, 65536, 327680)
}

/// 64KB 読み取りバッファ級の確保・解放（プール外れのとき）。
fn buf64k<A: GlobalAlloc>(a: &A) -> (u64, u64) {
    let lay = l(65536);
    let mut held: Vec<*mut u8> = Vec::with_capacity(128);
    let n = 400_000;
    let t = Instant::now();
    for i in 0..n {
        unsafe {
            let p = a.alloc(lay);
            *p = 1;
            *p.add(65535) = 1;
            held.push(p);
            if held.len() == 128 || i % 3 == 0 {
                let q = held.swap_remove(0);
                a.dealloc(q, lay);
            }
        }
    }
    let ns = t.elapsed().as_nanos() as u64;
    for p in held {
        unsafe { a.dealloc(p, lay) };
    }
    (ns, n as u64 * 2)
}

/// Vec の倍々成長（応答の蓄積バッファ）。
fn vec_growth<A: GlobalAlloc>(a: &A) -> (u64, u64) {
    let n = 200_000;
    let mut ops = 0u64;
    let t = Instant::now();
    for _ in 0..n {
        unsafe {
            let mut size = 16;
            let mut p = a.alloc(l(size));
            while size < 65536 {
                p = a.realloc(p, l(size), size * 2);
                *p.add(size) = 1;
                size *= 2;
                ops += 1;
            }
            a.dealloc(p, l(size));
            ops += 2;
        }
    }
    (t.elapsed().as_nanos() as u64, ops)
}

/// 他スレッドでの解放（ロガー・オフロード越しの受け渡し）。
fn cross_thread<A: GlobalAlloc + Sync + 'static>(a: &'static A) -> (u64, u64) {
    let batches = 2_000;
    let per = 1000;
    let (tx, rx) = mpsc::sync_channel::<Vec<usize>>(8);
    let t = Instant::now();
    let consumer = thread::spawn(move || {
        for b in rx {
            for p in b {
                unsafe { a.dealloc(p as *mut u8, l(64)) };
            }
        }
    });
    let mut batch = Vec::with_capacity(per);
    for _ in 0..batches {
        for _ in 0..per {
            let p = unsafe { a.alloc(l(64)) };
            unsafe { *p = 1 };
            batch.push(p as usize);
        }
        tx.send(std::mem::replace(&mut batch, Vec::with_capacity(per)))
            .unwrap();
    }
    drop(tx);
    consumer.join().unwrap();
    (t.elapsed().as_nanos() as u64, (batches * per * 2) as u64)
}

/// 4 スレッドが独立に live_set を回す（スレッドローカル性・スケーラビリティ）。
fn mt_live<A: GlobalAlloc + Sync + 'static>(a: &'static A) -> (u64, u64) {
    let t = Instant::now();
    let hs: Vec<_> = (0..4)
        .map(|_| thread::spawn(move || live_set(a).1))
        .collect();
    let ops: u64 = hs.into_iter().map(|h| h.join().unwrap()).sum();
    (t.elapsed().as_nanos() as u64, ops)
}

static VA: veil_alloc::VeilAlloc = veil_alloc::VeilAlloc;
static MI: mimalloc::MiMalloc = mimalloc::MiMalloc;
static SYS: System = System;

type Scen = (&'static str, fn(usize) -> (u64, u64));

macro_rules! scen {
    ($name:literal, $f:ident) => {
        ($name, |which| match which {
            0 => $f(&VA),
            1 => $f(&MI),
            _ => $f(&SYS),
        })
    };
}

fn main() {
    // 分類の健全性（ベンチの前提）: 64KiB はちょうど 1 クラス、1MiB 超は OS 直接。
    assert_eq!(veil_alloc::usable_size(l(65536)), 65536);
    assert_eq!(veil_alloc::usable_size(l(2 << 20)), 2 << 20);
    let filter: Vec<String> = std::env::args().skip(1).collect();
    let scenarios: Vec<Scen> = vec![
        scen!("lifo_small", lifo_small),
        scen!("request_batch", request_batch),
        scen!("live_set", live_set),
        scen!("live_small", live_small),
        scen!("live_mid", live_mid),
        scen!("live_big", live_big),
        scen!("live_xl", live_xl),
        scen!("live_large", live_large),
        scen!("buf64k", buf64k),
        scen!("vec_growth", vec_growth),
        scen!("cross_thread", cross_thread),
        scen!("mt_live", mt_live),
    ];
    let names = ["veil-alloc", "mimalloc", "system"];
    println!("| scenario | {} |", names.join(" | "));
    println!("|---|---|---|---|");
    for (name, f) in scenarios {
        if !filter.is_empty() && !filter.iter().any(|x| x == name) {
            continue;
        }
        let mut res: [Vec<f64>; 3] = [vec![], vec![], vec![]];
        for _ in 0..ROUNDS {
            for (w, r) in res.iter_mut().enumerate() {
                // 別スレッドで走らせ、スレッドローカルな状態の持ち越しを避ける。
                let (ns, ops) = thread::spawn(move || f(w)).join().unwrap();
                r.push(ns as f64 / ops as f64);
            }
        }
        let med = |v: &mut Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        let m: Vec<f64> = res.iter_mut().map(med).collect();
        println!(
            "| {name} | {:.2} | {:.2} ({:+.0}%) | {:.2} ({:+.0}%) |",
            m[0],
            m[1],
            (m[1] / m[0] - 1.0) * 100.0,
            m[2],
            (m[2] / m[0] - 1.0) * 100.0
        );
    }
    // 全シナリオ後の veil-alloc の保持量（OS へ返し切れていない量）。
    veil_alloc::collect(true);
    let st = veil_alloc::stats();
    println!(
        "\nveil-alloc after collect(true): active={}KiB dirty={}KiB huge={}KiB segments={} cached={} heaps={} abandoned={} purged_total={}MiB remote_frees={}",
        st.active_bytes / 1024,
        st.dirty_bytes / 1024,
        st.huge_bytes / 1024,
        st.segments,
        st.cached_segments,
        st.heaps,
        st.abandoned_heaps,
        st.purged_bytes_total >> 20,
        st.remote_frees_total
    );
}
