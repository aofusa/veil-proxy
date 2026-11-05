//! HTTP/2 の要求方向のストリーミング（F-32）
//!
//! F-233 で `src/proxy.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// F-32: HTTP/2 リクエスト方向ストリーミング
// ====================

/// 所有バッファ（`Bytes`）をバックエンドへ全量書き込む（部分書き込みを正しく処理）。
///
/// 共通の `write_all`（io.rs）は単一 `write` 前提で部分書き込み時に誤動作するため、
/// ストリーミング経路では本ヘルパーで `write` をループ呼びする。`Bytes` は `advance` で
/// ゼロコピーに前進できるため、再アロケーション・再コピーは発生しない。
#[cfg(feature = "http2")]
pub(super) async fn backend_write_all_bytes<B>(backend: &mut B, mut buf: Bytes) -> io::Result<()>
where
    B: crate::runtime::io::AsyncWriteRent,
{
    use bytes::Buf;
    while !buf.is_empty() {
        let len = buf.len();
        let (res, returned) = backend.write(buf).await;
        match res {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "backend write returned 0",
                ));
            }
            Ok(n) if n >= len => return Ok(()),
            Ok(n) => {
                let mut b = returned;
                b.advance(n);
                buf = b;
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                buf = returned;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// 1 つの DATA フレーム分のボディを chunked エンコードしてバックエンドへ送る。
///
/// チャンクサイズ行と終端 CRLF のみ小バッファを確保し、**ペイロード本体は受信フレームの
/// 所有バッファ（`Bytes`）をそのまま書き込む（ゼロコピー）**。`writev` 未実装のため 3 回の
/// `write` に分かれるが、ストリーミングバックエンドは Nagle 有効（nodelay 未設定）で
/// カーネルが結合するため実パケット数は抑えられる。
#[cfg(feature = "http2")]
pub(super) async fn send_backend_chunk<B>(backend: &mut B, data: Bytes) -> io::Result<()>
where
    B: crate::runtime::io::AsyncWriteRent,
{
    if data.is_empty() {
        return Ok(());
    }
    let mut header = Vec::with_capacity(18);
    push_chunk_size_line(&mut header, data.len());
    backend_write_all_bytes(backend, Bytes::from(header)).await?;
    backend_write_all_bytes(backend, data).await?;
    backend_write_all_bytes(backend, Bytes::from_static(b"\r\n")).await?;
    Ok(())
}

/// HTTP/1.1 レスポンスバイト列からステータスコードとボディを抽出（管理 API Purge 用）
#[cfg(all(feature = "http2", feature = "admin"))]
pub(super) fn parse_http1_admin_response(resp: &[u8]) -> (u16, Vec<u8>) {
    let status = if resp.starts_with(b"HTTP/1.1 200") {
        200
    } else if resp.starts_with(b"HTTP/1.1 401") {
        401
    } else if resp.starts_with(b"HTTP/1.1 403") {
        403
    } else if resp.starts_with(b"HTTP/1.1 503") {
        503
    } else if resp.starts_with(b"HTTP/1.1 501") {
        501
    } else {
        400
    };
    let body = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| resp[i + 4..].to_vec())
        .unwrap_or_default();
    (status, body)
}

/// HTTP/2 応答ヘッダーへ WASM レスポンスフィルタを適用（B-30）
#[cfg(all(feature = "http2", feature = "wasm"))]
pub(super) async fn apply_h2_wasm_response_headers(
    wasm_modules: &crate::wasm_plugin_config::WasmModules,
    status: u16,
    header_store: Vec<(Bytes, Bytes)>,
) -> Result<Vec<(Bytes, Bytes)>, crate::wasm::LocalResponse> {
    if wasm_modules.is_empty() {
        return Ok(header_store);
    }
    let config = CURRENT_CONFIG.load();
    let Some(ref wasm_engine) = config.wasm_filter_engine else {
        return Ok(header_store);
    };

    // WASM エンジン API（`src/wasm/engine.rs`）は `Vec<(Vec<u8>, Vec<u8>)>` 前提
    // （エンジン内部の型は本タスクの対象ファイル範囲外のため変更しない）。
    // wasm_modules が空でない稀なケースのみこの境界変換コストを払う。
    let vec_headers: Vec<(Vec<u8>, Vec<u8>)> = header_store
        .into_iter()
        .map(|(k, v)| (k.to_vec(), v.to_vec()))
        .collect();

    let wasm_result = wasm_engine
        .clone()
        .on_response_headers_with_modules_async(wasm_modules.clone(), status, vec_headers, true)
        .await;

    match wasm_result {
        crate::wasm::FilterResult::Continue {
            headers: modified_headers,
            ..
        } => Ok(modified_headers
            .into_iter()
            .map(|(k, v)| (Bytes::from(k), Bytes::from(v)))
            .collect()),
        // 応答ヘッダの段のローカルレスポンス（B-300。呼び出し元が元の応答の代わりに送る）
        crate::wasm::FilterResult::LocalResponse(resp) => Err(resp),
    }
}

