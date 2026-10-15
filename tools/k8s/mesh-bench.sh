#!/usr/bin/env bash
# メッシュのデータプレーンの性能比較（F-225）。fortio のクライアント → サーバの経路に両側のサイドカーを挟み、
# 本家（stock = Envoy）と veil、メッシュ無し（none）を交互に測る（AGENTS.md の交互 A/B）。
#
#   tools/k8s/mesh-bench.sh [rounds] [duration]      例: tools/k8s/mesh-bench.sh 3 20s
#
# 測るもの（1 ラウンドにつき各モード）:
#   - HTTP/1.1 の最大スループット（-c 16 -qps 0）
#   - HTTP/1.1 の一定レート（1000 qps）での p50 / p90 / p99 / p99.9 の遅延
#   - gRPC（ping）の最大スループット
#   - サイドカー（クライアント側 + サーバ側）の CPU 時間（cgroup の usage_usec）と RSS
#   - GATEWAY=1（既定）: Gateway API のゲートウェイ（Envoy と veil。mode = gw-stock / gw-veil）の同じ項目
#     （gRPC 以外。CPU と RSS はゲートウェイの Pod）
# 結果は TSV（target/tmp/mesh-bench.tsv）。前提: mesh-e2e.sh と同じ（kind・istioctl・veil のイメージ）。
set -uo pipefail
export PATH="$HOME/.local/bin:$PATH"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
mkdir -p "$ROOT/target/tmp"
export TMPDIR="$ROOT/target/tmp"
ROUNDS="${1:-3}"
DUR="${2:-20s}"
CLUSTER="${KIND_CLUSTER:-mesh}"
ISTIO_VERSION="${ISTIO_VERSION:-1.31.1}"
VEIL_PROXY="${VEIL_PROXY_IMAGE:-veil.local/istio-proxy:$ISTIO_VERSION}"
OUT="$TMPDIR/mesh-bench.tsv"

kubectl config use-context "kind-$CLUSTER" >/dev/null
kind load docker-image --name "$CLUSTER" "$VEIL_PROXY" >/dev/null 2>&1

deploy() { # deploy <mode>
  local mode="$1" ns="bench-$1" ann inject
  case "$mode" in
    veil) ann="sidecar.istio.io/proxyImage: $VEIL_PROXY"; inject=enabled ;;
    stock) ann="sidecar.istio.io/inject: \"true\""; inject=enabled ;;
    none) ann="sidecar.istio.io/inject: \"false\""; inject=disabled ;;
  esac
  kubectl create ns "$ns" >/dev/null 2>&1
  kubectl label ns "$ns" istio-injection="$inject" --overwrite >/dev/null
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
      annotations: {$ann, proxy.istio.io/config: '{"concurrency": 2}'}
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
      annotations: {$ann, proxy.istio.io/config: '{"concurrency": 2}'}
    spec:
      containers:
      - name: fortio
        image: fortio/fortio:latest
        imagePullPolicy: IfNotPresent
        args: ["server", "-http-port", "18080", "-grpc-port", "disabled", "-redirect-port", "disabled"]
EOF
  kubectl -n "$ns" rollout status deploy/server deploy/client --timeout=300s >/dev/null
}

# サイドカーの CPU 時間（usec）と RSS（kB）。none は 0
sidecar_stats() { # sidecar_stats <ns> <deploy>
  local ns="$1" d="$2"
  kubectl -n "$ns" exec "deploy/$d" -c istio-proxy -- sh -c \
    'u=$(grep usage_usec /sys/fs/cgroup/cpu.stat | cut -d" " -f2); p=$(pgrep -f usr/local/bin/envoy | head -1); r=$(grep VmRSS /proc/$p/status | tr -s " " | cut -d" " -f2); echo "$u $r"' 2>/dev/null || echo "0 0"
}

run_mode() { # run_mode <mode> <round>
  local mode="$1" round="$2" ns="bench-$1" url grpc out qps p50 p90 p99 p999 gqps
  url="http://server.$ns:8080/echo?size=128"
  grpc="server.$ns:8079"
  read -r c0 _ <<<"$(sidecar_stats "$ns" client)"; read -r s0 _ <<<"$(sidecar_stats "$ns" server)"
  out="$(kubectl -n "$ns" exec deploy/client -c fortio -- fortio load -c 16 -qps 0 -t "$DUR" -loglevel Error "$url" 2>&1)"
  qps="$(echo "$out" | awk '/All done/{print $(NF-1)}')"
  read -r c1 _ <<<"$(sidecar_stats "$ns" client)"; read -r s1 _ <<<"$(sidecar_stats "$ns" server)"
  local reqs cpu_per
  reqs="$(echo "$out" | awk '/All done/{print $3}')"
  cpu_per="$(awk -v a=$((c1 - c0 + s1 - s0)) -v n="${reqs:-1}" 'BEGIN{ if (n>0) printf "%.1f", a/n; else print 0 }')"
  out="$(kubectl -n "$ns" exec deploy/client -c fortio -- fortio load -c 16 -qps 1000 -t "$DUR" -loglevel Error "$url" 2>&1)"
  p50="$(echo "$out" | awk '/# target 50%/{printf "%.3f", $4*1000}')"
  p90="$(echo "$out" | awk '/# target 90%/{printf "%.3f", $4*1000}')"
  p99="$(echo "$out" | awk '/# target 99%/{printf "%.3f", $4*1000}')"
  p999="$(echo "$out" | awk '/# target 99.9%/{printf "%.3f", $4*1000}')"
  out="$(kubectl -n "$ns" exec deploy/client -c fortio -- fortio load -grpc -ping -c 16 -qps 0 -t "$DUR" -loglevel Error "$grpc" 2>&1)"
  gqps="$(echo "$out" | awk '/All done/{print $(NF-1)}')"
  local crss srss
  read -r _ crss <<<"$(sidecar_stats "$ns" client)"; read -r _ srss <<<"$(sidecar_stats "$ns" server)"
  printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" "$round" "$mode" "$qps" "$cpu_per" "$p50" "$p90" "$p99" "$p999" "$gqps" "$crss" "$srss" | tee -a "$OUT"
}

