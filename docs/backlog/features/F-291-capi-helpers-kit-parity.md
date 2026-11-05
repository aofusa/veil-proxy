# F-291: C 向けの kit `veil-capi-helpers` を作り、C からも Rust・Python と同じ機能を使えるようにする

**状態: 未着手（2026-11-05 起票。調査・設計のみ）**

調査の全文（機能 × 層の対応表・FFI の ABI 案・モジュールごとの API 案）は `docs/artifacts/v0110/survey-03-capi-helpers.md`（git 管理外）。

## 今の状態

- 層の作り:
  - Rust: 本体 `src/api/`（feature `handler`・`api-core`・`api`。default・`full*` に入らない）の公開フック（`App::with(Plugin)`・`wrap_on`・
    `Request::extensions`・`Response::after_send`・`serve_dir`・`on_worker_start`・`veil::api::client`）の上に、別クレート `crates/veil-kit`
    （crates.io の `veil-proxy-kit`）が乗る。暗号は aws-lc-rs（OpenBSD・NetBSD は ring）。
  - Python: コア `crates/veil-python`（`veil-api`）が FastAPI 互換の束縛と、ネイティブが要る部分（CORS＝`veil_kit::cors`、静的配信、gRPC チャネル）を
    持ち、kit `crates/veil-python-kit`（`veil-api-kit`）は純 Python。
  - C: `crates/veil-capi`（`libveil.{so,a}`・`veil.dll` + `include/veil.h`、63 関数）。ルート・ストリーミング・gRPC サーバ・L4・アップグレード・
    WebSocket・multipart・圧縮・複数リスナーは Rust と同等。**kit に当たるものが無い**。
- C で使えない機能（コアの FFI が足りないもの）: ミドルウェア、要求ごとの値（`extensions`）、応答後の処理（`after_send`）、起動・停止・ワーカー開始の
  フック、応答ヘッダの追加（`veil_response_set_header` は置換だけで `Set-Cookie` を複数付けられない）、要求ヘッダの列挙・同名の 2 個目以降、
  TLS か・HTTP の版、CORS・静的ディレクトリ（`config.toml` 経由のみ）、HTTP クライアント・gRPC クライアント、WebSocket のサブプロトコル選択
  （`graphql-transport-ws` に要る）、gRPC reflection（Python にも無い）、JSON 本文の検証と 422、プールのスレッドを握らない非同期完了、試験用の呼び出し。
- kit の機能（C に無い）: 署名・暗号化 cookie と鍵、セッション、Basic・Bearer・API キー・JWT（検証・発行・JWKS）・OAuth2 パスワードフロー、
  authenticate・require のガード、リダイレクト（オープンリダイレクト対策）、1 ファイル応答、DI、タスク（every・cron・spawn）、BackgroundTasks、
  HTTP クライアントの JSON・再試行、gRPC の Balanced、GraphQL のトランスポート。

## 改修内容（設計）

Python と同じ 2 層にする。**プロキシ本体には触れない**（追加は `crates/` と、共有する場合だけ `src/api/` の feature `handler`/`grpc-handler` の中）。

### A. コア `veil-capi` に足す FFI（kit の土台）

規約は今の `veil.h` に合わせる（`int` の戻り値 + `veil_last_error()`、返す文字列は NUL 終端でなく `size_t *len`、構造体は先頭に `size_t size` と
`*_INIT` マクロ。B-209）。**C のコールバックはワーカーで呼ばない**（INLINE ルートを除く今の規則）。C のミドルウェアはルートのハンドラと同じ
`call_blocking` の閉包の中で順に呼び、要求あたりのスレッドの切り替えは 1 回のまま。スレッドを越える完了は `runtime::offload::await_completion`。

| 追加 | 形（抜粋） |
|---|---|
| 応答ヘッダの追加 | `veil_response_add_header` |
| ヘッダの列挙 | `veil_request_header_count`・`veil_request_header_at`・`veil_request_header_next`（同名の 2 個目以降。HTTP/2 の分割 `Cookie`） |
| TLS・版 | `veil_request_is_tls`・`veil_request_version` |
| 要求ごとの値 | `veil_request_set_local(req, key, val, free)`・`veil_request_get_local`（`extensions` に 1 個、使ったときだけ確保） |
| ミドルウェア | `veil_register_middleware(cfg, prefix, before, after, ud)`（`before` は 0=続ける・1=打ち切り・負=500。順序は Rust の `wrap_on` と同じ規則） |
| 応答後の処理 | `veil_response_after_send(resp, fn, ud)`（ハンドラ用プールで実行） |
| ライフサイクル | `veil_config_on_startup`・`on_shutdown`・`on_worker_start` |
| CORS・静的配信 | `veil_config_cors(cfg, const veil_cors_t*)`（`veil_kit::cors` に委譲。Python の `register_cors` と同じ）、`veil_config_serve_dir`（`App::serve_dir`） |
| WebSocket | `veil_ws_accept_ex`（サブプロトコル・Origin・最大メッセージ）・`veil_ws_protocol` |
| 遅延応答 | `veil_response_defer` → 任意のスレッドで `veil_deferred_*` → `veil_deferred_complete`（プールのスレッドを即返す） |
| JSON 本文の検証 | `veil_register_route_ex2`（`body_schema_json`、登録時にコンパイルし要求ごとは Rust で検査して 422。Python の `body_constraints` と共用を検討） |
| HTTP クライアント・gRPC チャネル | `veil_client_*`（同期・非同期）、`veil_grpc_channel_*`。専用のクライアントスレッドで動かし、Python の `grpc_client.rs` と共通化する。INLINE ルート・ワーカーから同期版を呼んだら `VEIL_ERR_STATE` |
| gRPC reflection | `veil_grpc_reflection(cfg, fds, len)`（Python にも足すと 3 言語が揃う） |
| 試験用 | `veil_test_*`（ソケットを開かずディスパッチ）か、初版は helpers の「ループバックで起動して当てる」補助 |

