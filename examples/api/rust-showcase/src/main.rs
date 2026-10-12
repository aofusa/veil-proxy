//! veil API ライブラリの総合サンプル。
//!
//! - `#[veil::route]` マクロでのルート定義（パス・クエリ・JSON 本文・状態）
//! - `#[derive(veil::Schema)]` による OpenAPI 反映と自動バリデーション（422）
//! - `App` でのプログラム的なルート登録・ブロッキングハンドラ
//! - tower の層（`/admin` だけに認証、全体に応答ヘッダ）
//! - GraphQL（async-graphql）
//! - L4 ネイティブハンドラ（TCP エコー・UDP エコー）
//! - config.toml の取り込みと Rust コードでの上書き
use async_graphql::{EmptyMutation, EmptySubscription, Object, Schema};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use veil::prelude::*;

// アロケータは組み込む側が選ぶ（veil はライブラリとして使うとき #[global_allocator] を定義しない）。
// ここでは veil 専用アロケータ（`full` に含まれる veil-alloc feature、unix のみ）を使う。
// mimalloc 等を使うなら mimalloc クレートを依存に足して `mimalloc::MiMalloc` を指定する。
#[global_allocator]
static GLOBAL: veil::veil_alloc::VeilAlloc = veil::veil_alloc::VeilAlloc;

/// ユーザー（OpenAPI の components.schemas に載る）
#[derive(Debug, Clone, Serialize, Deserialize, veil::Schema)]
struct User {
    id: u64,
    /// 表示名
    #[validate(length(min = 1, max = 50))]
    name: String,
    #[validate(range(min = 0, max = 150))]
    age: u32,
    #[validate(email)]
    email: Option<String>,
}

/// ユーザー作成の入力
#[derive(Debug, Deserialize, veil::Schema)]
struct CreateUser {
    #[validate(length(min = 1, max = 50))]
    name: String,
    #[validate(range(min = 0, max = 150))]
    age: u32,
    #[validate(email)]
    email: Option<String>,
}

/// 一覧の検索条件
#[derive(Debug, Deserialize, veil::Schema)]
struct ListQuery {
    #[validate(range(min = 1, max = 100))]
    limit: Option<u32>,
    prefix: Option<String>,
}

/// 共有状態（`App::state` で登録し、`State<T>` で受け取る）
struct Counter {
    created: AtomicU64,
}

/// 挨拶を返す
#[veil::route(path = "/hello", method = "GET", tags = ["demo"])]
async fn hello() -> &'static str {
    "Hello, World!"
}

/// パスパラメータとクエリパラメータ
#[veil::route(path = "/users/{id}", method = "GET", tags = ["users"])]
async fn get_user(id: u64, verbose: Option<bool>) -> Result<Json<User>, ApiError> {
    if id == 0 {
        return Err(ApiError::not_found("user 0 does not exist"));
    }
    let mut user = User {
        id,
        name: format!("user{}", id),
        age: 20,
        email: None,
    };
    if verbose.unwrap_or(false) {
        user.email = Some(format!("user{}@example.com", id));
    }
    Ok(Json(user))
}

/// JSON 本文（自動バリデーション、失敗は 422）
#[veil::route(path = "/users", method = "POST", status = 201, tags = ["users"])]
async fn create_user(body: Json<CreateUser>, counter: State<Counter>) -> Json<User> {
    let id = counter.created.fetch_add(1, Ordering::Relaxed) + 1;
    Json(User {
        id,
        name: body.name.clone(),
        age: body.age,
        email: body.email.clone(),
    })
}

/// クエリをまとめて構造体で受ける
#[veil::route(path = "/users", method = "GET", tags = ["users"])]
async fn list_users(q: Query<ListQuery>) -> Json<serde_json::Value> {
    let limit = q.limit.unwrap_or(3);
    let prefix = q.prefix.clone().unwrap_or_else(|| "user".into());
    let users: Vec<String> = (1..=limit).map(|i| format!("{}{}", prefix, i)).collect();
    Json(serde_json::json!({ "users": users }))
}

