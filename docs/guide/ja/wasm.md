# WASM 拡張（Proxy-Wasm）

[← ドキュメント目次](README.md) · [English](../wasm.md)

## WASM拡張機能

VeilはProxy-Wasm ABI v0.2.1に完全準拠したWASM拡張システムを提供します。Nginx/Envoy向けに作成されたProxy-WasmモジュールをそのままVeilで使用できます。

### 特徴

- **Proxy-Wasm v0.2.1準拠**: Nginx/Envoyと100%互換
- **AOTコンパイル・自動キャッシュ**: モジュールを AOT コンパイルし、初回ロード時に `.wasm` の隣へ `.cwasm` サイドカーを自動生成。次回起動時はそれを `deserialize` して高速起動する（`.wasm` が新しい場合や wasmtime 版が変わった場合は自動無効化し、エラー時は再コンパイルへフォールバック）。明示的な `.cwasm` パス指定も可。
- **Pooling Allocator**: 高速なインスタンス化
- **非同期実行（Head-of-Line ブロッキングなし）**: wasmtime async support + Fuel ベースの協調的 yield（ゲストの CPU 時間で約 1ms ごと。`yield_interval_us`）で実行。CPU バウンドなフィルタが io_uring ワーカーの他 I/O をストールさせない
- **Capability制限**: モジュールごとの細かい権限制御（デフォルト全て無効）
- **Pulley インタープリタ選択**（`[wasm] interpreter = true`、F-135）: Cranelift ネイティブ JIT の代わりに wasmtime の Pulley ポータブルバイトコードインタープリタで実行し、ネイティブコードを一切生成しない。W^X 制約のあるホストや実行可能 `mmap` を許可できない環境向け。ネイティブ JIT より低速。既定は `false`（Cranelift JIT）。AOT サイドカーキャッシュはファイル名を分離（`.pulley.cwasm`）し、JIT 版と Pulley 版が同じキャッシュを奪い合って毎回再コンパイルする事態を避ける。**OpenBSD（全アーキ）・NetBSD（全アーキ）・FreeBSD aarch64 ではこの設定は常に無視され、常に Pulley が使われる**（B-52/B-55。`interpreter = false` を明示指定すると起動時に警告ログが出るが無視される）
- **HTTP/3 が HTTP/1.1・HTTP/2 と同等のライフサイクルに対応**（F-132）: HTTP/3 経路でも `on_log` がすべての離脱点（`LocalResponse` による早期 return を含む）から確実に呼ばれるようになり、`Backend::File`（静的配信）ルートにも `on_response_headers` フィルタが適用される。`Backend::Proxy` ルートではリクエスト/レスポンスボディフィルタ（`on_request_body`/`on_response_body`）も動作する（HTTP/3 は WASM 適用時に本文全体を既にバッファしているため `end_of_stream=true` の1回呼びで実装）。レスポンス本文を書き換えた場合は `content-length` を新しい長さへ更新し、nghttp3 の `H3_MESSAGE_ERROR`（本文長不一致・重複)を回避する。
- **gRPC トレイラーフィルタ**（F-133）: gRPC over H2C で `on_request_trailers`/`on_response_trailers` が実行されるようになり、レスポンストレイラーの `grpc-status`/`grpc-message` を書き換えたり、クライアント送信のリクエストトレイラーを見て `LocalResponse` でリクエストを拒否できる。リクエストトレイラーは観測のみ（バックエンドへの転送機構は無い）。レスポンストレイラーの `Pause`/`LocalResponse` は HEADERS/DATA が送出済みのため適用できず、元のトレイラーをそのまま通して警告ログを出す。
- **L4 network filter（Proxy-Wasm `StreamContext` ABI）**（F-133）: `[[l4]]` TCP リスナーに `wasm_modules = ["name"]` を指定すると、生バイト列を検査・書き換えできる（`proxy_on_new_connection`、`proxy_on_downstream_data`、`proxy_on_upstream_data`、`proxy_on_downstream_connection_close`、`proxy_on_upstream_connection_close`）。データは `BufferType::DownstreamData`/`UpstreamData`（値 `2`/`3`）として `proxy_get_buffer_bytes`/`proxy_set_buffer_bytes` 経由でアクセスし、`proxy_close_stream` で接続をクローズできる。**未設定時はコスト増なし**: `wasm_modules` が空（既定）なら従来どおり `splice`/ゼロコピー経路のまま `is_empty()` 判定 1 つだけが追加コスト。モジュールを設定すると（接続確立時に1回だけ）`splice` を使わないユーザー空間バッファ経由の転送へ切り替わる（WASM にデータを見せる必要があるため）。

