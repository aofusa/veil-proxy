# F-268: Python のヘルパーパッケージ `veil-api-kit`（`veil_kit`）と FastAPI の生態系との互換（Depends・BackgroundTasks・ASGI）

**状態: 完了（2026-10-26。親は [F-258](F-258-v0100-plan.md)。前提は [F-263](F-263-python-async-runtime-websocket.md)・[F-266](F-266-multipart-file-upload.md)）**

## 依頼

Python のライブラリ利用でも Rust（[F-267](F-267-rust-kit-crate.md)）と同じく、Cookie・リダイレクト・静的ファイル配信・CORS・認証認可の
サンプルかヘルパーパッケージを用意する。パッケージにするなら GraphQL・gRPC（クライアントを含む）・動的な依存性注入・
バックグラウンドタスクも含める。可能なら、veil 本体や性能に影響を出さない範囲で既存の FastAPI の生態系を使えるようにする。

## 決定と置き場所

ヘルパーパッケージを作る（サンプルは各機能の例として `examples/api/python-kit` に置く）。機能を 2 か所に分ける:

| 置き場所 | 入れるもの | 理由 |
|---|---|---|
| `veil-api`（今の wheel、`import veil`） | 引数の束縛に関わるもの: `Depends`・`Security`・`BackgroundTasks`・`Header()`・`Cookie()`・`Query()`・`Path()`・`Body()`（`Form()`・`File()` は F-266）、`APIRouter`、`app.dependency_overrides`、**ASGI の実行**（`veil.asgi`） | 束縛は登録時に決めて要求ごとに `inspect` しない今の作り（`_build_params`）に組み込む必要がある。ASGI は Rust 側の実装が要る |
| `veil-api-kit`（新規、純 Python の wheel、`import veil_kit`） | Cookie の署名・セッション・CORS・認証（Basic・Bearer・API キー・JWT・OAuth2 のパスワードフロー）・リダイレクトの補助・静的ファイルの補助・GraphQL（strawberry）・gRPC（サーバの補助とクライアント）・動的な DI のコンテナ・定期タスク・テスト用クライアント | 依存（strawberry・grpcio・PyJWT 等）を extras で選べる。`veil-api` の wheel を重くしない |

- PyPI の名前 `veil-api-kit` は公開前に空きを確かめる。版は `veil-api` と揃え、`veil-api==<同じ版>` に依存する。
- **本体・性能への影響を出さない**: どちらも `crates/veil-python` と純 Python の範囲（本体のバイナリは変わらない）。FastAPI 互換の
  束縛は登録時に解決し、使わないルートの要求の経路は今と同じ（`tools/api-bench/python` で確かめる）。

## 使う外部のライブラリ（2026-10-25 追記。依存は `veil-api-kit` の extras に閉じ、`veil-api` 本体は増やさない）

| 用途 | ライブラリ | どこで・どう使うか |
|---|---|---|
| GraphQL | **strawberry**（`veil-api-kit[graphql]`） | `veil_kit.graphql.mount(app, "/graphql", schema)`。strawberry の `Schema.execute`（非同期）を常駐のループ（F-263）で呼ぶ。サブスクリプションは strawberry の非同期ジェネレータを SSE と WebSocket（`graphql-transport-ws`）へ流す。GraphiQL は同梱（オフライン、F-264 と同じ方法）。graphene・ariadne は同じ形で書ける例を文書に置く（依存にはしない） |
| gRPC のコード生成 | **grpcio-tools**（開発時の依存。`python -m grpc_tools.protoc`） | 生成した `_pb2.py`・`_pb2_grpc.py` をそのまま使う: サーバは `veil.grpc.Server` に `add_XxxServicer_to_server(servicer, server)` を呼ぶ（今の互換層）、クライアントは生成した `XxxStub(channel)` に 4. の `veil_kit.grpc.Channel` を渡す |
| gRPC の実行時 | **protobuf**（メッセージの `SerializeToString`・`FromString`）。**grpcio は任意**（`veil-api-kit[grpcio]`） | veil の経路では grpcio の C 拡張・スレッドを使わない（直列化だけ protobuf）。grpcio の本物のチャネル・`grpc.aio` を併用してもよい（干渉しないことを試験で確かめる） |
| JWT | PyJWT（`veil-api-kit[jwt]`） | `veil_kit.security` の JWT の検証 |
| pydantic | pydantic v2（任意、今と同じ） | 引数・`response_model`・WebSocket の `receive`/`send` のスキーマ |
| ASGI のアプリ | FastAPI・Starlette 等（利用者が入れる） | `veil.asgi`（下の 2.）。veil は FastAPI に依存しない |
| イベントループ | uvloop（任意） | F-263 の常駐のループ |

