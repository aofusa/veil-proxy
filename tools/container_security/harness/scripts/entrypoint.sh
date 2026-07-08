#!/usr/bin/env bash
set -euo pipefail

PHASE="${PHASE:-all}"

run_phase() {
    case "$1" in
        health)
            exec /scripts/health_check.sh
            ;;
        fuzz)
            exec /usr/local/bin/fuzz-http
            ;;
        chaos)
            exec /scripts/chaos_load.sh
            ;;
        security)
            exec /scripts/security_scan.sh
            ;;
        h2spec)
            exec /scripts/h2spec_run.sh
            ;;
        toxiproxy)
            exec /scripts/toxiproxy_chaos.sh
            ;;
        circuit_breaker)
            exec /scripts/circuit_breaker_chaos.sh
            ;;
        slowloris)
            exec /scripts/slowloris_chaos.sh
            ;;
        admin_security)
            exec /scripts/admin_security_probe.sh
            ;;
        compression_cache)
            exec /scripts/compression_cache_probe.sh
            ;;
        http3)
            exec /scripts/http3_probe.sh
            ;;
        l4_flood)
            exec /scripts/l4_flood_probe.sh
            ;;
        websocket)
            exec /scripts/websocket_probe.sh
            ;;
        grpc)
            exec /scripts/grpc_probe.sh
            ;;
        rate_limit)
            exec /scripts/rate_limit_probe.sh
            ;;
        wasm_security)
            exec /scripts/wasm_security_probe.sh
            ;;
        metrics)
            exec /scripts/metrics_probe.sh
            ;;
        otel)
            exec /scripts/otel_probe.sh
            ;;
        access_log)
            exec /scripts/access_log_probe.sh
            ;;
        grpc_web)
            exec /scripts/grpc_web_probe.sh
            ;;
        all)
            /scripts/health_check.sh
            /usr/local/bin/fuzz-http
            /scripts/h2spec_run.sh
            /scripts/chaos_load.sh
            /scripts/security_scan.sh
            ;;
        *)
            echo "不明なフェーズ: $1" >&2
            exit 1
            ;;
    esac
}

if [[ $# -gt 0 ]]; then
    run_phase "$1"
else
    run_phase "${PHASE}"
fi