[English](README.md) | [日本語](docs/readme/README.ja.md)

<p align="center">
  <img src="docs/images/veil_logo.webp" alt="Veil Logo" width="300" align="middle" />
  &nbsp;&nbsp;&nbsp;
  <img src="docs/images/veil_logo_text.svg" alt="Veil" height="50" align="middle" />
</p>

# Veil - High-Performance Reverse Proxy Server

Veil is a reverse proxy and web server written in Rust. Its data plane runs on a custom
io_uring runtime (no tokio/monoio) with rustls and optional kernel TLS, and it speaks
HTTP/1.1, HTTP/2 (TLS and h2c), HTTP/3 (QUIC), gRPC, WebSocket and raw TCP/UDP (L4).

## Highlights

- **Fast data plane** — io_uring on Linux (epoll/kqueue/WSAPoll elsewhere), zero-copy
  `splice(2)`/`sendfile(2)`, kTLS offload, per-thread workers with `SO_REUSEPORT`.
- **Protocols** — HTTP/1.1, HTTP/2, h2c, HTTP/3, gRPC / gRPC-Web, WebSocket, L4 TCP/UDP proxy,
  Unix domain socket listeners and backends.
- **Proxy features** — path/host/header routing, load balancing, health checks, circuit
  breaker, retries, response compression, proxy cache, buffering, header rewriting.
