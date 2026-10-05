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
| `[server]` | `http3_enabled` | `false` | HTTP/3を有効化 |
| `[logging]` | `level` | `"info"` | ログレベル |
| `[logging]` | `format` | `"text"` | ログ形式 |
| `[logging]` | `channel_size` | `100000` | ログチャネルバッファサイズ |
| `[logging]` | `flush_interval_ms` | `1000` | フラッシュ間隔（ミリ秒） |
| `[prometheus]` | `enabled` | `false` | Prometheusメトリクスを有効化 |
| `[prometheus]` | `path` | `"/__metrics"` | メトリクスエンドポイントパス |
| `[performance]` | `reuseport_balancing` | `"kernel"` | SO_REUSEPORT振り分け方式 |
| `[performance]` | `huge_pages_enabled` | `false` | Huge Pagesを有効化 |
| `[performance]` | `open_file_cache_enabled` | `false` | OpenFileCacheを有効化 |
| `[performance]` | `open_file_cache_valid_duration_secs` | `60` | キャッシュ有効期間（秒） |
| `[performance]` | `open_file_cache_max_entries` | `10000` | 最大キャッシュエントリ数 |
| `[static_file_cache]` | `enabled` | `false` | 静的ファイル本体キャッシュ（F-146/F-150。HTTP/2・HTTP/3、および kTLS 無効時の HTTP/1.1）を有効化 |
| `[static_file_cache]` | `valid_duration_secs` | `60` | キャッシュ有効期間（秒） |
| `[static_file_cache]` | `max_entries` | `1024` | 最大キャッシュエントリ数 |
| `[static_file_cache]` | `max_file_size_bytes` | `1048576` | キャッシュ対象の最大ファイルサイズ（バイト） |
| `[static_file_cache]` | `max_total_bytes` | `67108864` | キャッシュ全体の最大バイト数 |
| `[static_file_cache]` | `revalidate_mtime` | `false` | ヒット時に mtime を再検証するか（true でホットパスに syscall が復活） |
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
| `[http3]` | `max_udp_payload_size` | `1350` | 最大UDPペイロードサイズ |
| `[http3]` | `initial_max_data` | `10000000` | 初期最大データ（10MB） |
| `[http3]` | `initial_max_stream_data_bidi_local` | `1000000` | ストリームデータ双方向ローカル（1MB） |
| `[http3]` | `initial_max_stream_data_bidi_remote` | `1000000` | ストリームデータ双方向リモート（1MB） |
| `[http3]` | `initial_max_stream_data_uni` | `1000000` | ストリームデータ単方向（1MB） |
| `[http3]` | `initial_max_streams_bidi` | `100` | 最大双方向ストリーム数 |
| `[http3]` | `initial_max_streams_uni` | `100` | 最大単方向ストリーム数 |
| `[http3]` | `cc_algorithm` | `"bbr"` | QUIC 輻輳制御（`reno`/`cubic`/`bbr`/`bbr2`/`bbr2_gcongestion`） |
| `[http3]` | `pacing` | `true` | Packet Pacing を有効化 |
| `[http3]` | `max_pacing_rate` | （なし） | 最大 pacing レート（バイト/秒）。未指定で制限なし |
| `[http3]` | `hystart` | `true` | HyStart++ を有効化 |
| `[http3]` | `mmsg_batch_size` | `64` | UDP mmsg / io_uring パイプライン化 RECVMSG/SENDMSG バッチ幅（1..=128） |
| `[http3]` | `recv_drain_max` | `64` | **reactor バックエンド専用**（FreeBSD/OpenBSD/NetBSD/macOS、Linux の `--features epoll`）: 1 イテレーションあたりに掻き出す UDP データグラム数の上限（1..=4096）。大きくするほど 1 イテレーションあたりの固定費（select/タイマー往復 + 接続スイープ）を多くのデータグラムへ償却できる（F-151 でこの量だけで 3.2 倍の差を実測）が、drain 中は送信・タイムアウト・バックエンド通知が待たされるため p99 レイテンシとのトレードオフ。Linux 既定の io_uring バックエンドは `mmsg_batch_size` の RECVMSG パイプラインを使うため本キーを参照しない |
| `[http3]` | `compression_enabled` | `false` | 圧縮を有効化 |
| `[http3]` | `gso_gro_enabled` | `false` | GSO/GROを有効化 |
| `[http3]` | `alt_svc_enabled` | `true` | H1/H2 応答へ Alt-Svc で HTTP/3 を広告（`server.http3_enabled` 時のみ） |
| `[http3]` | `alt_svc` | _(自動)_ | Alt-Svc ヘッダー値の上書き（未指定時は listen ポートから `h3=":PORT"; ma=…`） |
| `[http3]` | `alt_svc_ma_secs` | `86400` | 自動生成 Alt-Svc の max-age（秒） |

