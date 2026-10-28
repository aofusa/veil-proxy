#![no_main]

//! HTTP/3 のリクエストのフィールドセクションの検査のファジング（F-279）。
//! 受け入れた要求は `:method` を持ち、全フィールドの値が正しいことを検証する。

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    veil::fuzz_api::h3_field_section_smoke(data);
});
