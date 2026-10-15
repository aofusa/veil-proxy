#!/usr/bin/env bash
# Gateway API の conformance（F-225）を Istio の GatewayClass `istio` で回す。veil のモードでは、Istio が Gateway ごとに
# 作るゲートウェイの Deployment を GatewayClass の既定の ConfigMap（`gateway.istio.io/defaults-for-class`）で
# veil のイメージにする。
#
#   tools/k8s/mesh-gateway-conformance.sh <stock|veil> [extra conformance args...]
#
# 前提: kind のクラスタ（Istio と Gateway API の CRD）、Go（$GO、既定 ~/tools/go/bin/go）、gateway-api の
# リポジトリ（$GATEWAY_API_DIR、既定 ~/tools/gateway-api。CRD と同じ版のタグ）。結果は target/tmp/gateway-conformance-<mode>.{log,yaml}。
set -uo pipefail
export PATH="$HOME/.local/bin:$PATH"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
mkdir -p "$ROOT/target/tmp"
MODE="${1:-veil}"
shift || true
CLUSTER="${KIND_CLUSTER:-mesh}"
ISTIO_VERSION="${ISTIO_VERSION:-1.31.1}"
VEIL_PROXY="${VEIL_PROXY_IMAGE:-veil.local/istio-proxy:$ISTIO_VERSION}"
GO="${GO:-$HOME/tools/go/bin/go}"
GAPI="${GATEWAY_API_DIR:-$HOME/tools/gateway-api}"
OUT="$ROOT/target/tmp/gateway-conformance-$MODE"

kubectl config use-context "kind-$CLUSTER" >/dev/null
kubectl -n istio-system delete configmap veil-gatewayclass-defaults >/dev/null 2>&1
if [ "$MODE" = veil ]; then
  kind load docker-image --name "$CLUSTER" "$VEIL_PROXY" >/dev/null 2>&1
  cat <<EOF | kubectl apply -f - >/dev/null
apiVersion: v1
kind: ConfigMap
metadata:
  name: veil-gatewayclass-defaults
  namespace: istio-system
  labels: {gateway.istio.io/defaults-for-class: istio}
data:
  deployment: |
    spec:
      template:
        spec:
          containers:
          - name: istio-proxy
            image: $VEIL_PROXY
            imagePullPolicy: IfNotPresent
EOF
fi
cd "$GAPI" || exit 1
export GOPATH="$HOME/tools/gopath" GOCACHE="$HOME/tools/gocache" GOTMPDIR="$HOME/tools/gotmp" TMPDIR="$ROOT/target/tmp"
mkdir -p "$GOTMPDIR"
"$GO" test ./conformance -run TestConformance -timeout 60m -count=1 -args \
  --gateway-class=istio --allow-crds-mismatch --cleanup-base-resources=true \
  --conformance-profiles=GATEWAY-HTTP --skip-provisional-tests \
  --organization=veil --project=veil-mesh --url=https://example.invalid/veil --version=dev \
  --contact=none --report-output="$OUT.yaml" "$@" > "$OUT.log" 2>&1
rc=$?
kubectl -n istio-system delete configmap veil-gatewayclass-defaults >/dev/null 2>&1
grep -E "^(--- FAIL|--- PASS|FAIL|ok)" "$OUT.log" | grep -v "^--- PASS" | head -40
grep -c -- "--- PASS" "$OUT.log" | sed 's/^/passed subtests: /'
echo "exit=$rc (log: $OUT.log, report: $OUT.yaml)"
exit $rc
