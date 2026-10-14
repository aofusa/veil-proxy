# VDS: 制御プレーンからのルート・アップストリームの動的配信

[← ドキュメント目次](README.md) · [English](../vds.md)

VDS（Veil Discovery Service）は、**制御プレーン** から稼働中の veil のルートとアップストリームを配信する仕組みです。
Envoy の xDS（State-of-the-World）の最小サブセットで、同じ意味論（バージョン・nonce・ACK/NACK）を gRPC の双方向ストリーム（既定）か REST-JSON の長輪詢で運びます。
制御プレーンが新しいスナップショットを公開すると、veil はそれを検証し、通常のホットリロード経路で（無停止で）差し替えて応答（ACK）します。
検証に失敗したスナップショットは拒否（NACK）され、前の設定のまま動き続けます。

> **要件**: `--features vds` でビルド（`--features full` ほか `full*` に含まれます）。
> プロトコルの機械可読なスキーマは [vds-schema.json](../vds-schema.json) です。

- 制御プレーンとの通信は **専用スレッド** で行い、データプレーン（ワーカーのイベントループ）には一切触れません。
- スナップショットの適用は SIGHUP リロードとまったく同じ検証を通るので、不正なスナップショットでプロキシが落ちることはありません。
- 制御プレーンは、下記のプロトコルを実装した任意のサーバです。参照実装として `tools/vds/server`（Rust、gRPC + REST）と
  `tools/vds/vds_server.py`（Python、REST）を同梱しています。

## 設定

`[vds]` は起動時に読み込まれます。SIGHUP では反映されません。変更するときは veil を再起動してください。

```toml
[vds]
enabled = true
server = "https://cp.example.com:18000"
node_id = "edge-1"
cluster = "prod"
token_file = "/etc/veil/vds/token"
```

| キー | 既定値 | 説明 |
|------|--------|------|
| `enabled` | `false` | VDS クライアントのスレッドを起動する |
| `server` | `""` | 制御プレーンのベース URL: `http://host:port[/base]` または `https://...`（有効時は必須。`servers` を指定したときは不要） |
| `servers` | `[]` | 冗長化のための複数の制御プレーン（F-211）。`server` より優先。接続・ストリームが失敗したらすぐ次を試し、全部を 1 周したら再試行のバックオフで待つ |
| `transport` | `"grpc"` | `"grpc"`: 双方向ストリーム（h2c または TLS + ALPN h2）、`"rest"`: `{server}/v1/discovery` の長輪詢 |
| `keepalive_secs` | `30` | gRPC の接続の TCP キープアライブ（`0` で無効） |
| `node_id` | `""` | 制御プレーンへ送るノード ID。空ならホスト名 |
| `cluster` | `""` | 制御プレーンへ送るクラスタ（グループ）名 |
| `token_file` | `""` | Bearer トークンを書いたファイル。要求（REST）・接続（gRPC）のたびに読み直すのでローテーションできる。空なら `Authorization` を送らない |
| `ca_cert` | `""` | `https` の追加の信頼 CA（PEM）。空なら組み込みの web PKI ルートだけ |
| `client_cert` / `client_key` | `""` | 相互 TLS 用のクライアント証明書・鍵（PEM）。両方指定したときだけ使う |
| `poll_timeout_secs` | `30` | REST: 変化が無いときに制御プレーンが要求を保留してよい時間（`poll_timeout_secs` として送る）。HTTP のタイムアウトはこの値 + 15 秒 |
| `retry_initial_ms` | `500` | エラー後の最初の再試行までの待ち時間（毎回 2 倍） |
| `retry_max_ms` | `30000` | 再試行待ちの上限 |
| `max_response_bytes` | `16777216` | これを超える応答（gRPC はメッセージ）は拒否する（16 MiB） |
| `max_resources` | `10000` | これを超える数のリソースを持つスナップショットは拒否する |
| `initial_fetch_timeout_ms` | `0` | 起動時、最初のスナップショットを適用するまでリスナーを開かずに待つ上限（`0` は待たない）。期限を過ぎたらファイルの設定で開始する。古いルートで要求を受けてはいけない Kubernetes のサイドカー向け（F-211） |