/// gRPC トレイラー（`grpc-status`/`grpc-message` 等）へ WASM レスポンスフィルタを適用（F-133）。
///
/// `wasm_modules` が空なら早期 return（追加コストなし）。トレイラー送出時点で
/// HEADERS/DATA は既に送出済みのため、`LocalResponse`/`Pause` は適用不能として
/// 警告ログのみ出し、トレイラーはそのまま（あるいは `Continue` の変更のみ反映して）通す。
#[cfg(all(feature = "http2", feature = "grpc", feature = "wasm"))]
pub(super) async fn apply_h2_wasm_response_trailers(
    wasm_modules: &crate::wasm_plugin_config::WasmModules,
    trailers: Vec<(Bytes, Bytes)>,
) -> Vec<(Bytes, Bytes)> {
    if wasm_modules.is_empty() {
        return trailers;
    }
    let config = CURRENT_CONFIG.load();
    let Some(ref wasm_engine) = config.wasm_filter_engine else {
        return trailers;
    };

    // WASM エンジン API は `Vec<(Vec<u8>, Vec<u8>)>` 前提（境界変換、上記と同様）。
    let vec_trailers: Vec<(Vec<u8>, Vec<u8>)> = trailers
        .iter()
        .map(|(k, v)| (k.to_vec(), v.to_vec()))
        .collect();

    match wasm_engine
        .on_response_trailers_with_modules(wasm_modules, vec_trailers)
        .await
    {
        crate::wasm::FilterResult::Continue { headers, .. } => headers
            .into_iter()
            .map(|(k, v)| (Bytes::from(k), Bytes::from(v)))
            .collect(),
        crate::wasm::FilterResult::LocalResponse(_) => {
            // gRPC トレイラー送出時点で HEADERS/DATA は送出済みのためローカルレスポンスは
            // 適用できない（既存の送出済みレスポンスを差し替えられない）。元のトレイラーを
            // そのまま通す（grpc-status 欠落によるクライアントハングを避ける）。
            crate::logging::log_throttled!(
                warn,
                "WASM module requested a local response on gRPC response trailers, \
                 but the response head is already sent; keeping the original trailers"
            );
            trailers
        }
    }
}

/// HTTP/2 静的応答（`type = "File"`・`mode = "memory"`）のステータス・ヘッダ・本文（B-32・B-266・F-286・F-287）。
/// 共通の `static_response::build_bytes_response` に `server`・`alt-svc` を足す。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
pub(super) fn build_h2_static_response(
    sreq: &crate::static_response::StaticRequest<'_>,
    data: Bytes,
    mime_type: &str,
    validators: &crate::static_response::Validators,
    security: &SecurityConfig,
    compression: &CompressionConfig,
    static_ctx: Option<(&Path, &cache::StaticContentCacheConfig)>,
    memory: Option<&crate::static_response::MemoryContent>,
) -> (u16, Vec<(Bytes, Bytes)>, Bytes) {
    let (status, mut header_store, body) = crate::static_response::build_bytes_response(
        sreq,
        data,
        mime_type,
        validators,
        security,
        compression,
        static_ctx,
        memory,
    );
    if let Some(ref g) = get_server_header_guard() {
        if !security.removes_response_header(b"server") {
            header_store.push((Bytes::from_static(b"server"), g.value_bytes()));
        }
    }
    // F-94: HTTP/3 広告（Alt-Svc）
    if let Some(g) = get_alt_svc_guard() {
        if !security.removes_response_header(b"alt-svc") {
            header_store.push((Bytes::from_static(b"alt-svc"), g.value_bytes()));
        }
    }
    (status, header_store, body)
}

/// HTTP/2 用レスポンスボディ圧縮（`static_response::compress_body` と同じ。失敗したら元のまま）
#[cfg(all(feature = "http2", feature = "compression"))]
#[inline]
pub(super) fn compress_body_h2(
    body: &[u8],
    encoding: AcceptedEncoding,
    compression: &CompressionConfig,
) -> Vec<u8> {
    crate::static_response::compress_body(body, encoding, compression)
}

/// compression feature 無効時のスタブ
#[cfg(all(feature = "http2", not(feature = "compression")))]
#[inline]
pub(super) fn compress_body_h2(
    body: &[u8],
    _encoding: AcceptedEncoding,
    _compression: &CompressionConfig,
) -> Vec<u8> {
    body.to_vec()
}

#[cfg(all(test, feature = "http2"))]
mod streaming_tests {
    use super::*;

    /// chunked チャンクサイズ行のエンコードが正しいこと（F-32 リクエストストリーミング）。
    #[test]
    fn test_push_chunk_size_line() {
        fn line(n: usize) -> Vec<u8> {
            let mut b = Vec::new();
            push_chunk_size_line(&mut b, n);
            b
        }
        assert_eq!(line(0), b"0\r\n");
        assert_eq!(line(1), b"1\r\n");
        assert_eq!(line(15), b"f\r\n");
        assert_eq!(line(16), b"10\r\n");
        assert_eq!(line(255), b"ff\r\n");
        assert_eq!(line(256), b"100\r\n");
        assert_eq!(line(65535), b"ffff\r\n");
        assert_eq!(line(16384), b"4000\r\n");
        // 既存バッファへの追記であること（前の内容を保持）
        let mut b = b"prefix".to_vec();
        push_chunk_size_line(&mut b, 16384);
        assert_eq!(b, b"prefix4000\r\n");
    }

    /// チャンクサイズ行が標準ライブラリの 16 進表現と一致すること（網羅確認）。
    #[test]
    fn test_push_chunk_size_line_matches_hex() {
        for n in [
            0usize, 1, 7, 9, 10, 16, 100, 1000, 4096, 16384, 65535, 65536, 1_048_576, 200_000,
        ] {
            let mut b = Vec::new();
            push_chunk_size_line(&mut b, n);
            let expected = format!("{:x}\r\n", n).into_bytes();
            assert_eq!(b, expected, "mismatch for n={}", n);
        }
    }
}
