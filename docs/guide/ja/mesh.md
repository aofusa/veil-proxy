# サービスメッシュのデータプレーン（Envoy・linkerd2-proxy の代わりに veil）

[← ドキュメント一覧](README.md) · [English](../mesh.md)

veil は**メッシュのプロキシそのもの**を置き換えられる。Istio が注入する Envoy（サイドカー・ingress / egress
ゲートウェイ・ambient の waypoint）と、Linkerd が注入する linkerd2-proxy。制御プレーン（istiod、Linkerd の
destination・identity・policy）は無改造のまま使い、veil の Pod と本家のプロキシの Pod が同じメッシュで相互に
通信する（F-215〜F-225）。メッシュのプロキシを残して[その横で veil を動かす構成](kubernetes.md)とは別のもの。

```
                 istiod / Linkerd の制御プレーン（無改造）
                    │ xDS（ADS・SDS）          │ Destination / Policy / Identity の gRPC
   ┌────────────────▼───────┐        ┌────────▼───────────────┐
   │ pilot-agent + veil     │◀──────▶│ linkerd2-proxy-identity │
   │ （/usr/local/bin/envoy） │ mTLS │ + veil（linkerd2-proxy）  │
   └────────────────────────┘        └─────────────────────────┘
```

## 仕組み

- **別のエンジン。** メッシュのデータプレーンは独立したモジュール（`src/mesh/`、feature は `mesh`・`mesh-istio`・
  `mesh-linkerd`）。既存のプロキシの経路は変えていない（共有するのは HTTP/2 のサーバの駆動部だけで、ストリームごとの
  処理を総称化して単相化している）。メッシュの feature を付けないビルドは従来と同じバイナリで、`default`・`full*` には
  入らない。
- **起動のされ方で切り替わる同じバイナリ。** `argv[0]` が `envoy`（Istio の pilot-agent は `/usr/local/bin/envoy` を
  起動する）なら Envoy のブートストラップを読み、pilot-agent の UDS で Delta ADS・SDS を話す。`argv[0]` が
  `linkerd2-proxy` なら環境変数 `LINKERD2_PROXY_*` を読み、Linkerd の制御プレーンと gRPC で話す。
- **1 つのモデル。** Envoy の xDS も Linkerd のポリシーも同じ中間表現（リスナー・フィルタチェーン・ルート・クラスタ・
  エンドポイント・RBAC）に変換する。更新は `ArcSwap` で差し替え、データプレーンは読むだけ。長く使われる接続の要求も
  次の要求から新しい設定を使う。
- **透過転送**はメッシュ自身の iptables（istio-init / istio-cni・linkerd-init）と `SO_ORIGINAL_DST` を使う。Linux のみ。

## イメージ

どちらも veil は静的リンクの **musl** 版で、それ以外は本家のプロキシのイメージのまま（pilot-agent とテンプレート、
linkerd2-proxy-init と linkerd2-proxy-identity）。

```bash
docker build -f docker/Dockerfile.musl --build-arg CARGO_FEATURES=mesh-istio,mesh-linkerd,epoll,wasm -t veil:musl-mesh .
docker build -f docker/Dockerfile.mesh-istio   -t veil.local/istio-proxy:1.31.1 .        # /usr/local/bin/envoy を差し替え
docker build -f docker/Dockerfile.mesh-linkerd -t veil.local/linkerd-proxy:edge-26.10.1 . # /usr/lib/linkerd/linkerd2-proxy を差し替え
```

`epoll` は readiness reactor を選ぶ（コンテナの既定の seccomp は io_uring を拒否する）。`wasm` は Istio の WasmPlugin に
要る。試験した版は Istio 1.31.1 と Linkerd edge-26.10.1（proxy v2.370.0）。

## Istio

| 対象 | 方法 |
|------|------|
| サイドカー | Pod の注釈 `sidecar.istio.io/proxyImage: veil.local/istio-proxy:1.31.1`（名前に `/` が無いと注入のテンプレートが無視する）。メッシュ全体なら `istioctl install --set values.global.proxy.image=...` |
| ingress / egress ゲートウェイ（Istio の Gateway） | ゲートウェイの Deployment に `inject.istio.io/templates: gateway` と同じ注釈 |
| Gateway API のゲートウェイ・ambient の waypoint | `spec.infrastructure.parametersRef` で ConfigMap を指し、その `deployment` のオーバーレイで `istio-proxy` コンテナのイメージを指定する |

対応しているもの（`tools/k8s/mesh-e2e.sh` の stock / veil / mixed で確認）:

- サイドカーの inbound / outbound（virtualInbound / virtualOutbound・元の宛先・プロトコルの検出）、HTTP/1.1・
  HTTP/2（h2c・gRPC）・TCP・パススルー、`PERMISSIVE` / `STRICT` の mTLS（Istio の ALPN・SAN の検証・XFCC・
  `127.0.0.6` からの inbound）、peer metadata の交換（Istio の観測のラベル）。
- トラフィック管理のタスク: フォルト注入（abort・delay）、タイムアウト、リトライ、ヘッダの変更、トラフィックの移行、
  書き換えとサブセット、ミラーリング、サーキットブレーカと外れ値検出、一貫性ハッシュ。
- セキュリティ: `AuthorizationPolicy`（HTTP・TCP）、`RequestAuthentication`（インラインの JWKS の JWT）。
- 観測: Istio の標準メトリクス（pilot-agent の統合メトリクス経由の `istio_requests_total` 等）、Telemetry API の
  アクセスログ（Envoy の既定の書式）、分散トレース（Zipkin・B3）。
