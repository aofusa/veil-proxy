# F-108: F-107 追加 E2E / container_security の CI 組み込み

## 概要

[F-107](F-107-http3-grpc-streaming-web-coverage.md) で追加した gRPC over HTTP/3 ストリーミング細分化・圧縮・gRPC-Web E2E と container_security プローブ（S-G-H3-14〜17）を CI に組み込む。

**本作業（F-107 実装フェーズ）では対象外。** ハーネス・テスト本体は F-107、GHA 配線のみ本チケット。

## 改修内容

1. `.github/workflows/ci.yml` の E2E ジョブで F-107 新規テストが実行されること（`full` features）
2. `container-security-nightly.yml` で `h3_grpc_oversized_metadata` / `h3_grpc_rst_flood` / `h3_grpc_web_*` が実行されること
3. 失敗時の artifact / Job Summary に新規プローブ名が載ること

## 受け入れ条件

- [ ] F-107 完了後に着手
- [ ] CI 上で新規 E2E / プローブが実行され、結果が artifact に残る
- [ ] 既知フレーキーは既存方針どおり（失敗で全体を落とさない or リトライ）

## 対応状況

完了（末尾「対応結果」参照）。

## 対応結果（2026-10-05、完了）

`container-security-nightly.yml` に毎日の `e2e` ジョブ（io_uring / epoll の 2 マトリクス、`tests/e2e_setup.sh test`）を追加したため、本チケットで追加した E2E は nightly で必ず実行される（PR CI では F-119 のとおり重いので回さない）。container_security 側のプローブ・fuzz ターゲットは `run.sh` の既定フェーズに含まれており、毎日の `suite` ジョブ（glibc/musl）で実行され、結果は `container-security-results-*` artifact に残る。

ワークフローは GitHub Actions 上でしか実行できないため、ローカルでは YAML の構文検証と、
同じ環境変数での `tools/container_security/run.sh` の実行（v0.7.0 リリース前検証）で確認した。