## 設計

### 1. FastAPI の書き方との互換（`veil-api`）

- `Depends(fn, use_cache=True)`: 依存の関数（同期・`async`・ジェネレータ（`yield` の後を応答の後に実行）・`async` ジェネレータ）を
  入れ子で解決する。依存の木は **ルートの登録時** に `inspect.signature` で 1 回作り、要求ごとは決まった順に呼ぶだけ。
  同じ依存は 1 要求で 1 回（`use_cache`）。`Security(fn, scopes=[..])` と `SecurityScopes`。
- `app.dependency_overrides[fn] = other`（試験での差し替え。辞書の変更を検出して依存の木を作り直す。要求中の差し替えは次の要求から）。
- 引数の印 `Query()`・`Path()`・`Header()`（`_` → `-` の変換、`convert_underscores`）・`Cookie()`・`Body(embed=True)` と検証
  （`ge`・`le`・`min_length`・`pattern` 等。pydantic があれば `Annotated[int, Query(ge=1)]` を pydantic で、無ければ今の
  `body_constraints` と同じ検査）。違反は FastAPI と同じ形の 422。
- `APIRouter(prefix, tags, dependencies)`・`app.include_router(router)`・ルートの `dependencies=[Depends(..)]`・`response_model`
  （pydantic で直列化・絞り込み）・`status_code`・`responses`。OpenAPI にも反映（`securitySchemes` を含む）。
- `BackgroundTasks`: 引数に書くと要求ごとに渡す。`add_task(fn, *args)` は **応答を送り終えた後** に実行（同期の関数はハンドラ用プール、
  `async` は常駐のループ（F-263））。Rust 側は F-267 の `Response::after_send` を使う。
- `veil.testclient.TestClient(app)`（`requests` 風。実際にループバックで起動して当てる。`httpx` があればその API）。
- 対応表（`crates/veil-python/docs/quickstart.md` の FastAPI 対応表）を更新し、「無いもの」を明記する（例: `Request.form()` の
  ストリーミング・`StaticFiles` の `html=True` の細部など、実装時に表で確定する）。

### 2. FastAPI・Starlette のアプリをそのまま載せる（ASGI、`veil.asgi`）

```python
from fastapi import FastAPI
import veil
api = FastAPI()
...
veil.asgi.run(api, listen="0.0.0.0:8000")          # uvicorn の代わり（HTTP/1.1・h2c・TLS の h2・HTTP/3）
# または veil のアプリに一部だけ載せる
app = veil.App()
app.mount("/legacy", api)                           # /legacy 以下を ASGI へ。他は veil のネイティブのルート
```

- ASGI 3.0 の `http`・`websocket`（F-263 の WebSocket の受け口から ASGI のイベントへ）・`lifespan` のスコープ。
  `http.response.trailers`・`http.response.pathsend`（ファイル送信を veil の sendfile に回す）の拡張に対応する。
- 実行は常駐のループ（F-263）。HTTP の解析・TLS・HTTP/2・HTTP/3・圧縮・アクセスログは veil（Rust）が行い、ASGI のアプリには
  イベント（`receive` / `send`）だけを渡す。本文の断片は `bytes` に 1 回だけ写す（ASGI の仕様上 `bytes` が要る）。
