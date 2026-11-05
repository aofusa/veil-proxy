# B-300: Proxy-Wasm の Pause を待たず、応答ヘッダの段のローカルレスポンスを無視する（全プロトコル・メッシュ）

**状態: 未修正（2026-11-05 起票。コードレビューで発見。行番号は f8dc80de）**

## 事象（コードの読み取り）

- **Pause を待たない**: HTTP/1.1（`src/proxy/h1.rs:1025`）・HTTP/2（`h2.rs:1658`）・HTTP/3（`http3_server/request_task.rs:968`）・メッシュ（`mesh/wasm.rs:128`）は
  Pause を受けても続ける。本文の段の Pause は元の本文のまま続く（`http_executor.rs:61-67,94-100`）。gRPC の呼び出しを待つ認可フィルタは常に素通しで、
  その結果は要求に影響できない新しいインスタンスへ届く。本文の段で出した `http_call` の結果は捨てられる（`server.rs:612-619`）。HTTP/3 だけ Pause のとき
  要求本文の段を飛ばす（HTTP/1.1・HTTP/2 と違う）。要求ごとに `warn!` が出る。
- **応答ヘッダの段のローカルレスポンス**: `sendfile.rs:895`・`h2_request_body.rs:121`・`http3_server/upstream.rs:48`・`mesh/wasm.rs:156` で無視される。文書の段の表にも無い。
- HTTP/2 の要求トレーラーの段のローカルレスポンスは `on_log` を飛ばす（`h2.rs:1696-1713`）。

## 改修案

- Pause を待つ: `dispatch_http_call`・gRPC の呼び出しの完了で同じインスタンスを resume するまで要求を止める（HTTP/3 はメインループで待たず B-97 の
  `FuturesUnordered` に置く）。待つ上限（`http_call` の timeout）を超えたら失敗としてポリシー（[F-293](../features/F-293-wasm-failure-policy.md)）。待てない段
  （ストリーミングの途中等）の Pause は `unsupported_pause` の失敗として数える。全プロトコルで同じ段を同じ順に呼ぶ。
- 応答ヘッダの段のローカルレスポンスを、応答を送る前なら置き換えて送る。
- 要求ごとの `warn!` をやめる（`debug!` か `log_throttled!`）。トレーラーの段でも `on_log` を呼ぶ。
- 試験: http_call で Pause → 結果で 403・200 を返すフィルタ（`tests/` の既存の F-62 のモジュール）を HTTP/1.1・HTTP/2・HTTP/3 で。応答ヘッダの段で 403 を返すフィルタ。

## 関連

- [F-293](../features/F-293-wasm-failure-policy.md)、[B-299](B-299-wasm-fail-closed-bypasses.md)、[B-56](B-56-wasm-http-call-resume-epoch-expiry.md)（F-62 の Pause/resume）
