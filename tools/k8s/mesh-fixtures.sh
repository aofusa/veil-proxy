#!/usr/bin/env bash
# istiod が生成した xDS をフィクスチャとして保存する（F-222）。kind の Istio に代表的な設定（VirtualService・
# DestinationRule・ServiceEntry・PeerAuthentication・AuthorizationPolicy・RequestAuthentication・Telemetry）を入れ、
# veil のサイドカー（client = outbound、whoami = inbound）が受け取った Delta ADS の応答をそのまま保存する。
#
#   tools/k8s/mesh-fixtures.sh            → tests/fixtures/istio/<Istio の版>/{client,whoami}/NNNN-<型>.pb
#
# veil は環境変数 VEIL_MESH_XDS_RECORD のディレクトリへ応答を書く（SDS の応答＝秘密鍵は書かない）。保存したものは
# tests/mesh_istio_fixtures.rs が変換して、全部 ACK できること（NACK の理由が無いこと）と主な中身を確かめる。
# 前提: mesh-e2e.sh と同じ（kind のクラスタ・Istio・veil のイメージ）。
set -uo pipefail
export PATH="$HOME/.local/bin:$PATH"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
mkdir -p "$ROOT/target/tmp"
export TMPDIR="$ROOT/target/tmp"
CLUSTER="${KIND_CLUSTER:-mesh}"
ISTIO_VERSION="${ISTIO_VERSION:-1.31.1}"
VEIL_PROXY="${VEIL_PROXY_IMAGE:-veil.local/istio-proxy:$ISTIO_VERSION}"
NS=xds-fixtures
OUT="$ROOT/tests/fixtures/istio/$ISTIO_VERSION"

kubectl config use-context "kind-$CLUSTER" >/dev/null
kind load docker-image --name "$CLUSTER" "$VEIL_PROXY" >/dev/null 2>&1
kubectl delete ns "$NS" --wait=true >/dev/null 2>&1
kubectl create ns "$NS" >/dev/null
kubectl label ns "$NS" istio-injection=enabled >/dev/null

# 先に設定を入れてから Pod を起こす（最初の応答に全部が入る）
cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: v1
kind: Service
metadata: {name: fortio}
spec:
  selector: {app: fortio}
  ports: [{name: http, port: 8080}, {name: grpc, port: 8079}, {name: tcp, port: 8078}]
---
apiVersion: v1
kind: Service
metadata: {name: whoami}
spec:
  selector: {app: whoami}
  ports: [{name: http, port: 80}]
---
apiVersion: networking.istio.io/v1
kind: DestinationRule
metadata: {name: fortio}
spec:
  host: fortio
  trafficPolicy:
    loadBalancer: {simple: LEAST_REQUEST}
    connectionPool:
      tcp: {maxConnections: 100}
      http: {http1MaxPendingRequests: 50, maxRequestsPerConnection: 10}
    outlierDetection: {consecutive5xxErrors: 3, interval: 5s, baseEjectionTime: 30s, maxEjectionPercent: 50}
  subsets:
  - {name: v1, labels: {version: v1}}
  - {name: v2, labels: {version: v2}}
---
apiVersion: networking.istio.io/v1
kind: DestinationRule
metadata: {name: whoami}
spec:
  host: whoami
  trafficPolicy:
    loadBalancer:
      consistentHash: {httpHeaderName: x-user}
---
apiVersion: networking.istio.io/v1
kind: VirtualService
metadata: {name: fortio}
spec:
  hosts: [fortio]
  http:
  - match: [{headers: {x-canary: {exact: "1"}}}]
    route: [{destination: {host: fortio, subset: v2}}]
  - match: [{uri: {prefix: /fault}}]
    fault:
      delay: {percentage: {value: 50}, fixedDelay: 1s}
      abort: {percentage: {value: 10}, httpStatus: 503}
    route: [{destination: {host: fortio, subset: v1}}]
  - match: [{uri: {regex: "^/re/[0-9]+$"}}]
    rewrite: {uri: /echo}
    route: [{destination: {host: fortio, subset: v1}}]
  - route:
    - {destination: {host: fortio, subset: v1}, weight: 80}
    - {destination: {host: fortio, subset: v2}, weight: 20}
    timeout: 3s
    retries: {attempts: 3, perTryTimeout: 1s, retryOn: "5xx,reset,connect-failure"}
    mirror: {host: fortio, subset: v2}
    mirrorPercentage: {value: 25}
    headers:
      request: {set: {x-env: test}, remove: [x-debug]}
      response: {add: {x-served-by: veil}}
    corsPolicy:
      allowOrigins: [{exact: "https://a.example"}]
      allowMethods: [GET, POST]
      maxAge: 600s
