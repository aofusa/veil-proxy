# B-239: Docker のベースイメージがタグ無し・`latest` で、`Dockerfile.lib` が rustup を `curl | sh` で入れる

**状態: 未修正（2026-10-27 起票。セキュリティレビュー SC-03）**

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

## 関連

- [B-238](B-238-ci-workflows-tag-pinned-no-permissions.md)、[B-158](B-158-unfixed-image-vulns-and-advisory-gaps.md)
