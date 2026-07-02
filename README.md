[English](README.md) | [日本語](README.ja.md)

<p align="center">
  <img src="docs/images/veil_logo.webp" alt="Veil Logo" width="300" align="middle" />
  &nbsp;&nbsp;&nbsp;
  <img src="docs/images/veil_logo_text.svg" alt="Veil" height="50" align="middle" />
</p>

# Veil - High-Performance Reverse Proxy Server

A high-performance reverse proxy server using io_uring (custom runtime) and rustls.

## Documentation

- [AGENTS.md](AGENTS.md) — Contributor and AI agent quick reference (workflow, design philosophy, constraints, pointers).

## Features

### Core Features
- **Asynchronous I/O**: Efficient I/O processing with custom io_uring runtime (no monoio/tokio dependency in data plane)
- **TLS**: Memory-safe pure Rust TLS implementation with rustls
- **kTLS**: Kernel TLS offload support via rustls + custom kTLS module (Linux 5.15+)
- **HTTP/2**: HTTP/2 support via TLS ALPN negotiation (stream multiplexing, HPACK compression)
- **H2C Server**: HTTP/2 Cleartext (H2C) server support without TLS (Prior Knowledge mode, RFC 7540 Section 3.4)
- **HTTP/3**: QUIC/UDP-based HTTP/3 support using quiche (0-RTT connection establishment)
- **Fast Allocator**: High-speed memory allocation with mimalloc + Huge Pages support
- **Fast Routing**: O(log n) path matching with Radix Tree (matchit)

### Proxy Features
- **Connection Pool**: Latency reduction through backend connection reuse (HTTP/HTTPS support)
- **Load Balancing**: Request distribution to multiple backends (Round Robin/Least Connections/IP Hash/Weighted/Consistent Hash)
- **Health Check**: Automatic failover with HTTP/TCP/gRPC active health checks (HTTP with status code validation, TCP connect-only, gRPC Health Checking Protocol)
- **L4 Stream Proxy**: TCP-level load balancing with Round Robin/LeastConn, TLS passthrough, zero-copy `splice(2)` kernel forwarding (no userspace buffer), connection limiting (requires `l4-proxy` feature)
- **Circuit Breaker**: Per-server circuit breaker (Closed→Open→HalfOpen), outlier detection/ejection, EWMA latency tracking (requires `metrics` feature; request retry is not implemented)
- **Proxy Cache**: Memory and disk-based response caching (ETag/304, stale-while-revalidate, stale-if-error)
- **Cache Purge Admin API**: Cache invalidation via HTTP (`PURGE` method or `POST /__admin/cache/purge`) with exact/prefix/glob/all modes and Bearer token auth
- **Buffering Control**: Response buffering to prevent slow clients from blocking backends (Streaming/Full/Adaptive modes)
- **WebSocket Support**: Bidirectional proxy with Upgrade header detection (Fixed/Adaptive polling modes)
- **H2C Client**: HTTP/2 backend connection without TLS (gRPC support)
- **H2C Server**: HTTP/2 Cleartext server support (Prior Knowledge mode, for internal networks)
- **Header Manipulation**: Add/remove request/response headers (X-Real-IP, HSTS, etc.)
- **Redirect**: 301/302/307/308 HTTP redirects (with path preservation option)
- **SNI Configuration**: Specify SNI name when connecting to HTTPS backends via IP (virtual host support)

### HTTP Processing
- **Keep-Alive**: Full HTTP/1.1 Keep-Alive support
- **Chunked Transfer**: RFC 7230 compliant chunked decoder (state machine based)
- **Via Header**: RFC 7230 Section 5.7.1 compliant Via header insertion for proxy chain tracking
- **100 Continue**: RFC 7231 Section 5.1.1 compliant Expect: 100-continue handling
- **Host Validation**: RFC 7230 Section 5.4 compliant mandatory Host header check for HTTP/1.1
- **Hop-by-hop Headers**: RFC 7230 Section 6.1 compliant header stripping (Connection, Keep-Alive, TE, etc.)
- **Range Requests**: RFC 7233 compliant Range header parsing and 206 Partial Content support
- **TE Header**: RFC 7230 Section 4.3 compliant TE header parsing (trailers support)
- **Buffer Pool**: Thread-local buffer pool with configurable sizes (reduces memory allocation overhead)
- **Response Compression**: Dynamic Gzip/Brotli/Zstd compression with Accept-Encoding negotiation

### Performance
- **CPU Affinity**: Pin worker threads to CPU cores
- **CBPF Distribution**: Client IP-based load balancing with SO_REUSEPORT (Linux 4.6+)
- **OpenFileCache**: File metadata cache to reduce system calls (canonicalize, metadata, mime_guess) - 60-67% reduction in system calls for static file serving. On cache miss, the blocking `canonicalize`/`metadata`/disk reads run on a dedicated offload thread pool (completion signaled via `eventfd` + `POLL_ADD`) so the io_uring event loop never blocks
- **HTTP/2 Response Streaming**: For non-compressed responses, the backend body is forwarded to the HTTP/2 client as DATA frames incrementally instead of being fully buffered — both `Content-Length` and `Transfer-Encoding: chunked` responses. Chunked bodies are decoded zero-copy via a span-based decoder (sub-slices of the read buffer, no intermediate `Vec`). Each DATA frame obeys HTTP/2 flow control (connection/stream window + `WINDOW_UPDATE`), so backpressure follows the client's receive rate and RSS does not scale with payload size (OOM resistance for large downloads)
- **HTTP/2 Request Streaming (uploads)**: For proxied uploads, the backend connection is opened as soon as the request **HEADERS** arrive (before the body finishes), and each incoming `DATA` frame is forwarded to the backend as a `Transfer-Encoding: chunked` chunk **without buffering the whole body**. The frame's owned buffer is sent zero-copy (only the chunk-size line and CRLF are small allocations); flow-control accounting (`WINDOW_UPDATE`) runs without copying into `request_body`. The proxy does not read the next frame until the backend write completes, so client→proxy→backend backpressure propagates naturally and process-heap retention is at most one frame (RSS independent of upload size). Eligible for `Proxy` backends over HTTP/1.1 (non-h2c), `buffering` mode ≠ `full`, no WASM body filter, non-gRPC; the body-size limit (`max_request_body_size`) is enforced mid-transfer (413 + `RST_STREAM`). Non-eligible requests fall back to the buffered path with no behavior change
- **HTTP/3 Streaming (both directions)**: Because quiche's `Connection`/`h3::Connection` are not `Send` and must be driven by the single-threaded QUIC event loop, HTTP/3 streaming uses an **actor model** (`src/http3_stream.rs`): the main loop owns all QUIC/H3 I/O (`send_response`/`send_body`/`recv_body`) while a **backend task** (spawned on the same-thread io_uring executor) does backend TCP I/O. They communicate over a **single-threaded SPSC async channel** (`Rc<RefCell>`, **no atomics/locks**) plus a wakeup `Notify`, and the main loop multiplexes packet receive, task wakeups, and timeouts via `select_biased!`. Response bodies (`Content-Length`/chunked/EOF framing; chunked decoded zero-copy via the span decoder) and request bodies (forwarded as `Transfer-Encoding: chunked`) are carried as `bytes::Bytes` across the actor boundary without deep copies. Bounded channels give **bidirectional backpressure** (slow client → response channel full → backend reads pause; slow backend → request channel full → `recv_body` pauses → QUIC flow control throttles the client), so RSS stays bounded and does not scale with payload size. Request framing is finalized only **after the first body chunk actually arrives** (an h3 GET sends HEADERS and the fin separately, so `more_frames` alone does not imply a body). Compression-eligible responses buffer-then-compress; TLS backends fall back to the buffered path. Eligible for `Proxy` backends over plaintext HTTP/1.1, `buffering` mode ≠ `full`, no WASM module, non-gRPC, security-allowed

### Operations
- **Graceful Shutdown**: Safe termination via SIGINT/SIGTERM
- **Graceful Reload**: Hot reload configuration via SIGHUP (zero downtime)
- **TLS Certificate Hot Reload**: Zero-downtime certificate rotation via mtime detection + ArcSwap; existing connections use old cert, new handshakes pick up new cert automatically
- **Panic Recovery**: Connection-level panic catching to recovery worker thread (only affected connection terminates)
- **Async Logging**: High-performance async logging with ftlog
- **Config Validation**: Detailed configuration file validation at startup
- **Prometheus Metrics**: Export request counts, latency, active connections, upstream health, circuit breaker state, connection pool stats, gRPC/WASM durations, etc. via metrics endpoint (requires configuration, disabled by default); runtime enable/disable without restart
- **OpenTelemetry (OTLP/HTTP)**: Push Prometheus metrics to any OTLP-compatible collector (Grafana Tempo, Jaeger, etc.) via a dedicated std thread — fully tokio-free (requires `opentelemetry` feature)

### Security
- **HTTP to HTTPS Redirect**: Automatic 301 redirect from HTTP to HTTPS
- **Connection Limit**: Global concurrent connection limit
- **Rate Limiter**: Sliding window rate limiting
- **IP Restriction**: IP address filtering with CIDR support
- **Privilege Dropping**: Drop to unprivileged user after root startup
- **seccomp Filter**: BPF-based system call restriction with argument-level PROT_EXEC validation for mmap/mprotect (optional)
- **io_uring Opcode Restrictions**: `IORING_REGISTER_RESTRICTIONS` applied at ring creation to allow only necessary opcodes (ACCEPT/RECV/SEND/CONNECT/TIMEOUT/SPLICE/POLL_ADD)
- **Landlock Sandbox**: Filesystem access restriction (Linux 5.13+)
- **systemd Sandbox**: Namespace isolation and system call restriction support

## Build

### Dependencies

The following system libraries are required depending on enabled features:

| Dependency | Required for | Notes |
|------------|-------------|-------|
| `cmake` | `http3` feature (quiche/BoringSSL) | Must be installed before building |
| `nasm` | `aws-lc-rs` (TLS, always required) | Assembly optimizations for crypto |

On Debian/Ubuntu:

```bash
apt-get install -y cmake nasm
```

### Local Build

```bash
# Default build — kTLS + HTTP/2 + mimalloc (recommended)
cargo build --release

# Full featured build — all optional features enabled
cargo build --release --features full

# Minimal build — no optional features
cargo build --release --no-default-features
```

The binary is generated at `target/release/veil`.

### Distribution Build (glibc 2.28, Docker)

