#![no_main]

//! HTTP/2 の接続状態機械（SETTINGS の値・ストリームの列・CONTINUATION）のファジング（F-279）。
//! フレームを処理するたびに HPACK の動的表・同時ストリーム数・溜めるヘッダブロックが上限内であることを検証する。

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    veil::fuzz_api::http2_connection_state_smoke(data);
});
