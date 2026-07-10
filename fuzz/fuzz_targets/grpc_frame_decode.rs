#![no_main]

//! gRPC Length-Prefixed Message デコーダのファジング（F-94）。
//! 任意バイト列で panic せず Ok/Err を返すことを検証する。

use libfuzzer_sys::fuzz_target;
use veil::grpc::framing::{decode_grpc_frame, decode_grpc_frame_with_max_size, GrpcFrameDecoder};

fuzz_target!(|data: &[u8]| {
    // 単発デコード（デフォルト上限）
    let _ = decode_grpc_frame(data);

    // 小さめ上限で MessageTooLarge 経路を刺激
    let _ = decode_grpc_frame_with_max_size(data, 64);

    // ストリーミングデコーダ: 1 バイトずつ / 半分ずつ
    let mut dec = GrpcFrameDecoder::with_max_size(1024 * 1024);
    if data.is_empty() {
        let _ = dec.decode_next();
        return;
    }
    // 1 バイト分割
    for b in data.iter().take(4096) {
        dec.push(std::slice::from_ref(b));
        match dec.decode_next() {
            Ok(Some(_)) | Ok(None) | Err(_) => {}
        }
    }
    // 残りをまとめて push
    if data.len() > 4096 {
        dec.push(&data[4096..]);
        while matches!(dec.decode_next(), Ok(Some(_))) {}
    }
});
