# F-222: Istio のサイドカーのアダプタ（Envoy のブートストラップ・xDS v3 の変換・SDS・管理 API の模倣）

**状態: 完了（feat/mesh-dataplane-impl、2026-10-15）**

## 目的

`istio-proxy` コンテナの Envoy を veil に差し替える。istiod・pilot-agent は無改造で使う。

## 方針: pilot-agent を残し Envoy のバイナリだけ差し替える

pilot-agent は Envoy に対して、ブートストラップの生成、istiod との xDS の中継（istiod へは 1 本の接続、Envoy へは
UDS `./etc/istio/proxy/XDS`）、証明書（CSR → istiod の CA → SDS を UDS `./var/run/secrets/workload-spiffe-uds/socket` で配る）、
DNS プロキシ、健康チェックの書き換え（15021）、Wasm モジュールの取得（ECDS のリモート URL をローカルのファイルに書き換える）、
終了時の排出を行う。これらを作り直さず使えるので、veil は「Envoy として振る舞う」部分だけを持てばよい。
pilot-agent は `ProxyConfig.binaryPath`（既定 `/usr/local/bin/envoy`）を起動するので、そこに veil（Envoy 互換の起動モード）を置く。
イメージは musl の veil + pilot-agent（`docker/Dockerfile.mesh-istio`、Envoy は入れない）。
注入は `sidecar.istio.io/proxyImage` 注釈か、注入テンプレート（`values.global.proxy.image`）でイメージを差し替える。

## 設計

1. **起動**: pilot-agent が Envoy に渡すコマンドライン（`-c <bootstrap>`・`--drain-time-s`・`--drain-strategy`・
   `--parent-shutdown-time-s`・`--local-address-ip-version`・`--file-flush-interval-msec`・`--disable-hot-restart`・
   `--allow-unknown-static-fields`・`-l`・`--component-log-level`・`--concurrency` 等）を受け付ける（`veil` の argv[0] が `envoy`
   のとき、または `veil envoy-compat` サブコマンド）。未知のフラグは警告して無視。`--concurrency` を `[server] threads` に。
2. **ブートストラップ**（JSON / YAML、Envoy v3）: `node`（id・cluster・metadata = Istio のワークロードの情報）、`static_resources`
   （xds-grpc・sds-grpc の UDS のクラスタ、Prometheus の 15090 のリスナー、`agent` クラスタ）、`dynamic_resources`（ADS）、
   `admin`、`stats_config`・`tracing`。pilot-agent の生成するテンプレートの範囲を読む。
3. **xDS クライアント**: ADS を UDS で、Delta xDS（Istio 1.22 以降の既定）と SotW の両方。型は LDS・RDS・CDS・EDS・SDS・ECDS
   （と istiod の NDS 等の Istio 拡張は無視して ACK）。専用スレッド（`ctrl_http` の gRPC を UDS 対応に）で受け、変換・検証して
   F-218 のメッシュモデルへ差し替える。ACK / NACK はリソース単位（Delta）。`node` の `metadata` は pilot-agent が渡すものをそのまま送る。
4. **変換**（`src/mesh/istio/`）: istiod が生成する Envoy の設定を基準に、扱う型を列挙して対応する。
   - リスナー: `virtualOutbound`（15001、`use_original_dst`）・`virtualInbound`（15006）・ポートごとのリスナー、リスナーフィルタ
     （`original_dst`・`tls_inspector`・`http_inspector`）、フィルタチェーンの一致（宛先ポート・宛先 IP・SNI・ALPN・
     `transport_protocol`・送信元）。
   - ネットワークフィルタ: `http_connection_manager`・`tcp_proxy`・`istio.metadata_exchange`・`istio.stats`・`rbac`（TCP）・
     `sni_cluster`・`ext_authz`。
   - HTTP フィルタ: `router`・`rbac`・`jwt_authn`・`fault`・`cors`・`grpc_stats`・`istio.stats`・`istio.metadata_exchange`
     （peer metadata）・`ext_authz`・`local_ratelimit`・`wasm`（ECDS）・`alpn`。
   - ルート（RDS）・クラスタ（CDS: `EDS`・`ORIGINAL_DST`・`STATIC`・`STRICT_DNS`、`transport_socket` の TLS・`tls` の
     SDS の参照、`circuit_breakers`・`outlier_detection`・`lb_policy`・`common_lb_config`・`typed_extension_protocol_options`
     の HTTP/2）・エンドポイント（EDS: ロカリティ・優先度・重み・`metadata` の `istio`/`envoy.transport_socket_match`）。
   - 未知の `typed_config`: `is_optional` なら無視して記録、そうでなければそのリソースを NACK（`error_detail` に型名）。
     **認可系のフィルタを黙って落とさない**（F-220 の安全側）。
   - EnvoyFilter で入った設定は上記の規則のとおり NACK になり、istioctl の `proxy-status` に出る（対象外を明示）。
