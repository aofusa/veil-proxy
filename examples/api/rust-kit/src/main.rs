//! veil-proxy-kit（F-267）のサンプル。E2E（tests/api/library_e2e.sh kit）がこれを起動して叩く。
//!
//! - Cookie（素・署名付き・暗号化）・リダイレクト（開いたリダイレクトの防止）・静的ファイル（本体の File バックエンド）
//! - CORS（/api）・認証（Basic・API キー・JWT・主体とガード）・セッション
//! - 依存性注入（singleton・scoped・transient、`#[inject]` の自作の抽出子）
//! - バックグラウンドタスク（応答の後・定期・cron）
//! - HTTP クライアント（HTTP/1.1・h2c・HTTPS・UDS）と gRPC のサーバ・生成したクライアント
//! - GraphQL（HTTP・SSE・WebSocket のサブスクリプション）
use async_graphql::{EmptyMutation, Object, Schema, Subscription};
use futures::{Stream, StreamExt};
use serde::Deserialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use veil::api::client::{Client, Http2};
use veil::api::grpc::{
    BoxStream, Code, Request as GrpcRequest, Response as GrpcResponse, Status, Streaming,
};
use veil::api::FromRequestAsync;
use veil::prelude::*;
use veil_kit::auth::jwt::{encode_hmac, Claims, Jwt, JwtKey, JwtVerifier};
use veil_kit::auth::{authenticate, require, ApiKey, Auth, BasicAuth, BearerToken, Principal};
use veil_kit::cookie::{
    Cookie, CookieJar, Key, PrivateCookieJar, ResponseCookieExt, SignedCookieJar,
};
use veil_kit::cors::Cors;
use veil_kit::di::{Container, Dep, Resolver};
use veil_kit::redirect::Redirect;
use veil_kit::session::{MemoryStore, Session, Sessions};
use veil_kit::tasks::{BackgroundTasks, Tasks};

pub mod kit {
    veil::include_proto!("kit.v1");
}
use kit::greeter_client::GreeterClient;
use kit::greeter_server::{add_greeter, Greeter};
use kit::*;

#[cfg(unix)]
#[global_allocator]
static GLOBAL: veil::veil_alloc::VeilAlloc = veil::veil_alloc::VeilAlloc;

const JWT_SECRET: &[u8] = b"kit-example-secret-0123456789abcdef";

// ===================== Cookie・リダイレクト =====================

#[veil::route(path = "/cookie/visit", method = "GET")]
async fn visit(jar: CookieJar) -> Response {
    let n: u32 = jar.get("visits").and_then(|v| v.parse().ok()).unwrap_or(0) + 1;
    Response::text(format!("visit {}", n))
        .with_cookie(Cookie::new("visits", n.to_string()).http_only(true))
}

#[veil::route(path = "/cookie/signed", method = "POST")]
async fn signed_set(jar: SignedCookieJar) -> Response {
    let c = jar.cookie("user", "alice");
    Response::text("set").with_cookie(c)
}

#[veil::route(path = "/cookie/signed", method = "GET")]
async fn signed_get(jar: SignedCookieJar) -> String {
    jar.get("user").unwrap_or_else(|| "invalid".into())
}

#[veil::route(path = "/cookie/private", method = "POST")]
async fn private_set(jar: PrivateCookieJar) -> Response {
    let c = jar.cookie("secret", "s3cr3t");
    Response::text("set").with_cookie(c)
}

#[veil::route(path = "/cookie/private", method = "GET")]
async fn private_get(jar: PrivateCookieJar) -> String {
    jar.get("secret").unwrap_or_else(|| "invalid".into())
}

#[veil::route(path = "/redirect", method = "GET")]
async fn redirect(next: Option<String>) -> Redirect {
    next.as_deref()
        .and_then(|n| Redirect::safe(n, &["app.example.com"]))
        .unwrap_or_else(|| Redirect::to("/"))
}

#[veil::route(path = "/moved", method = "POST")]
async fn moved() -> Redirect {
    Redirect::see_other("/done")
}

// ===================== CORS（/api） =====================

#[veil::route(path = "/api/hello", method = "GET")]
async fn api_hello() -> &'static str {
    "hello"
}

