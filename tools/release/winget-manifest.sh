#!/usr/bin/env bash
# F-252: winget のマニフェスト（Aofusa.Veil、MSI、x64・arm64）を作る。
#
#   tools/release/winget-manifest.sh <version> <out-dir> <base-url> <SHA256SUMS> [x64-product-code arm64-product-code]
#
# <base-url> は成果物の置き場所（本番は https://github.com/aofusa/veil-proxy/releases/download/v<ver>。rc の試験では
# runner の中の HTTP サーバ）。SHA256 は F-247 の SHA256SUMS から引く。
# 出力: <out-dir>/manifests/a/Aofusa/Veil/<version>/Aofusa.Veil{,.installer,.locale.en-US}.yaml
set -euo pipefail
VER="${1:?version}"; OUT="${2:?out-dir}"; BASE="${3:?base-url}"; SUMS="${4:?SHA256SUMS}"
PC_X64="${5:-}"; PC_ARM="${6:-}"
X64="veil-${VER}-x86_64-pc-windows-msvc.msi"; ARM="veil-${VER}-aarch64-pc-windows-msvc.msi"
sha() { awk -v f="$1" '$2==f || $2=="*"f {print toupper($1)}' "$SUMS"; }
SX64="$(sha "$X64")"; SARM="$(sha "$ARM")"
[ -n "$SX64" ] && [ -n "$SARM" ] || { echo "error: $X64 / $ARM not in $SUMS" >&2; exit 1; }
D="$OUT/manifests/a/Aofusa/Veil/$VER"; mkdir -p "$D"
pc() { [ -n "$1" ] && printf '    ProductCode: "%s"\n' "$1"; true; }
cat > "$D/Aofusa.Veil.yaml" <<Y
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.version.1.9.0.schema.json
PackageIdentifier: Aofusa.Veil
PackageVersion: $VER
DefaultLocale: en-US
ManifestType: version
ManifestVersion: 1.9.0
Y
{
cat <<Y
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.installer.1.9.0.schema.json
PackageIdentifier: Aofusa.Veil
PackageVersion: $VER
InstallerType: wix
Scope: machine
UpgradeBehavior: install
Commands:
  - veil
ReleaseDate: $(date -u +%Y-%m-%d)
Installers:
  - Architecture: x64
    InstallerUrl: $BASE/$X64
    InstallerSha256: $SX64
Y
pc "$PC_X64"
cat <<Y
  - Architecture: arm64
    InstallerUrl: $BASE/$ARM
    InstallerSha256: $SARM
Y
pc "$PC_ARM"
cat <<Y
ManifestType: installer
ManifestVersion: 1.9.0
Y
} > "$D/Aofusa.Veil.installer.yaml"
cat > "$D/Aofusa.Veil.locale.en-US.yaml" <<Y
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.defaultLocale.1.9.0.schema.json
PackageIdentifier: Aofusa.Veil
PackageVersion: $VER
PackageLocale: en-US
Publisher: aofusa
PublisherUrl: https://github.com/aofusa
PublisherSupportUrl: https://github.com/aofusa/veil-proxy/issues
PackageName: veil
PackageUrl: https://github.com/aofusa/veil-proxy
License: MIT OR Apache-2.0
LicenseUrl: https://github.com/aofusa/veil-proxy/blob/main/LICENSE-MIT
ShortDescription: High-performance reverse proxy and web server (HTTP/1.1, HTTP/2, HTTP/3)
Description: |-
  veil is a reverse proxy and web server for HTTP/1.1, HTTP/2 and HTTP/3 with Proxy-Wasm filters.
  The package installs a default configuration at %ProgramData%\\veil\\config.toml that serves a page on
  http://127.0.0.1:8080/. Run "veil" to start it.
Moniker: veil
Tags:
  - http3
  - reverse-proxy
  - web-server
ReleaseNotesUrl: https://github.com/aofusa/veil-proxy/releases/tag/v$VER
Documentations:
  - DocumentLabel: Running veil
    DocumentUrl: https://github.com/aofusa/veil-proxy/blob/main/docs/guide/running.md
ManifestType: defaultLocale
ManifestVersion: 1.9.0
Y
echo "$D"
