# B-270: h2c の上流の応答に、HTTP/1.1・HTTP/2 のクライアントでは `[route.compression]` が効かない（HTTP/3 は圧縮する）

**状態: 修正済み（v0.10.1）**

## 出典

[B-266](B-266-head-local-responses-send-body.md) と同じ調査。v0.10.0 のソースのデバッグビルドで、`use_h2c = true` の上流（veil の静的配信、
`text/plain` 4.8KB）へ送るルートに `[route.compression] enabled = true` を書き、`Accept-Encoding: gzip` で要求した。

## 事象

| クライアント → 上流 | HTTP/1.1 の上流 | h2c の上流 |
|---|---|---|
| HTTP/1.1 | gzip（実機） | **無圧縮**（実機。`forward::proxy_h2c` は圧縮の設定を受け取らない） |
| h2c・HTTP/2（TLS） | gzip（実機） | **無圧縮**（実機。`h2_proxy_h2c`・`h2_relay_h2_streaming` は圧縮しない） |
| HTTP/3 | 圧縮する（コード） | 圧縮する（コード。`http3_stream::h2_relay_compressed`） |

- TLS の h2 の上流（`http2 = "auto"`/`"on"`）は、HTTP/1.1 のクライアント（`proxy_h1_via_https_h2`）・HTTP/2 のクライアント
  （`h2_emit_upstream_response` に `compression` を渡している）とも圧縮する。h2c の上流だけ抜けている。
- gRPC は圧縮しない（`application/grpc` は gRPC のメッセージ圧縮を使う）ので、この漏れは gRPC 以外の h2c の上流（HTTP/2 で話すアプリ・
  サイドカー同士）で起きる。

## 影響

- P2（h2c の上流へ送り、`[route.compression]` を有効にしたルート）: HTTP/1.1・HTTP/2 で受けると転送量が減らない。同じ設定で HTTP/3 だけ圧縮する。

## 修正方針

- HTTP/1.1: `proxy_h2c` に圧縮の設定と `client_encoding` を渡し、`proxy_h1_via_https_h2` と同じ判定（gRPC・本文の無い応答を除く）で圧縮する。
  今の `proxy_h2c` は応答を全部溜めて 1 回で書くので、圧縮もその場で済む。
- HTTP/2: `h2_proxy_h2c`（バッファ）は `h2_emit_upstream_response` に `Some((compression, client_encoding))` を渡す。全二重の
  `h2_relay_h2_streaming` は、圧縮する応答だけ本文を溜めて圧縮する（HTTP/3 の `h2_relay_compressed` と同じ。gRPC と本文の無い応答は今のまま中継）。
- 圧縮しない応答（`Accept-Encoding` 無し・型が対象外・小さい）は今のまま流す。
- 試験: E2E で HTTP/1.1・h2c・HTTP/2・HTTP/3 × h2c の上流で `Content-Encoding` が付くこと、gRPC は圧縮しないこと、ストリーミングの gRPC の
  双方向が今のまま動くこと。

## 修正（v0.10.1）

- h2c の上流の応答にも、HTTP/1.1・HTTP/2 のクライアントで `[route.compression]` をかける（溜める上限は [B-281](B-281-unbounded-response-collection-for-compression.md)）。
- HTTP/3 のバッファ経路で `Content-Encoding` が付かなかった件は [B-279](B-279-h3-buffered-compression-no-content-encoding.md)。
- 統合試験 `b270_h2c_upstream_responses_are_compressed`（`/front`・`/front-buffered`）。

## 関連

- [F-171](../features/F-171-h2c-upstream-streaming.md)（h2c の上流の全二重の中継）、[B-269](B-269-h1-static-files-not-compressed.md)、
  [B-267](B-267-response-header-policy-missing-on-proxy-paths.md)（同じ経路の応答ヘッダ）、[B-271](B-271-alt-svc-missing-on-some-h1-paths.md)
