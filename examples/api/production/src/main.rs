//! A veil API application with the recommended production settings (`config.toml`).
use veil::prelude::*;

// veil の既定はアロケータを決めない（handler では #[global_allocator] を定義しない）。本番ではアプリが選ぶ
#[cfg(unix)]
#[global_allocator]
static GLOBAL: veil::veil_alloc::VeilAlloc = veil::veil_alloc::VeilAlloc;

/// Liveness / readiness probe: no I/O, so it answers even when the dependencies are slow.
#[veil::route(path = "/healthz", method = "GET")]
async fn healthz() -> &'static str {
    "ok"
}

#[veil::route(path = "/api/items/{id}", method = "GET")]
async fn item(id: u64) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "id": id, "name": format!("item {id}") }))
}

/// A list large enough to be compressed (`[api] compression`, from 256 bytes).
#[veil::route(path = "/api/items", method = "GET")]
async fn items() -> Json<serde_json::Value> {
    let list: Vec<_> = (0..50)
        .map(|i| serde_json::json!({ "id": i, "name": format!("item {i}") }))
        .collect();
    Json(serde_json::json!({ "items": list }))
}

/// Bodies above `[api] max_body_size` (1 MiB here) are answered with 413 before the handler runs.
#[veil::route(path = "/api/items", method = "POST", status = 201)]
async fn create(req: &Request) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "received": req.body().len() }))
}

#[veil::main]
async fn main() -> Result<(), veil::api::Error> {
    let path = std::env::var("APP_CONFIG").unwrap_or_else(|_| "config.toml".to_string());
    veil::serve(Config::from_file(path)?).await
}
