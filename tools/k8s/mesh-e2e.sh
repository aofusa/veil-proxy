#!/usr/bin/env bash
# メッシュのデータプレーンの E2E（F-225）。kind のクラスタに Istio を入れ、サイドカーのプロキシを
# 本家（stock = Envoy）・veil・混在（mixed）で動かして同じ台本を流す。
#
#   tools/k8s/mesh-e2e.sh <stock|veil|mixed> <smoke|tasks|all> [--keep]
#
# 使うイメージ: veil-istio-proxy:<istio の版>（docker/Dockerfile.mesh-istio。musl の veil を istio/proxyv2 に入れたもの）
# 試験のアプリ: fortio（HTTP 8080・gRPC 8079・TCP 8078）、traefik/whoami（HTTP、受け取った要求ヘッダを返す）、curl
# 前提: docker・kind・kubectl・istioctl（~/.local/bin でも可）。
set -uo pipefail
export PATH="$HOME/.local/bin:$PATH"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
mkdir -p "$ROOT/target/tmp"
export TMPDIR="$ROOT/target/tmp"   # snap 版 docker は /tmp を読めない
MODE="${1:-veil}"
SUITE="${2:-smoke}"
KEEP="${3:-}"
CLUSTER="${KIND_CLUSTER:-mesh}"
ISTIO_VERSION="${ISTIO_VERSION:-1.31.1}"
VEIL_PROXY="${VEIL_PROXY_IMAGE:-veil-istio-proxy:$ISTIO_VERSION}"
NS="mesh-$MODE"
FAIL=0
PASS=0

ok()   { echo "  ok   $*"; PASS=$((PASS+1)); }
bad()  { echo "  FAIL $*"; FAIL=1; }
log()  { echo "== $*"; }
retry() { # retry <seconds> <command...>
  local t="$1"; shift
  local end=$((SECONDS + t))
  while [ $SECONDS -lt $end ]; do "$@" >/dev/null 2>&1 && return 0; sleep 2; done
  return 1
}

ensure_cluster() {
  if ! kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
    log "kind クラスタ $CLUSTER を作成"
    kind create cluster --name "$CLUSTER" --wait 120s || exit 1
  fi
  kubectl config use-context "kind-$CLUSTER" >/dev/null
  if ! kubectl get ns istio-system >/dev/null 2>&1; then
    log "Istio $ISTIO_VERSION を導入"
    istioctl install --set profile=minimal -y >/dev/null || exit 1
  fi
  kubectl -n istio-system rollout status deploy/istiod --timeout=300s >/dev/null
  for img in fortio/fortio:latest traefik/whoami:latest curlimages/curl:latest; do
    docker image inspect "$img" >/dev/null 2>&1 || docker pull -q "$img" >/dev/null
  done
  local imgs="fortio/fortio:latest traefik/whoami:latest curlimages/curl:latest"
  [ "$MODE" != stock ] && imgs="$imgs $VEIL_PROXY"
  for img in $imgs; do
    kind load docker-image --name "$CLUSTER" "$img" >/dev/null 2>&1 || { echo "failed to load $img"; exit 1; }
  done
}

# proxy_annotation <role>: veil のプロキシにするワークロードだけ注釈を付ける
proxy_annotation() {
  case "$MODE" in
    veil) echo "sidecar.istio.io/proxyImage: $VEIL_PROXY" ;;
    mixed) [ "$1" = server ] && echo "sidecar.istio.io/proxyImage: $VEIL_PROXY" || echo "sidecar.istio.io/inject: \"true\"" ;;
    *) echo "sidecar.istio.io/inject: \"true\"" ;;
  esac
}

deploy() {
  kubectl create ns "$NS" >/dev/null 2>&1
  kubectl label ns "$NS" istio-injection=enabled --overwrite >/dev/null
  kubectl create ns "$NS-plain" >/dev/null 2>&1
  local server_ann client_ann client2_ann
  server_ann="$(proxy_annotation server)"
  client_ann="$(proxy_annotation client)"
  # mixed では client2 を veil、server2 を Envoy にして逆向きも試す
  if [ "$MODE" = mixed ]; then client2_ann="sidecar.istio.io/proxyImage: $VEIL_PROXY"; else client2_ann="$client_ann"; fi
  cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: v1
kind: ServiceAccount
metadata: {name: fortio}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: fortio}
spec:
  replicas: 2
  selector: {matchLabels: {app: fortio}}
  template:
    metadata:
      labels: {app: fortio, version: v1}
      annotations: {$server_ann}
    spec:
      serviceAccountName: fortio
      containers:
      - name: fortio
        image: fortio/fortio:latest
        imagePullPolicy: IfNotPresent
        args: ["server", "-http-port", "8080", "-grpc-port", "8079", "-tcp-port", "8078", "-redirect-port", "disabled"]
