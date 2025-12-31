# JWT Filter

Proxy-WASM用のJWT認証フィルタ（HS256/RS256対応、JWKSサポート）

## 概要

JWT（JSON Web Token）の検証と認証を行うフィルタ。
HS256（HMAC-SHA256）とRS256（RSA-SHA256）の両方に対応し、JWKS（JSON Web Key Set）URLからの公開鍵取得もサポートしています。

## ビルド

```bash
cargo build --target wasm32-wasip1 --release
```

出力: `target/wasm32-wasip1/release/jwt_filter.wasm`

## 機能

- **アルゴリズム対応**: HS256（HMAC-SHA256）、RS256（RSA-SHA256）
- **JWKSサポート**: OIDCプロバイダーなどからJWKSを取得して検証
- **クレーム検証**: `exp`（有効期限）、`iss`（発行者）、`aud`（対象者）の検証
- **クレーム転送**: JWTクレームをHTTPヘッダにマッピング
- **パス除外**: 認証をスキップするパスを設定可能

## 設定例

### HS256（シークレットキー）

```json
{
  "algorithms": ["HS256"],
  "static_keys": {
    "default": "dGhpcyBpcyBhIHNlY3JldCBrZXkgZm9yIEhTMjU2",
    "key1": "YW5vdGhlciBzZWNyZXQga2V5"
  },
  "header_name": "Authorization",
  "issuer": "https://example.com",
  "audience": "api.example.com",
  "claims_to_headers": {
    "sub": "X-User-ID",
    "email": "X-User-Email"
  },
  "skip_paths": ["/health", "/metrics"]
}
```

### RS256（JWKS）

```json
{
  "algorithms": ["RS256"],
  "jwks_url": "https://auth.example.com/.well-known/jwks.json",
  "jwks_cache_ttl_secs": 3600,
  "jwks_upstream": "jwks",
  "issuer": "https://auth.example.com",
  "audience": "api.example.com",
  "claims_to_headers": {
    "sub": "X-User-ID",
    "email": "X-User-Email",
    "roles": "X-User-Roles"
  },
  "skip_paths": ["/health", "/metrics", "/public"]
}
```

## 設定オプション

| オプション | 型 | デフォルト | 説明 |
|-----------|------|---------|------|
| `algorithms` | array | ["RS256", "HS256"] | 許可するアルゴリズム |
| `static_keys` | object | {} | HS256用のシークレットキー（kid → base64urlエンコードされたシークレット） |
| `jwks_url` | string | null | RS256用のJWKS取得URL |
| `jwks_cache_ttl_secs` | number | 3600 | JWKSキャッシュのTTL（秒） |
| `jwks_upstream` | string | "jwks" | JWKS取得用のupstream名 |
| `header_name` | string | "Authorization" | トークンを取得するヘッダ名 |
| `issuer` | string | null | 期待される発行者（`iss`クレーム） |
| `audience` | string | null | 期待される対象者（`aud`クレーム） |
| `claims_to_headers` | object | {} | JWTクレームをHTTPヘッダにマッピング（クレーム名 → ヘッダ名） |
| `skip_paths` | array | ["/health", "/metrics"] | 認証をスキップするパス |

## トークン形式

### Authorizationヘッダ

```
Authorization: Bearer {token}
```

または

```
Authorization: {token}
```

`Bearer`プレフィックスは自動的に処理されます。

### JWT構造

```
{header}.{payload}.{signature}
```

- **header**: アルゴリズムとキーID（kid）を含む
- **payload**: クレーム（exp、iss、aud、sub、emailなど）
- **signature**: ヘッダとペイロードの署名

## クレーム検証

### 有効期限（exp）

トークンの`exp`クレームが現在時刻より前の場合は拒否されます。

### 発行者（iss）

`issuer`が設定されている場合、JWTの`iss`クレームと一致する必要があります。

### 対象者（aud）

`audience`が設定されている場合、JWTの`aud`クレームが以下と一致する必要があります：
- 文字列の場合: 完全一致
- 配列の場合: 配列内に含まれる

## クレーム転送

`claims_to_headers`で設定されたクレームは、検証成功後にHTTPリクエストヘッダとして追加されます。

