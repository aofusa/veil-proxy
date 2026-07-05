# 回帰コーパス（F-80）

修正済みクラッシュ・境界入力を **version-controlled な seed** として固定する場所。
実行時コーパス（`tools/container_security/fuzz/corpus/<target>/` および cargo-fuzz 既定の
`fuzz/corpus/<target>/`）は `.gitignore` されており永続化されないため、回帰を確実に
再現させたい既知入力はここに置き、fuzz ランナーが起動時にコーパスへ複製する。

## レイアウト

```
fuzz/regression_corpus/<target>/<name>
```

`<target>` は fuzz ターゲット名（`hpack_decode` 等）、`<name>` は再現入力ファイル
（生バイト列）。ファジング入力そのものなので拡張子は付けない。

## 現在の seed

| ターゲット | seed | 由来 |
|-----------|------|------|
| `hpack_decode` | `b21_huffman_shift_panic` | [B-21](../../docs/backlog/bugs/B-21-hpack-huffman-decode-shift-panic.md) の cargo-fuzz クラッシュ入力（Huffman シフト panic） |
| `hpack_decode` | `b21_all_zero_64` | B-21 関連: 符号長超過をまたぐ全ゼロ列 |

## 使い方

`tools/container_security/fuzz/run_libfuzzer.sh` / `run_libfuzzer_asan.sh` は
実行前にこのディレクトリの seed を対象コーパスへ複製する（`SEED_REGRESSION_CORPUS=0`
で無効化可能）。ローカルでは:

```bash
cp -n fuzz/regression_corpus/hpack_decode/* fuzz/corpus/hpack_decode/ 2>/dev/null || true
cargo fuzz run hpack_decode
```

## 新規クラッシュ検出時のフロー

1. クラッシュ再現入力を単体テストへ固定（例: `huffman::tests::test_huffman_decode_invalid_no_panic`）。
2. 同じ入力を `fuzz/regression_corpus/<target>/` へ追加。
3. backlog に bug チケットを起票し、修正コミットで両方を含める。
