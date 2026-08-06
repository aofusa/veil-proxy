#!/usr/bin/env bash
#
# FreeBSD / OpenBSD × x86_64 / aarch64 の full-system QEMU VM ヘルパ（B-47）
# ============================================================================
#
# 目的:
#   - `packaging/` が配布する BSD バイナリを **実 OS 上でビルド・E2E 検証**し、
#     ビルド済みバイナリを host 側へ取り出す（packaging/scripts/build-bsd.sh へ渡す）。
#   - 従来 tools/qemu にあったのは aarch64 のみ（`aarch64-vm.sh` = Linux arm64、
#     `fbsd-arm64-vm.sh` = FreeBSD arm64）。本スクリプトは **FreeBSD/OpenBSD の
#     x86_64 と aarch64 の 4 通り**を同じインタフェースで扱う。
#
# 前提:
#   - Docker（`tools/qemu/helper` のイメージ経由で qemu-system-* を起動する。ホストに
#     qemu / UEFI ファーム / sudo は不要）。
#   - x86_64 ゲストは **ホストに /dev/kvm があれば KVM 加速**される（実用速度）。
#     aarch64 ゲストは x86_64 ホストでは TCG（低速）。
#   - SSH 鍵（既定 `~/.ssh/veil_qemu_key`）。python3 + pexpect（provision に使用）。
#
# native モード（Docker 不使用、`VEIL_QEMU_NATIVE=1`）:
#   Docker が無いホスト（例: Apple Silicon macOS で Docker 未導入）向けに、
#   helper コンテナを介さずホストの qemu-system-* を直接起動するモード。
#   `VEIL_QEMU_NATIVE=1` を明示するか、`docker` コマンドが無い環境では**自動的に**
#   native へ切り替わる（`VEIL_QEMU_NATIVE=0` を明示すれば自動切替を止められる）。
#   Docker モードの挙動・出力は本モードの有無に関わらず一切変更していない。
#
#   macOS（Apple Silicon, M1〜M4）での前提:
#     brew install qemu                                  # qemu-system-{aarch64,x86_64} + EDK2 ファーム
#     brew install cdrtools                               # mkisofs（cloud-init シード ISO 作成用）
#     python3 -m pip install --user --break-system-packages pexpect   # provision 系スクリプトが使用
#
#   aarch64 ゲスト（FreeBSD/OpenBSD/NetBSD の arm64）は、Apple Silicon ホストでは
#   **HVF アクセラレータ**（`-machine virt,accel=hvf,gic-version=3 -cpu host`）で
#   ネイティブ速度に近い速度で動く（TCG のような数分〜数十分のブートにはならない）。
#   x86_64 ホスト用の QEMU（Homebrew qemu on Apple Silicon には x86_64 TCG も同梱）は
#   引き続き TCG。これにより、Linux aarch64 は KVM 非対応ホストでは実用不能だった
#   full-system E2E/ビルド検証が、Apple Silicon 実機（M4 等）では aarch64 BSD ゲストに
#   限り実用速度で行える。
#
# 使い方:
#   tools/qemu/bsd-vm.sh <os> <arch> <command> [args]
#     os   : freebsd | openbsd | netbsd
#     arch : x86_64 | aarch64
#
#   # FreeBSD amd64 を作って full features でビルドし E2E まで回して取り出す
#   tools/qemu/bsd-vm.sh freebsd x86_64 setup
#   tools/qemu/bsd-vm.sh freebsd x86_64 up
#   tools/qemu/bsd-vm.sh freebsd x86_64 grow          # root FS 拡張（FreeBSD のみ）
#   tools/qemu/bsd-vm.sh freebsd x86_64 provision     # SSH 鍵注入 + sshd 有効化
#   tools/qemu/bsd-vm.sh freebsd x86_64 toolchain     # rust/cmake/llvm 等を導入
#   tools/qemu/bsd-vm.sh freebsd x86_64 build         # in-VM full features ビルド
#   tools/qemu/bsd-vm.sh freebsd x86_64 e2e           # in-VM tests/e2e_setup.sh test
#   tools/qemu/bsd-vm.sh freebsd x86_64 fetch         # release バイナリを host へ取得
#   tools/qemu/bsd-vm.sh freebsd x86_64 down
#
#   # Docker クロスビルド済みの FreeBSD amd64 バイナリを VM で E2E だけ回す
#   tools/qemu/bsd-vm.sh freebsd x86_64 e2e --prebuilt packaging/output/veil-freebsd-amd64
#
# コマンド一覧:
#   all        setup → provision → toolchain → build → e2e → fetch を一括実行
#   setup      helper イメージ build + ゲストイメージ取得 + SSH 鍵生成
#   up         VM 起動（detached、telnet シリアルコンソール + hostfwd ssh）
#   wait       SSH 到達までブロック
#   grow       ディスク拡張（FreeBSD: qemu-img resize + single-user growfs）
#   provision  SSH 鍵注入 + sshd 有効化（FreeBSD はシリアル single-user 経由）
#   toolchain  VM 内へ rust / cmake / llvm / bash / curl 等を導入
#   sync       リポジトリを VM へ転送（tar over ssh）
#   build      VM 内で full features リリースビルド
#   e2e        VM 内で tests/e2e_setup.sh test を実行
#   fetch      VM 内の release バイナリを host（packaging/build/）へ取得
#   ssh/scp/console/status/down
#
# 環境変数:
#   VEIL_QEMU_NATIVE  1 で Docker を使わずホストの qemu-system-* を直接起動する
#                     （docker コマンドが無い場合は既定で自動的に 1 相当になる。
#                     0 を明示すると自動切替を止める＝docker 呼び出しがそのまま失敗する）
#   VEIL_QEMU_DIR  VM 資材の親ディレクトリ（既定 ~/qemu-images）
#   BASE_IMG       プロビジョニング済みイメージ。setup でこれを backing file とする
#                  qcow2 オーバーレイを作る（元イメージは変更しない）
#   IMG            使用するディスクイメージのパス（既定 ${WORKDIR}/disk.qcow2）
#   KEY            SSH 鍵（既定 ~/.ssh/veil_qemu_key）
#   CARGO_FEATURES freature セット（既定: freebsd=full-freebsd / openbsd=full-openbsd。
#                  いずれも --no-default-features 併用でアロケータを差し替える）
#   CONSOLE_WAIT   1 で qemu がシリアルコンソール接続を待ってから起動する
#                  （ブートローダのプロンプトを取り逃さない。provision で使用）
#   GUEST_ROOT     VM 内のリポジトリ配置先
#                  （既定: FreeBSD=/root/veil-proxy, OpenBSD=/usr/obj/veil-proxy）
#   VM_SMP/VM_MEM_MB/GROW_GB
#   FREEBSD_VER (14.3-RELEASE) / OPENBSD_VER (7.9。CDN は直近リリースのみ保持)
#   HELPER_IMG     helper イメージ名（既定 veil-qemu:local）
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${HERE}/../.." && pwd)"

OS_NAME="${1:-}"; ARCH="${2:-}"; COMMAND="${3:-}"
[[ $# -ge 3 ]] && shift 3 || true

usage() {
    sed -n '2,60p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 1
}
case "${OS_NAME}" in freebsd|openbsd|netbsd) ;; *) echo "ERROR: os must be freebsd|openbsd|netbsd" >&2; usage ;; esac
case "${ARCH}" in x86_64|aarch64) ;; *) echo "ERROR: arch must be x86_64|aarch64" >&2; usage ;; esac
[[ -n "${COMMAND}" ]] || usage

VEIL_QEMU_DIR="${VEIL_QEMU_DIR:-${HOME}/qemu-images}"
WORKDIR="${WORKDIR:-${VEIL_QEMU_DIR}/${OS_NAME}-${ARCH}}"
KEY="${KEY:-${HOME}/.ssh/veil_qemu_key}"
HELPER_IMG="${HELPER_IMG:-veil-qemu:local}"
# native モード判定: `VEIL_QEMU_NATIVE=1` で明示指定するか、docker コマンドが
# 無い環境（Docker 未導入の macOS ホスト等）では自動的に native（Docker を使わず
# ホストの qemu-system-* を直接起動する）モードへ切り替える。
# `VEIL_QEMU_NATIVE=0` を明示した場合は docker が無くても自動切替しない
# （その場合は docker 呼び出し自体がエラーになる＝明示指定を尊重する）。
if [[ -n "${VEIL_QEMU_NATIVE:-}" ]]; then
    NATIVE="${VEIL_QEMU_NATIVE}"
elif ! command -v docker >/dev/null 2>&1; then
    NATIVE=1
else
    NATIVE=0
fi
VM_SMP="${VM_SMP:-4}"
VM_MEM_MB="${VM_MEM_MB:-4096}"
GROW_GB="${GROW_GB:-24}"
FREEBSD_VER="${FREEBSD_VER:-14.3-RELEASE}"
# ゲストの root パスワード（シリアルコンソールからのデバッグ用。SSH は鍵のみ）
VM_ROOT_PASSWORD="${VM_ROOT_PASSWORD:-veil}"
# x86_64 ゲストのファームウェア（bios=SeaBIOS / uefi=OVMF+q35）。
# 既定は bios。`BASE_IMG` に配布の素の VM-IMAGE 由来イメージを指す場合は uefi が要る。
VM_FIRMWARE="${VM_FIRMWARE:-bios}"
# OpenBSD の CDN は直近数リリースしか保持しない（例: 7.6 は既に 404）。
# 既定は入手可能な最新に追従させ、古いリリースを使う場合は OPENBSD_VER で指定する
# （`curl -s https://cdn.openbsd.org/pub/OpenBSD/ | grep -oE '"[0-9]\.[0-9]/"'` で確認できる）。
OPENBSD_VER="${OPENBSD_VER:-7.9}"
# NetBSD: OS バージョン（配布イメージ）とパッケージリポジトリのバージョンは別軸
# （実地確認済み: 10.1 リリースのパッケージは pkgsrc の "10.0_2026Q2" 系列に入っている）。
NETBSD_VER="${NETBSD_VER:-10.1}"
NETBSD_PKG_VER="${NETBSD_PKG_VER:-10.0}"
# ↑ cdn.NetBSD.org は .../NetBSD/<arch>/10.0/All/ を .../10.0_2026Q2/All/ 等へ
# 302 リダイレクトする（curl -sIL で確認済み）。curl -fL を使うので追従される。
NAME="veil-${OS_NAME}-${ARCH}"

