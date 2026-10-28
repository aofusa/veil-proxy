# HTTP/2・HTTP/3・WebSocket

[← ドキュメント目次](README.md) · [English](../protocols.md)

## HTTP/2サポート

HTTP/2（RFC 7540）をTLS ALPNネゴシエーションによりサポートします。

### 特徴

| 項目 | 効果 |
|------|------|
| ストリーム多重化 | 単一接続で複数リクエストを並列処理 |
| HPACKヘッダー圧縮 | ヘッダーオーバーヘッドを大幅削減。デコードは符号化表の線形探索ではなく **4-bit LUT 状態機械**（F-121: 256 状態 × 16 peek、16 KiB・L1 常駐のパック表）。release マイクロベンチで旧線形デコーダ比約 12 倍。EOS パディング検査と B-21（不正入力で panic しない）は維持 |
| サーバープッシュ | 先行リソース送信によるレイテンシ削減 |
| フロー制御 | ストリーム・コネクションレベルの制御 |

### 有効化

```bash
# HTTP/2フィーチャー付きでビルド
cargo build --release --features http2
```

```toml
# config.toml
[server]
listen = "0.0.0.0:443"
http2_enabled = true  # HTTP/2を有効化（ALPN h2）
```

### 詳細設定

`[http2]` セクションでHTTP/2プロトコルの詳細パラメータを設定できます：

```toml
[http2]
# HPACK動的テーブルサイズ（デフォルト: 65536）
header_table_size = 65536

# 同時ストリーム数（デフォルト: 256）
max_concurrent_streams = 256

# ストリームウィンドウサイズ（デフォルト: 1048576 = 1MB）
initial_window_size = 1048576

# 最大フレームサイズ（デフォルト: 65536）
max_frame_size = 65536

# 最大ヘッダーリストサイズ（デフォルト: 65536）
max_header_list_size = 65536

# コネクションウィンドウサイズ（デフォルト: 1048576 = 1MB）
connection_window_size = 1048576
```

### DoS対策

HTTP/2 DoS攻撃対策はデフォルトで有効です。`[http2]` セクションで設定できます：

| 攻撃 | CVE | 設定項目 | デフォルト |
|------|-----|----------|-----------|
| Rapid Reset | CVE-2023-44487 | `max_rst_stream_per_second` | 100 |
| CONTINUATION Flood | CVE-2024-24786 | `max_continuation_frames` | 10 |
| 制御フレームフラッド | - | `max_control_frames_per_second` | 500 |
| HPACK Bomb | - | `max_header_block_size` | 65536 |
| Slow Loris | - | `stream_idle_timeout_secs` | 60 |

```toml
[http2]
# RST_STREAMレート制限（1秒あたり）
# Rapid Reset攻撃対策 (CVE-2023-44487)
max_rst_stream_per_second = 100

# 制御フレームレート制限（1秒あたり）
# PING/SETTINGSフラッド対策
max_control_frames_per_second = 500

# CONTINUATIONフレーム制限（ヘッダーブロックあたり）
# CONTINUATION Flood対策 (CVE-2024-24786)
max_continuation_frames = 10

# 最大ヘッダーブロックサイズ（バイト）
# HPACK Bomb対策
max_header_block_size = 65536

# ストリームアイドルタイムアウト（秒）
# Slow Loris対策（0で無効化）
stream_idle_timeout_secs = 60
```

制限を超過した場合、サーバーは `ENHANCE_YOUR_CALM` (0xb) エラーで応答し、接続を閉じます。

### HTTP/1.1フォールバック

HTTP/2をサポートしないクライアントは自動的にHTTP/1.1にフォールバックします。

### H2C (HTTP/2 Cleartext) サーバー

VeilはH2C（HTTP/2 Cleartext、Prior Knowledgeモード）サーバーとしても動作できます。TLSなしでHTTP/2接続を受け付けます。
h2c と平文 HTTP/1.1 は平文リスナー `[server].listen_cleartext` で受け、プロトコル検出で判別します（F-200）。

#### 特徴

- **Prior Knowledgeモード**: RFC 7540 Section 3.4に準拠したH2C接続
- **プロトコル自動検出**: 平文リスナーで h2c と平文 HTTP/1.1 を自動判別（TLS は別リスナー `listen`）
- **内部ネットワーク向け**: 平文通信のため、本番環境では内部ネットワークでのみ使用を推奨

