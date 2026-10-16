# F-229: 外部認可（ext_authz）: Istio の CUSTOM の AuthorizationPolicy と TOML のルート

**状態: 未着手（2026-10-16 起票・設計。親 F-215 の残件。F-220 で「後回し」としたもの）**

## 目的

- 要求ごとに外部の認可サービスへ問い合わせ、その答えで通すか拒否するかを決める（Envoy の `ext_authz`）。
- Istio では、AuthorizationPolicy の `action: CUSTOM` と meshConfig の `extensionProviders`
  （`envoyExtAuthzHttp`・`envoyExtAuthzGrpc`）がこれを使う。今の veil は、このフィルタを要するリスナーを拒否（NACK）している。
- メッシュ無しの TOML のルートにも、同じ機能を出す（F-217・F-220 と同じく一般機能にする）。Linkerd には相当する機能が無いので対象外。

## Istio が配る設定（段階 0 で xDS のフィクスチャで確かめる）

以下は前提である。CUSTOM のポリシーを置いた bookinfo で istiod の xDS を記録し（`VEIL_MESH_XDS_RECORD`）、
`tests/fixtures/istio/1.31.1/` に足して確かめる。

- HTTP: `envoy.filters.http.ext_authz`（`http_service` か `grpc_service`）と、その前の RBAC フィルタ。
  - RBAC の shadow の規則が、CUSTOM のポリシーに一致したかを動的メタデータに書く。
  - ext_authz の `filter_enabled_metadata` がそれを見て、一致した要求だけを問い合わせる。
- TCP: `envoy.filters.network.ext_authz`（gRPC だけ）。
- 主なフィールド:
  - 問い合わせの相手と時間: `timeout`。
  - 失敗時の扱い: `failure_mode_allow`、`status_on_error`。
  - 本文を送るか: `with_request_body`（`max_request_bytes`・`allow_partial_message`）。
  - 送るヘッダ: `allowed_headers`（HTTP のとき）。
  - 認可の答えから上流と応答へ写すヘッダ: `allowed_upstream_headers`・`allowed_client_headers`。
  - `include_peer_certificate`。

## 設計

### 共通の部品（`src/ext_authz.rs`、feature `ext-authz`、`full*` と `mesh` に含める）

- `AuthzClient` に 2 種類を持たせる。
  - **HTTP**: 元の要求のメソッド・パス（`path_prefix` を前に付ける）と、許可されたヘッダを、`[upstreams.<name>]` と同じ接続プールで
    送る。2xx なら許可し、それ以外はその応答を（許可されたヘッダと本文ごと）クライアントへ返す。
  - **gRPC**: `envoy.service.auth.v3.Authorization/Check` を `http2::upstream_mux` の多重化接続で送る。
    - 符号化は prost を使う（VDS・メッシュと共有）。
    - `CheckResponse` の `ok_response`（ヘッダの追加・削除）と `denied_response`（状態コード・ヘッダ・本文）を解釈する。
- 呼び出しの Future は `runtime::time::timeout` で包む。満了や接続の失敗は、`failure_mode_allow` なら通し、そうでなければ
  `status_on_error`（既定 403）を返す。
- ホットパスでの扱い（問い合わせ自体は要求ごとに必要）:
  - 要求の符号化は、要求バッファのプールの `BytesMut` に直接書く。
  - 送るヘッダの選び方（`allowed_headers` の照合器）と、ポリシーごとの固定のフィールドはロード時に作る。
  - ext_authz を使わないルート・リスナーは、`Option` を見る分岐 1 個だけにする。
  - 認可の答えはキャッシュしない（Envoy もしない）。

### メッシュ（Istio）

- `src/mesh/xds/translate.rs` で、2 つの ext_authz フィルタ（HTTP 用・TCP 用）をフィルタチェーンの段に変換する。
- `filter_enabled_metadata` は、RBAC の shadow の結果（`src/mesh/xds/rbac.rs` は dry-run の評価を持っている）を要求の文脈に置き、
  それを見て判定する。
- TCP の ext_authz は接続時に 1 回だけ問い合わせる（L4 の RBAC と同じ位置）。
- 管理 API: `/stats` に `ext_authz.ok`・`denied`・`error`・`failure_mode_allowed` を出す（Envoy の名前）。

### TOML

```toml
[[route]]
[route.ext_authz]
http_upstream = "authz"        # [upstreams.<name>]。grpc_upstream と排他
# grpc_upstream = "authz-grpc"
path_prefix = "/check"
timeout_ms = 200
failure_mode_allow = false
status_on_error = 403
allowed_headers = ["authorization", "x-user-*"]
allowed_upstream_headers = ["x-user-id"]
include_body_bytes = 0         # 0 = 本文を送らない
```

- 評価は、ルート照合と JWT（F-220）の後、`[route.traffic]` の選択の前に行う。
- HTTP/1.1・HTTP/2・HTTP/3 の各経路で同じ位置に置く。
- HTTP/3 では、問い合わせをワーカーの `FuturesUnordered` の中で待つ（メインループで await しない。B-97）。

## 受け入れ条件

- （必須、F-215）既存のプロキシの性能が退行しない（`suite_ab.sh`）。メッシュのイメージは musl 版。
- Istio の公式タスク External Authorization（`samples/extauthz`。HTTP 8000・gRPC 9000）を `mesh-e2e.sh` の tasks に足し、
  stock / veil / mixed で通す。確かめる組み合わせ:
  - 許可・拒否
  - ヘッダの受け渡し
  - 認可サービスの停止時の `failure_mode_allow` の両方
- フィクスチャの変換の試験（`tests/mesh_istio_fixtures.rs`）。
- TOML の E2E（`tests/standalone_e2e.rs`、テスト内の小さな認可サーバ。HTTP と gRPC、HTTP/1.1・HTTP/2・HTTP/3）。
- 認可の問い合わせのコストを Envoy と比べる（`mesh-bench.sh` に ext_authz 付きの経路）。
- `docs/guide/security.md`・`mesh.md`（英日）・`examples/config.toml`。

## 関連

F-215、F-220、F-222、`src/mesh/xds/rbac.rs`、`src/jwt_verify.rs`。