# 使用するディスクイメージ。既定は setup が作る ${WORKDIR}/disk.qcow2。
#
# `BASE_IMG` に**プロビジョニング済みイメージ**を指すと、それを backing file とする
# qcow2 オーバーレイを ${WORKDIR}/disk.qcow2 として作り、元イメージを一切変更せずに
# 起動する（setup 時に作成）。ストックの FreeBSD amd64 VM-IMAGE はシリアルコンソールへ
# 何も出力せず `provision` のコンソール操作が効かないため、既に root SSH 鍵を仕込んだ
# イメージがある場合はこの経路を使う:
#
#   BASE_IMG=~/qemu-images/freebsd-14.3-amd64.qcow2 \
#     tools/qemu/bsd-vm.sh freebsd x86_64 setup
#
# `IMG` を直接指定すると（オーバーレイを作らず）そのイメージを使う。
IMG="${IMG:-${WORKDIR}/disk.qcow2}"
IMG_NAME="$(basename "${IMG}")"
IMG_DIR="$(cd "$(dirname "${IMG}")" 2>/dev/null && pwd || echo "${WORKDIR}")"

# os/arch ごとに固定のポートを割り当て（4 VM を同時起動しても衝突しない）
_port_base() {
    case "${OS_NAME}-${ARCH}" in
        freebsd-x86_64) echo 2310 ;;
        freebsd-aarch64) echo 2320 ;;
        openbsd-x86_64) echo 2330 ;;
        openbsd-aarch64) echo 2340 ;;
        netbsd-x86_64) echo 2350 ;;
        netbsd-aarch64) echo 2360 ;;
    esac
}
PORT_BASE="$(_port_base)"
SSH_PORT="${SSH_PORT:-${PORT_BASE}}"
CON_PORT="${CON_PORT:-$((PORT_BASE + 1))}"
# QMP（x86_64 のみ使用）。FreeBSD amd64 はシリアルへ出力しないため、
# ローダへブラインドでキーを送ってシリアルを有効化するのに使う（qmp-sendkeys.py）。
QMP_PORT="${QMP_PORT:-$((PORT_BASE + 2))}"

# ゲストの SSH ユーザ（FreeBSD/OpenBSD とも root で運用する）
SSH_USER="${SSH_USER:-root}"
SSH_OPTS=(-i "${KEY}" -p "${SSH_PORT}"
  -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
  -o ConnectTimeout=90 -o ServerAliveInterval=20 -o LogLevel=ERROR)
# scp のポート指定は **-P**（-p は「タイムスタンプを保持」で意味が違う）。
# SSH_OPTS をそのまま渡すとポートが効かず 22 番へ繋ぎに行って失敗する。
SCP_OPTS=(-i "${KEY}" -P "${SSH_PORT}"
  -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
  -o ConnectTimeout=90 -o ServerAliveInterval=20 -o LogLevel=ERROR)

log() { echo "[${OS_NAME}-${ARCH}] $*" >&2; }
die() { echo "[${OS_NAME}-${ARCH}] ERROR: $*" >&2; exit 1; }

# helper コンテナ内で 1 コマンド実行（WORKDIR を /w にマウント）。
# native モードでは Docker を使わず、ホスト上で直接コマンドを実行する
# （cwd=${WORKDIR}。docker 版が /img・/base にマウントするパスへの参照
# （引数が "/img/..." "/base/..." で始まる場合）は実ホストパスへ読み替える）。
helper() {
    if [[ "${NATIVE}" == "1" ]]; then
        _native_run "$@"
    elif [[ "${IMG_DIR}" == "${WORKDIR}" ]]; then
        docker run --rm -v "${WORKDIR}:/w" -w /w "${HELPER_IMG}" "$@"
    else
        docker run --rm -v "${WORKDIR}:/w" -v "${IMG_DIR}:/img" -w /w "${HELPER_IMG}" "$@"
    fi
}

