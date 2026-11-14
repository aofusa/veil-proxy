# B-388: 上流の HTTP/2 の応答の疑似ヘッダ・トレーラを検査せず、HTTP/1.1 の応答へ CR・LF を書き得た

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の指摘 F19 から確認。

## 事象

- `http2::upstream_mux` の応答の検査（B-318）は `:` で始まらない名前だけを見ていた。トレーラのブロックは復号した
  フィールドを全部渡すので、HPACK で任意のバイトを運べる `:` で始まる名前・値の CR・LF がそのまま残る。
- HTTP/1.1 のクライアントへの経路は、そのトレーラを応答のヘッダ（h2c の溜める経路）か chunked のトレーラ
  （TLS の h2）にそのまま書くので、悪意のある上流が応答を分けられた。
- `:status` が複数・ヘッダの途中にある応答、END_STREAM の無いトレーラも受けていた（RFC 9113 §8.1・§8.3）。

## 対応

- `malformed_response_fields(fields, trailer, end_stream)` を足す。ヘッダは先頭に `:status` がちょうど 1 つ、
  トレーラは疑似ヘッダを持たず END_STREAM で終わること、名前は token・値は NUL・CR・LF を含まないことを全部の
  フィールドで確かめ、違反はストリームを PROTOCOL_ERROR でリセットする。

## テスト

- `http2::upstream_mux::tests::response_pseudo_headers_and_trailers_are_validated`。
