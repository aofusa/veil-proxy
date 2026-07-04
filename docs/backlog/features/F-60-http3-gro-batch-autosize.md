# F-60: HTTP/3 GRO 一括 recv・GSO/GRO セグメントサイズ自動調整（F-33 残件）

## 出典

`docs/artifacts/completed_tasks_remaining_work_report.md`（F-33 残タスク）。

## 概要

1. GRO で集約したデータグラムを quiche の `recv` へ **一括** で渡す最適化。
2. GSO / GRO のセグメントサイズを環境に応じて **自動調整**。

## 現状

- quiche に batch recv API が無いため、GRO で束ねても per-segment で `recv` を呼んでいる
  （F-45 で per-segment オーバーヘッドは削減済み）。
- セグメントサイズは固定。

## 改修内容（案）

- quiche の recv 呼び出し前後の借用・境界計算をさらに削り、セグメント境界をスライスで
  ゼロコピー分割して渡す。
- パス MTU / 実測スループットに基づく GSO/GRO サイズの動的調整（上限・下限クランプ付き）。

## 受け入れ条件

- [ ] HTTP/3 E2E が回帰なし。
- [ ] ベンチで per-datagram のコスト削減を確認。

## 依存・リスク

- quiche の API 制約（一括受信不可）。効果は環境依存。P3・任意継続。

## 対応状況: 未着手