# helper() の native 版実行本体。
_native_run() {
    local args=() a base_dir
    base_dir=""
    [[ -f "${WORKDIR}/.base_img_dir" ]] && base_dir="$(cat "${WORKDIR}/.base_img_dir")"
    for a in "$@"; do
        case "$a" in
            /img/*) a="${IMG_DIR}/${a#/img/}" ;;
            /base/*) [[ -n "${base_dir}" ]] && a="${base_dir}/${a#/base/}" ;;
        esac
        args+=("$a")
    done
    ( cd "${WORKDIR}" && "${args[@]}" )
}

# cloud-localds 相当（NoCloud の `cidata` ラベル ISO9660 シードを作る）。
# macOS には cloud-localds が無いため、native モードでは
# mkisofs/genisoimage/xorrisofs（Linux 系・Homebrew `cdrtools`）→
# macOS 標準の hdiutil の順に試す。
_make_cidata_iso() {
    local out="$1" user_data="$2" meta_data="$3"
    local src; src="$(mktemp -d "${TMPDIR:-/tmp}/veil-cidata-src.XXXXXX")"
    cp "${user_data}" "${src}/user-data"
    cp "${meta_data}" "${src}/meta-data"
    if command -v mkisofs >/dev/null 2>&1; then
        mkisofs -output "${out}" -volid cidata -joliet -rock "${src}" >/dev/null
    elif command -v genisoimage >/dev/null 2>&1; then
        genisoimage -output "${out}" -volid cidata -joliet -rock "${src}" >/dev/null
    elif command -v xorrisofs >/dev/null 2>&1; then
        xorrisofs -output "${out}" -volid cidata -joliet -rock "${src}" >/dev/null
    elif command -v hdiutil >/dev/null 2>&1; then
        # hdiutil は拡張子の無い出力名に自動で `.iso` を付け足すため、一旦
        # 別名（拡張子無し）へ出力してから目的のファイル名へ move する。
        local tmp_base; tmp_base="$(mktemp -u "${TMPDIR:-/tmp}/veil-cidata-out.XXXXXX")"
        hdiutil makehybrid -iso -joliet -default-volume-name cidata -o "${tmp_base}" "${src}" >/dev/null
        mv "${tmp_base}.iso" "${out}"
    else
        rm -rf "${src}"
        die "cidata ISO を作るツールが無い（mkisofs/genisoimage/xorrisofs/hdiutil のいずれかが必要。macOS は 'brew install cdrtools' で mkisofs を導入できる）"
    fi
    rm -rf "${src}"
}

# ---------------------------------------------------------------------------
# イメージ URL
# ---------------------------------------------------------------------------
_image_url() {
    case "${OS_NAME}-${ARCH}" in
        # **BASIC-CLOUDINIT 版**を使う。cloud-init が入っているので、NoCloud シード
        # （cloud-localds で作る `cidata` ラベルのディスク）から SSH 公開鍵を注入でき、
        # **シリアルコンソール操作なしで provision が完結する**。
        # 素の VM-IMAGE はシリアルへ出力せず（amd64）、コンソール経由の鍵注入が
        # 現実的でないため採用しない（tools/qemu/README.md 参照）。
        freebsd-x86_64)
            echo "https://download.freebsd.org/releases/VM-IMAGES/${FREEBSD_VER}/amd64/Latest/FreeBSD-${FREEBSD_VER}-amd64-BASIC-CLOUDINIT-ufs.qcow2.xz" ;;
        freebsd-aarch64)
            echo "https://download.freebsd.org/releases/VM-IMAGES/${FREEBSD_VER}/aarch64/Latest/FreeBSD-${FREEBSD_VER}-arm64-aarch64-BASIC-CLOUDINIT-ufs.qcow2.xz" ;;
        # miniroot（数十 MB）を使い、sets は HTTP ミラーから取得する
        # （install イメージは 700MB 超で、どのみち sets は HTTP 指定にするため）。
        openbsd-x86_64)
            echo "https://cdn.openbsd.org/pub/OpenBSD/${OPENBSD_VER}/amd64/miniroot${OPENBSD_VER//./}.img" ;;
        openbsd-aarch64)
            echo "https://cdn.openbsd.org/pub/OpenBSD/${OPENBSD_VER}/arm64/miniroot${OPENBSD_VER//./}.img" ;;
        # NetBSD（F-140）: 両アーキとも起動可能な**生イメージ（gzip 圧縮）**が配布
        # されているのでそのまま qcow2 化して使う。cloud-init 相当が無いため
        # provision はシリアルへ root ログインして行う（netbsd-provision.py）。
        # x86_64 は `-live.img.gz`、aarch64 は evbarm-aarch64 の `gzimg/arm64.img.gz`
        # （FreeBSD の VM-IMAGE に相当するもの。旧来は install ISO を sysinst で
        # シリアル自動操作していたが、実機で言語選択メニューのまま止まり動作しない
        # ことが判明したため、この既製ブータブルイメージへ切り替えた）。
        netbsd-x86_64)
            echo "https://cdn.netbsd.org/pub/NetBSD/NetBSD-${NETBSD_VER}/images/NetBSD-${NETBSD_VER}-amd64-live.img.gz" ;;
        netbsd-aarch64)
            echo "https://cdn.netbsd.org/pub/NetBSD/NetBSD-${NETBSD_VER}/evbarm-aarch64/binary/gzimg/arm64.img.gz" ;;
    esac
}

# ---------------------------------------------------------------------------
# setup
# ---------------------------------------------------------------------------
cmd_setup() {
    mkdir -p "${WORKDIR}"
    if [[ "${NATIVE}" == "1" ]]; then
        log "native モード（Docker 不使用）: helper イメージの build をスキップする"
    else
        log "helper イメージを build（${HELPER_IMG}）"
        docker build -t "${HELPER_IMG}" "${HERE}/helper"
    fi
    [[ -f "${KEY}" ]] || { log "SSH 鍵を生成: ${KEY}"; ssh-keygen -t ed25519 -N '' -f "${KEY}" >/dev/null; }

    if [[ -n "${BASE_IMG:-}" ]]; then
        [[ -f "${BASE_IMG}" ]] || die "BASE_IMG not found: ${BASE_IMG}"
        local base_dir base_name
        base_dir="$(cd "$(dirname "${BASE_IMG}")" && pwd)"
        base_name="$(basename "${BASE_IMG}")"
        log "プロビジョニング済みイメージのオーバーレイを作成（元イメージは変更しない）: ${BASE_IMG}"
        if [[ "${NATIVE}" == "1" ]]; then
            ( cd "${WORKDIR}" && qemu-img create -f qcow2 -F qcow2 -b "${base_dir}/${base_name}" "${IMG_NAME}" ) >/dev/null
        else
            docker run --rm -v "${WORKDIR}:/w" -v "${base_dir}:/base:ro" -w /w "${HELPER_IMG}" \
                qemu-img create -f qcow2 -F qcow2 -b "/base/${base_name}" "${IMG_NAME}" >/dev/null
        fi
        # 起動時にも backing file を同じパス（/base）で見せる必要があるため記録しておく
        echo "${base_dir}" > "${WORKDIR}/.base_img_dir"
        log "setup 完了（オーバーレイ: ${IMG}、backing: ${BASE_IMG}）"
        return 0
    fi

    local url; url="$(_image_url)"
    if [[ "${OS_NAME}" == "freebsd" ]]; then
        # 配布イメージは `base.qcow2` として保持し、実際に起動するのはその
        # **qcow2 オーバーレイ**（`disk.qcow2`）にする。こうすると
        #   - 再プロビジョニングはオーバーレイを作り直すだけ（再ダウンロード不要）
        #   - 元イメージが汚れない
        # という利点がある。`reset` サブコマンドでオーバーレイだけ作り直せる。
        local base="${WORKDIR}/base.qcow2"
        if [[ ! -f "${base}" ]]; then
            log "FreeBSD VM-IMAGE（BASIC-CLOUDINIT）を DL + 展開: ${url}"
            curl -fL --retry 3 -o "${base}.xz" "${url}"
            xz -dc "${base}.xz" > "${base}"
            rm -f "${base}.xz"
        fi
        [[ -f "${IMG}" ]] || _create_overlay
        _write_cloudinit_seed
    elif [[ "${OS_NAME}" == "openbsd" ]]; then
        # OpenBSD は ready-made な qcow2 が無いため miniroot からの autoinstall。
        if [[ ! -f "${WORKDIR}/miniroot.img" ]]; then
            log "OpenBSD インストーライメージを DL: ${url}"
            curl -fL --retry 3 -o "${WORKDIR}/miniroot.img" "${url}"
        fi
        if [[ ! -f "${IMG}" ]]; then
            log "空のターゲットディスクを作成（${GROW_GB}G）"
            helper qemu-img create -f qcow2 "${IMG_NAME}" "${GROW_GB}G" >/dev/null
        fi
        _write_openbsd_autoinstall
    else
        # NetBSD（F-140）。x86_64/aarch64 とも起動可能な生イメージ（gzip 圧縮）を
        # DL して qcow2 化し、FreeBSD と同じ「base.qcow2 + 起動用オーバーレイ」構成
        # にする（cloud-init は無いので seed は作らない。provision はシリアル
        # ログイン経由 = netbsd-provision.py）。
        local base="${WORKDIR}/base.qcow2"
        if [[ ! -f "${base}" ]]; then
            log "NetBSD イメージを DL + qcow2 変換: ${url}"
            curl -fL --retry 3 -o "${WORKDIR}/live.img.gz" "${url}"
            gunzip -kf "${WORKDIR}/live.img.gz"
            helper qemu-img convert -f raw -O qcow2 live.img base.qcow2
            rm -f "${WORKDIR}/live.img.gz" "${WORKDIR}/live.img"
        fi
        [[ -f "${IMG}" ]] || _create_overlay
    fi
    log "setup 完了"
}

# base.qcow2 から起動用オーバーレイ（disk.qcow2）を作る。
#
# 配布イメージの仮想サイズは ~6GiB しかなく、cloud-init の初回処理
# （デバッグシンボルの展開など）で **ゲストのファイルシステムが満杯**になり、
# sshd まで到達しない（`No space left on device` が延々と出る = 実測）。
# そこでオーバーレイ作成時に +${GROW_GB}G して、cloud-init の growfs に拡張させる。
_create_overlay() {
    local base_size total
    base_size="$(helper qemu-img info --output=json base.qcow2 | tr -d ' \n' \
        | sed -n 's/.*"virtual-size":\([0-9]*\).*/\1/p')"
    [[ -n "${base_size}" ]] || die "base.qcow2 の仮想サイズを取得できなかった"
    total=$(( base_size + GROW_GB * 1024 * 1024 * 1024 ))
    log "起動用オーバーレイを作成（base + ${GROW_GB}G = $(( total / 1024 / 1024 / 1024 ))G）"
    helper qemu-img create -f qcow2 -F qcow2 -b base.qcow2 "${IMG_NAME}" "${total}" >/dev/null
}

# cloud-init（NoCloud）のシードディスクを作る。
#
# FreeBSD の BASIC-CLOUDINIT イメージは cloud-init 入りなので、`cidata` ラベルの
# ディスクを 1 本足すだけで root の SSH 公開鍵注入と sshd 有効化ができる。
# **シリアルコンソールを一切使わない**ので、amd64 でローダ/カーネルがシリアルへ
# 出力しない問題（tools/qemu/README.md 参照）を完全に回避できる。
_write_cloudinit_seed() {
    local pub; pub="$(cat "${KEY}.pub")"
    # 鍵は `users:` 経由ではなく **write_files で /root/.ssh/authorized_keys を直接置く**。
    # cloud-init の `users:` による root の鍵設定はディストリ差があり、FreeBSD の
    # BASIC-CLOUDINIT イメージでは適用されず `Permission denied (publickey)` になった。
    cat > "${WORKDIR}/user-data" <<EOF
#cloud-config
disable_root: false
ssh_pwauth: false
# シリアルコンソールからのデバッグ用に root パスワードを設定する。
# SSH は鍵認証のみ（ssh_pwauth: false）で、VM のポートは 127.0.0.1 のみに
# フォワードされるローカル開発用 VM なので、固定パスワードで問題ない。
chpasswd:
  expire: false
  list: |
    root:${VM_ROOT_PASSWORD}
write_files:
  - path: /root/.ssh/authorized_keys
    permissions: '0600'
    owner: 'root:wheel'
    content: |
      ${pub}
runcmd:
  - [ sh, -c, "chmod 700 /root/.ssh" ]
  - [ sysrc, sshd_enable=YES ]
  - [ sh, -c, "sed -i '' -e 's/^#*PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config" ]
  - [ sh, -c, "sed -i '' -e 's/^#*PubkeyAuthentication.*/PubkeyAuthentication yes/' /etc/ssh/sshd_config" ]
  - [ service, sshd, restart ]
EOF
    # instance-id を変えると cloud-init は「別インスタンス」とみなして設定を**再適用**する。
    # 鍵やユーザデータを変えたあとに seed を作り直して再起動すれば反映される。
    cat > "${WORKDIR}/meta-data" <<EOF
instance-id: veil-${OS_NAME}-${ARCH}-$(date +%s)
local-hostname: veil-${OS_NAME}-${ARCH}
EOF
    if [[ "${NATIVE}" == "1" ]]; then
        log "cloud-init シードを生成（native: mkisofs/hdiutil 等）"
        _make_cidata_iso "${WORKDIR}/seed.img" "${WORKDIR}/user-data" "${WORKDIR}/meta-data"
    else
        log "cloud-init シードを生成（cloud-localds）"
        helper cloud-localds seed.img user-data meta-data
    fi
}

