//! ハンドラの登録（静的: マクロ / 動的: `App::route`・C・Python）と実行（F-188）。
//!
//! 設定ロード時に `type = "Handler"` のルートを名前で引いて `Backend::Handler` に
//! `Arc<HandlerEntry>` を持たせる（F-159 の `resolved_backend`）。リクエスト時に
//! レジストリは検索しない。

use super::pattern::PathPattern;
use super::request::Request;
use super::response::Response;
use super::schema::Definitions;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

/// ハンドラの Future（ワーカースレッド上で完結するため `Send` を要求しない）。
pub type HandlerFuture = Pin<Box<dyn Future<Output = Response> + 'static>>;

/// 動的に登録するハンドラ関数。
pub type HandlerFn = Arc<dyn Fn(Request) -> HandlerFuture + Send + Sync>;

/// OpenAPI のパラメータ 1 件。
#[derive(Debug, Clone)]
pub struct ParamMeta {
    pub name: String,
    /// `"path"` / `"query"` / `"header"`
    pub location: &'static str,
    pub required: bool,
    pub schema: Value,
    pub description: Option<String>,
}

/// OpenAPI のリクエスト本文。
#[derive(Debug, Clone)]
pub struct BodyMeta {
    pub content_type: &'static str,
    pub schema: Value,
    pub required: bool,
}

/// OpenAPI の応答 1 件。
#[derive(Debug, Clone)]
pub struct ResponseMeta {
    pub status: u16,
    pub description: String,
    /// (content-type, schema)
    pub content: Option<(&'static str, Value)>,
}

/// ルート 1 件の OpenAPI 用メタデータ（マクロ・C・Python から集める）。
#[derive(Debug, Clone, Default)]
pub struct OperationMeta {
    pub operation_id: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub parameters: Vec<ParamMeta>,
    pub request_body: Option<BodyMeta>,
    pub responses: Vec<ResponseMeta>,
    /// 参照される名前付きスキーマ（components.schemas に入る）。
    pub definitions: Definitions,
    /// 自動バリデーションがあるか（422 応答を OpenAPI に載せる）。
    pub validates: bool,
    /// OpenAPI に載せない（組み込みの `/docs` 等）。
    pub hidden: bool,
    /// OpenAPI の Operation Object をそのまま与える（Python・C のメタデータ JSON）。
    /// 指定があれば上の各項目より優先してマージする。
    pub raw: Option<Value>,
}

/// マクロ（`#[veil::route]`）が静的に登録するルート定義。
pub struct RouteDef {
    pub name: &'static str,
    pub path: &'static str,
    pub methods: &'static [&'static str],
    pub handler: fn(Request) -> HandlerFuture,
    pub meta: fn() -> OperationMeta,
    /// 要求本文をストリーミングで受け取る（HTTP/2。`#[veil::route(streaming = true)]`、F-197）
    pub streaming_request: bool,
}

inventory::collect!(RouteDef);

/// 登録済みのハンドラ 1 件。
pub struct HandlerEntry {
    pub name: Arc<str>,
    pub pattern: PathPattern,
    /// 大文字のメソッド名。空なら全メソッド。
    pub methods: Vec<String>,
    pub func: HandlerFn,
    pub meta: OperationMeta,
    /// 要求本文をストリーミングで受け取る（HTTP/2 で `Request::take_body_stream` に届いた順に流す。F-197）
    pub streaming_request: bool,
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
    /// 動的登録用のエントリを作る。
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
        })
    }

    /// 要求本文のストリーミング受信を有効にする。
    pub fn with_streaming_request(mut self, on: bool) -> HandlerEntry {
        self.streaming_request = on;
        self
    }

    /// リクエストを処理する（パスを再照合してパラメータを取り出し、ハンドラを呼ぶ）。
    ///
    /// ルーティング層の照合は前方一致を含むため、ここで完全一致を確かめて不一致は 404 にする。
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

/// プロセス全体のハンドラ表（起動時・リロード時のみ参照するコールドパス）。
static REGISTRY: once_cell::sync::Lazy<RwLock<HashMap<Arc<str>, Arc<HandlerEntry>>>> =
    once_cell::sync::Lazy::new(|| RwLock::new(HashMap::new()));

/// ハンドラを登録する。同じ名前の二重登録はエラー。
pub fn register(entry: HandlerEntry) -> Result<Arc<HandlerEntry>, String> {
    let mut map = REGISTRY.write().unwrap();
    if map.contains_key(&entry.name) {
        return Err(format!("handler '{}' is already registered", entry.name));
    }
    let entry = Arc::new(entry);
    map.insert(entry.name.clone(), entry.clone());
    Ok(entry)
}

/// 名前でハンドラを引く（設定ロード時の `load_backend` が使う）。
pub fn lookup(name: &str) -> Option<Arc<HandlerEntry>> {
    REGISTRY.read().unwrap().get(name).cloned()
}

/// 登録済みの全ハンドラ（名前順）。
pub fn all() -> Vec<Arc<HandlerEntry>> {
    let mut v: Vec<_> = REGISTRY.read().unwrap().values().cloned().collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// マクロで定義されたルート（`inventory`）をレジストリへ登録する（何度呼んでも 1 回だけ）。
pub fn register_macro_routes() -> Result<(), String> {
    static DONE: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    DONE.get_or_init(|| {
        for def in inventory::iter::<RouteDef> {
            let f = def.handler;
            let func: HandlerFn = Arc::new(move |req| f(req));
            let entry = HandlerEntry::new(def.name, def.path, def.methods, func, (def.meta)())?
                .with_streaming_request(def.streaming_request);
            register(entry)?;
        }
        Ok(())
    })
    .clone()
}

/// async 関数からハンドラ関数を作る。
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

/// ブロッキングな関数からハンドラ関数を作る（ハンドラ用プールへ offload して実行）。
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
