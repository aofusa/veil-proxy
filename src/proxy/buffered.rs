//! バッファリングの結果型と、バッファリング対応の送受信
//!
//! F-233 で `src/proxy.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// バッファリング結果型
// ====================

/// バッファリングされたボディ結果
pub(super) enum BufferedBodyResult {
    /// メモリ内にバッファリング
    Memory(Vec<u8>),
    /// ディスクにスピルオーバー
    Disk { path: std::path::PathBuf, size: u64 },
    /// バッファサイズ制限超過 (507 Insufficient Storage)
    LimitExceeded,
    /// バッファリング失敗（ストリーミングにフォールバック）
    Failed,
    /// `mode = "adaptive"` で溜めない応答（応答の Content-Length が閾値を超える・chunked・長さ不明。B-296）。
    /// `initial` はヘッダと同じ読み込みに入っていた本文
    Stream {
        kind: crate::http_utils::ResponseBody,
        initial: Vec<u8>,
    },
}

impl BufferedBodyResult {}

// ====================
// HTTPリクエスト送信とレスポンス受信（バッファリング対応版）
// ====================
//
// バッファリングが有効な場合、バックエンドからのレスポンス全体を
// メモリにバッファリングしてからクライアントに転送します。
// これにより、バックエンド接続を早期に解放し、低速クライアントによる
// バックエンドスレッド占有を防止します。
// ====================

