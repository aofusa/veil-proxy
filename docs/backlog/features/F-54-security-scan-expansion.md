# F-54: セキュリティスキャン拡充

## 目的

SAST/DAST、TLS 設定、依存関係脆弱性をコンテナパイプラインで継続検証する。

## 現状

- h2spec（HTTP/2）、Trivy（イメージ）
- `tools/container_security/security/run_testssl.sh` — `drwetter/testssl.sh` コンテナ
- `tools/container_security/security/run_cargo_audit.sh` — 依存関係脆弱性
- `tools/container_security/security/run_cargo_deny.sh` + `deny.toml` — ライセンス・advisory
- `SECURITY.md` — HIGH/CRITICAL 対応フロー

## 完了（2026-07-06）

- **OWASP ZAP（DAST）** → [F-66](F-66-dast-owasp-zap.md)（完了）。スマグリング能動テストは
  [F-76](F-76-http-smuggling-active-tests.md)（完了）。
- **SBOM（CycloneDX/SPDX）** → [F-65](F-65-sbom-generation.md)（完了）。
- **gitleaks（シークレットスキャン）** → [F-75](F-75-secret-scan-gitleaks.md)（完了）。
- **seccomp 違反の意図的発火テスト** → `src/security.rs::test_seccomp_denies_disallowed_syscall`
  を新設（許可リスト外 syscall が Filter=EPERM / Strict=SIGSYS 即死になることを子プロセスで
  実機検証。既存の PROT_EXEC 引数フィルタ発火テストと相補的）。

## 残件（分離）

- **Nuclei（DAST）** と **Landlock 違反の意図的発火**コンテナテストは外部ツール／専用環境を
  要するため [F-83](F-83-nuclei-landlock-firing.md) へ分離した。

## 受け入れ条件

- [x] `run.sh` で TLS・依存関係スキャンが docker のみで完結（testssl / cargo-audit / cargo-deny）。
- [x] HIGH/CRITICAL の対応フローを `SECURITY.md` に記載。