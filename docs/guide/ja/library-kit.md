# ヘルパークレート `veil-proxy-kit`（Rust）

[← ドキュメント目次](README.md) · [English](../library-kit.md) · [API サーバライブラリ](library.md)

`veil-proxy-kit`（ライブラリ名 `veil_kit`）は、[API サーバライブラリ](library.md) の上に、Web API でよく使う部品を足します:
Cookie・リダイレクト・静的ファイル・CORS・認証（Basic・API キー・JWT）・セッション・HTTP と gRPC のクライアントの補助・
GraphQL・依存性注入・バックグラウンドタスク。veil とは別のクレートで、プロキシのバイナリ・veil の既定の feature・`full*` には
入りません。重い依存（async-graphql・prost）は kit の feature の後ろにあります。

Python では、パッケージ `veil-api-kit`（`import veil_kit`）が同じ部品を FastAPI と同じ名前で持ちます。
[library.md#python](library.md#python) と `veil-api` の文書（<https://veil-api.readthedocs.io> の「Kit」）を見てください。

```toml
[dependencies]
veil = { package = "veil-proxy", version = "0.9", default-features = false, features = ["full", "api"] }
veil-kit = { package = "veil-proxy-kit", version = "0.9", features = ["full"] }
```

| モジュール | feature | 内容 |
|---|---|---|
| `cookie` | `cookie`（既定） | `CookieJar` 抽出子、`Cookie`（`Secure`・`HttpOnly`・`SameSite`・`Max-Age`・`Partitioned`、RFC 6265bis の検査、`__Secure-`・`__Host-` の接頭辞）、`SignedCookieJar`（HMAC-SHA256）、`PrivateCookieJar`（AES-256-GCM）、鍵の入れ替えのできる `Key` |
| `redirect` | 常に | `Redirect::to / see_other / temporary / permanent / moved`、開いたリダイレクトを防ぐ `Redirect::safe(url, allowed_hosts)` |
| `serve` | 常に | `serve::dir(prefix, dir)`（veil のファイル配信）、ハンドラから 1 ファイルを返す `serve::file(path, max)` |
| `cors` | `cors`（既定） | プリフライトに自動で答える CORS のミドルウェア `Cors` |
| `auth` | `auth`（既定）・`jwt` | `BasicAuth`・`BearerToken`・`ApiKey<H>`・`Jwt<Claims>`・`Principal`・`Auth<T>`・`authenticate`・`require`・`OAuth2PasswordBearer` |
| `session` | `session` | `Sessions`（暗号化 Cookie かサーバ側の保存先）、`Session` 抽出子、`MemoryStore`、`SessionStore` |
| `client` | 常に | `veil::api::client` の再公開、`get_json`・`post_json`・`retry` |
| `grpc` | `grpc` | `ProstCodec`・`connect`・`Balanced`（ラウンドロビンと切り替え） |
| `graphql` | `graphql`・`graphiql` | async-graphql 7 を HTTP・SSE・WebSocket（`graphql-transport-ws`・`graphql-ws`）で |
| `di` | `di`（既定） | `Container`（singleton・scoped・transient）、`Dep<T>`、`Resolver`、実行中の差し替え |
| `tasks` | `tasks`（既定）・`cron` | `BackgroundTasks`（応答の後）、`Tasks::spawn / every / cron` |
| `full` | – | 上の全部 |

例は [`examples/api/rust-kit`](../../../examples/api/rust-kit/)（全モジュール。gRPC のサーバとクライアント、GraphQL の
サブスクリプションを含む）と、同じことを tower-http だけで行う [`examples/api/rust-tower-recipes`](../../../examples/api/rust-tower-recipes/)。
どちらも `tests/api/library_e2e.sh kit` で動かしています。

## 抽出子とプラグイン

kit の抽出子は veil のものと同じくハンドラの引数に書きます。`#[veil::route]` のマクロは型名で見分けます
（`CookieJar`・`SignedCookieJar`・`PrivateCookieJar`・`BasicAuth`・`BearerToken`・`ApiKey`・`Jwt`・`Auth`・`MaybeAuth`・
`Session`・`Dep`・`BackgroundTasks`）。別の名前で `veil::api::FromRequestAsync` を実装した型には、引数の前に `#[inject]` を書きます:

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

ミドルウェアとフックは `App::with(plugin)`（`Cors`・`Container`・`Tasks`・`Sessions`・`serve::dir(..)`・`graphql::mount(..)`）で、
`auth` の関数は `App::wrap_on(prefix, ..)` で入れます。

## Cookie とリダイレクト

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

App::new().state(Key::derive_from(secret_bytes)?)   // 署名付き・暗号化の jar は App::state から鍵を読む
```

- `CookieJar` は `Cookie` ヘッダを 1 回だけ走査します。値はヘッダの切り出し（`Bytes`）で、複製しません。
- `Cookie::to_header()` は不正な名前・値（CR/LF・`;`・空白）を拒み、`__Secure-`・`__Host-` の接頭辞の条件を守らせ、
  `Max-Age` を 400 日で打ち切ります。`SameSite::None`・`partitioned(true)` は `Secure` を足します。`with_cookie` は不正な Cookie を
  送らずにログへ出します。
- 鍵は `Key::from(&[u8; 64])`（署名 32 バイト・暗号 32 バイト）、`Key::derive_from(secret)`（32 バイト以上）、`Key::generate()`。
  `key.with_fallback(&old)` で古い鍵の Cookie も受け付けます。
- 署名付きの値は `base64url(HMAC-SHA256(name=value)).value`、暗号化の値は名前を関連データにした AES-256-GCM なので、
  Cookie を別の名前へ移せません。暗号は veil の TLS と同じプロバイダの crate（aws-lc-rs、OpenBSD・NetBSD は ring）。

`Redirect` は `IntoResponse` を実装しています。`Redirect::safe(next, &["app.example.com"])` は、`next` が自サイトのパス
（`//host`・`/\host` でない）か、ホストが一覧にある `http(s)` の URL（ユーザー情報付きは拒否）のときだけ `Some` を返します。

## 静的ファイル

`serve::dir("/assets", "/srv/www")` は既定の設定の `App::serve_dir` です: `open_file_cache` と `static_file_cache` を付けた
`type = "File"` のルートで、要求はハンドラを通りません。sendfile・kTLS・Range・ETag がプロキシと同じく効きます。
事前圧縮した `.br`・`.gz` のファイルは使いません。`compression` は HTTP/2・HTTP/3 でファイルをその場で圧縮します（HTTP/1.1 はそのまま送ります）。
`serve::dir_with(prefix, dir, ServeOptions { .. })` は `index`・`mode`・2 つのキャッシュ・`compression`・その他のルートのキーを
取ります。File バックエンドには SPA のフォールバックがありません。必要なら `index.html` を返すルートをハンドラで書いてください。

`serve::file(path, max_size).await` はファイル 1 個をファイル用の offload のプールで読み、拡張子からメディアタイプを決めて返します
（無ければ 404、403、413）。ハンドラが選ぶ小さなファイル向けで、検査していない要求のパスには使わないでください。

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

- プリフライト（`Access-Control-Request-Method` 付きの `OPTIONS`）にはミドルウェアが 204 で答えます。プリフライトが届くよう、
  プラグインは接頭辞の下に `OPTIONS` の受け皿のルートも登録します。
- Origin は完全一致か、先頭の `*.` 1 段（サブドメインだけ）で照合します。他の Origin には CORS のヘッダを付けません。
- `allow_any_origin()` と `allow_credentials(true)` の組み合わせは起動時のエラーです（スキームの無い Origin も）。
- ヘッダの値は起動時に 1 回だけ組み立てます。すべての Origin を許すとき以外は `Vary: Origin` を足します。
- プロキシのルートの `[route.traffic.cors]` とは別物です。同じパスに両方を設定しないでください。

## 認証と認可

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

- `BasicAuth`（`matches(user, password)` は定数時間で比べる）・`BearerToken`・`ApiKey`（`x-api-key`、`KeyHeader` の型で任意のヘッダ。
  `is_one_of(&keys)`）: 無ければ `WWW-Authenticate` 付きの 401。
- `authenticate(f)` は `f` が返した主体を要求の拡張領域（`Request::extensions()`）に置き、`Auth<T>` がそれを読みます（無ければ 401）。
  `MaybeAuth<T>` は省略可。`require(check)` は主体が無ければ 401、条件を満たさなければ 403。後に登録したミドルウェアが先に動くので、
  `require` を `authenticate` より先に登録します。
- `Jwt<C>`（feature `jwt`）は `App::state` の `JwtVerifier` で `Authorization: Bearer` を検証します: `HS256/384/512`・
  `RS256/384/512`・`PS256/384/512`・`ES256`・`ES384`・`EdDSA`。鍵ごとにアルゴリズムが決まり（`alg` が違うトークンは拒否。`none` は
  通らない）、`kid` で鍵を選び、`exp`・`nbf` は 60 秒の猶予、`iss`・`aud` は設定したときに検査します。鍵は秘密・PEM（`PUBLIC KEY`・
  `RSA PUBLIC KEY`）・JWK・JWK Set から作れ、`verifier.refresh_jwks(&client, url).await` で入れ替えます（`Tasks::every` で定期に）。
  `encode_hmac` は HMAC のトークンを発行します。`jsonwebtoken` に依存せず、検証は TLS のプロバイダの crate で行います。
- `OAuth2PasswordBearer::new("/token")` はベアラートークンを読み、OpenAPI の `securitySchemes` の項目（`security_scheme()`）を返します。
  veil が生成する OpenAPI へ自動では入りません。

## セッション

```rust
App::new().with(Sessions::new(Key::from(&material)).store(MemoryStore::new()).on("/app"))

#[veil::route(path = "/app/count", method = "GET")]
async fn count(session: Session) -> String {
    let n: u64 = session.get("n").unwrap_or(0) + 1;
    session.insert("n", n);
    n.to_string()
}
```

保存先が無ければセッション全体が暗号化された 1 個の Cookie です（`veil_session`、`HttpOnly`・`Secure`・`SameSite=Lax`、24 時間、
約 4 KB まで）。`store(..)` を付けると Cookie には署名付きの乱数の ID だけが入り、データはサーバに残ります。`SessionStore` は同期の
trait です（ワーカーの上で呼ぶので短く）。ログインの後は `cycle_id()`、ログアウトでは `destroy()`。Cookie はセッションが変わったときだけ
送ります。

暗号化した Cookie には期限（`ttl`）が入り、サーバが検査するので、クライアントが持ち続けた複製も `ttl` を過ぎれば使えません。
ただしそれより前に失効させることはできません（`destroy()` の後も、その前に保存された複製は期限まで通ります）。失効が要るなら
保存先を使います。`MemoryStore` は期限切れを 1 分ごとに捨て、最大 `max_entries`（既定 100 000）件まで持ちます（一杯になったら
期限の近い 1 割を捨てる）。

## HTTP クライアント

`veil::api::client` は veil 本体の一部（feature `handler`）で、kit は便利関数付きで再公開します。

```rust
use veil::api::client::{Client, Http2};
let client = Client::builder().http2(Http2::Auto).timeout(Duration::from_secs(5)).build()?;
let res = client.post("https://api.example.com/v1/items").json(&item).send().await?;
let created: Item = res.error_for_status()?.json()?;
```

- ワーカーのランタイムの上で動き（ハンドラ・タスクから呼ぶ）、プロキシの上流の部品を使います: 名前解決は offload のプール、TLS は
  上流の TLS クライアント（信頼アンカーは `[tls_client] ca`、または `ClientBuilder::ca(["system", "/etc/ssl/my-ca.pem"])`。mTLS は
  `client_cert(cert, key)`）、HTTP/2 は多重化された上流の接続。
- `Http2::Auto`（既定）: HTTPS は ALPN で `h2` と `http/1.1` を申し出、平文の HTTP は HTTP/1.1。`Http2::On`: TLS の上の h2、
  平文の TCP は prior knowledge の h2c。`Http2::Off`: HTTP/1.1。
- `ClientBuilder::unix_sockets(true)` で作ったクライアントは `http://unix:/run/app.sock:/path` で Unix ソケットへ（既定は無効。
  利用者から受けた URL でローカルのソケットへ届かないように）。制御文字（CR・LF 等）を含むメソッド・ヘッダ・URL は要求に書かず、
  `ErrorKind::Encode`・`ErrorKind::Url` の失敗にします。
- HTTP/1.1 の接続はワーカーごとにプールします（`pool_idle_timeout`、既定 60 秒）。プールの接続が切れていた冪等な要求は、新しい接続で
  1 回だけ送り直します。
- 本文は全部読みます（`max_response_size`、既定 64 MiB）。エラーには `kind()`（`Connect`・`Timeout`・`Protocol`・`Status` 等）があり、
  502・504 の `Response` に変換できます。
- クライアントは起動時に作ってください。独自の CA・クライアント証明書はクライアントを作るときにディスクから読みます。

`veil_kit::client::{get_json, post_json, retry}` は JSON の往復と、接続エラー・タイムアウト・502〜504 での指数バックオフの再試行を足します。

## gRPC クライアント

`veil-build` は `<service>_client` のモジュールも生成します（`Builder::build_client(false)` で止める）:

```rust
use hello::greeter_client::GreeterClient;
let c = GreeterClient::connect("http://127.0.0.1:50051")?;           // https:// は TLS の上の h2
let reply = c.say_hello(HelloRequest { name: "veil".into() }).await?;  // Response<HelloReply>
let mut s = c.count_up(CountRequest { count: 3 }).await?.into_inner(); // ClientStreaming<CountReply>
while let Some(m) = s.message().await? { .. }
let sum = c.sum(futures::stream::iter(numbers)).await?;               // Stream + Unpin なら何でも
```

クライアントは `veil::api::grpc::Channel`（`Channel::connect(url)`、またはクライアントの TLS の設定を使う
`Channel::with_client(&client, url)`）の薄い層で、HTTP/2 のクライアントの上で unary と 3 種のストリーミングを運びます: 双方向の
メタデータ（`Request::with_metadata`・`Response::metadata()`・`trailers()`）、期限（`Request::with_timeout`・`Channel::timeout`。
`grpc-timeout` で送る）、メッセージの圧縮（`Channel::compression`）、`grpc-status`・`grpc-message` からの `Status`。kit の
`grpc::Balanced` は呼び出しを複数のチャネルへ分け、`UNAVAILABLE` の unary を次のチャネルで送り直します。

## GraphQL

```rust
let schema = Schema::build(Query, Mutation, Subscription).finish();
App::new().with(veil_kit::graphql::mount("/graphql", schema))
```

`POST`（単発とバッチ。`Content-Type` は `application/json` に限り、他は 415）と `GET`（問い合わせとサブスクリプション。
`GET` の mutation は `Allow: POST` 付きの 405。他サイトのリンクや `SameSite=Lax` の Cookie で mutation を実行させない）、受け付けるなら `application/graphql-response+json`、SSE（`Accept: text/event-stream`）と
WebSocket（`graphql-transport-ws`・`graphql-ws`、HTTP/1.1）のサブスクリプション、`graphiql` feature ではブラウザの `GET` に GraphiQL
（ページはスクリプトを CDN から読む）。kit は async-graphql 7 に固定しています。

## 依存性注入

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

- `singleton` は最初に使うときに作ります（`value(v)` は作ってある値を登録）。`scoped` は要求ごとに 1 回作り（要求の拡張領域に置き、
  その要求のすべての `Dep` で共有）、`transient` は毎回作ります。
- `container.replace::<T, _, _>(provider)`・`replace_value(v)` はサーバの実行中に提供者を差し替えます（試験・設定のリロード）。
  処理中の要求は古い提供者で終わります。差し替えるにはコンテナの clone を持っておきます。
- スロットは提供者の登録時に作り、起動時に凍結します。要求ごとの処理は不変の表を 1 回引く・`ArcSwap` の load・提供者の呼び出しです。
  依存の循環は最初に解決したときに報告します（500、ログ）。提供者のエラー（`DiError`）は 500 の応答になり、理由はログに出ます。

## バックグラウンドタスク

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
        .cron("0 */5 * * * *", || async { report().await })?   // feature cron、UTC
        .spawn(|| async { consume_queue().await }),
)
```

- `BackgroundTasks` の仕事は応答を送った後（HTTP/1.1 は書き終えた後、HTTP/2・HTTP/3 は最後のフレームを積んだ後）に
  `Response::after_send` で動きます。`add` はワーカーで spawn し、`add_blocking` はハンドラ用のプールを使います。`Tasks` のプラグインが
  必要で、そのミドルウェア（`/`、または `background_on(prefix)`）は要求ごとに小さな確保を 1 回します。
- アプリのタスクは最初に起動したワーカー（`on_all_workers()` なら全ワーカー）で動き、`every` は最大 10% のゆらぎで始まり、重ならず、
  panic を捕まえてログへ出し、graceful shutdown の開始で止まります（実行中の 1 回は中断しません）。cron の式は 6 項目
  （`秒 分 時 日 月 曜日`）か 5 項目で、`*`・数・範囲・列挙・刻みが使えます。

## tower を使う場合

[`examples/api/rust-tower-recipes`](../../../examples/api/rust-tower-recipes/) は同じことを tower-http と `App::layer`・`layer_on` で行います:
`CorsLayer`（プリフライトが届くよう `OPTIONS` の受け皿のルート付き）、`SetResponseHeaderLayer`、ベアラートークンの
`ValidateRequestHeaderLayer::custom`、ヘッダだけで書く Cookie とリダイレクト、`veil::tokio`（`tokio` を shim に差し替え）の上で
`App::tokio_service` に載せた `ServeDir` と、推奨の `App::serve_dir`。tower-cookies の `Cookies` は `http` の要求の拡張領域にあり、
veil のハンドラには届きません。
