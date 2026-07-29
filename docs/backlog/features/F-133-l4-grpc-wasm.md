# F-133: L4 / gRPC への WASM (Proxy-Wasm) 適用

## 目的

`docs/artifacts/f132_wasm_design.md` 3節の設計に基づき、次の2点を実装する。

1. **gRPC**: HTTP/2 上の gRPC は既存 h2 WASM 配線（ヘッダ・ボディ）がそのまま効くが、
   トレイラー（`grpc-status`/`grpc-message`）は未配線だった。また `proxy_grpc_*` ホスト関数
   （`src/wasm/host/grpc.rs`）と `src/wasm/grpc_integration.rs` の実際の呼び出し元があるかを確認する。
2. **L4**: Proxy-Wasm の network filter ABI（`proxy_on_new_connection`/`proxy_on_downstream_data`/
   `proxy_on_upstream_data`/`proxy_on_downstream_close`/`proxy_on_upstream_close`）を
   `src/wasm/engine.rs` に実装し、`src/l4/proxy.rs` の TCP パススルー転送ループから呼ぶ。

## 現状の事実（対応前の調査結果）

- `FilterEngine::on_request_trailers_with_modules`/`on_response_trailers_with_modules` は
  実装済みだったが、**`src/wasm/` の外から呼び出しゼロ**（gRPC トレイラー送出経路
  `src/proxy.rs` の `h2_proxy_h2c` / `drive_h2_streams` のいずれからも未配線）。
- `src/wasm/host/grpc.rs` の `proxy_grpc_call`/`proxy_grpc_stream`/`proxy_grpc_send` は
  呼び出しを `HttpContext::pending_grpc_calls`/`pending_grpc_streams` に**登録するのみ**で、
  実際に外部 gRPC サービスへ発呼する実行系（ネットワーク I/O）が存在しない
  （`take_pending_grpc_calls()` を消費する箇所が皆無）。
  `src/wasm/grpc_integration.rs` の `process_grpc_response`/`on_grpc_*` は
  受信コールバックを WASM へ配送するための関数だが、これも呼び出し元がゼロ
  （＝ WASM モジュールが自発的に発呼する gRPC クライアントとしての機能は未実装のまま）。
  **本チケットではこの発呼実行系の実装は対象外**（別チケット化を推奨、下記「既知の限界」参照）。
- L4 には Proxy-Wasm ホスト関数の配線自体が皆無だった
  （`BufferType::DownstreamData=2`/`UpstreamData=3` は定数定義のみで「not supported」とコメントされていた）。

## 改修内容

### gRPC トレイラー

- `src/proxy.rs`:
  - `h2_dispatch` にリクエストトレイラー WASM フィルタを追加
    （`ctx.trailers` が空なら即 return。gRPC リクエストの場合のみ非空になる）。
    `LocalResponse` はそのまま応答して打ち切り、`Pause` は未対応として警告ログのみ。
  - `apply_h2_wasm_response_trailers` を追加し、`h2_proxy_h2c` の
    `has_trailers` 分岐（gRPC over H2C バックエンドからのレスポンス）で
    `H2RespMsg::Trailers` を送出する前に適用。`grpc-status` メトリクス記録は
    フィルタ後の値を使う。`Pause`/`LocalResponse` は HEADERS/DATA 送出済みのため
    適用不能として元のトレイラーを保持し警告ログを出す。
- リクエストトレイラーの経路を通すため `src/http2/stream.rs`（`Stream::request_trailers`）・
  `src/http2/connection.rs`（`decode_and_set_headers` のトレイラー保存・
  `take_request_parts`/`H2RequestParts::trailers`）・`src/proxy.rs`（`H2RequestCtx::trailers`）
  を追加した。従来は「トレイラーは検証のみで破棄」だったため、この保存自体が新規動作
  （`#[cfg(feature = "wasm")]` で無効時は完全に元の動作＝破棄のまま）。
- HTTP/3 (`src/http3_server.rs`) の gRPC トレイラー経路は対象外
  （並行作業でロックされているファイルのため本チケットでは触れていない）。

### L4 network filter

- `src/wasm/engine.rs` に追加:
  - `on_new_connection_with_modules` / `on_downstream_data_with_modules` /
    `on_upstream_data_with_modules` / `on_downstream_close_with_modules` /
    `on_upstream_close_with_modules`。
  - 内部で `proxy_on_context_create`（root の子として stream context を作成）→
    `proxy_on_new_connection`/`proxy_on_downstream_data`/`proxy_on_upstream_data`/
    `proxy_on_downstream_close`/`proxy_on_upstream_close` を呼ぶ。
  - `proxy_action_t` の戻り値（`Continue=0`/`Pause=1`）と、`proxy_close_stream` 呼び出し
    （`HttpContext::close_requested`、新規フィールド）の両方を見て
    `NetworkFilterResult::{Continue, Pause, Close}` を返す。
  - `NetworkAction`（`Continue`/`Close`。`on_new_connection` 用）、
    `NetworkFilterResult`（データフィルタ用）を新規公開型として追加。
