# Header Filter

Proxy-WASM用のシンプルなヘッダ操作フィルタ

## 概要

リクエスト/レスポンスヘッダを追加する基本的なフィルタ。
設定不要で動作し、固定のヘッダを自動的に追加します。
WASM フィルタの実装例としても参照できます。

## ビルド

```bash
cargo build --target wasm32-wasip1 --release
```

出力: `target/wasm32-wasip1/release/header_filter.wasm` (約144 KB)

## 機能

このフィルタは設定を受け取らず、以下の固定ヘッダを自動的に追加します。

### リクエストヘッダ

| ヘッダ名 | 値 | 説明 |
|---------|-----|------|
| `X-Veil-Proxy-Filter` | `header-filter-v1` | フィルタ識別子 |
| `X-Veil-Request-Id` | `req-{context_id}` | リクエストID（コンテキストIDを含む） |

### レスポンスヘッダ

| ヘッダ名 | 値 | 説明 |
|---------|-----|------|
| `X-Veil-Processed` | `true` | フィルタ処理済みフラグ |
| `X-Veil-Filter-Version` | `1.0.0` | フィルタバージョン |
| `X-Veil-Context-Id` | `{context_id}` | コンテキストID |

## 設定

このフィルタは設定を必要としません。WASMファイルを読み込むだけで動作します。

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
path = "/path/to/header_filter.wasm"
# configは不要
```

## 動作フロー

1. **リクエスト受信時**
   - `X-Veil-Proxy-Filter`と`X-Veil-Request-Id`ヘッダを追加
   - ログにコンテキストIDを出力

2. **レスポンス受信時**
   - `X-Veil-Processed`、`X-Veil-Filter-Version`、`X-Veil-Context-Id`ヘッダを追加
   - ログにコンテキストIDを出力

## 実装ファイル

| ファイル | 説明 |
|---------|------|
| `src/lib.rs` | Proxy-WASM統合・ヘッダ操作ロジック |

## 注意事項

- このフィルタは設定を受け取りません。固定のヘッダを追加するだけのシンプルな実装です
- カスタマイズが必要な場合は、ソースコードを直接編集してください
