//! cargo-fuzz 向けの薄い公開 API（ホットパス外）。

/// HTTP/1 ヘッダー名・値の境界検証（RFC 7230 token / injection 防止）。
#[inline]
pub fn validate_http_header_boundary(name: &[u8], value: &[u8]) -> bool {
    crate::http_utils::is_valid_header_name(name) && crate::http_utils::is_valid_header_value(value)
}

/// HTTP リクエストスマグリング分類（B-23 デシンク防御）のファジング（ホットパス外）。
///
/// `classify_request_framing` はフロントエンド／バックエンドの本文長解釈を一致させ、
/// CL.TE / TE.CL デシンクを塞ぐ **信頼境界の関門**である。外部から HTTP ヘッダーで
/// 任意の Content-Length / Transfer-Encoding 組み合わせが到達し得る。
///
/// 本エントリは `data` を改行区切りのヘッダーブロックとみなして `(name, value)` へ分解し、
/// 分類器に通す。任意入力で **panic せず**、かつ **反スマグリング不変条件**（Content-Length と
/// Transfer-Encoding が同時に存在するなら必ず拒否 = `Err`。`Ok` になればデシンクの温床）を
/// 満たすことを検証する。ファザーは戻り値を捨て、クラッシュ / assert 失敗だけを不具合として扱う。
pub fn http_request_smuggling_smoke(data: &[u8]) {
    use crate::http_utils::classify_request_framing;

    let mut headers: Vec<(&[u8], &[u8])> = Vec::new();
    let mut saw_content_length = false;
    let mut saw_transfer_encoding = false;

    for line in data.split(|&b| b == b'\n') {
        // CR を落とし、空行はスキップ（ヘッダー終端相当）。
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let Some(idx) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let name = line[..idx].trim_ascii();
        let value = line[idx + 1..].trim_ascii();
        if name.eq_ignore_ascii_case(b"content-length") {
            saw_content_length = true;
        } else if name.eq_ignore_ascii_case(b"transfer-encoding") {
            saw_transfer_encoding = true;
        }
        headers.push((name, value));
    }

    let result = classify_request_framing(headers.iter().copied());

    // 反スマグリング不変条件: CL と TE が両方存在するなら必ず拒否されねばならない。
    if saw_content_length && saw_transfer_encoding {
        assert!(
            result.is_err(),
            "CL+TE combination must be rejected (anti-desync invariant); headers={headers:?}"
        );
    }
    // それ以外は Ok/Err どちらでも良い（panic しないことが主目的）。
    let _ = result;
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

/// io_uring executor への擬似 CQE 注入ファジング（F-84、ホットパス外）。
///
/// コアランタイム（`src/runtime/uring/executor.rs`）の op テーブル・完了ディスパッチ経路を、
/// 実リング（カーネル）を介さずに Fuzzer 生成の操作列で駆動する。異常な `res` 値、
/// `user_data` の偽造・stale 世代、完了順序の逆転・重複到着に対して panic せず、
/// detach ガードの exactly-once 実行（B-07 の UAF/リーク対策の意味論）と
/// スロット非リークを不変条件として検査する。詳細は
/// `runtime::executor::fuzz_op_table_sequence` の doc コメントを参照。
///
/// io_uring 固有の op テーブル実装が対象のため、reactor（`--features epoll`）ビルドでは
/// 該当する完了ディスパッチ経路自体が存在しない（readiness モデルのため op テーブル・
/// detach ガードの概念が構造的に無い。`src/runtime/reactor/mod.rs` の doc 参照）。
/// そのビルドでは no-op にする（fuzz クレートは既定で uring バックエンドを使用するため
/// 通常はこの分岐へ到達しない）。
#[cfg(veil_rt_uring)]
pub fn io_uring_executor_smoke(data: &[u8]) {
    crate::runtime::executor::fuzz_op_table_sequence(data);
}

/// reactor ビルド向け no-op（上記 doc 参照）。
#[cfg(veil_rt_reactor)]
pub fn io_uring_executor_smoke(_data: &[u8]) {}

/// HTTP/3 フレームワイヤ形式のファジング（F-112、ホットパス外）。
///
/// 本番経路は quiche 内部で H3 フレームを解釈する。本エントリは RFC 9114 の
/// type/length varint レイアウトを純関数で走査し、任意バイト列で panic しないことを検証する。
pub fn http3_frame_decode_smoke(data: &[u8]) {
    crate::http3_wire::http3_frame_decode_smoke(data);
}

/// QPACK 整数/文字列プレフィックスのファジング（F-112、ホットパス外）。
///
/// 本番経路は quiche の QPACK に委譲。本エントリは RFC 9204 のプレフィックス整数・
/// 文字列リテラル・命令先頭の走査を純関数で行い、任意入力で panic しないことを検証する。
pub fn qpack_decode_smoke(data: &[u8]) {
    crate::http3_wire::qpack_decode_smoke(data);
}

/// ネイティブ gRPC Content-Type 判定のファジング（F-112、ホットパス外）。
/// `application/grpc` と `application/grpc-web*` の境界を任意入力で刺激する。
pub fn native_grpc_content_type_smoke(data: &[u8]) {
    let _ = crate::proxy::is_native_grpc_content_type(data);
}

#[cfg(test)]
mod io_uring_executor_tests {
    use super::*;

    /// 代表的な操作列（正常完了・detach・偽造 CQE・境界）で不変条件が成立すること。
    #[test]
    fn io_uring_executor_smoke_handles_representative_sequences() {
        // 空・単一 alloc・切り詰め入力。
        io_uring_executor_smoke(b"");
        io_uring_executor_smoke(&[0]);
        io_uring_executor_smoke(&[1]); // id 指定バイト不足で途中終了

        // alloc → CQE(res=-1) → take の正常完了経路。
        io_uring_executor_smoke(&[0, 1, 0, 0xFF, 0xFF, 0xFF, 0xFF, 4, 0]);

        // alloc → detach(pending) → （フラッシュで遅延 CQE が届きガードが 1 回走る）。
        io_uring_executor_smoke(&[0, 3, 0]);

        // alloc → CQE → detach(done: ガード即時実行) 。
        io_uring_executor_smoke(&[0, 1, 0, 42, 0, 0, 0, 3, 0]);

        // 偽造 user_data への CQE（センチネル u64::MAX・無効 0・ランダム）。
        io_uring_executor_smoke(&[
            0, // alloc
            2, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // u64::MAX
            2, 0, 0, 0, 0, 0, 0, 0, 0, // 無効 ID 0
            2, 0x37, 0x13, 0, 0, 0x01, 0, 0, 0, // 適当な世代不一致
        ]);
    }

    /// 決定的な擬似乱数列による長い操作列でも不変条件が成立すること（ミニファザー）。
    #[test]
    fn io_uring_executor_smoke_survives_pseudo_random_sequences() {
        // 単純な LCG で 256 本の 96 バイト列を決定的に生成する。
        let mut state = 0x243F_6A88_85A3_08D3u64;
        for _ in 0..256 {
            let mut buf = [0u8; 96];
            for b in buf.iter_mut() {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                *b = (state >> 33) as u8;
            }
            io_uring_executor_smoke(&buf);
        }
    }
}

#[cfg(test)]
mod smuggling_tests {
    use super::*;

    /// スマグリング smoke: 任意入力で panic せず、CL+TE は必ず拒否される（反デシンク不変条件）。
    #[test]
    fn http_request_smuggling_smoke_handles_arbitrary_input() {
        // 無害・不正・境界入力（panic しないこと）。
        for bad in [
            &b""[..],
            &b"\n\n\n"[..],
            &b"Content-Length"[..], // コロンなし
            &b":\r\n"[..],
            &b"Transfer-Encoding: chunked\r\n"[..],
            &b"Content-Length: 10\r\n"[..],
            &b"garbage \xff\x00 bytes: value"[..],
        ] {
            http_request_smuggling_smoke(bad);
        }

        // CL+TE の各種組み合わせ（`Content-Length: 0` + chunked = B-23 の取りこぼしを含む）は
        // すべて assert（Err 要求）を通過すること。もし分類器が Ok を返せば panic する。
        for desync in [
            &b"Content-Length: 0\r\nTransfer-Encoding: chunked\r\n"[..],
            &b"content-length: 5\r\ntransfer-encoding: chunked\r\n"[..],
            &b"Transfer-Encoding: chunked\r\nContent-Length: 42\r\n"[..],
        ] {
            http_request_smuggling_smoke(desync);
        }
    }
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
