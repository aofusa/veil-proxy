# B-481: HTTP/2 のストリーミングの要求で上流を 2 回選び、ラウンドロビン・重み付けが偏る

**状態: 未着手（2026-11-21 起票。機能 × プロトコルの網羅性の確認、feat/protocol-parity）**

## 出典

機能とプロトコルの対応の確認（`docs/artifacts/protocol-parity/audit.md` の #1）。

## 事象

- HTTP/2 で本文が HEADERS の後から届く要求（アップロード・gRPC のストリーミング等）は、`h2_route_streaming_plan` が
  上流が HTTP/2 か（gRPC を中継できるか）を決めるために `upstream_group.select(client_ip)` を呼び、続く
  `h2_serve_streaming` が `select_with_header_fn` でもう一度選ぶ。
- `RoundRobin`・`Weighted` は選ぶたびに共有のカウンタ（`rr_counter`）を進めるので、1 要求でカウンタが 2 進む。
  2 台の上流なら、続けて届くストリーミングの要求は全部同じ上流へ行く（偶数台なら半分の上流しか使わない）。
  HTTP/1.1・HTTP/3 は 1 要求で 1 回だけ選ぶ。
- 計画の側は Consistent Hash の `header:`・`cookie:` キーを見ずに `client_ip` で選ぶので、HTTP/2 か否かの判断を
  実際に送る上流とは別の上流で行うこともある。
- HTTP/3 も、分類（`classify`）で上流を選んだ後にバッファ経路へ落ちる場合（gRPC を HTTP/1.1 の上流へ送る構成・
  `http2` feature の無いビルド）、`handle_proxy` が選び直してカウンタを 2 進める。

## 改修案

- 計画で選んだ上流をそのままストリーミングのタスクへ渡し、`h2_serve_streaming` では選び直さない（HTTP/3 の
  `BackendTaskParams` と同じ形）。計画でも `select_with_header_fn` を使う。
- HTTP/3 の分類で選んだ後にバッファ経路へ落とすときも、選んだ上流をバッファ経路へ渡して選び直さない。

## 受け入れ条件

- 単体: 2 台のラウンドロビンへ HTTP/2 のストリーミングの要求を続けて送ると、両方の上流へ交互に届く。
- 既存の E2E・統合テストが通る。

## 関連

- [B-272](B-272-circuit-breaker-outlier-h1-only.md)（`record_request` の共有）・[F-97](../features/F-97-http3-grpc-app-layer-coverage.md)
