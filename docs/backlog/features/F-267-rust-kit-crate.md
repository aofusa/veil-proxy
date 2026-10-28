# F-267: Rust のヘルパークレート `veil-proxy-kit`（`veil_kit`）と tower のサンプル、API ライブラリの HTTP / gRPC クライアント

**状態: 完了（2026-10-26。2026-10-25 起票・設計。親は [F-258](F-258-v0100-plan.md)）**

## 依頼

Rust のライブラリ利用で、Cookie・リダイレクト・静的ファイル配信・CORS・認証認可を **tower のサンプル** で示す、または
**ヘルパークレート** で提供する。ヘルパークレートにするなら GraphQL（async-graphql）・gRPC（prost、クライアントを含む）・
動的な依存性注入・バックグラウンドタスクも含める。

## 決定

**両方やる**: ヘルパークレート `veil-proxy-kit`（ライブラリ名 `veil_kit`、`crates/veil-kit`、ワークスペースのメンバー）を作り、
加えて tower の生態系（tower-http 等）だけで同じことをするサンプル `examples/api/rust-tower-recipes` を置く。理由:

- 既存の tower の互換（`App::layer`・`layer_on`）は本文が `http_body_util::Full<Bytes>`（溜める）で、静的ファイル・大きな応答に向かない。
  tower-http の `ServeDir` は tokio の fs を要る。性能と機能の両方で veil に合わせた実装が要る。
- 一方で、既に tower を使っている人には tower のまま移れる道を示したい（サンプル）。

### 本体との関係（制約）

- `veil_kit` は `veil`（`api` feature）に依存する **別のクレート**。`veil` の default・`full*` に入らず、プロキシのバイナリには関係しない。
- F-207 で async-graphql を `veil` から外した方針（使わない人に依存を強いない）は守る: GraphQL・gRPC・JWT 等は **`veil_kit` の
  feature**（既定は off、`full` で全部）。
- `veil` 側に足すものは「kit が使う土台」で、ライブラリの範囲（`src/api/`、`cfg(feature = "handler")`）に限る:
  静的ファイルのルートの注入（下の 3.）・応答後の処理のフック（8.）・非同期の抽出子（7.）・HTTP クライアント（6.）。

## 設計（`veil_kit` のモジュールと feature）

| モジュール | feature | 内容 |
|---|---|---|
| `cookie` | `cookie`（既定） | 要求の Cookie の解析（`CookieJar` 抽出子、`Cookie` ヘッダを 1 回だけ走査し値は `Bytes` の切り出し）、`Set-Cookie` の組み立て（`Secure`・`HttpOnly`・`SameSite`・`Max-Age`・`Partitioned`、RFC 6265bis の検査）、署名付き（HMAC-SHA256）・暗号化（AES-256-GCM）Cookie。暗号は TLS のプロバイダの crate（aws-lc-rs / ring）を使い依存を増やさない |
| `redirect` | 既定 | `Redirect::to / permanent / temporary / see_other`（301/302/303/307/308）、`Location` の検査（CR/LF の拒否）、開いたリダイレクトを防ぐ `Redirect::safe(url, allowed_hosts)` |
| `serve` | 既定 | 静的ファイル（3.） |
| `cors` | `cors`（既定） | CORS のミドルウェア（4.） |
| `auth` | `auth`（既定）・`jwt` | 認証・認可（5.） |
| `session` | `session` | Cookie のセッション（署名付き Cookie の中身 / サーバ側の保存先の trait。メモリの実装付き） |
| `client` | 既定 | `veil::api::client`（6.）の再公開と便利関数（JSON の送受信、再試行） |
| `grpc` | `grpc` | prost の codec（`ProstCodec`）・サーバの登録の補助・**クライアント**（6.）・`veil-build` の再公開 |
| `graphql` | `graphql` | async-graphql のマウント（GET・POST・バッチ・GraphiQL（同梱、オフライン）・SSE と WebSocket（`graphql-transport-ws`、F-262）のサブスクリプション） |
| `di` | `di`（既定） | 動的な依存性注入（7.） |
| `tasks` | `tasks`（既定） | バックグラウンドタスク（8.） |
| `full` | – | 全部 |

