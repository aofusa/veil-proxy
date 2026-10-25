//! Library startup: the [`App`] builder, `serve`, shared state and shutdown.

use super::config::{compose, Config, ConfigError};
use super::handler::{self, HandlerEntry, HandlerFn, OperationMeta, RouteDef};
use super::request::Request;
use super::response::IntoResponse;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Errors raised while starting or running the library.
#[derive(Debug, Clone)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<ConfigError> for Error {
    fn from(e: ConfigError) -> Error {
        Error(e.0)
    }
}

impl From<String> for Error {
    fn from(e: String) -> Error {
        Error(e)
    }
}

// ===================== 共有状態 =====================

type StateMap = HashMap<TypeId, Arc<dyn Any + Send + Sync>>;

/// State registered before startup (see `App::state`).
static PENDING_STATE: Mutex<Option<StateMap>> = Mutex::new(None);
/// State frozen at startup (read without locking at request time).
static STATE: OnceLock<StateMap> = OnceLock::new();

/// Looks up a value registered with `App::state` by its type.
pub fn state<T: Send + Sync + 'static>() -> Option<Arc<T>> {
    let any = STATE.get()?.get(&TypeId::of::<T>())?.clone();
    any.downcast::<T>().ok()
}

fn freeze_state() {
    let map = PENDING_STATE.lock().unwrap().take().unwrap_or_default();
    let _ = STATE.set(map);
}

// ===================== ライブラリ設定（[api]） =====================

/// The `[api]` section (keys absent from the main configuration; a hook strips and reads them).
#[derive(Debug, Clone)]
pub struct ApiSettings {
    /// Whether to serve OpenAPI (`/openapi.json` and Swagger UI). Defaults to `true`.
    pub openapi_enabled: bool,
    /// Path of the Swagger UI. Defaults to `/docs`.
    pub openapi_path: String,
    /// Path of the OpenAPI JSON document. Defaults to `/openapi.json`.
    pub openapi_json_path: String,
    /// `info.title` of the OpenAPI document. Defaults to `veil API`.
    pub openapi_title: String,
    /// `info.version` of the OpenAPI document. Defaults to `0.1.0`.
    pub openapi_version: String,
    /// `info.description` of the OpenAPI document (optional).
    pub openapi_description: Option<String>,
    /// Maximum request body size, in bytes, that a handler accepts. Defaults to 10 MiB.
    pub max_body_size: usize,
    /// Number of threads in the blocking handler pool. `0` chooses a default automatically.
    pub handler_threads: usize,
    /// Number of workers of the tokio-compatible runtime. Defaults to `2`.
    pub tokio_threads: usize,
    // F-197: gRPC の受信メッセージ上限。
    /// Maximum size of a received gRPC message (`grpc-handler`).
    pub grpc_max_message_size: usize,
    // F-209: gRPC の応答の圧縮。
    /// Compression of gRPC responses (`none`, `gzip` or `deflate`).
    pub grpc_response_compression: String,
    // F-209
    /// gRPC messages smaller than this size are not compressed.
    pub grpc_response_compression_min_size: usize,
    // F-209
    /// Origins allowed by gRPC-Web CORS (`"*"` allows all; empty disables CORS).
    pub grpc_web_cors_origins: Vec<String>,
    // F-209
    /// Maximum size of a sent gRPC message (0 = unlimited).
    pub grpc_max_send_message_size: usize,
    // F-261
    /// Seconds a read on an upgraded connection may wait before failing (0 = no limit).
    pub upgrade_idle_timeout_secs: u64,
    // F-262
    /// Maximum size of an assembled WebSocket message (larger closes with 1009).
    pub websocket_max_message_size: usize,
    // F-262
    /// Maximum size of one WebSocket frame.
    pub websocket_max_frame_size: usize,
    // F-262
    /// Seconds between server Pings on idle WebSocket connections (0 = none).
    pub websocket_ping_interval_secs: u64,
    // F-262
    /// Seconds without a received message before a WebSocket connection is closed (0 = no limit).
    pub websocket_idle_timeout_secs: u64,
    // F-262
    /// Negotiate permessage-deflate (RFC 7692) when the client offers it.
    pub websocket_compression: bool,
    // F-262
    /// List WebSocket routes in the OpenAPI document.
    pub openapi_websocket: bool,
}

