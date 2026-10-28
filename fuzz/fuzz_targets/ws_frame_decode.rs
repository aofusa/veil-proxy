#![no_main]

//! WebSocket のフレームの復号器のファジング（F-262）。任意のバイト列で panic しないこと。
//! `cargo fuzz run ws_frame_decode --features api`

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    veil::fuzz_api::ws_frame_decode_smoke(data);
});
