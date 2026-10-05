#!/usr/bin/env bash
#
# aarch64 full-system QEMU VM 管理スクリプト（F-128）
# =====================================================
#
# 目的:
#   - **実 aarch64 カーネル**上で veil の io_uring バックエンドを E2E / tools/perf
#     検証するための環境を用意する。
#   - QEMU **user-mode**（qemu-aarch64）は io_uring の syscall エミュレーションが
#     不完全で io_uring ランタイムを起動できないため、**full-system emulation**
#     （qemu-system-aarch64、実カーネル）を使う。
#   - ホストに qemu-system-aarch64 / UEFI / cloud-image-utils が無く sudo も使えない
#     ため、`tools/qemu/helper` の Docker イメージ経由で QEMU を起動する。
#
# 注意:
#   - x86_64 ホスト上の aarch64 full-system は TCG（純ソフトウェアエミュレーション）
#     で動くため **非常に低速**（初回ブート/apt に数十分かかることがある）。ビルドは
#     ホスト側でクロスコンパイルし、**生成済みバイナリのみ VM へ転送して実行**する
#     運用を推奨（VM 内フルビルドは避ける）。
#   - VM はゲスト Ubuntu 24.04 arm64 cloud image。ユーザ veil / パスワード veil、
#     SSH 公開鍵認証（鍵は初回 setup で生成）。SSH はホスト 127.0.0.1:${SSH_PORT}。
#
# 使い方:
#   tools/qemu/aarch64-vm.sh setup     # ヘルパイメージ + cloud image + cloud-init 準備
#   tools/qemu/aarch64-vm.sh up        # VM を detached で起動
#   tools/qemu/aarch64-vm.sh wait      # SSH 到達までブロック（最大 ${WAIT_TIMEOUT}s）
#   tools/qemu/aarch64-vm.sh ssh 'cmd' # VM 内でコマンド実行
#   tools/qemu/aarch64-vm.sh scp SRC DST  # ホスト→VM ファイル転送
#   tools/qemu/aarch64-vm.sh status    # コンテナ状態 + SSH 到達性
#   tools/qemu/aarch64-vm.sh console   # シリアルコンソール末尾を表示
#   tools/qemu/aarch64-vm.sh down      # VM 停止・削除
#
# 環境変数（デフォルト）:
#   VEIL_QEMU_DIR   VM 資材の置き場（default: ~/qemu-images/aarch64）
#   SSH_PORT        ホスト側 SSH フォワードポート（default: 2222）
#   VM_SMP          vCPU 数（default: 4）
#   VM_MEM_MB       メモリ MB（default: 4096）
#   VM_DISK_SIZE    ディスクサイズ（default: 14G）
#   UBUNTU_IMG_URL  ベース cloud image URL
#   WAIT_TIMEOUT    wait のタイムアウト秒（default: 1800）
#   VEIL_QEMU_NATIVE 1 で Docker を使わずホストの qemu-system-aarch64 を直接起動する
#                   （docker コマンドが無ければ自動で 1 相当。0 を明示すると自動切替しない）
#
# native モード（Docker 不使用）:
#   `bsd-vm.sh` の native モードと同じ考え方。Apple Silicon macOS（Docker 未導入）では
#   **HVF**（`-machine virt,accel=hvf -cpu host`）で起動するため、x86_64 ホストの TCG と
#   違って Ubuntu cloud image が実用速度で動き、VM 内のネイティブビルドも現実的になる
#   （`linux-aarch64-e2e.sh` の `NATIVE_BUILD=1`）。それ以外のホストでは TCG
#   （`-cpu cortex-a72`）。前提: `brew install qemu cdrtools`（mkisofs）。
#   Docker モードの挙動は本モードの有無に関わらず変えていない。
set -euo pipefail

VEIL_QEMU_DIR="${VEIL_QEMU_DIR:-${HOME}/qemu-images/aarch64}"
SSH_PORT="${SSH_PORT:-2222}"
VM_SMP="${VM_SMP:-4}"
VM_MEM_MB="${VM_MEM_MB:-4096}"
VM_DISK_SIZE="${VM_DISK_SIZE:-14G}"
UBUNTU_IMG_URL="${UBUNTU_IMG_URL:-https://cloud-images.ubuntu.com/releases/noble/release/ubuntu-24.04-server-cloudimg-arm64.img}"
WAIT_TIMEOUT="${WAIT_TIMEOUT:-1800}"

