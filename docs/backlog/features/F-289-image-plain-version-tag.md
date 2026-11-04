# F-289: コンテナイメージに版だけのタグ（`vX.Y.Z`）を付ける

**状態: 完了（v0.10.1）**

## 依頼

利用者の指示（2026-11-04）: タグ `vX.Y.Z` の公開では、コンテナイメージを `latest`＝`vX.Y.Z`＝`musl-vX.Y.Z` と `glibc-vX.Y.Z` で
公開する。今後の版も同じ。

## 仕様

| 版 | musl の index | glibc の index |
|----|---------------|----------------|
| 正式版 `vX.Y.Z` | `musl-vX.Y.Z`・`vX.Y.Z`・`musl`・`latest` | `glibc-vX.Y.Z`・`glibc` |
| プレリリース `vX.Y.Z-rc.N` | `musl-vX.Y.Z-rc.N`・`vX.Y.Z-rc.N` | `glibc-vX.Y.Z-rc.N` |

- `vX.Y.Z` は版のタグ（上書きしない。既にあれば公開を止める）。`latest`・`musl`・`glibc` は正式版でだけ動かす。
- ghcr.io（`tools/release/publish-images.sh`）と Docker Hub（`tools/release/mirror-dockerhub.sh`）の両方。付けた後に
  digest が版のタグと同じことを確かめる。

## 変更

- `tools/release/publish-images.sh`・`tools/release/mirror-dockerhub.sh`・`release.yml` の説明。
- 文書: `docs/guide/container.md`・`ja/container.md`・`packaging/container/dockerhub.md`・`packaging/README.md`・
  `docs/releases/v0.10.1.md`。

## 確認

v0.10.1-rc.6 で ghcr.io に `v0.10.1-rc.6` が `musl-v0.10.1-rc.6` と同じ digest で付くこと、v0.10.1 の公開で
`latest`・`v0.10.1`・`musl-v0.10.1` が同じ digest になること（Docker Hub も）。
