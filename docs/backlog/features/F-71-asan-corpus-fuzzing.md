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

## 残件

- **TSAN/MSAN**: ThreadSanitizer（データ競合）・MemorySanitizer（未初期化読み）は
  nightly + 追加設定が必要。ASAN で基盤を確立後に検討。
- コーパスの外部永続化（Git LFS/S3）と CI（[F-57](F-57-container-security-ci.md)）
  nightly での長時間実行・シード還流。
- 実行して顕在化したクラッシュは bugs/ に起票（本 PR 時点では未実行＝ビルド重量のため）。

## 受け入れ条件

- `SKIP_LIBFUZZER_ASAN=0 ... run_libfuzzer_asan.sh` が ASAN 有効でクラッシュ無く完走し、
  コーパスが `corpus/<target>/` に蓄積されること。
