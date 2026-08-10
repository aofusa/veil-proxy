# F-148 Proxy-Wasm プラグイン設定の TOML 記述とルート単位の上書き

- ステータス: **完了**
- 優先度: 中
- 関連: F-43（モジュールリストの Arc 共有）、B-64（`load_backend` はリクエストごとに呼ばれる）

## 背景・課題

Proxy-Wasm モジュールへ渡す plugin configuration は、これまで
`[[wasm.modules]]` の `configuration` に **JSON 文字列を 1 本だけ** 書く方式しか無かった。

```toml
[[wasm.modules]]
name = "header_filter"
path = "assets/wasm/header_filter.wasm"
configuration = '{"header_name": "x-veil", "header_value": "1"}'
```

この方式には 2 つの不便がある。

1. **設定ファイルが TOML なのに、モジュール設定だけ JSON 文字列**になり、
   エディタの補完も構文チェックも効かず、クォートのエスケープが必要になる。
2. **同じモジュールをルートごとに違うパラメータで使えない**。
   モジュールの設定はモジュール定義に 1 つだけ紐づくため、
   「`/api` では厳しめの WAF、`/static` では緩め」といった使い分けができず、
   同じ `.wasm` を別名で複数回ロードする（メモリ・起動時間の二重消費）しかなかった。

## 改修内容

### 1. `configuration` が TOML テーブルも受け付ける（後方互換）

`configuration` の型を「文字列 **または** TOML テーブル」の
untagged enum（`PluginConfiguration`）にする。

```toml
# 従来どおり JSON 文字列（そのままバイト列としてモジュールへ渡る）
[[wasm.modules]]
name = "header_filter"
path = "assets/wasm/header_filter.wasm"
configuration = '{"header_name": "x-veil"}'

# TOML テーブル（JSON オブジェクトへ変換してからモジュールへ渡る）
[[wasm.modules]]
name = "waf_filter"
path = "assets/wasm/waf_filter.wasm"
[wasm.modules.configuration]
mode = "block"
max_body_size = 65536
patterns = ["union select", "<script"]
```

Proxy-Wasm の ABI 上、plugin configuration は**単なるバイト列**であり、
実際には JSON を期待するモジュールが大多数であるため、
TOML テーブルは **JSON オブジェクトへシリアライズしてから**渡す
（モジュール側の実装は一切変更不要）。

TOML → JSON の変換規則:

| TOML | JSON |
|------|------|
| String | string |
| Integer | number |
| Float | number（`NaN`/`Inf` は `null`） |
| Boolean | bool |
| Datetime | RFC 3339 相当の string |
| Array | array |
| Table | object（キー順は TOML の出現順を保持） |

### 2. ルート単位の設定上書き

`[[route]]` に `module_configuration`（モジュール名 → 設定値のマップ）を追加する。
値の型はモジュール定義側と同じ `PluginConfiguration`。

```toml
[[route]]
modules = ["header_filter", "waf_filter"]

[route.module_configuration.waf_filter]
mode = "log_only"          # モジュール定義の "block" を上書き

[route.conditions]
path = "/static/*"
[route.action]
type = "File"
path = "./www"
```

**合成規則（ルート優先）**

| モジュール定義 | ルート | 実効設定 |
|---|---|---|
| なし | なし | 空バイト列 |
| あり | なし | モジュール定義の値 |
| なし | あり | ルートの値 |
| Table | Table | **ディープマージ**（同名キーはルート優先、ネストしたテーブルも再帰マージ、配列は置換） |
| 上記以外の組み合わせ（片方でも文字列） | あり | **ルートの値で完全置換**（文字列は不透明なバイト列でありマージ不能なため） |

L4（`[[l4]]`）リスナーの `wasm_modules` にも同じ
`module_configuration` を用意し、HTTP ルートと挙動を揃える。

### 3. 実効設定はホットパスで作らない