/// バッファリング転送でHTTPリクエストを処理
///
/// バックエンドからレスポンス全体を受信してバッファに格納し、
/// バックエンド接続を解放してからクライアントへ送信します。
///
/// 戻り値: Option<(status_code, response_size, backend_wants_keep_alive)>
/// バッファリング転送でリクエストを処理
pub(super) async fn proxy_request_buffered<R>(
    client_stream: &mut ServerTls,
    backend_stream: &mut R,
    request: Vec<u8>,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
    max_chunked_body_size: u64,
    buffering_config: &buffering::BufferingConfig,
    cache_ctx: Option<&mut CacheSaveContext>,
    security: &SecurityConfig,
    #[cfg(feature = "wasm")] wasm_modules: &crate::wasm_plugin_config::WasmModules,
    // F-285: Some なら溜めた本文へ Wasm の応答本文のフェーズをかけ、その後で（設定があれば）圧縮する
    #[cfg(feature = "wasm")] wasm_body: Option<(&CompressionConfig, crate::config::ClientAccept)>,
    // `buffering` の適用を数えるホスト（`mode = "adaptive"` は溜めると決めたときだけ数える。B-296）
    metrics_host: &str,
) -> Option<(u16, u64, bool, bool)>
where
    R: AsyncReader
        + AsyncWriter
        + Unpin
        + crate::runtime::io::AsyncReadRent
        + crate::runtime::io::AsyncWriteRentExt,
{
    // B-245: HEAD への応答は本文を持たない（書く前に判定する）
    let is_head = request_is_head(&request);

    // 1. リクエストヘッダー送信
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
            ChunkedTransferResult::SizeLimitExceeded => return None,
        }
    } else {
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

    // 4. レスポンスを受信してバッファリング（adaptive は応答の長さを見て溜めるか決める。Wasm の応答本文の
    // フェーズは溜めた本文にかけるので常に溜める）
    #[cfg(feature = "wasm")]
    let allow_stream = wasm_body.is_none();
    #[cfg(not(feature = "wasm"))]
    let allow_stream = true;
    let mut cache_ctx = cache_ctx;
    let buffered = receive_and_buffer_response(
        backend_stream,
        buffering_config,
        cache_ctx.as_deref_mut(),
        is_head,
        allow_stream,
    )
    .await;
    if buffering_config.mode == crate::buffering::BufferingMode::Adaptive
        && matches!(&buffered, Some((_, _, b, ..)) if !matches!(b, BufferedBodyResult::Stream { .. }))
    {
        record_buffering_used(metrics_host);
    }

    match buffered {
        Some((
            status_code,
            headers_data,
            body_result,
            backend_wants_keep_alive,
            until_close,
            _chunked,
        )) => {
            // ステータス行を HTTP/1.1 に直し、応答ヘッダの方針・Alt-Svc・Wasm・CORS をかける（他の HTTP/1.1 の
            // 経路と同じ。以前は上流の head をそのまま返していた。B-267・B-268・B-271）
            let head = super::compress::finish_h1_upstream_head(
                &headers_data,
                status_code,
                security,
                #[cfg(feature = "wasm")]
                wasm_modules,
            )
            .await;
            // B-17: ボディのバッファリングに失敗した場合、クライアントへは未送信のため
            // ヘッダーだけ送って（CL 分のボディを待たせて）ハングさせず、
            // None → 呼び出し元の 502 送出 + クローズに委ねる
            if matches!(body_result, BufferedBodyResult::Failed) {
                return None;
            }
            let mut headers_data = match head {
                Ok(h) => h,
                // 応答ヘッダの段のローカルレスポンス（B-300。上流の本文は読み切っている）
                Err(resp) => {
                    return super::sendfile::write_h1_wasm_local(client_stream, &resp, is_head)
                        .await
                        .map(|(st, n)| (st, n, backend_wants_keep_alive, false));
                }
            };
            // B-296: 溜めない応答はヘッダと最初の本文を送り、残りを区切りどおりに流す
            if let BufferedBodyResult::Stream { kind, initial } = body_result {
                let start = super::transfer::response_body_start(kind, &initial);
                if let Some(ctx) = cache_ctx.as_deref_mut() {
                    ctx.append_body(&initial[..start.send]);
                }
                let mut first = headers_data;
                let head_len = first.len() as u64;
                first.extend_from_slice(&initial[..start.send]);
                match timeout(
                    Duration::from_secs(buffering_config.client_write_timeout_secs),
                    client_stream.write_all(first),
                )
                .await
                {
                    Ok((Ok(_), _)) => {}
                    _ => return Some((status_code, 0, false, true)),
                }
                let sent_first = head_len + start.send as u64;
                let out = super::transfer::transfer_response_body_rest(
                    backend_stream,
                    client_stream,
                    kind,
                    start,
                    cache_ctx,
                )
                .await;
                return Some((
                    status_code,
                    sent_first + out.total,
                    backend_wants_keep_alive && out.reusable(kind),
                    out.client_must_close(kind),
                ));
            }
            #[cfg(feature = "wasm")]
            let mut body_result = body_result;
            // F-285: Wasm の応答本文のフェーズ（メモリに溜まった本文だけ。ディスクへ溢れた本文はかけずに流す）
            #[cfg(feature = "wasm")]
            if let (Some((compression, client_encoding)), BufferedBodyResult::Memory(raw)) =
                (wasm_body, &mut body_result)
            {
                if !is_head && !matches!(status_code, 100..=199 | 204 | 304) {
                    let raw = std::mem::take(raw);
                    let decoded = if _chunked {
                        crate::http_utils::decode_chunked_body(&raw)
                    } else {
                        raw
                    };
                    match super::compress::wasm_filter_response_body(
                        wasm_modules,
                        Bytes::from(decoded),
                    )
                    .await
                    {
                        Ok(b) => {
                            let (ct, ce) = h1_head_content_type_encoding(&headers_data);
                            let enc = compression.should_compress(
                                client_encoding,
                                ct.as_deref(),
                                Some(b.len()),
                                ce.as_deref(),
                            );
                            let body = match enc {
                                Some(enc) => {
                                    super::compress::add_h1_content_encoding(
                                        &mut headers_data,
                                        enc,
                                    );
                                    crate::static_response::compress_body(&b, enc, compression)
                                }
                                None => b.to_vec(),
                            };
                            super::compress::set_h1_content_length(&mut headers_data, body.len());
                            body_result = BufferedBodyResult::Memory(body);
                        }
                        Err(local) => {
                            let response = super::h1::wasm_local_response_h1(&local, is_head);
                            let len = response.len() as u64;
                            let _ = timeout(
                                Duration::from_secs(buffering_config.client_write_timeout_secs),
                                client_stream.write_all(response),
                            )
                            .await;
                            return Some((local.status_code, len, backend_wants_keep_alive, false));
                        }
                    }
                }
            }
            // B-262: 接続の終わりで区切る本文は長さが分かったので Content-Length を付ける。veil はクライアントの
            // 接続を閉じるときに close_notify を送らないので、閉じるだけでは厳格な TLS クライアントは切り詰めと区別できない
            if until_close {
                let len = match &body_result {
                    BufferedBodyResult::Memory(b) => Some(b.len() as u64),
                    BufferedBodyResult::Disk { size, .. } => Some(*size),
                    _ => None,
                };
                if let Some(len) = len {
                    add_content_length(&mut headers_data, len);
                }
            }
            // 5. バッファからクライアントへ送信
            let mut total = 0u64;

            // buffer_headers の設定に応じてヘッダーとボディを送信
            if buffering_config.buffer_headers {
                // buffer_headers = true: ヘッダーとボディを結合して送信（デフォルト動作）
                // これにより、クライアントへの書き込み回数を削減
                match body_result {
                    BufferedBodyResult::Memory(body_data) => {
                        // ヘッダーとボディを結合（headers_data を move してコピー回数を削減）
                        let mut combined = headers_data;
                        combined.extend_from_slice(&body_data);
                        let combined_len = combined.len();

                        let write_result = timeout(
                            Duration::from_secs(buffering_config.client_write_timeout_secs),
                            client_stream.write_all(combined),
                        )
                        .await;

                        if matches!(write_result, Ok((Ok(_), _))) {
                            total = combined_len as u64;
                        }
                    }
                    BufferedBodyResult::Disk { path, size } => {
                        // ヘッダーを先に送信（所有権を移動、clone 不要）
                        let headers_len = headers_data.len();
                        let write_result = timeout(
                            Duration::from_secs(buffering_config.client_write_timeout_secs),
                            client_stream.write_all(headers_data),
                        )
                        .await;

                        if !matches!(write_result, Ok((Ok(_), _))) {
                            let _ = crate::runtime::io::remove_file(&path).await;
                            return Some((status_code, 0, false, true));
                        }

                        total = headers_len as u64;

                        // ディスクから読み込んでクライアントに送信
                        match send_disk_buffer_to_client(
                            client_stream,
                            &path,
                            size,
                            buffering_config.client_write_timeout_secs,
                        )
                        .await
                        {
                            Some(sent) => {
                                total += sent;
                            }
                            None => {
                                let _ = crate::runtime::io::remove_file(&path).await;
                                return Some((status_code, total, false, true));
                            }
                        }
                        let _ = crate::runtime::io::remove_file(&path).await;
                    }
                    BufferedBodyResult::Failed => {
                        // ヘッダーのみ送信（所有権を移動、clone 不要）
                        let headers_len = headers_data.len();
                        let write_result = timeout(
                            Duration::from_secs(buffering_config.client_write_timeout_secs),
                            client_stream.write_all(headers_data),
                        )
                        .await;
                        if matches!(write_result, Ok((Ok(_), _))) {
                            total = headers_len as u64;
                        }
                        return Some((status_code, total, false, true));
                    }
                    // 溜めない応答は上で送り終えている（ここへは来ない）
                    BufferedBodyResult::Stream { .. } => {
                        return Some((status_code, total, false, true))
                    }
                    BufferedBodyResult::LimitExceeded => {
                        // 507 Insufficient Storage を送信
                        let err_buf = ERR_MSG_INSUFFICIENT_STORAGE.to_vec();
                        let _ = timeout(
                            Duration::from_secs(buffering_config.client_write_timeout_secs),
                            client_stream.write_all(err_buf),
                        )
                        .await;
                        // 507 エラー時は接続を閉じる (should_close = true, backend keep-alive = false)
                        return Some((507, 0, false, true));
                    }
                }
            } else {
                // buffer_headers = false: ヘッダーを先に送信し、ボディは別途送信
                // ヘッダー送信（所有権を移動、clone 不要）
                let headers_len = headers_data.len();
                let write_result = timeout(
                    Duration::from_secs(buffering_config.client_write_timeout_secs),
                    client_stream.write_all(headers_data),
                )
                .await;

                if !matches!(write_result, Ok((Ok(_), _))) {
                    // ディスクファイルがあればクリーンアップ（非同期削除）
                    if let BufferedBodyResult::Disk { ref path, .. } = body_result {
                        let _ = crate::runtime::io::remove_file(path).await;
                    }
                    return Some((status_code, 0, false, true));
                }

                total = headers_len as u64;

                // ボディ送信（メモリまたはディスクから）
                match body_result {
                    BufferedBodyResult::Memory(body_data) => {
                        if !body_data.is_empty() {
                            let body_len = body_data.len();
                            let write_result = timeout(
                                Duration::from_secs(buffering_config.client_write_timeout_secs),
                                client_stream.write_all(body_data),
                            )
                            .await;

                            if !matches!(write_result, Ok((Ok(_), _))) {
                                return Some((status_code, total, false, true));
                            }

                            total += body_len as u64;
                        }
                    }
                    BufferedBodyResult::Disk { path, size } => {
                        // ディスクから読み込んでクライアントに送信
                        match send_disk_buffer_to_client(
                            client_stream,
                            &path,
                            size,
                            buffering_config.client_write_timeout_secs,
                        )
                        .await
                        {
                            Some(sent) => {
                                total += sent;
                            }
                            None => {
                                let _ = crate::runtime::io::remove_file(&path).await;
                                return Some((status_code, total, false, true));
                            }
                        }
                        let _ = crate::runtime::io::remove_file(&path).await;
                    }
                    BufferedBodyResult::Failed => {
                        return Some((status_code, total, false, true));
                    }
                    // 溜めない応答は上で送り終えている（ここへは来ない）
                    BufferedBodyResult::Stream { .. } => {
                        return Some((status_code, total, false, true))
                    }
                    BufferedBodyResult::LimitExceeded => {
                        // すでにヘッダー送信済みのため、507を返すことはできないので接続を閉じる
                        // （上流の本文は読み残しがあるので上流接続も戻さない。B-244）
                        return Some((status_code, total, false, true));
                    }
                }
            }

            // B-262: 接続の終わりで区切る本文は長さを付けずに送っているので、クライアントの接続を閉じて
            // 本文の終わりを伝える（閉じないとクライアントは本文の続きを待ち続ける）
            Some((status_code, total, backend_wants_keep_alive, until_close))
        }
        None => None,
    }
}

