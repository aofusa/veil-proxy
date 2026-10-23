# Migrating from Envoy

[← Documentation index](README.md) · [日本語](ja/migration-envoy.md)

There are two ways to replace Envoy with veil:

1. **As the proxy itself.** An Envoy that you run with a static `envoy.yaml` (an edge proxy, an API gateway) or with
   your own xDS control plane becomes veil with a `config.toml`, and optionally [VDS](vds.md) for routes and upstreams
   pushed by a control plane. This guide covers this case.
2. **As a service mesh data plane.** Under Istio or Linkerd, veil speaks Envoy's xDS (Delta ADS, SDS) and Linkerd's
   policies directly, so nothing has to be translated: you replace the proxy image. See [mesh.md](mesh.md).

The guide was checked against Envoy 1.36 and veil 0.9. Every complete example below passes `veil -t` in CI
(`tools/lint/check-guide-configs.py`), and the examples marked *(tested)* are also run side by side with Envoy and
compared response by response (`tests/migration/`).

## Concepts

| Envoy | veil | Notes |
|---|---|---|
| `static_resources.listeners[]` | `[server] listen` (TLS) / `listen_cleartext` (plaintext), `[http3]`, `[[l4]]` | One TLS and one plaintext HTTP listener; more ports through `[[l4]]` for TCP / UDP. |
| `filter_chains[].transport_socket` (`DownstreamTlsContext`) | `[tls]` | One certificate set (SNI selection between several certificates is not available on TOML listeners). |
| `http_connection_manager` | built in | HTTP/1.1, HTTP/2, h2c and HTTP/3 on the listeners above. |
| `route_config.virtual_hosts[].domains` | `[route.conditions] host` | Exact names and `*.example.com`. |
| `routes[].match` | `[route.conditions]` | `prefix` → `path = "/x/*"`, `path` → `path = "/x"` (exact), `headers` → `header`, `query_parameters` → `query`. No `safe_regex`. |
| `routes[].route.cluster` | `[route.action] type = "Proxy"`, `upstream = "<cluster>"` | |
| `clusters[]` | `[upstreams."<name>"]` | `lb_policy`, health checks, outlier detection, circuit breaker. |
| `load_assignment.endpoints` | `servers = [...]` | URLs with weights. |
| HTTP filters (`envoy.filters.http.*`) | route-level settings and Proxy-Wasm modules | See the tables below. |
| `envoy.filters.http.wasm` | `[wasm]` + route `modules` | Proxy-Wasm ABI 0.2.1: Envoy's Rust / C++ / TinyGo filters run unchanged ([wasm.md](wasm.md)). |
| xDS (LDS / RDS / CDS / EDS) | [VDS](vds.md) | Routes and upstreams only, State-of-the-World; listeners and TLS need a restart. |
| `admin` | `[admin]`, `[prometheus]` | Reload status, metrics on `/__metrics`. |
| hot restart | `SIGHUP` | Zero-downtime reload in the same process; the listeners are kept. |

