#!/usr/bin/env bash
# メッシュのデータプレーンの E2E（F-225）。kind のクラスタに Istio を入れ、サイドカーのプロキシを
# 本家（stock = Envoy）・veil・混在（mixed）で動かして同じ台本を流す。
#
#   tools/k8s/mesh-e2e.sh <stock|veil|mixed> <smoke|tasks|gateway|telemetry|wasm|all> [--keep]
#
# 使うイメージ: veil.local/istio-proxy:<istio の版>（docker/Dockerfile.mesh-istio。musl の veil を istio/proxyv2 に入れたもの）
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
# Istio の注入テンプレートは `/` を含む名前のときだけ sidecar.istio.io/proxyImage を使う
VEIL_PROXY="${VEIL_PROXY_IMAGE:-veil.local/istio-proxy:$ISTIO_VERSION}"
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

# proxy_annotation <role>: veil のプロキシにするワークロードだけ注釈を付ける。
# mixed は whoami・client2 = veil、fortio・client = Envoy（veil ⇄ Envoy の両方向を試す）
proxy_annotation() {
  case "$MODE" in
    veil) echo "sidecar.istio.io/proxyImage: $VEIL_PROXY" ;;
    mixed) case "$1" in
             whoami|client2) echo "sidecar.istio.io/proxyImage: $VEIL_PROXY" ;;
             *) echo "sidecar.istio.io/inject: \"true\"" ;;
           esac ;;
    *) echo "sidecar.istio.io/inject: \"true\"" ;;
  esac
}

deploy() {
  kubectl create ns "$NS" >/dev/null 2>&1
  kubectl label ns "$NS" istio-injection=enabled --overwrite >/dev/null
  kubectl create ns "$NS-plain" >/dev/null 2>&1
  local fortio_ann whoami_ann client_ann client2_ann
  fortio_ann="$(proxy_annotation fortio)"
  whoami_ann="$(proxy_annotation whoami)"
  client_ann="$(proxy_annotation client)"
  client2_ann="$(proxy_annotation client2)"
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
      annotations: {$fortio_ann}
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
      annotations: {$whoami_ann}
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
  local d want
  for d in whoami fortio client client2; do
    case "$(proxy_annotation $d)" in *proxyImage*) want=veil ;; *) want=Envoy ;; esac
    if proxy_is_veil $d; then got=veil; else got=Envoy; fi
    [ "$got" = "$want" ] && ok "$d proxy is $got" || bad "$d proxy is $got (want $want)"
  done

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
  # client2 → fortio（mixed では veil → Envoy）: HTTP/1.1・h2c・TCP
  out="$(c2exec curl -s -m 10 -o /dev/null -w '%{http_code}' http://fortio."$NS":8080/echo)" || true
  if [ "$out" = 200 ]; then ok "HTTP/1.1 client2 -> fortio"; else bad "client2 -> fortio: $out"; fi
  out="$(c2exec curl -s -m 10 --http2-prior-knowledge -o /dev/null -w '%{http_code} %{http_version}' http://fortio."$NS":8080/echo)" || true
  if [ "$out" = "200 2" ]; then ok "h2c client2 -> fortio"; else bad "h2c client2 -> fortio: $out"; fi
  out="$(c2exec sh -c "printf 'ping-tcp' | curl -s -m 3 telnet://fortio.$NS:8078" 2>/dev/null)" || true
  if echo "$out" | grep -q "ping-tcp"; then ok "TCP client2 -> fortio"; else bad "TCP client2 -> fortio: $out"; fi
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
  # istioctl proxy-config（管理 API の config_dump・clusters?format=json を読む）
  local cpod t pcok=1
  cpod="$(kubectl -n "$NS" get pod -l app=client -o jsonpath='{.items[0].metadata.name}')"
  for t in listeners clusters routes endpoints secret; do
    out="$(istioctl proxy-config "$t" -n "$NS" "$cpod" 2>&1)" || pcok=0
    if echo "$out" | grep -qiE "panic|error"; then pcok=0; fi
    [ "$(echo "$out" | wc -l)" -ge 2 ] || pcok=0
  done
  if [ $pcok = 1 ] && istioctl proxy-config secret -n "$NS" "$cpod" 2>&1 | grep -q "default.*ACTIVE.*true"; then
    ok "istioctl proxy-config listeners/clusters/routes/endpoints/secret"; else bad "istioctl proxy-config"; fi
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
      annotations: {$(proxy_annotation fortio)}
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
  kubectl -n "$NS" delete destinationrule fortio >/dev/null 2>&1
  kubectl -n "$NS" delete deploy fortio-v2 >/dev/null 2>&1
  # Retries: fortio の ?status=503:50 は半分の確率で 503。4 回まで再送すれば 50 回中ほぼ全部 200 になる
  cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: networking.istio.io/v1
