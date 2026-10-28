# B-262: HTTP/1.1 のバッファリングのルートで、接続の終わりで区切る応答を送った後に接続を閉じず、クライアントが待ち続ける

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の F-282（KT-01）の E2E を足したとき、`buffering = "full"` のルートで上流が `Content-Length` も chunked も無い応答を返して閉じると、
HTTP/1.1 のクライアントが本文を受け取った後もタイムアウトまで待った（curl は rc=28）。平文の上流（`/echo-full/*`）でも同じで、TLS とは関係しない。
B-244 で接続の終わりで区切る本文を最後まで読むようにした経路で、送った後の扱いが抜けていた。

## 事象

- `proxy::buffered::proxy_request_buffered` は上流の応答ヘッダをそのまま（長さを付けずに）送り、続けて溜めた本文を送る。
- 本文の終わりは接続を閉じることでしか伝わらないのに、戻り値の「クライアントの接続を閉じるか」が常に `false` で、キープアライブで次の要求を待っていた。

## 影響

- P2（HTTP/1.1 で受ける `buffering = "full"`（または `adaptive` でバッファした）ルートで、上流が接続の終わりで本文を区切るとき）: 応答が終わらない。

## 修正（v0.10.0）

- `receive_and_buffer_response` が本文の区切りが接続の終わりか（`until_close`）を返す。`proxy_request_buffered` はそのとき、溜め終えて長さが分かった
  本文に `Content-Length` を付けて送り（`Transfer-Encoding` があるときは付けない）、送った後にクライアントの接続を閉じる。veil はクライアントの接続を
  閉じるときに close_notify を送らないので、閉じるだけでは厳格な TLS クライアント（rustls 等）は切り詰めとみなす（E2E で発覚）。
- ストリーミングの経路で接続の終わりで区切る応答を HTTP/1.1 のクライアントへ送るときの同じ問題（以前からの挙動）は [F-284](../features/F-284-tls-close-notify-to-clients.md)。
- 試験: 単体 `add_content_length_to_close_delimited_head`。E2E `test_f282_tls_upstream_eof_body_requires_close_notify_buffered`（HTTP/1.1 のバッファ経路。
  close_notify 付きは 200 で全文が届く、無しは 502）。

## 関連

- [B-244](B-244-h1-chunked-timeout-upstream-conn-pooled-mid-response.md)、[B-261](B-261-h3-ktls-upstream-control-record-and-close-notify.md)、
  [F-282](../features/F-282-security-review-followup-hardening.md)
