#!/usr/bin/env bash
# Linkerd のサイドカー（linkerd2-proxy）を veil にした E2E（F-223・F-225）。kind の上の Linkerd（制御プレーンは本家）で
# 本家の linkerd2-proxy の Pod と veil の Pod を動かし、Linkerd のタスクを試す。
#
#   tools/k8s/linkerd-e2e.sh <stock|veil|mixed> <smoke|tasks|ops|certs|viz|all> [--keep]
#
#   stock: 全 Pod が本家の linkerd2-proxy（台本の正しさの確認）
#   veil:  全 Pod が veil
#   mixed: whoami・client2 が veil、fortio・client が本家（veil ⇄ 本家の両方向）
#
# 前提: kind・kubectl・linkerd（CLI）。veil のイメージ（既定 veil.local/linkerd-proxy:<ver>）は
# docker/Dockerfile.mesh-linkerd で作る（`config.linkerd.io/proxy-image` の注釈で注入する）。
set -uo pipefail
export PATH="$HOME/.local/bin:$PATH"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
mkdir -p "$ROOT/target/tmp"
export TMPDIR="$ROOT/target/tmp"
MODE="${1:-veil}"
SUITE="${2:-smoke}"
KEEP="${3:-}"
CLUSTER="${KIND_CLUSTER:-mesh}"
VEIL_IMAGE="${VEIL_LINKERD_IMAGE:-veil.local/linkerd-proxy}"
VEIL_VERSION="${VEIL_LINKERD_VERSION:-edge-26.10.1}"
NS="l5d-$MODE"
PASS=0
FAIL=0

ok()   { echo "  ok   $*"; PASS=$((PASS+1)); }
bad()  { echo "  FAIL $*"; FAIL=1; }
log()  { echo "== $*"; }
retry() { # retry <seconds> <command...>
  local t="$1"; shift
  local end=$((SECONDS + t))
  while [ $SECONDS -lt $end ]; do "$@" && return 0; sleep 2; done
  return 1
}

ensure_cluster() {
  kubectl config use-context "kind-$CLUSTER" >/dev/null || exit 1
  if ! kubectl get ns linkerd >/dev/null 2>&1; then
    log "Linkerd を導入"
    if ! kubectl get crd httproutes.gateway.networking.k8s.io >/dev/null 2>&1; then
      kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.2.1/standard-install.yaml >/dev/null 2>&1
    fi
    linkerd install --crds | kubectl apply -f - >/dev/null || exit 1
    linkerd install | kubectl apply -f - >/dev/null || exit 1
  fi
  kubectl -n linkerd rollout status deploy --timeout=300s >/dev/null
  for img in fortio/fortio:latest traefik/whoami:latest curlimages/curl:latest hashicorp/http-echo:latest; do
    docker image inspect "$img" >/dev/null 2>&1 || docker pull -q "$img" >/dev/null
    kind load docker-image --name "$CLUSTER" "$img" >/dev/null 2>&1
  done
  if [ "$MODE" != stock ]; then
    kind load docker-image --name "$CLUSTER" "$VEIL_IMAGE:$VEIL_VERSION" >/dev/null 2>&1 || { echo "failed to load $VEIL_IMAGE:$VEIL_VERSION"; exit 1; }
  fi
}

# proxy_ann <role>: veil にするワークロードだけ注釈を付ける
proxy_ann() {
  local veil="config.linkerd.io/proxy-image: $VEIL_IMAGE, config.linkerd.io/proxy-version: $VEIL_VERSION, config.linkerd.io/image-pull-policy: IfNotPresent"
  case "$MODE" in
    veil) echo "$veil" ;;
    mixed) case "$1" in whoami|client2) echo "$veil" ;; *) echo "linkerd.io/inject: enabled" ;; esac ;;
    *) echo "linkerd.io/inject: enabled" ;;
  esac
}

