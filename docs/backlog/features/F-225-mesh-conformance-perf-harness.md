# F-225: メッシュの試験・計測基盤（kind・混在・conformance・Envoy / linkerd2-proxy との比較）

**状態: 完了（feat/mesh-dataplane-impl、2026-10-15）**

## 目的

「完全に置き換えた」「本家より速く軽い」を判定する試験と計測を、実装より先に用意する。今の `tools/k8s/kind-e2e.sh`（F-196）は
本家のメッシュのプロキシの隣で veil を動かす構成の試験だけ。

## 設計

- **`tools/k8s/mesh-e2e.sh <istio|linkerd> <stock|veil|mixed> <suite>`**: kind のクラスタ・メッシュの導入（版は固定。最初は
  Istio 1.31・Linkerd edge-26.10、行列で増やす）・プロキシのイメージの差し替え（Istio は `sidecar.istio.io/proxyImage`・
  `values.global.proxy.image`、Linkerd は `config.linkerd.io/proxy-image`）・試験の実行・片付け。`mixed` は名前空間の半分の
  Pod だけ veil にする（両方向の相互通信を必ず含む）。snap の docker の `/tmp` の罠は F-196 と同じく `target/tmp`。
- **スイート**:
  - `smoke`: 疎通（HTTP/1.1・HTTP/2・gRPC・opaque の TCP）・mTLS の証拠・終了。
  - `istio-tasks`: Istio の公式タスクを台本化したもの（Request Routing・Fault Injection・Traffic Shifting・TCP Traffic Shifting・
    Request Timeouts・Circuit Breaking・Mirroring・Authentication Policy・Authorization（HTTP・TCP・JWT・deny・dry-run）・
    Ingress / Egress・Telemetry（メトリクス・トレース・アクセスログ）・Wasm Plugin）。期待値は本家（stock）で同じ台本を流して
    確かめた結果に合わせる（台本の誤りと veil の誤りを分ける）。
  - `linkerd-tasks`: Linkerd のタスク（mTLS・Restricting Access・Per-Route Policy・Retries・Timeouts・Traffic Split・
    Circuit Breaking・opaque ports・multicluster（任意））と `linkerd check --proxy`・viz。
  - `gateway-api`: Gateway API の conformance（`sigs.k8s.io/gateway-api/conformance`、コアと HTTPRoute・GRPCRoute の拡張の一部）を
    Istio の GatewayClass で。
  - `chaos`: 制御プレーンの再起動・証明書の更新（短い有効期限）・エンドポイントの大量入れ替え（Deployment のスケール 1 ↔ 50）・
    Pod の削除中の要求。
- **xDS のフィクスチャの採取**: stock の Envoy の `/config_dump` と istiod の xDS（`istioctl x internal-debug` 等）を版ごとに保存し、
  F-222 の変換の単体試験に使う（`tests/fixtures/istio/<version>/`）。
- **計測**（`tools/k8s/mesh-bench.sh`）: 同じ kind のクラスタ（ノードの CPU を固定、負荷生成と被測定を別ノード）で、
  クライアント → プロキシ → プロキシ → サーバの経路を、fortio の一定レート（例: 1,000・5,000 rps、HTTP/1.1・gRPC、
  64 接続）で p50・p90・p99・p99.9 の追加レイテンシ（メッシュ無しとの差）と、プロキシの CPU（cgroup の usage）・RSS を測る。
  stock と veil を交互に（AGENTS.md の交互 A/B）。規模の項目として、メッシュのサービス数 10・1,000・5,000 でのプロキシの RSS と
  設定の反映時間（Service を足してから要求が通るまで）。
- **目標の決定**: 最初の計測（stock のベースライン）で数値を記録し、本チケットの受け入れ条件に目標（例: 追加 p99 が Envoy の
  ½・CPU が ⅔・RSS が ½、linkerd2-proxy に対して同等以上）を書き足す。結果は `docs/perf/` と `docs/guide/kubernetes.md`。

- **既存のプロキシの非退行（必須、F-215）**: メッシュ外の性能は `tools/perf/suite_ab.sh` の main とブランチの交互 A/B で判定し、
  各子チケットの完了時とマージ前に回す（比の中央値 0.98 以上、どの構成も 0.95 未満にならない）。
- イメージは musl 版（`docker/Dockerfile.musl` の veil。`Dockerfile.mesh-istio`・`Dockerfile.mesh-linkerd` も musl の veil を使う）。

## 受け入れ条件

- stock の Istio・Linkerd で全スイートが通る（台本の正しさの確認）こと、ベースラインの計測値が記録されていること。
- 以降の各チケット（F-216〜F-224）の受け入れ条件はこの基盤のスイートで判定する。


## 実装の結果

- `tools/k8s/mesh-e2e.sh`（smoke・tasks・gateway・telemetry・wasm・ambient・observe・chaos）、`linkerd-e2e.sh`（smoke・tasks・ops・
  certs・viz）、`mesh-gateway-conformance.sh`、`mesh-bench.sh`（サイドカーとゲートウェイ）、`linkerd-bench.sh`、`mesh-fixtures.sh`、
  `mesh-syscalls.sh`。stock で全スイートが通ることを確かめてから veil / mixed で通した。
- カオス: 負荷の最中の istiod の再起動と Pod の削除、istiod の停止中も前の設定で流れ戻れば新しい設定が届く、プロキシの kill からの復帰。
- 最終の結果（musl のイメージ、2026-10-16）: Istio の all（smoke・tasks・gateway・telemetry・wasm・ambient・observe・chaos）は
  veil 87/87・mixed 87/87、Linkerd の all は veil 39/39・mixed 39/39・stock 38/38、Gateway API の conformance は veil で全部合格。
- 性能（docs/perf/README.md の「サービスメッシュ」）: Istio のサイドカーの HTTP/1.1 の最大 3,221 → 6,732 rps、Linkerd 4,109 → 7,261 rps。

## 関連

F-215、F-196（`tools/k8s/kind-e2e.sh`）、`tools/perf`（交互 A/B の原則）。
