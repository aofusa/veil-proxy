# B-501: arc_swap の Guard を `.await` をまたいで持つと、スレッドの速いスロット（8 個）が埋まり、以後の設定の読み取りが全部、全ワーカー共有の参照カウントへの書き込みになる

**状態: 完了（2026-11-25、feat/httparena-perf f680dc43。F-323 の aofish の perf で見つけた）**

## 事象

- aofish の perf（81aaa3cf、epoll、h2c の `Respond` へ h2load 64 接続 × 100 ストリーム、ワーカー 8 本）で、ユーザー時間の約 5.9% が
  arc_swap だった: `h2_dispatch` の `CURRENT_CONFIG.load()`（1.67%）、`h2_push_base_headers` の `SERVER_HEADER_VALUE.load()`
  （1.66%）、遅い経路の `LocalNode::new_helping`（0.94%）・`confirm_helping`（0.86%）。
- arc_swap（1.9）の `load()` は、スレッドごとに 8 個ある速いスロット（`DEBT_SLOT_CNT`）の 1 つに番地を書いて Guard を守る。
  空きが無いと遅い経路（`HybridProtection::fallback`）に落ち、Guard を参照カウントの加算で守り（`into_inner`）、捨てるときに減算する。
  どちらも全ワーカーが同じ `Arc` の行へ書く（F-321 で消した共有の行への書き込みと同じ型）。速いスロットは Guard が生きている間
  使われ続ける。
- HTTP/2 の接続（`handle_http2_connection`・`handle_h2c_connection`）は、`settings` を取るための `CURRENT_CONFIG.load()` の Guard を
  接続の間ずっと持っていた。1 ワーカーに 8 接続（64 接続 ÷ 8 ワーカー。HttpArena は 256〜1024 接続 ÷ 64 ワーカー = 4〜16）を
  超えると、そのワーカーの以後の読み取りはすべて遅い経路になる。
- 同じ形（Guard を `.await` をまたいで持つ）が他にも 20 か所あった（clippy の `await_holding_invalid_type` で数えた）:
  - HTTP/3 の要求のタスク: 経路の解決の前に読んだ Guard を、管理 API でなければ要求の終わりまで（上流への中継の間も）持つ。
    メトリクスの判定・経路の解決（`resolve_route` の中）でも別に読み、1 要求で 3〜5 回読む。
  - Proxy-Wasm: 各フェーズ（HTTP/1.1・HTTP/2・HTTP/3 の要求ヘッダ・本文・トレーラ、応答ヘッダ・本文・トレーラ、`on_log`、
    WebSocket のトンネル）が `config.wasm_filter_engine` を取るために設定を読み直し、Guard を持ったまま Wasm を呼ぶ（Wasm の
    呼び出しはインスタンスの空き待ち・燃料の切れ目で中断する）。
  - L4 の TLS・メッシュの TLS のハンドシェイク、Wasm の `http_call` の offload の待ち。
  - HTTP/1.1 のメトリクス・管理 API の応答の書き込み（通常の要求は経路の解決の後で手放していた）。

## 原因

- `CURRENT_CONFIG.load()` の Guard を必要な値を取り出した後も持ち続けていた（接続・要求の単位で `let config = ...load();`）。
  `drop(config)` で手放しても、一度借用した Guard はコルーチンの状態に区画の終わりまで領域が残るので、clippy では区画で区切らないと
  見分けられない（実行時に持っているかどうかとは別）。

## 直し方

- clippy.toml の `await-holding-invalid-types` に `arc_swap::Guard` を入れ、`.await` をまたいで Guard を持つ箇所を clippy で検出する
  （CI の `cargo clippy --all-targets --features full`・`full,epoll` は `-D warnings`）。
