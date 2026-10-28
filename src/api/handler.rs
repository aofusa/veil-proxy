// F-188, F-159: 設定ロード時に `resolved_backend` が名前で引く。
//! Handler registration (static: macros; dynamic: `App::route`, C and Python) and execution.
//!
//! When the configuration is loaded, a route of `type = "Handler"` is looked up by name and its
//! `Backend::Handler` holds an `Arc<HandlerEntry>`. The registry is not searched at request time.

use super::pattern::PathPattern;
use super::request::Request;
use super::response::Response;
use super::schema::Definitions;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

/// The future of a handler (it does not need to be `Send` because it completes on the worker thread).
pub type HandlerFuture = Pin<Box<dyn Future<Output = Response> + 'static>>;

/// A dynamically registered handler function.
pub type HandlerFn = Arc<dyn Fn(Request) -> HandlerFuture + Send + Sync>;

/// One OpenAPI parameter.
#[derive(Debug, Clone)]
pub struct ParamMeta {
    /// The parameter name.
    pub name: String,
    /// `"path"` / `"query"` / `"header"`
    pub location: &'static str,
    /// Whether the parameter is required.
    pub required: bool,
    /// JSON Schema。
    pub schema: Value,
    /// The description.
    pub description: Option<String>,
}

/// An OpenAPI request body.
#[derive(Debug, Clone)]
pub struct BodyMeta {
    /// The `Content-Type` of the body.
    pub content_type: &'static str,
    /// The JSON Schema of the body.
    pub schema: Value,
    /// Whether the body is required.
    pub required: bool,
}

/// One OpenAPI response.
#[derive(Debug, Clone)]
pub struct ResponseMeta {
    /// The HTTP status code.
    pub status: u16,
    /// The description of the response.
    pub description: String,
    /// (content-type, schema)
    pub content: Option<(&'static str, Value)>,
}

/// OpenAPI metadata for one route (collected from macros, C and Python).
#[derive(Debug, Clone, Default)]
pub struct OperationMeta {
    /// The OpenAPI `operationId` (unique per route).
    pub operation_id: String,
    /// The OpenAPI `summary`.
    pub summary: Option<String>,
    /// The OpenAPI `description`.
    pub description: Option<String>,
    /// The OpenAPI `tags`.
    pub tags: Vec<String>,
    /// Path, query and header parameters.
    pub parameters: Vec<ParamMeta>,
    /// The request body.
    pub request_body: Option<BodyMeta>,
    /// The list of responses.
    pub responses: Vec<ResponseMeta>,
    /// Referenced named schemas (placed in `components.schemas`).
    pub definitions: Definitions,
    /// Whether the route validates automatically (a 422 response is then documented in OpenAPI).
    pub validates: bool,
    /// Omit from OpenAPI (such as the built-in `/docs`).
    pub hidden: bool,
    /// An OpenAPI Operation Object given as is (the metadata JSON of Python and C).
    /// When present, it is merged and takes precedence over the fields above.
    pub raw: Option<Value>,
    // F-262
    /// The route is a WebSocket endpoint (documented as `GET` → 101 with `x-veil-websocket`).
    pub websocket: Option<WebSocketMeta>,
}

// F-262: OpenAPI に WebSocket のルートを載せるための情報（`x-veil-websocket`）。
/// OpenAPI metadata of a WebSocket route.
#[derive(Debug, Clone, Default)]
pub struct WebSocketMeta {
    /// The subprotocols the route accepts (`Sec-WebSocket-Protocol`).
    pub subprotocols: Vec<String>,
    /// The schema of the messages the client sends.
    pub receive: Option<Value>,
    /// The schema of the messages the server sends.
    pub send: Option<Value>,
    /// The format of the messages (such as `text/json`), if known.
    pub message_format: Option<String>,
}

/// A route definition registered statically by the `#[veil::route]` macro.
pub struct RouteDef {
    /// The name of the route (the function name; also the default `operation_id`).
    pub name: &'static str,
    /// The path pattern (in the `/users/{id}` form).
    pub path: &'static str,
    /// The accepted methods, in upper case. Empty means all methods.
    pub methods: &'static [&'static str],
    /// The handler body.
    pub handler: fn(Request) -> HandlerFuture,
    /// A function returning the OpenAPI metadata (called when the document is generated).
    pub meta: fn() -> OperationMeta,
    // F-197: `#[veil::route(streaming = true)]`
    /// Receive the request body as a stream (HTTP/2).
    pub streaming_request: bool,
    // F-270: `#[veil::route(compress = false)]`
    /// Whether responses may be compressed by the route's `[route.compression]`.
    pub compress: bool,
}

inventory::collect!(RouteDef);

/// One registered handler.
pub struct HandlerEntry {
    /// The name of the handler (referenced by `handler` in `[[route]]`).
    pub name: Arc<str>,
    /// The path pattern.
    pub pattern: PathPattern,
    /// Upper-case method names. Empty means all methods.
    pub methods: Vec<String>,
    /// The handler body.
    pub func: HandlerFn,
    /// The OpenAPI metadata.
    pub meta: OperationMeta,
    // F-197
    /// Receive the request body as a stream (on HTTP/2, chunks reach `Request::take_body_stream` in arrival order).
    pub streaming_request: bool,
    // F-270
    /// Whether responses may be compressed by the route's `[route.compression]` (default `true`).
    pub compress: bool,
}

