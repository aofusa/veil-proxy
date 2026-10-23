#!/usr/bin/env bash
# F-247: 既定の設定（config.toml）と既定のページ（www/）を、入手経路ごとに packaging/defaults/ の雛形から作る。
#
#   tools/release/render-defaults.sh <channel> <out-dir> [--www-dir DIR]
#   tools/release/render-defaults.sh --list
#   tools/release/render-defaults.sh --check BIN      # 全経路を作って BIN -t で検査する（CI）
#
# <out-dir>/config.toml と <out-dir>/www/ を作る。--www-dir は File のルート（とその OS のサンドボックスの許可）を
# 差し替える（ports の PREFIX が既定と違うとき）。経路ごとの値は下の表と F-247 の表で同じにする。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DEF="$ROOT/packaging/defaults"

# channel | listen | www dir | security | performance | header
CHANNELS="
container|0.0.0.0:8080|/var/www/|linux-container|linux|container
linux-package|127.0.0.1:8080|/usr/share/veil/www/|linux-package|linux|linux-package
archive-linux|127.0.0.1:8080|examples/www/|linux-user|linux|archive
archive-macos|127.0.0.1:8080|examples/www/|macos|none|archive
archive-windows|127.0.0.1:8080|examples/www/|windows|none|archive
archive-freebsd|127.0.0.1:8080|examples/www/|freebsd-user|none|archive
archive-openbsd|127.0.0.1:8080|examples/www/|openbsd-user|none|archive
archive-netbsd|127.0.0.1:8080|examples/www/|netbsd-user|none|archive
winget|127.0.0.1:8080|C:\\ProgramData\\veil\\www\\|windows|none|winget
ports-freebsd|127.0.0.1:8080|/usr/local/share/veil/www/|freebsd-package|none|ports-freebsd
ports-openbsd|127.0.0.1:8080|/usr/local/share/veil/www/|openbsd-package|none|ports-openbsd
ports-netbsd|127.0.0.1:8080|/usr/pkg/share/veil/www/|netbsd-package|none|ports-netbsd
"

header() {
    case "$1" in
        container) cat <<'H'
# Default configuration of the published veil container image (ghcr.io/aofusa/veil).
#
#   docker run --rm -p 8080:8080 ghcr.io/aofusa/veil      # then open http://localhost:8080/
#
# It serves the page in /var/www on port 8080 and logs every request to standard output (`docker logs`).
# Use your own files by mounting them over these paths:
#   -v "$PWD/site:/var/www:ro"                                  your HTML
#   -v "$PWD/config.toml:/etc/veil/conf.d/config.toml:ro"       your configuration
# Quickstart: http://localhost:8080/quickstart.html and
#             https://github.com/aofusa/veil-proxy/blob/main/docs/guide/container.md
H
            ;;
        linux-package) cat <<'H'
# Default configuration installed by the veil deb / rpm package (/etc/veil/config.toml).
#
#   sudo systemctl enable --now veil       # then: curl http://127.0.0.1:8080/
#
# Edit this file and run `sudo systemctl reload veil`. The package never overwrites your changes.
# Logs: `journalctl -u veil`.
H
            ;;
        archive) cat <<'H'
# Example configuration shipped in the veil release archive. veil does not read it by itself:
#
#   ./veil -c examples/config.toml         # run from the directory of the archive, then open http://127.0.0.1:8080/
#
# To use veil for real, copy this file (and a www directory) where you want, change the paths below to absolute
# paths, and start veil with -c FILE, VEIL_CONFIG=FILE, or put it at the default path (see `veil --help`).
H
            ;;
        winget) cat <<'H'
# Default configuration installed by the veil package for winget (%ProgramData%\veil\config.toml).
#
#   veil                                    # then open http://127.0.0.1:8080/
#
# Edit this file (as an administrator) and restart veil. Upgrades and uninstalls keep your changes.
H
            ;;
        ports-*) cat <<H
# Default configuration installed by the veil package of ${1#ports-}.
#
# Start it with the rc.d script (see pkg-message), then: curl http://127.0.0.1:8080/
# Edit this file and reload veil (SIGHUP). Package upgrades keep your changes.
H
            ;;
    esac
}