`config::load_backend` は **リクエストごとに呼ばれる**（B-64）。
そのため合成は設定ロード時に 1 回だけ行い、結果を
`Route::resolved_modules: Option<Arc<Vec<ModuleRef>>>` に持たせる。
`load_backend` は `Arc` を clone するだけになり、
**従来あった `Arc::new(modules.clone())`（リクエストごとの `Vec<String>` ディープコピー）が消える**
＝ 本改修はホットパスのアロケーションを 1 つ減らす。

さらに `LoadedModule::configuration` と `HttpContext::plugin_configuration` /
`vm_configuration` を `Vec<u8>` から `Arc<[u8]>` へ変更し、
**WASM 実行のたびに発生していた設定バイト列のディープコピーを Arc の参照カウント増加に置き換える**。
空設定は `once_cell::Lazy` の共有インスタンスを clone するためアロケーションしない。

### 4. 依存追加なし

TOML → JSON 変換は `src/wasm_plugin_config.rs` に約 80 行の
専用エンコーダとして実装する（`serde_json` を直接依存に追加しない）。
コールドパス（起動・リロード時）専用。

## 影響範囲

- `src/wasm_plugin_config.rs`（新規・feature 非依存で常にコンパイル）
- `src/config.rs`（`Route.module_configuration` / `Route.resolved_modules`、
  `Backend::modules_arc()` の型、`L4Config.module_configuration`、参照整合性チェック）
- `src/wasm/types.rs`（`ModuleConfig.configuration` の型）
- `src/wasm/registry.rs`（`LoadedModule.configuration: Arc<[u8]>`）
- `src/wasm/engine.rs`（`&[String]` → `&[ModuleRef]`、実効設定の解決）
- `src/wasm/context.rs` / `src/wasm/host/buffers.rs`
- `src/proxy.rs` / `src/http3_server.rs` / `src/l4.rs`（型の追従）
- `examples/config.toml` / `contrib/config/config.toml` / README / README.ja

## 検証結果

| 対象 | 結果 |
|---|---|
| 単体（`--lib --features full`） | 841 passed / 0 failed |
| 統合（`--bins --test integration_tests`） | 53 passed / 0 failed |
| 新規統合（`--test f148_wasm_plugin_config`） | 3 passed / 0 failed |
| Proxy-Wasm 適合（`--test proxy_wasm_conformance`） | 14 passed / 0 failed |
| プロパティ（`--test config_proptest`） | 4 passed / 0 failed |
| E2E Linux x86_64 io_uring（既定） | 542 passed / 0 failed |
| E2E Linux x86_64 reactor（`full,epoll`） | 542 passed / 0 failed |
| E2E FreeBSD 14.3 aarch64（`full-freebsd`） | 541 passed / 1 failed（**B-61** = 既知・本変更と無関係。単体実行では 31 秒で pass） |
| E2E OpenBSD 7.9 aarch64（`full-openbsd`） | 541 passed / 0 failed |
| E2E NetBSD 10.1 aarch64（`full-netbsd`） | **B-62**（既知）によりスイート完走不可。ビルドは成功 |
| feature 組み合わせビルド 16 種 + clippy(full/no-default, all-targets) + fmt | 警告・エラーゼロ |

feature 組み合わせは default / `--no-default-features` / `full` / `full-container` /
`wasm` / `http2` / `http3` / `l4-proxy` / `grpc-full` / `admin` / `access-log` / `metrics` /
`ktls` / `mimalloc` / `opentelemetry` / `full,epoll` の 16 種。
実装中に検出した 2 件の feature 組み合わせ警告
（`wasm` のみ有効時の未使用 `resolve_l4_wasm_modules`、`l4-proxy` のみ有効時の `unused_mut`）は
`#[allow]` を使わず cfg ゲートとスコープ調整で解消した。

## 後方互換性

既存の `configuration = '<JSON 文字列>'` は **untagged enum の先頭バリアント**として
そのまま受理され、バイト列も従来と 1 バイトも変わらない。
`module_configuration` を書かなければ挙動は完全に従来どおり。
