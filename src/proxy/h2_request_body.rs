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
    wasm_modules: &Arc<Vec<crate::wasm_plugin_config::ModuleRef>>,
    status: u16,
    header_store: Vec<(Bytes, Bytes)>,
) -> Vec<(Bytes, Bytes)> {
    if wasm_modules.is_empty() {
        return header_store;
    }
    let config = CURRENT_CONFIG.load();
    let Some(ref wasm_engine) = config.wasm_filter_engine else {
        return header_store;
    };

    // WASM エンジン API（`src/wasm/engine.rs`）は `Vec<(Vec<u8>, Vec<u8>)>` 前提
    // （エンジン内部の型は本タスクの対象ファイル範囲外のため変更しない）。
    // wasm_modules が空でない稀なケースのみこの境界変換コストを払う。
    let vec_headers: Vec<(Vec<u8>, Vec<u8>)> = header_store
        .iter()
        .map(|(k, v)| (k.to_vec(), v.to_vec()))
        .collect();

    let wasm_result = wasm_engine
        .clone()
        .on_response_headers_with_modules_async(wasm_modules.clone(), status, vec_headers, true)
        .await;

    if let crate::wasm::FilterResult::Continue {
        headers: modified_headers,
        ..
    } = wasm_result
    {
        modified_headers
            .into_iter()
            .map(|(k, v)| (Bytes::from(k), Bytes::from(v)))
            .collect()
    } else {
        header_store
    }
}

/// gRPC トレイラー（`grpc-status`/`grpc-message` 等）へ WASM レスポンスフィルタを適用（F-133）。
///
/// `wasm_modules` が空なら早期 return（追加コストなし）。トレイラー送出時点で
/// HEADERS/DATA は既に送出済みのため、`LocalResponse`/`Pause` は適用不能として
/// 警告ログのみ出し、トレイラーはそのまま（あるいは `Continue` の変更のみ反映して）通す。
#[cfg(all(feature = "http2", feature = "grpc", feature = "wasm"))]
pub(super) async fn apply_h2_wasm_response_trailers(
    wasm_modules: &Arc<Vec<crate::wasm_plugin_config::ModuleRef>>,
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
        crate::wasm::FilterResult::Pause | crate::wasm::FilterResult::LocalResponse(_) => {
            // gRPC トレイラー送出時点で HEADERS/DATA は送出済みのため Pause/LocalResponse は
            // 適用できない（既存の送出済みレスポンスを差し替えられない）。元のトレイラーを
            // そのまま通す（grpc-status 欠落によるクライアントハングを避ける）。
            warn!(
                "WASM module requested Pause/LocalResponse on gRPC response trailers, \
                 but response head is already sent; keeping original trailers"
            );
            trailers
        }
    }
}

/// `Arc<Vec<u8>>` を `Bytes::from_owner` でゼロコピー化するための薄いラッパ（F-157）。
///
/// `Backend::MemoryFile` はコンテンツを `Arc<Vec<u8>>` で保持しており、そのまま
/// `Bytes` へ変換すると 1 回のディープコピーが発生する。`AsRef<[u8]>` を実装した
/// この newtype を `Bytes::from_owner` に渡すことで、`Arc` の参照カウントだけで
/// `Bytes` 化できる（`Bytes` の内部が `Arc` を保持し続けるだけでコピーは発生しない）。
#[cfg(feature = "http2")]
pub(super) struct ArcVecBytes(pub(super) Arc<Vec<u8>>);

#[cfg(feature = "http2")]
impl AsRef<[u8]> for ArcVecBytes {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

/// HTTP/2 静的応答の圧縮ネゴシエーションとヘッダー構築（B-32）
///
/// F-157: `data` は `Bytes`（参照カウント共有）で受け取り、非圧縮時はそのまま返す。
/// 圧縮する場合のみ `compress_body_h2` で新規バッファを確保する。呼び出し元
/// （`h2_sendfile`）が渡す `data` はキャッシュヒット時に参照カウントクローンだけで
/// 得られるため、非圧縮の静的配信ホットパスからディープコピーが完全に消える。
///
/// `static_ctx`（F-169）: 呼び出し元が静的配信のパスを把握している場合のみ
/// `Some((path, static_content_cache_cfg))` を渡す。この場合、圧縮結果を
/// `cache::compressed` でキャッシュする（`static_content_cache_cfg.enabled` が
/// `false` なら内部でキャッシュを使わず毎回圧縮する）。プロキシ応答は毎回内容が
/// 異なるためキャッシュしてはならず、その呼び出し元は必ず `None` を渡す
/// （`Backend::MemoryFile` もパスを持たないため `None`）。
#[cfg(feature = "http2")]
pub(super) fn build_h2_compressed_file_response(
    data: Bytes,
    mime_type: &str,
    security: &SecurityConfig,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    static_ctx: Option<(&Path, &cache::StaticContentCacheConfig)>,
) -> (Vec<(Bytes, Bytes)>, Bytes) {
    let should_compress = compression.should_compress(
        client_encoding,
        Some(mime_type.as_bytes()),
        Some(data.len()),
        None,
    );

    let mut header_store: Vec<(Bytes, Bytes)> = Vec::with_capacity(8);
    header_store.push((
        Bytes::from_static(b"content-type"),
        Bytes::copy_from_slice(mime_type.as_bytes()),
    ));
    if let Some(ref g) = get_server_header_guard() {
        header_store.push((Bytes::from_static(b"server"), g.value_bytes()));
    }
    for (k, v) in &security.add_response_headers {
        header_store.push((
            Bytes::copy_from_slice(k.as_bytes()),
            Bytes::copy_from_slice(v.as_bytes()),
        ));
    }
    // F-94: HTTP/3 広告（Alt-Svc）
    if let Some(g) = get_alt_svc_guard() {
        header_store.push((Bytes::from_static(b"alt-svc"), g.value_bytes()));
    }

    let response_body = if let Some(enc) = should_compress {
        let encoding_name: &'static [u8] = match enc {
            AcceptedEncoding::Zstd => b"zstd",
            AcceptedEncoding::Brotli => b"br",
            AcceptedEncoding::Gzip => b"gzip",
            AcceptedEncoding::Deflate => b"deflate",
            AcceptedEncoding::Identity => b"",
        };
        if !encoding_name.is_empty() {
            header_store.push((
                Bytes::from_static(b"content-encoding"),
                Bytes::from_static(encoding_name),
            ));
            header_store.push((
                Bytes::from_static(b"vary"),
                Bytes::from_static(b"Accept-Encoding"),
            ));
        }
        match static_ctx {
            Some((path, static_cfg)) if static_cfg.enabled => {
                let level = cache::compressed::compression_level(enc, compression);
                cache::compressed::get_or_compress(path, enc, level, static_cfg, || {
                    compress_body_h2(&data, enc, compression)
                })
            }
            _ => Bytes::from(compress_body_h2(&data, enc, compression)),
        }
    } else {
        data
    };

