# B-320: Proxy-Wasm のホストの検査漏れ（決めていない Pause・ホストのヒープ・NULL の確保・呼び出しの ID）

**状態: 修正済み（v0.11.0）**

## 事象

v0.11.0 前のセキュリティスキャン（Claude Security の F20・F21・F48・F49・F57）で判明。

- **F20**: Pause して `dispatch_http_call` したフィルタの呼び出しをその場で解決したあと、フィルタが再開
  （`proxy_continue_stream`）もローカル応答もしなくても、要求を続けていた（決めていない Pause が許可になる）。
  `max_http_calls` 回を超えて残った呼び出しも同じ。`proxy_continue_stream` は何もしていなかった。
- **F21**: `proxy_set_buffer_bytes` はゲストの `size`（最大 4GiB）までホストのバッファを 0 で埋めて伸ばしてから
  捨てていた（ゲストのメモリの上限と関係なくホストのヒープを確保できる）。
- **F49**: ゲストのアロケータの失敗（0）を有効な場所として扱い、ゲストのメモリの先頭へクライアントのバイトを書いていた。
- **F48**: gRPC の呼び出し・ストリームの ID と HTTP の呼び出しのトークンをコンテキストごとに 1 から数えていた。
  コンテキストはコールバックごとに作るので、gRPC の実行器の表（モジュール名・ID）で同じモジュールの別の要求の
  呼び出しに送ったり、取り消したり、置き換えたりできた。
- **F57**: 要求ヘッダの段の「本文を待つ Pause」は本文の段で決める前提だが、WebSocket（HTTP/1.1 と拡張 CONNECT）は
  本文の段を走らせないので、決めずに通っていた。

## 修正

- `proxy_continue_stream` で `stream_resumed` を立て、`settle` は再開もローカル応答も無ければ Pause のままにする
  （`failure_policy` の `unsupported_pause`）。
- `proxy_set_buffer_bytes` の範囲はバッファの終わりで切る（Envoy と同じ）。
- 各 `allocate_wasm_memory` はサイズが 0 でないのに 0 を返したら失敗にする（i32 を超えるサイズも失敗）。
- ID・トークンはプロセスで一意（`next_unique_id`、1..=i32::MAX）。
- WebSocket の要求ヘッダの段は `end_of_stream = true` で呼ぶ。

文書: docs/guide/wasm.md（英日）。単体試験 `wasm::tests`（ID の一意性）。

## 関連

- docs/artifacts/security/v0110-claude-security/（F20・F21・F48・F49・F57）
