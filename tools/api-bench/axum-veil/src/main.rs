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

fn main() {
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