kind: VirtualService
metadata: {name: fortio}
spec:
  hosts: [fortio]
  http:
  - match: [{uri: {prefix: /retry}}]
    rewrite: {uri: /echo}
    retries: {attempts: 4, retryOn: 5xx}
    route: [{destination: {host: fortio}}]
  - match: [{uri: {prefix: /noretry}}]
    rewrite: {uri: /echo}
    retries: {attempts: 0}
    route: [{destination: {host: fortio}}]
  - route: [{destination: {host: fortio}}]
    mirror: {host: whoami, port: {number: 80}}
    mirrorPercentage: {value: 100}
EOF
  sleep 5
  local okn loop50
  loop50='for i in $(seq 50); do curl -s -m 5 -o /dev/null -w "%{http_code}\n" "$0"; done'
  okn="$(cexec sh -c "$loop50" "http://fortio.$NS:8080/retry?status=503:50" | grep -c '^200$')"
  if [ "${okn:-0}" -ge 45 ]; then ok "retries on 5xx ($okn/50 succeeded)"; else bad "retries: only $okn/50 succeeded"; fi
  okn="$(cexec sh -c "$loop50" "http://fortio.$NS:8080/noretry?status=503:50" | grep -c '^200$')"
  if [ "${okn:-0}" -le 40 ]; then ok "no retries when attempts=0 ($okn/50 succeeded)"; else bad "attempts=0 still retried ($okn/50)"; fi
  # Mirroring: fortio への要求を whoami へ複製する（whoami の受け側の istio_requests_total が増える）
  local wpod before after
  wpod="$(kubectl -n "$NS" get pod -l app=whoami -o jsonpath='{.items[0].metadata.name}')"
  dest_total() { kubectl -n "$NS" exec "$wpod" -c istio-proxy -- curl -s localhost:15090/stats/prometheus 2>/dev/null | grep '^istio_requests_total{reporter="destination"' | awk '{s+=$NF} END{print s+0}'; }
  before="$(dest_total)"
  cexec sh -c 'for i in $(seq 10); do curl -s -m 5 -o /dev/null "$0"; done' "http://fortio.$NS:8080/echo"
  after="$before"
  for _ in $(seq 15); do after="$(dest_total)"; [ "$after" -ge $((before + 10)) ] && break; sleep 2; done
  if [ "$after" -ge $((before + 10)) ]; then ok "mirroring to whoami ($before -> $after)"; else bad "mirroring: $before -> $after"; fi
  kubectl -n "$NS" delete virtualservice fortio >/dev/null
  # Circuit breaking: 接続 1・保留 1 に絞り、同時 10 本で溢れた分は 503（UO）
  cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: networking.istio.io/v1
kind: DestinationRule
metadata: {name: fortio-cb}
spec:
  host: fortio
  trafficPolicy:
    connectionPool:
      tcp: {maxConnections: 1}
      http: {http1MaxPendingRequests: 1, maxRequestsPerConnection: 1}
EOF
  sleep 5
  out="$(fexec fortio load -c 10 -qps 0 -n 200 -loglevel Warning "http://fortio.$NS:8080/echo?delay=20ms" 2>&1 | grep -E '^Code (200|503)' | tr '\n' ' ')" || true
  if echo "$out" | grep -q "Code 503" && echo "$out" | grep -q "Code 200"; then ok "circuit breaking: overflow -> 503 ($out)"; else bad "circuit breaking: $out"; fi
  kubectl -n "$NS" delete destinationrule fortio-cb >/dev/null
  # JWT（RequestAuthentication + requestPrincipals と claim の条件。Istio の samples の鍵とトークン）
  cat <<'EOF' | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: security.istio.io/v1
