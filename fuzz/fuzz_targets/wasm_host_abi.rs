#![no_main]

//! Proxy-Wasm ホスト ABI 境界（マップ直列化）のファジング。
//! WASM ゲスト→ホストへ渡るマップの復元経路（`deserialize_headers`）に任意バイト列を
//! 流し、panic/UB なし + ラウンドトリップ冪等を検証する（F-70 ホスト ABI 境界）。
//! `wasm` feature 有効時のみ実体が動く（`cargo fuzz run wasm_host_abi --features wasm`）。
//! 無効時は no-op としてビルドのみ通す。

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    #[cfg(feature = "wasm")]
    {
        veil::fuzz_api::wasm_host_abi_map_smoke(data);
    }
    #[cfg(not(feature = "wasm"))]
    {
        let _ = data;
    }
});
