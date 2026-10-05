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

A fully commented reference of every key is in [examples/config.toml](examples/config.toml).

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