impl ApiSettings {
    pub(crate) fn from_value(v: Option<&toml::Value>) -> ApiSettings {
        let s = |k: &str, d: &str| -> String {
            v.and_then(|t| t.get(k))
                .and_then(|x| x.as_str())
                .unwrap_or(d)
                .to_string()
        };
        let b = |k: &str, d: bool| {
            v.and_then(|t| t.get(k))
                .and_then(|x| x.as_bool())
                .unwrap_or(d)
        };
        let n = |k: &str, d: usize| {
            v.and_then(|t| t.get(k))
                .and_then(|x| x.as_integer())
                .map(|i| i.max(0) as usize)
                .unwrap_or(d)
        };
        ApiSettings {
            openapi_enabled: b("openapi_enabled", true),
            openapi_path: s("openapi_path", "/docs"),
            openapi_json_path: s("openapi_json_path", "/openapi.json"),
            openapi_title: s("openapi_title", "veil API"),
            openapi_version: s("openapi_version", "0.1.0"),
            openapi_description: v
                .and_then(|t| t.get("openapi_description"))
                .and_then(|x| x.as_str())
                .map(|x| x.to_string()),
            max_body_size: n("max_body_size", 10 * 1024 * 1024),
            handler_threads: n("handler_threads", 0),
            tokio_threads: n("tokio_threads", 2),
            grpc_max_message_size: n("grpc_max_message_size", 4 * 1024 * 1024),
            grpc_response_compression: s("grpc_response_compression", "none"),
            grpc_response_compression_min_size: n("grpc_response_compression_min_size", 1024),
            grpc_web_cors_origins: v
                .and_then(|t| t.get("grpc_web_cors_origins"))
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            grpc_max_send_message_size: n("grpc_max_send_message_size", 0),
            upgrade_idle_timeout_secs: n("upgrade_idle_timeout_secs", 0) as u64,
            websocket_max_message_size: n("websocket_max_message_size", 16 * 1024 * 1024),
            websocket_max_frame_size: n("websocket_max_frame_size", 16 * 1024 * 1024),
            websocket_ping_interval_secs: n("websocket_ping_interval_secs", 0) as u64,
            websocket_idle_timeout_secs: n("websocket_idle_timeout_secs", 0) as u64,
            websocket_compression: b("websocket_compression", false),
            openapi_websocket: b("openapi_websocket", true),
        }
    }
}

/// Maximum body size a handler accepts (`[api] max_body_size`); read on the request path.
static MAX_BODY_SIZE: AtomicUsize = AtomicUsize::new(10 * 1024 * 1024);

/// The current body size limit.
#[inline]
pub fn max_body_size() -> usize {
    MAX_BODY_SIZE.load(Ordering::Relaxed)
}

/// Number of workers of the tokio-compatible runtime (`[api] tokio_threads`).
pub(crate) static TOKIO_THREADS: AtomicUsize = AtomicUsize::new(2);

/// The `[api]` settings most recently read by the hook.
pub(crate) static SETTINGS: Mutex<Option<ApiSettings>> = Mutex::new(None);

/// The current `[api]` settings (defaults before startup).
pub fn settings() -> ApiSettings {
    SETTINGS
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| ApiSettings::from_value(None))
}

// ===================== App =====================

/// A route awaiting registration (layers are applied and the registry is filled at startup).
pub(crate) struct PendingRoute {
    pub name: String,
    pub path: String,
    pub methods: Vec<String>,
    pub func: HandlerFn,
    pub meta: OperationMeta,
    pub streaming_request: bool,
}

/// Middleware wrapping handler functions (such as `layer_on`); applies only to routes matching `prefix`.
pub(crate) struct Middleware {
    pub prefix: String,
    pub wrap: Box<dyn Fn(HandlerFn) -> HandlerFn + Send + Sync>,
}

/// Builder for an API server.
///
/// ```no_run
/// use veil::prelude::*;
/// App::new()
///     .get("/hello", |_req: Request| async { "hello" })
///     .run()
///     .unwrap();
/// ```
pub struct App {
    config: Result<Config, ConfigError>,
    pub(crate) routes: Vec<PendingRoute>,
    middleware: Vec<Middleware>,
    include_macro_routes: bool,
    setup: Vec<SetupFn>,
}

/// Initialization run at startup (see `App::on_setup`).
type SetupFn = Box<dyn FnOnce() -> Result<(), String> + Send>;

