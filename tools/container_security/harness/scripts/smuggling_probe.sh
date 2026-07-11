#!/usr/bin/env bash
# HTTP リクエストスマグリング能動プローブ（TLS 経由・openssl s_client 使用）。
# 旧 run_smuggling.sh の python プローブを Rust/シェル方針に沿って bash + openssl へ置換。
# 引数: $1 = Veil コンテナ IP（443/TLS）
# 期待: CL.TE / TE.CL / 複数 CL / 非終端 TE は 400。単独 chunked は非 400（誤検知しない）。
set -uo pipefail

HOST="${1:?usage: smuggling_probe.sh <veil-ip>}"
PORT="${SMUGGLING_TLS_PORT:-443}"

# 生バイト列（printf '%b' 用に \r\n をエスケープ）を TLS で送り、応答の
# ステータスコードのみを取り出す。応答がなければ 0。
send_tls() {
    local raw="$1" out code
    out=$(printf '%b' "${raw}" \
        | timeout 6 openssl s_client -quiet -connect "${HOST}:${PORT}" -servername localhost 2>/dev/null \
        | head -1)
    code=$(printf '%s' "${out}" | awk '{print $2}')
    if [[ "${code}" =~ ^[0-9]+$ ]]; then printf '%s\n' "${code}"; else printf '0\n'; fi
}

fails=0

# (name expected raw)
check() {
    local name="$1" want="$2" raw="$3" code
    code=$(send_tls "${raw}")
    if [[ "${code}" == "${want}" ]]; then
        printf 'PASS %-26s expect=%s got=%s\n' "${name}" "${want}" "${code}"
    else
        printf 'FAIL %-26s expect=%s got=%s\n' "${name}" "${want}" "${code}"
        fails=$((fails + 1))
    fi
}

check "CL>0 + TE:chunked" 400 \
    'POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 6\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\nX'
check "CL:0 + TE:chunked" 400 \
    'POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n'
check "dup Content-Length" 400 \
    'POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\nhello'
check "TE not terminal chunked" 400 \
    'POST / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked, gzip\r\n\r\n5\r\nhello\r\n0\r\n\r\n'

# 誤検知チェック: 単独 chunked は 400 にならないこと。
legit=$(send_tls 'POST / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n0\r\n\r\n')
if [[ "${legit}" != "400" ]]; then
    printf 'PASS %-26s got=%s\n' "legit chunked (non-400)" "${legit}"
else
    printf 'FAIL %-26s got=%s\n' "legit chunked (non-400)" "${legit}"
    fails=$((fails + 1))
fi

# ---------------------------------------------------------------------------
# F-109: HTTP/3 経由のスマグリング刺激（H3→H1 変換時の CL/TE ヘッダインジェクション）
# ---------------------------------------------------------------------------
VEIL_HTTP3_PORT="${VEIL_HTTP3_PORT:-${PORT}}"
VEIL_HOST="${VEIL_HOST:-${HOST}}"
run_h3_smuggle() {
    local mode="$1"
    local name="$2"
    if ! command -v http3-client >/dev/null 2>&1; then
        printf 'WARN %-26s http3-client missing (skip H3 smuggling)\n' "${name}"
        return 0
    fi
    export VEIL_HOST
    export VEIL_SNI="${VEIL_SNI:-${VEIL_HOST}}"
    export VEIL_HTTP3_PORT
    export HTTP3_MODE="${mode}"
    export HTTP3_PATH="${HTTP3_PATH:-/}"
    export HTTP3_REPORT="/results/http3_smuggling_${mode}_report.txt"
    set +e
    HTTP3_MODE="${mode}" http3-client
    local rc=$?
    set -e
    # 攻撃モードは crash 無しで 0。rc!=0 でも post-health で生存を確認するため WARN。
    if [[ "${rc}" -eq 0 ]]; then
        printf 'PASS %-26s http3-client mode=%s\n' "${name}" "${mode}"
    else
        printf 'WARN %-26s http3-client mode=%s rc=%s (health decides)\n' "${name}" "${mode}" "${rc}"
    fi
}

echo "F-109: HTTP/3 smuggling phase udp_port=${VEIL_HTTP3_PORT}"
run_h3_smuggle smuggling_cl_te "h3_smuggling_cl_te"
run_h3_smuggle smuggling_dup_cl "h3_smuggling_dup_cl"
run_h3_smuggle smuggling_te_inject "h3_smuggling_te_inject"

# H3 刺激後の生存確認（TLS HTTP/1.1）
post_h3=$(send_tls 'GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n')
if [[ "${post_h3}" == "200" ]] || [[ "${post_h3}" != "0" && "${post_h3}" != "" ]]; then
    printf 'PASS %-26s got=%s\n' "h3_smuggling_post_health" "${post_h3}"
else
    # openssl 経路でコードが取れない場合もある — curl があれば再試行
    if command -v curl >/dev/null 2>&1; then
        hc=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
            "https://${HOST}:${PORT}/" 2>/dev/null || echo "000")
        if [[ "${hc}" == "200" ]]; then
            printf 'PASS %-26s got=%s\n' "h3_smuggling_post_health" "${hc}"
        else
            printf 'FAIL %-26s got=%s\n' "h3_smuggling_post_health" "${hc}"
            fails=$((fails + 1))
        fi
    else
        printf 'WARN %-26s could not confirm health (got=%s)\n' "h3_smuggling_post_health" "${post_h3}"
    fi
fi

if [[ "${fails}" -eq 0 ]]; then
    echo "smuggling: ok (fails=0)"
    exit 0
fi
echo "smuggling: FAILURES (fails=${fails})"
exit 1
