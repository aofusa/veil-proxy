# WASM Extensions (Proxy-Wasm)

[← Documentation index](README.md) · [日本語](ja/wasm.md)

## WASM Extension System

Veil provides a WASM extension system fully compliant with Proxy-Wasm ABI v0.2.1. Proxy-Wasm modules created for Nginx/Envoy can be used with Veil without modification.

### Features

- **Proxy-Wasm v0.2.1 Compliant**: 100% compatible with Nginx/Envoy
- **AOT Compilation & Auto-Cache**: Modules are AOT-compiled; a sidecar named after the module's content (`<module>.wasm.<hash>.cwasm`, the xxh3-128 of the `.wasm`) is generated next to each `.wasm` on first load and reused via `deserialize` on subsequent startups. A changed `.wasm` gets a new cache whatever its mtime, so restoring an older module (`cp -p`, tar, a package downgrade) never loads the newer module's native code; the cache of the previous content is removed once the new one is written. A cache from another wasmtime version is recompiled, and any error falls back to recompilation. veil no longer reads the old `<module>.wasm.cwasm` name and does not remove it; delete it unless the configuration names it. Explicit `.cwasm` paths are also supported.
- **Pooling Allocator**: High-speed instance creation
- **Async Execution (no Head-of-Line blocking)**: Modules run on wasmtime async support with cooperative yielding (a callback that keeps running yields about every millisecond of run time, `yield_interval_us`; with `metering = "clock"` the first yield comes within about 10 ms of its start), so a CPU-heavy filter cannot stall the io_uring worker's other I/O. With the native JIT the run time is measured with wasmtime epochs and the thread's CPU time (`[wasm] metering = "clock"`), which adds no per-instruction accounting to the guest code
- **Capability Restrictions**: Fine-grained per-module permission control (all disabled by default)
- **Optional Pulley Interpreter** (`[wasm] interpreter = true`, F-135): runs Wasm through wasmtime's Pulley portable-bytecode interpreter instead of the Cranelift native JIT, emitting no native code at all. Useful on hosts with W^X constraints or where executable `mmap` cannot be granted. Slower than the native JIT. Default is `false` (Cranelift JIT). The AOT sidecar cache uses a distinct filename (`.pulley.cwasm`) so JIT and Pulley builds never fight over — or invalidate — the same cache file. **On OpenBSD (any arch), NetBSD (any arch), and FreeBSD aarch64 this setting is always ignored and Pulley is always used** (B-52/B-55; explicitly setting `interpreter = false` logs a startup warning and is otherwise ignored)
- **HTTP/3 parity with HTTP/1.1/HTTP/2** (F-132): the HTTP/3 path now runs the full filter lifecycle — `on_log` fires from every exit point (including early `LocalResponse` returns), `Backend::File` static-serving routes also get the `on_response_headers` filter applied, and `Backend::Proxy` routes run request/response body filters (`on_request_body`/`on_response_body`, applied once with `end_of_stream=true` since HTTP/3 already buffers the whole body when a WASM module is configured). A response body rewrite updates `content-length` to match, avoiding the H3 message-framing error nghttp3 raises on a mismatch/duplicate.
- **gRPC trailer filtering** (F-133, F-313): `on_request_trailers`/`on_response_trailers` run over HTTP/2 and HTTP/3 alike, so a module can rewrite `grpc-status`/`grpc-message` on the trailers of a response from an HTTP/2 upstream (h2c, or h2 over TLS), or reject a request based on client-sent request trailers (via `LocalResponse`). Request trailers are only observed this way (there is no client-trailer forwarding to the backend); response trailer `Pause`/`LocalResponse` is not applied on either protocol (over HTTP/2 the HEADERS/DATA are already sent by that point), so the original trailers are kept and a warning is logged.
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

