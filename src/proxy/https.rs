//! HTTPS の上流のプロキシ（接続プール）
//!
//! F-233 で `src/proxy.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// HTTPS プロキシ（コネクションプール対応）
// ====================

/// バックエンドへの新規 HTTPS（TCP+TLS）接続を確立する。
///
/// 成功時は確立済みの `ClientTls` を返す。失敗時は `(ステータスコード, クライアントへ
/// 返すエラーメッセージ)` を返す（接続エラー=502 / タイムアウト=504）。クライアントへの
/// 書き込みは呼び出し側が行う。
pub(super) async fn connect_https_backend_fresh(
    target: &ProxyTarget,
    connect_timeout: Duration,
    tls_profile: crate::upstream_tls::TlsProfile,
) -> Result<ClientTls, (u16, &'static [u8])> {
    let addr = target.conn_addr(); // F-41/F-170: スタック上に構築（ヒープ確保なし、UDS 対応。TCP は従来と不変）
    let addr = addr.as_str();
    let backend_tcp = match timeout(connect_timeout, connect_target(target, addr)).await {
        Ok(Ok(stream)) => {
            let _ = stream.set_nodelay(true);
            stream
        }
        Ok(Err(e)) => {
            error!("Proxy connect error to {}: {}", addr, e);
            return Err((502, ERR_MSG_BAD_GATEWAY));
        }
        Err(_) => {
            error!("Proxy connect timeout to {}", addr);
            return Err((504, ERR_MSG_GATEWAY_TIMEOUT));
        }
    };

    // TLS接続（タイムアウト付き）
    // SNI名を使用（sni_nameが設定されていればそれを使用、なければhostを使用）
    // 上流の TLS の種類（検証しない・上流の mTLS。F-217）に合ったコネクタで張る
    let sni = target.sni();
    let tls_result = {
        let connector = crate::config::tls_connector(tls_profile);
        timeout(connect_timeout, connector.connect(backend_tcp, sni)).await
    };

    match tls_result {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(e)) => {
            error!("TLS connect error to {} (SNI: {}): {}", target.host, sni, e);
            Err((502, ERR_MSG_BAD_GATEWAY))
        }
        Err(_) => {
            error!("TLS connect timeout to {} (SNI: {})", target.host, sni);
            Err((504, ERR_MSG_GATEWAY_TIMEOUT))
        }
    }
}

