# F-113: F-112 追加 E2E/プローブの CI 組み込み

## 概要

[F-112](F-112-http3-grpc-cache-lb-coverage.md) で追加した E2E・libFuzzer ターゲット（`qpack_decode` / `http3_frame_decode`）・container_security プローブ（0-RTT Anti-Replay、Pumba HTTP/3）を CI（GitHub Actions / nightly）へ配線する。

**本作業では対象外**（F-112 から分離）。

## 改修内容

1. `.github/workflows/ci.yml` の E2E / feature マトリクスに F-112 追加テストが含まれることの確認（`cargo test --features full` で自動拾いなら配線不要）
2. `container-security-nightly.yml` で新 fuzz ターゲットと pumba H3 パスが実行されること
3. 必要ならジョブタイムアウト・artifact 名を調整

## 受け入れ条件

- [ ] F-112 追加の E2E が PR CI または nightly で実行される
- [ ] `qpack_decode` / `http3_frame_decode` が fuzz ジョブに含まれる
- [ ] 失敗時に artifacts が残る

## 対応状況

完了（末尾「対応結果」参照）。

## 対応結果（2026-10-05、完了）

`container-security-nightly.yml` に毎日の `e2e` ジョブ（io_uring / epoll の 2 マトリクス、`tests/e2e_setup.sh test`）を追加したため、本チケットで追加した E2E は nightly で必ず実行される（PR CI では F-119 のとおり重いので回さない）。container_security 側のプローブ・fuzz ターゲットは `run.sh` の既定フェーズに含まれており、毎日の `suite` ジョブ（glibc/musl）で実行され、結果は `container-security-results-*` artifact に残る。 `qpack_decode` / `http3_frame_decode` は毎日の短時間 libFuzzer と週次の ASAN libFuzzer の両方の `FUZZ_TARGETS` に含めた。Pumba の HTTP/3 パスは週次の `chaos-extended` ジョブ（`SKIP_PUMBA=0`）で実行する。

ワークフローは GitHub Actions 上でしか実行できないため、ローカルでは YAML の構文検証と、
同じ環境変数での `tools/container_security/run.sh` の実行（v0.7.0 リリース前検証）で確認した。