- **Extensible** — Proxy-Wasm filters (Wasmtime) on every protocol.
- **Dynamic** — [VDS](docs/guide/vds.md) pushes routes and upstreams from a control plane (xDS-style long polling,
  ACK/NACK), and built-in [ACME](docs/guide/acme.md) issues and renews certificates (HTTP-01, Let's Encrypt).
- **Embeddable** — use veil as a [library](docs/guide/library.md) to write API servers in Rust (macros, OpenAPI,
  tower, GraphQL, L4 handlers), C (`libveil`) or Python (FastAPI-style) on the same io_uring data plane.
- **Operable** — hot reload (`SIGHUP`) of config and certificates, Prometheus metrics,
  OpenTelemetry, structured access logs, admin API.
- **Secure by default** — memory-safe Rust, seccomp/Landlock (Linux), capsicum (FreeBSD),
  pledge/unveil (OpenBSD), Seatbelt (macOS), privilege dropping.
- **Cross-platform** — Linux, FreeBSD, OpenBSD, NetBSD, macOS and Windows (x86_64 / aarch64).

Performance against nginx is published in [docs/perf/README.md](docs/perf/README.md).

## Install

Prebuilt binaries and packages (`.deb`, `.rpm`, tarballs for Linux/BSD/macOS/Windows) are
attached to each [GitHub release](https://github.com/aofusa/veil-proxy/releases).

To build from source (Rust stable, `cmake` and `nasm`):

```bash
cargo build --release                    # default: kTLS + HTTP/2 + mimalloc
cargo build --release --features full    # everything (HTTP/3, gRPC, WASM, cache, metrics, ...)
```

The binary is `target/release/veil`. See [docs/guide/building.md](docs/guide/building.md)
for feature flags, packaging and BSD/macOS/Windows builds.

## Quickstart

1. Create a certificate (self-signed for testing):

   ```bash
   openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:secp384r1 -nodes \
     -keyout key.pem -out cert.pem -days 365 -subj "/CN=localhost"
   ```

2. Write `config.toml` — serve `/static/` from disk and proxy everything else:

   ```toml
   [server]
   listen = "0.0.0.0:443"
   http2_enabled = true

   [tls]
   cert_path = "/etc/veil/ssl/cert.pem"
   key_path  = "/etc/veil/ssl/key.pem"

   # Static files under /static/
   [[route]]
   [route.conditions]
   path = "/static/*"
   [route.action]
   type = "File"
   path = "/var/www/"

   # Everything else goes to the application
   [[route]]
   [route.conditions]
   path = "/*"
   [route.action]
   type = "Proxy"
   url = "http://127.0.0.1:8080"
   ```

3. Validate and run:

   ```bash
   veil -t -c config.toml   # check the configuration
   veil -c config.toml      # start (SIGHUP reloads, SIGTERM shuts down gracefully)
   curl -k https://localhost/static/index.html
   ```

   Or run the container image (`latest` = musl; `glibc` is also published, for `linux/amd64` and `linux/arm64`):

   ```bash
   docker run --rm -p 443:443 -p 443:443/udp \
     -v "$PWD/config.toml:/etc/veil/conf.d/config.toml:ro" \
     -v "$PWD/cert.pem:/etc/veil/ssl/cert.pem:ro" -v "$PWD/key.pem:/etc/veil/ssl/key.pem:ro" \
     ghcr.io/aofusa/veil:latest
   ```

   The image is the `full-container` build (epoll readiness reactor), so it runs under the container's default
   seccomp profile (Docker's default and Kubernetes' `RuntimeDefault` forbid io_uring).

A fully commented reference of every key is in [examples/config.toml](examples/config.toml).

### Use veil as a library

```toml
# Cargo.toml (the crate is `veil-proxy` on crates.io; keep the name `veil`, the macros refer to it)
[dependencies]
veil = { package = "veil-proxy", version = "0.8", default-features = false, features = ["full", "api"] }
serde_json = "1"
```

With HTTP/3 on Linux, also add `AWS_LC_SYS_NO_PREFIX_<target> = "1"` to `[env]` in `.cargo/config.toml`
([library guide](docs/guide/library.md#cargotoml)). Python: `pip install veil-api` (`import veil`).

```rust
use veil::prelude::*;

#[veil::route(path = "/hello/{name}", method = "GET")]
async fn hello(name: String) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "hello": name }))
}

#[veil::main]
async fn main() -> Result<(), veil::api::Error> {
    veil::serve(Config::load()?).await   // plaintext HTTP/1.1 + h2c on 127.0.0.1:8080, OpenAPI at /docs
}
```

See the [library guide](docs/guide/library.md) (Rust, C API, Python, building artifacts).

## Documentation

| Topic | Guide |
|-------|-------|
| Feature overview | [docs/guide/features.md](docs/guide/features.md) |
| Platforms & runtime backends | [docs/guide/platforms.md](docs/guide/platforms.md) |
| Building & packaging | [docs/guide/building.md](docs/guide/building.md) |
| Running, validation, reload & shutdown | [docs/guide/running.md](docs/guide/running.md) |
| Configuration reference | [docs/guide/configuration.md](docs/guide/configuration.md) |
| TLS, kTLS & certificate reload | [docs/guide/tls.md](docs/guide/tls.md) |
| Routing, redirects & headers | [docs/guide/routing.md](docs/guide/routing.md) |
| Load balancing, health checks & resilience | [docs/guide/load-balancing.md](docs/guide/load-balancing.md) |
| L4 stream proxy | [docs/guide/l4-proxy.md](docs/guide/l4-proxy.md) |
| HTTP/2, HTTP/3 & WebSocket | [docs/guide/protocols.md](docs/guide/protocols.md) |
| Compression, cache & buffering | [docs/guide/compression-cache.md](docs/guide/compression-cache.md) |
| WASM extensions | [docs/guide/wasm.md](docs/guide/wasm.md) |
| API server library (Rust, C, Python) | [docs/guide/library.md](docs/guide/library.md) |
| VDS: dynamic routes from a control plane | [docs/guide/vds.md](docs/guide/vds.md) |
| ACME: automatic certificates | [docs/guide/acme.md](docs/guide/acme.md) |
| Metrics, tracing & logging | [docs/guide/observability.md](docs/guide/observability.md) |
| Admin & cache purge APIs | [docs/guide/admin-api.md](docs/guide/admin-api.md) |
| Security & sandboxing | [docs/guide/security.md](docs/guide/security.md) |
| Performance tuning & benchmarks | [docs/guide/performance.md](docs/guide/performance.md) |
| Testing (developers) | [docs/guide/testing.md](docs/guide/testing.md) |
| References & logos | [docs/guide/references.md](docs/guide/references.md) |

Contributors: start with [AGENTS.md](AGENTS.md) (design philosophy, constraints and workflow).

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.

(c) 2025 aofusa
