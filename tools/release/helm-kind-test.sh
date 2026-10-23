#!/usr/bin/env bash
# F-254: kind で Helm chart を入れて試験する。
#
#   tools/release/helm-kind-test.sh <veil-image>        # 例: ghcr.io/aofusa/veil:musl-v0.9.0
#
#   1. 既定の values: helm test、port-forward で / と h2c が 200、アクセスログ、Pod の securityContext
#   2. TLS・HTTP/3・metrics・ServiceMonitor・ダッシュボードの values で upgrade: Pod が入れ替わり、HTTPS・/__metrics が返る
#   3. 自分の設定（--set-file config）で upgrade: 新しい設定で返る
set -uo pipefail
IMAGE="${1:?image}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CHART="$ROOT/deploy/helm/veil"
CL="veil-helm-$$"
W="$(mktemp -d "${HOME}/veil-kind.XXXXXX")"
FAILS=0
ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; echo "::error title=helm kind test::$*"; FAILS=$((FAILS+1)); }
cleanup() { [ -n "${PF:-}" ] && kill "$PF" 2>/dev/null; kind delete cluster --name "$CL" >/dev/null 2>&1; rm -rf "$W"; }
trap cleanup EXIT
repo="${IMAGE%:*}"; tag="${IMAGE##*:}"

kind create cluster --name "$CL" --wait 120s >/dev/null || { echo "kind create failed"; exit 1; }
# kind load docker-image は /tmp に書き出す（snap の docker は /tmp を見られない）。$HOME の下のファイルから入れる
docker save "$IMAGE" -o "$W/image.tar" && kind load image-archive "$W/image.tar" --name "$CL" >/dev/null \
    || { echo "kind load failed"; exit 1; }
kubectl apply -f https://raw.githubusercontent.com/prometheus-operator/prometheus-operator/v0.86.0/example/prometheus-operator-crd/monitoring.coreos.com_servicemonitors.yaml >/dev/null

pf() { # port-forward を張り直す
    [ -n "${PF:-}" ] && kill "$PF" 2>/dev/null; sleep 1
    kubectl port-forward svc/veil "$@" >/dev/null 2>&1 & PF=$!
    for _ in $(seq 1 30); do curl -s -o /dev/null http://127.0.0.1:18080/ && return 0; sleep 1; done
}
H=(--set "image.repository=$repo" --set "image.tag=$tag" --set image.pullPolicy=Never)

# 1. 既定
helm install veil "$CHART" "${H[@]}" --wait --timeout 180s >/dev/null && ok "install (default values)" || { bad "install"; kubectl describe pod -l app.kubernetes.io/instance=veil | tail -30; kubectl logs -l app.kubernetes.io/instance=veil --tail 30; }
helm test veil --timeout 120s >/dev/null && ok "helm test" || { bad "helm test"; kubectl logs veil-test 2>/dev/null | tail; }
pf 18080:8080
[ "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:18080/)" = 200 ] && ok "GET / via port-forward" || bad "GET /"
curl -s http://127.0.0.1:18080/quickstart.html | grep -q '<title>veil quickstart</title>\|quickstart' && ok "GET /quickstart.html" || bad "GET /quickstart.html"
[ "$(curl -s --http2-prior-knowledge -o /dev/null -w '%{http_version}' http://127.0.0.1:18080/)" = 2 ] && ok "h2c" || bad "h2c"
seen=0; for _ in $(seq 1 15); do kubectl logs deploy/veil | grep -q 'type=access' && { seen=1; break; }; sleep 1; done
[ "$seen" = 1 ] && ok "access log in kubectl logs" || bad "no access log"
sc="$(kubectl get pod -l app.kubernetes.io/instance=veil -o jsonpath='{.items[0].spec.securityContext.runAsNonRoot} {.items[0].spec.securityContext.seccompProfile.type} {.items[0].spec.containers[0].securityContext.readOnlyRootFilesystem} {.items[0].spec.containers[0].securityContext.capabilities.drop[0]}')"
[ "$sc" = "true RuntimeDefault true ALL" ] && ok "securityContext: $sc" || bad "securityContext: $sc"
kubectl get svc veil -o jsonpath='{.spec.ports[*].appProtocol}' | grep -q . && bad "Service has appProtocol" || ok "Service has no appProtocol"

# 2. TLS・HTTP/3・metrics
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout "$W/k" -out "$W/c" -days 1 -subj /CN=veil \
    -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" >/dev/null 2>&1
kubectl create secret tls veil-tls --cert "$W/c" --key "$W/k" >/dev/null
old="$(kubectl get pod -l app.kubernetes.io/instance=veil -o jsonpath='{.items[0].metadata.name}')"
helm upgrade veil "$CHART" "${H[@]}" --set tls.secretName=veil-tls --set http3.enabled=true --set metrics.enabled=true \
    --set metrics.serviceMonitor.enabled=true --set grafanaDashboard.enabled=true --wait --timeout 180s >/dev/null \
    && ok "upgrade (tls, http3, metrics)" || bad "upgrade (tls, http3, metrics)"
kubectl rollout status deploy/veil --timeout 120s >/dev/null
kubectl wait --for=delete "pod/$old" --timeout=90s >/dev/null 2>&1 && ok "the pod was replaced on a configuration change" || bad "the pod was not replaced"
pf 18080:8080 18443:443
[ "$(curl -sk -o /dev/null -w '%{http_code} %{http_version}' https://127.0.0.1:18443/)" = "200 2" ] && ok "HTTPS + HTTP/2 via the Service" || bad "HTTPS"
curl -s http://127.0.0.1:18080/__metrics | grep -q '^veil_proxy_http_requests_total' && ok "/__metrics" || bad "/__metrics"
kubectl get servicemonitor veil >/dev/null 2>&1 && ok "ServiceMonitor created" || bad "no ServiceMonitor"
kubectl get configmap veil-dashboard >/dev/null 2>&1 && ok "dashboard ConfigMap created" || bad "no dashboard ConfigMap"
[ "$(kubectl get svc veil-h3 -o jsonpath='{.spec.ports[0].protocol}/{.spec.ports[0].port}')" = "UDP/443" ] && ok "Service veil-h3 has UDP 443" || bad "no veil-h3 UDP Service"

# 3. 自分の設定
cat > "$W/my.toml" <<'T'
[server]
listen_cleartext = "0.0.0.0:8080"
enable_secure_redirect = false
[[route]]
[route.conditions]
path = "/"
[route.action]
type = "Redirect"
redirect_url = "https://example.com/"
redirect_status = 302
T
helm upgrade veil "$CHART" "${H[@]}" --set-file config="$W/my.toml" --wait --timeout 180s >/dev/null && ok "upgrade (own config)" || bad "upgrade (own config)"
kubectl rollout status deploy/veil --timeout 120s >/dev/null
pf 18080:8080
[ "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:18080/)" = 302 ] && ok "own config is served" || bad "own config not served"

[ "$FAILS" = 0 ] && echo "HELM KIND TEST: ALL OK" || { echo "HELM KIND TEST: $FAILS failure(s)"; exit 1; }
