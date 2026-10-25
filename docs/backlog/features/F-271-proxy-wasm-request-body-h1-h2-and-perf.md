# F-271: Proxy-Wasm の要求本文のフェーズを HTTP/1.1・HTTP/2 にも入れ、要求ごとのインスタンスの費用を減らす

**状態: 完了（第 1 段。wt/core → feat/v0100、2026-10-25。2026-10-25 起票・設計。親は [F-258](F-258-v0100-plan.md)。HttpArena の veil 単体（infrastructure）の前提）**

## 背景（調査で確認）

- `on_http_request_body` は **HTTP/3 にだけ** 繋がっている（`src/http3_server/request_task.rs` 913〜945 行、F-132 の「H3 only」）。
  HTTP/1.1（chunked を含む）・HTTP/2 には呼び出しが無い。HttpArena の veil 単体（infrastructure）は `/baseline11` の POST の本文の整数を
  サーバに読み込んだモジュールで足す必要があり、今は答えられない。
- コールバックのたびに新しいインスタンスを作り、`_start`・`proxy_on_vm_start`・`proxy_on_configure` を毎回実行する
  （`src/wasm/engine.rs` 952〜1008 行）。プラグインの `configuration`（データセットを渡す唯一の手段）も毎回複製・解析される。
- `type = "Respond"` のルートは Wasm のモジュールを持てない（`backend.rs` 131・151 行）ので、「フィルタが全部答える」ルートに
  File や Handler を間に合わせで置くしかない。
- WASI に `path_open` が無い（`src/wasm/host/wasi.rs`）ので、起動時にファイルを読めない。

## 制約

- 本体の機能（プロキシの Wasm）なので、**Wasm のモジュールを設定していないルートの経路は 1 行も変えない**（`modules` が空なら今と同じ）。
- ホットパス: 本文フェーズは「本文の読み取りを許したモジュールがあるルート」だけ。本文を溜めるのは
  `allow_request_body_read` のモジュールがあるときだけで、上限は既存の `max_body_size` 系。
- 新しい io_uring のオペコードは増やさない。Wasm の実行は今と同じく協調的に yield する。

## 設計

### 1. 要求本文のフェーズ（HTTP/1.1・HTTP/2）

- HTTP/1.1: ヘッダのフェーズの後、本文のあるルートで `allow_request_body_read` のモジュールがあれば、本文（Content-Length・chunked）を
  溜めて `on_http_request_body(len, end_of_stream=true)` を呼ぶ。`send_local_response` されたら上流へ送らず応答する。
  本文を書き換えたら（`set_buffer`）新しい本文で上流へ送る（`Content-Length` を直す、chunked は Content-Length に変える）。
  - 第 1 段は「全部溜めてから 1 回」（end_of_stream=true のみ）。断片ごとの呼び出し（ストリーミング・`Pause` で溜める）は第 2 段。
- HTTP/2: ストリームの本文を溜めて同じく 1 回。HTTP/3 の今の実装（`apply_wasm_request_body`）と同じ関数を共有する
  （`src/wasm/http_executor.rs`。F-234 の共通化の方針）。
- 本文フェーズの `Pause` は今の HTTP/3 と同じく警告して続行（第 2 段で対応）。

### 2. インスタンスの費用

- **VM の文脈を作り直さない**: ワーカーごと・モジュールごとに「VM と root context を作って `_start`・`on_vm_start`・`on_configure` を
  済ませたインスタンス」を 1 個持ち、要求ごとは **http context の作成だけ**（`proxy_on_context_create`）にする（Envoy と同じ寿命）。
  - 要求の間で線形メモリが残ることになるので、モジュールが要求をまたいで状態を持てる（Proxy-Wasm の仕様どおり）。既存の
    「毎回きれいなインスタンス」に依存した安全の前提（`max_memory_size` の上限・トラップ時の扱い）を見直す: トラップしたインスタンスは捨てて作り直す、
    メモリの上限を超えたら作り直す。
  - 設定（`[[wasm.modules]] instance_reuse = true`、既定は **今の挙動（毎回作る）** にし、計測と互換を確かめてから既定を変えるかを決める）。
- `configuration` の解析は root context の `on_configure` で 1 回だけになる（上の結果）。

### 3. `Respond` のルートにモジュールを付ける

