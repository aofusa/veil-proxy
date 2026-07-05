# F-80: 回帰コーパスとしての既知クラッシュ固定

親: [F-72](F-72-security-testing-further-hardening.md) 項目 6 / [F-71](F-71-asan-corpus-fuzzing.md)。

## 目的

既知クラッシュ・境界入力を fuzz コーパス / 単体テストに固定し、修正後の回帰を防ぐ。

## 現状

- B-21（HPACK Huffman シフト panic）: 回帰単体テスト
  `huffman::tests::test_huffman_decode_invalid_no_panic` で固定済み。
- B-22（パスワイルドカード境界）: `routing::tests::test_path_wildcard_boundary_*` +
  `tests/routing_proptest.rs` で固定済み。

## 改修案

- 各 fuzz ターゲットの `corpus/<target>/` に既知クラッシュ再現入力を seed として追加
  （[F-71](F-71-asan-corpus-fuzzing.md) の永続コーパス機構を利用）。
- 新規クラッシュ検出時は「単体テスト固定 + corpus seed 追加」を定型フローとして
  SECURITY.md に記載。

## 受け入れ条件

- 既知クラッシュ入力が corpus seed として存在し、`cargo fuzz run` の第 1 コーパスに含まれる。
- 対応する回帰単体テストが `cargo test` で通過。