- これで FastAPI のミドルウェア・依存・プラグイン（SQLAlchemy の依存、`fastapi-users`、Starlette の `SessionMiddleware` 等）が
  そのまま動く。性能は ASGI のアプリが律速なので、ネイティブのルート（1.）より遅い。文書で「移行の入口」と位置付け、
  重いルートからネイティブへ移す手順を書く。
- WSGI（Flask・Django）は範囲外（後続の候補。`a2wsgi` のような変換で ASGI 経由なら動くことは文書に書く）。

#### ASGI を入れたときの影響（2026-10-25 追記）

| 対象 | 影響 | 対策 |
|---|---|---|
| veil 本体（プロキシ・Web サーバのバイナリ） | **無し**（`crates/veil-python` の中だけ。`nonregression` で確かめる） | – |
| ASGI を使わないアプリ | **無し**（`veil.asgi` を import しなければ何も起動しない。常駐のループも F-263 の `async def` のためのもので、ASGI のためには増えない） | – |
| 同じプロセスのネイティブのルート（`mount` で混ぜたとき） | ASGI のアプリはループのスレッドで GIL を長く持つ（1 要求でスコープの dict・`receive`/`send` のコルーチン・ミドルウェアの層を Python で回す）。同期のハンドラ（プールのスレッド）・`async def` のハンドラ（同じループ）と GIL・ループを取り合い、ネイティブのルートの遅延とスループットが下がる | `app.mount("/legacy", api, event_loops=1)` で **ASGI 専用のループ** を分けられるようにする（既定は分ける）。GIL の取り合いは残るので、文書に「高い要求率のルートはネイティブへ移す」と書き、混ぜたときの性能を計測して載せる |
| ASGI のアプリ自身の性能 | ASGI のアプリが律速（FastAPI の依存の解決・pydantic・Starlette のミドルウェア）。veil が受け持つ HTTP の解析・TLS・HTTP/2・HTTP/3・圧縮の分だけ uvicorn より軽い見込み。本文は ASGI の仕様上 `bytes` に 1 回写す | uvicorn・granian と交互に測って文書に載せる（目標: uvicorn より速い） |
| ループを止める同期の処理 | ASGI のアプリの `async def` の中の同期の I/O はループ全体を止める（uvicorn と同じ） | ASGI 専用のループに分けて、ネイティブの `async def` に波及させない |
| メモリ | FastAPI・Starlette・pydantic の import（数十 MB） | `veil.asgi` を使ったときだけ |
| 機能の差 | ASGI のアプリには veil の Python の機能（`Depends` の登録時の解決・OpenAPI の統合）が効かない。OpenAPI は FastAPI のものが `/legacy/openapi.json` に出る | 文書の対応表に書く。veil の `/openapi.json` に ASGI 側の文書を合成するかは後続の候補 |
| ベンチマーク | HttpArena の veil-python はネイティブのルートで出す（ASGI は使わない）。ASGI のホストとしての提出（engine 区分）は後続の候補 | – |

### 3. `veil-api-kit`（`veil_kit`）

