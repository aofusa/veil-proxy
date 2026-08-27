#!/usr/bin/env bash
# HTTP/1.1（TLS ポート経由）の allocs/req を測る。
# veil の平文リスナーは h2c 専用なので、HTTP/1.1 は必ず TLS ポートへ投げる（AGENTS.md / F-155）。
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CFG="${1:-h2c_proxy_alloc}"; IMG="${2:-veil:alloc-mainbase}"; P="${3:-/3b.html}"; N="${4:-20000}"
NET=veil-perf-lab; V=lab-veil; H2=local/h2load:latest
bash "$REPO/tools/perf/h2c_proxy_lab.sh" up "$CFG" "$IMG" || exit 1
# ウォームアップ
docker run --rm --network "$NET" --entrypoint h2load "$H2" \
    --h1 -n 3000 -c 100 "https://$V:443$P" >/dev/null 2>&1
scrape() {
    docker run --rm --network "$NET" curlimages/curl:latest -s \
        --http2-prior-knowledge "http://$V:8080/__metrics" 2>/dev/null \
        | awk '/^veil_alloc_/ {print $1"="$2}'
}
before="$(scrape)"; [ -z "$before" ] && { echo "!! metrics 取得不可" >&2; exit 1; }
out=$(docker run --rm --network "$NET" --entrypoint h2load "$H2" \
    --h1 -n "$N" -c 100 "https://$V:443$P" 2>&1)
done_req=$(awk '/^requests:/ {print $4}' <<<"$out")
after="$(scrape)"
echo "$out" | grep -E 'finished in|^requests:'
python3 - "$done_req" <<PY
import sys
req=float(sys.argv[1] or 1)
def p(s):
    d={}
    for l in s.strip().splitlines():
        k,_,v=l.partition('=')
        try: d[k]=float(v)
        except ValueError: pass
    return d
b,a=p('''$before'''),p('''$after''')
for k in sorted(a):
    if 'allocs_total' in k or 'bytes_total' in k:
        print(f"{k:34s} {(a[k]-b.get(k,0))/req:10.3f}/req")
PY
