# Compression, Proxy Cache & Buffering

[← Documentation index](README.md) · [日本語](ja/compression-cache.md)

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
