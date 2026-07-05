# F-80: 回帰コーパスとしての既知クラッシュ固定

親: [F-72](F-72-security-testing-further-hardening.md) 項目 6 / [F-71](F-71-asan-corpus-fuzzing.md)。

## 目的

既知クラッシュ・境界入力を fuzz コーパス / 単体テストに固定し、修正後の回帰を防ぐ。

## 現状

- B-21（HPACK Huffman シフト panic）: 回帰単体テスト
  `huffman::tests::test_huffman_decode_invalid_no_panic` で固定済み。
- B-22（パスワイルドカード境界）: `routing::tests::test_path_wildcard_boundary_*` +
  `tests/routing_proptest.rs` で固定済み。

## 実装済み（2026-07-05）

- **`fuzz/regression_corpus/<target>/`** を新設（version-controlled）。実行時コーパス
  （`.gitignore` 対象で永続化されない）と分離し、既知クラッシュを確実に固定する。
  `fuzz/.gitignore` で live corpus/artifacts は除外しつつ regression_corpus は追跡。
- **seed**: `hpack_decode/b21_huffman_shift_panic`（B-21 の cargo-fuzz クラッシュ入力）と
  `hpack_decode/b21_all_zero_64`（符号長超過をまたぐ全ゼロ列）を固定。
- **ランナー配線**: `run_libfuzzer.sh` / `run_libfuzzer_asan.sh` / `run_libfuzzer_tsan.sh` が
  起動時に regression_corpus の seed を対象コーパスへ複製（`SEED_REGRESSION_CORPUS=1`、
  既存は上書きしない `cp -n`）。cargo-fuzz が第 1 コーパスとして必ず読む。
- **フロー文書化**: `fuzz/regression_corpus/README.md` に「単体テスト固定 + seed 追加 +
  bug 起票」の定型フローを記載。B-21 は `huffman::tests`、B-22 は `routing::tests` +
  `routing_proptest` で単体固定済み。

## 残件

- 上記フローの SECURITY.md への集約（README にはあり）。
- HTTP/2 frame・config など他ターゲットの既知境界入力の seed 追加（随時）。

## 受け入れ条件

- [x] 既知クラッシュ入力が corpus seed として存在し、`cargo fuzz run` の第 1 コーパスに含まれる。
- [x] 対応する回帰単体テストが `cargo test` で通過（B-21/B-22）。
