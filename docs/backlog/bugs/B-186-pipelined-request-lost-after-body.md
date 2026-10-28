# B-186: 本文を読む新しい経路で、本文の後ろに続いた次の要求（パイプライン）を捨てる

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 のデータプレーンのレビュー（`docs/artifacts/v0100/review-dataplane.md` の M-2）。F-266・F-271 の経路で表面化した。

## 現象

HTTP/1.1 で、本文の末尾と次の要求が同じセグメントで届く（パイプライン、または 1 回の write で本文と次の要求を送る
クライアント）と、次の要求の一部または全部が消える。接続は keep-alive のまま次の要求を待つので、丸ごと消えたら
クライアントは応答を待ったまま `READ_TIMEOUT` まで止まり、一部だけ消えたら途中から解析して 400 で閉じる。

- ストリーミング受信のハンドラ（F-266 の `pump_api_body`）
- Proxy-Wasm の要求本文の段（F-271。`read_full_request_body`）
- Proxy-Wasm の要求ヘッダの段のローカル応答の後の本文の読み捨て（F-271 で毎回閉じるのをやめた。`drain_request_body`）
- 同じ関数を使う既存の経路（ハンドラの本文の読み込み、File・Redirect 等のローカル応答の前の読み捨て）も同じ

veil 自身が解析するので、フロントとバックの解釈違い（スマグリング）にはならない。

## 原因

`drain_request_body`・`read_full_request_body`・`pump_api_body` は本文の残りをソケットから読むとき、Content-Length（または
chunked の終端）を超えて読んだバイトを捨てていた。次の要求として残すのは、ヘッダと一緒に読んだ分（`pipelined`、B-129）だけ
だった。

## 直したこと

- 3 つの関数が、本文の終わりより後ろまで読んだ分を `overflow: &mut BytesMut` の後ろに足す（chunked は
  `ChunkedDecoder::feed_counted` で終端の位置を求める）。呼び出し側は `pipelined` を渡し、本文を読み終えた後に今までどおり
  蓄積へ戻す。`handle_backend`・`handle_api_handler`・`handle_api_streaming` は `pipelined` を受け取る。
- 本文を読み過ぎるのはヘッダと一緒に本文を読み切れなかったとき（`pipelined` が空のとき）だけなので、順序は崩れない。
  ホットパスは引数が 1 つ増えるだけ（読み過ぎたときだけ `BytesMut` へ写す）。

## 試験

- 単体 `proxy::h1::b186_overflow_tests`（socketpair で本文の残りと次の要求を 1 回で書き、`drain_request_body`・
  `read_full_request_body`・`pump_api_body` が次の要求をそのまま返すこと。Content-Length・chunked）。
- E2E `b186_wasm_paths_keep_pipelined_request_after_body`（`tests/standalone_e2e.rs`。Wasm の要求本文の段のローカル応答
  （Content-Length・chunked）・本文を書き換えて上流へ送る経路・要求ヘッダの段のローカル応答の読み捨てで、本文の残りと次の
  要求を 1 回で送り、両方の応答が返ること。直す前は 2 つ目の応答が来ずに止まる）。
- `tests/api/library_e2e.sh rust` の「streaming request pipelined (HTTP/1.1)」（ストリーミング受信のハンドラ）。