deploy() {
  log "deploy ($MODE)"
  # 前回の後片付け（--wait=false の削除）が終わるのを待つ
  kubectl wait --for=delete ns/"$NS" ns/"$NS-plain" --timeout=300s >/dev/null 2>&1
  kubectl create ns "$NS" >/dev/null 2>&1
  kubectl annotate ns "$NS" linkerd.io/inject=enabled --overwrite >/dev/null
  kubectl create ns "$NS-plain" >/dev/null 2>&1
  cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
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
      annotations: {$(proxy_ann whoami)}
    spec:
      serviceAccountName: whoami
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
metadata: {name: whoami-b}
spec:
  replicas: 1
  selector: {matchLabels: {app: whoami-b}}
  template:
    metadata:
      labels: {app: whoami-b}
      annotations: {$(proxy_ann whoami)}
    spec:
      serviceAccountName: whoami
      containers: [{name: whoami, image: traefik/whoami:latest, imagePullPolicy: IfNotPresent}]
---
apiVersion: v1
kind: Service
metadata: {name: whoami-b}
spec:
  selector: {app: whoami-b}
  ports: [{name: http, port: 80}]
---
apiVersion: v1
kind: ServiceAccount
metadata: {name: fortio}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: fortio}
spec:
  replicas: 1
  selector: {matchLabels: {app: fortio}}
  template:
    metadata:
      labels: {app: fortio}
      annotations: {$(proxy_ann fortio), config.linkerd.io/opaque-ports: "8078"}
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
metadata:
  name: fortio
  annotations: {config.linkerd.io/opaque-ports: "8078"}
spec:
  selector: {app: fortio}
  ports: [{name: http, port: 8080}, {name: grpc, port: 8079}, {name: tcp, port: 8078}]
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
      annotations: {$(proxy_ann client)}
    spec:
      serviceAccountName: client
      containers:
      - {name: curl, image: curlimages/curl:latest, imagePullPolicy: IfNotPresent, command: ["sleep", "infinity"]}
      - {name: fortio, image: fortio/fortio:latest, imagePullPolicy: IfNotPresent, args: ["server", "-http-port", "18080", "-grpc-port", "disabled", "-redirect-port", "disabled"]}
---
apiVersion: v1
kind: ServiceAccount
metadata: {name: client2}
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
      annotations: {$(proxy_ann client2)}
    spec:
      serviceAccountName: client2
      containers:
      - {name: curl, image: curlimages/curl:latest, imagePullPolicy: IfNotPresent, command: ["sleep", "infinity"]}
EOF
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
      containers: [{name: curl, image: curlimages/curl:latest, imagePullPolicy: IfNotPresent, command: ["sleep", "infinity"]}]
EOF
  kubectl -n "$NS" rollout status deploy/whoami deploy/whoami-b deploy/fortio deploy/client deploy/client2 --timeout=300s >/dev/null || {
    kubectl -n "$NS" get pods; return 1; }
  kubectl -n "$NS-plain" rollout status deploy/plain --timeout=300s >/dev/null || return 1
}

cexec()  { kubectl -n "$NS" exec deploy/client -c curl -- "$@"; }
c2exec() { kubectl -n "$NS" exec deploy/client2 -c curl -- "$@"; }
fexec()  { kubectl -n "$NS" exec deploy/client -c fortio -- "$@"; }
pexec()  { kubectl -n "$NS-plain" exec deploy/plain -c curl -- "$@"; }

is_veil() { # is_veil <deploy>
  kubectl -n "$NS" get deploy "$1" -o jsonpath='{.spec.template.metadata.annotations.config\.linkerd\.io/proxy-image}' | grep -q veil
}

