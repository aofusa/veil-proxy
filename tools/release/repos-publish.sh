#!/usr/bin/env bash
# F-251: 公開した版の deb・rpm を aofusa/veil-packages（GitHub Pages）の apt・dnf のリポジトリに足す（release-downstream.yml）。
#
#   tools/release/repos-publish.sh <tag>
#
# 公開したリリースから deb・rpm と SHA256SUMS を取り、cosign の署名とハッシュを確かめてから build-repos.sh で足して署名し、
# veil-packages に push する。要る環境変数: PACKAGES_TOKEN（veil-packages に Contents の書き込み）、
# PACKAGES_GPG_KEY（署名用の副鍵の秘密鍵。ASCII armor）。
set -euo pipefail
TAG="${1:?tag}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
REPO="${PACKAGES_REPO:-aofusa/veil-packages}"
BASE="https://github.com/aofusa/veil-proxy/releases/download/$TAG"
W="$(mktemp -d)"; mkdir -p "$W/new"
curl -fsSL -o "$W/new/SHA256SUMS" "$BASE/SHA256SUMS"
curl -fsSL -o "$W/new/SHA256SUMS.sigstore.json" "$BASE/SHA256SUMS.sigstore.json"
cosign verify-blob "$W/new/SHA256SUMS" --bundle "$W/new/SHA256SUMS.sigstore.json" \
    --certificate-identity-regexp '^https://github.com/aofusa/veil-proxy/\.github/workflows/release\.yml@refs/tags/v' \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com
for f in $(awk '{print $2}' "$W/new/SHA256SUMS" | grep -E '\.(deb|rpm)$'); do curl -fsSL -o "$W/new/$f" "$BASE/$f"; done
(cd "$W/new" && sha256sum -c --ignore-missing SHA256SUMS)
export GNUPGHOME="$W/gnupg"; mkdir -m 700 "$GNUPGHOME"
gpg --batch --import <<<"${PACKAGES_GPG_KEY:?}"
KEY="$(gpg --batch --with-colons --list-keys | awk -F: '/^fpr/ {print $10; exit}')"
git clone -q --depth 1 "https://x-access-token:${PACKAGES_TOKEN:?}@github.com/$REPO.git" "$W/site"
"$ROOT/tools/release/build-repos.sh" "$W/site" "$W/new" "$KEY" 3
cd "$W/site"
git add -A
if git diff --cached --quiet; then echo "repositories already have $TAG"; exit 0; fi
# パッケージ（1 版で数百 MB）が履歴に積もって Pages の上限（1GB）を超えないように、毎回 1 commit だけの履歴で置き換える
branch="$(git symbolic-ref --short HEAD 2>/dev/null || echo main)"
git checkout -q --orphan publish
git -c user.name="veil release" -c user.email="aofusa@users.noreply.github.com" commit -q -m "veil ${TAG#v}"
git push -q --force origin "publish:$branch"
echo "pushed $TAG to $REPO"
