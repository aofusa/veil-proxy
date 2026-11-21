# F-313: HTTP/3 でも Proxy-Wasm のトレーラの段（`on_http_request_trailers`・`on_http_response_trailers`）を走らせる

**状態: 未着手（2026-11-21 起票。機能 × プロトコルの網羅性の確認、feat/protocol-parity）**

## 今の状態

- HTTP/2 は gRPC（h2c・h2 の上流）の応答のトレーラを `on_http_response_trailers` に通し（`grpc-status`・`grpc-message` を
  書き換えられる）、クライアントが送った要求のトレーラを `on_http_request_trailers` に通す（`wasm.md` の F-133）。
- HTTP/3 はどちらも走らせない。要求のトレーラは HEADERS の 2 つ目の節として受けて捨て（`http3_server/mod.rs`）、
  応答のトレーラはそのままクライアントへ送る。同じモジュールが HTTP/3 の gRPC には効かない。

## 改修内容

- HTTP/3 のバッファ経路（Wasm のあるルートはバッファ経路）で、上流の応答のトレーラを送る前に `on_response_trailers_with_modules`
  へ通す（HTTP/2 の `apply_h2_wasm_response_trailers` と同じ扱い。止める・ローカル応答の扱いも同じ）。
- 要求のトレーラを、Wasm のあるルートのバッファ経路のストリームでは捨てずに溜め、本文の後に `on_request_trailers_with_modules`
  へ通す（上流へは送らない。HTTP/2 と同じ。F-133 の調査のとおりどの経路にも要求のトレーラを送る口が無い）。
- モジュールの無いルートに費用を足さない（トレーラが無ければ分岐だけ）。

## 受け入れ条件

- E2E または統合: `grpc-status` を書き換えるモジュール（既存の F-133 の試験のモジュール）が HTTP/3 の gRPC の応答にも効く。
  要求のトレーラを送る HTTP/3 のクライアントで `on_http_request_trailers` が呼ばれる。
- 文書（wasm、英日）。

## 関連

- [F-133](F-133-l4-grpc-wasm.md)・[B-268](../bugs/B-268-wasm-response-headers-skipped-on-some-paths.md)
