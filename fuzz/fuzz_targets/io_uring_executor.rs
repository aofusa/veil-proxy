//! io_uring executor 擬似 CQE 注入ファジング（F-84）。
//!
//! Fuzzer 生成のバイト列を「op テーブル操作列 + 擬似 CQE」として解釈し、実カーネルを
//! 介さずにランタイムの完了ディスパッチ経路を駆動する。異常 res・偽造/stale user_data・
//! 完了順序逆転で panic / ガード二重実行 / スロットリークが起きないことを検査する。
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    veil::fuzz_api::io_uring_executor_smoke(data);
});
