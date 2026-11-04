# B-282: 上流が本文の途中で閉じた応答を、HTTP/2 のクライアントへ空の END_STREAM で完結した応答に見せる

**状態: 修正済み（v0.10.1）**

## 出典

[B-281](B-281-unbounded-response-collection-for-compression.md) の修正で HTTP/2 の逐次送出（`h2_stream_body_cl`・`h2_stream_body_chunked`）を読んで見つけた（統合試験で再現）。

## 事象

HTTP/2 のクライアントへ上流の応答を逐次送る経路は、上流が `Content-Length` に届く前・chunked の終端の前に閉じると、「graceful」として
空の END_STREAM でストリームを閉じていた。クライアントは切り詰めた本文を完結した応答として受け取る（`Content-Length` の無い chunked 由来の
応答では気づく手段が無い）。HTTP/3 は同じ場合にストリームをリセットする（B-86）。

## 修正（v0.10.1）

- どちらの場合も `RST_STREAM(INTERNAL_ERROR)` で打ち切る（`src/proxy/h2.rs`）。
- 統合試験 `b282_truncated_upstream_body_resets_h2_and_h3_streams`（Content-Length・chunked の途中で閉じる上流で、h2c・HTTP/2・HTTP/3 の
  クライアントが完結した応答を受け取らない）。

## 関連

- [B-281](B-281-unbounded-response-collection-for-compression.md)、B-244、[B-86](B-86-http3-tls-backend-plaintext-overflow.md)