- HTTP/2 の接続: `settings` を 1 文で取り出す（Guard は文の終わりで捨てる）。
- HTTP/1.1・HTTP/2・HTTP/3 の要求の入口: 設定は 1 回だけ区画の中で読み、メトリクス・管理 API の応答か経路の解決の結果を値で取り出す
  （`h1_management_response`・`h2_route_request`・`h3_route_request`。応答の書き込みは区画の外）。HTTP/3 は `resolve_route_in` を
  使い、読み取りを 1 要求 1 回（静的配信の圧縮の設定は文の中で 1 回）にした。
- Proxy-Wasm: 要求の入口で、モジュールのあるルートならエンジンのワーカーの写し（`wasm::worker_engine`。`Rc<WorkerEngine>`、元の
  `Arc` はエンジンが差し替わるまでワーカーが持つ）を取り、要求の並び（`WasmModules::for_request(modules, engine)`）に持たせる。
  各フェーズは `WasmModules::engine()` から借り、設定を読み直さない。`_async` の API（`on_request_headers_with_modules_async` 等・
  `on_request_complete_async`）は `&self`・`&WasmModules` で受け取る（呼び出しごとのエンジンとモジュールの並びの `Arc` の複製
  ＝共有の行への書き込みも消える。F-318 の改修 3）。ワーカーの写しはルートの表と同じ点（アイドルの古い世代・スレッドの終わり）で手放す。
  エンジンの無い設定ではモジュールを走らせない（以前もどのフェーズもエンジンが無ければ何もしなかった）。
- L4・メッシュの TLS、Wasm の `http_call`、メッシュのリスナーの表: 値を取り出してから待つ。

## 試験

- clippy（`--all-targets`、`full`・`full,epoll`・`full,mesh-istio,mesh-linkerd`（epoll も）・`full,handler,grpc-handler`）で検出が 0。
- 単体・統合・E2E（HTTP/1.1・HTTP/2・HTTP/3 の Proxy-Wasm、管理 API・メトリクス、WebSocket のトンネル、L4 の TLS）。
- aofish の交互 A/B（h2c・h2 の `Respond`、Wasm の各プロトコル、パイプライン）。

## 結果

aofish（16 スレッド、サーバは CPU 0〜7・ワーカー 8 本、負荷は CPU 8〜15）の交互 A/B（t1 → f680dc43、2 回、1 要求の CPU）:

| 経路 | epoll | io_uring |
|---|---|---|
| h2c の `Respond` | 2.42 → 2.35 µs（−3.1%） | 2.45 → 2.47 µs（+1.0%） |
| h2 の `Respond` | 3.04 → 2.96 µs（−2.8%） | 3.39 → 3.17 µs（−6.5%） |
| HTTP/3 の `Respond` | 5.68 → 5.24 µs（−7.6%） | 5.30 → 5.30 µs（±0%） |
| HTTP/3 の静的配信 | 30.68 → 28.62 µs（−6.7%） | 29.65 → 30.57 µs（+3.1%） |
| h1 の Wasm | 51.60 → 51.42 µs（−0.3%） | 30.47 → 29.28 µs（−3.9%） |

- 他の経路（Wasm の h2c・h2・HTTP/3・パイプライン、h1・パイプラインの `Respond`、JSON・静的配信の TLS）は ±4% の揺れの範囲。
  io_uring のパイプラインの `Respond` の +3.9%（両方の順で同じ向き）は、4 回の交互で測り直すと 1.95 → 1.85 µs（−4.9%）で揺れだった。
- この計測は 1 ワーカーあたり 8 接続（h2load 64 接続 ÷ 8 ワーカー）で、速いスロットが埋まるかどうかの境目。HttpArena
  （256〜4096 接続 ÷ 64 ワーカー）では 1 ワーカーの接続がもっと多く、効きはここより大きいはず。

## 関連

- [F-321](../features/F-321-core-path-per-request-shared-writes.md)・[F-323](../features/F-323-h2-per-stream-fixed-cost.md)・
  [F-318](../features/F-318-wasm-per-request-shared-writes.md)・F-297（`CURRENT_CONFIG.load()` は要求で 1 回）