---
apiVersion: v1
kind: Service
metadata: {name: fortio}
spec:
  selector: {app: fortio}
  ports:
  - {name: http, port: 8080}
  - {name: grpc, port: 8079}
  - {name: tcp, port: 8078}
---
apiVersion: v1
kind: ServiceAccount
metadata: {name: whoami}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: whoami}
spec:
  replicas: 1
  selector: {matchLabels: {app: whoami}}
  template:
    metadata:
      labels: {app: whoami, version: v1}
      annotations: {$server_ann}
    spec:
      serviceAccountName: whoami
      containers:
      - name: whoami
        image: traefik/whoami:latest
        imagePullPolicy: IfNotPresent
        args: ["--port", "8080"]
---
apiVersion: v1
kind: Service
metadata: {name: whoami}
spec:
  selector: {app: whoami}
  ports: [{name: http, port: 80, targetPort: 8080}]
---
apiVersion: v1
kind: ServiceAccount
metadata: {name: client}
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
      annotations: {$client_ann}
    spec:
      serviceAccountName: client
      containers:
      - name: curl
        image: curlimages/curl:latest
        imagePullPolicy: IfNotPresent
        command: ["sleep", "infinity"]
      - name: fortio
        image: fortio/fortio:latest
        imagePullPolicy: IfNotPresent
        args: ["server", "-http-port", "18080", "-grpc-port", "disabled", "-redirect-port", "disabled"]
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: client2}
spec:
  replicas: 1
  selector: {matchLabels: {app: client2}}
  template:
    metadata:
      labels: {app: client2}
      annotations: {$client2_ann}
    spec:
      serviceAccountName: client
      containers:
      - name: curl
        image: curlimages/curl:latest
        imagePullPolicy: IfNotPresent
        command: ["sleep", "infinity"]
EOF
  # メッシュ外（サイドカー無し）のクライアントとサーバ
  cat <<EOF | kubectl -n "$NS-plain" apply -f - >/dev/null
apiVersion: apps/v1
kind: Deployment
metadata: {name: plain}
spec:
  replicas: 1
  selector: {matchLabels: {app: plain}}
  template:
    metadata: {labels: {app: plain}}
    spec:
      containers:
      - name: curl
        image: curlimages/curl:latest
        imagePullPolicy: IfNotPresent
        command: ["sleep", "infinity"]
      - name: whoami
        image: traefik/whoami:latest
        imagePullPolicy: IfNotPresent
        args: ["--port", "8080"]
---
apiVersion: v1
kind: Service
metadata: {name: plain}
spec:
  selector: {app: plain}
  ports: [{name: http, port: 80, targetPort: 8080}]
EOF
  kubectl -n "$NS" rollout status deploy/fortio deploy/whoami deploy/client deploy/client2 --timeout=300s >/dev/null || {
    kubectl -n "$NS" get pods; kubectl -n "$NS" describe pods | tail -40; return 1; }
  kubectl -n "$NS-plain" rollout status deploy/plain --timeout=300s >/dev/null || return 1
}

cexec() { kubectl -n "$NS" exec deploy/client -c curl -- "$@"; }
c2exec() { kubectl -n "$NS" exec deploy/client2 -c curl -- "$@"; }
fexec() { kubectl -n "$NS" exec deploy/client -c fortio -- "$@"; }

proxy_is_veil() { # proxy_is_veil <deploy>
  kubectl -n "$NS" exec "deploy/$1" -c istio-proxy -- /usr/local/bin/envoy --version 2>/dev/null | grep -q veil
}

