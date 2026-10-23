#!/usr/bin/env bash
# F-247: GitHub Release の draft の本文を組み立てる。
#
#   tools/release/release-notes.sh <tag> <assets-dir> [image-digests-file] > notes.md
#
# 本文は docs/releases/v<ver>.md（git で管理する。rc も同じ版のファイル）。その後ろに、入れ方・成果物の表・
# SHA256SUMS・検証の方法・イメージの digest を足す。<assets-dir> には draft に付ける成果物と SHA256SUMS がある。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TAG="${1:?tag}"; ASSETS="${2:?assets dir}"; DIGESTS="${3:-}"
VER="${TAG#v}"; BASE="${VER%%-*}"
NOTES="$ROOT/docs/releases/v$BASE.md"
[ -s "$NOTES" ] || { echo "error: $NOTES is missing" >&2; exit 1; }
[ -s "$ASSETS/SHA256SUMS" ] || { echo "error: $ASSETS/SHA256SUMS is missing" >&2; exit 1; }
REPO="https://github.com/aofusa/veil-proxy"

case "$VER" in *-*) printf '> **プレリリース（%s）**: 公開前の確認用です。本番の環境には使わないでください。\n\n' "$TAG" ;; esac
# 本文（先頭の HTML コメント＝作業メモは出さない）
awk 'BEGIN{c=0} /^<!--/{c=1} c&&/-->/{c=0; next} !c' "$NOTES"

cat <<EOF

---

## 入れ方

| 入れ方 | コマンド | 既定の設定とページ |
|---|---|---|
| コンテナ | \`docker run --rm -p 8080:8080 ghcr.io/aofusa/veil:musl-v$BASE\` | イメージに入っている（\`/etc/veil/conf.d/config.toml\`） |
| Debian・Ubuntu | [apt のリポジトリ]($REPO/blob/main/docs/guide/running.md#apt-and-dnf-repositories) を足して \`sudo apt install veil\`、または下の \`.deb\` | \`/etc/veil/config.toml\`（入れると起動） |
| RHEL 系・Fedora・Amazon Linux | [dnf のリポジトリ]($REPO/blob/main/docs/guide/running.md#apt-and-dnf-repositories) を足して \`sudo dnf install veil\`、または下の \`.rpm\` | \`/etc/veil/config.toml\`（\`sudo systemctl enable --now veil\`） |
| Homebrew（macOS・Linux） | \`brew install aofusa/tap/veil\` | \`\$(brew --prefix)/etc/veil/config.toml\` |
| winget（Windows） | \`winget install Aofusa.Veil\`、または下の \`.msi\` | \`%ProgramData%\\veil\\config.toml\` |
| Rust | \`cargo binstall veil-proxy\`・\`cargo install veil-proxy --locked --features full\` | **入らない**（設定とページは自分で用意する） |
| アーカイブ（下の \`.tar.gz\`・\`.zip\`） | 展開して \`./veil -c examples/config.toml\` | **入らない**（\`examples/\` は自動では読まない例） |

パッケージで入れたものは、既定の設定で \`http://127.0.0.1:8080/\` にページを返します。設定ファイルの決まり方
（\`-c\` → \`VEIL_CONFIG\` → 既定のパス）と置き場所は [running.md]($REPO/blob/main/docs/guide/running.md#configuration-file)。

## 成果物

| ファイル | 大きさ |
|---|---|
EOF
for f in $(ls "$ASSETS" | sort); do
    case "$f" in SHA256SUMS*|*.sigstore.json|*.sig) continue ;; esac
    sz="$(wc -c < "$ASSETS/$f" | tr -d ' ')"
    printf '| `%s` | %s MB |\n' "$f" "$(awk -v b="$sz" 'BEGIN{printf "%.1f", b/1048576}')"
done
cat <<EOF

\`SHA256SUMS\`:

\`\`\`
$(cat "$ASSETS/SHA256SUMS")
\`\`\`

## 検証

\`\`\`sh
# チェックサムの署名（Sigstore の keyless。このリポジトリの release の workflow が署名した）
cosign verify-blob SHA256SUMS --bundle SHA256SUMS.sigstore.json \\
  --certificate-identity-regexp '^https://github.com/aofusa/veil-proxy/\\.github/workflows/release\\.yml@refs/tags/v' \\
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
sha256sum -c SHA256SUMS --ignore-missing
# 個々の成果物の出自（GitHub の attestation）
gh attestation verify veil-$BASE-x86_64-unknown-linux-gnu.tar.gz --repo aofusa/veil-proxy
# コンテナイメージの署名
cosign verify ghcr.io/aofusa/veil:musl-v$BASE \\
  --certificate-identity-regexp '^https://github.com/aofusa/veil-proxy/' \\
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
\`\`\`
EOF
if [ -n "$DIGESTS" ] && [ -s "$DIGESTS" ]; then
    printf '\n## コンテナイメージ\n\n```\n'
    cat "$DIGESTS"
    printf '```\n'
fi
