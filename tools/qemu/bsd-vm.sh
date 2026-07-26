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
# 使い方:
#   tools/qemu/bsd-vm.sh <os> <arch> <command> [args]
#     os   : freebsd | openbsd
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
#   VEIL_QEMU_DIR  VM 資材の親ディレクトリ（既定 ~/qemu-images）
#   BASE_IMG       プロビジョニング済みイメージ。setup でこれを backing file とする
#                  qcow2 オーバーレイを作る（元イメージは変更しない）
#   IMG            使用するディスクイメージのパス（既定 ${WORKDIR}/disk.qcow2）
#   KEY            SSH 鍵（既定 ~/.ssh/veil_qemu_key）
#   CARGO_FEATURES freature セット（既定: freebsd=full-freebsd / openbsd=full-openbsd。
#                  いずれも --no-default-features 併用でアロケータを差し替える）
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
case "${OS_NAME}" in freebsd|openbsd) ;; *) echo "ERROR: os must be freebsd|openbsd" >&2; usage ;; esac
case "${ARCH}" in x86_64|aarch64) ;; *) echo "ERROR: arch must be x86_64|aarch64" >&2; usage ;; esac
[[ -n "${COMMAND}" ]] || usage

VEIL_QEMU_DIR="${VEIL_QEMU_DIR:-${HOME}/qemu-images}"
WORKDIR="${WORKDIR:-${VEIL_QEMU_DIR}/${OS_NAME}-${ARCH}}"
KEY="${KEY:-${HOME}/.ssh/veil_qemu_key}"
HELPER_IMG="${HELPER_IMG:-veil-qemu:local}"
VM_SMP="${VM_SMP:-4}"
VM_MEM_MB="${VM_MEM_MB:-4096}"
GROW_GB="${GROW_GB:-24}"
FREEBSD_VER="${FREEBSD_VER:-14.3-RELEASE}"
# OpenBSD の CDN は直近数リリースしか保持しない（例: 7.6 は既に 404）。
# 既定は入手可能な最新に追従させ、古いリリースを使う場合は OPENBSD_VER で指定する
# （`curl -s https://cdn.openbsd.org/pub/OpenBSD/ | grep -oE '"[0-9]\.[0-9]/"'` で確認できる）。
OPENBSD_VER="${OPENBSD_VER:-7.9}"
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
    esac
}
PORT_BASE="$(_port_base)"
SSH_PORT="${SSH_PORT:-${PORT_BASE}}"
CON_PORT="${CON_PORT:-$((PORT_BASE + 1))}"

# ゲストの SSH ユーザ（FreeBSD/OpenBSD とも root で運用する）
SSH_USER="${SSH_USER:-root}"
SSH_OPTS=(-i "${KEY}" -p "${SSH_PORT}"
  -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
  -o ConnectTimeout=90 -o ServerAliveInterval=20 -o LogLevel=ERROR)

log() { echo "[${OS_NAME}-${ARCH}] $*" >&2; }
die() { echo "[${OS_NAME}-${ARCH}] ERROR: $*" >&2; exit 1; }

# helper コンテナ内で 1 コマンド実行（WORKDIR を /w にマウント）
helper() {
    if [[ "${IMG_DIR}" == "${WORKDIR}" ]]; then
        docker run --rm -v "${WORKDIR}:/w" -w /w "${HELPER_IMG}" "$@"
    else
        docker run --rm -v "${WORKDIR}:/w" -v "${IMG_DIR}:/img" -w /w "${HELPER_IMG}" "$@"
    fi
}

