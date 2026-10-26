# veil の API アプリケーションを本番で動かす

[← ドキュメント目次](README.md) · [English](../library-production.md) · [API サーバライブラリ](library.md)

ライブラリの既定は開発向けです: 待ち受けは `127.0.0.1:8080` だけ、要求ごとにアクセスログを標準出力へ 1 行書き、
`/openapi.json`・`/docs` で OpenAPI と Swagger UI を出します。このページは本番で変えるものを Rust と Python について
まとめます（C の API も同じキーを読みます）。ここに書くのはすべて設定のキーか文書にある API で、検証やエラー処理を
外すものはありません。

完全な例は [examples/api/production/](../../../examples/api/production/) です: 1 つの `config.toml` を Rust のプログラム
（`src/main.rs`・`Cargo.toml`）と Python のプログラム（`app.py`）が読みます。`tests/api/library_e2e.sh production` が両方を起動して、
下の設定を確かめます。

## 推奨の設定

設定ファイルの中のコメントは、例のファイルと同じ英語のままにしています（試験が文書と例の一致を確かめます）。

<!-- not-a-full-config -->
```toml
# Recommended production settings for a veil API application (docs/guide/library-production.md).
# The Rust (src/main.rs) and Python (app.py) examples both load this file. Keys left out keep their defaults.

[server]
listen_cleartext = "0.0.0.0:8080"    # reachable from other hosts and containers (the library default is 127.0.0.1)
enable_secure_redirect = false       # serve plaintext here; TLS is terminated in front, or add a TLS listener
# threads = 0                        # default: one worker per CPU the process may use (affinity and cgroup quota)
graceful_shutdown_timeout_secs = 30  # SIGTERM: stop accepting, finish requests in flight for up to 30 s

[logging]
level = "info"
format = "json"                      # one JSON object per line for a log collector

[access_log]
enabled = false                      # the library default writes one line per request to stdout

[api]
openapi_enabled = false              # no /openapi.json and /docs on the public listener
compression = true                   # br / gzip for JSON and text responses of 256 bytes or more
max_body_size = 1048576              # 1 MiB request bodies
websocket_ping_interval_secs = 30    # keep idle WebSockets alive through proxies and NAT
websocket_idle_timeout_secs = 120    # close WebSockets that sent nothing for 2 minutes

[prometheus]
enabled = true
path = "/metrics"
allowed_ips = ["127.0.0.1", "::1", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"]

[tls_client]
ca = "system"                        # outgoing TLS (veil::api::client, gRPC channels) trusts the OS store
```

Rust は `Config::from_file("config.toml")`（`Config::load()` は今のディレクトリの `config.toml` を読む）、Python は
`veil.run(config_path="config.toml")`、C は `veil_config_from_file` で読みます。同じキーはコードでも（`ConfigBuilder::set`・
`veil.run(overrides={...})`）、配備ごとに `VEIL_OVERRIDE_<n>='key = value'`（ファイルより優先）でも設定できます。
以下で各項目の理由を書きます。

## 待ち受けのアドレス

- コンテナの中やロードバランサの後ろでは `0.0.0.0`（か 1 つのインタフェースのアドレス）で待ち受けます。既定の
  `127.0.0.1:8080` はコンテナのブリッジ網から届きません。
