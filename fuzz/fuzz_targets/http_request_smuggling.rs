#![no_main]
//! HTTP リクエストスマグリング分類（B-23 デシンク防御）のファジングターゲット。
//!
//! 任意のヘッダーブロックを分類器に通し、panic せず、かつ CL+TE 組み合わせが
//! 必ず拒否される（反デシンク不変条件）ことを検証する。詳細は
//! `veil::fuzz_api::http_request_smuggling_smoke` を参照。

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    veil::fuzz_api::http_request_smuggling_smoke(data);
});
