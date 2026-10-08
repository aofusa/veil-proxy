# F-98: HTTP/3・gRPC アプリ層テストの CI 組み込み

## 概要

[F-97](F-97-http3-grpc-app-layer-coverage.md) で追加する E2E / `tools/container_security`
プローブを CI（GitHub Actions 等）へ配線する。

**F-97 本体のテスト実装とは分離**する。F-97 作業では本チケットの実装を行わない。

## 改修内容

1. `.github/workflows/ci.yml` または nightly ワークフローで F-97 追加 E2E が走ることを確認
2. `container-security-nightly.yml` で S-H3-14/15・gRPC Path Bypass・WASM Crash・gRPC-Web Base64 DOS が実行されること
3. 必要なら Job Summary / artifact へ結果添付
4. フレーキー対策（タイムアウト・再試行方針）の文書化

## 受け入れ条件

- [ ] F-97 の新規プローブが nightly / 該当ジョブで実行される
- [ ] 失敗時にレポートが artifact として残る
- [ ] 既存 CI 時間予算を著しく悪化させない（必要なら nightly のみ）

## 対応状況

完了（末尾「対応結果」参照）。

## 対応結果（2026-10-05、完了）

`container-security-nightly.yml` に毎日の `e2e` ジョブ（io_uring / epoll の 2 マトリクス、`tests/e2e_setup.sh test`）を追加したため、本チケットで追加した E2E は nightly で必ず実行される（PR CI では F-119 のとおり重いので回さない）。container_security 側のプローブ・fuzz ターゲットは `run.sh` の既定フェーズに含まれており、毎日の `suite` ジョブ（glibc/musl）で実行され、結果は `container-security-results-*` artifact に残る。

ワークフローは GitHub Actions 上でしか実行できないため、ローカルでは YAML の構文検証と、
同じ環境変数での `tools/container_security/run.sh` の実行（v0.7.0 リリース前検証）で確認した。
