# B-251: 上流への接続失敗・接続タイムアウトを要求ごとに `error!` で出す（ログ洪水）

**状態: 未修正（2026-10-27 起票）**

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

## 関連

- [B-235](B-235-http3-warn-per-invalid-datagram.md)、[F-282](../features/F-282-security-review-followup-hardening.md)