#[veil::route(path = "/api/echo", method = "POST")]
async fn api_echo(req: &Request) -> Response {
    Response::json_bytes(req.body().clone())
}

// ===================== 認証 =====================

#[veil::route(path = "/auth/basic", method = "GET")]
async fn basic(auth: BasicAuth) -> Response {
    if auth.matches("admin", "pw") {
        Response::text(format!("basic {}", auth.user))
    } else {
        Response::error(401, "Unauthorized").with_header("www-authenticate", "Basic realm=\"kit\"")
    }
}

#[veil::route(path = "/auth/key", method = "GET")]
async fn api_key(key: ApiKey) -> Response {
    if key.is_one_of(&["k-123"]) {
        Response::text("key ok")
    } else {
        Response::error(403, "Forbidden")
    }
}

#[derive(Deserialize, veil::Schema)]
struct TokenRequest {
    user: String,
    admin: bool,
}

/// JWT を発行する（HS256）
#[veil::route(path = "/auth/token", method = "POST")]
async fn token(body: Json<TokenRequest>) -> Response {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = Claims {
        sub: Some(body.user.clone()),
        iss: Some("veil-kit-example".into()),
        exp: Some(now + 300),
        scope: Some(if body.admin { "read admin" } else { "read" }.into()),
        ..Default::default()
    };
    match encode_hmac(&JwtKey::hs256(JWT_SECRET), &claims) {
        Ok(t) => Response::text(t),
        Err(e) => Response::error(500, &e.to_string()),
    }
}

#[veil::route(path = "/auth/me", method = "GET")]
async fn me(token: Jwt<Claims>) -> String {
    format!("me {}", token.0.sub.unwrap_or_default())
}

/// /admin は authenticate（JWT → Principal）と require（admin のスコープ）のミドルウェアの後ろ
#[veil::route(path = "/admin/area", method = "GET")]
async fn admin_area(user: Auth<Principal>) -> String {
    format!("admin {}", user.subject)
}

// ===================== セッション =====================

#[veil::route(path = "/session/login", method = "POST")]
async fn login(session: Session, body: &Request) -> &'static str {
    session.insert("user", body.text().to_string());
    session.cycle_id();
    "logged in"
}

#[veil::route(path = "/session/whoami", method = "GET")]
async fn whoami(session: Session) -> String {
    let n: u64 = session.get("n").unwrap_or(0) + 1;
    session.insert("n", n);
    format!(
        "{} {}",
        session
            .get::<String>("user")
            .unwrap_or_else(|| "anonymous".into()),
        n
    )
}

#[veil::route(path = "/session/logout", method = "POST")]
async fn logout(session: Session) -> &'static str {
    session.destroy();
    "bye"
}

// ===================== 依存性注入 =====================

struct Db {
    created: u64,
}
struct RequestId(String);
struct Stamp(u64);

static DB_MADE: AtomicU64 = AtomicU64::new(0);
static STAMPS: AtomicU64 = AtomicU64::new(0);

/// 自作の抽出子（型名が kit の一覧に無いので `#[inject]` を付ける）
struct UserAgent(String);

impl FromRequestAsync for UserAgent {
    async fn from_request_async(req: &Request) -> Result<Self, Response> {
        Ok(UserAgent(
            req.header("user-agent").unwrap_or("-").to_string(),
        ))
    }
}

#[veil::route(path = "/di", method = "GET")]
async fn di(
    db: Dep<Db>,
    a: Dep<RequestId>,
    b: Dep<RequestId>,
    s1: Dep<Stamp>,
    s2: Dep<Stamp>,
    #[inject] ua: UserAgent,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "db": db.created,
        "request_id": a.0.0.clone(),
        "scoped_shared": Arc::ptr_eq(&a.0, &b.0),
        "transient_distinct": s1.0.0 != s2.0.0,
        "ua": ua.0,
    }))
}

// ===================== バックグラウンドタスク =====================

static BG_DONE: AtomicU64 = AtomicU64::new(0);
static BG_BLOCKING_DONE: AtomicU64 = AtomicU64::new(0);
static TICKS: AtomicU64 = AtomicU64::new(0);
static CRON_TICKS: AtomicU64 = AtomicU64::new(0);
static STARTED: AtomicU64 = AtomicU64::new(0);