pub(super) async fn proxy_https_pooled(
    mut client_stream: ServerTls,
    target: &ProxyTarget,
    security: &SecurityConfig,
    compression: &CompressionConfig,
    buffering_config: &buffering::BufferingConfig,
    client_encoding: AcceptedEncoding,
    pool_key: &str,
    request: Vec<u8>,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
    client_wants_close: bool,
    tls_profile: crate::upstream_tls::TlsProfile,
    wasm_modules: crate::wasm_plugin_config::WasmModules,
) -> Option<(ServerTls, u16, u64, bool)> {
    // セキュリティ設定からタイムアウトを取得
    let connect_timeout = Duration::from_secs(security.backend_connect_timeout_secs);
    // セキュリティ設定からchunked最大サイズを取得
    let max_chunked = security.max_chunked_body_size as u64;
    // バッファリングが有効かどうか判定
    // F-97: gRPC は Full バッファをバイパス
    let is_grpc_req = request_bytes_indicate_grpc(&request);
    let buffering_enabled = !is_grpc_req
        && buffering_config.is_enabled()
        && buffering_config.should_buffer(Some(content_length));

    // プールから取り出した keep-alive 接続は、バックエンド側の idle タイムアウト等で
    // 既に閉じられていることがある。その場合バックエンドからの最初の read が即座に EOF を
    // 返し、クライアントへ何も送信されないまま 502 相当で終わってしまう（負荷時に顕在化する
    // 「応答が空になる」フレーキーの原因）。クライアントへ未送信であれば、新規接続で透過的に
    // 一度だけリトライする。
    //
    // リトライが安全なのは「リクエストボディがクライアントストリームから未読でない」場合に
    // 限る（ボディ全体が initial_body 内にあり、chunked でない）。ボディをストリーム転送する
    // リクエストは再送できないためリトライしない。
    // F-177: 冪等なメソッドに限る（POST 等は上流が処理済みの可能性があるので再送しない）。
    let replayable =
        !is_chunked && content_length <= initial_body.len() && request_line_is_idempotent(&request);
    // request / wasm_modules はリトライ時に再利用するため、リトライ可能な場合のみ複製を保持する。
    // （非リトライ要求では複製せず move するためホットパスに余分な割り当てを足さない）
    let mut request_holder = Some(request);

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        // 接続取得: 初回はプール優先。リトライ時は必ず新規接続。
        let pooled = if attempt == 1 {
            HTTPS_POOL.with(|p| p.borrow_mut().get(pool_key))
        } else {
            None
        };
        let (mut backend_stream, from_pool) = match pooled {
            Some(stream) => (stream, true),
            None => match connect_https_backend_fresh(target, connect_timeout, tls_profile).await {
                Ok(stream) => (stream, false),
                Err((code, msg)) => {
                    let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(msg.to_vec())).await;
                    return Some((client_stream, code, 0, true));
                }
            },
        };

        // リトライ可能要求の 1 回目は複製を渡し（次の試行のため原本を保持）、それ以外は move する。
        // 複製は要求バッファのプールから取る（要求ごとのヒープ確保にしない）。
        let req = match request_holder.as_deref() {
            Some(original) if replayable && attempt == 1 => {
                let mut copy = request_buf_get(original.len());
                copy.extend_from_slice(original);
                copy
            }
            _ => request_holder.take().unwrap_or_default(),
        };
        // wasm_modules は通常空（割り当てなし）のため毎試行クローンしても実質コストは無い。
        let wasm_mods = wasm_modules.clone();

        let result = if buffering_enabled
            && (!compression.enabled || client_encoding == AcceptedEncoding::Identity)
        {
            let host_str_for_metrics = &target.host;
            record_buffering_used(host_str_for_metrics);
            proxy_request_buffered(
                &mut client_stream,
                &mut backend_stream,
                req,
                content_length,
                is_chunked,
                initial_body,
                max_chunked,
                buffering_config,
                None,
                security,
            )
            .await
        } else {
            // リクエスト送信とレスポンス受信（圧縮対応）
            proxy_https_request_with_compression(
                &mut client_stream,
                &mut backend_stream,
                req,
                content_length,
                is_chunked,
                initial_body,
                max_chunked,
                compression,
                client_encoding,
                security,
                wasm_mods,
            )
            .await
        };

        match result {
            Some((status_code, total, backend_wants_keep_alive, client_must_close)) => {
                // プールから取り出した接続が応答前に死んでいた（total==0 かつ status は初期値 502 = レスポンス未受信）。
                // クライアントへ未送信のため、新規接続で一度だけ透過リトライ。死んだ接続はプールに戻さない。
                if from_pool && total == 0 && status_code == 502 && replayable && attempt < 2 {
                    continue;
                }
                // B-17: クライアントへ 1 バイトも送らないままバックエンド異常で終わった場合、
                // エラーページ（502/504）を即時送出してクローズする
                if total == 0 && status_code >= 500 {
                    let err_buf = if status_code == 504 {
                        ERR_MSG_GATEWAY_TIMEOUT.to_vec()
                    } else {
                        ERR_MSG_BAD_GATEWAY.to_vec()
                    };
                    let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                    return Some((client_stream, status_code, 0, true));
                }
                // バックエンドがKeep-Aliveを許可している場合、プールに返却
                if backend_wants_keep_alive {
                    let max_idle = security.max_idle_connections_per_host;
                    let idle_timeout = security.idle_connection_timeout_secs;
                    HTTPS_POOL.with(|p| {
                        p.borrow_mut()
                            .put(pool_key, backend_stream, max_idle, idle_timeout)
                    });
                }
                // 408 (body timeout) sends Connection: close — must actually close
                // B-17: 上流異常でボディが完結しなかった場合もクローズする
                let should_close = client_wants_close || status_code == 408 || client_must_close;
                return Some((client_stream, status_code, total, should_close));
            }
            None => {
                // プール接続でのエラーかつクライアントへ未送信なら新規接続でリトライ
                if from_pool && replayable && attempt < 2 {
                    continue;
                }
                // エラー発生時は接続を破棄
                let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                return Some((client_stream, 502, 0, true));
            }
        }
    }
}

