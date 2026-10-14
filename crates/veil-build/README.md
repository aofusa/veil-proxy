# veil-build

Code generator for gRPC services on [veil](../../README.md)'s API library (`veil::api::grpc`, feature `grpc-handler`).
Use it from `build.rs`; the generated code depends on `veil` and `prost` only (no tokio, no tonic).

```toml
[dependencies]
veil = { git = "https://github.com/aofusa/veil-proxy", default-features = false, features = ["full", "api"] }
prost = "0.13"            # the version prost-build 0.13 generates code for

[build-dependencies]
veil-build = { git = "https://github.com/aofusa/veil-proxy" }
```

```rust
// build.rs
fn main() -> std::io::Result<()> {
    veil_build::compile_protos(&["proto/helloworld.proto"], &["proto"])
}
```

```rust
pub mod hello { veil::include_proto!("helloworld"); }
use hello::greeter_server::{add_greeter, Greeter};
```

For each service the output has a module `<service>_server` with:

| Item | Content |
|------|---------|
| `trait <Service>` | one `async fn` per RPC (`Request<T>` / `Request<Streaming<T>>` in, `Response<T>` / `Response<BoxStream<T>>` out) |
| `fn add_<service>(app, impl) -> App` | registers every method with `ProstCodec` |
| `ProstCodec` | `veil::api::grpc::Codec<T>` for every prost message type |
| `SERVICE_NAME`, `METHODS` | `<package>.<Service>` and the `/<package>.<Service>/<Method>` paths |

veil itself does not depend on prost: the codec lives in the generated code (F-208). Options:
`veil_build::configure().veil_path("::my_veil")` changes the path of the `veil` crate, and `config_mut()` exposes the
`prost_build::Config`.

See the [gRPC section of the library guide](../../docs/guide/library.md#grpc) and
[`examples/api/rust-grpc`](../../examples/api/rust-grpc/).
