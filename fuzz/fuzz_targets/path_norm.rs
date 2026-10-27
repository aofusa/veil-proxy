#![no_main]

//! パスの正規化のファジング（F-279）。
//! 書き換えた結果が `/` で始まり、クエリを保ち、正規化を有効にしたときドットのセグメントが残らないことを検証する。

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    veil::fuzz_api::path_norm_smoke(data);
});
