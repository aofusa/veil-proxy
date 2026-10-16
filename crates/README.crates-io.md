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
veil = { package = "veil-proxy", version = "0.8", default-features = false, features = ["full", "api"] }
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

## HTTP/3 on Linux needs one line of Cargo configuration

On Linux, quiche shares rustls' `aws-lc-sys` and needs it built without symbol prefixes. Add this to
`.cargo/config.toml` of your project (use your target triple; the build stops with this message when it is missing):

```toml
[env]
AWS_LC_SYS_NO_PREFIX_x86_64_unknown_linux_gnu = { value = "1", force = true }
# AWS_LC_SYS_NO_PREFIX_aarch64_unknown_linux_gnu = { value = "1", force = true }
```

Builds without the `http3` feature, and other operating systems, do not need it.

## Documentation

- API reference: <https://docs.rs/veil-proxy>
- Guides (proxy, library, mesh, performance): <https://github.com/aofusa/veil-proxy/tree/main/docs/guide>
- Python bindings: the `veil-api` package on PyPI (`pip install veil-api`, `import veil`)

## License

Apache-2.0 OR MIT.