kind: RequestAuthentication
metadata: {name: jwt}
spec:
  selector: {matchLabels: {app: whoami}}
  jwtRules:
  - issuer: "testing@secure.istio.io"
    jwks: '{ "keys":[ {"e":"AQAB","kid":"DHFbpoIUqrY8t2zpA2qXfCmr5VO5ZEr4RzHU_-envvQ","kty":"RSA","n":"xAE7eB6qugXyCAG3yhh7pkDkT65pHymX-P7KfIupjf59vsdo91bSP9C8H07pSAGQO1MV_xFj9VswgsCg4R6otmg5PV2He95lZdHtOcU5DXIg_pbhLdKXbi66GlVeK6ABZOUW3WYtnNHD-91gVuoeJT_DwtGGcp4ignkgXfkiEm4sw-4sfb4qdt5oLbyVpmW6x9cfa7vs2WTfURiCrBoUqgBo_-4WTiULmmHSGZHOjzwa8WtrtOQGsAFjIbno85jp6MnGGGZPYZbDAa_b3y5u-YpW7ypZrvD8BgtKVjgtQgZhLAGezMt0ua3DRrWnKqTZ0BJ_EyxOGuHJrLsn00fnMQ"}]}'
---
apiVersion: security.istio.io/v1
kind: AuthorizationPolicy
metadata: {name: require-jwt}
spec:
  selector: {matchLabels: {app: whoami}}
  action: ALLOW
  rules:
  - from:
    - source:
        requestPrincipals: ["testing@secure.istio.io/testing@secure.istio.io"]
    when:
    - key: request.auth.claims[groups]
      values: ["group1"]
EOF
  local tok
  tok="$(tr -d '\n' < "$ROOT/tools/k8s/testdata/groups-scope.jwt")"
  if retry 60 sh -c "kubectl -n $NS exec deploy/client -c curl -- curl -s -m 5 -o /dev/null -w '%{http_code}' http://whoami.$NS/ | grep -q 403"; then ok "JWT: no token -> 403"; else bad "JWT: no token not denied"; fi
  out="$(cexec curl -s -m 10 -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $tok" http://whoami."$NS"/)" || true
  if [ "$out" = 200 ]; then ok "JWT: valid token with group1 -> 200"; else bad "JWT valid token: $out"; fi
  out="$(cexec curl -s -m 10 -o /dev/null -w '%{http_code}' -H "Authorization: Bearer ${tok}x" http://whoami."$NS"/)" || true
  if [ "$out" = 401 ]; then ok "JWT: tampered token -> 401"; else bad "JWT tampered token: $out"; fi
  kubectl -n "$NS" delete requestauthentication jwt >/dev/null
  kubectl -n "$NS" delete authorizationpolicy require-jwt >/dev/null
  # 次の試験の前に、ポリシーの削除がプロキシへ届くのを待つ
  retry 60 sh -c "kubectl -n $NS exec deploy/client -c curl -- curl -s -m 5 -o /dev/null -w '%{http_code}' http://whoami.$NS/ | grep -q 200" || true
  # Telemetry API のアクセスログ（Envoy の既定の書式で stdout）
  cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: telemetry.istio.io/v1
kind: Telemetry
metadata: {name: logs}
spec:
  accessLogging:
  - providers: [{name: envoy}]
EOF
  sleep 6
  cexec curl -s -m 5 -o /dev/null "http://fortio.$NS:8080/echo?accesslog=1"
  if retry 30 sh -c "kubectl -n $NS logs deploy/client -c istio-proxy --since=60s | grep -q '\"GET /echo?accesslog=1 HTTP/1.1\" 200 - via_upstream'"; then
    ok "access log (Telemetry API, Envoy format)"; else bad "access log: $(kubectl -n "$NS" logs deploy/client -c istio-proxy --since=60s | grep accesslog | tail -1)"; fi
  kubectl -n "$NS" delete telemetry logs >/dev/null
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
  kubectl -n "$NS" delete destinationrule fortio >/dev/null 2>&1
  kubectl -n "$NS" delete deploy fortio-v2 >/dev/null 2>&1
}