/// HTTP/1.1 の応答ヘッダの `Content-Type`・`Content-Encoding` の値（F-285 の圧縮の判定）
#[cfg(feature = "wasm")]
fn h1_head_content_type_encoding(head: &[u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let mut storage = [httparse::EMPTY_HEADER; 64];
    let mut resp = httparse::Response::new(&mut storage);
    if resp.parse(head).is_err() {
        return (None, None);
    }
    let find = |n: &str| {
        resp.headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(n))
            .map(|h| h.value.to_vec())
    };
    (find("content-type"), find("content-encoding"))
}

/// 応答ヘッダ（`\r\n\r\n` で終わる）の終わりに `Content-Length` を足す。`Transfer-Encoding` があるときは足さない
/// （併記は区切りの食い違いになる。そのときは接続を閉じて本文の終わりを伝える）
fn add_content_length(headers_data: &mut Vec<u8>, len: u64) {
    if !headers_data.ends_with(b"\r\n\r\n") {
        return;
    }
    let has_te = headers_data
        .split(|&b| b == b'\n')
        .any(|line| line.len() >= 18 && line[..18].eq_ignore_ascii_case(b"transfer-encoding:"));
    if has_te {
        return;
    }
    headers_data.truncate(headers_data.len() - 2);
    headers_data.extend_from_slice(b"Content-Length: ");
    let mut n = itoa::Buffer::new();
    headers_data.extend_from_slice(n.format(len).as_bytes());
    headers_data.extend_from_slice(b"\r\n\r\n");
}