- `type = "Respond"` も `modules` を持てるようにし、フィルタが `send_local_response` しなければ設定の固定応答を返す。
  上流への接続を一切試みないルートになる（infrastructure の提出物の「フィルタが答えるルート」に使う）。

### 4. 起動時のデータ

- WASI の `path_open` は足さない（サンドボックスの面を広げない）。データは `configuration` に表で渡すか、`[[wasm.modules]] data_files = { dataset = "/data/dataset.json" }`
  （新設）で **起動時に veil が読み**、`proxy_get_property(["plugin_data", "dataset"])` で読めるようにする（読むのは設定ロード時。
  ファイルのパスは設定に書いたものだけ）。

## 試験

- `tests/proxy_wasm_conformance.rs`: 本文フェーズを HTTP/1.1（Content-Length・chunked・分割到着）・HTTP/2・HTTP/3 で同じ結果、
  `send_local_response`・本文の書き換え、上限超え。インスタンスの再利用でトラップ後に作り直す、要求をまたぐ状態。
- E2E（`tests/e2e_setup.sh`、io_uring と epoll）: HttpArena の `/baseline11`（GET・POST CL・POST chunked）を Wasm で答える例
  （`examples/wasm/` にサンプルを置く）、`Respond` + モジュール、`data_files`。
- 非退行: Wasm の無いルートの交互 A/B（`tools/perf` の suite_ab の代表構成）。Wasm のある構成（`tools/perf` の wasm 構成）で再利用の効果を記録。
- ファジング: 本文フェーズのホスト関数（`fuzz/`）。

## 文書

- `docs/guide/wasm.md`（英日）: フェーズ表（HTTP/1.1・2・3）、`instance_reuse`、`Respond` + モジュール、`data_files`。`examples/config.toml`。

## 受け入れ条件

- 3 プロトコルで本文フェーズが動き、Wasm を設定していないルートの性能が変わらない。文書（英日）・`backlog.md` が更新されている。

## 実施内容（2026-10-25）

- **要求本文のフェーズ（HTTP/1.1・HTTP/2）**: 3 経路とも `wasm::http_executor::apply_wasm_request_body` を共有し、
  `RequestMeta`（メソッド・パス・ヘッダ・送信元）を渡して本文のコールバックからもプロパティ `request.path` 等が読めるようにした。
  - HTTP/1.1（`src/proxy/h1.rs`）: ヘッダのフェーズの後、本文があり `FilterEngine::wants_request_body`（`allow_request_body_read` の
    モジュールがある）なら、本文を上限付きで読み切って（Content-Length・chunked。ハンドラと同じ `read_full_request_body`。上限は
    `max_request_body_size`・`max_chunked_body_size`、超えたら 413）1 回呼ぶ。`send_local_response` ならその応答、本文を書き換えたら
    `Transfer-Encoding`・`Content-Length` を外して新しい `Content-Length` でバックエンドへ渡す。
  - HTTP/2（`src/proxy/h2.rs` の `h2_dispatch`）: Wasm のルートはバッファ経路なので `ctx.body` に対して 1 回呼ぶ。書き換えたら
    `content-length` を合わせた `H2RequestCtx` に差し替える。
  - HTTP/1.1 のフィルタのローカル応答（ヘッダ・本文のフェーズ）は、本文を読み切れていれば接続を続ける（以前は毎回閉じていた）。
    ヘッダのフェーズの `end_of_stream` を「本文が無い」で決めるよう直した（以前は「本文の先頭がまだ届いていない」でも真だった）。
- **インスタンスの使い回し**（`[[wasm.modules]] instance_reuse = true`、既定 false）: `src/wasm/instance_pool.rs`。モジュールごとに
  スレッドで決めた区画（8 区画 × 2 個）に、`_start`・`proxy_on_vm_start`・`proxy_on_configure` を済ませたインスタンスを持つ。
  コールバックは `FilterEngine::open_session`（プールから取って `proxy_on_context_create` だけ。無ければ今までのライフサイクル）と
  `close_session`（成功なら `proxy_on_delete` してプールへ戻す、トラップ・エラーなら捨てる）で囲む。ルートの `module_configuration`
  ごとに別のインスタンス（設定の `Arc` で区別）。使うのは要求・応答のヘッダと本文と `on_log`（要求ごとに走るもの）。
  トレイラ・`on_http_call_response`・gRPC・tick・L4 のコールバックは今までどおり毎回新しいインスタンス。