smoke() {
  log "smoke ($MODE)"
  local d out
  for d in whoami fortio client client2; do
    local img
    img="$(kubectl -n "$NS" get pod -l app="$d" -o jsonpath='{.items[0].spec.initContainers[?(@.name=="linkerd-proxy")].image}{.items[0].spec.containers[?(@.name=="linkerd-proxy")].image}')"
    if is_veil "$d"; then
      case "$img" in *veil*) ok "$d proxy is veil" ;; *) bad "$d proxy should be veil: $img" ;; esac
    else
      case "$img" in *cr.l5d.io*) ok "$d proxy is linkerd2-proxy" ;; *) bad "$d proxy should be linkerd2-proxy: $img" ;; esac
    fi
  done
  if retry 60 sh -c "kubectl -n $NS exec deploy/client -c curl -- curl -s -m 5 http://whoami/ | grep -q '^Hostname: whoami'"; then ok "client -> whoami HTTP/1.1"; else bad "client -> whoami"; fi
  out="$(cexec curl -s -m 10 http://whoami/)" || true
  if echo "$out" | grep -qi "^L5d-Client-Id: client.$NS.serviceaccount.identity.linkerd.cluster.local"; then ok "automatic mTLS (l5d-client-id = client)"; else bad "mTLS client id: $(echo "$out" | grep -i l5d)"; fi
  out="$(c2exec curl -s -m 10 http://whoami/)" || true
  if echo "$out" | grep -qi "^L5d-Client-Id: client2.$NS.serviceaccount"; then ok "client2 -> whoami (mTLS, client2 identity)"; else bad "client2: $(echo "$out" | head -3)"; fi
  out="$(cexec sh -c 'head -c 300000 /dev/zero | tr "\0" a > /tmp/body; curl -s -m 20 -o /dev/null -w "%{http_code}" -X POST --data-binary @/tmp/body http://fortio:8080/echo')" || true
  if [ "$out" = 200 ]; then ok "POST 300KB"; else bad "POST 300KB: $out"; fi
  out="$(cexec curl -s -m 10 -H 'Transfer-Encoding: chunked' -d 'hello chunked' http://fortio:8080/echo)" || true
  if [ "$out" = "hello chunked" ]; then ok "chunked request body"; else bad "chunked: $out"; fi
  out="$(fexec fortio load -h2 -n 5 -qps 0 -loglevel Error http://fortio:8080/echo 2>&1)" || true
  if echo "$out" | grep -q "Code 200 : 5"; then ok "h2c (prior knowledge)"; else bad "h2c: $(echo "$out" | tail -3)"; fi
  out="$(fexec fortio load -grpc -ping -n 5 -qps 0 -loglevel Error fortio:8079 2>&1)" || true
  if echo "$out" | grep -q "SERVING : 5"; then ok "gRPC"; else bad "gRPC: $(echo "$out" | tail -3)"; fi
  out="$(fexec fortio load -n 3 -qps 0 -loglevel Error tcp://fortio:8078 2>&1)" || true
  if echo "$out" | grep -qE "Total Bytes sent: [1-9]|Total Bytes received: [1-9]|tcp OK : 3"; then ok "opaque TCP (opaque-ports)"; else bad "opaque TCP: $(echo "$out" | tail -3)"; fi
  out="$(pexec curl -s -m 10 -o /dev/null -w '%{http_code}' "http://whoami.$NS/")" || true
  if [ "$out" = 200 ]; then ok "unmeshed client -> meshed whoami (all-unauthenticated)"; else bad "unmeshed -> whoami: $out"; fi
  out="$(linkerd check --proxy -n "$NS" 2>&1)" || true
  if echo "$out" | grep -q "^×"; then bad "linkerd check --proxy: $(echo "$out" | grep -A2 '^×' | head -6)"; else ok "linkerd check --proxy (no errors)"; fi
}

tasks() {
  log "tasks ($MODE)"
  local out i n
  # Traffic split（HTTPRoute の parentRef が Service）: 全部 whoami-b へ
  cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: split}
spec:
  parentRefs: [{name: whoami, kind: Service, group: "", port: 80}]
  rules:
  - matches: [{headers: [{name: x-canary, value: "1"}]}]
    backendRefs: [{name: whoami-b, port: 80}]
  - backendRefs: [{name: whoami, port: 80, weight: 0}, {name: whoami-b, port: 80, weight: 100}]
