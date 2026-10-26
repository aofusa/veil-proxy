//! tower の生態系（tower-http）で書く、Cookie・リダイレクト・静的ファイル・CORS・認証のレシピ（F-267）。
//!
//! - `App::layer` / `App::layer_on` は tower の `Layer` をハンドラに掛ける（veil のワーカーの上で動く）。
//! - tokio を使う tower のサービス（tower-http の `ServeDir`）は `App::tokio_service` で載せる。
//! - 静的ファイルは veil の File バックエンド（`App::serve_dir`）を推奨（sendfile・kTLS・Range・キャッシュが効き、
//!   ハンドラを通らない）。
//!
//! 同じことを veil-proxy-kit で書くと examples/api/rust-kit（`veil_kit::cors`・`auth`・`cookie`・`redirect`・`serve`）。
use http::header::{HeaderName, HeaderValue};
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::validate_request::ValidateRequestHeaderLayer;
use veil::api::tower::VeilBody;
use veil::prelude::*;

#[cfg(unix)]
#[global_allocator]
static GLOBAL: veil::veil_alloc::VeilAlloc = veil::veil_alloc::VeilAlloc;

#[veil::route(path = "/hello", method = "GET")]
async fn hello() -> &'static str {
    "hello"
}

/// Bearer トークンの検査は tower-http の ValidateRequestHeaderLayer::custom（/admin だけ）
#[veil::route(path = "/admin/stats", method = "GET")]
async fn stats() -> &'static str {
    "stats"
}

/// Cookie はヘッダを直接読み書きする（tower-cookies の `Cookies` は http の extensions に入り、veil のハンドラには
/// 渡らない。署名・暗号化・検査が要るなら veil_kit::cookie）
#[veil::route(path = "/theme", method = "GET")]
async fn theme(req: &Request) -> Response {
    match req.header("cookie").and_then(|c| {
        c.split(';')
            .map(str::trim)
            .find(|p| p.starts_with("theme="))
    }) {
        Some(t) => Response::text(t.to_string()),
        None => Response::text("set")
            .with_header("set-cookie", "theme=dark; Path=/; HttpOnly; SameSite=Lax"),
    }
}

/// リダイレクトは 302 と Location（veil_kit::redirect::Redirect は CR/LF の除去と開いたリダイレクトの防止付き）
#[veil::route(path = "/old", method = "GET")]
async fn old() -> Response {
    Response::new(302).with_header("location", "/hello")
}

#[veil::main]
async fn main() -> Result<(), veil::api::Error> {
    let www = std::env::var("VEIL_WWW").unwrap_or_else(|_| "www".into());
    let config = Config::load()?
        .builder()
        .listen_cleartext(&std::env::var("VEIL_LISTEN").unwrap_or_else(|_| "127.0.0.1:8080".into()))
        .build();
    let cors = CorsLayer::new()
        .allow_origin(HeaderValue::from_static("https://app.example.com"))
        .allow_methods([http::Method::GET, http::Method::POST])
        .allow_headers([http::header::CONTENT_TYPE]);
    App::new()
        .config(config)
        // 推奨: veil の静的配信（/www/* → VEIL_WWW/*）
        .serve_dir("/www", &www, veil::api::ServeOptions::default())
        // tower-http の ServeDir（URI のパスをそのまま使うので /files/a.txt → VEIL_WWW/files/a.txt）
        .tokio_service(&["GET", "HEAD"], "/files/{*rest}", ServeDir::new(&www))
        // 認証（/admin だけ）
        .layer_on(
            "/admin",
            ValidateRequestHeaderLayer::custom(|req: &mut http::Request<VeilBody>| {
                let ok = req
                    .headers()
                    .get(http::header::AUTHORIZATION)
                    .is_some_and(|v| v == "Bearer secret");
                if ok {
                    Ok(())
                } else {
                    Err(http::Response::builder()
                        .status(401)
                        .header("www-authenticate", "Bearer")
                        .body(VeilBody::default())
                        .unwrap())
                }
            }),
        )
        // CORS（プリフライトの OPTIONS も、CorsLayer が答える。OPTIONS を受けるルートが要る）
        .route(&["OPTIONS"], "/{*rest}", |_req: Request| async {
            Response::not_found()
        })
        .layer(cors)
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("x-powered-by"),
            HeaderValue::from_static("veil"),
        ))
        .run()
}