---
apiVersion: networking.istio.io/v1
kind: ServiceEntry
metadata: {name: external}
spec:
  hosts: [api.example.com]
  location: MESH_EXTERNAL
  resolution: DNS
  ports: [{number: 443, name: tls, protocol: TLS}, {number: 80, name: http, protocol: HTTP}]
---
apiVersion: security.istio.io/v1
kind: PeerAuthentication
metadata: {name: whoami-strict}
spec:
  selector: {matchLabels: {app: whoami}}
  mtls: {mode: STRICT}
---
apiVersion: security.istio.io/v1
kind: AuthorizationPolicy
metadata: {name: whoami-allow}
spec:
  selector: {matchLabels: {app: whoami}}
  action: ALLOW
  rules:
  - from: [{source: {principals: ["cluster.local/ns/$NS/sa/client"]}}]
    to: [{operation: {methods: ["GET"], paths: ["/api/*"]}}]
    when: [{key: "request.headers[x-team]", values: ["blue"]}]
---
apiVersion: security.istio.io/v1
kind: AuthorizationPolicy
metadata: {name: whoami-deny}
spec:
  selector: {matchLabels: {app: whoami}}
  action: DENY
  rules:
  - to: [{operation: {paths: ["/admin*"]}}]
---
apiVersion: security.istio.io/v1
kind: RequestAuthentication
metadata: {name: whoami-jwt}
spec:
  selector: {matchLabels: {app: whoami}}
  jwtRules:
  - issuer: testing@secure.istio.io
    jwksUri: https://raw.githubusercontent.com/istio/istio/release-1.31/security/tools/jwt/samples/jwks.json
---
apiVersion: telemetry.istio.io/v1
kind: Telemetry
metadata: {name: mesh-default}
spec:
  accessLogging: [{providers: [{name: envoy}]}]
EOF

rec='proxy.istio.io/config: "{\"proxyMetadata\": {\"VEIL_MESH_XDS_RECORD\": \"/etc/istio/proxy/xds\"}}"'
cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: v1
kind: ServiceAccount
metadata: {name: client}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: fortio-v1}
spec:
  selector: {matchLabels: {app: fortio, version: v1}}
  template:
    metadata: {labels: {app: fortio, version: v1}}
    spec:
      containers:
      - {name: fortio, image: fortio/fortio:latest, imagePullPolicy: IfNotPresent, args: ["server"]}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: fortio-v2}
spec:
  selector: {matchLabels: {app: fortio, version: v2}}
  template:
    metadata: {labels: {app: fortio, version: v2}}
    spec:
      containers:
      - {name: fortio, image: fortio/fortio:latest, imagePullPolicy: IfNotPresent, args: ["server"]}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: whoami}
spec:
  selector: {matchLabels: {app: whoami}}
  template:
    metadata:
      labels: {app: whoami}
      annotations: {sidecar.istio.io/proxyImage: $VEIL_PROXY, $rec}
    spec:
      containers:
      - {name: whoami, image: traefik/whoami:latest, imagePullPolicy: IfNotPresent}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: client}
spec:
  selector: {matchLabels: {app: client}}
  template:
    metadata:
      labels: {app: client}
      annotations: {sidecar.istio.io/proxyImage: $VEIL_PROXY, $rec}
    spec:
      serviceAccountName: client
      containers:
      - {name: curl, image: curlimages/curl:latest, imagePullPolicy: IfNotPresent, command: ["sleep", "infinity"]}
EOF
kubectl -n "$NS" rollout status deploy --timeout=300s >/dev/null || { echo "workloads not ready"; exit 1; }
sleep 20
rm -rf "$OUT"
for app in client whoami; do
  mkdir -p "$OUT/$app"
  pod="$(kubectl -n "$NS" get pod -l app=$app -o name | head -1)"
  for f in $(kubectl -n "$NS" exec "$pod" -c istio-proxy -- ls /etc/istio/proxy/xds); do
    kubectl -n "$NS" exec "$pod" -c istio-proxy -- cat "/etc/istio/proxy/xds/$f" > "$OUT/$app/$f"
  done
  echo "$app: $(ls "$OUT/$app" | wc -l) responses ($(du -sh "$OUT/$app" | cut -f1))"
  nack="$(kubectl -n "$NS" exec "$pod" -c istio-proxy -- curl -s localhost:15000/config_dump 2>/dev/null | grep -c error_state)"
  echo "$app: error_state in config_dump: $nack"
done
kubectl delete ns "$NS" --wait=false >/dev/null 2>&1
