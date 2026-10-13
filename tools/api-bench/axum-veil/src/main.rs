//! axum + veil の tokio 互換レイヤー、および veil の API ライブラリ。
//! `api-bench-axum-veil <serve|percore|mount|native> [addr] [threads]`
//!
//! - `serve` / `percore`: axum-tokio と同じコード（tokio は veil の shim）。
//! - `mount`: veil の HTTP サーバ（ワーカーごとに受理）に axum の Router を `tokio_service` で載せる。
//! - `native`: 同じ 2 つのエンドポイントを veil のネイティブハンドラ（`#[veil::route]`）で書いたもの。
#[path = "../../common.rs"]
mod common;

use veil::prelude::*;

#[veil::route(path = "/plaintext", method = "GET")]
async fn plaintext() -> &'static str {
    "Hello, World!"
}

#[veil::route(path = "/json/{name}", method = "GET")]
async fn json(name: String) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "message": format!("Hello, {name}") }))
}

fn config(addr: &str, threads: usize) -> Config {
    Config::new()
        .builder()
        .listen_cleartext(addr)
        .threads(threads)
        .log_level("warn")
        .openapi_enabled(false)
        .build()
}

/// `--features count-allocs`: 確保（alloc・realloc）の累計を 1 秒ごとに標準エラーへ出す（allocs/req を求める）。
#[cfg(feature = "count-allocs")]
mod count_allocs {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    static ALLOCS: AtomicU64 = AtomicU64::new(0);
    /// `ALLOC_TRACE=<n>`: 起動から 4 秒後（負荷の最中）に、確保 n 回ぶんのバックトレースを出す（どこで確保しているかを見る）
    static TRACE_LEFT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

    thread_local! {
        static IN_TRACE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    fn maybe_trace(l: Layout) {
        if TRACE_LEFT.load(Relaxed) <= 0 {
            return;
        }
        let _ = IN_TRACE.try_with(|t| {
            if t.get() || TRACE_LEFT.fetch_sub(1, Relaxed) <= 0 {
                return;
            }
            t.set(true);
            eprintln!("=== alloc size={}\n{}", l.size(), std::backtrace::Backtrace::force_capture());
            t.set(false);
        });
    }

    pub struct Counting;

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            ALLOCS.fetch_add(1, Relaxed);
            maybe_trace(l);
            System.alloc(l)
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            System.dealloc(p, l)
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
            ALLOCS.fetch_add(1, Relaxed);
            maybe_trace(l);
            System.realloc(p, l, n)
        }
    }

    #[global_allocator]
    static GLOBAL: Counting = Counting;

    pub fn report_every_second() {
        if let Some(n) = std::env::var("ALLOC_TRACE").ok().and_then(|v| v.parse().ok()) {
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(4));
                TRACE_LEFT.store(n, Relaxed);
            });
        }
        std::thread::spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            eprintln!("allocs {}", ALLOCS.load(Relaxed));
        });
    }
}

fn main() {
    #[cfg(feature = "count-allocs")]
    count_allocs::report_every_second();
    let a = common::args();
    match a.mode.as_str() {
        "serve" => common::run_serve(&a.addr, a.threads),
        "percore" => common::run_percore(&a.addr, a.threads),
        "mount" => App::new()
            .config(config(&a.addr, a.threads))
            .without_macro_routes()
            .tokio_service(&["GET"], "/{*rest}", common::router())
            .run()
            .expect("run"),
        "native" => App::new()
            .config(config(&a.addr, a.threads))
            .run()
            .expect("run"),
        m => panic!("unknown mode {m} (serve|percore|mount|native)"),
    }
}
