//! ライブラリの起動（F-188）: `App` ビルダ・`serve`・共有状態・停止。

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

/// ライブラリの起動・実行時エラー。
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

/// 起動前に登録された状態（`App::state`）。
static PENDING_STATE: Mutex<Option<StateMap>> = Mutex::new(None);
/// 起動時に凍結した状態（リクエスト時はロック無しで読む）。
static STATE: OnceLock<StateMap> = OnceLock::new();

/// `App::state` で登録した値を型で引く。
pub fn state<T: Send + Sync + 'static>() -> Option<Arc<T>> {
    let any = STATE.get()?.get(&TypeId::of::<T>())?.clone();
    any.downcast::<T>().ok()
}

fn freeze_state() {
    let map = PENDING_STATE.lock().unwrap().take().unwrap_or_default();
    let _ = STATE.set(map);
}

// ===================== ライブラリ設定（[api]） =====================

/// `[api]` セクション（本体の設定には無いキー。フックが取り除いて読む）。
#[derive(Debug, Clone)]
pub struct ApiSettings {
    pub openapi_enabled: bool,
    pub openapi_path: String,
    pub openapi_json_path: String,
    pub openapi_title: String,
    pub openapi_version: String,
    pub openapi_description: Option<String>,
    pub max_body_size: usize,
    pub handler_threads: usize,
    pub tokio_threads: usize,
    /// gRPC の受信メッセージ上限（F-197、`grpc-handler`）
    pub grpc_max_message_size: usize,
    /// gRPC の応答の圧縮（`none`・`gzip`・`deflate`。F-209）
    pub grpc_response_compression: String,
    /// この大きさ未満の gRPC メッセージは圧縮しない（F-209）
    pub grpc_response_compression_min_size: usize,
    /// gRPC-Web の CORS で許す Origin（`"*"` で全部、空なら CORS 無し。F-209）
    pub grpc_web_cors_origins: Vec<String>,
    /// gRPC の送信メッセージの上限（0 = 無制限。F-209）
    pub grpc_max_send_message_size: usize,
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
        }
    }
}

/// ハンドラが受け取る本文の上限（`[api] max_body_size`）。リクエスト経路から読む。
static MAX_BODY_SIZE: AtomicUsize = AtomicUsize::new(10 * 1024 * 1024);

/// 現在の本文上限。
#[inline]
pub fn max_body_size() -> usize {
    MAX_BODY_SIZE.load(Ordering::Relaxed)
}

/// tokio 互換ランタイムのワーカー数（`[api] tokio_threads`）。
pub(crate) static TOKIO_THREADS: AtomicUsize = AtomicUsize::new(2);

/// 最後にフックで読んだ `[api]` 設定。
pub(crate) static SETTINGS: Mutex<Option<ApiSettings>> = Mutex::new(None);

/// 現在の `[api]` 設定（起動前は既定値）。
pub fn settings() -> ApiSettings {
    SETTINGS
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| ApiSettings::from_value(None))
}

// ===================== App =====================

/// 登録待ちのルート（起動時にレイヤー適用・レジストリ登録する）。
pub(crate) struct PendingRoute {
    pub name: String,
    pub path: String,
    pub methods: Vec<String>,
    pub func: HandlerFn,
    pub meta: OperationMeta,
    pub streaming_request: bool,
}

/// ハンドラ関数を包むミドルウェア（`layer_on` 等）。`prefix` に一致するルートだけに掛かる。
pub(crate) struct Middleware {
    pub prefix: String,
    pub wrap: Box<dyn Fn(HandlerFn) -> HandlerFn + Send + Sync>,
}

/// API サーバのビルダ（設計書の `Router` も同じもの）。
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

/// 起動時の初期化処理（`App::on_setup`）。
type SetupFn = Box<dyn FnOnce() -> Result<(), String> + Send>;

/// 設計書の `veil::Router`（`App` と同じ）。
pub type Router = App;

impl Default for App {
    fn default() -> Self {
        App::new()
    }
}

impl App {
    /// カレントディレクトリに `config.toml` があれば取り込み、無ければライブラリ既定設定で作る。
    pub fn new() -> App {
        App {
            config: Config::load(),
            routes: Vec::new(),
            middleware: Vec::new(),
            include_macro_routes: true,
            setup: Vec::new(),
        }
    }

    /// 設定を指定する（`Config::from_file(..)?.builder()...build()` 等）。
    pub fn config(mut self, config: Config) -> App {
        self.config = Ok(config);
        self
    }

    /// マクロ（`#[veil::route]`）のルートを含める（既定で含む。設計書の記法との互換用）。
    pub fn route_from_macros(mut self) -> App {
        self.include_macro_routes = true;
        self
    }

    /// マクロのルートを含めない。
    pub fn without_macro_routes(mut self) -> App {
        self.include_macro_routes = false;
        self
    }

