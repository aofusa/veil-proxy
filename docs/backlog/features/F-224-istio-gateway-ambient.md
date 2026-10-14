# F-224: Istio のゲートウェイ・ambient（waypoint・HBONE、ztunnel は検討）

**状態: 未着手（2026-10-14 起票・設計。親 F-215）**

## 目的

Istio で Envoy が動いている残りの場所、ingress / egress ゲートウェイと ambient の waypoint を veil にする。

## 設計

- **ingress / egress ゲートウェイ**: 中身はサイドカーと同じ pilot-agent + Envoy で、透過転送が無く、明示ポートのリスナー
  （Gateway リソース・Gateway API の Gateway）と外向きの TLS（SIMPLE・MUTUAL・PASSTHROUGH・ISTIO_MUTUAL・AUTO_PASSTHROUGH）。
  F-222 の変換をそのまま使い、足りないのは外部向けの TLS の終端の種類（SDS で届く `credentialName` の証明書）と SNI での
  振り分け（F-216）。veil の本来の強み（HTTP/3・kTLS・静的なゲートウェイの性能）がそのまま効く場所なので、サイドカーより先に
  効果を出せる。HTTP/3 は Istio のゲートウェイの実験的な QUIC の設定（`PILOT_ENABLE_QUIC_LISTENERS`）に合わせる。
- **waypoint**（ambient の L7）: Gateway API の Gateway（`istio-waypoint` クラス）として Deployment で動く Envoy。ztunnel から
  **HBONE**（ポート 15008 の mTLS の上の HTTP/2 CONNECT。内側に元の TCP）で受け、L7 の処理（F-219〜F-221）をして、宛先の
  ztunnel へ HBONE で送る。veil に足すもの: HTTP/2 の拡張なし CONNECT のトンネルの受け・送り（ストリームの上の双方向転送）、
  HBONE の多重化（宛先の ztunnel ごとに 1 本の HTTP/2 接続に複数のトンネル。`upstream_mux` の拡張）、内側の HTTP の処理。
  istiod が waypoint に生成する xDS は F-222 と同じ変換（内側の「接続」のリスナー `connect_terminate`・`main_internal` 等の
  内部リスナーの扱いを追加）。
- **ztunnel**（L4、ノードごと、Rust）: Envoy ではないので本チケットの必須ではない。置き換えるなら、istio-cni から Pod の
  network namespace の fd を UDS（ZDS）で受け取り、各 Pod の名前空間の中にリスナーを作る（`setns`）・Istio 独自の xDS 型
  （`istio.workload.Address`・`istio.security.Authorization`）を受ける・HBONE の受け送り、が要る。veil のワーカーモデル
  （ワーカーごとに `SO_REUSEPORT` のリスナー）を Pod の名前空間ごとに持つ形になり、設計の変更が大きい。waypoint の後に、
  ztunnel の性能を測ってから判断する（記録のみ）。

## 受け入れ条件

- ingress ゲートウェイ: Istio のタスク（Ingress Gateways・Secure Gateways の SIMPLE / MUTUAL・Ingress without TLS termination・
  Gateway API）と Gateway API の conformance（コア）を F-225 で通す。egress ゲートウェイ: Egress Gateways のタスク。
- waypoint: ambient の bookinfo で waypoint を veil にして、L7 の認可・トラフィックの移行（ambient のタスク）が動く。ztunnel は本家。
- ゲートウェイの性能を Envoy のゲートウェイと比べる（F-225）。

## 関連

F-215、F-222、F-216（SNI）、F-219〜F-221、HTTP/2 の上流（F-174）。