| モジュール | extras | 内容 |
|---|---|---|
| `veil_kit.cookies` | – | `response.set_cookie(...)`（`veil.Response` に足すのは `veil-api` 側。kit は署名付き Cookie `itsdangerous` 互換の形式・鍵の入れ替え） |
| `veil_kit.sessions` | – | Cookie のセッション（Starlette の `SessionMiddleware` と同じ使い方）と、サーバ側の保存先（メモリ・Redis（`redis` extras）） |
| `veil_kit.cors` | – | `CORSMiddleware(app, allow_origins=..)` と同じ引数。**中身は Rust**: `veil-api` に `app.cors(...)` のネイティブの実装を足し（プリフライトを Rust で返す、ヘッダは組み立て済み）、kit はその薄い包み（Python の層で毎要求ヘッダを作らない） |
| `veil_kit.security` | `jwt`（PyJWT） | `HTTPBasic`・`HTTPBearer`・`APIKeyHeader`・`OAuth2PasswordBearer`・`OAuth2PasswordRequestForm`（FastAPI と同じ名前と使い方、`Depends` で使う）・JWT の検証の補助・`requires_scopes` |
| `veil_kit.redirects` | – | `RedirectResponse`（`veil-api` 側）に、許可したホストだけへ飛ばす `safe_redirect` |
| `veil_kit.staticfiles` | – | `StaticFiles(directory=..)` と同じ引数で **本体の File バックエンドのルートを注入**（F-267 の `App::serve_dir` と同じ。Python の層を通らず sendfile） |
| `veil_kit.graphql` | `graphql`（strawberry） | strawberry のスキーマのマウント（GET・POST・GraphiQL 同梱（オフライン）・SSE と WebSocket（`graphql-transport-ws`）のサブスクリプション）。今の `examples/api/python-graphql` を取り込む |
| `veil_kit.grpc` | `grpc`（grpcio・protobuf） | サーバ: 今の `veil.grpc` の便利関数（`_pb2_grpc` の servicer の登録・リフレクション・health・OpenAPI への記述子（F-265））。**クライアント**: 下の 4. |
| `veil_kit.di` | – | 動的な DI のコンテナ（下の 5.） |
| `veil_kit.tasks` | – | 定期タスク（`@tasks.every(seconds=30)`・cron 式（`croniter` があれば））・常駐タスク・停止時の取消し（F-263 の lifespan の上） |
| `veil_kit.testing` | – | `TestClient` の補助（依存の差し替えの文脈マネージャ） |

### 4. gRPC クライアント（`veil_kit.grpc.Channel`）

- `grpc_tools` が生成するスタブ（`demo_pb2_grpc.DemoStub(channel)`）がそのまま使える **grpcio の `Channel` 互換** の
  `unary_unary`・`unary_stream`・`stream_unary`・`stream_stream` を持つチャネルを、Rust の `veil::api::grpc::Channel`（F-267 の 6.）の上に作る。
  - 同期（ハンドラ用プールのスレッドから呼ぶ。GIL を外して待つ）と `grpc.aio` 互換の非同期（常駐のループから呼ぶ。完了は通知 fd）。
  - メタデータ・期限・圧縮・`grpc.RpcError`（`code()`・`details()`）。TLS は F-259 の信頼アンカー。
  - grpcio を入れずに使える（protobuf のメッセージの `SerializeToString` / `FromString` だけ使う）。grpcio の本物のチャネルも
    そのまま使えることを文書に書く（grpcio は自前のスレッドを持つので、veil のプールと干渉しないことを確かめる）。
- Rust の土台に Python の束縛を足すのは `crates/veil-python/src/`（`grpc_client.rs`）。
- ベンチマーク（HttpArena の unary-grpc）の standard は「その言語の標準の gRPC ライブラリを使い、protobuf の直列化を自前で書かない」。
  依存の無い `veil.grpc.unary_unary`（直列化を利用者が渡す低水準の口）は自前の実装と見なされる恐れがあるので、提出物は
  **grpcio-tools で生成した `_pb2.py`・`_pb2_grpc.py` + `veil.grpc.Server`（`add_XxxServicer_to_server` をそのまま呼ぶ）** を使う（F-269）。

### 5. 動的な DI（`veil_kit.di`）

- `Container` に `singleton` / `scoped`（要求ごと）/ `transient` で提供者を登録し、`Inject(Repo)`（`Depends` と同じ印の仕組み）で受ける。
  提供者の差し替え（`container.override(Repo, FakeRepo)`）、`async` の提供者、終了処理（ジェネレータ）。
- 解決の木は登録時に作る（1. の `Depends` の仕組みの上に作る。要求ごとに型の検索をしない）。スレッド安全（ハンドラ用プールの複数スレッド）。