# How the CPU budget and the yield interval are measured: "clock" (wasmtime epochs and the
# thread's CPU time) or "fuel" (wasmtime fuel, counts guest instructions but runs the guest slower).
# Default: "clock", or "fuel" when Wasm runs on the Pulley interpreter
# metering = "clock"

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
| `on_http_request_trailers` | – | requests with trailers | requests with trailers |
| `on_http_response_headers` | yes | yes | yes |
| `on_http_response_body` | yes | yes | yes |
| `on_http_response_trailers` | – | HTTP/2 upstream (h2c, h2) | HTTP/2 upstream (h2c, h2) |
| `on_log` | yes | yes | yes |

A local response (`send_local_response`) from any request phase or from `on_http_response_headers` replaces the
upstream response; veil does not read the rest of the upstream response and does not reuse that upstream connection.
A local response from `on_http_response_trailers` cannot be sent because the response head is already out over
HTTP/2; HTTP/3 does the same, and the original trailers are kept. `on_log` runs after a local response too.
The request phases run in Proxy-Wasm order (headers, body, trailers) on every protocol.

The request-body phase runs when the request has a body and at least one of the route's modules has
`allow_request_body_read = true`. veil reads the whole body first (Content-Length or chunked, up to
`max_request_body_size` / `max_chunked_body_size`; larger bodies get `413`) and calls
`on_http_request_body` once with `end_of_stream = true`. The callback can read the request path, method
and headers as well. If the module replaces the body, veil sends the new body upstream with a matching
`Content-Length` (a chunked request is forwarded with `Content-Length`, without `Expect`). Routes without such a
module keep streaming the body upstream as before. For a chunked request with `Expect: 100-continue`, veil sends
`100 Continue` before reading the body (a request with `Content-Length` already gets it before the filters run).

The response-body phase runs when at least one of the route's modules has `allow_response_body_read = true`. veil
reads the whole upstream response first (HTTP/1.1, h2c and HTTPS upstreams; on HTTP/1.1 clients up to the route's
`[route.buffering] max_memory_buffer`, default 10 MB, larger responses get `507` unless `disk_buffer_path` is set, in
which case they are sent unfiltered) and calls `on_http_response_body` once with `end_of_stream = true`. A replaced body is
sent with a matching `Content-Length`, and `[route.compression]` compresses the filtered body. A local response from this
phase replaces the upstream response. The phase is skipped for `HEAD`, `1xx`, `204`, `304` and gRPC responses, and
responses of such routes are not stored in `[route.cache]` (a cache hit would skip the filter). Routes without such a module
keep streaming responses.

