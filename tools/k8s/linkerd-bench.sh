#!/usr/bin/env bash
# Linkerd のデータプレーンの性能比較（F-225）。fortio のクライアント → サーバの経路に両側の linkerd-proxy を挟み、
# 本家（stock = linkerd2-proxy）と veil、メッシュ無し（none）を交互に測る（AGENTS.md の交互 A/B）。
#
#   tools/k8s/linkerd-bench.sh [rounds] [duration]      例: tools/k8s/linkerd-bench.sh 2 20s
#
# 測るもの（1 ラウンドにつき各モード）: HTTP/1.1 の最大スループット（-c 16 -qps 0）、1000 qps での遅延の分位、
# gRPC（ping）の最大スループット、プロキシ（クライアント側 + サーバ側）の CPU 時間と RSS。
# Linkerd のプロキシのイメージにはシェルが無いので、CPU と RSS は kubelet の stats/summary（コンテナ単位）から読む。
# 結果は TSV（target/tmp/linkerd-bench.tsv）。前提: linkerd-e2e.sh と同じ（kind・linkerd・veil のイメージ）。
set -uo pipefail
export PATH="$HOME/.local/bin:$PATH"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
mkdir -p "$ROOT/target/tmp"
export TMPDIR="$ROOT/target/tmp"
ROUNDS="${1:-2}"
DUR="${2:-20s}"
CLUSTER="${KIND_CLUSTER:-mesh}"
VEIL_IMAGE="${VEIL_LINKERD_IMAGE:-veil.local/linkerd-proxy}"
VEIL_VERSION="${VEIL_LINKERD_VERSION:-edge-26.10.1}"
OUT="$TMPDIR/linkerd-bench.tsv"

kubectl config use-context "kind-$CLUSTER" >/dev/null
kind load docker-image --name "$CLUSTER" "$VEIL_IMAGE:$VEIL_VERSION" >/dev/null 2>&1
NODE="$(kubectl get nodes -o jsonpath='{.items[0].metadata.name}')"

deploy() { # deploy <mode>
  local mode="$1" ns="l5d-bench-$1" ann
  case "$mode" in
    veil) ann="linkerd.io/inject: enabled, config.linkerd.io/proxy-image: $VEIL_IMAGE, config.linkerd.io/proxy-version: $VEIL_VERSION, config.linkerd.io/image-pull-policy: IfNotPresent" ;;
    stock) ann="linkerd.io/inject: enabled" ;;
    none) ann="linkerd.io/inject: disabled" ;;
  esac
  kubectl create ns "$ns" >/dev/null 2>&1
  cat <<EOF | kubectl -n "$ns" apply -f - >/dev/null
apiVersion: apps/v1
kind: Deployment
metadata: {name: server}
spec:
  replicas: 1
  selector: {matchLabels: {app: server}}
  template:
    metadata:
      labels: {app: server}
      annotations: {$ann, config.linkerd.io/proxy-cpu-limit: "2"}
    spec:
      containers:
      - name: fortio
        image: fortio/fortio:latest
        imagePullPolicy: IfNotPresent
        args: ["server", "-http-port", "8080", "-grpc-port", "8079", "-redirect-port", "disabled"]
---
apiVersion: v1
kind: Service
metadata: {name: server}
spec:
  selector: {app: server}
  ports: [{name: http, port: 8080}, {name: grpc, port: 8079}]
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: client}
spec:
  replicas: 1
  selector: {matchLabels: {app: client}}
  template:
    metadata:
      labels: {app: client}
      annotations: {$ann, config.linkerd.io/proxy-cpu-limit: "2"}
    spec:
      containers:
      - name: fortio
        image: fortio/fortio:latest
        imagePullPolicy: IfNotPresent
        args: ["server", "-http-port", "18080", "-grpc-port", "disabled", "-redirect-port", "disabled"]
EOF
  kubectl -n "$ns" rollout status deploy/server deploy/client --timeout=300s >/dev/null
}

