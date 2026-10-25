#![no_main]

//! multipart/form-data の解析器のファジング（F-266）。任意のバイト列で panic せず、溜めた本文と
//! 2 つに分けて流した本文で同じパートになること。`cargo fuzz run multipart_parse --features api`

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    veil::fuzz_api::multipart_parse_smoke(data);
});