### 1. 共通の方針

- ミドルウェアは veil のネイティブの `App::wrap_on`（`HandlerFn` を包む）で実装し、tower の `Layer` も実装する（tower の利用者向け。
  `tower-compat` のとき）。
- 要求ごとの確保を増やさない: 設定で決まるヘッダ（CORS の `Access-Control-Allow-*` 等）は組み立て済みの `Bytes`（`from_static` か
  起動時に 1 回作った `Bytes` の `clone`＝参照カウント）。

### 2. Cookie・リダイレクト

- `CookieJar` は抽出子（`FromRequest`）。応答側は `Response::with_cookie(Cookie)`（`Set-Cookie` を足す拡張 trait）。
- 署名・暗号化の鍵は `Key::from(&[u8; 64])`、鍵の入れ替え（古い鍵で検証だけ）に対応。

### 3. 静的ファイル（`veil_kit::serve::dir`）

- **本体の File バックエンドをそのまま使う**: `App::serve_dir("/static", "/srv/www", ServeOptions { index, cache, compression, .. })`
  （`veil` 側に足す。中身は `config_override` の合成で `[[route]] type = "File"` と `[route.open_file_cache]`・
  `[route.static_file_cache]`・`[route.compression]` を注入するだけ）。sendfile・splice・kTLS・dirfd 相対の `openat2`
  （F-153）・事前圧縮ファイル・Range・ETag がそのまま効く。ハンドラの経路を通らないので要求ごとの費用はプロキシの静的配信と同じ。
- kit は `ServeOptions` の便利な既定（SPA の `fallback = "index.html"` 等）と、ハンドラからファイルを 1 個返す `serve::file(path)`
  （`offload` で開き、`Response::stream` で送る。小さなファイル向け）を持つ。

### 4. CORS

- `Cors::new().allow_origins([..]).allow_methods([..]).allow_headers([..]).allow_credentials(true).max_age(600)`。
  `Origin` の照合は完全一致の集合（起動時に作る）と、任意で正規表現 1 個。プリフライトは同じパスの `OPTIONS` を自動で登録して
  204 を返す（ハンドラを呼ばない）。`*` と credentials の組み合わせは起動時にエラー。`Vary: Origin` を付ける。
- 本体の `[route.traffic.cors]`（プロキシのルート用）とは別物。ハンドラのルートに本体の CORS が効くかは実装時に確かめ、
  効くなら kit の文書でそちらも案内する（二重に付けない注意）。

### 5. 認証・認可

- 抽出子: `BasicAuth`・`BearerToken`・`ApiKey<"x-api-key">`・`Jwt<Claims>`（`jwt` feature。HS256/384/512・RS256・ES256・EdDSA、
  `kid` で鍵を選ぶ、`exp`・`nbf`・`aud`・`iss` の検査、JWKS の取得は 6. のクライアントで起動時と定期更新（8. のタスク））。
  署名の検証は TLS のプロバイダの crate を使う（`jsonwebtoken` に依存しない。aws-lc-rs / ring を二重に持たないため）。
- ガード: `wrap_on("/admin", require(|p: &Principal| p.has_scope("admin")))`。断るときは 401（`WWW-Authenticate` 付き）か 403。
- 認可の判定は `Principal`（主体・スコープ・役割）を要求の拡張（`Request` の型付きの拡張領域。`veil` 側に `Request::extensions()` を
  足す。使わない要求では空の `Option` のまま）に置いて、ハンドラは `Auth<Principal>` 抽出子で受ける。
- FastAPI の `OAuth2PasswordBearer` に当たる `OAuth2Bearer::new("/token")`（OpenAPI の `securitySchemes` に載る）。

### 6. HTTP クライアント・gRPC クライアント（`veil` 側の土台 + kit）

