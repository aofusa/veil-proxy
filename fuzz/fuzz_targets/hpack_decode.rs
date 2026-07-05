#![no_main]

use libfuzzer_sys::fuzz_target;
use veil::http2::hpack::HpackDecoder;

fuzz_target!(|data: &[u8]| {
    let mut decoder = HpackDecoder::new(4096);
    let _ = decoder.decode(data);
});
