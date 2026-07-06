#!/usr/bin/env bash
# F-76: HTTP リクエストスマグリング能動テスト（CL.TE / TE.CL / 複数 CL）。
# ZAP baseline（受動）ではカバーされないプロキシ特有の高リスク領域を能動検査する。
# 稼働中の Veil コンテナへ crafted な生リクエストを TLS 経由で送り、曖昧フレーミングが
# 一貫して 400 で拒否される（＝バックエンドへ転送されずスマグリング不成立）ことを確認。
# docker のみで完結（python:3-slim + 標準ライブラリ ssl/socket）。
#
# 前提: run.sh から呼ばれ、Veil コンテナが ${NET_NAME} 上で稼働していること。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tools/container_security/results}"
PY_IMAGE="${SMUGGLING_PY_IMAGE:-python:3-slim}"
NET_NAME="${NET_NAME:-veil-sec-test-net}"
VEIL_CONTAINER="${VEIL_CONTAINER:-veil-sec-proxy}"
REPORT="${RESULTS_DIR}/smuggling_report.txt"

mkdir -p "${RESULTS_DIR}"

if [[ "${SKIP_SMUGGLING:-0}" == "1" ]]; then
    echo "smuggling: skipped (SKIP_SMUGGLING=1)" | tee "${REPORT}"
    exit 0
fi

ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' \
    "${VEIL_CONTAINER}" 2>/dev/null || true)
if [[ -z "${ip}" ]]; then
    echo "smuggling: skipped (Veil コンテナ未起動)" | tee "${REPORT}"
    exit 0
fi

: >"${REPORT}"
echo "smuggling active test start target=${ip}:443" | tee -a "${REPORT}"

# python プローブ: 各ベクタを生バイト列で送り、ステータス行を検査する。
# 期待: CL.TE / TE.CL / 複数 CL は 400。単独 chunked は非 400（誤検知しないこと）。
probe='
import socket, ssl, sys
host = sys.argv[1]; port = 443
ctx = ssl.create_default_context()
ctx.check_hostname = False
ctx.verify_mode = ssl.CERT_NONE

def send(raw):
    s = socket.create_connection((host, port), timeout=5)
    ss = ctx.wrap_socket(s, server_hostname="localhost")
    ss.sendall(raw)
    data = b""
    try:
        while True:
            chunk = ss.recv(4096)
            if not chunk: break
            data += chunk
            if b"\r\n\r\n" in data: break
    except Exception:
        pass
    ss.close()
    line = data.split(b"\r\n", 1)[0].decode("latin1", "replace")
    code = 0
    parts = line.split(" ")
    if len(parts) >= 2 and parts[1].isdigit():
        code = int(parts[1])
    return code, line

vectors = [
    ("CL>0 + TE:chunked", b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 6\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\nX", 400),
    ("CL:0 + TE:chunked", b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n", 400),
    ("dup Content-Length", b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\nhello", 400),
    ("TE not terminal chunked", b"POST / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked, gzip\r\n\r\n5\r\nhello\r\n0\r\n\r\n", 400),
]
fails = 0
for name, raw, want in vectors:
    try:
        code, line = send(raw)
    except Exception as e:
        print("PROBE-ERROR %-26s %s" % (name, e)); fails += 1; continue
    ok = (code == want)
    print("%-4s %-26s expect=%d got=%d (%s)" % ("PASS" if ok else "FAIL", name, want, code, line))
    if not ok: fails += 1

# 誤検知チェック: 単独 chunked は 400 にならないこと。
try:
    code, line = send(b"POST / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n0\r\n\r\n")
    ok = (code != 400)
    print("%-4s %-26s got=%d (%s)" % ("PASS" if ok else "FAIL", "legit chunked (non-400)", code, line))
    if not ok: fails += 1
except Exception as e:
    print("PROBE-ERROR legit chunked %s" % e); fails += 1

print("smuggling: %s (fails=%d)" % ("ok" if fails == 0 else "FAILURES", fails))
sys.exit(1 if fails else 0)
'

set +e
docker run --rm --network "${NET_NAME}" "${PY_IMAGE}" \
    python3 -c "${probe}" "${ip}" 2>&1 | tee -a "${REPORT}"
rc="${PIPESTATUS[0]}"
set -e

# 非ブロッキング（検出＝実装の退行なので警告としてレポートに残すが suite は継続）。
if [[ "${rc}" -ne 0 ]]; then
    echo "smuggling: WARNING 一部ベクタが期待通り拒否されず（退行の可能性・backlog 要確認）" | tee -a "${REPORT}"
    [[ "${SMUGGLING_BLOCKING:-0}" == "1" ]] && exit 1
fi

echo "smuggling 完了: ${REPORT}"
