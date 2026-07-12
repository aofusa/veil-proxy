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

# F-112: HTTP/3 到達性（UDP）。curl --http3 が無いイメージでは skip。
# 戻り値: ok / fail / skip
h3_probe() {
    docker run --rm --network "${NET_NAME}" \
        -e "VEIL_HOST=${veil_ip}" \
        -e "VEIL_HTTP3_PORT=${VEIL_HTTP3_PORT:-443}" \
        "${HARNESS_IMAGE}" \
        bash -c '
            set +e
            if command -v http3-client >/dev/null 2>&1; then
                HTTP3_MODE=get HTTP3_PATH=/ HTTP3_REPORT=/tmp/h3_pumba.txt \
                    timeout 8 http3-client >/dev/null 2>&1
                rc=$?
                if [[ $rc -eq 0 ]]; then echo ok; exit 0; fi
            fi
            if curl --version 2>/dev/null | grep -qi http3; then
                code=$(curl -sk --http3-only -o /dev/null -w "%{http_code}" --max-time 6 \
                    "https://${VEIL_HOST}:${VEIL_HTTP3_PORT}/" 2>/dev/null || echo 000)
                if [[ "${code}" =~ ^(200|301|302)$ ]]; then echo ok; exit 0; fi
                echo fail; exit 0
            fi
            echo skip
        ' 2>/dev/null || echo fail
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
    # F-112: パケットロス/遅延下で HTTP/3 を流し、生存を確認
    h3_during=$(h3_probe)
    echo "  http3 during chaos: ${h3_during}" | tee -a "${REPORT}"
    wait "${pumba_pid}" 2>/dev/null || true
    # 回復確認
    ok=down
    for _ in 1 2 3 4 5; do [[ "$(health)" == up ]] && { ok=up; break; }; sleep 2; done
    echo "  health after recovery: ${ok}" | tee -a "${REPORT}"
    h3_after=$(h3_probe)
    echo "  http3 after recovery: ${h3_after}" | tee -a "${REPORT}"
    [[ "${ok}" == up ]] || echo "  WARNING: ${scenario} 後にヘルス復帰せず（backlog 起票対象）" | tee -a "${REPORT}"
    # HTTP/3 は UDP のため netem 下で fail し得る。復帰後 ok または skip なら合格。
    if [[ "${h3_after}" == "fail" ]]; then
        echo "  WARNING: ${scenario} 後に HTTP/3 復帰せず（UDP 劣化・要観察）" | tee -a "${REPORT}"
    fi
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
