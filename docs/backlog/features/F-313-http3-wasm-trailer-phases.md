# F-313: HTTP/3 でも Proxy-Wasm のトレーラの段（`on_http_request_trailers`・`on_http_response_trailers`）を走らせる

**状態: 完了（feat/protocol-parity。2026-11-21 起票・2026-11-22 実装。機能 × プロトコルの網羅性の確認）**

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

## 対応（2026-11-22）

- 要求のトレーラ: HTTP/3 のトレーラの節を、バッファ経路の要求（`BufferedReq.trailers`）では捨てずに取っておく。見出しと同じ束で
  届いて分類の前のものは接続の置き場（`unclassified_trailers`）に置き、分類でバッファ経路になったら移し、ほかは捨てる。要求の
  タスク（`H3BufferedTask.req_trailers`）が本文の段の後に `on_request_trailers_with_modules` を呼び、ローカル応答なら上流へ送らずに
  返す（上流へは送らない。HTTP/2 と同じ）。
- 応答のトレーラ: 上流（h2c・TLS 上の h2）の応答のトレーラを、本文の段の後・ヘッダへのマージと gRPC のトレーラの送出の前に
  `apply_h3_wasm_response_trailers` へ通す。ローカル応答は HTTP/2 と同じく使わず、元のトレーラのまま送って警告を出す。
- 順序: HTTP/2 は要求のトレーラの段を本文の段の前に呼んでいた。Proxy-Wasm の順（ヘッダ → 本文 → トレーラ）にそろえて、
  HTTP/2 も本文の段の後に呼ぶ。
- モジュールの無いルート・トレーラの無い要求と応答は分岐だけ（トレーラの置き場は要るときだけ確保する）。
- 試験: 自己完結型 E2E `f313_wasm_trailer_phases_on_http2_and_http3`（h2c・HTTP/2・HTTP/3。F-133 の
  `grpc_trailer_filter` に `x-wasm-reject: 1` の要求のトレーラを 403 で断る段を足した）。応答のトレーラの書き換え、要求の
  トレーラ（見出しと同じ束・300ms 後）での拒否。HTTP/3 の変更を戻すと HTTP/3 は `grpc-status: 5`・`original` を受けて失敗する
  ことを確かめた。
- 文書: wasm（英日。フェーズの表に `on_http_response_trailers` の行を足し、HTTP/3 を埋めた）。

## 関連

- [F-133](F-133-l4-grpc-wasm.md)・[B-268](../bugs/B-268-wasm-response-headers-skipped-on-some-paths.md)
