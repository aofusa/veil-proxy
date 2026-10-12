#!/usr/bin/env bash
# Kubernetes サイドカーの E2E（F-196）。kind のクラスタに veil をサイドカーとしてデプロイし、
# Istio / Linkerd のメッシュ内で動くことを確かめる。
#
#   tools/k8s/kind-e2e.sh <musl|glibc> <plain|istio|linkerd|all> [--keep]
#
# 使うイメージ: veil:<variant>-k8s（docker/Dockerfile.<variant> を CARGO_FEATURES=full-container でビルド）。
# 検査項目:
#   1. クライアント → (メッシュ mTLS) → veil → アプリ の疎通（HTTP/1.1・h2c）
#   2. mTLS の証拠（Istio: X-Forwarded-Client-Cert の SPIFFE ID、Linkerd: l5d-client-id）
#   3. メッシュ外からの拒否（Istio の STRICT mTLS）
#   4. veil 自身の TLS（Secret の証明書）と Secret 更新での差し替え（F-180）
#   5. 環境変数 VEIL_OVERRIDE_<n> の反映（F-179）
#   6. VDS でのルート追加（F-04）
#   7. Pod 削除時の graceful shutdown（猶予内に終了）
# 前提: docker・kind・kubectl・istioctl・linkerd・openssl（~/.local/bin でも可）。
set -uo pipefail
export PATH="$HOME/.local/bin:$PATH"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
# kind load は docker save を TMPDIR に書く。snap 版 docker は /tmp に書けないので target/ 配下にする
mkdir -p "$ROOT/target/tmp"
export TMPDIR="$ROOT/target/tmp"
VARIANT="${1:-musl}"
MESH="${2:-all}"
KEEP="${3:-}"
CLUSTER="${KIND_CLUSTER:-veil-e2e}"
IMAGE="veil:${VARIANT}-k8s"
FAIL=0
WORK="$(mktemp -d -p "$ROOT/target/tmp" 2>/dev/null || mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

ok()   { echo "  ok   $*"; }
bad()  { echo "  FAIL $*"; FAIL=1; }
log()  { echo "== $*"; }
retry() { # retry <seconds> <command...>  コマンドが成功するまで繰り返す
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
  for img in "$IMAGE" traefik/whoami:latest python:3.12-alpine curlimages/curl:latest; do
    docker image inspect "$img" >/dev/null 2>&1 || docker pull -q "$img" >/dev/null
    kind load docker-image --name "$CLUSTER" "$img" >/dev/null 2>&1 || { echo "failed to load $img"; exit 1; }
  done
}

install_mesh() {
  case "$1" in
    istio)
      if ! kubectl get ns istio-system >/dev/null 2>&1; then
        log "Istio を導入"
        istioctl install --set profile=minimal -y >/dev/null || exit 1
      fi
      kubectl -n istio-system rollout status deploy/istiod --timeout=300s >/dev/null ;;
    linkerd)
      if ! kubectl get ns linkerd >/dev/null 2>&1; then
        log "Linkerd を導入"
        # 近年の Linkerd は Gateway API の CRD を前提にする
        kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.2.1/standard-install.yaml >/dev/null
        linkerd install --crds | kubectl apply -f - >/dev/null || exit 1
        linkerd install | kubectl apply -f - >/dev/null || exit 1
      fi
      linkerd check --wait 300s >/dev/null || { linkerd check; exit 1; } ;;
  esac
}

gen_cert() { # gen_cert <cn> → $WORK/<cn>.{crt,key}
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 7 \
    -subj "/CN=$1" -addext "subjectAltName=DNS:veil-app,DNS:localhost" \
    -keyout "$WORK/$1.key" -out "$WORK/$1.crt" 2>/dev/null
}

