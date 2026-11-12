# B-360: HTTP/1.1 の中継の応答を返した後に閉じるとき、上流の応答ヘッダに `Connection: close` を付けていなかった

**状態: 修正済み（feat/beat-nginx、2026-11-12）**

[F-303](../features/F-303-concentrate-connections.md) の計測（lab7）で、HTTP/1.1 の中継（`proxy_h1`）の 1 回目に wrk が
`Socket errors: read 171` を数えて見つけた。

## 事象

veil が応答の後にクライアントの接続を閉じる HTTP/1.1 の要求（要求の `Connection: close`、F-303 で接続を別のワーカーへ
動かすとき）でも、上流から受け取った応答ヘッダをそのまま返していた。上流の `Connection: keep-alive`・`Keep-Alive` が
そのままクライアントに届くか、何も付かないので、クライアントは同じ接続で次の要求を送ってから閉じられたことに気付く
（wrk の read のエラー。冪等でない要求ではクライアントが再送できない）。

veil が自分で作る応答（静的配信・リダイレクト・キャッシュのヒット・上流が HTTP/2 の中継・API）は閉じるときに
`Connection: close` を付けていた。上流の応答ヘッダを流す次の経路だけが付けていなかった。

- `proxy_http_request_with_compression`・`proxy_https_request_with_compression`（ユーザー空間の転送と圧縮・圧縮の失敗時）
- `proxy_request_buffered`（`buffering`・Wasm の本文のフェーズ）
- splice の経路（kTLS・平文。応答ヘッダの方針が無いときは上流の head を 1 バイトも変えずに送っていた）

## 対応

閉じるかどうか（`client_wants_close`）を各経路の応答ヘッダを作る箇所まで渡し、閉じるときは `compress::set_h1_close` で
`Connection`・`Keep-Alive` の行を `Connection: close` の 1 行に置き換える（その場で書き換える。閉じない応答の経路は今までと
同じで、splice の経路も閉じないときは上流の head をそのまま送る）。Wasm の `on_http_response_headers`・CORS をかけた後に
置き換えるので、閉じる応答では必ず `close` になる。

RFC 9112 §9.6: `close` を受け取ったサーバは最後の応答に `close` を付けるべき（SHOULD）。

## テスト

- 単体: `set_h1_close_replaces_connection_headers`（大文字小文字・`Keep-Alive`・複数行・2 回かけても 1 行・空行で終わらない
  ものは触らない）、`finish_h1_upstream_head_marks_close`（閉じないときは従来どおり）。
- E2E: `test_connection_close_header` を、`/`（圧縮のあるルート）と `/api/`（圧縮の無いルート。kTLS の E2E では splice）で
  応答の `Connection` がちょうど `close` 1 つ・`Keep-Alive` 無し・接続が閉じられることを確かめる形にした（以前は
  `keep-alive` でも通っていた）。
