# 設定リファレンス

[← ドキュメント目次](README.md) · [English](../configuration.md)

## 設定

デフォルトでは `/etc/veil/config.toml` を読み込みます。
`-c` または `--config` オプションで別のパスを指定できます。

### デフォルト値一覧

主要な設定項目のデフォルト値を以下に示します：

| セクション | 項目 | デフォルト値 | 説明 |
|-----------|------|-------------|------|
| `[server]` | `server_header_enabled` | `false` | Serverヘッダーを有効化 |
| `[server]` | `server_header_value` | `"veil"` | Serverヘッダーの値 |
| `[server]` | `http2_enabled` | `false` | HTTP/2を有効化 |
| `[server]` | `listen` | なし | TLS リスナー（HTTP/1.1 + h2 ALPN）。`listen` / `listen_cleartext` の少なくとも一方が必須（[リスナー](#リスナーlisten-と-listen_cleartext)を参照） |
| `[server]` | `listen_cleartext` | なし | 平文リスナー（HTTP/1.1 + h2c をプロトコル検出で判別） |
| `[server]` | `enable_secure_redirect` | `true` | `listen_cleartext` の要求をすべて HTTPS へ 301 で返す（`listen` が要る。`false` で平文のまま配信） |
| `[server]` | `allow_http` | `true` | `listen_cleartext` で平文 HTTP/1.1 を受ける（false なら 426） |
| `[server]` | `allow_h2c` | `true` | `listen_cleartext` で h2c を受ける（false なら GOAWAY） |
| `[http3]` | `enabled` | `false` | `listen` と同じアドレス（UDP）、または `[http3].listen` で HTTP/3 を有効化 |
| `[[listener]]` | `tls` | 必須 | 待ち受けのポートを足す。`true` = TLS、`false` = 平文（[複数のリスナー](#複数のリスナーlistener)を参照） |
| `[[listener]]` | `http2` | `[server].http2_enabled` | TLS: ALPN で h2 を出す |
| `[[listener]]` | `secure_redirect` | `false` | 平文: HTTPS へ 301 で返す（`[server].enable_secure_redirect` の既定は `true` なので注意） |
| `[logging]` | `level` | `"info"` | ログレベル |
| `[logging]` | `format` | `"text"` | ログ形式 |
| `[logging]` | `channel_size` | `16384` | ログチャネルバッファサイズ（全スロットを起動時に確保して常駐。1 件 約 88 バイト。満杯時はブロックせず破棄） |
| `[logging]` | `flush_interval_ms` | `1000` | フラッシュ間隔（ミリ秒） |
| `[vds]` | `enabled` | `false` | 制御プレーンからルート・アップストリームを受け取る（[VDS](vds.md)を参照） |
| `[acme]` | `enabled` | `false` | 証明書の自動発行・更新（[ACME](acme.md)を参照） |
| `[prometheus]` | `enabled` | `false` | Prometheusメトリクスを有効化 |
| `[prometheus]` | `path` | `"/__metrics"` | メトリクスエンドポイントパス |
| `[performance]` | `reuseport_balancing` | `"kernel"` | SO_REUSEPORT振り分け方式 |
| `[performance]` | `huge_pages_enabled` | `false` | Huge Pagesを有効化 |
| `[performance]` | `keepalive_direct_read_ms` | `20` | io_uring・平文 HTTP/1.1 のキープアライブで、次の要求をこの時間だけ直接読みにいき、その後はバッファを持たずに待つ（`0` = 常に先に待つ。F-205） |
| `[performance]` | `open_file_cache_enabled` | `false` | OpenFileCacheを有効化 |
| `[performance]` | `open_file_cache_valid_duration_secs` | `60` | キャッシュ有効期間（秒） |
| `[performance]` | `open_file_cache_max_entries` | `10000` | 最大キャッシュエントリ数 |
| `[performance]` | `open_file_cache_watch` | `false` | ディスクの変更を通知で追従する（ルートの既定値。[圧縮・キャッシュ](compression-cache.md#静的配信のキャッシュがディスクの変更に追従する)） |
| `[static_file_cache]` | `enabled` | `false` | 静的ファイル本体キャッシュ（F-146/F-150。HTTP/2・HTTP/3、および kTLS 無効時の HTTP/1.1）を有効化 |
| `[static_file_cache]` | `valid_duration_secs` | `60` | キャッシュ有効期間（秒） |
| `[static_file_cache]` | `max_entries` | `1024` | 最大キャッシュエントリ数 |
| `[static_file_cache]` | `max_file_size_bytes` | `1048576` | キャッシュ対象の最大ファイルサイズ（バイト） |
| `[static_file_cache]` | `max_total_bytes` | `67108864` | キャッシュ全体の最大バイト数 |
| `[static_file_cache]` | `revalidate_mtime` | `false` | ヒット時に mtime を再検証するか（true でホットパスに syscall が復活） |
| `[static_file_cache]` | `watch` | `false` | ディスクの変更を通知で追従する（ヒット時は原子変数の読み取り 1 回。ルートの既定値） |
| `[static_file_cache]` | *(専用キー無し)* | — | F-169: 本セクションが有効なとき、File ルート静的配信の圧縮結果（パス/エンコーディング/レベル単位）もキャッシュする。上限は本セクションの `max_entries`/`max_total_bytes` を共用し、本体キャッシュのエントリと連動して無効化される |
| `[tls]` | `ktls_enabled` | `false` | kTLSを有効化 |
| `[tls]` | `ktls_fallback_enabled` | `true` | kTLS失敗時のrustlsフォールバック |
| `[tls]` | `tcp_cork_enabled` | `true` | TCP_CORKを有効化 |
| `[tls]` | `cipher_suites` | `[]`（rustls 既定） | 許可する TLS 暗号スイート（nginx の `ssl_ciphers` 相当。記載順 = サーバ優先度順。不正名は起動エラー。詳細は examples/config.toml 参照） |
| `[tls]` | `auto_reload` | `false` | 証明書の自動リロード（mtime 検知 + SIGHUP） |
| `[tls]` | `reload_interval_secs` | `60` | 証明書変更チェック間隔（秒） |
| `[buffer_pool]` | `read_buffer_size` | `65536` | 読み込みバッファサイズ（64KB） |
| `[buffer_pool]` | `initial_read_buffers` | `32` | 読み込みバッファ初期数 |
| `[buffer_pool]` | `max_read_buffers` | `128` | 読み込みバッファ最大数 |
| `[buffer_pool]` | `request_buffer_size` | `1024` | リクエストバッファサイズ（1KB） |
| `[buffer_pool]` | `initial_request_buffers` | `16` | リクエストバッファ初期数 |
| `[buffer_pool]` | `large_request_buffer_size` | `4096` | 大容量リクエストバッファ（4KB） |
| `[http2]` | `header_table_size` | `65536` | HPACKテーブルサイズ（64KB） |
| `[http2]` | `max_concurrent_streams` | `256` | 最大同時ストリーム数 |
| `[http2]` | `initial_window_size` | `1048576` | ストリームウィンドウサイズ（1MB） |
| `[http2]` | `max_frame_size` | `65536` | 最大フレームサイズ（64KB） |
| `[http2]` | `max_header_list_size` | `65536` | 最大ヘッダーリストサイズ（64KB） |
| `[http2]` | `connection_window_size` | `1048576` | コネクションウィンドウ（1MB） |
| `[http2]` | `max_rst_stream_per_second` | `100` | RST_STREAMレート制限 |
| `[http2]` | `max_control_frames_per_second` | `500` | 制御フレームレート制限 |
| `[http2]` | `max_continuation_frames` | `10` | 最大CONTINUATIONフレーム数 |
| `[http2]` | `max_header_block_size` | `65536` | 最大ヘッダーブロック（64KB） |
| `[http2]` | `stream_idle_timeout_secs` | `60` | ストリームアイドルタイムアウト（秒） |
| `[http3]` | `max_idle_timeout` | `30000` | 最大アイドルタイムアウト（ミリ秒、30秒） |
| `[http3]` | `max_udp_payload_size` | `1452` | 最大UDPペイロードサイズ。経路 MTU の探索の上限（v0.8.0 より前は `1350`。1350 固定だと 54KB の応答に 42 パケット（nginx 39）で、Linux で 6.7% 遅かった） |
| `[http3]` | `discover_pmtu` | `true` | 経路 MTU の探索（quiche、RFC 8899 の DPLPMTUD）。1200 バイトから始め、探索のパケットが届いた大きさだけを使うので、MTU の小さい経路でも動く。UDP のソケットに DF を立てる（Linux は `IP_PMTUDISC_PROBE`、BSD・macOS は `IP_DONTFRAG`/`IPV6_DONTFRAG`） |
| `[http3]` | `initial_max_data` | `10000000` | 初期最大データ（10MB） |
| `[http3]` | `initial_max_stream_data_bidi_local` | `1000000` | ストリームデータ双方向ローカル（1MB） |
| `[http3]` | `initial_max_stream_data_bidi_remote` | `1000000` | ストリームデータ双方向リモート（1MB） |
| `[http3]` | `initial_max_stream_data_uni` | `1000000` | ストリームデータ単方向（1MB） |
| `[http3]` | `initial_max_streams_bidi` | `100` | 最大双方向ストリーム数 |
| `[http3]` | `initial_max_streams_uni` | `100` | 最大単方向ストリーム数 |
| `[http3]` | `cc_algorithm` | `"cubic"` | QUIC 輻輳制御（`reno`/`cubic`/`bbr`/`bbr2`/`bbr2_gcongestion`）。v0.8.0 より前の既定は `"bbr"`。veil は quiche の pacing の送出時刻まで待たずに送るので、BBR は送出が一気になって受け手で捨てられ再送が増えていた（CUBIC で HTTP/3 の 54KB が FreeBSD +8〜20%・Linux +4〜6%） |
| `[http3]` | `pacing` | `true` | Packet Pacing を有効化 |
| `[http3]` | `max_pacing_rate` | （なし） | 最大 pacing レート（バイト/秒）。未指定で制限なし |
| `[http3]` | `hystart` | `true` | HyStart++ を有効化 |
| `[http3]` | `mmsg_batch_size` | `64` | UDP mmsg / io_uring パイプライン化 RECVMSG/SENDMSG バッチ幅（1..=128） |
| `[http3]` | `recv_drain_max` | `1024` | **reactor バックエンド専用**（FreeBSD/OpenBSD/NetBSD/macOS、Linux の `--features epoll`）: 1 イテレーションあたりに掻き出す UDP データグラム数の上限（1..=4096）。大きくするほど 1 イテレーションあたりの固定費（select/タイマー往復 + 接続スイープ）を多くのデータグラムへ償却できる（F-151 でこの量だけで 3.2 倍の差を実測）が、drain 中は送信・タイムアウト・バックエンド通知が待たされるため p99 レイテンシとのトレードオフ。v0.8.0 より前の既定は 64。1024 で FreeBSD の HTTP/3 3B が 37% 伸びた（p99 は 11 → 14ms）。Linux 既定の io_uring バックエンドは `mmsg_batch_size` の RECVMSG パイプラインを使うため本キーを参照しない |
| `[http3]` | `compression_enabled` | `false` | 圧縮を有効化 |
| `[http3]` | `gso_gro_enabled` | `false` | GSO/GROを有効化 |
| `[http3]` | `alt_svc_enabled` | `true` | H1/H2 応答へ Alt-Svc で HTTP/3 を広告（`[http3] enabled = true` 時のみ） |
| `[http3]` | `alt_svc` | _(自動)_ | Alt-Svc ヘッダー値の上書き（未指定時は listen ポートから `h3=":PORT"; ma=…`） |
| `[http3]` | `alt_svc_ma_secs` | `86400` | 自動生成 Alt-Svc の max-age（秒） |

設定ファイル例（`examples/config.toml`）:

```toml
[server]
# TLS リスナー: HTTP/1.1 + HTTP/2（ALPN）。[tls] が必要。ここでは平文を受け付けない。
listen = "0.0.0.0:443"
# Unix ドメインソケット（UDS）リスナー（Unix 系のみ）
# [server].listen / [server].listen_cleartext は "unix:<path>" 形式も指定できる。例:
#   listen = "unix:/run/veil/https.sock"
# リスナー側の対象はこの 2 つのみ（[[l4]].listen・[http3].listen は非対応。
# HTTP/3 は QUIC/UDP のため UDS 不可。listen が unix: のときは [http3].listen 必須）。
# 上流バックエンドへの UDS 接続は対応済み（下の「UDS バックエンド」節を参照）。
# Windows では設定検証でエラーになる。
# AF_UNIX に SO_REUSEPORT は無いため、起動時に 1 度だけ bind し、各ワーカーがその fd を
# dup(2) して accept する（カーネルが分散する）。既存のソケットファイルは自動 unlink
# （通常ファイル・ディレクトリがある場合は安全側に倒して起動エラー）。
# UDS 接続の peer アドレスはプレースホルダ 127.0.0.1:0 になる
# （IP ブロックリスト・アクセスログの送信元 IP もこの値）。
# unix_socket_permissions = "0660"   # ソケットファイルのパーミッション（8 進数文字列）
# 平文リスナー: HTTP/1.1 + h2c をプロトコル検出で判別（オプション、F-200）
# 既定（enable_secure_redirect = true）ではこのポートへの要求をすべて
# https://<Host>[:<listen のポート>] へ 301 で返す。下の「リスナー」節を参照。
listen_cleartext = "0.0.0.0:80"
# enable_secure_redirect = true    # false: 平文で実際に配信する（listen が無いときは必須）
# allow_http = true                # listen_cleartext で平文 HTTP/1.1 を受ける（false なら 426）
# allow_h2c = true                 # listen_cleartext で h2c を受ける（false なら GOAWAY）
# ワーカースレッド数（オプション）
# 未指定または0の場合はCPUコア数と同じスレッド数を使用
threads = 4
# HTTP/2を有効化（--features http2 でビルド時のみ）
http2_enabled = true
# HTTP/3 は [http3] enabled = true で有効化（--features http3 でビルド時のみ）
# Serverヘッダー設定（オプション）
# セキュリティ考慮事項: Serverヘッダーはサーバーソフトウェア情報を公開します
# 本番環境では無効化を推奨
# server_header_enabled = false
# カスタムServerヘッダー値（server_header_enabled = true時のみ有効）
# デフォルト: "veil"（プロトコル固有の値: "veil/http1.1", "veil/http2", "veil/http3"）
# server_header_value = "MyServer/1.0"

[logging]
# ログレベル: "trace", "debug", "info", "warn", "error", "off"
level = "info"
# ログ出力形式: "text", "json"
# format = "text"
# ログチャネルサイズ（高負荷時のログドロップ防止）
channel_size = 16384
# フラッシュ間隔（ミリ秒）
flush_interval_ms = 1000
# 最大ログファイルサイズ（バイト、0=ローテーションなし）
# ログファイルパス（オプション、未指定で標準エラー出力）
# file_path = "/var/log/veil.log"

[security]
# 権限降格設定（Linux専用）
drop_privileges_user = "nobody"
drop_privileges_group = "nogroup"
# グローバル同時接続上限（0 = 無制限）
max_concurrent_connections = 10000
# 要求のパスの正規化。ルートの照合・[route.security] の検査の前に行い、正規化したパスを上流へも送る
# （veil と上流でパスの解釈が揃う。HTTP/1.1・HTTP/2・HTTP/3）。
# "base"（既定）: 予約されていない文字の %XX を戻し、"\" を "/" にし、"." と ".." のセグメントを取り除く
# "merge_slashes": base + 連続する "/" を 1 つにする
# "off": 生のパスで照合して送る（v0.8.0 より前の動作）
# %2F・%5C はそのまま残す。絶対形式の要求先（http://host/path）は origin 形式に直し、
# それ以外の origin 形式でない要求先（CONNECT と OPTIONS の * を除く）は 400。
# path_normalization = "base"

# seccomp システムコール制限（Linux専用）
# まずログモードで動作確認後、filterモードに変更推奨
enable_seccomp = true
seccomp_mode = "filter"  # "disabled" / "log" / "filter" / "strict"

# Landlock ファイルシステム制限（Linux 5.13+）
enable_landlock = true
landlock_read_paths = ["/etc/veil", "/usr", "/lib", "/lib64"]
landlock_write_paths = ["/var/log/veil"]

[performance]
# SO_REUSEPORT の振り分け方式
# "kernel" = カーネルデフォルト（3元タプルハッシュ）【既定】
# "cbpf"   = フローハッシュ（4タプル）ベースのCBPF（同一接続を固定ワーカーへ振り分け、
#            キャッシュ・セッション再利用効率向上、Linux 4.6+必須）
reuseport_balancing = "cbpf"

# Huge Pages (Large OS Pages) の使用
# TLBミス削減により5-10%のパフォーマンス向上
huge_pages_enabled = true

# OpenFileCache（ファイルメタデータキャッシュ）
# ファイルメタデータ（canonicalize、metadata、mime_guess）をキャッシュしてシステムコールを削減
# パフォーマンス向上: 60〜67%のシステムコール削減（キャッシュヒット時）
# 
# 効果:
#   - canonicalize、metadata、mime_guessのシステムコールをキャッシュ
#   - 1リクエストあたり5〜6回のシステムコールを2回に削減（キャッシュヒット時）
# 
#   - nginx の open_file_cache と同じく通常ファイルの fd を保持し、HTTP/1.1 の静的配信で
#     リクエストごとの open/close を行わない
#
# 注意事項:
#   - ファイル変更の検出が最大60秒（デフォルト）遅延する可能性
#   - シンボリックリンク変更の検出が遅延する可能性
#   - 有効期間内に置き換え・削除されたファイルは、エントリが失効するまで保持中の fd から
#     配信され続ける（nginx と同じ）
#   - キャッシュした通常ファイル 1 つにつき fd を 1 つ使う（最大 max_entries）
#   - 静的ファイル配信に最適（動的に変更されるファイルには不向き）
#
# ルーティングごとの設定:
#   - 各ルーティング（[path_routes]や[host_routes]）で`open_file_cache`セクションを指定可能
#   - ルーティング設定がない場合は、このグローバル設定が使用される
#
# デフォルト: false（無効）
#open_file_cache_enabled = false

# OpenFileCacheの有効期間（秒、グローバルデフォルト）
# キャッシュされたファイル情報が有効とみなされる期間
# デフォルト: 60秒
#open_file_cache_valid_duration_secs = 60

# OpenFileCacheの最大エントリ数（グローバルデフォルト）
# キャッシュに保持する最大ファイル情報数
# デフォルト: 10000
#open_file_cache_max_entries = 10000

[tls]
cert_path = "/path/to/cert.pem"
key_path = "/path/to/key.pem"
ktls_enabled = true         # kTLS有効化（Linux 5.15+ または FreeBSD 13.0+、feature flag必須。F-126）
                            # FreeBSD で **HW kTLS オフロードが無い場合は false を推奨**（F-155）:
                            # software kTLS は 16KB の TLS レコードごとにカーネルワーカースレッドへ
                            # 暗号処理をディスパッチし、スループットが直列化する。無効化するだけで
                            # 54KB の対 nginx 比が HTTP/1.1 で 0.51 → 1.09、HTTP/2 で 0.56 → 1.16 と
                            # 逆転した（同一 VM 実測）。詳細は docs/perf/README.md。
ktls_fallback_enabled = true # kTLS失敗時のrustlsフォールバック（デフォルト: true）
tcp_cork_enabled = true     # kTLS設定時にTCP_CORKを使用（デフォルト: true）

# 統合ルーティング（AWS ALB準拠）
# 配列の順序で評価（first-match方式）

[[route]]
[route.conditions]
host = "example.com"
[route.action]
type = "File"
path = "/var/www/example"
mode = "sendfile"

[[route]]
[route.conditions]
host = "api.example.com"
[route.action]
type = "Proxy"
url = "http://localhost:8080"

# 静的ファイル（完全一致）
[[route]]
[route.conditions]
host = "example.com"
path = "/robots.txt"
[route.action]
type = "File"
path = "/var/www/robots.txt"

# ディレクトリ配信（末尾スラッシュあり）
[[route]]
[route.conditions]
host = "example.com"
path = "/static/*"
[route.action]
type = "File"
path = "/var/www/assets/"
mode = "sendfile"
# OpenFileCache設定（ルーティングごと、グローバル設定を上書き）
[route.open_file_cache]
enabled = true
valid_duration_secs = 300  # 5分（静的ファイルは変更頻度が低い）
max_entries = 50000
watch = true  # ディスクの変更を通知で追従する
# 静的ファイル本体キャッシュ設定（F-146、ルーティングごと、グローバル設定を上書き。
# HTTP/2・HTTP/3 限定。HTTP/1.1 は sendfile(2) を使うため対象外）
[route.static_file_cache]
enabled = true
valid_duration_secs = 300
max_file_size_bytes = 2097152  # 2 MiB
watch = true

# ディレクトリ配信（末尾スラッシュなし - 同じ動作、リダイレクトなし）
[[route]]
[route.conditions]
host = "example.com"
path = "/docs"
[route.action]
type = "File"
path = "/var/www/docs/"

# カスタムインデックスファイル
[[route]]
[route.conditions]
host = "example.com"
path = "/user/*"
[route.action]
type = "File"
path = "/var/www/user/"
index = "profile.html"

# プロキシ（末尾スラッシュあり）
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080/app/"

# プロキシ（末尾スラッシュなし - 同じ動作）
[[route]]
[route.conditions]
host = "example.com"
path = "/backend"
[route.action]
type = "Proxy"
url = "http://localhost:3000"

# ルート
[[route]]
[route.conditions]
host = "example.com"
path = "/"
[route.action]
type = "File"
path = "/var/www/index.html"
```

## リスナー（`listen` と `listen_cleartext`）

待ち受けは 2 種類のリスナーと HTTP/3 で構成します（F-200）。`listen` と `listen_cleartext` の少なくとも一方が必須で、
同じアドレスにはできません（1 つのポートで TLS と平文を自動判別することはなくなりました）。

| キー | プロトコル | 必要なもの |
|------|-----------|-----------|
| `[server].listen` | TLS: HTTP/1.1 + HTTP/2（ALPN）。平文は TLS ハンドシェイクの失敗で閉じる | `[tls]` |
| `[server].listen_cleartext` | 平文: HTTP/1.1 + h2c（prior knowledge）。プロトコル検出（`MSG_PEEK`）で判別 | `enable_secure_redirect = true` なら `listen`、h2c は `http2` feature |
| `[http3] enabled = true` | `listen` と同じアドレス（UDP）、`[http3].listen` があればそちらで HTTP/3 | `listen`・`[tls]`・`http3` feature |

`listen_cleartext` のキー（すべて `[server]` 配下）:

| キー | 既定 | 意味 |
|------|------|------|
| `allow_http` | `true` | 平文 HTTP/1.1 を受ける。`false` なら HTTP/1.1 は `426 Upgrade Required` |
| `allow_h2c` | `true` | h2c を受ける。`false` なら h2c は `GOAWAY(PROTOCOL_ERROR)` |
| `enable_secure_redirect` | `true` | 要求（HTTP/1.1・h2c とも）を `https://<Host>[:<listen のポート>]` へ `301` で返す（443 ならポート省略。h2c には HTTP/2 の 301）。`listen` が無いのに `true` は設定エラー（リダイレクト先が無い）。平文で配信したいときは `false` |

検証規則（`veil -t` で報告されます）:

- `listen` も `listen_cleartext` も無い: エラー
- `listen` と `listen_cleartext` が同じアドレス: エラー
- `allow_http = false` かつ `allow_h2c = false`: エラー（何も受けないため）
- `enable_secure_redirect = true`（既定）で `listen` が無い: エラー
- `[tls]` が必要なのは `listen` か `[http3] enabled = true` があるときだけ（`listen_cleartext` だけの構成では不要。[TLS なしの平文専用サーバ](#tls-なしの平文専用サーバ)を参照）
- `[http3] enabled = true` で `listen` が無い: エラー（QUIC は TLS 必須）。`Alt-Svc` は HTTP/3 の実際のポートで広告されます
- UDS: `listen`・`listen_cleartext` は `unix:<path>` を受けます。HTTP/3 は UDS 不可
- 下の削除されたキーは、移行先を示すエラーで起動しません（黙って無視しません）

TLS や平文のポートを 2 つ以上開くときは [`[[listener]]`](#複数のリスナーlistener) を足します。

`listen_cleartext` は `listen` と同じく全ワーカースレッドで受け付けます（旧 `http` のリダイレクト専用リスナーは
1 スレッドでした）。待機中のワーカーはおよそ 1 つあたり 2 MB の RSS を使うので、TLS + リダイレクトの構成は以前より
数 MB 多くなります。`listen` のスループットは変わりません。

### 旧キーからの移行

`[server].http`・`tls_only`・`h2c_enabled`・`h2c_listen`・`h2c_http1`・`http3_enabled` は互換レイヤー無しで削除されました。

| 旧 | 新 |
|----|----|
| `listen` + `tls_only = true`（既定） | `listen` |
| `http = "0.0.0.0:80"`（リダイレクト専用リスナー） | `listen_cleartext = "0.0.0.0:80"`（リダイレクトが既定） |
| `h2c_enabled = true` + `h2c_listen = "0.0.0.0:8080"` | `listen_cleartext = "0.0.0.0:8080"` + `enable_secure_redirect = false` |
| `h2c_enabled = true`（H2C 専用）+ `h2c_http1 = true` | `listen_cleartext = <旧 listen>`（`listen` は書かない）+ `enable_secure_redirect = false` |
| `tls_only = false` + `h2c_enabled = true`（同一ポートで自動判別） | `listen` と別ポートの `listen_cleartext` + `enable_secure_redirect = false` |
| `http3_enabled = true`（+ `[http3] listen`） | `[http3] enabled = true`（+ 必要なら `[http3] listen`） |

`http`（リダイレクト）と `h2c_listen`（h2c 配信）の両方を使っていた構成は、目的の違う 2 つのリスナーが要ります。
片方を `listen_cleartext` にし、もう片方を [`[[listener]]`](#複数のリスナーlistener) で足してください。

### 複数のリスナー（`[[listener]]`）

`[[listener]]`（`[[route]]` と同じ最上位の表の配列）で、同じプロセスにポートを足し、ポートごとにプロトコルの設定を
変えられます（F-260）。`[server].listen`・`listen_cleartext` はそのまま使えます。veil はこれを `"tls"`・`"cleartext"`
という名前の暗黙のリスナーにして（スレッド数・経路は今までと同じ）先に開き、続けて `[[listener]]` を書いた順に開きます。
どちらの書き方でも、両方を混ぜても構いません。`[server]` 自体も省略できます。

| キー | 既定 | 意味 |
|------|------|------|
| `name` | `"listener-<添字>"` | ログ・エラー・`redirect_to` で使う名前。重複は不可（`"tls"`・`"cleartext"` は暗黙のリスナーの名前） |
| `listen` | 必須 | `host:port` か `unix:<path>` |
| `tls` | 必須 | `true`: `[tls]` の証明書で TLS（HTTP/1.1 + ALPN の h2）。`false`: 平文の HTTP/1.1 + h2c（接続ごとにプロトコル検出） |
| `http2` | `[server].http2_enabled` | TLS のみ。`false` は ALPN を出さないので、クライアントは必ず HTTP/1.1 で話す（h2 は選ばれない） |
| `http3` | `false` | TLS のみ。同じアドレスの UDP で HTTP/3 も受ける（TCP のリスナーのみ。`http3` feature が要る） |
| `http3_listen` | `listen` と同じ | HTTP/3 の UDP を別のアドレスにするとき（`http3 = true` が要る） |
| `allow_http` | `true` | 平文のみ。`false` なら HTTP/1.1 は `426` |
| `allow_h2c` | `true` | 平文のみ。`false` なら h2c は `GOAWAY(PROTOCOL_ERROR)` |
| `secure_redirect` | **`false`** | 平文のみ。`true` なら要求をすべて `https://<Host>[:<ポート>]` へ `301` で返す |
| `redirect_to` | 最初の TLS のリスナー | 平文のみ。リダイレクト先のポートを取る TLS のリスナー名（`secure_redirect = true` が要る） |
| `unix_socket_permissions` | `[server].unix_socket_permissions` | `unix:<path>` のソケットファイルのパーミッション |
| `threads` | `[server].threads` | このリスナーのワーカースレッド数 |

`secure_redirect` の既定は `false` で、`[server].enable_secure_redirect`（既定 `true`）と違います。`[server]` の既定は
平文だけの設定をうっかり公開しないためのもので、`[[listener]]` は 1 個ずつ明示して書くので、リダイレクトを頼まない限り
平文のリスナーは平文で配ります。暗黙の `"cleartext"` リスナーは `[server]` の既定のままです。

全リスナーで共有するもの（プロセスに 1 個）: 証明書（`[tls]`。SNI の複数証明書を含む）、ルート（`[[route]]`。分けたいときは
`host`・`path` の条件で）、`[http2]`・`[http3]` のチューニング、`max_concurrent_connections`、`[admin]`・`[prometheus]`。
`Alt-Svc` は HTTP/3 の最初のリスナーの UDP ポートを広告します。各リスナーは自分の `threads` 本のワーカースレッドを持ちます
（`http3 = true` なら HTTP/3 のワーカーも同じ数）。

検証規則（`veil -t`）。上の規則に加えて:

- アドレスはすべて違うこと。リスナー同士・HTTP/3 の UDP・`[[l4]]` のリスナー・`[acme] challenge_listen` を比べ、
  `0.0.0.0:p`（`[::]:p`）は任意の `addr:p` と重なるとみなします（`SO_REUSEPORT` では 2 回目の bind が失敗せず、接続が黙って分かれるため）。
- 平文のリスナーに TLS のキー（`http2`・`http3`）、TLS のリスナーに平文のキー（`allow_http`・`allow_h2c`・`secure_redirect`・
  `redirect_to`）を書くとエラー。知らないキーもエラー。
- `redirect_to` は TLS のリスナーの名前であること。`secure_redirect = true` には TLS のリスナーが 1 個以上要る。
- `[http3] enabled`・`[http3].listen` は `[server].listen` にだけ効く。`[[listener]]` では `http3 = true` を使う。
- `[tls] client_auth` は HTTP/3 のリスナーと併用できない。

リロード（SIGHUP）では `allow_http`・`allow_h2c`・`secure_redirect`・`redirect_to` が反映されます。リスナーの追加・削除・変更
（名前・アドレス・`tls`・`http2`・`http3`・`threads` など）は警告を出して起動時の待ち受けのままにします。反映には再起動が要ります
（[リロードで変えられない設定](running.md#リロードで変えられない設定)）。

1 プロセスで 4 ポート（ベンチマーク HttpArena のプロトコルの構成）:

```toml
[server]
http2_enabled = true

[tls]
cert_path = "/etc/veil/cert.pem"
key_path = "/etc/veil/key.pem"

[[listener]]
name = "plain"            # HTTP/1.1 + h2c
listen = "0.0.0.0:8080"
tls = false

[[listener]]
name = "https-h1"         # TLS で HTTP/1.1 だけ（ALPN で h2 を選ばない）
listen = "0.0.0.0:8081"
tls = true
http2 = false

[[listener]]
name = "h2c"              # h2c だけ（HTTP/1.1 は 426）
listen = "0.0.0.0:8082"
tls = false
allow_http = false

[[listener]]
name = "https"            # TLS の h2 + HTTP/1.1 と、UDP 8443 の HTTP/3
listen = "0.0.0.0:8443"
tls = true
http3 = true
```

80 番を 443 へリダイレクトし、社内向けに別ポートで h2c を配る:

```toml
[[listener]]
name = "https"
listen = "0.0.0.0:443"
tls = true

[[listener]]
name = "http"
listen = "0.0.0.0:80"
tls = false
secure_redirect = true    # https://<Host>/ へ 301（443 はポートを省略）
redirect_to = "https"

[[listener]]
name = "internal"
listen = "10.0.0.5:8080"
tls = false
allow_http = false        # h2c だけ
```

## HTTP to HTTPS リダイレクト

`listen_cleartext` は既定（`enable_secure_redirect = true`）で HTTP を HTTPS へリダイレクトします。

### 設定

```toml
[server]
listen = "0.0.0.0:443"
listen_cleartext = "0.0.0.0:80"  # HTTPS へリダイレクト（既定の動作）
```

### 動作

- `http://example.com/path` へのアクセスは `https://example.com/path` に301リダイレクト（h2c クライアントには HTTP/2 の 301）
- Hostヘッダーからドメイン名を取得し、リダイレクト先URLを構築
- **ポートの動作**: リダイレクト先URLは `[server].listen` 設定のポートを使用（`[[listener]]` では `redirect_to` のリスナー、
  既定は最初の TLS のリスナーのポート）
  - listenポートが443の場合（デフォルト）: `https://example.com/path`（ポート省略）
  - listenポートが8443の場合: `https://example.com:8443/path`（ポート包含）

### セキュリティ考慮事項

- **リダイレクト専用**: HTTPではリダイレクトのみを行い、コンテンツは一切配信しません
- **301 Moved Permanently**: ブラウザがリダイレクト先をキャッシュするため、2回目以降は直接HTTPSにアクセスします
- **初回アクセス**: 初回HTTPアクセス時のみ平文通信が発生しますが、コンテンツは含まれません

### 注意事項

- 特権ポート（80番）を使用するため、以下のいずれかが必要です：
  1. rootで起動（権限降格機能と併用を推奨）
  2. `CAP_NET_BIND_SERVICE`ケイパビリティを付与

```bash
# ケイパビリティを付与する場合
sudo setcap 'cap_net_bind_service=+ep' ./target/release/veil
```

## Serverヘッダー設定

クライアントに送信する`Server` HTTPレスポンスヘッダーを制御します。

### セキュリティ考慮事項

Serverヘッダーはサーバーソフトウェア情報を公開するため、攻撃者が脆弱性を特定する手がかりになり得ます。**本番環境では無効化を推奨**します（デフォルト: 無効）。

### 設定

`[server]`セクションで設定します：

```toml
[server]
# Serverヘッダーを有効化（デフォルト: false）
# セキュリティ考慮事項: サーバーソフトウェア情報を公開
# 本番環境では無効化を推奨
server_header_enabled = false

# カスタムServerヘッダー値（server_header_enabled = true時のみ有効）
# デフォルト: "veil"
# 未指定の場合、プロトコルごとに自動設定:
#   - HTTP/1.1: "veil/http1.1"
#   - HTTP/2: "veil/http2"
#   - HTTP/3: "veil/http3"
server_header_value = "MyServer/1.0"
```

### 動作

| 設定 | 動作 |
|------|------|
| `server_header_enabled = false` | Serverヘッダーを送信しない（デフォルト、本番環境推奨） |
| `server_header_enabled = true`、`server_header_value`未指定 | プロトコル固有の値: `veil/http1.1`、`veil/http2`、または`veil/http3` |
| `server_header_enabled = true`、`server_header_value = "Custom"` | すべてのプロトコルでカスタム値を使用: `Server: Custom` |

### 用途

- **開発/テスト**: どのサーバーが応答しているかを識別するために有効化
- **本番環境**: サーバー情報を隠すために無効化（セキュリティベストプラクティス）
- **カスタムブランディング**: Serverヘッダーが必要な場合にカスタム値を設定

## TLS なしの平文専用サーバ

終端する TLS が無いとき（`listen` が無く `[http3] enabled = true` でもない）は `[tls]` を **省略できます**。`listen_cleartext` だけで、
`enable_secure_redirect = false` にします（リダイレクトには `listen` が要るため）。`listen` か HTTP/3 があって使える
`[tls] cert_path` / `key_path` が無いと、検証で "TLS certificate is required" のエラーになります。

```toml
[server]
listen_cleartext = "0.0.0.0:8080"   # HTTP/1.1 + h2c をプロトコル検出で判別
enable_secure_redirect = false      # 平文で配信する（既定は HTTPS への 301）
# allow_http = true                 # 平文 HTTP/1.1 を受ける（false なら 426）
# allow_h2c = true                  # h2c を受ける（false なら GOAWAY）。http2 feature が必要
```

[API ライブラリ](library.md#ライブラリモードの設定)の既定の形です。平文リスナーを信頼できないネットワークに公開しないでください。

## VDS（`[vds]`）

制御プレーンから配信されるルートとアップストリーム。キー: `enabled`・`server`・`servers`・`transport`・`keepalive_secs`・`node_id`・`cluster`・
`token_file`・`ca_cert`・`client_cert`・`client_key`・`poll_timeout_secs`・`retry_initial_ms`・`retry_max_ms`・`max_response_bytes`・
`max_resources`・`initial_fetch_timeout_ms`。`vds` feature（`full` に含まれる）が必要です。
優先順位は `config.toml` < VDS < `VEIL_OVERRIDE_<n>` < `--override` です。詳細は [VDS](vds.md)。

## ACME（`[acme]`）

証明書の自動発行・更新（HTTP-01）。キー: `enabled`・`directory_url`・`contact`・`domains`・`storage_dir`・`accept_terms_of_service`・
`challenge_listen`・`renew_before_days`・`check_interval_secs`・`directory_ca_cert`・`order_timeout_secs`。`acme` feature（`full` に含まれる）と
`[tls] cert_path`/`key_path` が必要です。詳細は [ACME](acme.md)。

## ネイティブハンドラ（`[[l4]] handler`・`[api]`・`type = "Handler"`）

これらは [API サーバライブラリ](library.md)（feature `handler`・`l4-handler`・`api`）のもので、プロキシ本体のバイナリには含まれません。

- `[[route]]` の `action.type = "Handler"` と `handler = "<名前>"` は、登録済みの Rust/C/Python ハンドラへルーティングします。
  `handler` feature が無いビルドでは設定エラーです。
- `[[l4]]` の `handler = "<名前>"`（feature `l4-handler`）: 転送せず、登録済みの L4 ハンドラで接続・データグラムを処理します。このとき `upstreams` は省略できます。
  [L4 プロキシ](l4-proxy.md#ネイティブハンドラ)を参照。
- `[api]`（ライブラリ専用）: `openapi_enabled`・`openapi_path`・`openapi_json_path`・`openapi_title`・`openapi_version`・`openapi_description`・
  `max_body_size`・`handler_threads`・`tokio_threads`・`grpc_max_message_size`。[ライブラリガイド](library.md#api-キーライブラリ専用)を参照。