# ゲートウェイ（F-224）: メッシュ無しの名前空間に Gateway API のゲートウェイを 2 つ（Envoy と veil）立て、
# 同じ fortio のサーバへ HTTPRoute で流す。クライアントはサイドカー無しの fortio
deploy_gw() {
  local ns=bench-none
  cat <<EOF | kubectl -n "$ns" apply -f - >/dev/null
apiVersion: v1
kind: ConfigMap
metadata: {name: gw-stock-opts}
data:
  service: |
    spec:
      type: ClusterIP
---
apiVersion: v1
kind: ConfigMap
metadata: {name: gw-veil-opts}
data:
  service: |
    spec:
      type: ClusterIP
  deployment: |
    spec:
      template:
        spec:
          containers:
          - name: istio-proxy
            image: $VEIL_PROXY
            imagePullPolicy: IfNotPresent
EOF
  local m
  for m in stock veil; do
    cat <<EOF | kubectl -n "$ns" apply -f - >/dev/null
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: gw-$m
  annotations: {proxy.istio.io/config: '{"concurrency": 2}'}
spec:
  gatewayClassName: istio
  infrastructure: {parametersRef: {group: "", kind: ConfigMap, name: gw-$m-opts}}
  listeners: [{name: http, port: 80, protocol: HTTP}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: gw-$m}
spec:
  parentRefs: [{name: gw-$m}]
  rules: [{backendRefs: [{name: server, port: 8080}]}]
EOF
  done
  kubectl -n "$ns" wait --for=condition=Programmed gateway/gw-stock gateway/gw-veil --timeout=300s >/dev/null
  kubectl -n "$ns" rollout status deploy/gw-stock-istio deploy/gw-veil-istio --timeout=300s >/dev/null
}

gw_stats() { # gw_stats <ns> <deploy>
  kubectl -n "$1" exec "deploy/$2" -c istio-proxy -- sh -c \
    'u=$(grep usage_usec /sys/fs/cgroup/cpu.stat | cut -d" " -f2); p=$(pgrep -f usr/local/bin/envoy | head -1); r=$(grep VmRSS /proc/$p/status | tr -s " " | cut -d" " -f2); echo "$u $r"' 2>/dev/null || echo "0 0"
}

run_gw() { # run_gw <stock|veil> <round>
  local m="$1" round="$2" ns=bench-none url out qps reqs cpu_per p50 p90 p99 p999 g0 g1 rss
  url="http://gw-$m-istio.$ns/echo?size=128"
  read -r g0 _ <<<"$(gw_stats "$ns" "gw-$m-istio")"
  out="$(kubectl -n "$ns" exec deploy/client -c fortio -- fortio load -c 16 -qps 0 -t "$DUR" -loglevel Error "$url" 2>&1)"
  qps="$(echo "$out" | awk '/All done/{print $(NF-1)}')"
  reqs="$(echo "$out" | awk '/All done/{print $3}')"
  read -r g1 _ <<<"$(gw_stats "$ns" "gw-$m-istio")"
  cpu_per="$(awk -v a=$((g1 - g0)) -v n="${reqs:-1}" 'BEGIN{ if (n>0) printf "%.1f", a/n; else print 0 }')"
  out="$(kubectl -n "$ns" exec deploy/client -c fortio -- fortio load -c 16 -qps 1000 -t "$DUR" -loglevel Error "$url" 2>&1)"
  p50="$(echo "$out" | awk '/# target 50%/{printf "%.3f", $4*1000}')"
  p90="$(echo "$out" | awk '/# target 90%/{printf "%.3f", $4*1000}')"
  p99="$(echo "$out" | awk '/# target 99%/{printf "%.3f", $4*1000}')"
  p999="$(echo "$out" | awk '/# target 99.9%/{printf "%.3f", $4*1000}')"
  read -r _ rss <<<"$(gw_stats "$ns" "gw-$m-istio")"
  printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" "$round" "gw-$m" "$qps" "$cpu_per" "$p50" "$p90" "$p99" "$p999" "-" "$rss" "-" | tee -a "$OUT"
}

printf "round\tmode\thttp_qps\tsidecar_cpu_us_per_req\tp50_ms@1k\tp90_ms@1k\tp99_ms@1k\tp999_ms@1k\tgrpc_qps\tclient_rss_kb\tserver_rss_kb\n" | tee "$OUT"
for m in none stock veil; do deploy "$m"; done
[ "${GATEWAY:-1}" = 1 ] && deploy_gw
sleep 10
for r in $(seq "$ROUNDS"); do
  # 交互（ラウンドごとに順序を入れ替える）
  if [ $((r % 2)) = 1 ]; then order="none stock veil"; else order="veil stock none"; fi
  for m in $order; do run_mode "$m" "$r"; done
  if [ "${GATEWAY:-1}" = 1 ]; then
    if [ $((r % 2)) = 1 ]; then gorder="stock veil"; else gorder="veil stock"; fi
    for m in $gorder; do run_gw "$m" "$r"; done
  fi
done