deploy() { # deploy <overlay> <namespace>
  local ov="$1" ns="$2"
  kubectl delete ns "$ns" --wait=true --ignore-not-found >/dev/null
  kubectl apply -f "$ROOT/deploy/kubernetes/overlays/$ov/namespace.yaml" >/dev/null
  retry 60 kubectl -n "$ns" get serviceaccount default || return 1
  gen_cert v1.veil.test
  kubectl -n "$ns" create secret tls veil-tls --cert="$WORK/v1.veil.test.crt" --key="$WORK/v1.veil.test.key" >/dev/null
  kubectl -n "$ns" create secret generic veil-vds-token --from-literal=token="$(openssl rand -hex 16)" >/dev/null
  kubectl -n "$ns" create configmap veil-vds-server --from-file=vds_server.py="$ROOT/tools/vds/vds_server.py" >/dev/null
  kubectl kustomize "$ROOT/deploy/kubernetes/overlays/$ov" | sed "s#image: veil:musl-k8s#image: $IMAGE#" \
    | kubectl apply -f - >/dev/null || return 1
  kubectl -n "$ns" rollout status deploy/veil-vds --timeout=300s >/dev/null &&
  kubectl -n "$ns" rollout status deploy/veil-app --timeout=300s >/dev/null &&
  kubectl -n "$ns" wait --for=condition=Ready pod/client --timeout=300s >/dev/null
}

kc() { kubectl -n "$NS" exec client -c curl -- "$@"; }