5. **SDS**: `default`（自分の証明書・鍵）と `ROOTCA`（信頼アンカー）を UDS から受け、F-217 の差し替えに渡す。
6. **管理 API の模倣**（15000、127.0.0.1 のみ）: pilot-agent と istioctl が使うもの。
   - `/ready`（初回の LDS・CDS の適用後に 200。pilot-agent の 15021 の `/healthz/ready` がこれと統計を見る）、`/server_info`、
     `/stats`（pilot-agent が見るもの: `listener_manager.workers_started` 等を対象の版で確認して出す）、
     `/stats/prometheus`（15090 からも）、`/drain_listeners?inboundonly&graceful`（F-187 の graceful shutdown へ）、
     `/quitquitquit`、`/config_dump`（istioctl `proxy-config` が読む。メッシュモデルを Envoy の型に戻して出す。
     最初は listeners・routes・clusters・endpoints・secrets（鍵は出さない）の範囲）、`/logging`（ログレベルの変更）。
7. **Wasm**: Istio の WasmPlugin は Proxy-Wasm の ABI。ECDS で届く `envoy.extensions.filters.http.wasm.v3.Wasm` を veil の
   Proxy-Wasm のプラグイン設定に変換する（モジュールはローカルのファイルに pilot-agent が書き換え済み）。
8. **DNS**: pilot-agent の DNS プロキシ（15053）はそのまま使える（veil は関与しない）。

## 受け入れ条件

- （必須、F-215）既存のプロキシの性能が退行しない（`tools/perf/suite_ab.sh` の main との交互 A/B、比の中央値 0.98 以上・0.95 未満なし）。
  コンテナで試すときのイメージは musl 版。（段階ごと）

- (a) 透過転送 + mTLS + ルーティング: bookinfo が全 Pod veil で動き、VirtualService・DestinationRule・ServiceEntry の公式タスク、
  PeerAuthentication の STRICT・PERMISSIVE、Envoy の Pod との混在。
- (b) 認可・認証: F-220 の受け入れ条件。
- (c) テレメトリ: F-221 の受け入れ条件（Kiali・Prometheus・トレース）。
- (d) WasmPlugin: Istio のタスク（Wasm Plugin）の例が動く。
- 全段: istioctl `proxy-status` が SYNCED、`proxy-config listeners/routes/clusters/endpoints/secret` が中身を出す、
  `istioctl analyze` に veil 起因の警告が無い、Pod の起動 → Ready が Envoy と同等以下、SIGTERM で排出して終わる。
- istiod が生成した xDS を対象の版ごとにフィクスチャとして保存し（`tests/fixtures/istio/<version>/`）、変換の単体試験。
- `docs/guide/kubernetes.md`（英日）に「Istio のプロキシとして使う」の節。


## 実装の結果

- pilot-agent を残して `/usr/local/bin/envoy` を veil に差し替える。ブートストラップ・Delta ADS・SDS・ECDS（WasmPlugin → Proxy-Wasm、
  B-131）・管理 API（istioctl proxy-status / proxy-config）。
- istiod の xDS のフィクスチャ（`tests/fixtures/istio/1.31.1/`、`tools/k8s/mesh-fixtures.sh`・`VEIL_MESH_XDS_RECORD`）と変換の試験
  `tests/mesh_istio_fixtures.rs`。
- bookinfo を全 Pod veil で動かす・istioctl analyze にプロキシ起因の指摘なし・Pod の作成 → Ready は Envoy 46 秒に対して veil 19〜34 秒・
  SIGTERM の排出（負荷の最中に Pod を消して 99.7% 成功、Envoy は 99.8%）・カオス（istiod の再起動・停止、プロキシの kill）。

## 関連

F-215、F-216〜F-221、F-224（ゲートウェイ・waypoint は同じ変換を使う）、F-211（xDS 互換の入口 = 9 はここで実現）、F-187（排出）、
Proxy-Wasm（`docs/guide/wasm.md`）。