## 試験

- `crates/veil-python/tests/`: `Depends` の入れ子・キャッシュ・ジェネレータの後始末・差し替え、引数の印の検証と 422、`APIRouter`、
  `BackgroundTasks` の実行順（応答の後）、ASGI のスコープごとの往復。
- 互換の試験: FastAPI の公式チュートリアルの主要な例（Path/Query/Body・Depends・Security の OAuth2 + JWT・BackgroundTasks・
  APIRouter・WebSocket・Request Files・CORS・StaticFiles）を `tests/api/fastapi_compat/` に置き、`from fastapi import` を
  `from veil import`（と `veil_kit`）に置き換えただけで通る割合を記録する（目標は全部。通らないものは対応表の「無いもの」へ）。
- ASGI: FastAPI のアプリ（`fastapi-users` 等のプラグイン付きの例を 1 個）を `veil.asgi.run` で動かし、同じ試験を uvicorn と veil で通す。
  Starlette の試験群の ASGI サーバ互換の部分（`httpx` の ASGI 経由でないもの）を参考に E2E を作る。
- gRPC クライアント: `grpc_tools` の生成スタブで veil のサーバ・grpcio のサーバの両方へ 4 種類の呼び出し、期限切れ、TLS。
- 性能（`tools/api-bench/python`）: ネイティブのルート（`Depends` 無し）が変わらない、`Depends` 2 段・`BackgroundTasks` 付きの費用、
  `veil.asgi.run`（FastAPI）を uvicorn・granian と交互に比べる（目標: uvicorn より速い）。

## 文書

- `crates/veil-python/docs/`（英日）: `dependency-injection.md`（`Depends` を書き直し）、`fastapi.md`（新規。互換表・ASGI・移行の手順）、
  `kit.md`（新規）、`requests-responses.md`（Cookie・リダイレクト）。`veil-api-kit` の README（PyPI 用、英語）。
- `release.yml` に `veil-api-kit` の wheel（純 Python）の公開を足す（`veil-api` と同じ版・同じ draft の手順）。

## 受け入れ条件

- FastAPI の互換の試験が目標どおり通り、FastAPI のアプリが ASGI で動く。kit の全モジュールが E2E で動く。
- ネイティブのルートの性能が変わらない。本体のバイナリが変わらない。文書（英日）・`backlog.md` が更新されている。

## 実施内容（2026-10-26）

### `veil-api`（`crates/veil-python`）: FastAPI の書き方

- `veil/params.py`（新規）: `Query`・`Path`・`Header`（`convert_underscores`）・`Cookie`・`Body`（`embed`）の印（既定値・`Annotated` の
  どちらでも）と検査（`gt`・`ge`・`lt`・`le`・`min_length`・`max_length`・`pattern`。違反は FastAPI と同じ形の 422）、`Depends`・
  `Security`・`SecurityScopes`・`BackgroundTasks`。
- `veil/_deps.py`（新規）: 依存の木。ルートの初回の要求（と `app.dependency_overrides` の変更後。変更回数を数える `Overrides`）に
  署名から 1 回だけ作り、要求ごとは決まった順に呼ぶだけ。同期・`async`・ジェネレータ・非同期ジェネレータ（`yield` の後は応答の前に
  実行、ハンドラの例外は `yield` で投げる）、`use_cache`、失敗した依存の記憶（422 の重複を出さない）、`async def` のルートの同期の
  依存はスレッドプール（`_veil_inline = True` は直接）。依存を使わないルートは今までの束縛のまま（`_build_params` の分岐だけ）。