- **`veil::api::client`**（`veil` に新設、feature `handler`）: ワーカーのランタイムの上で動く非同期の HTTP クライアント。
  本体の上流の部品をそのまま使う: HTTP/1.1 は上流の接続プール、HTTP/2 は `http2::upstream_mux`（多重化・接続とストリームの
  フロー制御。F-174）、TLS は上流の TLS（信頼アンカーは F-259 の `[tls_client] ca`）、名前解決は `runtime::dns::resolve`。
  ```rust
  let client = veil::api::client::Client::builder().http2(Http2::Auto).timeout(Duration::from_secs(5)).build()?;
  let res = client.post("https://api.example.com/v1/x").json(&body)?.send().await?;
  let v: Value = res.json().await?;
  ```
  - 要求・応答の本文は `Bytes` / ストリーム。接続プールはワーカーごと（スレッドローカル。本体の上流のプールと同じ作り）。
  - h2c（prior knowledge）・`unix:` の上流（F-170）・mTLS（F-217 の設定の形）を選べる。
- **gRPC クライアント**: `veil-build` に `client = true` を足し、`<Service>Client<C>` を生成する（unary・3 種のストリーミング、
  メタデータ・期限（`grpc-timeout`）・圧縮・`Status`）。通信は `veil::api::grpc::Channel`（上の HTTP/2 クライアントの上に
  gRPC の 5 バイトの見出し・トレーラを載せたもの。サーバ側と同じ `src/grpc/` の符号化を共有）。codec は F-208 の `Codec<T>`。
  kit の `grpc` は `ProstCodec` と `Channel::connect(url)` の便利関数、負荷分散（複数の宛先のラウンドロビン）・再試行の方針を持つ。
- tonic の互換は範囲外（tonic のクライアントは今どおり `veil::tokio` 上で使える。文書で両方を案内）。

### 7. 動的な依存性注入（`veil_kit::di`）

- `Container`: 型ごとに提供者を登録する。寿命は `singleton`（起動時かつ初回に作る）・`scoped`（要求ごとに 1 回作り、同じ要求の中で共有）・
  `transient`（毎回作る）。提供者は同期・非同期の関数で、他の依存を引数に取れる（`|db: Dep<Pool>| async move { Repo::new(db) }`）。
  ```rust
  let di = Container::new()
      .singleton(|| PgPool::connect(..))
      .scoped(|pool: Dep<PgPool>, user: Auth<Principal>| async move { Repo::for_user(pool, user) });
  App::new().with(di).route(..);
  #[veil::route(path = "/items", method = "GET")]
  async fn items(repo: Dep<Repo>) -> Json<Vec<Item>> { .. }
  ```
- **「動的」の意味**: 提供者を実行時に差し替えられる（`di.replace::<Mailer>(..)`、試験での差し替え＝FastAPI の `dependency_overrides`、
  設定のリロードでの作り直し）。差し替えは型ごとの `ArcSwap` のスロットで、要求中の呼び出しは古い提供者で最後まで動く。
- **ホットパス（AGENTS.md「リクエスト時にレジストリを引かない」）**: 型から提供者のスロットへの解決は **ルートの登録時**（`App` の
  組み立て時）に 1 回行い、ハンドラの閉包にスロットの `Arc` を持たせる。要求ごとは `ArcSwap::load` と提供者の呼び出しだけ
  （`HashMap` の検索をしない）。`scoped` の値は要求ごとの小さな配列（ルートが使う scoped の個数分、登録時に決まる）に置く。
  依存の循環は登録時に検出してエラー。
- `veil` 側の変更: 抽出子の trait が同期（`FromRequest`）だけなので、非同期の抽出子 `FromRequestAsync`（`async fn`）を足し、マクロ
  （`crates/veil-macros/src/route.rs`）が型名の一覧に無い型でも `#[inject]` 属性か `Dep<..>`・`Auth<..>` のような
  「抽出子を実装した型」を引数に取れるようにする（今は型名で決め打ちしているため、利用者の型は path/query と見なされる）。
  `veil` の `App::with(impl Plugin)`（登録時のフック）も足す。

### 8. バックグラウンドタスク（`veil_kit::tasks`）

