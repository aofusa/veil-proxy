//! 圧縮対応の送受信と応答の転送
//!
//! F-233 で `src/proxy.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// HTTPリクエスト送信とレスポンス受信（圧縮対応版）
// ====================
//
// 圧縮設定が有効な場合、バックエンドからのレスポンスを動的に圧縮して
// クライアントに転送します。
//
// 圧縮判定:
// 1. compression.enabled が true
// 2. クライアントが Accept-Encoding で圧縮をサポート
// 3. Content-Type が圧縮対象
// 4. Content-Length が min_size 以上
// 5. バックエンドのレスポンスが未圧縮
// ====================

/// HTTPリクエストを送信してレスポンスを受信（圧縮対応版）
/// 戻り値: Option<(status_code, response_size, backend_wants_keep_alive)>
pub(super) async fn proxy_http_request_with_compression(
    client_stream: &mut ServerTls,
    backend_stream: &mut TcpStream,
    request: Vec<u8>,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
    max_chunked_body_size: u64,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    cache_ctx: Option<&mut CacheSaveContext>,
    security: &SecurityConfig,
    wasm_modules: crate::wasm_plugin_config::WasmModules,
) -> Option<(u16, u64, bool, bool)> {
    // 1. リクエストヘッダー送信（タイムアウト付き）
    let write_result = timeout(WRITE_TIMEOUT, backend_stream.write_all(request)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return None;
    }

    // 2. リクエストボディ送信
    if !initial_body.is_empty() {
        let body_buf = initial_body.to_vec();
        let write_result = timeout(WRITE_TIMEOUT, backend_stream.write_all(body_buf)).await;
        if !matches!(write_result, Ok((Ok(_), _))) {
            return None;
        }
    }

    // 3. 残りのリクエストボディを転送
    if is_chunked {
        // Chunked転送の場合（DoS対策: ルートごとのmax_chunked_body_sizeで制限）
        match transfer_chunked_body(
            client_stream,
            backend_stream,
            initial_body,
            max_chunked_body_size,
        )
        .await
        {
            ChunkedTransferResult::Complete => {}
            ChunkedTransferResult::Failed => return None,
            // 不正なチャンク形式（要求の密輸の防止で打ち切る）
            ChunkedTransferResult::Invalid => return None,
            ChunkedTransferResult::SizeLimitExceeded => {
                return None;
            }
        }
    } else {
        // Content-Length転送の場合
        let remaining = content_length.saturating_sub(initial_body.len());
        if remaining > 0 {
            let body_timeout = Duration::from_secs(security.client_body_timeout_secs);
            match timeout(
                body_timeout,
                transfer_exact_bytes(client_stream, backend_stream, remaining),
            )
            .await
            {
                Ok(transferred) if transferred >= remaining as u64 => {}
                Ok(_) => return None,
                Err(_) => {
                    let _ = client_stream
                        .write_all(ERR_MSG_REQUEST_TIMEOUT.to_vec())
                        .await;
                    return Some((408, 0, false, true));
                }
            }
        }
    }

    // 4. レスポンスを受信して転送（圧縮対応、キャッシュ保存対応）
    let (total, status_code, backend_wants_keep_alive, client_must_close) =
        transfer_response_with_compression(
            backend_stream,
            client_stream,
            compression,
            client_encoding,
            cache_ctx,
            security,
            wasm_modules,
        )
        .await;

    Some((
        status_code,
        total,
        backend_wants_keep_alive,
        client_must_close,
    ))
}

// ====================
// レスポンス転送（圧縮対応版）
// ====================