#[veil::route(path = "/tasks/enqueue", method = "POST")]
async fn enqueue(tasks: BackgroundTasks) -> &'static str {
    tasks.add(async {
        veil::runtime::sleep(Duration::from_millis(50)).await;
        BG_DONE.fetch_add(1, Ordering::SeqCst);
    });
    tasks.add_blocking(|| {
        BG_BLOCKING_DONE.fetch_add(1, Ordering::SeqCst);
    });
    "queued"
}

#[veil::route(path = "/tasks/status", method = "GET")]
async fn task_status() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "background": BG_DONE.load(Ordering::SeqCst),
        "background_blocking": BG_BLOCKING_DONE.load(Ordering::SeqCst),
        "ticks": TICKS.load(Ordering::SeqCst),
        "cron": CRON_TICKS.load(Ordering::SeqCst),
        "started": STARTED.load(Ordering::SeqCst),
    }))
}

// ===================== 静的ファイル（ハンドラから 1 ファイル） =====================

#[veil::route(path = "/download", method = "GET")]
async fn download() -> Response {
    let dir = std::env::var("VEIL_STATIC_DIR").unwrap_or_else(|_| "static".into());
    veil_kit::serve::file(format!("{}/hello.txt", dir), 1 << 20).await
}

// ===================== HTTP クライアント =====================

fn client_for(mode: &str) -> Client {
    let mut b = Client::builder().timeout(Duration::from_secs(10));
    b = match mode {
        "h2c" | "h2" => b.http2(Http2::On),
        "h1" => b.http2(Http2::Off),
        // 信頼アンカーを足さない（自己署名の証明書は拒まれる）
        "noca" => return b.build().expect("client settings"),
        _ => b.http2(Http2::Auto),
    };
    // 信頼する CA（既定は自分の TLS の証明書）
    if let Ok(ca) = std::env::var("VEIL_CLIENT_CA").or_else(|_| std::env::var("VEIL_TLS_CERT")) {
        b = b.ca([ca]);
    }
    b.build().expect("client settings")
}

/// `GET /client/fetch?mode=h1|h2c|h2|auto&url=...`: 自分（127.0.0.1・localhost・unix:）にだけ要求して、
/// ステータス・プロトコル・本文を返す
#[veil::route(path = "/client/fetch", method = "GET")]
async fn client_fetch(mode: Option<String>, url: String) -> Response {
    let local = [
        "http://127.0.0.1:",
        "https://127.0.0.1:",
        "https://localhost:",
        "http://localhost:",
        "http://unix:",
    ];
    if !local.iter().any(|p| url.starts_with(p)) {
        return Response::error(400, "only local URLs");
    }
    let client = client_for(mode.as_deref().unwrap_or("auto"));
    match client.get(&url).header("x-from", "kit-client").send().await {
        Ok(r) => Response::text(format!(
            "status={} version={:?} body={}",
            r.status(),
            r.version(),
            r.text()
        )),
        Err(e) => Response::error(502, &format!("{:?}: {}", e.kind(), e)),
    }
}

/// JSON の往復（POST /api/echo）と再試行の補助
#[veil::route(path = "/client/json", method = "GET")]
async fn client_json() -> Response {
    let port = own_port();
    let client = Client::new();
    let url = format!("http://127.0.0.1:{}/api/echo", port);
    let v: Result<serde_json::Value, _> =
        veil_kit::client::post_json(&client, &url, &serde_json::json!({"n": 42, "s": "x"})).await;
    let retried = veil_kit::client::retry(veil_kit::client::Retry::default(), || {
        client
            .get(&format!("http://127.0.0.1:{}/api/hello", port))
            .send()
    })
    .await
    .map(|r| r.text())
    .unwrap_or_default();
    match v {
        Ok(v) => Response::text(format!("json n={} retry={}", v["n"], retried)),
        Err(e) => Response::error(502, &e.to_string()),
    }
}

fn own_port() -> String {
    std::env::var("VEIL_LISTEN")
        .ok()
        .and_then(|l| l.rsplit(':').next().map(str::to_string))
        .unwrap_or_else(|| "8080".into())
}

// ===================== gRPC =====================

struct GreeterService;