### B. `crates/veil-capi-helpers`（新規、kit 本体）

- **Rust で `veil-kit` を再利用し C ABI を出す**（C で暗号・JWT を書き直さず、OpenSSL に依存しない。Rust kit と cookie・JWT の形式と検査が同じになる）。
  記号の接頭辞 `veil_kit_`、ヘッダ `veil_kit.h`、ライブラリ `libveil_kit.{so,a}`・`veil_kit.dll`、pkg-config `veil-kit`。
- モジュール: cookie（署名・暗号化・鍵の回転）、redirect（safe）、send_file、auth（Basic・Bearer・API キー・principal・authenticate・require_scope）、
  jwt（HMAC・PEM・JWKS、発行、Bearer の既製ミドルウェア）、oauth2、session（既定はメモリストア・暗号化 cookie）、di（文字列キー、singleton・scoped・transient）、
  tasks（every・cron・spawn・BackgroundTasks）、client（JSON・retry）、grpc（Balanced）、graphql（トランスポートのみ、実行器は C のコールバック）、
  openapi の補助。header-only の補助（JSON 書き出し器・ルート表マクロ・`VEIL_KIT_JSON_ERROR`）。
- クレート構成: `veil-capi` の中身を rlib の `crates/veil-capi-core`（lib 名 `veil_capi`）へ移し、`veil-capi`（成果物、lib 名 `veil`）と
  `veil-capi-helpers`（成果物、lib 名 `veil_kit`）が再公開する。`libveil` の名前と中身は変えない。`libveil_kit` は `libveil` の上位集合で、
  **C のプログラムはどちらか一方だけをリンクする**（2 つ入れるとランタイムとグローバルが 2 コピーになる）。pkg-config の `veil-kit` は `Requires: veil` にしない。

### C. 配布・試験・文書

- `build-lib.sh`・`assemble-lib-zip.sh`（`veil_kit.h`・`libveil_kit`・`veil-kit.pc`、新たに CMake の config `veil::veil`・`veil::kit`）・`docker/Dockerfile.lib`・
  `verify-tag.sh`・`bindings.yml`（`library_e2e.sh c-kit` を Linux・macOS・Windows）。C の成果物を GitHub Release に載せるかは別に判断。
- 試験: `cargo test -p veil-capi -p veil-capi-helpers`、`nm -D` で全記号の公開、`examples/api/c-kit` + `library_e2e.sh c-kit`（CORS・静的・複数 Set-Cookie・
  署名 cookie の改ざん拒否・**Rust kit が発行した cookie・JWT・セッションを C が受け付ける相互運用**・認証の 401/403・BackgroundTasks・cron・DI・GraphQL・
  クライアント・422・遅延応答、epoll ビルドでも）、ヘッダだけを C99 と C++ でコンパイル、`nonregression` にプロキシのバイナリへ `veil_kit_`・`veil_client_` 等が
  入っていないことを足す。
- 文書: `library-kit.md`・`library.md`（英日）に C の節、`crates/veil-capi/README.md`・`crates/veil-capi-helpers/README.md`。設定キーは増やさない。

## 分割（実装の順）

1. コア FFI 第 1 弾: add_header・ヘッダ列挙・is_tls/version・locals・middleware・after_send・lifecycle・serve_dir・cors・ws_accept_ex・reflection
2. クレート分割（veil-capi-core）と記号の公開の回帰試験（**最初に実証が要る**: 依存 rlib の `#[no_mangle]` を cdylib が fat LTO でも公開するか）
3. veil-capi-helpers 第 1 弾: cookie・redirect・send_file・auth・jwt・session・JSON・ルート表・openapi + `examples/api/c-kit` + E2E
4. コア FFI 第 2 弾: HTTP クライアント・gRPC チャネル（Python と共通化）・遅延応答・JSON の検証・試験用呼び出し
5. veil-capi-helpers 第 2 弾: di・tasks・client の JSON/retry・grpc Balanced・graphql
6. 配布（zip・pkg-config・CMake・bindings.yml）。文書は各段に同梱

## 受け入れ条件

- 上の対応表で「C の不足」の行が全部埋まる（Rust・Python にあって C に無い機能が無い。言語に不要なもの＝tower・ASGI・tokio 互換は除く）。
- `tests/api/library_e2e.sh nonregression` が通り、`cargo build --features full` のバイナリと性能が変わらない。
- `library_e2e.sh c-kit` が Linux（io_uring・epoll）・macOS・Windows で通る。

## 未決事項・リスク

- cookie・セッションの形式は **Rust kit と同じ**（2026-11-05 に利用者が決定。Python kit の itsdangerous 形式とは非互換）。
- 外部セッションストアを C のコールバックで受けると、Rust kit の `SessionStore`（同期・ワーカー上）と噛み合わない。ハンドラ用プールで呼ぶ版を veil-kit に足すか、
  C はメモリストア + 暗号化 cookie に限るか。
- GraphQL の GET の mutation 拒否に操作種別の判定が要る（コールバックで問う案）。
- libveil と libveil_kit の二重リンクの検出は文書と pkg-config で足りるか。

## 関連

- [F-188](F-188-api-library-core.md)（API ライブラリ）、[F-262](F-262-handler-websocket-rust.md)（WebSocket）、[F-210](F-210-capi-python-feature-parity.md)（C・Python の機能の同等化）