### ビルド

```bash
cargo build --release --features wasm
```

### 設定

```toml
[wasm]
enabled = true

# Cranelift ネイティブ JIT の代わりに Pulley インタープリタで実行する（F-135）。
# デフォルト: false。OpenBSD では常に true 相当（B-52、この設定値は無視される）。
# interpreter = false

# デフォルト設定（オプション）
[wasm.defaults]
# 最大実行時間（ミリ秒、デフォルト: 100）
max_execution_time_ms = 100

  # Poolingアロケータ設定
  [wasm.defaults.pooling]
  # メモリプール総数（デフォルト: 128）
  total_memories = 128
  # テーブルプール総数（デフォルト: 128）
  total_tables = 128
  # インスタンスごとの最大メモリサイズ（デフォルト: 10MB）
  max_memory_size = 10485760

# モジュール定義
[[wasm.modules]]
name = "my_filter"
path = "/etc/veil/wasm/my_filter.wasm"
configuration = '{"key": "value"}'

[wasm.modules.capabilities]
# 全てデフォルトfalse、必要な権限のみ有効化
allow_logging = true
allow_request_headers_read = true
allow_request_headers_write = true
allow_send_local_response = true
allow_http_calls = true
allowed_upstreams = ["webdis"]  # HTTP呼び出し許可先
```

**注意**: 特定のルートにWASMモジュールを適用するには、ルート設定の`modules`フィールドを使用してください（ルーティングセクションを参照）。

### プラグイン設定の記述方法（F-148）

`configuration` は、モジュールへ Proxy-Wasm の *plugin configuration*
（`proxy_on_configure` / `proxy_get_buffer(PluginConfiguration)`）として渡されるバイト列です。
**文字列**（従来方式。バイト列をそのまま渡す。多くは JSON）と
**TOML テーブル**（JSON オブジェクトへ変換してから渡すため、モジュール側の実装は変更不要）の
どちらでも記述できます。

```toml
# 1. 文字列形式（従来どおり。バイト列は 1 バイトも変わらない）
[[wasm.modules]]
name = "header_filter"
path = "/etc/veil/wasm/header_filter.wasm"
configuration = '{"header_name": "x-veil"}'

# 2. TOML テーブル形式（JSON オブジェクトへ変換して渡す）
[[wasm.modules]]
name = "waf_filter"
path = "/etc/veil/wasm/waf_filter.wasm"
[wasm.modules.configuration]
mode = "block"
max_body_size = 65536
patterns = ["union select", "<script"]
```

TOML → JSON の変換規則: String → string、Integer/Float → number（`NaN`/`Inf` は `null`）、
Boolean → bool、Datetime → RFC 3339 相当の string、Array → array、Table → object。

#### ルート単位の上書き

`[[route]]`（および `[[l4]]`）に `module_configuration`（モジュール名 → 設定値のマップ）を
書くと、**同じモジュールをルートごとに違うパラメータで使えます**
（同じ `.wasm` を別名で二重にロードする必要がありません）。

```toml
[[route]]
modules = ["header_filter", "waf_filter"]

# このルートだけ WAF を log_only にする（他のルートはモジュール定義の "block" のまま）
[route.module_configuration.waf_filter]
mode = "log_only"

[route.conditions]
path = "/static/*"
[route.action]
type = "File"
path = "./www"
```

合成規則（ルート優先）:

