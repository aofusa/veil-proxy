# 圧縮・プロキシキャッシュ・バッファリング

[← ドキュメント目次](README.md) · [English](../compression-cache.md)

## レスポンス圧縮

動的レスポンス圧縮（Gzip、Brotli、Zstd）をサポートします。Accept-Encodingヘッダーに基づいて、クライアントに送信する前にレスポンスを圧縮します。

### 特徴

| 項目 | 説明 |
|------|------|
| **複数アルゴリズム対応** | Gzip、Brotli、Zstd、Deflateをサポート |
| **Content-Typeフィルタリング** | text/HTML/JSON等のみ圧縮 |
| **最小サイズ閾値** | 小さなレスポンスは圧縮スキップ |
| **Accept-Encodingネゴシエーション** | 最適なエンコーディングを自動選択 |

### 有効化

圧縮はデフォルトで**無効**です（kTLS最適化のゼロコピーsendfileを維持）。
ルートごとに `compression` セクションで有効化します：

```toml
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080"

[route.compression]
  enabled = true
```

### 設定オプション

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `enabled` | 圧縮を有効化 | false |
| `preferred_encodings` | エンコーディング優先順位（配列） | ["zstd", "br", "gzip"] |
| `gzip_level` | Gzip圧縮レベル（1-9） | 4 |
| `brotli_level` | Brotli圧縮レベル（0-11） | 4 |
| `zstd_level` | Zstd圧縮レベル（1-22） | 3 |
| `min_size` | 圧縮する最小サイズ（バイト） | 1024 |
| `compressible_types` | 圧縮対象のMIMEタイプ（プレフィックスマッチ） | text/*, application/json等 |
| `skip_types` | スキップするMIMEタイプ（プレフィックスマッチ） | image/*, video/*, audio/*等 |

### 圧縮レベルガイドライン

| アルゴリズム | レベル | 速度 | 圧縮率 | 用途 |
|-------------|--------|------|--------|------|
| Gzip | 1-3 | 高速 | 低 | リアルタイム、高スループット |
| Gzip | 4-6 | バランス | 中 | 汎用 |
| Gzip | 7-9 | 低速 | 高 | 静的アセット、帯域優先 |
| Brotli | 0-4 | 高速 | 中 | 動的コンテンツ |
| Brotli | 5-9 | バランス | 高 | 汎用 |
| Brotli | 10-11 | 低速 | 最高 | 静的アセット |
| Zstd | 1-3 | 高速 | 中 | リアルタイムAPI |
| Zstd | 4-9 | バランス | 高 | 汎用 |
| Zstd | 10-22 | 低速 | 最高 | アーカイブ |

### 設定例

```toml
# API圧縮（高速、バランス重視）
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080"

[route.compression]
  enabled = true
  preferred_encodings = ["zstd", "br", "gzip"]
  zstd_level = 3
  brotli_level = 4
  gzip_level = 4
  min_size = 1024

# 静的アセット（高圧縮率）
[[route]]
[route.conditions]
host = "example.com"
path = "/static/*"
[route.action]
type = "File"
path = "/var/www/static"

[route.compression]
  enabled = true
  preferred_encodings = ["br", "gzip"]
  brotli_level = 6
  gzip_level = 6
  min_size = 256
```

### デフォルト圧縮対象タイプ

以下のMIMEタイプはデフォルトで圧縮されます：

- `text/*`（HTML、CSS、プレーンテキスト等）
- `application/json`
- `application/javascript`
- `application/xml`
- `application/xhtml+xml`
- `application/rss+xml`
- `application/atom+xml`
- `image/svg+xml`
- `application/wasm`

### デフォルトスキップタイプ

以下のMIMEタイプは圧縮**されません**（既に圧縮済み、またはバイナリ）：

- `image/*`
- `video/*`
- `audio/*`
- `application/octet-stream`
- `application/zip`
- `application/gzip`
- `application/x-gzip`
- `application/x-brotli`

### HTTP/3圧縮設定

HTTP/3では `[http3]` セクションで別途圧縮設定が可能です：

```toml
[http3]
compression_enabled = true

  [http3.compression]
  preferred_encodings = ["br", "gzip"]
  brotli_level = 5
  gzip_level = 5
```

> **Note**: 圧縮を有効にすると、圧縮されたレスポンスに対してはkTLSのゼロコピーsendfile最適化は使用されません。大きなファイルの最大スループットを得るには、静的ファイルルートでは圧縮を無効にすることを検討してください。

## プロキシキャッシュ

バックエンドレスポンスをキャッシュし、バックエンド負荷の軽減とレスポンス時間の改善を実現します。

### 特徴

| 機能 | 説明 |
|------|------|
| **メモリキャッシュ** | サイズ制限付きの高速インメモリLRUキャッシュ |
| **ディスクキャッシュ** | monoio非同期I/Oを使用した大容量レスポンス保存 |
| **ETag/If-None-Match** | 条件付きリクエストに対する304 Not Modifiedレスポンス |
| **If-Modified-Since** | 日付ベースの条件付きリクエスト検証 |
| **stale-while-revalidate** | バックグラウンドで更新しながらstaleコンテンツを提供 |
| **stale-if-error** | バックエンドエラー時にstaleコンテンツを提供 |
| **Varyヘッダーサポート** | リクエストヘッダーに基づくキャッシュ分離 |
| **パターンベース無効化** | globパターンによるキャッシュ無効化 |

### 有効化

キャッシュはデフォルトで**無効**です。ルートごとに `cache` セクションで有効化します：

```toml
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080"

[route.cache]
  enabled = true
```

### 設定オプション

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `enabled` | キャッシュを有効化 | false |
| `max_memory_size` | メモリキャッシュ最大サイズ（バイト） | 100MB |
| `disk_path` | ディスクキャッシュディレクトリ（オプション） | なし |
| `max_disk_size` | ディスクキャッシュ最大サイズ（バイト） | 1GB |
| `memory_threshold` | これより大きいレスポンスはディスクへ（バイト） | 64KB |
| `default_ttl_secs` | Cache-Controlがない場合のデフォルトTTL | 300 |
| `methods` | キャッシュ対象HTTPメソッド | ["GET", "HEAD"] |
| `cacheable_statuses` | キャッシュ対象ステータスコード | [200, 301, 302, 304] |
| `bypass_patterns` | キャッシュスキップ用globパターン | [] |
| `respect_vary` | Varyヘッダーによるキャッシュ分離を尊重 | true |
| `enable_etag` | ETag/If-None-Match検証を有効化 | true |
| `stale_while_revalidate` | バックグラウンド更新中にstaleを提供 | false |
| `stale_if_error` | バックエンドエラー時にstaleを提供 | false |
| `include_query` | クエリパラメータをキャッシュキーに含める | true |
| `key_headers` | キャッシュキーに含めるリクエストヘッダー | [] |

### 設定例

```toml
[[route]]
[route.conditions]
host = "example.com"
path = "/cached-api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080"

[route.cache]
  enabled = true
  max_memory_size = 104857600  # 100MB
  disk_path = "/var/cache/veil/api"
  max_disk_size = 1073741824   # 1GB
  memory_threshold = 65536     # 64KB
  default_ttl_secs = 300
  methods = ["GET", "HEAD"]
  cacheable_statuses = [200, 301, 302, 304]
  bypass_patterns = ["/cached-api/user/*", "/cached-api/session"]
  respect_vary = true
  enable_etag = true
  stale_while_revalidate = true
  stale_if_error = true
  include_query = true
  key_headers = ["Authorization"]  # ユーザーごとのキャッシュ
```

### キャッシュキー生成

キャッシュキーは以下から生成されます：
1. ホスト名
2. リクエストパス
3. クエリパラメータ（`include_query = true` の場合）
4. 指定された `key_headers` の値

### 注意事項

- `streaming` バッファリングモード使用時、kTLSのゼロコピー転送は維持されます
- キャッシュは `Cache-Control: no-cache`、`no-store`、`private` ヘッダーを尊重します
- `respect_vary = true` の場合、`Vary: *` レスポンスはキャッシュされません

> [!CAUTION]
> **stale_if_error**: この機能を有効にすると、バックエンドが502/504エラーを返した際に、veil-proxyは古いキャッシュコンテンツ（最大1時間前のデータ）を提供する場合があります。これにより可用性は向上しますが、リアルタイムの正確性が重要なアプリケーション（金融データ、医療記録、在庫管理システムなど）では**データ整合性の問題**を引き起こす可能性があります。この機能を有効にする前に、ユースケースを慎重に評価してください。

## バッファリング制御

低速クライアントによるバックエンド接続の占有を防止するためのレスポンスバッファリングを制御します。

> **補足**: `mode` は **HTTP/2 リクエスト（アップロード）方向**にも適用されます。`streaming`/`adaptive` では適格な HTTP/2 アップロードを受信しながらバックエンドへストリーミングし（*HTTP/2 リクエストストリーミング*を参照）、`full` ではアップロードを全バッファしてから転送します。

### 特徴

| 機能 | 説明 |
|------|------|
| **Streamingモード** | パススルー転送（デフォルト、kTLS維持） |
| **Fullバッファリング** | クライアント送信前にレスポンス全体をバッファ |
| **Adaptiveモード** | レスポンスサイズに基づく自動切り替え |
| **ディスクスピルオーバー** | メモリ制限超過時にディスクへ書き込み |

### モード

| モード | 説明 | 用途 |
|--------|------|------|
| `streaming` | 直接転送（デフォルト） | 大きなファイル、リアルタイムAPI、kTLS最適化 |
| `full` | レスポンス全体をバッファ | 低速クライアント対応、小さなレスポンス |
| `adaptive` | Content-Lengthに基づく自動切り替え | 混在ワークロード |

### 有効化

バッファリングはデフォルトで **streaming（パススルー）** です。ルートごとに `buffering` セクションで設定します：

```toml
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080"

[route.buffering]
  mode = "adaptive"
```

### 設定オプション

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `mode` | バッファリングモード（`streaming`/`full`/`adaptive`） | `streaming` |
| `max_memory_buffer` | メモリバッファ最大サイズ（バイト） | 10MB |
| `adaptive_threshold` | adaptiveモードのサイズ閾値（バイト） | 1MB |
| `disk_buffer_path` | ディスクスピルオーバーディレクトリ（オプション） | なし |
| `max_disk_buffer` | ディスクバッファ最大サイズ（バイト） | 100MB |
| `client_write_timeout_secs` | クライアント書き込みタイムアウト | 60 |
| `buffer_headers` | ヘッダーもボディと一緒にバッファリング | true |

### 設定例

```toml
[[route]]
[route.conditions]
host = "example.com"
path = "/buffered-api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080"

[route.buffering]
  mode = "adaptive"
  adaptive_threshold = 1048576   # 1MB
  max_memory_buffer = 10485760   # 10MB
  disk_buffer_path = "/var/tmp/veil/buffer"
  max_disk_buffer = 104857600    # 100MB
  client_write_timeout_secs = 60
  buffer_headers = true
```

### Adaptiveモードの動作

```
Content-Length <= adaptive_threshold → フルバッファリング
Content-Length > adaptive_threshold  → ストリーミング
Content-Length 不明（chunked）       → ストリーミング
```

### kTLS互換性

- **Streamingモード**: kTLS `splice(2)` ゼロコピー転送が完全に維持されます
- **Full/Adaptiveモード**: レスポンスはユーザースペースバッファを経由します（kTLS最適化なし）

> **Note**: kTLSで最大パフォーマンスを得るには、低レイテンシが重要なルートで `streaming` モードを使用してください。