Routes are tried **in the order they are written** (the same as Envoy's routes within a virtual host); put specific
routes first. veil has no separate virtual-host level: write the `host` condition on each route.

## Route matching and actions

| Envoy | veil | |
|---|---|---|
| `match: { prefix: "/api/" }` | `path = "/api/*"` | = (also matches `/api`) |
| `match: { path: "/healthz" }` | `path = "/healthz"` | = exact (a `path` without `*` is exact, except `"/"`, which matches every path) |
| `match: { safe_regex: ... }` | — | — use prefixes, or a Proxy-Wasm filter |
| `headers: [{ name, string_match }]` | `header = { "X-Version" = "v2" }` | ≈ exact values |
| `query_parameters` | `query = { "token" = "..." }` | ≈ exact values |
| source IP (`rbac` / `remote_ip` matcher) | `source_ip = ["10.0.0.0/8"]` or `[route.security] allowed_ips` | = |
| `route: { cluster }` | `type = "Proxy"`, `upstream` | = |
| `prefix_rewrite: "/"` | the matched prefix is always removed; put a path in the upstream URL to add one | ≈ ([routing.md](routing.md)) |
| `host_rewrite_literal` | `add_request_headers = { "Host" = "..." }` | ≈ |
| `weighted_clusters` | `[route.traffic] split` | = |
| `direct_response` | `type = "Respond"` | = |
| `redirect` (`https_redirect`, `path_redirect`) | `type = "Redirect"` / `listen_cleartext` with `enable_secure_redirect` | = |
| `timeout`, `idle_timeout` | — on TOML routes | ≈ global I/O timeouts; per-route timeouts are planned (F-226; the mesh data plane has them) |
| `retry_policy` | idempotent retry once on a closed pooled connection | ≈ conditional retries are planned (F-226) |
| `request_mirror_policies` | — | — planned (F-226) |
| `request_headers_to_add` / `response_headers_to_add` / `*_to_remove` | `add_request_headers` / `add_response_headers` / `remove_*` | = |

## HTTP filters

| Envoy filter | veil | |
|---|---|---|
| `router` | built in | = |
| `cors` | `[route.traffic.cors]` | = preflight answers; add `Access-Control-Allow-Origin` to responses with `add_response_headers` |
| `fault` (abort) | `[route.traffic.fault]` | = |
| `fault` (delay) | — | — planned (F-226) |
| `local_ratelimit` | `rate_limit_requests_per_min` | ≈ per client IP and route |
| `ratelimit` (global service) | — | — not available |
| `jwt_authn` | `[route.jwt]` | = local JWKS file or inline; remote JWKS URIs are not fetched |
| `rbac` | `allowed_ips` / `denied_ips` / `allowed_methods`, `[tls] client_allowed_sans` | ≈ |
| `ext_authz` | — | — planned (F-229) |
| `compressor` (gzip, brotli, zstd) | `[route.compression]` | = |
| `cache` | `[route.cache]` | ≈ for HTTP/1.1 clients and `http://` upstreams (the rest: F-257) |
| `buffer` | `[route.buffering] mode = "full"` | = |
| `grpc_web` | — on proxy routes | — the API library serves gRPC-Web itself ([library.md](library.md)); on proxy routes translate in the backend |
| `wasm` | Proxy-Wasm `modules` | = |
| `lua` | Proxy-Wasm (a Lua filter exists as an example) | ≈ |
| `health_check` (pass-through / fixed answer) | `type = "Respond"` on the health path | ≈ |

## Clusters

| Envoy | veil | |
|---|---|---|
| `lb_policy: ROUND_ROBIN` / `LEAST_REQUEST` / `RING_HASH` / `MAGLEV` / `RANDOM` | `algorithm = "round_robin"` / `"least_conn"` / `"consistent_hash"` / `"consistent_hash"` / — | ≈ |
| endpoint `load_balancing_weight` | `{ url = "...", weight = 3 }` with `algorithm = "weighted"` | = |
| `health_checks` (HTTP / TCP / gRPC) | `[upstreams.X.health_check] check_type = "http" / "tcp" / "grpc"` | = |
| `outlier_detection` | `[upstreams.X.outlier_detection]` | = error-rate based ejection |
| `circuit_breakers` (max connections / requests) | `[upstreams.X.circuit_breaker]` | ≈ **different meaning**: veil's breaker opens on failures (Envoy's limits concurrency); limit connections with `max_idle_connections_per_host` and `[security] max_concurrent_connections` |
| `transport_socket` (`UpstreamTlsContext`) | `https://` URLs, `sni_name`, `tls_client_cert` / `tls_client_key` / `tls_ca` | = |
| `http2_protocol_options` | `use_h2c = true` / ALPN | = multiplexed |
| `STRICT_DNS` / `LOGICAL_DNS` | host names in `servers` (resolved off the data plane, cached) | ≈ |
| `EDS` | VDS `Upstream` resources | ≈ the whole server list per update |

