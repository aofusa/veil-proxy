# WASM 拡張（Proxy-Wasm）

[← ドキュメント目次](README.md) · [English](../wasm.md)

## WASM拡張機能

VeilはProxy-Wasm ABI v0.2.1に完全準拠したWASM拡張システムを提供します。Nginx/Envoy向けに作成されたProxy-WasmモジュールをそのままVeilで使用できます。

### 特徴

- **Proxy-Wasm v0.2.1準拠**: Nginx/Envoyと100%互換
- **AOTコンパイル・自動キャッシュ**: モジュールを AOT コンパイルし、初回ロード時に `.wasm` の隣へ `.cwasm` サイドカーを自動生成。次回起動時はそれを `deserialize` して高速起動する（`.wasm` が新しい場合や wasmtime 版が変わった場合は自動無効化し、エラー時は再コンパイルへフォールバック）。明示的な `.cwasm` パス指定も可。
- **Pooling Allocator**: 高速なインスタンス化
- **非同期実行（Head-of-Line ブロッキングなし）**: wasmtime async support + Fuel ベースの協調的 yield（約10k命令ごと）で実行。CPU バウンドなフィルタが io_uring ワーカーの他 I/O をストールさせない
- **Capability制限**: モジュールごとの細かい権限制御（デフォルト全て無効）
- **Pulley インタープリタ選択**（`[wasm] interpreter = true`、F-135）: Cranelift ネイティブ JIT の代わりに wasmtime の Pulley ポータブルバイトコードインタープリタで実行し、ネイティブコードを一切生成しない。W^X 制約のあるホストや実行可能 `mmap` を許可できない環境向け。ネイティブ JIT より低速。既定は `false`（Cranelift JIT）。AOT サイドカーキャッシュはファイル名を分離（`.pulley.cwasm`）し、JIT 版と Pulley 版が同じキャッシュを奪い合って毎回再コンパイルする事態を避ける。**OpenBSD（全アーキ）・NetBSD（全アーキ）・FreeBSD aarch64 ではこの設定は常に無視され、常に Pulley が使われる**（B-52/B-55。`interpreter = false` を明示指定すると起動時に警告ログが出るが無視される）
- **HTTP/3 が HTTP/1.1・HTTP/2 と同等のライフサイクルに対応**（F-132）: HTTP/3 経路でも `on_log` がすべての離脱点（`LocalResponse` による早期 return を含む）から確実に呼ばれるようになり、`Backend::File`（静的配信）ルートにも `on_response_headers` フィルタが適用される。`Backend::Proxy` ルートではリクエスト/レスポンスボディフィルタ（`on_request_body`/`on_response_body`）も動作する（HTTP/3 は WASM 適用時に本文全体を既にバッファしているため `end_of_stream=true` の1回呼びで実装）。レスポンス本文を書き換えた場合は `content-length` を新しい長さへ更新し、nghttp3 の `H3_MESSAGE_ERROR`（本文長不一致・重複)を回避する。
- **gRPC トレイラーフィルタ**（F-133）: gRPC over H2C で `on_request_trailers`/`on_response_trailers` が実行されるようになり、レスポンストレイラーの `grpc-status`/`grpc-message` を書き換えたり、クライアント送信のリクエストトレイラーを見て `LocalResponse` でリクエストを拒否できる。リクエストトレイラーは観測のみ（バックエンドへの転送機構は無い）。レスポンストレイラーの `Pause`/`LocalResponse` は HEADERS/DATA が送出済みのため適用できず、元のトレイラーをそのまま通して警告ログを出す。
- **L4 network filter（Proxy-Wasm `StreamContext` ABI）**（F-133）: `[[l4]]` TCP リスナーに `wasm_modules = ["name"]` を指定すると、生バイト列を検査・書き換えできる（`proxy_on_new_connection`、`proxy_on_downstream_data`、`proxy_on_upstream_data`、`proxy_on_downstream_connection_close`、`proxy_on_upstream_connection_close`）。データは `BufferType::DownstreamData`/`UpstreamData`（値 `2`/`3`）として `proxy_get_buffer_bytes`/`proxy_set_buffer_bytes` 経由でアクセスし、`proxy_close_stream` で接続をクローズできる。**未設定時はコスト増なし**: `wasm_modules` が空（既定）なら従来どおり `splice`/ゼロコピー経路のまま `is_empty()` 判定 1 つだけが追加コスト。モジュールを設定すると（接続確立時に1回だけ）`splice` を使わないユーザー空間バッファ経由の転送へ切り替わる（WASM にデータを見せる必要があるため）。

### ビルド

```bash
cargo build --release --features wasm
```

### 設定

