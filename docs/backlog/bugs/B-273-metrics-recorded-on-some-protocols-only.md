# B-273: メトリクスの一部が特定のプロトコルでしか数えられず、文書にある 4 系列はどこからも記録されない

**状態: 未着手（2026-11-04 起票）**

## 出典

[B-266](B-266-head-local-responses-send-body.md) と同じ調査。`src/metrics.rs` の記録用の関数ごとに呼び出し元を数えた（コードで確認）。

## 事象

ガイド（`observability.md`）の表のうち、次はプロトコルによって数えない・どこでも数えない。

| メトリクス | HTTP/1.1 | HTTP/2 | HTTP/3 |
|---|---|---|---|
| `veil_grpc_requests_total` | 数えない | 数える（`h2_emit_upstream_response`・`h2_relay_h2_streaming`） | **数えない**（gRPC は h2c の上流へ中継する。B-39・F-171） |
| `veil_proxy_buffering_used_total` | 数える（`forward`・`https`） | **数えない** | **数えない** |
| `veil_circuit_breaker_state` | 更新する（`forward`） | **更新しない**（[B-272](B-272-circuit-breaker-outlier-h1-only.md)） | **更新しない** |
| `veil_proxy_cache_hits_total` 等 | 数える | キャッシュ自体が無い（[F-257](../features/F-257-proxy-cache-h2-h3.md)） | 同左 |
| `veil_retry_total` | **記録しない**（`record_retry` の呼び出しが無い。F-177 の再送も数えない） | 同左 | 同左 |
| `veil_circuit_breaker_open_total` | **記録しない**（`record_circuit_breaker_open` の呼び出しが無い） | 同左 | 同左 |
| `veil_proxy_cache_stores_total` | **記録しない**（`record_cache_store` の呼び出しが無い） | 同左 | 同左 |
| `veil_grpc_stream_duration_seconds` | **記録しない**（`observe_grpc_stream_duration` の呼び出しが無い） | 同左 | 同左 |

- 要求数・遅延・大きさ（`veil_proxy_http_requests_total` 等）はアクセスログの出口（`logging`）で全プロトコル数えている。

## 影響

- P3: ダッシュボード（`examples/grafana/`）・アラートが、クライアントのプロトコルによって 0 のまま、または常に 0 になる。gRPC を HTTP/3 で受けると
  gRPC のパネルに出ない。

## 修正方針

- gRPC の要求数: HTTP/3 の gRPC の中継（`http3_stream` の h2 のタスク・`request_task` のバッファ経路）で、トレーラーの `grpc-status` が決まったところで数える。
- バッファリングの回数: HTTP/2・HTTP/3 で `buffering = "full"` の経路を選んだところで数える。
- 記録しない 4 系列: 記録する箇所を足す（ブレーカーが開いた遷移・F-177 の再送・キャッシュへの保存・gRPC のストリームの終わり）か、使わないならメトリクスと文書から消す。
  Grafana の例が使っているかも確かめる（F-256 の PromQL の試験）。
- 要求ごとの経路は今と同じく、メトリクスが無効なら分岐 1 回だけ（ラベルの文字列を要求ごとに作らない）。
- 試験: `/metrics` を HTTP/1.1・HTTP/2・HTTP/3 の gRPC・バッファリングの要求の後に読み、各系列が増えること。

## 関連

- [B-272](B-272-circuit-breaker-outlier-h1-only.md)、[F-257](../features/F-257-proxy-cache-h2-h3.md)、[F-256](../features/F-256-grafana-dashboard.md)、[F-171](../features/F-171-h2c-upstream-streaming.md)