- **`Respond` + モジュール**: `FixedResponse::modules`（`with_modules`）をロード時に入れ、`Backend::modules_arc`・`modules` が返す。
  3 経路とも Wasm のフェーズは固定応答より前なので、フィルタが答えなければ固定応答になる。CORS の写し（F-273）も引き継ぐ。
- **`data_files`**: `[[wasm.modules]] data_files = { 名前 = "パス" }`。モジュールのロード（起動・リロード）で読み、
  `HttpContext::plugin_data` に `Arc` で渡してプロパティ `plugin_data.<名前>` で返す。読めなければ設定のエラー。
- **サンプル**: `examples/wasm-filters/baseline-filter`（HttpArena の `/baseline11` 相当・`/dataset`・`/rewrite`・`/trap`）。
  ビルド済みを `tests/fixtures/wasm/baseline_filter.wasm` に置いた。
- **試験**: 単体 `wasm::tests::instance_reuse_tests`（5 件: 使い回しで `on_configure` 1 回・context が増える・トラップで捨てる、
  使い回さないと毎回新しい、`data_files`、無いファイルはロードのエラー、本文のフェーズの応答と書き換え）。E2E
  `tests/standalone_e2e.rs` の `f271_wasm_respond_body_phase_reuse_and_data_files`（HTTP/1.1・h2c・TLS の HTTP/2・HTTP/3 で
  GET・POST・`/dataset`・`/rewrite`（上流へ書き換えた本文）、HTTP/1.1 の chunked と keep-alive、`instance_reuse` の真偽、トラップ）。

- **AOT サイドカーの書き込みの競合**: `.cwasm` の一時ファイルの名前を書き手ごと（pid と連番）にした。同じ名前だと、別の書き手が
  rename 済みの inode を切り詰めて書き直し、それを mmap している側が SIGBUS になる（同じモジュールを並行してロードする単体試験で起きた）。

### 決めたこと

- `instance_reuse` の既定は false（今の挙動）。既定を変えるかは F-276 の計測の後に決める。
- 使い回すインスタンスでも http context は **コールバック（フェーズ）ごとに 1 つ** 作る（今までの「コールバックごとに新しい
  インスタンス」と同じ粒度。要求のヘッダのフェーズと本文のフェーズで同じ context を共有する Envoy の形にはしていない）。
  1 要求で複数のフェーズにまたがる状態は、フィルタの root context・グローバル変数に置くことになる。
- メモリの上限は今までどおりプーリングアロケータの `max_memory_size` で効く（超えた `memory.grow` は失敗し、トラップなら捨てる）。
  使い回しのために持つインスタンスはワーカーの区画ごとに 2 個まで（`total_memories` を使い切らない）。
- HTTP/1.1 で本文のフェーズのために本文を読み切るとき、同じ読み取りに入っていた次の要求（本文のある要求のパイプライン）は
  捨てる（ハンドラの経路と同じ。本文の後ろのパイプラインはまれ）。
- 本文のフェーズの `Pause` は今の HTTP/3 と同じく警告して元の本文で続ける（第 2 段）。
- `on_http_response_body` の HTTP/1.1・HTTP/2 は本チケットの範囲外（文書のフェーズ表に「–」と書いた）。

### 残したこと

- E2E で、同じ path の header 条件のルートがルートのキャッシュで飛ばされる不具合を見つけた（[B-170](../bugs/B-170-route-cache-skips-earlier-dynamic-route.md)。
  本チケットでは直さず、試験は path で振り分けた）。

- 本文の断片ごとの呼び出し（ストリーミング・`Pause` で溜める）と、1 要求の全フェーズで 1 つの http context を使う形（第 2 段）。
- `tests/e2e_setup.sh` の共有 E2E への組み込み（standalone の E2E で 3 プロトコル・io_uring と epoll を回した）と、本文フェーズの
  ホスト関数のファジング。
- 性能（Wasm の無いルートの交互 A/B、`instance_reuse` の効果）は F-276 の計測に回す。Wasm の無いルートに増えたのは
  `modules_to_apply.is_empty()` の分岐だけ。
