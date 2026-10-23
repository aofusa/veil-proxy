# F-254: Helm chart

**状態: 実装済み（feat/distribution-tickets、2026-10-23。v0.9.0 で公開）**

## 目的

- `helm install veil oci://ghcr.io/aofusa/charts/veil` だけで、veil が Kubernetes で動くようにする。
  - 動かし方は、既定の設定の Deployment + Service。
  - `kubectl port-forward svc/veil 8080:8080` → `curl http://127.0.0.1:8080/` で、イメージに入っている既定のページが返る（公開イメージの既定と同じ。F-246）。
- values で、次のことを変えられるようにする。
  - 自分の設定（TOML）・ページ・TLS
  - HTTP/3（UDP）
  - メトリクス（ServiceMonitor）・Grafana のダッシュボード（F-256）
  - 台数・資源

## 今の状態

- `deploy/kubernetes/` に kustomize がある（F-196・F-215）。
  - `base/`: アプリ + veil のサイドカー、VDS。
  - `overlays/`: `plain`・`istio`・`linkerd`。
  - 文書は `docs/guide/kubernetes.md`・`mesh.md`。
- **securityContext:** 今の manifest は `runAsNonRoot: true`（65532）・`seccompProfile: RuntimeDefault`・`readOnlyRootFilesystem`・`capabilities: drop [ALL]`。
- **イメージの既定:** 公開イメージの既定の設定（`packaging/container/config.toml`）は、root で起動して `nonroot` に降格する前提。
  `runAsNonRoot` で始めたときに、降格の設定がどう振る舞うかは確かめる（失敗するなら chart の既定の設定から降格を外す）。
- Helm chart は無い。

## 設計

### chart の作り

- **置き場所:** `deploy/helm/veil/`。kustomize は残す（サイドカー・メッシュの例として）。
  - chart は「veil を 1 つのサービス（ゲートウェイ・静的配信・リバースプロキシ）として置く」形を主にする。
  - サイドカーは `kubernetes.md` の kustomize へ案内する。
- **`Chart.yaml`:** `version` は chart の版。`appVersion` は veil の版。veil のリリースと同時に上げる（同じ版にする）。
- **templates:**
  - `deployment.yaml`、`service.yaml`、`configmap.yaml`（設定）
  - `hpa.yaml`・`pdb.yaml`・`servicemonitor.yaml`・`networkpolicy.yaml`（どれも既定は無効）
  - `grafana-dashboard-configmap.yaml`（既定は無効。F-256）
  - `NOTES.txt`（port-forward と curl の手順）
- **values の主なもの:**
  - `image.repository`: 既定は `ghcr.io/aofusa/veil`。Docker Hub（F-248）にも変えられる。
  - `image.tag`: 既定は `appVersion`。`image.digest` も指定できる。
  - `config`: TOML の文字列。既定は公開イメージの既定と同じ内容。
    `existingConfigMap` も指定できる。
  - 設定のハッシュを Pod の注釈に入れ、設定を変えたら入れ替える。
    SIGHUP でのリロードは使わない（ConfigMap の反映の遅れと混ざるため）。
  - `extraVolumes`・`extraVolumeMounts`: ページ・証明書（Secret）。
  - `tls.secretName`: `listen` の TLS と HTTP/3 を有効にする。
  - `service.type`・`service.ports`: 既定は ClusterIP の `8080/TCP`。HTTP/3 では、同じポートの UDP を足す。
    LoadBalancer で TCP と UDP を混ぜるときの注意を書く。
  - `metrics.enabled`: `[prometheus]` を有効にし、ServiceMonitor を作る。
  - `resources`・`replicaCount`・`autoscaling`・`podDisruptionBudget`・`nodeSelector`・`tolerations`・`affinity`。
  - `securityContext`: 今の manifest と同じ。
    - `runAsNonRoot`・65532、`RuntimeDefault`、`readOnlyRootFilesystem`、`drop [ALL]`、`allowPrivilegeEscalation: false`。
    - 書き込みの要るパス（キャッシュ・一時）は `emptyDir`。
- **probe:** `tcpSocket` の 8080。HTTP の probe は、既定の設定の `/` に向ける。
  - 管理用の API（`[admin]`）を有効にしたときは、そちらを使えるようにする。
- **Service に `appProtocol` を付けない**（Linkerd が h2c を壊す。F-196・AGENTS.md）。
- **io_uring:** 公開イメージは `full-container`（epoll）なので、`RuntimeDefault` の seccomp で動く。

### 公開

- `release.yml` に `helm` のジョブを足す（`image-publish` の後）。
  1. `helm package` → `helm push oci://ghcr.io/aofusa/charts`。
  2. cosign で chart の OCI の成果物に署名する。
- rc では、版のタグだけを push する（`helm install --version <rc>` で試せる）。
- Artifact Hub への登録（`artifacthub-repo.yml`）は任意。登録するなら、署名の情報も載せる。

### 試験

- **静的な検査:**
  - `helm lint --strict`
  - `helm template` を kubeconform で検査する（k8s の 3 つの minor 版、CRD の schema 込み）。
  - values の組み合わせ（既定・TLS・HTTP/3・metrics・existingConfigMap・HPA）を `ct lint` で回す。
- **kind での試験（CI）:** `ct install` で、次のことを確かめる。
  - 既定の values: port-forward で `/`・`/quickstart.html`・h2c が 200。
  - TLS の values: 自己署名の Secret で HTTPS・HTTP/2 が 200。
  - metrics の values: `/__metrics` が返り、ServiceMonitor が作られる（Prometheus Operator の CRD を入れる）。
  - 設定を変えて `helm upgrade` すると、Pod が入れ替わり、新しい設定で返る。
  - Pod の securityContext が上のとおり（`kubectl get pod -o json` で確かめる）。
- kind での偽の失敗（並行ビルド・CRD 名など）は、メモリの記録の手順に従う。

### 文書

- `docs/guide/kubernetes.md`（英日）に「Helm」の節を足す。
  - 入れ方、主な values、TLS・HTTP/3・metrics・ダッシュボードの例。
  - kustomize（サイドカー・メッシュ）との使い分け。
- `deploy/helm/veil/README.md`: values の表（`helm-docs` で作る）。

## 受け入れ条件

- `helm install veil oci://ghcr.io/aofusa/charts/veil` → port-forward → `curl http://127.0.0.1:8080/` が既定のページを返す。
- 上の試験が CI（rc のタグを含む）で通る。chart に署名があり、検証できる。

## 依存・リスク

- F-246（イメージの既定）は済み。Docker Hub（F-248）・ダッシュボード（F-256）は任意の値として後から足せる。
- 非 root で始めたときの降格の設定の挙動を、実装の最初に確かめる。

## 関連

- [F-196](F-196-kubernetes-sidecar-mesh.md)
- [F-215](F-215-service-mesh-dataplane.md)
- [F-246](F-246-container-try-it-defaults.md)
- [F-256](F-256-grafana-dashboard.md)