impl Greeter for GreeterService {
    async fn say_hello(
        &self,
        req: GrpcRequest<HelloRequest>,
    ) -> Result<GrpcResponse<HelloReply>, Status> {
        let who = req
            .metadata_value("x-who")
            .map(|v| String::from_utf8_lossy(v).into_owned());
        let name = req.into_inner().name;
        let r = GrpcResponse::new(HelloReply {
            message: format!("Hello {}", name),
        })
        .with_trailer("x-trailer", "t1");
        Ok(match who {
            Some(w) => r.with_metadata("x-who-echo", w),
            None => r,
        })
    }

    async fn count(
        &self,
        req: GrpcRequest<CountRequest>,
    ) -> Result<GrpcResponse<BoxStream<CountReply>>, Status> {
        let n = req.into_inner().count.min(10_000);
        Ok(GrpcResponse::new(Box::pin(futures::stream::iter(
            (1..=n).map(|n| Ok(CountReply { n })),
        ))))
    }

    async fn sum(
        &self,
        req: GrpcRequest<Streaming<Number>>,
    ) -> Result<GrpcResponse<SumReply>, Status> {
        let mut s = req.into_inner();
        let mut total = 0;
        while let Some(n) = s.message().await? {
            total += n.value;
        }
        Ok(GrpcResponse::new(SumReply { total }))
    }

    async fn chat(
        &self,
        req: GrpcRequest<Streaming<Line>>,
    ) -> Result<GrpcResponse<BoxStream<Line>>, Status> {
        let s = req.into_inner().map(|m| {
            m.map(|l| Line {
                text: l.text.to_uppercase(),
            })
        });
        Ok(GrpcResponse::new(Box::pin(s)))
    }

    async fn fail(
        &self,
        req: GrpcRequest<FailRequest>,
    ) -> Result<GrpcResponse<HelloReply>, Status> {
        let r = req.into_inner();
        Err(Status::new(
            Code::from_u8(r.code as u8).unwrap_or(Code::Unknown),
            r.message,
        ))
    }
}

/// 生成したクライアントで自分の gRPC サービスを呼ぶ（`tls=1` なら HTTPS の h2、既定は h2c）
#[veil::route(path = "/grpc/call", method = "GET")]
async fn grpc_call(tls: Option<u8>) -> Response {
    match grpc_calls(tls == Some(1)).await {
        Ok(s) => Response::text(s),
        Err(e) => Response::error(502, &format!("{:?}: {}", e.code(), e.message())),
    }
}

async fn grpc_calls(tls: bool) -> Result<String, Status> {
    let ch = if tls {
        let port = std::env::var("VEIL_TLS_LISTEN")
            .ok()
            .and_then(|l| l.rsplit(':').next().map(str::to_string))
            .ok_or_else(|| Status::unavailable("no TLS listener"))?;
        veil::api::grpc::Channel::with_client(
            &client_for("h2"),
            &format!("https://localhost:{}", port),
        )?
    } else {
        veil::api::grpc::Channel::connect(&format!("http://127.0.0.1:{}", own_port()))?
    }
    .timeout(Duration::from_secs(5));
    let c = GreeterClient::new(ch);
    let hello = c
        .say_hello(
            GrpcRequest::new(HelloRequest { name: "kit".into() }).with_metadata("x-who", "e2e"),
        )
        .await?;
    let echo = hello
        .metadata()
        .iter()
        .find(|(n, _)| &n[..] == b"x-who-echo")
        .map(|(_, v)| String::from_utf8_lossy(v).into_owned())
        .unwrap_or_default();
    let trailer = hello
        .trailers()
        .iter()
        .find(|(n, _)| &n[..] == b"x-trailer")
        .map(|(_, v)| String::from_utf8_lossy(v).into_owned())
        .unwrap_or_default();
    let mut counted = c.count(CountRequest { count: 5 }).await?.into_inner();
    let mut ns = Vec::new();
    while let Some(m) = counted.message().await? {
        ns.push(m.n);
    }
    let sum = c
        .sum(futures::stream::iter(
            (1..=10).map(|value| Number { value }),
        ))
        .await?
        .into_inner()
        .total;
    let chat: Vec<String> = c
        .chat(futures::stream::iter(
            ["a", "b"].map(|t| Line { text: t.into() }),
        ))
        .await?
        .into_inner()
        .into_stream()
        .map(|r| r.map(|l| l.text).unwrap_or_else(|e| format!("err {}", e)))
        .collect()
        .await;
    let failed = match c
        .fail(FailRequest {
            code: 5,
            message: "no such thing".into(),
        })
        .await
    {
        Err(s) => format!("{:?}/{}", s.code(), s.message()),
        Ok(_) => "unexpected ok".into(),
    };
    Ok(format!(
        "hello={} echo={} trailer={} count={:?} sum={} chat={} fail={}",
        hello.get_ref().message,
        echo,
        trailer,
        ns,
        sum,
        chat.join(","),
        failed
    ))
}