## Listeners

| Envoy | veil | |
|---|---|---|
| `tcp_proxy` | `[[l4]]` (`protocol = "tcp"`) | = with health checks and least-connections |
| `udp_proxy` | `[[l4]]` (`protocol = "udp"`) | = |
| TLS passthrough by SNI (`tls_inspector` + `server_names`) | `[[l4]] tls = "passthrough"` | ≈ ([l4-proxy.md](l4-proxy.md)) |
| `proxy_protocol` listener filter | — | — not available |
| `access_log` | `[access_log]` | ≈ text or JSON with a fixed field set |
| `stats_sinks` / Prometheus stats | `[prometheus]` (`/__metrics`), `[opentelemetry]` | ≈ different metric names ([observability.md](observability.md)) |
| `tracing` | `[opentelemetry]` | ≈ |

## Examples

### Edge proxy with a cluster and a direct response *(tested)*

Envoy:

<!-- not-a-full-config -->
```yaml
static_resources:
  listeners:
    - name: http
      address: { socket_address: { address: 0.0.0.0, port_value: 8080 } }
      filter_chains:
        - filters:
            - name: envoy.filters.network.http_connection_manager
              typed_config:
                "@type": type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager
                stat_prefix: http
                route_config:
                  virtual_hosts:
                    - name: app
                      domains: ["*"]
                      routes:
                        - match: { path: "/healthz" }
                          direct_response: { status: 200, body: { inline_string: "ok" } }
                        - match: { prefix: "/" }
                          route: { cluster: app }
                http_filters:
                  - name: envoy.filters.http.router
                    typed_config: { "@type": type.googleapis.com/envoy.extensions.filters.http.router.v3.Router }
  clusters:
    - name: app
      type: STRICT_DNS
      lb_policy: LEAST_REQUEST
      load_assignment:
        cluster_name: app
        endpoints:
          - lb_endpoints:
              - endpoint: { address: { socket_address: { address: app1, port_value: 3000 } } }
              - endpoint: { address: { socket_address: { address: app2, port_value: 3000 } } }
      health_checks:
        - timeout: 2s
          interval: 10s
          unhealthy_threshold: 3
          healthy_threshold: 2
          http_health_check: { path: "/health" }
```

veil:

```toml
[server]
listen_cleartext = "0.0.0.0:8080"
enable_secure_redirect = false

[upstreams."app"]
algorithm = "least_conn"
servers = ["http://app1:3000", "http://app2:3000"]

  [upstreams."app".health_check]
  path = "/health"
  interval_secs = 10
  timeout_secs = 2
  unhealthy_threshold = 3
  healthy_threshold = 2

[[route]]
[route.conditions]
path = "/healthz"
[route.action]
type = "Respond"
status = 200
body = "ok"
content_type = "text/plain"   # Envoy's direct_response sends text/plain

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "Proxy"
upstream = "app"
```

### Canary with weighted clusters, CORS and fault injection

Envoy (route part):

<!-- not-a-full-config -->
```yaml
routes:
  - match: { prefix: "/api/" }
    route:
      weighted_clusters:
        clusters:
          - { name: stable, weight: 90 }
          - { name: canary, weight: 10 }
    typed_per_filter_config:
      envoy.filters.http.cors:
        "@type": type.googleapis.com/envoy.extensions.filters.http.cors.v3.CorsPolicy
        allow_origin_string_match: [{ exact: "https://app.example.com" }]
        allow_methods: "GET,POST"
      envoy.filters.http.fault:
        "@type": type.googleapis.com/envoy.extensions.filters.http.fault.v3.HTTPFault
        abort: { http_status: 503, percentage: { numerator: 1, denominator: HUNDRED } }
```

veil:

