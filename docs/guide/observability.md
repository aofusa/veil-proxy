# Metrics, Tracing & Logging

[← Documentation index](README.md) · [日本語](ja/observability.md)

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
| `veil_wasm_fuel_consumed_total` | Counter | filter, phase | Total wasmtime fuel consumed by WASM filters |

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
| `app_file_path` | App log (INFO/WARN/DEBUG/TRACE) output path | none (**stdout**) |
| `error_file_path` | Error log (ERROR) output path | none (**stderr**) |

### Log Output Destinations (by stream)

Application logs and error logs are routed to **separate destinations by level**, and each log line carries a `type` field (`app` / `error` / `access`) so mixed streams remain distinguishable:

| Stream | Levels | Config key | Default | `type` |
|--------|--------|-----------|---------|--------|
| App log | INFO / WARN / DEBUG / TRACE | `app_file_path` | **stdout** | `app` |
| Error log | ERROR | `error_file_path` | **stderr** | `error` |
| Access log | (per-request) | `[access_log].file_path` | **stdout** | `access` |

When a file path is set for any stream, its **parent directory** is automatically added to `[security].landlock_write_paths` so logging is not denied when Landlock is enabled (daily-rotated files live under the same directory).

### Log Output Formats

#### Text Format (Default)

```
2024-01-01 00:00:00.000+00 0ms INFO type=app main [main.rs:123] Server started
```

#### JSON Format

Suitable for integration with structured log collection systems (Elasticsearch, Loki, etc.).

```json
{"timestamp":"2024-01-01T00:00:00.000Z","level":"INFO","type":"app","target":"veil","file":"main.rs","line":123,"message":"Server started"}
```

### Configuration Example

```toml
[logging]
level = "info"
format = "text"  # or "json"
channel_size = 100000
flush_interval_ms = 1000
# App log -> stdout by default; Error log -> stderr by default.
app_file_path = "/var/log/veil/veil.log"          # optional
error_file_path = "/var/log/veil/veil.error.log"  # optional
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
file_path = "/var/log/veil/access.log"  # omit for stdout; parent dir auto-added to landlock_write_paths
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
{"timestamp":"2026-01-01T00:00:00Z","type":"access","method":"GET","host":"example.com","path":"/api/data","status":200,"duration_ms":12,"client_ip":"10.0.0.1","upstream":"192.168.1.10:8080","req_body_size":0,"resp_body_size":1024,"user_agent":"curl/8.0"}
```