smoke() {
  log "smoke ($MODE)"
  local want_server want_client
  case "$MODE" in veil) want_server=1; want_client=1 ;; mixed) want_server=1; want_client=0 ;; *) want_server=0; want_client=0 ;; esac
  if proxy_is_veil whoami; then [ $want_server = 1 ] && ok "server proxy is veil" || bad "server proxy should be Envoy"; else [ $want_server = 0 ] && ok "server proxy is Envoy" || bad "server proxy is not veil"; fi
  if proxy_is_veil client; then [ $want_client = 1 ] && ok "client proxy is veil" || bad "client proxy should be Envoy"; else [ $want_client = 0 ] && ok "client proxy is Envoy" || bad "client proxy is not veil"; fi

  # HTTP/1.1 と mTLS（受け側が付ける XFCC に双方の SPIFFE ID）
  local out
  out="$(cexec curl -s -m 10 http://whoami."$NS"/)" || true
  if echo "$out" | grep -q "^Hostname: whoami"; then ok "HTTP/1.1 client -> whoami"; else bad "HTTP/1.1 client -> whoami: $out"; fi
  if echo "$out" | grep -qi "X-Forwarded-Client-Cert: By=spiffe://cluster.local/ns/$NS/sa/whoami;Hash=[0-9a-f]\{64\};Subject=\"\";URI=spiffe://cluster.local/ns/$NS/sa/client"; then
    ok "mTLS (XFCC carries both SPIFFE IDs)"; else bad "mTLS evidence missing: $(echo "$out" | grep -i forwarded-client-cert)"; fi
  if echo "$out" | grep -q "RemoteAddr: 127.0.0.6:"; then ok "inbound connects to the app from 127.0.0.6"; else bad "inbound source address: $(echo "$out" | grep RemoteAddr)"; fi
  if echo "$out" | grep -qi "X-Request-Id: "; then ok "x-request-id generated"; else bad "x-request-id missing"; fi
  # 逆向き（mixed では client2 = veil → whoami）
  out="$(c2exec curl -s -m 10 http://whoami."$NS"/)" || true
  if echo "$out" | grep -q "^Hostname: whoami"; then ok "HTTP/1.1 client2 -> whoami"; else bad "client2 -> whoami: $out"; fi
  # h2c（prior knowledge）
  out="$(cexec curl -s -m 10 --http2-prior-knowledge -o /dev/null -w '%{http_code} %{http_version}' http://fortio."$NS":8080/echo)" || true
  if [ "$out" = "200 2" ]; then ok "h2c client -> fortio"; else bad "h2c: $out"; fi
  # POST の本文（Content-Length・chunked）
  out="$(cexec sh -c "head -c 300000 /dev/zero | tr '\\0' a | curl -s -m 10 --data-binary @- http://fortio.$NS:8080/echo | wc -c")" || true
  if [ "${out// /}" = "300000" ]; then ok "POST 300KB body echoed"; else bad "POST body: $out"; fi
  out="$(cexec sh -c "printf 'hello' | curl -s -m 10 -H 'Transfer-Encoding: chunked' --data-binary @- http://fortio.$NS:8080/echo")" || true
  if [ "$out" = "hello" ]; then ok "chunked request body"; else bad "chunked: $out"; fi
  # キープアライブで 20 回
  out="$(cexec sh -c "for i in \$(seq 20); do echo http://fortio.$NS:8080/echo; done | xargs curl -s -m 20 -o /dev/null -w '%{http_code}\n' | sort | uniq -c")" || true
  if echo "$out" | grep -q "20 200"; then ok "20 keep-alive requests"; else bad "keep-alive: $out"; fi
  # gRPC（fortio の ping）
  out="$(fexec fortio load -grpc -ping -n 50 -c 5 -qps 0 fortio."$NS":8079 2>&1 | grep -E 'SERVING|All done|Ended after' | tail -2)" || true
  if fexec fortio load -grpc -ping -n 50 -c 5 -qps 0 fortio."$NS":8079 2>&1 | grep -q "SERVING : 50"; then ok "gRPC ping x50"; else bad "gRPC: $out"; fi
  # TCP（fortio の echo）
  out="$(fexec fortio load -n 20 -c 2 -qps 0 tcp://fortio."$NS":8078 2>&1 | grep -E 'tcp OK|Total Bytes|Error' | tr '\n' ' ')" || true
  if echo "$out" | grep -q "tcp OK : 20"; then ok "TCP echo x20"; else bad "TCP echo: $out"; fi
  # メッシュ外のサービスへ（passthrough）
  out="$(cexec curl -s -m 10 http://plain."$NS"-plain/)" || true
  if echo "$out" | grep -q "^Hostname: plain"; then ok "mesh -> non-mesh service (passthrough)"; else bad "passthrough: $out"; fi
  if echo "$out" | grep -qi "X-Envoy-Peer-Metadata-Id: sidecar~"; then ok "peer metadata propagated outbound"; else bad "peer metadata missing"; fi
  # メッシュ外 → メッシュ（PERMISSIVE）
  out="$(kubectl -n "$NS-plain" exec deploy/plain -c curl -- curl -s -m 10 http://whoami."$NS"/)" || true
  if echo "$out" | grep -q "^Hostname: whoami"; then ok "non-mesh -> mesh (PERMISSIVE)"; else bad "PERMISSIVE: $out"; fi
  # STRICT
  cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: security.istio.io/v1