/// Alias of [`App`].
pub type Router = App;

impl Default for App {
    fn default() -> Self {
        App::new()
    }
}

impl App {
    /// Creates an app, loading `config.toml` from the current directory if present and the library defaults otherwise.
    pub fn new() -> App {
        App {
            config: Config::load(),
            routes: Vec::new(),
            middleware: Vec::new(),
            include_macro_routes: true,
            setup: Vec::new(),
        }
    }

    /// Sets the configuration (for example `Config::from_file(..)?.builder()...build()`).
    pub fn config(mut self, config: Config) -> App {
        self.config = Ok(config);
        self
    }

    /// Includes the routes declared with `#[veil::route]` (the default).
    pub fn route_from_macros(mut self) -> App {
        self.include_macro_routes = true;
        self
    }

    /// Excludes the routes declared with macros.
    pub fn without_macro_routes(mut self) -> App {
        self.include_macro_routes = false;
        self
    }

    /// Registers shared state, which handlers receive through the `State<T>` extractor or `req.state::<T>()`.
    pub fn state<T: Send + Sync + 'static>(self, value: T) -> App {
        let mut g = PENDING_STATE.lock().unwrap();
        g.get_or_insert_with(HashMap::new)
            .insert(TypeId::of::<T>(), Arc::new(value));
        drop(g);
        self
    }

    /// Registers an async function as a handler (all methods when `methods` is empty).
    pub fn route<F, Fut, R>(self, methods: &[&str], path: &str, f: F) -> App
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + 'static,
        R: IntoResponse,
    {
        let name = auto_name(methods, path);
        let meta = OperationMeta {
            operation_id: name.clone(),
            ..Default::default()
        };
        self.route_fn(&name, methods, path, handler::handler_fn(f), meta)
    }

    /// Registers a blocking function as a handler (run on the handler pool).
    pub fn route_blocking<F, R>(self, methods: &[&str], path: &str, f: F) -> App
    where
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: IntoResponse + Send + 'static,
    {
        let name = auto_name(methods, path);
        let meta = OperationMeta {
            operation_id: name.clone(),
            ..Default::default()
        };
        self.route_fn(&name, methods, path, handler::blocking_handler_fn(f), meta)
    }

    /// Registers a handler function with a name and metadata (used by the C and Python bindings).
    pub fn route_fn(
        mut self,
        name: &str,
        methods: &[&str],
        path: &str,
        func: HandlerFn,
        meta: OperationMeta,
    ) -> App {
        self.routes.push(PendingRoute {
            name: name.to_string(),
            path: path.to_string(),
            methods: methods.iter().map(|m| m.to_string()).collect(),
            func,
            meta,
            streaming_request: false,
        });
        self
    }

    // F-197
    /// Registers a handler function that receives the request body as a stream (see `Request::take_body_stream`).
    pub fn route_fn_streaming(
        mut self,
        name: &str,
        methods: &[&str],
        path: &str,
        func: HandlerFn,
        meta: OperationMeta,
    ) -> App {
        self = self.route_fn(name, methods, path, func, meta);
        if let Some(last) = self.routes.last_mut() {
            last.streaming_request = true;
        }
        self
    }

    /// Registers a `GET` route (shorthand for `route(&["GET"], path, f)`).
    pub fn get<F, Fut, R>(self, path: &str, f: F) -> App
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + 'static,
        R: IntoResponse,
    {
        self.route(&["GET"], path, f)
    }

    /// Registers a `POST` route (shorthand for `route(&["POST"], path, f)`).
    pub fn post<F, Fut, R>(self, path: &str, f: F) -> App
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + 'static,
        R: IntoResponse,
    {
        self.route(&["POST"], path, f)
    }

    /// Registers a `PUT` route (shorthand for `route(&["PUT"], path, f)`).
    pub fn put<F, Fut, R>(self, path: &str, f: F) -> App
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + 'static,
        R: IntoResponse,
    {
        self.route(&["PUT"], path, f)
    }

    /// Registers a `DELETE` route (shorthand for `route(&["DELETE"], path, f)`).
    pub fn delete<F, Fut, R>(self, path: &str, f: F) -> App
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + 'static,
        R: IntoResponse,
    {
        self.route(&["DELETE"], path, f)
    }

    /// Registers a `PATCH` route (shorthand for `route(&["PATCH"], path, f)`).
    pub fn patch<F, Fut, R>(self, path: &str, f: F) -> App
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + 'static,
        R: IntoResponse,
    {
        self.route(&["PATCH"], path, f)
    }

    /// Registers middleware that wraps handler functions (all routes when `prefix` is `"/"`).
    /// Use `layer` / `layer_on` (`api-core`) for tower layers.
    pub fn wrap_on(
        mut self,
        prefix: &str,
        wrap: impl Fn(HandlerFn) -> HandlerFn + Send + Sync + 'static,
    ) -> App {
        self.middleware.push(Middleware {
            prefix: prefix.to_string(),
            wrap: Box::new(wrap),
        });
        self
    }

    /// For tests: the function of the last registered route with the middleware applied in the same order as at startup (used by the gRPC tests).
    #[cfg(all(test, feature = "grpc-handler"))]
    pub(crate) fn last_route_wrapped(&self) -> HandlerFn {
        let route = self.routes.last().expect("registered");
        let mut func = route.func.clone();
        for mw in &self.middleware {
            if path_has_prefix(&route.path, &mw.prefix) {
                func = (mw.wrap)(func);
            }
        }
        func
    }

    /// Adds initialization to run at startup, before handlers are registered (used by extensions such as database connection setup and L4 handlers).
    pub fn on_setup(mut self, f: impl FnOnce() -> Result<(), String> + Send + 'static) -> App {
        self.setup.push(Box::new(f));
        self
    }

    /// Starts the server and blocks until shutdown (SIGINT/SIGTERM or `veil::shutdown()`).
    pub fn run(self) -> Result<(), Error> {
        let config = self.config?;
        for f in self.setup {
            f()?;
        }
        let mut pending: Vec<PendingRoute> = Vec::new();
        if self.include_macro_routes {
            for def in inventory::iter::<RouteDef> {
                let f = def.handler;
                pending.push(PendingRoute {
                    name: def.name.to_string(),
                    path: def.path.to_string(),
                    methods: def.methods.iter().map(|m| m.to_string()).collect(),
                    func: Arc::new(move |req| f(req)),
                    meta: (def.meta)(),
                    streaming_request: def.streaming_request,
                });
            }
        }
        pending.extend(self.routes);
        for route in pending {
            let mut func = route.func;
            // 登録順に外側へ包む（後から登録したミドルウェアほど外側）。
            for mw in &self.middleware {
                if path_has_prefix(&route.path, &mw.prefix) {
                    func = (mw.wrap)(func);
                }
            }
            let methods: Vec<&str> = route.methods.iter().map(|s| s.as_str()).collect();
            let entry =
                HandlerEntry::new(route.name.as_str(), &route.path, &methods, func, route.meta)?
                    .with_streaming_request(route.streaming_request);
            handler::register(entry)?;
        }
        start(config)
    }
}

