# Routing, Redirects & Headers

[← Documentation index](README.md) · [日本語](ja/routing.md)

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
# WASM module names to apply to this route (directly under [[route]], NOT under [route.action])
modules = ["header_filter", "waf_filter"]

# Optional (F-148): override each module's plugin configuration for this route only.
# Only names listed in `modules` above may appear here.
[route.module_configuration.waf_filter]
mode = "log_only"

[route.conditions]
host = "api.example.com"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080/"
```

See [Plugin Configuration](wasm.md#plugin-configuration-f-148) for the value types and merge rules.

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
#
# NOTE (F-159): the file is read **once at configuration load time** and served from
# memory afterwards. Replacing the file on disk therefore requires a config reload
# (SIGHUP). Before F-159 this path re-read the file on every request, which defeated
# the whole point of "memory" mode.
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

**HTTP/2 to HTTPS backends (F-175)**: the `http2` option controls the upstream ALPN for
`https://` backends. It can be set on `[upstreams.X]` (group default), on a server entry
(`{ url = "https://...", http2 = "off" }`) or on a route's `url = "https://..."` action.

| `http2` | Behavior |
|---------|----------|
| `"auto"` (default) | Offer `h2, http/1.1` (for requests from HTTP/2 and HTTP/3 clients; see below). If the backend picks `h2`, requests go over a **multiplexed** HTTP/2 connection (F-174: many requests share one connection, both flow-control windows honored); if it picks `http/1.1`, the negotiated connection is used for HTTP/1.1 and the worker remembers the choice for 5 minutes (new connections then offer only `http/1.1`) |
| `"on"` | Require HTTP/2; a backend that does not pick `h2` gets a 502 |
| `"off"` | Offer only `http/1.1` (the v0.7 behavior) |

With `"auto"`, requests received over **HTTP/2 and HTTP/3** use the backend's HTTP/2 (full-duplex,
so gRPC streaming works; measured +80% / +55% for 3-byte responses from HTTP/2 / HTTP/3 clients
against nginx). Requests received over **HTTP/1.1** keep HTTP/1.1 to the backend under `"auto"`:
each client connection already has its own pooled backend connection, so going through the
multiplexed connection only adds a hop (measured −12% at 3 B, −13.5% at 54 KB). With `"on"` they use
HTTP/2 too: this applies to requests whose body has been fully received (no body, or a
`Content-Length` body already read), and the response is streamed back (`Transfer-Encoding: chunked`
unless the backend sent `content-length`). Requests still uploading a body, WASM-filtered routes and
routes with `buffering` stay on HTTP/1.1. Response
compression is applied the same way as for HTTP/1.1 backends (gRPC is never compressed). Use
`http2 = "off"` for backends whose HTTP/2 implementation you do not trust or that serve very large
responses that should not share one TCP connection.

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

**H2C Backend Connection Multiplexing (F-174)**: each worker keeps a pool of upstream HTTP/2 connections keyed by backend address, and **many requests share one connection concurrently** (one actor task per connection writes HEADERS/DATA and dispatches received frames to per-stream channels). A new connection is opened only when every pooled connection has reached the peer's `SETTINGS_MAX_CONCURRENT_STREAMS` or received GOAWAY. Request bodies respect both the connection and the stream flow-control windows (B-106), and received DATA is acknowledged with WINDOW_UPDATE only after the downstream has consumed it. A connection with no streams is closed with GOAWAY after `idle_connection_timeout_secs` (`0` disables pooling); the actor notices an upstream close immediately, so stale connections are not handed out. Streams refused by the upstream (REFUSED_STREAM, or above GOAWAY's `last_stream_id`) and idempotent requests that fail before the response head are retried once on a fresh stream. This is what makes gRPC relaying (`client → veil (TLS h2/h3) → backend (h2c)`) avoid a handshake per call and per-call upstream connections.

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

#### Unix Domain Socket Backends

veil can connect to upstream backends over `AF_UNIX` (Unix only). The notation matches
nginx's `proxy_pass http://unix:/path/to.sock:/uri;`:

```
http://unix:<socket-path>[:<path-prefix>]     # plaintext HTTP / h2c
https://unix:<socket-path>[:<path-prefix>]    # TLS terminated on top of the socket
```

```toml
[upstreams."uds-pool"]
algorithm = "round_robin"
servers = [
  "http://unix:/run/app1.sock",                              # no path prefix ("/")
  { url = "http://unix:/run/app2.sock:/api", use_h2c = true } # upstream path gets "/api" prepended
]

  [upstreams."uds-pool".health_check]
  enabled = true
  check_type = "tcp"
  interval_secs = 10
  timeout_secs = 5

# Single-backend form
[[route]]
[route.conditions]
path = "/app/*"
[route.action]
type = "Proxy"
url = "http://unix:/run/app1.sock"
```

Notes and limitations:

- The path-prefix separator is only recognised when the text after the **last** `:` starts
  with `/`. **Socket paths therefore cannot contain `:`.**
- A Unix socket has no host or port, so the `Host` header sent upstream defaults to
  `localhost`. Override it with `add_request_headers = { Host = "..." }` under
  `[route.security]`. For `https://unix:...`, the SNI name comes from `sni_name`
  (defaulting to `localhost`).
- Connection pools, logs, metrics and the consistent-hash node id identify these
  backends as `unix:<socket-path>`. Identifiers for TCP backends are unchanged.
- Health checks (`http`, `tcp`, `grpc`) work against `unix:` upstreams, but
  `timeout_secs` does not apply to the UDS connect itself (only to reads/writes).
- `[[l4]].upstreams` accepts `unix:<socket-path>` (no scheme, same notation as
  `[[l4]].listen`) for TCP stream proxying. L4 UDP upstreams cannot use UDS.
- HTTP/3 **upstreams** cannot use UDS (QUIC is UDP). Downstream HTTP/3 requests proxied
  to a UDS backend are supported.
- Windows rejects `unix:` upstream URLs at config validation time.
- On OpenBSD, backend socket paths are added to the `unveil(2)` allow-list automatically
  and the `unix` pledge promise is requested. On FreeBSD, capability mode (capsicum)
  cannot be combined with any upstream connection, UDS included.

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
| | `rate_limit_requests_per_min` | Request limit per minute **per client IP and route, shared by all workers** (B-31: sliding window over a process-wide 65,536-slot atomic table; requests to one route do not consume another route's limit; keys that hash to the same slot share a count, which can only make the limit stricter) | 0 (unlimited) |
| | `allowed_ips` | Allowed IP/CIDR (array) | all allowed |
| | `denied_ips` | Denied IP/CIDR (array, takes priority) | none |
| Connection Pool | `max_idle_connections_per_host` | Max idle connections per host | 256 |
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

Values are expanded in a **single pass** over the template, so an expanded value is never
re-scanned for placeholders. If a client IP (or Host, or URI) happens to contain the literal
text `$host`, that text is emitted as-is rather than being substituted again. An unrecognized
placeholder (e.g. `$foo`) is left in the value unchanged.

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
