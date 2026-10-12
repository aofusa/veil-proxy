# API サーバライブラリ（Rust・C・Python）

[← ドキュメント目次](README.md) · [English](../library.md)

veil はリバースプロキシとして使えるだけでなく、自分のプログラムに **ライブラリ** として組み込んで API サーバを書くことができます。
普通の関数を HTTP ハンドラ（または TCP/UDP ハンドラ）として登録すると、io_uring・TLS・kTLS・HTTP/1.1・HTTP/2・HTTP/3・
ホットリロード・サンドボックスといった veil のネットワーク層がそのまま動きます。

3 つのフロントエンドが同じ実装を共有します。

| フロントエンド | 場所 | 主な用途 |
|----------------|------|----------|
| Rust ライブラリ | `veil` クレート（`api` feature 群） | 最速。マクロ・OpenAPI・tower・GraphQL |
| C API | [`crates/veil-capi`](../../../crates/veil-capi/README.md)（`libveil.so` / `libveil.a`、`veil.h`） | C・C++ など C FFI を持つ言語 |
| Python | [`crates/veil-python`](../../../crates/veil-python/README.md)（wheel、`import veil`） | FastAPI 風のデコレータ |

- [アーキテクチャ](#アーキテクチャ)
- [veil への依存](#veil-への依存)
- [ライブラリモードの設定](#ライブラリモードの設定)
- [ルーティングマクロ](#ルーティングマクロ)
- [Request・Response・エラー](#requestresponseエラー)
- [バリデーションと OpenAPI](#バリデーションと-openapi)
- [共有状態](#共有状態)
- [tower の層と tokio 前提ライブラリ](#tower-の層と-tokio-前提ライブラリ)
- [GraphQL](#graphql)
- [L4（TCP/UDP）ハンドラ](#l4tcpudpハンドラ)
- [TLS と HTTP/3](#tls-と-http3)
- [メモリアロケータ](#メモリアロケータ)
- [C API](#c-api)
- [Python](#python)
- [ライブラリ成果物のビルド](#ライブラリ成果物のビルド)
- [テスト](#テスト)
- [制限事項](#制限事項)

## アーキテクチャ

- ハンドラは、veil の通常のルーティング表に `type = "Handler"` のルートとして載ります。設定のロード時にライブラリが
  `[[route]]` を注入するので、`config.toml` に書いた `File`・`Proxy` などのルートと共存できます。
- **ハンドラは veil 独自のランタイム上で動きます。** `async fn` のハンドラは、接続を受け付けたワーカースレッド上で、
  その接続を処理しているのと同じ io_uring（または epoll/kqueue）のイベントループで poll されます。データプレーンに
  tokio・monoio はありません。tokio 前提のライブラリ（sqlx・reqwest など）が必要なときは、データプレーンに tokio を
  入れずに済む橋渡し（[tower の層と tokio 前提ライブラリ](#tower-の層と-tokio-前提ライブラリ)）を使います。
- ワーカー上でブロッキング処理をしてはいけません。Rust の **同期** `fn` ハンドラ・C のコールバック・Python のハンドラは、
  veil の `offload` を通じて別のハンドラ用スレッドプール（`[api] handler_threads`）で実行されるので、遅い DB 呼び出しが
  他の接続を止めることはありません。
- **リリースバイナリにライブラリのコードは入りません。** `handler`・`macro`・`openapi`・`tokio-interface`・`graphql`・
  `l4-handler`・`api` の各 feature は `default` にも `full*` にも含まれません。`cargo build --features full` で作るプロキシは
  従来とまったく同じです。
- プロキシで使えるものはそのまま使えます。`[server]`・`[tls]`・`[security]`・`[logging]`・`[prometheus]` などのセクション、
  SIGHUP リロード、graceful shutdown（SIGINT/SIGTERM または `veil::shutdown()`）です。

## veil への依存

### Cargo feature

| feature | 追加されるもの |
|---------|----------------|
| `handler` | ネイティブ `Handler` ルート、`Request`/`Response`、設定ビルダ、ライブラリ既定設定（`http2` を含意） |
| `macro` | `#[veil::route]`・`#[veil::main]`・`#[veil::l4_handler]`・`#[derive(veil::Schema)]`（`handler` を含意） |
| `openapi` | OpenAPI 3.1 の生成、`/openapi.json` と Swagger UI（`macro` を含意） |
| `tokio-interface` | tower の `Layer`/`Service` 互換と tokio への橋渡し（`veil::api::tower`・`veil::api::tokio_compat`） |
| `graphql` | `async-graphql` スキーマ向けの `App::graphql()` |
| `l4-handler` | `[[l4]]` リスナーの TCP/UDP を Rust 関数で処理（`l4-proxy` を含意） |
| `api` | 上記すべて（`handler`・`macro`・`openapi`・`tokio-interface`・`graphql`・`l4-handler`） |

`vds` と `acme` は別の feature で、`full*` に含まれ、プロキシ本体でもライブラリでも使えます（[VDS](vds.md)・[ACME](acme.md) を参照）。

### Cargo.toml

veil は crates.io には公開されていません。git 依存か path 依存を使います。feature は用途で選びます。`full` は HTTP/3・kTLS・
gRPC・WASM などプロキシ機能を一通り含み、コンテナ向けは `full-container` です。`default-features` は切ります
（アロケータの選び方は [メモリアロケータ](#メモリアロケータ) を参照）。

```toml
[dependencies]
veil = { git = "https://github.com/aofusa/veil-proxy", default-features = false, features = ["full", "api"] }
# ローカルのチェックアウトを使う場合:
# veil = { path = "../veil-proxy", default-features = false, features = ["full", "api"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# HTTP/3 を使うときの推奨: veil がビルドに使う vendoring 版 quiche を使う。
# 複数ストリームの STREAM フレームを 1 パケットへ詰める差分で、外すと小さな応答が
# 1 リクエスト 1 パケットに戻る（F-172）。cargo は最上位パッケージの [patch] しか見ない。
[patch.crates-io]
quiche = { git = "https://github.com/aofusa/veil-proxy" }
# または: quiche = { path = "../veil-proxy/third_party/quiche" }
```

平文 HTTP の API だけなら `default-features = false` と `features = ["api"]` で足ります（TLS・HTTP/3 なし）。
`examples/api/rust-showcase` はすべての機能を使った完全なプロジェクトです。

### 最小のプログラム

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

`veil::serve(config)` は停止まで呼び出しスレッドをブロックします。マクロを使わない場合は `App` ビルダを使います。

```rust
use veil::prelude::*;

fn main() -> Result<(), veil::api::Error> {
    App::new()
        .get("/hello", |_req: Request| async { "hello" })
        .post("/echo", |req: Request| async move { req.body().clone() })
        .run()
}
```

`App` には `get/post/put/delete/patch`、`route(&[メソッド], パス, f)`、`route_blocking(...)`（ハンドラ用プールで実行）、
`state(値)`、`config(cfg)`、`without_macro_routes()`、`on_setup(f)`（ルート登録の前に 1 回実行）があります。

## ライブラリモードの設定

ライブラリには **既定設定**（クレートに埋め込み、`src/api/default_config.toml`）があります。

```toml
[server]
listen = "0.0.0.0:8080"
h2c_enabled = true     # 平文 HTTP/2（prior knowledge）と ...
h2c_http1 = true       # ... 同じポートの平文 HTTP/1.1
tls_only = false
http2_enabled = true

[logging]
level = "info"

[security]
enable_seccomp = false # ホスト言語やユーザコードを制限しない
enable_landlock = false

[api]
openapi_enabled = true
openapi_path = "/docs"
openapi_json_path = "/openapi.json"
openapi_title = "veil API"
openapi_version = "0.1.0"
```

これに `[server] threads` = CPU 数が加わります。設定ファイルが無くても起動し、`0.0.0.0:8080` で平文の HTTP/1.1 と h2c を受け、
**TLS 証明書は不要** です（`[tls]` を省略できるのはこの平文専用構成だけです。[設定リファレンス](configuration.md#tls-なしの平文専用サーバ)を参照）。

### 設定の合成と優先順位

設定は次の順（下ほど優先）で合成されます。

1. 上記のライブラリ既定設定
2. カレントディレクトリの `config.toml`（あれば。`Config::load()` / `App::new()`）、または `Config::from_file(path)` で渡したファイル、
   または `Config::from_toml_str(..)` で渡した文字列。既定設定の上に深くマージされるので、変えたいキーだけ書けば足ります
3. 環境変数 `VEIL_OVERRIDE_<n>`（プロキシと同じ規則。[起動・運用](running.md#環境変数による設定の上書きveil_override_n)を参照）
4. Rust コードでのビルダ（`ConfigBuilder`）による上書き

そのあとハンドラのルートと組み込みの OpenAPI ルートが注入され、全ルートが具体性の高い順に並べ替えられます
（[ルーティングマクロ](#ルーティングマクロ)）。

同じ合成が SIGHUP リロードのたびに行われるので、ビルダと環境変数の上書きは **リロード後も保たれます**。読み直されるのは
`config.toml` の内容だけです。`[server] listen`・`[tls]`・`[[l4]]`・スレッド数の変更に再起動が要るのはプロキシと同じです。

### ビルダ

```rust
let config = Config::load()?            // または Config::new()（既定設定だけ）、Config::from_file("app.toml")?
    .builder()
    .listen("0.0.0.0:8080")             // [server].listen
    .threads(4)                         // [server].threads
    .log_level("warn")                  // [logging].level
    .openapi_title("My API")            // [api].*
    .max_body_size(1 << 20)
    .set("security.enable_seccomp = true")?     // 任意のキー。`--override` と同じ構文
    .build();
```

型付きのセッタは `listen`・`threads`・`tls_only`・`h2c_enabled`・`http2_enabled`・`log_level`・`openapi_enabled`・
`openapi_path`・`openapi_json_path`・`openapi_title`・`openapi_version`・`max_body_size`・`handler_threads`・
`tls(cert, key)`・`http3(listen)` です。それ以外のキーは `set("key = value")` と `set_value(path, toml_value)` で上書きできます。
`Config::to_toml_string()` は合成結果（環境変数の上書きとルート注入の前）を返すので、デバッグに使えます。

### `[api]` キー（ライブラリ専用）

`[api]` はライブラリモードでだけ読まれます。プロキシ本体のバイナリは知りません。

| キー | 既定値 | 説明 |
|------|--------|------|
| `openapi_enabled` | `true` | `/openapi.json` と Swagger UI を公開する |
| `openapi_path` | `"/docs"` | Swagger UI のパス（ブラウザが unpkg.com から `swagger-ui-dist` を読む） |
| `openapi_json_path` | `"/openapi.json"` | OpenAPI ドキュメントのパス |
| `openapi_title` | `"veil API"` | `info.title` |
| `openapi_version` | `"0.1.0"` | `info.version`（API 自体のバージョン） |
| `openapi_description` | なし | `info.description` |
| `max_body_size` | `10485760` | ハンドラへ渡すリクエスト本文の上限（バイト） |
| `handler_threads` | `0` | 同期 Rust 関数・C コールバック・Python ハンドラを実行するハンドラ用プールのスレッド数（`0` = 組み込みの既定） |
| `tokio_threads` | `2` | tokio 互換ランタイムのワーカー数 |

## ルーティングマクロ

```rust
#[veil::route(path = "/users/{id}", method = "GET", tags = ["users"], summary = "ユーザー取得")]
async fn get_user(id: u64, verbose: Option<bool>) -> Result<Json<User>, ApiError> { ... }
```

| 引数 | 意味 |
|------|------|
| `path`（必須） | パスのパターン。`{name}` は 1 セグメント（空は不可）、末尾の `{*name}` は残り全部（空も可）、それ以外は完全一致 |
| `method = "GET"` / `methods = ["GET", "POST"]` | 許可するメソッド。省略すると全メソッド。`GET` のルートは `HEAD` にも応答する |
| `name` | ハンドラ名（既定は関数名）。重複不可 |
| `tags`・`summary`・`description` | OpenAPI のメタデータ（`summary`/`description` を省略すると doc コメントを使う） |
| `status = 201` | OpenAPI に載せる成功ステータス |
| `offload = true/false` | ハンドラ用プールで実行するか。同期 `fn` は既定 `true`、`async fn` は既定 `false` |
| `hidden = true` | OpenAPI ドキュメントに載せない |

### 引数の束縛（FastAPI 方式）

引数は型と名前で束縛されます。

| 引数 | 取得元 |
|------|--------|
| `&Request` / `Request` | リクエスト全体 |
| `Json<T>` | JSON 本文 |
| `Query<T>` | クエリ文字列全体を `T` にデシリアライズ |
| `Form<T>` | `application/x-www-form-urlencoded` の本文 |
| `State<T>` | `App::state` で登録した共有状態 |
| `Headers`・`Bytes`（別名 `Body`） | リクエストヘッダ / 生の本文 |
| パスの `{name}` と同名の引数 | パスパラメータ（`FromStr` で変換） |
| それ以外の単純な型 | クエリパラメータ。`Option<T>` なら省略可 |

束縛の失敗（欠落・変換失敗）と検証の失敗は、FastAPI 形式の **422** を返します（後述）。

### 並び順と衝突

first-match のルーティングでは記述順が意味を持ってしまうため、ライブラリは全ルート（マクロ・`App`・`config.toml` の `[[route]]`）を
具体性の高い順に並べ替えます。パスのセグメントが多い順、静的セグメントが多い順、前方一致・キャッチオールより完全一致、
メソッド条件のあるもの、`host`/`header`/`query`/`source_ip` 条件が多い順で、同順位なら宣言順です。
`/users/me` は `/users/{id}` より先に、`/users/{id}` は `/users/{*rest}` より先に評価されます。

host・正規化したパス（パラメータ名は無視）・その他の条件が **同一** で、メソッド集合が重なる（または両方メソッド無指定の）2 つのルートは、
後者が到達不能になるため **起動エラー** になります（両方のルートを表示します）。`config.toml` で `type = "Handler"`・`handler = "get_user"` のように
ハンドラ名を指したルートは、そのハンドラ用に自動注入されるルートを置き換えるので、`[route.security]` などのルート設定を足せます。

> ハンドラのパスは完全一致です。TOML の `[[route]]` の静的な `path` はセグメント境界での前方一致です。

## Request・Response・エラー

`Request` には `method()`・`path()`・`query()`・`query_param(name)`・`header(name)`・`headers()`・`body()`（`&Bytes`）・`text()`・
`json::<T>()`・`param(name)`・`param_as::<T>(name)`・`client_ip()`・`version()`（HTTP/1.1・2・3）・`state::<T>()` があります。

ハンドラは `IntoResponse` を実装する型なら何でも返せます。

| 戻り値の型 | 応答 |
|------------|------|
| `&'static str`・`String` | 200 `text/plain` |
| `Html<T>` | 200 `text/html` |
| `Json<T>`（`T: Serialize`）・`serde_json::Value` | 200 `application/json` |
| `Bytes`・`Vec<u8>` | 200 バイナリ |
| `()` | 204 |
| `(u16, T)` | ステータスの上書き（例: `(201, Json(..))`） |
| `Option<T>` | `None` は 404 |
| `Result<T, E>` | 両方が `IntoResponse` |
| `Response` | 完全な制御: `Response::new(status)`・`Response::json(&v)`・`.with_status()`・`.with_header()`・`.with_body()` |
| `ApiError` | 指定ステータスで `{"detail": "メッセージ"}` |

`ApiError::new(status, msg)`・`bad_request`・`not_found`・`internal` は `?` で返す用途です。`serde_json::Error` は 400 の `ApiError` に変換されます。

## バリデーションと OpenAPI

`#[derive(veil::Schema)]` は構造体・列挙に JSON Schema（`ApiSchema`）とバリデータ（`Validate`）を実装します。`Json<T>`・`Query<T>`・
`Form<T>` 引数の `T` がこれを derive していれば、マクロが自動で検証します。

```rust
#[derive(Deserialize, veil::Schema)]
struct CreateUser {
    /// 表示名
    #[validate(length(min = 1, max = 50))]
    name: String,
    #[validate(range(min = 0, max = 150))]
    age: u32,
    #[validate(email)]
    email: Option<String>,
    #[schema(example = "https://example.com", description = "ホームページ")]
    #[validate(url)]
    home: Option<String>,
}
```

| 属性 | 効果 |
|------|------|
| `#[validate(range(min, max, exclusive_min, exclusive_max))]` | 数値の範囲。スキーマの `minimum`/`maximum` にも反映 |
| `#[validate(length(min, max, equal))]` | 文字列・コレクションの長さ。`minLength`/`maxLength` |
| `#[validate(email)]`・`#[validate(url)]` | 簡易な形式検査。スキーマの `format` |
| `#[validate(nested)]` | 内側の構造体も検証する |
| `#[validate(custom = "path::to::fn")]` | `fn(&T) -> Result<(), impl Display>` |
| `#[schema(description, example, format, pattern)]` | スキーマのメタデータ（doc コメントも `description` になる） |
| serde の `rename`・`rename_all`・`default`・`skip`・`flatten` | スキーマの生成時に尊重される |

未知の `#[validate(...)]` キー（`regex` など）は受け付けて無視するので、`validator` クレートの derive と併用できます。

### 422 の形式

失敗時は FastAPI と同じ形で 422 を返します。

```json
{"detail": [
  {"type": "less_than_equal", "loc": ["query", "limit"], "msg": "Input should be less than or equal to 100"},
  {"type": "too_short", "loc": ["body", "name"], "msg": "Value should have at least 1 item(s)/character(s)"}
]}
```

`loc` は `path`・`query`・`body` のいずれかで始まり、続けてフィールドのパス（配列の添字は数値）が入ります。
主な `type` は `missing`・`parsing`・`json_invalid`・`greater_than_equal`・`less_than_equal`・`too_short`・`too_long`・`length`・
`value_error`・`url_parsing` です。

### OpenAPI と Swagger UI

`openapi` feature では、マクロのメタデータ（パス・パラメータ・リクエスト本文・戻り値の型から推測した応答
（`Json<T>`・`Result<Json<T>, _>`・`(u16, Json<T>)`・文字列）・`components.schemas`・検証付きルートの 422 応答）から
OpenAPI **3.1** のドキュメントを起動時（とリロードのたび）に 1 回だけ生成してメモリに保持し、リクエスト時は保持したバイト列を返すだけです。
`openapi_json_path`（`/openapi.json`）で JSON を、`openapi_path`（`/docs`）で Swagger UI を公開します。`[api] openapi_enabled = false` で両方を止められます。

## 共有状態

```rust
struct Db { /* プール */ }

#[veil::route(path = "/items", method = "GET")]
async fn items(db: State<Db>) -> Json<Vec<String>> { ... }

App::new().state(Db::connect()?).run()?;
```

`State<T>` は `Arc<T>` です。値は起動時に固定され、ロック無しで読まれます。抽出子の外では `veil::api::state::<T>()` と
`req.state::<T>()` で読めます。ハンドラは複数のワーカースレッドで動くので、`T` は `Send + Sync` で、内部可変性
（アトミック・`Mutex` など）を使ってください。

## tower の層と tokio 前提ライブラリ

`tokio-interface` が必要です。リクエストとレスポンスは `http::Request<Full<Bytes>>` / `http::Response` でやり取りします。

```rust
App::new()
    // 全ルートに tower の Layer を掛ける。veil のワーカー上で実行される（中で tokio のタイマーや I/O を使わない）
    .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(name, value))
    // /admin 以下のルートだけ（セグメント境界の前方一致）
    .layer_on("/admin", RequireToken)
    // 既存の tower Service（axum の Router など）を tokio ランタイムで動かしてマウントする
    .tokio_service(&["GET", "POST"], "/legacy/{*rest}", axum_router)
    .run()?;
```

- `layer` / `layer_on` は veil のイベントループの中でハンドラを包みます。tokio のリアクタを必要としない層
  （認証・ヘッダ付与・CORS など）向けです。tokio のタイマーを使う層（tower の `Timeout` など）は tokio ランタイムが要るので、ここでは使えません。
- `tokio_service` は tokio 互換ランタイムでサービスを実行し、結果は `offload` と同じ「結果スロット + 通知 fd」でワーカーへ返します
  （スレッドをまたいで `Waker` を呼びません）。
- 任意のハンドラの中で tokio 前提のライブラリを使うときは `veil::api::tokio_compat::run` を通します。

  ```rust
  let body = veil::api::tokio_compat::run(async move {
      reqwest::get("https://example.com").await?.text().await
  }).await?;
  ```

  `run` はタスクが panic すると panic します（→ 500）。`try_run` は `Result` を返し、`spawn` は切り離したタスクを起動し、
  `handle()` は `tokio::runtime::Handle` を返します。ランタイムは遅延起動で、ワーカー数は `[api] tokio_threads`（既定 2）です。
  データプレーン（受付・TLS・HTTP・ハンドラの poll）が tokio 上で動くことはありません。

## GraphQL

`graphql` が必要です。`async-graphql` のスキーマは利用者が用意します（`Cargo.toml` に `async-graphql` を追加）。

```rust
let schema = Schema::build(QueryRoot, EmptyMutation, EmptySubscription).finish();
App::new().graphql("/graphql", schema).run()?;
```

`POST` は JSON 本文（単発またはバッチ）、`GET` は `?query=...&variables=...` を受けます。クエリの無い `GET` でブラウザ
（`Accept: text/html`）なら GraphiQL を返します。クライアントが求めたときは `application/graphql-response+json`、
それ以外は `application/json` で応答します。サブスクリプションは非対応です。

## L4（TCP/UDP）ハンドラ

`l4-handler` が必要です。`handler` キーを持つ `[[l4]]` リスナーは上流へ転送せず、TCP 接続（または UDP データグラム）を Rust 関数へ渡します。
`handler` の無いリスナーは従来の `splice(2)` 転送経路のままです。[L4 プロキシ](l4-proxy.md#ネイティブハンドラ)も参照してください。

```rust
#[veil::l4_handler(name = "tcp_echo", protocol = "tcp")]
async fn tcp_echo(conn: &mut L4Connection) -> Result<(), L4Error> {
    let mut buf = [0u8; 4096];
    loop {
        let n = conn.read(&mut buf).await?;      // 0 = 相手が閉じた
        if n == 0 { return Ok(()); }
        conn.write_all(&buf[..n]).await?;
    }
}

#[veil::l4_handler(name = "udp_echo", protocol = "udp")]
async fn udp_echo(dgram: L4Datagram) -> Option<bytes::Bytes> {
    Some(dgram.data)                              // Some(..) は送信元へ返信される
}
```

```toml
[[l4]]
name = "tcp_echo"
listen = "0.0.0.0:9000"
handler = "tcp_echo"        # upstreams は不要

[[l4]]
name = "udp_echo"
listen = "0.0.0.0:9001"
protocol = "udp"
handler = "udp_echo"
```

TOML を書かずに `App::l4_tcp(name, listen, f)` / `App::l4_udp(name, listen, f)` でハンドラ登録と `[[l4]]` の注入を一度に行えます
（`config.toml` に同じ `name` の `[[l4]]` があればそちらが優先されます）。`L4Connection` には `read`・`read_bytes`・`write`・`write_all`・
`write_bytes`（`Bytes` をコピーせず送る）・`shutdown_write`・`peer_addr`・`listener_name` があり、すべての操作がリスナーの
`idle_timeout_secs` で打ち切られます（`L4Error::Timeout`）。存在しないハンドラ名や、逆のプロトコルで登録されたハンドラ名は起動エラーです。
`[[l4]]` リスナーは他と同様、SIGHUP では再読み込みされません。

## TLS と HTTP/3

既定は平文です。HTTPS（HTTP/1.1 + ALPN による HTTP/2）にするには次のようにします。

```rust
let config = Config::load()?.builder().listen("0.0.0.0:8443").tls("/etc/veil/cert.pem", "/etc/veil/key.pem").build();
```

`tls()` は `[tls] cert_path/key_path` を設定し、h2c 専用モードを解除します（`h2c_enabled = false`、`h2c_http1 = false`）。
`.http3("0.0.0.0:8443")` を足すと UDP ポートで HTTP/3 も受けます（`http3` feature が必要で、`full` に含まれます）。
そのほかの `[tls]`・`[http2]`・`[http3]`・`[security]` のキーは `config.toml` か `set(..)` で設定します。ライブラリモードでは
サンドボックスは **既定でオフ** です。`enable_seccomp` / `enable_landlock` を有効にすることもできますが、ハンドラが行う操作
（ファイル・ソケット・DNS）を許可する必要があります。

## メモリアロケータ

`veil` クレート自身が、アロケータ feature が有効なときにプロセス全体の `#[global_allocator]` を宣言します
（unix では `veil-alloc`、または `mimalloc`。`full` は `veil-alloc` を、`default` は `mimalloc` を含みます）。そのため veil に依存する
Rust プログラムはプロセス全体でそのアロケータになり、同じバイナリで自前の `#[global_allocator]` を宣言すると
（重複して）コンパイルエラーになります。次のどちらかを選びます。

- プロキシと同じアロケータをそのまま使う（`features = ["full", "api"]` なら何もしない）。
- 自前のアロケータを使う。`full`・`default`・`veil-alloc`・`mimalloc`・`jemalloc` を有効にせず、必要な feature を個別に並べ
  （例: `default-features = false, features = ["ktls", "http2", "http3", "api"]`）、自分のバイナリで `#[global_allocator]` を宣言する。

アロケータ feature は排他です（[メモリアロケータ](performance.md#メモリアロケータf-183)を参照）。
C と Python のビルド（[`veil-capi`](../../../crates/veil-capi/README.md)・[`veil-python`](../../../crates/veil-python/README.md)）は
アロケータ feature を一切有効にしないので、ホストプロセス（C プログラムや Python インタプリタ）のアロケータを置き換えません。
また io_uring ではなく epoll（readiness リアクタ）バックエンドでビルドされ、gRPC・WASM・VDS・ACME は含みません。

## C API

`crates/veil-capi` は `libveil.so`（動的）・`libveil.a`（静的）とヘッダ [`veil.h`](../../../crates/veil-capi/include/veil.h) をビルドします。
ルートごとに C のコールバックを登録し、リクエストとレスポンスのオブジェクトを受け取ります。

```c
#include "veil.h"

static int hello(const veil_request_t *req, veil_response_t *resp, void *ud) {
    return veil_response_set_text(resp, "Hello from C");
}

int main(void) {
    veil_config_t *cfg = veil_config_new();           /* または veil_config_from_file("config.toml") */
    veil_config_listen(cfg, "0.0.0.0:8080");
    veil_register_route(cfg, "/hello", "GET", hello, NULL);
    return veil_run(cfg);                              /* ブロックする。cfg は消費される */
}
```

```bash
cargo build -p veil-capi --release      # target/release/libveil.so と libveil.a
gcc hello.c -Icrates/veil-capi/include -Ltarget/release -lveil -lpthread -ldl -lm \
    -Wl,-rpath,$PWD/target/release -o hello          # 動的リンク
gcc hello.c -Icrates/veil-capi/include target/release/libveil.a -lpthread -ldl -lm -o hello   # 静的リンク
```

**スレッドモデル。** 既定ではコールバックは **ハンドラ用プール**（`[api] handler_threads`）で実行されるので、ブロックしてもかまいません（DB・ファイル）。
ただし **複数のスレッドから同時に呼ばれる** ので、共有データを保護し、`user_data` をスレッドセーフにしてください。
`veil_register_route_ex(..., VEIL_ROUTE_INLINE)` はコールバックをネットワークワーカー上で直接実行します（スレッドの切り替えが無い）。
即座に終わる処理だけに使ってください。ブロックするとそのワーカーの全接続が止まります。リクエストアクセサが返すポインタは
NUL 終端ではなく、コールバックから戻るまで有効です。OpenAPI は C のルートにも生成されます（`veil_register_route_ex` に
Operation Object を JSON で渡すと説明が反映されます）。設定は `veil_config_set(cfg, "server.threads", "4")` で上書きでき、
`VEIL_OVERRIDE_<n>` も通常どおり効きます。詳細（所有権の表・エラーコード・API 一覧）は
[crates/veil-capi/README.md](../../../crates/veil-capi/README.md)、サンプルは [examples/api/c-hello/](../../../examples/api/c-hello/) です。

## Python

`crates/veil-python` は PyO3 の拡張（abi3、Python 3.8 以上）で、`veil` として import します。

```bash
pip install maturin
cd crates/veil-python && maturin develop --release     # または maturin build --release（wheel は target/wheels/）
pip install pydantic                                    # 任意
```

```python
import veil

@veil.get("/hello/{name}")
def hello(name: str, greeting: str = "Hello"):          # name はパス、greeting はクエリ（注釈で型変換）
    return {"message": f"{greeting}, {name}"}

@veil.post("/users", status_code=201,
           body_constraints={"name": {"type": "string", "minLength": 1, "required": True}})
def create_user(body: dict):
    return body

veil.run(listen="0.0.0.0:8080")                         # ブロックする。Ctrl-C / SIGTERM で graceful に停止
```

- デコレータは `@veil.get/post/put/delete/patch/route`。`app = veil.App(title=..., version=...)` を作って `@app.get(...)`・`app.run()` も使えます。
  引数は名前と注釈で束縛され、戻り値は変換されます（dict/list → JSON、`str` → テキスト、`veil.Response`、`(body, status)`、`None` → 204）。
  `veil.HTTPException(404, "...")` は `{"detail": ...}` になります。**pydantic** があれば `BaseModel` の本文を検証して OpenAPI に反映し、
  無ければ `body_constraints` で dict の本文を検証します。OpenAPI は `/openapi.json` と `/docs` で公開されます。
- **offload と GIL。** ハンドラは GIL を握って実行されます。既定（`offload=True`）ではハンドラ用プールで実行されるので、
  イベントループが GIL を待つことはありません。`offload=False` はネットワークワーカー上で直接実行する指定で、自明に軽いハンドラ専用です。
  純 Python のハンドラの並列度は GIL に制限されるので、CPU バウンドな処理は複数プロセス構成にしてください。
- 設定: `veil.run(config_path=None, *, listen, threads, overrides={"server.threads": 4}, openapi=True)`。`VEIL_OVERRIDE_<n>` も通常どおり効きます。

詳細は [crates/veil-python/README.md](../../../crates/veil-python/README.md)、サンプルは
[examples/api/python-hello/](../../../examples/api/python-hello/)・
[examples/api/python-fastapi-like/](../../../examples/api/python-fastapi-like/) です。

## ライブラリ成果物のビルド

Rust のライブラリは単なる Cargo 依存です。C と Python の成果物は次のスクリプトで作ります。

```bash
packaging/scripts/build-lib.sh [--kind all|rust|c|python] [--docker]
```

指定した種類（既定は `all`）をビルドし（`--docker` は `docker/Dockerfile.lib` を使ってコンテナ内でビルド）、
`packaging/output/lib/` に 1 つのアーカイブ `veil-lib-<version>-<arch>-linux-gnu.zip` を出力します。中身は
`libveil.so`/`libveil.a` と `veil.h`（C）、Python の wheel、Rust 向けのソースと使い方のメモです。
プロキシ本体のパッケージは[ビルドとパッケージ](building.md)を参照してください。

## テスト

```bash
tests/api/library_e2e.sh [rust|c|python|nonregression|all]     # 既定: all
```

Rust のショーケース、C のサンプル（動的・静的）、Python のサンプルをビルド・起動し、curl で叩きます（ルーティング・検証・
OpenAPI・tower・GraphQL・L4 エコー・HTTP/1.1 と h2c）。`nonregression` はプロキシ本体のバイナリにライブラリのコードが入っていないことを確認します。
cargo・gcc・curl・nc・python3 が必要です。単体テストは `cargo test --lib --features full,api`、`cargo test -p veil-capi`、Python は `tests/test_binding.py` です。
[テスト](testing.md)も参照してください。

## 制限事項

- `serve` / `run` は **プロセスにつき 1 回** だけ呼べます（設定フックと共有状態がプロセス全体のため）。2 回目はエラーになります。
- GraphQL のサブスクリプションは非対応です。
- Python のハンドラは GIL に制限されます。C のコールバックはスレッドセーフである必要があります。
- Swagger UI のページは `swagger-ui-dist` を CDN から読むため、`/docs` を表示するにはブラウザがインターネットに接続できる必要があります
  （`/openapi.json` はオフラインでも使えます）。
- ライブラリモードは `/etc/veil` の設定ファイルを使いません。カレントディレクトリの `config.toml`（または渡したパス）だけです。