impl std::fmt::Debug for HandlerEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandlerEntry")
            .field("name", &self.name)
            .field("pattern", &self.pattern.as_str())
            .field("methods", &self.methods)
            .finish()
    }
}

impl HandlerEntry {
    /// Creates an entry for dynamic registration.
    pub fn new(
        name: impl Into<Arc<str>>,
        path: &str,
        methods: &[&str],
        func: HandlerFn,
        meta: OperationMeta,
    ) -> Result<HandlerEntry, String> {
        let pattern = PathPattern::parse(path)?;
        let mut ms = Vec::with_capacity(methods.len());
        for m in methods {
            let up = m.trim().to_ascii_uppercase();
            if up.is_empty() || !up.bytes().all(|b| b.is_ascii_uppercase()) {
                return Err(format!("invalid HTTP method '{}' for route '{}'", m, path));
            }
            if !ms.contains(&up) {
                ms.push(up);
            }
        }
        Ok(HandlerEntry {
            name: name.into(),
            pattern,
            methods: ms,
            func,
            meta,
            streaming_request: false,
            compress: true,
        })
    }

    // F-270
    /// Allows or forbids compressing this handler's responses.
    pub fn with_compression(mut self, on: bool) -> HandlerEntry {
        self.compress = on;
        self
    }

    /// Enables streaming reception of the request body.
    pub fn with_streaming_request(mut self, on: bool) -> HandlerEntry {
        self.streaming_request = on;
        self
    }

    /// Handles a request: re-matches the path, extracts the parameters and calls the handler.
    ///
    /// Matching in the routing layer includes prefix matches, so an exact match is verified here and a mismatch gives 404.
    pub async fn call(&self, mut req: Request) -> Response {
        match self.pattern.captures(&req.path) {
            Some(params) => req.params = params,
            None => return Response::not_found(),
        }
        if !self.methods.is_empty()
            && !self.methods.iter().any(|m| m.as_bytes() == &req.method[..])
            && !(&req.method[..] == b"HEAD" && self.methods.iter().any(|m| m == "GET"))
        {
            return Response::error(405, "Method Not Allowed");
        }
        (self.func)(req).await
    }
}

/// The process-wide handler table (a cold path referenced only at startup and reload).
static REGISTRY: once_cell::sync::Lazy<RwLock<HashMap<Arc<str>, Arc<HandlerEntry>>>> =
    once_cell::sync::Lazy::new(|| RwLock::new(HashMap::new()));

/// Registers a handler. Registering the same name twice is an error.
pub fn register(entry: HandlerEntry) -> Result<Arc<HandlerEntry>, String> {
    let mut map = REGISTRY.write().unwrap();
    if map.contains_key(&entry.name) {
        return Err(format!("handler '{}' is already registered", entry.name));
    }
    let entry = Arc::new(entry);
    map.insert(entry.name.clone(), entry.clone());
    Ok(entry)
}

/// Looks up a handler by name (used by `load_backend` when the configuration is loaded).
pub fn lookup(name: &str) -> Option<Arc<HandlerEntry>> {
    REGISTRY.read().unwrap().get(name).cloned()
}

/// All registered handlers, ordered by name.
pub fn all() -> Vec<Arc<HandlerEntry>> {
    let mut v: Vec<_> = REGISTRY.read().unwrap().values().cloned().collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// Registers the routes defined by macros (`inventory`) in the registry (only once, however often it is called).
pub fn register_macro_routes() -> Result<(), String> {
    static DONE: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    DONE.get_or_init(|| {
        for def in inventory::iter::<RouteDef> {
            let f = def.handler;
            let func: HandlerFn = Arc::new(move |req| f(req));
            let entry = HandlerEntry::new(def.name, def.path, def.methods, func, (def.meta)())?
                .with_streaming_request(def.streaming_request)
                .with_compression(def.compress);
            register(entry)?;
        }
        Ok(())
    })
    .clone()
}

/// Builds a handler function from an async function.
pub fn handler_fn<F, Fut, R>(f: F) -> HandlerFn
where
    F: Fn(Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = R> + 'static,
    R: super::response::IntoResponse,
{
    Arc::new(move |req| {
        let fut = f(req);
        Box::pin(async move { fut.await.into_response() })
    })
}

/// Builds a handler function from a blocking function (run by offloading to the handler pool).
pub fn blocking_handler_fn<F, R>(f: F) -> HandlerFn
where
    F: Fn(Request) -> R + Send + Sync + 'static,
    R: super::response::IntoResponse + Send + 'static,
{
    let f = Arc::new(f);
    Arc::new(move |req| {
        let f = Arc::clone(&f);
        Box::pin(async move {
            match crate::runtime::offload::offload_handler(move || f(req).into_response()).await {
                Some(r) => r,
                None => Response::error(500, "handler panicked"),
            }
        })
    })
}
