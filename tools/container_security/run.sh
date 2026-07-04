#!/usr/bin/env bash
# Veil コンテナ向けファジング・カオス・セキュリティスキャン（docker コマンドのみ）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/common.sh
source "${SCRIPT_DIR}/lib/common.sh"

TRIVY_SEVERITY="${TRIVY_SEVERITY:-HIGH,CRITICAL}"
SKIP_TRIVY="${SKIP_TRIVY:-0}"
SKIP_H2SPEC="${SKIP_H2SPEC:-0}"
SKIP_TOXIPROXY="${SKIP_TOXIPROXY:-0}"
SKIP_LIBFUZZER="${SKIP_LIBFUZZER:-1}"
SKIP_CARGO_AUDIT="${SKIP_CARGO_AUDIT:-1}"
SKIP_CARGO_DENY="${SKIP_CARGO_DENY:-1}"
SKIP_TESTSSL="${SKIP_TESTSSL:-0}"
# 拡充フェーズ（F-64〜F-71）。重いもの・専用トポロジが要るものは既定 SKIP。
SKIP_SEMGREP="${SKIP_SEMGREP:-0}"
SKIP_SBOM="${SKIP_SBOM:-0}"
SKIP_ZAP="${SKIP_ZAP:-1}"
SKIP_BAD_BACKEND="${SKIP_BAD_BACKEND:-1}"
SKIP_RESOURCE_EXHAUSTION="${SKIP_RESOURCE_EXHAUSTION:-1}"
SKIP_PUMBA="${SKIP_PUMBA:-1}"
SKIP_LIBFUZZER_ASAN="${SKIP_LIBFUZZER_ASAN:-1}"
export SKIP_SEMGREP SKIP_SBOM SKIP_ZAP SKIP_BAD_BACKEND SKIP_RESOURCE_EXHAUSTION SKIP_PUMBA SKIP_LIBFUZZER_ASAN
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
    local i
    for ((i = 1; i <= 30; i++)); do
        if docker logs "${VEIL_CONTAINER}" 2>&1 | grep -q "Security restrictions applied"; then
            break
        fi
        sleep 1
        [[ "${i}" -eq 30 ]] && die "Veil のセキュリティ制限が適用されていません"
    done
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

bootstrap_chaos_infra() {
    if [[ "${SKIP_TOXIPROXY}" == "1" ]]; then
        return 0
    fi
    # shellcheck source=chaos/toxiproxy_setup.sh
    source "${SCRIPT_DIR}/chaos/toxiproxy_setup.sh"
    start_chaos_infra
}

poll_reload_ready() {
    local veil_host="$1"
    local metrics_path="${METRICS_PATH:-/__metrics}"
    docker run --rm --network "${NET_NAME}" --entrypoint curl "${HARNESS_IMAGE}" \
        -sk --max-time 3 "https://${veil_host}:443${metrics_path}" 2>/dev/null \
        | grep -q 'veil_' || return 1
    docker run --rm --network "${NET_NAME}" \
        -e "VEIL_HOST=${veil_host}" "${HARNESS_IMAGE}" health >/dev/null 2>&1
}

sighup_chaos() {
    log "SIGHUP カオス: 設定リロードシグナル送信"
    docker kill --signal=SIGHUP "${VEIL_CONTAINER}" >/dev/null
    local i veil_host
    for ((i = 1; i <= RELOAD_POLL_ATTEMPTS; i++)); do
        veil_host="$(resolve_veil_host)"
        if [[ -n "${veil_host}" ]] && poll_reload_ready "${veil_host}"; then
            log "SIGHUP 後メトリクス+ヘルス復帰 (${i}/${RELOAD_POLL_ATTEMPTS})"
            return 0
        fi
        sleep 1
    done
    log "警告: SIGHUP 後のメトリクス/ヘルス復帰を確認できませんでした"
    return 1
}