- `src/wasm/context.rs`: `HttpContext` に `downstream_data`/`upstream_data`
  （`BodyBuffer`、CoW）、`*_modified` フラグ、`close_requested` を追加。
- `src/wasm/host/buffers.rs`: `BufferType::DownstreamData=2`/`UpstreamData=3` を
  `get_buffer`/`check_read_capability`/`check_write_capability`/`set_buffer` に配線
  （従来「not supported」だった値を実装）。
- `src/wasm/host/stream.rs`: `proxy_close_stream` を no-op から
  `HttpContext::close_requested = true` を立てる実装に変更
  （Proxy-Wasm ABI の `Action` には Close が無く、host 関数呼び出しでしか検知できないため）。
- `src/wasm/capabilities.rs`: `allow_downstream_data_read/write`・
  `allow_upstream_data_read/write` を追加（デフォルト false、`CapabilityPreset::Extended` では true）。
- `src/config.rs`: `L4ListenerConfig::wasm_modules: Vec<String>`（`#[cfg(feature = "wasm")]`、
  デフォルト空）を追加。`validate_config` で参照モジュール名の存在チェックを追加。
- `src/l4/proxy.rs`:
  - `bidirectional_forward` に `wasm_modules: &[String]` 引数を追加。
    **ホットパス絶対規則**: 関数先頭で `wasm_modules.is_empty()` を1回だけ判定する
    分岐が唯一の追加コストで、空（デフォルト）なら従来の
    `forward_direction_splice`/`forward_direction` へそのままフォールスルーする。
  - モジュールが設定されている場合のみ `forward_direction_wasm`
    （新規、`#[cfg(feature = "wasm")]`）を使う。splice を使わず、読み取ったバイト列を
    都度 `Bytes` へコピーして `FilterEngine` に渡し、返ってきたデータを書き込む
    （design doc 通り「WASM 有効時はユーザー空間バッファを通す」を実装。
    この経路自体が意図的なコスト増であり、既存の splice ゼロコピー経路には一切影響しない）。
  - 接続確立時に `on_new_connection_with_modules` を1回、転送ループ終了後に
    `on_downstream_close_with_modules`/`on_upstream_close_with_modules` を1回ずつ呼ぶ。
  - **TLS terminate 経路（`bidirectional_forward_tls_terminate`）にも同様に配線した**
    （レビュー指摘で追加）。この経路はもともと splice を使わない単一ループの
    ユーザー空間ポーリング実装（TLS 復号後の平文を read/write で中継）のため、
    「WASM 有効時のみユーザー空間バッファ経由」という切替そのものは不要で、
    同じループへ `on_downstream_data_with_modules`/`on_upstream_data_with_modules`
    呼び出しを差し込むだけで済む。`wasm_modules.is_empty()` を関数先頭で1回だけ判定し、
    空（デフォルト）なら以降のコードパスは無変更（分岐1つのみが追加コスト）。
    途中の `return`（書き込みエラー時の早期終了）は `'outer` ラベル付きループの
    `break 'outer` に置き換え、close コールバックがすべての終了経路で呼ばれるようにした。

### 設定・ドキュメント

- `examples/config.toml`: `[[l4]]` の例に `wasm_modules` を追加。
- `README.md`/`docs/readme/README.ja.md`: L4 設定リファレンス表、WASM Extension の
  Features 一覧、Capability 一覧に上記を追記。

## テスト

- 単体（`src/wasm/tests.rs::f133_network_filter_tests`）: 新規ゲストモジュール
  `tests/fixtures/wasm/network_filter.wasm`
  （`examples/wasm-filters/network-filter/`、`StreamContext` 実装。
  downstream の `foo`→`bar`、upstream の `bar`→`baz` を同じ長さで置換し、
  downstream データに `CLOSE_ME` を含むと `proxy_close_stream` を呼ぶ）を使い、
  ディスパッチ（空モジュールリストのゼロコピーパススルー）・データ書き換え・
  `Close` アクション・`on_new_connection`/`on_downstream_close`/`on_upstream_close`
  がパニックしないことを検証。