```toml
[wasm]
enabled = true

# Cranelift ネイティブ JIT の代わりに Pulley インタープリタで実行する（F-135）。
# デフォルト: false。OpenBSD では常に true 相当（B-52、この設定値は無視される）。
# interpreter = false

# デフォルト設定（オプション）
[wasm.defaults]
# 最大実行時間（ミリ秒、デフォルト: 100）
max_execution_time_ms = 100

  # Poolingアロケータ設定
  [wasm.defaults.pooling]
  # メモリプール総数（デフォルト: 128）
  total_memories = 128
  # テーブルプール総数（デフォルト: 128）
  total_tables = 128
  # インスタンスごとの最大メモリサイズ（デフォルト: 10MB）
  max_memory_size = 10485760

# モジュール定義
[[wasm.modules]]
name = "my_filter"
path = "/etc/veil/wasm/my_filter.wasm"
configuration = '{"key": "value"}'

[wasm.modules.capabilities]
# 全てデフォルトfalse、必要な権限のみ有効化
allow_logging = true
allow_request_headers_read = true
allow_request_headers_write = true
allow_send_local_response = true
allow_http_calls = true
allowed_upstreams = ["webdis"]  # HTTP呼び出し許可先
```

**注意**: 特定のルートにWASMモジュールを適用するには、ルート設定の`modules`フィールドを使用してください（ルーティングセクションを参照）。

### プラグイン設定の記述方法（F-148）

`configuration` は、モジュールへ Proxy-Wasm の *plugin configuration*
（`proxy_on_configure` / `proxy_get_buffer(PluginConfiguration)`）として渡されるバイト列です。
**文字列**（従来方式。バイト列をそのまま渡す。多くは JSON）と
**TOML テーブル**（JSON オブジェクトへ変換してから渡すため、モジュール側の実装は変更不要）の
どちらでも記述できます。

```toml
# 1. 文字列形式（従来どおり。バイト列は 1 バイトも変わらない）
[[wasm.modules]]
name = "header_filter"
path = "/etc/veil/wasm/header_filter.wasm"
configuration = '{"header_name": "x-veil"}'

# 2. TOML テーブル形式（JSON オブジェクトへ変換して渡す）
[[wasm.modules]]
name = "waf_filter"
path = "/etc/veil/wasm/waf_filter.wasm"
[wasm.modules.configuration]
mode = "block"
max_body_size = 65536
patterns = ["union select", "<script"]
```

TOML → JSON の変換規則: String → string、Integer/Float → number（`NaN`/`Inf` は `null`）、
Boolean → bool、Datetime → RFC 3339 相当の string、Array → array、Table → object。

#### ルート単位の上書き

`[[route]]`（および `[[l4]]`）に `module_configuration`（モジュール名 → 設定値のマップ）を
書くと、**同じモジュールをルートごとに違うパラメータで使えます**
（同じ `.wasm` を別名で二重にロードする必要がありません）。

```toml
[[route]]
modules = ["header_filter", "waf_filter"]

# このルートだけ WAF を log_only にする（他のルートはモジュール定義の "block" のまま）
[route.module_configuration.waf_filter]
mode = "log_only"

[route.conditions]
path = "/static/*"
[route.action]
type = "File"
path = "./www"
```

合成規則（ルート優先）:

| モジュール定義 | ルート | 実効設定 |
|---|---|---|
| なし | なし | 空バイト列 |
| あり | なし | モジュール定義の値 |
| なし | あり | ルートの値 |
| Table | Table | **ディープマージ**（同名キーはルート優先、ネストしたテーブルも再帰マージ、配列は置換） |
| 上記以外（片方でも文字列） | あり | ルートの値で**完全置換**（文字列は不透明なバイト列でマージ不能なため） |

`module_configuration` に、そのルートの `modules`（L4 なら `wasm_modules`）へ
列挙していないモジュール名を書くと**起動時に設定エラー**になります。

合成と TOML → JSON 変換は**設定ロード時（起動・SIGHUP リロード）に 1 回だけ**実行され、
リクエスト経路では `Arc` の clone しか行いません。

### デフォルト設定

`[wasm.defaults]` セクションでは、WASMランタイムのグローバル設定を行えます：

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `max_execution_time_ms` | WASM呼び出しごとの最大実行時間（ミリ秒） | 100 |
| `interpreter` | Cranelift ネイティブ JIT の代わりに Pulley インタープリタで実行する（F-135）。OpenBSD では常に `true` 相当（B-52） | false |

#### Poolingアロケータ設定

`[wasm.defaults.pooling]` セクションでは、高速インスタンス化のためのPoolingアロケータを設定します：

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `total_memories` | メモリプール総数 | 128 |
| `total_tables` | テーブルプール総数 | 128 |
| `max_memory_size` | インスタンスごとの最大メモリサイズ（バイト） | 10MB (10485760) |

### Capability一覧