- `veil/routing.py`（新規）: `APIRouter`（`prefix`・`tags`・`dependencies`・`responses`、WebSocket のルートも）と `include_router`。
- `veil/__init__.py`: ルートの `dependencies`・`response_model`（pydantic の `TypeAdapter`。`response_model_exclude_none`）・
  `responses`・`response_class`・`include_in_schema`・`deprecated`・`description`・`operation_id`、`@app.head`・`@app.options`、
  `Request.cookies`・`http_version`、`Response.set_cookie`・`delete_cookie`・`background`・`status_code`/`content` の別名、
  `HTMLResponse`・`PlainTextResponse`・`RedirectResponse`・`FileResponse`、`app.cors()`・`app.serve_dir()`・`app.mount()`・
  `app.add_middleware()`。OpenAPI に header・cookie の引数、埋め込みの本文、依存の木の引数と `security`（`x-veil-security-schemes` を
  本体の `src/api/openapi.rs` が `components.securitySchemes` へ移す。単体試験 `raw_security_schemes_move_to_components`）。
- `veil/testclient.py`（新規）: サーバ無しでルートを呼ぶ `TestClient`（Starlette と同じ使い方。Rust 側の CORS・静的ファイル・圧縮・
  WebSocket は通らないことを docstring に書いた）。
- `veil/asgi.py`（新規）: `veil.asgi.run(app)` と `app.mount(path, asgi_app)`。`http`（`http.response.trailers`・`http.response.pathsend`）・
  `websocket`（F-263 の受け口から ASGI のイベントへ）・`lifespan`。Rust 側は `register_asgi`（`prefix/{*__veil_asgi}` を全メソッドで登録、
  常駐のループで実行）。同じパスの WebSocket のルートと GET のルートは Rust 側で 1 本にまとめ、`Upgrade` ヘッダで振り分ける
  （GraphQL の HTTP と WebSocket を同じパスに置くため）。
- Rust（`crates/veil-python/src/`）: `NativeRequest.cookies`・`http_version`、`BackgroundTasks` を `Response::after_send`（F-267）で
  応答の後に実行（Python の呼び出しは `spawn_handler` へ回し、ワーカーは GIL を取らない）、`register_cors`（中身は `veil_kit::cors::Cors`。
  プリフライトは Python を呼ばない）、`register_static`（`App::serve_dir`）、`register_route(hidden=)`。
- **gRPC のクライアント**（`src/grpc_client.rs`、新規）: `GrpcChannel`・`GrpcCall`。`veil::api::grpc::Channel`（F-267）を専用の
  クライアントスレッドの `block_on` で動かし、Python からの命令はキュー + `WakeFlag`（そのスレッドで作り、待ち手ごとに分ける）で渡す。
  `recv`・`status` は GIL を外して待ち、`recv_cb`・`status_cb` は handler プールから Python を呼ぶ（asyncio はこれで待つ）。
  受けた状態の trailers を読むために本体に `Status::metadata()` を足した。
- 不具合: `body_constraints`（と印の検査）の `pattern` が全体一致だった [B-171](../bugs/B-171-python-pattern-fullmatch.md)。

### `veil-api-kit`（`crates/veil-python-kit`、純 Python、`import veil_kit`）

- setuptools（`license = "Apache-2.0 OR MIT"` の SPDX）、版は `veil-api` と同じで `veil-api==<版>` に依存。extras は `jwt`・`graphql`・
  `grpc`・`grpcio`・`redis`・`cron`・`all`。
- モジュール: `cookies`（itsdangerous の `TimestampSigner` 互換の署名・鍵の入れ替え）、`sessions`（Starlette の `SessionMiddleware` と
  同じ形式の Cookie、`MemoryStore`・`RedisStore`）、`cors`（`CORSMiddleware` → `app.cors`）、`security`（FastAPI と同じ名前の方式・
  `JWT`（PyJWT、JWKS）・`requires_scopes`）、`redirects`（`safe_redirect`）、`staticfiles`（`StaticFiles` → 本体の File バックエンド）、
  `graphql`（strawberry、HTTP・SSE・WebSocket（`graphql-transport-ws`・`graphql-ws`）・GraphiQL）、`grpc`（grpcio の `Channel` 互換・
  `grpc.aio` 風、`RpcError` は grpcio があれば `grpc.RpcError`、`ssl_channel_credentials`）、`di`（`Container`・`Inject`、
  singleton・scoped・transient、`override`、遅い登録）、`tasks`（`every`・`cron`（組み込みの解析、croniter があればそれ）・`background`）、
  `testing`。単体試験 `tests/test_kit.py`（拡張無しの python3 でも通る）。