/// バックエンドからレスポンスを受信してバッファリング
///
/// 戻り値: Option<(status_code, headers_data, body_result, backend_wants_keep_alive, until_close)>
///
/// `until_close` は上流が接続を閉じて本文を終えた応答か（長さの無いままクライアントへ送るので、送った後に閉じる）。
pub(super) async fn receive_and_buffer_response<R>(
    backend_stream: &mut R,
    buffering_config: &buffering::BufferingConfig,
    mut cache_ctx: Option<&mut CacheSaveContext>,
    is_head: bool,
    // `mode = "adaptive"` で閾値を超える・長さの分からない応答を溜めずに返してよいか（B-296）
    allow_stream: bool,
) -> Option<(u16, Vec<u8>, BufferedBodyResult, bool, bool, bool)>
where
    R: AsyncReadRent + Unpin,
{
    let mut accumulated = Vec::with_capacity(BUF_SIZE);

    // ヘッダー読み取り
    loop {
        let read_buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;

        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => return None,
        };

        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                return None;
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                return None;
            }
        };

        returned_buf.set_valid_len(n);
        accumulated.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);

        // B-11: バックエンド由来の 1xx 中間応答を読み捨てる。
        drain_interim_responses(&mut accumulated);

        // ヘッダーが完全に受信されたかチェック
        if let Some(parsed) = parse_http_response(&accumulated) {
            // B-246: Content-Length の食い違い・Transfer-Encoding との併記は本文の区切りが決まらない（502）
            if parsed.framing_invalid {
                warn!("Backend response has conflicting Content-Length / Transfer-Encoding");
                return None;
            }
            let status_code = parsed.status_code;

            let header_len = parsed.header_len;
            let body_start = accumulated[header_len..].to_vec();
            let headers_data = accumulated[..header_len].to_vec();

            // キャッシュコンテキストにヘッダーを設定
            if let Some(ref mut ctx) = cache_ctx {
                let mut headers_storage = [httparse::EMPTY_HEADER; 64];
                let mut response = httparse::Response::new(&mut headers_storage);
                if response.parse(&headers_data).is_ok() {
                    let headers: Vec<(Box<[u8]>, Box<[u8]>)> = response
                        .headers
                        .iter()
                        .map(|h| (h.name.as_bytes().into(), h.value.into()))
                        .collect();
                    ctx.set_headers(headers, status_code);
                }
            }

            // ボディをバッファリング
            // B-245: HEAD・1xx・204・304 は本文を持たない。B-246: TE は CL より優先
            let kind = parsed.body(is_head);
            let until_close = matches!(kind, ResponseBody::UntilClose);
            let chunked = matches!(kind, ResponseBody::Chunked);
            // B-296: adaptive は**応答の** Content-Length で決める（以前は要求の長さを見ていたので GET は常に溜めた）。
            // 閾値以下の長さ・本文の無い応答だけ溜め、超える・chunked・長さ不明は流す
            if allow_stream && buffering_config.mode == crate::buffering::BufferingMode::Adaptive {
                let small = match kind {
                    ResponseBody::None => true,
                    ResponseBody::Length(len) => len <= buffering_config.adaptive_threshold,
                    ResponseBody::Chunked | ResponseBody::UntilClose => false,
                };
                if !small {
                    return Some((
                        status_code,
                        headers_data,
                        BufferedBodyResult::Stream {
                            kind,
                            initial: body_start,
                        },
                        !parsed.is_connection_close,
                        until_close,
                        chunked,
                    ));
                }
            }
            let (body_result, reusable) = buffer_response_body_with_spillover(
                backend_stream,
                kind,
                body_start,
                buffering_config,
                cache_ctx,
            )
            .await;
            // B-244: 本文を区切りどおりに読み切った接続だけを戻す
            let backend_wants_keep_alive = !parsed.is_connection_close && reusable;

            return Some((
                status_code,
                headers_data,
                body_result,
                backend_wants_keep_alive,
                until_close,
                chunked,
            ));
        }

        // ヘッダーが大きすぎる場合は中止
        // B-234: ヘッダが 65 個以上・不正な応答は待たずに失敗
        if accumulated.len() > MAX_HEADER_SIZE
            || crate::http_utils::response_head_invalid(&accumulated)
        {
            return None;
        }
    }
}