    /// 共有状態を登録する（ハンドラは `State<T>` 抽出子・`req.state::<T>()` で受け取る）。
    pub fn state<T: Send + Sync + 'static>(self, value: T) -> App {
        let mut g = PENDING_STATE.lock().unwrap();
        g.get_or_insert_with(HashMap::new)
            .insert(TypeId::of::<T>(), Arc::new(value));
        drop(g);
        self
    }

    /// async 関数をハンドラとして登録する（`methods` が空なら全メソッド）。
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

    /// ブロッキングな関数をハンドラとして登録する（ハンドラ用プールで実行）。
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

    /// 名前・メタデータ付きでハンドラ関数を登録する（C・Python のバインディングが使う）。
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

    /// 要求本文をストリーミングで受け取るハンドラ関数を登録する（`Request::take_body_stream`、F-197）。
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

    pub fn get<F, Fut, R>(self, path: &str, f: F) -> App
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + 'static,
        R: IntoResponse,
    {
        self.route(&["GET"], path, f)
    }

    pub fn post<F, Fut, R>(self, path: &str, f: F) -> App
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + 'static,
        R: IntoResponse,
    {
        self.route(&["POST"], path, f)
    }

    pub fn put<F, Fut, R>(self, path: &str, f: F) -> App
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + 'static,
        R: IntoResponse,
    {
        self.route(&["PUT"], path, f)
    }

    pub fn delete<F, Fut, R>(self, path: &str, f: F) -> App
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + 'static,
        R: IntoResponse,
    {
        self.route(&["DELETE"], path, f)
    }

    pub fn patch<F, Fut, R>(self, path: &str, f: F) -> App
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + 'static,
        R: IntoResponse,
    {
        self.route(&["PATCH"], path, f)
    }

    /// ハンドラ関数を包むミドルウェアを登録する（`prefix` が `"/"` なら全ルート）。
    /// tower の層は `layer` / `layer_on`（`api-core`）を使う。
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

    /// 試験用: 最後に登録したルートの関数に、起動時と同じ順でミドルウェアを掛けたもの。
    #[cfg(test)]
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

    /// 起動時（ハンドラ登録の前）に実行する初期化処理を足す（DB 接続の確立・L4 ハンドラ等の拡張が使う）。
    pub fn on_setup(mut self, f: impl FnOnce() -> Result<(), String> + Send + 'static) -> App {
        self.setup.push(Box::new(f));
        self
    }

    /// 起動して停止（SIGINT/SIGTERM・`veil::shutdown()`）までブロックする。
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

/// `prefix` がパスのセグメント境界での前方一致か（`"/"` は全部）。
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

/// メソッドとパスから自動のハンドラ名を作る（例: `GET /users/{id}` → `get_users_id`）。
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

/// 設定フックを入れて本体の起動経路（`run_server`）で起動する。
fn start(config: Config) -> Result<(), Error> {
    let inline = config.inline.clone();
    let overrides = config.overrides.clone();
    crate::config_override::set_library_hook(Box::new(move |root: &mut toml::Value| {
        compose(root, inline.as_ref(), &overrides)?;
        let api = root.as_table_mut().and_then(|t| t.remove("api"));
        let settings = ApiSettings::from_value(api.as_ref());
        MAX_BODY_SIZE.store(settings.max_body_size, Ordering::Relaxed);
        TOKIO_THREADS.store(settings.tokio_threads.max(1), Ordering::Relaxed);
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

/// 注入するハンドラ（利用者のもの + 有効な組み込み（OpenAPI 等））。
fn builtin_and_user_entries(settings: &ApiSettings) -> Result<Vec<Arc<HandlerEntry>>, String> {
    #[cfg(feature = "openapi")]
    super::openapi::refresh(settings)?;
    let _ = settings;
    Ok(handler::all()
        .into_iter()
        .filter(|e| super::is_builtin_enabled(e, settings))
        .collect())
}

/// 起動して停止までブロックする（`Config` だけで、ルートはマクロのもの）。
pub fn serve_blocking(config: Config) -> Result<(), Error> {
    App::new().config(config).run()
}

/// 設計書の `veil::serve(config).await`（`#[veil::main]` の中で使う）。
/// 中では `serve_blocking` を呼ぶ（呼び出したスレッドは停止までブロックする）。
pub async fn serve(config: Config) -> Result<(), Error> {
    serve_blocking(config)
}

/// ルータ（`App`）を指定して起動する（設計書の `veil::run_with_router` 相当）。
pub async fn serve_with_router(config: Config, router: Router) -> Result<(), Error> {
    router.config(config).run()
}

/// 実行中のサーバを停止する（SIGTERM と同じ graceful shutdown）。
pub fn shutdown() {
    crate::config::SHUTDOWN_FLAG.store(true, Ordering::SeqCst);
}

/// `#[veil::main]` が使う最小の executor（main の async ブロックを実行する）。
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
