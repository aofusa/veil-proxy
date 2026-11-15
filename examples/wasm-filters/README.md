# WASM Filters

veil-proxy用のProxy-WASMフィルタサンプル集

## フィルタ一覧

| フィルタ | 説明 | サイズ |
|---------|------|--------|
| [passthrough-filter](./passthrough-filter/) | 無変更パススルー（perf 計測用ベースライン、F-89） | 130 KB |
| [header-filter](./header-filter/) | ヘッダ追加・削除 | 144 KB |
| [baseline-filter](./baseline-filter/) | 要求をフィルタだけで返す（`Respond` + モジュール・要求本文のフェーズ・`data_files`・`instance_reuse` の例と試験、F-271） | 144 KB |
| [cache-filter](./cache-filter/) | 分散キャッシュ（Redis/Memcached） | - |
| [jwt-filter](./jwt-filter/) | JWT認証（HS256/RS256、JWKS） | - |
| [waf-filter](./waf-filter/) | Web Application Firewall | 1.2 MB |
| [lua-filter](./lua-filter/) | Luaスクリプト実行 | 503 KB |

## ビルド方法

### 前提条件

```bash
# WASM ターゲットをインストール
rustup target add wasm32-wasip1
```

### 全フィルタをビルド

```bash
cd examples/wasm-filters

# Header Filter
cd header-filter && cargo build --target wasm32-wasip1 --release && cd ..

# Cache Filter
cd cache-filter && cargo build --target wasm32-wasip1 --release && cd ..

# JWT Filter
cd jwt-filter && cargo build --target wasm32-wasip1 --release && cd ..

# WAF Filter
cd waf-filter && cargo build --target wasm32-wasip1 --release && cd ..

# Lua Filter
cd lua-filter && cargo build --target wasm32-wasip1 --release && cd ..
```

### ビルド済みWASMファイルの場所

```
target/wasm32-wasip1/release/
├── header_filter.wasm
├── cache_filter.wasm
├── jwt_filter.wasm
├── waf_filter.wasm
└── lua_filter.wasm
```

## veil-proxyでの使用方法

### 設定例 (config.toml)

```toml
# WASMフィルタを読み込む（モジュールごとに名前・パス・設定・権限を書く）
[wasm]
enabled = true

[[wasm.modules]]
name = "waf"
path = "/path/to/waf_filter.wasm"
# インスタンスを要求の間で使い回す（ルールの正規表現をインスタンスごとに 1 回だけ組み立てる）
instance_reuse = true
configuration = '''
{
  "mode": "block",
  "crs_level": "2"
}
'''
[wasm.modules.capabilities]
allow_logging = true
allow_request_headers_read = true
allow_send_local_response = true

[[wasm.modules]]
name = "lua"
path = "/path/to/lua_filter.wasm"
configuration = '''
{
  "script": "function on_request() veil.log('info', veil.get_path()) return 'continue' end"
}
'''
[wasm.modules.capabilities]
allow_logging = true
allow_request_headers_read = true
allow_request_headers_write = true
allow_response_headers_read = true
allow_response_headers_write = true
allow_send_local_response = true

# ルートの modules に並べた順に走る
[[route]]
modules = ["waf", "lua"]
[route.conditions]
path = "/api/*"
[route.action]
type = "Proxy"
upstream = "backend"
```

権限（`capabilities`）は既定ですべて無効です。各フィルタの README の設定例と、
[docs/guide/wasm.md](../../docs/guide/wasm.md) を参照してください。

## フィルタ詳細

### Header Filter

シンプルなヘッダ操作フィルタ。Proxy-WASMの基本実装例。
設定不要で動作し、固定のヘッダを自動的に追加します。

詳細: [header-filter/README.md](./header-filter/README.md)

### Cache Filter

HTTP GETレスポンスを外部キャッシュバックエンド（Redis/Memcached）にキャッシュするフィルタ。

機能:
- Redis（Webdis HTTP API経由）対応
- Memcached（HTTP API経由）対応
- キャッシュキー生成（URL、ヘッダベース）
- Cache-Controlヘッダ対応
- キャッシュバイパス機能

詳細: [cache-filter/README.md](./cache-filter/README.md)

### JWT Filter

JWT（JSON Web Token）の検証と認証を行うフィルタ。

機能:
- HS256（HMAC-SHA256）対応
- RS256（RSA-SHA256）対応
- JWKS（JSON Web Key Set）サポート
- クレーム検証（exp、iss、aud）
- クレーム転送（HTTPヘッダマッピング）

詳細: [jwt-filter/README.md](./jwt-filter/README.md)

### WAF Filter

OWASP ModSecurity CRSベースの攻撃検出。

対応攻撃:
- SQLインジェクション
- クロスサイトスクリプティング (XSS)
- パストラバーサル
- コマンドインジェクション
- リモート/ローカルファイルインクルード（RFI/LFI）

CRS保護レベル:
- Level 1: 基本保護（誤検知少）
- Level 2: 中程度の保護（デフォルト）
- Level 3: 厳格な保護（包括的）

詳細: [waf-filter/README.md](./waf-filter/README.md)

### Lua Filter

純粋Rust製Luaインタープリタ (外部ライブラリ不使用)。

機能:
- クロージャ
- パターンマッチング (`string.match`, `string.gsub`)
- 標準ライブラリ (`string.*`, `math.*`, `table.*`)
- ビット演算子
- Proxy-WASMバインディング（`veil.*`）

詳細: [lua-filter/README.md](./lua-filter/README.md)

## ライセンス

各フィルタはveil-proxyと同じライセンスの下で提供されます。
