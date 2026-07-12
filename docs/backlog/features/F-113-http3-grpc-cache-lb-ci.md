# F-113: F-112 追加 E2E/プローブの CI 組み込み

## 概要

[F-112](F-112-http3-grpc-cache-lb-coverage.md) で追加した E2E・libFuzzer ターゲット（`qpack_decode` / `http3_frame_decode`）・container_security プローブ（0-RTT Anti-Replay、Pumba HTTP/3）を CI（GitHub Actions / nightly）へ配線する。

**本作業では対象外**（F-112 から分離）。

## 改修内容（予定）

1. `.github/workflows/ci.yml` の E2E / feature マトリクスに F-112 追加テストが含まれることの確認（`cargo test --features full` で自動拾いなら配線不要）
2. `container-security-nightly.yml` で新 fuzz ターゲットと pumba H3 パスが実行されること
3. 必要ならジョブタイムアウト・artifact 名を調整

## 受け入れ条件

- [ ] F-112 追加の E2E が PR CI または nightly で実行される
- [ ] `qpack_decode` / `http3_frame_decode` が fuzz ジョブに含まれる
- [ ] 失敗時に artifacts が残る

## 対応状況

未着手。