HELPER_IMAGE="veil-qemu-aarch64:local"
CONTAINER="veil-arm-vm"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SSH_KEY="${VEIL_QEMU_DIR}/id_vm"

if [[ -n "${VEIL_QEMU_NATIVE:-}" ]]; then
    NATIVE="${VEIL_QEMU_NATIVE}"
elif ! command -v docker >/dev/null 2>&1; then
    NATIVE=1
else
    NATIVE=0
fi

log() { echo "[aarch64-vm] $*" >&2; }
die() { echo "[aarch64-vm] ERROR: $*" >&2; exit 1; }

ssh_opts=(-i "${SSH_KEY}" -p "${SSH_PORT}"
  -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
  -o ConnectTimeout=90 -o ServerAliveInterval=20 -o ServerAliveCountMax=6)

# qemu-img 等を helper コンテナ（Docker モード）またはホスト（native）で実行する
helper() {
    if [[ "${NATIVE}" == "1" ]]; then
        (cd "${VEIL_QEMU_DIR}" && "$@")
    else
        docker run --rm -v "${VEIL_QEMU_DIR}:/w" -w /w "${HELPER_IMAGE}" "$@"
    fi
}

# native: cloud-init シード（ISO9660, volid cidata）を作る
make_seed_native() {
    local src; src="$(mktemp -d)"
    cp "${VEIL_QEMU_DIR}/user-data" "${VEIL_QEMU_DIR}/meta-data" "${src}/"
    rm -f "${VEIL_QEMU_DIR}/seed.img"
    if command -v mkisofs >/dev/null 2>&1; then
        mkisofs -output "${VEIL_QEMU_DIR}/seed.img" -volid cidata -joliet -rock "${src}" >/dev/null 2>&1
    elif command -v genisoimage >/dev/null 2>&1; then
        genisoimage -output "${VEIL_QEMU_DIR}/seed.img" -volid cidata -joliet -rock "${src}" >/dev/null 2>&1
    else
        rm -rf "${src}"
        die "cidata ISO を作るツールが無い（mkisofs/genisoimage。macOS は 'brew install cdrtools'）"
    fi
    rm -rf "${src}"
}

# native: AAVMF/EDK2 の aarch64 UEFI コードを探す
find_efi_code() {
    local c
    for c in /opt/homebrew/share/qemu/edk2-aarch64-code.fd /usr/local/share/qemu/edk2-aarch64-code.fd \
             /usr/share/AAVMF/AAVMF_CODE.fd /usr/share/qemu/edk2-aarch64-code.fd; do
        [[ -f "${c}" ]] && { echo "${c}"; return 0; }
    done
    return 1
}

cmd_setup() {
    mkdir -p "${VEIL_QEMU_DIR}"

    if [[ "${NATIVE}" == "1" ]]; then
        command -v qemu-system-aarch64 >/dev/null || die "qemu-system-aarch64 が必要です（brew install qemu）"
    else
        command -v docker >/dev/null || die "docker が必要です"
        log "ヘルパ Docker イメージをビルド: ${HELPER_IMAGE}"
        docker build -t "${HELPER_IMAGE}" "${SCRIPT_DIR}/helper"
    fi

    if [[ ! -f "${VEIL_QEMU_DIR}/ubuntu-arm64.img" ]]; then
        log "Ubuntu 24.04 arm64 cloud image をダウンロード（初回のみ、~600MB）"
        curl -fSL -o "${VEIL_QEMU_DIR}/ubuntu-arm64.img" "${UBUNTU_IMG_URL}"
    fi

    log "VM ディスクを作成・リサイズ（${VM_DISK_SIZE}）"
    cp -f "${VEIL_QEMU_DIR}/ubuntu-arm64.img" "${VEIL_QEMU_DIR}/vm.img"
    helper qemu-img resize vm.img "${VM_DISK_SIZE}"

    if [[ ! -f "${SSH_KEY}" ]]; then
        log "SSH 鍵を生成: ${SSH_KEY}"
        ssh-keygen -t ed25519 -N '' -f "${SSH_KEY}" -q
    fi

    log "cloud-init seed を生成"
    local pub; pub="$(cat "${SSH_KEY}.pub")"
    cat > "${VEIL_QEMU_DIR}/user-data" <<EOF
#cloud-config
hostname: veilarm
users:
  - name: veil
    sudo: ALL=(ALL) NOPASSWD:ALL
    shell: /bin/bash
    lock_passwd: false
    ssh_authorized_keys:
      - ${pub}
chpasswd:
  list: |
    veil:veil
    root:root
  expire: false
ssh_pwauth: true
# TCG エミュレーションを軽くするため不要サービスを無効化（起動高速化）
bootcmd:
  - systemctl mask snapd.service snapd.seeded.service snapd.socket multipathd.service || true
EOF
    cat > "${VEIL_QEMU_DIR}/meta-data" <<EOF
instance-id: veilarm-001
local-hostname: veilarm
EOF
    if [[ "${NATIVE}" == "1" ]]; then
        make_seed_native
    else
        helper cloud-localds seed.img user-data meta-data
    fi
    rm -f "${VEIL_QEMU_DIR}/varstore.img" "${VEIL_QEMU_DIR}/efi_code.img"
    log "setup 完了"
}

