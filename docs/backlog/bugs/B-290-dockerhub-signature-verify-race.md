# B-290: Docker Hub へ写した直後の署名の検証が `no signatures found` で落ちる

**状態: 修正済み（v0.10.2 以降の公開で効く。v0.10.1 はジョブの再実行で通過）**

## 出典

v0.10.1 の公開（2026-11-05）の `release-downstream.yml` の `dockerhub`（`tools/release/mirror-dockerhub.sh`）。

## 事象

musl のイメージは写して署名の検証まで通ったが、続く glibc のイメージで `oras copy -r` とタグ付けの後の
`cosign verify` が `Error: no signatures found` で失敗した。直後に Docker Hub の referrers API を引くと、glibc の index にも
署名（`application/vnd.dev.sigstore.bundle.v0.3+json`）が 1 件あった。

## 原因

Docker Hub は push された referrer（cosign v3 の署名）を referrers API に少し遅れて反映することがある。スクリプトは
写した直後に 1 回だけ検証していた。

## 修正

- `tools/release/mirror-dockerhub.sh`: `cosign verify` を 20 秒おきに最大 6 回まで試す（最後まで見つからなければ従来どおり失敗）。
- v0.10.1 は失敗したジョブを再実行して公開を終えた。

## 関連

- F-248（Docker Hub への複製）