// ===================== GraphQL =====================

struct Query;

#[Object]
impl Query {
    async fn add(&self, a: i32, b: i32) -> i32 {
        a + b
    }
}

struct Sub;

#[Subscription]
impl Sub {
    async fn count(&self, upto: i32) -> impl Stream<Item = i32> {
        futures::stream::iter(1..=upto)
    }
}

// ===================== 起動 =====================

#[veil::main]
async fn main() -> Result<(), veil::api::Error> {
    let mut builder = Config::load()?.builder().listen_cleartext(
        &std::env::var("VEIL_LISTEN").unwrap_or_else(|_| "127.0.0.1:8080".into()),
    );
    if let (Ok(listen), Ok(cert), Ok(key)) = (
        std::env::var("VEIL_TLS_LISTEN"),
        std::env::var("VEIL_TLS_CERT"),
        std::env::var("VEIL_TLS_KEY"),
    ) {
        builder = builder.listen(&listen).tls(&cert, &key);
    }
    let static_dir = std::env::var("VEIL_STATIC_DIR").unwrap_or_else(|_| "static".into());

    let di = Container::new()
        .singleton(|_r: Resolver| async {
            Ok(Db {
                created: DB_MADE.fetch_add(1, Ordering::SeqCst) + 1,
            })
        })
        .scoped(|r: Resolver| async move {
            let id = r
                .request()
                .and_then(|q| q.header("x-request-id"))
                .unwrap_or("none")
                .to_string();
            Ok(RequestId(id))
        })
        .transient(|_r: Resolver| async { Ok(Stamp(STAMPS.fetch_add(1, Ordering::SeqCst))) });

    let tasks = Tasks::new()
        .spawn(|| async {
            STARTED.fetch_add(1, Ordering::SeqCst);
        })
        .every(Duration::from_millis(200), || async {
            TICKS.fetch_add(1, Ordering::SeqCst);
        })
        .cron("* * * * * *", || async {
            CRON_TICKS.fetch_add(1, Ordering::SeqCst);
        })
        .map_err(veil::api::Error)?;

    let schema = Schema::build(Query, EmptyMutation, Sub).finish();
    let verifier = JwtVerifier::new([JwtKey::hs256(JWT_SECRET)]).issuer("veil-kit-example");

    let app = App::new()
        .config(builder.build())
        .state(Key::derive_from(b"kit-example-cookie-secret-0123456789").expect("long enough"))
        .state(verifier)
        .with(di)
        .with(tasks)
        .with(
            Cors::new()
                .allow_origins(["https://app.example.com", "https://*.example.org"])
                .allow_credentials(true)
                .on("/api"),
        )
        .with(
            Sessions::new(Key::generate())
                .secure(false)
                .store(MemoryStore::new())
                .on("/session"),
        )
        .with(veil_kit::serve::dir_with(
            "/static",
            &static_dir,
            veil_kit::serve::with_index("index.html"),
        ))
        .with(veil_kit::graphql::mount("/graphql", schema))
        // /admin: JWT を主体（Principal）にし、admin のスコープを要求する（後に登録したミドルウェアが外側＝先に動く）
        .wrap_on("/admin", require(|p: &Principal| p.has_scope("admin")))
        .wrap_on(
            "/admin",
            authenticate(|req: &Request| {
                let Ok(t) = BearerToken::from_headers(req) else {
                    return Ok(None);
                };
                let v = req.state::<JwtVerifier>().expect("registered");
                Ok(v.verify::<Claims>(&t.0).ok().map(|c| c.to_principal()))
            }),
        );
    add_greeter(app, GreeterService).run()
}