EOF
  if retry 60 sh -c "kubectl -n $NS exec deploy/client -c curl -- curl -s -m 5 http://whoami/ | grep -q '^Hostname: whoami-b'"; then ok "traffic split (weight 0/100 -> whoami-b)"; else bad "traffic split"; fi
  out="$(cexec curl -s -m 5 -H 'x-canary: 1' http://whoami/)" || true
  if echo "$out" | grep -q '^Hostname: whoami-b'; then ok "HTTPRoute header match"; else bad "header match"; fi
  kubectl -n "$NS" delete httproutes.gateway.networking.k8s.io split >/dev/null
  # Timeouts（HTTPRoute の timeouts.request）
  cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: fortio-timeout}
spec:
  parentRefs: [{name: fortio, kind: Service, group: "", port: 8080}]
  rules:
  - matches: [{path: {type: PathPrefix, value: /echo}}]
    timeouts: {request: 1s}
    backendRefs: [{name: fortio, port: 8080}]
EOF
  if retry 60 sh -c "kubectl -n $NS exec deploy/client -c curl -- curl -s -m 10 -o /dev/null -w '%{http_code}' 'http://fortio:8080/echo?delay=3s' | grep -q 504"; then ok "request timeout 1s -> 504"; else bad "timeout: $(cexec curl -s -m 10 -o /dev/null -w '%{http_code}' 'http://fortio:8080/echo?delay=3s')"; fi
  out="$(cexec curl -s -m 10 -o /dev/null -w '%{http_code}' 'http://fortio:8080/echo?delay=100ms')" || true
  if [ "$out" = 200 ]; then ok "fast request within the timeout -> 200"; else bad "timeout fast: $out"; fi
  kubectl -n "$NS" delete httproutes.gateway.networking.k8s.io fortio-timeout >/dev/null
  # Retries（HTTPRoute の注釈 retry.linkerd.io/http）: 50% で 503 を返す宛先が、リトライで概ね成功する
  cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: fortio-retry
  annotations: {retry.linkerd.io/http: "5xx", retry.linkerd.io/limit: "4"}
spec:
  parentRefs: [{name: fortio, kind: Service, group: "", port: 8080}]
  rules:
  - backendRefs: [{name: fortio, port: 8080}]
EOF
  sleep 8
  n=0
  for i in $(seq 30); do
    out="$(cexec curl -s -m 10 -o /dev/null -w '%{http_code}' 'http://fortio:8080/echo?status=503:50')" || true
    [ "$out" = 200 ] && n=$((n+1))
  done
  if [ "$n" -ge 26 ]; then ok "retries on 5xx ($n/30 succeeded at 50% failure)"; else bad "retries: $n/30"; fi
  kubectl -n "$NS" delete httproutes.gateway.networking.k8s.io fortio-retry >/dev/null
  # Restricting access（Server + MeshTLSAuthentication + AuthorizationPolicy）: client だけ許可
  cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: policy.linkerd.io/v1beta3
kind: Server
metadata: {name: whoami-http}
spec:
  podSelector: {matchLabels: {app: whoami}}
  port: 80
  proxyProtocol: HTTP/1
---
apiVersion: policy.linkerd.io/v1alpha1
kind: MeshTLSAuthentication
metadata: {name: client-only}
spec:
  identities: ["client.$NS.serviceaccount.identity.linkerd.cluster.local"]
---
apiVersion: policy.linkerd.io/v1alpha1
kind: AuthorizationPolicy
metadata: {name: whoami-client}
spec:
  targetRef: {group: policy.linkerd.io, kind: Server, name: whoami-http}
  requiredAuthenticationRefs: [{name: client-only, kind: MeshTLSAuthentication, group: policy.linkerd.io}]
EOF
  if retry 60 sh -c "kubectl -n $NS exec deploy/client2 -c curl -- curl -s -m 5 -o /dev/null -w '%{http_code}' http://whoami/ | grep -q 403"; then ok "AuthorizationPolicy denies client2 (403)"; else bad "authz client2: $(c2exec curl -s -m 5 -o /dev/null -w '%{http_code}' http://whoami/)"; fi
  out="$(cexec curl -s -m 10 -o /dev/null -w '%{http_code}' http://whoami/)" || true
  if [ "$out" = 200 ]; then ok "AuthorizationPolicy allows client"; else bad "authz client: $out"; fi
  out="$(pexec curl -s -m 10 -o /dev/null -w '%{http_code}' "http://whoami.$NS/")" || true
  if [ "$out" = 403 ]; then ok "AuthorizationPolicy denies unmeshed (403)"; else bad "authz unmeshed: $out"; fi
  # Per-route policy（Server を parentRef にした HTTPRoute + ルート単位の AuthorizationPolicy）
  cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: policy.linkerd.io/v1beta3
