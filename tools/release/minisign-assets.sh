#!/usr/bin/env bash
# F-249: 公開したリリースのアーカイブに minisign の署名（<アーカイブ>.sig）を付ける（release-downstream.yml）。
# cargo binstall は Cargo.toml の [package.metadata.binstall.signing] の公開鍵でこれを検証し、署名の無い・合わない
# アーカイブを入れない。
#
#   tools/release/minisign-assets.sh <tag>
#
# 先に SHA256SUMS の cosign の署名とハッシュを確かめ、CI が作ったものだけに署名する。
# 要る環境変数: MINISIGN_SECRET_KEY（minisign の秘密鍵ファイルの中身。パスワード無し）、GH_TOKEN
set -euo pipefail
TAG="${1:?tag}"
REPO="${GITHUB_REPOSITORY:-aofusa/veil-proxy}"
W="$(mktemp -d)"; cd "$W"
gh release download "$TAG" --repo "$REPO" --pattern 'SHA256SUMS*' --pattern '*.tar.gz' --pattern '*.zip'
cosign verify-blob SHA256SUMS --bundle SHA256SUMS.sigstore.json \
    --certificate-identity-regexp '^https://github.com/aofusa/veil-proxy/\.github/workflows/release\.yml@refs/tags/v' \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com
sha256sum -c --ignore-missing SHA256SUMS
# 鍵のファイルは「untrusted comment: …」の行と鍵の行の 2 行。secret に鍵の行だけを入れると minisign は
# 「Error while loading the secret key file」で止まるので、コメントの行が無ければ足す
umask 077
case "${MINISIGN_SECRET_KEY:?}" in
    "untrusted comment:"*) printf '%s\n' "$MINISIGN_SECRET_KEY" > "$W/minisign.key" ;;
    *) printf 'untrusted comment: veil release signing\n%s\n' "$(printf '%s' "$MINISIGN_SECRET_KEY" | tr -d '\r\n ')" > "$W/minisign.key" ;;
esac
for f in *.tar.gz *.zip; do
    minisign -S -s "$W/minisign.key" -m "$f" -x "$f.sig" -t "veil $TAG $f" </dev/null
done
rm -f "$W/minisign.key"
gh release upload "$TAG" --repo "$REPO" --clobber ./*.sig
echo "signed and uploaded $(ls ./*.sig | wc -l) signatures"
