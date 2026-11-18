# B-464: ハンドラの溜めた本文の応答に Transfer-Encoding: chunked のヘッダがあると、HTTP/1.1 で本文を chunk に符号化せずに chunked と書いていた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「HTTP/1.1 handler responses advertise Transfer-Encoding: chunked from a handler header but write the buffered body
unchunked, so the body bytes become response framing」
から確認。

## 事象

- `api::dispatch::write_http1_head` は、応答がストリームでなくても、ハンドラが付けた `transfer-encoding: chunked` の
  ヘッダがあれば `Transfer-Encoding: chunked` を書いて `Content-Length` を落としていた。本文（`resp.body`）は
  そのまま送るので、クライアント・前段のプロキシは本文を chunk の大きさの行・終わりとして読んだ。
- 上流の応答のヘッダをそのまま返すハンドラ・`App::tokio_service` の tower・axum のサービスは、chunked の本文を
  復号した後も `transfer-encoding: chunked` を残す（hyper・veil のクライアントとも）。本文の先頭を決められる者は
  `0\r\n\r\nHTTP/1.1 200 OK ...` で、前段の接続の次の要求への応答を差し込めた。

## 対応

- `call_with` で、ストリームでない応答（アップグレードを除く）からハンドラの `Transfer-Encoding` を落とし、HEAD 以外は
  `Content-Length` も落とす（`write_http1_head` が本文の長さで付ける。HEAD はハンドラの長さが落とした本文の長さ）。
- `write_http1_head` がヘッダの `transfer-encoding: chunked` を chunked の印と見るのは、`call` が HEAD でストリームを
  落としたとき（本文が空）だけにする。HTTP/2・HTTP/3 は従来どおり（`h2_h3_headers` が落とす）。

## テスト

- `api::dispatch::tests::buffered_body_framing_ignores_handler_te_and_length`（本文に応答の形の文字列を置き、
  `transfer-encoding: chunked` と食い違う `content-length` を付けたハンドラで、GET は本文の長さの `Content-Length`
  だけ、HEAD はハンドラの長さで chunked にしない）。既存の `head_drops_stream`・`http1_head_streaming_is_chunked`。