- **要求に紐づくもの**（FastAPI の `BackgroundTasks`）: 抽出子 `BackgroundTasks` に `add(async move { .. })` / `add_blocking(|| ..)`。
  応答を **送り終えた後** に実行する。`veil` 側に `Response::after_send(FnOnce)`（`Option` の欄 1 個。使わない応答では確認 1 回だけ）を足し、
  HTTP/1.1 は応答を書いた後・HTTP/2・3 はストリームの終わりで、ワーカーで `spawn`（ブロッキングは `offload_handler`）する。
- **アプリに紐づくもの**: `Tasks::spawn(fut)`（起動後に常駐）・`Tasks::every(Duration, f)`（定期。重なり防止・ゆらぎ）・
  `Tasks::cron("0 */5 * * * *", f)`（`cron` feature）。どのワーカーで動かすか（既定はワーカー 0。`on_all_workers` で全ワーカー）。
  - graceful shutdown（F-187）: 停止の開始で取消しの通知を出し、`[server] shutdown_timeout` まで待ってから drop。
  - 失敗（panic）は捕まえて `error!` と `veil_kit_task_failures_total`（`metrics` を有効にしたとき）。
- 重い処理はハンドラ用プール（`offload_handler`）。FS 用プールは使わない（AGENTS.md）。

### 9. GraphQL（`graphql` feature）

- `examples/api/rust-graphql` の橋渡し（F-207 で利用者のコードにしたもの）を kit に取り込む: `veil_kit::graphql::mount(app, "/graphql", schema)`。
  GET・POST・バッチ・`application/graphql-response+json`・SSE のサブスクリプション、**WebSocket のサブスクリプション**
  （`graphql-transport-ws`、F-262 の上）、GraphiQL を **同梱**（F-264 と同じ方法でオフライン。`graphiql` feature）。
- async-graphql の版は kit の依存で固定し、README に対応表を書く。

### 10. tower のサンプル（`examples/api/rust-tower-recipes`、独立クレート `[workspace]`）

- tower-http の `CorsLayer`・`SetResponseHeaderLayer`・`ValidateRequestHeaderLayer`（Bearer）・`RequireAuthorizationLayer`、
  tower-cookies、リダイレクト（`http::Response` で 302）、静的ファイル（tower-http の `ServeDir` を `veil::tokio` の上で動かす例と、
  推奨の `App::serve_dir` の例を並べる）、`App::layer` / `layer_on` での組み立て。`tests/api/library_e2e.sh` で動かす。
- 同じ機能を kit で書いた `examples/api/rust-kit`（全モジュールの例。GraphQL・gRPC のサーバとクライアント・DI・タスク込み）。

## 試験

- kit の各モジュールの単体（Cookie の RFC 6265bis の例・署名と暗号化の往復と改ざん検出、CORS のプリフライトと許可外の Origin、
  JWT の各アルゴリズム（`rcgen` で鍵を作る）と期限、DI の寿命・差し替え・循環の検出、タスクの応答後の実行と停止時の待ち）。
- E2E（`library_e2e.sh kit`）: `rust-kit` と `rust-tower-recipes` に curl・grpcurl・tonic のクライアントで当てる。
  `veil::api::client` は HTTP/1.1・h2・h2c・HTTPS（自己署名の CA を `tls_ca` で）・UDS。gRPC クライアントは tonic のサーバ
  （`tests/api/grpc_client/`）と veil のサーバの両方に。
- 非退行: `nonregression` の検査に `veil_kit::` を足す（full に無い）。`cargo tree --features api -p veil` に async-graphql・
  prost（`grpc-handler` 無し）・jsonwebtoken が出ない。
- 性能（`tools/api-bench`）: CORS + JWT + DI（scoped 1 個）を通した `/json` の CPU / 要求を、素の veil のハンドラと axum + tower-http と比べる。
  kit を使わないルートは変わらないこと。

## 文書

- `crates/veil-kit/README.md`（英語、crates.io 用）と `docs/guide/library-kit.md`（新規、英日）。`docs/guide/library.md`（英日）から案内。
  Limitations の「gRPC クライアントが無い」を更新。
- crates.io への公開は `release.yml` の対象に `veil-proxy-kit` を足す（`veil-proxy-build`・`veil-proxy-macros` と同じ手順。
  名前が空いているかを公開前に確かめる）。