/// ディスクバッファをクライアントに送信
pub(super) async fn send_disk_buffer_to_client(
    client_stream: &mut ServerTls,
    path: &std::path::Path,
    size: u64,
    timeout_secs: u64,
) -> Option<u64> {
    // whole-file 読み込みはイベントループをブロックするためオフロードする（F-29）。
    let path_owned = path.to_path_buf();
    // 理由付き allow: 同期 FS は offload 閉包内（専用ワーカースレッド）で実行され、イベントループを塞がない。
    #[allow(clippy::disallowed_methods)]
    let data = match crate::runtime::offload::offload(move || std::fs::read(path_owned)).await {
        Ok(d) => d,
        Err(e) => {
            error!("Failed to read disk buffer: {}", e);
            return None;
        }
    };

    // クライアントに送信
    let write_result = timeout(
        Duration::from_secs(timeout_secs),
        client_stream.write_all(data),
    )
    .await;

    match write_result {
        Ok((Ok(_), _)) => Some(size),
        _ => None,
    }
}

/// レスポンスボディをバッファリング（ディスクスピルオーバー対応）
///
/// 本文を区切り（`kind`）どおりに読み切ったときだけ `Memory`・`Disk` を返し、キャッシュに保存する。
/// 途中で止まった・区切りが不正な本文は `Failed`（クライアントへは未送信なので 502 にする）、
/// 上限を超えた本文は `LimitExceeded`（以前はディスク無しのとき切り詰めた本文を元の Content-Length で送っていた）。
/// 2 つ目の戻り値は上流接続を再利用してよいか（終端の後ろにバイトが無く、接続の終わりで区切る応答でない）。
pub(super) async fn buffer_response_body_with_spillover<R>(
    backend_stream: &mut R,
    kind: ResponseBody,
    initial_body: Vec<u8>,
    buffering_config: &buffering::BufferingConfig,
    cache_ctx: Option<&mut CacheSaveContext>,
) -> (BufferedBodyResult, bool)
where
    R: AsyncReadRent + Unpin,
{
    let mut body = initial_body;
    let start = response_body_start(kind, &body);
    body.truncate(start.send);
    let mut extra = start.extra;
    let mut complete = start.done;
    let mut decoder = start.into_decoder();
    let mut remaining = match kind {
        ResponseBody::Length(cl) => cl.saturating_sub(body.len()),
        _ => 0,
    };
    // ディスクがあればディスクの上限、無ければメモリの上限まで溜められる
    let limit = if buffering_config.disk_buffer_path.is_some() {
        buffering_config.max_disk_buffer
    } else {
        buffering_config.max_memory_buffer
    };

    debug!(
        "buffer_response_body_with_spillover: kind={:?}, initial_body_len={}, max_mem={}, max_disk={}",
        kind,
        body.len(),
        buffering_config.max_memory_buffer,
        buffering_config.max_disk_buffer
    );

    if let ResponseBody::Length(cl) = kind {
        if cl > limit {
            ftlog::warn!(
                "Response size {} exceeds buffer limit {}, aborting buffer",
                cl,
                limit
            );
            return (BufferedBodyResult::LimitExceeded, false);
        }
    }

    while !complete {
        let read_buf = buf_get();
        let (res, mut returned_buf) =
            match timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await {
                Ok(result) => result,
                // B-244: 上流が止まった本文は途中（以前は読めた分を完全な本文として送り、キャッシュしていた）
                Err(_) => return (BufferedBodyResult::Failed, false),
            };
        let n = match res {
            Ok(n) if n > 0 => n,
            res => {
                buf_put(returned_buf);
                // 接続の終わりで区切る応答だけは EOF が終端。TLS は close_notify を受けたときだけ（F-282）
                if res.is_ok()
                    && matches!(kind, ResponseBody::UntilClose)
                    && backend_stream.eof_was_clean()
                {
                    break;
                }
                return (BufferedBodyResult::Failed, false);
            }
        };
        returned_buf.set_valid_len(n);
        let chunk = returned_buf.as_valid_slice();
        let take = match kind {
            ResponseBody::Length(_) => {
                let take = n.min(remaining);
                remaining -= take;
                complete = remaining == 0;
                take
            }
            ResponseBody::Chunked => {
                let (fr, consumed) = decoder.feed_counted(chunk);
                match fr {
                    ChunkedFeedResult::Complete => complete = true,
                    ChunkedFeedResult::Continue => {}
                    ChunkedFeedResult::Invalid | ChunkedFeedResult::SizeLimitExceeded => {
                        buf_put(returned_buf);
                        return (BufferedBodyResult::Failed, false);
                    }
                }
                consumed
            }
            ResponseBody::UntilClose => n,
            ResponseBody::None => 0,
        };
        extra |= take < n;
        if body.len() + take > limit {
            buf_put(returned_buf);
            ftlog::warn!("Response exceeds buffer limit {}, aborting buffer", limit);
            return (BufferedBodyResult::LimitExceeded, false);
        }
        body.extend_from_slice(&chunk[..take]);
        buf_put(returned_buf);
    }

    let reusable = !extra && !matches!(kind, ResponseBody::UntilClose);

    // キャッシュに保存（読み切った本文だけ）
    if let Some(ctx) = cache_ctx {
        ctx.append_body(&body);
        ctx.save_to_cache();
    }

    if body.len() > buffering_config.max_memory_buffer {
        if let Some(ref disk_path) = buffering_config.disk_buffer_path {
            debug!(
                "Response size {} exceeds memory limit {}, spilling to disk",
                body.len(),
                buffering_config.max_memory_buffer
            );
            let key = buffering::disk_buffer::unique_key();
            let size = body.len() as u64;
            return match buffering::disk_buffer::write_to_disk(disk_path, key.as_bytes(), body)
                .await
            {
                Ok(path) => (BufferedBodyResult::Disk { path, size }, reusable),
                Err(e) => {
                    error!("Failed to write disk buffer: {}", e);
                    (BufferedBodyResult::Failed, false)
                }
            };
        }
    }

    (BufferedBodyResult::Memory(body), reusable)
}

