#!/usr/bin/env bash
# F-256: examples/grafana の compose を立ち上げ、負荷を掛けてから、ダッシュボードの全パネルの式を Prometheus に問い合わせる。
# compose の設定で動かしている機能のパネルは結果が空でないこと（メトリクスの名前・ラベルを変えるとここで落ちる）。
#
#   tools/grafana/test-dashboard.sh [veil-image]
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
G="$ROOT/examples/grafana"
IMAGE="${1:-ghcr.io/aofusa/veil:latest}"
FAILS=0
ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; echo "::error title=grafana dashboard::$*"; FAILS=$((FAILS+1)); }
python3 "$ROOT/tools/grafana/gen_dashboard.py" --check || FAILS=$((FAILS+1))

PW="$(head -c 18 /dev/urandom | base64 | tr -dc 'A-Za-z0-9')"
export GRAFANA_ADMIN_PASSWORD="$PW" VEIL_IMAGE="$IMAGE"
cd "$G"
cleanup() { docker compose -p veil-grafana-test down -v >/dev/null 2>&1; }
trap cleanup EXIT
docker compose -p veil-grafana-test up -d >/dev/null || { echo "compose up failed"; exit 1; }
echo "waiting for traffic and scrapes (90s)"
sleep 90

# Prometheus の問い合わせ（compose の 127.0.0.1:9090）
python3 - "$G/veil-overview.json" <<'PY' || FAILS=$((FAILS+1))
import json, sys, urllib.parse, urllib.request
d = json.load(open(sys.argv[1]))
# compose の設定で動いている機能（結果が空でないことを求める）
must = {
    "Requests per second by status", "Requests per second by host", "5xx ratio", "Requests per second",
    "Response size (p50 / p95)", "Request duration p50 / p95 / p99", "Request duration p99 by host",
    "Active connections by host", "Upstream server health (1 = healthy)",
    "Circuit breaker state (0 closed, 1 open, 2 half-open)", "Connection pool hit ratio", "Connection pool size",
    "Cache hit ratio", "Cache stores and evictions per second", "Cache entries",
}
def panels(ps):
    for p in ps:
        if p.get("type") == "row":
            yield from panels(p.get("panels", []))
        else:
            yield p
fails = 0; seen = set()
for p in panels(d["panels"]):
    title = p["title"]; seen.add(title)
    n = 0
    for t in p["targets"]:
        e = t["expr"]
        for k, v in {"$__rate_interval": "1m", "$job": ".*", "$instance": ".*", "$host": ".*", "$upstream": ".*"}.items():
            e = e.replace(k, v)
        url = "http://127.0.0.1:9090/api/v1/query?" + urllib.parse.urlencode({"query": e})
        r = json.load(urllib.request.urlopen(url))
        if r.get("status") != "success":
            print(f"FAIL {title}: query error {r}"); fails += 1; continue
        n += len(r["data"]["result"])
    if n:
        print(f"ok   {title}: {n} series")
    elif title in must:
        print(f"FAIL {title}: no data"); print(f"::error title=grafana dashboard::{title}: no data"); fails += 1
    else:
        print(f"info {title}: no data (the feature is not used in the example)")
missing = must - seen
if missing:
    print("FAIL panels missing from the dashboard:", missing); fails += 1
sys.exit(1 if fails else 0)
PY

# Grafana がダッシュボードとデータソースを読み込んでいる（初回の起動は DB の移行で数分かかることがある）
for _ in $(seq 1 120); do curl -fsS -o /dev/null http://127.0.0.1:3000/api/health 2>/dev/null && break; sleep 2; done
if curl -fsS -u "admin:$PW" http://127.0.0.1:3000/api/dashboards/uid/veil-overview | grep -q '"title":"veil overview"'; then ok "Grafana: the dashboard is provisioned"
else bad "Grafana: dashboard not found"; fi
if curl -fsS -u "admin:$PW" http://127.0.0.1:3000/api/datasources/uid/prometheus/health | grep -q '"status":"OK"'; then ok "Grafana: the Prometheus data source is healthy"
else bad "Grafana: data source not healthy"; fi
# 匿名では見られない
code="$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:3000/api/dashboards/uid/veil-overview)"
[ "$code" = 401 ] && ok "Grafana: anonymous access is refused" || bad "Grafana: anonymous access returned $code"

[ "$FAILS" = 0 ] && echo "DASHBOARD TEST: ALL OK" || { echo "DASHBOARD TEST: $FAILS failure(s)"; exit 1; }