| Capability | 説明 | デフォルト |
|-----------|------|----------|
| `allow_logging` | ログ出力 | false |
| `allow_metrics` | メトリクス操作 | false |
| `allow_shared_data` | 共有データ | false |
| `allow_request_headers_read` | リクエストヘッダー読み取り | false |
| `allow_request_headers_write` | リクエストヘッダー書き換え | false |
| `allow_request_body_read` | リクエストボディ読み取り | false |
| `allow_request_body_write` | リクエストボディ書き換え | false |
| `allow_response_headers_read` | レスポンスヘッダー読み取り | false |
| `allow_response_headers_write` | レスポンスヘッダー書き換え | false |
| `allow_response_body_read` | レスポンスボディ読み取り | false |
| `allow_response_body_write` | レスポンスボディ書き換え | false |
| `allow_downstream_data_read` | L4 downstream（クライアント→proxy）接続データ読み取り（F-133） | false |
| `allow_downstream_data_write` | L4 downstream 接続データ書き換え（F-133） | false |
| `allow_upstream_data_read` | L4 upstream（proxy→バックエンド）接続データ読み取り（F-133） | false |
| `allow_upstream_data_write` | L4 upstream 接続データ書き換え（F-133） | false |
| `allow_send_local_response` | ローカルレスポンス送信 | false |
| `allow_http_calls` | HTTP外部呼び出し | false |
| `allowed_upstreams` | 許可upstream | [] |

### Rustによる拡張機能開発

#### 1. プロジェクト作成

```bash
cargo new --lib my-filter
cd my-filter
```

#### 2. Cargo.toml

```toml
[package]
name = "my-filter"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
proxy-wasm = "0.2"
log = "0.4"

[profile.release]
lto = true
opt-level = "s"

[workspace]
```

#### 3. src/lib.rs

```rust
use proxy_wasm::traits::*;
use proxy_wasm::types::*;

proxy_wasm::main! {{
    proxy_wasm::set_log_level(LogLevel::Debug);
    proxy_wasm::set_root_context(|_| -> Box<dyn RootContext> {
        Box::new(MyFilterRoot)
    });
}}

struct MyFilterRoot;

impl Context for MyFilterRoot {}

impl RootContext for MyFilterRoot {
    fn get_type(&self) -> Option<ContextType> {
        Some(ContextType::HttpContext)
    }

    fn create_http_context(&self, context_id: u32) -> Option<Box<dyn HttpContext>> {
        Some(Box::new(MyFilter { context_id }))
    }
}

struct MyFilter {
    context_id: u32,
}

impl Context for MyFilter {}

impl HttpContext for MyFilter {
    fn on_http_request_headers(&mut self, _: usize, _: bool) -> Action {
        // リクエストヘッダーにカスタムヘッダーを追加
        self.add_http_request_header("X-My-Filter", "enabled");
        
        // ヘッダー値を取得
        if let Some(path) = self.get_http_request_header(":path") {
            log::info!("Request path: {}", path);
        }
        
        Action::Continue
    }

    fn on_http_response_headers(&mut self, _: usize, _: bool) -> Action {
        // レスポンスヘッダーを追加
        self.add_http_response_header("X-Processed-By", "my-filter");
        Action::Continue
    }
}
```

#### 4. ビルド

```bash
# WASIターゲットを追加
rustup target add wasm32-wasip1

# ビルド
cargo build --target wasm32-wasip1 --release

# 出力: target/wasm32-wasip1/release/my_filter.wasm
```

#### 5. 配置と設定

```bash
# WASMモジュールを配置
cp target/wasm32-wasip1/release/my_filter.wasm /etc/veil/wasm/

# config.tomlに設定を追加
```

### 外部サービス連携（HTTP呼び出し）

Proxy-Wasmの`dispatch_http_call`を使用して外部HTTPサービス（Redis用Webdis等）を呼び出せます。
フィルタが呼び出し後に`Action::Pause`を返すと、Veilはリクエスト経路上で上流コールを
インライン解決し（ブロッキングHTTPクライアントは専用オフロードスレッドで実行するため
イベントループはブロックしません）、`proxy_on_http_call_response`で同一WASMインスタンスを
再開します。動作例は`examples/wasm-filters/http-call-filter/`にあります。

```rust
fn on_http_request_headers(&mut self, _: usize, _: bool) -> Action {
    // Webdis経由でRedisにアクセス
    self.dispatch_http_call(
        "webdis",  // upstream名（config.tomlで定義）
        vec![
            (":method", "GET"),
            (":path", "/GET/my_key"),
            (":authority", "webdis"),
        ],
        None,
        vec![],
        Duration::from_millis(50),
    ).unwrap();
    
    Action::Pause  // レスポンス待ち
}

fn on_http_call_response(&mut self, _: u32, _: usize, body_size: usize, _: usize) {
    if let Some(body) = self.get_http_call_response_body(0, body_size) {
        // Redisからの値を処理
        log::info!("Redis response: {:?}", body);
    }
    self.resume_http_request();
}
```
