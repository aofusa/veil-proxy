# Migrating from nginx

[← Documentation index](README.md) · [日本語](ja/migration-nginx.md)

This guide maps an nginx configuration to veil's `config.toml`: the concepts, directive by directive (including
what veil does not have), worked examples, and the operational differences. It was checked against nginx 1.28 and
veil 0.9. Every complete example below passes `veil -t` in CI (`tools/lint/check-guide-configs.py`), and the
examples marked *(tested)* are also run side by side with nginx and compared response by response
(`tests/migration/`).

- [Concepts](#concepts)
- [Directives](#directives)
- [Examples](#examples)
- [Operations](#operations)
- [Performance settings](#performance-settings)
- [After migrating](#after-migrating)

## Concepts

| nginx | veil | Notes |
|---|---|---|
| `http { server { ... } }` | the whole `config.toml` | One file. `[server]` holds the listeners for every host. |
| `listen 443 ssl; listen 80;` | `[server] listen` (TLS) and `listen_cleartext` (plaintext) | One TLS listener and one plaintext listener; virtual hosts are routes with a `host` condition. |
| `server_name` | `[route.conditions] host` | Exact names and `*.example.com`. No regular expressions. |
| `location` | `[[route]]` | Routes are tried **in the order they are written**; the first match wins (nginx picks the longest prefix / first regex). Put specific routes first. |
| `location /static/` | `path = "/static/*"` (or `"/static/"`) | Prefix match. `path = "/robots.txt"` on a file is an exact match. No regex locations. |
| `root` / `alias` | `type = "File"`, `path = "/var/www/..."` | Always **alias** semantics: the matched prefix is removed and the rest is joined to `path`. |
| `proxy_pass` | `type = "Proxy"`, `url` or `upstream` | The matched prefix is removed, then joined to the URL's path (see [Proxy paths](#proxy-paths)). |
| `upstream { server ...; }` | `[upstreams."name"] servers = [...]` | Plus active health checks, circuit breaker and outlier detection. |
| `return 301 ...;` | `type = "Redirect"` | `redirect_url`, `redirect_status`, `preserve_path`. |
| `return 200 "ok";` | `type = "Respond"` | `status`, `body`, `content_type`. |
| `include` | — | One file; generate it, or use `-o` / `VEIL_OVERRIDE_<n>` for per-environment values ([running.md](running.md)). |
| OpenResty / njs scripts | Proxy-Wasm filters | [wasm.md](wasm.md); a Lua interpreter filter exists as an example (`examples/wasm-filters/lua-filter`). |

### Proxy paths

veil always removes the matched prefix before joining the path to the upstream URL. To keep the prefix (nginx's
`proxy_pass http://backend;` without a URI), put the prefix in the URL:

| nginx | veil |
|---|---|
| `location /api/ { proxy_pass http://app:3000; }` → `/api/users` reaches `/api/users` | `path = "/api/*"`, `url = "http://app:3000/api/"` |
| `location /api/ { proxy_pass http://app:3000/; }` → `/api/users` reaches `/users` | `path = "/api/*"`, `url = "http://app:3000/"` |

## Directives

Legend: **=** same meaning, **≈** close (see notes), **—** not available (the alternative is given).

### Listening, TLS and protocols

| nginx | veil | |
|---|---|---|
| `listen 443 ssl http2;` | `[server] listen = "0.0.0.0:443"`, `http2_enabled = true` | = HTTP/1.1 and HTTP/2 via ALPN |
| `listen 443 quic reuseport;` + `add_header Alt-Svc` | `[http3] enabled = true`, `listen = "0.0.0.0:443"` | = Alt-Svc is added by veil |
| `listen 80;` + `return 301 https://...` | `listen_cleartext = "0.0.0.0:80"` (redirects to https by default) | = `enable_secure_redirect = false` serves plaintext instead |
| `http2 on` on a plaintext port (h2c) | `listen_cleartext` (HTTP/1.1 and h2c prior knowledge) | ≈ nginx has no h2c prior knowledge on the same port |
| `ssl_certificate` / `ssl_certificate_key` | `[tls] cert_path` / `key_path` | = reloaded automatically (`auto_reload`) |
| `ssl_protocols TLSv1.2 TLSv1.3;` | — | ≈ rustls: TLS 1.2 and 1.3 only, modern ciphers only; not configurable down |
| `ssl_ciphers` | — | ≈ fixed safe set (rustls) |
| `ssl_session_cache` / `ssl_session_tickets` | `[tls] session_resumption` | = shared by all workers ([tls.md](tls.md)) |
| `ssl_verify_client on; ssl_client_certificate` | client certificates ([tls.md](tls.md#client-certificates-and-upstream-mtls-f-217)) | = |
| `ssl_stapling` | — | — not supported |
| `proxy_ssl_certificate` (mTLS upstream) | `tls_client_cert` / `tls_client_key` / `tls_ca` on the Proxy action | = |
| `proxy_ssl_server_name` / `proxy_ssl_name` | `sni_name` | = |
| kTLS (`ssl_conf_command Options KTLS`) | `[tls] ktls_enabled = true` | = Linux; see [tls.md](tls.md) |
| ACME (certbot outside nginx) | `[acme]` | ≈ built in ([acme.md](acme.md)) |

### Locations, files and redirects

| nginx | veil | |
|---|---|---|
| `location = /x` | `path = "/x"` | ≈ a File route on a file matches exactly; on other actions `"/x"` also matches `/x/...` |
| `location ~ \.php$` (regex) | — | — use prefixes / exact paths, or a Proxy-Wasm filter |
| `root` / `alias` / `index` | `type = "File"`, `path`, `index` | = alias semantics |
| `try_files $uri $uri/ /index.html;` (SPA) | — | — not available; use a route per asset prefix and a final `type = "File"` route on `index.html`, or a filter |
| `autoindex on;` | — | — directory listings are not generated |
| `return 301 https://$host$request_uri;` | `type = "Redirect"`, `redirect_url = "https://$host$request_uri"` | = |
| `rewrite ^/old/(.*)$ /new/$1 permanent;` | `type = "Redirect"`, `redirect_url = "/new"`, `preserve_path = true` on `path = "/old/*"` | ≈ prefix rewrites only |
| `rewrite ... last;` (internal) | — | — no internal rewrite; the prefix removal of Proxy/File covers the common cases |
| `open_file_cache` | `[open_file_cache]` and `[static_file_cache]` (also per route) | = ([performance.md](performance.md)) |
| `sendfile on;` | default for `type = "File"` (`mode = "sendfile"`) | = |
| `error_page 404 /404.html;` | — | — not available |

### Proxying and upstreams

| nginx | veil | |
|---|---|---|
| `proxy_pass http://app;` | `url = "http://app"` | = see [Proxy paths](#proxy-paths) |
| `proxy_set_header X-Real-IP $remote_addr;` | `[route.security] add_request_headers = { "X-Real-IP" = "$client_ip" }` | = variables `$client_ip`, `$host`, `$request_uri` |
| `proxy_set_header Host $host;` | — | = the client's Host is forwarded by default |
| `X-Forwarded-For` | `add_request_headers = { "X-Forwarded-For" = "$client_ip" }` | ≈ replaces instead of appending |
| `proxy_hide_header` / `add_header` | `remove_response_headers` / `add_response_headers` | = also on static files |
| `proxy_http_version 1.1;` + keepalive to upstreams | default (pooled connections) | = `max_idle_connections_per_host`, `idle_connection_timeout_secs` |
| HTTP/2 to the upstream | `use_h2c = true` (plaintext) / ALPN for `https://` | = multiplexed ([routing.md](routing.md#h2c-http2-over-cleartext-proxy)) |
| `grpc_pass` | `type = "Proxy"` to an h2c / h2 upstream | = gRPC, multiplexed ([routing.md](routing.md#h2c-http2-over-cleartext-proxy)) |
| WebSocket (`Upgrade` headers) | automatic | = |
| `proxy_connect_timeout` | `backend_connect_timeout_secs` | = |
| `proxy_read_timeout` / `proxy_send_timeout` | — on TOML routes | ≈ global I/O timeouts; per-route request timeouts are planned (F-226) |
| `proxy_next_upstream` | idempotent retry once on a closed pooled connection | ≈ no conditional retries on status codes yet (F-226) |
| `proxy_buffering on/off` | `[route.buffering] mode` | = `streaming` / `full` / `adaptive` ([compression-cache.md](compression-cache.md)) |
| `client_max_body_size` | `[route.security] max_request_body_size` | = |
| `client_header_timeout` / `client_body_timeout` | `client_header_timeout_secs` / `client_body_timeout_secs` | = |
| `upstream` round-robin / `weight=` | `algorithm = "round_robin"` / `"weighted"` + `weight` | = |
| `least_conn;` | `algorithm = "least_conn"` | = |
| `ip_hash;` / `hash $cookie_x consistent;` | `algorithm = "ip_hash"` / `"consistent_hash"` + `hash_key` | = |
| `max_fails=` / `fail_timeout=` (passive) | `[upstreams.X.outlier_detection]` / `circuit_breaker` | ≈ plus active `health_check` (nginx Plus only) |
| `split_clients` (canary) | `[route.traffic] split` | = |
| `proxy_cache` | `[route.cache]` | = memory and disk, `stale_while_revalidate`, `stale_if_error` |
| `proxy_cache_key` | `include_query`, `key_headers` | ≈ |
| unix socket upstream (`unix:/run/app.sock`) | `url = "http://unix:/run/app.sock"` | = |

### Security and traffic control

| nginx | veil | |
|---|---|---|
| `allow` / `deny` | `[route.security] allowed_ips` / `denied_ips`, or `[route.conditions] source_ip` | = |
| `limit_req zone=... rate=60r/m;` | `rate_limit_requests_per_min = 60` | ≈ per client IP and route, sliding window, no burst setting |
| `limit_conn` | `[security] max_concurrent_connections` | ≈ global, not per key |
| `limit_except GET { deny all; }` | `allowed_methods = ["GET", "HEAD"]` | = |
| `auth_basic` | — | — not available; use `[route.jwt]` or a Proxy-Wasm filter |
| `auth_request` (subrequest) | — | — not on TOML routes; ext_authz is planned (F-229); JWT verification is built in (`[route.jwt]`) |
| `add_header Strict-Transport-Security ...` | `add_response_headers` | = |
| `server_tokens off;` | default (no `Server` header) | = `server_header_enabled` turns it on |
| CORS (`add_header Access-Control-*`) | `[route.traffic.cors]` (preflight) + `add_response_headers` | = |
| `stream {}` (TCP / UDP) | `[[l4]]` | = TCP and UDP, with health checks ([l4-proxy.md](l4-proxy.md)) |
| process sandboxing (none in nginx) | seccomp, Landlock, privilege dropping, capsicum, pledge/unveil | veil only ([security.md](security.md)) |

### Compression, logging and status

| nginx | veil | |
|---|---|---|
| `gzip on;` / ngx_brotli | `[route.compression] enabled = true` | = gzip, brotli and zstd (`preferred_encodings`) |
| `gzip_types` | `[route.compression] compressible_types` / `skip_types` | = |
| `access_log` + `log_format` | `[access_log]` (`format = "text"` or `"json"`, `fields`) | ≈ a fixed field set (method, host, path, status, duration, client IP, upstream, sizes, user agent) |
| `error_log` | `[logging]` | = |
| `stub_status` | `[prometheus]` (`/__metrics`) | ≈ far more detail ([observability.md](observability.md)) |
| OpenTelemetry module | `[opentelemetry]` | = OTLP/HTTP |

## Examples

### Static site *(tested)*

nginx:

```nginx
server {
    listen 8080;
    root /var/www;
    index index.html;
}
```

veil:

```toml
[server]
listen_cleartext = "0.0.0.0:8080"
enable_secure_redirect = false

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
index = "index.html"
```

### Reverse proxy with an upstream group and headers *(tested)*

nginx:

```nginx
upstream app {
    least_conn;
    server 10.0.0.11:3000;
    server 10.0.0.12:3000;
}
server {
    listen 8080;
    location /api/ {
        proxy_pass http://app/api/;
        proxy_set_header X-Real-IP $remote_addr;
        add_header X-Frame-Options DENY;
    }
    location / {
        root /var/www;
    }
}
```

veil:

```toml
[server]
listen_cleartext = "0.0.0.0:8080"
enable_secure_redirect = false

[upstreams."app"]
algorithm = "least_conn"
servers = ["http://10.0.0.11:3000", "http://10.0.0.12:3000"]

  [upstreams."app".health_check]
  path = "/health"
  interval_secs = 10

[[route]]
[route.conditions]
path = "/api/*"
[route.action]
type = "Proxy"
upstream = "app"
[route.security]
add_request_headers = { "X-Real-IP" = "$client_ip" }
add_response_headers = { "X-Frame-Options" = "DENY" }

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
```

The upstream URLs carry no path, so `/api/users` reaches `http://10.0.0.11:3000/users`. To keep the `/api/` prefix as
nginx does here, write the servers as `"http://10.0.0.11:3000/api/"`.

### HTTPS with HTTP/2, HTTP/3 and a redirect from port 80

nginx:

```nginx
server {
    listen 80;
    return 301 https://$host$request_uri;
}
server {
    listen 443 ssl;
    listen 443 quic reuseport;
    http2 on;
    ssl_certificate     /etc/veil/ssl/cert.pem;
    ssl_certificate_key /etc/veil/ssl/key.pem;
    add_header Alt-Svc 'h3=":443"; ma=86400';
    location / { proxy_pass http://127.0.0.1:3000; }
}
```

veil:

```toml
[server]
listen = "0.0.0.0:443"
listen_cleartext = "0.0.0.0:80"   # redirects to https (enable_secure_redirect defaults to true)
http2_enabled = true

[tls]
cert_path = "/etc/veil/ssl/cert.pem"
key_path = "/etc/veil/ssl/key.pem"

[http3]
enabled = true
listen = "0.0.0.0:443"

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:3000"
```

### Virtual hosts, redirects and a fixed response *(tested)*

nginx:

```nginx
server {
    listen 8080;
    server_name old.example.com;
    return 301 https://new.example.com$request_uri;
}
server {
    listen 8080;
    server_name api.example.com;
    location = /healthz { return 200 "ok"; }
    location / { proxy_pass http://127.0.0.1:3000; }
}
```

veil:

```toml
[server]
listen_cleartext = "0.0.0.0:8080"
enable_secure_redirect = false

[[route]]
[route.conditions]
host = "old.example.com"
[route.action]
type = "Redirect"
redirect_url = "https://new.example.com$request_uri"
redirect_status = 301

[[route]]
[route.conditions]
host = "api.example.com"
path = "/healthz"
[route.action]
type = "Respond"
status = 200
body = "ok"
content_type = "text/plain"

[[route]]
[route.conditions]
host = "api.example.com"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:3000"
```

### Rate limiting, IP allow list and methods

nginx:

```nginx
limit_req_zone $binary_remote_addr zone=api:10m rate=120r/m;
server {
    listen 8080;
    location /admin/ {
        allow 10.0.0.0/8;
        deny all;
        limit_except GET POST { deny all; }
        limit_req zone=api;
        proxy_pass http://127.0.0.1:3000/admin/;
    }
}
```

veil:

```toml
[server]
listen_cleartext = "0.0.0.0:8080"
enable_secure_redirect = false

[[route]]
[route.conditions]
path = "/admin/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:3000/admin/"
[route.security]
allowed_ips = ["10.0.0.0/8"]
allowed_methods = ["GET", "POST"]
rate_limit_requests_per_min = 120
```

### Cache and compression

nginx:

```nginx
proxy_cache_path /var/cache/nginx keys_zone=c:10m max_size=1g;
server {
    listen 8080;
    gzip on;
    location / {
        proxy_cache c;
        proxy_cache_valid 200 5m;
        proxy_pass http://127.0.0.1:3000;
    }
}
```

veil:

```toml
[server]
listen_cleartext = "0.0.0.0:8080"
enable_secure_redirect = false

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:3000"
[route.cache]
enabled = true
default_ttl_secs = 300
cacheable_statuses = [200]
[route.compression]
enabled = true
```

### TCP and UDP (`stream {}`)

nginx:

```nginx
stream {
    server { listen 5432; proxy_pass 10.0.0.20:5432; }
    server { listen 53 udp; proxy_pass 10.0.0.53:53; }
}
```

veil (see [l4-proxy.md](l4-proxy.md) for every key):

```toml
# veil needs an HTTP listener even when it is used only for [[l4]] (here on loopback only)
[server]
listen_cleartext = "127.0.0.1:8080"
enable_secure_redirect = false

[[l4]]
name = "postgres"
listen = "0.0.0.0:5432"

  [[l4.upstreams]]
  addr = "10.0.0.20:5432"

[[l4]]
name = "dns"
listen = "0.0.0.0:53"
protocol = "udp"

  [[l4.upstreams]]
  addr = "10.0.0.53:53"
```

## Operations

| nginx | veil |
|---|---|
| `nginx -t` | `veil -t` (`-c FILE`) |
| `nginx -s reload` / `systemctl reload nginx` | `kill -HUP <pid>` / `systemctl reload veil`: zero-downtime reload; the listeners are kept |
| `nginx -s quit` | `SIGTERM` / `SIGINT`: graceful shutdown (in-flight requests finish, `graceful_shutdown_timeout_secs`) |
| `-c /etc/nginx/nginx.conf` | `-c FILE`, `VEIL_CONFIG`, or the default path ([running.md](running.md#configuration-file)) |
| per-environment `include`s / envsubst | `-o "key = value"` / `VEIL_OVERRIDE_<n>` |
| logs in `/var/log/nginx` | standard output / the journal by default; `file_path` for files |

veil validates the whole file before it starts or reloads: a reload with an invalid file keeps the running
configuration.

## Performance settings

| nginx | veil | |
|---|---|---|
| `worker_processes auto;` | `[server] threads = 0` | = one worker per CPU |
| `worker_cpu_affinity` | default | = workers are pinned |
| `reuseport` | default, with `reuseport_balancing = "cbpf"` | = |
| `worker_connections` / `worker_rlimit_nofile` | raised automatically to the hard limit | = |
| `sendfile on; tcp_nopush on;` | default for static files | = |
| `open_file_cache max=...` | `[static_file_cache]` + `[open_file_cache]` | = both are needed for zero-offload static serving |
| `ssl_conf_command Options KTLS` | `[tls] ktls_enabled = true` | = |
| `http3_stream_buffer_size` etc. | `[http3] mmsg_batch_size` / `recv_drain_max` | ≈ the main HTTP/3 knob ([performance.md](performance.md)) |

When comparing throughput, compare under the same conditions (the same `open_file_cache`, kTLS on or off on both),
as described in [docs/perf/README.md](../perf/README.md).

## After migrating

1. `veil -t -c config.toml` until it passes.
2. Start veil next to nginx on another port and replay a sample of real requests to both; compare status codes and
   the headers your clients depend on.
3. Check the access log format your log pipeline expects (`format = "json"` and `fields`).
4. Move the listener (or the load balancer target) and watch `/__metrics` ([Grafana dashboard](observability.md#grafana-dashboard)).
