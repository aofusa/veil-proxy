# Load Balancing, Health Checks & Resilience

[← Documentation index](README.md) · [日本語](ja/load-balancing.md)

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
