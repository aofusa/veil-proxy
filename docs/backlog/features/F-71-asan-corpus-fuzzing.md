# F-71: ASAN ファジングパイプライン + Corpus 永続化

出典: `security_chaos_fuzzing_report.md` §2.1.1・§2.1.2。親: [F-52](F-52-cargo-fuzz-libfuzzer.md)。

## 目的

`unsafe`（io_uring/kTLS/ゼロコピー周辺）のメモリ不正使用（heap-buffer-overflow・
use-after-free 等）を AddressSanitizer 併用で検出し、発見シードをコーパスとして
回次間で再利用してカバレッジを漸増させる。

## 実装済み

- `tools/container_security/fuzz/run_libfuzzer_asan.sh`（既定 `SKIP_LIBFUZZER_ASAN=1`）。
  - `cargo fuzz run --sanitizer address` を全ターゲットに適用。
  - 永続コーパス `tools/container_security/fuzz/corpus/<target>/` をマウントし
    第 1 コーパスディレクトリとして渡す（発見シードが蓄積・再利用される）。
- `.gitignore` でコーパス実体は除外しつつ `corpus/.gitkeep` で構造を維持。

## 実装済み（追加・2026-07-05）

- **TSAN パイプライン**: `tools/container_security/fuzz/run_libfuzzer_tsan.sh`
  （`cargo fuzz run --sanitizer thread`、既定 `SKIP_LIBFUZZER_TSAN=1`）を新設し、
  `run.sh` フェーズ 1d + `report.sh` に配線。競合が起こり得るターゲット
  （`hpack_decode`/`http2_frame_decode`）に限定。永続 corpus + 回帰 seed も共有。
- **回帰 seed の version-control（[F-80](F-80-regression-corpus.md)）**: `fuzz/regression_corpus/`
  を新設し、ASAN/TSAN/通常 fuzz ランナーが起動時に永続コーパスへ複製する
  （`SEED_REGRESSION_CORPUS=1`）。B-21 クラッシュ入力を seed 固定。
- **CI 統合（[F-57](F-57-container-security-ci.md)）**: nightly workflow の `workflow_dispatch`
  入力で ASAN を選択実行、`results/` を artifact 保存。

## 残件

- **MSAN**: MemorySanitizer（未初期化読み）は instrumented std（`-Zbuild-std`）が必須のため
  別途対応（TSAN より重い）。
- コーパスの外部永続化（Git LFS/S3）での回次間シード還流（現状はランナー内 volume）。

## 受け入れ条件

- `SKIP_LIBFUZZER_ASAN=0 ... run_libfuzzer_asan.sh` が ASAN 有効でクラッシュ無く完走し、
  コーパスが `corpus/<target>/` に蓄積されること。
