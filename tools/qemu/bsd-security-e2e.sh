#!/usr/bin/env bash
# OS 固有のサンドボックス（FreeBSD capsicum / OpenBSD pledge+unveil / NetBSD chroot+特権降格）
# 下での E2E を VM 内で実行する（F-176 N7）。
#
# 使い方: tools/qemu/bsd-security-e2e.sh <freebsd|openbsd|netbsd> <x86_64|aarch64>
# 前提: VM が起動していて `bsd-vm.sh <os> <arch> build` 済み（release バイナリを使う）。
# 中身は `bsd-vm.sh <os> <arch> security-e2e`（VM 内の手順は bsd-security-e2e-guest.sh）。
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "${HERE}/bsd-vm.sh" "${1:?os}" "${2:?arch}" security-e2e
