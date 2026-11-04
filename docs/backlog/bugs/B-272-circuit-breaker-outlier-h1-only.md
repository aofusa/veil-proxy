# B-272: サーキットブレーカー・外れ値の排除（passive）が HTTP/1.1 のクライアントの要求でしか動かない

**状態: 未着手（2026-11-04 起票）**

## 出典

[B-266](B-266-head-local-responses-send-body.md) と同じ調査（メトリクスの記録箇所を数えていて、`set_circuit_breaker_state` が `proxy/forward.rs` にしか無いことから気づいた）。
コードで確かめた（実機の再現はまだ）。

## 事象

- 上流の 1 要求の成否と遅延を `UpstreamGroup::record_outcome` に渡すと、サーバごとのサーキットブレーカー（`record_success`・`record_failure`）、
  外れ値の排除（`outlier_detection` のエラー率による `eject_for`）、EWMA の遅延が更新される（F-06）。
- `record_outcome` を呼ぶのは `proxy::forward::handle_proxy`（HTTP/1.1 のクライアント。上流 HTTP/1.1・HTTPS・h2c・TLS の h2 とも）だけ。
  HTTP/2（`h2_proxy`・`h2_serve_streaming`）・HTTP/3（`request_task::handle_proxy`・`http3_stream` のバックエンドタスク）は
  `acquire`・`release`（least_conn の接続数）だけで、成否を記録しない。
- サーバの選択（`candidates()`）は開いたブレーカー・排除中のサーバを避けるので、HTTP/1.1 の要求が無いと、
  - 上流が 5xx・接続失敗を返し続けてもブレーカーが開かず、外れ値も排除されない。
  - HTTP/1.1 の要求で開いたブレーカーが、HTTP/2・HTTP/3 の成功では閉じない（半開の試行が HTTP/1.1 の要求でしか進まない）。
- `veil_circuit_breaker_state` も HTTP/1.1 の経路でしか更新しない（[B-273](B-273-metrics-recorded-on-some-protocols-only.md)）。
- 別件: EWMA の遅延（`avg_latency_ms`）はどこからも読まれない（ガイドの「EWMA latency tracking」は記録だけ）。

## 影響

- P1（`[upstreams.*.circuit_breaker]`・`outlier_detection` を有効にし、ブラウザ（HTTP/2・HTTP/3）や gRPC（HTTP/2）を受けるサイト）:
  ガイド（`load-balancing.md` の Circuit Breaker & Resilience）が書く障害の連鎖を防ぐ仕組みが、HTTP/2・HTTP/3 の要求では働かない。

## 修正方針

- HTTP/2・HTTP/3 のプロキシの全経路で、上流の応答の結果（ステータス・接続失敗・タイムアウト）が決まったところで `record_outcome` を呼ぶ。
  HTTP/1.1 と同じく 5xx と接続・応答の失敗を失敗とする。gRPC は HTTP のステータスが 200 なので、`grpc-status` を失敗に数えるかを決める
  （Envoy の outlier detection は gRPC のステータスを `UNAVAILABLE` 等で失敗に写す設定がある）。
- サーバの添字は `select_with_key` が返すときに一緒に返す（`servers.iter().position` で探し直さない）。時刻は粗い時計（`coarse_now`）で、
  要求ごとの syscall を増やさない。
- EWMA の遅延は、使う負荷分散（例: least_latency / P2C）を足すまでガイドから消すか「記録のみ」と書く。
- 試験: E2E で h2c・HTTP/2・HTTP/3 の要求だけで、5xx を返す上流のブレーカーが開いて別のサーバへ寄る・時間が経って半開で戻ること、
  外れ値の排除が効くこと。

## 関連

- [F-06](../features/resilience-outlier-detection.md)（サーキットブレーカー・外れ値の排除）、[B-273](B-273-metrics-recorded-on-some-protocols-only.md)、[B-266](B-266-head-local-responses-send-body.md)
