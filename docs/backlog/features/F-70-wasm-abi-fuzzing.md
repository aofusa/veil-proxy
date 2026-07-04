# F-70: WASM モジュール/ABI 境界ファジング

出典: `security_chaos_fuzzing_report.md` §2.1.3。親: [F-52](F-52-cargo-fuzz-libfuzzer.md)。

## 目的

Proxy-Wasm では信頼できない `.wasm` バイト列が wasmtime のバリデータ/コンパイラへ渡される。
任意バイト列でパニック・UB を起こさず必ずグレースフルに拒否（`Ok`/`Err`）することを検証する。

## 実装済み

- `src/fuzz_api.rs::wasm_module_smoke(bytes)`（`#[cfg(feature = "wasm")]`）。
  既定 Config の wasmtime `Engine` でバイト列を検証・コンパイルのみ実施（インスタンス化なし）。
- fuzz ターゲット `fuzz/fuzz_targets/wasm_abi.rs` + `fuzz/Cargo.toml` の `[[bin]] wasm_abi`。
  既定 fuzz ビルドに wasmtime を強制しないよう passthrough feature 化
  （`[features] wasm = ["veil/wasm"]`、実行は `cargo fuzz run wasm_abi --features wasm`）。
- `cargo check --lib --features wasm` 通過を確認済み。

## 残件

- **ホスト ABI 境界のファジング**: WASM 側からホスト（Veil）へコールバックされる
  host functions（`src/wasm/host/`）の引数境界（オフセット/長さ/文字列）に対する
  構造化ファジング。現状はモジュールバイト列（コンパイル境界）のみ。
- ASAN 併用実行（[F-71](F-71-asan-corpus-fuzzing.md)）と corpus 蓄積。

## 受け入れ条件

- `cargo fuzz run wasm_abi --features wasm` が短時間走行でクラッシュ無く完走すること。
