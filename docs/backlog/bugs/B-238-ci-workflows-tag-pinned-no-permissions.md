# B-238: `ci.yml` などの Actions がタグ固定で、`permissions:` が無い

**状態: 未修正（2026-10-27 起票。セキュリティレビュー SC-02）**

## 出典

2026-10-27 のセキュリティレビュー（v0.9.2 の `.github/workflows/*` の読解）。

## 事象

- 公開系（`release.yml`・`dist.yml`・`release-downstream.yml`・`ghcr-page.yml`・`examples-nightly.yml`）は Actions を SHA で固定し、`permissions:` を
  明示し、cosign で署名している。
- `ci.yml`・`container-security-nightly.yml`・`release-sbom.yml` は `actions/checkout@v4`・`Swatinem/rust-cache@v2`・`anchore/scan-action@v6`・
  `dtolnay/rust-toolchain@stable` のタグ固定で、`ci.yml` には `permissions:` が無い（既定の `GITHUB_TOKEN` の権限はリポジトリの設定次第）。
- タグは動く（tj-actions/changed-files の事故のように、タグを差し替えられると任意のコードが PR の CI で動く）。

## 改修案

- 全 workflow を SHA 固定（`# vN.N.N` のコメント付き）にし、Dependabot（`.github/dependabot.yml` の `github-actions`）で更新する。
- `permissions: contents: read` を最上位に書き、必要な job だけ広げる。

## 受け入れ条件

- `grep -rn "uses:.*@v[0-9]" .github/workflows` が 0 件。PR の CI が通る。

## 関連

- [B-239](B-239-docker-base-images-latest-curl-sh.md)、[F-247](../features/F-247-distribution-foundation.md)
