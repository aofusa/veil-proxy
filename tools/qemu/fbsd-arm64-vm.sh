#!/usr/bin/env bash
#
# FreeBSD 14.x arm64 (aarch64) full-system QEMU VM ヘルパ（v0.6.0 / aarch64 BSD 検証）
# ============================================================================
#
# **重要**: FreeBSD arm64 は Linux aarch64 と異なり TCG（KVM 不可ホスト）でも実用起動する。
# よって aarch64 の「ビルド + 動作確認」を当環境の FreeBSD arm64 VM で実施できる
# （Linux aarch64 の full-system は TCG で氷河化/ソフトロックし不可。tools/qemu/README 参照）。
#
# aarch64-unknown-freebsd は Rust Tier3（prebuilt std 無し）でクロスビルドに build-std が
# 要るため、**VM 内でネイティブビルド**する（`pkg install rust cmake llvm` → cargo build）。
#
# 前提: qemu-system-aarch64 + AAVMF(UEFI) を内包した helper イメージ
#       （veil-qemu-aarch64:local、tools/qemu/helper/Dockerfile。`setup` で build する）。
#       SSH 鍵ペア（既定 ~/.ssh/veil_qemu_key）。python3 + pexpect（provision に使用）。
#
# 使い方（フル一巡）:
#   tools/qemu/fbsd-arm64-vm.sh setup       # helper build + イメージ DL + 鍵生成（初回のみ）
#   tools/qemu/fbsd-arm64-vm.sh up          # VM 起動（telnet console + hostfwd ssh）
#   tools/qemu/fbsd-arm64-vm.sh grow        # (推奨) ディスク +20G 拡張（rust+build に必要）
#   tools/qemu/fbsd-arm64-vm.sh provision   # single-user で SSH 鍵注入 + sshd 有効化
#   tools/qemu/fbsd-arm64-vm.sh smoke       # source 転送 + in-VM ネイティブビルド + HTTPS smoke
#   tools/qemu/fbsd-arm64-vm.sh ssh 'uname -a'
#   tools/qemu/fbsd-arm64-vm.sh down
#
# 落とし穴（project memory / VM 検証で確認済み）:
#   - virtio-net-pci には romfile=（空）が必須（efi-virtio.rom 不足で起動失敗するため）。
#   - シリアルに getty 無し + root SSH 既定無効 → loader メニューで single-user（"2"）から
#     provision（bsd-arm64-provision.py）。telnet console は IAC(0xff) があり pexpect は latin-1。
#   - root FS が ~5G と小さい。qemu-img resize 後の online growfs はマウント中 root で不可
#     → single-user で / を ro 再マウント → fsck → growfs（`grow` が実施）。
#   - pkg は IGNORE_OSVERSION=yes ASSUME_ALWAYS_YES=yes で userland 版不一致プロンプト回避。
#   - aws-lc-sys の bindgen が **libclang** を要求 → `pkg install llvm` + LIBCLANG_PATH。
#   - sshd は TCG で banner 応答が遅い → ssh ConnectTimeout=90 程度。
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${HERE}/../.." && pwd)"
WORKDIR="${WORKDIR:-${HOME}/qemu-images/fbsd-aarch64}"
IMG_XZ_URL="${IMG_XZ_URL:-https://download.freebsd.org/releases/VM-IMAGES/14.3-RELEASE/aarch64/Latest/FreeBSD-14.3-RELEASE-arm64-aarch64.qcow2.xz}"
IMG="${IMG:-${WORKDIR}/fbsd-arm64.qcow2}"
SSH_PORT="${SSH_PORT:-2223}"
CON_PORT="${CON_PORT:-2224}"
HELPER_IMG="${HELPER_IMG:-veil-qemu-aarch64:local}"
NAME="${NAME:-veil-fbsd-arm64}"
KEY="${KEY:-${HOME}/.ssh/veil_qemu_key}"
GROW_GB="${GROW_GB:-20}"
SSH_OPTS=(-i "${KEY}" -p "${SSH_PORT}" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=90 -o ServerAliveInterval=20)

log() { echo "[fbsd-arm64] $*" >&2; }
qemu() { docker run --rm -v "${WORKDIR}:/w" -w /w "${HELPER_IMG}" "$@"; }

cmd_setup() {
    mkdir -p "${WORKDIR}"
    log "helper イメージを build（${HELPER_IMG}）"
    docker build -t "${HELPER_IMG}" "${HERE}/helper"
    [[ -f "${KEY}" ]] || { log "SSH 鍵を生成: ${KEY}"; ssh-keygen -t ed25519 -N '' -f "${KEY}" >/dev/null; }
    if [[ ! -f "${IMG}" ]]; then
        log "FreeBSD arm64 VM-IMAGE を DL + 展開"
        curl -fsSL -o "${IMG}.xz" "${IMG_XZ_URL}"
        xz -dk -T0 "${IMG}.xz" && mv "${WORKDIR}/$(basename "${IMG_XZ_URL}" .xz)" "${IMG}" 2>/dev/null || true
        [[ -f "${IMG}" ]] || { xz -dc "${IMG}.xz" > "${IMG}"; }
    fi
    log "setup 完了。次: up → grow → provision → smoke"
}

