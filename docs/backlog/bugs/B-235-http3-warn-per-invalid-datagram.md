# B-235: 不正な UDP のデータグラム 1 通ごとに `warn!` を出していて、ゴミを流し込むだけでログを洪水にできる

**状態: 修正済み（v0.10.0）**

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

## 修正（v0.10.0）

- 不正なパケットのヘッダ（`quiche::Header::from_slice` の失敗）・`conn.recv` の失敗・不正な要求（RFC 9114 §4.1.2）・h3 の poll の失敗を
  `debug!` に下げた。どれも相手（偽装した送信元を含む）がデータグラム・要求ごとに起こせる。
- 不正なデータグラムの数をメトリクス `veil_http3_invalid_packets_total` で出す（`docs/guide/observability.md` 英・日）。
- 送信の失敗（`sendmsg`・`sendmmsg` の flush）は自分の側の問題なので `warn!` のまま。

## 関連

- [B-222](B-222-http3-no-connection-limit-no-retry.md)
