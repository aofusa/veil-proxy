# Security Policy

## 脆弱性の報告

セキュリティ上の問題は GitHub Security Advisories またはメンテナへ非公開で連絡してください。

## container_security スイート

`tools/container_security/run.sh` は Docker のみで以下を実行します。

| フェーズ | ツール | スキップ環境変数 |
|----------|--------|------------------|
| イメージ | Trivy | `SKIP_TRIVY=1` |
| HTTP/2 | h2spec | `SKIP_H2SPEC=1` |
| ファジング | libFuzzer | `SKIP_LIBFUZZER=1`（既定） |
| ファジング（ASAN+corpus） | libFuzzer + AddressSanitizer | `SKIP_LIBFUZZER_ASAN=1`（既定, F-71） |
| TLS | testssl.sh | `SKIP_TESTSSL=1` |
| SAST | semgrep（`p/rust`+`p/security-audit`） | `SKIP_SEMGREP=1`（F-64） |
| SBOM | syft（CycloneDX/SPDX） | `SKIP_SBOM=1`（F-65） |
| DAST | OWASP ZAP baseline | `SKIP_ZAP=1`（既定, F-66） |
| カオス（プロトコル違反） | bad_backend モック | `SKIP_BAD_BACKEND=1`（既定, F-67） |
| カオス（netem） | Pumba / tc | `SKIP_PUMBA=1`（既定, F-69） |
| カオス（リソース枯渇） | cgroup 制約 | `SKIP_RESOURCE_EXHAUSTION=1`（既定, F-68） |
| 依存関係 | cargo-audit | `SKIP_CARGO_AUDIT=1`（既定） |
| ライセンス | cargo-deny | `SKIP_CARGO_DENY=1`（既定） |

WASM モジュール/ABI 境界のファジングターゲット `wasm_abi` は
`cargo fuzz run wasm_abi --features wasm` で実行する（F-70）。

## HIGH / CRITICAL 対応フロー

1. **検知**: Trivy / cargo-audit / cargo-deny のレポートを `tools/container_security/results/` で確認する。
2. **トリアージ**: 本番バイナリ（`veil:glibc`）に実際にバンドルされる依存か、ビルド専用かを切り分ける。
3. **修正**: パッチ版への更新、feature 無効化、代替クレートへの置換のいずれかで解消する。
4. **検証**: `SKIP_TRIVY=0 SKIP_CARGO_AUDIT=0 ./tools/container_security/run.sh` を再実行し、同一 ID が再出しないことを確認する。
5. **記録**: 対応内容を PR 説明またはリリースノートに記載する。

### 例外（許容）の判断基準

- 修正版が未リリースで、当該コードパスが無効 feature またはテスト専用のみに限定される場合は、チケット化して期限付きで追跡する。
- コンテナベースイメージ由来で Veil バイナリに影響しない場合は、ベースイメージ更新 PR を別途起票する。

## 定期実行（推奨）

```bash
# 既定（重いフェーズはスキップ）
./tools/container_security/run.sh

# nightly / リリース前
SKIP_LIBFUZZER=0 SKIP_CARGO_AUDIT=0 SKIP_CARGO_DENY=0 SKIP_TRIVY=0 ./tools/container_security/run.sh
```