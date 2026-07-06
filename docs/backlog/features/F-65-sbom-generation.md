# F-65: SBOM（ソフトウェア部品表）自動生成

出典: `security_chaos_fuzzing_report.md` §2.3.2。親: [F-54](F-54-security-scan-expansion.md)。

## 目的

将来のゼロデイ脆弱性に対する影響調査を迅速化するため、ソース（Cargo.lock）と
コンテナイメージの双方から SBOM を生成・成果物化する。

## 実装済み

- `tools/container_security/security/run_sbom.sh`（docker `anchore/syft`）。
  - ソース: `dir:` スキャン → `sbom_source.cdx.json`（CycloneDX、`target/` 等除外）。
  - イメージ: docker デーモン経由 → `sbom_image.spdx.json`（SPDX）。
- `run.sh` フェーズ 4e として配線（`SKIP_SBOM` トグル、`report.sh` に `sbom` 追加）。

## 実行結果

- ソース: **823 コンポーネント**（Rust クレート）。
- イメージ（`veil:glibc`）: **7 パッケージ**（distroless 相当の最小構成）。

## 実装済み（CI 添付・2026-07-05）

- **`.github/workflows/container-security-nightly.yml` の `sbom-release` ジョブ**で
  `run_sbom.sh` を実行し、`sbom_source.cdx.json`（CycloneDX）+ `sbom_image.spdx.json`（SPDX）を
  **artifact として保存**（retention 90 日、[F-57](F-57-container-security-ci.md)）。

## 残件

- なし（本チケットでの残件は解消）。
- `grype`連携、GitHub Releaseへの正式アタッチなど、CI・外部インフラが必要なタスクについては [F-81](F-81-sbom-ci-integration.md) として未着手の別タスクへ分離したため、本タスクは完了とする。

## 受け入れ条件

- `SKIP_SBOM=0 ./tools/container_security/run.sh` が docker のみで両 SBOM を生成する。
