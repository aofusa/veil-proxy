# F-57: container_security CI/CD 統合

## 目的

`tools/container_security/run.sh` を GitHub Actions 等に組み込み、nightly でフルスイートを実行する。

## 改修案

- PR: ゲート（fuzz ブラックボックス + h2spec ゲート + 短時間 chaos）
- nightly: `H2SPEC_FULL=1`、libFuzzer 長時間、cargo-audit
- Artifact: `results/`、h2spec JUnit、Trivy レポート
- `SKIP_*` / `H2SPEC_*` 環境変数のドキュメント化

## 受け入れ条件

- docker 必須の workflow がグリーン
- 失敗時に results artifact が残る