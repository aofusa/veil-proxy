#!/usr/bin/env bash
# F-247: GitHub Release の draft の本文を組み立てる。
#
#   tools/release/release-notes.sh <tag> <assets-dir> [image-digests-file] > notes.md
#
# 本文は docs/releases/v<ver>.md（git で管理する。rc も同じ版のファイル）。その後ろに、既定の設定の置き場所・成果物の表・
# SHA256SUMS・検証の方法・イメージの digest を足す。<assets-dir> には draft に付ける成果物と SHA256SUMS がある。
# 公開のページなので足す部分も英語で書く（入れ方の表は本文の方にある）。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TAG="${1:?tag}"; ASSETS="${2:?assets dir}"; DIGESTS="${3:-}"
VER="${TAG#v}"; BASE="${VER%%-*}"
NOTES="$ROOT/docs/releases/v$BASE.md"
[ -s "$NOTES" ] || { echo "error: $NOTES is missing" >&2; exit 1; }
[ -s "$ASSETS/SHA256SUMS" ] || { echo "error: $ASSETS/SHA256SUMS is missing" >&2; exit 1; }
REPO="https://github.com/aofusa/veil-proxy"

case "$VER" in *-*) printf '> **Pre-release (%s)**: for testing before the release. Do not use it in production.\n\n' "$TAG" ;; esac
# 本文（先頭の HTML コメント＝作業メモは出さない）
awk 'BEGIN{c=0} /^<!--/{c=1} c&&/-->/{c=0; next} !c' "$NOTES"

cat <<NOTES_EOF

---

## Default configuration by install method

| Install method | Configuration and page | Notes |
|---|---|---|
| Container | in the image (\`/etc/veil/conf.d/config.toml\`) | \`docker run --rm -p 8080:8080 ghcr.io/aofusa/veil:musl-v$BASE\` |
| deb (Debian, Ubuntu) | \`/etc/veil/config.toml\`, \`/usr/share/veil/www/\` | the service starts on install |
| rpm (RHEL-compatible, Fedora, Amazon Linux) | \`/etc/veil/config.toml\`, \`/usr/share/veil/www/\` | \`sudo systemctl enable --now veil\` |
| Homebrew | \`\$(brew --prefix)/etc/veil/config.toml\` | \`brew services start veil\` |
| winget / MSI | \`%ProgramData%\\veil\\config.toml\` | |
| BSD ports | FreeBSD \`/usr/local/etc/veil/\`, OpenBSD \`/etc/veil/\`, NetBSD \`/usr/pkg/etc/veil/\` | |
| \`cargo binstall\`, \`cargo install\` | **not installed**: provide your own configuration and page | |
| Archives (\`.tar.gz\`, \`.zip\` below) | **not installed**: \`./veil -c examples/config.toml\` | \`examples/\` is not read automatically |

With a package's default configuration, veil serves a page on \`http://127.0.0.1:8080/\`. How veil finds its
configuration file (\`-c\`, then \`VEIL_CONFIG\`, then the default path):
[running.md]($REPO/blob/main/docs/guide/running.md#configuration-file).

## Assets

| File | Size |
|---|---|
NOTES_EOF
for f in $(ls "$ASSETS" | sort); do
    case "$f" in SHA256SUMS*|*.sigstore.json|*.sig) continue ;; esac
    sz="$(wc -c < "$ASSETS/$f" | tr -d ' ')"
    printf '| `%s` | %s MB |\n' "$f" "$(awk -v b="$sz" 'BEGIN{printf "%.1f", b/1048576}')"
done
cat <<NOTES_EOF

\`SHA256SUMS\`:

\`\`\`
$(cat "$ASSETS/SHA256SUMS")
\`\`\`

## Verifying

\`\`\`sh
# The checksums' signature (Sigstore keyless, signed by this repository's release workflow)
cosign verify-blob SHA256SUMS --bundle SHA256SUMS.sigstore.json \\
  --certificate-identity-regexp '^https://github.com/aofusa/veil-proxy/\\.github/workflows/release\\.yml@refs/tags/v' \\
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
sha256sum -c SHA256SUMS --ignore-missing
# Where an asset was built (GitHub build-provenance attestation)
gh attestation verify veil-$BASE-x86_64-unknown-linux-gnu.tar.gz --repo aofusa/veil-proxy
# The container image's signature
cosign verify ghcr.io/aofusa/veil:musl-v$BASE \\
  --certificate-identity-regexp '^https://github.com/aofusa/veil-proxy/' \\
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
\`\`\`

When the release is published, the archives also get minisign signatures (\`<archive>.sig\`), which \`cargo binstall\` checks.
NOTES_EOF
if [ -n "$DIGESTS" ] && [ -s "$DIGESTS" ]; then
    printf '\n## Container images\n\n```\n'
    cat "$DIGESTS"
    printf '```\n'
fi
