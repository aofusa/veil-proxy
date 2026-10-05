# F-81: SBOMのCIパイプライン統合およびRelease添付

親: [F-65](F-65-sbom-generation.md)

## 目的

F-65にて実装されたSBOM（CycloneDX / SPDX）生成機能を活用し、CIおよびリリースフローなどの外部インフラとの連携を行う。

## 改修内容

- `grype` 等と SBOM を連携した、SBOM ベースの脆弱性照合の CI パイプラインへの組み込み。
- GitHub **Release** への SBOM の正式アタッチ（タグ発行フロー確立後。現状は nightly artifact として保存されているものを Release アセットに昇格させる）。

## 受け入れ条件

- GitHub Actions 等の CI 経由で脆弱性照合が実行されること。
- GitHub Release 作成時に SBOM ファイルが自動的にアタッチされること。

## 対応結果（2026-10-05、完了）

`container-security-nightly.yml` の `sbom` ジョブで SBOM（source CycloneDX / image SPDX）を入力に **grype（anchore/scan-action）** で脆弱性を照合し、high 以上で失敗させる。新設の `.github/workflows/release-sbom.yml` が **`release: published` を契機に SBOM を生成してリリースアセットへ `gh release upload`** する（リリースページは手動作成の運用なので、タグ push ではなく公開イベントを使う）。

ワークフローは GitHub Actions 上でしか実行できないため、ローカルでは YAML の構文検証と、
同じ環境変数での `tools/container_security/run.sh` の実行（v0.7.0 リリース前検証）で確認した。