kind: HTTPRoute
metadata: {name: whoami-app}
spec:
  parentRefs: [{name: whoami-http, kind: Server, group: policy.linkerd.io}]
  rules:
  - matches: [{path: {type: PathPrefix, value: /app}}]
---
apiVersion: policy.linkerd.io/v1alpha1
kind: AuthorizationPolicy
metadata: {name: whoami-app-client2}
spec:
  targetRef: {group: policy.linkerd.io, kind: HTTPRoute, name: whoami-app}
  requiredAuthenticationRefs: [{name: client2-only, kind: MeshTLSAuthentication, group: policy.linkerd.io}]
---
apiVersion: policy.linkerd.io/v1alpha1
kind: MeshTLSAuthentication
metadata: {name: client2-only}
spec:
  identities: ["client2.$NS.serviceaccount.identity.linkerd.cluster.local"]
EOF
  if retry 60 sh -c "kubectl -n $NS exec deploy/client2 -c curl -- curl -s -m 5 -o /dev/null -w '%{http_code}' http://whoami/app | grep -q 200"; then ok "per-route policy allows client2 on /app"; else bad "per-route /app client2: $(c2exec curl -s -m 5 -o /dev/null -w '%{http_code}' http://whoami/app)"; fi
  out="$(c2exec curl -s -m 10 -o /dev/null -w '%{http_code}' http://whoami/other)" || true
  if [ "$out" = 404 ]; then ok "per-route policy: unmatched route -> 404"; else bad "per-route /other: $out"; fi
  kubectl -n "$NS" delete authorizationpolicy.policy.linkerd.io whoami-client whoami-app-client2 >/dev/null 2>&1
  kubectl -n "$NS" delete httproute.policy.linkerd.io whoami-app >/dev/null 2>&1
  kubectl -n "$NS" delete meshtlsauthentication client-only client2-only >/dev/null 2>&1
  kubectl -n "$NS" delete servers.policy.linkerd.io whoami-http >/dev/null 2>&1
  retry 60 sh -c "kubectl -n $NS exec deploy/client2 -c curl -- curl -s -m 5 -o /dev/null -w '%{http_code}' http://whoami/ | grep -q 200" || true
  # Circuit breaking（failure accrual）: 2 つのエンドポイントのうち常に 500 を返す方を外す
  cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: apps/v1
kind: Deployment
metadata: {name: echo-good}
spec:
  selector: {matchLabels: {app: echo, role: good}}
  template:
    metadata:
      labels: {app: echo, role: good}
      annotations: {$(proxy_ann whoami)}
    spec:
      containers: [{name: echo, image: hashicorp/http-echo:latest, imagePullPolicy: IfNotPresent, args: ["-listen=:5678", "-text=good"]}]
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: echo-bad}
spec:
  selector: {matchLabels: {app: echo, role: bad}}
  template:
    metadata:
      labels: {app: echo, role: bad}
      annotations: {$(proxy_ann whoami)}
    spec:
      containers: [{name: echo, image: hashicorp/http-echo:latest, imagePullPolicy: IfNotPresent, args: ["-listen=:5678", "-text=bad", "-status-code=500"]}]
---
apiVersion: v1
kind: Service
metadata:
  name: echo
  annotations: {balancer.linkerd.io/failure-accrual: consecutive, balancer.linkerd.io/failure-accrual-consecutive-max-failures: "3", balancer.linkerd.io/failure-accrual-consecutive-min-penalty: "30s"}
spec:
  selector: {app: echo}
  ports: [{name: http, port: 80, targetPort: 5678}]
