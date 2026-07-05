//! cargo-fuzz 向けの薄い公開 API（ホットパス外）。

/// HTTP/1 ヘッダー名・値の境界検証（RFC 7230 token / injection 防止）。
#[inline]
pub fn validate_http_header_boundary(name: &[u8], value: &[u8]) -> bool {
    crate::http_utils::is_valid_header_name(name) && crate::http_utils::is_valid_header_value(value)
}

/// WASM モジュールバイト列の検証・コンパイル境界のスモークファジング（ホットパス外）。
///
/// Proxy-Wasm では信頼できない `.wasm` バイト列が wasmtime のバリデータ/コンパイラへ
/// 渡される。任意バイト列でパニックや UB を起こさず、必ず `Ok`/`Err` を返して
/// グレースフルに拒否することを検証するためのエントリポイント。
///
/// 返値はコンパイルが成功したか（`Ok`）。ファザーは戻り値を捨て、
/// クラッシュ（panic / SIGABRT / sanitizer 検知）だけを不具合として扱う。
#[cfg(feature = "wasm")]
pub fn wasm_module_smoke(bytes: &[u8]) -> bool {
    // 本番 registry と同じく信頼境界の外側。default Config でバイト列を
    // 検証・コンパイルのみ行う（インスタンス化はしない）。
    let engine = wasmtime::Engine::default();
    wasmtime::Module::new(&engine, bytes).is_ok()
}
