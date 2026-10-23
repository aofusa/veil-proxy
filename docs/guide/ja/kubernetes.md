# Kubernetes（サイドカー、Istio / Linkerd）

veil はアプリと同じ Pod に置く **サイドカーコンテナ** として動き、Service の入口になる（F-196）。**Istio**・
**Linkerd** のメッシュ内でもそのまま動く。Pod 間はメッシュのプロキシが mTLS で暗号化し、veil は Pod 内の平文
HTTP/1.1・h2c を受ける。マニフェストは [`deploy/kubernetes/`](../../../deploy/kubernetes/README.md)。

```
client Pod ──(メッシュ mTLS)──▶ [istio-proxy / linkerd-proxy] ─▶ veil :8080 (HTTP/1.1 + h2c) ─▶ app 127.0.0.1:9000
                                                                veil :8443 (TLS、Secret の証明書)
```

## Helm

chart は veil を単独の Deployment と Service（ゲートウェイ・静的なサイト・リバースプロキシ）として入れます。サイドカーの
構成とメッシュには、下のマニフェストを使ってください。

```bash
helm install veil oci://ghcr.io/aofusa/charts/veil --version <版>
kubectl port-forward svc/veil 8080:8080
curl http://127.0.0.1:8080/        # イメージに入っているページ
```

既定の設定（values から ConfigMap に作る）は、イメージの `/var/www` を 8080 番の平文の HTTP/1.1・h2c で配ります。Pod は
非 root（65532）・読み取り専用のルート・全ケイパビリティを落とす・`RuntimeDefault` の seccomp で動き、veil が自分の seccomp と
Landlock を足します。設定を変えると Pod が入れ替わります（checksum の注釈）。chart はイメージと同じく cosign（keyless）で
署名しています。

| 値 | 既定 | |
|---|---|---|
| `image.repository` / `tag` / `digest` | `ghcr.io/aofusa/veil` / `musl-v<appVersion>` / — | `docker.io/aofusa/veil` も使える |
| `config` / `existingConfigMap` | — | 自分の `config.toml`（`--set-file config=config.toml`） |
| `tls.secretName` | — | `kubernetes.io/tls` の Secret。TLS のリスナー（8443 番、Service は `service.tlsPort`）を足す |
| `http3.enabled` | `false` | UDP 8443 の QUIC。別の Service `<release>-veil-h3`（`tls.secretName` が要る） |
| `metrics.enabled` / `metrics.serviceMonitor.enabled` | `false` | `/__metrics` と Prometheus Operator の ServiceMonitor |
| `grafanaDashboard.enabled` | `false` | [ダッシュボードの例](observability.md) を Grafana の sidecar 向けの ConfigMap にする |
| `extraVolumes` / `extraVolumeMounts` | — | 自分のファイル。例: `/var/www` にマウントした ConfigMap |
| `autoscaling` / `podDisruptionBudget` / `networkPolicy` | 無効 | |

すべての値の説明は [deploy/helm/veil/values.yaml](../../../deploy/helm/veil/values.yaml) にあります。Service に `appProtocol` は
付けません（下のメッシュの注意）。chart は kind で `tools/release/helm-kind-test.sh` を回して試験しています。

## イメージ

公開イメージ `ghcr.io/aofusa/veil:latest`（musl 版。`:glibc` も同等。どちらも `linux/amd64`・`linux/arm64`）はそのまま使える。
`full-container`（epoll の readiness リアクタ）のビルドなので、Pod の seccomp（`RuntimeDefault` は io_uring を禁じる）のまま動く
（[docker/README.md](../../../docker/README.md)）。

自分でビルドするときも同じ `full-container` でビルドする。
**musl 版をメイン**にし、glibc 版も同等に使える。

```bash
docker build -f docker/Dockerfile.musl  --build-arg CARGO_FEATURES=full-container -t veil:musl-k8s .
docker build -f docker/Dockerfile.glibc --build-arg CARGO_FEATURES=full-container -t veil:glibc-k8s .
```

## 構成

| パス | 内容 |
|------|------|
| `deploy/kubernetes/base` | Deployment（app + veil）・Service・VDS の参照制御プレーン・試験クライアント・`config.toml`・VDS のスナップショット |
| `deploy/kubernetes/overlays/plain` | メッシュ無し（名前空間 `veil-plain`） |
| `deploy/kubernetes/overlays/istio` | `istio-injection=enabled` の名前空間と STRICT の `PeerAuthentication`（`veil-istio`） |
| `deploy/kubernetes/overlays/linkerd` | `linkerd.io/inject: enabled` の名前空間（`veil-linkerd`） |

