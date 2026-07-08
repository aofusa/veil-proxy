#!/usr/bin/env bash
# プロトコル差分（differential）プローブ。
# 旧 run_differential.sh の python プローブを bash + openssl / /dev/tcp へ置換。
# 引数: $1 = nginx IP（80/plain）, $2 = Veil IP（443/TLS）
# 同一リクエスト集合を両プロキシへ送りステータスを比較。Veil が厳格拒否する
# 既知のスマグリング差分は allowlist 扱い、それ以外の差分のみ発散として数える。
set -uo pipefail

NGINX_HOST="${1:?usage: differential_probe.sh <nginx-ip> <veil-ip>}"
VEIL_HOST="${2:?usage: differential_probe.sh <nginx-ip> <veil-ip>}"

# plain HTTP（nginx:80）へ生バイト列を送りステータスコードを取り出す。
send_plain() {
    local raw="$1" code
    exec 3<>"/dev/tcp/${NGINX_HOST}/80" || { echo 0; return; }
    printf '%b' "${raw}" >&3
    code=$(timeout 6 head -1 <&3 | awk '{print $2}')
    exec 3>&- 3<&- 2>/dev/null || true
    if [[ "${code}" =~ ^[0-9]+$ ]]; then printf '%s\n' "${code}"; else printf '0\n'; fi
}

# TLS（veil:443）へ生バイト列を送りステータスコードを取り出す。
send_tls() {
    local raw="$1" out code
    out=$(printf '%b' "${raw}" \
        | timeout 6 openssl s_client -quiet -connect "${VEIL_HOST}:443" -servername localhost 2>/dev/null \
        | head -1)
    code=$(printf '%s' "${out}" | awk '{print $2}')
    if [[ "${code}" =~ ^[0-9]+$ ]]; then printf '%s\n' "${code}"; else printf '0\n'; fi
}

diverge=0

# case: name allow(0/1) raw
run_case() {
    local name="$1" allow="$2" raw="$3" nc vc note=""
    nc=$(send_plain "${raw}")
    vc=$(send_tls "${raw}")
    if [[ "${nc}" != "${vc}" ]]; then
        if [[ "${allow}" == "1" ]]; then
            note=" (allowlisted: Veil strict smuggling rejection)"
        else
            diverge=$((diverge + 1))
            note=" <-- UNEXPECTED DIVERGENCE"
        fi
    fi
    printf '%-22s nginx=%s veil=%s%s\n' "${name}" "${nc}" "${vc}" "${note}"
}

run_case "normal GET" 0 \
    'GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n'
run_case "normal POST CL" 0 \
    'POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello'
run_case "chunked POST" 0 \
    'POST / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n0\r\n\r\n'
run_case "CL+TE (smuggling)" 1 \
    'POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n0\r\n\r\n'
run_case "dup CL (smuggling)" 1 \
    'POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nContent-Length: 6\r\nConnection: close\r\n\r\nhello'

if [[ "${diverge}" -eq 0 ]]; then
    echo "differential: ok (unexpected=0)"
    exit 0
fi
echo "differential: DIVERGENCES (unexpected=${diverge})"
exit 1
