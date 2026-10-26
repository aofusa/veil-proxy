# API サーバライブラリ（Rust・C・Python）

[← ドキュメント目次](README.md) · [English](../library.md)

veil はリバースプロキシとして使えるだけでなく、自分のプログラムに **ライブラリ** として組み込んで API サーバを書くことができます。
普通の関数を HTTP ハンドラ（または TCP/UDP ハンドラ）として登録すると、io_uring・TLS・kTLS・HTTP/1.1・HTTP/2・HTTP/3・
ホットリロード・サンドボックスといった veil のネットワーク層がそのまま動きます。

3 つのフロントエンドが同じ実装を共有します。

| フロントエンド | 場所 | 主な用途 |
|----------------|------|----------|
| Rust ライブラリ | `veil` クレート（`api` feature 群） | 最速。マクロ・OpenAPI・tower・gRPC・任意の GraphQL ライブラリ |
| C API | [`crates/veil-capi`](../../../crates/veil-capi/README.md)（`libveil.so` / `libveil.a`、`veil.h`） | C・C++ など C FFI を持つ言語 |
| Python | [`crates/veil-python`](../../../crates/veil-python/README.md)（wheel、`import veil`） | FastAPI 風のデコレータ |

Rust では、別のクレート [`veil-proxy-kit`](library-kit.md) が Cookie・リダイレクト・静的ファイル・CORS・認証（JWT）・セッション・
GraphQL・依存性注入・バックグラウンドタスクを足します。

本番で使う設定（待ち受けのアドレス・アクセスログ・OpenAPI・圧縮・スレッド・ファイル記述子・停止・メトリクス）は
[library-production.md](library-production.md) にあります。

