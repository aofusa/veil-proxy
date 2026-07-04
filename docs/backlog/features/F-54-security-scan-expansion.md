# F-54: セキュリティスキャン拡充

## 目的

SAST/DAST、TLS 設定、依存関係脆弱性をコンテナパイプラインで継続検証する。

## 現状

- h2spec（HTTP/2）、Trivy（イメージ）
- `tools/container_security/security/run_testssl.sh` — `drwetter/testssl.sh` コンテナ
- `tools/container_security/security/run_cargo_audit.sh` — 依存関係脆弱性
- `tools/container_security/security/run_cargo_deny.sh` + `deny.toml` — ライセンス・advisory
- `SECURITY.md` — HIGH/CRITICAL 対応フロー

## 改修内容（残件）

- OWASP ZAP / Nuclei（DAST）
- SBOM（CycloneDX/SPDX）、gitleaks
- seccomp/Landlock 違反の意図的発火テスト

## 受け入れ条件

- `run.sh` で TLS・依存関係スキャンが docker のみで完結
- HIGH/CRITICAL の対応フローを SECURITY または README に記載