# OpenBSD autoinstall(8) の応答ファイル。installer から
# http://10.0.2.2:8000/auto_install.conf として取得させる（slirp のゲートウェイ =
# qemu を実行している helper コンテナ自身。同コンテナ内で python3 -m http.server を上げる）。
_write_openbsd_autoinstall() {
    local pub; pub="$(cat "${KEY}.pub")"
    # 応答ファイルに無い質問はインストーラの**既定値**が使われる。必要最小限だけ書く。
    # ディスクは miniroot が sd0、インストール先の qcow2 が sd1（_write_boot 参照）。
    local parttable="whole"
    [[ "${ARCH}" == "aarch64" ]] && parttable="GPT"
    cat > "${WORKDIR}/auto_install.conf" <<EOF
System hostname = veil-${OS_NAME}-${ARCH}
Password for root = ${VM_ROOT_PASSWORD}
Allow root ssh login = prohibit-password
Public ssh key for root account = ${pub}
Network interfaces = vio0
IPv4 address for vio0 = dhcp
Setup a user = no
What timezone are you in = UTC
Which disk is the root disk = sd1
Use (W)hole disk MBR, whole disk (G)PT or (E)dit = ${parttable}
Use (A)uto layout, (E)dit auto layout, or create (C)ustom layout = auto
Location of sets = http
HTTP Server = cdn.openbsd.org
Unable to connect using https. Use http instead = yes
Set name(s) = -game* -x*
Directory does not contain SHA256.sig. Continue without verification = yes
EOF
    log "auto_install.conf を生成: ${WORKDIR}/auto_install.conf"
}

# ---------------------------------------------------------------------------
# up / down
# ---------------------------------------------------------------------------
_kvm_args() {
    # x86_64 ゲスト × x86_64 ホスト で /dev/kvm があれば KVM 加速する。
    if [[ "${ARCH}" == "x86_64" && "$(uname -m)" == "x86_64" && -r /dev/kvm ]]; then
        echo "--device=/dev/kvm"
    fi
}

# native モードでの EDK2/OVMF ファーム探索。Homebrew qemu（macOS）の配置と
# Linux ディストリ（AAVMF/OVMF パッケージ）の配置の両方を試す。
_native_fw_aarch64_code() {
    local f
    for f in /opt/homebrew/share/qemu/edk2-aarch64-code.fd \
             /usr/local/share/qemu/edk2-aarch64-code.fd \
             /usr/share/AAVMF/AAVMF_CODE.fd; do
        [[ -f "${f}" ]] && { echo "${f}"; return 0; }
    done
    return 1
}
_native_fw_x86_64_code() {
    local f
    for f in /opt/homebrew/share/qemu/edk2-x86_64-code.fd \
             /usr/local/share/qemu/edk2-x86_64-code.fd \
             /usr/share/OVMF/OVMF_CODE.fd; do
        [[ -f "${f}" ]] && { echo "${f}"; return 0; }
    done
    return 1
}
_native_fw_x86_64_vars() {
    local f
    for f in /opt/homebrew/share/qemu/edk2-i386-vars.fd \
             /usr/local/share/qemu/edk2-i386-vars.fd \
             /usr/share/OVMF/OVMF_VARS.fd; do
        [[ -f "${f}" ]] && { echo "${f}"; return 0; }
    done
    return 1
}

# native モード用 boot.sh を生成する。docker 版と drives/seed_drive/console_args は
# 完全に共通（呼び出し元の _write_boot で計算済みのものをそのまま受け取る）で、
# 違いはアクセラレータ選択とファームウェアの入手経路（コンテナ内固定パス→ホスト探索）
# ・起動コマンド（docker run → 直接 exec）だけ。
_write_boot_native() {
    local drives="$1" seed_drive="$2" console_args="$3"
    # macOS の /bin/bash（3.2）でも動く 64MiB ゼロ埋めパディング（truncate が無い
    # 環境向けに dd ベースのフォールバックを用意する）。boot.sh 内で使うため、
    # 生成先スクリプトへそのままの文字列として埋め込む（ヒアドキュメントは非quoted
    # だが、この変数の中身は既に展開済みの文字列なので $out 等は再展開されない）。
    local pad64m_fn='_pad64m() {
    local out="$1" src="${2:-}"
    if command -v truncate >/dev/null 2>&1; then
        truncate -s 64m "${out}"
    else
        dd if=/dev/zero of="${out}" bs=1m count=64 2>/dev/null
    fi
    [ -n "${src}" ] && dd if="${src}" of="${out}" conv=notrunc 2>/dev/null
    return 0
}'

    if [[ "${ARCH}" == "x86_64" ]]; then
        command -v qemu-system-x86_64 >/dev/null 2>&1 \
            || die "qemu-system-x86_64 が PATH に無い（brew install qemu）"
        local accel="tcg" cpu="qemu64"
        if [[ "$(uname -m)" == "x86_64" && -r /dev/kvm ]]; then accel="kvm"; cpu="host"; fi
        if [[ "${VM_FIRMWARE}" == "uefi" ]]; then
            local fw_code fw_vars
            fw_code="$(_native_fw_x86_64_code)" \
                || die "x86_64 UEFI ファーム(edk2-x86_64-code.fd / OVMF_CODE.fd)が見つからない（brew install qemu）"
            fw_vars="$(_native_fw_x86_64_vars)" \
                || die "x86_64 UEFI VARS(edk2-i386-vars.fd / OVMF_VARS.fd)が見つからない（brew install qemu）"
            cat > "${WORKDIR}/boot.sh" <<EOF
#!/bin/bash
set -e
cd "${WORKDIR}"
${pad64m_fn}
[ -f efi_code.fd ] || cp "${fw_code}" efi_code.fd
[ -f efi_vars.fd ] || cp "${fw_vars}" efi_vars.fd
exec qemu-system-x86_64 -machine q35,accel=${accel} -cpu ${cpu} -smp ${VM_SMP} -m ${VM_MEM_MB} \
  -drive if=pflash,format=raw,readonly=on,file=efi_code.fd \
  -drive if=pflash,format=raw,file=efi_vars.fd \
  ${drives} \
  ${seed_drive}-netdev user,id=net0,hostfwd=tcp:0.0.0.0:${SSH_PORT}-:22 \
  -device virtio-net-pci,netdev=net0 \
  ${console_args}
EOF
        else
            cat > "${WORKDIR}/boot.sh" <<EOF
#!/bin/bash
set -e
cd "${WORKDIR}"
exec qemu-system-x86_64 -machine pc,accel=${accel} -cpu ${cpu} -smp ${VM_SMP} -m ${VM_MEM_MB} \
  ${drives} \
  ${seed_drive}-netdev user,id=net0,hostfwd=tcp:0.0.0.0:${SSH_PORT}-:22 \
  -device virtio-net-pci,netdev=net0 \
  ${console_args}
EOF
        fi
    else
        command -v qemu-system-aarch64 >/dev/null 2>&1 \
            || die "qemu-system-aarch64 が PATH に無い（brew install qemu）"
        local fw_code accel_args
        fw_code="$(_native_fw_aarch64_code)" \
            || die "aarch64 UEFI ファーム(edk2-aarch64-code.fd / AAVMF_CODE.fd)が見つからない（brew install qemu）"
        # Apple Silicon（Darwin + arm64 ホスト）かつ qemu が hvf アクセラレータを
        # サポートしていれば HVF でネイティブ速度のまま aarch64 ゲストを起動できる。
        # それ以外（x86_64 ホストでの aarch64 ゲスト等）は従来どおり TCG。
        if [[ "$(uname -s)" == "Darwin" && "$(uname -m)" == "arm64" ]] \
            && qemu-system-aarch64 -accel help 2>/dev/null | grep -qi '^hvf'; then
            accel_args="-machine virt,accel=hvf,gic-version=3 -cpu host"
            log "Apple Silicon + HVF で aarch64 ゲストをアクセラレーションする"
        else
            accel_args="-machine virt -cpu cortex-a72"
        fi
        cat > "${WORKDIR}/boot.sh" <<EOF
#!/bin/bash
set -e
cd "${WORKDIR}"
${pad64m_fn}
[ -f efi_code.img ] || _pad64m efi_code.img "${fw_code}"
[ -f varstore.img ] || _pad64m varstore.img
exec qemu-system-aarch64 ${accel_args} -smp ${VM_SMP} -m ${VM_MEM_MB} \
  -drive if=pflash,format=raw,file=efi_code.img,readonly=on \
  -drive if=pflash,format=raw,file=varstore.img \
  ${drives} \
  ${seed_drive}-netdev user,id=net0,hostfwd=tcp:0.0.0.0:${SSH_PORT}-:22 \
  -device virtio-net-pci,netdev=net0,romfile= \
  ${console_args}
EOF
    fi
    chmod +x "${WORKDIR}/boot.sh"
}