VDS も設定の一部なので、`VEIL_OVERRIDE_<n>` と `--override` が使えます
（[起動・運用](running.md#環境変数による設定の上書きveil_override_n)を参照）。例: `VEIL_OVERRIDE_1='vds.server = "http://cp:18000"'`。

## 状態: メトリクスと管理 API

VDS が有効なとき、クライアントの状態（F-211）を Prometheus（`[prometheus] enabled = true`）に出します。

| メトリクス | 種類 | 意味 |
|-----------|------|------|
| `veil_proxy_vds_connected` | gauge | 制御プレーンが応答している間 `1`、それ以外 `0` |
| `veil_proxy_vds_applied_info{version_info}` | gauge | 常に `1`。ラベルが適用中のスナップショットの版 |
| `veil_proxy_vds_updates_total{result="applied"\|"rejected"}` | counter | 適用（ACK）・拒否（NACK）したスナップショットの数 |
| `veil_proxy_vds_last_update_timestamp_seconds` | gauge | 最後にスナップショットを適用・拒否した時刻（UNIX 秒） |

[管理 API](admin-api.md) の `GET {path_prefix}/vds`（既定 `/__admin/vds`）は同じ内容を JSON で返します。

```json
{"enabled":true,"connected":true,"server":"http://cp-1:18000","version_info":"3f2a9c1d04b7e612",
 "last_error":null,"updates_acked":4,"updates_nacked":1,"last_update_unix":1791700000}
```

`last_error` は最後に拒否したスナップショットの理由（または最後の接続エラー）です。VDS が無効なら
`{"enabled":false}` を返します。

## スナップショットで制御できるもの

スナップショットはルートとアップストリームの **完全な** 集合（State-of-the-World）です。差分ではなく、毎回前のものを置き換えます。

- `[[route]]` は全体が置き換わります。`config.toml` のルートは **マージされません**。ルートを含むスナップショットを受け取っている間、
  `config.toml` のルートは使われません。フォールバックが要るなら、スナップショットに含めてください。
- `[upstreams.<name>]` も同様に全体が置き換わります。
- ルートだけを含むスナップショットは `config.toml` の `upstreams` に触れません（逆も同じ）。
- **空のスナップショット**（`"resources": []`）は「ルートもアップストリームも無し」を意味し、両方を空にします。
- 最初のスナップショットを受け取るまで（および制御プレーンに届かない間）は、`config.toml` と最後に適用したスナップショットで動き続けます。

**VDS では変更できないもの**（再起動が必要）: `[server]` のリスナー、`[tls]`、`[[l4]]`、`[security]` などその他すべてのセクション。
置き換わるのは `route` と `upstreams` だけです。

### 優先順位

```
config.toml  <  VDS スナップショット  <  環境変数 VEIL_OVERRIDE_<n>  <  コマンドラインの --override
```

右のものが優先されます。たとえば `VEIL_OVERRIDE_1='route.0.action.path = "/srv"'` は VDS が配信したルートを書き換えます。
スナップショットと上書きはリロードのたびに適用し直されるので、SIGHUP をしても配信済みのスナップショットは保たれます。

## プロトコル

同じメッセージと意味論を 2 種類の転送で運べます（`[vds] transport`）。

- **gRPC**（既定）: 長寿命の双方向ストリーム 1 本。制御プレーンはスナップショットが変わった時点で押し出すので、輪詢はありません。
- **REST**: JSON の長輪詢（下記）。HTTP/2 を話せない制御プレーン向けです。

### gRPC（既定）

サービスは [`proto/veil/vds/v1/vds.proto`](../../../proto/veil/vds/v1/vds.proto) で定義しています。

```proto
service VeilDiscoveryService {
  rpc StreamSnapshots(stream DiscoveryRequest) returns (stream DiscoveryResponse);
}
message DiscoveryRequest  { string version_info = 1; Node node = 2; string type_url = 3;
                            string response_nonce = 4; ErrorDetail error_detail = 5; }
message DiscoveryResponse { string version_info = 1; string nonce = 2; string type_url = 3;
                            repeated Resource resources = 4; }
message Resource { string type_url = 1; string name = 2; string json = 3; }
```

veil は VDS のスレッドで `POST {server}/veil.vds.v1.VeilDiscoveryService/StreamSnapshots` を h2c（`http://`）または
TLS + ALPN `h2`（`https://`）で開きます（veil 自身の HTTP/2 クライアント。tokio・tonic は使いません）。`token_file` があれば
`authorization: Bearer <token>` を付けます（接続のたびに読み直す）。各フィールドの意味は REST と同じです。`Resource` は REST の
`resources` 配列の 1 要素で、`@type` を `type_url` に、`name` を `name` に移し、残りのオブジェクトを JSON にして `json` に入れたものです。

1. 接続したら veil は `version_info = ""`（再接続なら最後に適用した版）の要求を送る。
2. 制御プレーンは、スナップショットがそのノードの版と違えば（かつ拒否されていなければ）`DiscoveryResponse` を送る。
3. veil は適用し、同じストリームで次の要求を送る: **ACK**（新しい `version_info` と応答の `nonce`）か **NACK**（古い `version_info`・`nonce`・`error_detail`）。
4. ストリームが終わる・失敗する・`grpc-status` が 0 以外なら、指数バックオフで張り直し、最後に適用した版から再開する。
   `keepalive_secs`（TCP キープアライブ）で、押し出しの無い長い間に制御プレーンが消えたことを検知する。

### REST

すべての要求は `POST {server}/v1/discovery`（`Content-Type: application/json`）です。`token_file` を設定していれば、
各要求に `Authorization: Bearer <token>` が付きます。

### DiscoveryRequest

```json
{
  "version_info": "3b1f9c2a77d04e51",
  "node": {
    "id": "edge-1",
    "cluster": "prod",
    "user_agent_name": "veil",
    "user_agent_version": "0.7.0"
  },
  "type_url": "type.veil.dev/veil.v1.Snapshot",
  "response_nonce": "17",
  "poll_timeout_secs": 30
}
```

| フィールド | 意味 |
|------------|------|
| `version_info` | 最後に **適用に成功した** バージョン。最初のスナップショットの前は空文字 |
| `node` | `id`（`node_id`）・`cluster`・veil のバージョン（`user_agent_*`） |
| `type_url` | 常に `type.veil.dev/veil.v1.Snapshot` |
| `response_nonce` | このクライアントが最後に処理した（適用または拒否した）応答の `nonce`。最初は空 |
| `poll_timeout_secs` | サーバがこの要求を保留してよい最長時間 |
| `error_detail` | NACK のときだけ付く（後述）: `{"code": 3, "message": "..."}` |

### DiscoveryResponse

```json
{
  "version_info": "9d02ce8e1b56a3f0",
  "nonce": "18",
  "type_url": "type.veil.dev/veil.v1.Snapshot",
  "resources": [
    {
      "@type": "type.veil.dev/veil.v1.Upstream",
      "name": "api",
      "servers": ["http://10.0.0.11:8080", "http://10.0.0.12:8080"]
    },
    {
      "@type": "type.veil.dev/veil.v1.Route",
      "conditions": { "path": "/api/*" },
      "action": { "type": "Proxy", "upstream": "api" }
    }
  ]
}
```

| フィールド | 意味 |
|------------|------|
| `version_info` | このスナップショットの不透明なバージョン文字列（内容のハッシュなど） |
| `nonce` | この応答を識別する不透明なトークン。次の要求の `response_nonce` に返ってくる |
| `type_url` | 省略可。あれば `type.veil.dev/veil.v1.Snapshot` でなければならず、違う値だと拒否される |
| `resources` | リソースの配列。各要素は `@type` を持つオブジェクト |

### リソースの種類

| `@type` | 内容 |
|---------|------|
| `type.veil.dev/veil.v1.Route` | `[[route]]` 1 件。TOML と同じ形の JSON（`conditions`・`action`・`security` など）。`name` フィールドは無視される |
| `type.veil.dev/veil.v1.Upstream` | `[upstreams.<name>]` 1 件。必須の `name` がテーブル名で、残りのフィールドがテーブルの内容（`servers`・`algorithm` など） |

JSON の `null` は取り除かれます（TOML に null は無いため）。未知の `@type`、オブジェクトでないリソース、`name` の無いアップストリーム、
`max_resources` を超える数のリソースがあると、スナップショット全体が拒否されます。`Route` リソースの並びが評価順（先頭から一致したものを採用）で、
ファイルの場合と同じです。

`config.toml` のルート・アップストリームに書けるものはすべて使えます。[設定リファレンス](configuration.md)・
[ルーティング](routing.md)・[ロードバランシング](load-balancing.md)・[examples/config.toml](../../../examples/config.toml)を参照してください。

#### 長輪詢・304・ACK・NACK

1. veil が現在の `version_info` を付けて要求を送る。
2. 制御プレーンに新しいものが無ければ、変化があるか `poll_timeout_secs` が過ぎるまで要求を **保留** し、過ぎたら
   **`304 Not Modified`**（本文なし）を返す。veil はすぐに次の要求を送る。
3. 違うバージョンがあれば、制御プレーンは **`200`** で `DiscoveryResponse` を返す。
4. veil はスナップショットを検証・適用する（リソースの検査 → 設定全体の検証とリロード）。
   - **ACK** — 成功: 次の要求に新しい `version_info` とその応答の `nonce` を付ける。
   - **NACK** — 失敗: 前の設定のまま動き続け、次の要求に **古い** `version_info`・その応答の `nonce`・`error_detail` を付ける（1 回だけ送る）。
     制御プレーンは、拒否されたバージョンを内容が変わるまで送り直さないようにする（参照実装はそうしている）。
5. それ以外のステータスやネットワークエラー: 警告をログに出し、指数バックオフ（`retry_initial_ms` から `retry_max_ms` まで倍々）で待って再試行する。
   最後に正常だった設定は動き続ける。

## セキュリティ

- 信頼できるネットワークの外では `https://` を使い、制御プレーンがプライベート CA を使うなら `ca_cert` を設定します。
- `client_cert` + `client_key` で **相互 TLS** になり、制御プレーンが各 veil ノードを認証できます。
- `token_file` は Bearer トークンです。環境変数ではなくファイルに置いてください（[コンテナ内の証明書と鍵](tls.md#コンテナでの証明書秘密鍵)を参照）。
  要求ごとに読み直すので、ファイルを差し替えるだけでローテーションでき、再起動は不要です。
- スナップショットはトラフィックをどこへでも向けられます。制御プレーンは設定そのものと同じ扱いにし、公開できる主体を絞り、通信路を認証してください。
- サンドボックス（`[security]`）が有効なら VDS のスレッドもその中で動くので、上記のファイルは読める必要があります（`landlock_read_paths`）。

## 参照制御プレーン

参照実装を 2 つ同梱しています。どちらもスナップショットのファイルを配信し、ノードごとの状態を持ちます。

| | `tools/vds/server`（Rust） | `tools/vds/vds_server.py`（Python） |
|---|---|---|
| 転送 | gRPC **と** REST を 1 つのポートで（HTTP/1.1・h2c・TLS） | REST のみ |
| 起動 | `cargo run --release --manifest-path tools/vds/server/Cargo.toml -- ...` または `docker/Dockerfile.vds-server` のイメージ | `python3 tools/vds/vds_server.py ...`（3.11 以上・標準ライブラリのみ） |

```bash
veil-vds-server --snapshot snapshot.toml --listen 0.0.0.0:18000 \
    [--token-file token] [--hold 30] [--tls-cert cert.pem --tls-key key.pem]
python3 tools/vds/vds_server.py --snapshot snapshot.toml --listen 0.0.0.0:18000 \
    [--token-file token] [--hold 30]
```

スナップショットは `[[route]]` と `[upstreams.<name>]` だけを `config.toml` とまったく同じ書式で書いた TOML ファイルです。

```toml
[upstreams.api]
servers = ["http://10.0.0.11:8080", "http://10.0.0.12:8080"]

[[route]]
[route.conditions]
path = "/api/*"
[route.action]
type = "Proxy"
upstream = "api"
```

サーバはファイルを監視し、内容が変わると（バージョンは SHA-256 の先頭 16 桁）gRPC のストリームへ押し出し、保留中の REST の要求に応答します。TOML として解析できない
スナップショットは公開されず、前のものが残ります。エンドポイントは gRPC のサービス（Rust 版のみ）、`POST /v1/discovery`、`GET /v1/status`
（ノードごとの ACK 済みバージョン・最後の NACK・転送を JSON で返す）、`GET /healthz` です。veil 側の設定は次のとおりです。

```toml
[vds]
enabled = true
server = "http://127.0.0.1:18000"
```

テストや小規模な運用向けの参照実装です。本番では自前の制御プレーンにプロトコルを実装し（[vds-schema.json](../vds-schema.json)）、上記のとおり保護してください。

## Kubernetes での運用

スナップショットを **ConfigMap** にして小さな制御プレーン Pod にマウントし、すべての veil をその Service に向けます。
ConfigMap を編集すれば、ロールアウト無しで ConfigMap の反映遅延（通常 1 分以内）ですべてのノードに届きます。

```yaml
apiVersion: v1
kind: ConfigMap
metadata: {name: veil-vds-snapshot}
data:
  snapshot.toml: |
    [[route]]
    [route.conditions]
    path = "/"
    [route.action]
    type = "Proxy"
    url = "http://127.0.0.1:9000"
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: veil-vds}
spec:
  replicas: 1
  selector: {matchLabels: {app: veil-vds}}
  template:
    metadata: {labels: {app: veil-vds}}
    spec:
      containers:
        - name: vds
          image: veil-vds-server:latest   # docker/Dockerfile.vds-server
          args: ["--snapshot", "/etc/vds/snapshot.toml", "--listen", "0.0.0.0:18000",
                 "--token-file", "/etc/veil/vds/token"]
          volumeMounts:
            - {name: snapshot, mountPath: /etc/vds, readOnly: true}
            - {name: token, mountPath: /etc/veil/vds, readOnly: true} # Secret
      volumes:
        - {name: snapshot, configMap: {name: veil-vds-snapshot}}
        - {name: token, secret: {secretName: veil-vds-token}}
---
apiVersion: v1
kind: Service
metadata: {name: veil-vds}
spec:
  selector: {app: veil-vds}
  ports: [{name: grpc-vds, port: 18000, targetPort: 18000}]   # Istio は grpc- 接頭辞、Linkerd は HTTP/2 を自動判別
```

スナップショットを `subPath` でマウントしないでください（kubelet は `subPath` のマウントを更新しません）。veil のコンテナでは
`[vds] server = "http://veil-vds:18000"` を設定し、`token_file` は同じ Secret を指します。そのまま適用できる例
（サイドカー Deployment・VDS 制御プレーン・Istio/Linkerd 向け overlay）は [`deploy/kubernetes/`](../../../deploy/kubernetes/) にあります。
`kubectl edit configmap veil-vds-snapshot` で、すべての veil のルートが数秒〜1 分で切り替わります。

## トラブルシューティング

| 症状 | 確認すること |
|------|--------------|
| 何も起きない | `[vds] enabled = true`、正しい `server` の URL、ログの `[VDS]` 行（`subscribing to ...`） |
| どの版で動いているか・なぜ拒否されたか | `GET /__admin/vds` か `veil_proxy_vds_*` のメトリクス |
| 起動に `initial_fetch_timeout_ms` かかる | 期限内に制御プレーンが応答しなかった（ログに `no snapshot within initial_fetch_timeout_ms`）。ファイルの設定で開始しています |
| `rejected snapshot` の警告 | メッセージが検証エラーです。スナップショットを直してください（前の設定は動き続けています） |
| `control plane returned 401` / `grpc-status 16` | `token_file` の内容が制御プレーンのトークンと一致していない |
| `stream ... failed` で `HTTP 404`・`did not negotiate h2` | 制御プレーンが gRPC を話さない: `transport = "rest"` にする（例: `tools/vds/vds_server.py`） |
| TLS エラー | プライベート CA なら `ca_cert`、証明書のサーバ名、mTLS なら `client_cert`/`client_key` |
| リロードでスナップショットが消えた？ | 消えません。SIGHUP のたびに適用し直されます。同じキーに `-o`/`VEIL_OVERRIDE_<n>` があるとそちらが優先されます |
