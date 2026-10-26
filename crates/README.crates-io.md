# veil-proxy

A high-performance reverse proxy and API server library for Linux (io_uring), FreeBSD, OpenBSD, NetBSD, macOS and
Windows. The data plane runs on its own runtime (io_uring or epoll/kqueue/WSAPoll, no tokio) with rustls, kTLS,
HTTP/1.1, HTTP/2, HTTP/3 (quiche), gRPC and Proxy-Wasm.

This crate is both:

- the `veil` binary: `cargo install veil-proxy --features full` (configuration: a TOML file, see the
  [configuration reference](https://github.com/aofusa/veil-proxy/blob/main/examples/config.toml)), and
- a library for writing API servers in Rust (`veil::api`, routing macros, OpenAPI, gRPC, tower and a tokio-free
  tokio compatibility layer).

The library name is `veil`. Depend on it under that name so that the macros' generated code (`::veil::...`)
resolves:

```toml
[dependencies]
veil = { package = "veil-proxy", version = "0.10", default-features = false, features = ["full", "api"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

```rust
use veil::prelude::*;

#[veil::route(path = "/users/{id}", method = "GET")]
async fn get_user(id: u64) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "id": id }))
}

#[veil::main]
async fn main() -> Result<(), veil::api::Error> {
    veil::serve(Config::load()?).await
}
```

## Build requirements

The `Cargo.toml` entry is all you need; there is nothing to add to `.cargo/config.toml`, also for HTTP/3 on Linux.
quiche and rustls share one `aws-lc-sys`, whose build needs a C compiler, CMake and libclang (bindgen).

## Documentation

- API reference: <https://docs.rs/veil-proxy>
- Guides (proxy, library, mesh, performance): <https://github.com/aofusa/veil-proxy/tree/main/docs/guide>
- Python bindings: the `veil-api` package on PyPI (`pip install veil-api`, `import veil`)

## License

Apache-2.0 OR MIT.
