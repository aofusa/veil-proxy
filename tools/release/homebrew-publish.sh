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
if ! git push -q origin HEAD 2>"$W/push.err"; then
    cat "$W/push.err" >&2
    # token に Workflows の書き込みが無いと .github/workflows を含む push は拒まれる。formula と README だけを置き、
    # tap の CI は token に権限を足した後の版で置く
    grep -q 'without `workflow` scope' "$W/push.err" || exit 1
    echo "::warning title=Homebrew::HOMEBREW_TAP_TOKEN cannot write workflows: pushed the formula without the tap's CI (.github/workflows/test.yml). Give the token Workflows: Read and write to add it"
    git reset -q --soft HEAD~1 2>/dev/null || git update-ref -d HEAD
    git rm -q --cached .github/workflows/test.yml
    git -c user.name="veil release" -c user.email="aofusa@users.noreply.github.com" commit -q -m "veil $VER"
    git push -q origin HEAD
fi
echo "pushed veil $VER to $TAP"
