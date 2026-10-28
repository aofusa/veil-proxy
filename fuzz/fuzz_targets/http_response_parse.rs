#![no_main]

//! 上流の応答のヘッダの解析と chunked の本文の復号のファジング（F-279）。
//! 一括と 1 バイトずつの復号が一致し、厳格な復号器が受け入れた本文の終わりで緩い復号器も終わることを検証する。

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    veil::fuzz_api::http_response_parse_smoke(data);
});