gateway() {
  log "gateway ($MODE)"
  local gw_ann ip out
  gw_ann="$(proxy_annotation whoami)"
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 7 -subj "/CN=gw.example.com" \
    -addext "subjectAltName=DNS:gw.example.com" -keyout "$TMPDIR/gw-$NS.key" -out "$TMPDIR/gw-$NS.crt" 2>/dev/null
  kubectl -n "$NS" delete secret gw-cert >/dev/null 2>&1
  kubectl -n "$NS" create secret tls gw-cert --cert="$TMPDIR/gw-$NS.crt" --key="$TMPDIR/gw-$NS.key" >/dev/null
  cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: v1
kind: ServiceAccount
metadata: {name: ingress}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: ingress}
spec:
  selector: {matchLabels: {istio: ingress-$NS}}
  template:
    metadata:
      annotations: {inject.istio.io/templates: gateway, $gw_ann}
      labels: {istio: ingress-$NS, sidecar.istio.io/inject: "true"}
    spec:
      serviceAccountName: ingress
      containers:
      - {name: istio-proxy, image: auto, imagePullPolicy: IfNotPresent}
---
apiVersion: v1
kind: Service
metadata: {name: ingress}
spec:
  selector: {istio: ingress-$NS}
  ports:
  - {name: http, port: 80, targetPort: 8080}
  - {name: https, port: 443, targetPort: 8443}
---
apiVersion: networking.istio.io/v1
kind: Gateway
metadata: {name: gw}
spec:
  selector: {istio: ingress-$NS}
  servers:
  - port: {number: 8080, name: http, protocol: HTTP}
    hosts: ["*"]
  - port: {number: 8443, name: https, protocol: HTTPS}
    hosts: ["gw.example.com"]
    tls: {mode: SIMPLE, credentialName: gw-cert}
---
apiVersion: networking.istio.io/v1
kind: VirtualService
metadata: {name: via-gw}
spec:
  hosts: ["*"]
  gateways: [gw]
  http:
  - match: [{uri: {prefix: /fortio/}}]
    rewrite: {uri: /}
    route: [{destination: {host: fortio, port: {number: 8080}}}]
  - route: [{destination: {host: whoami, port: {number: 80}}}]
EOF
  kubectl -n "$NS" rollout status deploy/ingress --timeout=240s >/dev/null || { bad "gateway deploy"; return; }
  if proxy_is_veil ingress; then
    case "$gw_ann" in *proxyImage*) ok "gateway proxy is veil" ;; *) bad "gateway proxy should be Envoy" ;; esac
  else
    case "$gw_ann" in *proxyImage*) bad "gateway proxy is not veil" ;; *) ok "gateway proxy is Envoy" ;; esac
  fi
  ip="$(kubectl -n "$NS" get svc ingress -o jsonpath='{.spec.clusterIP}')"
  local pexec="kubectl -n $NS-plain exec deploy/plain -c curl --"
  if retry 60 sh -c "$pexec curl -s -m 5 http://$ip/ | grep -q '^Hostname: whoami'"; then ok "Istio Gateway HTTP -> whoami"; else bad "gateway HTTP"; fi
  out="$($pexec curl -s -m 10 http://$ip/)" || true
  if echo "$out" | grep -qi "X-Forwarded-For: "; then ok "gateway adds X-Forwarded-For"; else bad "gateway XFF missing"; fi
  if echo "$out" | grep -qi "X-Forwarded-Client-Cert: By=spiffe://cluster.local/ns/$NS/sa/whoami;.*URI=spiffe://cluster.local/ns/$NS/sa/ingress"; then
    ok "gateway -> backend uses mTLS (gateway identity)"; else bad "gateway mTLS: $(echo "$out" | grep -i client-cert)"; fi
  if retry 60 sh -c "$pexec curl -s -m 5 --cacert /dev/null -k --resolve gw.example.com:443:$ip https://gw.example.com/ | grep -q '^Hostname: whoami'"; then
    ok "Istio Gateway HTTPS (SIMPLE, credentialName)"; else bad "gateway HTTPS"; fi
  out="$($pexec curl -s -m 10 -o /dev/null -w '%{http_code}' http://$ip/fortio/echo)" || true
  if [ "$out" = 200 ]; then ok "gateway prefix rewrite -> fortio"; else bad "gateway rewrite: $out"; fi
  # Gateway API（HTTPRoute）
  if ! kubectl get crd httproutes.gateway.networking.k8s.io >/dev/null 2>&1; then
    kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.2.1/standard-install.yaml >/dev/null 2>&1
  fi
  # 生成されるゲートウェイの Deployment は ConfigMap のオーバーレイ（parametersRef）でイメージを差し替える
  local gimg="docker.io/istio/proxyv2:$ISTIO_VERSION"
  case "$gw_ann" in *proxyImage*) gimg="$VEIL_PROXY" ;; esac
  cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: v1
