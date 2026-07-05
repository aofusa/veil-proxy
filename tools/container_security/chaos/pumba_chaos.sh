#!/usr/bin/env bash
# Pumba によるカーネル/ネットワーク層カオス（netem: パケットロス・遅延・重複・破損）。
# Toxiproxy はアプリ寄りの L7 プロキシだが、Pumba は `tc netem` をコンテナの
# ネットワーク名前空間へ直接適用し、より低レイヤーの障害を再現する（F-53 拡充）。
#
# 稼働中の Veil コンテナに対し netem を時限適用し、適用中/回復後のヘルスを確認する。
# docker のみで完結（gaiaadm/pumba + docker.sock）。既定 SKIP。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tools/container_security/results}"
PUMBA_IMAGE="${PUMBA_IMAGE:-gaiaadm/pumba:latest}"
NET_NAME="${NET_NAME:-veil-sec-test-net}"
VEIL_CONTAINER="${VEIL_CONTAINER:-veil-sec-proxy}"
HARNESS_IMAGE="${HARNESS_IMAGE:-veil-sec-harness:local}"
DURATION="${PUMBA_DURATION:-20s}"
REPORT="${RESULTS_DIR}/pumba_chaos_report.txt"

mkdir -p "${RESULTS_DIR}"

if [[ "${SKIP_PUMBA:-1}" == "1" ]]; then
    echo "pumba: skipped (SKIP_PUMBA=1)" | tee "${REPORT}"
    exit 0
fi

docker inspect "${VEIL_CONTAINER}" >/dev/null 2>&1 || {
    echo "pumba: skipped (Veil コンテナ未起動)" | tee "${REPORT}"; exit 0; }

veil_ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "${VEIL_CONTAINER}")
: >"${REPORT}"
echo "pumba chaos start target=${VEIL_CONTAINER} duration=${DURATION}" | tee -a "${REPORT}"

health() { # 適用中/後のヘルス確認（応答があれば OK）
    docker run --rm --network "${NET_NAME}" -e "VEIL_HOST=${veil_ip}" \
        "${HARNESS_IMAGE}" health >/dev/null 2>&1 && echo up || echo down
}

# netem シナリオ: ロス10% → 遅延100ms±20ms → 重複5% → 破損2% → 順序逆転（F-69）。
for scenario in \
    "netem --duration ${DURATION} loss --percent 10" \
    "netem --duration ${DURATION} delay --time 100 --jitter 20" \
    "netem --duration ${DURATION} duplicate --percent 5" \
    "netem --duration ${DURATION} corrupt --percent 2" \
    "netem --duration ${DURATION} delay --time 40 reorder --percent 30 --correlation 50" ; do
    echo "== pumba ${scenario} ==" | tee -a "${REPORT}"
    docker run --rm \
        -v /var/run/docker.sock:/var/run/docker.sock \
        "${PUMBA_IMAGE}" \
        --log-level warning \
        ${scenario} "re2:^/${VEIL_CONTAINER}$" 2>&1 | tee -a "${REPORT}" &
    pumba_pid=$!
    sleep 3
    echo "  health during chaos: $(health)" | tee -a "${REPORT}"
    wait "${pumba_pid}" 2>/dev/null || true
    # 回復確認
    ok=down
    for _ in 1 2 3 4 5; do [[ "$(health)" == up ]] && { ok=up; break; }; sleep 2; done
    echo "  health after recovery: ${ok}" | tee -a "${REPORT}"
    [[ "${ok}" == up ]] || echo "  WARNING: ${scenario} 後にヘルス復帰せず（backlog 起票対象）" | tee -a "${REPORT}"
done

# 複合障害（F-69）: loss + delay を **同時** に適用する。pumba の netem サブコマンドは
# 1 回につき 1 種の impairment しか付けられない（tc qdisc は単一のため後勝ちで置換される）
# ため、対象コンテナのネットワーク名前空間へ直接 `tc netem delay ... loss ...` を適用する。
IPROUTE2_IMAGE="${IPROUTE2_IMAGE:-gaiadocker/iproute2:latest}"
COMPOUND_IFACE="${COMPOUND_IFACE:-eth0}"
if [[ "${SKIP_PUMBA_COMPOUND:-0}" != "1" ]]; then
    echo "== compound delay 80ms + loss 8% (simultaneous) ==" | tee -a "${REPORT}"
    if docker run --rm --net "container:${VEIL_CONTAINER}" --cap-add NET_ADMIN \
        "${IPROUTE2_IMAGE}" \
        tc qdisc add dev "${COMPOUND_IFACE}" root netem delay 80ms loss 8% \
        >>"${REPORT}" 2>&1; then
        sleep 3
        echo "  health during compound: $(health)" | tee -a "${REPORT}"
        # qdisc を除去して回復。
        docker run --rm --net "container:${VEIL_CONTAINER}" --cap-add NET_ADMIN \
            "${IPROUTE2_IMAGE}" \
            tc qdisc del dev "${COMPOUND_IFACE}" root >>"${REPORT}" 2>&1 || true
        ok=down
        for _ in 1 2 3 4 5; do [[ "$(health)" == up ]] && { ok=up; break; }; sleep 2; done
        echo "  health after recovery: ${ok}" | tee -a "${REPORT}"
        [[ "${ok}" == up ]] || echo "  WARNING: compound 後にヘルス復帰せず（backlog 起票対象）" | tee -a "${REPORT}"
    else
        # tc 適用に失敗（イメージ非対応・権限不足等）はスキップ扱い（suite を止めない）。
        echo "  compound: skipped (tc 適用不可: ${IPROUTE2_IMAGE} / NET_ADMIN)" | tee -a "${REPORT}"
    fi
fi

echo "pumba: ok" | tee -a "${REPORT}"
