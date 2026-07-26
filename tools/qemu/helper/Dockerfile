# QEMU ヘルパイメージ（F-120/F-128、B-47 で x86_64 ゲスト対応を追加）
#
# ホストに qemu-system-* / UEFI ファーム / cloud-image-utils が無い（sudo 不可）
# 環境向けに、これらを内包した Docker イメージを提供する。
# tools/qemu/*.sh はこのイメージ経由で full-system QEMU を起動する。
#
# 収録するアーキテクチャ:
#   - aarch64 ゲスト（qemu-system-arm + AAVMF UEFI）: Linux arm64 / FreeBSD arm64 /
#     OpenBSD arm64。x86_64 ホストでは TCG。
#   - x86_64 ゲスト（qemu-system-x86 + OVMF UEFI）: FreeBSD amd64 / OpenBSD amd64。
#     ホストが x86_64 なら `--device /dev/kvm` で KVM 加速が効き、実用速度で
#     in-VM フルビルド + E2E が回る。
FROM debian:12-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
      qemu-system-arm qemu-efi-aarch64 \
      qemu-system-x86 ovmf \
      qemu-utils cloud-image-utils \
      openssh-client socat netcat-openbsd curl ca-certificates \
      xz-utils gzip tar python3 python3-pexpect \
    && rm -rf /var/lib/apt/lists/*
