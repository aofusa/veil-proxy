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

/// Proxy-Wasm ホスト ABI 境界（マップ直列化）のファジング（ホットパス外）。
///
/// WASM ゲスト側がホストへ渡すマップ（ヘッダ等）は `deserialize_headers` で
/// 復元される。B-19 で SDK 互換のワイヤ形式へ移行した経路であり、信頼できない
/// バイト列（オフセット/長さ/NUL 位置）が直接この関数へ到達する。任意入力で
/// パニック・UB を起こさず必ず `Some`/`None` を返し、さらに **復元に成功した
/// マップは再直列化・再復元でビット同一（ラウンドトリップ冪等）** であることを
/// 検証する。冪等性が崩れると host/guest 間でマップ内容が食い違い、
/// リクエストスマグリング等の温床になり得るため不変条件として検査する。
///
/// ファザーは戻り値を捨て、クラッシュ（panic / sanitizer 検知 / assert 失敗）
/// だけを不具合として扱う。
#[cfg(feature = "wasm")]
pub fn wasm_host_abi_map_smoke(bytes: &[u8]) {
    use crate::wasm::host::abi::{deserialize_headers, serialize_headers};

    let Some(map) = deserialize_headers(bytes) else {
        return;
    };
    // 復元に成功したマップは正規形へ直列化でき、再復元で同一に戻ること（冪等）。
    let reserialized = serialize_headers(&map);
    let reparsed =
        deserialize_headers(&reserialized).expect("re-serialized canonical map must deserialize");
    assert_eq!(
        map, reparsed,
        "host ABI map roundtrip must be idempotent (guest/host consistency)"
    );
}

#[cfg(all(test, feature = "wasm"))]
mod tests {
    use super::*;

    /// ホスト ABI マップ smoke: 有効・不正・境界入力のいずれでも panic せず、
    /// 有効入力ではラウンドトリップ冪等（assert）が成立すること。
    #[test]
    fn wasm_host_abi_map_smoke_handles_arbitrary_input() {
        // 空・切り詰め・過大 num_pairs 等の不正入力（None 経路、panic しない）。
        for bad in [
            &b""[..],
            &b"\x01"[..],
            &b"\xff\xff\xff\xff"[..],
            &[2, 0, 0, 0, 0, 0, 0, 0][..],
        ] {
            wasm_host_abi_map_smoke(bad);
        }

        // 有効なマップを正規形で構築し、冪等 assert 経路を通す。
        use crate::wasm::host::abi::serialize_headers;
        let valid = serialize_headers(&[
            (b":method".to_vec(), b"GET".to_vec()),
            (b"x-empty".to_vec(), b"".to_vec()),
        ]);
        wasm_host_abi_map_smoke(&valid);
    }
}