EOF
  kubectl -n "$NS" rollout status deploy/echo-good deploy/echo-bad --timeout=240s >/dev/null || bad "echo deploy"
  sleep 5
  for i in $(seq 20); do cexec curl -s -m 5 -o /dev/null http://echo/ || true; done
  n=0
  for i in $(seq 20); do
    out="$(cexec curl -s -m 5 -o /dev/null -w '%{http_code}' http://echo/)" || true
    [ "$out" = 200 ] && n=$((n+1))
  done
  if [ "$n" -ge 18 ]; then ok "failure accrual ejects the failing endpoint ($n/20 ok)"; else bad "failure accrual: $n/20"; fi
  kubectl -n "$NS" delete deploy echo-good echo-bad >/dev/null 2>&1
  kubectl -n "$NS" delete svc echo >/dev/null 2>&1
}

ops() {
  log "ops ($MODE)"
  local out
  # 制御プレーンの再起動: 既存の接続・新しい要求が続く
  kubectl -n linkerd rollout restart deploy/linkerd-destination deploy/linkerd-identity >/dev/null
  kubectl -n linkerd rollout status deploy/linkerd-destination deploy/linkerd-identity --timeout=300s >/dev/null
  if retry 90 sh -c "kubectl -n $NS exec deploy/client -c curl -- curl -s -m 5 http://whoami/ | grep -q '^Hostname: whoami'"; then ok "traffic continues after the control plane restarts"; else bad "after control plane restart"; fi
  # 新しい Pod が（再起動後の制御プレーンから）証明書を受け取れる
  kubectl -n "$NS" rollout restart deploy/whoami >/dev/null
  if kubectl -n "$NS" rollout status deploy/whoami --timeout=240s >/dev/null; then ok "new pod becomes ready (certified)"; else bad "new pod not ready"; fi
  if retry 60 sh -c "kubectl -n $NS exec deploy/client -c curl -- curl -s -m 5 http://whoami/ | grep -q '^L5d-Client-Id: client'"; then ok "mTLS to the new pod"; else bad "mTLS to new pod"; fi
  # SIGTERM: Pod の削除が猶予内に終わる
  local start=$SECONDS
  kubectl -n "$NS" delete pod -l app=client2 --wait=true --timeout=90s >/dev/null 2>&1
  if [ $((SECONDS - start)) -lt 60 ]; then ok "pod terminates promptly on SIGTERM ($((SECONDS - start))s)"; else bad "pod termination took $((SECONDS - start))s"; fi
  out="$(kubectl -n "$NS" exec deploy/whoami -c linkerd-proxy -- /usr/lib/linkerd/linkerd2-proxy --version 2>/dev/null || true)"
  case "$out" in *veil*) is_veil whoami && ok "linkerd2-proxy --version reports veil" ;; esac
}

# viz の確認（emojivoto・booksapp。linkerd viz stat / edges / tap / top）
viz() {
  log "viz ($MODE)"
  if ! kubectl get ns linkerd-viz >/dev/null 2>&1; then
    linkerd viz install | kubectl apply -f - >/dev/null || { bad "viz install"; return; }
  fi
  kubectl -n linkerd-viz rollout status deploy --timeout=300s >/dev/null
  local img app ns
  for img in ghcr.io/buoyantio/emojivoto-emoji-svc:v11 ghcr.io/buoyantio/emojivoto-web:v11 ghcr.io/buoyantio/emojivoto-voting-svc:v11 \
             ghcr.io/buoyantio/booksapp:v0.0.5 ghcr.io/buoyantio/booksapp-traffic:v0.0.3; do
    docker image inspect "$img" >/dev/null 2>&1 || docker pull -q "$img" >/dev/null
    kind load docker-image --name "$CLUSTER" "$img" >/dev/null 2>&1
  done
  curl -sL -m 30 https://run.linkerd.io/emojivoto.yml -o "$TMPDIR/emojivoto.yml"
  curl -sL -m 30 https://run.linkerd.io/booksapp.yml -o "$TMPDIR/booksapp.yml"
  for app in emojivoto booksapp; do
    ns="$NS-$app"
    kubectl create ns "$ns" >/dev/null 2>&1
    # emojivoto は名前空間を名前で参照するので置き換える。veil の注入は mode に従う
    # （mixed は emojivoto の web・emoji と booksapp の webapp・books を veil）
    sed -e "s/emojivoto:/$ns:/g; s/^  namespace: emojivoto$/  namespace: $ns/; /^kind: Namespace/,/^---/d" "$TMPDIR/$app.yml" > "$TMPDIR/$app-$MODE.yml"
    case "$MODE" in
      stock) linkerd inject "$TMPDIR/$app-$MODE.yml" ;;
      veil) linkerd inject --proxy-image "$VEIL_IMAGE" --proxy-version "$VEIL_VERSION" --image-pull-policy IfNotPresent "$TMPDIR/$app-$MODE.yml" ;;
      mixed) linkerd inject "$TMPDIR/$app-$MODE.yml" | python3 -c '