On HTTP/1.1 the connection stays open after a filter's local response (`send_local_response`), and a request that
was pipelined behind the body is served next. The body of a local response is not sent for `HEAD` (the
`Content-Length` stays the body's length) or for `1xx`, `204` and `304`.

### Pseudo-headers and Request Properties

As in Envoy, the request header map shows `:authority`, `:path`, `:method` and `:scheme` before the other headers, and
the response header map shows `:status`, on HTTP/1.1, HTTP/2, HTTP/3 and WebSocket over HTTP/2 and HTTP/3, so filters
written for Envoy can read `get_http_request_header(":path")`. `:path` includes the query string. `:authority` is the
request's `:authority`, or the `Host` header on HTTP/1.1 (missing for an HTTP/1.0 request without `Host`). `:scheme` is
`https` on TLS connections and `http` on cleartext HTTP/1.1; on HTTP/2 it is the request's own `:scheme`. A WebSocket
request over HTTP/2 and HTTP/3 (extended CONNECT) shows `:method` as `GET`, the method veil uses for the upstream.
The response phases and `on_log` see the request's pseudo-headers too, but not its other headers; a filter that needs
them keeps them in its HTTP context with `instance_reuse = true`.

Pseudo-headers are read-only. Replacing, adding or removing one returns `BadArgument`, so a filter cannot rewrite the
path, method, authority or status. Setting one to its current value is accepted and changes nothing, so a filter can
write back a whole map it read with `set_http_request_headers`. veil never forwards these entries as headers.

The properties `request.host` (the same value as `:authority`), `request.scheme` and `request.protocol` (`HTTP/1.0`,
`HTTP/1.1`, `HTTP/2` or `HTTP/3`) follow the same rules.

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

[wasm.modules.capabilities]
allowed_properties = ["plugin_data.*"]
```

Each file is read when the configuration is loaded (startup and reload) and exposed to the filter as the
property `plugin_data.<name>` (`get_property(["plugin_data", "dataset"])`). Nothing is read from disk per
request. A missing file is a configuration error. Like every property, it is readable only when the module's
`allowed_properties` includes it (`"plugin_data.*"` or `"plugin_data.dataset"`).

### Default Settings

The `[wasm.defaults]` section allows you to configure global WASM runtime settings:

| Option | Description | Default |
|--------|-------------|---------|
| `max_execution_time_ms` | CPU budget of one callback in milliseconds: the CPU time it may use, not counting the time it yielded to other requests or waited for another thread (with `metering = "fuel"`, measured in fuel, about 1M per millisecond). The startup callbacks of a new instance (`_start`, `proxy_on_vm_start`, `proxy_on_configure`) get their own budget of ten times this value | 100 |
| `yield_interval_us` | How often a running callback yields the worker to other requests (approximate microseconds). With the default `metering = "clock"` it is also the period of the thread that advances the epoch while a callback has been running for more than 5 ms, kept between 100 µs and 100 ms; until a callback runs that long the thread advances the epoch every 10 ms, so a long callback yields for the first time within about 10 ms of its start and every `yield_interval_us` after that. With `"fuel"` it is measured in fuel. Smaller values shorten the wait of other requests on the same worker behind a CPU-heavy filter, but every callback pauses more often, more `instance_reuse` instances are open at once, and each request costs more | 1000 |
| `interpreter` | Run Wasm via the Pulley interpreter instead of the Cranelift native JIT (F-135). Always forced to `true` on OpenBSD (B-52) | false |
| `metering` | How `max_execution_time_ms` and `yield_interval_us` are measured. `clock`: a thread advances the wasmtime epoch every 10 ms while filters run, every `yield_interval_us` while a callback has been running for more than 5 ms, and every 100 ms when no filter has run for a while (each period at most a quarter of `max_execution_time_ms`); a running callback checks it at function entries and loop back-edges and adds the CPU time its thread used since the last check, so time taken by other threads on a busy machine or spent waiting for the disk on a page fault is not charged (on Windows, which has no precise thread CPU clock, the wall time between checks, up to 10 ms per check). Each wake of the thread takes a CPU from a worker, which is why it runs at `yield_interval_us` only for long callbacks: waking it every millisecond while requests flow cost about 15% more CPU per Wasm request with the epoll backend in veil's measurements. `fuel`: wasmtime fuel counts guest instructions, which makes the guest code run slower (3–14% more CPU per Wasm request in veil's measurements) and is needed for `veil_wasm_fuel_consumed_total`. The default is `clock` with the native JIT and `fuel` on the Pulley interpreter (`interpreter = true`, and always on OpenBSD, NetBSD and FreeBSD aarch64), where guest code is 10–30 times slower and a time budget would cut off modules that fit in fuel. An engine setting: a module given as an explicit `.cwasm` must be compiled with the same metering, and changing it recompiles the sidecar caches | `clock` (`fuel` on Pulley) |
| `grpc_connect_timeout_ms` | Upper bound on connecting (TCP and TLS) to a gRPC upstream for `proxy_grpc_call`/`proxy_grpc_stream`. A longer guest `timeout_ms` still gives up connecting after this time, so an unreachable upstream does not stall every plugin's gRPC calls. At most 4096 calls and 4096 stream messages wait for the gRPC thread; beyond that the host call returns `InternalFailure` | 5000 |
| `http_call_max_timeout_ms` | Upper bound on the `timeout_ms` a plugin passes to `proxy_http_call` (a negative value counts as 0) | 60000 |
| `shared_data_max_keys` | Maximum number of keys in one module's shared data; `proxy_set_shared_data` with a new key beyond it returns `InternalFailure` (existing keys can still be updated) | 100000 |
| `failure_policy` | What to do when a filter fails: `fail_closed`, `fail_open` or `fail_reload` (see [Failure Handling](#failure-handling)). A module's own `failure_policy` in `[[wasm.modules]]` overrides it | `fail_closed` |

### Failure Handling

A filter fails when a callback traps (a panic in the SDK, `proc_exit`, `unreachable`), runs out of its CPU budget
(`max_execution_time_ms`), runs past about one second of wall time including the time it yielded to other requests
(about 60 seconds for the startup callbacks of a new instance, which run once per instance and are bounded by ten
times the CPU budget, so a module that builds large tables at startup is not rejected just because the server is busy),
when no instance can be created (all `total_memories` slots stay busy for about 130 ms), when the startup callbacks
of a new instance fail, when the `proxy_on_http_call_response` that resumes a paused request traps, or when a
callback returns `Pause` with nothing veil can wait for (see below). `failure_policy` decides what happens to the
request:

| `failure_policy` | Request | Instance |
|---|---|---|
| `fail_closed` (default) | `503` (gRPC clients see `UNAVAILABLE`) | discarded; the next request gets a new one |
| `fail_open` | The failed module is skipped and the request continues with the next module. Header changes the module made before it failed are kept | discarded |
| `fail_reload` | `503` | discarded, and every reused instance of the module (`instance_reuse`) is recreated before its next use |

Unlike Envoy's `FAIL_CLOSED`, one failed request does not make later requests fail: veil always starts the next
request on a healthy instance. A local response the filter set before it failed is always sent, whatever the policy.
Each failure counts in `veil_wasm_failures_total{filter, phase, reason, action}` (`reason` is `trap`, `fuel`,
`timeout`, `instantiate`, `lifecycle`, `http_call_resume` or `unsupported_pause`; `action` is `rejected` or
`continued`) and is logged as a warning (rate-limited).

`Pause` is honored when veil can wait for what the filter waits for: a `dispatch_http_call` made in any phase
(resolved inline, see [External Service Integration](#external-service-integration-http-calls)), or the request body
when `on_http_request_headers` returns `Pause` before the body (`end_of_stream = false`) and the module reads the body
(`allow_request_body_read`); the same for the response. A `Pause` that waits for a `proxy_grpc_call`/`proxy_grpc_stream`,
or that has nothing to wait for, is an `unsupported_pause` failure. After a `dispatch_http_call`, the request goes on
only when `proxy_on_http_call_response` resumes it (`resume_http_request`, i.e. `proxy_continue_stream`) or sends a
local response; a filter that does neither, or that still has calls pending after `max_http_calls` rounds, stays paused
and is handled as above (before 0.11.0 such a request went on). WebSocket upgrades (HTTP/1.1 and the extended CONNECT of
HTTP/2 and HTTP/3) run no body phase, so `on_http_request_headers` sees `end_of_stream = true` there.

L4 network filters (`[[l4]] wasm_modules`) follow the same `failure_policy`: under `fail_closed` (the default) a failing
`proxy_on_new_connection`, `proxy_on_downstream_data` or `proxy_on_upstream_data` closes the connection (`phase` is
`new_connection`, `downstream_data` or `upstream_data`); under `fail_open` the module is skipped. When
`proxy_on_downstream_data`/`proxy_on_upstream_data` returns `Pause`, veil does not forward the bytes: it holds them and
passes them again, followed by the next bytes read, on the next call. If more than 1 MiB is held, the connection is
closed. Before 0.11.0, L4 filters always continued after a failure and forwarded the bytes on `Pause`.

At startup and on reload veil runs each module's `_start`, `proxy_on_vm_start` and `proxy_on_configure` once. If one
traps or returns `false` (for example the plugin rejects its configuration), the configuration is rejected: startup
fails, and a reload keeps the previous configuration.

A reload loads the modules again from the new `[wasm]` section, so modules (and `data_files`) that the reload adds or
changes take effect from the next request; requests already in progress finish with the previous modules. Routes
and `[[l4]]` listeners may list only modules defined in an enabled `[wasm]` section; any other name rejects the
configuration. On FreeBSD in capability mode veil cannot open `.wasm` files after startup, so it rejects a reload
that changes `[wasm]` (restart to apply such changes).

On Linux with `[security] seccomp_mode = "filter"` or `"strict"`, veil cannot create executable memory after startup,
so a reload cannot compile modules to native code. If the reloaded `[wasm]` section and the module and `data_files`
files are unchanged, the reload keeps the running modules (it does not run `_start` or `proxy_on_configure` again)
and applies the rest of the configuration. A reload that changes `[wasm]` or one of those files is rejected and the
previous configuration stays in effect; restart veil to apply it. With `interpreter = true` the modules need no
executable memory, so such a reload works as usual. With `seccomp_mode = "log"` veil makes the same decision but lets
a reload that has to compile modules through and logs a warning, so the kernel log shows only the compilations that
`filter` and `strict` would reject.

Versions before 0.11.0 always continued after a failure (as `fail_open` does now, without the metric). Set
`failure_policy = "fail_open"` in `[wasm.defaults]` to keep that behavior.

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
| `allow_logging` | Log output (`proxy_log`; control characters in the message, such as CR and LF, are written as `\n`, `\r`, `\u{1b}` so that logged client data cannot add lines) | false |
| `allow_metrics` | Metrics operations | false |
| `allow_shared_data` | Shared data access (`proxy_set_shared_data`/`proxy_get_shared_data`). One table per module name, shared by every request and worker and kept across config reloads (Envoy shares it per `vm_id`). Also required for shared queues: a queue belongs to the module that registered it and only that module dequeues from it; another module enqueues only after resolving the queue by the owner's module name (`vm_id`) | false |
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
| `allowed_upstreams` | Upstreams `proxy_http_call`/`proxy_grpc_call` may call. Empty allows every upstream group | [] |
| `allowed_properties` | Properties `proxy_get_property` may read: exact paths (`"source.address"`), prefixes (`"request.*"`) or `"*"`. Empty allows none, and a property that is not allowed reads as not found. veil provides `request.path`, `request.url_path`, `request.host`, `request.method`, `request.scheme`, `request.protocol`, `request.query`, `request.time`, `request.total_size`, `response.code`, `response.code_details`, `source.address`, `destination.address`, `plugin_name`, `plugin_root_id` and `plugin_data.<name>` | [] |
| `allow_property_write` | Set custom properties with `proxy_set_property` | false |

A host function called without its capability returns `NotAllowed`. SDKs that unwrap the status (the Rust SDK
`proxy-wasm` does for most calls) then panic, the callback traps, and the request is handled by `failure_policy`
(by default `503`). veil warns at load time when a module imports `proxy_send_local_response`, `proxy_http_call`,
`proxy_grpc_call`/`proxy_grpc_stream`, `proxy_define_metric`, the shared data functions or `proxy_set_property`
without the matching capability.

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
When a filter returns `Action::Pause` after dispatching a call (in any HTTP phase), Veil resolves the upstream
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

Veil sends the call as HTTP/1.1 and sets `Host`, `Content-Length` and `Connection` itself; `host`,
`content-length`, `transfer-encoding` and `connection` headers passed by the filter are dropped. A call whose
`:method`, `:path` or header names and values could split the request (CR, LF, NUL, a space in `:path`, a name that
is not a token) is not sent: the filter receives a `504` response with `x-wasm-error: http_call_failed`. The body
is sent byte for byte.
