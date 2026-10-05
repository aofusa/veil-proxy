# L4 ストリームプロキシ

[← ドキュメント目次](README.md) · [English](../l4-proxy.md)

## L4ストリームプロキシ

TCP/UDPレイヤー（L4）のロードバランシングプロキシです。HTTPプロキシとは異なり、プロトコルペイロードを解析せずに生のストリーム/データグラムを転送します。データベース、メッセージブローカー、Redis、SMTP、DNS、その他の非HTTPプロトコルに適しています。

> **要件**: `--features l4-proxy` でビルド（`--features full` に含まれます）

### 機能

- **RoundRobin / LeastConn** ロードバランシング
- **TCP / UDP 両対応**: リスナーごとに `protocol = "tcp"`（デフォルト）または `protocol = "udp"` を指定
- **TLSパススルー**: 復号なしでTLS接続をそのまま転送（TCPのみ。UDPはTLS/DTLS非対応）
- **接続数制限**: `max_connections` を超えた接続を拒否（UDPでは同時セッション数の上限として扱う）
- **接続タイムアウト**: upstream への接続タイムアウト設定（TCPのみ）
- **ヘルスチェック連携**: L4リスナーごとにTCPまたはgRPCヘルスチェックを設定可能（UDPバックエンドも既存のTCP connectベースのヘルスチェックを流用。UDP到達性そのものの確認はscope外）
- **独立スレッド**: 各L4リスナーは専用スレッド上の io_uring ランタイムで動作

### UDPセッションテーブル

UDPはコネクションレスのため、nginx stream の UDP / Envoy UDP proxy 相当のセッションテーブル方式で転送します。

- リスナー UDP ソケット 1 本を全セッションで共有し、`recvfrom` でクライアントアドレスごとに振り分けます。
- 新規クライアントアドレス到着時、設定されたロードバランシングアルゴリズムで upstream を選択し、専用の UDP ソケットを `connect()` してセッションテーブルへ登録します。
- クライアント → upstream: リスナーの受信ループが直接転送します。
- upstream → クライアント: セッションごとの専用タスクが upstream ソケットを受信し、共有リスナーソケット経由でクライアントへ返送します。
- どちらの方向にも `idle_timeout_secs` 秒間トラフィックがなければセッションを退去します。
- 独自の非同期 UDP ソケット（`runtime/udp.rs`）は io_uring / reactor（epoll/kqueue）の両バックエンドで同一コードで動作し、**新規 io_uring オペコードを一切追加しません**。`recvfrom`/`sendto`/`send`/`recv` を try-first で発行し、`EAGAIN` の場合のみ既存の汎用 fd readiness 待機（`wait_readable_fd`/`wait_writable_fd`）にフォールバックします。

### 設定

L4リスナーは `[[l4]]` セクションで定義します（HTTPルートとは独立）。L4リスナーは起動時にポートをバインドするため、**SIGHUPによるホットリロードはできません**。

**スレッドモデル（F-156）**: TCP リスナーは `[server].threads` と同じ数のワーカースレッドで
起動し、各ワーカーが `SO_REUSEPORT`（FreeBSD は `SO_REUSEPORT_LB`）でリスナーソケットを
複製して独立したイベントループで accept・転送します（HTTP/H2C ワーカーと同じ方式）。
F-156 より前は **`threads` の値に関わらず 1 スレッドでしか動作しておらず**、マルチコア環境で
L4 のスループットが頭打ちになっていました（FreeBSD 実測で対 nginx 0.58 → 1.09）。
ロードバランシング状態（ラウンドロビン位置・上流ごとの接続数）と `max_connections` の
カウンタは全ワーカーで共有されるため、ワーカー数を増やしても分散精度と上限は保たれます。
UDP リスナーは従来どおりシングルスレッドです（セッションテーブルがワーカー間で分断されるため）。

