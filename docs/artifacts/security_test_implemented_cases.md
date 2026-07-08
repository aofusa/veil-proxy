# Veil セキュリティテスト 実装済みテストケース一覧

`tools/container_security` の現状（F-90 拡充後）。正はソースと
[security_test_required_cases.md](security_test_required_cases.md)。

## 既存フェーズ（default features 中心）

| フェーズ | スクリプト | 対応 ID |
|----------|-----------|---------|
| HTTP ファジング | `harness/tools/fuzz-http` | P-01 |
| libFuzzer | `fuzz/run_libfuzzer.sh` | C-05, C-06, W-05 (opt-in) |
| h2spec | `harness/scripts/h2spec_run.sh` | P-02 |
| chaos_load | `harness/scripts/chaos_load.sh` | C-01 |
| toxiproxy/circuit_breaker/slowloris | `harness/scripts/*_chaos.sh` | C-02〜C-04 |
| security_scan | `harness/scripts/security_scan.sh` | TLS, メソッド制限, TRACE, パストラバーサル |
| testssl | `security/run_testssl.sh` | P-05 |
| smuggling | `security/run_smuggling.sh` | P-04 |
| semgrep | `security/run_semgrep.sh` | S-01 |
| gitleaks | `security/run_gitleaks.sh` | S-04 |
| cargo-audit/deny | `security/run_cargo_*.sh` | S-03 |
| SBOM | `security/run_sbom.sh` | — |
| Trivy | `run.sh` | S-05 |
| bad_backend/pumba/resource/syscall/e2e_sanitizer | `chaos/*.sh` (opt-in) | C-07〜C-10 |

## F-90 新規プローブ（full features）

| プローブ | スクリプト | 対応 ID |
|----------|-----------|---------|
| admin_security | `harness/scripts/admin_security_probe.sh` | E-01〜E-06 |
| compression_cache | `harness/scripts/compression_cache_probe.sh` | E-07〜E-10 |
| http3 | `harness/scripts/http3_probe.sh` | P-03 (curl HTTP/3 未対応時は TLS 生存) |
| l4_flood | `harness/scripts/l4_flood_probe.sh` | P-10 |
| websocket | `harness/scripts/websocket_probe.sh` | P-08 |
| grpc | `harness/scripts/grpc_probe.sh` | P-06, P-07 |
| rate_limit | `harness/scripts/rate_limit_probe.sh` | E-11, E-12 |
| wasm_security | `harness/scripts/wasm_security_probe.sh` | W-01〜W-03 |
| metrics_probe | `harness/scripts/metrics_probe.sh` | E-13 |
| otel_probe | `harness/scripts/otel_probe.sh` | E-14 |

オーケストレーション: `security/run_full_features_security.sh`（`run.sh` フェーズ 4j）。

## F-88（clippy disallowed-methods）

| 項目 | 実装 |
|------|------|
| `clippy.toml` disallowed-methods | ホットパス同期 I/O 禁止 |
| CI | `.github/workflows/ci.yml` `cargo clippy --features full --tests` |
| 正当 allow | `src/` 各所に理由付き `#[allow(clippy::disallowed_methods)]` |

## 設定

- `fixtures/veil-config.toml`: full 経路（wasm/grpc/ws/rate-limit/otel/l4/cache/compression/admin）
- `lib/common.sh`: WASM マウント、プローブ用環境変数

## opt-in（手動有効化）

| 変数 | 内容 |
|------|------|
| `RUN_WASM_FUZZ=1` | W-05 libFuzzer wasm 系 |
| `SKIP_FULL_FEATURES_SECURITY=1` | F-90 プローブ群スキップ |
| `FULL_FEATURES_SECURITY_BLOCKING=1` | プローブ失敗で suite 失敗 |