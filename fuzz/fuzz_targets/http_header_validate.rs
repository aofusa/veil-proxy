#![no_main]

use libfuzzer_sys::fuzz_target;
use veil::fuzz_api;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    let split = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    let (name, value) = data.split_at(split);
    let _ = fuzz_api::validate_http_header_boundary(name, value);
});