/// HTTPSリクエストを送信してレスポンスを受信（圧縮対応版）
/// 戻り値: Option<(status_code, response_size, backend_wants_keep_alive)>
pub(super) async fn proxy_https_request_with_compression(
    client_stream: &mut ServerTls,
    backend_stream: &mut ClientTls,
    request: Vec<u8>,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
    max_chunked_body_size: u64,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    security: &SecurityConfig,
    wasm_modules: crate::wasm_plugin_config::WasmModules,
) -> Option<(u16, u64, bool, bool)> {
    // HEAD の応答は本文を持たない（B-245）
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
            ChunkedTransferResult::SizeLimitExceeded => {
                return None;
            }
        }
    } else {
        let remaining = content_length.saturating_sub(initial_body.len());
        if remaining > 0 {
            let body_timeout = Duration::from_secs(security.client_body_timeout_secs);
            let timed_out = futures::select_biased! {
                _ = futures::FutureExt::fuse(crate::runtime::time::sleep(body_timeout)) => {
                    true
                }
                transferred = futures::FutureExt::fuse(transfer_exact_bytes(client_stream, backend_stream, remaining)) => {
                    if transferred < remaining as u64 { return None; }
                    false
                }
            };
            if timed_out {
                let _ = client_stream
                    .write_all(ERR_MSG_REQUEST_TIMEOUT.to_vec())
                    .await;
                return Some((408, 0, false, true));
            }
        }
    }

    // 4. レスポンスを受信して転送（圧縮対応）
    let (total, status_code, backend_wants_keep_alive, client_must_close) =
        transfer_https_response_with_compression(
            backend_stream,
            client_stream,
            compression,
            client_encoding,
            security,
            wasm_modules,
            is_head,
        )
        .await;

    Some((
        status_code,
        total,
        backend_wants_keep_alive,
        client_must_close,
    ))
}

