# B-458: Helm chart・Kubernetes の例・本番向けの設定の例が、メトリクスをプライベートの全範囲へ許し、LoadBalancer・イングレス越しの外部の要求に返していた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Metrics endpoint on the public listener is guarded only by an allowlist that admits every RFC 1918 source, so
traffic SNATed by kube-proxy, load balancers or an ingress gets through」・「'Recommended production' config allows
/metrics from all RFC 1918 ranges on a public listener meant to sit behind a TLS terminator」（2 件）から確認。
後の検証の「Default metrics allowlist lets external clients read /__metrics once traffic reaches veil through kube-proxy SNAT or an ingress」も同じ指摘。

## 事象

- メトリクス（`[prometheus]`）はサイトと同じリスナー・ポートで返し、`allowed_ips` は TCP の相手のアドレスを見る。
  Helm chart の `metrics.allowedIps` の既定は `10.0.0.0/8`・`172.16.0.0/12`・`192.168.0.0/16`・`127.0.0.1/32`、
  `deploy/kubernetes/base/config.toml` は `10.0.0.0/8`、本番向けの例（`examples/api/production/config.toml` と
  `library-production.md`）は RFC 1918 の全範囲だった。
- `externalTrafficPolicy: Cluster` の LoadBalancer・NodePort の Service、イングレス、前段の TLS の終端を通る要求は、
  kube-proxy の SNAT・プロキシのためにノード・Pod・ロードバランサのプライベートなアドレスから届く。インターネットの
  誰でも `/__metrics` を読めた（要求数・遅延・上流名等）。veil 本体の既定（空＝ループバックだけ。B-231）より緩い。

## 対応

- Helm chart: `metrics.allowedIps` の既定を空（ループバックだけ。`kubectl port-forward` は届く）にした。
  `metrics.serviceMonitor.enabled` で空のままなら、Prometheus の Pod の CIDR を書くよう `fail` で止める
  （黙って 403 で監視が落ちるより分かりやすい）。`NOTES.txt` に許すアドレスを出す。
- Kubernetes の例・本番向けの例: ループバックだけにして、足すならスクレイパーのアドレスで、前段のプロキシ・ノードを含む
  範囲は書かない理由をコメントに書いた（文書のコード片も同じ）。
- 利用者ガイド（`kubernetes.md`・`observability.md`・`library-production.md` の英日）に、検査は TCP の相手を見ること・
  SNAT の後ろでは範囲で許すと外部の要求も通ることを書いた。
- `tools/release/helm-check.sh`・`helm-kind-test.sh` は `metrics.allowedIps` を渡し、渡さない ServiceMonitor が
  止まることも確かめる。

## テスト

- `tools/release/helm-check.sh`（ServiceMonitor は `metrics.allowedIps` を求める）・`helm-kind-test.sh`。
- `tests/api/library_e2e.sh` の production（ループバックから `/metrics`、文書と例の設定の一致）。
