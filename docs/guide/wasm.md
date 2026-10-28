# WASM Extensions (Proxy-Wasm)

[← Documentation index](README.md) · [日本語](ja/wasm.md)

## WASM Extension System

Veil provides a WASM extension system fully compliant with Proxy-Wasm ABI v0.2.1. Proxy-Wasm modules created for Nginx/Envoy can be used with Veil without modification.

### Features

- **Proxy-Wasm v0.2.1 Compliant**: 100% compatible with Nginx/Envoy
- **AOT Compilation & Auto-Cache**: Modules are AOT-compiled; a `.cwasm` sidecar is generated next to each `.wasm` on first load and reused via `deserialize` on subsequent startups (invalidated automatically when the `.wasm` is newer or the wasmtime version changes; falls back to recompilation on any error). Explicit `.cwasm` paths are also supported.
- **Pooling Allocator**: High-speed instance creation
- **Async Execution (no Head-of-Line blocking)**: Modules run on wasmtime async support with fuel-based cooperative yielding (every ~10k instructions), so a CPU-heavy filter cannot stall the io_uring worker's other I/O
- **Capability Restrictions**: Fine-grained per-module permission control (all disabled by default)
- **Optional Pulley Interpreter** (`[wasm] interpreter = true`, F-135): runs Wasm through wasmtime's Pulley portable-bytecode interpreter instead of the Cranelift native JIT, emitting no native code at all. Useful on hosts with W^X constraints or where executable `mmap` cannot be granted. Slower than the native JIT. Default is `false` (Cranelift JIT). The AOT sidecar cache uses a distinct filename (`.pulley.cwasm`) so JIT and Pulley builds never fight over — or invalidate — the same cache file. **On OpenBSD (any arch), NetBSD (any arch), and FreeBSD aarch64 this setting is always ignored and Pulley is always used** (B-52/B-55; explicitly setting `interpreter = false` logs a startup warning and is otherwise ignored)
- **HTTP/3 parity with HTTP/1.1/HTTP/2** (F-132): the HTTP/3 path now runs the full filter lifecycle — `on_log` fires from every exit point (including early `LocalResponse` returns), `Backend::File` static-serving routes also get the `on_response_headers` filter applied, and `Backend::Proxy` routes run request/response body filters (`on_request_body`/`on_response_body`, applied once with `end_of_stream=true` since HTTP/3 already buffers the whole body when a WASM module is configured). A response body rewrite updates `content-length` to match, avoiding the H3 message-framing error nghttp3 raises on a mismatch/duplicate.
- **gRPC trailer filtering** (F-133): `on_request_trailers`/`on_response_trailers` now run for gRPC-over-H2C requests, so a module can rewrite `grpc-status`/`grpc-message` on the response trailers, or reject a request based on client-sent request trailers (via `LocalResponse`). Request trailers are only observed this way (there is no client-trailer forwarding to the backend); response trailer `Pause`/`LocalResponse` is not applicable since HEADERS/DATA are already sent by that point, so the original trailers are kept and a warning is logged.
- **L4 network filter (Proxy-Wasm `StreamContext` ABI)** (F-133): `[[l4]]` TCP listeners can attach WASM modules via `wasm_modules = ["name"]` to inspect/rewrite raw bytes (`proxy_on_new_connection`, `proxy_on_downstream_data`, `proxy_on_upstream_data`, `proxy_on_downstream_connection_close`, `proxy_on_upstream_connection_close`), using `BufferType::DownstreamData`/`UpstreamData` (values `2`/`3`) via `proxy_get_buffer_bytes`/`proxy_set_buffer_bytes`, and can close the connection via `proxy_close_stream`. **Zero cost when unset**: an empty `wasm_modules` list (the default) takes the same `splice`/zero-copy path as before with one added `is_empty()` branch per connection. When modules are configured, the listener switches (once, at connection setup) to a userspace-buffer copy loop instead of `splice`, since the data must be visible to WASM.

### Build

```bash
cargo build --release --features wasm
```

### Configuration

