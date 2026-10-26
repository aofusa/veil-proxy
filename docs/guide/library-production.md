# Running a veil API application in production

[← Documentation index](README.md) · [日本語](ja/library-production.md) · [API server library](library.md)

The library's defaults are chosen for development: the server listens on `127.0.0.1:8080` only, every request is
written to the access log on standard output, and OpenAPI with Swagger UI is served at `/openapi.json` and `/docs`.
This page lists what to change for production, for Rust and Python (the C API reads the same keys). Everything here
is a configuration key or a documented API; nothing turns off validation or error handling.

A complete example is [examples/api/production/](../../examples/api/production/): one `config.toml` loaded by a
Rust program (`src/main.rs`, `Cargo.toml`) and by a Python program (`app.py`). `tests/api/library_e2e.sh production` starts both and
checks the settings below.

## The recommended configuration

<!-- not-a-full-config -->
```toml
# Recommended production settings for a veil API application (docs/guide/library-production.md).
# The Rust (src/main.rs) and Python (app.py) examples both load this file. Keys left out keep their defaults.

[server]
listen_cleartext = "0.0.0.0:8080"    # reachable from other hosts and containers (the library default is 127.0.0.1)
enable_secure_redirect = false       # serve plaintext here; TLS is terminated in front, or add a TLS listener
# threads = 0                        # default: one worker per CPU the process may use (affinity and cgroup quota)
graceful_shutdown_timeout_secs = 30  # SIGTERM: stop accepting, finish requests in flight for up to 30 s

[logging]
level = "info"
format = "json"                      # one JSON object per line for a log collector

[access_log]
enabled = false                      # the library default writes one line per request to stdout

[api]
openapi_enabled = false              # no /openapi.json and /docs on the public listener
compression = true                   # br / gzip for JSON and text responses of 256 bytes or more
max_body_size = 1048576              # 1 MiB request bodies
websocket_ping_interval_secs = 30    # keep idle WebSockets alive through proxies and NAT
websocket_idle_timeout_secs = 120    # close WebSockets that sent nothing for 2 minutes

[prometheus]
enabled = true
path = "/metrics"
allowed_ips = ["127.0.0.1", "::1", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"]

[tls_client]
ca = "system"                        # outgoing TLS (veil::api::client, gRPC channels) trusts the OS store
```

Load it with `Config::from_file("config.toml")` (Rust; `Config::load()` reads `config.toml` in the current
directory), `veil.run(config_path="config.toml")` (Python) or `veil_config_from_file` (C). The same keys can be set
in code (`ConfigBuilder::set`, `veil.run(overrides={...})`) or per deployment with
`VEIL_OVERRIDE_<n>='key = value'`, which wins over the file. The sections below explain each choice.

## Listening addresses

- Inside a container or behind a load balancer, listen on `0.0.0.0` (or the address of one interface). The default
  `127.0.0.1:8080` is not reachable from the container's bridge network.