- WasmPlugin（ECDS → veil の Proxy-Wasm。C++ SDK 製のモジュールも動く。`FAIL_CLOSE` はモジュールが落ちたら 503）。
- ゲートウェイ: HTTP、`credentialName` の HTTPS `SIMPLE` / `MUTUAL`、TLS の `PASSTHROUGH`（SNI）、egress ゲートウェイ、
  Gateway API の `HTTPRoute`。
- ambient: veil を **waypoint** にする（HBONE: 15008 の mTLS の上の HTTP/2 CONNECT、Envoy の内部リスナー、
  `filter_chain_matcher`）。waypoint での L7 の `AuthorizationPolicy` と `HTTPRoute` の振り分け。ztunnel は本家のまま。
- `istioctl proxy-status` / `proxy-config`（listeners・routes・clusters・endpoints・secrets）。Envoy の管理 API（15000:
  `/config_dump`・`/clusters?format=json`・`/stats`・`/ready`・`/drain_listeners`・`/quitquitquit`）を返す。

## Linkerd

| 対象 | 方法 |
|------|------|
| サイドカー | Pod の注釈 `config.linkerd.io/proxy-image: veil.local/linkerd-proxy` と `config.linkerd.io/proxy-version: edge-26.10.1`、または `linkerd inject --proxy-image veil.local/linkerd-proxy --proxy-version edge-26.10.1` |

init コンテナ（`linkerd-init`）も同じイメージで動くので、veil のイメージには本家と同じ版のタグを付ける。

対応しているもの（`tools/k8s/linkerd-e2e.sh` の stock / veil / mixed で確認）:

- identity: `linkerd2-proxy-identity` の CSR で `Identity.Certify`、証明書の有効期間の 70% で更新。証明書が届くと
  `/ready` が 200 になる。
- 宛先とポリシー: `OutboundPolicies.Watch`（プロトコルの検出、`HTTPRoute` / `GRPCRoute` の一致、ヘッダのフィルタ、
  リダイレクト、失敗の注入、重み付きのバックエンド、タイムアウト、リトライ、failure accrual）、`Destination.Get`
  （エンドポイント・ID・HTTP/2 のヒント・opaque transport）、`InboundServerPolicies.WatchPort`（`Server`・
  `AuthorizationPolicy`・`MeshTLSAuthentication`・ルート単位の認可・inbound の既定のポリシー）。
- プロキシ同士の取り決め（本家と混在させるのに要る）: DNS の SAN の ID で ALPN なしの mTLS、`l5d-orig-proto` による
  HTTP/1.1 → HTTP/2 の昇格、opaque transport（ALPN `transport.l5d.io/v1` と transport header）、アプリへの
  `l5d-client-id`、プロキシが作った応答の `l5d-proxy-error`。
- タスク: Automatic mTLS・Restricting Access To Services・Per-Route Policy・Retries・Timeouts・Traffic Split・
  Circuit Breaking（failure accrual）。短い有効期間の証明書の更新、制御プレーンの再起動。
- 観測: `/metrics` は linkerd2-proxy と同じ名前とラベル（`request_total`・`response_total`・`response_latency_ms`・
  `tcp_*`・`control_identity_cert_expiration_timestamp_seconds`）なので `linkerd viz stat`・`edges` が動く。4190 の
  tap（`linkerd viz tap` / `top`。tap サービスの ID からだけ受ける）。emojivoto・booksapp を全 Pod veil で動かせる。

## 環境変数

| 変数 | 意味 |
|------|------|
| `VEIL_MESH_LOG` | メッシュのデータプレーンのログの水準（`trace` … `error`）。Envoy の `-l` と `LINKERD2_PROXY_LOG` より優先 |
| `VEIL_MESH_KTLS` | `1` でメッシュの mTLS に kTLS を使う（既定は無効。ノードに `tls` モジュールが無いと接続ごとにフォールバックのログが出る） |

## 違い・制限

- veil が実装していない Envoy のフィルタは、任意（optional）の指定が無ければ拒否（NACK）する。Envoy と同じく前の設定の
  まま動く。`config_dump` は istioctl が読む項目を返し、全項目ではない。
- X.509 v1 のクライアント証明書は受け付けない（webpki）。Envoy（BoringSSL）は受け付ける。メッシュが発行する証明書は v3。
- ztunnel（ambient の L4）は置き換えない。Linkerd のマルチクラスタのゲートウェイ（`authority_override`）と、古い
  `ServiceProfile` のルート（`GetProfile`）は使わない（現在の `HTTPRoute` のポリシーを使う）。
- tap のイベントは要求が終わったときに出す（1 本の要求の 3 つのイベントは同じ時刻を持つ）。

## 性能

kind の上で fortio で測った（`tools/k8s/mesh-bench.sh`。クライアントとサーバの両側のサイドカー、交互のラウンド）。
表は [docs/perf/README.md](../../perf/README.md#サービスメッシュ) にある。

## 試験

```bash
tools/k8s/mesh-e2e.sh    <stock|veil|mixed> <smoke|tasks|gateway|telemetry|wasm|ambient|all>   # Istio
tools/k8s/linkerd-e2e.sh <stock|veil|mixed> <smoke|tasks|ops|certs|viz|all>                    # Linkerd
tools/k8s/mesh-bench.sh  [rounds] [duration]                                                   # Istio のサイドカーの性能
```

`stock` は同じ試験を本家のプロキシで回す（台本の確認）。`mixed` は一部のワークロードを veil、残りを本家にして両方向の
通信を通す。kind・kubectl・istioctl・linkerd が要る。