/// レスポンスヘッダーを解析し、必要に応じて圧縮してクライアントに転送
/// キャッシュコンテキストが指定されている場合、レスポンスボディをキャプチャしてキャッシュに保存
///
/// 戻り値: (転送バイト数, ステータス, backend_wants_keep_alive, client_must_close)
/// `client_must_close` は B-17: 上流異常（ヘッダー不完全・CL 未達 EOF 等）でクライアント
/// 接続を即時クローズすべき場合に true。
#[cfg_attr(not(feature = "wasm"), allow(unused_variables))]
pub(super) async fn transfer_response_with_compression(
    backend_stream: &mut TcpStream,
    client_stream: &mut ServerTls,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    mut cache_ctx: Option<&mut CacheSaveContext>,
    security: &SecurityConfig,
    wasm_modules: crate::wasm_plugin_config::WasmModules,
) -> (u64, u16, bool, bool) {
    let mut accumulated = Vec::with_capacity(BUF_SIZE);
    let mut total = 0u64;
    let mut status_code = 502u16;
    // 初期値false: エラー時はKeep-Aliveを無効化
    let mut backend_wants_keep_alive = false;

    // kqueue: 要求を送った直後の 1 回目は応答がまだ届いていないのが普通なので、
    // 先読み（EAGAIN）+ 確認用 poll(2) を打たずに kevent の通知を待ってから読む
    // （`runtime::reactor::tcp::TcpStream::readable_lazy` 参照）。
    #[cfg(veil_poller_kqueue)]
    let mut first_read = true;

    // ヘッダー読み取り用バッファ
    loop {
        // B-17: ヘッダー読取は専用の短いタイムアウトで打ち切り、504 へ即変換する
        let read_buf = buf_get();
        #[cfg(veil_poller_kqueue)]
        let wait_first = std::mem::replace(&mut first_read, false);
        let read_result = timeout(BACKEND_HEADER_TIMEOUT, async {
            #[cfg(veil_poller_kqueue)]
            if wait_first {
                if let Err(e) = backend_stream.readable_lazy().await {
                    return (Err(e), read_buf);
                }
            }
            backend_stream.read(read_buf).await
        })
        .await;

        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => {
                warn!("Backend response header read timeout");
                return (total, 504, false, true);
            }
        };

        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                // B-17: ヘッダー完了前の EOF は 502 として即時応答する
                return (total, status_code, backend_wants_keep_alive, true);
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                return (total, status_code, backend_wants_keep_alive, true);
            }
        };

        returned_buf.set_valid_len(n);
        accumulated.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);

        // B-11: バックエンド由来の 1xx 中間応答（100 Continue / 103 Early Hints 等）を
        // 読み捨てる（最終応答と誤認して転送するとクライアントが最終応答を待ち続ける）。
        drain_interim_responses(&mut accumulated);

        // ヘッダーが完全に受信されたかチェック
        if let Some(parsed) = parse_http_response(&accumulated) {
            status_code = parsed.status_code;
            backend_wants_keep_alive = !parsed.is_connection_close;

            let header_len = parsed.header_len;
            let body_start = &accumulated[header_len..];

            // Content-Type と Content-Encoding を取得
            let content_type = extract_header_value(&accumulated[..header_len], b"content-type");
            let existing_encoding =
                extract_header_value(&accumulated[..header_len], b"content-encoding");

            // 圧縮すべきか判定
            let should_compress = compression.should_compress(
                client_encoding,
                content_type,
                parsed.content_length,
                existing_encoding,
            );

            if let Some(encoding) = should_compress {
                // 圧縮有効: ヘッダーを書き換えて圧縮転送
                // 注意: 圧縮時はキャッシュ保存をスキップ（圧縮後のデータをキャッシュするには追加実装が必要）
                // リクエストごとに通る経路なので info ではなく debug（既定レベルで出さない）。
                ftlog::debug!(
                    "[Compression] Initializing compressed transfer with {:?}",
                    encoding
                );
                let result = transfer_compressed_response(
                    client_stream,
                    backend_stream,
                    &accumulated[..header_len],
                    body_start,
                    parsed.content_length,
                    parsed.is_chunked,
                    encoding,
                    compression,
                    backend_wants_keep_alive,
                    security,
                )
                .await;

                return (result.0, status_code, result.1, false);
            } else {
                // 圧縮無効: そのまま転送（キャッシュ保存対応）

                // キャッシュコンテキストがある場合、ヘッダーを設定
                if let Some(ref mut ctx) = cache_ctx {
                    // ヘッダーを解析してキャッシュコンテキストに保存
                    let mut headers_storage = [httparse::EMPTY_HEADER; 64];
                    let mut response = httparse::Response::new(&mut headers_storage);
                    if response.parse(&accumulated[..header_len]).is_ok() {
                        let headers: Vec<(Box<[u8]>, Box<[u8]>)> = response
                            .headers
                            .iter()
                            .map(|h| (h.name.as_bytes().into(), h.value.into()))
                            .collect();
                        ctx.set_headers(headers, status_code);
                    }

                    // 初期ボディをキャプチャ
                    ctx.append_body(body_start);
                }

                // ヘッダーを修正（security.add_response_headersを追加、remove_response_headersを削除）。
                // WASM のフィルタが無ければ、1 本のバッファへ直接書く（以前はヘッダの行ごとに `format!` と
                // `Vec` を作って連結していた。FreeBSD の h1_proxy_tls 3B のプロファイルで fmt・realloc が上位。v0.8.0）
                #[cfg(feature = "wasm")]
                let wasm_active = !wasm_modules.is_empty();
                #[cfg(not(feature = "wasm"))]
                let wasm_active = false;
                let mut modified_headers = if !wasm_active {
                    render_response_head(&accumulated[..header_len], status_code, security)
                } else {
                    let mut modified_headers = accumulated[..header_len].to_vec();

                    // ヘッダーをパースして操作
                    let mut headers_storage = [httparse::EMPTY_HEADER; 64];
                    let mut response = httparse::Response::new(&mut headers_storage);
                    if response.parse(&modified_headers).is_ok() {
                        let mut new_header_lines = Vec::new();

                        // ステータス行を追加
                        let status_line = format!(
                            "HTTP/1.1 {} {}\r\n",
                            status_code,
                            status_code_to_reason(status_code)
                        );
                        new_header_lines.push(status_line.into_bytes());

                        // 既存のヘッダーを追加（削除対象を除外）
                        // eq_ignore_ascii_case でアロケーションなしに比較
                        for header in response.headers.iter() {
                            if !security
                                .remove_response_headers
                                .iter()
                                .any(|h| header.name.as_bytes().eq_ignore_ascii_case(h.as_bytes()))
                            {
                                new_header_lines.push(
                                    format!(
                                        "{}: {}\r\n",
                                        header.name,
                                        std::str::from_utf8(header.value).unwrap_or("")
                                    )
                                    .into_bytes(),
                                );
                            }
                        }

                        // 追加するヘッダーを追加
                        for (header_name, header_value) in &security.add_response_headers {
                            new_header_lines.push(
                                format!("{}: {}\r\n", header_name, header_value).into_bytes(),
                            );
                        }
                        // F-94: HTTP/3 広告（Alt-Svc）
                        if let Some(g) = get_alt_svc_guard() {
                            let mut line = Vec::with_capacity(16 + g.value().len());
                            line.extend_from_slice(b"Alt-Svc: ");
                            line.extend_from_slice(g.value());
                            line.extend_from_slice(b"\r\n");
                            new_header_lines.push(line);
                        }

                        // WASMレスポンスヘッダーフィルタを適用
                        #[cfg(feature = "wasm")]
                        {
                            if !wasm_modules.is_empty() {
                                let config = CURRENT_CONFIG.load();
                                if let Some(ref wasm_engine) = config.wasm_filter_engine {
                                    // 現在のヘッダーをVec<(Vec<u8>, Vec<u8>)>形式に変換
                                    let current_headers: Vec<(Vec<u8>, Vec<u8>)> = new_header_lines
                                        .iter()
                                        .skip(1) // ステータス行をスキップ
                                        .filter_map(|line| {
                                            let line_str = std::str::from_utf8(line).ok()?;
                                            let line_trimmed = line_str.trim_end_matches("\r\n");
                                            if line_trimmed.is_empty() {
                                                return None;
                                            }
                                            let colon_pos = line_trimmed.find(':')?;
                                            let name =
                                                line_trimmed.as_bytes()[..colon_pos].to_vec();
                                            let value = line_trimmed[colon_pos + 1..]
                                                .trim_start()
                                                .as_bytes()
                                                .to_vec();
                                            Some((name, value))
                                        })
                                        .collect();

                                    // WASMフィルタを実行（レスポンスヘッダー処理）
                                    let wasm_result = wasm_engine
                                        .clone()
                                        .on_response_headers_with_modules_async(
                                            wasm_modules.clone(),
                                            status_code,
                                            current_headers,
                                            true, // end_of_stream
                                        )
                                        .await;

                                    match wasm_result {
                                        crate::wasm::FilterResult::Continue {
                                            headers: modified_headers,
                                            ..
                                        } => {
                                            // WASMから修正されたヘッダーで置き換え
                                            new_header_lines.clear();

                                            // ステータス行を再追加
                                            let status_line = format!(
                                                "HTTP/1.1 {} {}\r\n",
                                                status_code,
                                                status_code_to_reason(status_code)
                                            );
                                            new_header_lines.push(status_line.into_bytes());

                                            // WASMから返されたヘッダーを追加
                                            for (name, value) in modified_headers {
                                                let mut line = Vec::with_capacity(
                                                    name.len() + value.len() + 4,
                                                );
                                                line.extend_from_slice(&name);
                                                line.extend_from_slice(b": ");
                                                line.extend_from_slice(&value);
                                                line.extend_from_slice(b"\r\n");
                                                new_header_lines.push(line);
                                            }
                                        }
                                        crate::wasm::FilterResult::LocalResponse(_) => {
                                            // レスポンスヘッダー処理ではLocalResponseは無視
                                            // （すでにバックエンドレスポンスを受信しているため）
                                        }
                                        crate::wasm::FilterResult::Pause => {
                                            // 非同期処理待ち（現在は未実装）
                                        }
                                    }
                                }
                            }
                        }

                        // ヘッダー終了マーカーを追加
                        new_header_lines.push(b"\r\n".to_vec());

                        // 結合
                        modified_headers = new_header_lines.into_iter().flatten().collect();
                    }
                    modified_headers
                };
                // `[route.traffic.cors]` の実際の応答のヘッダ（F-273。CORS の無いルートは分岐だけ）
                crate::route_cors::apply_h1(&mut modified_headers, security);

                // 修正したヘッダー + 初期ボディを送信
                // F-59: 平文接続では 1 回の SENDMSG（scatter-gather）で送出
                let body_data = body_start.to_vec();
                let write_result = timeout(
                    WRITE_TIMEOUT,
                    client_stream.write_all_vectored(modified_headers, body_data),
                )
                .await;
                if !matches!(write_result, Ok((Ok(()), _, _))) {
                    return (total, status_code, false, true);
                }
                total += header_len as u64;
                total += body_start.len() as u64;

                // 残りのボディを転送（キャッシュキャプチャ対応）
                let body_remaining = if let Some(cl) = parsed.content_length {
                    cl.saturating_sub(body_start.len())
                } else if parsed.is_chunked {
                    // Chunked の場合は終端まで転送
                    usize::MAX
                } else {
                    0
                };

                if body_remaining > 0 {
                    let transferred = transfer_response_body_with_cache(
                        backend_stream,
                        client_stream,
                        parsed.content_length,
                        parsed.is_chunked,
                        body_start,
                        cache_ctx,
                    )
                    .await;
                    total += transferred;

                    // B-17: Content-Length 宣言に満たないまま上流が終了した場合、
                    // クライアントは残りボディを待ち続けるため接続を即クローズして通知する
                    if parsed.content_length.is_some() && transferred < body_remaining as u64 {
                        warn!(
                            "Backend response body incomplete: {} < {}",
                            transferred, body_remaining
                        );
                        return (total, status_code, false, true);
                    }
                } else if parsed.content_length.is_some() {
                    // B-162: 本文が最初の読み込みに収まった（小さい応答）ときも保存する。以前は残りを転送する
                    // `transfer_response_body_with_cache` の中でしか保存せず、小さい応答は一度もキャッシュされなかった。
                    // Content-Length のある完全な本文だけ（接続の終わりで区切る応答は途中かもしれないので保存しない）。
                    if let Some(ctx) = cache_ctx {
                        ctx.save_to_cache();
                    }
                }

                return (total, status_code, backend_wants_keep_alive, false);
            }
        }

        // B-17: レスポンスヘッダーの上限超過は 502 で即時応答する
        // B-234: ヘッダが 65 個以上・不正な応答は、上限か読み取りのタイムアウトまで待たずに 502
        if accumulated.len() > MAX_RESPONSE_HEADER_SIZE
            || crate::http_utils::response_head_invalid(&accumulated)
        {
            warn!(
                "Backend response header too large (> {} bytes) or invalid",
                MAX_RESPONSE_HEADER_SIZE
            );
            return (0, 502, false, true);
        }
    }
}

