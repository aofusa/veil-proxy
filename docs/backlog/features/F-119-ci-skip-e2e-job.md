# F-119: CI（GitHub Actions）の E2E ジョブをスキップ化

## 背景

v0.5.0 リリース準備の一環。PR / push ゲート CI（`.github/workflows/ci.yml`）の
`e2e` ジョブは GitHub ホストランナー上で実行時間が長く、E2E スイートはローカルの
`./tests/e2e_setup.sh test` で実施する運用のため、workflow 上では実行しない方針とする。

## 改修内容

- `.github/workflows/ci.yml` の `e2e` ジョブを **スキップ**させる（`if: false` 等で
  ジョブ定義は残し、再有効化を容易にする）。
- fmt / clippy / feature マトリクスビルド / ユニット・統合テストの各ジョブは維持する。

## 受け入れ条件

- [x] push / pull_request で `e2e` ジョブが実行されない（スキップ扱い）。
- [x] 他ジョブの動作は変更なし。
