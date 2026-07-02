#![no_main]

use libfuzzer_sys::fuzz_target;
use std::io::Write;
use tempfile::NamedTempFile;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(mut tmp) = NamedTempFile::new() {
            if tmp.write_all(text.as_bytes()).is_ok() {
                let _ = veil::config::test_config_file(tmp.path());
            }
        }
    }
});