- プロトコルの違う複数のポートは `[[listener]]`（Rust の `Listener`、Python の `veil.Listener`）。
  [複数のリスナー](library.md#複数のリスナー) を見てください。`[[listener]]` を 1 つでも書くと、既定の
  `127.0.0.1:8080` は開きません。
- veil で TLS を終端するなら、`[tls] cert_path`・`key_path` と TLS のリスナーを足します。HTTP/3 はキー 1 つです:

<!-- not-a-full-config -->
```toml
[tls]
cert_path = "/etc/veil/tls/cert.pem"
key_path = "/etc/veil/tls/key.pem"
auto_reload = true                   # 更新した証明書を再起動なしで使う（SIGHUP でも）

[[listener]]
name = "https"
listen = "0.0.0.0:443"
tls = true
http3 = true                         # UDP の 443 で HTTP/3 も（http3 feature）。QUIC は既定の設定のままでよい

[[listener]]
name = "redirect"
listen = "0.0.0.0:80"
tls = false
secure_redirect = true               # https のリスナーへ 301
```

  更新した証明書は新しいハンドシェイクから使います（開いている接続は古いまま）。ACME（[acme.md](acme.md)）なら
  veil が自分で更新します。

## ワーカーとスレッド

- `[server] threads` は既定のままにします: プロセスが動ける CPU 1 個につきネットワークのワーカー 1 本。この数は CPU の
  affinity（`taskset`・`docker --cpuset-cpus`）と cgroup の CPU のクォータ（`docker --cpus`・Kubernetes の CPU の limits）に
  従うので、2 CPU に絞ったコンテナではワーカーは 2 本です。同じホストの他のプロセスに CPU を残すときだけ設定します。
- `[api] handler_threads` は、ブロッキングする Rust の関数（`route_blocking`）・C のコールバック・Python のハンドラを動かす
  プールの大きさです。既定は CPU 数の 2 倍（最低 4）。ブロッキングするハンドラが I/O を待つ（同期のデータベースの
  ドライバ等）ときに増やします。CPU を使う処理のために増やしても速くなりません。

## ログ

- ライブラリではアクセスログが既定で有効で、要求ごとに標準出力へ 1 行書きます。行の組み立てと書き込みはワーカーが、
  フラッシュはバックグラウンドのスレッドが行います。要求率が高いなら無効にする（`[access_log] enabled = false`）か、
  端末より安い先へ送ります: `format = "json"` と `file_path = "/var/log/app/access.log"`、またはコンテナの実行環境が
  集める標準出力。項目は [observability.md](observability.md) にあります。
  Rust の例の計測（release、ワーカー 2 本、`GET /api/items/7`、64 接続）では、アクセスログでスループットが 15% 下がりました
  （44,543 と 52,277 要求/秒）。Python のハンドラでは差はばらつきの範囲でした（時間の大半はハンドラ自身）。
- `[logging] format = "json"` でアプリケーションのログを機械で読める形にします。`level = "info"` のままで構いません
  （veil は `info` で要求ごとのログを出しません）。

## OpenAPI と Swagger UI

`/openapi.json` と `/docs` は全ルートと引数を説明します。公開の API では無効にし（`[api] openapi_enabled = false`、
Python は `veil.run(openapi=False)`）、役に立つ場所だけで有効にします（例: ステージングで
`VEIL_OVERRIDE_1='api.openapi_enabled = true'`）。Swagger UI のページはブラウザで unpkg.com からスクリプトを読みます。

## 圧縮

`[api] compression = true` で、ハンドラの応答のうち 256 バイト以上の `application/json`・`text/*`・JavaScript・SVG を
br か gzip で圧縮します（クライアントの `Accept-Encoding` で決まる）。[応答の圧縮](library.md#応答の圧縮) を見てください。
既定のレベル（brotli 4・gzip 4）は API の応答に合っています。大きくてあまり変わらない文書を返すルートは、
そのルートの `[route.compression]` で上げられます（`brotli_level`・`gzip_level`）。圧縮済みの応答（画像・アーカイブ）は
型で除かれます。`compression` feature が必要です（`full` に含まれ、Python の wheel にも入っています）。

## 静的ファイル

`App::serve_dir`（Rust）と `veil_kit.staticfiles.StaticFiles`・`app.serve_dir`（Python）は、プロキシのファイル配信
（sendfile・Range・`ETag`）でディレクトリを配ります。事前圧縮した `.br`・`.gz` は使いません。ルートの
`[route.compression]` が HTTP/2・HTTP/3 でファイルをその場で圧縮し、結果をキャッシュします（HTTP/1.1 はそのまま送ります）。
よく読まれるディレクトリではキャッシュを有効にします:

<!-- not-a-full-config -->
```toml
[[route]]
[route.conditions]
path = "/assets/*"
[route.action]
type = "File"
path = "/srv/app/assets"
[route.open_file_cache]
enabled = true
valid_duration_secs = 60
watch = true                         # ディスクの変更にすぐ追従する（下の注を参照）
[route.static_file_cache]
enabled = true
watch = true
```

`watch` は、`valid_duration_secs` を待たずに inotify・kqueue でディスクの変更に追従させます（veil 0.10）。使わないなら、
動いている間にファイルが変わるときは `valid_duration_secs` を短くするか、新しいファイルを新しい名前で置きます。

## Python

- 普通の `def` のハンドラはハンドラ用プールで動きます。小さな計算だけでブロッキングしないハンドラは `offload=False` で
  ネットワークのワーカーの上で直接動かせます（例のヘルスチェック）。
- I/O を待つハンドラ（HTTP の呼び出し・非同期のドライバのデータベース・`asyncio.sleep`）は `async def` にします:
  常駐のイベントループで動き、待つ間スレッドを占有しません。その中でブロッキングする関数を呼ばないでください
  （`await veil.run_in_threadpool(f, ...)` か普通の `def` のハンドラにする）。
- CPU を使う処理はサーバに関係なく GIL で律速されます。`run_in_threadpool` はそれをイベントループの外へ出します。
  サポートする形は「1 プロセスに複数のワーカーのスレッド」です。同じポートで複数のプロセスを動かすのは推奨しません
  （プロセスごとにプール・ループ・メトリクスが別になる）。1 プロセスで足りなければ、別のポート（かコンテナ）で
  複数動かしてロードバランサの後ろに置きます。
- `[api] python_event_loops`（既定 1）は常駐のイベントループの数です。`async def` のハンドラ自体が律速でなければ 1 で
  足ります。増やすと同時に動くコルーチンは増えますが、GIL は 1 つのままです。`python_event_loop = "auto"` は uvloop が
  入っていれば使い（`pip install uvloop`）、asyncio のループより速くなります。`python_event_loop_queue`（65536）は
  ループを待てる呼び出しの数で、超えた要求は 503 になります。
- WebSocket を使うなら、`websocket_ping_interval_secs` と `websocket_idle_timeout_secs` を設定して死んだ接続を閉じます。

## ファイル記述子

開いた接続（と WebSocket）は 1 本につきファイル記述子を 1 つ使います。veil は起動時に soft の上限を hard まで上げますが、
hard の上限は自分では上げられません。プロセスを起動する側で設定します:

- systemd: ユニットに `LimitNOFILE=1048576`。
- Docker: `docker run --ulimit nofile=1048576:1048576 ...`（Compose は `ulimits: nofile: {soft: 1048576, hard: 1048576}`）。
- Kubernetes: コンテナの実行環境の既定（多くは 1048576）が効きます。コンテナの中で `ulimit -Hn` で確かめます。

## 停止

SIGTERM（か SIGINT、`veil::shutdown()`・`veil.shutdown()`）を受けると、veil は接続の受け付けを止め、アイドルの
キープアライブの接続を閉じ、HTTP/2・HTTP/3 では GOAWAY を送り、WebSocket をコード 1001 で閉じ、処理中の要求を
`[server] graceful_shutdown_timeout_secs`（30）まで待ちます。その後も開いている接続は切ります。アップグレードした接続は
停止の開始で `shutdown_signal()` が解決します。Kubernetes では `terminationGracePeriodSeconds` をこの時間より長くします。
Python の `lifespan`・`on_shutdown` はサーバが止まった後に動きます。Windows には SIGTERM が無いので `shutdown()` を使います。

## 外向きの TLS

`veil::api::client`・gRPC のチャネル（Rust と `veil_kit.grpc`）・ヘルスチェック・VDS は、既定では同梱の公開 CA の一覧で
サーバを検証します。`[tls_client] ca = "system"` で OS の信頼ストアを使い（起動時に 1 回読む）、PEM のパスで社内の CA を
足せます: `ca = ["system", "/etc/app/internal-ca.pem"]`。

## メモリのアロケータ

ライブラリでは veil はグローバルアロケータを選ばず、アプリケーションが選びます。Rust のサーバでは、例と同じように
バイナリで veil のアロケータ（か mimalloc）を宣言します:

```rust
#[cfg(unix)]
#[global_allocator]
static GLOBAL: veil::veil_alloc::VeilAlloc = veil::veil_alloc::VeilAlloc;
```

宣言しなければシステムのアロケータです。Python の wheel と C のライブラリはホストのプロセスのアロケータを置き換えません。
[メモリアロケータ](library.md#メモリアロケータ) を見てください。

## ヘルスチェックとメトリクス

- ヘルスチェックは I/O をしない普通のルートで書きます（`ok` を返す `GET /healthz`）。依存先を確かめる必要があるなら、
  別に readiness のルートを作ります。ロードバランサや Kubernetes のプローブをそこへ向けます。
- `[prometheus] enabled = true` は、全リスナーの `path`（既定 `/__metrics`）でメトリクスを返します。`allowed_ips` の
  アドレスだけで、他のクライアントには 403 を返します。要求の数・遅延のヒストグラム・接続数は
  [observability.md](observability.md) にあります。

## セキュリティのサンドボックス

ライブラリでは seccomp と Landlock は既定で無効です（ハンドラが何をするか veil には分からないため）。アプリケーションが
使うファイル・ソケット・DNS が分かったら有効にできます（`[security] enable_seccomp = true`、`enable_landlock = true` と
`landlock_read_paths`・`landlock_write_paths`）。[security.md](security.md) を見てください。
