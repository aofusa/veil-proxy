#![no_main]

use libfuzzer_sys::fuzz_target;
use veil::http2::frame::{FrameDecoder, FrameHeader};

fuzz_target!(|data: &[u8]| {
    if data.len() < FrameHeader::SIZE {
        return;
    }

    let decoder = FrameDecoder::new(1 << 14);
    if let Ok(header) = decoder.decode_header(data) {
        let end = FrameHeader::SIZE.saturating_add(header.length as usize);
        if end <= data.len() {
            let payload = &data[FrameHeader::SIZE..end];
            let _ = decoder.decode(&header, payload);
        }
    }
});
