# F-196: Kubernetes サイドカーデプロイ（Istio・Linkerd 連携）

**状態: 完了（feat/f188-api-library-vds-k8s）**

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

## 結果（2026-10-12、feat/f188-api-library-vds-k8s）

- `deploy/kubernetes/`（base + overlays plain / istio / linkerd）、`tools/k8s/kind-e2e.sh`、`docs/guide/kubernetes.md`（英日）。
- kind（Kubernetes 1.34 系）+ Istio 1.31.1 + Linkerd edge-26.10.1 で、musl・glibc の full-container イメージとも
  plain / Istio / Linkerd の全項目（疎通 HTTP/1.1・h2c、mTLS の証拠、STRICT での拒否、Secret 更新、環境変数上書き、
  VDS でのルート追加、SIGTERM から 1 秒以内の停止）が ALL OK。
- 見つけた落とし穴: Service の `appProtocol: http` を Linkerd は HTTP/1.1 固定と解釈し h2c が壊れる（付けない）。
  デモの whoami は PID 1 で SIGTERM を無視するため Pod の終了は猶予いっぱいかかる（veil 自体は 1 秒以内）。
