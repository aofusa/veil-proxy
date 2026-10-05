# F-104: F-103 追加 E2E/プローブの CI 組み込み

## 概要

[F-103](F-103-http3-grpc-edge-h3-coverage.md) で追加した gRPC over HTTP/3 エッジ E2E および container_security プローブ（S-G-H3-09〜13 / S-H3-18〜20）を CI に組み込む。

**F-103 作業では対象外**（本チケットは CI 専用）。

## 改修内容（案）

1. `.github/workflows/ci.yml` の E2E ジョブで F-103 追加テストが full features で実行されること（既存 full E2E に含まれる想定なら確認のみ）
2. `container-security-nightly.yml` で新規 `h3_grpc_*` / `h3_control_*` / `h3_cid_exhaustion` / `h3_token_spoofing` が実行・レポートされること
3. 必要なら Job Summary に新規 ID を列挙

## 受け入れ条件

- [ ] F-103 の新規 E2E が CI E2E パスで実行される
- [ ] 新規 container_security プローブが nightly（または該当 workflow）で実行される
- [ ] 失敗時に results artifact から追跡可能

## 対応状況

完了（末尾「対応結果」参照）。

## 対応結果（2026-10-05、完了）

`container-security-nightly.yml` に毎日の `e2e` ジョブ（io_uring / epoll の 2 マトリクス、`tests/e2e_setup.sh test`）を追加したため、本チケットで追加した E2E は nightly で必ず実行される（PR CI では F-119 のとおり重いので回さない）。container_security 側のプローブ・fuzz ターゲットは `run.sh` の既定フェーズに含まれており、毎日の `suite` ジョブ（glibc/musl）で実行され、結果は `container-security-results-*` artifact に残る。

ワークフローは GitHub Actions 上でしか実行できないため、ローカルでは YAML の構文検証と、
同じ環境変数での `tools/container_security/run.sh` の実行（v0.7.0 リリース前検証）で確認した。
