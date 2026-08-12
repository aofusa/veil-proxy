#!/usr/bin/env bash
# packaging/output の成果物に、期待するコミットの内容が実際に入っているかを検証する。
#
# 背景（2026-08-12 に実際に踏んだ事故）:
#   `docker build` は **呼び出した時点**のソースをビルドコンテキストとして送る。
#   複数ターゲットを直列に流している最中にソースを変更すると、
#   「先に始まったターゲットは古いソース、後から始まったターゲットは新しいソース」
#   という**混在状態**になる。しかも成果物のタイムスタンプは新しくなるため、
#   ファイル一覧を眺めるだけでは古いバイナリを見分けられない。
#   実際に F-152 の変更後、aarch64-musl だけが新しく、
#   aarch64-gnu / x86_64-gnu / x86_64-musl は古いままだった。
#
# 使い方:
#   packaging/scripts/verify-artifacts.sh <検証したい文字列> [<検証したい文字列>...]
#
#   例) 直近の変更で追加した設定キー名やログ書式を指定する:
#       packaging/scripts/verify-artifacts.sh recv_drain_max
#
# 判定方法:
#   バイナリ内の文字列（`strings`）に、指定した文字列がすべて含まれるかを見る。
#   設定キー名・ログのフォーマット文字列は最適化後もバイナリに残るため、
#   「そのコミットのソースからビルドされたか」の実用的な指標になる。
#
# 終了コード: すべての成果物が期待文字列を含めば 0、1 つでも欠ければ 1。
# 注意: `pipefail` は使わない。`strings ... | grep -q` は grep が最初の一致で終了するため
# `strings` が SIGPIPE で落ち、pipefail 下ではパイプライン全体が非ゼロになる。
# その結果「一致しているのに STALE と誤判定する」（実際にこのスクリプトの初版で踏んだ）。
set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
OUTPUT_DIR="${ROOT}/packaging/output"

if [[ $# -lt 1 ]]; then
    echo "Usage: $(basename "$0") <expected-string> [<expected-string>...]" >&2
    echo "  例: $(basename "$0") recv_drain_max" >&2
    exit 2
fi
EXPECTED=("$@")

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

fail=0
checked=0

# バイナリ 1 つを検証する。
check_binary() {
    local label="$1" bin="$2"
    local missing=()
    local s n
    for s in "${EXPECTED[@]}"; do
        # `grep -c` で件数を取る（`-q` は SIGPIPE を誘発するため使わない。上のコメント参照）。
        n="$(strings -a "${bin}" 2>/dev/null | grep -cF -- "${s}")" || n=0
        if [[ "${n}" -eq 0 ]]; then
            missing+=("${s}")
        fi
    done
    checked=$((checked + 1))
    if [[ ${#missing[@]} -eq 0 ]]; then
        printf 'OK    %s\n' "${label}"
    else
        printf 'STALE %s  (欠落: %s)\n' "${label}" "${missing[*]}"
        fail=1
    fi
}

shopt -s nullglob

# --- tar.gz / zip（スタンドアロン配布物） ---
for f in "${OUTPUT_DIR}"/*.tar.gz "${OUTPUT_DIR}"/*.zip; do
    d="${WORK}/$(basename "${f}")"
    mkdir -p "${d}"
    case "${f}" in
        *.tar.gz) tar xzf "${f}" -C "${d}" 2>/dev/null ;;
        *.zip)    unzip -qo "${f}" -d "${d}" 2>/dev/null ;;
    esac
    bin="$(find "${d}" -type f \( -name veil -o -name 'veil.exe' \) | head -1)"
    if [[ -n "${bin}" ]]; then
        check_binary "$(basename "${f}")" "${bin}"
    else
        printf 'SKIP  %s（veil バイナリが見つからない）\n' "$(basename "${f}")"
    fi
done

# --- .deb ---
for f in "${OUTPUT_DIR}"/*.deb; do
    d="${WORK}/$(basename "${f}")"
    mkdir -p "${d}"
    ( cd "${d}" && ar x "${f}" 2>/dev/null && tar xf data.tar.* 2>/dev/null )
    bin="$(find "${d}" -type f -name veil | head -1)"
    if [[ -n "${bin}" ]]; then
        check_binary "$(basename "${f}")" "${bin}"
    else
        printf 'SKIP  %s（展開できない: ar/tar が必要）\n' "$(basename "${f}")"
    fi
done

# --- .rpm ---
for f in "${OUTPUT_DIR}"/*.rpm; do
    d="${WORK}/$(basename "${f}")"
    mkdir -p "${d}"
    if command -v rpm2cpio >/dev/null 2>&1 && command -v cpio >/dev/null 2>&1; then
        ( cd "${d}" && rpm2cpio "${f}" 2>/dev/null | cpio -idm --quiet 2>/dev/null )
        bin="$(find "${d}" -type f -name veil | head -1)"
        if [[ -n "${bin}" ]]; then
            check_binary "$(basename "${f}")" "${bin}"
        else
            printf 'SKIP  %s（展開できない）\n' "$(basename "${f}")"
        fi
    else
        printf 'SKIP  %s（rpm2cpio/cpio が無い）\n' "$(basename "${f}")"
    fi
done

echo "----"
echo "検証: ${checked} 件"
if [[ ${fail} -ne 0 ]]; then
    echo "STALE な成果物があります。該当ターゲットを再ビルドしてください。" >&2
    exit 1
fi
echo "すべての成果物が期待する内容を含んでいます。"
