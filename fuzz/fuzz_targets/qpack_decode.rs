#![no_main]

//! QPACK プレフィックス整数/文字列/ブロック走査のファジング（F-112）。
//! 任意バイト列で panic しないことを検証する。

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    veil::fuzz_api::qpack_decode_smoke(data);
});
