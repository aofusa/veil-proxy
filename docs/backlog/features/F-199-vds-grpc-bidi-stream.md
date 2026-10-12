# F-199: VDS の gRPC 双方向ストリーム

**状態: 進行中（feat/f197-f200-impl）**

## 要望

VDS（F-04）で、現在の REST-JSON の長輪詢に加えて **gRPC の双方向ストリーム**も使えるようにする。

**VDS 本体の機能**（プロキシ用ビルドでも使う）: gRPC の転送は既存の `vds` feature（`full*` に含まれる）に入れる。
API ライブラリ専用の F-197・F-198 とは違い、`handler` 系 feature には依存しない。

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
- 依存: `vds = ["dep:serde_json", "grpc", "http2"]` とする（`grpc` が prost と `src/grpc/` のフレーミング・ステータスを持つ。
  `full*` は元々 `grpc-full` を含むので、プロキシ用ビルドの依存は増えない）。接続は `http2::upstream_mux`。
  tokio・tonic は使わない（VDS 専用スレッドで veil のランタイムを 1 本動かす）。
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

## 結果

- `proto/veil/vds/v1/vds.proto`。veil 本体は同じ定義を prost の derive で `src/vds_grpc.rs` に持つ（本体のビルドに protoc を
  要求しない）。ワイヤ互換は tonic-build で .proto から生成した Rust の参照制御プレーンとの E2E で確認している。
- `[vds] transport = "grpc" | "rest"`（既定 `"grpc"`）、`keepalive_secs`（既定 30）。HTTP/2 の PING ではなく TCP キープアライブ
  で死活を見る（`http2::upstream_mux` に PING の送信を足さずに済み、押し出しの無い長い間の相手の消失も同じく検知できる）。
- 接続は `http2::upstream_mux::H2Mux`（`idle_timeout = 0` でストリームの終わりに接続も閉じる）。TLS は VDS 専用の rustls
  設定（追加 CA・mTLS、ALPN は `h2` だけ）。トークンは接続ごとに読み直す。
- 適用・ACK/NACK・状態は REST と共通の関数（`vds::apply_snapshot`・`record_applied`・`record_rejected`）。
- 参照制御プレーン `tools/vds/server`（tonic + axum、gRPC と REST を 1 ポート、TLS 可、SIGTERM 対応）と
  `docker/Dockerfile.vds-server`。Kubernetes のサンプル（`deploy/kubernetes`）はこれに切り替え、Service のポート名を
  `grpc-vds`（Istio は gRPC として扱い、Linkerd は HTTP/2 を自動判別。`appProtocol: http` は Linkerd が HTTP/1.1 に固定するので付けない）。
  Python 版（`tools/vds/vds_server.py`）は REST の最小参照実装として残す。

試験:

- 単体: `vds_grpc::tests`（Resource → REST 形・ACK/NACK の要求・grpc-status）。
- standalone E2E: `f04_vds_*`（Python・REST）、`f199_vds_rest_on_rust_control_plane`、`f199_vds_grpc_stream_h2c`
  （制御プレーンの再起動からの再接続を含む）、`f199_vds_grpc_stream_tls`（TLS + 追加 CA + Bearer）。4 つとも同じ
  シナリオ（配信・更新・NACK で前の版を保つ・修正）で同じ結果。
- kind E2E（`tools/k8s/kind-e2e.sh`）: VDS を gRPC ストリームで Istio・Linkerd 越しに配信し、`/v1/status` で
  `transport = grpc` の ACK と制御プレーン再起動後の再接続を確認する項目を追加。