/// 上流の応答のヘッダ（ステータス行から空行まで）を、クライアントへ返す形にする（WASM のフィルタが無いとき）。
///
/// ステータス行は `HTTP/1.1 <code> <reason>` に直し、`remove_response_headers` を除き、`add_response_headers` と
/// Alt-Svc を足す。確保は返すバッファ 1 本だけ。解析できなければ受け取ったまま返す（従来どおり）。
pub(super) fn render_response_head(raw: &[u8], status: u16, security: &SecurityConfig) -> Vec<u8> {
    let mut storage = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut storage);
    if response.parse(raw).is_err() {
        return raw.to_vec();
    }
    let added: usize = security
        .add_response_headers
        .iter()
        .map(|(k, v)| k.len() + v.len() + 4)
        .sum();
    let alt_svc = get_alt_svc_guard();
    let mut out = Vec::with_capacity(
        raw.len() + added + alt_svc.as_ref().map(|g| g.value().len() + 11).unwrap_or(0) + 16,
    );
    out.extend_from_slice(b"HTTP/1.1 ");
    let mut code = itoa::Buffer::new();
    out.extend_from_slice(code.format(status).as_bytes());
    out.push(b' ');
    out.extend_from_slice(status_code_to_reason(status).as_bytes());
    out.extend_from_slice(b"\r\n");
    for header in response.headers.iter() {
        if security
            .remove_response_headers
            .iter()
            .any(|h| header.name.as_bytes().eq_ignore_ascii_case(h.as_bytes()))
        {
            continue;
        }
        out.extend_from_slice(header.name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(header.value);
        out.extend_from_slice(b"\r\n");
    }
    for (name, value) in &security.add_response_headers {
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    // F-94: HTTP/3 広告（Alt-Svc）
    if let Some(g) = alt_svc {
        out.extend_from_slice(b"Alt-Svc: ");
        out.extend_from_slice(g.value());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out
}

/// 圧縮してレスポンスを転送
/// 戻り値: (転送バイト数, backend_wants_keep_alive)
#[cfg(feature = "compression")]
pub(super) async fn transfer_compressed_response(
    client_stream: &mut ServerTls,
    backend_stream: &mut TcpStream,
    original_headers: &[u8],
    initial_body: &[u8],
    content_length: Option<usize>,
    is_chunked: bool,
    encoding: AcceptedEncoding,
    compression: &CompressionConfig,
    backend_wants_keep_alive: bool,
    security: &SecurityConfig,
) -> (u64, bool) {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    let mut total = 0u64;

    // 1. まず全てのボディデータを収集（ストリーミングは将来の改善）
    let mut body_data = initial_body.to_vec();

    if let Some(cl) = content_length {
        let remaining = cl.saturating_sub(initial_body.len());
        if remaining > 0 {
            let mut remaining_to_read = remaining;
            while remaining_to_read > 0 {
                let read_buf = buf_get();
                let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;

                let (res, mut returned_buf) = match read_result {
                    Ok(result) => result,
                    Err(_) => {
                        return (total, false);
                    }
                };

                let n = match res {
                    Ok(0) => {
                        buf_put(returned_buf);
                        break;
                    }
                    Ok(n) => n.min(remaining_to_read),
                    Err(_) => {
                        buf_put(returned_buf);
                        return (total, false);
                    }
                };

                returned_buf.set_valid_len(n);
                body_data.extend_from_slice(returned_buf.as_valid_slice());
                buf_put(returned_buf);
                remaining_to_read = remaining_to_read.saturating_sub(n);
            }
        }
    } else if is_chunked {
        // Chunked の場合はデコードして収集
        let mut decoder = ChunkedDecoder::new_unlimited();

        // 初期ボディをデコーダにフィード
        let initial_result = decoder.feed(initial_body);
        if initial_result == ChunkedFeedResult::Complete {
            // 初期ボディで完了（本来はデコード済みボディが必要だが、簡略化）
        } else {
            // 残りを読み取り
            loop {
                let read_buf = buf_get();
                let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;

                let (res, mut returned_buf) = match read_result {
                    Ok(result) => result,
                    Err(_) => {
                        // タイムアウト
                        break;
                    }
                };

                let n = match res {
                    Ok(0) => {
                        buf_put(returned_buf);
                        break;
                    }
                    Ok(n) => n,
                    Err(_) => {
                        buf_put(returned_buf);
                        break;
                    }
                };

                returned_buf.set_valid_len(n);
                let chunk = returned_buf.as_valid_slice();
                body_data.extend_from_slice(chunk);
                let feed_result = decoder.feed(chunk);
                buf_put(returned_buf);

                if feed_result == ChunkedFeedResult::Complete {
                    break;
                }
            }
        }
    }

    // 2. ボディを圧縮
    let compressed_body = match encoding {
        AcceptedEncoding::Zstd => match zstd_compress_reused(&body_data, compression.zstd_level) {
            Some(compressed) => compressed,
            None => {
                return transfer_uncompressed_fallback(
                    client_stream,
                    original_headers,
                    &body_data,
                    security,
                )
                .await;
            }
        },
        AcceptedEncoding::Gzip => {
            let level = Compression::new(compression.gzip_level);
            let mut encoder = GzEncoder::new(Vec::new(), level);
            if encoder.write_all(&body_data).is_err() {
                // 圧縮失敗: 非圧縮で送信
                return transfer_uncompressed_fallback(
                    client_stream,
                    original_headers,
                    &body_data,
                    security,
                )
                .await;
            }
            match encoder.finish() {
                Ok(data) => data,
                Err(_) => {
                    return transfer_uncompressed_fallback(
                        client_stream,
                        original_headers,
                        &body_data,
                        security,
                    )
                    .await;
                }
            }
        }
        AcceptedEncoding::Brotli => {
            let mut compressed = Vec::new();
            let params = brotli::enc::BrotliEncoderParams {
                quality: compression.brotli_level as i32,
                ..Default::default()
            };
            let mut input = std::io::Cursor::new(&body_data);
            if brotli::BrotliCompress(&mut input, &mut compressed, &params).is_err() {
                return transfer_uncompressed_fallback(
                    client_stream,
                    original_headers,
                    &body_data,
                    security,
                )
                .await;
            }
            compressed
        }
        AcceptedEncoding::Deflate => {
            let level = Compression::new(compression.gzip_level);
            let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), level);
            if encoder.write_all(&body_data).is_err() {
                return transfer_uncompressed_fallback(
                    client_stream,
                    original_headers,
                    &body_data,
                    security,
                )
                .await;
            }
            match encoder.finish() {
                Ok(data) => data,
                Err(_) => {
                    return transfer_uncompressed_fallback(
                        client_stream,
                        original_headers,
                        &body_data,
                        security,
                    )
                    .await;
                }
            }
        }
        AcceptedEncoding::Identity => {
            // 圧縮なし（ここには来ないはず）
            body_data
        }
    };

    // 3. 新しいヘッダーを構築
    let new_headers =
        build_compressed_headers(original_headers, encoding, compressed_body.len(), security);

    // 4. ヘッダー送信（所有権を移動、clone 不要）
    let new_headers_len = new_headers.len();
    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(new_headers)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return (total, false);
    }
    total += new_headers_len as u64;

    // 5. 圧縮済みボディ送信（所有権を移動、clone 不要）
    let compressed_len = compressed_body.len();
    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(compressed_body)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return (total, false);
    }
    total += compressed_len as u64;

    (total, backend_wants_keep_alive)
}

