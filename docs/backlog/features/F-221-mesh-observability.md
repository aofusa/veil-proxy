# F-221: メッシュの観測（Istio・Linkerd の標準メトリクス・分散トレース・アクセスログ）

**状態: 未着手（2026-10-14 起票・設計。親 F-215）**

## 目的

Kiali・Grafana の Istio のダッシュボード・`linkerd viz` が、veil のプロキシからそのまま動くようにする。今の veil は
`veil_proxy_*` の Prometheus メトリクスと OTLP/HTTP のメトリクスのエクスポート（`src/otel.rs`）があり、**分散トレース
（スパン）は無い**。

## 設計

- **Istio の標準メトリクス**: `istio_requests_total`・`istio_request_duration_milliseconds`（ヒストグラム）・
  `istio_request_bytes`・`istio_response_bytes`・`istio_tcp_sent_bytes_total`・`istio_tcp_received_bytes_total`・
  `istio_tcp_connections_opened_total`・`istio_tcp_connections_closed_total`。ラベルは `reporter`・`source_workload`・
  `source_workload_namespace`・`source_principal`・`source_app`・`source_version`・`destination_*`・`destination_service`・
  `request_protocol`・`response_code`・`grpc_response_status`・`response_flags`・`connection_security_policy` 等。
  ポート 15090 の `/stats/prometheus`（pilot-agent が 15020 でアプリのメトリクスと合成する）。Telemetry API でのラベルの
  追加・削除・無効化に従う。
- **相手のメタデータ**: 送信元・宛先のワークロード名等は相手の情報が要る。Istio はプロキシ同士で交換する（HTTP は
  `x-envoy-peer-metadata`・`x-envoy-peer-metadata-id` ヘッダ、TCP は ALPN `istio-peer-exchange` の後の交換。近年の版は
  `baggage` 系に移行中のため、F-222 の着手時に対象の版の実装を確認する）。自分のメタデータはブートストラップの `node.metadata`
  から作り、相手のものは接続の確立時に受け取る。EDS のエンドポイントのメタデータからも引ける。
- **Linkerd のメトリクス**: 4191 の `/metrics` に `request_total`・`response_total`・`response_latency_ms`（ヒストグラム）・
  `tcp_open_total`・`tcp_close_total`・`tcp_read_bytes_total`・`tcp_write_bytes_total`、ラベル `direction`・`authority`・
  `target_addr`・`tls`・`client_id`/`server_id`・`dst_*`（Destination のメトリクスのラベル）・`route_*`。viz が使う名前を
  対象の版で確認して揃える。
- **コスト**: ラベルの組（自分 × 相手 × ルート × 応答コード）は接続の確立時と最初の応答で決まる。ラベルの組ごとのカウンタ
  （アトミック）を `ArcSwap` の表で持ち、要求ごとは表引き 1 回と加算だけ（要求ごとの `String`・ラベルの組み立て無し）。
  応答コードはクラス別の配列で持って表引きを避ける。カーディナリティの上限（超えたら `other` に寄せる）を持つ。
- **分散トレース**: W3C `traceparent` / `tracestate` と B3 の伝播（ヘッダを読み、子スパンの ID を足して上流へ）。スパンは
  OTLP（gRPC / HTTP）・Zipkin で送る（専用スレッドでまとめて送る。`otel.rs` を拡張）。サンプリングは Istio の Telemetry API・
  メッシュの既定（1%）に従い、サンプルしない要求はヘッダの伝播だけでスパンを作らない（ホットパスの確保を増やさない）。
  Linkerd はプロキシのトレースが任意（`config.linkerd.io/trace-collector`）なので同じ仕組みで対応する。
- **アクセスログ**: Istio の既定の書式（`%START_TIME%`… の TEXT / JSON）と Telemetry API の `accessLogging`。今の構造化アクセスログ
  （`access_log.rs`）に Envoy の書式の変換を足す。

## 受け入れ条件

- （必須、F-215）既存のプロキシの性能が退行しない（`tools/perf/suite_ab.sh` の main との交互 A/B、比の中央値 0.98 以上・0.95 未満なし）。
  コンテナで試すときのイメージは musl 版。

- F-225 の kind で、Kiali のグラフ（bookinfo）に veil の Pod 間の辺と mTLS の鍵マークが出る、Istio の Grafana ダッシュボードの
  主要パネルが埋まる、`linkerd viz stat/edges/top`（tap は任意）が emojivoto で動く、Jaeger に bookinfo のトレースが繋がって出る。
- 標準メトリクスを有効にしても `tools/perf` の代表構成で退行が 2% 以内（交互 A/B）。
- `docs/guide/observability.md`（英日）。

## 関連

F-215、F-217（主体）、F-222・F-223、F-10（OpenTelemetry）、`src/access_log.rs`。