cmd_up_native() {
    local efi accel cpu
    efi="$(find_efi_code)" || die "aarch64 UEFI ファーム（edk2-aarch64-code.fd）が見つからない（brew install qemu）"
    if [[ "$(uname -s)" == "Darwin" && "$(uname -m)" == "arm64" ]]; then
        accel="hvf,gic-version=3"; cpu="host"
    else
        accel="tcg"; cpu="cortex-a72"
    fi
    cmd_down_native
    (
        cd "${VEIL_QEMU_DIR}"
        if [[ ! -f varstore.img ]]; then
            dd if=/dev/zero of=varstore.img bs=1m count=64 2>/dev/null || dd if=/dev/zero of=varstore.img bs=1M count=64 2>/dev/null
            dd if=/dev/zero of=efi_code.img bs=1m count=64 2>/dev/null || dd if=/dev/zero of=efi_code.img bs=1M count=64 2>/dev/null
            dd if="${efi}" of=efi_code.img conv=notrunc 2>/dev/null
        fi
        nohup qemu-system-aarch64 \
            -machine "virt,accel=${accel}" -cpu "${cpu}" -smp "${VM_SMP}" -m "${VM_MEM_MB}" \
            -nographic \
            -drive if=pflash,format=raw,file=efi_code.img,readonly=on \
            -drive if=pflash,format=raw,file=varstore.img \
            -drive if=virtio,format=qcow2,file=vm.img \
            -drive if=virtio,format=raw,file=seed.img \
            -netdev "user,id=net0,hostfwd=tcp:127.0.0.1:${SSH_PORT}-:22" \
            -device virtio-net-pci,netdev=net0,romfile= \
            < /dev/null > console.log 2>&1 &
        echo $! > qemu.pid
        disown || true
    )
    log "起動しました（native, accel=${accel}）。'wait' で SSH 到達を待てます。"
}

cmd_down_native() {
    local pidf="${VEIL_QEMU_DIR}/qemu.pid"
    if [[ -f "${pidf}" ]] && kill -0 "$(cat "${pidf}")" 2>/dev/null; then
        cmd_ssh 'sudo poweroff' >/dev/null 2>&1 || true
        local i
        for i in $(seq 1 30); do kill -0 "$(cat "${pidf}")" 2>/dev/null || break; sleep 2; done
        kill -9 "$(cat "${pidf}")" 2>/dev/null || true
        log "VM を停止しました"
    fi
    rm -f "${pidf}"
}