# TOML のリテラル文字列（'...'）で入れる（Windows のパスの \ をそのまま使うため）
toml_lit() { printf "'%s'" "$1"; }

render() {
    local channel="$1" out="$2" www_override="${3:-}"
    local line; line="$(grep "^${channel}|" <<<"$CHANNELS" || true)"
    [ -n "$line" ] || { echo "error: unknown channel: $channel (see --list)" >&2; exit 2; }
    local listen www sec perf hdr
    IFS='|' read -r _ listen www sec perf hdr <<<"$line"
    [ -n "$www_override" ] && www="$www_override"
    local www_path="${www%/}"; www_path="${www_path%\\}"
    mkdir -p "$out"
    # python 等に頼らず bash だけで埋める（deb・rpm を作るコンテナ・macOS の bash 3.2 でも動く）
    local sec_file="$DEF/security/$sec.toml" perf_file="$DEF/performance-$perf.toml"
    [ -f "$sec_file" ] || { echo "error: missing $sec_file" >&2; exit 2; }
    local www_lit="'$www'" l
    {
        while IFS= read -r l || [ -n "$l" ]; do
            case "$l" in
                @HEADER@) header "$hdr" ;;
                @SECURITY@)
                    while IFS= read -r x || [ -n "$x" ]; do printf '%s\n' "${x//@WWW_PATH@/"$www_path"}"; done < "$sec_file" ;;
                @PERFORMANCE@)
                    if [ "$perf" != none ]; then echo; cat "$perf_file"; fi ;;
                *)
                    l="${l//@LISTEN@/"$listen"}"
                    printf '%s\n' "${l//@WWW_DIR@/"$www_lit"}" ;;
            esac
        done < "$DEF/config.toml.in"
    } > "$out/config.toml"
    if grep -q '@[A-Z_]*@' "$out/config.toml"; then echo "error: unreplaced placeholder in $out/config.toml" >&2; exit 2; fi
    rm -rf "$out/www"; cp -R "$DEF/www" "$out/www"
}

case "${1:-}" in
    --list) cut -d'|' -f1 <<<"$CHANNELS" | sed '/^$/d' ;;
    --check)
        bin="${2:?usage: --check BIN}"
        tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
        fail=0
        for ch in $(cut -d'|' -f1 <<<"$CHANNELS" | sed '/^$/d'); do
            # 雛形の検査は TOML と設定の値の検証まで（-t）。他の OS 向けのキーは警告で無視される。
            # -t はページのディレクトリがあることも見るので、アーカイブの例（相対パス）はその置き方のまま、
            # 他は作ったページの場所に差し替えて検査する（差し替えない生成物の値は --list の表のとおり）。
            case "$ch" in
                archive-*) render "$ch" "$tmp/$ch/examples"; cfg=examples/config.toml ;;
                *) render "$ch" "$tmp/$ch" "$tmp/$ch/www/"; cfg=config.toml ;;
            esac
            if (cd "$tmp/$ch" && "$bin" -t -c "$cfg" >"$tmp/$ch.log" 2>&1); then echo "ok   $ch"
            else echo "FAIL $ch"; sed 's/^/     /' "$tmp/$ch.log"; fail=1; fi
        done
        # コンテナの Dockerfile は git に入っている生成物をコピーする（ビルドの段に python が無い）。雛形と一致すること
        render container "$tmp/container-as-is"
        if cmp -s "$tmp/container-as-is/config.toml" "$ROOT/packaging/container/config.toml"; then echo "ok   packaging/container/config.toml is up to date"
        else echo "FAIL packaging/container/config.toml differs from the template (regenerate it: $0 container DIR, then copy DIR/config.toml)"; fail=1; fi
        exit "$fail"
        ;;
    ""|-h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *)
        ch="$1"; out="${2:?usage: <channel> <out-dir> [--www-dir DIR]}"; www=""
        if [ "${3:-}" = --www-dir ]; then www="${4:?--www-dir DIR}"; fi
        render "$ch" "$out" "$www"
        ;;
esac