/// 同期関数は自動でハンドラ用プールへ offload される（イベントループをブロックしない）
#[veil::route(path = "/compute/{n}", method = "GET")]
fn compute(n: u64) -> String {
    let n = n.min(1_000_000);
    let sum: u64 = (1..=n).sum();
    format!("{}", sum)
}

/// キャッチオール
#[veil::route(path = "/files/{*rest}", method = "GET")]
async fn files(req: &Request) -> String {
    format!("file: {}", req.param("rest").unwrap_or_default())
}

/// 管理用（tower の層で認証する）
#[veil::route(path = "/admin/stats", method = "GET", tags = ["admin"])]
async fn admin_stats(counter: State<Counter>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "created": counter.created.load(Ordering::Relaxed) }))
}

/// TCP エコー（L4 ネイティブハンドラ）
#[veil::l4_handler(name = "tcp_echo", protocol = "tcp")]
async fn tcp_echo(conn: &mut L4Connection) -> Result<(), L4Error> {
    let mut buf = [0u8; 4096];
    loop {
        let n = conn.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        conn.write_all(&buf[..n]).await?;
    }
}

/// UDP エコー（返り値が送信元へ返る）
#[veil::l4_handler(name = "udp_echo", protocol = "udp")]
async fn udp_echo(dgram: L4Datagram) -> Option<bytes::Bytes> {
    let mut out = b"echo:".to_vec();
    out.extend_from_slice(&dgram.data);
    Some(out.into())
}

struct QueryRoot;

#[Object]
impl QueryRoot {
    /// 2 つの数の和
    async fn add(&self, a: i32, b: i32) -> i32 {
        a + b
    }
    async fn greeting(&self, name: String) -> String {
        format!("Hello, {}!", name)
    }
}

/// `/admin` 以下だけに掛ける簡易認証（tower の Layer）
#[derive(Clone)]
struct RequireToken;

impl<S> tower::Layer<S> for RequireToken {
    type Service = RequireTokenService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        RequireTokenService { inner }
    }
}

#[derive(Clone)]
struct RequireTokenService<S> {
    inner: S,
}

type VeilBody = veil::api::tower::VeilBody;

impl<S> tower::Service<http::Request<VeilBody>> for RequireTokenService<S>
where
    S: tower::Service<http::Request<VeilBody>, Response = http::Response<VeilBody>> + Clone + 'static,
    S::Future: 'static,
{
    type Response = http::Response<VeilBody>;
    type Error = S::Error;
    type Future = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Self::Response, S::Error>>>>;

    fn poll_ready(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<VeilBody>) -> Self::Future {
        let ok = req
            .headers()
            .get("authorization")
            .map(|v| v == "Bearer secret")
            .unwrap_or(false);
        if !ok {
            return Box::pin(async {
                Ok(http::Response::builder()
                    .status(401)
                    .body(VeilBody::new(bytes::Bytes::from_static(b"unauthorized")))
                    .unwrap())
            });
        }
        let fut = self.inner.call(req);
        Box::pin(fut)
    }
}

#[veil::main]
async fn main() -> Result<(), veil::api::Error> {
    // config.toml があれば取り込み、無ければライブラリ既定（平文 0.0.0.0:8080、スレッド数 = CPU 数）。
    // 環境変数 VEIL_LISTEN があればコード側で listen_cleartext（平文）を上書きする例。
    let mut builder = Config::load()?.builder().openapi_title("veil showcase");
    if let Ok(listen) = std::env::var("VEIL_LISTEN") {
        builder = builder.listen_cleartext(&listen);
    }
    let config = builder.build();

    let schema = Schema::build(QueryRoot, EmptyMutation, EmptySubscription).finish();

    App::new()
        .config(config)
        .state(Counter {
            created: AtomicU64::new(0),
        })
        // プログラム的なルート登録（マクロと併用可能）
        .get("/ping", |_req: Request| async { "pong" })
        .route_blocking(&["GET"], "/blocking", |_req| {
            std::thread::sleep(std::time::Duration::from_millis(5));
            "done"
        })
        // tower の層: /admin だけに認証、全体に応答ヘッダ
        .layer_on("/admin", RequireToken)
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            http::header::HeaderName::from_static("x-powered-by"),
            http::HeaderValue::from_static("veil"),
        ))
        .graphql("/graphql", schema)
        .run()
}
