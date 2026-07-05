# B-18: HTTP/3 GSO バッチが sendmsg の UDP ペイロード上限を超え EMSGSIZE でバッチ全体が破棄され得る

## 出典

F-60（GSO/GRO セグメントサイズ自動調整）の実装中にコードレビューで検出（2026-07-05）。
**同日修正済み。**

## 事象

`send_pending_packets`（`src/http3_server.rs`）の GSO バッチ構築は
`MAX_GSO_SEGMENTS = 64` 個までパケットを連結してから `send_gso_async`
（1 回の `sendmsg(UDP_SEGMENT)`）で送出するが、**合計バイト数の上限チェックが無かった**。

- 64 セグメント × 1350B（デフォルトセグメントサイズ）= 86,400B
- UDP `sendmsg` のペイロード上限は 65,507B（65535 − 8(UDP) − 20(IPv4)）

このため大きなレスポンスの送信バースト（1 コネクションで 48 個以上の
フルサイズパケットが同時ペンディング）で `sendmsg` が **EMSGSIZE** を返し、
バッチ内の全パケット（最大 64 個）が送信されないまま警告ログのみで破棄される。
QUIC のロス検出・再送で回復はするが、帯域・レイテンシを大きく浪費する
（バースト時に毎回発生し得る）。

## 修正内容（2026-07-05）

- `MAX_GSO_BATCH_BYTES = 65507` を新設し、バッチへの追加**前**に
  「追加すると合計が上限を超える」場合は現在のバッチを先に flush する。
- flush 判定を純関数 `gso_batch_must_flush_before_append`（均一サイズ要求 +
  バイト上限）へ抽出し、単体テスト `test_gso_batch_flush_rules` で
  境界値（65507 ちょうど / +1）を含めて検証。

## 関連

- 親: [F-60](../features/F-60-http3-gro-batch-autosize.md)（GSO/GRO 自動調整の実装中に検出）
- 参考: F-33（GSO/GRO オフロード導入）、F-45（GRO バッチ受信最適化）