/// compression feature 無効時のスタブ
#[cfg(not(feature = "compression"))]
pub(super) async fn transfer_compressed_response(
    client_stream: &mut ServerTls,
    _backend_stream: &mut TcpStream,
    original_headers: &[u8],
    initial_body: &[u8],
    _content_length: Option<usize>,
    _is_chunked: bool,
    _encoding: AcceptedEncoding,
    _compression: &CompressionConfig,
    _backend_wants_keep_alive: bool,
    security: &SecurityConfig,
) -> (u64, bool) {
    transfer_uncompressed_fallback(client_stream, original_headers, initial_body, security).await
}

/// 圧縮失敗時のフォールバック（非圧縮で送信）
pub(super) async fn transfer_uncompressed_fallback(
    client_stream: &mut ServerTls,
    original_headers: &[u8],
    body_data: &[u8],
    security: &SecurityConfig,
) -> (u64, bool) {
    let mut total = 0u64;

    // ヘッダー送信（所有権ベースの I/O なので to_vec、clone は不要）
    let headers_len = original_headers.len();
    let mut head = original_headers.to_vec();
    // `[route.traffic.cors]` の実際の応答のヘッダ（F-273）
    crate::route_cors::apply_h1(&mut head, security);
    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(head)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return (total, false);
    }
    total += headers_len as u64;

    // ボディ送信
    let body_len = body_data.len();
    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(body_data.to_vec())).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return (total, false);
    }
    total += body_len as u64;

    (total, true)
}