| モジュール定義 | ルート | 実効設定 |
|---|---|---|
| なし | なし | 空バイト列 |
| あり | なし | モジュール定義の値 |
| なし | あり | ルートの値 |
| Table | Table | **ディープマージ**（同名キーはルート優先、ネストしたテーブルも再帰マージ、配列は置換） |
| 上記以外（片方でも文字列） | あり | ルートの値で**完全置換**（文字列は不透明なバイト列でマージ不能なため） |

`module_configuration` に、そのルートの `modules`（L4 なら `wasm_modules`）へ
列挙していないモジュール名を書くと**起動時に設定エラー**になります。

合成と TOML → JSON 変換は**設定ロード時（起動・SIGHUP リロード）に 1 回だけ**実行され、
リクエスト経路では `Arc` の clone しか行いません。

### プロトコルごとのフィルタのフェーズ

| フェーズ | HTTP/1.1 | HTTP/2 | HTTP/3 |
|---|---|---|---|
| `on_http_request_headers` | ○ | ○ | ○ |
| `on_http_request_body` | ○ | ○ | ○ |
| `on_http_request_trailers` | – | h2c の gRPC | – |
| `on_http_response_headers` | ○ | ○ | ○ |
| `on_http_response_body` | ○ | ○ | ○ |
| `on_log` | ○ | ○ | ○ |

要求のフェーズと `on_http_response_headers` のローカル応答（`send_local_response`）は、上流の応答の代わりに返す（上流の応答の残りは
読まず、その上流の接続は使い回さない）。`on_http_response_trailers` のローカル応答は、応答のヘッダを送った後なので送れない（元の
トレーラーのまま）。ローカル応答の後も `on_log` は呼ぶ。

要求本文のフェーズは、要求に本文があり、ルートのモジュールのどれかが `allow_request_body_read = true` のときに走る。
veil は本文を全部読んでから（Content-Length・chunked。上限は `max_request_body_size`・`max_chunked_body_size`、超えたら
`413`）`on_http_request_body` を `end_of_stream = true` で 1 回呼ぶ。コールバックからは要求のパス・メソッド・ヘッダも読める。
モジュールが本文を書き換えたら、新しい本文を合った `Content-Length` で上流へ送る（chunked の要求も `Content-Length` で送り、
`Expect` は外す）。そういうモジュールの無いルートは、今までどおり本文を上流へ流す。`Expect: 100-continue` の chunked の要求には、
本文を読む前に `100 Continue` を返す（`Content-Length` の要求にはフィルタより前に返している）。

応答本文のフェーズは、ルートのモジュールのどれかが `allow_response_body_read = true` のときに走る。veil は上流の応答を全部読んでから
（上流は HTTP/1.1・h2c・HTTPS。HTTP/1.1 のクライアントではルートの `[route.buffering] max_memory_buffer`〈既定 10MB〉まで。超えたら `507`、
`disk_buffer_path` があればフィルタを通さずに送る）`on_http_response_body` を `end_of_stream = true` で 1 回呼ぶ。書き換えた本文は
合った `Content-Length` で送り、`[route.compression]` はフィルタの後の本文を圧縮する。このフェーズのローカル応答は上流の応答の代わりに返す。
`HEAD`・`1xx`・`204`・`304`・gRPC の応答では呼ばない。そういうルートの応答は `[route.cache]` に保存しない（キャッシュに当たるとフィルタを
通らないため）。そういうモジュールの無いルートは今までどおり応答を流す。

HTTP/1.1 では、フィルタのローカル応答（`send_local_response`）の後も接続を続け、本文の後ろにパイプラインで届いた要求を
続けて処理する。ローカル応答の本文は、`HEAD` の要求（`Content-Length` は本文の長さのまま）と `1xx`・`204`・`304` では送らない。

### VM の文脈の使い回し（`instance_reuse`）

```toml
[[wasm.modules]]
name = "baseline"
path = "/etc/veil/wasm/baseline_filter.wasm"
instance_reuse = true   # 既定 false
```