# ---------------------------------------------------------------------------
# イメージ URL
# ---------------------------------------------------------------------------
_image_url() {
    case "${OS_NAME}-${ARCH}" in
        freebsd-x86_64)
            echo "https://download.freebsd.org/releases/VM-IMAGES/${FREEBSD_VER}/amd64/Latest/FreeBSD-${FREEBSD_VER}-amd64.qcow2.xz" ;;
        freebsd-aarch64)
            echo "https://download.freebsd.org/releases/VM-IMAGES/${FREEBSD_VER}/aarch64/Latest/FreeBSD-${FREEBSD_VER}-arm64-aarch64.qcow2.xz" ;;
        openbsd-x86_64)
            echo "https://cdn.openbsd.org/pub/OpenBSD/${OPENBSD_VER}/amd64/install${OPENBSD_VER//./}.img" ;;
        openbsd-aarch64)
            echo "https://cdn.openbsd.org/pub/OpenBSD/${OPENBSD_VER}/arm64/install${OPENBSD_VER//./}.img" ;;
    esac
}

# ---------------------------------------------------------------------------
# setup
# ---------------------------------------------------------------------------
cmd_setup() {
    mkdir -p "${WORKDIR}"
    log "helper イメージを build（${HELPER_IMG}）"
    docker build -t "${HELPER_IMG}" "${HERE}/helper"
    [[ -f "${KEY}" ]] || { log "SSH 鍵を生成: ${KEY}"; ssh-keygen -t ed25519 -N '' -f "${KEY}" >/dev/null; }

    if [[ -n "${BASE_IMG:-}" ]]; then
        [[ -f "${BASE_IMG}" ]] || die "BASE_IMG not found: ${BASE_IMG}"
        local base_dir base_name
        base_dir="$(cd "$(dirname "${BASE_IMG}")" && pwd)"
        base_name="$(basename "${BASE_IMG}")"
        log "プロビジョニング済みイメージのオーバーレイを作成（元イメージは変更しない）: ${BASE_IMG}"
        docker run --rm -v "${WORKDIR}:/w" -v "${base_dir}:/base:ro" -w /w "${HELPER_IMG}" \
            qemu-img create -f qcow2 -F qcow2 -b "/base/${base_name}" "${IMG_NAME}" >/dev/null
        # 起動時にも backing file を同じパス（/base）で見せる必要があるため記録しておく
        echo "${base_dir}" > "${WORKDIR}/.base_img_dir"
        log "setup 完了（オーバーレイ: ${IMG}、backing: ${BASE_IMG}）"
        return 0
    fi

    local url; url="$(_image_url)"
    if [[ "${OS_NAME}" == "freebsd" ]]; then
        if [[ ! -f "${IMG}" ]]; then
            log "FreeBSD VM-IMAGE を DL + 展開: ${url}"
            curl -fL --retry 3 -o "${IMG}.xz" "${url}"
            xz -dc "${IMG}.xz" > "${IMG}"
            rm -f "${IMG}.xz"
        fi
    else
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
    fi
    log "setup 完了"
}

