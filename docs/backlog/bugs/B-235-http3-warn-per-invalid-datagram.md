# B-235: 不正な UDP のデータグラム 1 通ごとに `warn!` を出していて、ゴミを流し込むだけでログを洪水にできる

**状態: 未修正（2026-10-27 起票。セキュリティレビュー PR-04）**

## 出典

2026-10-27 のセキュリティレビュー（v0.9.2 の `src/http3_server/event_loop.rs` L296 付近の読解）。

## 事象

- `quiche::Header::from_slice` の失敗で `warn!("[HTTP/3] Invalid packet header: {}", e)` を出す。送信元を偽装した UDP のゴミ 1 通ごとに 1 行。
- AGENTS.md の「要求ごとのログを `info!` 以上で出さない」に反する。ディスクと CPU（ftlog のチャネルが詰まると送信側が待つ）を消費させられる。

## 改修案

- `debug!` に下げ、件数は Prometheus のカウンタ（`veil_http3_invalid_packets_total`）で見せる。メトリクス名を足すので `docs/guide/`（英・日）を更新する。
- 同じ経路の `warn!`（`accept` の失敗・`recv` の失敗）も要求ごとでないかを確かめる。

## 受け入れ条件

- UDP のゴミを 10 万通送ってもログが 10 万行にならない。カウンタが増える。

## 関連

- [B-222](B-222-http3-no-connection-limit-no-retry.md)
