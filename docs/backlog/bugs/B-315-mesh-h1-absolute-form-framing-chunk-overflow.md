# B-315: メッシュの HTTP/1.1: 絶対形式の要求先で経路の認可を素通り・曖昧な本文の区切りを上流へ送る・chunked の長さの溢れで panic

**状態: 修正済み（v0.11.0）**

## 事象

v0.11.0 前のセキュリティスキャン（Claude Security の F1・F2・F13・F26・F12・F55）で判明。

1. **絶対形式の要求先（F1・F2、HIGH）。** メッシュの HTTP/1.1 の受け口（`src/mesh/http1.rs` の `serve`）が要求行の
   要求先をそのまま正規化・JWT・RBAC・ルートの照合に使い、上流の要求行にも書いていた。`GET http://x/admin/delete`
   は `/admin*` の DENY（や `notPaths` の ALLOW、Linkerd のパス単位の認可）に合わず、既定のルート（`prefix: /`）で
   上流へ届き、上流（RFC 9112 で絶対形式を受ける）は `/admin/delete` として処理していた。データプレーンの
   HTTP/1.1（`origin_form_target`。F-236 の F7）と HTTP/2（拒否）は対処済みで、メッシュの HTTP/1.1 だけ漏れていた。
2. **曖昧な本文の区切り（F13）。** `Content-Length` と `Transfer-Encoding` の両方・複数の `Content-Length`・
   最後が chunked でない TE を受け付け、接続を使い回す上流へそのまま送っていた（要求の密輸）。
3. **HTTP/2 → HTTP/1.1 の `content-length` の重複（F26）。** 最初の 1 つだけを確かめて全部を上流へ書いていた。
4. **chunked の長さの溢れ（F12・F55）。** 上流の chunked の応答をほどくとき `i + 2 + size + 2` が
   `ffffffffffffffff` で折り返し、範囲外を切り出して panic していた（メッシュの HTTP/2 → HTTP/1.1）。
   共有の `http_utils::decode_chunked_body` も `pos + chunk_size` が折り返して panic していた。メッシュでは
   不正な長さを 0（本文の終わり）と読み、切り詰めた応答を成功に見せてもいた。
5. `Content-Length` の値を `str::parse` で読み、`+5`・Unicode の空白を受け付けていた（データプレーンの F17 と同根）。

## 修正

- 要求先は `origin_form_target` で origin 形式にしてから使う（絶対形式は authority を Host にする。それ以外の
  `/` で始まらない要求先は CONNECT と `OPTIONS *` を除き 400）。
- 要求の区切りは `classify_request_framing`（データプレーンと同じ）で検査し、曖昧なものは 400。
- `http_utils::parse_content_length`（`1*DIGIT`、前後の SP・HTAB だけ許す）を足し、メッシュの要求・応答と
  HTTP/2 → HTTP/1.1 の `content-length` に使う。複数の `content-length` は全部が同じ値のときだけ受ける。
- chunked の長さは溢れを検査し、不正な長さはストリームのリセット（`INTERNAL_ERROR`）にする。
  `decode_chunked_body` は飽和加算にする。

単体試験 `http_utils::chunked_span_tests::{parse_content_length_is_strict, decode_chunked_body_huge_size_does_not_panic}`。

## 関連

- B-314（同じスキャンのメッシュの JWT）・B-316（network RBAC）
- docs/artifacts/security/v0110-claude-security/（F1・F2・F12・F13・F26・F55）