cmd_up() {
    [[ -f "${VEIL_QEMU_DIR}/vm.img" ]] || die "先に 'setup' を実行してください"
    if [[ "${NATIVE}" == "1" ]]; then cmd_up_native; return; fi
    docker rm -f "${CONTAINER}" >/dev/null 2>&1 || true

    # UEFI ファーム: CODE（読み取り専用）+ VARS（書き込み可）を 64MiB flash として用意
    cat > "${VEIL_QEMU_DIR}/boot.sh" <<'BOOT'
#!/bin/bash
set -e
cd /w
if [ ! -f varstore.img ]; then
  truncate -s 64m varstore.img
  truncate -s 64m efi_code.img
  dd if=/usr/share/AAVMF/AAVMF_CODE.fd of=efi_code.img conv=notrunc 2>/dev/null
fi
exec qemu-system-aarch64 \
  -machine virt -cpu cortex-a72 -smp "${VM_SMP}" -m "${VM_MEM_MB}" \
  -nographic \
  -drive if=pflash,format=raw,file=efi_code.img,readonly=on \
  -drive if=pflash,format=raw,file=varstore.img \
  -drive if=virtio,format=qcow2,file=vm.img \
  -drive if=virtio,format=raw,file=seed.img \
  -netdev user,id=net0,hostfwd=tcp:0.0.0.0:2222-:22 \
  -device virtio-net-pci,netdev=net0,romfile=
BOOT

    log "VM を起動: コンテナ ${CONTAINER}（SSH → 127.0.0.1:${SSH_PORT}）"
    docker run -d --name "${CONTAINER}" -p "${SSH_PORT}:2222" \
        -e VM_SMP="${VM_SMP}" -e VM_MEM_MB="${VM_MEM_MB}" \
        -v "${VEIL_QEMU_DIR}:/w" -w /w \
        "${HELPER_IMAGE}" bash /w/boot.sh
    log "起動しました（TCG のため初回ブートは低速）。'wait' で SSH 到達を待てます。"
}

cmd_wait() {
    log "SSH 到達を待機（最大 ${WAIT_TIMEOUT}s）"
    local deadline=$(( $(date +%s) + WAIT_TIMEOUT ))
    while (( $(date +%s) < deadline )); do
        if ssh "${ssh_opts[@]}" veil@127.0.0.1 'true' 2>/dev/null; then
            log "SSH 到達 OK"; return 0
        fi
        sleep 15
    done
    die "SSH がタイムアウトしました（${WAIT_TIMEOUT}s）。'console' で状態を確認してください"
}

cmd_ssh() {
    ssh "${ssh_opts[@]}" veil@127.0.0.1 "$@"
}

cmd_scp() {
    [[ $# -eq 2 ]] || die "usage: scp SRC DST"
    scp -i "${SSH_KEY}" -P "${SSH_PORT}" \
        -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        "$1" "veil@127.0.0.1:$2"
}

cmd_status() {
    if [[ "${NATIVE}" == "1" ]]; then
        if [[ -f "${VEIL_QEMU_DIR}/qemu.pid" ]] && kill -0 "$(cat "${VEIL_QEMU_DIR}/qemu.pid")" 2>/dev/null; then
            echo "qemu: running (pid $(cat "${VEIL_QEMU_DIR}/qemu.pid"))"
        else
            echo "qemu: not running"
        fi
    else
        docker ps --filter "name=${CONTAINER}" --format 'container: {{.Status}}' || true
    fi
    if ssh "${ssh_opts[@]}" -o ConnectTimeout=10 veil@127.0.0.1 'uname -mr' 2>/dev/null; then
        echo "ssh: reachable"
    else
        echo "ssh: not reachable yet"
    fi
}

cmd_console() {
    if [[ "${NATIVE}" == "1" ]]; then tail -"${1:-30}" "${VEIL_QEMU_DIR}/console.log"; return; fi
    docker logs "${CONTAINER}" 2>&1 | tail -"${1:-30}"
}

cmd_down() {
    if [[ "${NATIVE}" == "1" ]]; then cmd_down_native; return; fi
    docker rm -f "${CONTAINER}" >/dev/null 2>&1 && log "VM を停止・削除しました" || log "VM は起動していません"
}

case "${1:-}" in
    setup)   cmd_setup ;;
    up)      cmd_up ;;
    wait)    cmd_wait ;;
    ssh)     shift; cmd_ssh "$@" ;;
    scp)     shift; cmd_scp "$@" ;;
    status)  cmd_status ;;
    console) shift; cmd_console "${1:-30}" ;;
    down)    cmd_down ;;
    *) grep -E '^#( |$)' "$0" | sed 's/^# \{0,1\}//'; exit 1 ;;
esac
