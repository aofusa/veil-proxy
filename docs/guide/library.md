# API Server Library (Rust, C, Python)

[← Documentation index](README.md) · [日本語](ja/library.md)

Besides being a reverse proxy, veil can be embedded in your own program as a **library** for writing API
servers. You register ordinary functions as HTTP handlers (or TCP/UDP handlers), and veil's network layer —
io_uring, TLS, kTLS, HTTP/1.1, HTTP/2, HTTP/3, hot reload, sandboxing — does the rest.

Three front ends share the same implementation:

| Front end | Where | Typical use |
|-----------|-------|-------------|
| Rust library | the `veil` crate with the `api` feature set | the fastest option; macros, OpenAPI, tower, GraphQL |
| C API | [`crates/veil-capi`](../../crates/veil-capi/README.md) (`libveil.so` / `libveil.a`, `veil.h`) | C, C++ and any language with a C FFI |
| Python | [`crates/veil-python`](../../crates/veil-python/README.md) (wheel, `import veil`) | FastAPI-style decorators |

- [Architecture](#architecture)
- [Depending on veil](#depending-on-veil)
- [Configuration in library mode](#configuration-in-library-mode)
- [Routing macros](#routing-macros)
- [Request, Response and errors](#request-response-and-errors)
- [Validation and OpenAPI](#validation-and-openapi)
- [Shared state](#shared-state)
- [tower layers and tokio libraries](#tower-layers-and-tokio-libraries)
- [GraphQL](#graphql)
- [L4 (TCP/UDP) handlers](#l4-tcpudp-handlers)
- [TLS and HTTP/3](#tls-and-http3)
- [Memory allocator](#memory-allocator)
- [C API](#c-api)
- [Python](#python)
- [Building library artifacts](#building-library-artifacts)
- [Testing](#testing)
- [Limitations](#limitations)

## Architecture

- A handler is registered as a `type = "Handler"` route in veil's normal routing table. The library injects
  `[[route]]` entries into the configuration while it is loaded, so handlers coexist with `File`, `Proxy`
  and other route types you write in `config.toml`.
- **Handlers run on veil's own runtime.** Each `async fn` handler is polled on the worker thread that accepted
  the connection, on the same io_uring (or epoll/kqueue) event loop that serves the connection. The data
  plane has no tokio or monoio. If you need a tokio-based library (sqlx, reqwest, ...), use the bridge in
  [tower layers and tokio libraries](#tower-layers-and-tokio-libraries), which keeps tokio out of the data plane.
- Blocking work must not run on a worker. A **synchronous** `fn` handler (Rust), a C callback and a Python
  handler are executed on a separate handler thread pool (`[api] handler_threads`) through veil's `offload`,
  so a slow database call does not stall other connections.
- **The release binary contains no library code.** The `handler`, `macro`, `openapi`, `tokio-interface`,
  `graphql`, `l4-handler` and `api` features are in neither `default` nor `full*`. `cargo build --features full`
  produces exactly the same proxy as before.
- Everything that works for the proxy still works: the `[server]`, `[tls]`, `[security]`, `[logging]`,
  `[prometheus]`, ... sections, SIGHUP reload, graceful shutdown (SIGINT/SIGTERM or `veil::shutdown()`).

## Depending on veil

### Cargo features

| Feature | Adds |
|---------|------|
| `handler` | native `Handler` routes, `Request`/`Response`, configuration builder, library default configuration (implies `http2`) |
| `macro` | `#[veil::route]`, `#[veil::main]`, `#[veil::l4_handler]`, `#[derive(veil::Schema)]` (implies `handler`) |
| `openapi` | OpenAPI 3.1 generation, `/openapi.json` and Swagger UI (implies `macro`) |
| `tokio-interface` | tower `Layer`/`Service` compatibility and the tokio bridge ([`veil::api::tower`], [`veil::api::tokio_compat`]) |
| `graphql` | `App::graphql()` for `async-graphql` schemas |
| `l4-handler` | Rust functions as TCP/UDP handlers on `[[l4]]` listeners (implies `l4-proxy`) |
| `api` | all of the above (`handler`, `macro`, `openapi`, `tokio-interface`, `graphql`, `l4-handler`) |

`vds` and `acme` are separate, are part of `full*`, and work in the proxy binary and the library alike
(see [VDS](vds.md) and [ACME](acme.md)).

### Cargo.toml

veil is not published on crates.io; use a git or path dependency. Pick a feature set: `full` includes
HTTP/3, kTLS, gRPC, WASM and the other proxy features, and `full-container` is the variant for containers.
Turn `default-features` off (see [Memory allocator](#memory-allocator) for how the allocator is chosen).

```toml
[dependencies]
veil = { git = "https://github.com/aofusa/veil-proxy", default-features = false, features = ["full", "api"] }
# or a local checkout:
# veil = { path = "../veil-proxy", default-features = false, features = ["full", "api"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Recommended when HTTP/3 is enabled: use the vendored quiche that veil is built against.
# It packs STREAM frames of several streams into one packet; without it small responses go back
# to one packet per request (F-172). Cargo only honors [patch] in the top-level package.
[patch.crates-io]
quiche = { git = "https://github.com/aofusa/veil-proxy" }
# or: quiche = { path = "../veil-proxy/third_party/quiche" }
```

A minimal feature set for plain HTTP APIs is `features = ["api"]` with `default-features = false` (no TLS,
no HTTP/3). `examples/api/rust-showcase` is a complete project that uses every feature.

### Minimal program

```rust
use veil::prelude::*;

#[veil::route(path = "/users/{id}", method = "GET")]
async fn get_user(id: u64) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "id": id }))
}

#[veil::main]
async fn main() -> Result<(), veil::api::Error> {
    veil::serve(Config::load()?).await
}
```

`veil::serve(config)` blocks the calling thread until shutdown. Without macros, use the `App` builder:

```rust
use veil::prelude::*;

fn main() -> Result<(), veil::api::Error> {
    App::new()
        .get("/hello", |_req: Request| async { "hello" })
        .post("/echo", |req: Request| async move { req.body().clone() })
        .run()
}
```

`App` offers `get/post/put/delete/patch`, `route(&[methods], path, f)`, `route_blocking(...)` (runs on the
handler pool), `state(value)`, `config(cfg)`, `without_macro_routes()` and `on_setup(f)` (runs once before the
routes are registered).

## Configuration in library mode

The library has its own **default configuration** (embedded in the crate, `src/api/default_config.toml`):

```toml
[server]
listen = "0.0.0.0:8080"
h2c_enabled = true     # plaintext HTTP/2 (prior knowledge) ...
h2c_http1 = true       # ... and plaintext HTTP/1.1 on the same port
tls_only = false
http2_enabled = true

[logging]
level = "info"

[security]
enable_seccomp = false # do not restrict the host language / your code
enable_landlock = false

[api]
openapi_enabled = true
openapi_path = "/docs"
openapi_json_path = "/openapi.json"
openapi_title = "veil API"
openapi_version = "0.1.0"
```

plus `[server] threads` = the number of CPUs. So the server starts with no configuration file at all, speaks
plaintext HTTP/1.1 and h2c on `0.0.0.0:8080`, and needs **no TLS certificate** (`[tls]` may be omitted only in
this plaintext-only configuration; see [Configuration](configuration.md#plaintext-only-servers-without-tls)).

### Sources and precedence

The configuration is composed, lowest to highest priority:

1. the library default configuration above;
2. `config.toml` in the current directory, if it exists (`Config::load()` / `App::new()`), or the file given to
   `Config::from_file(path)`, or the string given to `Config::from_toml_str(..)`. The file is deep-merged over
   the defaults, so you only write the keys you want to change;
3. environment overrides `VEIL_OVERRIDE_<n>` (same rules as the proxy, see
   [Running](running.md#configuration-override-via-environment-variables-veil_override_n));
4. overrides set in Rust code with the builder (`ConfigBuilder`).

Then the handler routes and the built-in OpenAPI routes are injected, and all routes are sorted by specificity
(see [Routing macros](#routing-macros)).

The same composition runs again on every SIGHUP reload, so builder and environment overrides **survive a
reload**; only the contents of `config.toml` are re-read. `[server] listen`, `[tls]`, `[[l4]]` and the thread
count still cannot change without a restart (same as the proxy).

### Builder

```rust
let config = Config::load()?            // or Config::new() (defaults only), Config::from_file("app.toml")?
    .builder()
    .listen("0.0.0.0:8080")             // [server].listen
    .threads(4)                         // [server].threads
    .log_level("warn")                  // [logging].level
    .openapi_title("My API")            // [api].*
    .max_body_size(1 << 20)
    .set("security.enable_seccomp = true")?     // any key, same syntax as `--override`
    .build();
```

Typed setters: `listen`, `threads`, `tls_only`, `h2c_enabled`, `http2_enabled`, `log_level`, `openapi_enabled`,
`openapi_path`, `openapi_json_path`, `openapi_title`, `openapi_version`, `max_body_size`, `handler_threads`,
`tls(cert, key)` and `http3(listen)`. `set("key = value")` and `set_value(path, toml_value)` reach every other
key. `Config::to_toml_string()` returns the composed result (before environment overrides and route injection)
for debugging.

### `[api]` keys (library only)

`[api]` is read only in library mode; the proxy binary does not know it.

| Key | Default | Description |
|-----|---------|-------------|
| `openapi_enabled` | `true` | publish `/openapi.json` and the Swagger UI |
| `openapi_path` | `"/docs"` | Swagger UI path (loads `swagger-ui-dist` from unpkg.com in the browser) |
| `openapi_json_path` | `"/openapi.json"` | OpenAPI document path |
| `openapi_title` | `"veil API"` | `info.title` |
| `openapi_version` | `"0.1.0"` | `info.version` (your API's version) |
| `openapi_description` | none | `info.description` |
| `max_body_size` | `10485760` | maximum request body handed to a handler, in bytes |
| `handler_threads` | `0` | threads of the handler pool used for synchronous Rust fns, C callbacks and Python handlers (`0` = built-in default) |
| `tokio_threads` | `2` | worker threads of the tokio compatibility runtime |

## Routing macros

```rust
#[veil::route(path = "/users/{id}", method = "GET", tags = ["users"], summary = "Get a user")]
async fn get_user(id: u64, verbose: Option<bool>) -> Result<Json<User>, ApiError> { ... }
```

| Argument | Meaning |
|----------|---------|
| `path` (required) | path pattern: `{name}` is one segment (non-empty), a trailing `{*name}` captures the rest (may be empty); other segments are exact |
| `method = "GET"` / `methods = ["GET", "POST"]` | allowed methods; omit for all. A `GET` route also answers `HEAD` |
| `name` | handler name (default: the function name); must be unique |
| `tags`, `summary`, `description` | OpenAPI metadata (doc comments are used when `summary`/`description` are absent) |
| `status = 201` | default success status for OpenAPI |
| `offload = true/false` | run on the handler pool; default `true` for a sync `fn`, `false` for `async fn` |
| `hidden = true` | leave the route out of the OpenAPI document |

### Arguments (FastAPI style)

Arguments are bound by type and name:

| Argument | Source |
|----------|--------|
| `&Request` / `Request` | the whole request |
| `Json<T>` | JSON body |
| `Query<T>` | the whole query string deserialized into `T` |
| `Form<T>` | `application/x-www-form-urlencoded` body |
| `State<T>` | shared state registered with `App::state` |
| `Headers`, `Bytes` (alias `Body`) | request headers / raw body |
| a name that matches `{name}` in the path | path parameter, parsed with `FromStr` |
| any other plain type | query parameter; `Option<T>` makes it optional |

Failures to bind (missing, unparsable) and failed validation return **422** in FastAPI's format (see below).

### Ordering and conflicts

`first-match` routing would make route order significant, so the library sorts all routes (macros, `App`, and
`[[route]]` written in `config.toml`) by specificity, most specific first: more path segments, then more static
segments, then exact over prefix/catch-all, then routes with a method condition, then more `host`/`header`/
`query`/`source_ip` conditions; equal routes keep declaration order. `/users/me` beats `/users/{id}`, which
beats `/users/{*rest}`.

Two routes with the **same** host, normalized path (parameter names are ignored) and other conditions, and
overlapping methods (or both without methods) make the later one unreachable; that is a **startup error**
listing both. A path in `config.toml` that points at a handler by name (`type = "Handler"`, `handler = "get_user"`)
replaces the automatically injected route for that handler, so you can add `[route.security]` and other
route settings to it.

> Handler paths are exact matches. A static `path` in a TOML `[[route]]` is a segment-boundary prefix match.

## Request, Response and errors

`Request` gives `method()`, `path()`, `query()`, `query_param(name)`, `header(name)`, `headers()`, `body()`
(`&Bytes`), `text()`, `json::<T>()`, `param(name)`, `param_as::<T>(name)`, `client_ip()`, `version()`
(HTTP/1.1, 2 or 3) and `state::<T>()`.

A handler may return anything that implements `IntoResponse`:

| Return type | Response |
|-------------|----------|
| `&'static str`, `String` | 200 `text/plain` |
| `Html<T>` | 200 `text/html` |
| `Json<T>` (`T: Serialize`), `serde_json::Value` | 200 `application/json` |
| `Bytes`, `Vec<u8>` | 200 binary |
| `()` | 204 |
| `(u16, T)` | status override, e.g. `(201, Json(..))` |
| `Option<T>` | `None` is 404 |
| `Result<T, E>` | both sides are `IntoResponse` |
| `Response` | full control: `Response::new(status)`, `Response::json(&v)`, `.with_status()`, `.with_header()`, `.with_body()` |
| `ApiError` | `{"detail": "message"}` with the given status |

`ApiError::new(status, msg)`, `bad_request`, `not_found` and `internal` are meant for `?`; a `serde_json::Error`
converts to a 400 `ApiError`.

## Validation and OpenAPI

`#[derive(veil::Schema)]` implements a JSON Schema (`ApiSchema`) and a validator (`Validate`) for a struct or
enum. When a `Json<T>`, `Query<T>` or `Form<T>` argument's `T` derives it, the macro validates automatically.

```rust
#[derive(Deserialize, veil::Schema)]
struct CreateUser {
    /// Display name
    #[validate(length(min = 1, max = 50))]
    name: String,
    #[validate(range(min = 0, max = 150))]
    age: u32,
    #[validate(email)]
    email: Option<String>,
    #[schema(example = "https://example.com", description = "Home page")]
    #[validate(url)]
    home: Option<String>,
}
```

| Attribute | Effect |
|-----------|--------|
| `#[validate(range(min, max, exclusive_min, exclusive_max))]` | numeric bounds; also `minimum`/`maximum` in the schema |
| `#[validate(length(min, max, equal))]` | string / collection length; `minLength`/`maxLength` |
| `#[validate(email)]`, `#[validate(url)]` | simple format checks; `format` in the schema |
| `#[validate(nested)]` | validate the inner struct as well |
| `#[validate(custom = "path::to::fn")]` | `fn(&T) -> Result<(), impl Display>` |
| `#[schema(description, example, format, pattern)]` | schema metadata (doc comments also become `description`) |
| serde `rename`, `rename_all`, `default`, `skip`, `flatten` | respected when building the schema |

Unknown `#[validate(...)]` keys (such as `regex`) are accepted and ignored so the derive can be used next to the
`validator` crate.

### 422 format

Failures return status 422 with the same shape as FastAPI:

```json
{"detail": [
  {"type": "less_than_equal", "loc": ["query", "limit"], "msg": "Input should be less than or equal to 100"},
  {"type": "too_short", "loc": ["body", "name"], "msg": "Value should have at least 1 item(s)/character(s)"}
]}
```

`loc` starts with `path`, `query` or `body`, followed by the field path (array indices are numbers).
Typical `type` values: `missing`, `parsing`, `json_invalid`, `greater_than_equal`, `less_than_equal`,
`too_short`, `too_long`, `length`, `value_error`, `url_parsing`.

### OpenAPI and Swagger UI

With the `openapi` feature, an OpenAPI **3.1** document is generated once at startup (and on every reload) from
the macro metadata — paths, parameters, request bodies, response types inferred from the return type
(`Json<T>`, `Result<Json<T>, _>`, `(u16, Json<T>)`, strings), `components.schemas`, and the 422 response for
validated routes — and kept in memory; a request just returns the stored bytes. It is served at
`openapi_json_path` (`/openapi.json`) and a Swagger UI at `openapi_path` (`/docs`). Set `[api]
openapi_enabled = false` to turn both off.

## Shared state

```rust
struct Db { /* pool */ }

#[veil::route(path = "/items", method = "GET")]
async fn items(db: State<Db>) -> Json<Vec<String>> { ... }

App::new().state(Db::connect()?).run()?;
```

`State<T>` is an `Arc<T>`; values are frozen at startup and read without locking. `veil::api::state::<T>()` and
`req.state::<T>()` read it outside extractors. Handlers run on several worker threads, so `T` must be
`Send + Sync` and use interior mutability (atomics, `Mutex`, ...).

## tower layers and tokio libraries

Requires `tokio-interface`. Requests and responses are exchanged as `http::Request<Full<Bytes>>` /
`http::Response`.

```rust
App::new()
    // a tower Layer around every route; runs on veil's worker (do not use tokio timers/IO inside)
    .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(name, value))
    // only routes under /admin (segment-boundary prefix)
    .layer_on("/admin", RequireToken)
    // mount an existing tower Service (e.g. an axum Router) and run it on the tokio runtime
    .tokio_service(&["GET", "POST"], "/legacy/{*rest}", axum_router)
    .run()?;
```

- `layer` / `layer_on` wrap handlers inside veil's event loop. Use them for layers that do not need a tokio
  reactor (authentication, headers, CORS, ...). Layers that use tokio timers (for example tower's `Timeout`)
  need the tokio runtime and must not be used here.
- `tokio_service` runs the service on the tokio compatibility runtime and hands the result back to the worker
  through the same result-slot-plus-notification-fd mechanism as `offload` (no cross-thread `Waker`).
- Inside any handler, call tokio-based libraries through `veil::api::tokio_compat::run`:

  ```rust
  let body = veil::api::tokio_compat::run(async move {
      reqwest::get("https://example.com").await?.text().await
  }).await?;
  ```

  `run` panics (→ 500) if the task panicked; `try_run` returns `Result`; `spawn` starts a detached task and
  `handle()` returns the `tokio::runtime::Handle`. The runtime starts lazily with `[api] tokio_threads` workers
  (default 2). The data plane (accept, TLS, HTTP, handler polling) never runs on tokio.

## GraphQL

Requires `graphql`; bring your own `async-graphql` schema (add `async-graphql` to your `Cargo.toml`).

```rust
let schema = Schema::build(QueryRoot, EmptyMutation, EmptySubscription).finish();
App::new().graphql("/graphql", schema).run()?;
```

`POST` takes a JSON body (single or batch); `GET` takes `?query=...&variables=...`; a `GET` without a query from a
browser (`Accept: text/html`) returns GraphiQL. Responses use `application/graphql-response+json` when the client
asks for it, otherwise `application/json`. Subscriptions are not supported.

## L4 (TCP/UDP) handlers

Requires `l4-handler`. A `[[l4]]` listener with a `handler` key does not forward to upstreams; it passes each
TCP connection (or UDP datagram) to a Rust function. Listeners without `handler` keep the unchanged `splice(2)`
forwarding path. See also [L4 proxy](l4-proxy.md#native-handlers).

```rust
#[veil::l4_handler(name = "tcp_echo", protocol = "tcp")]
async fn tcp_echo(conn: &mut L4Connection) -> Result<(), L4Error> {
    let mut buf = [0u8; 4096];
    loop {
        let n = conn.read(&mut buf).await?;      // 0 = peer closed
        if n == 0 { return Ok(()); }
        conn.write_all(&buf[..n]).await?;
    }
}

#[veil::l4_handler(name = "udp_echo", protocol = "udp")]
async fn udp_echo(dgram: L4Datagram) -> Option<bytes::Bytes> {
    Some(dgram.data)                              // Some(..) is sent back to the sender
}
```

```toml
[[l4]]
name = "tcp_echo"
listen = "0.0.0.0:9000"
handler = "tcp_echo"        # no upstreams needed

[[l4]]
name = "udp_echo"
listen = "0.0.0.0:9001"
protocol = "udp"
handler = "udp_echo"
```

Without writing TOML, `App::l4_tcp(name, listen, f)` and `App::l4_udp(name, listen, f)` register the handler and
inject the `[[l4]]` entry (a `[[l4]]` with the same `name` in `config.toml` wins). `L4Connection` offers `read`,
`read_bytes`, `write`, `write_all`, `write_bytes` (zero-copy `Bytes`), `shutdown_write`, `peer_addr` and
`listener_name`; every operation is bounded by the listener's `idle_timeout_secs` (`L4Error::Timeout`). A handler
name that is missing or registered for the other protocol is a startup error. As for all `[[l4]]` listeners,
SIGHUP does not reload them.

## TLS and HTTP/3

The default is plaintext. To serve HTTPS (HTTP/1.1 + HTTP/2 via ALPN):

```rust
let config = Config::load()?.builder().listen("0.0.0.0:8443").tls("/etc/veil/cert.pem", "/etc/veil/key.pem").build();
```

`tls()` sets `[tls] cert_path/key_path` and turns off the h2c-only mode (`h2c_enabled = false`,
`h2c_http1 = false`). Add `.http3("0.0.0.0:8443")` (needs the `http3` feature, included in `full`) for HTTP/3 on a
UDP port. All other `[tls]`, `[http2]`, `[http3]` and `[security]` keys can be set in `config.toml` or with
`set(..)`. The security sandbox is **off** by default in library mode; enabling `enable_seccomp` /
`enable_landlock` is possible but must allow whatever your handlers do (files, sockets, DNS).

## Memory allocator

When veil is used as a library (any build with the `handler` feature, which `api` includes), it does **not**
declare a `#[global_allocator]`: the application chooses. The allocator features (`veil-alloc` in `full`,
`mimalloc` in `default`) still compile the allocator code, so you can pick it explicitly in your binary:

```rust
// veil's own allocator (unix; `veil-alloc` is part of `full`)
#[global_allocator]
static GLOBAL: veil::veil_alloc::VeilAlloc = veil::veil_alloc::VeilAlloc;

// or mimalloc (add the `mimalloc` crate to your dependencies)
// #[global_allocator]
// static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
```

Without any declaration the Rust system allocator is used. The `veil` binary (built without `handler`) keeps
selecting its allocator as before. `examples/api/rust-showcase` declares `VeilAlloc`.

Allocator features are mutually exclusive (see [Memory allocators](performance.md#memory-allocators-f-183)).
The C and Python builds ([`veil-capi`](../../crates/veil-capi/README.md), [`veil-python`](../../crates/veil-python/README.md))
enable no allocator feature, so they never replace the allocator of the host process (a C program or the Python
interpreter). They are also built on the epoll (readiness reactor) backend rather than io_uring, and without the
gRPC, WASM, VDS and ACME features.

## C API

`crates/veil-capi` builds `libveil.so` (dynamic) and `libveil.a` (static) and the header
[`veil.h`](../../crates/veil-capi/include/veil.h). A C callback is registered per route and called with a
request and a response object:

```c
#include "veil.h"

static int hello(const veil_request_t *req, veil_response_t *resp, void *ud) {
    return veil_response_set_text(resp, "Hello from C");
}

int main(void) {
    veil_config_t *cfg = veil_config_new();           /* or veil_config_from_file("config.toml") */
    veil_config_listen(cfg, "0.0.0.0:8080");
    veil_register_route(cfg, "/hello", "GET", hello, NULL);
    return veil_run(cfg);                              /* blocks; consumes cfg */
}
```

```bash
cargo build -p veil-capi --release      # target/release/libveil.so and libveil.a
gcc hello.c -Icrates/veil-capi/include -Ltarget/release -lveil -lpthread -ldl -lm \
    -Wl,-rpath,$PWD/target/release -o hello          # dynamic
gcc hello.c -Icrates/veil-capi/include target/release/libveil.a -lpthread -ldl -lm -o hello   # static
```

**Threading.** By default the callback runs on the **handler pool** (`[api] handler_threads`), so it may block
(database, files), but it is called from **several threads at once** — protect shared data and make `user_data`
thread-safe. `veil_register_route_ex(..., VEIL_ROUTE_INLINE)` runs the callback on the network worker instead
(no thread hop); only use it for work that finishes immediately, because blocking stalls every connection of that
worker. Request accessors return non-NUL-terminated pointers valid until the callback returns. OpenAPI is
generated for C routes too (pass an Operation Object as JSON to `veil_register_route_ex` to describe them).
Settings are applied with `veil_config_set(cfg, "server.threads", "4")`, and `VEIL_OVERRIDE_<n>` works as usual.
Details (ownership table, error codes, API list): [crates/veil-capi/README.md](../../crates/veil-capi/README.md).
Example: [examples/api/c-hello/](../../examples/api/c-hello/).

## Python

`crates/veil-python` is a PyO3 extension (abi3, Python 3.8+) imported as `veil`:

```bash
pip install maturin
cd crates/veil-python && maturin develop --release     # or: maturin build --release   (wheel in target/wheels/)
pip install pydantic                                    # optional
```

```python
import veil

@veil.get("/hello/{name}")
def hello(name: str, greeting: str = "Hello"):          # name: path, greeting: query (typed by annotation)
    return {"message": f"{greeting}, {name}"}

@veil.post("/users", status_code=201,
           body_constraints={"name": {"type": "string", "minLength": 1, "required": True}})
def create_user(body: dict):
    return body

veil.run(listen="0.0.0.0:8080")                         # blocks; Ctrl-C / SIGTERM stops gracefully
```

- Decorators: `@veil.get/post/put/delete/patch/route`, or `app = veil.App(title=..., version=...)` with
  `@app.get(...)` and `app.run()`. Arguments are bound by name and annotation, returns are converted
  (dict/list → JSON, `str` → text, `veil.Response`, `(body, status)`, `None` → 204), `veil.HTTPException(404, "...")`
  gives `{"detail": ...}`. With **pydantic** installed, `BaseModel` bodies are validated and described in OpenAPI;
  without it, `body_constraints` validates dict bodies. OpenAPI is served at `/openapi.json` and `/docs`.
- **offload and the GIL.** Handlers hold the GIL while they run. By default (`offload=True`) they run on the
  handler pool, so the event loop never waits for the GIL. `offload=False` runs on the network worker and is for
  trivially cheap handlers only. Pure-Python handlers are limited to the GIL's parallelism; for CPU-bound work use
  several processes.
- Configuration: `veil.run(config_path=None, *, listen, threads, overrides={"server.threads": 4}, openapi=True)`;
  `VEIL_OVERRIDE_<n>` works as usual.

Details: [crates/veil-python/README.md](../../crates/veil-python/README.md). Examples:
[examples/api/python-hello/](../../examples/api/python-hello/),
[examples/api/python-fastapi-like/](../../examples/api/python-fastapi-like/).

## Building library artifacts

The Rust library is just a Cargo dependency. For the C and Python artifacts:

```bash
packaging/scripts/build-lib.sh [--kind all|rust|c|python] [--docker]
```

builds the selected kinds (`all` is the default; `--docker` runs the build in a container using
`docker/Dockerfile.lib`) and writes a single archive `veil-lib-<version>-<arch>-linux-gnu.zip` to
`packaging/output/lib/`. It contains `libveil.so`/`libveil.a` with `veil.h` (C), the Python wheel, and the
Rust source/usage notes. See [Building](building.md) for the proxy packages.

## Testing

```bash
tests/api/library_e2e.sh [rust|c|python|nonregression|all]     # default: all
```

builds and starts the Rust showcase, the C example (dynamic and static) and the Python examples and calls them
with curl (routing, validation, OpenAPI, tower, GraphQL, L4 echo, HTTP/1.1 and h2c), and the `nonregression` step
checks that the proxy binary is unaffected. Needs cargo, gcc, curl, nc and python3. Unit tests: `cargo test
--lib --features full,api`, `cargo test -p veil-capi`, and `tests/test_binding.py` for Python. See also
[Testing](testing.md).

## Limitations

- `serve` / `run` can be called **once per process** (configuration hooks and shared state are process-wide); a
  second call returns an error.
- GraphQL subscriptions are not supported.
- Python handlers are bound by the GIL; C callbacks must be thread-safe.
- The Swagger UI page loads `swagger-ui-dist` from a CDN, so the browser needs internet access to render `/docs`
  (`/openapi.json` works offline).
- Library mode does not use a configuration file in `/etc/veil`: `config.toml` in the current directory (or the path
  you pass) is the only file.