# プロキシのコンテナの累積 CPU（ナノ秒）と RSS（kB）。none は 0
proxy_stats() { # proxy_stats <ns> <app>
  kubectl get --raw "/api/v1/nodes/$NODE/proxy/stats/summary" 2>/dev/null | python3 -c '
import json, sys
ns, app = sys.argv[1], sys.argv[2]
d = json.load(sys.stdin)
for p in d.get("pods", []):
    r = p["podRef"]
    if r["namespace"] != ns or not r["name"].startswith(app + "-"):
        continue
    for c in p.get("containers", []):
        if c["name"] == "linkerd-proxy":
            print(c["cpu"]["usageCoreNanoSeconds"], c["memory"].get("rssBytes", 0) // 1024)
            sys.exit(0)
print("0 0")' "$1" "$2"
}

run_mode() { # run_mode <mode> <round>
  local mode="$1" round="$2" ns="l5d-bench-$1" url grpc out qps p50 p90 p99 p999 gqps reqs cpu_per
  url="http://server.$ns:8080/echo?size=128"
  grpc="server.$ns:8079"
  read -r c0 _ <<<"$(proxy_stats "$ns" client)"; read -r s0 _ <<<"$(proxy_stats "$ns" server)"
  out="$(kubectl -n "$ns" exec deploy/client -c fortio -- fortio load -c 16 -qps 0 -t "$DUR" -loglevel Error "$url" 2>&1)"
  qps="$(echo "$out" | awk '/All done/{print $(NF-1)}')"
  reqs="$(echo "$out" | awk '/All done/{print $3}')"
  # stats/summary は 10〜15 秒ごとに更新されるので少し待つ
  sleep 15
  read -r c1 _ <<<"$(proxy_stats "$ns" client)"; read -r s1 _ <<<"$(proxy_stats "$ns" server)"
  cpu_per="$(awk -v a="$(( (c1 - c0 + s1 - s0) / 1000 ))" -v n="${reqs:-1}" 'BEGIN{ if (n>0 && a>0) printf "%.1f", a/n; else print 0 }')"
  out="$(kubectl -n "$ns" exec deploy/client -c fortio -- fortio load -c 16 -qps 1000 -t "$DUR" -loglevel Error "$url" 2>&1)"
  p50="$(echo "$out" | awk '/# target 50%/{printf "%.3f", $4*1000}')"
  p90="$(echo "$out" | awk '/# target 90%/{printf "%.3f", $4*1000}')"
  p99="$(echo "$out" | awk '/# target 99%/{printf "%.3f", $4*1000}')"
  p999="$(echo "$out" | awk '/# target 99.9%/{printf "%.3f", $4*1000}')"
  out="$(kubectl -n "$ns" exec deploy/client -c fortio -- fortio load -grpc -ping -c 16 -qps 0 -t "$DUR" -loglevel Error "$grpc" 2>&1)"
  gqps="$(echo "$out" | awk '/All done/{print $(NF-1)}')"
  local crss srss
  read -r _ crss <<<"$(proxy_stats "$ns" client)"; read -r _ srss <<<"$(proxy_stats "$ns" server)"
  printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" "$round" "$mode" "$qps" "$cpu_per" "$p50" "$p90" "$p99" "$p999" "$gqps" "$crss" "$srss" | tee -a "$OUT"
}

printf "round\tmode\thttp_qps\tproxy_cpu_us_per_req\tp50_ms@1k\tp90_ms@1k\tp99_ms@1k\tp999_ms@1k\tgrpc_qps\tclient_rss_kb\tserver_rss_kb\n" | tee "$OUT"
for m in none stock veil; do deploy "$m"; done
sleep 10
for r in $(seq "$ROUNDS"); do
  if [ $((r % 2)) = 1 ]; then order="none stock veil"; else order="veil stock none"; fi
  for m in $order; do run_mode "$m" "$r"; done
done

# 後片付け（ノードの CPU の要求を空ける）。KEEP=1 で残す
[ "${KEEP:-0}" = 1 ] || kubectl delete ns l5d-bench-none l5d-bench-stock l5d-bench-veil --wait=false >/dev/null 2>&1
