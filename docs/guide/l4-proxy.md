# L4 Stream Proxy

[← Documentation index](README.md) · [日本語](ja/l4-proxy.md)

## L4 Stream Proxy

TCP/UDP-level (L4) load balancing proxy. Unlike the HTTP proxy, it forwards raw streams/datagrams without inspecting protocol payloads. Useful for databases, message brokers, Redis, SMTP, DNS, and any non-HTTP protocol.

> **Requires**: build with `--features l4-proxy` (included in `--features full`)

### Features

- **Round Robin / LeastConn** load balancing across upstream servers
- **TCP and UDP**: set `protocol = "tcp"` (default) or `protocol = "udp"` per listener
- **TLS Passthrough**: forward encrypted TLS without termination (SNI routing is not yet implemented; TCP only — UDP has no TLS/DTLS support)
- **Connection Limiting**: reject connections when `max_connections` is reached (for UDP this caps the number of concurrent client sessions)
- **Connect Timeout**: configurable upstream connection timeout (TCP only)
- **Health Check Integration**: TCP or gRPC health checks per L4 listener (UDP backends reuse the same TCP-connect-based health check; UDP reachability itself is out of scope)
- **Independent threads**: each L4 listener runs in its own thread on the io_uring runtime

### UDP Session Table

UDP is connectionless, so the UDP path uses a session-table design similar to nginx stream UDP / Envoy UDP proxy:

- A single listener UDP socket is shared across all sessions; `recvfrom` demultiplexes incoming datagrams by client address.
- On a new client address, an upstream is chosen via the configured load-balancing algorithm and a dedicated UDP socket is `connect()`-ed to it, then registered in the session table.
- Client → upstream: forwarded directly from the listener's receive loop.
- Upstream → client: a per-session task receives from the upstream socket and sends back through the shared listener socket.
- Sessions are evicted after `idle_timeout_secs` of no traffic in either direction.
- The custom async UDP socket (`runtime/udp.rs`) works identically on both the io_uring and reactor (epoll/kqueue) backends and adds **no new io_uring opcodes** — it does try-first `recvfrom`/`sendto`/`send`/`recv` and falls back to the existing generic-fd readiness wait (`wait_readable_fd`/`wait_writable_fd`) on `EAGAIN`.

### Configuration

L4 listeners are defined using `[[l4]]` sections (separate from HTTP routes). L4 listeners are started at launch and **cannot be hot-reloaded** via SIGHUP.

**Threading (F-156)**: TCP listeners run on `[server].threads` worker threads, each replicating the listener socket with `SO_REUSEPORT` (`SO_REUSEPORT_LB` on FreeBSD) and accepting on its own event loop — the same model as the HTTP/H2C workers. Before F-156 the L4 listener ran on a **single thread regardless of `threads`**, capping throughput on multi-core hosts (measured on FreeBSD: 0.58 → 1.09 relative to nginx). Load-balancing state (round-robin position, per-upstream connection counts) and the `max_connections` counter are shared across all workers, so distribution accuracy and the limit are preserved. UDP listeners remain single-threaded (the session table would be partitioned across workers).

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

```toml
# UDP proxy for DNS (session-table forwarding, requires --features l4-proxy)
[[l4]]
name = "dns-udp-proxy"
listen = "0.0.0.0:53"
protocol = "udp"                # "tcp" (default) or "udp"
lb = "round_robin"
idle_timeout_secs = 30          # evict a client session after 30s of no traffic

  [[l4.upstreams]]
  addr = "10.0.0.1:53"

  [[l4.upstreams]]
  addr = "10.0.0.2:53"
```

### Configuration Reference

| Option | Description | Default |
|--------|-------------|---------|
| `name` | Listener name (appears in logs) | required |
| `listen` | Bind address (e.g. `"0.0.0.0:3306"`) | required |
| `protocol` | Transport protocol: `tcp` or `udp` | `tcp` |
| `lb` | Load balancing: `round_robin` or `least_conn` | `round_robin` |
| `tls` | TLS mode: `none`, `passthrough`, or `terminate` (TCP only; ignored with a warning for `udp`) | `none` |
| `max_connections` | Max simultaneous connections/sessions (0 = unlimited) | `0` |
| `connect_timeout_secs` | Upstream connect timeout in seconds (TCP only) | `10` |
| `idle_timeout_secs` | Idle timeout in seconds before closing a connection/session | `600` |
| `wasm_modules` | WASM network filter module names (requires `wasm` feature, F-133). Empty (default) = WASM disabled and the zero-copy `splice`/`sendfile` path is used unchanged. When non-empty, `splice` is bypassed and data is routed through a userspace buffer so WASM modules can inspect/rewrite it (`proxy_on_downstream_data`/`proxy_on_upstream_data`); this switch is decided once per connection. | `[]` |
| `module_configuration` | Per-listener Proxy-Wasm plugin configuration override (module name → string or TOML table, F-148). Same merge rules as `[[route]]`; only names listed in `wasm_modules` are accepted. | (none) |
| `upstreams[].addr` | Upstream address (`"host:port"`) | required |
| `upstreams[].weight` | Weight (reserved for weighted RR) | `1` |
| `health_check` | Optional health check config (same as upstream health_check) | none |

### Notes

- L4 listeners bind ports at startup. **SIGHUP does not reload L4 configuration**.
- HTTP proxy and L4 proxy can coexist — they listen on different ports.
- TLS termination (`tls = "terminate"`) is reserved for future implementation; currently treated as passthrough.
- `protocol = "udp"` does not support TLS/DTLS. A UDP listener with `tls` set to anything other than `none` logs a startup warning and is forced to `none`.
- UDP health checks reuse the existing TCP-connect-based check; verifying actual UDP reachability is out of scope (protocol-dependent and not generally meaningful for connectionless traffic).
