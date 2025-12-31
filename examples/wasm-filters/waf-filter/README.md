# WAF Filter

Proxy-WASM用のWeb Application Firewall (WAF) フィルタ

## 概要

OWASP ModSecurity Core Rule Set (CRS) にインスパイアされた正規表現ベースのWAFフィルタ。
SQLインジェクション、XSS、パストラバーサル、コマンドインジェクションを検出・ブロック。

## ビルド

```bash
cargo build --target wasm32-wasip1 --release
```

出力: `target/wasm32-wasip1/release/waf_filter.wasm` (約1.2 MB)

## 設定例

### 基本設定（CRS Level 2）

```json
{
  "mode": "block",
  "crs_level": "2",
  "anomaly_scoring": false,
  "inspect_body": false,
  "whitelist_paths": ["/health", "/metrics"]
}
```

### CRS Level 1（最小限の保護）

```json
{
  "mode": "block",
  "crs_level": "1",
  "anomaly_scoring": false
}
```

### CRS Level 3（厳格な保護）

```json
{
  "mode": "block",
  "crs_level": "3",
  "anomaly_scoring": true,
  "anomaly_threshold": 10,
  "inspect_body": true
}
```

### 検出モード（ブロックしない）

```json
{
  "mode": "detect",
  "crs_level": "2",
  "inspect_body": true
}
```

## 設定オプション

| オプション | 型 | デフォルト | 説明 |
|-----------|------|---------|------|
| `mode` | string | "block" | "block": ブロック, "detect": 検出のみ, "off": 無効化 |
| `crs_level` | string | "2" | CRS保護レベル（"1", "2", "3"） |
| `anomaly_scoring` | bool | false | 異常スコアリングモード（true: スコア累積、false: 即座にブロック） |
| `anomaly_threshold` | number | 5 | 異常スコア閾値（anomaly_scoring=true時） |
| `inspect_body` | bool | false | リクエストボディを検査するか |
| `whitelist_paths` | array | ["/health", "/metrics"] | 検査をスキップするパス |
| `whitelist_ips` | array | [] | 検査をスキップするIPアドレス |
| `custom_rules` | array | [] | カスタムルール定義 |

## CRS保護レベル

### Level 1: 基本保護

最小限の高信頼度ルールで、誤検知が少ない。
高トラフィックAPIや可用性を優先するアプリケーションに適しています。

検出対象:
- SQLインジェクション（UNION SELECT、DROP/DELETE/TRUNCATE、INSERT INTO）
- XSS（`<script>`タグ、イベントハンドラ）
- パストラバーサル（`../`シーケンス、機密ファイルパス）

### Level 2: 中程度の保護（デフォルト）

Level 1に加えて、より多くの攻撃パターンを検出します。

追加検出対象:
- コマンドインジェクション（シェルメタ文字、危険なコマンド）
- リモート/ローカルファイルインクルード（RFI/LFI）
- スキャナ検出（User-Agent、パスパターン）

### Level 3: 厳格な保護

Level 2に加えて、高度な攻撃手法を検出します。

追加検出対象:
- プロトコル異常（NULLバイト、制御文字）
- エバージョン手法（エンコーディング、複数エンコーディング）
- データ漏洩パターン（クレジットカード、SSN、パスワード）

## カスタムルール

カスタムルールを追加して、特定のパターンを検出できます。

```json
{
  "mode": "block",
  "crs_level": "2",
  "custom_rules": [
    {
      "id": "custom-001",
      "pattern": "(?i)admin.*bypass",
      "targets": ["uri", "query"],
      "action": "block",
      "message": "Admin bypass attempt detected"
    }
  ]
}
```

カスタムルールの設定:

| フィールド | 型 | 説明 |
|-----------|------|------|
| `id` | string | ルールID（必須） |
| `pattern` | string | 正規表現パターン（必須） |
| `targets` | array | 検査対象（"uri", "query", "body", "user-agent"など） |
| `action` | string | アクション（"block", "log", "allow"） |
| `message` | string | 検出時のメッセージ |

## 動作フロー

1. **リクエスト受信**
   - モードが`off`の場合は通過

2. **ホワイトリストチェック**
   - パスが`whitelist_paths`に含まれる場合は通過
   - IPアドレスが`whitelist_ips`に含まれる場合は通過

3. **検査対象の収集**
   - URI/パス
   - クエリ文字列
   - ヘッダ（User-Agent、Referer、Cookie）
   - ボディ（`inspect_body=true`かつPOSTリクエストの場合）

4. **ルール検査**
   - 選択されたCRSレベル（1/2/3）のルールを適用
   - カスタムルールを適用
   - URLデコードを自動的に実行

5. **結果判定**
   - **anomaly_scoring=false（即座モード）**: 最初のマッチで判定
   - **anomaly_scoring=true（スコアリングモード）**: スコアを累積し、閾値に達したら判定
   - **block モード**: 403レスポンスを返却（`X-WAF-Block`、`X-WAF-Category`ヘッダ付き）
   - **detect モード**: ログ出力のみ（リクエストは継続）

## レスポンスヘッダ

ブロック時には以下のヘッダが追加されます：

| ヘッダ | 説明 |
|--------|------|
| `X-WAF-Block` | マッチしたルールID |
| `X-WAF-Category` | 攻撃カテゴリ（例: "SQL Injection"） |

## 実装ファイル

| ファイル | 説明 |
|---------|------|
| `src/lib.rs` | Proxy-WASM統合 |
| `src/rules.rs` | ルールエンジン・設定管理 |
| `src/crs_level1.rs` | CRS Level 1ルール定義 |
| `src/crs_level2.rs` | CRS Level 2ルール定義 |
| `src/crs_level3.rs` | CRS Level 3ルール定義 |

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
path = "/path/to/waf_filter.wasm"
config = '''
{
  "mode": "block",
  "crs_level": "2",
  "anomaly_scoring": false,
  "inspect_body": true,
  "whitelist_paths": ["/health", "/metrics", "/public"]
}
'''
```

## 注意事項

- 本番環境では`whitelist_paths`を適切に設定してください
- 誤検知を避けるため、まず`detect`モードでテストしてから`block`モードに切り替えることを推奨します
- `anomaly_scoring=true`の場合、複数の低重要度のマッチでもブロックされる可能性があります
- `inspect_body=true`はパフォーマンスに影響するため、必要な場合のみ有効にしてください
- CRS Level 3は最も厳格ですが、誤検知の可能性も高くなります