- E2E（`tests/e2e_tests.rs`/`tests/e2e_setup.sh`、レビュー指摘で追加）:
  - `tests/e2e_setup.sh`: `network_filter.wasm`/`grpc_trailer_filter.wasm` を
    `copy_wasm_module` に追加、`[[wasm.modules]]` に両モジュールを登録
    （capabilities は downstream/upstream data の read/write、request/response
    headers の read/write）。**既存リスナー・既存ルートは一切変更しない**:
    新規ポート `PROXY_L4_WASM_PORT=8448` で `[[l4]] name = "l4-wasm-network-filter"`
    （`tls = "none"`、backend は既存の HTTP body-echo バックエンド
    `BACKEND_ECHO_PORT`、`wasm_modules = ["network_filter"]`）を追加し、
    gRPC 側は既存の `/grpc.test.v1.TestService/*`（host=localhost/127.0.0.1、
    `header_filter` 適用）とは別に、新しい host 条件
    （`grpc-wasm-trailer.test`）で `grpc_trailer_filter` 適用ルートを追加した。
    `network_filter`/`grpc_trailer_filter` 関連の `[[l4]]`/`[[route]]`/
    `[[wasm.modules]]` は他の config_type（wasm/default 以外）では生成されない
    ため、`config_type` ごとの起動を壊さない
    （`./tests/e2e_setup.sh start` → `health` → `stop` で確認済み。
    新ポートへの生 TCP 疎通も手動で確認: `foo-data-foo` → `baz-data-baz`、
    `CLOSE_ME` → 応答なしで切断）。
  - `tests/grpc_server/src/main.rs`: `unary_call` が受信した
    `x-wasm-request-rewrite` リクエストメタデータを `x-echoed-wasm-header`
    レスポンスメタデータへ反射するようにした（値が無ければ何もしないため
    既存テストへの影響なし）。これにより「WASM が付与したリクエストヘッダが
    バックエンドまで届いたか」をクライアント視点で検証できる。
  - `tests/e2e_tests.rs`:
    - `test_l4_wasm_downstream_upstream_data_rewrite`: 生 TCP で
      `PROXY_L4_WASM_PORT` へ `foo-data-foo` を送り、echo バックエンド往復後
      `baz-data-baz` が返ることを検証（1 往復で downstream/upstream 両方向を検証）。
    - `test_l4_wasm_close_on_marker`: `CLOSE_ME` を送るとレスポンスが一切
      返らない（`proxy_close_stream` 経由の切断）ことを検証。
    - `test_l4_wasm_disabled_listener_still_passthrough`: `wasm_modules`
      未設定の既存 `l4-passthrough` リスナーが従来どおり動くことを再確認
      （退行防止。既存の `test_l4_tcp_passthrough_forward`/
      `test_l4_passthrough_large_payload` も無変更のまま引き続き対象）。
    - `test_grpc_wasm_trailer_mutation`: `grpc-wasm-trailer.test` ルート経由で
      `grpc-status`/`grpc-message` が `grpc_trailer_filter` の書き換え値
      （`0`/`"rewritten-by-wasm"`）になることを検証。
    - `test_grpc_wasm_request_header_mutation`: 同ルートで `x-echoed-wasm-header`
      レスポンスヘッダが `"applied"` であること（＝ WASM が付与した
      リクエストヘッダがバックエンドまで届いたこと）を検証。
  - 新規ゲストモジュール `examples/wasm-filters/grpc-trailer-filter/`
    （`tests/fixtures/wasm/grpc_trailer_filter.wasm`）を追加。
    `on_http_request_headers` で `x-wasm-request-rewrite: applied` を付与し、
    `on_http_response_trailers` で `grpc-status`/`grpc-message` を
    無条件に書き換える。
  - E2E の**実行**は親タスク側が担当（本チケット実装時点では単体テストのみ
    実行し、E2E テストコード自体は追加済み・`./tests/e2e_setup.sh start/health/stop`
    でのプロキシ起動確認と、新ポートへの生 TCP による手動確認のみ実施）。

## 追記: E2E で判明した欠落バグの修正（フォローアップ）

E2E `test_grpc_wasm_request_header_mutation` の実行で、上記「改修内容」記載の
リクエストヘッダ配線が実際には機能していないバグが見つかった。

- **事象**: `h2_dispatch`（`src/proxy.rs`）内、`on_request_headers` の
  `FilterResult::Continue { .. } => {}` が変更後ヘッダを**破棄**していた。
  `ctx: &H2RequestCtx` が不変参照のため、リクエストヘッダフィルタの戻り値を
  そのまま使わずに握りつぶす実装のまま残っていた。HTTP/3 経路
  （`wasm_request_headers`）・HTTP/1.1 経路（`headers_for_proxy` 再構築）は
  正しく反映されており、**HTTP/2（gRPC 含む）だけが欠落**していた。