/// 圧縮用にヘッダーを書き換え
// compression feature 有効時のみ呼び出される
#[cfg(feature = "compression")]
pub(super) fn build_compressed_headers(
    original_headers: &[u8],
    encoding: AcceptedEncoding,
    compressed_length: usize,
    security: &SecurityConfig,
) -> Vec<u8> {
    let mut headers_storage = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers_storage);

    if response.parse(original_headers).is_err() {
        return original_headers.to_vec();
    }

    let status_code = response.code.unwrap_or(200);
    let reason = status_code_to_reason(status_code);

    let mut new_headers = Vec::with_capacity(original_headers.len() + 64);

    // ステータス行
    new_headers.extend_from_slice(b"HTTP/1.1 ");
    let mut code_buf = itoa::Buffer::new();
    new_headers.extend_from_slice(code_buf.format(status_code).as_bytes());
    new_headers.extend_from_slice(b" ");
    new_headers.extend_from_slice(reason.as_bytes());
    new_headers.extend_from_slice(b"\r\n");

    // 元のヘッダーをコピー（Content-Length, Content-Encoding, Transfer-Encoding を除く）
    // 削除対象のヘッダーも除外
    // アロケーションなしで大文字小文字無視ヘッダー比較
    for header in response.headers.iter() {
        let n = header.name.as_bytes();
        if n.eq_ignore_ascii_case(b"content-length")
            || n.eq_ignore_ascii_case(b"content-encoding")
            || n.eq_ignore_ascii_case(b"transfer-encoding")
            || security
                .remove_response_headers
                .iter()
                .any(|h| n.eq_ignore_ascii_case(h.as_bytes()))
        {
            continue;
        }
        new_headers.extend_from_slice(header.name.as_bytes());
        new_headers.extend_from_slice(b": ");
        new_headers.extend_from_slice(header.value);
        new_headers.extend_from_slice(b"\r\n");
    }

    // Content-Encoding を追加
    new_headers.extend_from_slice(b"Content-Encoding: ");
    new_headers.extend_from_slice(encoding.as_header_value());
    new_headers.extend_from_slice(b"\r\n");

    // Content-Length を追加（圧縮後のサイズ）
    new_headers.extend_from_slice(b"Content-Length: ");
    let mut len_buf = itoa::Buffer::new();
    new_headers.extend_from_slice(len_buf.format(compressed_length).as_bytes());
    new_headers.extend_from_slice(b"\r\n");

    // Vary ヘッダーを追加（キャッシュ制御）
    new_headers.extend_from_slice(b"Vary: Accept-Encoding\r\n");

    // 追加するヘッダーを追加
    for (header_name, header_value) in &security.add_response_headers {
        new_headers.extend_from_slice(header_name.as_bytes());
        new_headers.extend_from_slice(b": ");
        new_headers.extend_from_slice(header_value.as_bytes());
        new_headers.extend_from_slice(b"\r\n");
    }
    // F-94: HTTP/3 広告（Alt-Svc）
    append_alt_svc_header_line(&mut new_headers);

    // ヘッダー終端
    new_headers.extend_from_slice(b"\r\n");
    // `[route.traffic.cors]` の実際の応答のヘッダ（F-273）
    crate::route_cors::apply_h1(&mut new_headers, security);

    new_headers
}

