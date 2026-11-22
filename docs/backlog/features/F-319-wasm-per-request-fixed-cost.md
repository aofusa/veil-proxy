# F-319: Proxy-Wasm の 1 要求あたりの固定費を減らす（ヘッダの写し・確保・コールバックの回数・fuel）

**状態: 未着手（2026-11-22 起票。F-317 の子。F-318 の後に、単発の費用を測ってから行う）**

## 今の状態

ノート PC（4 CPU、veil-load、`docs/artifacts/benchmark/httparena-perf/results/m1.tsv`）の 1 要求あたりの CPU（µs）:

| 経路 | v0.10.1 | main |
|---|---|---|
| h1 Wasm 無し（`/plain3`） | 19.3 | 12.3 |
| h1 Wasm の GET（`/baseline11`） | 40.4 | 37.1 |
| h1 Wasm の POST（本文 20 B） | 55.8 | 49.8 |
| h1 Wasm の `/json/{n}` | 58.7 | 52.7 |
| h2c Wasm 無し | 8.4 | 4.5 |
| h2c Wasm の GET（`/baseline2`） | 23.3 | 29.5 |

- Wasm の上乗せは h1 で 約 25 µs、POST は本文の段の分さらに 約 13 µs。nginx の C のモジュールとの差の大半。
- **h2c の Wasm だけ main で遅くなっている**（上乗せ 14.9 → 25.0 µs。h1 は 21.1 → 24.8 µs）。共有の機械で揺れがあるので、まず
  交互 A/B で確かめる（v0.10.1 以降に Wasm の HTTP/2 の経路へ入った変更: B-408 の `set_request_info`〈`:authority` の `Arc::from`〉等）。

要求ごとの主な費用（`proxy/h1.rs`・`proxy/h2.rs`・HTTP/3・`wasm/engine.rs`）:

1. ヘッダの写し: `Vec<(Vec<u8>, Vec<u8>)>` を要求ごとに作る（ヘッダの数 × 2 回の確保）。ゲストへ渡すのは `proxy_get_header_map_pairs`
   の直列化のときだけで、多くのフィルタは数個のヘッダしか見ない。
2. 文字列の確保: `Arc::from(path)`・`Arc::from(method)`・`Arc::from(client_ip)`・`Arc::from(authority)`。
3. コールバックの回数: 1 要求で `proxy_on_context_create` → `proxy_on_request_headers` →（POST）`proxy_on_request_body` → 応答の後に
   `proxy_on_log` → `proxy_on_done`・`proxy_on_delete`。それぞれ `call_async`（ファイバーへの出入り）と `arm_store_until`
   （`set_fuel`・`fuel_async_yield_interval`・`set_epoch_deadline`）。フィルタが `send_local_response` で応答したあとの
   `on_request_complete_async` もファイバーを通る。proxy-wasm の Rust SDK は全コールバックをエクスポートするので、
   「エクスポートが無ければ呼ばない」は SDK のフィルタには効かない。
4. fuel: `consume_fuel(true)` はゲストの全関数の基本ブロックに残量の検査を入れる（ゲストの実行が遅くなる）。打ち切り
   （`max_execution_time_ms`）と協調的な yield（`yield_interval_us`）の両方に使っている。エポック（`epoch_interruption(true)`、
   100 ms ごとの専用スレッド）は既にある。
5. 呼び出しのフューチャの `Box::pin`（`on_request_headers_with_modules_async`）。

## 改修内容（測ってから、効くものだけ）

1. ヘッダは要求のバッファ（`Bytes`）を借りた表を `HttpContext` に置き、`proxy_get_header_map_pairs`・`proxy_get_header_map_value` で
   ゲストのメモリへ直接書く（写しは変更されたときだけ作る。`proxy_add/replace/remove_header_map_value` で初めて所有の表にする）。
2. path・method・authority・client_ip は `Bytes` のスライスか、ワーカーの作業領域に置く（`Arc::from` をやめる）。
3. コールバックを減らす:
   - `proxy_on_context_create` と `proxy_on_request_headers` を 1 回のファイバーの出入りで呼ぶ（`arm_store_until` も 1 回）。
   - 要求本文の段は本文が空のとき呼ばない（今も GET で呼んでいないかを確かめる）。
   - 応答の後の段（`proxy_on_log`・`proxy_on_done`・`proxy_on_delete`）を使わないモジュールを設定で宣言できるようにする
     （例: `[[wasm.modules]] phases = ["request_headers", "request_body"]`。既定は今と同じで全部呼ぶ。外部契約なので文書・
     `examples/config.toml` も直す）。SDK のフィルタでも効く。
4. fuel をやめ、打ち切りと yield をエポックで行う案を測る: エポックの専用スレッドを 1 ms 周期にし、
   `epoch_deadline_async_yield_and_update` で協調的に yield、`max_execution_time_ms` は yield の回数で打ち切る。ゲストの実行時間の
   割合（perf）と、エポックの周期を短くした費用（1 ms ごとの原子変数の加算 1 回）を比べる。差が小さければやらない。
   AGENTS.md の「WASM 等の CPU バウンド処理も協調的に yield する」は守る。
5. h2c の退行（上の表）を交互 A/B で確かめ、本当なら原因の変更を特定して直す。

## 受け入れ条件

- ノート PC の交互 A/B で、h1・h2c の Wasm の GET・POST・`/json` の 1 要求あたりの CPU が下がる（目安: 上乗せ 25 µs → 15 µs 以下）。
  allocs/req も記録する。
- Proxy-Wasm の E2E（全段・全プロトコル・epoll）・単体・`cargo test`。fuel をやめる場合は、無限ループのフィルタが
  `max_execution_time_ms` で打ち切られ、他の要求が止まらない試験。

## 関連

- [F-317](F-317-httparena-infra-score-plan.md)（親）・[F-318](F-318-wasm-per-request-shared-writes.md)・B-175・B-173・B-408・F-271