kind: ConfigMap
metadata: {name: gapi-options}
data:
  deployment: |
    spec:
      template:
        spec:
          containers:
          - name: istio-proxy
            image: $gimg
            imagePullPolicy: IfNotPresent
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: gapi}
spec:
  gatewayClassName: istio
  infrastructure:
    parametersRef: {group: "", kind: ConfigMap, name: gapi-options}
  listeners:
  - {name: http, port: 80, protocol: HTTP, allowedRoutes: {namespaces: {from: Same}}}
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: gapi}
spec:
  parentRefs: [{name: gapi}]
  rules:
  - matches: [{path: {type: PathPrefix, value: /app}}]
    filters:
    - type: RequestHeaderModifier
      requestHeaderModifier: {add: [{name: x-gapi, value: "1"}]}
    backendRefs: [{name: whoami, port: 80}]
EOF
  if retry 180 sh -c "kubectl -n $NS get deploy gapi-istio >/dev/null 2>&1"; then
    kubectl -n "$NS" rollout status deploy/gapi-istio --timeout=240s >/dev/null
    local gip
    gip="$(kubectl -n "$NS" get svc gapi-istio -o jsonpath='{.spec.clusterIP}')"
    if retry 90 sh -c "$pexec curl -s -m 5 http://$gip/app | grep -qi '^X-Gapi: 1'"; then ok "Gateway API HTTPRoute (header modifier)"; else bad "Gateway API HTTPRoute"; fi
    out="$($pexec curl -s -m 5 -o /dev/null -w '%{http_code}' http://$gip/other)" || true
    if [ "$out" = 404 ]; then ok "Gateway API unmatched path -> 404"; else bad "Gateway API unmatched: $out"; fi
    case "$gw_ann" in
      *proxyImage*) if kubectl -n "$NS" exec deploy/gapi-istio -- /usr/local/bin/envoy --version 2>/dev/null | grep -q veil; then ok "Gateway API gateway is veil"; else bad "Gateway API gateway is not veil"; fi ;;
    esac
  else
    bad "Gateway API deployment not created"
  fi
  kubectl -n "$NS" delete httproute gapi >/dev/null 2>&1
  kubectl -n "$NS" delete gateway.gateway.networking.k8s.io gapi >/dev/null 2>&1
  kubectl -n "$NS" delete configmap gapi-options >/dev/null 2>&1
  kubectl -n "$NS" delete virtualservice via-gw >/dev/null 2>&1
  kubectl -n "$NS" delete gateway.networking.istio.io gw >/dev/null 2>&1
  kubectl -n "$NS" delete deploy ingress >/dev/null 2>&1
  kubectl -n "$NS" delete svc ingress >/dev/null 2>&1
}