```toml
[wasm]
enabled = true

# Run Wasm via the Pulley interpreter instead of the Cranelift native JIT (F-135).
# Default: false. Always forced to true on OpenBSD regardless of this setting (B-52).
# interpreter = false

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

### Plugin Configuration (F-148)

`configuration` is the byte string handed to the module as its Proxy-Wasm *plugin configuration*
(`proxy_on_configure` / `proxy_get_buffer(PluginConfiguration)`). It can be written **either** as a
string (the original form — passed through verbatim, typically JSON) **or** as a TOML table
(serialized to a JSON object before being passed, so modules need no changes):

```toml
# 1. String form (backwards compatible — bytes are passed through unchanged)
[[wasm.modules]]
name = "header_filter"
path = "/etc/veil/wasm/header_filter.wasm"
configuration = '{"header_name": "x-veil"}'

# 2. TOML table form (converted to a JSON object)
[[wasm.modules]]
name = "waf_filter"
path = "/etc/veil/wasm/waf_filter.wasm"
[wasm.modules.configuration]
mode = "block"
max_body_size = 65536
patterns = ["union select", "<script"]
```

TOML → JSON conversion: String → string, Integer/Float → number (`NaN`/`Inf` → `null`),
Boolean → bool, Datetime → RFC 3339 string, Array → array, Table → object.

#### Per-route override

`[[route]]` (and `[[l4]]`) accept `module_configuration`, a map from module name to a
configuration value of the same type, letting one loaded module serve several routes with
different parameters:

```toml
[[route]]
modules = ["header_filter", "waf_filter"]

# Only this route runs the WAF in log-only mode; other routes keep "block"
[route.module_configuration.waf_filter]
mode = "log_only"