run_mesh() {
  local mesh="$1"
  NS="veil-$mesh"
  log "[$VARIANT/$mesh] デプロイ"
  install_mesh "$mesh"
  deploy "$mesh" "$NS" || { bad "deploy"; kubectl -n "$NS" get pods; kubectl -n "$NS" describe pods | tail -40; return; }
  # 近年のメッシュはネイティブサイドカー（restartPolicy: Always の initContainers）として注入するので両方を見る
  local containers; containers=$(kubectl -n "$NS" get pod -l app=veil-app \
    -o jsonpath='{.items[0].spec.initContainers[*].name} {.items[0].spec.containers[*].name}')
  echo "  pod containers: $containers"
  case "$mesh" in
    istio) [[ "$containers" == *istio-proxy* ]] && ok "istio-proxy injected next to veil" || bad "istio-proxy not injected" ;;
    linkerd) [[ "$containers" == *linkerd-proxy* ]] && ok "linkerd-proxy injected next to veil" || bad "linkerd-proxy not injected" ;;
  esac

  # 1. 疎通
  local out; out=$(kc curl -s -m 10 http://veil-app/)
  [[ "$out" == *Hostname* ]] && ok "client -> veil -> app (HTTP/1.1)" || bad "HTTP/1.1: $out"
  out=$(kc curl -s -m 10 --http2-prior-knowledge http://veil-app/ -o /dev/null -w '%{http_version} %{http_code}')
  [[ "$out" == "2 200" ]] && ok "h2c through the mesh ($out)" || bad "h2c: $out"
  # 2. mTLS の証拠（メッシュのプロキシが付けたヘッダを veil がアプリへ転送している）
  case "$mesh" in
    istio) [[ "$(kc curl -s -m 10 http://veil-app/)" == *"X-Forwarded-Client-Cert: By=spiffe://"* ]] \
             && ok "mTLS: X-Forwarded-Client-Cert carries the SPIFFE identity" || bad "no XFCC header" ;;
    linkerd) out=$(kc curl -s -m 10 http://veil-app/)
             [[ "$out" == *"L5d-Client-Id: default.$NS.serviceaccount.identity.linkerd.cluster.local"* ]] \
             && ok "mTLS: l5d-client-id identifies the meshed client" || bad "no l5d-client-id: $(echo "$out" | grep -i l5d)" ;;
  esac
  # 3. メッシュ外からの拒否（Istio STRICT）
  if [ "$mesh" = istio ]; then
    kubectl delete pod outsider -n default --ignore-not-found >/dev/null
    out=$(kubectl run outsider -n default --rm -i --restart=Never --image=curlimages/curl:latest --image-pull-policy=IfNotPresent \
          -- curl -s -m 5 -o /dev/null -w '%{http_code}' "http://veil-app.$NS/" 2>/dev/null)
    [[ "$out" != *200* ]] && ok "STRICT mTLS rejects a client outside the mesh ($out)" || bad "outside client got 200"
  fi
  # 4. veil 自身の TLS と Secret 更新（F-180）
  subj() { kc sh -c "curl -skv -m 10 https://veil-app:443/ -o /dev/null 2>&1 | grep -i 'subject:'"; }
  out=$(subj); [[ "$out" == *v1.veil.test* ]] && ok "TLS served from the Secret (CN=v1)" || bad "TLS v1: $out"
  gen_cert v2.veil.test
  kubectl -n "$NS" create secret tls veil-tls --cert="$WORK/v2.veil.test.crt" --key="$WORK/v2.veil.test.key" \
    --dry-run=client -o yaml | kubectl apply -f - >/dev/null
  if retry 180 bash -c "kubectl -n $NS exec client -c curl -- sh -c \"curl -skv -m 10 https://veil-app:443/ -o /dev/null 2>&1 | grep -q 'CN=v2.veil.test'\""; then
    ok "Secret update hot-reloaded (CN=v2, ..data symlink swap)"
  else bad "certificate not reloaded after Secret update: $(subj)"; fi
  # 5. 環境変数の上書き（F-179）
  # config.toml は threads = 1。VEIL_OVERRIDE_2 で 2 にしている（pipefail 下で grep -q を使うと SIGPIPE で誤判定するので変数に取る）
  local vlog; vlog=$(kubectl -n "$NS" logs deploy/veil-app -c veil 2>/dev/null)
  [[ "$vlog" == *"Workers: 2"* ]] && ok "VEIL_OVERRIDE_2 applied (threads 1 -> 2)" || bad "VEIL_OVERRIDE not applied"
  # 6. VDS でルートを追加（F-04）
  cat > "$WORK/snapshot.toml" <<'TOML'
[[route]]
[route.conditions]
path = "/vds-added"
[route.action]
type = "Redirect"
redirect_url = "https://example.com/from-vds"
redirect_status = 302

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:9000"
TOML
  kubectl -n "$NS" create configmap veil-vds-snapshot --from-file=snapshot.toml="$WORK/snapshot.toml" \
    --dry-run=client -o yaml | kubectl apply -f - >/dev/null
  if retry 240 bash -c "kubectl -n $NS exec client -c curl -- curl -s -m 10 -o /dev/null -w '%{redirect_url}' http://veil-app/vds-added | grep -q from-vds"; then
    ok "VDS delivered a new route through the mesh"
  else bad "VDS route not applied"; kubectl -n "$NS" logs deploy/veil-app -c veil | grep -i vds | tail -5; fi
  # 7. graceful shutdown: veil が SIGTERM を受けてから止まるまでの時間をログの時刻で測る
  #    （Pod 全体の終了時間はアプリ側のコンテナにも左右される。デモの whoami は PID 1 で SIGTERM を
  #    無視するため猶予いっぱいかかる）
  local pod; pod=$(kubectl -n "$NS" get pod -l app=veil-app -o jsonpath='{.items[0].metadata.name}')
  kubectl -n "$NS" logs -f "$pod" -c veil > "$WORK/shutdown.log" 2>/dev/null &
  local lp=$!
  sleep 1
  kubectl -n "$NS" delete pod "$pod" --wait=true --timeout=90s >/dev/null
  kill $lp 2>/dev/null
  local dt; dt=$(python3 - "$WORK/shutdown.log" <<'PY'
import re, sys, datetime
t = {}
for line in open(sys.argv[1], errors="replace"):
    m = re.match(r"(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d\.\d+)", line)
    if not m:
        continue
    ts = datetime.datetime.strptime(m.group(1)[:23], "%Y-%m-%d %H:%M:%S.%f")
    if "Received shutdown signal" in line:
        t["start"] = ts
    if "Server shutdown complete" in line:
        t["end"] = ts
print(f"{(t['end'] - t['start']).total_seconds():.1f}" if len(t) == 2 else "none")
PY
)
  if [[ "$dt" != none ]] && python3 -c "import sys; sys.exit(0 if float('$dt') < 10 else 1)"; then
    ok "veil shut down gracefully ${dt}s after SIGTERM"
  else bad "veil graceful shutdown: $dt"; fi
  kubectl -n "$NS" rollout status deploy/veil-app --timeout=300s >/dev/null && ok "replacement pod ready" || bad "replacement pod"
  [ "$KEEP" = "--keep" ] || kubectl delete ns "$NS" --wait=false >/dev/null
}

ensure_cluster
case "$MESH" in
  all) for m in plain istio linkerd; do run_mesh "$m"; done ;;
  plain|istio|linkerd) run_mesh "$MESH" ;;
  *) echo "usage: $0 <musl|glibc> <plain|istio|linkerd|all> [--keep]"; exit 2 ;;
esac
[ "$FAIL" = 0 ] && echo "ALL OK ($VARIANT/$MESH)" || { echo "SOME CHECKS FAILED ($VARIANT/$MESH)"; exit 1; }
