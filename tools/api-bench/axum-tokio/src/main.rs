//! axum + 本物の tokio。`api-bench-axum-tokio <serve|percore> [addr] [threads]`
#[path = "../../common.rs"]
mod common;

fn main() {
    let a = common::args();
    match a.mode.as_str() {
        "serve" => common::run_serve(&a.addr, a.threads),
        "percore" => common::run_percore(&a.addr, a.threads),
        m => panic!("unknown mode {m} (serve|percore)"),
    }
}