## 受け入れ条件

- kit の全モジュールとサンプル 2 個が E2E で動く。本体のバイナリと `veil` の既定の依存が変わらない。
- 文書（英日）・`backlog.md` が更新されている。

## 実施内容（2026-10-26）

### veil 側の土台（`src/api/`、`cfg(feature = "handler")` の範囲）

- `Request::extensions()`（型付きの拡張領域。`OnceLock` で、使わない要求では確保しない。`share()` で要求の clone と同じ表を共有）、
  非同期の抽出子 `FromRequestAsync`（`FromRequest` の型には blanket impl）。マクロ（`crates/veil-macros/src/route.rs`）は kit の型名
  （`Dep`・`Auth`・`MaybeAuth`・`CookieJar`・`SignedCookieJar`・`PrivateCookieJar`・`BackgroundTasks`・`BasicAuth`・`BearerToken`・
  `ApiKey`・`Jwt`・`Session`）と `#[inject]` の引数を非同期の抽出子として扱う。
- `Response::after_send`（`Option` の欄 1 個。応答を送った後（ストリームは終わった後）に drop で動く）、`App::with(impl Plugin)`、
  `App::on_worker_start`（`WorkerInfo`、ワーカーの起動直後に `listener_workers` から呼ぶ）、`App::serve_dir`・`ServeOptions`
  （`config_override` の合成で `type = "File"` のルートと `open_file_cache`・`static_file_cache`・`compression` を注入）、
  `ShutdownSignal::new()` の公開。
- **`veil::api::client`**（`src/api/client.rs`）: `Client`・`ClientBuilder`（`http2`・`timeout`・`connect_timeout`・
  `pool_idle_timeout`・`max_response_size`・`ca`（F-259 の信頼アンカーと同じ指定）・`client_cert`・`danger_accept_invalid_certs`・
  `user_agent`・`default_header`）・`RequestBuilder`（`header`・`bearer_auth`・`basic_auth`・`query`・`body`・`json`・`form`・`timeout`）・
  `ClientResponse`・`ClientError`（`kind()`、`Response` へは 502 / 504）。HTTP/2 は `http2::upstream_mux` の多重化接続（TLS の h2・h2c）、
  HTTP/1.1 はワーカーごとのプール（宛先ごとに 64 本まで、プールの接続が要求前に切れていた冪等な要求は 1 回だけ送り直す）、TLS は
  上流の `tls_connector`（`TlsProfile`）、名前解決は `proxy::connect_target`（`runtime::dns::resolve`）、`unix:` の宛先。多重化接続の
  層は状態コードしか返さないので、プールに接続が無いときはクライアントが TCP・TLS を自分で張ってから登録し（`upstream_mux::pool_has_live`・
  `remember_h1` を `pub(crate)` に）、接続・証明書の失敗を `Connect` / `Timeout` として理由付きで返す（E2E の最初の実行で TLS の検証失敗が
  `Protocol` に見えて分かった）。
- **gRPC のクライアント**（`src/api/grpc/channel.rs`）: `Channel`（`connect`・`with_client`・`timeout`・`compression`・`metadata`・
  `max_message_size`、unary・サーバ・クライアント・双方向のストリーミング。双方向は要求の汲み上げをローカルのタスクで回す）、
  `IntoRequest`・`IntoStreamingRequest`、`Request::with_metadata`・`with_timeout`、`Response::trailers()`、クライアント側の `Streaming`
  （`ClientStreaming` として再公開）。`veil-build` に `build_client`・`build_server`（既定はどちらも true）を足し、`<service>_client` の
  `<Service>Client`（`new`・`connect`・`channel`）を生成する。
- 本体のバイナリ（`full`）は変わらない: 追加はすべて `handler` の範囲で、`nonregression` が `veil::api::client`・
  `veil::api::grpc::channel`・`veil_kit::` の記号が full に無いことを検査する。

### `crates/veil-kit`（パッケージ `veil-proxy-kit`、ライブラリ `veil_kit`、ワークスペースのメンバー）

