# F-90: container_security full features セキュリティテスト網羅拡充

出典: [container_security_coverage_report.md](../../artifacts/container_security_coverage_report.md)。
親: [F-07](fuzzing-chaos-security.md) / [F-57](F-57-container-security-ci.md)。

## 目的

`Cargo.toml` の `full` features でビルドした Veil イメージに対し、
HTTP/3・gRPC・WebSocket・L4・Admin・Cache・Compression・WASM・rate-limit 等の
攻撃面を `tools/container_security` で自動検証する。

## 改修内容

- `fixtures/veil-config.toml` を full 経路対応に拡張（wasm/grpc/ws/rate-limit/otel 等）。
- 新規プローブスクリプト（harness/scripts/ + security/run_*.sh）:
  - `admin_security_probe.sh` — 認証バイパス試行
  - `compression_cache_probe.sh` — 圧縮爆弾・キャッシュデセプション
  - `http3_probe.sh` — HTTP/3 到達性
  - `l4_flood_probe.sh` — L4 接続フラッド
  - `websocket_probe.sh` — WebSocket 異常 Upgrade
  - `grpc_probe.sh` — gRPC 不正フレーム (H2C)
  - `rate_limit_probe.sh` — 429 遮断
  - `wasm_security_probe.sh` — Proxy-Wasm コンプライアンス
  - `metrics_probe.sh` — メトリクス高頻度アクセス
  - `otel_probe.sh` — OTLP 到達不能時の生存
- `run.sh` / `entrypoint.sh` / `report.sh` へフェーズ配線。
- [security_test_required_cases.md](../../artifacts/security_test_required_cases.md) との
  ギャップ分析・実装状況ドキュメントを `docs/artifacts/` に整備。

## 受け入れ条件

- [x] `SKIP_FULL_FEATURES_SECURITY=0 ./tools/container_security/run.sh` が全新規プローブを実行する。
- [x] 必要テストケース一覧の P-03〜P-11, E-01〜E-15, W-01〜W-03 がプローブとして実装されている。
- [x] 失敗テストは実装バグとテストケース問題を切り分け、バグは backlog へ起票する（[security_test_failures.md](../../artifacts/security_test_failures.md)、B-29〜B-33）。

## 依存・リスク

- HTTP/3 プローブはホスト curl の HTTP/3 対応に依存。未対応時は UDP 到達性 + ヘルスで代替。
- gRPC/WebSocket は専用バックエンドなしでも「異常入力耐性」は検証可能。正常系は E2E (`tests/e2e_tests.rs`) が担当。