# F-81: SBOMのCIパイプライン統合およびRelease添付

親: [F-65](F-65-sbom-generation.md)

## 目的

F-65にて実装されたSBOM（CycloneDX / SPDX）生成機能を活用し、CIおよびリリースフローなどの外部インフラとの連携を行う。

## 改修内容（未着手）

- `grype` 等と SBOM を連携した、SBOM ベースの脆弱性照合の CI パイプラインへの組み込み。
- GitHub **Release** への SBOM の正式アタッチ（タグ発行フロー確立後。現状は nightly artifact として保存されているものを Release アセットに昇格させる）。

## 受け入れ条件

- GitHub Actions 等の CI 経由で脆弱性照合が実行されること。
- GitHub Release 作成時に SBOM ファイルが自動的にアタッチされること。
