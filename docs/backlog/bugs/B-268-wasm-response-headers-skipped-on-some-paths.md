# B-268: Proxy-Wasm の `on_http_response_headers` が、HTTP/2 → HTTP/1.1 の上流・HTTP/1.1 → h2c の上流・HTTP/1.1 の圧縮とバッファの経路で呼ばれない

**状態: 未着手（2026-11-04 起票）**

## 出典

[B-266](B-266-head-local-responses-send-body.md) と同じ調査。v0.10.0 のソースのデバッグビルドに `modules = ["header_filter"]`
（`tests/fixtures/wasm/header_filter.wasm`。`on_http_response_headers` で `X-Veil-Processed` を足す）のルートを置き、上流ごとに確かめた。

## 事象

ガイド（`wasm.md` の Filter Phases by Protocol）は `on_http_response_headers` を HTTP/1.1・HTTP/2・HTTP/3 とも「yes」と書く。実際は次のとおり。

| クライアント → 上流 | 呼ばれるか | 確かめ方 |
|---|---|---|
| HTTP/1.1 → HTTP/1.1（圧縮しない応答） | ✓ | 実機 |
| HTTP/1.1 → HTTP/1.1（`[route.compression]` で圧縮する応答。`compress::transfer_compressed_response`） | **×** | 実機（`Accept-Encoding: gzip` で `X-Veil-Processed` が無い） |
| HTTP/1.1 → HTTP/1.1、`buffering = "full"`（`proxy::buffered::proxy_request_buffered`） | **×** | コード（モジュールを受け取らない） |
| HTTP/1.1 → h2c（`forward::proxy_h2c`） | **×** | 実機（モジュールを受け取らない） |
| HTTP/2 → HTTP/1.1（`h2_proxy_http`・`h2_proxy_https`） | **×** | 平文の上流は実機（h2c・TLS の HTTP/2 とも）、TLS の上流はコード |
| HTTP/2 → h2c・TLS の h2（`h2_emit_upstream_response`） | ✓ | 実機（h2c） |
| HTTP/3 → すべて（Wasm のルートはバッファ経路。`request_task::handle_proxy`） | ✓ | コード |
| 静的配信（File・memory。全プロトコル） | ✓ | 実機（HTTP/1.1・HTTP/2） |

- HTTP/2 で一番よくある構成（HTTP/1.1 の上流）で応答ヘッダのフィルタが効かない。`h2_proxy` は Wasm のモジュールを h2c・TLS の h2 の経路にだけ渡し、
  HTTP/1.1 の上流の経路（`h2_proxy_http`・`h2_proxy_https` → `h2_relay_backend_response`）には渡していない。
- HTTP/1.1 の経路は、圧縮するかどうか・バッファするかどうか・上流のプロトコルで、フィルタが効いたり効かなかったりする。
- `on_log` などのほかのフェーズは全経路で呼ばれる（`on_request_complete`）。

## 影響

- P1（Proxy-Wasm で応答ヘッダを直すルート。例: 認証の後の `Set-Cookie` の書き換え、内部ヘッダの除去、セキュリティヘッダの付与）:
  クライアントのプロトコルや `Accept-Encoding` によって、フィルタを通らない応答が返る。フィルタを安全のために使っていると素通しになる。

## 修正方針

- 上流の応答の head を作る全経路で、`[route.compression]`・`buffering`・上流のプロトコルにかかわらず `on_http_response_headers` を呼ぶ:
  - HTTP/2: `h2_proxy_http`・`h2_proxy_https` に Wasm のモジュールを渡し、`h2_relay_backend_response` の head を作るところで
    `apply_h2_wasm_response_headers` を呼ぶ（h2c の経路と同じ）。
  - HTTP/1.1: `transfer_compressed_response`・`proxy_request_buffered`・`proxy_h2c` にモジュールを渡す。応答ヘッダのフィルタは
    `compress.rs` の非圧縮の経路にある処理を関数に切り出して共有する（今は行ごとに `format!` している。F-202 と同じく確保を減らす）。
- フィルタが `content-length` を変える・`content-encoding` を消すときの扱いを、圧縮の前後どちらで呼ぶか含めて決める
  （Envoy は圧縮フィルタより前に Wasm の応答ヘッダが来る構成が普通。圧縮前のヘッダを見せ、圧縮後に `content-length` を直す）。
- モジュールの無いルートは今と同じ（`is_empty()` の分岐だけ）。
- 試験: E2E で HTTP/1.1（圧縮あり・なし・`buffering = "full"`）・h2c・HTTP/2・HTTP/3 × 上流 HTTP/1.1・HTTPS・h2c で `X-Veil-Processed` が付くこと。
  `tests/proxy_wasm_conformance.rs` にもプロトコルの軸を足す。

## 関連

- [F-132](../features/F-132-http3-wasm-parity.md)（HTTP/3 の Wasm を HTTP/1.1・HTTP/2 にそろえた）、[F-271](../features/F-271-proxy-wasm-request-body-h1-h2-and-perf.md)
  （要求本文のフェーズを HTTP/1.1・HTTP/2 に足した）、[F-285](../features/F-285-wasm-response-body-h1-h2.md)（応答本文のフェーズ）、
  [B-267](B-267-response-header-policy-missing-on-proxy-paths.md)（同じ経路の `add/remove_response_headers`）