    (header_store, response_body)
}

/// F-169 Part B: zstd 圧縮コンテキストをスレッドローカルに保持して使い回す。
///
/// `zstd::encode_all` は呼び出しごとに内部コンテキスト（ワークスペース確保を伴う）
/// を作り捨てていた。ワーカースレッドは固定でリクエストを処理し続けるため、
/// スレッドローカルに `zstd::bulk::Compressor` を 1 つ保持し使い回すことで、
/// 確保自体を消す。**「確保が減った」ことと「速くなった」ことは別の主張**であり
/// （`AGENTS.md` F-168 の教訓）、本変更はスループット改善を約束するものではなく、
/// ホットパス絶対規則（不要な確保をしない）に沿わせるための一貫性改修である。
///
/// 圧縮レベルは設定リロードで変わり得るため、使い回す前に必ず
/// `set_compression_level` で反映する（level 変更自体のコストは
/// コンテキスト新規確保よりは軽い）。
#[cfg(all(feature = "http2", feature = "compression"))]
pub(super) fn zstd_compress_reuse_ctx(body: &[u8], level: i32) -> Vec<u8> {
    zstd_compress_reused(body, level).unwrap_or_else(|| body.to_vec())
}

/// スレッドローカルの zstd 圧縮コンテキストを使い回して `body` を圧縮する。
/// 失敗したら `None`（呼び出し側が無圧縮へフォールバックする）。
///
/// B-99: HTTP/1.1 のプロキシ圧縮経路（`transfer_compressed_response` /
/// `transfer_https_compressed_response`）は F-169 の後も `zstd::encode_all`（呼び出しごとに
/// コンテキストを確保・初期化するワンショット API）のままで、54KB の zstd レベル 3 に
/// 1 リクエストあたり約 1.6ms の CPU を使っていた（静的配信の 6 倍）。HTTP/2・HTTP/3 と
/// 同じくこの関数でコンテキストを使い回す。
#[cfg(feature = "compression")]
pub(super) fn zstd_compress_reused(body: &[u8], level: i32) -> Option<Vec<u8>> {
    use std::cell::RefCell;

    thread_local! {
        static ZSTD_COMPRESSOR: RefCell<Option<zstd::bulk::Compressor<'static>>> =
            const { RefCell::new(None) };
    }

    ZSTD_COMPRESSOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        let compressor = match slot.as_mut() {
            Some(c) => c,
            None => slot.get_or_insert(zstd::bulk::Compressor::new(level).ok()?),
        };
        compressor.set_compression_level(level).ok()?;
        compressor.compress(body).ok()
    })
}

/// HTTP/2 用レスポンスボディ圧縮ヘルパー関数
///
/// バイト配列を受け取り、指定されたエンコーディングで圧縮して返します。
/// 圧縮に失敗した場合は元のデータをそのまま返します。
#[cfg(all(feature = "http2", feature = "compression"))]
pub(super) fn compress_body_h2(
    body: &[u8],
    encoding: AcceptedEncoding,
    compression: &CompressionConfig,
) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    match encoding {
        AcceptedEncoding::Zstd => zstd_compress_reuse_ctx(body, compression.zstd_level),
        AcceptedEncoding::Gzip => {
            let level = Compression::new(compression.gzip_level);
            let mut encoder = GzEncoder::new(Vec::with_capacity(body.len()), level);
            if encoder.write_all(body).is_err() {
                return body.to_vec();
            }
            encoder.finish().unwrap_or_else(|_| body.to_vec())
        }
        AcceptedEncoding::Brotli => {
            let mut compressed = Vec::with_capacity(body.len());
            let params = brotli::enc::BrotliEncoderParams {
                quality: compression.brotli_level as i32,
                ..Default::default()
            };
            let mut input = std::io::Cursor::new(body);
            if brotli::BrotliCompress(&mut input, &mut compressed, &params).is_err() {
                return body.to_vec();
            }
            compressed
        }
        AcceptedEncoding::Deflate => {
            use flate2::write::DeflateEncoder;
            let level = Compression::new(compression.gzip_level);
            let mut encoder = DeflateEncoder::new(Vec::with_capacity(body.len()), level);
            if encoder.write_all(body).is_err() {
                return body.to_vec();
            }
            encoder.finish().unwrap_or_else(|_| body.to_vec())
        }
        AcceptedEncoding::Identity => body.to_vec(),
    }
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
