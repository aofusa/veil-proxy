# B-19: Proxy-Wasm ABI 不一致（BufferType 番号・マップ直列化形式）で SDK モジュールが panic する

## 出典

F-62（HTTP コール Pause/resume 配線）の E2E 実装中に検出（2026-07-05）。
**同日修正済み。**

## 事象

proxy-wasm Rust SDK（0.2 系）製モジュールがホスト API を呼ぶと SDK 内部で panic する
2 つの ABI 不一致があった。header_filter.wasm（既存 E2E）はヘッダ**書き込み系** API しか
使わないため露見していなかったが、**読み取り系** API（`get_plugin_configuration` /
`get_http_call_response_headers` / `get_http_request_headers` 等）を使うモジュールは
すべて即 panic する状態だった。

1. **BufferType 番号の不一致**（`src/wasm/constants.rs`）
   - 旧実装: `HTTP_CALL_RESPONSE_BODY=2` / `PLUGIN_CONFIGURATION=4` / `VM_CONFIGURATION=5`
   - SDK/ABI: `DOWNSTREAM_DATA=2` / `UPSTREAM_DATA=3` / `HTTP_CALL_RESPONSE_BODY=4` /
     `GRPC_RECEIVE_BUFFER=5` / `VM_CONFIGURATION=6` / `PLUGIN_CONFIGURATION=7`
   - SDK の `get_plugin_configuration()`（buffer_type=7）が未知タイプ → `NOT_ALLOWED(13)`
     → SDK 側 `panic!("unexpected status: 13")`。

2. **マップ直列化形式の不一致**（`serialize_headers` / `deserialize_headers`）
   - 旧実装: `[num][k1_len][k1][v1_len][v1]...` のインターリーブ形式
   - SDK/ABI: `[num][k1_len][v1_len]...[k1]\0[v1]\0...`（サイズテーブル + NUL 終端）
   - SDK の `deserialize_map` が旧形式を読むと範囲外アクセスで panic
     （`range end index ... out of range`）。逆方向（SDK→ホスト）はヘッダが
     文字化けした状態で黙って誤パースされていた。

## 修正内容（2026-07-05）

- `constants.rs` の BufferType 番号を SDK/ABI 準拠へ修正（`DOWNSTREAM_DATA` /
  `UPSTREAM_DATA` を追加）。単体テスト `test_buffer_type_constants` で全値を固定。
- マップ直列化を新設の共通モジュール `src/wasm/host/abi.rs` へ集約し、
  SDK 互換のワイヤ形式で実装（`headers.rs` / `stream.rs` / `http_call.rs` の
  重複実装 3 箇所を置換）。不正データはオーバーフローせず `None` を返す。
  単体テスト（ラウンドトリップ・レイアウト検証・不正データ拒否）を追加。

## 関連

- 検出: [F-62](../features/F-62-proxy-wasm-http-call-benchmark.md)
- 併発: [B-20](B-20-wasm-sync-call-async-store-panic.md)