#### 有効化

```bash
# HTTP/2フィーチャー付きでビルド
cargo build --release --features http2
```

```toml
# config.toml
[server]
listen_cleartext = "0.0.0.0:8080"   # 平文: HTTP/1.1 + h2c
enable_secure_redirect = false      # 平文で配信する（既定は HTTPS への 301 で、`listen` が要る）
# allow_http = true                 # 平文 HTTP/1.1 を受ける（false なら 426）
# allow_h2c = true                  # h2c を受ける（false なら GOAWAY）
```

#### 使用方法

```bash
# curlでH2C接続をテスト
curl --http2-prior-knowledge http://localhost:8080/
```

#### 注意事項

- `--features http2` でビルドする必要があります
- **TLS リスナー（`[server].listen`）は平文を受け付けません**。h2c / 平文 HTTP/1.1 は別ポートの
  `listen_cleartext` で受けます（`listen` を書かなければ平文専用サーバで、`[tls]` も不要）
- 平文通信のため、本番環境では内部ネットワークでのみ使用を推奨
- ALPNネゴシエーションは行われません（Prior Knowledgeモード）
- プロトコル検出は接続開始時の最初の数バイトを確認して行われます
- 規則と移行表は[リスナー](configuration.md#リスナーlisten-と-listen_cleartext)を参照
- 1 つのプロセスで TLS と平文のリスナーを複数動かす（例: HTTP/1.1 だけの TLS、h2c だけ、HTTP/2 + HTTP/3 を別ポートで）には
  `[[listener]]` を使います。`http2`・`http3`・`allow_http`・`allow_h2c` はリスナーごとに設定します。
  [複数のリスナー](configuration.md#複数のリスナーlistener)を参照

## HTTP/3サポート

HTTP/3（RFC 9114）をQUIC/UDPベースでサポートします。Cloudflare製の[quiche](https://github.com/cloudflare/quiche)を使用。

### 特徴

| 項目 | 効果 |
|------|------|
| 1-RTT の接続確立 | QUIC はトランスポートと TLS のハンドシェイクを 1 往復にまとめる。0-RTT のデータは受け取るが、要求はハンドシェイクの完了後に処理する（再送攻撃を受けない） |
| Head-of-Lineブロッキング解消 | パケットロスが他ストリームに影響しない |
| 接続マイグレーション | ネットワーク切り替え時も接続維持 |
| GSO/GRO最適化 | 高パフォーマンスUDP処理 |

### 有効化

```bash
# HTTP/3フィーチャー付きでビルド
cargo build --release --features http3
```

```toml
# config.toml
[server]
listen = "0.0.0.0:443"

[http3]
enabled = true  # HTTP/3を有効化（QUIC/UDP）。listen と同じアドレスで起動。`listen` と [tls] が必要
```

`[http3] enabled` は `[server].listen` に効きます。`[[listener]]` はそれぞれの `http3 = true` で HTTP/3 を有効にします
（UDP は同じアドレス、または `http3_listen`）。その他の `[http3]` のキーはすべての HTTP/3 リスナーに効き、`Alt-Svc` は
最初のものを広告します。[複数のリスナー](configuration.md#複数のリスナーlistener)を参照。

### 詳細設定

`[http3]` セクションでHTTP/3（QUIC）プロトコルの詳細パラメータを設定できます：

```toml
[http3]
# HTTP/3リッスンアドレス（UDP、未指定時はserver.listenと同じ）
listen = "0.0.0.0:443"

# 最大アイドルタイムアウト（ミリ秒、デフォルト: 30000）
max_idle_timeout = 30000

# 最大UDPペイロードサイズ（デフォルト: 1350）
max_udp_payload_size = 1350

# 初期最大データサイズ（コネクション全体、デフォルト: 10000000）
initial_max_data = 10000000

# 初期最大ストリームデータサイズ（双方向ローカル、デフォルト: 1000000）
initial_max_stream_data_bidi_local = 1000000

# 初期最大ストリームデータサイズ（双方向リモート、デフォルト: 1000000）
initial_max_stream_data_bidi_remote = 1000000

# 初期最大ストリームデータサイズ（単方向、デフォルト: 1000000）
initial_max_stream_data_uni = 1000000

# 初期最大双方向ストリーム数（デフォルト: 100）
initial_max_streams_bidi = 100

# 初期最大単方向ストリーム数（デフォルト: 100）
initial_max_streams_uni = 100

# GSO/GRO最適化（UDPパフォーマンス最適化）
# GSO (Generic Segmentation Offload) / GRO (Generic Receive Offload) は
# カーネルレベルでUDPパケットの送受信を効率化する機能です。
#
# 効果:
#   - 送信(GSO): 同一宛先・同一サイズの QUIC パケットを連結し 1 回の sendmsg(UDP_SEGMENT) で送出
#   - 受信(GRO): 同一フローの複数データグラムを 1 回の recvmsg で集約受信
#   - システムコール回数の削減・CPU使用率の低減
#   - HTTP/3 受信ループは単一バッファを再利用し、GRO セグメントをスライスのまま quiche へ
#     渡すため、データグラム毎のヒープ確保とコピーを排除（ゼロコピー受信）。非対応カーネルでは
#     単発データグラム送受信に自動フォールバック。
#   - 本設定と独立に、HTTP/3 データプレーンは recvmmsg(2)/sendmmsg(2) で複数データグラム
#     （異なる接続宛も含む）を常時 1 システムコールにバッチングする（F-115）。コンテナ実行時は
#     seccomp 許可リストに recvmmsg/sendmmsg が必要（docker/assets/security/seccomp.json は対応済み）。
#
# 注意:
#   - Linux 5.0+ でサポート
#   - 一部の仮想環境やDockerでは期待通りに動作しない場合あり
#   - 問題が発生した場合は false に設定してください
#
# デフォルト: false
gso_gro_enabled = false

# Alt-Svc（HTTP/1.1・HTTP/2 応答での HTTP/3 広告、F-94）
# 関連キーはすべて [http3] に集約。[http3] enabled = true のときのみ有効。
alt_svc_enabled = true          # 既定: true。false で広告を抑制
# alt_svc = "h3=\":443\"; ma=86400"  # 任意の全文上書き（未指定時は listen ポートから自動生成）
# alt_svc_ma_secs = 86400            # 自動生成時の max-age（秒、既定 86400）
```

### 注意事項

- HTTP/3はUDPベースのため、**kTLSは使用不可**です（TCPを使用しないため）
- UDPポート443をファイアウォールで開放する必要があります
- `[http3] enabled = true` のとき、HTTP/1.1・HTTP/2 応答に `Alt-Svc` で HTTP/3 を広告します（設定はすべて `[http3]`：`alt_svc_enabled`、任意で `alt_svc` / `alt_svc_ma_secs`）

## WebSocketサポート

WebSocket（RFC 6455）のプロキシに対応しています。
`Connection: Upgrade` と `Upgrade: websocket` ヘッダーを自動検出し、
双方向のデータ転送を行います。

### 動作

1. クライアントからの Upgrade リクエストを検出
2. バックエンドに Upgrade リクエストを転送
3. 101 Switching Protocols を受信
4. 双方向のバイパス転送を開始（設定されたポーリングモードで動作）
5. どちらかの接続が閉じるまで継続

### ポーリングモード

WebSocket双方向転送時のポーリング動作を設定で制御できます。

| モード | 説明 | 用途 |
|--------|------|------|
| `adaptive`（デフォルト） | データ転送時は短く、アイドル時は長くなる | 汎用、CPU効率重視 |
| `fixed` | 常に固定のタイムアウトを使用 | リアルタイムゲーム、低レイテンシ最優先 |

詳細な設定オプションは「[WebSocket設定](routing.md#websocket設定)」セクションを参照してください。

### 設定例

WebSocketは通常のProxyバックエンドで自動的にサポートされます：

```toml
# WebSocketアプリケーション（デフォルト設定）
[[route]]
[route.conditions]
host = "example.com"
path = "/ws/*"
[route.action]
type = "Proxy"
url = "http://localhost:3000"

# 低レイテンシ設定（リアルタイムゲーム向け）
[[route]]
[route.conditions]
host = "game.example.com"
path = "/ws/*"
[route.action]
type = "Proxy"
url = "http://localhost:3001"

[route.security]
  websocket_poll_mode = "fixed"
  websocket_poll_timeout_ms = 1
```

### 対応バックエンド

| プロトコル | サポート |
|-----------|---------|
| HTTP → WS | ✅ |
| HTTPS → WSS | ✅ |
