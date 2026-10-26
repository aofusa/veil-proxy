# Helper crate `veil-proxy-kit` (Rust)

[← Documentation index](README.md) · [日本語](ja/library-kit.md) · [API server library](library.md)

`veil-proxy-kit` (library name `veil_kit`) adds the pieces most web APIs need on top of the
[API server library](library.md): cookies, redirects, static files, CORS, authentication (Basic, API keys, JWT),
sessions, HTTP and gRPC client helpers, GraphQL, dependency injection and background tasks. It is a separate crate:
the proxy binary, veil's default features and the `full*` sets do not include it, and heavy dependencies
(async-graphql, prost) sit behind its own features.

For Python, the package `veil-api-kit` (`import veil_kit`) has the same parts with FastAPI's names; see
[library.md#python](library.md#python) and the `veil-api` documentation (<https://veil-api.readthedocs.io>, "Kit").

```toml
[dependencies]
veil = { package = "veil-proxy", version = "0.9", default-features = false, features = ["full", "api"] }
veil-kit = { package = "veil-proxy-kit", version = "0.9", features = ["full"] }
```

| Module | Feature | Contents |
|---|---|---|
| `cookie` | `cookie` (default) | `CookieJar` extractor, `Cookie` builder (`Secure`, `HttpOnly`, `SameSite`, `Max-Age`, `Partitioned`, RFC 6265bis checks, `__Secure-`/`__Host-` prefixes), `SignedCookieJar` (HMAC-SHA256), `PrivateCookieJar` (AES-256-GCM), `Key` with rotation |
| `redirect` | always | `Redirect::to / see_other / temporary / permanent / moved`, `Redirect::safe(url, allowed_hosts)` against open redirects |
| `serve` | always | `serve::dir(prefix, dir)` (veil's file server), `serve::file(path, max)` from a handler |
| `cors` | `cors` (default) | `Cors` middleware with automatic preflight answers |
| `auth` | `auth` (default), `jwt` | `BasicAuth`, `BearerToken`, `ApiKey<H>`, `Jwt<Claims>`, `Principal`, `Auth<T>`, `authenticate`, `require`, `OAuth2PasswordBearer` |
| `session` | `session` | `Sessions` (encrypted cookie or server store), `Session` extractor, `MemoryStore`, `SessionStore` |
| `client` | always | `veil::api::client` re-exported, `get_json`, `post_json`, `retry` |
| `grpc` | `grpc` | `ProstCodec`, `connect`, `Balanced` (round robin and failover) |
| `graphql` | `graphql`, `graphiql` | async-graphql 7 over HTTP, SSE and WebSocket (`graphql-transport-ws`, `graphql-ws`) |
| `di` | `di` (default) | `Container` (singleton, scoped, transient), `Dep<T>`, `Resolver`, run-time replacement |
| `tasks` | `tasks` (default), `cron` | `BackgroundTasks` (after the response), `Tasks::spawn / every / cron` |
| `full` | – | all of the above |

The examples [`examples/api/rust-kit`](../../examples/api/rust-kit/) (every module, including gRPC server and client
and GraphQL subscriptions) and [`examples/api/rust-tower-recipes`](../../examples/api/rust-tower-recipes/) (the same
tasks with tower-http only) are run by `tests/api/library_e2e.sh kit`.

## Extractors and plugins

Kit extractors are handler parameters like veil's own. The `#[veil::route]` macro recognizes them by name
(`CookieJar`, `SignedCookieJar`, `PrivateCookieJar`, `BasicAuth`, `BearerToken`, `ApiKey`, `Jwt`, `Auth`,
`MaybeAuth`, `Session`, `Dep`, `BackgroundTasks`); write `#[inject]` in front of a parameter whose type implements
`veil::api::FromRequestAsync` under another name:

```rust
struct UserAgent(String);
impl veil::api::FromRequestAsync for UserAgent {
    async fn from_request_async(req: &Request) -> Result<Self, Response> {
        Ok(UserAgent(req.header("user-agent").unwrap_or("-").into()))
    }
}

#[veil::route(path = "/me", method = "GET")]
async fn me(user: Auth<Principal>, #[inject] ua: UserAgent) -> String { .. }
```

Middleware and hooks install with `App::with(plugin)` (`Cors`, `Container`, `Tasks`, `Sessions`,
`serve::dir(..)`, `graphql::mount(..)`), or with `App::wrap_on(prefix, ..)` for the functions in `auth`.

## Cookies and redirects

```rust
use veil_kit::cookie::{Cookie, CookieJar, Key, ResponseCookieExt, SameSite, SignedCookieJar};

#[veil::route(path = "/visit", method = "GET")]
async fn visit(jar: CookieJar) -> Response {
    let n: u32 = jar.get("visits").and_then(|v| v.parse().ok()).unwrap_or(0) + 1;
    Response::text(format!("visit {}", n))
        .with_cookie(Cookie::new("visits", n.to_string()).http_only(true).same_site(SameSite::Lax))
}

#[veil::route(path = "/who", method = "GET")]
async fn who(jar: SignedCookieJar) -> String { jar.get("user").unwrap_or_default() }

App::new().state(Key::derive_from(secret_bytes)?)   // signed and private jars read the key from App::state
```

- `CookieJar` scans the `Cookie` headers once; values are slices of the header (`Bytes`), not copies.
- `Cookie::to_header()` rejects invalid names and values (CR/LF, `;`, spaces), enforces the `__Secure-` and
  `__Host-` prefixes, caps `Max-Age` at 400 days, and `SameSite::None` / `partitioned(true)` add `Secure`.
  `with_cookie` logs and drops an invalid cookie instead of sending it.
- `Key::from(&[u8; 64])` (32 bytes sign, 32 bytes encrypt), `Key::derive_from(secret)` (at least 32 bytes) or
  `Key::generate()`. `key.with_fallback(&old)` still accepts cookies made with the old key.
- Signed values are `base64url(HMAC-SHA256(name=value)).value`; private values are AES-256-GCM with the name as
  associated data, so a cookie cannot be moved to another name. The crypto comes from the same provider crate as
  veil's TLS (aws-lc-rs, or ring on OpenBSD and NetBSD).

`Redirect` implements `IntoResponse`. `Redirect::safe(next, &["app.example.com"])` returns `None` unless `next` is a
path on this site (not `//host` or `/\host`) or an `http(s)` URL whose host is listed (user info is rejected).

## Static files

`serve::dir("/assets", "/srv/www")` is `App::serve_dir` with the defaults: a `type = "File"` route with
`open_file_cache` and `static_file_cache`. Requests do not go through a handler; sendfile, kTLS, Range and ETag
work as for the proxy. Precompressed `.br` / `.gz` files are not used; `compression` compresses files on the fly for
HTTP/2 and HTTP/3 (HTTP/1.1 sends them as they are). `serve::dir_with(prefix, dir, ServeOptions { .. })` takes
`index`, `mode`, the two caches, `compression` and extra route keys. The File backend has no SPA fallback; serve an `index.html` route
with a handler if you need one.

`serve::file(path, max_size).await` reads one file on the file offload pool and answers with a media type from the
extension (404, 403 or 413 otherwise). Use it for small files a handler chooses, never with an unchecked request path.

## CORS

```rust
App::new().with(
    Cors::new()
        .allow_origins(["https://app.example.com", "https://*.example.org"])
        .allow_methods(["GET", "POST"])
        .allow_headers(["content-type", "authorization"])
        .allow_credentials(true)
        .max_age(Duration::from_secs(600))
        .on("/api"),
)
```

- Preflights (`OPTIONS` with `Access-Control-Request-Method`) are answered with 204 by the middleware; the plugin
  also registers an `OPTIONS` catch-all under the prefix so that preflights reach it.
- Origins match exactly or with one leading `*.` label (subdomains only). Other origins get no CORS headers.
- `allow_any_origin()` with `allow_credentials(true)` is a startup error (as are origins without a scheme).
- Header values are built once at startup. `Vary: Origin` is added unless any origin is allowed.
- This is independent of `[route.traffic.cors]` for proxy routes; do not configure both on the same paths.

## Authentication and authorization

```rust
use veil_kit::auth::{authenticate, require, Auth, BearerToken, Principal};
use veil_kit::auth::jwt::{Claims, Jwt, JwtKey, JwtVerifier};

App::new()
    .state(JwtVerifier::new([JwtKey::hs256(secret)]).issuer("https://auth.example.com").audience("api"))
    .wrap_on("/admin", require(|p: &Principal| p.has_scope("admin")))
    .wrap_on("/", authenticate(|req: &Request| {
        let Ok(t) = BearerToken::from_headers(req) else { return Ok(None) };
        let v = req.state::<JwtVerifier>().unwrap();
        Ok(v.verify::<Claims>(&t.0).ok().map(|c| c.to_principal()))
    }))
```

- `BasicAuth` (`matches(user, password)` compares in constant time), `BearerToken`, `ApiKey` (`x-api-key`, or any
  header through a `KeyHeader` type; `is_one_of(&keys)`): 401 with `WWW-Authenticate` when missing.
- `authenticate(f)` stores the principal returned by `f` in the request's extensions (`Request::extensions()`);
  `Auth<T>` reads it (401 when absent), `MaybeAuth<T>` makes it optional; `require(check)` answers 401 without a
  principal and 403 when the check fails. Register `require` before `authenticate`: the middleware registered last
  runs first.
- `Jwt<C>` (feature `jwt`) verifies `Authorization: Bearer` with the `JwtVerifier` in `App::state`: `HS256/384/512`,
  `RS256/384/512`, `PS256/384/512`, `ES256`, `ES384`, `EdDSA`. Each key fixes its algorithm (a token whose `alg`
  differs is rejected; `none` never passes), `kid` selects the key, `exp`/`nbf` have 60 s leeway, `iss`/`aud` are
  checked when configured. Keys come from secrets, PEM (`PUBLIC KEY`, `RSA PUBLIC KEY`), JWKs or a JWK Set;
  `verifier.refresh_jwks(&client, url).await` replaces them (schedule it with `Tasks::every`). `encode_hmac` issues
  HMAC tokens. No `jsonwebtoken` dependency: verification uses the TLS provider crate.
- `OAuth2PasswordBearer::new("/token")` reads the bearer token and gives the OpenAPI `securitySchemes` entry
  (`security_scheme()`); veil's generated OpenAPI does not add it by itself.

## Sessions

```rust
App::new().with(Sessions::new(Key::from(&material)).store(MemoryStore::new()).on("/app"))

#[veil::route(path = "/app/count", method = "GET")]
async fn count(session: Session) -> String {
    let n: u64 = session.get("n").unwrap_or(0) + 1;
    session.insert("n", n);
    n.to_string()
}
```

Without a store the whole session is one encrypted cookie (`veil_session`, `HttpOnly`, `Secure`, `SameSite=Lax`,
24 h; at most about 4 KB). With `store(..)` the cookie holds a signed random id and the data stays on the server;
`SessionStore` is a synchronous trait (calls run on the worker, so keep them fast). `cycle_id()` after login,
`destroy()` on logout. A cookie is only sent when the session changed.

## HTTP client

`veil::api::client` is part of veil (feature `handler`); the kit re-exports it with helpers.

```rust
use veil::api::client::{Client, Http2};
let client = Client::builder().http2(Http2::Auto).timeout(Duration::from_secs(5)).build()?;
let res = client.post("https://api.example.com/v1/items").json(&item).send().await?;
let created: Item = res.error_for_status()?.json()?;
```

- It runs on the worker's runtime (call it from handlers and tasks), with the proxy's upstream building blocks:
  name resolution on the offload pool, the upstream TLS client (trust anchors from `[tls_client] ca`, or
  `ClientBuilder::ca(["system", "/etc/ssl/my-ca.pem"])`; `client_cert(cert, key)` for mTLS), and HTTP/2 over the
  multiplexed upstream connections.
- `Http2::Auto` (default): HTTPS offers `h2` and `http/1.1` by ALPN; plain HTTP uses HTTP/1.1. `Http2::On`: h2 over
  TLS, h2c with prior knowledge over plain TCP. `Http2::Off`: HTTP/1.1.
- `http://unix:/run/app.sock:/path` talks to a Unix socket.
- HTTP/1.1 connections are pooled per worker (`pool_idle_timeout`, default 60 s); an idempotent request whose pooled
  connection turned out to be closed is sent once more on a new connection.
- Bodies are read whole (`max_response_size`, default 64 MiB). Errors have a `kind()` (`Connect`, `Timeout`,
  `Protocol`, `Status`, ...) and convert into a 502/504 `Response`.
- Build clients at startup: a custom CA or client certificate is read from disk when the client is built.

`veil_kit::client::{get_json, post_json, retry}` add JSON round trips and retries with exponential backoff for
connection errors, timeouts and 502–504.

## gRPC client

`veil-build` also generates a `<service>_client` module (`Builder::build_client(false)` turns it off):

```rust
use hello::greeter_client::GreeterClient;
let c = GreeterClient::connect("http://127.0.0.1:50051")?;           // https:// = h2 over TLS
let reply = c.say_hello(HelloRequest { name: "veil".into() }).await?;  // Response<HelloReply>
let mut s = c.count_up(CountRequest { count: 3 }).await?.into_inner(); // ClientStreaming<CountReply>
while let Some(m) = s.message().await? { .. }
let sum = c.sum(futures::stream::iter(numbers)).await?;               // any Stream + Unpin
```

The client is a thin layer over `veil::api::grpc::Channel` (`Channel::connect(url)`, or
`Channel::with_client(&client, url)` to use a client's TLS settings), which carries unary calls and the three
streaming kinds over the HTTP/2 client: metadata both ways (`Request::with_metadata`, `Response::metadata()`,
`trailers()`), deadlines (`Request::with_timeout`, `Channel::timeout`, sent as `grpc-timeout`), message compression
(`Channel::compression`) and `Status` from `grpc-status` / `grpc-message`. In the kit, `grpc::Balanced` spreads calls
over several channels and retries `UNAVAILABLE` unary calls on the next one.

## GraphQL

```rust
let schema = Schema::build(Query, Mutation, Subscription).finish();
App::new().with(veil_kit::graphql::mount("/graphql", schema))
```

`POST` (single and batch; the `Content-Type` must be `application/json`, otherwise 415) and `GET` (queries and
subscriptions; a mutation over `GET` gives 405 with `Allow: POST`, so a cross-site link or a `SameSite=Lax` cookie
cannot trigger one), `application/graphql-response+json` when accepted, subscriptions over SSE
(`Accept: text/event-stream`) and over WebSocket (`graphql-transport-ws` and `graphql-ws`, HTTP/1.1), and GraphiQL for
a browser `GET` with the `graphiql` feature (the page loads its scripts from a CDN). The kit pins async-graphql 7.

## Dependency injection

```rust
let di = Container::new()
    .singleton(|_r: Resolver| async { Ok(Pool::connect("postgres://db").await?) })
    .scoped(|r: Resolver| async move {
        let pool = r.get::<Pool>().await?;
        let user = r.extract::<Auth<Principal>>().await?;
        Ok(Repo::for_user(pool, user))
    })
    .transient(|_r: Resolver| async { Ok(RequestTimer::start()) });
App::new().with(di)

#[veil::route(path = "/items", method = "GET")]
async fn items(repo: Dep<Repo>) -> Json<Vec<Item>> { .. }
```

- `singleton` is created on first use (`value(v)` registers one that exists), `scoped` once per request (stored in the
  request's extensions and shared by every `Dep` of that request), `transient` on every use.
- `container.replace::<T, _, _>(provider)` and `replace_value(v)` swap providers while the server runs (tests,
  configuration reloads); requests in progress finish with the old provider. Keep a clone of the container to do so.
- Slots are created when providers are registered and frozen at startup; a request does one lookup in an immutable
  table, an `ArcSwap` load and the provider call. A dependency cycle is reported (500, logged) the first time it is
  resolved. Errors of providers (`DiError`) become 500 responses with the reason in the log.

## Background tasks

```rust
#[veil::route(path = "/signup", method = "POST")]
async fn signup(tasks: BackgroundTasks) -> &'static str {
    tasks.add(async { send_welcome_mail().await });
    tasks.add_blocking(|| write_audit_log());
    "ok"
}

App::new().with(
    Tasks::new()
        .every(Duration::from_secs(60), || async { refresh_cache().await })
        .cron("0 */5 * * * *", || async { report().await })?   // feature cron, UTC
        .spawn(|| async { consume_queue().await }),
)
```

- `BackgroundTasks` work runs after the response was sent (HTTP/1.1: written; HTTP/2 and HTTP/3: the last frame
  queued), through `Response::after_send`. `add` spawns on the worker, `add_blocking` uses the handler pool. The
  `Tasks` plugin must be installed; its middleware (on `/`, or `background_on(prefix)`) costs one small allocation per
  request.
- App tasks run on the first worker that starts (or every worker after `on_all_workers()`), start with up to 10%
  jitter for `every`, never overlap, catch and log panics, and stop when graceful shutdown starts (a running
  iteration is not interrupted). Cron expressions have six fields (`sec min hour day month weekday`) or five, with
  `*`, numbers, ranges, lists and steps.

## Using tower instead

[`examples/api/rust-tower-recipes`](../../examples/api/rust-tower-recipes/) does the same with tower-http on
`App::layer` / `layer_on`: `CorsLayer` (with an `OPTIONS` catch-all route so preflights reach it),
`SetResponseHeaderLayer`, `ValidateRequestHeaderLayer::custom` for bearer tokens, cookies and redirects with plain
headers, and `ServeDir` mounted with `App::tokio_service` on `veil::tokio` (the `tokio` crate patched to the shim) next
to the recommended `App::serve_dir`. tower-cookies' `Cookies` lives in the `http` request extensions and does not
reach veil handlers.