既定ではコールバックごとに新しいインスタンスで動く（`_start`・`proxy_on_vm_start`・`proxy_on_configure` が毎回走る）。
`instance_reuse = true` なら、インスタンスを 1 回作って設定し、後の要求で使い回す。要求ごとは HTTP context の作成
（`proxy_on_context_create`）とコールバックだけで、要求が終わると `proxy_on_delete` が走る（Envoy と同じ寿命）。
1 つの要求のフェーズ（要求ヘッダ・要求本文・応答ヘッダ・応答本文・`on_log`）はすべて同じ HTTP context で呼ぶので、
フィルタが HTTP context に持つ状態はフェーズをまたいで残る。1 つのインスタンスに複数の要求の HTTP context が同時に住む
（Envoy のワーカーの VM と同じ）。要求が `on_log` の前に終わったら（クライアントの切断等）、その context はインスタンスを
次に使うときに消す。フィルタのグローバル変数と root context の状態は要求をまたいで残る。トラップ・エラーを返した
インスタンスは捨て、次の要求は新しいインスタンスを使う。そこに context を持っていた要求の以後のフェーズは新しい
HTTP context で呼ぶ（フィルタは飛ばさないが、前の context に持っていた状態は失われる）。ワーカーはモジュールごと（とルートの `module_configuration` ごと）に
設定済みのインスタンスを持つ。上限はプーリングアロケータの `total_memories` の半分を、ワーカーと `instance_reuse` の
モジュールで分けた数。

### フィルタだけで要求に答える（`Respond` + モジュール）

`type = "Respond"` のルートにも `modules` を付けられる。フィルタが `send_local_response` で答えればその応答を返し、
答えなければルートの固定応答を返す。上流の無い、フィルタだけで答えるルートを作れる。

```toml
[[route]]
modules = ["baseline"]
[route.conditions]
path = "/baseline11"
[route.action]
type = "Respond"
status = 404
```

### 起動時に読むファイル（`data_files`）

```toml
[[wasm.modules]]
name = "baseline"
path = "/etc/veil/wasm/baseline_filter.wasm"
data_files = { dataset = "/etc/veil/data/dataset.json" }
```

各ファイルは設定のロード時（起動・リロード）に読み、プロパティ `plugin_data.<名前>` としてフィルタに見せる
（`get_property(["plugin_data", "dataset"])`）。要求ごとのディスクの読み取りは無い。ファイルが無ければ設定のエラー。
モジュールに `allowed_properties` を書くなら `"plugin_data.*"` を含める。

### デフォルト設定