#[cfg(test)]
mod tests {
    use super::add_content_length;

    /// B-262: 接続の終わりで区切る応答に長さを足す（Transfer-Encoding があるときは足さない）
    #[test]
    fn add_content_length_to_close_delimited_head() {
        let mut h =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n".to_vec();
        add_content_length(&mut h, 23);
        assert_eq!(
            h,
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\nContent-Length: 23\r\n\r\n"
        );

        let te = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n".to_vec();
        let mut h = te.clone();
        add_content_length(&mut h, 5);
        assert_eq!(h, te);
    }

    /// 決まった応答を 1 回で返す上流
    struct OneShot(Option<&'static [u8]>);

    impl crate::runtime::io::AsyncReadRent for OneShot {
        async fn read<T: crate::runtime::buf::IoBufMut>(
            &mut self,
            mut buf: T,
        ) -> crate::runtime::io::BufResult<usize, T> {
            let Some(c) = self.0.take() else {
                return (Ok(0), buf);
            };
            let n = c.len().min(buf.bytes_total());
            // SAFETY: write_ptr は bytes_total バイト書ける。n バイトだけ書いて初期化済みにする
            unsafe {
                std::ptr::copy_nonoverlapping(c.as_ptr(), buf.write_ptr(), n);
                buf.set_init(n);
            }
            (Ok(n), buf)
        }
    }

    #[cfg(all(veil_rt_uring, target_os = "linux"))]
    fn runtime_available() -> bool {
        crate::runtime::ring::IoUring::new(8, 0).is_ok()
    }

    #[cfg(not(all(veil_rt_uring, target_os = "linux")))]
    fn runtime_available() -> bool {
        true
    }

    /// B-296: adaptive は応答の Content-Length で決める（閾値以下は溜め、超える・chunked は流す）。
    /// Wasm の応答本文のフェーズがあるとき（allow_stream = false）は常に溜める
    #[test]
    fn adaptive_decides_on_the_response_length() {
        use super::{receive_and_buffer_response, BufferedBodyResult};
        use crate::buffering::{BufferingConfig, BufferingMode};
        if !runtime_available() {
            return;
        }
        let cfg = BufferingConfig {
            mode: BufferingMode::Adaptive,
            adaptive_threshold: 8,
            ..Default::default()
        };
        crate::runtime::block_on(async move {
            let cases: [(&'static [u8], bool, bool); 4] = [
                (
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello",
                    true,
                    false,
                ),
                (
                    b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nhello world",
                    true,
                    true,
                ),
                (
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
                    true,
                    true,
                ),
                (
                    b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nhello world",
                    false,
                    false,
                ),
            ];
            for (resp, allow_stream, streamed) in cases {
                let mut up = OneShot(Some(resp));
                let (status, _, body, ..) =
                    receive_and_buffer_response(&mut up, &cfg, None, false, allow_stream)
                        .await
                        .expect("response");
                assert_eq!(status, 200);
                assert_eq!(
                    matches!(body, BufferedBodyResult::Stream { .. }),
                    streamed,
                    "{}",
                    String::from_utf8_lossy(resp)
                );
            }
        });
    }
}
