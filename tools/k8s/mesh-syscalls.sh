#!/usr/bin/env bash
# 透過転送の接続あたりの syscall（F-216）。kind の Istio で whoami のサイドカー（veil）に strace を付け、
# 新しい接続を N 本（Connection: close の要求）送って、syscall の種類ごとの回数を接続あたりにして出す。
# 透過転送で増えるのは SO_ORIGINAL_DST の getsockopt だけであることを確かめる。
#
#   tools/k8s/mesh-syscalls.sh [image] [connections]
#
# strace はエフェメラルコンテナ（kubectl debug --profile=sysadmin、alpine + strace）から istio-proxy の
# プロセスに付ける。前提: mesh-e2e.sh と同じ（kind・Istio・veil のイメージ）。
set -uo pipefail
export PATH="$HOME/.local/bin:$PATH"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
mkdir -p "$ROOT/target/tmp"
export TMPDIR="$ROOT/target/tmp"
CLUSTER="${KIND_CLUSTER:-mesh}"
IMAGE="${1:-veil.local/istio-proxy:1.31.1}"
N="${2:-200}"
NS=mesh-syscalls
STRACE=veil.local/strace:1   # latest のタグだとエフェメラルコンテナが pull しに行く

kubectl config use-context "kind-$CLUSTER" >/dev/null
if ! docker image inspect "$STRACE" >/dev/null 2>&1; then
  printf 'FROM alpine:3.20\nRUN apk add --no-cache strace\n' | docker build -q -t "$STRACE" - >/dev/null || exit 1
fi
kind load docker-image --name "$CLUSTER" "$STRACE" "$IMAGE" >/dev/null 2>&1
kubectl delete ns "$NS" --wait=true >/dev/null 2>&1
kubectl create ns "$NS" >/dev/null
kubectl label ns "$NS" istio-injection=enabled >/dev/null
cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: apps/v1
kind: Deployment
metadata: {name: whoami}
spec:
  selector: {matchLabels: {app: whoami}}
  template:
    metadata: {labels: {app: whoami}, annotations: {sidecar.istio.io/proxyImage: $IMAGE}}
    spec:
      containers: [{name: whoami, image: traefik/whoami:latest, imagePullPolicy: IfNotPresent}]
---
apiVersion: v1
kind: Service
metadata: {name: whoami}
spec:
  selector: {app: whoami}
  ports: [{name: http, port: 80}]
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: client}
spec:
  selector: {matchLabels: {app: client}}
  template:
    metadata: {labels: {app: client}, annotations: {sidecar.istio.io/inject: "false"}}
    spec:
      containers: [{name: curl, image: curlimages/curl:latest, imagePullPolicy: IfNotPresent, command: ["sleep", "infinity"]}]
EOF
kubectl -n "$NS" rollout status deploy --timeout=300s >/dev/null || { echo "not ready"; exit 1; }
# 平文で受ける（PERMISSIVE）。メッシュ外の client から inbound（15006 の virtualInbound）へ直接入る
pod="$(kubectl -n "$NS" get pod -l app=whoami -o jsonpath='{.items[0].metadata.name}')"
kubectl -n "$NS" exec deploy/client -- curl -s -o /dev/null http://whoami/ ; sleep 2
# 同じ長さで 2 回数える: 何も送らない回（アイドルの 1 秒ごとの accept のタイムアウト等）と、N 本の接続を送る回。
# 差を N で割ったものが接続あたり
kubectl -n "$NS" debug "$pod" --image="$STRACE" --target=istio-proxy --profile=sysadmin -c tracer -q -- \
  sh -c 'pid=$(pidof envoy); timeout -s INT 30 strace -f -c -p "$pid"; echo ===LOAD===; timeout -s INT 30 strace -f -c -p "$pid"; echo ===END===' >/dev/null
for _ in $(seq 90); do
  kubectl -n "$NS" logs "$pod" -c tracer 2>/dev/null | grep -q '===LOAD===' && break
  sleep 1
done
sleep 2
kubectl -n "$NS" exec deploy/client -- sh -c "for i in \$(seq $N); do curl -s -o /dev/null -H 'Connection: close' http://whoami/; done"
for _ in $(seq 60); do
  kubectl -n "$NS" logs "$pod" -c tracer 2>/dev/null | grep -q '===END===' && break
  sleep 1
done
kubectl -n "$NS" logs "$pod" -c tracer > "$TMPDIR/mesh-syscalls.txt" 2>&1
echo "image: $IMAGE, connections: $N (inbound, plaintext, whoami sidecar; loaded 30s minus idle 30s)"
awk -v n="$N" 'BEGIN { phase = 0 }
  /===LOAD===/ {phase = 1; next}
  /^-----/ {next}
  NF >= 5 && $NF != "total" && $NF != "syscall" && $4 ~ /^[0-9]+$/ { c[phase, $NF] += $4; names[$NF] = 1 }
  END {
    for (k in names) {
      d = c[1, k] - c[0, k]
      if (d / n >= 0.05 || d / n <= -0.05) printf "%-20s %8d - %8d idle  %6.2f / connection\n", k, c[1, k], c[0, k], d / n
    }
  }' "$TMPDIR/mesh-syscalls.txt" | sort -k6 -n -r
kubectl delete ns "$NS" --wait=false >/dev/null 2>&1
