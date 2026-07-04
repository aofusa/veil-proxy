#!/usr/bin/env bash
# ホスト/コンテナ環境のカーネル能力チェック（条件付きスキップ用）
set -euo pipefail

should_skip_io_heavy_phases() {
    [[ "${KERNEL_IO_URING_AVAILABLE:-1}" == "0" && "${KERNEL_REQUIRE_IO_URING:-0}" == "1" ]]
}

check_kernel_capabilities() {
    local report="${RESULTS_DIR}/kernel_capabilities.txt"
    mkdir -p "${RESULTS_DIR}"

    local iouring=0 ktls=0 skip_reason=""
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

    if [[ "${iouring}" -eq 0 ]]; then
        skip_reason="io_uring_disabled"
    fi

    {
        echo "io_uring_available=${iouring}"
        echo "ktls_module_loaded=${ktls}"
        echo "skip_reason=${skip_reason}"
    } | tee "${report}"

    export KERNEL_IO_URING_AVAILABLE="${iouring}"
    export KERNEL_KTLS_AVAILABLE="${ktls}"
    export KERNEL_SKIP_REASON="${skip_reason}"

    if [[ "${iouring}" -eq 0 ]]; then
        log "スキップ情報: io_uring 非対応 — 重い I/O フェーズは継続（KERNEL_REQUIRE_IO_URING=1 でスキップ）"
    fi
    if [[ "${ktls}" -eq 0 ]]; then
        log "情報: kTLS モジュール未ロード（rustls フォールバック想定）"
    fi

    # 非対応カーネルでもオーケストレータ自体は exit 0 で継続
    return 0
}