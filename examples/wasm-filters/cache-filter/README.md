# Cache Filter

Proxy-WASM用の分散キャッシュフィルタ（Redis/Memcached対応）

## 概要

HTTP GETレスポンスを外部キャッシュバックエンドにキャッシュするフィルタ。
Redis（Webdis HTTP API経由）とMemcached（HTTP API経由）の両方に対応しています。

## ビルド

```bash
cargo build --target wasm32-wasip1 --release
```

出力: `target/wasm32-wasip1/release/cache_filter.wasm`

## 機能

- **キャッシュバックエンド**: Redis（Webdis経由）またはMemcached（HTTP API経由）
- **キャッシュキー生成**: URL、HTTPメソッド、指定ヘッダから生成
- **Cache-Control対応**: `no-store`、`private`ヘッダでキャッシュをスキップ
- **キャッシュバイパス**: `X-Cache-Bypass`ヘッダで強制的にスキップ
- **パス除外**: 特定パス（`/health`、`/metrics`など）をキャッシュ対象外に設定可能
- **TTL設定**: デフォルトTTLとCache-ControlヘッダからのTTL抽出

## 設定例

### 基本設定（Redis）

```json
{
  "backend": "redis",
  "redis": {
    "host": "webdis"
  },
  "default_ttl_secs": 300,
  "cache_methods": ["GET", "HEAD"],
  "key_headers": ["Accept", "Accept-Encoding"],
  "skip_paths": ["/health", "/metrics"],
  "upstream": "cache"
}
```

### Memcached設定

```json
{
  "backend": "memcached",
  "memcached": {
    "host": "memcached-http"
  },
  "default_ttl_secs": 600,
  "upstream": "cache"
}
```

## 設定オプション

| オプション | 型 | デフォルト | 説明 |
|-----------|------|---------|------|
| `backend` | string | "redis" | キャッシュバックエンド（"redis" または "memcached"） |
| `redis.host` | string | "webdis" | Redis（Webdis）のホスト名 |
| `memcached.host` | string | "memcached-http" | Memcached HTTP APIのホスト名 |
| `default_ttl_secs` | number | 300 | デフォルトのTTL（秒） |
| `cache_methods` | array | ["GET", "HEAD"] | キャッシュ対象のHTTPメソッド |
| `key_headers` | array | ["Accept", "Accept-Encoding"] | キャッシュキーに含めるヘッダ名 |
| `bypass_header` | string | "X-Cache-Bypass" | キャッシュバイパス用ヘッダ名 |
| `skip_paths` | array | ["/health", "/metrics"] | キャッシュ対象外のパス |
| `upstream` | string | "cache" | キャッシュバックエンドへのupstream名 |

## 動作フロー

1. **リクエスト受信**
   - HTTPメソッドがキャッシュ対象かチェック
   - パスがスキップ対象かチェック
   - `X-Cache-Bypass`ヘッダの有無をチェック

2. **キャッシュキー生成**
   - メソッド、パス、指定ヘッダからキーを生成
   - FNV-1aハッシュでキーを短縮

3. **キャッシュ取得**
   - バックエンドからキャッシュを取得
   - ヒット時: キャッシュされたレスポンスを返却（`X-Cache: HIT`）
   - ミス時: アップストリームへリクエストを転送

4. **レスポンス処理**
   - ステータスコードが200-299の場合のみキャッシュ
   - `Cache-Control: no-store`または`private`の場合はスキップ
   - レスポンスボディをシリアライズしてキャッシュに保存

## キャッシュキー形式

```
veil:cache:{hash}
```

ハッシュは以下の要素から生成されます：
- HTTPメソッド
- リクエストパス
- 指定されたヘッダ（`key_headers`で設定）

## キャッシュデータ形式

キャッシュされたレスポンスは以下のJSON形式で保存されます：

```json
{
  "status": 200,
  "headers": [["content-type", "application/json"]],
  "body": "base64エンコードされたボディ"
}
```

## バックエンド詳細

### Redis（Webdis）

WebdisはRedisコマンドをHTTP APIとして提供します。

- **GET**: `GET /GET/{key}` → `{"GET": "value"}` または `{"GET": null}`
- **SET**: `GET /SET/{key}/{value}/EX/{ttl}` → `{"SET": [true, "OK"]}`

### Memcached（HTTP API）

汎用的なMemcached HTTPプロキシを使用します。

- **GET**: `GET /get?key={key}` → `{"value": "...", "found": true}`
- **SET**: `POST /set` with JSON body `{"key": "...", "value": "...", "ttl": 300}`

## レスポンスヘッダ

| ヘッダ | 値 | 説明 |
|--------|-----|------|
| `X-Cache` | `HIT` または `MISS` | キャッシュヒット/ミスを示す |

## 使用例

### veil-proxy設定（config.toml）

```toml
[[route]]
[route.conditions]
path = "/api/*"
[route.action]
type = "Proxy"
upstream = "backend"

[[routes.wasm_filters]]
path = "/path/to/cache_filter.wasm"
config = '''
{
  "backend": "redis",
  "redis": {
    "host": "webdis"
  },
  "default_ttl_secs": 300,
  "cache_methods": ["GET"],
  "key_headers": ["Accept", "Accept-Language"],
  "skip_paths": ["/health", "/metrics"],
  "upstream": "cache"
}
'''
```

### キャッシュバイパス

特定のリクエストをキャッシュから除外する場合：

```bash
curl -H "X-Cache-Bypass: true" http://example.com/api/data
```

## 実装ファイル

| ファイル | 説明 |
|---------|------|
| `src/lib.rs` | Proxy-WASM統合・キャッシュロジック |
| `src/redis.rs` | Redis（Webdis）バックエンド実装 |
| `src/memcached.rs` | Memcached HTTP APIバックエンド実装 |

## 注意事項

- キャッシュバックエンドへの接続は非同期で行われます
- キャッシュの保存はfire-and-forget方式（エラー時もリクエストは継続）
- キャッシュキーの衝突を避けるため、適切な`key_headers`を設定してください
- 本番環境では`skip_paths`を適切に設定し、動的コンテンツを除外してください