/// Whether `prefix` is a prefix of the path on a segment boundary (`"/"` matches everything).
fn path_has_prefix(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return true;
    }
    path == prefix
        || path
            .strip_prefix(prefix)
            .map(|r| r.starts_with('/'))
            .unwrap_or(false)
}

/// Builds an automatic handler name from the methods and path (for example `GET /users/{id}` becomes `get_users_id`).
fn auto_name(methods: &[&str], path: &str) -> String {
    let mut name = methods
        .first()
        .map(|m| m.to_ascii_lowercase())
        .unwrap_or_else(|| "any".to_string());
    for c in path.chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c.to_ascii_lowercase());
        } else if !name.ends_with('_') {
            name.push('_');
        }
    }
    while name.ends_with('_') {
        name.pop();
    }
    name
}

/// Installs the configuration hook and starts through the main server entry point (`run_server`).
fn start(config: Config) -> Result<(), Error> {
    let inline = config.inline.clone();
    let overrides = config.overrides.clone();
    crate::config_override::set_library_hook(Box::new(move |root: &mut toml::Value| {
        compose(root, inline.as_ref(), &overrides)?;
        let api = root.as_table_mut().and_then(|t| t.remove("api"));
        let settings = ApiSettings::from_value(api.as_ref());
        MAX_BODY_SIZE.store(settings.max_body_size, Ordering::Relaxed);
        TOKIO_THREADS.store(settings.tokio_threads.max(1), Ordering::Relaxed);
        super::upgrade::set_idle_timeout_secs(settings.upgrade_idle_timeout_secs);
        super::ws::apply_settings(&settings);
        #[cfg(feature = "grpc-handler")]
        {
            super::grpc::set_max_message_size(settings.grpc_max_message_size);
            if let Err(e) = super::grpc::set_response_compression(
                &settings.grpc_response_compression,
                settings.grpc_response_compression_min_size,
            ) {
                ftlog::warn!("{}; responses are not compressed", e);
            }
            super::grpc::set_grpc_web_cors_origins(settings.grpc_web_cors_origins.clone());
            super::grpc::set_max_send_message_size(settings.grpc_max_send_message_size);
        }
        if settings.handler_threads > 0 {
            crate::runtime::offload::set_handler_pool_threads(settings.handler_threads);
        }
        let entries = builtin_and_user_entries(&settings)?;
        *SETTINGS.lock().unwrap() = Some(settings);
        #[cfg(feature = "l4-handler")]
        {
            super::l4::register_macro_handlers()?;
            super::l4::inject_l4(root)?;
        }
        super::routes::inject_handler_routes(root, &entries)?;
        super::routes::sort_and_check_routes(root)?;
        Ok(())
    }))
    .map_err(Error)?;

    freeze_state();

    // 環境変数 VEIL_OVERRIDE_<n>（F-179）も本体と同じ規則で当てる（CLI は無い）。
    let env = crate::config_override::env_overrides_from(std::env::vars_os()).map_err(Error)?;
    crate::config_override::set_global_overrides(env);

    // ファイルが無いときは存在しないパスを渡す（ライブラリモードでは空 = 既定設定のみ）。
    let path = config
        .file
        .clone()
        .unwrap_or_else(|| PathBuf::from("<veil-library-defaults>"));

    // 起動前に同じ経路で設定を検証し、エラーを呼び出し側へ返す（未登録ハンドラ・ルート衝突等）。
    crate::config::test_config_file(&path).map_err(|e| Error(e.to_string()))?;

    crate::entry::run_server(crate::entry::ServerOptions {
        config_path: path,
        test_config: false,
    });
    Ok(())
}