例：
```json
{
  "claims_to_headers": {
    "sub": "X-User-ID",
    "email": "X-User-Email"
  }
}
```

JWTに`sub: "user123"`と`email: "user@example.com"`が含まれている場合：
- `X-User-ID: user123`
- `X-User-Email: user@example.com`

がリクエストヘッダに追加されます。

## 動作フロー

1. **リクエスト受信**
   - パスがスキップ対象かチェック
   - 指定ヘッダからトークンを抽出

2. **トークン解析**
   - JWTを3つの部分（header、payload、signature）に分割
   - ヘッダをデコードしてアルゴリズムを確認
   - ペイロードをデコードしてクレームを取得

3. **クレーム検証**
   - `exp`: 有効期限チェック
   - `iss`: 発行者チェック（設定時）
   - `aud`: 対象者チェック（設定時）

4. **署名検証**
   - **HS256**: 設定されたシークレットキーで検証
   - **RS256**: JWKSから公開鍵を取得して検証（必要に応じてキャッシュ）

5. **成功時**
   - クレームをHTTPヘッダにマッピング
   - リクエストを継続

6. **失敗時**
   - 401 Unauthorizedレスポンスを返却
   - `WWW-Authenticate: Bearer`ヘッダを追加

## JWKSサポート

### JWKS URL

OIDCプロバイダーやAuth0などが提供するJWKSエンドポイントから公開鍵を取得します。

例：
```
https://auth.example.com/.well-known/jwks.json
```

### JWKSキャッシュ

取得したJWKSは設定されたTTL（デフォルト3600秒）の間キャッシュされます。
これにより、毎回JWKSを取得する必要がなくなり、パフォーマンスが向上します。

### JWKS形式

```json
{
  "keys": [
    {
      "kty": "RSA",
      "kid": "key-id-1",
      "use": "sig",
      "alg": "RS256",
      "n": "base64urlエンコードされたmodulus",
      "e": "base64urlエンコードされたexponent"
    }
  ]
}
```

## エラーレスポンス

認証失敗時は以下の形式で401レスポンスを返却します：

```json
{
  "error": "unauthorized",
  "message": "エラーメッセージ"
}
```

エラーメッセージの例：
- `"Missing authentication token"`
- `"Invalid token format"`
- `"Token expired"`
- `"Invalid issuer"`
- `"Invalid audience"`
- `"Invalid signature"`
- `"Key not found"`
- `"Unsupported algorithm"`

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
path = "/path/to/jwt_filter.wasm"
config = '''
{
  "algorithms": ["RS256"],
  "jwks_url": "https://auth.example.com/.well-known/jwks.json",
  "jwks_upstream": "jwks",
  "issuer": "https://auth.example.com",
  "audience": "api.example.com",
  "claims_to_headers": {
    "sub": "X-User-ID",
    "email": "X-User-Email"
  },
  "skip_paths": ["/health", "/metrics"]
}
'''
```

### リクエスト例

```bash
curl -H "Authorization: Bearer eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9..." \
  http://example.com/api/users
```

## 実装ファイル

| ファイル | 説明 |
|---------|------|
| `src/lib.rs` | Proxy-WASM統合・JWT検証ロジック |
| `src/crypto.rs` | HS256/RS256署名検証実装 |
| `src/jwks.rs` | JWKS取得・キャッシュ・パース実装 |

## セキュリティ考慮事項

- **シークレットキーの管理**: HS256のシークレットキーはbase64urlエンコードして設定してください
- **JWKS URLの検証**: 信頼できるJWKS URLのみを使用してください
- **HTTPSの使用**: JWKS URLはHTTPSで取得することを推奨します
- **トークンの有効期限**: `exp`クレームの検証を必ず有効にしてください
- **発行者・対象者の検証**: 本番環境では`issuer`と`audience`を設定してください

## 注意事項

- JWKSの取得は非同期で行われ、初回リクエスト時に遅延が発生する可能性があります
- JWKSキャッシュのTTLは適切に設定してください（短すぎると頻繁に取得、長すぎると鍵の更新が反映されない）
- `skip_paths`は認証を完全にスキップするため、公開エンドポイントのみに設定してください

