# F-57: container_security CI/CD 統合

## 目的

`tools/container_security/run.sh` を GitHub Actions 等に組み込み、nightly でフルスイートを実行する。

## 改修案

- PR: ゲート（fuzz ブラックボックス + h2spec ゲート + 短時間 chaos）
- nightly: `H2SPEC_FULL=1`、libFuzzer 長時間、cargo-audit
- Artifact: `results/`、h2spec JUnit、Trivy レポート
- `SKIP_*` / `H2SPEC_*` 環境変数のドキュメント化

## 実装済み（2026-07-05）

- **`.github/workflows/ci.yml`**（PR/push ゲート）: `rustfmt` / `clippy --features full --tests` /
  feature マトリクスビルド（no-default / default / full）/ ユニット + 統合 + ルーティング
  プロパティ（F-56）テスト / **E2E（`./tests/e2e_setup.sh test`）**。`concurrency` で
  古い実行をキャンセル、`Swatinem/rust-cache` でビルドキャッシュ。
- **`.github/workflows/container-security-nightly.yml`**（nightly cron + 手動）:
  glibc/musl マトリクスで Veil イメージをビルド → `run.sh` を実行（`SKIP_LIBFUZZER=0`・
  `H2SPEC_FULL=1`・`cargo-audit/deny/gitleaks` 有効・`FUZZ_MAX_TIME=300`）。
  `workflow_dispatch` 入力で ASAN/WASM fuzz を選択有効化。**`results/` を `if: always()` で
  artifact 保存**（失敗時も残る）、`suite_summary.json` を Job Summary へ出力。
  別ジョブで **SBOM（F-65）を成果物添付**（retention 90 日）。

## 残件

- ASAN/TSAN の nightly 定期実行はランナー時間の都合で `workflow_dispatch` 既定 off。
  セルフホストランナー確保後に schedule 化を検討。

## 受け入れ条件

- [x] docker 必須の workflow を定義（nightly、glibc/musl マトリクス）。
- [x] 失敗時に results artifact が残る（`if: always()` + upload-artifact）。
- [x] `SKIP_*` / `H2SPEC_*` 環境変数を container_security README に文書化済み。