`[wasm.defaults]` セクションでは、WASMランタイムのグローバル設定を行えます：

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `max_execution_time_ms` | 1 回のコールバックの CPU の量（ミリ秒。fuel で数え、1ms は約 100 万）。新しいインスタンスの起動時のコールバック（`_start`・`proxy_on_vm_start`・`proxy_on_configure`）は別の枠 | 100 |
| `yield_interval_us` | 実行中のコールバックが同じワーカーの他の要求へ制御を返す間隔（マイクロ秒の目安。fuel で数える）。小さくすると CPU を長く使うフィルタの後ろで待つ他の要求の待ちが短くなるが、どのコールバックも頻繁に止まり、`instance_reuse` のインスタンスが同時に多く開き、要求ごとの費用が増える | 1000 |
| `interpreter` | Cranelift ネイティブ JIT の代わりに Pulley インタープリタで実行する（F-135）。OpenBSD では常に `true` 相当（B-52） | false |
| `grpc_connect_timeout_ms` | `proxy_grpc_call`・`proxy_grpc_stream` で gRPC の上流へ接続する（TCP と TLS）時間の上限。ゲストの `timeout_ms` が長くても接続はこの時間で諦める（到達できない上流で全プラグインの gRPC が止まらないように）。gRPC のスレッドが取り込む前の呼び出し・ストリームのメッセージはそれぞれ 4096 件までで、超えるとホスト関数は `InternalFailure` を返す | 5000 |
| `http_call_max_timeout_ms` | `proxy_http_call` にプラグインが渡す `timeout_ms` の上限（負の値は 0 とみなす） | 60000 |
| `shared_data_max_keys` | 1 モジュールの shared data のキーの数の上限。超える新しいキーの `proxy_set_shared_data` は `InternalFailure` を返す（既存のキーは更新できる） | 100000 |
| `failure_policy` | フィルタが失敗したときの扱い。`fail_closed`・`fail_open`・`fail_reload`（[失敗の扱い](#失敗の扱い)）。`[[wasm.modules]]` の `failure_policy` で上書きできる | `fail_closed` |

### 失敗の扱い

次のときフィルタは失敗する: コールバックがトラップした（SDK の panic・`proc_exit`・`unreachable`）、CPU の量（`max_execution_time_ms`）を
使い切った、他の要求へ譲っていた時間を含めて約 1 秒の実時間を超えた、インスタンスを作れない（`total_memories` の枠が約 130ms 埋まったまま）、
新しいインスタンスの起動時のコールバックが失敗した、Pause した要求を再開する `proxy_on_http_call_response` がトラップした、待てるものの無い
`Pause` を返した（下記）。要求の扱いは `failure_policy` で決まる:

| `failure_policy` | 要求 | インスタンス |
|---|---|---|
| `fail_closed`（既定） | `503`（gRPC のクライアントには `UNAVAILABLE`） | 捨てて、次の要求は新しいインスタンスで |
| `fail_open` | 失敗したモジュールを飛ばし、次のモジュールへ続ける。失敗する前にモジュールが変えたヘッダは残る | 捨てる |
| `fail_reload` | `503` | 捨てて、そのモジュールの使い回しているインスタンス（`instance_reuse`）も次に使う前に作り直す |

Envoy の `FAIL_CLOSED` と違い、1 つの要求の失敗で後の要求は失敗しない（次の要求は必ず正常なインスタンスで始める）。失敗する前に
フィルタが設定したローカル応答は、ポリシーに関係なく返す。失敗は `veil_wasm_failures_total{filter, phase, reason, action}`
（`reason` は `trap`・`fuel`・`timeout`・`instantiate`・`lifecycle`・`http_call_resume`・`unsupported_pause`、`action` は `rejected`・
`continued`）で数え、警告をログに出す（頻度は抑える）。

`Pause` は、フィルタが待つものを veil が待てるときに守る: どのフェーズの `dispatch_http_call` でもその場で解決する
（[外部サービス連携](#外部サービス連携http呼び出し)）。`on_http_request_headers` が本文の前（`end_of_stream = false`）に `Pause` を返し、
モジュールが本文を読む（`allow_request_body_read`）なら本文のフェーズで続きを決める（応答も同じ）。`proxy_grpc_call`・`proxy_grpc_stream` を
待つ `Pause` や、待つものの無い `Pause` は `unsupported_pause` の失敗になる。`dispatch_http_call` の後は、
`proxy_on_http_call_response` が要求を再開する（`resume_http_request`、つまり `proxy_continue_stream`）かローカル応答を返したときだけ
要求を続ける。どちらもしないフィルタや、`max_http_calls` 回を過ぎてもコールが残るフィルタは Pause のままで、上と同じに扱う
（0.11.0 より前は続けていた）。WebSocket のアップグレード（HTTP/1.1 と HTTP/2・HTTP/3 の拡張 CONNECT）は本文の段を走らせないので、
`on_http_request_headers` には `end_of_stream = true` を渡す。

L4 のネットワークフィルタ（`[[l4]] wasm_modules`）も同じ `failure_policy` に従う: `fail_closed`（既定）では
`proxy_on_new_connection`・`proxy_on_downstream_data`・`proxy_on_upstream_data` が失敗すると接続を閉じる（`phase` は
`new_connection`・`downstream_data`・`upstream_data`）。`fail_open` ではそのモジュールを飛ばす。
`proxy_on_downstream_data`・`proxy_on_upstream_data` が `Pause` を返すと、veil はそのバイトを転送せずに持ち、次に読んだ
バイトを後ろに付けて次の呼び出しでもう一度渡す。持つ量が 1 MiB を超えると接続を閉じる。0.11.0 より前の L4 のフィルタは
失敗しても常に続け、`Pause` でもバイトを転送していた。

起動時とリロード時に、各モジュールの `_start`・`proxy_on_vm_start`・`proxy_on_configure` を 1 回走らせる。トラップしたり `false` を
返したり（プラグインが設定を拒否した等）したら設定を拒否する（起動は失敗し、リロードは前の設定のまま）。

リロードは新しい `[wasm]` からモジュールを読み直すので、リロードで足した・変えたモジュール（と `data_files`）は次の要求から効く
（処理中の要求は前のモジュールのまま終わる）。ルートと `[[l4]]` のリスナーが挙げられるのは、有効な `[wasm]` で定義したモジュール
だけで、それ以外の名前は設定を拒否する。FreeBSD の capability mode では起動後に `.wasm` を開けないので、`[wasm]` を変える
リロードは拒否する（変更は再起動で反映する）。

0.11.0 より前は失敗しても常に続けていた（今の `fail_open` と同じ。メトリクスは無かった）。`[wasm.defaults]` に
`failure_policy = "fail_open"` を書くと以前と同じになる。

#### Poolingアロケータ設定

`[wasm.defaults.pooling]` セクションでは、高速インスタンス化のためのPoolingアロケータを設定します：

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `total_memories` | メモリプール総数 | 128 |
| `total_tables` | テーブルプール総数 | 128 |
| `max_memory_size` | インスタンスごとの最大メモリサイズ（バイト） | 10MB (10485760) |

### Capability一覧

| Capability | 説明 | デフォルト |
|-----------|------|----------|
| `allow_logging` | ログ出力 | false |
| `allow_metrics` | メトリクス操作 | false |
| `allow_shared_data` | 共有データ（`proxy_set_shared_data`・`proxy_get_shared_data`）。モジュール名ごとに 1 つの表を全要求・全ワーカーで共有し、設定のリロードでも引き継ぐ（Envoy の `vm_id` ごとの共有に当たる）。共有キューにも要る: キューは登録したモジュールのもので、取り出せるのはそのモジュールだけ。他のモジュールは持ち主のモジュール名（`vm_id`）でキューを引いてから積む | false |
| `allow_request_headers_read` | リクエストヘッダー読み取り | false |
| `allow_request_headers_write` | リクエストヘッダー書き換え | false |
| `allow_request_body_read` | リクエストボディ読み取り | false |
| `allow_request_body_write` | リクエストボディ書き換え | false |
| `allow_response_headers_read` | レスポンスヘッダー読み取り | false |
| `allow_response_headers_write` | レスポンスヘッダー書き換え | false |
| `allow_response_body_read` | レスポンスボディ読み取り | false |
| `allow_response_body_write` | レスポンスボディ書き換え | false |
| `allow_downstream_data_read` | L4 downstream（クライアント→proxy）接続データ読み取り（F-133） | false |
| `allow_downstream_data_write` | L4 downstream 接続データ書き換え（F-133） | false |
| `allow_upstream_data_read` | L4 upstream（proxy→バックエンド）接続データ読み取り（F-133） | false |
| `allow_upstream_data_write` | L4 upstream 接続データ書き換え（F-133） | false |
| `allow_send_local_response` | ローカルレスポンス送信 | false |
| `allow_http_calls` | HTTP外部呼び出し | false |
| `allowed_upstreams` | `proxy_http_call`・`proxy_grpc_call` が呼べる上流。空なら全上流グループを許す | [] |

capability の無いホスト関数は `NotAllowed` を返す。その状態を unwrap する SDK（Rust の `proxy-wasm` は多くの呼び出しでそうする）は
panic し、コールバックがトラップして要求は `failure_policy` で扱われる（既定は `503`）。veil は、`proxy_send_local_response`・
`proxy_http_call`・`proxy_grpc_call`/`proxy_grpc_stream`・`proxy_define_metric`・shared data の関数・`proxy_set_property` を import するのに
対応する capability が無いモジュールを、ロード時に警告する。

### Rustによる拡張機能開発

#### 1. プロジェクト作成

```bash
cargo new --lib my-filter
cd my-filter
```

#### 2. Cargo.toml

```toml
[package]
name = "my-filter"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
proxy-wasm = "0.2"
log = "0.4"

[profile.release]
lto = true
opt-level = "s"

[workspace]
```

#### 3. src/lib.rs

```rust
use proxy_wasm::traits::*;
use proxy_wasm::types::*;

proxy_wasm::main! {{
    proxy_wasm::set_log_level(LogLevel::Debug);
    proxy_wasm::set_root_context(|_| -> Box<dyn RootContext> {
        Box::new(MyFilterRoot)
    });
}}

struct MyFilterRoot;

impl Context for MyFilterRoot {}

impl RootContext for MyFilterRoot {
    fn get_type(&self) -> Option<ContextType> {
        Some(ContextType::HttpContext)
    }

    fn create_http_context(&self, context_id: u32) -> Option<Box<dyn HttpContext>> {
        Some(Box::new(MyFilter { context_id }))
    }
}

struct MyFilter {
    context_id: u32,
}

impl Context for MyFilter {}

impl HttpContext for MyFilter {
    fn on_http_request_headers(&mut self, _: usize, _: bool) -> Action {
        // リクエストヘッダーにカスタムヘッダーを追加
        self.add_http_request_header("X-My-Filter", "enabled");
        
        // ヘッダー値を取得
        if let Some(path) = self.get_http_request_header(":path") {
            log::info!("Request path: {}", path);
        }
        
        Action::Continue
    }

    fn on_http_response_headers(&mut self, _: usize, _: bool) -> Action {
        // レスポンスヘッダーを追加
        self.add_http_response_header("X-Processed-By", "my-filter");
        Action::Continue
    }
}
```

#### 4. ビルド

```bash
# WASIターゲットを追加
rustup target add wasm32-wasip1

# ビルド
cargo build --target wasm32-wasip1 --release

# 出力: target/wasm32-wasip1/release/my_filter.wasm
```

#### 5. 配置と設定

```bash
# WASMモジュールを配置
cp target/wasm32-wasip1/release/my_filter.wasm /etc/veil/wasm/

# config.tomlに設定を追加
```

### 外部サービス連携（HTTP呼び出し）

Proxy-Wasmの`dispatch_http_call`を使用して外部HTTPサービス（Redis用Webdis等）を呼び出せます。
フィルタが呼び出し後に（HTTP のどのフェーズでも）`Action::Pause`を返すと、Veilはリクエスト経路上で上流コールを
インライン解決し（ブロッキングHTTPクライアントは専用オフロードスレッドで実行するため
イベントループはブロックしません）、`proxy_on_http_call_response`で同一WASMインスタンスを
再開します。動作例は`examples/wasm-filters/http-call-filter/`にあります。

```rust
fn on_http_request_headers(&mut self, _: usize, _: bool) -> Action {
    // Webdis経由でRedisにアクセス
    self.dispatch_http_call(
        "webdis",  // upstream名（config.tomlで定義）
        vec![
            (":method", "GET"),
            (":path", "/GET/my_key"),
            (":authority", "webdis"),
        ],
        None,
        vec![],
        Duration::from_millis(50),
    ).unwrap();
    
    Action::Pause  // レスポンス待ち
}

fn on_http_call_response(&mut self, _: u32, _: usize, body_size: usize, _: usize) {
    if let Some(body) = self.get_http_call_response_body(0, body_size) {
        // Redisからの値を処理
        log::info!("Redis response: {:?}", body);
    }
    self.resume_http_request();
}
```

Veil は呼び出しを HTTP/1.1 で送り、`Host`・`Content-Length`・`Connection` は Veil が付けます（フィルタが渡した
`host`・`content-length`・`transfer-encoding`・`connection` は捨てます）。`:method`・`:path`・ヘッダの名前と値に
要求を分けられる文字（CR・LF・NUL、`:path` の空白、token でない名前）があると送らず、フィルタには
`x-wasm-error: http_call_failed` 付きの `504` が返ります。本文はバイト列のまま送ります。
