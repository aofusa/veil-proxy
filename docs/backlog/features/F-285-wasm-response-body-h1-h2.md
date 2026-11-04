# F-285: Proxy-Wasm の `on_http_response_body` を HTTP/1.1・HTTP/2 でも呼ぶ（今は HTTP/3 だけ）

**状態: 未着手（2026-11-04 起票）**

## 今の状態

- 応答本文のフェーズ（`wasm::http_executor::apply_wasm_response_body`）を呼ぶのは HTTP/3 のバッファ経路（`request_task::handle_proxy`）だけ
  （F-132）。HTTP/1.1・HTTP/2 は呼ばない。ガイドの表（`wasm.md` の Filter Phases by Protocol）にも「–」と書いてある。
- 要求本文のフェーズは F-271 で HTTP/1.1・HTTP/2 にもそろえた。応答本文だけ残っている。
- 同じモジュール・同じ設定で、クライアントのプロトコルによって応答本文の検査・書き換え（WAF の応答の検査、本文中の秘密の伏せ字、本文の差し替え）が
  効いたり効かなかったりする。`allow_response_body_read`・`allow_response_body_write` の権限は全プロトコルで受け付ける。

## 改修内容

- `allow_response_body_read` のモジュールがあるルートでだけ、HTTP/1.1・HTTP/2 の上流の応答本文を全部溜めて `on_http_response_body` を
  `end_of_stream = true` で 1 回呼ぶ（HTTP/3・F-271 と同じ）。本文の上限は要求本文と同じく設定で決め、超えたら呼ばずに流すか 502 にするかを決める。
- 書き換えたら `content-length` を直し（B-46 と同じ）、chunked で来た応答は長さを付けて返す。圧縮（`[route.compression]`）は書き換えの後にかける。
- ローカル応答（`send_local_response`）はヘッダを送る前なら差し替え、HEAD・204・304 では本文を付けない（B-188）。
- そのモジュールの無いルートはストリーミング・`sendfile`/`splice` のまま（要求ごとの確保を足さない）。上流のプロトコル（HTTP/1.1・HTTPS・h2c・
  TLS の h2）すべてで同じに動くこと（[B-268](../bugs/B-268-wasm-response-headers-skipped-on-some-paths.md) の経路の漏れを先に直す）。
- 文書: `wasm.md` の表（英日）。

## 受け入れ条件

- E2E: 応答本文を書き換えるモジュールで、HTTP/1.1（平文・TLS・パイプライン）・h2c・HTTP/2・HTTP/3 × 上流 HTTP/1.1・h2c の応答が同じ本文・
  正しい長さで返る。`tests/proxy_wasm_conformance.rs` にプロトコルの軸を足す。
- 交互 A/B: モジュールの無いルートの性能が変わらない。

## 関連

- [F-132](F-132-http3-wasm-parity.md)、[F-271](F-271-proxy-wasm-request-body-h1-h2-and-perf.md)、[B-268](../bugs/B-268-wasm-response-headers-skipped-on-some-paths.md)、
  [B-188](../bugs/B-188-wasm-local-response-head-body.md)、[B-46](../bugs/B-46-http3-buffered-proxy-body-stall.md)
