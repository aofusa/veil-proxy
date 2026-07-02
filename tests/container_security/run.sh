#!/usr/bin/env bash
# Veil コンテナ向けファジング・カオス・セキュリティスキャン（docker コマンドのみ）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/common.sh
source "${SCRIPT_DIR}/lib/common.sh"

TRIVY_SEVERITY="${TRIVY_SEVERITY:-HIGH,CRITICAL}"
SKIP_TRIVY="${SKIP_TRIVY:-0}"
SKIP_H2SPEC="${SKIP_H2SPEC:-0}"
H2SPEC_FULL="${H2SPEC_FULL:-0}"
H2SPEC_STRICT="${H2SPEC_STRICT:-0}"

validate_veil_image_security() {
    local report="${RESULTS_DIR}/image_security_report.txt"
    mkdir -p "${RESULTS_DIR}"

    local user readonly_root seccomp
    user=$(docker inspect "${VEIL_CONTAINER}" --format '{{.Config.User}}')
    readonly_root=$(docker inspect "${VEIL_CONTAINER}" --format '{{.HostConfig.ReadonlyRootfs}}')
    seccomp=$(docker inspect "${VEIL_CONTAINER}" --format '{{json .HostConfig.SecurityOpt}}')

    {
        echo "user=${user}"
        echo "readonly_rootfs=${readonly_root}"
        echo "security_opt=${seccomp}"
    } | tee "${report}"

    [[ "${readonly_root}" == "true" ]] || die "ReadonlyRootfs が有効ではありません"
    echo "${seccomp}" | grep -q seccomp || die "seccomp プロファイルが適用されていません"
    # distroless は USER 未指定のため起動 UID は 0。特権降下は veil プロセス内で実施。
    docker logs "${VEIL_CONTAINER}" 2>&1 | grep -q "Security restrictions applied" \
        || die "Veil のセキュリティ制限が適用されていません"
    log "イメージ実行時セキュリティ検証: ok (readonly_rootfs, seccomp, privilege_drop)"
}

run_trivy_scan() {
    if [[ "${SKIP_TRIVY}" == "1" ]]; then
        log "Trivy スキャンをスキップ (SKIP_TRIVY=1)"
        return 0
    fi

    local report="${RESULTS_DIR}/trivy_report.txt"
    local image_tar="${RESULTS_DIR}/veil-image.tar"
    mkdir -p "${RESULTS_DIR}"
    log "Trivy イメージスキャン: ${VEIL_IMAGE}"

    docker pull aquasec/trivy:latest >/dev/null 2>&1 || true
    log "イメージを tar へエクスポート（docker.sock 非依存）"
    docker save "${VEIL_IMAGE}" -o "${image_tar}"

    if docker run --rm \
        --name "${TRIVY_CONTAINER}" \
        -v "${RESULTS_DIR}:/results:ro" \
        aquasec/trivy:latest \
        image \
        --input /results/veil-image.tar \
        --scanners vuln \
        --timeout 10m \
        --severity "${TRIVY_SEVERITY}" \
        --ignore-unfixed \
        --exit-code 0 \
        2>&1 | tee "${report}"; then
        log "Trivy スキャン完了"
    else
        log "Trivy スキャンで警告あり（レポート参照）"
    fi
    rm -f "${image_tar}"
}

sighup_chaos() {
    log "SIGHUP カオス: 設定リロードシグナル送信"
    docker kill --signal=SIGHUP "${VEIL_CONTAINER}" >/dev/null
    sleep 2
    run_harness health
}

main() {
    trap cleanup EXIT
    require_docker
    mkdir -p "${RESULTS_DIR}"
    cleanup || true

    log "=== Veil コンテナセキュリティテスト開始 ==="
    log "イメージ: ${VEIL_IMAGE}"

    build_harness_image
    start_network
    start_veil_container
    wait_veil_healthy 90

    inspect_container_security
    validate_veil_image_security

    # フェーズ 1: ファジング
    run_harness fuzz fuzz

    # フェーズ 2: h2spec（HTTP/2 準拠）
    if [[ "${SKIP_H2SPEC}" != "1" ]]; then
        run_harness h2spec h2spec
    else
        log "h2spec をスキップ (SKIP_H2SPEC=1)"
    fi

    # フェーズ 3: カオス負荷（並行）+ SIGHUP
    run_harness chaos chaos &
    local chaos_pid=$!
    sleep 3
    sighup_chaos || true
    wait "${chaos_pid}"

    # フェーズ 4: セキュリティスキャン（TLS・メソッド制限等）
    run_harness security security

    # フェーズ 5: コンテナイメージ脆弱性スキャン
    run_trivy_scan

    # 最終ヘルスチェック
    run_harness health health

    log "=== 全テスト完了 ==="
    log "結果: ${RESULTS_DIR}/"
}

main "$@"