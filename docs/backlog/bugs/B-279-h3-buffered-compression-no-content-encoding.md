# B-279: HTTP/3 のバッファ経路（`buffering = "full"` 等）で圧縮した本文に `Content-Encoding` が付かない

**状態: 修正済み（v0.10.1）**

## 出典

[B-270](B-270-h2c-upstream-responses-not-compressed.md) の統合試験に `buffering = "full"` のルートを足して見つけた（実機で確認）。

## 事象

HTTP/3 のバッファ経路（`H3BufferedTask`）は `[route.compression]` で本文を圧縮するのに、応答ヘッダに `Content-Encoding`・`Vary` を
足していなかった。クライアントは gzip 等のバイト列を平文として受け取る（壊れた応答）。

## 修正（v0.10.1）

- 圧縮したら `content-encoding`（選んだ方式）と、無ければ `vary: Accept-Encoding` を足す（`src/http3_server/request_task.rs`）。
- 統合試験 `b270_h2c_upstream_responses_are_compressed` の `/front-buffered`（全プロトコルで `Content-Encoding: gzip` と伸張した本文が一致）。

## 関連

- [B-270](B-270-h2c-upstream-responses-not-compressed.md)