_write_boot() {
    local phase="${1:-}"

    # --- ルートディスク -------------------------------------------------------
    # オーバーレイ運用（BASE_IMG）では backing file を読み取り専用でマウントするため、
    # qemu が backing に共有 write ロックを取ろうとして
    # `Could not open backing file: Failed to get shared "write" lock` になる。
    # backing のロックだけ無効化する（backing は qemu が O_RDONLY で開くので安全）。
    local drive_opts=""
    [[ -f "${WORKDIR}/.base_img_dir" ]] && drive_opts=",backing.file.locking=off"

    # ルートディスクは素の `-drive if=virtio,index=0` にする。
    # `-drive if=none` + `-device virtio-blk-pci,bootindex=0` へ変えたところ、
    # FreeBSD の boot2 が `Booting from Hard Disk...` のスピナーのまま進まなくなった
    # （実測）。ブート候補を増やさないため、追加メディアは下で CD-ROM として繋ぐ。
    local drives
    if [[ "${OS_NAME}" == "openbsd" && "${phase}" == "install" ]]; then
        # OpenBSD の install フェーズだけ miniroot を 1 台目（ゲストからは sd0）、
        # ターゲット qcow2 を 2 台目（sd1）にする。
        # auto_install.conf の「root disk = sd1」はこの並びに対応する。
        drives="-drive if=virtio,format=raw,file=miniroot.img,index=0 \\
  -drive if=virtio,format=qcow2,file=${IMG_NAME},index=1"
    else
        drives="-drive if=virtio,format=qcow2,file=${IMG_NAME},index=0${drive_opts}"
    fi

    # --- 追加メディア（cloud-init シード） -----------------------------------
    # FreeBSD は cloud-init シード（ISO9660、ラベル `cidata`）を繋ぐ。
    # **CD-ROM として繋ぐ**のが要点。virtio-blk のディスクとして足すと、FreeBSD の
    # ローダが起動デバイスを取り違えて `Failed to load kernel 'kernel'` になる
    # （bootindex を明示しても再現）。CD-ROM は NoCloud データソースの標準形でもある。
    local seed_drive=""
    if [[ "${OS_NAME}" == "freebsd" && -f "${WORKDIR}/seed.img" ]]; then
        if [[ "${ARCH}" == "x86_64" ]]; then
            seed_drive="-drive if=none,id=seed0,format=raw,file=seed.img,media=cdrom \\
  -device ide-cd,drive=seed0 \\
  "
        else
            # aarch64 の virt マシンには IDE が無いので virtio-scsi 経由の CD-ROM にする
            seed_drive="-device virtio-scsi-pci,id=scsi0 \\
  -drive if=none,id=seed0,format=raw,file=seed.img,media=cdrom \\
  -device scsi-cd,bus=scsi0.0,drive=seed0 \\
  "
        fi
    fi

    # --- コンソール ------------------------------------------------------------
    # `-serial ...,server,nowait` は**接続前の出力を捨てる**。ローダメニューなど
    # 起動直後の出力を見たいときは VM 起動直後にコンソールへ接続すること。
    # QMP は send-key / screendump / system_powerdown に使う（両アーキで公開する）。
    # `CONSOLE_WAIT=1` のときは `server,wait` にして **qemu がコンソール接続を待つ**。
    # `nowait` だと接続前の出力が捨てられ、ブートローダのプロンプト
    # （OpenBSD の `boot>` や FreeBSD のローダメニュー）を取り逃す競合が起きる。
    # provision のようにプロンプトを確実に掴みたい場面で使う。
    local con_mode="nowait"
    [[ "${CONSOLE_WAIT:-0}" == "1" ]] && con_mode="wait"
    local console_args="-nographic -serial telnet:0.0.0.0:${CON_PORT},server,${con_mode} \\
  -qmp telnet:0.0.0.0:${QMP_PORT},server,nowait -monitor none"

    if [[ "${NATIVE}" == "1" ]]; then
        _write_boot_native "${drives}" "${seed_drive}" "${console_args}"
        return 0
    fi

    if [[ "${ARCH}" == "x86_64" ]]; then
        local accel="tcg" cpu="qemu64"
        if [[ "$(uname -m)" == "x86_64" && -r /dev/kvm ]]; then accel="kvm"; cpu="host"; fi
        if [[ "${VM_FIRMWARE}" == "uefi" ]]; then
            # UEFI(OVMF) + q35。`BASE_IMG` に**配布の素の VM-IMAGE 由来イメージ**を
            # 指す場合は SeaBIOS ではカーネルまで進まず画面が真っ黒のまま止まるため、
            # こちらを使う（実測）。この経路ではカーネルの出力は
            # `Dual Console: Serial Primary` 以降シリアルへ移る。
            cat > "${WORKDIR}/boot.sh" <<EOF
#!/bin/bash
set -e
cd /w
[ -f efi_code.fd ] || cp /usr/share/OVMF/OVMF_CODE.fd efi_code.fd
[ -f efi_vars.fd ] || cp /usr/share/OVMF/OVMF_VARS.fd efi_vars.fd
exec qemu-system-x86_64 -machine q35,accel=${accel} -cpu ${cpu} -smp ${VM_SMP} -m ${VM_MEM_MB} \
  -drive if=pflash,format=raw,readonly=on,file=efi_code.fd \
  -drive if=pflash,format=raw,file=efi_vars.fd \
  ${drives} \
  ${seed_drive}-netdev user,id=net0,hostfwd=tcp:0.0.0.0:${SSH_PORT}-:22 \
  -device virtio-net-pci,netdev=net0 \
  ${console_args}
EOF
        else
        # **BIOS(SeaBIOS) で起動する**。UEFI(OVMF) では FreeBSD の efiboot / カーネルが
        # EFI コンソールを使い、`console="comconsole"` を設定してもシリアルへ出力されない
        # （実測）。SeaBIOS 経路なら SeaBIOS・boot2・ローダ・カーネルすべてがシリアルに出る。
        cat > "${WORKDIR}/boot.sh" <<EOF
#!/bin/bash
set -e
cd /w
exec qemu-system-x86_64 -machine pc,accel=${accel} -cpu ${cpu} -smp ${VM_SMP} -m ${VM_MEM_MB} \
  ${drives} \
  ${seed_drive}-netdev user,id=net0,hostfwd=tcp:0.0.0.0:${SSH_PORT}-:22 \
  -device virtio-net-pci,netdev=net0 \
  ${console_args}
EOF
        fi
    else
        # aarch64 は UEFI(AAVMF)。arm64 の BSD は efiboot がファームウェアの ConOut を
        # 引き継ぐため、シリアルが既定で使える（amd64 と事情が違う）。
        # virtio-net-pci には romfile=（空）が必須（efi-virtio.rom 不足で起動失敗するため）。
        cat > "${WORKDIR}/boot.sh" <<EOF
#!/bin/bash
set -e
cd /w
[ -f efi_code.img ] || { truncate -s 64m efi_code.img; dd if=/usr/share/AAVMF/AAVMF_CODE.fd of=efi_code.img conv=notrunc 2>/dev/null; }
[ -f varstore.img ] || truncate -s 64m varstore.img
exec qemu-system-aarch64 -machine virt -cpu cortex-a72 -smp ${VM_SMP} -m ${VM_MEM_MB} \
  -drive if=pflash,format=raw,file=efi_code.img,readonly=on \
  -drive if=pflash,format=raw,file=varstore.img \
  ${drives} \
  ${seed_drive}-netdev user,id=net0,hostfwd=tcp:0.0.0.0:${SSH_PORT}-:22 \
  -device virtio-net-pci,netdev=net0,romfile= \
  ${console_args}
EOF
    fi
}

# native モードで qemu.pid の指すプロセスを止める。
#   $1 = "force" : 即座に kill -9（docker 版の `docker rm -f` と同じ、猶予なし）。
#   省略時        : QMP で ACPI シャットダウンを試みてから kill -9 にフォールバック
#                   （cmd_down で使用）。
_native_stop() {
    local mode="${1:-graceful}"
    [[ -f "${WORKDIR}/qemu.pid" ]] || return 0
    local pid; pid="$(cat "${WORKDIR}/qemu.pid" 2>/dev/null || true)"
    if [[ -n "${pid}" ]] && kill -0 "${pid}" 2>/dev/null; then
        if [[ "${mode}" == "graceful" ]]; then
            python3 "${HERE}/qmp-sendkeys.py" --port "${QMP_PORT}" --powerdown >/dev/null 2>&1 || true
            local waited=0
            while (( waited < ${DOWN_TIMEOUT:-60} )); do
                kill -0 "${pid}" 2>/dev/null || break
                sleep 3; waited=$((waited + 3))
            done
        fi
        kill -0 "${pid}" 2>/dev/null && kill -9 "${pid}" 2>/dev/null
    fi
    rm -f "${WORKDIR}/qemu.pid"
    return 0
}

cmd_up() {
    _write_boot "${1:-}"
    if [[ "${NATIVE}" == "1" ]]; then
        # 前回分が残っていれば docker rm -f 相当（即時 kill）で片付ける
        _native_stop force
        nohup bash "${WORKDIR}/boot.sh" > "${WORKDIR}/qemu.log" 2>&1 &
        local qemu_pid=$!
        disown "${qemu_pid}" 2>/dev/null || disown 2>/dev/null || true
        echo "${qemu_pid}" > "${WORKDIR}/qemu.pid"
        log "起動: console=telnet 127.0.0.1:${CON_PORT}, ssh=127.0.0.1:${SSH_PORT}（pid=${qemu_pid}）"
        if [[ "${ARCH}" == "aarch64" ]]; then
            if [[ "$(uname -s)" == "Darwin" && "$(uname -m)" == "arm64" ]]; then
                log "aarch64 は HVF で加速されるためネイティブ速度で起動する"
            else
                log "aarch64 は TCG のため multi-user 到達に数分〜数十分かかる"
            fi
        fi
        return 0
    fi
    docker rm -f "${NAME}" >/dev/null 2>&1 || true
    # オーバーレイ運用時は backing file を /base（読み取り専用）で見せる。
    local base_mount=()
    if [[ -f "${WORKDIR}/.base_img_dir" ]]; then
        base_mount=(-v "$(cat "${WORKDIR}/.base_img_dir"):/base:ro")
    fi
    # shellcheck disable=SC2046  # _kvm_args は 0/1 個の引数を意図的に展開する
    docker run -d --name "${NAME}" $(_kvm_args) "${base_mount[@]}" \
        -p "${SSH_PORT}:${SSH_PORT}" -p "${CON_PORT}:${CON_PORT}" -p "${QMP_PORT}:${QMP_PORT}" \
        -v "${WORKDIR}:/w" -w /w "${HELPER_IMG}" bash /w/boot.sh >/dev/null
    log "起動: console=telnet 127.0.0.1:${CON_PORT}, ssh=127.0.0.1:${SSH_PORT}"
    [[ "${ARCH}" == "aarch64" ]] && log "aarch64 は TCG のため multi-user 到達に数分〜数十分かかる"
    return 0
}