## Kubernetes での設定

| 項目 | 方法 |
|------|------|
| 設定ファイル | ConfigMap を `/etc/veil/conf.d/config.toml` にマウント |
| リスナー | `[server] listen = "0.0.0.0:8443"`（TLS）と `listen_cleartext = "0.0.0.0:8080"`（平文 HTTP/1.1 + h2c）。Pod 間はメッシュが暗号化するので `enable_secure_redirect = false`（[リスナー](configuration.md#リスナーlisten-と-listen_cleartext)） |
| 環境ごとの差分 | 環境変数 `VEIL_OVERRIDE_<n>`（[running.md](running.md#環境変数による設定の上書きveil_override_n)）。overlay は `VEIL_OVERRIDE_3='vds.enabled = true'` で VDS を有効にする |
| TLS 証明書 | Secret をディレクトリでマウント（`subPath` は使わない）し `[tls] auto_reload = true`。Secret の更新（`..data` の付け替え）に追従する（[tls.md](tls.md#コンテナでの証明書秘密鍵)） |
| 動的なルート | [VDS](vds.md)。`veil-vds-snapshot` ConfigMap を更新すると、kubelet が 1 分程度で制御プレーンへ同期し、制御プレーンが gRPC のストリームで veil へ押し出して veil が適用する |
| 停止 | SIGTERM で受付を止め、アイドルなキープアライブ接続はすぐ閉じ、残りを `graceful_shutdown_timeout_secs` 内に終える（`terminationGracePeriodSeconds` より短くする） |
| 堅牢化 | 非 root（65532）・読み取り専用ルート FS・全 capability を落とし・`RuntimeDefault` seccomp で動く。veil 自身の seccomp・Landlock も有効のまま |

## メッシュでの注意

- **Service のポートに `appProtocol` を付けない。** Linkerd は `appProtocol: http` を HTTP/1.1 固定と解釈するため、
  同じポートの h2c（HTTP/2 prior knowledge）が壊れる。付けなければ両メッシュともプロトコルを検出する
  （Istio はポート名の接頭辞 `http`・`https` も使う）。
- Istio 1.2x 以降と最近の Linkerd はプロキシを **ネイティブサイドカー**（`restartPolicy: Always` の `initContainers`）で注入する。
- メッシュが付ける身元のヘッダ（Istio の `X-Forwarded-Client-Cert`、Linkerd の `l5d-client-id`）を veil はアプリへ
  転送するので、アプリはワークロードの身元で認可できる。
- 8443（veil 自身の TLS）はメッシュからは不透明な TLS。メッシュ外のクライアント向けに使う。
- アプリのコンテナは自分で SIGTERM を処理すること。処理しないと veil が止まっていても Pod は猶予いっぱい待つ
  （デモの `traefik/whoami` は PID 1 で SIGTERM を無視する）。

## E2E 試験（kind）

```bash
tools/k8s/kind-e2e.sh musl all     # plain・Istio・Linkerd
tools/k8s/kind-e2e.sh glibc all
```

kind のクラスタを作り、イメージを読み込み、メッシュを導入して overlay をデプロイし、疎通（HTTP/1.1・h2c）、mTLS の
証拠（Istio の XFCC の SPIFFE ID・Linkerd の `l5d-client-id`）、メッシュ外クライアントの拒否（Istio STRICT）、
Secret の証明書での TLS と Secret 更新での差し替え、`VEIL_OVERRIDE_<n>`、VDS でのルート追加（メッシュ越しの gRPC ストリーム、制御プレーン再起動後の再接続）、SIGTERM 後の
graceful shutdown を確かめる。docker・kind・kubectl・istioctl・linkerd・openssl が必要。snap 版 docker は `/tmp` を
読めないため、一時ファイルは `target/tmp` に置く。

## veil をメッシュのプロキシにする

上の構成ではメッシュのプロキシ（Istio は Envoy、Linkerd は linkerd2-proxy）が veil の隣に残ります。veil はそのプロキシ
自体も**置き換えられます**（サイドカー、Istio のゲートウェイと ambient の waypoint、Linkerd のサイドカー）。istiod・
Linkerd の制御プレーンは無改造のまま、veil と本家のプロキシが同じメッシュで相互に通信します。
[サービスメッシュのデータプレーン](mesh.md)を参照してください。
