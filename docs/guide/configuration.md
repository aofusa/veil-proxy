# Configuration Reference

[← Documentation index](README.md) · [日本語](ja/configuration.md)

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
| `[server]` | `listen` | none | TLS listener (HTTP/1.1 + h2 ALPN); at least one of `listen` / `listen_cleartext` is required (see [Listeners](#listeners-listen-and-listen_cleartext)) |
| `[server]` | `listen_cleartext` | none | Plaintext listener (HTTP/1.1 + h2c, auto-detected) |
| `[server]` | `enable_secure_redirect` | `true` | Answer every `listen_cleartext` request with a 301 to HTTPS (needs `listen`; `false` serves plaintext) |
| `[server]` | `allow_http` | `true` | `listen_cleartext` accepts plaintext HTTP/1.1 (else 426) |
| `[server]` | `allow_h2c` | `true` | `listen_cleartext` accepts h2c (else GOAWAY) |
| `[http3]` | `enabled` | `false` | Enable HTTP/3 on `listen`'s address (UDP), or on `[http3].listen` |
| `[logging]` | `level` | `"info"` | Log level |
| `[logging]` | `format` | `"text"` | Log format |
| `[logging]` | `channel_size` | `16384` | Log channel buffer size (slots are preallocated and resident, ~88 bytes each; full → messages are dropped, never blocking) |
| `[logging]` | `flush_interval_ms` | `1000` | Flush interval (ms) |
| `[logging]` | `app_file_path` | none (stdout) | App log (INFO/WARN/DEBUG) output path |
| `[logging]` | `error_file_path` | none (stderr) | Error log (ERROR) output path |
| `[vds]` | `enabled` | `false` | Receive routes/upstreams from a control plane (see [VDS](vds.md)) |
| `[acme]` | `enabled` | `false` | Automatic certificate issuance/renewal (see [ACME](acme.md)) |
| `[prometheus]` | `enabled` | `false` | Enable Prometheus metrics |
| `[prometheus]` | `path` | `"/__metrics"` | Metrics endpoint path |
| `[performance]` | `reuseport_balancing` | `"kernel"` | SO_REUSEPORT balancing |
| `[performance]` | `huge_pages_enabled` | `false` | Enable Huge Pages |
| `[performance]` | `keepalive_direct_read_ms` | `20` | io_uring, plaintext HTTP/1.1 keep-alive: read the next request directly for this long before waiting without a buffer (`0` = always wait first; F-205) |
| `[performance]` | `open_file_cache_enabled` | `false` | Enable OpenFileCache |
| `[performance]` | `open_file_cache_valid_duration_secs` | `60` | Cache validity (seconds) |
| `[performance]` | `open_file_cache_max_entries` | `10000` | Max cache entries |
| `[static_file_cache]` | `enabled` | `false` | Enable static content cache (F-146/F-150; HTTP/2/HTTP/3, and HTTP/1.1 when kTLS is disabled) |
| `[static_file_cache]` | `valid_duration_secs` | `60` | Cache validity (seconds) |
| `[static_file_cache]` | `max_entries` | `1024` | Max cache entries |
| `[static_file_cache]` | `max_file_size_bytes` | `1048576` | Max file size to cache (bytes) |
| `[static_file_cache]` | `max_total_bytes` | `67108864` | Max total cached bytes |
| `[static_file_cache]` | `revalidate_mtime` | `false` | Re-stat mtime on hit (true reintroduces a syscall on the hot path) |
| `[static_file_cache]` | *(no separate key)* | — | F-169: when this section is enabled, compressed variants of File-route static responses (per path/encoding/level) are also cached, reusing this section's `max_entries`/`max_total_bytes`; invalidated together with the body cache entry |
| `[tls]` | `ktls_enabled` | `false` | Enable kTLS |
| `[tls]` | `ktls_fallback_enabled` | `true` | kTLS fallback to rustls |
| `[tls]` | `tcp_cork_enabled` | `true` | Enable TCP_CORK |
| `[tls]` | `cipher_suites` | `[]` (rustls default) | Allowed TLS cipher suites (like nginx `ssl_ciphers`; listed order = server preference; unknown names fail at startup; see examples/config.toml) |
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
| `[http3]` | `cc_algorithm` | `"bbr"` | QUIC congestion control (`reno`/`cubic`/`bbr`/`bbr2`/`bbr2_gcongestion`) |
| `[http3]` | `pacing` | `true` | Enable packet pacing |
| `[http3]` | `max_pacing_rate` | *(none)* | Max pacing rate (bytes/s); omit for unlimited |
| `[http3]` | `hystart` | `true` | Enable HyStart++ |
| `[http3]` | `mmsg_batch_size` | `64` | UDP mmsg / io_uring pipelined RECVMSG/SENDMSG batch width (1..=128) |
| `[http3]` | `recv_drain_max` | `64` | **reactor backends only** (FreeBSD/OpenBSD/NetBSD/macOS, and Linux `--features epoll`): max UDP datagrams drained per event-loop iteration (1..=4096). Larger values amortize the per-iteration fixed cost (select/timer round-trip + connection sweep) over more datagrams — F-151 measured a 3.2x throughput swing from this quantity alone — at the cost of delaying sends/timeouts/backend wakeups (p99 latency). The Linux io_uring default backend uses the `mmsg_batch_size` RECVMSG pipeline instead and ignores this key |
| `[http3]` | `compression_enabled` | `false` | Enable compression |
| `[http3]` | `gso_gro_enabled` | `false` | Enable GSO/GRO |
| `[http3]` | `alt_svc_enabled` | `true` | Advertise HTTP/3 via Alt-Svc on H1/H2 responses (when `[http3] enabled = true`) |
| `[http3]` | `alt_svc` | _(auto)_ | Override Alt-Svc header value (default: `h3=":PORT"; ma=…` from listen port) |
| `[http3]` | `alt_svc_ma_secs` | `86400` | max-age for auto-generated Alt-Svc |

Configuration file example (`examples/config.toml`):

```toml
[server]
# TLS listener: HTTP/1.1 + HTTP/2 (ALPN). Requires [tls]. Plaintext is never accepted here.
listen = "0.0.0.0:443"
# Unix domain socket listeners (Unix only)
# [server].listen and [server].listen_cleartext also accept "unix:<path>", e.g.
#   listen = "unix:/run/veil/https.sock"
# Listener side is limited to these two: [[l4]].listen and [http3].listen
# are not supported (HTTP/3 is QUIC/UDP and cannot run over a unix socket — set
# [http3].listen explicitly).
# Upstream (backend) connections over UDS ARE supported — see "Unix domain socket
# backends" below.
# Windows rejects unix: addresses at config validation time.
# AF_UNIX has no SO_REUSEPORT, so veil binds once at startup and every worker dup(2)s
# that fd; the kernel spreads accepts across them. A stale socket file is unlinked
# automatically (a regular file or directory at that path is an error instead).
# The peer address of a UDS connection is reported as the placeholder 127.0.0.1:0
# (IP blocklist and access logs see this value).
# unix_socket_permissions = "0660"   # mode applied to the socket file (octal string)
# Plaintext listener: HTTP/1.1 + h2c, auto-detected (optional, F-200)
# By default (enable_secure_redirect = true) every request on this port is answered with a
# 301 to https://<Host>[:<listen port>]. See "Listeners" below.
listen_cleartext = "0.0.0.0:80"
# enable_secure_redirect = true    # false: actually serve plaintext (needed when there is no `listen`)
# allow_http = true                # accept plaintext HTTP/1.1 on listen_cleartext (else 426)
# allow_h2c = true                 # accept h2c on listen_cleartext (else GOAWAY)
# Number of worker threads (optional)
# If unspecified or 0, uses the same number of threads as CPU cores
threads = 4
# Enable HTTP/2 (only when built with --features http2)
http2_enabled = true
# HTTP/3 is enabled with [http3] enabled = true (only when built with --features http3)
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
channel_size = 16384
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
# The allowlist includes faccessat2 (issued by glibc 2.33+/musl for access()/faccessat());
# without it, file resolution fails with EPERM under seccomp and static serving returns 404.
enable_seccomp = true
seccomp_mode = "filter"  # "disabled" / "log" / "filter" / "strict"

# Landlock filesystem restriction (Linux 5.13+)
enable_landlock = true
landlock_read_paths = ["/etc/veil", "/usr", "/lib", "/lib64"]
landlock_write_paths = ["/var/log/veil"]

[performance]
# SO_REUSEPORT distribution method
# "kernel" = kernel default (3-tuple hash) [default]
# "cbpf"   = flow hash-based CBPF (4-tuple; pins each connection to a fixed
#            worker for cache/session-reuse efficiency, requires Linux 4.6+)
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
#   - Keeps the opened file descriptor of each regular file, like nginx's
#     open_file_cache, so HTTP/1.1 static responses need no open/close per request
#
# Notes:
#   - File change detection may be delayed up to 60 seconds (default)
#   - Symbolic link changes may be delayed
#   - A file replaced or deleted within the validity period keeps being served from
#     the cached descriptor until the entry expires (same as nginx)
#   - Uses one file descriptor per cached regular file (up to max_entries)
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
ktls_enabled = true         # Enable kTLS (Linux 5.15+ or FreeBSD 13.0+, requires feature flag; F-126)
                            # On FreeBSD **without hardware kTLS offload, set this to false** (F-155):
                            # software kTLS dispatches every 16KB TLS record to a kernel worker
                            # thread, serializing throughput. Disabling it flipped the 54KB
                            # veil/nginx ratio from 0.51 to 1.09 (HTTP/1.1) and 0.56 to 1.16 (HTTP/2)
                            # on the same VM. See docs/perf/README.md.
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
# Static content cache configuration (route-specific, overrides global setting).
# HTTP/2/HTTP/3 only — HTTP/1.1 already uses sendfile(2) and is unaffected.
[route.static_file_cache]
enabled = true
valid_duration_secs = 300
max_file_size_bytes = 2097152  # 2 MiB

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

## Listeners (`listen` and `listen_cleartext`)

Listening is configured by two listeners and HTTP/3 (F-200). At least one of `listen` and `listen_cleartext` is
required, and they must not use the same address (a single port no longer auto-detects TLS vs. plaintext).

| Key | Protocols | Requires |
|-----|-----------|----------|
| `[server].listen` | TLS: HTTP/1.1 + HTTP/2 (ALPN). Plaintext is rejected by the TLS handshake | `[tls]` |
| `[server].listen_cleartext` | Plaintext: HTTP/1.1 + h2c (prior knowledge), told apart by protocol detection (`MSG_PEEK`) | `listen` if `enable_secure_redirect = true`; the `http2` feature for h2c |
| `[http3] enabled = true` | HTTP/3 (UDP) on `listen`'s address, or on `[http3].listen` if set | `listen`, `[tls]`, the `http3` feature |

`listen_cleartext` keys (all under `[server]`):

| Key | Default | Meaning |
|-----|---------|---------|
| `allow_http` | `true` | Accept plaintext HTTP/1.1. When `false`, HTTP/1.1 gets `426 Upgrade Required` |
| `allow_h2c` | `true` | Accept h2c. When `false`, h2c gets `GOAWAY(PROTOCOL_ERROR)` |
| `enable_secure_redirect` | `true` | Answer every request (HTTP/1.1 and h2c) with `301` to `https://<Host>[:<listen port>]` (the port is omitted for 443; h2c gets an HTTP/2 301). `true` without `listen` is a configuration error (nowhere to redirect to). Set `false` to actually serve plaintext |

Validation rules (`veil -t` reports them):

- No `listen` and no `listen_cleartext`: error.
- `listen` equal to `listen_cleartext`: error.
- `allow_http = false` and `allow_h2c = false`: error (the listener would accept nothing).
- `enable_secure_redirect = true` (the default) without `listen`: error.
- `[tls]` is required only when `listen` or `[http3] enabled = true` is present (a `listen_cleartext`-only server needs none, see [Plaintext-only servers](#plaintext-only-servers-without-tls)).
- `[http3] enabled = true` without `listen`: error (QUIC requires TLS). The `Alt-Svc` header advertises HTTP/3's actual port.
- UDS: `listen` and `listen_cleartext` accept `unix:<path>`; HTTP/3 does not.
- The removed keys below are a startup error naming the replacement; they are not silently ignored.

`listen_cleartext` is served by every worker thread, like `listen` (the old `http` redirect listener ran on one
thread). An idle worker set costs roughly 2 MB of RSS per worker, so a TLS + redirect setup uses a few MB more than
before; throughput on `listen` is unchanged.

### Migrating from the old keys

The old keys `[server].http`, `tls_only`, `h2c_enabled`, `h2c_listen`, `h2c_http1` and `http3_enabled` were removed
with no compatibility layer.

| Old | New |
|-----|-----|
| `listen` + `tls_only = true` (default) | `listen` |
| `http = "0.0.0.0:80"` (redirect-only listener) | `listen_cleartext = "0.0.0.0:80"` (redirecting is the default) |
| `h2c_enabled = true` + `h2c_listen = "0.0.0.0:8080"` | `listen_cleartext = "0.0.0.0:8080"` + `enable_secure_redirect = false` |
| `h2c_enabled = true` (h2c-only server) + `h2c_http1 = true` | `listen_cleartext = <the old listen>` (no `listen`) + `enable_secure_redirect = false` |
| `tls_only = false` + `h2c_enabled = true` (one port auto-detecting) | `listen` and a separate `listen_cleartext` port + `enable_secure_redirect = false` |
| `http3_enabled = true` (+ `[http3] listen`) | `[http3] enabled = true` (+ `[http3] listen` if needed) |

A configuration that had both `http` (redirect) and `h2c_listen` (served h2c) needs two listeners for two purposes;
`listen_cleartext` is a single address, so run the redirect on a separate veil instance (or let your load balancer
do it) or choose one behavior.

## HTTP to HTTPS Redirect

`listen_cleartext` redirects HTTP to HTTPS by default (`enable_secure_redirect = true`).

### Configuration

```toml
[server]
listen = "0.0.0.0:443"
listen_cleartext = "0.0.0.0:80"  # redirects to HTTPS (the default behavior)
```

### Behavior

- Access to `http://example.com/path` is redirected to `https://example.com/path` with 301 (h2c clients get an HTTP/2 301)
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

## Plaintext-only Servers without TLS

`[tls]` may be **omitted** when there is no TLS to terminate: no `listen` and no `[http3] enabled = true`, only
`listen_cleartext` with `enable_secure_redirect = false` (a redirect needs `listen`). Any configuration with `listen` or
HTTP/3 and without a usable `[tls] cert_path` / `key_path` fails validation with "TLS certificate is required".

```toml
[server]
listen_cleartext = "0.0.0.0:8080"   # HTTP/1.1 + h2c, auto-detected
enable_secure_redirect = false      # serve plaintext (the default is a 301 to HTTPS)
# allow_http = true                 # accept plaintext HTTP/1.1 (426 when false)
# allow_h2c = true                  # accept h2c (GOAWAY when false); needs the `http2` feature
```

This is the default shape of the [API library](library.md#configuration-in-library-mode). Do not expose plaintext
listeners to untrusted networks.

## VDS (`[vds]`)

Routes and upstreams delivered by a control plane. Keys: `enabled`, `server`, `servers`, `transport`,
`keepalive_secs`, `node_id`, `cluster`, `token_file`, `ca_cert`, `client_cert`, `client_key`, `poll_timeout_secs`,
`retry_initial_ms`, `retry_max_ms`, `max_response_bytes`, `max_resources`, `initial_fetch_timeout_ms`. Requires the `vds` feature (in `full`). Precedence:
`config.toml` < VDS < `VEIL_OVERRIDE_<n>` < `--override`. See [VDS](vds.md).

## ACME (`[acme]`)

Automatic certificate issuance and renewal (HTTP-01). Keys: `enabled`, `directory_url`, `contact`, `domains`,
`storage_dir`, `accept_terms_of_service`, `challenge_listen`, `renew_before_days`, `check_interval_secs`,
`directory_ca_cert`, `order_timeout_secs`. Requires the `acme` feature (in `full`) and `[tls] cert_path`/`key_path`.
See [ACME](acme.md).

## Native handlers (`[[l4]] handler`, `[api]`, `type = "Handler"`)

These belong to the [API server library](library.md) (features `handler`, `l4-handler`, `api`); the proxy binary does not
include them.

- `[[route]]` `action.type = "Handler"` with `handler = "<name>"` routes to a registered Rust/C/Python handler. Without
  the `handler` feature this is a configuration error.
- `[[l4]]` `handler = "<name>"` (feature `l4-handler`): handle the connection/datagram with a registered L4 handler
  instead of forwarding; `upstreams` is then optional. See [L4 proxy](l4-proxy.md#native-handlers).
- `[api]` (library only): `openapi_enabled`, `openapi_path`, `openapi_json_path`, `openapi_title`, `openapi_version`,
  `openapi_description`, `max_body_size`, `handler_threads`, `tokio_threads`, `grpc_max_message_size`. See
  [the library guide](library.md#api-keys-library-only).
