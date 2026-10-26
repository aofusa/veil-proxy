# B-188: Proxy-Wasm のローカル応答が HEAD・204・304 にも本文を付け、keep-alive の接続で応答がずれる

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 のデータプレーンのレビュー（`docs/artifacts/v0100/review-dataplane.md` の L-1）。F-271 でローカル応答の後も接続を
続けるようにして表面化した。

## 現象

フィルタが HEAD の要求に `send_local_response(200, body = "...")` を返すと、veil は本文を付けて書き、接続を続ける。
クライアントは HEAD なので本文を読まず、残ったバイトを次の応答の先頭として読んで壊れる。204・304 も同じ。

## 直したこと

- `wasm_local_response_h1` が要求のメソッドを受け取り、HEAD と 1xx・204・304 では本文を書かない。HEAD は Content-Length を
  本文の長さのまま付ける（GET と同じヘッダ）。1xx・204・304 は Content-Length を補わない。Envoy も HEAD のローカル応答の
  本文を落とす。
- 文書: `docs/guide/wasm.md`（英日）。

## 試験

- 単体 `proxy::h1::b188_wasm_local_response_tests::head_and_bodyless_statuses_have_no_body`。
- E2E `b188_wasm_local_response_to_head_has_no_body`（`tests/standalone_e2e.rs`。HEAD と GET をパイプラインで送り、HEAD の
  応答に本文が無く、GET の応答がずれずに読めること。直す前は 2 つ目の応答の解析が壊れる）。