kind: PeerAuthentication
metadata: {name: default}
spec: {mtls: {mode: STRICT}}
EOF
  if retry 60 sh -c "! kubectl -n $NS-plain exec deploy/plain -c curl -- curl -s -m 5 http://whoami.$NS/ | grep -q Hostname"; then ok "STRICT rejects non-mesh clients"; else bad "STRICT did not reject"; fi
  out="$(cexec curl -s -m 10 http://whoami."$NS"/)" || true
  if echo "$out" | grep -q "^Hostname: whoami"; then ok "mesh client still allowed under STRICT"; else bad "STRICT mesh client: $out"; fi
  kubectl -n "$NS" delete peerauthentication default >/dev/null
  # 観測（受け側の標準メトリクス）
  local pod
  pod="$(kubectl -n "$NS" get pod -l app=whoami -o jsonpath='{.items[0].metadata.name}')"
  out="$(kubectl -n "$NS" exec "$pod" -c istio-proxy -- curl -s localhost:15090/stats/prometheus 2>/dev/null | grep '^istio_requests_total' | head -3)" || true
  if echo "$out" | grep -q "reporter=\"destination\".*source_workload=\"client\""; then ok "istio_requests_total (destination) has source_workload=client"; else bad "metrics: $out"; fi
  # istiod から見た同期
  out="$(istioctl proxy-status -v 1 -n "$NS" 2>&1)" || true
  local n_pods n_lines
  n_pods="$(kubectl -n "$NS" get pods --no-headers 2>/dev/null | grep -c Running)"
  # -v 1 は CDS・ECDS・EDS・LDS・RDS の列ごとに SYNCED / STALE / NOT SENT / IGNORED を出す
  n_lines="$(echo "$out" | grep -E "^(client|whoami|fortio)" | awk '{n=gsub(/SYNCED/,"&"); if (n>=4) c++} END{print c+0}')"
  if [ "$n_pods" = "$n_lines" ] && ! echo "$out" | grep -qE "STALE|ERROR"; then ok "istioctl proxy-status: all $n_lines proxies subscribed and in sync"; else bad "proxy-status ($n_lines/$n_pods): $out"; fi
}

