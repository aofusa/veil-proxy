# B-281: 圧縮・HTTP/2 への中継のために上流の応答本文を上限なく溜め、SSE は利用者へ届かない

**状態: 修正済み（v0.10.1）**

## 出典

B-266〜B-274 の修正のあと、応答本文を溜める経路を全部たどって見つけた（コードで確認。統合試験で再現）。

## 事象

- 圧縮する応答（HTTP/1.1 の `transfer_compressed_response`、HTTP/2 の `collect_response_body`、HTTP/3 の圧縮・バッファ経路、HTTP/3 → h2 の上流の
  圧縮）は本文を最後まで溜めてから圧縮する。溜める量に上限が無く、大きい応答・終わらない応答の大きさだけメモリを使う。
- `text/event-stream`（SSE）は既定の `compressible_types` の `text/` に当たるので、圧縮の効くルートでは溜められ、終わらない SSE は
  1 イベントも届かない。
- HTTP/2 のクライアントは、圧縮しないルートでも長さ不明（上流が閉じるまで）の応答を溜めていた（SSE が届かない）。

## 修正（v0.10.1）

- `text/event-stream` は設定によらず圧縮しない（`CompressionConfig::should_compress`）。
- 既知長が溜める上限（64 MiB、`pool::MAX_COLLECTED_RESPONSE_BODY`）を超える応答は圧縮せずに逐次送る。長さ不明・chunked で上限を超えたら
  打ち切る（ヘッダ送出前なので 502）。HTTP/1.1・HTTP/2・HTTP/3 の溜める経路すべてに同じ上限をかける。
- HTTP/2 のクライアントへの長さ不明の応答は溜めずに逐次送る（上流が閉じたら END_STREAM。TLS で close_notify の無い切断はリセット。F-282）。
- HTTP/2 のクライアントの `buffering = "full"` は、メトリクスを数えるだけで本文を溜めていなかった（長さ不明の応答を溜めていたので
  偶然 502 になっていた）。HTTP/1.1 の上流の応答を全部溜めてから送るようにする（区切りどおりに終わらなければ応答ヘッダの前に 502。
  HTTP/1.1・HTTP/3 と同じ。E2E `test_f282_http2_tls_upstream_eof_body_requires_close_notify`）。
- 単体試験 `test_should_compress_skips_event_stream_and_huge_bodies`、統合試験 `b281_sse_streams_through_compression_route_on_every_protocol`
  （chunked・閉じるまでの SSE が、圧縮するルートとしないルートで 4 プロトコルとも最初のイベントを 5 秒以内に受け取る）。
- `docs/guide/compression-cache.md`（英日）に書いた。

## 関連

- [B-282](B-282-h2-truncated-upstream-body-ends-cleanly.md)、B-244（区切りどおりに終わらない本文を完結に見せない）