# VM を止める。**まず QMP で ACPI シャットダウンを試み**、ゲストのファイルシステムを
# 壊さないようにする（`docker rm -f` は qemu を SIGKILL するため、cloud-init/growfs の
# 書き込み中に落とすとイメージが壊れて次回 boot2 が回り続ける = 実測）。
cmd_down() {
    if [[ "${NATIVE}" == "1" ]]; then
        _native_stop graceful
        log "removed ${NAME}"
        return 0
    fi
    if docker ps --filter "name=^/${NAME}$" --format '{{.Names}}' 2>/dev/null | grep -q .; then
        if python3 "${HERE}/qmp-sendkeys.py" --port "${QMP_PORT}" --powerdown >/dev/null 2>&1; then
            local waited=0
            while (( waited < ${DOWN_TIMEOUT:-60} )); do
                docker ps --filter "name=^/${NAME}$" --format '{{.Names}}' 2>/dev/null | grep -q . || break
                sleep 3; waited=$((waited + 3))
            done
        fi
    fi
    docker rm -f "${NAME}" >/dev/null 2>&1 || true
    log "removed ${NAME}"
}

cmd_status() {
    if [[ "${NATIVE}" == "1" ]]; then
        if [[ -f "${WORKDIR}/qemu.pid" ]] && kill -0 "$(cat "${WORKDIR}/qemu.pid")" 2>/dev/null; then
            echo "${NAME} running (pid $(cat "${WORKDIR}/qemu.pid"))"
        else
            echo "${NAME} not running"
        fi
    else
        docker ps -a --filter "name=^/${NAME}$" --format '{{.Names}} {{.Status}}' || true
    fi
    if ssh "${SSH_OPTS[@]}" -o ConnectTimeout=5 "${SSH_USER}@127.0.0.1" true 2>/dev/null; then
        echo "ssh: reachable"
    else
        echo "ssh: unreachable"
    fi
}

# シリアルコンソールは `-serial telnet:...` に出るため docker logs には現れない。
# telnet ポートへ繋いで一定時間読み出す（対話したい場合は telnet で直接繋ぐ）。
cmd_console() {
    local secs="${1:-5}"
    log "シリアルコンソール（telnet 127.0.0.1:${CON_PORT}）を ${secs}s 読み出す"
    log "対話する場合: telnet 127.0.0.1 ${CON_PORT}"
    timeout "${secs}" python3 "${HERE}/console-dump.py" "${CON_PORT}" || true
}

cmd_wait() {
    local timeout="${1:-1800}" waited=0
    log "SSH 到達待ち（最大 ${timeout}s）"
    while (( waited < timeout )); do
        if ssh "${SSH_OPTS[@]}" -o ConnectTimeout=10 "${SSH_USER}@127.0.0.1" true 2>/dev/null; then
            log "SSH 到達"; return 0
        fi
        sleep 10; waited=$((waited + 10))
    done
    die "SSH に到達できなかった（${timeout}s）"
}

# ---------------------------------------------------------------------------
# provision / grow
# ---------------------------------------------------------------------------
cmd_provision() {
    if [[ "${OS_NAME}" == "freebsd" ]]; then
        # 2 段構え:
        #   1. cloud-init（BASIC-CLOUDINIT イメージ + NoCloud シード）が root
        #      パスワード設定と growfs を行う。
        #   2. **シリアルコンソールから root ログインして SSH 公開鍵を注入**する。
        #      FreeBSD 版 cloud-init は `chpasswd` は適用するが `write_files` /
        #      `runcmd` は実行しないため、鍵注入は cloud-init に任せられない（実測）。
        [[ -f "${WORKDIR}/seed.img" ]] || die "cloud-init シードがない。先に setup を実行すること"
        log "VM を起動（初回は growfs / freebsd-update で 10 分以上かかることがある）"
        cmd_down
        cmd_up
        sleep 5
        log "シリアルコンソールから root ログインして SSH 鍵を注入"
        python3 "${HERE}/freebsd-provision.py" --mode login --con-port "${CON_PORT}" \
            --pubkey "${KEY}.pub" --password "${VM_ROOT_PASSWORD}"
        log "SSH 到達を確認"
        cmd_wait "${1:-600}"
        cmd_ssh 'uname -a'
        log "provision 完了"
    elif [[ "${OS_NAME}" == "openbsd" ]]; then
        # OpenBSD は autoinstall(8) がインストールと同時に SSH 公開鍵まで入れる。
        # miniroot を 1 台目に繋いだ install フェーズを起動し、応答ファイルを与える。
        [[ -f "${WORKDIR}/auto_install.conf" ]] || die "auto_install.conf が無い。先に setup を実行すること"
        log "OpenBSD autoinstall を実行（miniroot 起動 → 応答ファイル取得 → インストール）"
        cmd_down
        # ブートローダの `boot>` を確実に掴むため、qemu にコンソール接続を待たせる。
        # NOTE: bash では「VAR=x 関数呼び出し」の代入がシェルに**残る**ため、
        #       明示的に export → unset する（残ると再起動側も wait になり永久に起動しない）。
        CONSOLE_WAIT=1 cmd_up install
        unset CONSOLE_WAIT
        if [[ "${NATIVE}" == "1" ]]; then
            # native モード: helper コンテナが無いので、応答ファイル配布用の
            # HTTP サーバはホスト上で直接 python3 -m http.server を起動する
            # （--container を省略すると openbsd-autoinstall.py が native 動作する）。
            python3 "${HERE}/openbsd-autoinstall.py" --con-port "${CON_PORT}" --workdir "${WORKDIR}" \
                --arch "${ARCH}"
        else
            python3 "${HERE}/openbsd-autoinstall.py" --con-port "${CON_PORT}" --workdir "${WORKDIR}" \
                --container "${NAME}" --arch "${ARCH}"
        fi
        log "autoinstall 完了。miniroot を外して再起動する"
        cmd_down
        cmd_up
        log "SSH 到達を確認"
        cmd_wait "${1:-1200}"
        cmd_ssh 'uname -a'
        log "provision 完了"
    else
        # NetBSD（F-140）。x86_64/aarch64 とも起動可能な生イメージなので
        # cloud-init 相当は無く、FreeBSD の --mode login と同じ発想でシリアルへ
        # root ログインして SSH 鍵を注入する（netbsd-provision.py、両アーキ共通）。
        #
        # aarch64 は当初 install ISO から sysinst をシリアル自動操作する専用
        # スクリプトを使っていたが、実機（Apple Silicon + QEMU/HVF）で sysinst の
        # 言語選択メニューのまま止まり動作しないことが判明したため廃止し、x86_64 と
        # 同じブータブルイメージ経路に統一した。
        log "NetBSD イメージを起動し、シリアルから root ログインして SSH 鍵を注入"
        cmd_down
        # `nowait` + 固定 sleep だと、ブートローダのメニュー選択猶予（既定 5 秒の
        # カウントダウン）が qemu 起動〜python 接続までのオーバーヘッドだけで
        # 使い切られてしまい、シリアルコンソールへの切り替え操作（consdev com0）
        # が間に合わない（実測: B-52/B-54 と同種の取りこぼし）。
        # OpenBSD の install フェーズに倣い、`CONSOLE_WAIT=1`
        # （`-serial ...,server,wait`）でコンソール接続まで出力を保持させ、
        # メニューのカウントダウンを丸ごと使えるようにする。
        CONSOLE_WAIT=1 cmd_up
        unset CONSOLE_WAIT
        python3 "${HERE}/netbsd-provision.py" --con-port "${CON_PORT}" \
            --pubkey "${KEY}.pub" --password "${VM_ROOT_PASSWORD}"
        log "SSH 到達を確認"
        # aarch64 は TCG ホストだと multi-user 到達まで数十分かかりうるため
        # x86_64 より長めの既定タイムアウトにする。
        local default_wait=600
        [[ "${ARCH}" == "aarch64" ]] && default_wait=1800
        cmd_wait "${1:-${default_wait}}"
        cmd_ssh 'uname -a'
        log "provision 完了"
    fi
}

# 起動用オーバーレイを作り直して初期状態へ戻す（base.qcow2 は再利用するので DL 不要）。
# cloud-init も instance-id が変わるので設定を再適用する。
cmd_reset() {
    [[ -f "${WORKDIR}/base.qcow2" ]] || die "base.qcow2 が無い。先に setup を実行すること"
    cmd_down
    rm -f "${IMG}"
    _create_overlay
    # cloud-init シードは FreeBSD の BASIC-CLOUDINIT イメージ専用。
    # NetBSD x86_64 も base.qcow2 を持つが cloud-init は無いので対象外
    # （provision は毎回シリアルログインで鍵注入し直す）。
    [[ "${OS_NAME}" == "freebsd" ]] && _write_cloudinit_seed
    log "reset 完了（次の up で初期状態から起動する）"
}

cmd_grow() {
    [[ "${OS_NAME}" == "freebsd" ]] || { log "grow は FreeBSD 専用（OpenBSD は autoinstall 時に全ディスクを使う）"; return 0; }
    log "qcow2 を +${GROW_GB}G 拡張 → single-user で growfs"
    cmd_down; sleep 2
    helper qemu-img resize "${IMG_NAME}" "+${GROW_GB}G"
    cmd_up; sleep 6
    # gpart でパーティションを広げてから growfs（provision.py が fsck+growfs+reboot）。
    python3 - "${CON_PORT}" <<'PY'
import socket, sys, time
port = int(sys.argv[1])
s = socket.socket(); s.settimeout(6)
for _ in range(30):
    try:
        s.connect(("127.0.0.1", port)); break
    except OSError:
        time.sleep(2)
time.sleep(1)
s.settimeout(4)
deadline = time.time() + 300
while time.time() < deadline:
    try:
        d = s.recv(16384)
        if d and (b"Boot Multi user" in d or b"Autoboot" in d):
            break
    except OSError:
        pass
s.sendall(b"2"); time.sleep(30)
s.sendall(b"\r\n"); time.sleep(2)
s.sendall(b"gpart recover vtbd0 ; gpart resize -i 3 vtbd0\r\n"); time.sleep(10)
s.close()
PY
    python3 "${HERE}/freebsd-provision.py" --mode grow --con-port "${CON_PORT}"
    log "grow 完了"
}

