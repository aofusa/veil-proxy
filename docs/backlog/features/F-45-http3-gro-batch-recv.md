# F-45: HTTP/3 GRO 一括 recv とセグメントサイズ調整（F-33 残件）

- **優先度**: P3
- **対応状況**: 完了（2026-07-02）

## 完了メモ

- quiche の `recv` は 1 データグラム単位 API のため「一括で渡す」ことは API 上不可能。per-segment オーバーヘッドの削減として実装:
  - `connections` の RefCell 借用をセグメントごと 2 回 → **GRO バッチ全体で 1 回**に削減。
  - GRO はカーネルが同一フローを集約する性質を利用し、直前セグメントと同一 DCID の場合は新規接続判定（`contains_key` + Initial 検査）をスキップ。
- GSO 送信セグメントサイズは quiche が PMTU に応じて返す write サイズをそのまま均一バッチ化しており（`send_pending_packets`）、サイズ調整は quiche のパス MTU 探索に追従する設計で既に満たされている（固定値の手動調整は不要）。
- HTTP/3 E2E 33 件全通過。
- **出典**: `docs/artifacts/remaining_tasks_analysis.md` F-33 残件

## 機能説明・現状

F-33 で GSO 送信バッチングと GRO 受信の配線は完了したが、GRO で集約したデータグラムは現在セグメント毎に `quiche::Connection::recv` へ渡している。また GSO/GRO セグメントサイズは固定。

## 改修内容

- GRO 集約バッファをセグメント境界で分割しつつ、`quiche` の recv へ一括で渡せる形に最適化する（per-segment のオーバーヘッド削減）。
- パス MTU / ペーシング情報からセグメントサイズを自動調整する。

## 受け入れ条件

- HTTP/3 E2E が通過し、GRO 有効環境で受信スループットが退行しない。

## 依存・リスク

- quiche の API 制約（recv は 1 データグラム単位）のため、実質は呼び出しループの最適化・コピー排除が中心になる。