main() {
    trap cleanup EXIT
    require_docker
    mkdir -p "${RESULTS_DIR}"
    cleanup || true

    log "=== Veil コンテナセキュリティテスト開始 ==="
    log "イメージ: ${VEIL_IMAGE}"

    check_kernel_capabilities
    if should_skip_io_heavy_phases; then
        log "io_uring 必須環境で非対応のため chaos フェーズをスキップ (${KERNEL_SKIP_REASON})"
        export SKIP_CHAOS_LOAD=1
    fi
    build_harness_image
    start_network
    bootstrap_chaos_infra
    start_veil_container
    wait_veil_healthy 90

    inspect_container_security
    validate_veil_image_security

    # フェーズ 1: ブラックボックス HTTP ファジング
    run_harness fuzz fuzz

    # フェーズ 1b: libFuzzer（オプション、ビルド重い）
    if [[ "${SKIP_LIBFUZZER}" != "1" ]]; then
        "${SCRIPT_DIR}/fuzz/run_libfuzzer.sh" || log "libFuzzer で警告（レポート参照）"
    else
        log "libFuzzer をスキップ (SKIP_LIBFUZZER=1)"
    fi

    # フェーズ 1c: libFuzzer + ASAN（F-71、既定 SKIP。永続 corpus）
    "${SCRIPT_DIR}/fuzz/run_libfuzzer_asan.sh" || log "libFuzzer(ASAN) で警告（レポート参照）"

    # フェーズ 2: h2spec
    if [[ "${SKIP_H2SPEC}" != "1" ]]; then
        run_harness h2spec h2spec
    else
        log "h2spec をスキップ (SKIP_H2SPEC=1)"
    fi

    # フェーズ 3: カオス負荷 + SIGHUP（タイムアウト付き）
    if [[ "${SKIP_CHAOS_LOAD:-0}" != "1" ]]; then
        run_harness chaos chaos &
        local chaos_pid=$!
        sleep 3
        sighup_chaos || true
        wait_with_timeout "${chaos_pid}" "${CHAOS_TIMEOUT_SEC}" "chaos_load" || die "chaos フェーズ失敗"
    else
        log "chaos 負荷をスキップ (SKIP_CHAOS_LOAD=1)"
        echo "chaos: skipped (${KERNEL_SKIP_REASON:-manual})" >"${RESULTS_DIR}/chaos_report.txt"
    fi

    # フェーズ 3b: Toxiproxy 遅延注入・upstream 遮断
    if [[ "${SKIP_TOXIPROXY}" != "1" ]]; then
        run_harness toxiproxy toxiproxy || log "Toxiproxy カオスで警告（レポート参照）"
        run_harness circuit_breaker circuit_breaker || log "サーキットブレーカーカオスで警告（レポート参照）"
        run_harness slowloris slowloris || log "slowloris カオスで警告（レポート参照）"
    fi

    # フェーズ 3c: 拡充カオス（F-67 プロトコル違反 / F-68 リソース枯渇 / F-69 Pumba netem）
    # いずれも既定 SKIP。標準以外のトポロジ・特権が要るため個別トグルで明示有効化する。
    "${SCRIPT_DIR}/chaos/bad_backend_chaos.sh" || log "bad_backend カオスで警告（レポート参照）"
    "${SCRIPT_DIR}/chaos/pumba_chaos.sh" || log "pumba カオスで警告（レポート参照）"
    "${SCRIPT_DIR}/chaos/resource_exhaustion_chaos.sh" || log "resource_exhaustion カオスで警告（レポート参照）"

    # フェーズ 4: アプリセキュリティ（TLS・メソッド制限）
    export SKIP_TESTSSL=1
    run_harness security security

    # フェーズ 4a: testssl（Docker コンテナ）
    export SKIP_TESTSSL
    "${SCRIPT_DIR}/security/run_testssl.sh" || log "testssl で警告（レポート参照）"

    # フェーズ 4b: cargo-audit
    if [[ "${SKIP_CARGO_AUDIT}" != "1" ]]; then
        "${SCRIPT_DIR}/security/run_cargo_audit.sh" || log "cargo-audit で警告"
    else
        log "cargo-audit をスキップ (SKIP_CARGO_AUDIT=1)"
    fi

    # フェーズ 4c: cargo-deny
    if [[ "${SKIP_CARGO_DENY}" != "1" ]]; then
        "${SCRIPT_DIR}/security/run_cargo_deny.sh" || log "cargo-deny で警告"
    else
        log "cargo-deny をスキップ (SKIP_CARGO_DENY=1)"
    fi

    # フェーズ 4d: SAST（semgrep、F-64）
    "${SCRIPT_DIR}/security/run_semgrep.sh" || log "semgrep で警告（レポート参照）"

    # フェーズ 4e: SBOM 生成（syft、F-65）
    "${SCRIPT_DIR}/security/run_sbom.sh" || log "SBOM 生成で警告（レポート参照）"

    # フェーズ 4f: DAST（OWASP ZAP baseline、F-66、既定 SKIP）
    "${SCRIPT_DIR}/security/run_zap.sh" || log "ZAP で警告（レポート参照）"

    # フェーズ 5: Trivy イメージスキャン
    run_trivy_scan

    local i
    for ((i = 1; i <= 10; i++)); do
        if run_harness health health; then
            break
        fi
        sleep 2
        [[ "${i}" -eq 10 ]] && die "最終ヘルスチェック失敗"
    done

    # フェーズ 6: レポート集約
    # shellcheck source=lib/report.sh
    source "${SCRIPT_DIR}/lib/report.sh"
    aggregate_reports

    log "=== 全テスト完了 ==="
    log "結果: ${RESULTS_DIR}/"
}

main "$@"