```toml
# PostgreSQL TCPプロキシ（--features l4-proxy が必要）
[[l4]]
name = "postgres-proxy"          # ログ・メトリクス識別用の名前
listen = "0.0.0.0:5432"          # バインドアドレス
lb = "least_conn"                # "round_robin"（デフォルト）または "least_conn"
tls = "none"                     # "none"（デフォルト）、"passthrough"、"terminate"
max_connections = 200            # 0 = 無制限（デフォルト）
connect_timeout_secs = 5         # デフォルト: 10

  [[l4.upstreams]]
  addr = "10.0.0.1:5432"
  weight = 1

  [[l4.upstreams]]
  addr = "10.0.0.2:5432"
  weight = 1

  # オプション: TCPヘルスチェック
  [l4.health_check]
  check_type = "tcp"
  interval_secs = 10
  timeout_secs = 3
  unhealthy_threshold = 2
  healthy_threshold = 1
```

```toml
# Redisプロキシ
[[l4]]
name = "redis-proxy"
listen = "0.0.0.0:6379"
lb = "round_robin"

  [[l4.upstreams]]
  addr = "redis1.internal:6379"

  [[l4.upstreams]]
  addr = "redis2.internal:6379"
```

```toml
# DNS UDPプロキシ（セッションテーブル方式、--features l4-proxy が必要）
[[l4]]
name = "dns-udp-proxy"
listen = "0.0.0.0:53"
protocol = "udp"                # "tcp"（デフォルト）または "udp"
lb = "round_robin"
idle_timeout_secs = 30          # 30秒無通信でクライアントセッションを退去

  [[l4.upstreams]]
  addr = "10.0.0.1:53"

  [[l4.upstreams]]
  addr = "10.0.0.2:53"
```

### 設定リファレンス

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `name` | リスナー名（ログに表示） | 必須 |
| `listen` | バインドアドレス（例: `"0.0.0.0:3306"`） | 必須 |
| `protocol` | トランスポートプロトコル: `tcp` または `udp` | `tcp` |
| `lb` | ロードバランシング: `round_robin` または `least_conn` | `round_robin` |
| `tls` | TLSモード: `none`、`passthrough`、`terminate`（TCPのみ。`udp` では警告のうえ無視） | `none` |
| `max_connections` | 最大同時接続数/セッション数（0 = 無制限） | `0` |
| `connect_timeout_secs` | upstream接続タイムアウト（秒、TCPのみ） | `10` |
| `idle_timeout_secs` | アイドルタイムアウト（秒）。この時間通信がなければ接続/セッションを切断 | `600` |
| `wasm_modules` | WASM network filter モジュール名一覧（`wasm` feature 必須、F-133）。空（既定）なら WASM 無効で従来どおり `splice`/ゼロコピー経路を使う。指定すると `splice` を使わずユーザー空間バッファ経由の転送へ切り替わり、`proxy_on_downstream_data`/`proxy_on_upstream_data` でデータを検査・書き換えできる（切替判定は接続確立時に1回のみ） | `[]` |
| `module_configuration` | リスナー単位の Proxy-Wasm プラグイン設定上書き（モジュール名 → 文字列 or TOML テーブル、F-148）。合成規則は `[[route]]` と同じで、`wasm_modules` に列挙した名前のみ指定可 | （なし） |
| `upstreams[].addr` | upstreamアドレス（`"host:port"` 形式） | 必須 |
| `upstreams[].weight` | 重み（weighted RR用、現在予約） | `1` |
| `health_check` | ヘルスチェック設定（upstreamのhealth_checkと同形式） | なし |

### 注意事項

- L4リスナーは起動時にポートをバインドします。**SIGHUPで設定を再読み込みしても L4 設定は反映されません**。
- HTTPプロキシとL4プロキシは共存可能です（異なるポートをリッスン）。
- TLSターミネーション（`tls = "terminate"`）は将来実装予定です。現時点ではパススルーとして動作します。
- `protocol = "udp"` は TLS/DTLS 非対応です。`tls` に `none` 以外を指定した UDP リスナーは起動時に警告を出し、`none` として扱われます。
- UDP のヘルスチェックは既存の TCP connect ベースのチェックを流用します。UDP の到達性そのものの確認（プロトコル依存で一般化困難）は scope 外です。