- [アーキテクチャ](#アーキテクチャ)
- [veil への依存](#veil-への依存)
- [ライブラリモードの設定](#ライブラリモードの設定)
- [ルーティングマクロ](#ルーティングマクロ)
- [Request・Response・エラー](#requestresponseエラー)
- [バリデーションと OpenAPI](#バリデーションと-openapi)
- [共有状態](#共有状態)
- [tower の層と tokio 前提ライブラリ](#tower-の層と-tokio-前提ライブラリ)
- [tokio 互換](#tokio-互換)
- [HTTP クライアント](#http-クライアント)
- [GraphQL](#graphql)
- [L4（TCP/UDP）ハンドラ](#l4tcpudpハンドラ)
- [TLS と HTTP/3](#tls-と-http3)
- [メモリアロケータ](#メモリアロケータ)
- [C API](#c-api)
- [Python](#python)
- [ライブラリ成果物のビルド](#ライブラリ成果物のビルド)
- [性能](#性能)
- [対応プラットフォーム](#対応プラットフォーム)
- [テスト](#テスト)
- [制限事項](#制限事項)

## アーキテクチャ

- ハンドラは、veil の通常のルーティング表に `type = "Handler"` のルートとして載ります。設定のロード時にライブラリが
  `[[route]]` を注入するので、`config.toml` に書いた `File`・`Proxy` などのルートと共存できます。
- **ハンドラは veil 独自のランタイム上で動きます。** `async fn` のハンドラは、接続を受け付けたワーカースレッド上で、
  その接続を処理しているのと同じ io_uring（または epoll/kqueue）のイベントループで poll されます。データプレーンに
  tokio・monoio はありません。tokio 前提のライブラリ（hyper・reqwest・sqlx など）は、veil の tokio 無しの tokio 互換
  レイヤーを通じて同じワーカーで動きます（[tokio 互換](#tokio-互換)）。
- ワーカー上でブロッキング処理をしてはいけません。Rust の **同期** `fn` ハンドラ・C のコールバック・Python のハンドラは、
  veil の `offload` を通じて別のハンドラ用スレッドプール（`[api] handler_threads`）で実行されるので、遅い DB 呼び出しが
  他の接続を止めることはありません。
- **リリースバイナリにライブラリのコードは入りません。** `handler`・`macro`・`openapi`・`tokio-compat`・
  `tokio-compat-bridge`・`l4-handler`・`grpc-handler`・`api-core`・`api` の各 feature は `default` にも `full*` にも含まれません。`cargo build --features full` で作るプロキシは
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
| `openapi-ui-embedded` | Swagger UI の資産を同梱し、`/docs` をオフラインで使えるようにする（約 0.8 MB。`api`・`api-core`・C のライブラリ・Python の wheel に含む。`openapi` を含意） |
| `tokio-compat` | tokio 無しの tokio 互換レイヤー（`veil::tokio`・`App::tokio_service`・`veil::api::tokio_compat`）。[tokio 互換](#tokio-互換) を参照 |
| `tokio-compat-bridge` | `tokio-compat` の代わりに、本物の tokio ランタイムを同梱して Future をその上で動かす（`tokio-compat` と排他） |
| `l4-handler` | `[[l4]]` リスナーの TCP/UDP を Rust 関数で処理（`l4-proxy` を含意） |
| `grpc-handler` | gRPC サーバ。メッセージは利用者が選ぶ codec が符号化する（`veil-build` 経由なら prost）（`veil::api::grpc`、[gRPC](#grpc) を参照。`handler`・`grpc` を含意） |
| `api-core` | `handler`・`macro`・`openapi`・`l4-handler`・`grpc-handler` と tower の `Layer`/`Service` 互換（`veil::api::tower`） |
| `api` | `api-core` + `tokio-compat`（ライブラリの既定の使い方） |

tokio の橋渡しを使うときは `features = ["full", "api-core", "tokio-compat-bridge"]` を選びます。`tokio-compat` と
`tokio-compat-bridge` を両方有効にするとコンパイルエラーになります。

`vds` と `acme` は別の feature で、`full*` に含まれ、プロキシ本体でもライブラリでも使えます（[VDS](vds.md)・[ACME](acme.md) を参照）。

### Cargo.toml

crates.io には **`veil-proxy`** の名前で公開しています（`veil` は別のプロジェクトの名前）。依存のキーは `veil` のままにします。
ライブラリ名が `veil` で、マクロが生成するコードも `::veil::` を参照するためです。feature は用途で選びます。`full` は
HTTP/3・kTLS・gRPC・WASM などプロキシ機能を一通り含み、コンテナ向けは `full-container` です。`default-features` は切ります
（アロケータの選び方は [メモリアロケータ](#メモリアロケータ) を参照）。API の文書は [docs.rs/veil-proxy](https://docs.rs/veil-proxy)。

```toml
[dependencies]
veil = { package = "veil-proxy", version = "0.8", default-features = false, features = ["full", "api"] }
# リポジトリやローカルのチェックアウトを使う場合:
# veil = { package = "veil-proxy", git = "https://github.com/aofusa/veil-proxy", default-features = false, features = ["full", "api"] }
# veil = { package = "veil-proxy", path = "../veil-proxy", default-features = false, features = ["full", "api"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

ほかの設定は要りません。Linux で HTTP/3 を使う場合も、`.cargo/config.toml` に何も書かなくて構いません。

- quiche と rustls は 1 つの `aws-lc-sys` を共有します。
- quiche は `aws-lc-sys` の Rust のバインディング経由で呼ぶので、`aws-lc-sys` のシンボルの接頭辞の有無にかかわらずビルドできます。
- 以前の版のために足した `AWS_LC_SYS_NO_PREFIX_<triple>` は、あっても害はなく、消して構いません。
- `aws-lc-sys` のビルドには C コンパイラ・CMake・libclang（bindgen）が要ります。

複数ストリームの STREAM フレームを 1 パケットへ詰める vendoring 版 quiche（F-172）は通常の依存（`veil-proxy-quiche`）なので、
`[patch]` は要りません。

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
listen_cleartext = "127.0.0.1:8080"  # 平文 HTTP/1.1 + h2c（prior knowledge）をプロトコル検出で判別。ループバックのみ。`listen` も [tls] も無い
enable_secure_redirect = false     # 平文で配信する（サーバの既定は HTTPS への 301）
http2_enabled = true

[logging]
level = "info"

[access_log]
enabled = true         # 要求ごとに 1 行を標準出力に出す（プロキシのバイナリの既定は false）
format = "text"

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

これに `[server] threads` = CPU 数が加わります。設定ファイルが無くても起動し、`127.0.0.1:8080` で平文の HTTP/1.1 と h2c を受け、
**TLS 証明書は不要** です（`[tls]` を省略できるのはこの平文専用構成だけです。[設定リファレンス](configuration.md#tls-なしの平文専用サーバ)を参照）。

既定のリスナーは **ループバックアドレスだけ** で受けるので、設定なしで起動したアプリケーションには他のホストから届きません。
コンテナ内やロードバランサの背後などで外部に公開するときはアドレスを明示します（`config.toml` の `listen_cleartext = "0.0.0.0:8080"`、
Rust の `.listen_cleartext("0.0.0.0:8080")`、C の `veil_config_listen_cleartext(cfg, "0.0.0.0:8080")`、
Python の `veil.run(listen_cleartext="0.0.0.0:8080")`、または `VEIL_OVERRIDE_1='server.listen_cleartext = "0.0.0.0:8080"'`）。

### アクセスログ

ライブラリモードでは、既定で要求ごとに 1 行のアクセスログを **標準出力** に `text` 形式で出します
（`timestamp=… type=access method=… path=… status=… duration_ms=…`。アプリケーションのログと同じ出力先です）。
行は専用のスレッドが書き、`flush_interval_ms`（既定 1000 ミリ秒）ごとにフラッシュするので、応答から最大 1 秒遅れて出ます。

- 止める: `config.toml` の `[access_log] enabled = false`、ビルダの `.set("access_log.enabled = false")?`、
  `VEIL_OVERRIDE_1='access_log.enabled = false'`（Python の `veil.run(...)` と C も `VEIL_OVERRIDE_<n>` を読みます）。
- JSON やファイルへ: `format = "json"`、`file_path = "/var/log/app/access.log"`、`fields = [...]`。キーはプロキシの
  [`[access_log]`](observability.md) と同じです。
- 要求ごとに処理が増えます。ログを出さないサーバと性能を比べるときは止めてください。

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
`config.toml` の内容だけです。`[server] listen` / `listen_cleartext`・`[[listener]]`・`[tls]`・`[[l4]]`・スレッド数の変更に再起動が要るのはプロキシと同じです。

### ビルダ

```rust
let config = Config::load()?            // または Config::new()（既定設定だけ）、Config::from_file("app.toml")?
    .builder()
    .listen_cleartext("0.0.0.0:8080")   // [server].listen_cleartext（外部に公開。既定は 127.0.0.1:8080）
    .threads(4)                         // [server].threads
    .log_level("warn")                  // [logging].level
    .openapi_title("My API")            // [api].*
    .max_body_size(1 << 20)
    .set("security.enable_seccomp = true")?     // 任意のキー。`--override` と同じ構文
    .build();
```

型付きのセッタは `listen_cleartext`（平文のアドレス）・`listen`（TLS のアドレス）・`allow_http`・`allow_h2c`・`secure_redirect`・`threads`・`http2_enabled`・`log_level`・`openapi_enabled`・
`openapi_path`・`openapi_json_path`・`openapi_docs_ui`・`openapi_docs_cdn_url`・`openapi_title`・`openapi_version`・`max_body_size`・`handler_threads`・
`grpc_max_message_size`・`grpc_response_compression`・`grpc_response_compression_min_size`・`tls(cert, key)`・`http3(listen)` です。それ以外のキーは `set("key = value")` と `set_value(path, toml_value)` で上書きできます。
`Config::to_toml_string()` は合成結果（環境変数の上書きとルート注入の前）を返すので、デバッグに使えます。

### `[api]` キー（ライブラリ専用）

`[api]` はライブラリモードでだけ読まれます。プロキシ本体のバイナリは知りません。

| キー | 既定値 | 説明 |
|------|--------|------|
| `openapi_enabled` | `true` | `/openapi.json` と Swagger UI を公開する |
| `openapi_path` | `"/docs"` | Swagger UI のパス |
| `openapi_json_path` | `"/openapi.json"` | OpenAPI ドキュメントのパス |
| `openapi_docs_ui` | `"embedded"` | `/docs` の Swagger UI の配り方: `"embedded"`（同梱の資産。外へ通信しない）・`"cdn"`（ブラウザが `openapi_docs_cdn_url` から `swagger-ui-dist` を読む）・`"off"`（`/openapi.json` だけ）。`openapi-ui-embedded` feature が無いビルドでは既定が `"cdn"` で、`"embedded"` は設定のエラー |
| `openapi_docs_cdn_url` | `"https://cdn.jsdelivr.net/npm/swagger-ui-dist@5.34.0"` | `openapi_docs_ui = "cdn"` のときの `swagger-ui-dist` の URL |
| `openapi_title` | `"veil API"` | `info.title` |
| `openapi_version` | `"0.1.0"` | `info.version`（API 自体のバージョン） |
| `openapi_description` | なし | `info.description` |
| `max_body_size` | `10485760` | ハンドラへ渡すリクエスト本文の上限（バイト） |
| `handler_threads` | `0` | 同期 Rust 関数・C コールバック・Python ハンドラを実行するハンドラ用プールのスレッド数（`0` = 組み込みの既定） |
| `tokio_threads` | `2` | 同梱の tokio ランタイムのワーカー数（`tokio-compat-bridge` のみ） |
| `grpc_max_message_size` | `4194304` | 受信する gRPC メッセージ 1 つの上限（`grpc-handler`） |
| `grpc_max_send_message_size` | `0` | 送信する gRPC メッセージ 1 つの上限（0 = 無制限） |
| `grpc_response_compression` | `"none"` | gRPC の応答メッセージの圧縮: `"none"`・`"gzip"`・`"deflate"`（クライアントが `grpc-accept-encoding` で受けるときだけ。`compression` feature が必要） |
| `grpc_response_compression_min_size` | `1024` | この大きさ（バイト）未満の gRPC の応答メッセージは圧縮しない |
| `grpc_web_cors_origins` | `[]` | ブラウザから gRPC-Web で呼んでよい Origin（`["*"]` で全部）。空なら CORS のヘッダを付けず、プリフライトは 403 |
| `upgrade_idle_timeout_secs` | `0` | アップグレードした HTTP/1.1 の接続で `read` が待てる秒数（0 = 無制限） |
| `websocket_max_message_size` | `16777216` | 組み立て後の WebSocket のメッセージの上限（超えたら 1009 で閉じる） |
| `websocket_max_frame_size` | `16777216` | WebSocket の 1 フレームの上限 |
| `websocket_ping_interval_secs` | `0` | サーバから WebSocket に Ping を送る間隔（0 = 送らない） |
| `websocket_idle_timeout_secs` | `0` | フレームを受けない WebSocket を 1001 で閉じるまでの秒数（0 = 無制限） |
| `websocket_compression` | `false` | クライアントが申し出たら permessage-deflate（RFC 7692）を交渉する（`compression` feature が必要） |
| `openapi_websocket` | `true` | WebSocket のルートを OpenAPI に載せる |
| `openapi_grpc` | `true` | gRPC のメソッドを OpenAPI に載せる（[OpenAPI の gRPC](#openapi-の-grpc)） |
| `openapi_grpc_builtin` | `false` | 組み込みの `grpc.health.v1`・`grpc.reflection.*` のメソッドも載せる |
| `python_event_loops` | `1` | Python: `async def` のハンドラと WebSocket を動かす常駐のイベントループの数 |
| `python_event_loop` | `"auto"` | Python: `"auto"`（uvloop が入っていれば使う）・`"asyncio"`・`"uvloop"` |
| `python_event_loop_queue` | `65536` | Python: 1 つのループで待てる呼び出しの数（一杯なら 503） |
| `python_async_mode` | `"loop"` | Python: `async def` の実行先。`"loop"`（常駐のループ）か `"pool"`（0.10 より前と同じ） |
| `multipart_max_parts` | `100` | `multipart/form-data` の本文のパート数の上限（超えたら 413） |
| `multipart_max_field_size` | `1048576` | ファイルでないパートの大きさの上限 |
| `multipart_max_file_size` | `0` | ファイルのパートの大きさの上限（`0` = `max_body_size`） |
| `multipart_max_total_size` | `0` | `multipart/form-data` の本文全体の上限。ストリーミングのルートにも効く（`0` = `max_body_size`。`multipart_max_file_size` の方が大きければそちら） |
| `multipart_max_header_size` | `8192` | 1 パートのヘッダの大きさの上限 |
| `multipart_spool_threshold` | `1048576` | Python の `UploadFile`・C の `VEIL_ROUTE_FORM`: これより大きいファイルはメモリでなく一時ファイルに書く |
| `upload_tmp_dir` | OS の一時ディレクトリ | アップロードの名前の無い一時ファイルを置くディレクトリ（Landlock・unveil の書き込み許可に足す） |
| `compression` | `false` | クライアントが受け付けるときハンドラの応答を br・gzip で圧縮する。自前の `[route.compression]` の無いハンドラのルートに `{ preferred_encodings = ["br", "gzip"], min_size = 256, compressible_types = ["application/json", "text/", "application/javascript", "image/svg+xml"] }` を入れる（`compression` feature が必要。[応答の圧縮](#応答の圧縮)） |

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
| `compress = false` | このルートの応答を圧縮しない（[応答の圧縮](#応答の圧縮)） |

### 引数の束縛（FastAPI 方式）

引数は型と名前で束縛されます。

| 引数 | 取得元 |
|------|--------|
| `&Request` / `Request` | リクエスト全体 |
| `Json<T>` | JSON 本文 |
| `Query<T>` | クエリ文字列全体を `T` にデシリアライズ |
| `Form<T>` | `application/x-www-form-urlencoded` の本文（`multipart/form-data` の本文のファイル以外のフィールドも） |
| `MultipartForm<T>`・`UploadFile`・`Vec<UploadFile>`・`Option<UploadFile>`・`Multipart` | `multipart/form-data` の本文（[フォームとファイルのアップロード](#フォームとファイルのアップロード)） |
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

### ストリーミング本文とトレーラ

`Response::stream(status, stream)` は本文をできた順に送ります。ストリームは `BodyChunk::Data(bytes)` を返し、
最後に `BodyChunk::Trailers(fields)` を返せます。`BodyChunk::Abort(reason)` は途中で打ち切ります。HTTP/1.1 は chunked
（トレーラは最後のチャンクの後、打ち切りは接続を閉じる）、HTTP/2 は DATA フレームと末尾の HEADERS（打ち切りは RST_STREAM）、
HTTP/3 も同様（打ち切りはストリームのリセット）です。すでに用意できている断片は 1 フレーム 16KiB までまとめて送ります。
`HEAD` ではストリームを poll せずに捨てます。

届いた順に要求本文を受け取りたいルートは `streaming = true`（`#[veil::route(..., streaming = true)]` または
`App::route_fn_streaming`）にして、`req.take_body_stream()`（`Stream<Item = Bytes>`）で読みます。HTTP/2 ではヘッダが
届いた時点でハンドラが動き、要求と応答が同時に流れます（全二重）。HTTP/1.1 と HTTP/3 では本文を読み終えてから 1 断片として
渡します。HTTP/2 のストリーミング受信のルートには `max_body_size` が掛からないので、読みながら制限してください。
どちらの向きも tower の層をそのまま通ります。

本文が途中で切れたとき（クライアントの切断・ストリームのリセット・時間切れ、HTTP/1.1 では `Content-Length` に届かない）も、
本文のストリームは終わり（`None`）になります。本文を完全なものとして扱う前（アップロードしたファイルを確定する前など）に、
終わりの後で `RequestBodyStream::is_aborted()` を確かめてください。`Multipart` と gRPC の要求のストリームは、切れた本文を
自分でエラーにします。

```rust
#[veil::route(path = "/upper", method = "POST", streaming = true)]
async fn upper(req: &Request) -> Response {
    use futures::StreamExt;
    let body = req.take_body_stream()
        .map(|c| BodyChunk::Data(bytes::Bytes::from(c.to_ascii_uppercase())));
    Response::stream(200, body).with_header("content-type", "text/plain")
}
```

### プロトコルアップグレード（HTTP/1.1）

ハンドラは `101 Switching Protocols` で接続を引き取れます。`Request::upgrade_protocol()` は要求がアップグレードを求めているとき
（`Connection: upgrade` と `Upgrade` がある。HTTP/2・HTTP/3 では常に `None`）に `Upgrade` の値を返し、
`Response::switching_protocols(protocol, f)` は 101 を書いてから `veil::api::upgrade::Upgraded` の接続で `f` を呼びます。

```rust
use veil::api::upgrade::Upgraded;

#[veil::route(path = "/echo", method = "GET")]
async fn echo(req: Request) -> Response {
    if req.upgrade_protocol() != Some(&b"x-echo"[..]) {
        return Response::error(426, "Upgrade Required");
    }
    Response::switching_protocols("x-echo", |mut up: Upgraded| async move {
        let mut buf = bytes::BytesMut::with_capacity(4096);
        loop {
            let (res, b) = up.read(buf).await;          // veil のランタイムと同じ所有権渡しの読み取り
            buf = b;
            match res {
                Ok(n) if n > 0 => { if up.write_all(buf.split().freeze()).await.is_err() { break } }
                _ => break,
            }
        }
    })
}
```

- `Upgraded` は平文・rustls・kTLS で同じ型です: `read(BytesMut)`、`readable()`（途中で捨ててよい。`read` は io_uring では
  捨てると読んだデータを失うので、`select` には `readable` を使います）、`write_all(Bytes)`、`write_vectored_all(&[Bytes])`
  （ヘッダと本文を 1 回で書く）、`shutdown()`、`client_ip()`、`shutdown_signal()`（graceful shutdown の開始で解決。
  `[server] graceful_shutdown_timeout_secs` を過ぎても開いている接続は drop されます）。
- 要求ヘッダの直後にクライアントが送ったバイト列（同じパケットでも）は最初の `read` で返ります。
- 101 には `Connection: Upgrade`・`Upgrade: <protocol>` と `with_header` で足したヘッダが付きます。`Content-Length` は付かず、
  接続は HTTP に戻りません。アクセスログには 101 が 1 行残ります。
- 要求がアップグレードの要求でない（`Connection: upgrade` / `Upgrade` が無い）・HTTP/1.0・本文付き・HTTP/2 や HTTP/3 で届いた
  （RFC 8441 / RFC 9220 の拡張 CONNECT は非対応）ときは、veil が 400 に置き換えます。`h2c` は予約済みです（500 と警告）。
- アップグレードした接続も `[security] max_concurrent_connections` に数えます。`[api] upgrade_idle_timeout_secs`
  （既定 0 = 無し）を設定すると、それより長く待った `read` は `TimedOut` で失敗します。
- `tokio-compat` では `up.into_tokio_io()` が `veil::tokio::io::{AsyncRead, AsyncWrite}` を実装したものを返し、tokio 前提の
  プロトコル実装を載せられます（同じワーカーのタスクを経由するので 1 回余分に写します）。
- C: `veil_response_upgrade(resp, "x-echo", &callbacks, ud)`。L4 の TCP と同じコールバック（`on_connect` / `on_data` /
  `on_close`、`veil_l4_write`）で受けます。Rust でも `Upgraded::serve_blocking`（`l4-handler`）で同じ形にできます。

例: [examples/api/rust-websocket/](../../../examples/api/rust-websocket/)。

### WebSocket

`veil::api::ws` でハンドラが WebSocket（RFC 6455）を終端します。`WebSocketUpgrade` はハンドシェイク（HTTP/1.1・`GET`・
`Upgrade: websocket`・`Connection: upgrade`・`Sec-WebSocket-Version: 13`・16 バイトの `Sec-WebSocket-Key`）を検査する抽出子で、
正しくなければ 400、版違いは 426 と `Sec-WebSocket-Version: 13` を返します。`on_upgrade` は `Sec-WebSocket-Accept` 付きの
101 を返し、接続を渡して関数を呼びます。

```rust
use veil::api::ws::{Message, WebSocket, WebSocketUpgrade};

#[veil::route(path = "/ws", method = "GET", ws_protocols = ["chat.v1"])]
async fn ws(upgrade: WebSocketUpgrade) -> Response {
    upgrade
        .protocols(["chat.v1"])          // 任意: Sec-WebSocket-Protocol の選択
        .max_message_size(1 << 20)       // 任意: 既定は [api] websocket_max_message_size
        .on_upgrade(|mut ws: WebSocket| async move {
            while let Some(Ok(msg)) = ws.recv().await {
                match msg {
                    Message::Text(_) | Message::Binary(_) => { if ws.send(msg).await.is_err() { break } }
                    Message::Close(_) => break,
                    _ => {}              // Ping には自動で Pong を返す
                }
            }
        })
}
```

- `Message` は `Text(Utf8Bytes) | Binary(Bytes) | Ping(Bytes) | Pong(Bytes) | Close(Option<CloseFrame>)`。`Utf8Bytes` は
  `Bytes` の包み（検査は 1 回、複製は参照カウントだけ）です。
- `WebSocket`: `recv()`・`send(msg)`・`send_batch(&[msg])`（1 回の書き込み）・`flush()`・`ping(bytes)`・`close(code, reason)`・
  `protocol()`・`client_ip()`・`split()`。`split` は `WsReceiver`（接続を進める側。`recv` を呼び続ける）と `WsSender`
  （`Clone + Send`。どのタスク・スレッドからも待たずに積めるキュー。ブロードキャスト等に。積める量は 16 MiB まで）を返します。
- プロトコルの処理は veil が行います: 断片化したメッセージの組み立て（途中の制御フレームも可）、断片をまたぐ UTF-8 の検査
  （1007）、マスクの無いクライアントのフレーム・予約ビット・未知の opcode・不正な制御フレームは 1002、上限超えは 1009、
  受けた Close には同じコードを返します。関数が戻ると未送信を送り、閉じる手順を終えます（1000）。graceful shutdown では
  Close 1001 を送ります（`recv` は `Message::Close(Some(1001))` を返します）。
- フレームはプールの受信バッファの中で復号します（マスクは 8 バイトずつ外し、本文は `Bytes` の切り出し）。待っている接続は
  バッファを持ちません。サーバのフレームはヘッダと本文を 1 回の `writev` で送り、まとめて届いたメッセージへの返信は次の
  読み取りの前にまとめて送ります。
- `auto_pong(false)` で Ping に自動で応えず渡します。`[api] websocket_ping_interval_secs` で Ping を送り、
  `websocket_idle_timeout_secs` で黙った接続を閉じます。
- `origin(&["https://app.example"])` で許す `Origin` を絞れます（違えば 403）。既定は検査しません。ブラウザはクロスサイトの
  WebSocket のハンドシェイクにも Cookie を付けるので、Cookie で認証するなら絞ってください（クロスサイト WebSocket ハイジャック）。
- permessage-deflate（RFC 7692）は `[api] websocket_compression = true` のときだけ交渉します（既定は off。展開後の大きさは
  `websocket_max_message_size` で制限）。
- WebSocket は HTTP/1.1 だけです。HTTP/2・HTTP/3 のハンドシェイクは 400 になります（RFC 8441 / RFC 9220 は非対応）。
- メトリクス（`metrics` feature）: `veil_proxy_websocket_connections`（開いている接続数）と
  `veil_proxy_websocket_messages_total{direction="received"|"sent"}`。
- アップグレードした接続と同じく `[security] max_concurrent_connections` に数えます。

**OpenAPI.** WebSocket のルートは（`[api] openapi_websocket = false` でなければ）`GET` の操作として載ります。応答は 101・400・426、
ヘッダの引数 `Sec-WebSocket-Protocol`（`ws_protocols` から）、拡張 `x-veil-websocket` に `subprotocols`・`maxMessageSize`、
`ws_receive = T` / `ws_send = T`（`derive(veil::Schema)` の型）を書けばメッセージのスキーマ `receive`（クライアント → サーバ）・
`send`（サーバ → クライアント）と `messageFormat: "text/json"` が入ります。Swagger UI ではこれらの操作の「Try it out」を出しません。
`upgrade.messages::<In, Out>().validate(true)` で受けたテキストを `In` として検査し、合わなければ 1007 で閉じます。

**tokio のライブラリ.** `Upgraded::into_tokio_io()`（`tokio-compat`）を使えば、素の `Response::switching_protocols` の上で
tokio-tungstenite・fastwebsockets が `veil::tokio` 上で動きます。axum の `WebSocketUpgrade` は hyper の `OnUpgrade` が要るので
tower の橋渡しでは動きません。`veil::api::ws` か tokio-tungstenite を使ってください。

**C.** ルートのコールバックの中で `veil_ws_accept(req, resp, &callbacks, ud)` を呼ぶと応答が 101 になります（正しくなければ
400 / 426 で `VEIL_ERR_STATE`）。`on_open`・`on_message(ws, opcode, data, len, ud)`・`on_close(ws, code, ud)` はハンドラ用プールで
接続ごとに 1 つずつ順に呼ばれます。`veil_ws_send(ws, VEIL_WS_TEXT | VEIL_WS_BINARY, data, len)`（コピーする。どのスレッドからでも）と
`veil_ws_close(ws, code)` はフレームを積みます。OpenAPI には `x-veil-websocket` を含む操作の JSON を `veil_register_route_ex` に渡して載せます。

例: [examples/api/rust-websocket/](../../../examples/api/rust-websocket/)（`/ws` のエコー、`split` を使う `/chat` のブロードキャスト、
JSON の `/typed`）。C は [examples/api/c-features/](../../../examples/api/c-features/) の `/ws`。

## フォームとファイルのアップロード

veil は `multipart/form-data`（RFC 7578）を依存を足さずに自前で解析します。溜めた本文のパートは本文の切り出し（複製しない）で、
ストリーミングの本文は届いた順に解析します。

```rust
use veil::api::prelude::*;

#[derive(Deserialize, veil::Schema)]
struct Profile { name: String, #[validate(range(min = 0, max = 150))] age: u32 }

// 本文を溜める（[api] max_body_size まで）。ファイル以外のフィールドは T へ、ファイルは引数名で対応づける
#[veil::route(path = "/profile", method = "POST")]
async fn profile(form: MultipartForm<Profile>, avatar: UploadFile, banner: Option<UploadFile>) -> Json<Value> {
    Json(json!({"name": form.name, "avatar": avatar.file_name(), "size": avatar.len()}))
}

#[veil::route(path = "/docs", method = "POST")]
async fn docs(docs: Vec<UploadFile>) -> Json<Vec<usize>> { Json(docs.iter().map(|d| d.len()).collect()) }

// ストリーミング: ルートは自動で `streaming = true` になり、ファイルはメモリに溜まらない
#[veil::route(path = "/upload", method = "POST")]
async fn upload(mut form: Multipart) -> Result<Json<Value>, ApiError> {
    while let Some(mut part) = form.next_part().await? {
        if part.file_name().is_some() {
            let tmp = part.save_to_temp().await?;   // [api] upload_tmp_dir の名前の無いファイル。part.save_to(path) も可
            // tmp.file()・tmp.len()・tmp.into_file()
        } else {
            let value = part.text().await?;          // part.chunk().await・part.bytes().await も可
        }
    }
    Ok(Json(json!({"ok": true})))
}
```

- `MultipartForm<T>` はファイル以外のフィールドを `Form<T>` と同じ規則で `T` にします（`T` が `Schema` を derive していれば検証も）。
  `Form<T>` も multipart の本文を受け、ファイルは無視します。ファイルが無ければ `loc = ["body", 名前]` の 422、本文が壊れていれば 400、
  `[api] multipart_*` の上限を超えたら 413（`{"detail": "..."}`）です。
- `Multipart` は溜める抽出子と併用できません。ストリーミングのルートは `max_body_size` の制限を受けず（multipart の本文にはパートごとの上限と本文全体の上限 `multipart_max_total_size` が効く）、
  HTTP/1.1（`Content-Length` のある本文。chunked は溜める）・HTTP/2・HTTP/3 で同じです。名前の無いファイル（Linux は `O_TMPFILE`、
  他は作ってすぐ消す、Windows は閉じたら消す）への書き込みは offload のプールで行い、閉じると消えます。
- OpenAPI は本文を `multipart/form-data`、ファイルを `{"type": "string", "format": "binary"}` で書くので、Swagger UI でファイルを選べます。
- **Python:** `name: str = veil.Form()`・`avatar: veil.UploadFile = veil.File()`・`files: list[veil.UploadFile]`・
  `data: bytes = veil.File()`（FastAPI と同じ名前。`Annotated[str, veil.Form()]` も可。urlencoded のフォームも同じ `Form()`）。
  本文はハンドラの前に Rust で解析し、テキストと小さなファイルはメモリ、`multipart_spool_threshold` を超えるファイルは一時ファイルとして
  Python に渡すので、大きなアップロードでプロセスが膨らみません。`UploadFile` は `filename`・`content_type`・`headers`・`size`・`file` と
  `async` の `read`・`write`・`seek`・`close`（と `read_bytes()`）を持ちます。
- **C:** `veil_multipart_next(req, &part)` がパートを 1 つずつ返します（`name`・`filename`・`content_type` と、`data`/`data_len` か一時ファイルの
  `fd`）。ルートのフラグ `VEIL_ROUTE_FORM` を付けるとコールバックの前に本文を読み、大きなファイルは一時ファイルに書きます（コールバックから
  戻ると閉じる）。付けなければ溜めた本文を解析します。

例: [examples/api/rust-showcase/](../../../examples/api/rust-showcase/) の `/profile`・`/files-upload`・`/upload-stream`、
[examples/api/python-features/](../../../examples/api/python-features/) と [examples/api/c-features/](../../../examples/api/c-features/) の `/upload`。

## 応答の圧縮

ハンドラの応答は、プロキシの応答と同じ `[route.compression]` の表（[compression-cache.md](compression-cache.md)）で圧縮する。
HTTP/1.1・HTTP/2・HTTP/3、バッファの本文・ストリーミングの本文のどちらも対象（`compression` feature が必要。`full` には入り、
`api` には入らない）。`[api] compression = true`（ビルダの `.compression(true)`、Python の `veil.run(compression=True)`、C の
`veil_config_set(cfg, "api.compression", "true")`）にすると、自前の `[route.compression]` の無いハンドラのルートに JSON の API 向けの
既定（br、次に gzip、256 バイト以上、`application/json`・`text/*`・`application/javascript`・`image/svg+xml`）が入る。

```toml
[api]
compression = true            # ハンドラのルート全体の既定

[[route]]                     # ルートごとにも書ける（ルートの表が [api] compression より優先）
[route.conditions]
path = "/reports"
[route.action]
type = "Handler"
handler = "reports"
[route.compression]
enabled = true
preferred_encodings = ["br", "gzip"]
brotli_level = 5
```

- 方式は `Accept-Encoding` の q 値で選び、同じ q 値なら `preferred_encodings` の順（`q=0`・`*` も扱う）。`Accept-Encoding` が無い、
  または受け付ける方式が無いときはそのまま送る。
- 圧縮した応答には `Content-Encoding`・`Vary: Accept-Encoding` を付け、`Content-Length` を付け直す（強い `ETag` は弱い ETag にする）。
  `HEAD` には本文無しで `GET` と同じヘッダを付ける。
- 圧縮しないもの: `min_size` 未満の本文、`compressible_types` に無い（または `skip_types` にある）型、既に `Content-Encoding` がある応答、
  `Cache-Control: no-transform`、`Content-Range`、状態 1xx・204・206・304、プロトコルのアップグレード・WebSocket・gRPC。
  ストリーミングの本文（`Response::stream`・Python のジェネレータ・C の `veil_response_stream`）は断片ごとに圧縮して毎回 flush する。
  `text/event-stream` は `compressible_types` に書いたときだけ圧縮する。
- 止め方: ルートは `#[veil::route(..., compress = false)]`・`App::route_compression(false)`、応答ごとは `Response::no_compression()`。
  Python は `@app.get(..., compress=False)`・`veil.Response(..., compress=False)`（`JSONResponse`・`StreamingResponse` も）、
  C は `VEIL_ROUTE_NO_COMPRESS`・`veil_response_no_compression(resp)`。
- 圧縮しないルート・応答は確保を増やさない。エンコーダの作業領域はスレッドごとに使い回す。`[http3] compression*` のキーは
  プロキシの HTTP/3 の応答だけに効く。

例: [examples/api/rust-showcase/](../../../examples/api/rust-showcase/) の `/dataset/{count}`・`/dataset-raw/{count}`・`/stream-big`、
[examples/api/python-features/](../../../examples/api/python-features/) の `/data`・`/data-raw`・`/data-nc`・`/lines`、
[examples/api/c-features/](../../../examples/api/c-features/) の `/json`・`/json-raw`・`/json-nc`・`/lines`。

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

Swagger UI は既定で **同梱** です。ライブラリは `swagger-ui-dist`（版を固定し、gzip と brotli で事前圧縮。
`third_party/swagger-ui-dist/README.veil.md`）を持ち、`<openapi_path>/assets/<版>/` から
`Cache-Control: public, max-age=31536000, immutable`・`ETag`・`Accept-Encoding` に応じた `Content-Encoding: br`/`gzip`
で配ります（どちらも受けないクライアントには最初の 1 回だけ解凍したものを返します）。ページは他のオリジンへ 1 本も
要求しないので、閉じたネットワークでも `/docs` が動きます。ページには
`Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:;
font-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'self'` を付けます
（インラインのスクリプトは無く、初期化も同じオリジンのファイル）。オンラインの検証器のバッジは止めています。
`openapi_docs_ui = "cdn"` で以前の CDN のページ（同梱と同じ版に固定）、`"off"` で JSON だけになります。プロキシ本体の
バイナリ（`full` など `full*`）にはこれらの資産は入りません。

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

リクエストとレスポンスは `http::Request<Full<Bytes>>` / `http::Response` でやり取りします。

```rust
App::new()
    // 全ルートに tower の Layer を掛ける。veil のワーカー上で実行される
    .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(name, value))
    // /admin 以下のルートだけ（セグメント境界の前方一致）
    .layer_on("/admin", RequireToken)
    // 既存の tower Service（axum の Router など）をマウントする
    .tokio_service(&["GET", "POST"], "/legacy/{*rest}", axum_router)
    .run()?;
```

- `layer` / `layer_on`（`api-core`）は veil のイベントループの中でハンドラを包みます。
- `tokio_service`（`tokio-compat` または `tokio-compat-bridge`）は tower のサービスをマウントします。`tokio-compat` では
  ほかのハンドラと同じくリクエストを受け付けたワーカーで動きます。`tokio-compat-bridge` では同梱の tokio ランタイムで動き、
  結果は `offload` と同じ「結果スロット + 通知 fd」でワーカーへ返ります。
- 任意のハンドラの中で tokio 前提の Future を動かすには `veil::api::tokio_compat::run(fut)` を使います。`tokio-compat` では
  `fut.await` と同じです（2 つの feature の間でコードを変えずに済むように残しています）。`tokio-compat-bridge` では同梱の
  ランタイム（遅延起動、ワーカー数は `[api] tokio_threads`、既定 2）で動きます。

  ```rust
  let body = veil::api::tokio_compat::run(async move {
      reqwest::get("https://example.com").await?.text().await
  }).await?;
  ```

  `run` はタスクが panic すると panic します（→ 500）。`try_run` は `Result` を返し、`spawn` は切り離したタスクを起動し、
  `handle()` はランタイムのハンドルを返します。

## tokio 互換

`tokio-compat`（`api` に含む）は、tokio の公開 API を **tokio に依存せず** veil 独自のランタイムの上に実装します。
tokio 前提のクレート（hyper・reqwest・sqlx・tower-http など）が veil の io_uring（または epoll/kqueue）のワーカーで
そのまま動きます。データプレーンと veil のランタイムは何も変わりません（互換レイヤーは既存のランタイム API だけを使い、
プロキシ本体のバイナリには入りません）。

### 使い方

依存ツリー全体の `tokio` クレートを veil のシム（パッケージ名 `tokio`、版 1.99.0。`veil::tokio` を再エクスポート）に
置き換えます。

```toml
[dependencies]
veil = { package = "veil-proxy", version = "0.8", default-features = false, features = ["full", "api"] }
tokio = { version = "1", features = ["full"] }        # シムに解決される
reqwest = "0.12"
sqlx = { version = "0.8", features = ["runtime-tokio", "postgres"] }

[patch.crates-io]
# シムはパッケージ名が tokio なので crates.io には無く、リポジトリから取る。veil も同じタグから取り、シムと自分のコードが
# 同じ 1 つの veil を使うようにする（2 つになるとランタイムも 2 つになる）。
tokio = { git = "https://github.com/aofusa/veil-proxy", tag = "v0.9.2", package = "tokio" }   # crates/veil-tokio/tokio-shim
veil-proxy = { git = "https://github.com/aofusa/veil-proxy", tag = "v0.9.2" }
```

`veil::tokio` を直接使うコード（`use veil::tokio as tokio;`）はパッチ無しで動きます。`#[tokio::main]` / `#[tokio::test]` は
veil のランタイム（current-thread または multi-thread）を起動します。

### tokio との違い

- **work stealing はありません。** タスクは spawn したスレッドで動きます。ランタイム外のスレッドから `Handle` 経由で
  spawn するとランタイムのスレッドへ転送します。I/O オブジェクトは作ったスレッドに結び付きます。そのため、1 つのリスナーで
  受理して接続ごとにタスクを spawn するサーバ（`axum::serve`・hyper の受理ループ）は、マルチスレッドのランタイムでも
  1 本のスレッドで動きます。スレッドごとに `SO_REUSEPORT` のリスナーを持たせるか、veil の `App` に載せてください（[性能](#性能)）。
- **スレッドをまたぐ起床**（別スレッドの `Sender`・`spawn_blocking`・別スレッドへ渡した `Waker`）は、通知 fd を通じて
  所有スレッドへ届けます。veil 自身の Waker を別スレッドから呼ぶことはありません。
- `spawn_blocking`・`block_in_place`・`fs` は veil のハンドラ用スレッドプール（`[api] handler_threads`）で動きます。
- 対応: `task`（spawn・`JoinHandle`・`JoinSet`・`LocalSet`・`task_local!`・`yield_now`・協調スケジューリングの予算・
  `unconstrained`）、`time`（`pause`/`advance`/`start_paused` を含む）、`io`（トレイト・ユーティリティ・`split`・`duplex`・
  `copy_bidirectional`・バッファ付き I/O）、`net`（TCP・UDP・Unix のストリーム/データグラム/リスナー・`AsyncFd`・
  `lookup_host`）、`sync`（mpsc・oneshot・watch・broadcast・`Mutex`・`RwLock`・`Semaphore`・`Notify`・`Barrier`・
  `OnceCell`・`SetOnce`）、`signal`、`fs`、`select!`/`join!`/`try_join!`/`pin!`、`runtime::{Runtime, Builder, Handle}`。
- 非対応: `process`、ランタイムのメトリクス、タスクダンプ・トレースのフック、io_uring を使う `fs`、Windows の名前付きパイプ。
  **Windows**（F-204）の `net` は TCP・UDP・`lookup_host`（Unix ドメインソケットと `AsyncFd` は tokio と同じく unix だけ。
  `net::windows::named_pipe` は参照するクレートがビルドできるように型だけあり、パイプは開けません）、`signal` は `ctrl_c` と
  `signal::windows::{ctrl_c, ctrl_break, ctrl_close, ctrl_logoff, ctrl_shutdown}`（`SetConsoleCtrlHandler`。受け手のいない
  イベントは次のハンドラ、たとえば veil 本体の Ctrl-C での停止へ回します）。
  これらや tokio の内部を必要とするクレートは `tokio-compat-bridge` を使ってください。

### 互換性

tokio 自身のテスト（tokio 1.53.2 の `tokio/tests/*.rs`）をシムに対して `tools/tokio-compat/run_tokio_tests.py` で実行した
結果です。ランタイムの内部を確かめるテスト（`rt_*`・メトリクス・トレース・work stealing・io_uring の fs）は対象外で、
一覧はスクリプトにあります。

| 分類 | 合格 / 実行 | 合格率 |
|---|---|---|
| time | 53 / 53 | 100.0% |
| io | 126 / 133 | 94.7% |
| net | 113 / 113 | 100.0% |
| sync | 371 / 376 | 98.7% |
| task | 70 / 98 | 71.4% |
| macros | 69 / 70 | 98.6% |
| fs | 42 / 47 | 89.4% |
| signal | 9 / 11 | 81.8% |
| **合計** | **853 / 901** | **94.7%** |

`task` の残りの失敗は、タスク API を通じて tokio のランタイム内部を確かめるもの（`Handle` をまたぐタスク ID、
`JoinSet` が伝える panic、ランタイム外から動かす `LocalSet` など）が中心です。`io` の未達は `AsyncFd` の
`Interest::ERROR`/`PRIORITY` の準備待ちと、ランタイムを drop した時点で待っていた Future の起床です。ファイルごとの結果は
スクリプトを実行すると `docs/artifacts/tokio_compat_results.md` に出力されます。

実ライブラリは `tools/tokio-compat/ecosystem` で確認しています（Linux の io_uring と epoll、macOS・FreeBSD・OpenBSD・NetBSD の
kqueue のリアクタ。[対応プラットフォーム](#対応プラットフォーム)）: hyper のサーバ（HTTP/1.1 + h2c）と
クライアント、reqwest（タイムアウトを含む）、tower-http の `TimeoutLayer`、axum 0.8（`axum::serve`・抽出子・State・
graceful shutdown）、sqlx（SQLite・PostgreSQL）。

## HTTP クライアント

`veil::api::client`（feature `handler`）はワーカーのランタイムの上で動く非同期の HTTP クライアントで、ハンドラやタスクから
tokio 無しで他のサービスを呼べます:

```rust
use veil::api::client::{Client, Http2};

let client = Client::builder().http2(Http2::Auto).timeout(Duration::from_secs(5)).build()?;
let res = client.get("https://api.example.com/v1/items").bearer_auth(token).send().await?;
let items: Vec<Item> = res.error_for_status()?.json()?;
```

プロキシの上流の部品を使います: 名前解決は offload のプール、TLS は上流の TLS クライアント（信頼アンカーは `[tls_client] ca`、
または `ClientBuilder::ca`。mTLS は `client_cert`）、多重化した HTTP/2 の接続と、ワーカーごとの HTTP/1.1 のプール。
`Http2::Auto`（既定）は TLS では ALPN で h2 か HTTP/1.1 を選び、平文では HTTP/1.1。`Http2::On` は h2 と prior knowledge の h2c、
`Http2::Off` は HTTP/1.1。`http://unix:/path.sock:/x` で Unix ソケットへ接続します。応答の本文は全部読みます（`max_response_size`、
既定 64 MiB）。エラーには `kind()` があり、502・504 の `Response` に変換できます。詳しくは [library-kit.md](library-kit.md#http-クライアント)。

## GraphQL

veil は GraphQL のライブラリに依存しません。利用者が好きな実装（async-graphql・juniper 等）を選び、普通のハンドラとして
`App::route_fn` で載せます（1 つのパスに `GET` と `POST`）。ハンドラは `Request::body()` か `Request::query()` から問い合わせを
読み、結果を JSON で返します。サブスクリプションは `Response::stream` で Server-Sent Events として送れます。

[`examples/api/rust-graphql`](../../../examples/api/rust-graphql/) が async-graphql でこれを行う例で、コピーして使える
1 ファイル（`src/graphql.rs`、約 100 行）にまとめています:

```rust
mod graphql;   // examples/api/rust-graphql/src/graphql.rs をコピー

let schema = Schema::build(QueryRoot, MutationRoot, SubscriptionRoot).finish();
graphql::graphql(App::new(), "/graphql", schema).run()?;
```

`POST` は JSON 本文（単発またはバッチ）、`GET` は `?query=...&variables=...` を受け、クエリの無い `GET` でブラウザ
（`Accept: text/html`）なら GraphiQL を返します。クライアントが求めたときは `application/graphql-response+json`、
それ以外は `application/json` で応答します。`Accept: text/event-stream` の要求はサブスクリプションを SSE
（GraphQL over SSE の distinct connections モード）で返します（結果ごとに `event: next`、最後に `event: complete`）。

[`veil-proxy-kit`](library-kit.md#graphql) のクレートには同じ橋渡しが `veil_kit::graphql::mount` としてあり、WebSocket
（`graphql-transport-ws`・`graphql-ws`）のサブスクリプションも使えます。

> v0.7 までは `graphql` feature が `App::graphql(path, schema)` を提供していました。API ライブラリが async-graphql を
> 引き込まないように廃止しました（F-207）。同じ動作はサンプルの `src/graphql.rs` をコピーすれば得られます。

## gRPC

`grpc-handler`（`api` に含まれる）が必要です。ワイヤ処理（5 バイトのフレーミング・`grpc-status`/`grpc-message` の
トレーラ・`grpc-timeout`・gzip/deflate）は veil 自身の実装で、tokio も tonic も使いません。protobuf の実装にも依存しません。
メッセージの符号化と復号は、メソッドの登録時に渡す **codec**（`veil::api::grpc::Codec<T>`）が行います。`veil-build` が
生成するコードは [prost](https://docs.rs/prost) のメッセージと生成された `ProstCodec` を使い、他の protobuf ライブラリや
JSON も `Codec` を実装すれば使えます。gRPC の呼び出しは普通のハンドラのルート（`POST /<package>.<Service>/<Method>`）で、同じリスナーの HTTP/2
（TLS・h2c）と HTTP/3 で受けます。

### コード生成

`veil-build` を build-dependency に、`prost`（0.13。`veil-build` の prost-build が生成するコードに合う版）を依存に足します。

```rust
// build.rs
fn main() -> std::io::Result<()> {
    veil_build::compile_protos(&["proto/helloworld.proto"], &["proto"])
}
```

```rust
pub mod hello { veil::include_proto!("helloworld"); }
use hello::greeter_server::{add_greeter, Greeter};
use veil::api::grpc::{Request, Response, Status};

struct MyGreeter;
impl Greeter for MyGreeter {
    async fn say_hello(&self, req: Request<hello::HelloRequest>)
        -> Result<Response<hello::HelloReply>, Status> {
        Ok(Response::new(hello::HelloReply { message: format!("Hello {}", req.get_ref().name) }))
    }
}

add_greeter(App::new().grpc_health(), MyGreeter).run()
```

`veil-build` はサービスごとに `<service>_server` モジュールを作り、トレイト（RPC ごとに 1 メソッド。`async fn` で実装でき、
Future に `Send` は要りません）、`add_<service>(app, 実装)`、`METHODS`、`ProstCodec`（`add_<service>` がメソッドの登録に使う prost の codec）を生成します。サーバ・双方向ストリーミングは
`Response<BoxStream<T>>` を返し、クライアント・双方向ストリーミングは `Request<Streaming<T>>` を受け取ります。
`veil_build::configure().veil_path("::my_veil")` でクレートのパスを変えられ、`config_mut()` で `prost_build::Config` を触れます。

`veil-build` はクライアントのモジュール `<service>_client` も生成します（`build_client(false)` で止め、`build_server(false)` で
クライアントだけにします）:

```rust
use hello::greeter_client::GreeterClient;

let client = GreeterClient::connect("http://127.0.0.1:50051")?;   // https:// は TLS の上の h2
let reply = client.say_hello(hello::HelloRequest { name: "veil".into() }).await?.into_inner();
```

unary のメソッドはメッセージか `Request<T>` を取り、サーバストリーミングは `Response<ClientStreaming<T>>`（`message().await`・
`trailers()`）を返し、クライアントストリーミングと双方向は `Stream<Item = T> + Unpin` を取ります。通信は
`veil::api::grpc::Channel`（`Channel::connect(url)`、独自の TLS には `Channel::with_client(&http_client, url)`）で、メタデータ・期限
（`grpc-timeout`）・圧縮したメッセージを送り、トレーラから `Status` を返します。上の HTTP/2 のクライアントを使い、ワーカーの
ランタイムの上で動きます。

### コード生成なし

メソッドは codec と一緒に登録します。prost のメッセージ（`#[derive(prost::Message)]`）用の codec は 10 行ほどです:

```rust
use veil::api::grpc::{Bytes, BytesMut, Codec, Request, Response, Status, Streaming};

#[derive(Clone, Copy)]
struct ProstCodec;
impl<T: prost::Message + Default + 'static> Codec<T> for ProstCodec {
    fn encode(&self, m: &T, out: &mut BytesMut) -> Result<(), Status> {
        m.encode(out).map_err(|e| Status::internal(e.to_string()))
    }
    fn decode(&self, buf: Bytes) -> Result<T, Status> {
        T::decode(buf).map_err(|e| Status::internal(e.to_string()))
    }
    fn encoded_len(&self, m: &T) -> usize { m.encoded_len() }   // 任意: メッセージごとの確保を 1 回にする
}

App::new()
    .grpc_unary("/pkg.Svc/Get", ProstCodec, |req: Request<GetRequest>| async move { Ok(Response::new(reply)) })
    .grpc_server_streaming("/pkg.Svc/Watch", ProstCodec, |req: Request<WatchRequest>| async move {
        Ok(Response::new(futures::stream::iter(items.into_iter().map(Ok))))
    })
    .grpc_client_streaming("/pkg.Svc/Upload", ProstCodec, |req: Request<Streaming<Chunk>>| async move { .. })
    .grpc_bidi("/pkg.Svc/Chat", ProstCodec, |req: Request<Streaming<Msg>>| async move { .. })
```

`veil::api::grpc::BytesCodec` は符号化されたメッセージを `Bytes` のまま渡します（`Request<Bytes>`・`Response<Bytes>`）。
自分で復号する場合や、そのまま転送する場合に使います。

> v0.7 までは codec を取らず prost のメッセージだけを受け、`veil::api::grpc::prost` で prost を再エクスポートしていました。
> F-208 以降、`veil-build` の生成コードは自分で `ProstCodec` を登録します（再ビルドするだけで移行できます）。手で登録している
> コードは上のように codec の引数を足し、`prost` に直接依存してください。

### 型と挙動

| 項目 | 挙動 |
|------|------|
| `Request<T>` | `get_ref()`・`into_inner()`・`metadata()` / `metadata_value(name)`（`content-type`・`te`・`grpc-timeout`・`grpc-encoding` を除く要求ヘッダ）・`deadline()`・`client_ip()` |
| `Response<T>` | `with_metadata(name, value)`（応答ヘッダ）・`with_trailer(name, value)`（`grpc-status: 0` の後ろ） |
| `Status` | `Status::new(Code::X, msg)` や `Status::not_found(msg)` 等。`with_metadata` はトレーラに足す。最初のメッセージより前のエラーは Trailers-Only 応答 |
| エラーの詳細 | `Status::with_details(bytes)` が符号化済みの `google.rpc.Status`（`BadRequest`・`ErrorInfo` 等を入れたもの）を `grpc-status-details-bin` で送る。符号化は利用者の protobuf ライブラリで行う |
| インターセプタ | `App::grpc_interceptor(prefix, \|req: &Request<()>\| -> Result<(), Status>)` が `prefix`（`"/"` や `"/pkg.Svc/"`）以下の gRPC のメソッドの前に、メタデータ・期限・クライアントのアドレス・`method()` を渡して呼ばれる。`Err` ならその状態で終える（認証・クォータ）。同じ前置きの gRPC 以外の要求は素通し |
| 送信の上限 | `[api] grpc_max_send_message_size`（既定 0 = 無制限）。超える応答メッセージは `RESOURCE_EXHAUSTED` で終える |
| `Streaming<T>` | `Stream<Item = Result<T, Status>>` と `message().await` |
| 期限 | `grpc-timeout` を veil のタイマーで強制し、ハンドラを取り消して `DEADLINE_EXCEEDED` で終える（ストリームはメッセージの間で確認） |
| 取り消し | クライアントの RST_STREAM・切断でハンドラの Future と応答ストリームを drop する |
| 圧縮 | `grpc-encoding: gzip`・`deflate` の要求を戻す（`compression` feature が必要。`full` に含まれる。無いビルドは `identity` だけを受け、他は `UNIMPLEMENTED`）。応答は `[api] grpc_response_compression`（1 回の呼び出しだけなら `Response::with_compression(Compression::Gzip)`）の方式をクライアントが `grpc-accept-encoding` で受けるとき、`grpc_response_compression_min_size` バイト以上のメッセージを圧縮する。それ以外は非圧縮 |
| 上限 | 受信メッセージ 1 つあたり `[api] grpc_max_message_size`（既定 4MiB）。超えたら `RESOURCE_EXHAUSTED`。圧縮されたメッセージは解凍後の大きさに掛かり、超えた時点で解凍を打ち切る |
| gRPC-Web | `application/grpc-web`・`application/grpc-web+proto` と base64 の `application/grpc-web-text(+proto)` を同じメソッドで受ける（HTTP/1.1・HTTP/2・HTTP/3）。トレーラは本文の最後の枠（フラグ `0x80`）で返すので、ブラウザ（grpc-web・Connect）から変換用のプロキシ無しで veil を直接呼べる。`OPTIONS` のプリフライトは `[api] grpc_web_cors_origins` で答える |
| エラー | `content-type` が `application/grpc*` 以外は 415、HTTP/1.1 の gRPC（gRPC-Web を除く）は `grpc-status: 13`、登録済みサービスの未定義メソッドと、どのルートにも当たらないパスへの gRPC は `UNIMPLEMENTED` |
| ヘルスチェック | `App::grpc_health()` が `grpc.health.v1.Health`（`Check`・`Watch`）を登録する。サーバ全体（`""`）は `SERVING`、`veil::api::grpc::health::set_serving_status(service, status)` で変える |
| リフレクション | `App::grpc_reflection(&[hello::greeter_server::FILE_DESCRIPTOR_SET])` で `grpc.reflection.v1` と `v1alpha`（`ServerReflectionInfo`）を登録する。grpcurl・Postman・grpcui が `.proto` 無しでサービスの一覧と型を取得できる。`FILE_DESCRIPTOR_SET` は `veil-build` が生成する（コンパイルしたパッケージごとに渡す）。`list_services` は登録したすべての gRPC サービスを返す。組み込みのヘルスチェックとリフレクション自身の型は返さない |

完全なサンプルは `examples/api/rust-grpc`（単項・3 種のストリーミング・期限・エラー・ヘルスチェック）で、
`tests/api/library_e2e.sh grpc` が tonic のクライアントと HTTP/3 のクライアントで試験します。

### OpenAPI の gRPC

登録した gRPC のメソッドは（`[api] openapi_grpc = false` でなければ）`/openapi.json` と Swagger UI に
`POST /<package>.<Service>/<Method>`（grpc-gateway の出力と同じ形）として、タグ `gRPC: <package>.<Service>` の下に載ります。
OpenAPI は REST の記述なので、呼び出し方ではなくメソッドと型の一覧です。要求の本文は `application/grpc`・`application/grpc-web`、
200 の応答は `application/grpc` として載り、Swagger UI の「Try it out」はこの操作では出ません。操作ごとの `x-grpc` に
`service`・`method`・`kind`（`unary`・`server_streaming`・`client_streaming`・`bidi_streaming`）・`streaming`、型がわかれば
`requestType`・`responseType` が入ります。ストリーミングは `summary` にも書かれ、スキーマは 1 メッセージの型です。

メッセージの型は veil に渡した記述子（FileDescriptorSet）から取ります。`App::grpc_reflection(&[FILE_DESCRIPTOR_SET])` は
リフレクションと OpenAPI の両方に、`App::grpc_descriptors(&[FILE_DESCRIPTOR_SET])` は OpenAPI だけに使います（リフレクションの
サービスは登録しない）。型は proto3 の JSON マッピングで `components.schemas` に完全修飾名（`demo.v1.HelloRequest`）で入ります。
64 ビット整数は文字列、`bytes` は base64 の文字列、enum は値の名前、`repeated` は配列、`map` は `additionalProperties` の
オブジェクト、`oneof` は `oneOf`、再帰する型は `$ref`、well-known types（`Timestamp` は `date-time` の文字列、`Duration`・
`Struct`・`Value`・`Any`・ラッパ・`FieldMask`・`Empty`）はそれぞれの JSON の形です。`.proto` のコメント（`veil-build` が残す）は
メソッド・サービス・メッセージ・フィールドの説明になります。型のわからないメソッド（記述子が無い・自前の `Codec`）は本文が
`{"type": "string", "format": "binary"}` で載ります。下位のルートで登録したメソッドの種類は `App::grpc_kind(path, MethodKind::...)`
で指定します。組み込みのヘルスチェック・リフレクションと、サービスごとの `UNIMPLEMENTED` の受け皿は
`[api] openapi_grpc_builtin = true` のときだけ載ります。文書は起動時とリロード時に作り、要求の処理には影響しません。

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

`listen(addr)` が TLS のリスナー（`[server].listen`）で、`tls()` が `[tls] cert_path/key_path` です。ライブラリ既定の平文リスナー
（`listen_cleartext = "127.0.0.1:8080"`、ループバックのみ）はそのまま残るので、`listen_cleartext(..)` で移すか、`.secure_redirect(true)` で TLS ポートへの
リダイレクトにします。`.http3("0.0.0.0:8443")` を足すと UDP ポートで HTTP/3 も受けます（`http3` feature が必要で、`full` に含まれます。
`[http3] enabled = true` + `[http3] listen`）。
そのほかの `[tls]`・`[http2]`・`[http3]`・`[security]` のキーは `config.toml` か `set(..)` で設定します。ライブラリモードでは
サンドボックスは **既定でオフ** です。`enable_seccomp` / `enable_landlock` を有効にすることもできますが、ハンドラが行う操作
（ファイル・ソケット・DNS）を許可する必要があります。

### 複数のリスナー

プロトコルの違うポートを複数開く（例: HTTP/1.1 だけの TLS、h2c だけ、HTTP/2 + HTTP/3）には、ポートごとに `Listener` を
1 個足します。それぞれが `[[listener]]` の 1 項目になります（規則は[複数のリスナー](configuration.md#複数のリスナーlistener)）。

```rust
use veil::api::{Config, Listener};

let config = Config::load()?
    .builder()
    .tls("/etc/veil/cert.pem", "/etc/veil/key.pem")
    .listener(Listener::cleartext("0.0.0.0:8080").name("plain"))
    .listener(Listener::tls("0.0.0.0:8081").name("h1tls").http2(false))
    .listener(Listener::cleartext("0.0.0.0:8082").name("h2c").allow_http(false))
    .listener(Listener::tls("0.0.0.0:8443").name("https").http3(true))
    .build();
```

`Listener` のメソッド: `tls(addr)` / `cleartext(addr)`、`name`、`http2`、`http3`、`http3_listen`（`http3` を含む）、
`allow_http`、`allow_h2c`、`secure_redirect`、`redirect_to(name)`（`secure_redirect` を含む）、`threads`、
`unix_socket_permissions`。リスナーを 1 個でも足すと、ライブラリ既定の平文リスナー（`listen_cleartext = "127.0.0.1:8080"`）は
`listen_cleartext` を自分で設定しない限り開きません。C では `veil_listener_t` を埋めてポートごとに
`veil_config_add_listener(cfg, &l)` を呼び、Python では `veil.run(listeners=[veil.Listener("0.0.0.0:8443", tls=True, http3=True), ...])`
を渡します。

## メモリアロケータ

veil をライブラリとして使う場合（`handler` feature を含むビルド。`api` は含む）は、veil は `#[global_allocator]` を
**宣言しません**。アロケータはアプリケーションが選びます。アロケータ feature（`full` の `veil-alloc`、`default` の
`mimalloc`）はアロケータのコードをコンパイルするので、自分のバイナリで明示的に選べます。

```rust
// veil 専用アロケータ（unix。`veil-alloc` は `full` に含まれる）
#[cfg(unix)]
#[global_allocator]
static GLOBAL: veil::veil_alloc::VeilAlloc = veil::veil_alloc::VeilAlloc;

// または mimalloc（依存に `mimalloc` クレートを足す）
// #[global_allocator]
// static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
```

何も宣言しなければ Rust のシステムアロケータになります（Windows では上の例もこれになります）。`veil` バイナリ（`handler` 無しのビルド）は従来どおり
アロケータを選びます。`examples/api/rust-showcase` は `VeilAlloc` を宣言しています。

アロケータ feature は排他です（[メモリアロケータ](performance.md#メモリアロケータf-183)を参照）。
C と Python のビルド（[`veil-capi`](../../../crates/veil-capi/README.md)・[`veil-python`](../../../crates/veil-python/README.md)）は
アロケータ feature を一切有効にしないので、ホストプロセス（C プログラムや Python インタプリタ）のアロケータを置き換えません。
Linux では io_uring ではなく epoll（readiness リアクタ）バックエンドでビルドされ、他の OS は既定のリアクタ（kqueue・WSAPoll）です。
gRPC・WASM・VDS・ACME は含みません。

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
    veil_config_listen_cleartext(cfg, "0.0.0.0:8080");   /* 平文で外部に公開（既定は 127.0.0.1:8080）。TLS は veil_config_listen + veil_config_tls */
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

他の OS では動的ライブラリは `libveil.dylib`（macOS）・`veil.dll` + `veil.dll.lib`（Windows、MSVC）で、静的リンクに要る
システムライブラリも違います（OpenBSD・NetBSD に `libdl` は無い、Windows は `ws2_32.lib` 等）。一覧は rustc に聞けます:
`cargo rustc -p veil-capi --release --crate-type staticlib -- --print native-static-libs`
（`examples/api/c-hello/Makefile` は `LIBS=...` で受け取ります。MSVC のコマンドラインは `tests/api/library_e2e.sh` にあります）。

**スレッドモデル。** 既定ではコールバックは **ハンドラ用プール**（`[api] handler_threads`）で実行されるので、ブロックしてもかまいません（DB・ファイル）。
ただし **複数のスレッドから同時に呼ばれる** ので、共有データを保護し、`user_data` をスレッドセーフにしてください。
`veil_register_route_ex(..., VEIL_ROUTE_INLINE)` はコールバックをネットワークワーカー上で直接実行します（スレッドの切り替えが無い）。
即座に終わる処理だけに使ってください。ブロックするとそのワーカーの全接続が止まります。リクエストアクセサが返すポインタは
NUL 終端ではなく、コールバックから戻るまで有効です。OpenAPI は C のルートにも生成されます（`veil_register_route_ex` に
Operation Object を JSON で渡すと説明が反映されます）。設定は `veil_config_set(cfg, "server.threads", "4")` で上書きでき、
`VEIL_OVERRIDE_<n>` も通常どおり効きます。詳細（所有権の表・エラーコード・API 一覧）は
[crates/veil-capi/README.md](../../../crates/veil-capi/README.md)、サンプルは [examples/api/c-hello/](../../../examples/api/c-hello/) です。

**ストリーミング・gRPC・L4（F-210）。** C API も Rust と同じ機能を持ちます。veil はバイト列を運ぶだけで、メッセージの
直列化は好きなライブラリ（protobuf-c・nanopb・upb 等）で行います。

- 応答のストリーミング: `veil_stream_t *s = veil_response_stream(resp)` はそれまでに設定したステータスとヘッダを送り、
  コールバックから戻った後も使えるハンドルを返します（別スレッドで書き続けてもかまいません）。`veil_stream_write`
  （複製する。満杯なら待つ。切断後は `VEIL_ERR_CLOSED`）・`veil_stream_set_trailer`・`veil_stream_close`（必須。本文を終えて解放）。
- 要求本文: `veil_request_body_read(req, buf, cap)` で次の断片を読みます（0 で終わり）。ルートのフラグ
  `VEIL_ROUTE_STREAM_BODY` を付けると、本文が届く前にコールバックを呼び、届いた順に返します。
- gRPC: `veil_register_grpc(cfg, "/pkg.Svc/Method", VEIL_GRPC_UNARY | ..._SERVER_STREAMING | ..._CLIENT_STREAMING | ..._BIDI, fn, ud)`。
  コールバックの中で `veil_grpc_recv` / `veil_grpc_send`（符号化済みのバイト列）・`veil_grpc_set_status`・
  `veil_grpc_set_header` / `_set_trailer`・`veil_grpc_metadata`・`veil_grpc_deadline_ms`。
  `veil_register_grpc_health(cfg)` で `grpc.health.v1.Health` を足します。圧縮・期限・gRPC-Web・`[api] grpc_*` のキーは
  Rust のハンドラと同じで、HTTP/2・HTTP/3 の両方で動きます。
  `veil_grpc_descriptors(cfg, data, len)` に `FileDescriptorSet`（`protoc --include_imports --descriptor_set_out=...` の出力）を
  渡すと、メソッドがメッセージの型付きで OpenAPI に載ります（[OpenAPI の gRPC](#openapi-の-grpc)）。
- L4: `veil_register_l4_tcp(cfg, name, listen, &callbacks, ud)`（`on_connect` / `on_data` / `on_close`。ハンドラ用プールで
  接続ごとに 1 つずつ順に呼ばれ、`veil_l4_write` / `veil_l4_close` はコールバックの中で）と
  `veil_register_l4_udp(cfg, name, listen, fn, ud)`（`veil_l4_reply` でデータグラムに応答）。
- プロトコルアップグレード（HTTP/1.1）: `veil_response_upgrade(resp, protocol, &callbacks, ud)` で 101 を返し、接続を L4 の
  TCP と同じコールバックで受けます（[プロトコルアップグレード](#プロトコルアップグレードhttp11)）。
- WebSocket: `veil_ws_accept(req, resp, &callbacks, ud)`・`veil_ws_send`・`veil_ws_close`（[WebSocket](#websocket)）。
- フォーム: `veil_multipart_next` とルートのフラグ `VEIL_ROUTE_FORM`（[フォームとファイルのアップロード](#フォームとファイルのアップロード)）。
- ストリーミングの関数は `VEIL_ROUTE_INLINE` のルートでは使えません（`VEIL_ERR_STATE`）。

全部を使うサンプル: [examples/api/c-features/](../../../examples/api/c-features/)（Rust の gRPC のサンプルと同じ tonic の
相互運用試験を通ります）。

## Python

`crates/veil-python` は PyO3 の拡張（abi3、Python 3.9 以上）で、`veil` として import します。

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

veil.run(listen_cleartext="0.0.0.0:8080")               # ブロックする（外部に公開。既定は 127.0.0.1:8080）。Ctrl-C / SIGTERM で graceful に停止
```

- デコレータは `@veil.get/post/put/delete/patch/route`。`app = veil.App(title=..., version=...)` を作って `@app.get(...)`・`app.run()` も使えます。
  引数は名前と注釈で束縛され、戻り値は変換されます（dict/list → JSON、`str` → テキスト、`veil.Response`、`(body, status)`、`None` → 204）。
  `veil.HTTPException(404, "...")` は `{"detail": ...}` になります。**pydantic** があれば `BaseModel` の本文を検証して OpenAPI に反映し、
  無ければ `body_constraints` で dict の本文を検証します。OpenAPI は `/openapi.json` と `/docs` で公開されます。
- **offload と GIL。** ハンドラは GIL を握って実行されます。既定（`offload=True`）ではハンドラ用プールで実行されるので、
  イベントループが GIL を待つことはありません。`offload=False` はネットワークワーカー上で直接実行する指定で、自明に軽いハンドラ専用です。
  純 Python のハンドラの並列度は GIL に制限されるので、CPU バウンドな処理は複数プロセス構成にしてください。
- **サーバの中では**、ハンドラに Rust 側の要求（`veil._veil.NativeRequest`。`isinstance` では `veil.Request`）が渡ります。属性・メソッドは同じで、ハンドラが読んだ属性だけを作ります。よくある戻り値（`str`・`bytes`・`dict`/`list`・`None`・`veil.Response`）は Rust で本文を複製せずに応答にします（F-212）。Python 3.9 以上が必要です。
- **async def とイベントループ（F-263）。** `async def` のハンドラは **常駐の asyncio のイベントループ**（uvicorn と同じく専用のスレッド。
  uvloop が入っていれば uvloop）で動きます。`await` の間はスレッドを握らないので、数千の要求が同時に待て、`asyncio.create_task` の
  タスクは要求の後も動き続けます。中でブロックするとループ全体が止まるので、`await veil.run_in_threadpool(f, ...)` か普通の `def`
  にしてください。クライアントが切断するとハンドラのタスクはキャンセルされます。`[api] python_event_loops`・`python_event_loop`・
  `python_event_loop_queue` でループを調整し、`async_mode="pool"`（ルートごと、または `[api] python_async_mode = "pool"`）で 0.10 より
  前の動き（ハンドラ用プールのスレッドごとのイベントループ）に戻せます。`veil.App(lifespan=...)`・`@app.on_startup` / `@app.on_shutdown`・
  `@app.on_event("startup" | "shutdown")` はリスナーを開く前とサーバが止まった後にループで走ります（開始で失敗すると終了コード 1）。
  lifespan が yield した dict は `app.state` に入ります。
- **WebSocket（F-263）** は Starlette / FastAPI と同じ名前です。`@app.websocket("/ws")` を `async def f(websocket: veil.WebSocket)`
  に付けます（`accept`・`receive_text/bytes/json`・`iter_text/bytes/json`・`send_text/bytes/json`・`close`・`veil.WebSocketDisconnect`）。
  `veil.WebSocketEndpoint` のクラスも使えます。フレーム・Ping / Pong・UTF-8 の検査・クローズの手順・上限は Rust で処理し（`[api] websocket_*`
  が効く）、Python は組み立て済みのメッセージだけを見ます。WebSocket のルートは `x-veil-websocket` 付きで OpenAPI に載ります（`receive=` /
  `send=` に pydantic のモデル、`validate=True` で型に合わなければ 1007）。`veil.Response.switching_protocols(protocol, factory)` で任意の
  プロトコルへアップグレードできます（`veil.Protocol` か `veil.AsyncProtocol`）。
- **ストリーミング（F-210）。** ジェネレータ・非同期ジェネレータを返す（`text/plain` で断片ごとに送る）か、
  `veil.StreamingResponse(iterable, media_type=..., trailers=[...])`、Server-Sent Events なら `veil.EventSourceResponse(iterable)`
  （要素は文字列・JSON にできる値・`{"event", "data", "id", "retry"}` の dict）。`@veil.post(..., stream_body=True)` は本文が
  届く前にハンドラを呼び、`for chunk in request.stream()` で届いた順に読みます（`async def` でも使えます。断片ごとにプールの
  スレッドで待ちます）。
- **gRPC（F-210）** は `veil.grpc`（依存なし）: `@veil.grpc.unary_unary("/pkg.Svc/Method",
  request_deserializer=Req.FromString, response_serializer=Resp.SerializeToString)` と `unary_stream`・`stream_unary`・
  `stream_stream`。引数名は grpcio と同じなので直列化は何でも使えます（protobuf・betterproto・JSON、省略すると `bytes` のまま）。
  `context` は grpcio の `ServicerContext` の主要なメソッド（`abort`・`set_code`・`set_details`・`invocation_metadata`・
  `send_initial_metadata`・`set_trailing_metadata`・`time_remaining`・`is_active`）を持ちます。`pip install veil-api[grpc]` を
  入れると `grpc_tools.protoc` の生成コードがそのまま動きます: `veil.grpc.Server()` を `add_<Service>Servicer_to_server` に渡し、
  `server.add_insecure_port(...)`・`server.wait_for_termination()`。`veil.grpc.add_health()` で `grpc.health.v1.Health`。
  `async def` のメソッドと非同期ジェネレータも使えます。自分で `app = veil.App()` を作るときは `veil.grpc.Server(app)`・
  `veil.grpc.add_health(app)` と渡します（渡さないとモジュールの既定の App に登録され、`app.run()` が「配信されない」と警告します）。
- **L4（F-210）:** `veil.Protocol` のサブクラスに `@veil.l4_tcp(name, listen)`（asyncio と同じ `connection_made` /
  `data_received` / `connection_lost`。`self.transport.write` はコールバックの中で）、`def f(data, peer) -> bytes | None` に
  `@veil.l4_udp(name, listen)`。
- **フォームとファイル（F-266）:** FastAPI と同じ `veil.Form()`・`veil.File()`・`veil.UploadFile`（`python-multipart` は要りません。
  [フォームとファイルのアップロード](#フォームとファイルのアップロード)）。
- **FastAPI と同じ書き方の依存（F-268）:** `veil.Depends`（入れ子・1 要求で 1 回のキャッシュ・`yield` の後始末・`async`）、
  `veil.Security`・`SecurityScopes`、引数の印 `Query()`・`Path()`・`Header()`・`Cookie()`・`Body()` と検査（FastAPI と同じ形の 422）、
  `BackgroundTasks`（応答を送り終えた後に実行）、`APIRouter`・`include_router`、`app.dependency_overrides`、`response_model`、
  `request.cookies`・`response.set_cookie()`、`veil.testclient.TestClient`。ルートの依存の木は署名から 1 回だけ作り、
  これらを使わないルートの束縛は今までと同じです。
- **ASGI（F-268）:** `veil.asgi.run(fastapi_app)` で uvicorn の代わりに FastAPI・Starlette のアプリを動かし、
  `app.mount("/legacy", fastapi_app)` で veil のアプリのパスの下に載せます（`http`（trailers・pathsend の拡張）・`websocket`・
  `lifespan`）。HTTP/1.1・HTTP/2・HTTP/3・TLS・圧縮は veil が受け持ち、アプリは常駐のイベントループで動きます。
- **`veil-api-kit`（F-268）:** 純 Python のパッケージ（`pip install veil-api-kit`、`import veil_kit`、版は `veil-api` と同じ）。
  署名付き Cookie とセッション、`CORSMiddleware`（サーバが答える）、`fastapi.security` と同じ認証の方式と JWT（PyJWT）、
  `safe_redirect`、`StaticFiles`（サーバのファイル配信）、strawberry の GraphQL（HTTP・SSE・WebSocket）、`grpc_tools` の
  生成スタブを veil の HTTP/2 クライアントで使う gRPC のチャネル（同期と `grpc.aio` 風。grpcio は不要）、DI のコンテナ、
  定期・cron のタスク。
- **GraphQL:** 好きなライブラリを普通のルートとして載せます。[examples/api/python-graphql/](../../../examples/api/python-graphql/)
  は strawberry（クエリ・ミューテーション・SSE のサブスクリプション）。`veil_kit.graphql.mount` は同じことを WebSocket の
  サブスクリプションと GraphiQL 付きで行います。
- 設定: `veil.run(config_path=None, *, listen_cleartext, listen, tls_cert, tls_key, http3, listeners, threads, overrides={"server.threads": 4}, openapi=True)`。`VEIL_OVERRIDE_<n>` も通常どおり効きます。

文書全体（入門・FastAPI からの移行の対応表・API 一覧）は <https://veil-api.readthedocs.io> にあります（英語が正。日本語版は gettext で作る。手順は crates/veil-python/docs/conf.py の冒頭）。
詳細は [crates/veil-python/README.md](../../../crates/veil-python/README.md)、サンプルは
[examples/api/python-hello/](../../../examples/api/python-hello/)・
[examples/api/python-fastapi-like/](../../../examples/api/python-fastapi-like/)・
[examples/api/python-features/](../../../examples/api/python-features/)（ストリーミング・L4）・
[examples/api/python-grpc/](../../../examples/api/python-grpc/)（grpcio の生成コードと基本の層）・
[examples/api/python-websocket/](../../../examples/api/python-websocket/)（WebSocket・アップグレード・lifespan・イベントループ）・
[examples/api/python-kit/](../../../examples/api/python-kit/)（`Depends`・`veil-api-kit`・載せた FastAPI のアプリ・gRPC のクライアント）です。

## ライブラリ成果物のビルド

Rust のライブラリは単なる Cargo 依存です。C と Python の成果物は次のスクリプトで作ります。

```bash
packaging/scripts/build-lib.sh [--kind all|rust|c|python] [--docker]
```

指定した種類（既定は `all`）をビルドし（`--docker` は `docker/Dockerfile.lib` を使ってコンテナ内でビルド）、
`packaging/output/lib/` に 1 つのアーカイブ `veil-lib-<version>-<arch>-linux-gnu.zip` を出力します。中身は
`libveil.so`/`libveil.a` と `veil.h`（C）、Python の wheel、Rust 向けのソースと使い方のメモです。
プロキシ本体のパッケージは[ビルドとパッケージ](building.md)を参照してください。

## 性能

[tools/api-bench](../../../tools/api-bench/README.md) で測りました。各フレームワークで同じ 2 つのエンドポイント
（`GET /plaintext` → `Hello, World!`、`GET /json/{name}` → `{"message":"Hello, <name>"}`）を実装し、HTTP/1.1 のキープアライブで
`wrk -t2 -c64 -d10s`、対象の順番を入れ替えながら 3 ラウンド交互に測った中央値です。サーバは物理コア 1 つ（ハイパースレッド 2 本、
CPU 0-1、サーバのスレッド・ワーカーは 2）、wrk はもう 1 つの物理コア（CPU 2-3）で、Intel i5-3320M（2 コア 4 スレッド、2012 年の
ノート PC）です。絶対値は小さいので比率で見てください。「CPU µs/req」はサーバのコアの busy 時間（カーネルのループバック処理を含む）を
要求数で割った値です。生データはスクリプトの実行後に `docs/artifacts/api_bench_{rust,python}.tsv`。

### Rust: tokio 上の axum・`veil::tokio` 上の axum・veil のハンドラ

| 対象 | plaintext req/s | json req/s | p99 ms（plaintext） | CPU µs/req（plaintext） | RSS MB |
|---|---:|---:|---:|---:|---:|
| axum + tokio、`axum::serve` | 66,061 | 59,630 | 1.50 | 30.6 | 5.3 |
| axum + tokio、スレッドごと（`SO_REUSEPORT`） | 67,811 | 62,077 | 1.34 | 29.8 | 5.0 |
| axum + `veil::tokio`、`axum::serve` | 38,218 | 35,099 | 2.27 | 28.1 | 8.7 |
| axum + `veil::tokio`、スレッドごと | 54,491 | 49,930 | 1.48 | 37.2 | 8.2 |
| axum + `veil::tokio`、スレッドごと（epoll） | 56,607 | 52,010 | 1.35 | 35.8 | 7.0 |
| veil の `App` + axum の `Router`（`tokio_service`） | 49,960 | 46,013 | 3.37 | 41.5 | 12.0 |
| veil の `App` + axum の `Router`（epoll） | 46,905 | 44,225 | 3.34 | 44.0 | 9.8 |
| veil のネイティブハンドラ（`#[veil::route]`） | 68,832 | 63,069 | 2.83 | 29.4 | 11.4 |
| veil のネイティブハンドラ（epoll） | 67,013 | 61,303 | 2.78 | 30.2 | 10.0 |

`App` + `Router` の行は F-202（ハンドラ経路の 1 要求あたりの確保 14 → 4、応答のヘッダと小さな本文を 1 回で書く）の後に、
tokio 上の axum（スレッドごと、68,343 / 61,675 req/s）と同じ回で交互に測った値です（F-202 の前は 33,349 / 36,702 req/s）。
ネイティブの 2 行は F-205 の後に測り直した値です（同じ回の tokio 上の axum（スレッドごと）66,980 / 61,184 req/s と
5 ラウンド交互。`docs/artifacts/f206_final_h1.tsv`）。

- **axum は `veil::tokio` の上でそのまま動きます**（同じソースで、`tokio` クレートを shim に置き換えただけ）。コアあたりでは
  tokio 上の axum の **約 80%（io_uring）〜84%（epoll）** で、1 要求あたりの CPU は 20〜25% 多くなります。
- **マルチスレッドの `veil::tokio` ランタイムで `axum::serve` を動かすとスレッドは 1 本しか使われません**。work stealing が無いので、
  接続のタスクは受理したスレッドに留まります（サーバのコアの busy は 200% 中 85% ほど）。複数コアを使うには、スレッドごとに
  カレントスレッドのランタイムと `SO_REUSEPORT` のリスナーを持たせる（表の「スレッドごと」。`tools/api-bench/common.rs` の
  `run_percore`）か、ワーカーごとに受理する veil の `App` に Router を載せます。
- **veil のネイティブハンドラは tokio 上の axum の 103%（io_uring）/ 100%（epoll）**（両エンドポイント）で、1 要求あたりの
  CPU は同じか少ない（29.4 / 30.2 µs、axum は 30.5 µs）です。io_uring では、すぐ次の要求を送ってくるキープアライブの接続を
  readiness の確認 → 読み取りの 2 段ではなく期限付きの `RECV` 1 回で読みます（F-205、`[performance] keepalive_direct_read_ms`）。
  アイドル接続は今まで通り読み取りバッファを持ちません。axum の `Router` を載せる場合は `http::Request`/`http::Response`
  との変換が加わります。F-235 で、変換は要求のパスとヘッダの値（`Bytes`）を複製せずに共有し、応答の本文をキューに集めずに受け取り、
  載せたサービスの extensions に veil の `Request` を入れなくなりました。1 要求あたりの確保は 30 → 22 回（ネイティブは 4 回）、
  ネイティブハンドラに対する比は 67% → 76% です（同じ回の交互の計測で 49,960 対 65,945 req/s、2026-10-16）。22 回のうち
  約 12 回は axum の中（ルーティング・extensions・Box の Future）です。

### HTTP/2（h2c）と HTTP/3

`h2load -t2 -c64 -m10 -D10`（h2c。HTTP/3 は ngtcp2 付きでビルドした h2load）、同じマシン・エンドポイントで 3 ラウンド交互の
中央値です（`docs/artifacts/f206_final_{h2c,h3}.tsv`）。axum に HTTP/3 は無いので HTTP/3 の行は比較相手が無く、退行の監視に使います。

| 対象 | プロトコル | plaintext req/s | json req/s | CPU µs/req（plaintext） |
|---|---|---:|---:|---:|
| axum + tokio、スレッドごと（hyper の HTTP/2） | h2c | 87,586 | 78,050 | 23.3 |
| veil のネイティブハンドラ | h2c | 123,615 | 109,938 | 16.6 |
| veil のネイティブハンドラ（epoll） | h2c | 126,200 | 110,824 | 16.2 |
| veil のネイティブハンドラ（TLS） | HTTP/3 | 69,039 | 63,752 | 29.5 |
| veil のネイティブハンドラ（TLS、epoll） | HTTP/3 | 83,638 | 76,200 | 24.4 |

h2c では veil は **tokio 上の axum の 1.4 倍**の要求を、1 要求あたり 3 割少ない CPU で処理します（接続の全ストリームの
フレームのうち、1 周で用意できたものを 1 回の書き込みで送ります）。

### Python: FastAPI（uvicorn）と veil

| 対象 | plaintext req/s | json req/s | p99 ms（plaintext） | CPU µs/req（plaintext） | RSS MB |
|---|---:|---:|---:|---:|---:|
| FastAPI `async def`、uvicorn 1 ワーカー | 5,502 | 4,333 | 14.46 | 189.4 | 49.4 |
| FastAPI `async def`、uvicorn 2 ワーカー | 6,859 | 5,473 | 10.17 | 296.2 | 136.4 |
| FastAPI `def`、uvicorn 2 ワーカー | 3,661 | 3,174 | 50.23 | 551.5 | 140.6 |
| veil、`offload=True`（既定）、2 スレッド | 25,494 | 22,567 | 6.70 | 71.1 | 31.3 |
| veil、`offload=False`、2 スレッド | 26,229 | 24,226 | 2.95 | 77.0 | 30.4 |

FastAPI 0.143・uvicorn 0.54（uvloop・httptools）・pydantic 2。uvicorn のワーカーはプロセス、veil は 1 プロセスです。veil の行は
F-212 の後に測り直した値です（前のバインディングと 5 ラウンド交互。`docs/artifacts/f212_ab_py.tsv`。F-212 の前は `offload=True` で
15,848 / 12,237 req/s・110 µs/req）。FastAPI の行は F-201 の回の値です。veil は **2 ワーカーの FastAPI の 3.7〜4.4 倍**
（1 ワーカーの 4.6〜5.6 倍）の要求を、約 4 分の 1 の 1 要求あたり CPU と 4 分の 1 以下のメモリで処理します。HTTP の解析・
ルーティング・応答の生成を Rust で行い、Python で動くのはハンドラ本体だけだからです（要求のオブジェクトは Rust 側にあり、
ハンドラが読んだ属性だけが Python のオブジェクトになります）。
ただし Python のハンドラは GIL で直列化されるので、1 プロセスではコア数に比例して伸びません。`offload=True` は GIL を
待つ間もイベントループを止めませんが、ハンドラ用プールへの受け渡しのぶん p99 が高くなります。

## 対応プラットフォーム

ライブラリ（Rust・C・Python）と gRPC サーバ・VDS を、各 OS で `tests/api/library_e2e.sh` と `tests/standalone_e2e.rs` により
確認しました（F-203、2026-10）。

| OS | プロキシと VDS（`standalone_e2e`） | Rust の API | gRPC（h2c・h3） | C | Python | `veil::tokio` の実ライブラリ |
|---|---|---|---|---|---|---|
| Linux x86_64（io_uring・epoll） | ✓ | ✓ | ✓ | ✓ | ✓ | ✓（PostgreSQL を含む） |
| macOS 15 arm64（kqueue） | ✓ | ✓ | ✓ | ✓（clang） | ✓ | ✓ |
| FreeBSD 14.3 x86_64（kqueue） | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| OpenBSD 7.9 x86_64（kqueue） | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| NetBSD 10.1 x86_64（kqueue） | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Windows 11 x86_64（WSAPoll） | ✓（SIGHUP・Secret の差し替えの試験を除く） | ✓（SIGHUP を除く） | ✓ | ✓（MSVC） | ✓ | ✓（F-204） |

- **Windows** にはシグナルがありません。SIGHUP による設定の再読み込みと SIGTERM による graceful shutdown は使えません
  （Ctrl-C で止まります）。`veil::tokio` には Windows ではまだ `net`・`signal` がありません（[tokio 互換](#tokio-互換)）。
- C のライブラリは OS のツールチェーン（cc/clang、Windows は MSVC）でビルドします。リンクは [C API](#c-api) を参照してください。

## テスト

```bash
tests/api/library_e2e.sh [rust|grpc|upgrade|websocket|c|python|nonregression|all]     # 既定: all
```

Rust のショーケース、C のサンプル（動的・静的）、Python のサンプルをビルド・起動し、curl で叩きます（ルーティング・検証・
OpenAPI・tower・L4 エコー・ストリーミング・HTTP/1.1 と h2c、`examples/api/rust-graphql` で GraphQL と SSE のサブスクリプション）。`grpc` は `examples/api/rust-grpc` に対して gRPC の相互運用試験を行います（Linux では io_uring と epoll の両方のビルド）。`websocket` は WebSocket の試験（ハンドシェイク・エコー・断片化・まとめ送り・プロトコル違反・2,000 接続〈`WS_E2E_CONNS=16384` で増やせる〉・繋ぎ直しの繰り返し・graceful shutdown の 1001、平文・TLS・kTLS。docker と `crossbario/autobahn-testsuite` があれば Autobahn TestSuite の fuzzingclient も）です。`python` は `examples/api/python-websocket` に同じ試験と Autobahn の 1.*〜7.* を行い、`await asyncio.sleep(1)` の要求 2,000 本の同時実行（`PY_E2E_SLEEPERS` で増やせる）・lifespan・アップグレードも確かめます。`nonregression` はプロキシ本体のバイナリにライブラリのコードが入っていないことを確認します。
Linux・macOS・FreeBSD・OpenBSD・NetBSD・Windows（Git Bash）で動き、OS の `full*` feature を選びます。
cargo・C コンパイラと make（BSD は GNU make、Windows は MSVC）・HTTP/2 対応の curl・openssl・protoc（gRPC 用）・python3 が必要です
（python3 の名前が違うときは `PY3` で指定。Windows では `PY3=python`）。単体テストは `cargo test --lib --features full,api`、`cargo test -p veil-capi`、Python は `tests/test_binding.py` と `tests/test_aio.py` です。
[テスト](testing.md)も参照してください。

## 制限事項

- gRPC クライアント: `veil-build` の生成するクライアントと `Channel` は HTTP/2 だけです（gRPC-Web・HTTP/3 は無い）。
  `veil::tokio` の上の tonic も引き続き使えます。
- `veil::api::client` は本文を全部読みます（応答の本文のストリーミングはまだ無い）。リダイレクトを追わず、Cookie も保持しません。
- `serve` / `run` は **プロセスにつき 1 回** だけ呼べます（設定フックと共有状態がプロセス全体のため）。2 回目はエラーになります。
- Python のハンドラは GIL に制限されます。C のコールバックはスレッドセーフである必要があります。`async def` のハンドラの中でブロックすると、
  そのイベントループが止まります（上記）。
- プロトコルアップグレードと WebSocket は HTTP/1.1 だけです（HTTP/2・HTTP/3 の拡張 CONNECT〈RFC 8441 / RFC 9220〉は非対応）。
- ライブラリモードは `/etc/veil` の設定ファイルを使いません。カレントディレクトリの `config.toml`（または渡したパス）だけです。