- Several ports with different protocols go in `[[listener]]` (Rust `Listener`, Python `veil.Listener`); see
  [Several listeners](library.md#several-listeners). With any `[[listener]]` the default `127.0.0.1:8080` is not
  opened.
- To terminate TLS in veil, add a TLS listener with `[tls] cert_path` / `key_path`. HTTP/3 is one more key:

<!-- not-a-full-config -->
```toml
[tls]
cert_path = "/etc/veil/tls/cert.pem"
key_path = "/etc/veil/tls/key.pem"
auto_reload = true                   # pick up renewed certificates without a restart (SIGHUP works too)

[[listener]]
name = "https"
listen = "0.0.0.0:443"
tls = true
http3 = true                         # also HTTP/3 on UDP 443 (the http3 feature); QUIC defaults are fine

[[listener]]
name = "redirect"
listen = "0.0.0.0:80"
tls = false
secure_redirect = true               # 301 to the https listener
```

  Renewed certificates are used for new handshakes; open connections keep the old one. With ACME
  ([acme.md](acme.md)) veil renews the certificate itself.

## Workers and threads

- Leave `[server] threads` at its default: one network worker per CPU that the process may run on. The count
  follows the CPU affinity (`taskset`, `docker --cpuset-cpus`) and the cgroup CPU quota (`docker --cpus`,
  Kubernetes CPU limits), so a container limited to 2 CPUs starts 2 workers. Set it only to leave CPUs to other
  processes on the same host.
- `[api] handler_threads` sizes the pool that runs blocking Rust functions (`route_blocking`), C callbacks and
  Python handlers. The default is twice the CPU count (at least 4). Raise it when blocking handlers wait on I/O
  (a synchronous database driver), not for CPU-bound work.

## Logging

- The access log is on by default in library mode and writes one line per request to standard output. Each line is
  formatted and written by the worker and flushed by a background thread. At high request rates turn it off
  (`[access_log] enabled = false`), or keep it and send it somewhere cheaper than a terminal: `format = "json"` and
  `file_path = "/var/log/app/access.log"`, or standard output collected by the container runtime. The fields are
  listed in [observability.md](observability.md).
  In a measurement of the Rust example (release build, 2 workers, `GET /api/items/7`, 64 connections) the
  access log cost 15% of the throughput (44,543 against 52,277 requests/s). For Python handlers the difference was
  within noise, because the handler itself takes most of the time.
- `[logging] format = "json"` makes the application log machine-readable. Keep `level = "info"`: veil logs nothing
  per request at `info`.

## OpenAPI and Swagger UI

`/openapi.json` and `/docs` describe every route and its parameters. On a public API turn them off
(`[api] openapi_enabled = false`, Python `veil.run(openapi=False)`), and turn them on where they are useful, for
example in staging with `VEIL_OVERRIDE_1='api.openapi_enabled = true'`. The Swagger UI page loads its scripts from
unpkg.com in the browser.

## Compression

`[api] compression = true` compresses handler responses with br or gzip (the client's `Accept-Encoding` decides)
for `application/json`, `text/*`, JavaScript and SVG of 256 bytes or more; see
[Response compression](library.md#response-compression). The default levels (brotli 4, gzip 4) suit API responses;
a route that sends large, rarely changing documents can raise them in its own `[route.compression]`
(`brotli_level`, `gzip_level`). Responses that are already compressed (images, archives) are skipped by type.
Compression needs the `compression` feature (in `full`; the Python wheel has it).

## Static files

`App::serve_dir` (Rust) and `veil_kit.staticfiles.StaticFiles` / `app.serve_dir` (Python) serve a directory with
the proxy's file backend (sendfile, Range, `ETag`). Precompressed `.br` / `.gz` siblings are not used; the route's
`[route.compression]` compresses files on the fly for HTTP/2 and HTTP/3 and caches the result (HTTP/1.1 sends files
as they are).

`serve_dir` turns on the file metadata cache (`open_file_cache`) and the file contents cache for HTTP/2 and
HTTP/3 (`static_file_cache`) for its route (`ServeOptions { open_file_cache, static_file_cache, .. }` in Rust,
`cache=False` in Python turn them off). A hand-written `type = "File"` route enables them itself, always both
together:

<!-- not-a-full-config -->
```toml
[[route]]
[route.conditions]
path = "/assets/*"
[route.action]
type = "File"
path = "/srv/app/assets"
[route.open_file_cache]
enabled = true
valid_duration_secs = 60
watch = true                         # follow changes on disk right away (see below)
[route.static_file_cache]
enabled = true
watch = true
```

By default the cached metadata and contents are trusted for `valid_duration_secs`, so a file replaced while the
server runs is seen only after that. `watch = true` (veil 0.10) makes the caches, and the cache of compressed
results, follow changes on disk right away: a dedicated thread watches the route's directory (inotify on Linux,
kqueue on the BSDs and macOS, `ReadDirectoryChangesW` on Windows), and a cache hit only reads one atomic
variable. Set it to the same value for both caches; if only one follows, the other keeps the old content. For
`serve_dir` routes, which do not set `watch`, turn it on for every route with the defaults:

<!-- not-a-full-config -->
```toml
[performance]
open_file_cache_watch = true
[static_file_cache]
watch = true
```

When the watch limit is reached (`fs.inotify.max_user_watches` on Linux, a quarter of `RLIMIT_NOFILE` for kqueue)
veil logs a warning and falls back to `valid_duration_secs`. Without `watch`, keep `valid_duration_secs` short when
files change while the server runs, or deploy new files under new names.

## Python

- Plain `def` handlers run on the handler pool. A handler that only computes something small and never blocks can
  run directly on the network worker with `offload=False` (the health check in the example).
- Handlers that wait for I/O (HTTP calls, databases with async drivers, `asyncio.sleep`) should be `async def`:
  they run on the persistent event loop and do not hold a thread while they wait. Do not call blocking functions in
  them; use `await veil.run_in_threadpool(f, ...)` or a plain `def` handler.
- CPU-heavy work is limited by the GIL whatever the server does. `run_in_threadpool` keeps it off the event loop.
  One process with several worker threads is the supported layout; running several processes on the same port is
  not recommended (each process would have its own pools, loops and metrics). If one process is not enough, run
  several on different ports (or containers) behind a load balancer.
- `[api] python_event_loops` (default 1) is the number of persistent event loops. One loop is enough unless the
  `async def` handlers themselves are the bottleneck; more loops let more coroutines run, still under one GIL.
  `python_event_loop = "auto"` uses uvloop when it is installed (`pip install uvloop`), which is faster than the
  asyncio loop. `python_event_loop_queue` (65536) bounds the calls waiting for a loop; beyond it requests get 503.
- With WebSockets, `websocket_ping_interval_secs` (default 30) and `websocket_idle_timeout_secs` (default 120) close
  dead connections; lower them if proxies or NAT in front drop idle connections sooner.

## File descriptors

Every open connection (and WebSocket) uses one file descriptor. veil raises its soft limit to the hard limit at
startup, but it cannot raise the hard limit itself. Set it where the process starts:

- systemd: `LimitNOFILE=1048576` in the unit.
- Docker: `docker run --ulimit nofile=1048576:1048576 ...` (Compose: `ulimits: nofile: {soft: 1048576, hard: 1048576}`).
- Kubernetes: the container runtime's default (often 1048576) applies; check it with `ulimit -Hn` in the container.

## Shutdown

On SIGTERM (or SIGINT, or `veil::shutdown()` / `veil.shutdown()`) veil stops accepting connections, closes
keep-alive connections that are idle, sends GOAWAY on HTTP/2 and HTTP/3, closes WebSockets with code 1001 and waits
up to `[server] graceful_shutdown_timeout_secs` (30) for requests in flight. Connections still open after that are
dropped. Upgraded connections see `shutdown_signal()` resolve when shutdown starts. In Kubernetes, keep
`terminationGracePeriodSeconds` above this timeout. Python `lifespan` / `on_shutdown` functions run after the server
stopped. Windows has no SIGTERM; use `shutdown()` there.

## TLS for outgoing connections

`veil::api::client`, the gRPC channels (Rust and `veil_kit.grpc`), health checks and VDS verify servers with the
bundled public CA list by default. `[tls_client] ca = "system"` uses the operating system's trust store instead
(read once at startup), and a PEM path adds an internal CA: `ca = ["system", "/etc/app/internal-ca.pem"]`.

## Memory allocator

In library mode veil does not choose the global allocator; the application does. For a Rust server, declare veil's
allocator (or mimalloc) in the binary, as the example does:

```rust
#[cfg(unix)]
#[global_allocator]
static GLOBAL: veil::veil_alloc::VeilAlloc = veil::veil_alloc::VeilAlloc;
```

Without a declaration the system allocator is used. The Python wheel and the C library never replace the host
process's allocator. See [Memory allocator](library.md#memory-allocator).

## Health checks and metrics

- Write the health check as an ordinary route that does no I/O (`GET /healthz` returning `ok`), and a separate
  readiness route if the application must check its dependencies. Point the load balancer or the Kubernetes
  probes at them.
- `[prometheus] enabled = true` serves the metrics at `path` (default `/__metrics`) on every listener, for the
  addresses in `allowed_ips` only (other clients get 403). The request counters, latency histograms and connection
  gauges are described in [observability.md](observability.md).

## Security sandbox

seccomp and Landlock are off by default in library mode because veil cannot know what the handlers do. They can be
turned on (`[security] enable_seccomp = true`, `enable_landlock = true` with `landlock_read_paths` /
`landlock_write_paths`) once the application's files, sockets and DNS use are known; see [security.md](security.md).
