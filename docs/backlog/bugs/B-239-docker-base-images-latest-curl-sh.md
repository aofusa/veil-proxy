# B-239: Docker のベースイメージがタグ無し・`latest` で、`Dockerfile.lib` が rustup を `curl | sh` で入れる

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27 のセキュリティレビュー（v0.9.2 の `docker/Dockerfile.*` の読解）。

## 事象

- `messense/cargo-zigbuild`（タグ無し = latest）、`cgr.dev/chainguard/glibc-dynamic:latest`、`messense/rust-musl-cross:x86_64-musl`（動くタグ）。
  ビルドの再現性が無く、上流のイメージが差し替えられると配布物が変わる。
- `docker/Dockerfile.lib` は `curl https://sh.rustup.rs | sh`（sha256 の検証なし）。`tools/release/install-k8s-tools.sh` は sha256 を検証していて不揃い。

## 改修案

- ベースイメージを digest（`@sha256:...`）で固定し、Dependabot（`docker` エコシステム）か Renovate で更新する。
- rustup は `rustup-init` の sha256 を検証するか、公式の `rust:<version>` イメージへ替える。
- B-158 の方針（直す版の無い脆弱性も失敗）と合わせて、digest の更新を定期の作業にする。

## 受け入れ条件

- `grep -n "FROM" docker/Dockerfile.*` の全行に `@sha256:` がある。cacher 段と builder 段が同じ `--profile dist` のまま（F-184）。

## 修正（v0.10.0）

- `docker/Dockerfile.*` の外部イメージ（`FROM` と `ARG` の既定値）を `タグ@sha256:...` で固定した（多アーキの index の digest。aofish の `docker buildx imagetools inspect` で解決を確認）。ローカルでビルドする `veil:musl-mesh` と段の参照（`tool-builder`・`scratch`）は対象外。cacher 段と builder 段の `--profile dist` は変えていない。
- `docker/Dockerfile.lib` は `curl | sh` をやめ、`rustup-init` を版（1.29.2）と sha256（x86_64・aarch64）で固定して検証してから実行する（manylinux イメージで検証が通り起動することを確認）。
- `.github/dependabot.yml` に `docker`（`/docker`）を足し、digest の更新を週次の PR で受ける。

## 関連

- [B-238](B-238-ci-workflows-tag-pinned-no-permissions.md)、[B-158](B-158-unfixed-image-vulns-and-advisory-gaps.md)
