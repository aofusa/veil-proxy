# B-245: HTTP/1.1 の経路で HEAD・204・304 の上流応答に `Content-Length` があると本文を 30 秒待ち、上流接続とクライアント接続を閉じる

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27 のセキュリティレビュー 2 回目の FW-02。aofish の Docker Desktop で v0.9.2 を使って実機再現（`docs/artifacts/security/repro/fw-run.log` の T1）。

## 事象

`HEAD /head` を HTTP/1.1 のリスナーから HTTP/1.1 の上流へ中継し、上流が `Content-Length: 5` のヘッダだけを返す（RFC 9110 §9.3.2 のとおり本文なし）と:

- ヘッダはすぐクライアントへ届く。
- `src/proxy/compress.rs` L423〜L450（`https.rs` も同じ）は `body_remaining = content_length - body_start.len()` を method・status を見ずに計算し、
  `transfer_response_body_with_cache` で 5 バイトを **`READ_TIMEOUT`（30 秒）** 待つ。
- タイムアウト後 `WARN Backend response body incomplete: 0 < 5`（B-17 の切断）で上流接続を捨て、クライアント接続も閉じる。アクセスログは
  `method=HEAD ... duration_ms=30005`。同じクライアント接続の次の `GET` は 30 秒後に何も返らない。

B-104 で HTTP/3 の経路に入れた「HEAD・204・304 は本文なし」（`src/http3_stream.rs` L1280〜1365）と `proxy_h1_via_https_h2`（`forward.rs` L1163 の `no_body`）が
HTTP/1.1 の上流の経路に無い。E2E の `test_b104_http3_head_on_keepalive_upstream` も HTTP/3 だけ。

## 影響

- P2: HEAD 1 回につき 30 秒、ワーカーのタスクと上流の keep-alive 接続を占有する。HEAD の洪水で上流の接続を食い潰せる（監視系の HEAD で日常的に起きる）。
  クライアントの keep-alive 接続が切られる。
- 204/304 で `Content-Length` を返す上流（nginx は 304 に元の `Content-Length` を付ける）も同じ。

## 改修案

- `compress.rs`・`https.rs` の応答解析の直後に `no_body = method == HEAD || status == 204 || status == 304 || (100..200)` を判定し、
  `body_remaining = 0` にする（`Content-Length` ヘッダは転送してよい）。`1xx` は `drain_interim_responses` が処理済みか確認する。
- HTTP/1.1・HTTP/2 のリスナーに B-104 の HEAD の E2E を展開する。

## 修正（v0.10.0）

- `ParsedResponse::body(is_head)` が HEAD への応答・1xx・204・304 を本文なし（`ResponseBody::None`）とする。HEAD は要求を書く前に `request_is_head` で判定し、HTTP/1.1（平文・rustls・kTLS・`buffering = full`）と HTTP/2 で受けた要求の経路へ渡す（HTTP/3 は B-104 で対応済み）。Content-Length ヘッダはそのまま転送し、本文の無い応答は圧縮しない。
- 試験: 単体 `response_body_none_for_head_204_304`、E2E `test_b245_head_304_on_keepalive_upstream_h1_h2`（HEAD・304 に Content-Length: 5 を付ける上流。HTTP/1.1・HTTP/2 で 5 秒以内に応答し、同じクライアント接続の次の要求も通る）。修正前のコードで失敗することを確認。

## 関連

- [B-104](B-104-http3-upstream-no-keepalive.md)、[B-17](B-17-malformed-backend-client-hang.md)、[B-244](B-244-h1-chunked-timeout-upstream-conn-pooled-mid-response.md)
