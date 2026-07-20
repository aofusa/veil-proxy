#!/usr/bin/env bash
#
# FreeBSD 14.x arm64 (aarch64) full-system QEMU VM ヘルパ（v0.6.0 / aarch64 BSD 検証）
# ============================================================================
#
# 重要: **FreeBSD arm64 は Linux aarch64 と異なり TCG（KVM 不可ホスト）でも実用起動する**。
# よって aarch64 の「ビルド + 動作確認」を当環境の FreeBSD arm64 VM で実施できる
# （Linux aarch64 の full-system は TCG で氷河化/ソフトロックし不可。tools/qemu/README 参照）。
#
# aarch64-unknown-freebsd は Rust Tier3（prebuilt std 無し）でクロスビルドに build-std が
# 要るため、**VM 内でネイティブビルド**する（`pkg install rust cmake` → cargo build）。
#
# 前提: qemu-system-aarch64 + AAVMF(UEFI) を内包した helper イメージ（veil-qemu-aarch64:local、
#       tools/qemu/helper/Dockerfile）。FreeBSD arm64 VM-IMAGE の qcow2。
#
# 使い方（概略。詳細な provision は下のコメント参照）:
#   IMG=/path/to/FreeBSD-14.3-RELEASE-arm64-aarch64.qcow2 tools/qemu/fbsd-arm64-vm.sh up
#   tools/qemu/fbsd-arm64-vm.sh provision   # loader→single-user で SSH 鍵注入 + sshd 有効化
#   tools/qemu/fbsd-arm64-vm.sh grow        # (任意) qemu-img resize 後に single-user で growfs
#
# 落とし穴（project memory / VM 検証で確認済み）:
#   - virtio-net-pci には `romfile=`（空）を付けないと efi-virtio.rom 不足で起動失敗。
#   - シリアルに getty が無く root SSH も既定無効 → **loader メニューで "2"（single user）** を
#     送ると getty 不要の root シェルが出る。そこで `mount -u -o rw /` して
#     authorized_keys 注入 + `sysrc sshd_enable=YES` + `PermitRootLogin yes` → `exit`。
#   - qemu telnet シリアル(TCP)は IAC(0xff) を送るので pexpect は encoding="latin-1"。
#   - root FS が ~5G と小さい。`qemu-img resize +NN G` 後、**online growfs は不可**
#     （マウント中 root は not-clean 拒否）。**single-user で `mount -u -o ro /` →
#     `fsck -y /dev/gpt/rootfs` → `growfs -y /dev/gpt/rootfs`** で拡張する。
#   - pkg は `IGNORE_OSVERSION=yes ASSUME_ALWAYS_YES=yes` で userland 版不一致プロンプト回避。
#   - sshd は TCG で banner 応答が遅い → ssh ConnectTimeout=90 程度。
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IMG="${IMG:?set IMG=/path/to/FreeBSD-14.x-arm64.qcow2}"
WORKDIR="$(cd "$(dirname "${IMG}")" && pwd)"
SSH_PORT="${SSH_PORT:-2223}"
CON_PORT="${CON_PORT:-2224}"
HELPER_IMG="${HELPER_IMG:-veil-qemu-aarch64:local}"
NAME="${NAME:-veil-fbsd-arm64}"

cmd_up() {
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
    docker rm -f "${NAME}" 2>/dev/null || true
    docker run -d --name "${NAME}" -p "${SSH_PORT}:${SSH_PORT}" -p "${CON_PORT}:${CON_PORT}" \
        -v "${WORKDIR}:/w" -w /w "${HELPER_IMG}" bash /w/boot-fbsd-arm64.sh
    echo "started ${NAME}; console telnet 127.0.0.1:${CON_PORT}, ssh 127.0.0.1:${SSH_PORT}"
    echo "provision で SSH 鍵注入 (single-user)。provision スクリプトは project の"
    echo "qemu-images/fbsd-aarch64/provision_singleuser.py を参照。"
}
cmd_down() { docker rm -f "${NAME}" 2>/dev/null || true; echo "removed ${NAME}"; }

case "${1:-}" in
    up) cmd_up ;;
    down) cmd_down ;;
    *) echo "usage: IMG=... $0 {up|down}  (provision/grow は上のコメント手順を参照)"; exit 1 ;;
esac