/// The handlers to inject: the user's own plus the enabled built-ins (such as OpenAPI).
fn builtin_and_user_entries(settings: &ApiSettings) -> Result<Vec<Arc<HandlerEntry>>, String> {
    #[cfg(feature = "openapi")]
    super::openapi::refresh(settings)?;
    let _ = settings;
    Ok(handler::all()
        .into_iter()
        .filter(|e| super::is_builtin_enabled(e, settings))
        .collect())
}

/// Starts and blocks until shutdown (takes only a `Config`; the routes are the macro-declared ones).
pub fn serve_blocking(config: Config) -> Result<(), Error> {
    App::new().config(config).run()
}

/// Starts the server from within `#[veil::main]`, as `veil::serve(config).await`.
/// It calls `serve_blocking`, so the calling thread blocks until shutdown.
pub async fn serve(config: Config) -> Result<(), Error> {
    serve_blocking(config)
}

/// Starts the server with the given router ([`App`]).
pub async fn serve_with_router(config: Config, router: Router) -> Result<(), Error> {
    router.config(config).run()
}

/// Stops the running server (graceful shutdown, the same as SIGTERM).
pub fn shutdown() {
    crate::config::SHUTDOWN_FLAG.store(true, Ordering::SeqCst);
}

/// The minimal executor used by `#[veil::main]` to run the async block of `main`.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    futures::executor::block_on(fut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_prefixes() {
        assert_eq!(auto_name(&["GET"], "/users/{id}"), "get_users_id");
        assert_eq!(auto_name(&[], "/"), "any");
        assert!(path_has_prefix("/admin/users", "/admin"));
        assert!(path_has_prefix("/admin", "/admin/"));
        assert!(!path_has_prefix("/administrator", "/admin"));
        assert!(path_has_prefix("/x", "/"));
    }

    #[test]
    fn api_settings_defaults() {
        let s = ApiSettings::from_value(None);
        assert!(s.openapi_enabled);
        assert_eq!(s.openapi_path, "/docs");
        assert_eq!(s.max_body_size, 10 * 1024 * 1024);
    }
}
