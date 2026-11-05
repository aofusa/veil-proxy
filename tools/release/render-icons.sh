#!/usr/bin/env bash
# F-290: ロゴ（docs/images/veil_logo.webp）から配布物に入れるアイコンを作る。生成物はコミットする
# （ビルド時には変換しない。CI・BSD の VM・コンテナのビルドに ImageMagick を要求しないため）。
#
#   tools/release/render-icons.sh            # 作り直す（ImageMagick の magick か convert。webp を読めること）
#   tools/release/render-icons.sh --check    # 元画像が生成したときのままか・生成物が揃っているか（ImageMagick 不要。CI）
#
# 生成物:
#   packaging/icons/veil.ico                        Windows の veil.exe（build.rs）と MSI の ARPPRODUCTICON
#   packaging/icons/hicolor/<N>x<N>/apps/veil.png   deb・rpm・ports の /usr/share/icons/hicolor
#   packaging/defaults/www/favicon.ico              既定のページの favicon（16・32・48 だけの小さい ico）
#   packaging/icons/SOURCE.sha256                   生成に使った元画像の SHA-256（--check が比べる）
#
# 出力のバイト列は ImageMagick の版で揺れるので、--check は生成物そのものではなく元画像のハッシュを比べる。
# packaging/icons/io.github.aofusa.veil.metainfo.xml（AppStream）は手で書くファイルで、--check は appstreamcli が
# あれば検証する。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SRC_REL="docs/images/veil_logo.webp"
ICONS="$ROOT/packaging/icons"
FAVICON="$ROOT/packaging/defaults/www/favicon.ico"
METAINFO="$ICONS/io.github.aofusa.veil.metainfo.xml"
HICOLOR_SIZES="16 32 48 64 128 256 512"
ICO_SIZES="256,128,64,48,40,32,24,20,16"
FAVICON_SIZES="48,32,16"

outputs() {
    echo "$ICONS/veil.ico"
    echo "$FAVICON"
    for n in $HICOLOR_SIZES; do echo "$ICONS/hicolor/${n}x${n}/apps/veil.png"; done
}

check() {
    local fail=0 f
    if (cd "$ROOT" && sha256sum -c --status packaging/icons/SOURCE.sha256 2>/dev/null); then
        echo "ok   $SRC_REL is the image the icons were made from"
    else
        echo "FAIL $SRC_REL changed after the icons were made (run $0 and commit the result)"
        fail=1
    fi
    while IFS= read -r f; do
        if [ -s "$f" ]; then echo "ok   ${f#"$ROOT"/}"; else echo "FAIL missing ${f#"$ROOT"/} (run $0)"; fail=1; fi
    done < <(outputs)
    if command -v appstreamcli >/dev/null 2>&1; then
        if appstreamcli validate --no-net "$METAINFO"; then echo "ok   ${METAINFO#"$ROOT"/}"
        else echo "FAIL ${METAINFO#"$ROOT"/} is not valid AppStream metadata"; fail=1; fi
    else
        echo "skip appstreamcli is not installed (the AppStream metadata is not validated)"
    fi
    exit "$fail"
}

render() {
    local im
    if command -v magick >/dev/null 2>&1; then im=(magick)
    elif command -v convert >/dev/null 2>&1; then im=(convert)
    else echo "error: ImageMagick (magick or convert) is required to make the icons" >&2; exit 2; fi
    # 再生成で差分が出ないように、日時・ソフトウェア名などの付帯情報を書かない
    local strip=(-strip -define png:exclude-chunks=date,time,tIME)
    local src="$ROOT/$SRC_REL" n
    for n in $HICOLOR_SIZES; do
        mkdir -p "$ICONS/hicolor/${n}x${n}/apps"
        "${im[@]}" "$src" -background none -resize "${n}x${n}" -gravity center -extent "${n}x${n}" \
            "${strip[@]}" "PNG32:$ICONS/hicolor/${n}x${n}/apps/veil.png"
    done
    # 256 の項目は PNG 圧縮で入る（ImageMagick の ICO の書き出しは 256 以上を PNG にする）
    "${im[@]}" "$src" -background none "${strip[@]}" -define icon:auto-resize="$ICO_SIZES" "$ICONS/veil.ico"
    "${im[@]}" "$src" -background none "${strip[@]}" -define icon:auto-resize="$FAVICON_SIZES" "$FAVICON"
    (cd "$ROOT" && sha256sum "$SRC_REL" > packaging/icons/SOURCE.sha256)
    outputs | while IFS= read -r f; do printf '%8d  %s\n' "$(wc -c < "$f")" "${f#"$ROOT"/}"; done
}

case "${1:-}" in
    --check) check ;;
    "") render ;;
    -h|--help) sed -n '2,17p' "$0" ;;
    *) echo "usage: $0 [--check]" >&2; exit 2 ;;
esac
