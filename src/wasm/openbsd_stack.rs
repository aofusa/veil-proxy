//! OpenBSD 向け wasmtime ファイバスタックアロケータ（B-52）
//!
//! # 背景
//!
//! `wasmtime::Config::async_support(true)` を使うと、wasm の実行は **ファイバ**
//! （wasmtime が確保した専用スタックへスタックスイッチして実行する仕組み）の上で行われる。
//! wasmtime 既定のファイバスタックは `mmap(PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANON)`
//! で確保される。
//!
//! ところが **OpenBSD 6.4 以降は「スタックポインタが `MAP_STACK` 付きでマップされた
//! 領域を指していること」をカーネルが強制する**。`MAP_STACK` なしの領域をスタックとして
//! 使ったままシステムコール／トラップでカーネルへ入ると、プロセスは SIGSEGV で殺される。
//!
//! `MAP_STACK` は **mmap 時にしか付けられない**（`mprotect` では後付けできない）ため、
//! wasmtime の `StackCreator` 拡張点を使って OpenBSD 専用のスタック確保を差し込む。
//! これは実行方式（Cranelift ネイティブ JIT / Pulley インタープリタ）に関係なく必要で、
//! Pulley でもインタープリタループ自体がファイバスタック上で回る。
//!
//! # レイアウト
//!
//! ```text
//!   base                      base+page                    base+page+size
//!     |<-- guard (PROT_NONE) -->|<-- stack (RW, MAP_STACK) -->|
//!                                                             ^ top()
//! ```
//!
//! 低位側にガードページを 1 枚置き、スタックオーバーフローを検出できるようにする
//! （スタックは高位から低位へ伸びる）。匿名 mmap はカーネルがゼロ埋めするため
//! `zeroed` 要求は常に満たされる。
//!
//! # ホットパスについて
//!
//! スタック確保は wasm 実行ごとではなく **ファイバ生成時**に行われるコールドパス。
//! `mmap`/`munmap` はここでのみ使う。

use std::ops::Range;

use anyhow::{anyhow, Error};
use wasmtime::{StackCreator, StackMemory};

/// OpenBSD で `MAP_STACK` 付きのファイバスタックを確保する `StackCreator`。
pub struct MapStackCreator;

/// `MapStackCreator` が確保した 1 本のスタック。Drop で `munmap` する。
struct MapStackMemory {
    /// ガードページを含む予約領域の先頭。
    base: *mut u8,
    /// ガードページを含む予約領域の全長。
    total: usize,
    /// ページサイズ（= ガード領域の長さ）。
    page: usize,
}

// SAFETY: 保持しているのは自前で mmap した領域のアドレスとサイズのみで、内部可変状態を
// 持たない。領域そのものは wasmtime が所有し、単一のファイバからのみ使用される
// （`StackMemory` の契約）。
unsafe impl Send for MapStackMemory {}
unsafe impl Sync for MapStackMemory {}

impl Drop for MapStackMemory {
    fn drop(&mut self) {
        // SAFETY: base/total は new_stack で mmap したものと同一。
        unsafe {
            libc::munmap(self.base as *mut libc::c_void, self.total);
        }
    }
}

// SAFETY: `top()` はページ境界、`range()` はガードページを除いた RW 領域、
// `guard_range()` は PROT_NONE のガード領域をそれぞれ正しく返す。
unsafe impl StackMemory for MapStackMemory {
    fn top(&self) -> *mut u8 {
        // SAFETY: base + total は自前 mmap 領域の終端（1 past the end）。
        unsafe { self.base.add(self.total) }
    }

    fn range(&self) -> Range<usize> {
        let start = self.base as usize + self.page;
        start..(self.base as usize + self.total)
    }

    fn guard_range(&self) -> Range<*mut u8> {
        // SAFETY: base..base+page は mmap 済み領域内。
        self.base..unsafe { self.base.add(self.page) }
    }
}

// SAFETY: 返すスタックは要求サイズ以上・ページアラインで、低位側に PROT_NONE の
// ガードページを備える（`StackCreator` の契約）。
unsafe impl StackCreator for MapStackCreator {
    fn new_stack(&self, size: usize, _zeroed: bool) -> Result<Box<dyn StackMemory>, Error> {
        // SAFETY: sysconf は副作用のない問い合わせ。
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page <= 0 {
            return Err(anyhow!("sysconf(_SC_PAGESIZE) failed"));
        }
        let page = page as usize;

        // 要求サイズをページへ切り上げ、ガードページ 1 枚を足す。
        let stack_size = size.div_ceil(page) * page;
        let total = stack_size
            .checked_add(page)
            .ok_or_else(|| anyhow!("fiber stack size overflow"))?;

        // 1) 全体を PROT_NONE で予約する（ガードページはこのまま残す）。
        // SAFETY: addr=NULL の匿名 mmap。失敗は MAP_FAILED で判定する。
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                total,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(anyhow!(
                "mmap for wasm fiber stack failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let base = base as *mut u8;

        // 2) ガードページより上を **MAP_STACK 付きで貼り直す**。
        //    MAP_STACK は mmap 時にしか指定できないため MAP_FIXED で上書きする。
        // SAFETY: base..base+total は直前に予約した自前の領域。上書き対象はその内側
        //         （base+page 以降）に収まる。
        let stack = unsafe {
            libc::mmap(
                base.add(page) as *mut libc::c_void,
                stack_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED | libc::MAP_STACK,
                -1,
                0,
            )
        };
        if stack == libc::MAP_FAILED {
            let err = std::io::Error::last_os_error();
            // SAFETY: 予約済み領域の解放。
            unsafe { libc::munmap(base as *mut libc::c_void, total) };
            return Err(anyhow!(
                "mmap(MAP_STACK) for wasm fiber stack failed: {}",
                err
            ));
        }

        Ok(Box::new(MapStackMemory { base, total, page }))
    }
}