[route.conditions]
path = "/static/*"
[route.action]
type = "File"
path = "./www"
```

Merge rules (route wins):

| Module definition | Route | Effective configuration |
|---|---|---|
| — | — | empty byte string |
| set | — | module definition value |
| — | set | route value |
| Table | Table | **deep merge** (same-named keys take the route value, nested tables merge recursively, arrays are replaced) |
| any other combination (either side a string) | set | route value **replaces** the module value entirely (an opaque string cannot be merged) |

Naming a module in `module_configuration` that is not listed in that route's `modules`
(or that listener's `wasm_modules`) is a configuration error and rejected at startup.

Merging and TOML → JSON conversion run **once at config load time** (startup and SIGHUP
reload); the request path only clones an `Arc`.

### Filter Phases by Protocol

| Phase | HTTP/1.1 | HTTP/2 | HTTP/3 |
|---|---|---|---|
| `on_http_request_headers` | yes | yes | yes |
| `on_http_request_body` | yes | yes | yes |
| `on_http_request_trailers` | – | gRPC over h2c | – |
| `on_http_response_headers` | yes | yes | yes |
| `on_http_response_body` | – | – | yes |
| `on_log` | yes | yes | yes |

The request-body phase runs when the request has a body and at least one of the route's modules has
`allow_request_body_read = true`. veil reads the whole body first (Content-Length or chunked, up to
`max_request_body_size` / `max_chunked_body_size`; larger bodies get `413`) and calls
`on_http_request_body` once with `end_of_stream = true`. The callback can read the request path, method
and headers as well. If the module replaces the body, veil sends the new body upstream with a matching
`Content-Length` (a chunked request is forwarded with `Content-Length`, without `Expect`). Routes without such a
module keep streaming the body upstream as before. For a chunked request with `Expect: 100-continue`, veil sends
`100 Continue` before reading the body (a request with `Content-Length` already gets it before the filters run).

On HTTP/1.1 the connection stays open after a filter's local response (`send_local_response`), and a request that
was pipelined behind the body is served next. The body of a local response is not sent for `HEAD` (the
`Content-Length` stays the body's length) or for `1xx`, `204` and `304`.

### Reusing the VM Context (`instance_reuse`)

```toml
[[wasm.modules]]
name = "baseline"
path = "/etc/veil/wasm/baseline_filter.wasm"
instance_reuse = true   # default: false
```

By default every callback runs in a fresh instance (`_start`, `proxy_on_vm_start` and `proxy_on_configure`
run each time). With `instance_reuse = true` an instance is created and configured once, then reused for
later requests: each request only creates an HTTP context (`proxy_on_context_create`) and runs the callbacks,
and `proxy_on_delete` runs when the request is done. This is the same lifetime as Envoy. All phases of a request
(request headers, request body, response headers, response body and `on_log`) use the same HTTP context, so
state the filter keeps in its HTTP context carries over from one phase to the next. One instance holds the
HTTP contexts of several requests at once, as an Envoy worker's VM does. If a request ends before `on_log`
(for example the client disconnects), its context is deleted the next time the instance is used. The filter's
global variables and root-context state persist across requests. An instance that traps or returns an error
is discarded and the next request gets a new one; later phases of the requests whose contexts lived in it
run in a new HTTP context (the filter still runs, but state kept in the old context is lost). Each worker keeps
configured instances per module (and per route `module_configuration`), up to half of the pooling allocator's
`total_memories` divided among the workers and the modules with `instance_reuse`.

### Answering Requests from a Filter (`Respond` + modules)

A `type = "Respond"` route can carry `modules`. If a filter answers with `send_local_response`, veil returns
that response; otherwise it returns the route's fixed response. This makes routes that are answered entirely by
a filter, with no upstream:

```toml
[[route]]
modules = ["baseline"]
[route.conditions]
path = "/baseline11"
[route.action]
type = "Respond"
status = 404
```

### Files Loaded at Startup (`data_files`)

```toml
[[wasm.modules]]
name = "baseline"
path = "/etc/veil/wasm/baseline_filter.wasm"
data_files = { dataset = "/etc/veil/data/dataset.json" }
```

Each file is read when the configuration is loaded (startup and reload) and exposed to the filter as the
property `plugin_data.<name>` (`get_property(["plugin_data", "dataset"])`). Nothing is read from disk per
request. A missing file is a configuration error. If the module sets `allowed_properties`, include
`"plugin_data.*"`.

### Default Settings

The `[wasm.defaults]` section allows you to configure global WASM runtime settings:

| Option | Description | Default |
|--------|-------------|---------|
| `max_execution_time_ms` | Maximum execution time per WASM call (milliseconds) | 100 |
| `yield_interval_us` | How often a running callback yields the worker to other requests (approximate microseconds, measured in fuel). Smaller values shorten the wait of other requests on the same worker behind a CPU-heavy filter, but every callback pauses more often, more `instance_reuse` instances are open at once, and each request costs more | 1000 |
| `interpreter` | Run Wasm via the Pulley interpreter instead of the Cranelift native JIT (F-135). Always forced to `true` on OpenBSD (B-52) | false |
| `grpc_connect_timeout_ms` | Upper bound on connecting (TCP and TLS) to a gRPC upstream for `proxy_grpc_call`/`proxy_grpc_stream`. A longer guest `timeout_ms` still gives up connecting after this time, so an unreachable upstream does not stall every plugin's gRPC calls. At most 4096 calls and 4096 stream messages wait for the gRPC thread; beyond that the host call returns `InternalFailure` | 5000 |
| `http_call_max_timeout_ms` | Upper bound on the `timeout_ms` a plugin passes to `proxy_http_call` (a negative value counts as 0) | 60000 |
| `shared_data_max_keys` | Maximum number of keys in one module's shared data; `proxy_set_shared_data` with a new key beyond it returns `InternalFailure` (existing keys can still be updated) | 100000 |

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
| `allow_shared_data` | Shared data access (`proxy_set_shared_data`/`proxy_get_shared_data`). One table per module name, shared by every request and worker and kept across config reloads (Envoy shares it per `vm_id`) | false |
| `allow_request_headers_read` | Read request headers | false |
| `allow_request_headers_write` | Modify request headers | false |
| `allow_request_body_read` | Read request body | false |
| `allow_request_body_write` | Modify request body | false |
| `allow_response_headers_read` | Read response headers | false |
| `allow_response_headers_write` | Modify response headers | false |
| `allow_response_body_read` | Read response body | false |
| `allow_response_body_write` | Modify response body | false |
| `allow_downstream_data_read` | Read L4 downstream (client → proxy) connection data (F-133) | false |
| `allow_downstream_data_write` | Modify L4 downstream connection data (F-133) | false |
| `allow_upstream_data_read` | Read L4 upstream (proxy → backend) connection data (F-133) | false |
| `allow_upstream_data_write` | Modify L4 upstream connection data (F-133) | false |
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

Use Proxy-Wasm's `dispatch_http_call` to call external HTTP services (e.g., Webdis for Redis).
When a filter returns `Action::Pause` after dispatching a call, Veil resolves the upstream
call inline on the request path — the blocking HTTP client runs on a dedicated offload thread
so the event loop is never blocked — and resumes the same WASM instance via
`proxy_on_http_call_response`. A runnable example lives at
`examples/wasm-filters/http-call-filter/`.

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
