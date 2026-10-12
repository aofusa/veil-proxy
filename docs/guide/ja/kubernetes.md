# Kubernetes（サイドカー、Istio / Linkerd）

veil はアプリと同じ Pod に置く **サイドカーコンテナ** として動き、Service の入口になる（F-196）。**Istio**・
**Linkerd** のメッシュ内でもそのまま動く。Pod 間はメッシュのプロキシが mTLS で暗号化し、veil は Pod 内の平文
HTTP/1.1・h2c を受ける。マニフェストは [`deploy/kubernetes/`](../../../deploy/kubernetes/README.md)。

```
client Pod ──(メッシュ mTLS)──▶ [istio-proxy / linkerd-proxy] ─▶ veil :8080 (HTTP/1.1 + h2c) ─▶ app 127.0.0.1:9000
                                                                veil :8443 (TLS、Secret の証明書)
```

## イメージ

`full-container`（epoll の readiness リアクタ。コンテナ既定の seccomp プロファイルで動く）でビルドする。
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
| 環境ごとの差分 | 環境変数 `VEIL_OVERRIDE_<n>`（[running.md](running.md#環境変数による設定の上書きveil_override_n)）。overlay は `VEIL_OVERRIDE_3='vds.enabled = true'` で VDS を有効にする |
| TLS 証明書 | Secret をディレクトリでマウント（`subPath` は使わない）し `[tls] auto_reload = true`。Secret の更新（`..data` の付け替え）に追従する（[tls.md](tls.md#コンテナでの証明書秘密鍵)） |
| 動的なルート | [VDS](vds.md)。`veil-vds-snapshot` ConfigMap を更新すると、kubelet が 1 分程度で制御プレーンへ同期し、veil が適用する |
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
Secret の証明書での TLS と Secret 更新での差し替え、`VEIL_OVERRIDE_<n>`、VDS でのルート追加、SIGTERM 後の
graceful shutdown を確かめる。docker・kind・kubectl・istioctl・linkerd・openssl が必要。snap 版 docker は `/tmp` を
読めないため、一時ファイルは `target/tmp` に置く。
