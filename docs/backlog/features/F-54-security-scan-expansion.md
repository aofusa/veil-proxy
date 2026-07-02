# F-54: セキュリティスキャン拡充

## 目的

SAST/DAST、TLS 設定、依存関係脆弱性をコンテナパイプラインで継続検証する。

## 現状

- h2spec（HTTP/2）、Trivy（イメージ）
- `testssl.sh` コンテナによる TLS スキャン
- `cargo audit` コンテナによる依存関係スキャン

## 改修内容（残件）

- OWASP ZAP / Nuclei（DAST）
- `cargo deny`（ライセンス・クレート制限）
- SBOM（CycloneDX/SPDX）、gitleaks
- seccomp/Landlock 違反の意図的発火テスト

## 受け入れ条件

- `run.sh` で TLS・依存関係スキャンが docker のみで完結
- HIGH/CRITICAL の対応フローを SECURITY または README に記載