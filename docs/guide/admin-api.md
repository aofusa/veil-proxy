# Admin & Cache Purge APIs

[← Documentation index](README.md) · [日本語](ja/admin-api.md)

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
