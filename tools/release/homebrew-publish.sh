#!/usr/bin/env bash
# F-250: 公開した版の formula を aofusa/homebrew-tap に push する（release-downstream.yml）。
#
#   tools/release/homebrew-publish.sh <tag>
#
# 公開したリリースの SHA256SUMS を取り、cosign の署名を確かめてから formula を作る。tap の CI（tap-test.yml）と
# README も同じ commit で置く。要る環境変数: HOMEBREW_TAP_TOKEN（tap に Contents の書き込みを持つ fine-grained token）
set -euo pipefail
TAG="${1:?tag}"; VER="${TAG#v}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TAP="${HOMEBREW_TAP_REPO:-aofusa/homebrew-tap}"
BASE="https://github.com/aofusa/veil-proxy/releases/download/$TAG"
W="$(mktemp -d)"
curl -fsSL -o "$W/SHA256SUMS" "$BASE/SHA256SUMS"
curl -fsSL -o "$W/SHA256SUMS.sigstore.json" "$BASE/SHA256SUMS.sigstore.json"
cosign verify-blob "$W/SHA256SUMS" --bundle "$W/SHA256SUMS.sigstore.json" \
    --certificate-identity-regexp '^https://github.com/aofusa/veil-proxy/\.github/workflows/release\.yml@refs/tags/v' \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com
"$ROOT/tools/release/homebrew-formula.sh" "$VER" "$BASE" "$W/SHA256SUMS" > "$W/veil.rb"
git clone -q "https://x-access-token:${HOMEBREW_TAP_TOKEN:?}@github.com/$TAP.git" "$W/tap"
mkdir -p "$W/tap/Formula" "$W/tap/.github/workflows"
cp "$W/veil.rb" "$W/tap/Formula/veil.rb"
cp "$ROOT/packaging/homebrew/tap-test.yml" "$W/tap/.github/workflows/test.yml"
cp "$ROOT/packaging/homebrew/tap-README.md" "$W/tap/README.md"
cd "$W/tap"
git add Formula/veil.rb .github/workflows/test.yml README.md
if git diff --cached --quiet; then echo "formula already at $VER"; exit 0; fi
git -c user.name="veil release" -c user.email="aofusa@users.noreply.github.com" commit -q -m "veil $VER"
git push -q origin HEAD
echo "pushed veil $VER to $TAP"