/// レスポンスボディを転送（キャッシュキャプチャ対応版）
///
/// キャッシュコンテキストが指定されている場合、ボディをキャプチャしてキャッシュに保存します。
pub(super) async fn transfer_response_body_with_cache(
    backend_stream: &mut TcpStream,
    client_stream: &mut ServerTls,
    content_length: Option<usize>,
    is_chunked: bool,
    initial_body: &[u8],
    mut cache_ctx: Option<&mut CacheSaveContext>,
) -> u64 {
    let mut total = 0u64;

    if let Some(cl) = content_length {
        let remaining = cl.saturating_sub(initial_body.len());
        if remaining > 0 {
            let transferred = transfer_exact_bytes_from_backend_with_cache(
                backend_stream,
                client_stream,
                remaining,
                cache_ctx,
            )
            .await;
            total += transferred;
        }
    } else if is_chunked {
        // Chunked 転送（キャッシュキャプチャ対応）
        let mut decoder = ChunkedDecoder::new_unlimited();
        decoder.feed(initial_body);

        if decoder.is_complete() {
            // 転送完了後にキャッシュに保存
            if let Some(ctx) = cache_ctx {
                ctx.save_to_cache();
            }
            return total;
        }

        loop {
            let read_buf = buf_get();
            let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;

            let (res, mut returned_buf) = match read_result {
                Ok(result) => result,
                Err(_) => break,
            };

            let n = match res {
                Ok(0) => {
                    buf_put(returned_buf);
                    break;
                }
                Ok(n) => n,
                Err(_) => {
                    buf_put(returned_buf);
                    break;
                }
            };

            returned_buf.set_valid_len(n);
            let chunk = returned_buf.as_valid_slice();
            let feed_result = decoder.feed(chunk);

            // キャッシュコンテキストにボディをキャプチャ
            if let Some(ref mut ctx) = cache_ctx {
                ctx.append_body(chunk);
            }

            // クライアントに転送
            let chunk_data = chunk.to_vec();
            let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(chunk_data)).await;
            buf_put(returned_buf);

            if !matches!(write_result, Ok((Ok(_), _))) {
                break;
            }
            total += n as u64;

            if feed_result == ChunkedFeedResult::Complete {
                break;
            }
        }

        // 転送完了後にキャッシュに保存
        if let Some(ctx) = cache_ctx {
            ctx.save_to_cache();
        }
    }

    total
}