設定ファイル例（`examples/config.toml`）:

```toml
[server]
listen = "0.0.0.0:443"
# Unix ドメインソケット（UDS）リスナー（Unix 系のみ）
# [server].listen / [server].h2c_listen は "unix:<path>" 形式も指定できる。例:
#   listen = "unix:/run/veil/https.sock"
# リスナー側の対象はこの 2 つのみ（[[l4]].listen・[server].http・[http3].listen は非対応。
# HTTP/3 は QUIC/UDP のため UDS 不可。listen が unix: のときは [http3].listen 必須）。
# 上流バックエンドへの UDS 接続は対応済み（下の「UDS バックエンド」節を参照）。
# Windows では設定検証でエラーになる。
# AF_UNIX に SO_REUSEPORT は無いため、起動時に 1 度だけ bind し、各ワーカーがその fd を
# dup(2) して accept する（カーネルが分散する）。既存のソケットファイルは自動 unlink
# （通常ファイル・ディレクトリがある場合は安全側に倒して起動エラー）。
# UDS 接続の peer アドレスはプレースホルダ 127.0.0.1:0 になる
# （IP ブロックリスト・アクセスログの送信元 IP もこの値）。
# unix_socket_permissions = "0660"   # ソケットファイルのパーミッション（8 進数文字列）
# メインリスナーで平文接続を拒否する（既定: true）
# 適用範囲は [server].listen のみ。true の場合はプロトコル検出（MSG_PEEK）を行わず
# 常に TLS ハンドシェイクへ進むため、平文 HTTP/1.1 / h2c クライアントは切断される。
# false にすると従来どおり TLS ポートで h2c / 平文 HTTP/1.1 を受理する
# （h2c_enabled = true が必要）。
# [server].h2c_listen・[server].http（リダイレクト用）・[[l4]] には影響しない。
tls_only = true
# HTTP to HTTPSリダイレクト（オプション）
# HTTPアクセスを自動的にHTTPSにリダイレクト（301 Moved Permanently）
http = "0.0.0.0:80"
# ワーカースレッド数（オプション）
# 未指定または0の場合はCPUコア数と同じスレッド数を使用
threads = 4
# HTTP/2を有効化（--features http2 でビルド時のみ）
http2_enabled = true
# HTTP/3を有効化（--features http3 でビルド時のみ）
http3_enabled = true
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
channel_size = 100000
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
# 注意事項:
#   - ファイル変更の検出が最大60秒（デフォルト）遅延する可能性
#   - シンボリックリンク変更の検出が遅延する可能性
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
# 静的ファイル本体キャッシュ設定（F-146、ルーティングごと、グローバル設定を上書き。
# HTTP/2・HTTP/3 限定。HTTP/1.1 は sendfile(2) を使うため対象外）
[route.static_file_cache]
enabled = true
valid_duration_secs = 300
max_file_size_bytes = 2097152  # 2 MiB

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

## HTTP to HTTPS リダイレクト

HTTPアクセスを自動的にHTTPSにリダイレクトする機能です。

### 設定

```toml
[server]
listen = "0.0.0.0:443"
http = "0.0.0.0:80"  # HTTPリダイレクトを有効化
```

### 動作

- `http://example.com/path` へのアクセスは `https://example.com/path` に301リダイレクト
- Hostヘッダーからドメイン名を取得し、リダイレクト先URLを構築
- **ポートの動作**: リダイレクト先URLは `[server].listen` 設定のポートを使用
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
