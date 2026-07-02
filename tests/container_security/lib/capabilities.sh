#!/usr/bin/env bash
# ホスト/コンテナ環境のカーネル能力チェック（条件付きスキップ用）
set -euo pipefail

check_kernel_capabilities() {
    local report="${RESULTS_DIR}/kernel_capabilities.txt"
    mkdir -p "${RESULTS_DIR}"

    local iouring=0 ktls=0
    if [[ -r /proc/sys/kernel/io_uring_disabled ]]; then
        local disabled
        disabled=$(cat /proc/sys/kernel/io_uring_disabled 2>/dev/null || echo "1")
        [[ "${disabled}" == "0" ]] && iouring=1
    else
        # ファイルが無ければ利用可能とみなす（古いカーネル）
        iouring=1
    fi

    if lsmod 2>/dev/null | grep -q '^tls '; then
        ktls=1
    elif [[ -d /sys/module/tls ]]; then
        ktls=1
    fi

    {
        echo "io_uring_available=${iouring}"
        echo "ktls_module_loaded=${ktls}"
    } | tee "${report}"

    export KERNEL_IO_URING_AVAILABLE="${iouring}"
    export KERNEL_KTLS_AVAILABLE="${ktls}"

    if [[ "${iouring}" -eq 0 ]]; then
        log "警告: io_uring が制限されている環境です（テストは継続、一部機能はフォールバック）"
    fi
    if [[ "${ktls}" -eq 0 ]]; then
        log "情報: kTLS モジュール未ロード（rustls フォールバック想定）"
    fi
}