```toml
[server]
listen_cleartext = "0.0.0.0:8080"
enable_secure_redirect = false

[upstreams."stable"]
servers = ["http://stable:3000"]

[upstreams."canary"]
servers = ["http://canary:3000"]

[[route]]
[route.conditions]
path = "/api/*"
[route.action]
type = "Proxy"
upstream = "stable"
[route.traffic]
split = [
  { upstream = "stable", weight = 90 },
  { upstream = "canary", weight = 10 },
]
[route.traffic.cors]
allow_origins = ["https://app.example.com"]
allow_methods = ["GET", "POST"]
[route.traffic.fault]
abort_status = 503
abort_percent = 1.0
[route.security]
add_response_headers = { "Access-Control-Allow-Origin" = "https://app.example.com", "Vary" = "Origin" }
```

### TLS, HTTP/3 and JWT

Envoy: a listener with a `DownstreamTlsContext`, `http3_protocol_options` on a QUIC listener and the `jwt_authn`
filter with a local JWKS. veil:

```toml
[server]
listen = "0.0.0.0:443"
listen_cleartext = "0.0.0.0:80"
http2_enabled = true

[tls]
cert_path = "/etc/veil/ssl/cert.pem"
key_path = "/etc/veil/ssl/key.pem"

[http3]
enabled = true
listen = "0.0.0.0:443"

[[route]]
[route.conditions]
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:3000/"
[route.jwt]
issuer = "https://auth.example.com"
audiences = ["api"]
jwks_path = "/etc/veil/jwks.json"
```

### TCP proxy (`tcp_proxy`)

```toml
# veil needs an HTTP listener even when it is used only for [[l4]] (here on loopback only)
[server]
listen_cleartext = "127.0.0.1:8080"
enable_secure_redirect = false

[[l4]]
name = "postgres"
listen = "0.0.0.0:5432"
lb = "least_conn"

  [[l4.upstreams]]
  addr = "10.0.0.20:5432"

  [[l4.upstreams]]
  addr = "10.0.0.21:5432"
```

## From an xDS control plane to VDS

VDS keeps xDS's semantics (version, nonce, ACK / NACK; State-of-the-World) over a gRPC stream or REST long polling,
with two resource types: `Route` (a `[[route]]` entry as JSON) and `Upstream` (an `[upstreams.<name>]` table).
To move a control plane:

1. Generate veil `Route` / `Upstream` resources instead of `RouteConfiguration` / `Cluster` / `ClusterLoadAssignment`
   (the JSON has the same shape as the TOML, [vds.md](vds.md#resource-types)).
2. Keep listeners, TLS and `[[l4]]` in `config.toml` (VDS does not change them).
3. Start from the reference servers (`tools/vds/server` in Rust, `tools/vds/vds_server.py` in Python).

Delta xDS, ADS across resource types and on-demand (VHDS) are not part of VDS. Under Istio / Linkerd use the mesh
data plane instead ([mesh.md](mesh.md)), which speaks Delta ADS natively.

## Operations

| Envoy | veil |
|---|---|
| `envoy --mode validate -c envoy.yaml` | `veil -t -c config.toml` |
| hot restart / xDS updates | `SIGHUP` (whole file) / VDS (routes and upstreams) |
| `/ready`, `/stats`, `/config_dump` (admin) | `/__metrics`, `[admin]` (reload, status) |
| drain (`/drain_listeners`, `--drain-time-s`) | `SIGTERM`: graceful shutdown, `graceful_shutdown_timeout_secs` |
| `--concurrency` | `[server] threads` (0 = one per CPU) |

## After migrating

1. `veil -t -c config.toml` until it passes.
2. Run veil next to Envoy, send a sample of real requests to both, and compare status codes and the headers your
   clients depend on.
3. Map dashboards and alerts to veil's metric names ([observability.md](observability.md), the
   [Grafana dashboard](observability.md#grafana-dashboard)).
4. Move the traffic.