# ---------------------------------------------------------------------------
# toolchain / sync / build / e2e / fetch
# ---------------------------------------------------------------------------
cmd_ssh() { ssh "${SSH_OPTS[@]}" "${SSH_USER}@127.0.0.1" "$@"; }
cmd_scp() { scp "${SCP_OPTS[@]}" "$@"; }

# VM 内のリポジトリルート。
#
# OpenBSD は autoinstall の auto layout が `/` を 628M 程度しか取らないため、
# `/root` 配下ではビルド成果物が入らない（`No space left on device`）。
# auto layout で最も大きい `/usr/obj`（24G ディスクで ~8G）を使う。
# FreeBSD は `/` が単一の大きな領域なので `/root` でよい。
if [[ "${OS_NAME}" == "openbsd" ]]; then
    GUEST_ROOT="${GUEST_ROOT:-/usr/obj/veil-proxy}"
else
    # NetBSD もひとまず FreeBSD と同じ /root 配下（live image のパーティション構成が
    # OpenBSD の autoinstall auto layout ほど狭いかどうかは未検証。狭ければ OpenBSD と
    # 同様に GUEST_ROOT を広いパーティションへ変える必要がある）。
    GUEST_ROOT="${GUEST_ROOT:-/root/veil-proxy}"
fi

cmd_toolchain() {
    if [[ "${OS_NAME}" == "freebsd" ]]; then
        # gmake は tikv-jemalloc-sys（`full-freebsd` の jemalloc）のビルドに必須。
        # 無いと `failed to execute command: No such file or directory` で落ちる。
        # gmake  : tikv-jemalloc-sys（`full-freebsd` の jemalloc）のビルドに必須
        # protobuf: tests/grpc_server の prost-build が protoc を要求する（E2E に必要）
        log "pkg install rust cmake llvm gmake protobuf bash curl nasm git pkgconf"
        cmd_ssh 'env IGNORE_OSVERSION=yes ASSUME_ALWAYS_YES=yes pkg install -y rust cmake llvm gmake protobuf bash curl nasm git pkgconf >/tmp/pkg.log 2>&1 || { tail -20 /tmp/pkg.log; exit 1; }'
    elif [[ "${OS_NAME}" == "netbsd" ]]; then
        # NetBSD（F-140）: pkgsrc のバイナリパッケージ（pkgin）で rust-bin を導入する。
        # ソースからの rust ビルドは QEMU 上で数時間かかるため **必ず rust-bin を使う**
        # （`rust` パッケージ = ソースビルドを引くパッケージとは別物）。
        # NetBSD の SSH 非対話シェルの既定 PATH には /usr/sbin が入っておらず
        # （実測: PATH=/usr/bin:/bin:/usr/pkg/bin:/usr/local/bin）、pkg_add(8) は
        # /usr/sbin にあるため「pkg_add: not found」になる。絶対パスで呼ぶ。
        # また `uname -m` は amd64 を返すが pkgsrc パッケージのパス要素は x86_64
        # （amd64 は x86_64 へ 302 リダイレクトされる。ARCH は本スクリプトの
        # 引数なので既に x86_64/aarch64 で pkgsrc のパスと一致している）。
        local pkg_path="https://cdn.NetBSD.org/pub/pkgsrc/packages/NetBSD/${ARCH}/${NETBSD_PKG_VER}/All/"
        log "pkgin bootstrap + rust-bin/cmake/llvm 導入（PKG_PATH=${pkg_path}）"
        cmd_ssh "set -e
export PATH=/usr/pkg/bin:/usr/pkg/sbin:/usr/sbin:/sbin:\$PATH
export PKG_PATH='${pkg_path}'
if [ ! -x /usr/pkg/bin/pkgin ]; then
  echo 'pkgin が無いので /usr/sbin/pkg_add で bootstrap する'
  /usr/sbin/pkg_add -v pkgin >/tmp/pkg_add.log 2>&1 || { tail -40 /tmp/pkg_add.log; exit 1; }
fi
/usr/pkg/bin/pkgin -y update >/tmp/pkgin.log 2>&1 || { tail -40 /tmp/pkgin.log; exit 1; }
/usr/pkg/bin/pkgin -y install rust-bin cmake llvm clang libressl protobuf gmake bash curl git nasm pkgconf mozilla-rootcerts-openssl >>/tmp/pkgin.log 2>&1 || { tail -60 /tmp/pkgin.log; exit 1; }
/usr/pkg/sbin/mozilla-rootcerts-openssl install >/dev/null 2>&1 || /usr/pkg/bin/mozilla-rootcerts-openssl install >/dev/null 2>&1 || true
"
    else
        # OpenBSD も同様に protobuf（protoc）と gmake が要る
        log "OpenBSD 用 cc ラッパを設置（C ファイルのみ -include pthread.h）"
        cmd_ssh 'cat > /usr/local/bin/veil-cc <<'"'"'WRAP'"'"'
#!/bin/sh
# BoringSSL(boring-sys) は pthread_rwlock_t が <sys/types.h> から見える前提だが、
# OpenBSD では <pthread.h> にしかない。C ファイルのときだけ pthread.h を先に読ませる。
# アセンブリ(.S/.s) には付けない（付けると zstd-sys 等のアセンブルが壊れる）。
for a in "$@"; do
  case "$a" in
    *.S|*.s) exec /usr/bin/cc "$@" ;;
  esac
done
exec /usr/bin/cc -include pthread.h "$@"
WRAP
chmod +x /usr/local/bin/veil-cc'
        # NOTE: OpenBSD には llvm-19/20/21 が並存するため、曖昧な `llvm` を
        # `pkg_add -I`（非対話）で指定すると**黙って入らない**。
        # bindgen（aws-lc-rs / boring-sys）が libclang を要求するので、
        # 利用可能な llvm から 1 つを選んで明示的に入れる。
        # boring-sys は非 Apple ターゲットで `-lstdc++` を要求するが、OpenBSD の
        # C++ 標準ライブラリは **libc++**（libstdc++ は存在しない）。
        # /usr/local/lib に libstdc++.so → libc++.so.N の互換リンクを置き、
        # ビルド時に `-L/usr/local/lib` を渡して解決する。
        log "libstdc++ → libc++ 互換リンクを設置（boring-sys の -lstdc++ 対策）"
        cmd_ssh 'set -e
mkdir -p /usr/local/lib
LIBCXX=$(ls -1 /usr/lib/libc++.so.* 2>/dev/null | sort -V | tail -1)
[ -n "$LIBCXX" ] || { echo "libc++ not found"; exit 1; }
ln -sf "$LIBCXX" /usr/local/lib/libstdc++.so
ls -l /usr/local/lib/libstdc++.so'
        log "pkg_add rust cmake gmake protobuf bash curl git + llvm（バージョン明示）"
        cmd_ssh 'set -e
P="PKG_PATH=https://cdn.openbsd.org/pub/OpenBSD/$(uname -r)/packages/$(uname -m)/"
env $P pkg_add -I rust cmake gmake protobuf bash curl git >/tmp/pkg.log 2>&1 || { tail -20 /tmp/pkg.log; exit 1; }
if ! find /usr/local -name "libclang*so*" 2>/dev/null | grep -q .; then
  LLVM=$(env $P pkg_info -Q llvm 2>/dev/null | grep -E "^llvm-[0-9]" | sort -V | tail -1)
  [ -n "$LLVM" ] || { echo "no llvm package found"; exit 1; }
  echo "installing $LLVM"
  env $P pkg_add -I "$LLVM" >>/tmp/pkg.log 2>&1 || { tail -20 /tmp/pkg.log; exit 1; }
fi'
    fi
    if [[ "${OS_NAME}" == "netbsd" ]]; then
        cmd_ssh 'export PATH=/usr/pkg/bin:/usr/pkg/sbin:/usr/sbin:/sbin:$PATH; cargo --version; cmake --version | head -1; gmake --version 2>/dev/null | head -1; protoc --version 2>/dev/null'
    else
        cmd_ssh 'cargo --version; cmake --version | head -1; gmake --version 2>/dev/null | head -1; protoc --version 2>/dev/null'
    fi
}