/// バックエンドから正確なバイト数を読み取りクライアントに転送（キャッシュキャプチャ対応版）
pub(super) async fn transfer_exact_bytes_from_backend_with_cache(
    backend_stream: &mut TcpStream,
    client_stream: &mut ServerTls,
    mut remaining: usize,
    mut cache_ctx: Option<&mut CacheSaveContext>,
) -> u64 {
    let mut total = 0u64;

    while remaining > 0 {
        let read_buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;

        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => break,
        };

        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                break;
            }
            Ok(n) => n.min(remaining),
            Err(_) => {
                buf_put(returned_buf);
                break;
            }
        };

        returned_buf.set_valid_len(n);
        let chunk = returned_buf.as_valid_slice();

        // キャッシュコンテキストにボディをキャプチャ
        if let Some(ref mut ctx) = cache_ctx {
            ctx.append_body(&chunk[..n]);
        }

        // クライアントに転送
        let chunk_data = chunk[..n].to_vec();
        let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(chunk_data)).await;
        buf_put(returned_buf);

        if !matches!(write_result, Ok((Ok(_), _))) {
            break;
        }

        total += n as u64;
        remaining = remaining.saturating_sub(n);
    }

    // 転送完了後にキャッシュに保存
    if let Some(ctx) = cache_ctx {
        ctx.save_to_cache();
    }

    total
}

#[cfg(test)]
mod response_head_tests {
    use super::*;

    /// ヘッダの削除・追加とステータス行の書き直し（v0.8.0 の 1 本のバッファへの書き出し）
    #[test]
    fn render_response_head_applies_policy() {
        let sec = SecurityConfig {
            remove_response_headers: vec!["X-Powered-By".to_string()],
            add_response_headers: [("X-Frame-Options".to_string(), "DENY".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let raw =
            b"HTTP/1.0 200 OK\r\nContent-Length: 3\r\nx-powered-by: php\r\nX-Bin: \xff\xfe\r\n\r\n";
        let out = render_response_head(raw, 200, &sec);
        let want: &[u8] =
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nX-Bin: \xff\xfe\r\nX-Frame-Options: DENY\r\n\r\n";
        if get_alt_svc_guard().is_none() {
            assert_eq!(out, want);
        }
        // 解析できないものはそのまま
        assert_eq!(
            render_response_head(b"garbage", 200, &sec),
            b"garbage".to_vec()
        );
    }
}