telemetry() {
  log "telemetry ($MODE)"
  # Jaeger（Zipkin 互換の受け口）と Istio のトレースのプロバイダ（無ければ入れる）
  if ! kubectl -n istio-system get deploy jaeger >/dev/null 2>&1; then
    docker image inspect jaegertracing/all-in-one:latest >/dev/null 2>&1 || docker pull -q jaegertracing/all-in-one:latest >/dev/null
    kind load docker-image --name "$CLUSTER" jaegertracing/all-in-one:latest >/dev/null 2>&1
    cat <<'EOF' | kubectl -n istio-system apply -f - >/dev/null
apiVersion: apps/v1
kind: Deployment
metadata: {name: jaeger}
spec:
  selector: {matchLabels: {app: jaeger}}
  template:
    metadata: {labels: {app: jaeger}, annotations: {sidecar.istio.io/inject: "false"}}
    spec:
      containers:
      - name: jaeger
        image: jaegertracing/all-in-one:latest
        imagePullPolicy: IfNotPresent
        env: [{name: COLLECTOR_ZIPKIN_HOST_PORT, value: ":9411"}]
---
apiVersion: v1
kind: Service
metadata: {name: jaeger-collector}
spec:
  selector: {app: jaeger}
  ports: [{name: http-zipkin, port: 9411}, {name: http-query, port: 16686}]
EOF
    kubectl -n istio-system rollout status deploy/jaeger --timeout=180s >/dev/null
  fi
  if ! kubectl -n istio-system get cm istio -o jsonpath='{.data.mesh}' | grep -q "jaeger-collector"; then
    istioctl install -y --set profile=minimal --set meshConfig.enableTracing=true \
      --set 'meshConfig.extensionProviders[0].name=zipkin' \
      --set 'meshConfig.extensionProviders[0].zipkin.service=jaeger-collector.istio-system.svc.cluster.local' \
      --set 'meshConfig.extensionProviders[0].zipkin.port=9411' >/dev/null 2>&1
    sleep 10
  fi
  cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: telemetry.istio.io/v1
kind: Telemetry
metadata: {name: tracing}
spec:
  tracing:
  - providers: [{name: zipkin}]
    randomSamplingPercentage: 100
EOF
  sleep 8
  local tid j
  tid="$(printf '%032x' $((RANDOM * RANDOM)))"
  cexec curl -s -m 5 -o /dev/null -H "x-b3-traceid: $tid" -H "x-b3-spanid: 00000000000000aa" -H "x-b3-sampled: 1" "http://whoami.$NS/"
  j="$(kubectl -n istio-system get pod -l app=jaeger -o name | head -1)"
  local spans=""
  for _ in $(seq 20); do
    spans="$(kubectl -n istio-system exec "$j" -- wget -qO- "http://localhost:16686/api/traces/$tid" 2>/dev/null | python3 -c 'import json,sys; d=json.load(sys.stdin); t=(d.get("data") or [{}])[0]; print(len(t.get("spans",[])), " ".join(sorted(set(p["serviceName"] for p in t.get("processes",{}).values()))))' 2>/dev/null)"
    case "$spans" in 2*client*whoami*) break ;; esac
    sleep 2
  done
  case "$spans" in
    2*client.$NS*whoami.$NS*) ok "tracing: B3 context continued, client + server spans in Jaeger ($spans)" ;;
    *) bad "tracing: $spans" ;;
  esac
  kubectl -n "$NS" delete telemetry tracing >/dev/null
}

wasm() {
  log "wasm (WasmPlugin = Proxy-Wasm via ECDS)"
  # Istio のタスク（Wasm Plugin）の basic_auth を whoami のサイドカー（inbound）に入れる
  cat <<EOF | kubectl -n "$NS" apply -f - >/dev/null
apiVersion: extensions.istio.io/v1alpha1
kind: WasmPlugin
metadata: {name: basic-auth}
spec:
  selector: {matchLabels: {app: whoami}}
  url: oci://ghcr.io/istio-ecosystem/wasm-extensions/basic_auth:1.12.0
  phase: AUTHN
  pluginConfig:
    basic_auth_rules:
    - prefix: "/"
      request_methods: ["GET", "POST"]
      credentials: ["ok:test", "YWRtaW4zOmFkbWluMw=="]
EOF
  if retry 120 sh -c "kubectl -n $NS exec deploy/client -c curl -- curl -s -m 5 -o /dev/null -w '%{http_code}' http://whoami.$NS/ | grep -q 401"; then ok "WasmPlugin basic_auth: no credentials -> 401"; else bad "WasmPlugin: not enforced ($(cexec curl -s -m 5 -o /dev/null -w '%{http_code}' http://whoami."$NS"/))"; fi
  out="$(cexec curl -s -m 10 -o /dev/null -w '%{http_code}' -u ok:test http://whoami."$NS"/)" || true
  if [ "$out" = 200 ]; then ok "WasmPlugin basic_auth: valid credentials -> 200"; else bad "WasmPlugin valid credentials: $out"; fi
  out="$(cexec curl -s -m 10 -o /dev/null -w '%{http_code}' -u ok:wrong http://whoami."$NS"/)" || true
  if [ "$out" = 401 ]; then ok "WasmPlugin basic_auth: wrong credentials -> 401"; else bad "WasmPlugin wrong credentials: $out"; fi
  kubectl -n "$NS" delete wasmplugin basic-auth >/dev/null
  if retry 60 sh -c "kubectl -n $NS exec deploy/client -c curl -- curl -s -m 5 -o /dev/null -w '%{http_code}' http://whoami.$NS/ | grep -q 200"; then ok "WasmPlugin removed -> 200"; else bad "WasmPlugin removal not applied"; fi
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
  gateway) gateway ;;
  telemetry) telemetry ;;
  wasm) wasm ;;
  all) smoke; tasks; gateway; telemetry; wasm ;;
esac
cleanup
echo "== $MODE/$SUITE: pass=$PASS $([ $FAIL = 0 ] && echo ALL OK || echo FAILED)"
exit $FAIL
