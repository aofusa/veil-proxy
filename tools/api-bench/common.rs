//! axum の計測用アプリと起動方法（axum-tokio・axum-veil で共有。`#[path]` で取り込む）。
//!
//! 同じソースを、本物の tokio（axum-tokio）と veil の tokio 互換レイヤー（axum-veil。依存ツリーの tokio を
//! crates/veil-tokio/tokio-shim に置き換える）でビルドして比べる。

use axum::extract::Path;
use axum::routing::get;
use axum::{Json, Router};

pub fn router() -> Router {
    Router::new()
        .route("/plaintext", get(|| async { "Hello, World!" }))
        .route(
            "/json/{name}",
            get(|Path(name): Path<String>| async move {
                Json(serde_json::json!({ "message": format!("Hello, {name}") }))
            }),
        )
}

/// `serve`: マルチスレッドのランタイム（ワーカー `threads` 本）の上で `axum::serve` を 1 つ動かす。
pub fn run_serve(addr: &str, threads: usize) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
        axum::serve(listener, router()).await.expect("serve");
    });
}

/// `percore`: スレッドごとにカレントスレッドのランタイムと SO_REUSEPORT のリスナーを持つ（スレッド・パー・コア）。
pub fn run_percore(addr: &str, threads: usize) {
    let addr: std::net::SocketAddr = addr.parse().expect("addr");
    let hs: Vec<_> = (0..threads)
        .map(|_| {
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                rt.block_on(async move {
                    let sock = tokio::net::TcpSocket::new_v4().expect("socket");
                    sock.set_reuseaddr(true).expect("reuseaddr");
                    sock.set_reuseport(true).expect("reuseport");
                    sock.bind(addr).expect("bind");
                    let listener = sock.listen(1024).expect("listen");
                    axum::serve(listener, router()).await.expect("serve");
                });
            })
        })
        .collect();
    for h in hs {
        let _ = h.join();
    }
}

pub struct Args {
    pub mode: String,
    pub addr: String,
    pub threads: usize,
}

/// `<bin> <mode> [addr] [threads]`（既定: 127.0.0.1:3000、2 スレッド）。
pub fn args() -> Args {
    let a: Vec<String> = std::env::args().collect();
    Args {
        mode: a.get(1).cloned().unwrap_or_else(|| "serve".into()),
        addr: a.get(2).cloned().unwrap_or_else(|| "127.0.0.1:3000".into()),
        threads: a.get(3).and_then(|s| s.parse().ok()).unwrap_or(2),
    }
}
