#!/usr/bin/env bash
# F-254: Helm chart の静的な検査。lint、主な values の組み合わせの helm template を kubeconform で検査し、
# 生成した config.toml を veil -t に通す。
#
#   tools/release/helm-check.sh <veil-image>
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
IMAGE="${1:?veil image (for veil -t)}"
CHART="$ROOT/deploy/helm/veil"
W="$(mktemp -d "${HOME}/veil-helm.XXXXXX")"; trap 'rm -rf "$W"' EXIT
FAILS=0
ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; echo "::error title=helm::$*"; FAILS=$((FAILS+1)); }

# ダッシュボードは examples/grafana の写し（chart の外のファイルは読めない）
cmp -s "$ROOT/examples/grafana/veil-overview.json" "$CHART/dashboards/veil-overview.json" \
    && ok "dashboards/veil-overview.json matches examples/grafana" || bad "deploy/helm/veil/dashboards/veil-overview.json differs from examples/grafana/veil-overview.json"

helm lint --strict "$CHART" >/dev/null && ok "helm lint --strict" || bad "helm lint"

declare -A SETS=(
  [default]=""
  [tls]="--set tls.secretName=veil-tls"
  [tls-http3]="--set tls.secretName=veil-tls --set http3.enabled=true"
  [metrics]="--set metrics.enabled=true --set metrics.serviceMonitor.enabled=true --set grafanaDashboard.enabled=true"
  [scale]="--set autoscaling.enabled=true --set podDisruptionBudget.enabled=true --set networkPolicy.enabled=true"
)
for name in "${!SETS[@]}"; do
    # shellcheck disable=SC2086
    if ! helm template t "$CHART" ${SETS[$name]} > "$W/$name.yaml" 2>"$W/$name.err"; then bad "$name: helm template: $(cat "$W/$name.err")"; continue; fi
    if kubeconform -strict -summary -ignore-missing-schemas -kubernetes-version 1.31.0 "$W/$name.yaml" >"$W/$name.kc" 2>&1; then ok "$name: kubeconform"
    else bad "$name: kubeconform: $(tail -3 "$W/$name.kc")"; fi
    # 設定を取り出して veil -t（TLS の証明書は自己署名のものを置いて検査する）
    python3 - "$W/$name.yaml" "$W/$name.toml" <<'PY'
import sys, yaml
for d in yaml.safe_load_all(open(sys.argv[1])):
    if d and d.get("kind") == "ConfigMap" and "config.toml" in d.get("data", {}):
        open(sys.argv[2], "w").write(d["data"]["config.toml"])
PY
    mkdir -p "$W/tls"; [ -f "$W/tls/tls.crt" ] || openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
        -keyout "$W/tls/tls.key" -out "$W/tls/tls.crt" -days 1 -subj /CN=veil >/dev/null 2>&1
    chmod 644 "$W/tls/"* "$W/$name.toml"
    if docker run --rm -v "$W/$name.toml:/etc/veil/conf.d/config.toml:ro" -v "$W/tls:/etc/veil/tls:ro" "$IMAGE" \
        /veil -t -c /etc/veil/conf.d/config.toml >"$W/$name.t" 2>&1; then ok "$name: veil -t"
    else bad "$name: veil -t: $(tail -3 "$W/$name.t")"; fi
done
[ "$FAILS" = 0 ] && echo "HELM CHECK: ALL OK" || { echo "HELM CHECK: $FAILS failure(s)"; exit 1; }