/// HTTPSレスポンス転送（圧縮対応版）
#[cfg_attr(not(feature = "wasm"), allow(unused_variables))]
pub(super) async fn transfer_https_response_with_compression(
    backend_stream: &mut ClientTls,
    client_stream: &mut ServerTls,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    security: &SecurityConfig,
    wasm_modules: crate::wasm_plugin_config::WasmModules,
    is_head: bool,
) -> (u64, u16, bool, bool) {
    let mut accumulated = Vec::with_capacity(BUF_SIZE);
    let mut total = 0u64;
    let mut status_code = 502u16;
    // 初期値false: エラー時はKeep-Aliveを無効化
    let mut backend_wants_keep_alive = false;

    // ヘッダー読み取り用バッファ
    loop {
        // B-17: ヘッダー読取は専用の短いタイムアウトで打ち切り、504 へ即変換する
        let read_buf = buf_get();
        let read_result = timeout(BACKEND_HEADER_TIMEOUT, backend_stream.read(read_buf)).await;

        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => {
                warn!("Backend response timeout while reading headers");
                return (total, 504, false, true);
            }
        };

        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                warn!("Backend closed connection without sending response (read returned 0 bytes)");
                return (total, status_code, backend_wants_keep_alive, true);
            }
            Ok(n) => n,
            Err(e) => {
                buf_put(returned_buf);
                // kTLS使用時はEIO (os error 5) が発生することがある
                // これはバックエンドがTLS close_notifyを送信せずに接続を閉じた場合に発生
                if is_connection_closed_error(&e) {
                    debug!(
                        "Backend closed connection (kTLS EIO or connection reset): {}",
                        e
                    );
                } else {
                    warn!("Backend read error: {}", e);
                }
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
            // B-246: 本文の長さを決められない応答は中継しない（応答の分割になる）
            if parsed.framing_invalid {
                warn!("Backend response has conflicting Content-Length / Transfer-Encoding");
                return (0, 502, false, true);
            }
            status_code = parsed.status_code;
            let body_kind = parsed.body(is_head);
            backend_wants_keep_alive = !parsed.is_connection_close;

            let header_len = parsed.header_len;
            let body_start = &accumulated[header_len..];

            // Content-Type と Content-Encoding を取得
            let content_type = extract_header_value(&accumulated[..header_len], b"content-type");
            let existing_encoding =
                extract_header_value(&accumulated[..header_len], b"content-encoding");

            // 圧縮すべきか判定
            // 本文の無い応答（HEAD・204・304）は圧縮しない（本文を待たない。B-245）
            let should_compress = if body_kind == ResponseBody::None {
                None
            } else {
                compression.should_compress(
                    client_encoding,
                    content_type,
                    parsed.content_length,
                    existing_encoding,
                )
            };

            if let Some(encoding) = should_compress {
                // 圧縮有効: ヘッダーを書き換えて圧縮転送
                return match transfer_compressed_response(
                    client_stream,
                    backend_stream,
                    &accumulated[..header_len],
                    body_start,
                    body_kind,
                    encoding,
                    compression,
                    backend_wants_keep_alive,
                    security,
                )
                .await
                {
                    Some((total, reusable)) => (total, status_code, reusable, false),
                    // 本文が途中で止まった・クライアントへ書けない（B-244）
                    None => (0, 502, false, true),
                };
            } else {
                // 圧縮無効: そのまま転送（ヘッダー追加処理）
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
                        new_header_lines
                            .push(format!("{}: {}\r\n", header_name, header_value).into_bytes());
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
                                let current_headers: Vec<(Vec<u8>, Vec<u8>)> = new_header_lines
                                    .iter()
                                    .skip(1)
                                    .filter_map(|line| {
                                        let line_str = std::str::from_utf8(line).ok()?;
                                        let line_trimmed = line_str.trim_end_matches("\r\n");
                                        if line_trimmed.is_empty() {
                                            return None;
                                        }
                                        let colon_pos = line_trimmed.find(':')?;
                                        let name = line_trimmed.as_bytes()[..colon_pos].to_vec();
                                        let value = line_trimmed[colon_pos + 1..]
                                            .trim_start()
                                            .as_bytes()
                                            .to_vec();
                                        Some((name, value))
                                    })
                                    .collect();

                                let wasm_result = wasm_engine
                                    .clone()
                                    .on_response_headers_with_modules_async(
                                        wasm_modules.clone(),
                                        status_code,
                                        current_headers,
                                        true,
                                    )
                                    .await;

                                if let crate::wasm::FilterResult::Continue {
                                    headers: modified_headers_wasm,
                                    ..
                                } = wasm_result
                                {
                                    new_header_lines.clear();
                                    let status_line = format!(
                                        "HTTP/1.1 {} {}\r\n",
                                        status_code,
                                        status_code_to_reason(status_code)
                                    );
                                    new_header_lines.push(status_line.into_bytes());
                                    for (name, value) in modified_headers_wasm {
                                        let mut line =
                                            Vec::with_capacity(name.len() + value.len() + 4);
                                        line.extend_from_slice(&name);
                                        line.extend_from_slice(b": ");
                                        line.extend_from_slice(&value);
                                        line.extend_from_slice(b"\r\n");
                                        new_header_lines.push(line);
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

                // `[route.traffic.cors]` の実際の応答のヘッダ（F-273。CORS の無いルートは分岐だけ）
                crate::route_cors::apply_h1(&mut modified_headers, security);
                // F-59: ヘッダー + 初期ボディを平文接続では 1 回の SENDMSG で送出
                // B-244: 最初の読み込みのうち本文の終端までだけを送る（後ろは次の応答であり得る）
                let start = response_body_start(body_kind, body_start);
                let body_data = body_start[..start.send].to_vec();
                let write_result = timeout(
                    WRITE_TIMEOUT,
                    client_stream.write_all_vectored(modified_headers, body_data),
                )
                .await;
                if !matches!(write_result, Ok((Ok(()), _, _))) {
                    return (total, status_code, false, true);
                }
                total += header_len as u64;
                total += start.send as u64;

                // 残りのボディを転送。区切りどおりに終わったときだけ上流接続を戻す（B-244）
                let outcome = transfer_response_body_rest(
                    backend_stream,
                    client_stream,
                    body_kind,
                    start,
                    None,
                )
                .await;
                total += outcome.total;
                if !outcome.complete {
                    // B-17: 区切りに満たないまま上流が止まった・終わった。クライアントは残りを待ち続けるので閉じて知らせる
                    warn!("Backend response body incomplete ({:?})", body_kind);
                }
                return (
                    total,
                    status_code,
                    backend_wants_keep_alive && outcome.reusable(body_kind),
                    outcome.client_must_close(body_kind),
                );
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
