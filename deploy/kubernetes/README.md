# veil を Kubernetes のサイドカーとして動かす（F-196）

アプリのコンテナ（例: `traefik/whoami` を 127.0.0.1:9000）と同じ Pod に veil を置き、Service の入口にする構成。
Istio・Linkerd のメッシュ内でもそのまま動く（メッシュのプロキシが Pod 間を mTLS で暗号化し、veil は Pod 内の
平文 HTTP/1.1・h2c を受ける）。詳しくは [docs/guide/kubernetes.md](../../docs/guide/kubernetes.md)。

```
client Pod ──(mesh mTLS)──▶ [istio-proxy / linkerd-proxy] ─▶ veil :8080 (HTTP/1.1 + h2c) ─▶ app 127.0.0.1:9000
                                                             veil :8443 (TLS, Secret の証明書)
```

| パス | 内容 |
|------|------|
| `base/` | Deployment（app + veil）・Service・VDS の参照制御プレーン・試験用クライアント・`config.toml`・VDS のスナップショット |
| `overlays/plain` | メッシュ無し（名前空間 `veil-plain`） |
| `overlays/istio` | `istio-injection=enabled` の名前空間 + `PeerAuthentication` STRICT（`veil-istio`） |
| `overlays/linkerd` | `linkerd.io/inject: enabled` の名前空間（`veil-linkerd`） |

各 overlay は `VEIL_OVERRIDE_3='vds.enabled = true'`（F-179）で VDS（F-04）を有効にする。veil は制御プレーン
（`tools/vds/server`）を gRPC の双方向ストリームで購読する（F-199）。

## 前提として作るもの（リポジトリに置かない）

```bash
NS=veil-istio
kubectl apply -f overlays/istio/namespace.yaml
kubectl -n $NS create secret tls veil-tls --cert=tls.crt --key=tls.key          # veil の TLS（F-180）
kubectl -n $NS create secret generic veil-vds-token --from-literal=token=$(openssl rand -hex 16)
kubectl apply -k overlays/istio
```

イメージは `veil:musl-k8s`（`docker build -f docker/Dockerfile.musl --build-arg CARGO_FEATURES=full-container -t veil:musl-k8s .`）。
glibc 版を使う場合は `docker/Dockerfile.glibc` でビルドし、イメージ名を差し替える。VDS の制御プレーンは
`veil-vds-server:latest`（`docker build -f docker/Dockerfile.vds-server -t veil-vds-server:latest .`）。

## 運用のポイント

- 設定は ConfigMap（`config.toml`）。環境ごとの差分は `VEIL_OVERRIDE_<n>` で入れる（F-179）。
- 証明書は Secret をディレクトリでマウントする（`subPath` は更新が届かない）。`[tls] auto_reload = true` で
  Secret の更新（`..data` の付け替え）に追従する（F-180）。
- ルートは VDS で配信できる（`veil-vds-snapshot` ConfigMap を更新すると数十秒で反映、F-04）。
- `terminationGracePeriodSeconds` 内に graceful shutdown する（SIGTERM で新規受付を止め、アイドルな接続は
  すぐ閉じる。F-187）。
- ルートファイルシステムは読み取り専用・非 root（65532）・全 capability を落としても動く。

## 試験

`tools/k8s/kind-e2e.sh <musl|glibc> <plain|istio|linkerd|all>` が kind のクラスタで一連の確認を行う。
