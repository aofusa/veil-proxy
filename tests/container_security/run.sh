#!/usr/bin/env bash
# Veil コンテナ向けファジング・カオス・セキュリティスキャン（docker コマンドのみ）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/common.sh
source "${SCRIPT_DIR}/lib/common.sh"

TRIVY_SEVERITY="${TRIVY_SEVERITY:-HIGH,CRITICAL}"
SKIP_TRIVY="${SKIP_TRIVY:-0}"

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
    [[ -n "${user}" ]] || die "非 root ユーザーで実行されていません"
    echo "${seccomp}" | grep -q seccomp || die "seccomp プロファイルが適用されていません"
    log "イメージ実行時セキュリティ検証: ok"
}

run_trivy_scan() {
    if [[ "${SKIP_TRIVY}" == "1" ]]; then
        log "Trivy スキャンをスキップ (SKIP_TRIVY=1)"
        return 0
    fi

    local report="${RESULTS_DIR}/trivy_report.txt"
    mkdir -p "${RESULTS_DIR}"
    log "Trivy イメージスキャン: ${VEIL_IMAGE}"

    docker pull aquasec/trivy:latest >/dev/null 2>&1 || true
    if docker run --rm \
        --name "${TRIVY_CONTAINER}" \
        -v /var/run/docker.sock:/var/run/docker.sock:ro \
        aquasec/trivy:latest \
        image \
        --severity "${TRIVY_SEVERITY}" \
        --ignore-unfixed \
        --exit-code 0 \
        "${VEIL_IMAGE}" 2>&1 | tee "${report}"; then
        log "Trivy スキャン完了"
    else
        log "Trivy スキャンで警告あり（レポート参照）"
    fi
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

    # フェーズ 2: カオス負荷（並行）+ SIGHUP
    run_harness chaos chaos &
    local chaos_pid=$!
    sleep 3
    sighup_chaos || true
    wait "${chaos_pid}"

    # フェーズ 3: セキュリティスキャン（TLS・メソッド制限等）
    run_harness security security

    # フェーズ 4: コンテナイメージ脆弱性スキャン
    run_trivy_scan

    # 最終ヘルスチェック
    run_harness health health

    log "=== 全テスト完了 ==="
    log "結果: ${RESULTS_DIR}/"
}

main "$@"