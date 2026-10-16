#!/usr/bin/env bash
# Gateway API の conformance（F-225）を Istio の GatewayClass `istio` で回す。
#
#   tools/k8s/mesh-gateway-conformance.sh <stock|veil> [extra conformance args...]
#
# - kind には LoadBalancer が無いので、GatewayClass の既定の ConfigMap（`gateway.istio.io/defaults-for-class: istio`）で
#   ゲートウェイの Service を ClusterIP にし、試験のバイナリはクラスタの中の Pod（cluster-admin）で動かす。
# - veil のモードでは、同じ ConfigMap でゲートウェイの Deployment の istio-proxy を veil のイメージにする。
# 前提: kind のクラスタ（Istio と Gateway API の CRD）、Go（$GO、既定 ~/tools/go/bin/go）、gateway-api の
# リポジトリ（$GATEWAY_API_DIR、既定 ~/tools/gateway-api。CRD と同じ版のタグ）。結果は target/tmp/gateway-conformance-<mode>.{log,yaml}。
set -uo pipefail
export PATH="$HOME/.local/bin:$PATH"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
mkdir -p "$ROOT/target/tmp"
export TMPDIR="$ROOT/target/tmp"
MODE="${1:-veil}"
shift || true
CLUSTER="${KIND_CLUSTER:-mesh}"
ISTIO_VERSION="${ISTIO_VERSION:-1.31.1}"
VEIL_PROXY="${VEIL_PROXY_IMAGE:-veil.local/istio-proxy:$ISTIO_VERSION}"
GO="${GO:-$HOME/tools/go/bin/go}"
GAPI="${GATEWAY_API_DIR:-$HOME/tools/gateway-api}"
OUT="$ROOT/target/tmp/gateway-conformance-$MODE"
IMG=veil.local/gateway-conformance:latest
NS=gateway-conformance-runner

kubectl config use-context "kind-$CLUSTER" >/dev/null
# 試験のバイナリ（静的リンク）とイメージ
B="$ROOT/target/tmp/gwconf"
mkdir -p "$B"
if [ ! -x "$B/conformance.test" ]; then
  (cd "$GAPI" && CGO_ENABLED=0 GOPATH="$HOME/tools/gopath" GOCACHE="$HOME/tools/gocache" GOTMPDIR="$HOME/tools/gotmp" \
    "$GO" test -c -o "$B/conformance.test" ./conformance) || exit 1
fi
printf 'FROM alpine:3.20\nCOPY conformance.test /conformance.test\nENTRYPOINT ["/conformance.test"]\n' > "$B/Dockerfile"
docker build -q -t "$IMG" "$B" >/dev/null || exit 1
kind load docker-image --name "$CLUSTER" "$IMG" >/dev/null 2>&1

proxy_overlay=""
if [ "$MODE" = veil ]; then
  kind load docker-image --name "$CLUSTER" "$VEIL_PROXY" >/dev/null 2>&1
  proxy_overlay="
          containers:
          - name: istio-proxy
            image: $VEIL_PROXY
            imagePullPolicy: IfNotPresent"
fi
cat <<EOF | kubectl apply -f - >/dev/null
apiVersion: v1
kind: ConfigMap
metadata:
  name: veil-gatewayclass-defaults
  namespace: istio-system
  labels: {gateway.istio.io/defaults-for-class: istio}
data:
  service: |
    spec:
      type: ClusterIP
  deployment: |
    spec:
      template:
        spec:${proxy_overlay:- {\}}
EOF
# 前回の試験の名前空間が消えるのを待つ
kubectl wait --for=delete ns/gateway-conformance-infra ns/gateway-conformance-app-backend ns/gateway-conformance-web-backend --timeout=300s >/dev/null 2>&1
kubectl delete ns "$NS" --wait=true >/dev/null 2>&1
kubectl create ns "$NS" >/dev/null
kubectl -n "$NS" create serviceaccount runner >/dev/null
kubectl create clusterrolebinding gateway-conformance-runner --clusterrole=cluster-admin \
  --serviceaccount="$NS:runner" >/dev/null 2>&1
args="--gateway-class=istio --allow-crds-mismatch --cleanup-base-resources=true --conformance-profiles=GATEWAY-HTTP --skip-provisional-tests --organization=veil --project=veil-mesh --url=https://example.invalid/veil --version=$MODE --contact=none --report-output=/tmp/report.yaml $*"
cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: runner, annotations: {sidecar.istio.io/inject: "false"}}
spec:
  serviceAccountName: runner
  restartPolicy: Never
  containers:
  - name: runner
    image: $IMG
    imagePullPolicy: IfNotPresent
    command: ["sh", "-c", "/conformance.test -test.run TestConformance -test.timeout 90m -test.v $args; echo EXIT=\$?; cat /tmp/report.yaml 2>/dev/null | sed 's/^/REPORT /'; sleep 30"]
EOF
kubectl -n "$NS" wait --for=condition=Ready pod/runner --timeout=300s >/dev/null
kubectl -n "$NS" logs -f runner > "$OUT.log" 2>&1
sed -n 's/^REPORT //p' "$OUT.log" > "$OUT.yaml"
kubectl -n istio-system delete configmap veil-gatewayclass-defaults >/dev/null 2>&1
kubectl delete clusterrolebinding gateway-conformance-runner >/dev/null 2>&1
kubectl delete ns "$NS" --wait=false >/dev/null 2>&1
grep -E "^\s*--- FAIL" "$OUT.log" | head -40
echo "passed: $(grep -cE '^\s*--- PASS' "$OUT.log")  failed: $(grep -cE '^\s*--- FAIL' "$OUT.log")  skipped: $(grep -cE '^\s*--- SKIP' "$OUT.log")"
grep -E "^EXIT=" "$OUT.log"
