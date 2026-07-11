# F-100: test_coverage_report 追加テストの CI 組み込み

## 概要

[F-99](F-99-test-coverage-report-h3-grpc.md) で追加する E2E / `tools/container_security`
プローブを CI（GitHub Actions 等）へ配線する。

**F-99 本体のテスト実装とは分離**する。F-99 作業では本チケットの実装を行わない。

## 改修内容（予定）

1. `.github/workflows/ci.yml` または nightly ワークフローで F-99 追加 E2E が走ることを確認
2. `container-security-nightly.yml` で `h3_grpc_*` / `h3_grpc_web_*` が実行されること
3. 必要なら Job Summary / artifact へ結果添付
4. フレーキー対策（タイムアウト・再試行方針）の文書化

## 受け入れ条件

- [ ] F-99 の新規プローブが nightly / 該当ジョブで実行される
- [ ] 失敗時にレポートが artifact として残る
- [ ] 既存 CI 時間予算を著しく悪化させない（必要なら nightly のみ）

## 対応状況

未着手。**本作業（F-99）では対象外。**