cmd_sync() {
    # up 直後に呼ばれると SSH がまだ上がっておらず 255 で落ちるため待つ
    cmd_wait "${SYNC_WAIT_TIMEOUT:-900}" >/dev/null 2>&1 || die "VM の SSH に到達できない"
    log "リポジトリを VM へ転送（tar over ssh）"
    cmd_ssh "mkdir -p ${GUEST_ROOT}"
    # fuzz はワークスペースメンバだがゲストでは不要。転送後に members から外す。
    # ホスト側のビルド成果物（target/）は転送しない。
    # 巨大なうえゲストのアーキ/OS では使えず、OpenBSD では容量不足の原因になる。
    #
    # third_party（B-55 の vendoring 済み wasmtime = veil-wasmtime）は path 依存なので、
    # 転送しないとゲスト側で `cargo` がワークスペース解決に失敗する。
    # 転送先ディレクトリは展開前に **削除する**。`tar xzf -` は追加・上書きしかせず
    # **ホスト側で削除したファイルがゲストに残り続ける**ため。実際に
    # `src/tls_provider.rs` → `src/tls_provider/mod.rs` へ移動した際、ゲストに旧
    # ファイルが残って `E0761: file for module found at both ...` でビルドが壊れた
    # （F-142、実機で検出）。`target/` は転送対象外なので消えない（ビルドキャッシュは維持）。
    (cd "${ROOT}" && tar czf - \
        --exclude='./target' --exclude='*/target' --exclude='.git' \
        src benches tests examples contrib docker/assets third_party \
        Cargo.toml Cargo.lock build.rs clippy.toml .cargo) \
      | cmd_ssh "cd ${GUEST_ROOT} \
          && rm -rf src benches tests examples contrib docker/assets third_party .cargo \
          && tar xzf - \
          && sed -i'' -e 's|members = \[\".\", \"fuzz\"\]|members = [\".\"]|' Cargo.toml"
}

# aws-lc-sys の bindgen が libclang を要求するため、VM 内での LIBCLANG_PATH を解決する
_guest_env_prefix() {
    # bindgen 用。OpenBSD は /usr/local/llvmNN/lib、FreeBSD は /usr/local/llvm-NN/lib に入る。
    local pre='LIBCLANG_PATH=$(find /usr/local -name "libclang.so*" 2>/dev/null | head -1 | xargs dirname)'
    if [[ "${OS_NAME}" == "openbsd" ]]; then
        # OpenBSD は autoinstall の auto layout で `/` が ~628M しかなく、既定の
        # CARGO_HOME（/root/.cargo）にレジストリを展開すると溢れる。
        # 大きい /usr/obj へ逃がす（GUEST_ROOT も同じ理由で /usr/obj 配下）。
        pre="${pre} CARGO_HOME=/usr/obj/cargo"
        # quiche が使う BoringSSL（boring-sys）は `pthread_rwlock_t` が
        # <sys/types.h> から見えることを前提にしている（glibc/FreeBSD/macOS では真）。
        # OpenBSD では <pthread.h> にしかないため
        #   openssl/thread.h:81: error: unknown type name 'pthread_rwlock_t'
        # で C ビルドが失敗する。
        #
        # `CFLAGS_<target>` に `-include pthread.h` を足すと **アセンブリ(.S) にも**
        # 適用されて zstd-sys 等が壊れるため、**C ファイルのときだけ** `-include` する
        # cc ラッパ（toolchain で設置）を CC として使う。
        pre="${pre} CC_x86_64_unknown_openbsd=/usr/local/bin/veil-cc"
        pre="${pre} CC_aarch64_unknown_openbsd=/usr/local/bin/veil-cc"
        # bindgen は cc ラッパを経由せず自前の clang でヘッダを解析するため、
        # 同じ `-include pthread.h` を bindgen 側にも渡す必要がある
        # （C ビルドが通っても bindgen が同じ thread.h:81 で落ちる）。
        pre="${pre} BINDGEN_EXTRA_CLANG_ARGS_x86_64_unknown_openbsd='-include pthread.h'"
        pre="${pre} BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_openbsd='-include pthread.h'"
        # 上記 libstdc++ 互換リンクを見つけさせる
        pre="${pre} RUSTFLAGS='-L /usr/local/lib'"
        # autoinstall の auto layout では /usr/obj が 24G ディスクでも ~8G しかなく、
        # デバッグ情報付きの dev プロファイル（target/debug が 5G 超 + incremental 1.5G）
        # だと **E2E のビルド中に `No space left on device` で落ちる**（実測）。
        # E2E はデバッガを使わないのでデバッグ情報とインクリメンタルを切って容量を稼ぐ。
        pre="${pre} CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0"
    fi
    if [[ "${OS_NAME}" == "netbsd" ]]; then
        # pkgsrc の rust-bin/cmake/llvm は /usr/pkg 配下に入り、libclang も
        # /usr/pkg/lib/llvmNN/lib 配下（OpenBSD/FreeBSD の /usr/local とは別系統）。
        # 非対話 ssh セッションには既定で /usr/pkg/{bin,sbin} が PATH に無い。
        pre='LIBCLANG_PATH=$(find /usr/pkg -name "libclang.so*" 2>/dev/null | head -1 | xargs dirname)'
        pre="${pre} PATH=/usr/pkg/bin:/usr/pkg/sbin:/usr/sbin:/sbin:\$PATH"
    fi
    echo "${pre}"
}

# BSD 向けの既定 feature セット（Cargo.toml）。
#   full-freebsd : full と同じ機能セット + アロケータを jemalloc + POSIX AIO(F-127) 有効
#   full-openbsd : full と同じ機能セット + システムアロケータ（mimalloc/jemalloc を使わない）
#                  + 同梱 rustls(ring)/quiche(BoringSSL)。
#   full-netbsd  : full-openbsd と同一方針（システムアロケータ + 同梱 TLS）。
# いずれも `--no-default-features` と併用する（default features の mimalloc を外すため）。
# aarch64 は wasmtime がビルドできないため wasm 抜きのセットを使う（B-55）。
_default_features() {
    local suffix=""
    [[ "${ARCH}" == "aarch64" ]] && suffix="-aarch64"
    case "${OS_NAME}" in
        freebsd) echo "full-freebsd${suffix}" ;;
        openbsd) echo "full-openbsd${suffix}" ;;
        netbsd) echo "full-netbsd${suffix}" ;;
    esac
}
CARGO_FEATURES="${CARGO_FEATURES:-$(_default_features)}"

cmd_build() {
    cmd_sync
    log "in-VM リリースビルド（--no-default-features --features ${CARGO_FEATURES}）"
    cmd_ssh "cd ${GUEST_ROOT} && $(_guest_env_prefix) cargo build --release --no-default-features --features '${CARGO_FEATURES}'"
    cmd_ssh "ls -l ${GUEST_ROOT}/target/release/veil"
}

# e2e: VM 内で tests/e2e_setup.sh test を実行する。
#   --prebuilt <path>  ホスト側の（Docker クロスビルド済み）veil バイナリを VM へ転送し、
#                      veil 本体のビルドを省略する（VEIL_E2E_SKIP_VEIL_BUILD=1）。
cmd_e2e() {
    local prebuilt=""
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --prebuilt) prebuilt="$2"; shift 2 ;;
            *) die "unknown e2e option: $1" ;;
        esac
    done

    cmd_sync
    local env_prefix; env_prefix="$(_guest_env_prefix)"
    if [[ -n "${prebuilt}" ]]; then
        [[ -f "${prebuilt}" ]] || die "prebuilt binary not found: ${prebuilt}"
        log "事前ビルド済みバイナリを転送: ${prebuilt}"
        cmd_ssh "mkdir -p ${GUEST_ROOT}/target/debug"
        cmd_scp "${prebuilt}" "${SSH_USER}@127.0.0.1:${GUEST_ROOT}/target/debug/veil"
        cmd_ssh "chmod +x ${GUEST_ROOT}/target/debug/veil"
        env_prefix="${env_prefix} VEIL_E2E_SKIP_VEIL_BUILD=1"
    fi
    log "in-VM E2E（tests/e2e_setup.sh test、features=${CARGO_FEATURES}）"
    cmd_ssh "cd ${GUEST_ROOT} && ${env_prefix} VEIL_E2E_NO_DEFAULT_FEATURES=1 VEIL_E2E_FEATURES='${CARGO_FEATURES}' bash tests/e2e_setup.sh test"
}

# packaging へ渡すためにビルド済みバイナリを取り出す
cmd_fetch() {
    local out_dir="${ROOT}/packaging/build"
    local arch_label; arch_label="${ARCH}"
    mkdir -p "${out_dir}"
    local dest="${out_dir}/veil-${OS_NAME}-${arch_label}"
    log "VM から release バイナリを取得 → ${dest}"
    cmd_scp "${SSH_USER}@127.0.0.1:${GUEST_ROOT}/target/release/veil" "${dest}"
    chmod +x "${dest}"
    cmd_ssh 'uname -r' > "${dest}.os-version"
    log "取得完了: ${dest}（OS バージョン: $(cat "${dest}.os-version")）"
    log "packaging: ./packaging/scripts/build-bsd.sh --os ${OS_NAME} --arch ${ARCH} --binary ${dest} --os-version \$(cat ${dest}.os-version)"
}

# setup から fetch まで一気に実行する（再現用のワンショット）。
# 途中で失敗したら、その段階のサブコマンドから手動で再開できる。
cmd_all() {
    cmd_setup
    cmd_provision
    cmd_toolchain
    cmd_build
    cmd_e2e
    cmd_fetch
    log "all 完了: packaging/build/veil-${OS_NAME}-${ARCH} を取得済み"
    log "パッケージ化: ./packaging/scripts/build-bsd.sh --os ${OS_NAME} --arch ${ARCH} --from-qemu"
}

case "${COMMAND}" in
    all) cmd_all ;;
    setup) cmd_setup ;;
    reset) cmd_reset ;;
    up) cmd_up "$@" ;;
    wait) cmd_wait "$@" ;;
    grow) cmd_grow ;;
    provision) cmd_provision ;;
    toolchain) cmd_toolchain ;;
    sync) cmd_sync ;;
    build) cmd_build ;;
    e2e) cmd_e2e "$@" ;;
    fetch) cmd_fetch ;;
    ssh) cmd_ssh "$@" ;;
    scp) cmd_scp "$@" ;;
    status) cmd_status ;;
    console) cmd_console "$@" ;;
    down) cmd_down ;;
    *) usage ;;
esac
