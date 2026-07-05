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

## 実装済み（追加・2026-07-05）

- **ホスト ABI 境界ファジング**: `src/fuzz_api.rs::wasm_host_abi_map_smoke(bytes)`
  （`#[cfg(feature = "wasm")]`）。WASM ゲスト→ホストへ渡るマップの復元経路
  `wasm::host::abi::deserialize_headers`（B-19 で SDK 互換ワイヤ形式へ移行した経路）に
  任意バイト列を流し、**(1) panic/UB なしで必ず `Some`/`None`**、**(2) 復元成功マップは
  再直列化・再復元でビット同一（ラウンドトリップ冪等）** を検査する。冪等崩れは
  host/guest 間のマップ食い違い＝スマグリングの温床のため不変条件として `assert`。
- fuzz ターゲット `fuzz/fuzz_targets/wasm_host_abi.rs` + `fuzz/Cargo.toml` の
  `[[bin]] wasm_host_abi`（`wasm` passthrough feature）。
- `tools/container_security/fuzz/run_libfuzzer.sh` に WASM 系ターゲット
  （`wasm_abi` / `wasm_host_abi`）を **opt-in**（`RUN_WASM_FUZZ=1`、`--features wasm`）で配線。
- 回帰単体テスト `fuzz_api::tests::wasm_host_abi_map_smoke_handles_arbitrary_input`
  （不正・境界・有効入力を `cargo test --features wasm` で実証）。
- `abi` モジュールは host ABI 復元境界を fuzz から到達させるため `pub(crate)` へ拡大
  （公開 API 面は増やさない）。

## 残件

- `deserialize_headers` 以外の host functions（オフセット/長さでゲストメモリを読む
  `buffers.rs`・`http_call.rs` 等）の引数境界ファジング。wasmtime インスタンスの
  linear memory を跨ぐため、実インスタンス化を伴う構造化ハーネスが必要。
- ASAN 併用実行（[F-71](F-71-asan-corpus-fuzzing.md)）と corpus 蓄積。

## 受け入れ条件

- [x] `cargo fuzz run wasm_abi --features wasm` が短時間走行でクラッシュ無く完走すること。
- [x] `cargo fuzz run wasm_host_abi --features wasm` が同様に完走すること
  （ローカルは `cargo test --features wasm fuzz_api` で不変条件を実証）。
