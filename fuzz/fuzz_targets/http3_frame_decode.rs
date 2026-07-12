#![no_main]

//! HTTP/3 フレームワイヤ形式のファジング（F-112）。
//! 任意バイト列で panic せず Ok/Err / 走査打ち切りを返すことを検証する。

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    veil::fuzz_api::http3_frame_decode_smoke(data);
});
