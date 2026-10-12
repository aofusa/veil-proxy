# F-199: VDS の gRPC 双方向ストリーム

**状態: 設計済み・未着手（design/f197-f200）**

## 要望

VDS（F-04）で、現在の REST-JSON の長輪詢に加えて **gRPC の双方向ストリーム**も使えるようにする。

## 現状（調査結果）

| 項目 | 現状 |
|------|------|
| VDS の転送 | `src/vds.rs`。専用スレッドで `ctrl_http`（ブロッキング HTTP/1.1）を使い `POST /v1/discovery` を長輪詢 |
| 意味論 | xDS の State-of-the-World（`version_info`・`nonce`・ACK/NACK・`error_detail`）。適用は VDS オーバーレイ + 既存のリロード経路 |
| HTTP/2 クライアント | `http2::upstream_mux`（多重化接続・接続/ストリーム両方のフロー制御・要求本文のストリーム送信 `req_tx`・応答のストリーム受信）。h2c と TLS（ALPN h2）の両方 |
| gRPC | `src/grpc/` のフレーミング・ステータス、prost は optional 依存 |
| 参照制御プレーン | `tools/vds/vds_server.py`（標準ライブラリのみ、REST だけ） |

## 設計

### 1. プロトコル（`proto/veil/vds/v1/vds.proto`、リポジトリに置く）

```proto
syntax = "proto3";
package veil.vds.v1;

service VeilDiscoveryService {
  // xDS の ADS と同じ形: クライアントが要求（初回・ACK・NACK）を送り、サーバが版を押し出す
  rpc StreamSnapshots(stream DiscoveryRequest) returns (stream DiscoveryResponse);
}

message Node { string id = 1; string cluster = 2; string user_agent_name = 3; string user_agent_version = 4; }
message ErrorDetail { int32 code = 1; string message = 2; }
message DiscoveryRequest {
  string version_info = 1; Node node = 2; string type_url = 3; string response_nonce = 4; ErrorDetail error_detail = 5;
}
message Resource {
  string type_url = 1;      // type.veil.dev/veil.v1.Route / Upstream（REST と同じ）
  string name = 2;
  string json = 3;          // 本体（[[route]] / [upstreams.<name>] と同じスキーマの JSON。REST 版と 1:1）
}
message DiscoveryResponse { string version_info = 1; string nonce = 2; string type_url = 3; repeated Resource resources = 4; }
```

- リソース本体を JSON 文字列にするのは、TOML の設定スキーマと 1:1 に保ち、REST 版と同じ変換（`resources_to_overlay`）を
  共有するため。Envoy の xDS（`envoy.service.discovery.v3`）とのバイナリ互換は対象外（F-04 と同じ）。
- 意味論は REST 版と同じ: 初回は空の `version_info`、適用成功で新しい `version_info`（ACK）、失敗で前の `version_info` +
  `error_detail`（NACK）。サーバは変化があればいつでも押し出す（長輪詢の 304 は不要になる）。

### 2. クライアント（veil 側）

- `[vds] transport = "grpc" | "rest"`（既定 `"grpc"`）。`server` は `http://`（h2c）/ `https://`（TLS、ALPN h2）。
- VDS 専用スレッドで veil のランタイムを 1 本動かし（`runtime::block_on`）、`http2::upstream_mux` で
  `POST /veil.vds.v1.VeilDiscoveryService/StreamSnapshots` のストリームを開く。要求は `req_tx` へ gRPC フレームで送り、
  応答は `GrpcFrameDecoder` で切り出して prost でデコードする。トークンは `authorization: Bearer`、mTLS・追加 CA は
  REST 版と同じ設定キーを使う。
- 接続断・`grpc-status != 0` は指数バックオフで再接続し、最後に ACK した `version_info` から再開する。
  HTTP/2 の PING（keepalive、`[vds] keepalive_secs`、既定 30）で死活を見る。
- 適用処理（オーバーレイ・`reload_config`・ACK/NACK・状態）は REST 版と共通の関数にまとめる。

### 3. 参照制御プレーン

- Python の標準ライブラリでは gRPC を話せないため、Rust の独立クレート `tools/vds/server/`（ワークスペース外、
  tools/perf/h3load と同じ扱い。テスト・ツール用途なので tonic + tokio を使ってよい）で **REST と gRPC の両方**を提供する。
  スナップショットのファイル監視・ACK/NACK のログ・`/v1/status` は Python 版と同じ。
- Kubernetes のサンプル（`deploy/kubernetes/`）は Rust 版のイメージ（`docker/Dockerfile.vds-server`）に切り替える。
  Python 版は REST の最小参照実装として残す。

### 4. 試験

- 単体: proto のエンコード/デコード、ACK/NACK の状態遷移（REST と共通）。
- standalone E2E: gRPC の制御プレーンでの配信・更新・NACK・再接続（制御プレーンの再起動）・TLS + Bearer。
- kind E2E（`tools/k8s/kind-e2e.sh`）の VDS 項目を gRPC で実施（Istio・Linkerd のメッシュ越しの長寿命ストリーム）。

## 受け入れ条件

- REST と gRPC の両方で同じスナップショットが同じ結果になる。gRPC では更新が長輪詢の待ち無しで即時に届く。
- `docs/guide/vds.md`（英日）に gRPC の proto と設定を追記。

## 依存

- F-197 の gRPC 部品（フレーミング・prost）を共有する（ただし VDS はライブラリ用の `handler` には依存しない）。