import sys, yaml
docs = [d for d in yaml.safe_load_all(sys.stdin) if d]
for d in docs:
    if d.get("kind") == "Deployment" and d["metadata"]["name"] in ("web", "emoji", "webapp", "books"):
        a = d["spec"]["template"]["metadata"].setdefault("annotations", {})
        a["config.linkerd.io/proxy-image"] = sys.argv[1]
        a["config.linkerd.io/proxy-version"] = sys.argv[2]
        a["config.linkerd.io/image-pull-policy"] = "IfNotPresent"
print(yaml.safe_dump_all(docs))' "$VEIL_IMAGE" "$VEIL_VERSION" ;;
    esac 2>/dev/null | kubectl -n "$ns" apply -f - >/dev/null
    kubectl -n "$ns" rollout status deploy --timeout=300s >/dev/null || bad "$app deploy"
  done
  sleep 70
  ns="$NS-emojivoto"
  out="$(linkerd viz stat deploy -n "$ns" 2>&1)"
  if echo "$out" | awk '$1 ~ /^(emoji|voting|web)$/ && $3 ~ /%/' | wc -l | grep -q 3; then ok "viz stat (emojivoto: emoji, voting, web have success rates)"; else bad "viz stat emojivoto: $out"; fi
  # viz の Prometheus のスクレイプが回るまで待つ
  edges_ok() { local o; o="$(linkerd viz edges deploy -n "$1" 2>&1)"; shift; for e in "$@"; do echo "$o" | grep -qE "^${e% *} +${e#* } .*√" || return 1; done; }
  retry 120 edges_ok "$ns" "vote-bot web" "web emoji" "web voting"
  out="$(linkerd viz edges deploy -n "$ns" 2>&1)"
  if echo "$out" | grep -qE "^vote-bot +web .*√" && echo "$out" | grep -qE "^web +emoji .*√" && echo "$out" | grep -qE "^web +voting .*√"; then
    ok "viz edges (vote-bot->web, web->emoji, web->voting secured)"; else bad "viz edges: $out"; fi
  out="$(timeout 15 linkerd viz tap deploy/web -n "$ns" --max-rps 5 2>&1 | head -30)"
  if echo "$out" | grep -q "^req id=" && echo "$out" | grep -q "^end id="; then ok "viz tap deploy/web"; else bad "viz tap: $(echo "$out" | head -3)"; fi
  script -qc "stty cols 200 rows 40; TERM=xterm timeout 12 linkerd viz top deploy/web -n $ns" /dev/null > "$TMPDIR/top-$MODE.out" 2>&1
  if sed 's/\x1b\[[0-9;]*[A-Za-z]//g' "$TMPDIR/top-$MODE.out" | grep -aq "EmojiService"; then ok "viz top deploy/web"; else bad "viz top"; fi
  ns="$NS-booksapp"
  out="$(linkerd viz stat deploy -n "$ns" 2>&1)"
  if echo "$out" | awk '$1 ~ /^(authors|books|webapp)$/ && $3 ~ /%/' | wc -l | grep -q 3; then ok "viz stat (booksapp: authors, books, webapp)"; else bad "viz stat booksapp: $out"; fi
  retry 120 edges_ok "$ns" "webapp books" "webapp authors"
  out="$(linkerd viz edges deploy -n "$ns" 2>&1)"
  if echo "$out" | grep -qE "^webapp +books .*√" && echo "$out" | grep -qE "^webapp +authors .*√"; then ok "viz edges (booksapp)"; else bad "viz edges booksapp: $out"; fi
  out="$(linkerd check --proxy -n "$NS-emojivoto" 2>&1)" || true
  if echo "$out" | grep -q "^×"; then bad "linkerd check --proxy (emojivoto): $(echo "$out" | grep -A2 '^×' | head -6)"; else ok "linkerd check --proxy (emojivoto)"; fi
  if [ "$KEEP" != "--keep" ]; then kubectl delete ns "$NS-emojivoto" "$NS-booksapp" --wait=false >/dev/null 2>&1; fi
}

