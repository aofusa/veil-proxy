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

## 実装（2026-07-05）

1. **GRO 一括 recv**: F-45 で既に「バッチ全体で RefCell 借用 1 回・DCID 連続一致時の
   ルックアップスキップ・セグメント境界のスライスゼロコピー分割」まで最適化済みであり、
   quiche の `recv` API が 1 データグラム単位である制約上、これが到達可能な最適点で
   あることを確認（追加変更なし）。
2. **GSO セグメントサイズ自動調整**（`send_pending_packets`）:
   - 送信バッファを固定 1350B から上限クランプ長（`MAX_UDP_SEND_PAYLOAD` = 65507）で確保し、
     per-connection で quiche の PMTU 探索結果 `Connection::max_send_udp_payload_size()`
     （ハンドシェイク中 1200 → 検証後は経路 MTU と設定 `max_udp_payload_size` の小さい方）
     にスライスして追従。下限クランプは `MIN_UDP_SEND_PAYLOAD` = 1200（RFC 9000）。
   - これにより `max_udp_payload_size` を 1350 超（ジャンボフレーム等）に設定した場合も
     GSO セグメントが設定値まで自動で大きくなる。
3. **B-18 検出・修正**: GSO バッチ合計バイトの上限チェック欠落（64 セグメント × 1350B =
   86.4KB > sendmsg 上限 65507B → EMSGSIZE でバッチ全体破棄）を検出し、
   `MAX_GSO_BATCH_BYTES` による事前 flush で修正。flush 判定を純関数
   `gso_batch_must_flush_before_append` へ抽出し単体テストで境界値を検証。
   詳細: [B-18](../bugs/B-18-http3-gso-batch-emsgsize-overflow.md)

テスト: 単体 `test_gso_batch_flush_rules`、HTTP/3 E2E 35 件回帰なし。

## 対応状況: 完了（2026-07-05）