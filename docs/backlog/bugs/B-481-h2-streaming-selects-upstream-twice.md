# B-481: HTTP/2 のストリーミングの要求で上流を 2 回選び、ラウンドロビン・重み付けが偏る

**状態: 修正済み（feat/protocol-parity。2026-11-21 起票・2026-11-22 修正。機能 × プロトコルの網羅性の確認）**

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

## 対応（2026-11-22）

- 改修案（選んだ上流を渡す）は採らなかった。下見（`h2_route_streaming_plan`）・分類（HTTP/3 の `classify`）は
  `draw = false` で経路を解決するので、トラフィックの分割のあるルートでは下見の上流と本番の上流が別のグループになり得る。
- 代わりに下見では上流を選ばない。gRPC をストリーミングにしてよいかは、グループのどのサーバが選ばれても HTTP/2 で
  中継できるか（`UpstreamGroup::all_servers_h2`。グループの `use_h2c`、またはサーバ全部が h2c・ALPN の h2）で決める。
  上流を選ぶのは本番の 1 回だけ（`h2_serve_streaming`・HTTP/3 のストリーミング／バッファ経路）。
- HTTP/3 の分類も、選ぶ前に分かる「バッファ経路へ回す」条件（gRPC を HTTP/1.1 の上流へ送る構成・`http2` feature の
  無いビルドの h2c の上流）を選ぶ前に判定する。選んだ後に残る判定（HTTP/1.1 だけと分かっている HTTPS の上流の gRPC）は
  その上流の性質で決まるので、選び直しても結果は同じ（カウンタが 2 進むのはこの稀な構成だけ）。
- 自己完結型 E2E: `b481_round_robin_alternates_for_streaming_requests`（h2c・HTTP/2・HTTP/3 で、本文を後から送る
  アップロードと gRPC を 4 回ずつ送ると 2 台へ 2 回ずつ届く）。単体: `all_servers_h2` がカウンタを進めない。

## 受け入れ条件

- 単体: 2 台のラウンドロビンへ HTTP/2 のストリーミングの要求を続けて送ると、両方の上流へ交互に届く。
- 既存の E2E・統合テストが通る。

## 関連

- [B-272](B-272-circuit-breaker-outlier-h1-only.md)（`record_request` の共有）・[F-97](../features/F-97-http3-grpc-app-layer-coverage.md)