- **修正**: `h2_dispatch` に `wasm_request_headers_override: Option<Vec<HeaderField>>` を
  追加し、`Continue { headers, .. }` で変更後ヘッダを保持。WASM リクエストフィルタ
  ブロックの直後・`accept-encoding` パースや `h2_proxy` 呼び出しより前の位置で、
  `Some` の場合のみ `H2RequestCtx` をヘッダ差し替えでローカル再構築して `ctx` を
  シャドウイングする（`method`/`path`/`authority`/`trailers`/`start` は clone、
  `body` は `Bytes` の参照カウント複製、`client_ip` は `Box<str>` clone）。
  この経路は WASM モジュールが実際に適用されたリクエストでのみ発生し、
  WASM 未設定時は `None` 分岐 1 つのみでホットパスへのコスト増はない。
  配置位置により、WASM が `accept-encoding` を書き換えた場合の圧縮選択にも
  正しく反映される。
- **リクエストトレイラーの `Continue { .. } => {}` は意図的な破棄のまま**:
  「既知の限界」に記載の通り、リクエストトレイラー自体を上流バックエンドへ
  転送する経路が現状存在しない（`h2_proxy_h2c` の `h2c_client.send_request` は
  method/path/authority/headers/body のみを受け取り、トレイラー送信 API が無い。
  H1/HTTPS バックエンド経路も同様）。そのためここで変更後トレイラーを保持しても
  反映先が無く、観測可能な挙動差が生まれない。破棄している理由をコード側にも
  コメントで明記した。
- **確認範囲**: `src/proxy.rs` 内の他の `FilterResult::Continue { headers, .. }` /
  `Continue { headers: modified_headers, .. }` 分岐（HTTP/1.1 リクエスト・
  レスポンス、SendFile/MemoryFile レスポンス、gRPC レスポンストレイラー）は
  いずれも戻り値が実際に上流送出用バッファ／構造体へ反映されていることを
  1 箇所ずつ確認済み（今回の HTTP/2 リクエストヘッダのみが唯一の欠落だった）。

## 既知の限界・注意点

- **WASM モジュールが自発的に発呼する gRPC クライアント機能（`proxy_grpc_call` 等の
  実行系）は未実装のまま**。ホスト関数は「登録」までで、実際のネットワーク発呼・
  レスポンスの `process_grpc_response` 経由での配送を行う実行ループが存在しない。
  別チケットでの対応を推奨。
- gRPC **リクエスト**トレイラーは WASM へ見せられるようになったが、
  書き換えた内容をバックエンドへ転送する経路は存在しない
  （そもそも現行実装はクライアント送信トレイラーをバックエンドへ一切転送していない、
  本チケット以前からの制限）。`LocalResponse`/ログ用途としてのみ有効。
- L4 の `on_downstream_data`/`on_upstream_data` は Envoy 同様「フレーム境界」を
  持たないため、`end_of_stream` は常に `false` で呼ぶ
  （ソケット EOF は `on_downstream_close`/`on_upstream_close` で通知）。
- L4 設定は起動時のみ読み込まれ、SIGHUP ホットリロード対象外
  （既存の L4 リスナー全般の制限、F-133 で変更していない）。

## 検証コマンド

```bash
cargo build --features full
cargo test --lib --features full
cargo clippy --features full --all-targets -- -D warnings
cargo fmt --all
```

## 受け入れ条件

- [x] gRPC over H2C のレスポンストレイラーに WASM フィルタが適用され、
      `grpc-status`/`grpc-message` を書き換えられる。
- [x] gRPC リクエストトレイラーに WASM フィルタが適用される（`LocalResponse` 可）。
- [x] `[[l4]]` リスナーに `wasm_modules` を設定すると
      `proxy_on_new_connection`/`proxy_on_downstream_data`/`proxy_on_upstream_data`/
      `proxy_on_downstream_close`/`proxy_on_upstream_close` が動作する。
- [x] `wasm_modules` 未設定（デフォルト）の L4 リスナーは既存の splice/ゼロコピー経路と
      同一コード経路を通り、追加コストは `is_empty()` 判定1つのみ。
- [x] 単体テストがある（`src/wasm/tests.rs::f133_network_filter_tests`）。
- [x] `docs/backlog/backlog.md` に本チケットを追加。
- [x] L4 TLS terminate 経路（`bidirectional_forward_tls_terminate`）にも
      network filter を配線した。
- [x] E2E テストコードを追加した（`test_l4_wasm_downstream_upstream_data_rewrite`/
      `test_l4_wasm_close_on_marker`/`test_l4_wasm_disabled_listener_still_passthrough`/
      `test_grpc_wasm_trailer_mutation`/`test_grpc_wasm_request_header_mutation`）。
      **実行**は親タスク側が担当（`./tests/e2e_setup.sh test`）。