- 設計の表のとおりのモジュール（`cookie`・`redirect`・`serve`・`cors`・`auth`（`jwt`）・`session`・`client`・`grpc`・`graphql`
  （`graphiql`）・`di`・`tasks`（`cron`）・`full`）。veil には `api-core` だけで依存する。暗号は aws-lc-rs（OpenBSD・NetBSD は ring）。
  JWT は HS256/384/512・RS256/384/512・PS256/384/512・ES256・ES384・EdDSA、PEM（SPKI・`RSA PUBLIC KEY`）・JWK・JWKS、`kid`、
  `exp`・`nbf`・`iss`・`aud`。単体試験 23 本と doctest（README を含む）。
- `tools/release/publish-crates.sh` の公開順に `veil-proxy-kit`（`veil-proxy-build` の後）、メンバーの書き換えに `crates/veil-kit` を足した。
  `tools/release/bump-version.sh` は kit の `veil` 依存の版と Cargo.lock の `veil-proxy-kit` も上げる。Dockerfile の cacher 段・
  `.dockerignore` は `crates/` を丸ごと写しているので変更不要（コメントだけ更新）。`tools/release/check-docs.sh` は `veil_kit` の rustdoc も見る。
  crates.io の `veil-proxy-kit` は空いている（2026-10-26 に確認）。

### サンプルと試験

- `examples/api/rust-kit`（全モジュール、gRPC のサーバと生成クライアント（h2c・TLS、unary・3 種のストリーミング・トレーラ・
  エラー）、GraphQL の HTTP・SSE・WebSocket、DI・タスク）と `examples/api/rust-tower-recipes`（tower-http の `CorsLayer`・
  `SetResponseHeaderLayer`・`ValidateRequestHeaderLayer::custom`・`ServeDir`、Cookie・リダイレクトはヘッダで）。
- `tests/api/library_e2e.sh kit`（`all` に含む）: rust-kit を既定（io_uring）と epoll で、tower のサンプルを 1 回。`veil::api::client` は
  HTTP/1.1・h2c・HTTPS の h2（ALPN）・HTTPS の HTTP/1.1・UDS・自己署名の CA の拒否、gRPC クライアントは h2c と TLS。
- tower のサンプルで見つけた shim の不具合は [B-170](../bugs/B-170-tokio-shim-take-requires-unpin.md)。

### 設計からの変更・残したもの

- DI: マクロは型しか知らないので、ルートの登録時にスロットを解決する代わりに、起動時に凍結した表を `TypeId` で 1 回引く
  （ロック無し）。循環は登録時ではなく最初の解決時に検出する（500 とログ）。
- ミドルウェアの順序: veil の `wrap_on` は後に登録したものが外側（先に動く）。`require` は `authenticate` より先に登録する（文書に書いた）。
- `Tasks` のミドルウェアは対象の接頭辞で要求ごとに確保 1 回（`share()`）。`veil_kit_task_failures_total` は作らず、panic はログだけ。
  アプリのタスクは停止の開始の次の周期で止まる（実行中の 1 回は待たない）。
- `OAuth2PasswordBearer` の `securitySchemes` は OpenAPI へ自動では載らない（`security_scheme()` の JSON を利用者が足す）。
- GraphiQL は CDN から読む（オフライン同梱は F-264 の方法がまとまってから）。tower 側の `Layer` 実装は作らなかった（ネイティブの
  `wrap_on` だけ）。tower-cookies は `http` の拡張領域を veil のハンドラへ渡さないので届かない（サンプルはヘッダで書いた）。
- File バックエンドに SPA のフォールバックは無い（`ServeOptions` に足さなかった）。クライアント証明書・CA はクライアントを作るとき
  （起動時）に読む。FreeBSD の capability mode では任意の宛先へ接続できない（接続ブローカーの許可リスト）。
- gRPC クライアントは veil 自身のサーバ（h2c・TLS）で試験した（tonic のサーバとの相互接続の試験は置いていない）。
- 性能の比較（`tools/api-bench`）は行っていない。kit を使わないルートで増えたのは `Response` の `Option` の欄 1 個と `Request` の
  `OnceLock` 1 個（未使用なら確保しない）。