# 証明書の更新（ワークロードの証明書の有効期間を 1 分にして、更新の後も通信が続く）
certs() {
  log "certs ($MODE)"
  local e1 e2
  linkerd upgrade --set identity.issuer.issuanceLifetime=1m0s 2>/dev/null | kubectl apply -f - >/dev/null 2>&1
  # upgrade は proxy-injector も作り直す。準備ができる前に Pod を作ると注入されない（webhook は失敗を無視する）
  kubectl -n linkerd rollout status deploy/linkerd-identity deploy/linkerd-proxy-injector deploy/linkerd-destination --timeout=300s >/dev/null
  kubectl -n "$NS" rollout restart deploy/whoami deploy/client >/dev/null
  kubectl -n "$NS" rollout status deploy/whoami deploy/client --timeout=300s >/dev/null
  local d
  for d in whoami client; do
    if ! kubectl -n "$NS" get pod -l app=$d -o jsonpath='{.items[*].spec.initContainers[*].name} {.items[*].spec.containers[*].name}' | grep -q linkerd-proxy; then
      kubectl -n "$NS" rollout restart deploy/$d >/dev/null
      kubectl -n "$NS" rollout status deploy/$d --timeout=300s >/dev/null
    fi
  done
  e1="$(linkerd diagnostics proxy-metrics -n "$NS" deploy/whoami 2>/dev/null | awk '/^control_identity_cert_expiration_timestamp_seconds/{print $2; exit}')"
  sleep 75
  e2="$(linkerd diagnostics proxy-metrics -n "$NS" deploy/whoami 2>/dev/null | awk '/^control_identity_cert_expiration_timestamp_seconds/{print $2; exit}')"
  # 期限が変わり、まだ切れていないこと（最初の証明書は寿命の設定が効く前の 24 時間のものの場合があり、更新後の方が早く切れる）
  if [ -n "$e1" ] && [ -n "$e2" ] && awk -v a="$e1" -v b="$e2" -v now="$(date +%s)" 'BEGIN{exit !(b != a && b > now)}'; then ok "workload certificate renewed ($e1 -> $e2)"; else bad "certificate not renewed: $e1 -> $e2"; fi
  if retry 30 sh -c "kubectl -n $NS exec deploy/client -c curl -- curl -s -m 5 http://whoami/ | grep -q '^L5d-Client-Id: client'"; then ok "mTLS keeps working after the renewal"; else bad "mTLS after renewal"; fi
  linkerd upgrade --set identity.issuer.issuanceLifetime=24h0m0s 2>/dev/null | kubectl apply -f - >/dev/null 2>&1
  kubectl -n linkerd rollout status deploy/linkerd-identity deploy/linkerd-proxy-injector deploy/linkerd-destination --timeout=300s >/dev/null
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
  ops) ops ;;
  viz) viz ;;
  certs) certs ;;
  all) smoke; tasks; ops; certs; viz ;;
esac
cleanup
echo "== $MODE/$SUITE: pass=$PASS $([ $FAIL = 0 ] && echo ALL OK || echo FAILED)"
exit $FAIL
