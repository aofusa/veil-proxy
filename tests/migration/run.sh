#!/usr/bin/env bash
# F-255: 移行ガイドの代表的な例を、nginx・Envoy と veil に同じ要求を送って比べる（nightly）。
#
#   tests/migration/run.sh [veil-image]
#
# 各シナリオ（static・proxy・vhosts は nginx、edge は Envoy）で、同じ上流（traefik/whoami の app1・app2）と同じページ（www/）を
# 使い、requests の各行（メソッド・Host（- は無し）・パス）を両方へ送る。比べるもの:
#   - 状態コード、Location、X-Frame-Options
#   - 200 の応答: Content-Type（パラメータを除く）と本文。上流（whoami）の本文は、上流が受けた要求の行（メソッドとパス）と
#     X-Real-Ip の有無に揃えてから比べる（他の行はプロキシの実装で違う）
# 4xx・5xx の本文（エラーのページ）は比べない（nginx・Envoy・veil で文言が違う）。
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
M="$ROOT/tests/migration"
VEIL="${1:-ghcr.io/aofusa/veil:latest}"
NGINX="nginx:1.28.0-alpine@sha256:30f1c0d78e0ad60901648be663a710bdadf19e4c10ac6782c235200619158284"
ENVOY="envoyproxy/envoy:v1.36.2@sha256:4972515dd9a069b44beb43cebba7851596e72a8c61cd7a7c33d8f48efc5280ba"
WHOAMI="traefik/whoami:v1.11@sha256:200689790a0a0ea48ca45992e0450bc26ccab5307375b41c84dfc4f2475937ab"
NET="veil-migration-$$"
W="$(mktemp -d "${HOME}/veil-migration.XXXXXX")"
FAILS=0
cleanup() { docker rm -f $(docker ps -aq --filter "label=$NET") >/dev/null 2>&1; docker network rm "$NET" >/dev/null 2>&1; rm -rf "$W"; }
trap cleanup EXIT
docker network create "$NET" >/dev/null
run() { docker run -d --label "$NET" --network "$NET" "$@" >/dev/null; }
run --name "$NET-app1" --network-alias app1 "$WHOAMI" --port 9000
run --name "$NET-app2" --network-alias app2 "$WHOAMI" --port 9000
cp -R "$M/www" "$W/www"; chmod -R a+rX "$W"

fetch() { # <base-url> <method> <host> <path> <out-prefix>
    local h=(); [ "$3" != - ] && h=(-H "Host: $3")
    local d=(); [ "$2" = POST ] && d=(--data-binary '{"n":1}' -H 'Content-Type: application/json')
    local m=(-X "$2"); [ "$2" = HEAD ] && m=(-I)
    docker run --rm --network "$NET" curlimages/curl:8.17.0 -s -o /dev/stdout -D /dev/stderr "${m[@]}" "${h[@]}" "${d[@]}" "$1$4" \
        > "$5.body" 2> "$5.head"
}

compare() { # <label> <ref-prefix> <veil-prefix>
    python3 - "$1" "$2" "$3" <<'PY'
import hashlib, re, sys
label, a, b = sys.argv[1:]
def parse(p):
    head = open(p + ".head", "rb").read().decode("latin-1").split("\r\n")
    status = int(head[0].split()[1]) if head and head[0].startswith("HTTP") else 0
    hs = {}
    for l in head[1:]:
        if ":" in l:
            k, v = l.split(":", 1); hs[k.strip().lower()] = v.strip()
    body = open(p + ".body", "rb").read()
    return status, hs, body
def norm_body(hs, body):
    if b"\nRemoteAddr:" in body or body.startswith(b"Hostname:"):   # whoami
        t = body.decode("latin-1")
        line = next((l for l in t.split("\n") if re.match(r"^(GET|POST|HEAD|PUT|DELETE) ", l)), "")
        line = " ".join(line.split()[:2])
        real = "x-real-ip" if re.search(r"^X-Real-Ip:", t, re.M | re.I) else "-"
        return f"upstream saw: {line} ({real})"
    return "sha256:" + hashlib.sha256(body).hexdigest()[:16]
sa, ha, ba = parse(a); sb, hb, bb = parse(b)
diffs = []
if sa != sb: diffs.append(f"status {sa} != {sb}")
for k in ("location", "x-frame-options"):
    if ha.get(k) != hb.get(k): diffs.append(f"{k}: {ha.get(k)!r} != {hb.get(k)!r}")
if sa == 200 and sb == 200:
    ca = ha.get("content-type", "").split(";")[0]; cb = hb.get("content-type", "").split(";")[0]
    if ca != cb: diffs.append(f"content-type {ca!r} != {cb!r}")
    na, nb = norm_body(ha, ba), norm_body(hb, bb)
    if na != nb: diffs.append(f"body {na} != {nb}")
if diffs:
    print(f"FAIL {label}: " + "; ".join(diffs)); print(f"::error title=migration equivalence::{label}: " + "; ".join(diffs)); sys.exit(1)
print(f"ok   {label} ({sa})")
PY
}

for sc in static proxy vhosts edge; do
    d="$M/$sc"; ref="$NET-$sc-ref"; vl="$NET-$sc-veil"
    if [ -f "$d/envoy.yaml" ]; then
        run --name "$ref" --network-alias "$sc-ref" -v "$d/envoy.yaml:/etc/envoy/envoy.yaml:ro" "$ENVOY"
        kind=envoy
    else
        run --name "$ref" --network-alias "$sc-ref" -v "$d/nginx.conf:/etc/nginx/nginx.conf:ro" -v "$W/www:/var/www:ro" "$NGINX"
        kind=nginx
    fi
    run --name "$vl" --network-alias "$sc-veil" -v "$d/veil.toml:/etc/veil/conf.d/config.toml:ro" -v "$W/www:/var/www:ro" "$VEIL"
    sleep 4
    i=0
    while read -r method host path; do
        [ -z "$method" ] && continue
        i=$((i+1))
        fetch "http://$sc-ref:8080" "$method" "$host" "$path" "$W/$sc-$i-ref"
        fetch "http://$sc-veil:8080" "$method" "$host" "$path" "$W/$sc-$i-veil"
        compare "$sc ($kind vs veil): $method $host $path" "$W/$sc-$i-ref" "$W/$sc-$i-veil" || FAILS=$((FAILS+1))
    done < "$d/requests"
    docker rm -f "$ref" "$vl" >/dev/null
done
[ "$FAILS" = 0 ] && echo "MIGRATION EQUIVALENCE: ALL OK" || { echo "MIGRATION EQUIVALENCE: $FAILS difference(s)"; exit 1; }
