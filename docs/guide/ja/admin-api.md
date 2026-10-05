# 管理 API・キャッシュ Purge API

[← ドキュメント目次](README.md) · [English](../admin-api.md)

## Admin API

Admin APIは設定可能なプレフィックス（デフォルト: `/__admin`）配下でランタイム管理エンドポイントを提供します。すべてのエンドポイントでIP制限とBearerトークン認証が必要です（設定方法は[キャッシュPurge管理API](#キャッシュpurge管理api)を参照）。

### エンドポイント一覧

| メソッド | パス | 説明 |
|---------|------|------|
| `GET` | `/__admin/config` | 現在の設定をJSONダンプ（secretはマスク） |
| `GET` | `/__admin/stats` | ランタイム統計（uptime等） |
| `POST` | `/__admin/reload` | 設定ホットリロードをトリガー |
| `POST` | `/__admin/tls/reload` | TLS証明書ホットリロードをトリガー |
| `POST` | `/__admin/cache/purge` | キャッシュPurge（詳細は下記参照） |
| `PURGE` | 任意のパス | パスに一致するキャッシュエントリを削除 |

### 使用例

```bash
# 現在の設定を取得（secretはマスク済み）
curl -H "Authorization: Bearer changeme" https://proxy.example.com/__admin/config

# ランタイム統計を取得
curl -H "Authorization: Bearer changeme" https://proxy.example.com/__admin/stats
# → {"uptime_secs": 3600}

# 設定リロードをトリガー
curl -X POST -H "Authorization: Bearer changeme" https://proxy.example.com/__admin/reload
# → {"ok":true}

# TLS証明書リロードをトリガー
curl -X POST -H "Authorization: Bearer changeme" https://proxy.example.com/__admin/tls/reload
# → {"ok":true}
```

## キャッシュPurge管理API

プロキシを再起動せずにキャッシュエントリを無効化します。

> **必要なフィーチャー**: `--features cache` および config の `[admin]` セクション

### 設定

```toml
[admin]
enabled = true
path_prefix = "/__admin"    # 管理エンドポイントのプレフィックス
secret = "changeme"         # 認証用Bearerトークン
# allowed_ips = ["127.0.0.1", "::1", "10.0.0.0/8"]  # IPアドレス許可リスト（空の場合は全IP許可）
```

### Purge操作

すべてのPurgeリクエストに `Authorization: Bearer <secret>` ヘッダーが必要です。

| メソッド | パス | クエリ | 効果 |
|---------|------|--------|------|
| `PURGE` | 任意のパス | — | パスに一致するキャッシュエントリを削除 |
| `POST` | `/__admin/cache/purge` | `key=/path` | 正確なキーで削除 |
| `POST` | `/__admin/cache/purge` | `prefix=/api/` | プレフィックスで一括削除 |
| `POST` | `/__admin/cache/purge` | `pattern=/static/*.css` | globパターンで削除 |
| `POST` | `/__admin/cache/purge` | `all=true` | キャッシュ全削除 |

**レスポンス**: `{"purged": N}`（Nは削除されたエントリ数）

### 使用例

```bash
# 特定ページのキャッシュを削除
curl -X PURGE "https://proxy.example.com/blog/post-1" \
  -H "Authorization: Bearer changeme"

# API全体のキャッシュを削除
curl -X POST "https://proxy.example.com/__admin/cache/purge?prefix=/api/" \
  -H "Authorization: Bearer changeme"

# CSSファイルをglobパターンで削除
curl -X POST "https://proxy.example.com/__admin/cache/purge?pattern=/static/*.css" \
  -H "Authorization: Bearer changeme"

# キャッシュ全削除
curl -X POST "https://proxy.example.com/__admin/cache/purge?all=true" \
  -H "Authorization: Bearer changeme"
```

### アクセス制御

| 条件 | レスポンス |
|------|-----------|
| 送信元IPが `allowed_ips` に含まれない | `403 Forbidden` |
| `Authorization` ヘッダーなし | `401 Unauthorized` |
| 誤ったシークレット | `401 Unauthorized` |
| 管理API無効 | `404 Not Found` |
| 成功 | `200 OK`、`{"purged": N}` |

IP制限は認証より先にチェックされます。`allowed_ips` が空（デフォルト）の場合はすべてのIPを許可します。

| 形式 | 例 |
|------|-----|
| 単一IPv4 | `127.0.0.1` |
| IPv4 CIDR | `10.0.0.0/8` |
| 単一IPv6 | `::1` |
| IPv6 CIDR | `fe80::/10` |