To produce a binary compatible with older Linux distributions (glibc ≥ 2.28), use
[`messense/cargo-zigbuild`](https://github.com/messense/cargo-zigbuild) which links against a minimum glibc version via the Zig toolchain.

```bash
# Full featured build
docker run --rm -it -v $(pwd):/io -w /io messense/cargo-zigbuild bash -c \
  "apt-get update -y && apt-get install -y cmake nasm && \
   cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.28 --features full"

# Default build (no cmake/nasm required)
docker run --rm -it -v $(pwd):/io -w /io messense/cargo-zigbuild \
  cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.28
```

The binary is generated at `target/x86_64-unknown-linux-gnu/release/veil`.

> **Note**: `cmake` and `nasm` must be installed inside the container when building with `--features full` because the `http3` feature compiles quiche's bundled BoringSSL (requires cmake) and `aws-lc-rs` uses assembly optimizations (requires nasm). The default build without `http3` does not need cmake.

> **Cargo features**: The complete list of available feature flags is defined in the [`[features]` section of `Cargo.toml`](Cargo.toml).
> Key notes:
> - **Default features**: `ktls`, `http2`, `mimalloc`
> - **`full`**: enables everything (`ktls`, `http2`, `http3`, `grpc-full`, `wasm`, `compression`, `cache`, `metrics`, `websocket`, `rate-limit`, `buffering`, `mimalloc`)
> - **Allocator features** (`mimalloc`, `jemalloc`, `system-allocator`) are mutually exclusive — enable at most one
> - HTTP/3 is UDP-based and cannot be combined with kTLS

## Startup

```bash
# Start with default config file (/etc/veil/config.toml)
./veil

# Start with specified config file
./veil -c /path/to/config.toml
./veil --config /path/to/config.toml

# Show help
./veil --help

# Show version
./veil --version
```

### Command Line Options

| Option | Description | Default |
|--------|-------------|---------|
| `-c, --config <PATH>` | Path to config file | `/etc/veil/config.toml` |
| `-t, --test` | Test config file syntax and validity, then exit (nginx -t equivalent) | - |
| `-h, --help` | Show help message | - |
| `-V, --version` | Show version information | - |

### Configuration Validation

Test your configuration file before deploying or reloading:

```bash
# Test default config file
./veil -t

# Test specific config file
./veil -t -c /path/to/config.toml
```

**Validation checks:**
- TOML syntax parsing
- Configuration value validation
- TLS certificate and key file existence

**Output examples:**
```bash
# Success
veil: configuration file config.toml test is successful

# Failure (TLS cert not found)
veil: configuration file config.toml test failed
veil: TLS certificate not found: /path/to/cert.pem
```

**Note**: When reloading configuration via SIGHUP, if the new configuration is invalid, the reload is rejected and the server continues running with the previous valid configuration.

## TLS Certificate Generation

To generate a self-signed certificate for development/testing, run the following commands:

```bash
# Generate ECDSA private key (secp384r1)
openssl genpkey -algorithm EC -out server.key -pkeyopt ec_paramgen_curve:secp384r1 -pkeyopt ec_param_enc:named_curve

# Generate self-signed certificate (valid for 365 days)
openssl req -new -x509 -key server.key -out server.crt -days 365 -subj "/CN=localhost/O=Development/C=JP"
```

Specify the generated files in `config.toml`:

```toml
[tls]
cert_path = "./server.crt"
key_path = "./server.key"
```

> **Note**: In production, use certificates issued by a certificate authority such as Let's Encrypt.

## TLS Library

### rustls (Default)

- Memory-safe pure Rust implementation
- No additional dependencies
- Default when not using kTLS

### rustls + custom kTLS module (`--features ktls`)

- Performs TLS handshake with rustls
- After handshake completion, offloads to kTLS via the custom kernel TLS module (`src/ktls.rs`, `src/ktls_rustls.rs`)
- No additional external dependencies (pure Rust implementation)

```bash
# Build
cargo build --release --features ktls
```

## HTTP to HTTPS Redirect

This feature automatically redirects HTTP access to HTTPS.

### Configuration

```toml
[server]
listen = "0.0.0.0:443"
http = "0.0.0.0:80"  # Enable HTTP redirect
```

### Behavior

- Access to `http://example.com/path` is redirected to `https://example.com/path` with 301
- Domain name is extracted from the Host header to construct the redirect URL
- **Port handling**: The redirect URL uses the port from the `[server].listen` setting
  - If listen port is 443 (default): `https://example.com/path` (port omitted)
  - If listen port is 8443: `https://example.com:8443/path` (port included)

### Security Considerations

- **Redirect Only**: HTTP only performs redirects, no content is served
- **301 Moved Permanently**: Browsers cache the redirect destination, subsequent requests go directly to HTTPS
- **First Access**: Plain text communication occurs only on the first HTTP access, but no content is included

### Notes

- Using privileged port (80) requires one of the following:
  1. Start as root (recommend using with privilege dropping)
  2. Grant `CAP_NET_BIND_SERVICE` capability

```bash
# To grant capability
sudo setcap 'cap_net_bind_service=+ep' ./target/release/veil
```

## Configuration

By default, `/etc/veil/config.toml` is loaded.
Use the `-c` or `--config` option to specify a different path.

### Default Values Reference

The following table lists default values for major configuration options:

| Section | Option | Default Value | Description |
|---------|--------|---------------|-------------|
| `[server]` | `server_header_enabled` | `false` | Enable Server header |
| `[server]` | `server_header_value` | `"veil"` | Server header value |
| `[server]` | `http2_enabled` | `false` | Enable HTTP/2 |
| `[server]` | `http3_enabled` | `false` | Enable HTTP/3 |
| `[logging]` | `level` | `"info"` | Log level |
| `[logging]` | `format` | `"text"` | Log format |
| `[logging]` | `channel_size` | `100000` | Log channel buffer size |
| `[logging]` | `flush_interval_ms` | `1000` | Flush interval (ms) |
| `[prometheus]` | `enabled` | `false` | Enable Prometheus metrics |
| `[prometheus]` | `path` | `"/__metrics"` | Metrics endpoint path |
| `[performance]` | `reuseport_balancing` | `"cbpf"` | SO_REUSEPORT balancing |
| `[performance]` | `huge_pages_enabled` | `false` | Enable Huge Pages |
| `[performance]` | `open_file_cache_enabled` | `false` | Enable OpenFileCache |
| `[performance]` | `open_file_cache_valid_duration_secs` | `60` | Cache validity (seconds) |
| `[performance]` | `open_file_cache_max_entries` | `10000` | Max cache entries |
| `[tls]` | `ktls_enabled` | `false` | Enable kTLS |
| `[tls]` | `ktls_fallback_enabled` | `true` | kTLS fallback to rustls |
| `[tls]` | `tcp_cork_enabled` | `true` | Enable TCP_CORK |
| `[tls]` | `cipher_suites` | `[]` (rustls default) | Allowed TLS cipher suites (like nginx `ssl_ciphers`; listed order = server preference; unknown names fail at startup; see config.toml) |
| `[tls]` | `auto_reload` | `false` | Certificate hot reload (mtime detection + SIGHUP) |
| `[tls]` | `reload_interval_secs` | `60` | Certificate change check interval (seconds) |
| `[buffer_pool]` | `read_buffer_size` | `65536` | Read buffer size (64KB) |
| `[buffer_pool]` | `initial_read_buffers` | `32` | Initial read buffers |
| `[buffer_pool]` | `max_read_buffers` | `128` | Max read buffers |
| `[buffer_pool]` | `request_buffer_size` | `1024` | Request buffer size (1KB) |
| `[buffer_pool]` | `initial_request_buffers` | `16` | Initial request buffers |
| `[buffer_pool]` | `large_request_buffer_size` | `4096` | Large request buffer (4KB) |
| `[http2]` | `header_table_size` | `65536` | HPACK table size (64KB) |
| `[http2]` | `max_concurrent_streams` | `256` | Max concurrent streams |
| `[http2]` | `initial_window_size` | `1048576` | Stream window size (1MB) |
| `[http2]` | `max_frame_size` | `65536` | Max frame size (64KB) |
| `[http2]` | `max_header_list_size` | `65536` | Max header list size (64KB) |
| `[http2]` | `connection_window_size` | `1048576` | Connection window (1MB) |
| `[http2]` | `max_rst_stream_per_second` | `100` | RST_STREAM rate limit |
| `[http2]` | `max_control_frames_per_second` | `500` | Control frame rate limit |
| `[http2]` | `max_continuation_frames` | `10` | Max CONTINUATION frames |
| `[http2]` | `max_header_block_size` | `65536` | Max header block (64KB) |
| `[http2]` | `stream_idle_timeout_secs` | `60` | Stream idle timeout (seconds) |
| `[http3]` | `max_idle_timeout` | `30000` | Max idle timeout (ms, 30s) |
| `[http3]` | `max_udp_payload_size` | `1350` | Max UDP payload size |
| `[http3]` | `initial_max_data` | `10000000` | Initial max data (10MB) |
| `[http3]` | `initial_max_stream_data_bidi_local` | `1000000` | Stream data bidi local (1MB) |
| `[http3]` | `initial_max_stream_data_bidi_remote` | `1000000` | Stream data bidi remote (1MB) |
| `[http3]` | `initial_max_stream_data_uni` | `1000000` | Stream data uni (1MB) |
| `[http3]` | `initial_max_streams_bidi` | `100` | Max bidirectional streams |
| `[http3]` | `initial_max_streams_uni` | `100` | Max unidirectional streams |
| `[http3]` | `compression_enabled` | `false` | Enable compression |
| `[http3]` | `gso_gro_enabled` | `false` | Enable GSO/GRO |

Configuration file example (`config.toml`):

```toml
[server]
listen = "0.0.0.0:443"
# HTTP to HTTPS redirect (optional)
# Automatically redirect HTTP access to HTTPS (301 Moved Permanently)
http = "0.0.0.0:80"
# Number of worker threads (optional)
# If unspecified or 0, uses the same number of threads as CPU cores
threads = 4
# Enable HTTP/2 (only when built with --features http2)
http2_enabled = true
# Enable HTTP/3 (only when built with --features http3)
http3_enabled = true
# Server header configuration (optional)
# Security consideration: Server header reveals server software information
# Recommended to disable in production environments
# server_header_enabled = false
# Custom Server header value (only effective when server_header_enabled = true)
# Default: "veil" (protocol-specific values: "veil/http1.1", "veil/http2", "veil/http3")
# server_header_value = "MyServer/1.0"

[logging]
# Log level: "trace", "debug", "info", "warn", "error", "off"
level = "info"
# Log output format: "text", "json"
# format = "text"
# Log channel size (prevents log drops under high load)
channel_size = 100000
# Flush interval (milliseconds)
flush_interval_ms = 1000
# Maximum log file size (bytes, 0=no rotation)
# Log file path (optional, defaults to stderr)
# file_path = "/var/log/veil.log"

[security]
# Privilege dropping settings (Linux only)
drop_privileges_user = "nobody"
drop_privileges_group = "nogroup"
# Global concurrent connection limit (0 = unlimited)
max_concurrent_connections = 10000

# seccomp system call restriction (Linux only)
# Recommended to verify with log mode first, then switch to filter mode
enable_seccomp = true
seccomp_mode = "filter"  # "disabled" / "log" / "filter" / "strict"

# Landlock filesystem restriction (Linux 5.13+)
enable_landlock = true
landlock_read_paths = ["/etc/veil", "/usr", "/lib", "/lib64"]
landlock_write_paths = ["/var/log/veil"]

[performance]
# SO_REUSEPORT distribution method
# "kernel" = kernel default (3-tuple hash)
# "cbpf"   = client IP-based CBPF (improved cache efficiency, requires Linux 4.6+)
reuseport_balancing = "cbpf"

# Use Huge Pages (Large OS Pages)
# 5-10% performance improvement by reducing TLB misses
huge_pages_enabled = true

# OpenFileCache (File Metadata Cache)
# Caches file metadata (canonicalize, metadata, mime_guess) to reduce system calls
# Performance improvement: 60-67% reduction in system calls (cache hit)
# 
# Effects:
#   - Caches canonicalize, metadata, mime_guess system calls
#   - Reduces 5-6 system calls per request to 2 (cache hit)
# 
# Notes:
#   - File change detection may be delayed up to 60 seconds (default)
#   - Symbolic link changes may be delayed
#   - Optimal for static file serving (not suitable for dynamically changing files)
#
# Route-specific configuration:
#   - Each route ([path_routes] or [host_routes]) can specify `open_file_cache` section
#   - If route configuration is not specified, this global setting is used
#
# Default: false (disabled)
#open_file_cache_enabled = false

# OpenFileCache validity duration (seconds, global default)
# Duration for which cached file information is considered valid
# Default: 60 seconds
#open_file_cache_valid_duration_secs = 60

# OpenFileCache maximum entries (global default)
# Maximum number of file information entries to keep in cache
# Default: 10000
#open_file_cache_max_entries = 10000

[tls]
cert_path = "/path/to/cert.pem"
key_path = "/path/to/key.pem"
ktls_enabled = true         # Enable kTLS (Linux 5.15+, requires feature flag)
ktls_fallback_enabled = true # Fallback to rustls on kTLS failure (default: true)
tcp_cork_enabled = true     # Use TCP_CORK during kTLS setup (default: true)

# Unified routing (AWS ALB-compliant)
# Routes are evaluated in array order (first-match)

[[route]]
[route.conditions]
host = "example.com"
[route.action]
type = "File"
path = "/var/www/example"
mode = "sendfile"

[[route]]
[route.conditions]
host = "api.example.com"
[route.action]
type = "Proxy"
url = "http://localhost:8080"

# Static file (exact match)
[[route]]
[route.conditions]
host = "example.com"
path = "/robots.txt"
[route.action]
type = "File"
path = "/var/www/robots.txt"

# Directory serving (with trailing slash)
[[route]]
[route.conditions]
host = "example.com"
path = "/static/*"
[route.action]
type = "File"
path = "/var/www/assets/"
mode = "sendfile"
# OpenFileCache configuration (route-specific, overrides global setting)
# This enables file metadata caching for this route, reducing system calls
[route.open_file_cache]
enabled = true
valid_duration_secs = 300  # 5 minutes (static files change infrequently)
max_entries = 50000

# Directory serving (without trailing slash - same behavior, no redirect)
[[route]]
[route.conditions]
host = "example.com"
path = "/docs"
[route.action]
type = "File"
path = "/var/www/docs/"

# Custom index file
[[route]]
[route.conditions]
host = "example.com"
path = "/user/*"
[route.action]
type = "File"
path = "/var/www/user/"
index = "profile.html"

# Proxy (with trailing slash)
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080/app/"

# Proxy (without trailing slash - same behavior)
[[route]]
[route.conditions]
host = "example.com"
path = "/backend"
[route.action]
type = "Proxy"
url = "http://localhost:3000"

# Root
[[route]]
[route.conditions]
host = "example.com"
path = "/"
[route.action]
type = "File"
path = "/var/www/index.html"
```

## Routing

### Unified Routing (AWS ALB-compliant)

Routes are evaluated in array order (first-match). All routes use the unified `[[route]]` structure with `conditions` and `action` fields.

1. **Route conditions** (`[route.conditions]`): Match on host, path, headers, method, query parameters, or source IP
   - `host`: Host header matching (wildcard supported, e.g., "api.example.com", "*.example.com")
   - `path`: Path pattern matching (wildcard supported, e.g., "/api/*", "/static/*")
   - `header`: HTTP header matching (map for multiple headers, e.g., `{ "X-Version" = "v2" }`)
   - `method`: HTTP request method matching (array for multiple methods, e.g., `["GET", "POST"]`)
   - `query`: Query string parameter matching (map for multiple query params, e.g., `{ "token" = "secret" }`)
   - `source_ip`: Source IP matching (CIDR notation, array for multiple CIDRs, e.g., `["192.168.0.0/16", "10.0.0.0/8"]`)
   - All conditions are combined with AND logic. If a condition is not specified, it matches all requests (default route).
2. **Route action** (`[route.action]`): Backend action (File, Proxy, Redirect, etc.)
3. **Route-level settings** (`[route.security]`, `[route.cache]`, `[route.compression]`, `[route.buffering]`, `[route.open_file_cache]`): Override action-level settings
4. **Route-level WASM modules** (`modules`): List of WASM module names to apply to this route (set at route level, not under `route.action`)

### Backend Types

| Type | Description | Configuration Example |
|------|-------------|----------------------|
| `Proxy` | HTTP reverse proxy (single) | `{ type = "Proxy", url = "http://localhost:8080" }` |
| `Proxy` | HTTP reverse proxy (LB) | `{ type = "Proxy", upstream = "backend-pool" }` |
| `Proxy` | HTTPS proxy (with SNI) | `{ type = "Proxy", url = "https://192.168.1.100", sni_name = "api.example.com" }` |
| `File` | Static file serving | `{ type = "File", path = "/var/www", mode = "sendfile" }` |
| `Redirect` | HTTP redirect | `{ type = "Redirect", redirect_url = "https://new.example.com", redirect_status = 301 }` |

> **Note**: `Proxy` type uses either `url` (single backend) or `upstream` (load balancing). WebSocket is automatically supported for both. When connecting to HTTPS backends via IP, you can specify the SNI name with `sni_name`.

### Routing Behavior (Nginx-style)

#### 1. Static File (Exact Match)

If `path` in the configuration is a file, the file is returned only when the request path matches exactly.

```toml
# /robots.txt → returns /var/www/robots.txt
# /robots.txt/extra → 404 Not Found (cannot traverse below a file)
[[route]]
[route.conditions]
host = "example.com"
path = "/robots.txt"
[route.action]
type = "File"
path = "/var/www/robots.txt"
```

#### 2. Directory Serving (Alias Behavior)

If `path` in the configuration is a directory, the remaining path after removing the prefix is joined to the directory.
**Trailing slash is optional** (both behave the same).

```toml
# With trailing slash (traditional style)
[[route]]
[route.conditions]
host = "example.com"
path = "/static/*"
[route.action]
type = "File"
path = "/var/www/assets/"

# Without trailing slash (same behavior, no 301 redirect)
[[route]]
[route.conditions]
host = "example.com"
path = "/docs"
[route.action]
type = "File"
path = "/var/www/docs/"
```

| Request | Configuration | Resolved Path |
|---------|---------------|---------------|
| `/static/css/style.css` | `"/static/"` | `/var/www/assets/css/style.css` |
| `/static/` | `"/static/"` | `/var/www/assets/index.html` |
| `/docs` | `"/docs"` | `/var/www/docs/index.html` *returned directly |
| `/docs/` | `"/docs"` | `/var/www/docs/index.html` |
| `/docs/guide/intro.html` | `"/docs"` | `/var/www/docs/guide/intro.html` |

#### 3. Index File Specification

Use the `index` option to specify the file returned when accessing a directory.
Defaults to `index.html` if not specified.

```toml
# /user/ → returns /var/www/user/profile.html
[[route]]
[route.conditions]
host = "example.com"
path = "/user/*"
[route.action]
type = "File"
path = "/var/www/user/"
index = "profile.html"

# /app/ → returns /var/www/app/dashboard.html
[[route]]
[route.conditions]
host = "example.com"
path = "/app/*"
[route.action]
type = "File"
path = "/var/www/app/"
index = "dashboard.html"
```

#### 4. Proxy (Proxy Pass Behavior)

The remaining path after removing the prefix is joined to the backend URL.
**Trailing slash is optional**.

```toml
# With trailing slash
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080/app/"

# Without trailing slash (same behavior)
[[route]]
[route.conditions]
host = "example.com"
path = "/backend"
[route.action]
type = "Proxy"
url = "http://localhost:3000"
```

| Request | Configuration | Forwarded To |
|---------|---------------|--------------|
| `/api/v1/users` | `"/api/"` → `url = ".../app/"` | `http://localhost:8080/app/v1/users` |
| `/backend` | `"/backend"` → `url = ".../"` | `http://localhost:3000/` |
| `/backend/users` | `"/backend"` | `http://localhost:3000/users` |

### Route Conditions Examples

All conditions are combined with AND logic. If a condition is not specified, it matches all requests (default route).

#### Host and Path Conditions

```toml
# Host-based routing
[[route]]
[route.conditions]
host = "api.example.com"
[route.action]
type = "Proxy"
url = "http://localhost:8080"

# Path-based routing
[[route]]
[route.conditions]
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080"
```

#### HTTP Header Condition

```toml
# Match requests with X-Version header set to "v2"
[[route]]
[route.conditions]
host = "api.example.com"
path = "/api/*"
header = { "X-Version" = "v2" }
[route.action]
type = "Proxy"
url = "http://localhost:8080/v2/"

# Multiple headers (all must match)
[[route]]
[route.conditions]
host = "api.example.com"
path = "/api/*"
header = { "X-Version" = "v2", "X-API-Key" = "secret" }
[route.action]
type = "Proxy"
url = "http://localhost:8080/v2/"
```

#### HTTP Method Condition

```toml
# Match only GET and POST requests
[[route]]
[route.conditions]
host = "api.example.com"
path = "/api/*"
method = ["GET", "POST"]
[route.action]
type = "Proxy"
url = "http://localhost:8080/"
```

#### Query String Condition

```toml
# Match requests with token query parameter set to "secret"
[[route]]
[route.conditions]
host = "api.example.com"
path = "/api/*"
query = { "token" = "secret" }
[route.action]
type = "Proxy"
url = "http://localhost:8080/"

# Multiple query parameters (all must match)
[[route]]
[route.conditions]
host = "api.example.com"
path = "/api/*"
query = { "format" = "json", "version" = "1" }
[route.action]
type = "Proxy"
url = "http://localhost:8080/"
```

#### Source IP Condition

```toml
# Match requests from specific CIDR ranges
[[route]]
[route.conditions]
host = "admin.example.com"
path = "/admin/*"
source_ip = ["192.168.0.0/16", "10.0.0.0/8"]
[route.action]
type = "Proxy"
url = "http://localhost:9000/"
```

#### Combined Conditions

```toml
# All conditions must match (AND logic)
[[route]]
[route.conditions]
host = "api.example.com"
path = "/api/v2/*"
header = { "X-Version" = "v2", "X-API-Key" = "secret" }
method = ["GET", "POST"]
query = { "format" = "json" }
source_ip = ["192.168.0.0/16"]
[route.action]
type = "Proxy"
url = "http://localhost:8080/v2/"
```

### Proxy-Wasm Extension (Route-level Configuration)

WASM modules are configured at the route level (not under `route.action`):

```toml
[[route]]
[route.conditions]
host = "api.example.com"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080/"
# WASM module names to apply to this route
modules = ["header_filter", "waf_filter"]
```

### File Serving Mode

| Mode | Description | Use Case |
|------|-------------|----------|
| `sendfile` | Zero-copy transfer via sendfile system call | Large files, videos, images |
| `memory` | Load file into memory for delivery | Small files, favicon.ico, etc. |

```toml
# Directory serving (sendfile mode)
[[route]]
[route.conditions]
host = "example.com"
path = "/static/*"
[route.action]
type = "File"
path = "/var/www/static"
mode = "sendfile"

# Single file serving (memory mode)
[[route]]
[route.conditions]
host = "example.com"
path = "/favicon.ico"
[route.action]
type = "File"
path = "/var/www/favicon.ico"
mode = "memory"

# Default when type and mode are omitted (type = "File", mode = "sendfile")
[[route]]
[route.conditions]
host = "example.com"
path = "/"
[route.action]
path = "/var/www/html"
```

### Proxy Configuration

Supports proxying to HTTP and HTTPS backends:

```toml
# HTTP backend
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080"

# HTTPS backend (TLS client connection)
[[route]]
[route.conditions]
host = "example.com"
path = "/secure/*"
[route.action]
type = "Proxy"
url = "https://backend.example.com"
```

### H2C (HTTP/2 over cleartext) Proxy

When the backend supports H2C (HTTP/2 without TLS), specify `use_h2c = true` to communicate via HTTP/2.

```toml
# H2C connection to gRPC backend
[[route]]
[route.conditions]
host = "example.com"
path = "/grpc/*"
[route.action]
type = "Proxy"
url = "http://localhost:50051"
use_h2c = true
```

| Option | Description | Default |
|--------|-------------|---------|
| `use_h2c` | Use H2C (HTTP/2 without TLS) | false |

**H2C Use Cases:**
- Connecting to gRPC backends (internal network)
- Leverage HTTP/2 multiplexing and header compression for backend communication
- Uses Prior Knowledge mode (not via Upgrade)

> **Note**: H2C cannot be used with HTTPS backends (TLS connections). Use only in environments where TLS is not required, such as gRPC communication within internal networks.

#### SNI (Server Name Indication) Configuration

When connecting to HTTPS backends, you can specify a domain name for SNI even when the backend is specified by IP address.
This allows obtaining the correct certificate even from servers with virtual host configurations.

```toml
# IP address specification + SNI name
[[route]]
[route.conditions]
host = "example.com"
path = "/internal-api/*"
[route.action]
type = "Proxy"
url = "https://192.168.1.100:443"
sni_name = "api.internal.example.com"
```

| Setting | Description | Default |
|---------|-------------|---------|
| `sni_name` | SNI name for TLS connection (uses URL hostname if omitted) | URL hostname |

> **Note**: When `sni_name` is specified, TLS certificate verification is also performed against that name. The backend server's certificate must include the specified domain name (or wildcard).

### Load Balancing Configuration

Request distribution to multiple backends:

```toml
# Define upstream group
[upstreams."api-pool"]
algorithm = "round_robin"  # or "least_conn", "ip_hash"
servers = [
  "http://api1:8080",
  "http://api2:8080",
  "http://api3:8080"
]

  # Health check (optional)
  [upstreams."api-pool".health_check]
  interval_secs = 10
  path = "/health"
  timeout_secs = 5
  healthy_statuses = [200]
  unhealthy_threshold = 3
  healthy_threshold = 2

# Route referencing upstream
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
upstream = "api-pool"
```

#### SNI Configuration in Upstream

Upstream server entries support both string and struct formats.
Using struct format allows specifying SNI names when using IP addresses.

```toml
# HTTPS backend pool (with SNI name specification)
[upstreams."https-pool"]
algorithm = "least_conn"
servers = [
  # Struct format: IP address + SNI name
  { url = "https://192.168.1.100:443", sni_name = "api.example.com" },
  { url = "https://192.168.1.101:443", sni_name = "api.example.com" },
  # String format: domain name specification (SNI name automatically uses URL hostname)
  "https://api.example.com:443"
]

# Route referencing upstream
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
upstream = "https-pool"
```

> **Note**: String and struct formats can be mixed within the same array. The traditional string format continues to work for backward compatibility.

### WebSocket Configuration

WebSocket is automatically supported with regular Proxy. Polling behavior during bidirectional transfer can be customized via configuration.

#### Basic Configuration

```toml
# WebSocket application
[[route]]
[route.conditions]
host = "example.com"
path = "/ws/*"
[route.action]
type = "Proxy"
url = "http://localhost:3000"

# WebSocket with load balancing
[[route]]
[route.conditions]
host = "example.com"
path = "/ws-lb/*"
[route.action]
type = "Proxy"
upstream = "websocket-pool"
```

#### Polling Mode Configuration

Controls polling behavior during WebSocket bidirectional transfer.

| Option | Description | Default |
|--------|-------------|---------|
| `websocket_poll_mode` | Polling mode (`"fixed"` / `"adaptive"`) | `"adaptive"` |
| `websocket_poll_timeout_ms` | Initial timeout (milliseconds) | 1 |
| `websocket_poll_max_timeout_ms` | Maximum timeout (milliseconds) *adaptive only | 100 |
| `websocket_poll_backoff_multiplier` | Backoff multiplier *adaptive only | 2.0 |

#### Choosing Polling Mode

| Mode | Behavior | Use Case |
|------|----------|----------|
| `fixed` | Always uses fixed timeout | Real-time games, low latency priority |
| `adaptive` | Short when active, longer when idle | Chat, monitoring dashboards, balance focused |

**Adaptive Mode Behavior:**

```
Data transferred → Reset timeout (return to initial value)
Timeout occurred → Timeout × multiplier (extend up to max)

Example: initial=1ms, max=100ms, multiplier=2.0
1ms → 2ms → 4ms → 8ms → 16ms → 32ms → 64ms → 100ms (stops at max)
↓ When data arrives
1ms (reset)
```

#### WebSocket Configuration Examples

```toml
# Real-time game (low latency priority)
[[route]]
[route.conditions]
host = "game.example.com"
path = "/ws/*"
[route.action]
type = "Proxy"
url = "http://localhost:3000"

[route.security]
  websocket_poll_mode = "fixed"
  websocket_poll_timeout_ms = 1

# Chat application (balance focused)
[[route]]
[route.conditions]
host = "chat.example.com"
path = "/ws/*"
[route.action]
type = "Proxy"
url = "http://localhost:3001"

[route.security]
  websocket_poll_mode = "adaptive"
  websocket_poll_timeout_ms = 1
  websocket_poll_max_timeout_ms = 50
  websocket_poll_backoff_multiplier = 2.0

# Monitoring dashboard (CPU efficiency priority)
[[route]]
[route.conditions]
host = "monitor.example.com"
path = "/ws/*"
[route.action]
type = "Proxy"
url = "http://localhost:3002"

[route.security]
  websocket_poll_mode = "adaptive"
  websocket_poll_timeout_ms = 10
  websocket_poll_max_timeout_ms = 200
  websocket_poll_backoff_multiplier = 1.5
```

### Global Security Configuration

Configure server-wide security settings in the `[security]` section.

```toml
[security]
# Privilege dropping settings (Linux only, effective only when started as root)
drop_privileges_user = "veil"
drop_privileges_group = "veil"

# Global concurrent connection limit (0 = unlimited)
max_concurrent_connections = 10000

# seccomp system call restriction
enable_seccomp = true
seccomp_mode = "filter"

# Landlock filesystem restriction (Linux 5.13+)
enable_landlock = true
landlock_read_paths = ["/etc/veil", "/usr", "/lib", "/lib64"]
landlock_write_paths = ["/var/log/veil"]
```

#### Privilege and Connection Limits

| Option | Description | Default |
|--------|-------------|---------|
| `drop_privileges_user` | Username to drop to after startup | none |
| `drop_privileges_group` | Group name to drop to after startup | none |
| `max_concurrent_connections` | Maximum concurrent connections | 0 (unlimited) |
| `blocked_ips` | Front-line IP/CIDR blocklist; matching connections are dropped right after `accept` (before the TLS handshake / handler spawn), avoiding expensive work for known-bad IPs. CIDRs are parsed once at startup (zero-alloc check on the accept hot path) and hot-reloadable via SIGHUP. Evaluated earlier than per-route `denied_ips`. | `[]` |
| `allow_security_failures` | Behavior when security feature activation fails | false |

#### Security Feature Failure Handling

The `allow_security_failures` option controls the behavior when security features (sandbox, seccomp, Landlock) fail to activate.

| Value | Behavior | Use Case |
|-------|----------|----------|
| `false` (default) | Abort server startup on activation failure | **Recommended for production** - Ensures security features are reliably enabled |
| `true` | Continue startup with warnings on activation failure | Development/debugging - Allows development to continue even when security features are unavailable |

**Default Behavior (`allow_security_failures = false`):**

When security feature activation fails, the server outputs detailed error messages and aborts startup. This prevents the server from running in production with security features disabled.

```toml
[security]
# Default: false (abort on failure)
# allow_security_failures = false

enable_sandbox = true
enable_seccomp = true
enable_landlock = true
```

**Development/Debug Mode (`allow_security_failures = true`):**

Allows the server to start with warnings even when security features are unavailable (e.g., insufficient kernel version, missing privileges).

```toml
[security]
# Set to true only in development when security features are unavailable
allow_security_failures = true

enable_sandbox = true
enable_seccomp = true
enable_landlock = true
```

**Notes:**

- **Production environments should use `false` (default)**: Ensures security features are reliably enabled
- **Privilege drop failures**: Privilege drop failures always abort startup (regardless of `allow_security_failures` setting)
- **Error messages**: Detailed error messages and troubleshooting hints are displayed on failure

#### seccomp Configuration

| Option | Description | Default |
|--------|-------------|---------|
| `enable_seccomp` | Enable seccomp filter | false |
| `seccomp_mode` | seccomp mode | "disabled" |

| seccomp Mode | Description |
|--------------|-------------|
| `disabled` | Disabled |
| `log` | Log violations (no blocking, recommended for initial deployment) |
| `filter` | Reject violations with EPERM (**recommended for production**) |
| `strict` | SIGKILL on violation (most strict) |

#### Landlock Configuration (Linux 5.13+)

| Option | Description | Default |
|--------|-------------|---------|
| `enable_landlock` | Enable Landlock | false |
| `landlock_read_paths` | Read-only paths | `["/etc", "/usr", "/lib", "/lib64"]` |
| `landlock_write_paths` | Read-write paths | `["/var/log", "/tmp"]` |

**Supported ABI Versions:**

| ABI | Kernel | Added Features |
|-----|--------|----------------|
| v1 | 5.13+ | Basic filesystem access control |
| v2 | 5.19+ | File reference permission (REFER) |
| v3 | 6.2+ | TRUNCATE permission |
| v4 | 6.7+ | Network restriction (no FS changes) |
| v5+ | 6.10+ | IOCTL_DEV permission |

#### Sandbox Configuration (bubblewrap equivalent)

Achieve security isolation equivalent to bubblewrap by applying Linux namespace isolation, bind mounts, and capabilities restrictions.

| Option | Description | Default |
|--------|-------------|---------|
| `enable_sandbox` | Enable sandbox | false |
| `sandbox_unshare_mount` | Mount namespace isolation | true |
| `sandbox_unshare_uts` | UTS namespace isolation (hostname isolation) | true |
| `sandbox_unshare_ipc` | IPC namespace isolation | true |
| `sandbox_unshare_pid` | PID namespace isolation | false |
| `sandbox_unshare_user` | User namespace isolation | false |
| `sandbox_unshare_net` | Network namespace isolation (**Warning: disables networking**) | false |
| `sandbox_keep_capabilities` | Capabilities to keep | [] |
| `sandbox_ro_bind_mounts` | Read-only bind mounts (source:dest format) | standard paths |
| `sandbox_rw_bind_mounts` | Read-write bind mounts | [] |
| `sandbox_tmpfs_mounts` | tmpfs mount destinations | ["/tmp"] |
| `sandbox_mount_proc` | Mount /proc | true |
| `sandbox_mount_dev` | Create /dev | true |
| `sandbox_hostname` | Hostname inside sandbox | "veil-sandbox" |
| `sandbox_no_new_privs` | Set PR_SET_NO_NEW_PRIVS | true |

```toml
[security]
enable_sandbox = true
sandbox_unshare_mount = true
sandbox_unshare_uts = true
sandbox_unshare_ipc = true
sandbox_keep_capabilities = ["CAP_NET_BIND_SERVICE"]
sandbox_ro_bind_mounts = ["/usr:/usr", "/lib:/lib", "/lib64:/lib64"]
sandbox_tmpfs_mounts = ["/tmp"]
```

> **Note**: Setting `sandbox_unshare_net = true` will disable network communication. For reverse proxies, typically leave this as `false`.

> **Note**: When using privileged ports (below 1024), either grant `CAP_NET_BIND_SERVICE` capability or use unprivileged ports.
>
> ```bash
> sudo setcap 'cap_net_bind_service=+ep' ./target/release/veil
> ```

### Per-Route Security Configuration

Add a `security` subsection to each route for fine-grained security settings.

#### Configuration Options

| Category | Option | Description | Default |
|----------|--------|-------------|---------|
| Size Limits | `max_request_body_size` | Maximum request body size (bytes) | 10MB |
| | `max_chunked_body_size` | Maximum cumulative size for chunked transfer | 10MB |
| | `max_request_header_size` | Maximum request header size | 8KB |
| Timeouts | `client_header_timeout_secs` | Client header receive timeout | 30s |
| | `client_body_timeout_secs` | Client body receive timeout | 30s |
| | `backend_connect_timeout_secs` | Backend connection timeout | 10s |
| Access Control | `allowed_methods` | Allowed HTTP methods (array) | all allowed |
| | `rate_limit_requests_per_min` | Request limit per minute | 0 (unlimited) |
| | `allowed_ips` | Allowed IP/CIDR (array) | all allowed |
| | `denied_ips` | Denied IP/CIDR (array, takes priority) | none |
| Connection Pool | `max_idle_connections_per_host` | Max idle connections per host | 8 |
| | `idle_connection_timeout_secs` | Idle connection timeout | 30s |
| Header Manipulation | `add_request_headers` | Headers to add before forwarding to backend | none |
| | `remove_request_headers` | Headers to remove before forwarding to backend | none |
| | `add_response_headers` | Headers to add before sending to client | none |
| | `remove_response_headers` | Headers to remove before sending to client | none |
| WebSocket | `websocket_poll_mode` | Polling mode (`"fixed"` / `"adaptive"`) | `"adaptive"` |
| | `websocket_poll_timeout_ms` | Initial timeout (milliseconds) | 1 |
| | `websocket_poll_max_timeout_ms` | Maximum timeout (milliseconds) *adaptive only | 100 |
| | `websocket_poll_backoff_multiplier` | Backoff multiplier *adaptive only | 2.0 |

#### Security Configuration Examples

```toml
# Security settings for API
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080/app/"

[route.security]
  allowed_methods = ["GET", "POST", "PUT"]
  max_request_body_size = 5_242_880  # 5MB
  backend_connect_timeout_secs = 5
  rate_limit_requests_per_min = 60

# Admin API with IP restriction
[[route]]
[route.conditions]
host = "example.com"
path = "/admin/*"
[route.action]
type = "Proxy"
url = "http://localhost:9000/"

[route.security]
  allowed_ips = [
    "192.168.0.0/16",
    "10.0.0.0/8",
    "127.0.0.1"
  ]
  denied_ips = ["192.168.1.100"]
  allowed_methods = ["GET", "POST"]
```

#### IP Restriction Evaluation Order

IP restrictions are evaluated in **deny → allow** order (deny takes priority).

1. Matches `denied_ips` → Reject (403 Forbidden)
2. `allowed_ips` is empty → Allow
3. Matches `allowed_ips` → Allow
4. Otherwise → Reject (403 Forbidden)

| Format | Example |
|--------|---------|
| Single IPv4 | `192.168.1.1` |
| IPv4 CIDR | `192.168.0.0/24` |
| Single IPv6 | `::1` |
| IPv6 CIDR | `2001:db8::/32` |

## Load Balancing

Supports request distribution to multiple backend servers.

### Algorithms

| Algorithm | Description | Use Case |
|-----------|-------------|----------|
| `round_robin` | Distribute in order (default) | General purpose |
| `least_conn` | Select server with fewest connections | Long-lived connections |
| `ip_hash` | Hash by client IP | Session persistence |
| `weighted` | Weighted round robin proportional to `weight` | Heterogeneous server capacities |
| `consistent_hash` | 150-vnode consistent hash ring (xxh3) | Cache locality, sticky routing |

### Configuration Examples

```toml
# Define upstream group (string format)
[upstreams."backend-pool"]
algorithm = "round_robin"
servers = [
  "http://localhost:8080",
  "http://localhost:8081",
  "http://localhost:8082"
]

# Reference upstream in route
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"  # Specify upstream instead of URL
```

#### HTTPS Backends with SNI Name

Specify SNI name for HTTPS backends using IP addresses:

```toml
# HTTPS backend pool (mixed struct and string formats)
[upstreams."https-api-pool"]
algorithm = "least_conn"
servers = [
  # Struct format: IP address + SNI name specification
  { url = "https://192.168.1.100:443", sni_name = "api.internal.example.com" },
  { url = "https://192.168.1.101:443", sni_name = "api.internal.example.com" },
  # String format: domain name specification (SNI name automatically uses URL hostname)
  "https://api.example.com:443"
]
```

#### Weighted Round Robin

Route more traffic to higher-capacity servers by assigning relative weights:

```toml
[upstreams."weighted-api"]
algorithm = "weighted"
servers = [
  { url = "http://api1:8080", weight = 3 },  # receives 75% of traffic
  { url = "http://api2:8080", weight = 1 },  # receives 25% of traffic
]
```

> **Note**: `weight = 0` is treated as `weight = 1` (minimum). Offsets are built at startup — selection is lock-free (atomic fetch_add + binary search).

#### Consistent Hash

Route requests from the same source to the same backend (sticky routing):

```toml
# Hash by client IP (default)
[upstreams."ch-pool"]
algorithm = "consistent_hash"
servers = ["http://cache1:8080", "http://cache2:8080", "http://cache3:8080"]

# Hash by HTTP header value
[upstreams."ch-by-user"]
algorithm = "consistent_hash"
hash_key = "header:X-User-Id"
servers = ["http://shard1:8080", "http://shard2:8080"]

# Hash by Cookie value
[upstreams."ch-by-session"]
algorithm = "consistent_hash"
hash_key = "cookie:session_id"
servers = ["http://node1:8080", "http://node2:8080"]
```

> **Note**: Uses a 150-vnode ring per server (xxh3 hash). When a server becomes unhealthy it is excluded and the next node in the ring takes over.

### Compatibility with Single Backend

The traditional `url` specification continues to work:

```toml
# Traditional single backend specification
[[route]]
[route.conditions]
host = "example.com"
path = "/simple/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080"
```

## Health Check

Monitors backend server health and automatically excludes unhealthy servers.

### Behavior

1. Periodically sends HTTP requests in a background thread
2. Checks response status codes
3. Excludes server when consecutive failures reach threshold
4. Restores server when consecutive successes reach threshold

### Configuration Options

| Option | Description | Default |
|--------|-------------|---------|
| `check_type` | Check protocol: `http`, `tcp`, or `grpc` | `http` |
| `interval_secs` | Check interval (seconds) | 10 |
| `path` | Path to check (HTTP: request path; gRPC: service name) | `/` |
| `timeout_secs` | Timeout (seconds) | 5 |
| `healthy_statuses` | Status codes considered successful (HTTP only) | [200, 201, 202, 204, 301, 302, 304] |
| `unhealthy_threshold` | Consecutive failures to mark unhealthy | 3 |
| `healthy_threshold` | Consecutive successes to mark healthy | 2 |
| `use_tls` | Use TLS connection for health check | **false** |
| `verify_cert` | Verify TLS certificate (use_tls=true only) | **true** |

### HTTP Health Check (default)

Sends an HTTP/HTTPS request and validates the response status code.

```toml
[upstreams."api-servers"]
algorithm = "least_conn"
servers = [
  "http://api1.internal:8080",
  "http://api2.internal:8080"
]

  [upstreams."api-servers".health_check]
  check_type = "http"      # default, can be omitted
  interval_secs = 10
  path = "/health"
  timeout_secs = 5
  healthy_statuses = [200]
  unhealthy_threshold = 3
  healthy_threshold = 2
```

### TCP Health Check

Checks liveness by attempting a TCP connection only — no HTTP data is exchanged. Suitable for non-HTTP backends (databases, message brokers, etc.).

```toml
[upstreams."db-servers"]
algorithm = "round_robin"
servers = [
  "http://db1.internal:5432",
  "http://db2.internal:5432"
]

  [upstreams."db-servers".health_check]
  check_type = "tcp"
  interval_secs = 15
  timeout_secs = 3
  unhealthy_threshold = 2
  healthy_threshold = 1
```

### gRPC Health Check

Implements [gRPC Health Checking Protocol](https://github.com/grpc/grpc/blob/master/doc/health-checking.md) via HTTP/1.1 POST with `Content-Type: application/grpc`. A response of `grpc-status: 0` (OK) is treated as healthy.

> **Note**: This implementation uses HTTP/1.1 framing. It works with backends that expose a gRPC health endpoint over HTTP/1.1 (gRPC-Web compatible). Full HTTP/2 gRPC is not currently supported.

```toml
[upstreams."grpc-servers"]
algorithm = "least_conn"
servers = [
  "http://grpc1.internal:50051",
  "http://grpc2.internal:50051"
]

  [upstreams."grpc-servers".health_check]
  check_type = "grpc"
  interval_secs = 10
  path = "grpc.health.v1.Health"  # service name (empty = server-level check)
  timeout_secs = 5
  unhealthy_threshold = 3
  healthy_threshold = 2
```

### TLS Health Check

When `use_tls = true`, the health check uses TLS connection. Applies to both `http` and `grpc` check types.

```toml
  [upstreams."api-servers".health_check]
  check_type = "http"
  path = "/health"
  use_tls = true
  verify_cert = true   # set to false for self-signed certificates
```

> **Note**: When `verify_cert = false`, self-signed certificates are accepted. Not recommended for production.

### Log Output

Health status changes are logged:

```
[INFO] Upstream api1.internal:8080 is now unhealthy
[INFO] Upstream api1.internal:8080 is now healthy
```

## L4 Stream Proxy

TCP-level (L4) load balancing proxy. Unlike the HTTP proxy, it forwards raw TCP streams without inspecting protocol payloads. Useful for databases, message brokers, Redis, SMTP, and any non-HTTP binary protocol.

> **Requires**: build with `--features l4-proxy` (included in `--features full`)

### Features

- **Round Robin / LeastConn** load balancing across upstream TCP servers
- **TLS Passthrough**: forward encrypted TLS without termination (SNI routing is not yet implemented)
- **Connection Limiting**: reject connections when `max_connections` is reached
- **Connect Timeout**: configurable upstream connection timeout
- **Health Check Integration**: TCP or gRPC health checks per L4 listener
- **Independent threads**: each L4 listener runs in its own thread on the io_uring runtime

### Configuration

L4 listeners are defined using `[[l4]]` sections (separate from HTTP routes). L4 listeners are started at launch and **cannot be hot-reloaded** via SIGHUP.

```toml
# TCP proxy for PostgreSQL (requires --features l4-proxy)
[[l4]]
name = "postgres-proxy"          # identifies this listener in logs
listen = "0.0.0.0:5432"          # bind address
lb = "least_conn"                # "round_robin" (default) or "least_conn"
tls = "none"                     # "none" (default), "passthrough", or "terminate"
max_connections = 200            # 0 = unlimited (default)
connect_timeout_secs = 5         # default: 10

  [[l4.upstreams]]
  addr = "10.0.0.1:5432"
  weight = 1

  [[l4.upstreams]]
  addr = "10.0.0.2:5432"
  weight = 1

  # Optional TCP health check
  [l4.health_check]
  check_type = "tcp"
  interval_secs = 10
  timeout_secs = 3
  unhealthy_threshold = 2
  healthy_threshold = 1
```

```toml
# TCP proxy for Redis
[[l4]]
name = "redis-proxy"
listen = "0.0.0.0:6379"
lb = "round_robin"

  [[l4.upstreams]]
  addr = "redis1.internal:6379"

  [[l4.upstreams]]
  addr = "redis2.internal:6379"
```

```toml
# gRPC TCP proxy with gRPC health check
[[l4]]
name = "grpc-proxy"
listen = "0.0.0.0:50051"
lb = "least_conn"
max_connections = 500

  [[l4.upstreams]]
  addr = "grpc1.internal:50051"

  [[l4.upstreams]]
  addr = "grpc2.internal:50051"

  [l4.health_check]
  check_type = "grpc"
  path = "grpc.health.v1.Health"
  interval_secs = 15
  timeout_secs = 5
```

### Configuration Reference

| Option | Description | Default |
|--------|-------------|---------|
| `name` | Listener name (appears in logs) | required |
| `listen` | Bind address (e.g. `"0.0.0.0:3306"`) | required |
| `lb` | Load balancing: `round_robin` or `least_conn` | `round_robin` |
| `tls` | TLS mode: `none`, `passthrough`, or `terminate` | `none` |
| `max_connections` | Max simultaneous connections (0 = unlimited) | `0` |
| `connect_timeout_secs` | Upstream connect timeout in seconds | `10` |
| `upstreams[].addr` | Upstream address (`"host:port"`) | required |
| `upstreams[].weight` | Weight (reserved for weighted RR) | `1` |
| `health_check` | Optional health check config (same as upstream health_check) | none |

### Notes

- L4 listeners bind ports at startup. **SIGHUP does not reload L4 configuration**.
- HTTP proxy and L4 proxy can coexist — they listen on different ports.
- TLS termination (`tls = "terminate"`) is reserved for future implementation; currently treated as passthrough.

## Circuit Breaker & Resilience

Per-server circuit breaker, outlier detection, and EWMA latency tracking protect upstream servers from cascading failures.

### Circuit Breaker State Machine

```
Closed ──(failure_threshold exceeded)──▶ Open
  ▲                                         │
  │                                  (open_duration_secs)
  │                                         │
  └──(success_threshold successes)── HalfOpen ◀─(probe fails)──┐
                                         │                      │
                                         └──(probe succeeds)────┘
```

- **Closed**: Normal operation. Failures are tracked in a sliding window.
- **Open**: All requests are rejected immediately (fast-fail). After `open_duration_secs`, transitions to HalfOpen.
- **HalfOpen**: A limited number of probe requests are allowed. On sufficient successes → Closed. On failure → Open again.

When **all** servers in a pool have open circuit breakers, the pool falls back to healthy servers to avoid complete service unavailability.

### Configuration

```toml
[upstreams."api-pool"]
algorithm = "round_robin"
servers = ["http://api1:8080", "http://api2:8080"]

  [upstreams."api-pool".circuit_breaker]
  enabled = true
  failure_threshold = 5       # Open after this many failures
  failure_window_secs = 60    # Sliding window for failure counting
  open_duration_secs = 30     # Stay Open for this many seconds
  half_open_probes = 3        # Probe requests allowed in HalfOpen
  success_threshold = 2       # Successes in HalfOpen to close
  trip_on_timeout = true      # Count timeouts as failures
```

### Configuration Options

| Option | Description | Default |
|--------|-------------|---------|
| `enabled` | Enable circuit breaker | `false` |
| `failure_threshold` | Consecutive/window failures to open | `5` |
| `failure_window_secs` | Sliding window duration | `60` |
| `open_duration_secs` | How long to stay Open before HalfOpen | `30` |
| `half_open_probes` | Number of probe requests in HalfOpen | `3` |
| `success_threshold` | Successes in HalfOpen to close | `2` |
| `trip_on_timeout` | Treat connection timeouts as failures | `true` |

### Outlier Detection (Passive Ejection)

In addition to the circuit breaker, individual servers can be passively ejected based on error rate:

```toml
  [upstreams."api-pool".outlier_detection]
  enabled = true
  error_rate_threshold = 0.5  # Eject if error rate exceeds 50%
  interval_secs = 10          # Evaluation interval
  base_ejection_time_secs = 30  # Base ejection duration
  max_ejection_percent = 50   # At most 50% of servers ejected simultaneously
```

### Prometheus Metrics (Circuit Breaker)

| Metric | Type | Description |
|--------|------|-------------|
| `veil_circuit_breaker_open_total` | Counter | Total number of CB open events per upstream |
| `veil_circuit_breaker_state` | Gauge | Current CB state per upstream (0=Closed, 1=Open, 2=HalfOpen) |
| `veil_retry_total` | Counter | Total retry attempts |
| `veil_outlier_ejected` | Gauge | 1 if server is currently ejected |

## TLS Certificate Hot Reload

Zero-downtime certificate rotation without restarting the proxy.

### How It Works

- A background thread polls certificate file `mtime` every `reload_interval_secs` seconds.
- When a change is detected, the new certificate is loaded into an `ArcSwap`.
- **Existing TLS connections** continue using the old certificate (no disruption).
- **New TLS handshakes** automatically pick up the new certificate.
- A `SIGHUP` signal also triggers an immediate reload of both config and certificates.

### Configuration

```toml
[tls]
cert_path = "/etc/veil/cert.pem"
key_path  = "/etc/veil/key.pem"
# Zero-downtime certificate hot-reload
auto_reload = true
reload_interval_secs = 60  # Poll interval (default: 60s)
```

### Let's Encrypt Integration

```bash
# Renew certificate (certbot)
certbot renew --deploy-hook "touch /etc/veil/cert.pem"
# veil detects the mtime change and reloads automatically
```

> **Note**: If Landlock sandbox is enabled, the certificate directory must be included in `landlock_read_paths`.

## Redirect

Configure HTTP redirects (301/302/303/307/308). Use for non-WWW handling, HTTPS enforcement, legacy URL migration, etc.

### Configuration Options

| Option | Description | Default |
|--------|-------------|---------|
| `redirect_url` | Redirect destination URL (required) | - |
| `redirect_status` | Status code (301, 302, 303, 307, 308) | 301 |
| `preserve_path` | Append original path to redirect destination | false |

### Status Code Usage

| Code | Description | Use Case |
|------|-------------|----------|
| 301 | Moved Permanently | Permanent relocation (SEO preservation) |
| 302 | Found | Temporary redirect |
| 303 | See Other | POST to GET redirect |
| 307 | Temporary Redirect | Temporary (preserves method) |
| 308 | Permanent Redirect | Permanent (preserves method) |

### Configuration Examples

```toml
# Redirect to WWW
[[route]]
[route.conditions]
host = "example.com"
path = "/"
[route.action]
type = "Redirect"
redirect_url = "https://www.example.com/"
redirect_status = 301

# Legacy URL to new URL migration (preserve path)
[[route]]
[route.conditions]
host = "example.com"
path = "/legacy/*"
[route.action]
type = "Redirect"
redirect_url = "https://example.com/v2"
redirect_status = 301
preserve_path = true
# /legacy/users → https://example.com/v2/users
# /legacy/api/data → https://example.com/v2/api/data

# Force HTTP to HTTPS redirect (configured on different host)
[[route]]
[route.conditions]
host = "http.example.com"
path = "/"
[route.action]
type = "Redirect"
redirect_url = "https://example.com$request_uri"
redirect_status = 301
```

### Special Variables

The following variables can be used in `redirect_url`:

| Variable | Description |
|----------|-------------|
| `$request_uri` | Original request URI |
| `$path` | Path portion after prefix removal |

## Header Manipulation

Add or remove request/response headers. Configure security headers such as X-Real-IP, X-Forwarded-Proto, HSTS, etc.

### Request Header Manipulation

Add or remove headers before forwarding to the backend.

| Option | Description | Example |
|--------|-------------|---------|
| `add_request_headers` | Headers to add (table format) | `{ "X-Real-IP" = "$client_ip" }` |
| `remove_request_headers` | Headers to remove (array) | `["X-Debug-Token"]` |

#### Special Variables

The following variables can be used in `add_request_headers` values:

| Variable | Description |
|----------|-------------|
| `$client_ip` | Client IP address |
| `$host` | Host header from request |
| `$request_uri` | Request URI (path + query string) |

### Response Header Manipulation

Add or remove headers before sending to the client. Also applies to static file serving.

| Option | Description | Example |
|--------|-------------|---------|
| `add_response_headers` | Headers to add | `{ "Strict-Transport-Security" = "max-age=31536000" }` |
| `remove_response_headers` | Headers to remove | `["Server", "X-Powered-By"]` |

### Configuration Example

```toml
# Proxy with security headers
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080"

[route.security]
  # Add before forwarding to backend
  add_request_headers = { "X-Real-IP" = "$client_ip", "X-Forwarded-Proto" = "https" }
  # Remove before forwarding to backend
  remove_request_headers = ["X-Debug-Token", "X-Internal-Auth"]
  # Add before sending to client (security headers)
  add_response_headers = { "Strict-Transport-Security" = "max-age=31536000; includeSubDomains", "X-Frame-Options" = "DENY", "X-Content-Type-Options" = "nosniff" }
  # Remove before sending to client
  remove_response_headers = ["X-Powered-By"]
```

## Server Header Configuration

Control the `Server` HTTP response header sent to clients.

### Security Considerations

The Server header reveals server software information, which can help attackers identify vulnerabilities. It is **recommended to disable in production environments** (default: disabled).

### Configuration

Configure in the `[server]` section:

```toml
[server]
# Enable Server header (default: false)
# Security consideration: Reveals server software information
# Recommended to disable in production
server_header_enabled = false

# Custom Server header value (only effective when server_header_enabled = true)
# Default: "veil"
# When not specified, protocol-specific values are used:
#   - HTTP/1.1: "veil/http1.1"
#   - HTTP/2: "veil/http2"
#   - HTTP/3: "veil/http3"
server_header_value = "MyServer/1.0"
```

### Behavior

| Setting | Behavior |
|---------|----------|
| `server_header_enabled = false` | No Server header is sent (default, recommended for production) |
| `server_header_enabled = true`, `server_header_value` not specified | Protocol-specific values: `veil/http1.1`, `veil/http2`, or `veil/http3` |
| `server_header_enabled = true`, `server_header_value = "Custom"` | All protocols use the custom value: `Server: Custom` |

### Use Cases

- **Development/Testing**: Enable to identify which server is responding
- **Production**: Disable to hide server information (security best practice)
- **Custom Branding**: Set a custom value when Server header is required

## Response Compression

Supports dynamic response compression (Gzip, Brotli, Zstd). Compress responses before sending to clients based on Accept-Encoding header.

### Features

| Feature | Description |
|---------|-------------|
| **Multiple Algorithms** | Gzip, Brotli, Zstd, Deflate support |
| **Content-Type Filtering** | Only compress text/HTML/JSON/etc. |
| **Minimum Size Threshold** | Skip compression for small responses |
| **Accept-Encoding Negotiation** | Automatically select best encoding |

### Enabling

Compression is **disabled by default** to maintain kTLS optimization (zero-copy sendfile).
Enable per-route using the `compression` section:

```toml
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080"

[route.compression]
  enabled = true
```

### Configuration Options

| Option | Description | Default |
|--------|-------------|---------|
| `enabled` | Enable compression | false |
| `preferred_encodings` | Encoding priority order (array) | ["zstd", "br", "gzip"] |
| `gzip_level` | Gzip compression level (1-9) | 4 |
| `brotli_level` | Brotli compression level (0-11) | 4 |
| `zstd_level` | Zstd compression level (1-22) | 3 |
| `min_size` | Minimum size to compress (bytes) | 1024 |
| `compressible_types` | MIME types to compress (prefix match) | text/*, application/json, etc. |
| `skip_types` | MIME types to skip (prefix match) | image/*, video/*, audio/*, etc. |

### Compression Level Guidelines

| Algorithm | Level | Speed | Ratio | Use Case |
|-----------|-------|-------|-------|----------|
| Gzip | 1-3 | Fast | Low | Real-time, high throughput |
| Gzip | 4-6 | Balanced | Medium | General purpose |
| Gzip | 7-9 | Slow | High | Static assets, bandwidth priority |
| Brotli | 0-4 | Fast | Medium | Dynamic content |
| Brotli | 5-9 | Balanced | High | General purpose |
| Brotli | 10-11 | Slow | Highest | Static assets |
| Zstd | 1-3 | Fast | Medium | Real-time APIs |
| Zstd | 4-9 | Balanced | High | General purpose |
| Zstd | 10-22 | Slow | Highest | Archival |

### Configuration Examples

```toml
# API compression (fast, balanced)
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080"

[route.compression]
  enabled = true
  preferred_encodings = ["zstd", "br", "gzip"]
  zstd_level = 3
  brotli_level = 4
  gzip_level = 4
  min_size = 1024

# Static assets (high compression)
[[route]]
[route.conditions]
host = "example.com"
path = "/static/*"
[route.action]
type = "File"
path = "/var/www/static"

[route.compression]
  enabled = true
  preferred_encodings = ["br", "gzip"]
  brotli_level = 6
  gzip_level = 6
  min_size = 256
```

### Default Compressible Types

The following MIME types are compressed by default:

- `text/*` (HTML, CSS, plain text, etc.)
- `application/json`
- `application/javascript`
- `application/xml`
- `application/xhtml+xml`
- `application/rss+xml`
- `application/atom+xml`
- `image/svg+xml`
- `application/wasm`

### Default Skip Types

The following MIME types are **not** compressed (already compressed or binary):

- `image/*`
- `video/*`
- `audio/*`
- `application/octet-stream`
- `application/zip`
- `application/gzip`
- `application/x-gzip`
- `application/x-brotli`

### HTTP/3 Compression Settings

HTTP/3 can have separate compression settings in the `[http3]` section:

```toml
[http3]
compression_enabled = true

  [http3.compression]
  preferred_encodings = ["br", "gzip"]
  brotli_level = 5
  gzip_level = 5
```

> **Note**: When compression is enabled, kTLS zero-copy sendfile optimization is not used for compressed responses. For maximum throughput with large files, consider disabling compression for static file routes.

## Proxy Cache

Supports caching backend responses to reduce backend load and improve response times.

### Features

| Feature | Description |
|---------|-------------|
| **Memory Cache** | Fast in-memory LRU cache with configurable size limit |
| **Disk Cache** | Large response storage using monoio async I/O |
| **ETag/If-None-Match** | 304 Not Modified responses for conditional requests |
| **If-Modified-Since** | Date-based conditional request validation |
| **stale-while-revalidate** | Serve stale content while updating in background |
| **stale-if-error** | Serve stale content when backend returns errors |
| **Vary Header Support** | Separate cache entries based on request headers |
| **Pattern-based Invalidation** | Glob pattern cache invalidation |

### Enabling

Cache is **disabled by default**. Enable per-route using the `cache` section:

```toml
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080"

[route.cache]
  enabled = true
```

### Configuration Options

| Option | Description | Default |
|--------|-------------|---------|
| `enabled` | Enable caching | false |
| `max_memory_size` | Maximum memory cache size (bytes) | 100MB |
| `disk_path` | Disk cache directory (optional) | none |
| `max_disk_size` | Maximum disk cache size (bytes) | 1GB |
| `memory_threshold` | Responses larger than this go to disk (bytes) | 64KB |
| `default_ttl_secs` | Default TTL when Cache-Control is absent | 300 |
| `methods` | HTTP methods to cache | ["GET", "HEAD"] |
| `cacheable_statuses` | Status codes to cache | [200, 301, 302, 304] |
| `bypass_patterns` | Glob patterns to skip caching | [] |
| `respect_vary` | Honor Vary header for cache separation | true |
| `enable_etag` | Enable ETag/If-None-Match validation | true |
| `stale_while_revalidate` | Serve stale while updating in background | false |
| `stale_if_error` | Serve stale on backend errors | false |
| `include_query` | Include query parameters in cache key | true |
| `key_headers` | Request headers to include in cache key | [] |

### Configuration Example

```toml
[[route]]
[route.conditions]
host = "example.com"
path = "/cached-api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080"

[route.cache]
  enabled = true
  max_memory_size = 104857600  # 100MB
  disk_path = "/var/cache/veil/api"
  max_disk_size = 1073741824   # 1GB
  memory_threshold = 65536     # 64KB
  default_ttl_secs = 300
  methods = ["GET", "HEAD"]
  cacheable_statuses = [200, 301, 302, 304]
  bypass_patterns = ["/cached-api/user/*", "/cached-api/session"]
  respect_vary = true
  enable_etag = true
  stale_while_revalidate = true
  stale_if_error = true
  include_query = true
  key_headers = ["Authorization"]  # Per-user caching
```

### Cache Key Generation

Cache keys are generated from:
1. Host name
2. Request path
3. Query parameters (if `include_query = true`)
4. Specified `key_headers` values

### Notes

- When `streaming` buffering mode is used, kTLS zero-copy transfer is preserved
- Cache respects `Cache-Control: no-cache`, `no-store`, `private` headers
- `Vary: *` responses are not cached when `respect_vary = true`

> [!CAUTION]
> **stale_if_error**: When enabled, veil-proxy may serve outdated cached content (up to 1 hour old) when the backend returns 502/504 errors. This improves availability but may cause **data consistency issues** for applications where real-time accuracy is critical (e.g., financial data, medical records, inventory systems). Evaluate your use case carefully before enabling this feature.

## Buffering Control

Controls response buffering to prevent slow clients from blocking backend connections.

> **Note**: The `mode` also governs **HTTP/2 request (upload) direction**. With `streaming`/`adaptive`, eligible HTTP/2 uploads stream to the backend as they arrive (see *HTTP/2 Request Streaming*); with `full`, uploads are fully buffered before forwarding.

### Features

| Feature | Description |
|---------|-------------|
| **Streaming Mode** | Pass-through transfer (default, preserves kTLS) |
| **Full Buffering** | Buffer entire response before sending to client |
| **Adaptive Mode** | Automatically switch based on response size |
| **Disk Spillover** | Write large responses to disk when memory limit exceeded |

### Modes

| Mode | Description | Use Case |
|------|-------------|----------|
| `streaming` | Direct transfer (default) | Large files, real-time APIs, kTLS optimization |
| `full` | Buffer entire response | APIs with slow clients, small responses |
| `adaptive` | Auto-switch based on Content-Length | Mixed workloads |

### Enabling

Buffering is **streaming (pass-through) by default**. Configure per-route using the `buffering` section:

```toml
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080"

[route.buffering]
  mode = "adaptive"
```

### Configuration Options

| Option | Description | Default |
|--------|-------------|---------|
| `mode` | Buffering mode (`streaming`/`full`/`adaptive`) | `streaming` |
| `max_memory_buffer` | Maximum memory buffer size (bytes) | 10MB |
| `adaptive_threshold` | Size threshold for adaptive mode (bytes) | 1MB |
| `disk_buffer_path` | Disk spillover directory (optional) | none |
| `max_disk_buffer` | Maximum disk buffer size (bytes) | 100MB |
| `client_write_timeout_secs` | Client write timeout | 60 |
| `buffer_headers` | Buffer headers along with body | true |

### Configuration Example

```toml
[[route]]
[route.conditions]
host = "example.com"
path = "/buffered-api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080"

[route.buffering]
  mode = "adaptive"
  adaptive_threshold = 1048576   # 1MB
  max_memory_buffer = 10485760   # 10MB
  disk_buffer_path = "/var/tmp/veil/buffer"
  max_disk_buffer = 104857600    # 100MB
  client_write_timeout_secs = 60
  buffer_headers = true
```

### Adaptive Mode Behavior

```
Content-Length <= adaptive_threshold → Full buffering
Content-Length > adaptive_threshold  → Streaming
Content-Length unknown (chunked)     → Streaming
```

### kTLS Compatibility

- **Streaming mode**: kTLS `splice(2)` zero-copy transfer is fully preserved
- **Full/Adaptive modes**: Response passes through userspace buffer (no kTLS optimization)

> **Note**: For maximum performance with kTLS, use `streaming` mode for routes where low latency is critical.

## WebSocket Support

Supports WebSocket (RFC 6455) proxying.
Automatically detects `Connection: Upgrade` and `Upgrade: websocket` headers
and performs bidirectional data transfer.

### Behavior

1. Detect Upgrade request from client
2. Forward Upgrade request to backend
3. Receive 101 Switching Protocols
4. Start bidirectional bypass transfer (operates in configured polling mode)
5. Continue until either connection closes

### Polling Modes

Control polling behavior during WebSocket bidirectional transfer via configuration.

| Mode | Description | Use Case |
|------|-------------|----------|
| `adaptive` (default) | Short during data transfer, longer when idle | General purpose, CPU efficiency focused |
| `fixed` | Always uses fixed timeout | Real-time games, low latency priority |

See the "[WebSocket Configuration](#websocket-configuration)" section for detailed configuration options.

### Configuration Examples

WebSocket is automatically supported with regular Proxy backends:

```toml
# WebSocket application (default settings)
[[route]]
[route.conditions]
host = "example.com"
path = "/ws/*"
[route.action]
type = "Proxy"
url = "http://localhost:3000"

# Low latency configuration (for real-time games)
[[route]]
[route.conditions]
host = "game.example.com"
path = "/ws/*"
[route.action]
type = "Proxy"
url = "http://localhost:3001"

[route.security]
  websocket_poll_mode = "fixed"
  websocket_poll_timeout_ms = 1
```

### Supported Backends

| Protocol | Support |
|----------|---------|
| HTTP → WS | ✅ |
| HTTPS → WSS | ✅ |

## HTTP/2 Support

Supports HTTP/2 (RFC 7540) via TLS ALPN negotiation.

### Features

| Feature | Effect |
|---------|--------|
| Stream Multiplexing | Parallel processing of multiple requests on a single connection |
| HPACK Header Compression | Significantly reduces header overhead |
| Server Push | Latency reduction through proactive resource sending |
| Flow Control | Stream and connection level control |

### Enabling

```bash
# Build with HTTP/2 feature
cargo build --release --features http2
```

```toml
# config.toml
[server]
listen = "0.0.0.0:443"
http2_enabled = true  # Enable HTTP/2 (ALPN h2)
```

### Advanced Configuration

Configure detailed HTTP/2 protocol parameters in the `[http2]` section:

```toml
[http2]
# HPACK dynamic table size (default: 65536)
header_table_size = 65536

# Concurrent streams (default: 256)
max_concurrent_streams = 256

# Stream window size (default: 1048576 = 1MB)
initial_window_size = 1048576

# Maximum frame size (default: 65536)
max_frame_size = 65536

# Maximum header list size (default: 65536)
max_header_list_size = 65536

# Connection window size (default: 1048576 = 1MB)
connection_window_size = 1048576
```

### DoS Protection

HTTP/2 DoS attack mitigations are enabled by default. Configure in the `[http2]` section:

| Attack | CVE | Setting | Default |
|--------|-----|---------|---------|
| Rapid Reset | CVE-2023-44487 | `max_rst_stream_per_second` | 100 |
| CONTINUATION Flood | CVE-2024-24786 | `max_continuation_frames` | 10 |
| Control Frame Flood | - | `max_control_frames_per_second` | 500 |
| HPACK Bomb | - | `max_header_block_size` | 65536 |
| Slow Loris | - | `stream_idle_timeout_secs` | 60 |

```toml
[http2]
# RST_STREAM rate limit (per second)
# Rapid Reset attack mitigation (CVE-2023-44487)
max_rst_stream_per_second = 100

# Control frame rate limit (per second)
# Mitigates PING/SETTINGS flood attacks
max_control_frames_per_second = 500

# CONTINUATION frame limit (per header block)
# CONTINUATION Flood mitigation (CVE-2024-24786)
max_continuation_frames = 10

# Maximum header block size (bytes)
# HPACK Bomb mitigation
max_header_block_size = 65536

# Stream idle timeout (seconds)
# Slow Loris mitigation (0 = disabled)
stream_idle_timeout_secs = 60
```

When limits are exceeded, the server responds with `ENHANCE_YOUR_CALM` (0xb) error and closes the connection.

### HTTP/1.1 Fallback

Clients that don't support HTTP/2 automatically fall back to HTTP/1.1.

## HTTP/3 Support

Supports HTTP/3 (RFC 9114) based on QUIC/UDP. Uses Cloudflare's [quiche](https://github.com/cloudflare/quiche).

### Features

| Feature | Effect |
|---------|--------|
| 0-RTT Connection Establishment | Instant communication without TLS handshake |
| Head-of-Line Blocking Elimination | Packet loss doesn't affect other streams |
| Connection Migration | Maintains connection during network switches |
| GSO/GRO Optimization | High-performance UDP processing |

### Enabling

```bash
# Build with HTTP/3 feature
cargo build --release --features http3
```

```toml
# config.toml
[server]
listen = "0.0.0.0:443"
http3_enabled = true  # Enable HTTP/3 (QUIC/UDP)
```

### Advanced Configuration

Configure detailed HTTP/3 (QUIC) protocol parameters in the `[http3]` section:

```toml
[http3]
# HTTP/3 listen address (UDP, defaults to server.listen if unspecified)
listen = "0.0.0.0:443"

# Maximum idle timeout (milliseconds, default: 30000)
max_idle_timeout = 30000

# Maximum UDP payload size (default: 1350)
max_udp_payload_size = 1350

# Initial maximum data size (entire connection, default: 10000000)
initial_max_data = 10000000

# Initial maximum stream data size (bidirectional local, default: 1000000)
initial_max_stream_data_bidi_local = 1000000

# Initial maximum stream data size (bidirectional remote, default: 1000000)
initial_max_stream_data_bidi_remote = 1000000

# Initial maximum stream data size (unidirectional, default: 1000000)
initial_max_stream_data_uni = 1000000

# Initial maximum bidirectional streams (default: 100)
initial_max_streams_bidi = 100

# Initial maximum unidirectional streams (default: 100)
initial_max_streams_uni = 100

# GSO/GRO optimization (UDP performance optimization)
# GSO (Generic Segmentation Offload) / GRO (Generic Receive Offload) are
# kernel-level features that optimize UDP packet transmission and reception.
#
# Effects:
#   - Send (GSO): coalesce same-destination/same-size QUIC packets into one
#     sendmsg(UDP_SEGMENT) call
#   - Receive (GRO): coalesce multiple datagrams of the same flow in one recvmsg
#   - Reduce system call overhead and CPU usage
#   - The HTTP/3 receive loop reuses a single buffer and feeds GRO segments to
#     quiche as slices, eliminating per-datagram heap allocation and copies
#     (zero-copy receive). Falls back to single-datagram I/O on unsupported kernels.
#
# Notes:
#   - Supported on Linux 5.0+
#   - May not work as expected in some virtual environments or Docker
#   - Set to false if issues occur
#
# Default: false
gso_gro_enabled = false
```

### Notes

- HTTP/3 is UDP-based, so **kTLS cannot be used** (doesn't use TCP)
- UDP port 443 must be opened in the firewall
- Use Alt-Svc header to notify browsers of HTTP/3 support

## kTLS (Kernel TLS) Support

### Overview

kTLS is a Linux kernel feature that performs TLS data transfer phase encryption/decryption at the kernel level.
This project supports kTLS using rustls and a custom kernel TLS module (`src/ktls.rs`, `src/ktls_rustls.rs`).

### Performance Improvements

| Aspect | Effect |
|--------|--------|
| CPU Usage | 20-40% reduction (under high load) |
| Throughput | Up to 2x improvement |
| Latency | Reduced context switches |
| Zero-Copy | sendfile + TLS encryption |

### Enabling Procedure

```bash
# 1. Load kernel module
sudo modprobe tls

# 2. Build with ktls feature
cargo build --release --features ktls

# 3. Enable in config file (config.toml)
# [tls]
# ktls_enabled = true
# ktls_fallback_enabled = true  # optional
```

### Fallback Configuration

Control behavior when kTLS activation fails with `ktls_fallback_enabled`:

| Value | Behavior |
|-------|----------|
| `true` (default) | Continue with rustls on kTLS failure (graceful degradation) |
| `false` | kTLS required mode (reject connection on failure) |

**Benefits of disabling fallback (`ktls_fallback_enabled = false`):**

| Aspect | Effect |
|--------|--------|
| Performance Predictability | All connections guaranteed to use kTLS |
| Debug Ease | No mixed kTLS/rustls state |
| Early Environment Detection | Immediate failure when kTLS unavailable |

**Note:** When fallback is disabled, connections will fail in environments where kTLS is unavailable.
Verify the kernel module is loaded with `modprobe tls` beforehand.

```toml
[tls]
cert_path = "/path/to/cert.pem"
key_path = "/path/to/key.pem"
ktls_enabled = true
ktls_fallback_enabled = false  # kTLS required mode
```

### Requirements

- Linux 5.15 or higher (recommended, but works on earlier versions)
- `tls` kernel module loaded
- AES-GCM cipher suites (TLS 1.2/1.3)
- Built with ktls feature (`--features ktls`)

### Implementation Status

**With ktls feature enabled (`--features ktls`):**
- ✅ kTLS kernel module availability check
- ✅ Automatic kTLS activation after TLS handshake completion
- ✅ kTLS offload for both TX and RX
- ✅ Full async integration with monoio (io_uring)

**Default build (using rustls):**
- ❌ kTLS is not supported
- 👉 Build with `--features ktls` to use kTLS

### Security Considerations

| Risk | Mitigation |
|------|------------|
| Kernel Bugs | Pin kernel version, apply patches regularly |
| Session Key Exposure | TLS handshake runs in userspace (rustls) (maintains PFS) |
| DoS Attacks | Monitor kernel resources, rate limiting |

## WASM Extension System

Veil provides a WASM extension system fully compliant with Proxy-Wasm ABI v0.2.1. Proxy-Wasm modules created for Nginx/Envoy can be used with Veil without modification.

### Features

- **Proxy-Wasm v0.2.1 Compliant**: 100% compatible with Nginx/Envoy
- **AOT Compilation & Auto-Cache**: Modules are AOT-compiled; a `.cwasm` sidecar is generated next to each `.wasm` on first load and reused via `deserialize` on subsequent startups (invalidated automatically when the `.wasm` is newer or the wasmtime version changes; falls back to recompilation on any error). Explicit `.cwasm` paths are also supported.
- **Pooling Allocator**: High-speed instance creation
- **Async Execution (no Head-of-Line blocking)**: Modules run on wasmtime async support with fuel-based cooperative yielding (every ~10k instructions), so a CPU-heavy filter cannot stall the io_uring worker's other I/O
- **Capability Restrictions**: Fine-grained per-module permission control (all disabled by default)

### Build

```bash
cargo build --release --features wasm
```

### Configuration

```toml
[wasm]
enabled = true

# Default settings (optional)
[wasm.defaults]
# Maximum execution time (milliseconds, default: 100)
max_execution_time_ms = 100

  # Pooling allocator settings
  [wasm.defaults.pooling]
  # Total number of memory pools (default: 128)
  total_memories = 128
  # Total number of table pools (default: 128)
  total_tables = 128
  # Maximum memory size per instance (default: 10MB)
  max_memory_size = 10485760

# Module definition
[[wasm.modules]]
name = "my_filter"
path = "/etc/veil/wasm/my_filter.wasm"
configuration = '{"key": "value"}'

[wasm.modules.capabilities]
# All default to false, enable only required permissions
allow_logging = true
allow_request_headers_read = true
allow_request_headers_write = true
allow_send_local_response = true
allow_http_calls = true
allowed_upstreams = ["webdis"]  # Allowed HTTP call destinations
```

**Note**: To apply WASM modules to specific routes, use the `modules` field in the route configuration (see Routing section).

### Default Settings

The `[wasm.defaults]` section allows you to configure global WASM runtime settings:

| Option | Description | Default |
|--------|-------------|---------|
| `max_execution_time_ms` | Maximum execution time per WASM call (milliseconds) | 100 |

#### Pooling Allocator Settings

The `[wasm.defaults.pooling]` section configures the pooling allocator for high-speed instance creation:

| Option | Description | Default |
|--------|-------------|---------|
| `total_memories` | Total number of memory pools | 128 |
| `total_tables` | Total number of table pools | 128 |
| `max_memory_size` | Maximum memory size per instance (bytes) | 10MB (10485760) |

### Capability List

| Capability | Description | Default |
|-----------|-------------|---------|
| `allow_logging` | Log output | false |
| `allow_metrics` | Metrics operations | false |
| `allow_shared_data` | Shared data access | false |
| `allow_request_headers_read` | Read request headers | false |
| `allow_request_headers_write` | Modify request headers | false |
| `allow_request_body_read` | Read request body | false |
| `allow_request_body_write` | Modify request body | false |
| `allow_response_headers_read` | Read response headers | false |
| `allow_response_headers_write` | Modify response headers | false |
| `allow_response_body_read` | Read response body | false |
| `allow_response_body_write` | Modify response body | false |
| `allow_send_local_response` | Send local response | false |
| `allow_http_calls` | HTTP external calls | false |
| `allowed_upstreams` | Allowed upstreams | [] |

### Developing Extensions with Rust

#### 1. Create Project

```bash
cargo new --lib my-filter
cd my-filter
```

#### 2. Cargo.toml

```toml
[package]
name = "my-filter"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
proxy-wasm = "0.2"
log = "0.4"

[profile.release]
lto = true
opt-level = "s"

[workspace]
```

#### 3. src/lib.rs

```rust
use proxy_wasm::traits::*;
use proxy_wasm::types::*;

proxy_wasm::main! {{
    proxy_wasm::set_log_level(LogLevel::Debug);
    proxy_wasm::set_root_context(|_| -> Box<dyn RootContext> {
        Box::new(MyFilterRoot)
    });
}}

struct MyFilterRoot;

impl Context for MyFilterRoot {}

impl RootContext for MyFilterRoot {
    fn get_type(&self) -> Option<ContextType> {
        Some(ContextType::HttpContext)
    }

    fn create_http_context(&self, context_id: u32) -> Option<Box<dyn HttpContext>> {
        Some(Box::new(MyFilter { context_id }))
    }
}

struct MyFilter {
    context_id: u32,
}

impl Context for MyFilter {}

impl HttpContext for MyFilter {
    fn on_http_request_headers(&mut self, _: usize, _: bool) -> Action {
        // Add custom header to request
        self.add_http_request_header("X-My-Filter", "enabled");
        
        // Get header value
        if let Some(path) = self.get_http_request_header(":path") {
            log::info!("Request path: {}", path);
        }
        
        Action::Continue
    }

    fn on_http_response_headers(&mut self, _: usize, _: bool) -> Action {
        // Add response header
        self.add_http_response_header("X-Processed-By", "my-filter");
        Action::Continue
    }
}
```

#### 4. Build

```bash
# Add WASI target
rustup target add wasm32-wasip1

# Build
cargo build --target wasm32-wasip1 --release

# Output: target/wasm32-wasip1/release/my_filter.wasm
```

#### 5. Deploy and Configure

```bash
# Deploy WASM module
cp target/wasm32-wasip1/release/my_filter.wasm /etc/veil/wasm/

# Add configuration to config.toml
```

### External Service Integration (HTTP Calls)

Use Proxy-Wasm's `dispatch_http_call` to call external HTTP services (e.g., Webdis for Redis):

```rust
fn on_http_request_headers(&mut self, _: usize, _: bool) -> Action {
    // Access Redis via Webdis
    self.dispatch_http_call(
        "webdis",  // upstream name (defined in config.toml)
        vec![
            (":method", "GET"),
            (":path", "/GET/my_key"),
            (":authority", "webdis"),
        ],
        None,
        vec![],
        Duration::from_millis(50),
    ).unwrap();
    
    Action::Pause  // Wait for response
}

fn on_http_call_response(&mut self, _: u32, _: usize, body_size: usize, _: usize) {
    if let Some(body) = self.get_http_call_response_body(0, body_size) {
        // Process value from Redis
        log::info!("Redis response: {:?}", body);
    }
    self.resume_http_request();
}
```

## Prometheus Metrics

Export metrics such as request counts, latency, and body sizes in Prometheus format.

### Enabling

Prometheus metrics are **disabled** by default. They must be explicitly enabled in the `[prometheus]` section.

```toml
[prometheus]
enabled = true
```

> **Note**: Metrics are also disabled if the `[prometheus]` section itself does not exist.

### Configuration Options

| Option | Description | Default |
|--------|-------------|---------|
| `enabled` | Enable metrics endpoint | **false** |
| `path` | Metrics endpoint path | `/__metrics` |
| `allowed_ips` | Allowed IP/CIDR for access (array) | [] (all allowed) |

### Endpoint

```
GET /__metrics
```

Use the `path` option to change the endpoint path.

### Available Metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `veil_proxy_http_requests_total` | Counter | method, status, host | Total request count |
| `veil_proxy_http_request_duration_seconds` | Histogram | method, host | Request processing time (seconds) |
| `veil_proxy_http_request_size_bytes` | Histogram | - | Request body size |
| `veil_proxy_http_response_size_bytes` | Histogram | - | Response body size |
| `veil_proxy_http_active_connections` | Gauge | host | Active connection count |
| `veil_proxy_http_upstream_health` | Gauge | upstream, server | Upstream health status (1=healthy, 0=unhealthy) |
| `veil_proxy_cache_hits_total` | Counter | host | Total cache hit count |
| `veil_proxy_cache_misses_total` | Counter | host | Total cache miss count |
| `veil_proxy_cache_stores_total` | Counter | host, storage | Total cache store operations |
| `veil_proxy_cache_evictions_total` | Counter | reason | Total cache eviction count |
| `veil_proxy_cache_size_bytes` | Gauge | storage | Current cache size in bytes |
| `veil_proxy_cache_entries` | Gauge | storage | Current number of cache entries |
| `veil_proxy_buffering_used_total` | Counter | host | Total requests using buffering |
| `veil_circuit_breaker_open_total` | Counter | upstream | CB open event count |
| `veil_circuit_breaker_state` | Gauge | upstream | CB state (0=Closed, 1=Open, 2=HalfOpen) |
| `veil_retry_total` | Counter | upstream, result | Retry attempt count |
| `veil_outlier_ejected` | Gauge | upstream, server | Server ejection status (1=ejected) |
| `veil_connection_pool_size` | Gauge | upstream | Current connection pool size |
| `veil_connection_pool_hits_total` | Counter | upstream | Connection pool hit count |
| `veil_connection_pool_misses_total` | Counter | upstream | Connection pool miss count |
| `veil_grpc_requests_total` | Counter | method, status_code, upstream | gRPC request count |
| `veil_grpc_stream_duration_seconds` | Histogram | method | gRPC stream duration |
| `veil_wasm_filter_duration_seconds` | Histogram | filter, phase | WASM filter execution time |

### Runtime Enable/Disable

Prometheus metrics can be toggled at runtime without restarting:

```rust
// Internal API (used by admin/config reload)
veil::metrics::set_metrics_runtime_enabled(false);  // Disable → endpoint returns 404
veil::metrics::set_metrics_runtime_enabled(true);   // Re-enable
```

When disabled, the `/__metrics` endpoint returns `404 Not Found`. All recording functions become no-ops.

### Grafana Dashboard Examples

```promql
# Request rate (requests/second)
rate(veil_proxy_http_requests_total[5m])

# Error rate (4xx + 5xx)
sum(rate(veil_proxy_http_requests_total{status=~"4..|5.."}[5m])) 
  / sum(rate(veil_proxy_http_requests_total[5m]))

# Latency P95
histogram_quantile(0.95, rate(veil_proxy_http_request_duration_seconds_bucket[5m]))

# Request rate by host
sum by (host) (rate(veil_proxy_http_requests_total[5m]))
```

### Configuration Examples (config.toml)

```toml
# Basic configuration (accessible from all IPs)
[prometheus]
enabled = true
path = "/__metrics"

# Enhanced security (internal network only)
[prometheus]
enabled = true
path = "/metrics"
allowed_ips = [
  "127.0.0.1",
  "::1",
  "10.0.0.0/8",
  "172.16.0.0/12",
  "192.168.0.0/16"
]
```

### Access Control

When `allowed_ips` is configured, only the specified IP addresses/CIDRs can access the metrics endpoint.
When empty (default), all IPs can access.

| Format | Example |
|--------|---------|
| Single IPv4 | `127.0.0.1` |
| IPv4 CIDR | `10.0.0.0/8` |
| Single IPv6 | `::1` |
| IPv6 CIDR | `2001:db8::/32` |

### Prometheus Configuration Example

```yaml
# prometheus.yml
scrape_configs:
  - job_name: 'veil-proxy'
    static_configs:
      - targets: ['your-proxy-server:443']
    scheme: https
    tls_config:
      insecure_skip_verify: true  # For self-signed certificates
    metrics_path: /__metrics
```

## OpenTelemetry (OTLP/HTTP)

Push Prometheus metrics to any OTLP-compatible collector without the heavy OpenTelemetry SDK (fully tokio-free).

> **Requires**: `--features opentelemetry` (or `--features full`)

### Architecture

- A dedicated `std::thread` exports metrics on a configurable interval.
- Bridges the internal Prometheus registry to OTLP/HTTP JSON (`POST /v1/metrics`).
- Control messages (`Flush`, `Shutdown`) are sent via `std::sync::mpsc::channel` — no tokio involved.

### Configuration

```toml
[opentelemetry]
enabled = true
endpoint = "http://localhost:4318"   # OTLP/HTTP collector endpoint
service_name = "veil-proxy"          # service.name resource attribute
batch_interval_secs = 30             # Export interval (default: 30s)
```

### Configuration Options

| Option | Description | Default |
|--------|-------------|---------|
| `enabled` | Enable OTLP export | `false` |
| `endpoint` | OTLP/HTTP endpoint URL | `http://localhost:4318` |
| `service_name` | `service.name` resource attribute | `veil-proxy` |
| `batch_interval_secs` | Export interval (seconds) | `30` |

### Collector Compatibility

Any OTLP/HTTP collector accepting JSON payloads:

| Collector | Notes |
|-----------|-------|
| Grafana Alloy / Tempo | Set endpoint to `http://alloy:4318` |
| Jaeger (v1.35+) | Enable OTLP receiver |
| OpenTelemetry Collector | Standard OTLP/HTTP receiver |
| Prometheus Remote Write | Via otel-collector `prometheusremotewrite` exporter |

## Logging Configuration

Provides high-performance async logging using ftlog. ftlog internally uses a background thread and channel, minimizing impact on worker threads.

### Configuration Options

| Option | Description | Default |
|--------|-------------|---------|
| `level` | Log level (trace/debug/info/warn/error/off) | info |
| `format` | Log output format (text/json) | text |
| `channel_size` | Internal channel buffer size | 100000 |
| `flush_interval_ms` | Disk flush interval (milliseconds) | 1000 |
| `max_log_size` | Maximum log file size (bytes, 0=unlimited) | 104857600 |
| `file_path` | Log file path (defaults to stderr if unspecified) | none |

### Log Output Formats

#### Text Format (Default)

```
2024-01-01 00:00:00.000+00 0ms INFO main [main.rs:123] Server started
```

#### JSON Format

Suitable for integration with structured log collection systems (Elasticsearch, Loki, etc.).

```json
{"timestamp":"2024-01-01T00:00:00.000Z","level":"INFO","target":"veil","file":"main.rs","line":123,"message":"Server started"}
```

### Configuration Example

```toml
[logging]
level = "info"
format = "text"  # or "json"
channel_size = 100000
flush_interval_ms = 1000
file_path = "/var/log/veil.log"
```

### JSON Format Configuration Example

```toml
[logging]
level = "info"
format = "json"
file_path = "/var/log/veil.json"
```

## Structured Access Log

Write per-request access logs in JSON or text format, independent of the application log. Uses thread-local buffers to minimize heap allocation.

### Configuration

```toml
[access_log]
enabled = true
format = "json"                         # "json" or "text"
file_path = "/var/log/veil/access.log"  # omit for stderr
# Limit output fields (omit for all fields)
fields = ["timestamp", "method", "host", "path", "status", "duration_ms", "client_ip", "upstream"]
channel_size = 10000      # async channel capacity to the writer thread (default: 10000)
flush_interval_ms = 1000  # BufWriter flush interval in ms (default: 1000)
```

Access logs are written asynchronously by a dedicated writer thread. The hot path (worker thread) only pushes bytes into a bounded channel (`channel_size`). The writer thread holds the file/stderr handle exclusively, eliminating global lock contention. Log lines dropped when the channel is full are silently discarded without blocking request processing.

### Available Fields

| Field | Description |
|-------|-------------|
| `timestamp` | Request timestamp (RFC 3339) |
| `method` | HTTP method |
| `host` | Request Host header |
| `path` | Request path |
| `status` | HTTP response status code |
| `duration_ms` | Request duration in milliseconds |
| `client_ip` | Client IP address |
| `upstream` | Upstream server address |
| `req_body_size` | Request body size (bytes) |
| `resp_body_size` | Response body size (bytes) |
| `user_agent` | User-Agent header |

### Example JSON Output

```json
{"timestamp":"2026-01-01T00:00:00Z","method":"GET","host":"example.com","path":"/api/data","status":200,"duration_ms":12,"client_ip":"10.0.0.1","upstream":"192.168.1.10:8080","req_body_size":0,"resp_body_size":1024,"user_agent":"curl/8.0"}
```

## Cache Purge Administration API

Invalidate cached responses without restarting the proxy.

> **Requires**: `--features cache` and `[admin]` section in config

### Configuration

```toml
[admin]
enabled = true
path_prefix = "/__admin"    # Admin endpoint prefix
secret = "changeme"         # Bearer token for authentication
# allowed_ips = ["127.0.0.1", "::1", "10.0.0.0/8"]  # IP allowlist (empty = all IPs allowed)
```

### Purge Operations

All purge requests require `Authorization: Bearer <secret>` header.

| Method | Path | Query | Effect |
|--------|------|-------|--------|
| `PURGE` | any path | — | Purge exact cache entry matching path |
| `POST` | `/__admin/cache/purge` | `key=/path` | Purge exact key |
| `POST` | `/__admin/cache/purge` | `prefix=/api/` | Purge all entries with prefix |
| `POST` | `/__admin/cache/purge` | `pattern=/static/*.css` | Purge by glob pattern |
| `POST` | `/__admin/cache/purge` | `all=true` | Purge entire cache |

**Response**: `{"purged": N}` where N is the number of entries removed.

### Examples

```bash
# Purge a single page
PURGE https://proxy.example.com/blog/post-1 \
  -H "Authorization: Bearer changeme"

# Purge all API responses
curl -X POST "https://proxy.example.com/__admin/cache/purge?prefix=/api/" \
  -H "Authorization: Bearer changeme"

# Purge CSS files by glob
curl -X POST "https://proxy.example.com/__admin/cache/purge?pattern=/static/*.css" \
  -H "Authorization: Bearer changeme"

# Purge everything
curl -X POST "https://proxy.example.com/__admin/cache/purge?all=true" \
  -H "Authorization: Bearer changeme"
```

### Access Control

| Condition | Response |
|-----------|----------|
| Source IP not in `allowed_ips` | `403 Forbidden` |
| No `Authorization` header | `401 Unauthorized` |
| Wrong secret | `401 Unauthorized` |
| Admin disabled | `404 Not Found` |
| Success | `200 OK` with `{"purged": N}` |

IP filtering is checked before authentication. When `allowed_ips` is empty (default), all IPs are allowed.

| Format | Example |
|--------|---------|
| Single IPv4 | `127.0.0.1` |
| IPv4 CIDR | `10.0.0.0/8` |
| Single IPv6 | `::1` |
| IPv6 CIDR | `fe80::/10` |

## Admin API

The admin API exposes runtime management endpoints under a configurable prefix (default: `/__admin`). All endpoints require IP filtering and Bearer token authentication (see [Cache Purge Administration API](#cache-purge-administration-api) for configuration).

### Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/__admin/config` | Dump current config as JSON (secrets masked) |
| `GET` | `/__admin/stats` | Runtime stats (uptime, circuit breaker state) |
| `POST` | `/__admin/reload` | Trigger config hot-reload |
| `POST` | `/__admin/tls/reload` | Trigger TLS certificate hot-reload |
| `POST` | `/__admin/cache/purge` | Cache purge (see Cache Purge section) |
| `PURGE` | any path | Purge cache entry by path |

### Examples

```bash
# Get current config (secrets masked)
curl -H "Authorization: Bearer changeme" https://proxy.example.com/__admin/config

# Get runtime stats
curl -H "Authorization: Bearer changeme" https://proxy.example.com/__admin/stats
# → {"uptime_secs": 3600}

# Trigger config reload
curl -X POST -H "Authorization: Bearer changeme" https://proxy.example.com/__admin/reload
# → {"ok":true}

# Trigger TLS certificate reload
curl -X POST -H "Authorization: Bearer changeme" https://proxy.example.com/__admin/tls/reload
# → {"ok":true}
```

## Performance Tuning

### Worker Thread Count

Configure worker thread count in the `[server]` section of `config.toml`.

```toml
[server]
listen = "0.0.0.0:443"
threads = 0  # If unspecified or 0, uses same number as CPU cores
```

| Setting | Behavior |
|---------|----------|
| Unspecified | Same number of threads as CPU cores |
| `threads = 0` | Same number of threads as CPU cores |
| `threads = 4` | Start with 4 threads |

- Each worker thread is pinned to a CPU core (CPU affinity)
- If thread count exceeds core count, assigned round-robin
- Recommend setting lower in memory-constrained environments

### SO_REUSEPORT CBPF Load Balancing

#### Overview

When multiple worker threads listen on the same port using SO_REUSEPORT, the Linux kernel distributes connections by default using a 3-tuple hash (protocol + source IP + source port). In CBPF mode, a custom BPF program is attached to the kernel that selects workers based only on client IP address.

#### Effects

| Aspect | Kernel (default) | CBPF |
|--------|------------------|------|
| Distribution Key | protocol + src IP + src port | src IP only |
| Same Client | Varies by source port | Always same worker |
| CPU Cache Efficiency | Medium | High (improved L1/L2 hit rate) |
| TLS Session Resumption | Low-Medium | High (leverages session cache) |

#### Configuration

```toml
[performance]
# "kernel" = kernel default (backward compatibility)
# "cbpf"   = client IP-based CBPF (recommended)
reuseport_balancing = "cbpf"
```

#### Requirements

- **Linux 4.6 or higher** (SO_ATTACH_REUSEPORT_CBPF support)
- Automatically falls back to kernel default if CBPF attach fails

### Huge Pages (Large OS Pages)

#### Overview

Using Huge Pages (2MB) with the mimalloc allocator reduces TLB (Translation Lookaside Buffer) misses and improves performance.

#### Effects

| Aspect | Effect |
|--------|--------|
| TLB Misses | Significantly reduced (fewer page table lookups) |
| Page Faults | Reduced when using large amounts of memory |
| Performance | 5-10% improvement (workload dependent) |
| kTLS/splice | Especially effective with kernel integration |

#### Configuration

```toml
[performance]
huge_pages_enabled = true
```

#### OS-Level Configuration (Linux)

```bash
# Temporarily enable Huge Pages (128 pages = 256MB)
echo 128 | sudo tee /proc/sys/vm/nr_hugepages

# Persist (/etc/sysctl.conf)
echo "vm.nr_hugepages=128" | sudo tee -a /etc/sysctl.conf
sudo sysctl -p

# Check current Huge Pages status
grep -i huge /proc/meminfo
```

#### Container Environment Notes

In Docker/Kubernetes environments, Huge Pages must be reserved on the host side:

```bash
# Reserve Huge Pages on host
echo 128 | sudo tee /proc/sys/vm/nr_hugepages

# When starting Docker (optional)
docker run --shm-size=256m ...

# Kubernetes (add to Pod spec)
# resources.limits.hugepages-2Mi: "256Mi"
```

If Huge Pages are unavailable, automatically falls back to regular 4KB pages.

### System Configuration

```bash
# File descriptor limit
ulimit -n 65535

# Kernel parameters
sysctl -w net.core.somaxconn=65535
sysctl -w net.ipv4.tcp_max_syn_backlog=65535
sysctl -w net.core.netdev_max_backlog=65535

# io_uring settings (as needed)
sysctl -w kernel.io_uring_setup_flags=0
```

### Buffer Sizes and Timeouts

Constants in code (set at compile time, requires rebuild):

```rust
// Buffer sizes
const BUF_SIZE: usize = 65536;           // 64KB - optimal size for io_uring
const HEADER_BUF_CAPACITY: usize = 512;  // For HTTP headers
const MAX_HEADER_SIZE: usize = 8192;     // 8KB - header size limit
const MAX_BODY_SIZE: usize = 10485760;   // 10MB - body size limit

// Timeouts
const READ_TIMEOUT: Duration = Duration::from_secs(30);   // Read timeout
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);  // Write timeout
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10); // Backend connection timeout
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);   // Keep-Alive idle timeout
```

> **Note**: Some timeouts can be individually adjusted from config.toml via per-route security settings using `client_header_timeout_secs` and `backend_connect_timeout_secs`.

### Buffer Pool Configuration

The buffer pool reduces memory allocation overhead by pre-allocating buffers at startup. Configure in the `[buffer_pool]` section:

```toml
[buffer_pool]
# Read buffer size (bytes)
# Default: 65536 (64KB)
read_buffer_size = 65536

# Initial number of read buffers in pool
# Default: 32
initial_read_buffers = 32

# Maximum number of read buffers in pool
# Default: 128
max_read_buffers = 128

# Request construction buffer size (bytes)
# Default: 1024 (1KB)
request_buffer_size = 1024

# Initial number of request buffers in pool
# Default: 16
initial_request_buffers = 16

# Large request buffer size (bytes)
# Default: 4096 (4KB)
large_request_buffer_size = 4096

# Path string buffer size (bytes)
# Default: 256

# Response header buffer size (bytes)
# Default: 512
```

**Note**: Buffer pool configuration is optional. Default values are optimized for most use cases. Adjust only if you have specific memory constraints or performance requirements.

## Benchmarking

```bash
# Benchmark using wrk
wrk -t4 -c100 -d30s https://localhost/

# Comparison with kTLS enabled/disabled

# 1. kTLS disabled (using rustls)
cargo build --release
./veil -c ./config.toml &
wrk -t4 -c100 -d30s https://localhost/

# 2. kTLS enabled (using rustls + custom kTLS module)
cargo build --release --features ktls
# Set ktls_enabled = true in config.toml
./veil -c ./config.toml &
wrk -t4 -c100 -d30s https://localhost/
```

## Testing

Veil includes comprehensive test suites covering unit tests, integration tests, and end-to-end (E2E) tests.

### Test Overview

| Test Type | Count | Status |
|-----------|-------|--------|
| **Unit Tests** | 469 | ✅ All passing |
| **Integration Tests** | 12 | ✅ All passing |
| **E2E Tests** | 23 | ✅ All passing |
| **Benchmarks** | 12 files | ✅ Ready |

**Total: 504 tests - All passing ✅**

### Running Tests

#### Unit Tests

```bash
# Run all unit tests
cargo test --features full --bin veil

# Run specific test module
cargo test --features full --bin veil wasm::tests

# Run with output
cargo test --features full --bin veil -- --nocapture
```

#### Integration Tests

```bash
# Run integration tests
cargo test --test integration_tests --features full
```

#### E2E Tests

E2E tests require a running test environment. Use the setup script:

```bash
# Method 1: Automated (recommended)
./tests/e2e_setup.sh test

# Method 2: Manual
./tests/e2e_setup.sh start
cargo test --test e2e_tests --features full -- --test-threads=1
./tests/e2e_setup.sh stop

# Cleanup only
./tests/e2e_setup.sh clean
```

#### Benchmarks

```bash
# Start E2E environment
./tests/e2e_setup.sh start

# Run all benchmarks
cargo bench --features full

# Run specific benchmark
cargo bench --bench throughput --features full
cargo bench --bench latency --features full

# WASM filter overhead (requires the proxy started with the WASM route, e.g. via e2e_setup).
# Compares an identical request through a WASM-filtered route (/wasm/*) vs a plain route (/);
# the keep-alive group amortizes connection cost to isolate the per-request filter overhead.
# Expected order: a few µs to tens of µs per request for a header filter (machine/wasmtime
# dependent). Measure RSS separately with `/usr/bin/time -v`.
cargo bench --bench wasm --features wasm

# Stop environment
./tests/e2e_setup.sh stop

# Or use automated script
./tests/run_bench.sh          # All benchmarks
./tests/run_bench.sh throughput  # Throughput only
./tests/run_bench.sh latency     # Latency only
```

### Test Coverage

#### Unit Tests (469 tests)

- **CIDR/IP Filtering**: IP address filtering, CIDR range validation
- **Rate Limiting**: Sliding window rate limiting, entry management
- **Configuration Parsing**: TOML parsing, default values
- **Load Balancing**: Round Robin, Least Connections, IP Hash algorithms
- **Health Checks**: Server state management, success/failure counting
- **Connection Pooling**: Pool management, timeout validation
- **Cache Management**: Memory/disk cache, key generation
- **HTTP/2**: Frame encoding/decoding, HPACK compression
- **Security**: Security configuration, kernel version detection
- **WASM**: Proxy-Wasm ABI, filter lifecycle, host function callbacks
- **Utilities**: Various helper functions

#### Integration Tests (12 tests)

- TCP connection handling
- HTTP server responses
- Multiple server coordination
- Dynamic port allocation
- TLS certificate generation
- Configuration file generation
- Port availability utilities

#### E2E Tests (23 tests)

- **Proxy Core**: Basic requests, health endpoints
- **Header Manipulation**: Add/remove headers, backend ID
- **Load Balancing**: Round Robin distribution
- **Static File Serving**: Index files, large files
- **Compression**: gzip, brotli, priority handling
- **Backend Access**: Direct backend connections
- **Prometheus**: Metrics endpoint
- **Error Handling**: 404 responses
- **HTTP Redirect**: HTTP to HTTPS redirect
- **Concurrency**: Concurrent and sequential requests
- **Performance**: Response time validation
- **Content Types**: HTML, JSON handling
- **Keep-Alive**: Persistent connections
- **Custom Headers**: User-Agent, Host headers

### Environment Cleanup

All test environments are automatically cleaned up:

- **Rust Drop Traits**: Server structs automatically terminate on scope exit
- **Shell Script Traps**: Cleanup on success, failure, or interruption
- **Graceful Shutdown**: SIGTERM → wait → SIGKILL staged termination
- **Process Cleanup**: Automatic cleanup of remaining processes

The cleanup mechanism ensures a clean state after test execution, regardless of test outcome.

### Test Files Structure

```
veil-proxy/
├── src/
│   ├── main.rs          # 103 unit tests
│   ├── security.rs      # 26 unit tests
│   ├── cache/           # 50+ unit tests
│   ├── http2/           # 30+ unit tests
│   └── ...
├── tests/
│   ├── integration_tests.rs  # 13 integration tests
│   ├── e2e_tests.rs          # 24 E2E tests
│   ├── e2e_setup.sh         # E2E environment setup
│   ├── run_bench.sh         # Benchmark automation
│   └── common/
│       └── mod.rs            # Test utilities
└── benches/
    ├── throughput.rs      # Throughput benchmarks
    ├── latency.rs         # Latency benchmarks
    ├── http2.rs           # HTTP/2 benchmarks
    ├── http3.rs           # HTTP/3 benchmarks
    ├── tls.rs             # TLS benchmarks
    ├── compression.rs     # Compression benchmarks
    ├── connection_pool.rs # Connection pool benchmarks
    ├── cache.rs           # Cache benchmarks
    ├── load_balancing.rs  # Load balancing benchmarks
    ├── websocket.rs       # WebSocket benchmarks
    ├── memory.rs          # Memory usage benchmarks
    └── routing.rs         # Routing benchmarks
```

### Continuous Integration

For CI/CD pipelines:

```yaml
# Example GitHub Actions workflow
- name: Run tests
  run: |
    cargo test --features http2 --all-targets
    
- name: Run E2E tests
  run: |
    ./tests/e2e_setup.sh test
```

## Configuration File Validation

Performs detailed validation of the configuration file at startup and outputs clear error messages if problems are found.

### Validation Items

| Item | Check Content |
|------|---------------|
| TLS Certificate | File existence check |
| TLS Private Key | File existence check |
| Listen Address | Valid socket address format |
| Upstream URL | Valid URL format |
| Proxy URL | Valid URL format |
| File Path | File/directory existence check |
| File Mode | `sendfile` or `memory` |

### Error Message Examples

```
Error: TLS certificate file not found: /path/to/cert.pem
Error: Invalid proxy URL for route 'example.com:/api/': invalid-url
Error: Upstream 'backend-pool' not found
```

## Graceful Shutdown

When receiving SIGINT (Ctrl+C) or SIGTERM, the server terminates safely:

1. Stop accepting new connections
2. Complete processing of existing requests
3. Wait for all worker threads to finish
4. Terminate process

```bash
# Start server
./veil -c ./config.toml &

# Terminate safely
kill -SIGTERM $!
# or Ctrl+C
```

## Graceful Reload (Hot Reload)

When receiving SIGHUP, the server reloads the configuration file.
Existing connections are not interrupted, and new settings apply to new connections.

### Behavior

1. Receive SIGHUP signal
2. Reload config file specified at startup
3. Validate configuration
4. Lock-free configuration update via `ArcSwap`
5. New connections use new settings

> **Note**: On reload, the path specified with `-c` option at startup (or default `/etc/veil/config.toml`) is used.

```bash
# Edit config file
vim config.toml

# Reload configuration (zero downtime)
kill -SIGHUP $(pgrep veil)
```

### Supported Changes

| Item | Hot Reload Supported |
|------|---------------------|
| Routing configuration | ✅ |
| Security configuration | ✅ |
| Upstream configuration | ✅ |
| TLS certificates | ❌ |
| Listen address | ❌ (requires restart) |
| Worker thread count | ❌ (requires restart) |

## Self-Sandboxing

This server has built-in **self-isolation from within the code** without using external tools like bubblewrap.

### Why In-Code Implementation Instead of External Tools?

| Approach | Pros | Cons |
|----------|------|------|
| bubblewrap (external) | Flexible configuration, existing tool | Additional dependency, configuration complexity |
| **This server (built-in)** | Zero dependencies, declared in code, automatic inheritance | Linux kernel dependent |

### Implemented Self-Isolation Features

#### 1. Landlock Filesystem Restriction (Linux 5.13+)

Process can declare "from now on, I will only access these directories."

```toml
[security]
enable_landlock = true
landlock_read_paths = ["/etc/veil", "/usr", "/lib", "/lib64"]
landlock_write_paths = ["/var/log/veil"]
```

**Supported ABI Versions:**

| ABI | Kernel | Features |
|-----|--------|----------|
| v1 | 5.13+ | Basic filesystem access control |
| v2 | 5.19+ | File reference permission (REFER) |
| v3 | 6.2+ | TRUNCATE permission |
| v4 | 6.7+ | ioctl permission |

#### 2. seccomp System Call Restriction

Restricts system calls based on an allow list.

```toml
[security]
enable_seccomp = true
seccomp_mode = "filter"  # "log" / "filter" / "strict"
```

**Recommended Deployment Procedure:**

```bash
# 1. First verify with log mode
enable_seccomp = true
seccomp_mode = "log"

# 2. Check blocked system calls
journalctl -f | grep -i seccomp

# 3. Switch to filter mode if no issues
seccomp_mode = "filter"
```

#### 3. Privilege Dropping

After starting as root and creating listeners, drop to unprivileged user.

```toml
[security]
drop_privileges_user = "veil"
drop_privileges_group = "veil"
```

### About Namespace Isolation

> **Note**: Namespace isolation like `unshare(CLONE_NEWNET)` is **not recommended** for reverse proxies.
> Isolating the network namespace will break proxy functionality.
> 
> If namespace isolation is required, we recommend doing it at the **systemd level** (see below).

## Security Hardening (systemd Sandboxing)

io_uring is a powerful async I/O interface, but if exploited, it poses a risk of kernel privilege escalation.
This server can achieve robust security when combined with systemd's sandboxing features.

### Security Architecture (Defense in Depth)

```
┌─────────────────────────────────────────────────────────────────┐
│ systemd (PID 1) - Outer isolation layer                         │
│ ┌─────────────────────────────────────────────────────────────┐ │
│ │ Namespace isolation (ProtectSystem, PrivateTmp, PrivateDevices) │ │
│ │ ┌─────────────────────────────────────────────────────────┐ │ │
│ │ │ veil built-in security                                  │ │ │
│ │ │ ┌─────────────────────────────────────────────────────┐ │ │ │
│ │ │ │ Landlock (filesystem restriction)                   │ │ │ │
│ │ │ │ ┌─────────────────────────────────────────────────┐ │ │ │ │
│ │ │ │ │ seccomp (system call restriction)               │ │ │ │ │
│ │ │ │ │ ┌─────────────────────────────────────────────┐ │ │ │ │ │
│ │ │ │ │ │ Application (io_uring + rustls)             │ │ │ │ │ │
│ │ │ │ │ │ - Allow: io_uring_*, socket, read, write... │ │ │ │ │ │
│ │ │ │ │ │ - Deny: fork, execve, ptrace, mount...      │ │ │ │ │ │
│ │ │ │ │ └─────────────────────────────────────────────┘ │ │ │ │ │
│ │ │ │ └─────────────────────────────────────────────────┘ │ │ │ │
│ │ │ └─────────────────────────────────────────────────────┘ │ │ │
│ │ └─────────────────────────────────────────────────────────┘ │ │
│ └─────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

### Required System Calls

Minimum system calls required for this server to operate:

| Category | System Calls | Purpose |
|----------|--------------|---------|
| **io_uring** | `io_uring_setup`, `io_uring_enter`, `io_uring_register` | monoio runtime |
| **Network** | `socket`, `bind`, `listen`, `accept4`, `connect`, `sendto`, `recvfrom`, `sendmsg`, `recvmsg`, `setsockopt`, `getsockopt` | TCP/UDP sockets |
| **File I/O** | `openat`, `read`, `write`, `close`, `fstat`, `readv`, `writev` | Config, certificates, logs |
| **Memory** | `mmap`, `munmap`, `mprotect`, `brk`, `madvise`, `mremap`, `mlock`, `mlock2` | mimalloc, Huge Pages, io_uring registered buffers |
| **Threads** | `clone`, `clone3`, `futex`, `exit_group`, `set_tid_address` | Worker threads |
| **CPU Affinity** | `sched_setaffinity`, `sched_getaffinity` | CPU pinning |
| **Signals** | `rt_sigaction`, `rt_sigprocmask`, `rt_sigreturn` | SIGTERM/SIGHUP |
| **Time** | `clock_gettime`, `nanosleep` | Timeouts |
| **Other** | `prctl`, `ioctl`, `getrandom`, `fcntl`, `uname` | Various control |

### systemd Service File

A sandbox-enabled service file is provided at `contrib/systemd/veil.service`.

#### Installation

```bash
# 1. Create dedicated user
sudo useradd -r -s /sbin/nologin veil

# 2. Create directories
sudo mkdir -p /etc/veil
sudo mkdir -p /var/log/veil
sudo chown veil:veil /var/log/veil

# 3. Copy configuration files
sudo cp config.toml /etc/veil/
sudo cp server.crt server.key /etc/veil/
sudo chmod 600 /etc/veil/server.key
sudo chown -R veil:veil /etc/veil

# 4. Install binary
sudo cp target/release/veil /usr/local/bin/

# 5. Install service file
sudo cp contrib/systemd/veil.service /etc/systemd/system/
sudo systemctl daemon-reload

# 6. Enable and start service
sudo systemctl enable veil
sudo systemctl start veil
```

#### Important Configuration Items

```ini
[Service]
# === User ===
User=veil
Group=veil

# === Resource Limits ===
# io_uring registered buffers require memory lock
LimitMEMLOCK=infinity
LimitNOFILE=1048576

# === Filesystem Isolation ===
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ReadOnlyPaths=/etc/veil
ReadWritePaths=/var/log/veil

# === Namespace Isolation ===
RestrictNamespaces=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectKernelTunables=yes

# === Network ===
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK

# === Security Hardening ===
NoNewPrivileges=yes
MemoryDenyWriteExecute=yes
RestrictSUIDSGID=yes

# === System Call Restriction ===
# @system-service + io_uring + mlock
SystemCallFilter=@system-service
SystemCallFilter=io_uring_setup io_uring_enter io_uring_register
SystemCallFilter=mlock mlock2 mlockall munlock munlockall
SystemCallFilter=sched_setaffinity sched_getaffinity
SystemCallErrorNumber=EPERM
```

### Enabling Huge Pages

To maximize io_uring and mimalloc performance, enable Huge Pages.

```bash
# 1. Reserve Huge Pages (128 * 2MB = 256MB)
echo 128 | sudo tee /proc/sys/vm/nr_hugepages

# 2. Persist
echo "vm.nr_hugepages=128" | sudo tee -a /etc/sysctl.d/99-veil.conf
sudo sysctl -p /etc/sysctl.d/99-veil.conf

# 3. Remove MEMLOCK limit in systemd
# Set LimitMEMLOCK=infinity in veil.service
```

### Security Verification

How to verify the service's security state:

```bash
# Verify configuration with systemd-analyze
systemd-analyze security veil.service

# Check running security state
cat /proc/$(pgrep veil)/status | grep -E "Seccomp|NoNewPrivs|CapBnd"

# Expected output:
# Seccomp:        2                    # seccomp filter enabled
# NoNewPrivs:     1                    # Cannot gain new privileges
# CapBnd:         0000000000000c00     # Only CAP_NET_BIND_SERVICE
```

### Troubleshooting

#### io_uring Not Working

```bash
# Cause: System calls being blocked
# Solution: Add io_uring_* to SystemCallFilter
journalctl -u veil | grep -i "seccomp"

# Manual test
sudo strace -f -e trace=io_uring_setup /usr/local/bin/veil -c /etc/veil/config.toml
```

#### Memory Lock Failure

```bash
# Cause: MEMLOCK limit too low
# Solution: Set LimitMEMLOCK=infinity
cat /proc/$(pgrep veil)/limits | grep "locked memory"
```

#### Cannot Bind to Privileged Ports (443/80)

```bash
# Cause: Missing CAP_NET_BIND_SERVICE
# Solution 1: Configure in systemd
#   AmbientCapabilities=CAP_NET_BIND_SERVICE

# Solution 2: Grant capability to binary
sudo setcap 'cap_net_bind_service=+ep' /usr/local/bin/veil
```

### Alternative: Using with bubblewrap

For stricter isolation, combine systemd with bubblewrap:

```ini
[Service]
ExecStart=/usr/bin/bwrap \
    --ro-bind /usr /usr \
    --ro-bind /lib /lib \
    --ro-bind /lib64 /lib64 \
    --ro-bind /etc/veil /etc/veil \
    --bind /var/log/veil /var/log/veil \
    --unshare-pid \
    --die-with-parent \
    /usr/local/bin/veil -c /etc/veil/config.toml
```

In this configuration, systemd creates the outer "container" and bubblewrap provides an even stricter filesystem view.

## Panic Recovery

Veil implements connection-level panic catching to ensure high availability.

### Behavior

When a panic occurs during request processing:

| Scenario | Impact |
|----------|--------|
| **Without panic recovery** | Worker thread crashes, all connections on that worker are terminated |
| **With Veil's panic recovery** | Only the affected connection terminates, other connections continue normally |

### Implementation

- Uses `std::panic::catch_unwind` to wrap each connection's async task
- Panics are caught at the poll level and logged as errors
- `ConnectionGuard` ensures the connection counter is correctly decremented even on panic
- Worker threads remain alive and continue accepting new connections

### Logged Output

When a panic is caught:
```
[ERROR] Task panicked during poll: Any { .. }
```

### Notes

- This feature is automatically enabled; no configuration required
- Only protects against panics inside `monoio::spawn` tasks
- Panics in the accept loop or runtime initialization still terminate the worker thread

## References

### Core Libraries

- [monoio](https://github.com/bytedance/monoio): io_uring-based async runtime
- [rustls](https://github.com/rustls/rustls): Pure Rust TLS implementation
- [kTLS (custom)](https://docs.kernel.org/networking/tls.html): Custom kernel TLS module implemented in `src/ktls.rs` and `src/ktls_rustls.rs`
- [httparse](https://crates.io/crates/httparse): Fast HTTP parser
- [quiche](https://github.com/cloudflare/quiche): Cloudflare's QUIC/HTTP/3 implementation

### Performance

- [mimalloc](https://github.com/microsoft/mimalloc): Fast general-purpose memory allocator
- [matchit](https://crates.io/crates/matchit): Fast Radix Tree router
- [ftlog](https://crates.io/crates/ftlog): High-performance async logging library
- [memchr](https://crates.io/crates/memchr): SIMD-optimized string search
- [Linux Huge Pages](https://docs.kernel.org/admin-guide/mm/hugetlbpage.html): Large OS Pages configuration guide

### Monitoring

- [prometheus](https://crates.io/crates/prometheus): Prometheus metrics library

### CLI & Concurrency

- [clap](https://crates.io/crates/clap): Command line argument parser
- [arc-swap](https://crates.io/crates/arc-swap): Lock-free Arc swapping (for config hot reload)
- [ctrlc](https://crates.io/crates/ctrlc): Signal handling (for Graceful Shutdown)
- [signal-hook](https://crates.io/crates/signal-hook): SIGHUP handling (for Graceful Reload)
- [core_affinity](https://crates.io/crates/core_affinity): CPU affinity configuration

### Kernel Features

- [Linux Kernel TLS](https://docs.kernel.org/networking/tls.html): kTLS documentation
- [io_uring](https://kernel.dk/io_uring.pdf): io_uring design document
- [SO_REUSEPORT](https://lwn.net/Articles/542629/): Port sharing and load balancing

### Security

- [systemd.exec](https://www.freedesktop.org/software/systemd/man/systemd.exec.html): systemd security settings
- [seccomp](https://docs.kernel.org/userspace-api/seccomp_filter.html): Seccomp BPF filter
- [Landlock](https://docs.kernel.org/userspace-api/landlock.html): Filesystem sandbox
- [io_uring Security](https://www.kernel.org/doc/html/latest/userspace-api/io_uring.html): io_uring security considerations
- [bubblewrap](https://github.com/containers/bubblewrap): Unprivileged sandboxing tool

### WASM Extensions

- [Proxy-Wasm](https://github.com/proxy-wasm/spec): Proxy-Wasm ABI Specification
- [Wasmtime](https://wasmtime.dev/): WebAssembly Runtime
- [proxy-wasm-rust-sdk](https://github.com/proxy-wasm/proxy-wasm-rust-sdk): Rust SDK

## Logos

<table align="center">
  <tr>
    <th align="center">Main Logo (WebP)</th>
    <th align="center">Alternative Logo (SVG)</th>
    <th align="center">Logo Text (SVG)</th>
  </tr>
  <tr>
    <td align="center">
      <img src="docs/images/veil_logo.webp" alt="Veil Main Logo" width="200" />
    </td>
    <td align="center">
      <img src="docs/images/veil_logo_alternative.svg" alt="Veil Alternative Logo" width="200" />
    </td>
    <td align="center">
      <img src="docs/images/veil_logo_text.svg" alt="Veil Logo Text" width="200" />
    </td>
  </tr>
</table>

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.

(c) 2025 aofusa