tasks() {
  log "tasks ($MODE)"
  # Traffic shifting（重み付きの分割）: fortio に v2 を足して 80/20
  cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: apps/v1
kind: Deployment
metadata: {name: fortio-v2}
spec:
  replicas: 1
  selector: {matchLabels: {app: fortio, version: v2}}
  template:
    metadata:
      labels: {app: fortio, version: v2}
      annotations: {$(proxy_annotation server)}
    spec:
      serviceAccountName: fortio
      containers:
      - name: fortio
        image: fortio/fortio:latest
        imagePullPolicy: IfNotPresent
        args: ["server", "-http-port", "8080", "-grpc-port", "8079", "-tcp-port", "8078", "-redirect-port", "disabled"]
---
apiVersion: networking.istio.io/v1
kind: DestinationRule
metadata: {name: fortio}
spec:
  host: fortio
  subsets:
  - {name: v1, labels: {version: v1}}
  - {name: v2, labels: {version: v2}}
---
apiVersion: networking.istio.io/v1
kind: VirtualService
metadata: {name: fortio}
spec:
  hosts: [fortio]
  http:
  - match: [{headers: {x-fault: {exact: abort}}}]
    fault: {abort: {percentage: {value: 100}, httpStatus: 418}}
    route: [{destination: {host: fortio, subset: v1}}]
  - match: [{headers: {x-fault: {exact: delay}}}]
    fault: {delay: {percentage: {value: 100}, fixedDelay: 1s}}
    route: [{destination: {host: fortio, subset: v1}}]
  - match: [{uri: {prefix: /timeout}}]
    timeout: 0.5s
    rewrite: {uri: /echo}
    route: [{destination: {host: fortio, subset: v1}}]
  - match: [{uri: {prefix: /v2only}}]
    rewrite: {uri: /echo}
    route: [{destination: {host: fortio, subset: v2}}]
  - route:
    - {destination: {host: fortio, subset: v1}, weight: 80}
    - {destination: {host: fortio, subset: v2}, weight: 20}
    headers: {response: {add: {x-veil-test: shifted}}}
EOF
  kubectl -n "$NS" rollout status deploy/fortio-v2 --timeout=300s >/dev/null
  sleep 5
  local out
  if retry 60 sh -c "kubectl -n $NS exec deploy/client -c curl -- curl -s -m 5 -o /dev/null -w '%{http_code}' -H 'x-fault: abort' http://fortio.$NS:8080/echo | grep -q 418"; then ok "fault injection: abort 418"; else bad "fault abort"; fi
  out="$(cexec curl -s -m 10 -o /dev/null -w '%{http_code} %{time_total}' -H 'x-fault: delay' http://fortio."$NS":8080/echo)" || true
  if awk -v o="$out" 'BEGIN{split(o,a," "); exit !(a[1]==200 && a[2]>=0.9)}'; then ok "fault injection: delay 1s ($out)"; else bad "fault delay: $out"; fi
  out="$(cexec curl -s -m 10 -o /dev/null -w '%{http_code}' "http://fortio.$NS:8080/timeout?delay=2s")" || true
  if [ "$out" = 504 ]; then ok "request timeout 0.5s -> 504"; else bad "timeout: $out"; fi
  out="$(cexec curl -s -m 10 -D- -o /dev/null http://fortio."$NS":8080/echo)" || true
  if echo "$out" | grep -qi "x-veil-test: shifted"; then ok "response header add"; else bad "header add: $out"; fi
  # 80/20 の分割（200 回、v2 の割合が 10〜30%）
  local v2
  v2="$(cexec sh -c "for i in \$(seq 200); do curl -s -m 5 http://fortio.$NS:8080/debug | head -1; done" 2>/dev/null | grep -c ' on fortio-v2-' || true)"
  if [ "${v2:-0}" -ge 15 ] && [ "${v2:-0}" -le 70 ]; then ok "traffic shifting 80/20 (v2 got $v2/200)"; else bad "traffic shifting v2=$v2/200"; fi
  out="$(cexec curl -s -m 10 -o /dev/null -w '%{http_code}' http://fortio."$NS":8080/v2only)" || true
  if [ "$out" = 200 ]; then ok "uri rewrite + subset routing"; else bad "rewrite/subset: $out"; fi
  # Retries（上流が 503 を返すと 2 回まで再送: fortio の ?status=503:50 で半分失敗させる）
  cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: networking.istio.io/v1
kind: VirtualService
metadata: {name: whoami}
spec:
  hosts: [whoami]
  http:
  - route: [{destination: {host: whoami}}]
    retries: {attempts: 3, retryOn: 5xx}
EOF
  sleep 3
  # Authorization: client だけ GET を許す
  cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: security.istio.io/v1
kind: AuthorizationPolicy
metadata: {name: whoami-allow}
spec:
  selector: {matchLabels: {app: whoami}}
  action: ALLOW
  rules:
  - from: [{source: {principals: ["cluster.local/ns/$NS/sa/client"]}}]
    to: [{operation: {methods: ["GET"]}}]
EOF
  if retry 60 sh -c "kubectl -n $NS exec deploy/client -c curl -- curl -s -m 5 -o /dev/null -w '%{http_code}' -X POST http://whoami.$NS/ | grep -q 403"; then ok "AuthorizationPolicy denies POST"; else bad "authz POST not denied"; fi
  out="$(cexec curl -s -m 10 -o /dev/null -w '%{http_code}' http://whoami."$NS"/)" || true
  if [ "$out" = 200 ]; then ok "AuthorizationPolicy allows GET from client"; else bad "authz GET: $out"; fi
  out="$(kubectl -n "$NS" exec deploy/fortio -c fortio -- fortio curl -quiet http://whoami."$NS"/ 2>&1 | head -1)" || true
  if echo "$out" | grep -q 403; then ok "AuthorizationPolicy denies other principals"; else bad "authz other principal: $out"; fi
  kubectl -n "$NS" delete authorizationpolicy whoami-allow >/dev/null
  kubectl -n "$NS" delete virtualservice whoami fortio >/dev/null
  kubectl -n "$NS" delete destinationrule fortio >/dev/null
  kubectl -n "$NS" delete deploy fortio-v2 >/dev/null
}

cleanup() {
  if [ "$KEEP" != "--keep" ]; then
    kubectl delete ns "$NS" "$NS-plain" --wait=false >/dev/null 2>&1
  fi
}

ensure_cluster
deploy || { bad "deploy"; cleanup; exit 1; }
case "$SUITE" in
  smoke) smoke ;;
  tasks) tasks ;;
  all) smoke; tasks ;;
esac
cleanup
echo "== $MODE/$SUITE: pass=$PASS $([ $FAIL = 0 ] && echo ALL OK || echo FAILED)"
exit $FAIL
