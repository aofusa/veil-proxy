# B-251: 上流への接続失敗・接続タイムアウトを要求ごとに `error!` で出す（ログ洪水）

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27 のセキュリティレビュー 2 回目の LG-01。

## 事象

`src/proxy/forward.rs` L843（`Proxy connect error to {}: {}`）・L849（`Proxy connect timeout to {}`）は要求ごとに `error!` を出す。
上流が落ちている間、要求レートと同じ速度でログが増える（毎秒数万行）。AGENTS.md の「リクエストごとのログを `info!` 以上で出さない」に反し、
B-235（不正 UDP データグラムごとの `warn!`）と同種。ftlog の有界チャネルが詰まるとログ全体が遅れる。

## 改修案

- 上流単位（アドレスごと）の状態変化（healthy → unhealthy、復旧）だけを `warn!`/`error!` で出し、要求ごとの失敗は `debug!` + メトリクス
  （`veil_upstream_connect_errors_total{upstream}`）にする。既存のヘルスチェッカー（F-156）に状態があるならそれを使う。
- 同じ経路の `error!("No healthy upstream servers available")`（L344）も同じ。

## 修正（v0.10.0）

- `src/logging.rs` に呼び出し箇所ごとの間引き（`LogThrottle` と `log_throttled!`。atomics 2 個、失敗の経路だけで時刻を読む）を足し、上流への接続の失敗・タイムアウト・TLS の失敗・使える上流が無いときのログ（`forward.rs`・`https.rs`・`websocket.rs`・`h1.rs`・`h2.rs`・`http2/upstream_mux.rs`・`http3_stream.rs`、H2C の要求エラー）を **10 秒に 1 行**（間引いた件数を添える）にし、残りは `debug!`。
- メトリクス `veil_upstream_connect_errors_total{kind}`（`error`・`timeout`・`tls`・`no_healthy`）。上流のアドレスはラベルにしない（系列の上限。B-159）。
- 改修案の「上流の状態変化だけを出す」は、ヘルスチェックを設定しない構成でも効くよう箇所ごとの間引きにした（状態変化はヘルスチェッカーが既に出す）。
- 試験: 単体 `logging::log_throttle_tests`。落ちている上流へ 200 要求を送り、502 が 200 件・ログが 1 行・メトリクスが 200 であることを確認。
- 文書: `docs/guide/observability.md`（英日）、`examples/config.toml`、リリースノート。

## 関連

- [B-235](B-235-http3-warn-per-invalid-datagram.md)、[F-282](../features/F-282-security-review-followup-hardening.md)
