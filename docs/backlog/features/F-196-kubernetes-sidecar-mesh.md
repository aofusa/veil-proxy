# F-196: Kubernetes サイドカーデプロイ（Istio・Linkerd 連携）

**状態: 進行中（feat/f188-api-library-vds-k8s）**

## 目的

veil を Kubernetes のサイドカーコンテナとしてデプロイし、Istio・Linkerd のメッシュ（mTLS）と共存して動作することを確認する。
`full-container`（epoll）ベースで **musl 版をメイン**に、glibc 版でも同じ試験を行う。

## 設計

- `deploy/kubernetes/`（kustomize: base + overlays istio / linkerd / vds）。設定は ConfigMap、証明書は Secret（F-180）、
  環境ごとの差分は `VEIL_OVERRIDE_<n>`（F-179）、動的ルートは VDS（F-04）。
- `tools/k8s/kind-e2e.sh`: kind クラスタ作成 → イメージ投入 → メッシュ導入 → デプロイ → 試験 → 片付け。

## 受け入れ条件

- musl・glibc の両イメージで、Istio・Linkerd それぞれのメッシュ内でクライアント → veil → アプリの疎通、mTLS の確認、
  Secret 更新での証明書差し替え、VDS でのルート追加、graceful shutdown を確認する。
- `docs/guide/kubernetes.md`（英日）。
