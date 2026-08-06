# F-135: `[wasm] interpreter` オプション（Pulley インタープリタの任意選択）

## 目的

WASM 実行を Cranelift ネイティブ JIT 以外の方式でも選べるようにし、W^X 制約のある環境・実行可能 `mmap` を許可できない環境でも `wasm` feature を利用可能にする。OpenBSD では B-52 対応として wasmtime の Pulley インタープリタを常時強制しているが、これを他プラットフォームでも任意に選択できる設定として一般化する。

## 概要・改修内容

1. **設定追加**: `src/wasm/types.rs` の `WasmConfig` に `#[serde(default)] pub interpreter: bool`（既定 `false`）を追加。
2. **エンジン生成**: `src/wasm/registry.rs::create_engine` を `&WasmConfig` を受け取る形に変更し、`cfg!(target_os = "openbsd") || config.interpreter` が真のとき `Config::target("pulley64"/"pulley32")`（ポインタ幅で選択）を指定する。OpenBSD は設定値に関わらず常に Pulley（B-52、既存挙動を維持）。
3. **起動時警告**: OpenBSD で `interpreter = false` が明示された場合、`WasmConfig::validate()` で `ftlog::warn!` を1回出し、設定は無視して Pulley を使い続ける。
4. **AOT キャッシュ分離**: Pulley とネイティブ JIT の `.cwasm` はバイナリ非互換のため、Pulley 使用時はサイドカーキャッシュ名を `.pulley.cwasm` に分離（`load_or_compile_with_cache` に `suffix` 引数を追加、`ModuleRegistry` が生成時に `pulley: bool` を保持）。
5. **依存関係**: base の `[dependencies] wasmtime` の features に `"pulley"` を追加（従来は OpenBSD 向け target 別依存にのみ存在）。
6. **ドキュメント**: `examples/config.toml` / `contrib/config/config.toml` の `[wasm]` セクション、`README.md` / `docs/readme/README.ja.md` の WASM 拡張節に `interpreter` の説明を追記。

## テスト

- `src/wasm/tests.rs::interpreter_tests`:
  - `interpreter = true` で `ModuleRegistry::new` が成功し、`tests/fixtures/wasm/header_filter.wasm` をロードできる。
  - `interpreter = true` / `false` で同じモジュールを実行し、同じフィルタ結果（アクション・ヘッダ変更）になる（Pulley とネイティブ JIT の実行結果等価性）。

## 受け入れ条件

- [x] `docs/backlog/backlog.md` に F-135 チケットを追加。
- [x] `interpreter` 既定 `false` でデフォルト構成の挙動が変わらない。
- [x] OpenBSD の既存 Pulley 強制挙動（B-52）を壊さない。
- [x] `cargo build --features full` / `cargo test --lib --features full wasm` / `cargo clippy --features full --all-targets -- -D warnings` / `cargo build --no-default-features` が通る。