_write_boot() {
    cat > "${WORKDIR}/boot-fbsd-arm64.sh" <<EOF
#!/bin/bash
set -e
cd /w
[ -f efi_code_fbsd.img ] || { truncate -s 64m efi_code_fbsd.img; dd if=/usr/share/AAVMF/AAVMF_CODE.fd of=efi_code_fbsd.img conv=notrunc 2>/dev/null; }
[ -f varstore_fbsd.img ] || truncate -s 64m varstore_fbsd.img
exec qemu-system-aarch64 -machine virt -cpu cortex-a72 -smp 4 -m 4096 \\
  -drive if=pflash,format=raw,file=efi_code_fbsd.img,readonly=on \\
  -drive if=pflash,format=raw,file=varstore_fbsd.img \\
  -drive if=virtio,format=qcow2,file=$(basename "${IMG}") \\
  -netdev user,id=net0,hostfwd=tcp:0.0.0.0:${SSH_PORT}-:22 \\
  -device virtio-net-pci,netdev=net0,romfile= \\
  -nographic -serial telnet:0.0.0.0:${CON_PORT},server,nowait -monitor none
EOF
}

cmd_up() {
    _write_boot
    docker rm -f "${NAME}" 2>/dev/null || true
    docker run -d --name "${NAME}" -p "${SSH_PORT}:${SSH_PORT}" -p "${CON_PORT}:${CON_PORT}" \
        -v "${WORKDIR}:/w" -w /w "${HELPER_IMG}" bash /w/boot-fbsd-arm64.sh >/dev/null
    log "起動: console=telnet 127.0.0.1:${CON_PORT}, ssh=127.0.0.1:${SSH_PORT}（TCG のため multi-user 到達に数分）"
}
cmd_down() { docker rm -f "${NAME}" 2>/dev/null || true; log "removed ${NAME}"; }

cmd_provision() {
    log "single-user で SSH 鍵注入（bsd-arm64-provision.py --mode ssh）"
    python3 "${HERE}/bsd-arm64-provision.py" --mode ssh --con-port "${CON_PORT}" --pubkey "${KEY}.pub"
    log "provision 完了。multi-user 起動を待って ssh 可能（数分）"
}

cmd_grow() {
    log "qcow2 を +${GROW_GB}G 拡張 → single-user で growfs"
    docker rm -f "${NAME}" 2>/dev/null || true; sleep 2
    qemu qemu-img resize "$(basename "${IMG}")" "+${GROW_GB}G"
    cmd_up; sleep 6
    # single-user へ入り gpart recover/resize してから growfs（provision.py は fsck+growfs）。
    python3 - "${CON_PORT}" <<'PY'
import socket,sys,time
port=int(sys.argv[1]); s=socket.socket(); s.settimeout(6); s.connect(("127.0.0.1",port)); time.sleep(1)
try:
  while True:
    if not s.recv(16384): break
except: pass
# ローダーで single-user
s.settimeout(4); t=time.time()+300
while time.time()<t:
  try:
    d=s.recv(16384)
    if d and (b"Boot Multi user" in d or b"Autoboot" in d): break
  except: pass
s.sendall(b"2"); time.sleep(30)
s.sendall(b"\r\n"); time.sleep(2)
s.sendall(b"gpart recover vtbd0 ; gpart resize -i 3 vtbd0\r\n"); time.sleep(10)
s.close()
PY
    python3 "${HERE}/bsd-arm64-provision.py" --mode grow --con-port "${CON_PORT}"
    log "grow 完了（reboot 後 df で確認）"
}

cmd_ssh() { ssh "${SSH_OPTS[@]}" root@127.0.0.1 "$@"; }

cmd_smoke() {
    log "source を tar-over-ssh 転送（workspace fuzz メンバ除外 + benches 同梱）"
    cmd_ssh 'mkdir -p /root/veil-proxy'
    (cd "${ROOT}" && tar czf - src Cargo.toml Cargo.lock build.rs benches) | cmd_ssh 'cd /root/veil-proxy && tar xzf - && sed -i "" -e "s|members = \[\".\", \"fuzz\"\]|members = [\".\"]|" Cargo.toml'
    log "pkg install rust cmake llvm（TCG のため時間を要す）"
    cmd_ssh 'env IGNORE_OSVERSION=yes ASSUME_ALWAYS_YES=yes pkg install -y rust cmake llvm >/tmp/pkg.log 2>&1 || tail -3 /tmp/pkg.log'
    log "in-VM ネイティブビルド（--no-default-features --features http2、TCG で数時間規模）"
    cmd_ssh 'cd /root/veil-proxy && LIBCLANG_PATH=$(find /usr/local -name libclang.so | head -1 | xargs dirname) cargo build --release --no-default-features --features http2'
    log "HTTPS 静的配信 smoke"
    cat "${HERE}/fbsd-arm64-smoke.sh" | cmd_ssh 'cat > /root/smoke.sh && sh /root/smoke.sh'
}

case "${1:-}" in
    setup) cmd_setup ;;
    up) cmd_up ;;
    grow) cmd_grow ;;
    provision) cmd_provision ;;
    ssh) shift; cmd_ssh "$@" ;;
    smoke) cmd_smoke ;;
    down) cmd_down ;;
    *) echo "usage: $0 {setup|up|grow|provision|smoke|ssh <cmd>|down}"; exit 1 ;;
esac
