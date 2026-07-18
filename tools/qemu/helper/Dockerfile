# qemu-system-aarch64 ヘルパイメージ（F-120/F-128）
#
# ホストに qemu-system-aarch64 / UEFI ファーム / cloud-image-utils が無い
# （sudo 不可）環境向けに、これらを内包した Docker イメージを提供する。
# aarch64-vm.sh はこのイメージ経由で full-system QEMU を起動する。
FROM debian:12-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
      qemu-system-arm qemu-efi-aarch64 qemu-utils cloud-image-utils \
      openssh-client socat netcat-openbsd curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*