# OpenBSD autoinstall(8) の応答ファイル。installer から
# http://10.0.2.2:8000/auto_install.conf として取得させる（slirp のゲートウェイ =
# qemu を実行している helper コンテナ自身。同コンテナ内で python3 -m http.server を上げる）。
_write_openbsd_autoinstall() {
    local pub; pub="$(cat "${KEY}.pub")"
    cat > "${WORKDIR}/auto_install.conf" <<EOF
System hostname = veil-${ARCH}
Password for root = *************
Allow root ssh login = prohibit-password
Public ssh key for root account = ${pub}
Network interfaces = vio0
IPv4 address for vio0 = dhcp
Setup a user = no
What timezone are you in = UTC
Which disk is the root disk = sd1
Use (W)hole disk MBR, whole disk (G)PT or (E)dit = whole
Use (A)uto layout, (E)dit auto layout, or create (C)ustom layout = auto
Location of sets = http
HTTP Server = cdn.openbsd.org
Unable to connect using https. Use http instead = yes
Set name(s) = -game* -x* done
Directory does not contain SHA256.sig. Continue without verification = yes
Do you expect to run the X Window System = no
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

_write_boot() {
    # OpenBSD の install フェーズだけ miniroot を 1 台目（= 起動ディスク、ゲストからは
    # sd0）、ターゲット qcow2 を 2 台目（sd1）として繋ぐ。auto_install.conf の
    # 「root disk = sd1」はこの並びに対応する。install 後は miniroot を外して
    # ターゲットだけで起動する（そのとき sd0 になる）。
    # オーバーレイ運用（BASE_IMG）では backing file を読み取り専用マウントで見せるため、
    # qemu が backing に共有 write ロックを取ろうとして
    # `Could not open backing file: Failed to get shared "write" lock` で失敗する。
    # backing のロックだけ無効化する（backing は qemu が O_RDONLY で開くので安全）。
    local drive_opts=""
    [[ -f "${WORKDIR}/.base_img_dir" ]] && drive_opts=",backing.file.locking=off"

    local drives
    if [[ "${OS_NAME}" == "openbsd" && "${1:-}" == "install" ]]; then
        drives="-drive if=virtio,format=raw,file=miniroot.img,index=0 \\
  -drive if=virtio,format=qcow2,file=${IMG_NAME},index=1"
    else
        drives="-drive if=virtio,format=qcow2,file=${IMG_NAME},index=0${drive_opts}"
    fi

    if [[ "${ARCH}" == "x86_64" ]]; then
        local accel="tcg" cpu="qemu64"
        if [[ "$(uname -m)" == "x86_64" && -r /dev/kvm ]]; then accel="kvm"; cpu="host"; fi
        cat > "${WORKDIR}/boot.sh" <<EOF
#!/bin/bash
set -e
cd /w
exec qemu-system-x86_64 -machine q35,accel=${accel} -cpu ${cpu} -smp ${VM_SMP} -m ${VM_MEM_MB} \\
  ${drives} \\
  -netdev user,id=net0,hostfwd=tcp:0.0.0.0:${SSH_PORT}-:22 \\
  -device virtio-net-pci,netdev=net0 \\
  -nographic -serial telnet:0.0.0.0:${CON_PORT},server,nowait -monitor none
EOF
    else
        cat > "${WORKDIR}/boot.sh" <<EOF
#!/bin/bash
set -e
cd /w
[ -f efi_code.img ] || { truncate -s 64m efi_code.img; dd if=/usr/share/AAVMF/AAVMF_CODE.fd of=efi_code.img conv=notrunc 2>/dev/null; }
[ -f varstore.img ] || truncate -s 64m varstore.img
exec qemu-system-aarch64 -machine virt -cpu cortex-a72 -smp ${VM_SMP} -m ${VM_MEM_MB} \\
  -drive if=pflash,format=raw,file=efi_code.img,readonly=on \\
  -drive if=pflash,format=raw,file=varstore.img \\
  ${drives} \\
  -netdev user,id=net0,hostfwd=tcp:0.0.0.0:${SSH_PORT}-:22 \\
  -device virtio-net-pci,netdev=net0,romfile= \\
  -nographic -serial telnet:0.0.0.0:${CON_PORT},server,nowait -monitor none
EOF
    fi
}

cmd_up() {
    _write_boot "${1:-}"
    docker rm -f "${NAME}" >/dev/null 2>&1 || true
    # オーバーレイ運用時は backing file を /base（読み取り専用）で見せる。
    local base_mount=()
    if [[ -f "${WORKDIR}/.base_img_dir" ]]; then
        base_mount=(-v "$(cat "${WORKDIR}/.base_img_dir"):/base:ro")
    fi
    # shellcheck disable=SC2046  # _kvm_args は 0/1 個の引数を意図的に展開する
    docker run -d --name "${NAME}" $(_kvm_args) "${base_mount[@]}" \
        -p "${SSH_PORT}:${SSH_PORT}" -p "${CON_PORT}:${CON_PORT}" \
        -v "${WORKDIR}:/w" -w /w "${HELPER_IMG}" bash /w/boot.sh >/dev/null
    log "起動: console=telnet 127.0.0.1:${CON_PORT}, ssh=127.0.0.1:${SSH_PORT}"
    [[ "${ARCH}" == "aarch64" ]] && log "aarch64 は TCG のため multi-user 到達に数分〜数十分かかる"
    return 0
}

cmd_down() { docker rm -f "${NAME}" >/dev/null 2>&1 || true; log "removed ${NAME}"; }

cmd_status() {
    docker ps -a --filter "name=^/${NAME}$" --format '{{.Names}} {{.Status}}' || true
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
        log "single-user 経由で SSH 鍵注入（freebsd-provision.py --mode ssh）"
        python3 "${HERE}/freebsd-provision.py" --mode ssh --con-port "${CON_PORT}" --pubkey "${KEY}.pub"
    else
        # OpenBSD は autoinstall で鍵注入済み。install フェーズをここで実行する。
        log "OpenBSD autoinstall を実行（miniroot 起動 → 応答ファイル取得 → インストール）"
        cmd_up install
        python3 "${HERE}/openbsd-autoinstall.py" --con-port "${CON_PORT}" --workdir "${WORKDIR}" \
            --container "${NAME}" --arch "${ARCH}"
        log "autoinstall 完了。miniroot 無しで再起動する"
        cmd_down; cmd_up
    fi
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
cmd_scp() { scp "${SSH_OPTS[@]}" "$@"; }

# VM 内のリポジトリルート
GUEST_ROOT="/root/veil-proxy"

cmd_toolchain() {
    if [[ "${OS_NAME}" == "freebsd" ]]; then
        log "pkg install rust cmake llvm bash curl openssl nasm"
        cmd_ssh 'env IGNORE_OSVERSION=yes ASSUME_ALWAYS_YES=yes pkg install -y rust cmake llvm bash curl nasm git >/tmp/pkg.log 2>&1 || { tail -20 /tmp/pkg.log; exit 1; }'
    else
        log "pkg_add rust cmake llvm bash curl"
        cmd_ssh 'PKG_PATH=https://cdn.openbsd.org/pub/OpenBSD/$(uname -r)/packages/$(uname -m)/ pkg_add -I rust cmake llvm bash curl git >/tmp/pkg.log 2>&1 || { tail -20 /tmp/pkg.log; exit 1; }'
    fi
    cmd_ssh 'cargo --version; cmake --version | head -1'
}

cmd_sync() {
    log "リポジトリを VM へ転送（tar over ssh）"
    cmd_ssh "mkdir -p ${GUEST_ROOT}"
    # fuzz はワークスペースメンバだがゲストでは不要。転送後に members から外す。
    (cd "${ROOT}" && tar czf - \
        src benches tests examples contrib docker/assets \
        Cargo.toml Cargo.lock build.rs clippy.toml .cargo) \
      | cmd_ssh "cd ${GUEST_ROOT} && tar xzf - && sed -i'' -e 's|members = \[\".\", \"fuzz\"\]|members = [\".\"]|' Cargo.toml"
}

# aws-lc-sys の bindgen が libclang を要求するため、VM 内での LIBCLANG_PATH を解決する
_guest_env_prefix() {
    if [[ "${OS_NAME}" == "freebsd" ]]; then
        echo 'LIBCLANG_PATH=$(find /usr/local -name libclang.so\* 2>/dev/null | head -1 | xargs dirname)'
    else
        echo 'LIBCLANG_PATH=$(find /usr/local -name libclang.so\* 2>/dev/null | head -1 | xargs dirname)'
    fi
}

# BSD 向けの既定 feature セット（Cargo.toml）。
#   full-freebsd : full と同じ機能セット + アロケータを jemalloc + POSIX AIO(F-127) 有効
#   full-openbsd : full と同じ機能セット + システムアロケータ（mimalloc/jemalloc を使わない）
# どちらも `--no-default-features` と併用する（default features の mimalloc を外すため）。
_default_features() {
    case "${OS_NAME}" in
        freebsd) echo "full-freebsd" ;;
        openbsd) echo "full-openbsd" ;;
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

case "${COMMAND}" in
    setup) cmd_setup ;;
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
