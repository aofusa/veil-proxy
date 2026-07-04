#![no_main]

//! WASM モジュールバイト列（信頼境界外）の検証・コンパイル境界ファジング。
//! `wasm` feature 有効時のみ実体が動く（`cargo fuzz run wasm_abi --features wasm`）。
//! 無効時は no-op としてビルドのみ通す（既定 fuzz ビルドに wasmtime を強制しない）。

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    #[cfg(feature = "wasm")]
    {
        let _ = veil::fuzz_api::wasm_module_smoke(data);
    }
    #[cfg(not(feature = "wasm"))]
    {
        let _ = data;
    }
});