- 公開: `release.yml` に `kit` の job（`python -m build` で sdist と `py3-none-any` の wheel、版・依存の版が veil-api と同じ PEP 440 の綴り
  であることと 2 ファイルであることを確かめ、wheel を入れて試験）を足し、`pypi` の job が veil-api と一緒に TestPyPI（rc）・PyPI（正式）へ
  出す。公開後の確認は veil-api と veil-api-kit の両方。`tools/release/bump-version.sh` は kit の版・依存の版・`__version__` を PEP 440 の
  綴りで上げる（`0.10.0-rc.1` → `0.10.0rc1`、maturin と同じ）。`tools/release/check-docs.sh` は kit の README・docstring も見る。

### サンプル・試験・文書

- `examples/api/python-kit/`（`app.py`: Depends・Inject・APIRouter・BackgroundTasks・OAuth2 + JWT・スコープ・response_model・
  セッション・safe_redirect・CORS・StaticFiles・定期タスク・strawberry・`/legacy` に載せた FastAPI（HTTP・WebSocket）。
  `grpc_client.py`: grpc_tools の生成スタブで h2c・TLS・asyncio の 4 種類の呼び出し・エラー・期限）。
- `crates/veil-python/tests/test_fastapi_compat.py`（18 本）。`tests/api/library_e2e.sh python` に `run_python_kit`（上の全機能を curl で、
  WebSocket は `ws_check.py kit`、gRPC クライアントは `examples/api/python-grpc` を相手に h2c・TLS（CA と CA で署名した証明書）・asyncio）。
- 文書: `crates/veil-python/docs/` の `dependency-injection.md`（書き直し）・`fastapi.md`・`kit.md`（新規）・`requests-responses.md`
  （Cookie・リダイレクト・応答の型）・`quickstart.md`・`index.md`・`conf.py`（autoapi に kit）、`docs/guide/library.md`・`library-kit.md`（英日）。
  `crates/veil-python/docs/locale` には日本語の訳が無い（今までどおり gettext で後から作る）。
- 性能（`tools/api-bench/python`、改修前の HEAD を `BENCH_BASE_VENV` で交互に 3 回、wrk）: 依存を使わないルートは変わらない
  （plaintext offload=True 12,884 / 12,966 rps、json 12,975 / 13,173、CPU µs/req は 153〜155 で同じ。差は回ごとのばらつきの範囲）。
  記録は `docs/artifacts/f268/bench.tsv`。

### 設計からの変更・残したもの

- ASGI のアプリはネイティブの `async def` と同じ常駐のループで動く（専用のループ `event_loops=1` は作らなかった）。`pathsend` は
  Python でファイルを読む（sendfile にしていない）。要求の本文は呼ぶ前に読み切る。uvicorn・granian との ASGI の性能比較はしていない。
- `TestClient` はループバックで起動せずにルートを直接呼ぶ（Starlette と同じ）。FastAPI の公式チュートリアルを置き換えて通す
  `tests/api/fastapi_compat/` は作らず、互換の試験は `test_fastapi_compat.py` と `fastapi.md` の対応表（「無いもの」を含む）にした。
- `veil_kit.graphql` の GraphiQL は strawberry のページ（CDN から読む）。`@app.exception_handler`・Starlette のミドルウェアは
  ネイティブのルートには無い（ASGI で載せれば動く）。`allow_origin_regex` は無い。
- gRPC のサーバ側の補助は `veil.grpc` の再公開（F-265 の OpenAPI への記述子は F-265 で）。
