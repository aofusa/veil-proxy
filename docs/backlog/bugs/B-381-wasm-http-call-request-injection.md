# B-381: Proxy-Wasm の `http_call` が CR・LF を含む値で要求を分け、バイナリの本文を壊していた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の候補から確認。

## 事象

- `wasm::http_executor::build_http_request`（`proxy_http_call` の要求を組み立てる。要求ごとの呼び出しと
  `on_tick` の呼び出しの両方が通る）は `:method`・`:path`・ヘッダの名前と値を検査せずに `format!` で連結していた。
  モジュールがクライアントの値（クエリを復号した値など）をそのまま渡すと、CR・LF で呼び出し先へ別の要求を
  足せた（要求の密輸）。UTF-8 でない値は空文字に置き換わっていた。
- モジュールが `content-length`・`transfer-encoding`・`connection`・`host` を渡すと、veil が付ける
  `Host`・`Content-Length`・`Connection: close` と二重になっていた（上流の解釈がずれる）。
- 本文は `String::from_utf8_lossy` で読み直していたので、UTF-8 でない本文は U+FFFD に置き換わって長さが変わり、
  元の長さで付けた `Content-Length` と食い違っていた。

## 対応

- 要求を `Vec<u8>` で組み立てる。`:method` は token、`:path` は空でなく制御文字・空白を含まないこと、ヘッダの
  名前は token、値は CR・LF・NUL を含まないことを確かめ、満たさなければ送らない（モジュールには既存の失敗と
  同じ `504`・`x-wasm-error: http_call_failed` を返す）。
- `host`・`content-length`・`transfer-encoding`・`connection` はモジュールの値を捨て、veil が決める。
- 本文はバイト列のまま付ける。
- 利用者ガイド（`docs/guide/wasm.md`・`docs/guide/ja/wasm.md`）の外部サービス連携に書いた。

## テスト

- `wasm::http_executor::tests::build_http_request_rejects_injection_and_keeps_binary_body`・
  `test_build_http_request`。
