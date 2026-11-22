//! プロキシ処理（HTTP の上流・接続プール・h2c の上流）
//!
//! F-233 で `src/proxy.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// プロキシ処理
// ====================
//
// バックエンドコネクションプールを使用して、接続を再利用します。
// Connection: keep-alive をバックエンドに送信し、レスポンスの
// Connection ヘッダーに基づいて接続をプールに返却します。
// ====================

pub(super) async fn handle_proxy(
    mut client_stream: ServerTls,
    upstream_group: &UpstreamGroup,
    security: &SecurityConfig,
    compression: &CompressionConfig,
    buffering_config: &buffering::BufferingConfig,
    cache_config: &Arc<cache::CacheConfig>,
    method: &[u8],
    req_path: &[u8],
    prefix: &[u8],
    content_length: usize,
    is_chunked: bool,
    headers: &[(Bytes, Bytes)],
    initial_body: &[u8],
    client_wants_close: bool,
    wasm_modules: crate::wasm_plugin_config::WasmModules,
    client_ip: &str,
) -> Option<(ServerTls, u16, u64, bool)> {
    // クライアントの Accept-Encoding を解析
    let client_encoding = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(b"accept-encoding"))
        .map(|(_, value)| crate::config::ClientAccept::parse(value))
        .unwrap_or(crate::config::ClientAccept::NONE);

    // ホスト名を取得（UTF-8 でない Host の要求はキャッシュを使わないので、キャッシュへは元のバイト列を渡す。F-257）
    let host_bytes: &[u8] = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(b"host"))
        .map_or(&b"unknown"[..], |(_, v)| &v[..]);
    let host_str = std::str::from_utf8(host_bytes).unwrap_or("unknown");

    // RFC 7230 Section 4.3: TE ヘッダーを解析
    // クライアントがtrailersをサポートしているかを判定
    let _client_supports_trailers = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(b"te"))
        .map(|(_, value)| parse_te_header(value).supports_trailers)
        .unwrap_or(false);

    let path_str = std::str::from_utf8(req_path).unwrap_or("/");

    // ===================
    // キャッシュ（引き当て・当たりの応答は HTTP/2・HTTP/3 と共有する。F-257）
    // ===================
    let mut cache_save_ctx: Option<Box<CacheSaveContext>> = match cache_lookup::lookup(
        cache_config,
        method,
        host_bytes,
        req_path,
        header_pairs(headers),
    ) {
        cache_lookup::CacheLookup::Bypass => None,
        cache_lookup::CacheLookup::Miss(ctx) => Some(ctx),
        cache_lookup::CacheLookup::Hit(hit) => {
            // stale-while-revalidate: 裏で取り直す（304 で返すときも。以前は 304 のときは取り直さなかった）
            if hit.stale {
                spawn_background_revalidation(
                    &hit.key,
                    cache_config,
                    upstream_group,
                    security,
                    method,
                    req_path,
                    prefix,
                    header_pairs(headers),
                    client_ip,
                );
            }
            match cache_lookup::hit_body(&hit).await {
                Some(body) => {
                    let written = cache_lookup::write_h1_hit(
                        &mut client_stream,
                        &hit,
                        body,
                        security,
                        client_wants_close,
                    )
                    .await;
                    return written
                        .map(|(status, len)| (client_stream, status, len, client_wants_close));
                }
                // ディスクから読めなかった: エントリを消して上流から取り直す
                None => Some(cache_lookup::refill(
                    cache_config,
                    hit,
                    host_bytes,
                    header_pairs(headers),
                )),
            }
        }
    };

    // バッファリングモードのログ出力（デバッグ用）
    if buffering_config.is_enabled() {
        debug!(
            "Buffering enabled for {} {} (mode={:?})",
            host_str, path_str, buffering_config.mode
        );
    }

    // ロードバランシング: UpstreamGroup からサーバーを選択
    // F-97: Consistent Hash の header:/cookie: をリクエストヘッダから解決
    let server = match upstream_group.select_with_header_fn(client_ip, |name| {
        headers
            .iter()
            .find(|(n, _)| n.as_ref().eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_ref())
    }) {
        Some(s) => s,
        None => {
            // 利用可能なサーバーがない
            crate::metrics::record_upstream_connect_error(
                crate::metrics::UpstreamConnectError::NoHealthy,
            );
            crate::logging::log_throttled!(error, "No healthy upstream servers available");
            return write_gateway_error(client_stream, 502, cache_save_ctx.as_deref(), security)
                .await;
        }
    };

    // 接続カウンターを増加（Least Connections 用）
    server.acquire();

    // F-06: リクエスト結果記録用の開始時刻（記録は `UpstreamGroup::record_request`。B-272）
    // 粗い時計（起床時に読んだ時刻）を使う: 遅延は負荷分散・異常検知の平均にしか使わず、ミリ秒の精度で足りる。
    // 要求ごとに 2 回の `Instant::now()`（FreeBSD aarch64 のプロファイルで `__vdso_gettc` が上位）を減らす
    let resilience_start = crate::runtime::time::coarse_now();

    let target = &server.target;
    // コネクションプールキーの生成（F-170: 接続先表記＝conn_addr() を第 1 要素にする。
    // TCP では host:port と 1 バイトも変わらないため、プール再利用は不変）。
    // HTTPS: SNI と tls_profile 毎に別プール（B-30: 検証設定の異なる接続の再利用を防ぐ）
    let tls_profile = upstream_group.tls_profile();
    let pool_key_addr = target.conn_addr();
    let pool_key_addr = pool_key_addr.as_str();
    let pool_key = if target.use_tls && target.sni_name.is_some() {
        crate::http_utils::PoolKeyStr::tls_addr(pool_key_addr, target.sni(), tls_profile)
    } else if target.use_tls {
        crate::http_utils::PoolKeyStr::tls_addr_no_sni(pool_key_addr, tls_profile)
    } else {
        crate::http_utils::PoolKeyStr::plain_addr(pool_key_addr)
    };

    // リクエストパス構築
    // gRPC はフルパス保持（/* プレフィックス除去で UNIMPLEMENTED → B-40）
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    let preserve_grpc_path = headers.iter().any(|(n, v)| header_pair_is_grpc(n, v));
    let final_path_owned =
        compute_upstream_path(path_str, prefix, &target.path_prefix, preserve_grpc_path);
    let final_path = final_path_owned.as_str();

    // HTTPリクエスト構築（プール使用）
    // 定数バイト列を使用してアロケーションを削減
    let mut request = request_buf_get(1024);
    request.extend_from_slice(method);
    request.extend_from_slice(HEADER_SPACE);
    request.extend_from_slice(final_path.as_bytes());
    request.extend_from_slice(HEADER_HTTP11_HOST);
    request.extend_from_slice(target.host.as_bytes());

    if !target.is_default_port() {
        request.extend_from_slice(HEADER_PORT_COLON);
        let mut port_buf = itoa::Buffer::new();
        request.extend_from_slice(port_buf.format(target.port).as_bytes());
    }

    request.extend_from_slice(HEADER_CRLF);

    write_upstream_request_headers(
        &mut request,
        headers,
        security,
        client_ip,
        path_str,
        is_chunked,
    );

    // chunked リクエストはボディを chunked フレームのままバックエンドへ転送する。
    // 上のループで Transfer-Encoding を hop-by-hop ヘッダとして除去しているため、
    // chunked の場合はここで再付与しないと、バックエンドはボディ長を判別できず本文を
    // 読まないまま応答する。その結果、残った chunked フレーム
    // （例: `5\r\nhello\r\n0\r\n\r\n`）が keep-alive 接続上で次のリクエストとして
    // 解釈され、`400 Bad Request` の desync を引き起こす（負荷時に顕在化）。
    if is_chunked {
        request.extend_from_slice(b"Transfer-Encoding: chunked\r\n");
    }

    // バックエンドにはKeep-Aliveを要求
    request.extend_from_slice(HEADER_CONNECTION_KEEPALIVE_END);

    // F-175: HTTP/1.1 のクライアントは `http2 = "on"` のときだけ上流 HTTP/2 を使う。"auto" では
    // HTTP/1.1 のまま（クライアント接続ごとにプールの上流接続を持てるため多重化の利点が無く、
    // 多重化接続のアクターを経由するぶん 3B で -12%・54KB で -13.5% になった）。
    // 対象は本文を受信済みの要求（本文なし・受信済みの Content-Length）で、WASM・バッファリングの
    // 無いルート。上流が HTTP/1.1 を選んだら、クライアント接続を受け取り直して下の経路で続ける。
    #[cfg(feature = "http2")]
    let mut client_stream = if target.use_tls
        && target.http2 == crate::config::UpstreamHttp2::On
        && !is_chunked
        && content_length == initial_body.len()
        && wasm_modules.is_empty()
        && !buffering_config.is_enabled()
    {
        // F-236（F4）: HTTP/2 の上流にも [route.security] の要求ヘッダの方針を当てる
        let added = expanded_added_headers(
            security,
            client_ip,
            host_str.as_bytes(),
            std::str::from_utf8(req_path).unwrap_or("/"),
            ViaProto::Http11,
        );
        match proxy_h1_via_https_h2(
            client_stream,
            target,
            security,
            compression,
            client_encoding,
            method,
            final_path.as_bytes(),
            headers,
            initial_body,
            client_wants_close,
            tls_profile,
            &added,
            cache_save_ctx.as_deref_mut(),
        )
        .await
        {
            H1ViaH2::Done(r) => {
                server.release();
                // F-06: HTTP/1.1 経路と同じくサーキットブレーカー・異常検知へ反映する。
                let success = matches!(&r, Some((_, status, _, _)) if *status < 500);
                upstream_group.record_request(server, resilience_start, success);
                return r;
            }
            H1ViaH2::Http1(c) => c,
        }
    } else {
        client_stream
    };

    let result = if target.use_tls {
        // HTTPS接続（キャッシュの保存も平文と同じ。F-257）
        // 上流証明書検証は per-upstream の tls_profile のみで制御（B-30: VEIL_TLS_INSECURE はクライアント向け）
        proxy_https_pooled(
            client_stream,
            target,
            security,
            compression,
            buffering_config,
            client_encoding,
            pool_key.as_str(),
            request,
            content_length,
            is_chunked,
            initial_body,
            client_wants_close,
            tls_profile,
            wasm_modules,
            cache_save_ctx.as_deref_mut(),
        )
        .await
    } else if target.use_h2c || upstream_group.use_h2c() {
        // H2C (HTTP/2 over cleartext) 接続
        #[cfg(feature = "http2")]
        {
            // gRPCメッセージサイズ制限チェック
            let is_grpc = headers.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case(b"content-type") && value.starts_with(b"application/grpc")
            });
            if is_grpc && content_length > MAX_GRPC_BODY_SIZE {
                let err_buf = ERR_MSG_REQUEST_TOO_LARGE.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                Some((client_stream, 413, 0, true))
            } else {
                // B-420: 上流の h2c へは本文の全量を送る。以前は見出しと一緒に読めた分だけを送って END_STREAM にし、
                // 残りの本文をクライアントの接続の次の要求として読んでいた（要求の密輸）。chunked はフレーミングごと送っていた
                let full_body;
                let mut client_wants_close = client_wants_close;
                let body: &[u8] = if is_chunked || content_length > initial_body.len() {
                    match read_h2c_request_body(
                        &mut client_stream,
                        security,
                        headers,
                        content_length,
                        is_chunked,
                        is_grpc,
                        initial_body,
                    )
                    .await
                    {
                        Ok((b, pipelined)) => {
                            // 本文の後ろまで読んだ分（次の要求）はこの経路から返せないので、応答の後に閉じる
                            client_wants_close |= pipelined;
                            full_body = b;
                            &full_body
                        }
                        Err(status) => {
                            let err: &[u8] = match status {
                                413 => ERR_MSG_REQUEST_TOO_LARGE,
                                _ => ERR_MSG_BAD_REQUEST,
                            };
                            let _ =
                                timeout(WRITE_TIMEOUT, client_stream.write_all(err.to_vec())).await;
                            return Some((client_stream, status, 0, true));
                        }
                    }
                } else {
                    initial_body
                };
                let added = expanded_added_headers(
                    security,
                    client_ip,
                    host_str.as_bytes(),
                    std::str::from_utf8(req_path).unwrap_or("/"),
                    ViaProto::Http11,
                );
                proxy_h2c(
                    client_stream,
                    target,
                    security,
                    compression,
                    client_encoding,
                    #[cfg(feature = "wasm")]
                    &wasm_modules,
                    method,
                    final_path.as_bytes(),
                    headers,
                    body,
                    client_wants_close,
                    &added,
                    cache_save_ctx.as_deref_mut(),
                )
                .await
            }
        }
        #[cfg(not(feature = "http2"))]
        {
            // HTTP/2 feature が無効な場合はHTTP/1.1にフォールバック
            warn!("H2C requested but http2 feature not enabled, falling back to HTTP/1.1");
            proxy_http_pooled(
                client_stream,
                target,
                security,
                compression,
                buffering_config,
                client_encoding,
                pool_key.as_str(),
                request,
                content_length,
                is_chunked,
                initial_body,
                client_wants_close,
                cache_save_ctx.as_deref_mut(),
                wasm_modules.clone(),
            )
            .await
        }
    } else {
        // HTTP接続（キャッシュ保存・バッファリング対応）
        proxy_http_pooled(
            client_stream,
            target,
            security,
            compression,
            buffering_config,
            client_encoding,
            pool_key.as_str(),
            request,
            content_length,
            is_chunked,
            initial_body,
            client_wants_close,
            cache_save_ctx.as_deref_mut(),
            wasm_modules,
        )
        .await
    };

    // 接続カウンターを減少（Least Connections 用）
    server.release();

    // F-06: リクエスト結果をサーキットブレーカー・異常検知へ反映（5xx をバックエンド障害として扱う）
    let success = match &result {
        Some((_, status, _, _)) => *status < 500,
        None => false,
    };
    upstream_group.record_request(server, resilience_start, success);

    // stale-if-error は応答を書く前に決める（`write_gateway_error`・`cache_lookup::gateway_error_stale`。B-492）
    result
}

/// 上流から応答を得られなかった要求（veil が 502・504 を返す。クライアントへはまだ何も書いていない）。stale-if-error の
/// ルートで期限切れの保存した応答があれば、エラーページの代わりにそれを返す（B-492）。要求の本文を読み切ったとは
/// 限らないので、どちらも返した後に閉じる。戻り値は [`handle_proxy`] と同じ形
pub(super) async fn write_gateway_error(
    mut client_stream: ServerTls,
    status: u16,
    cache_ctx: Option<&CacheSaveContext>,
    security: &SecurityConfig,
) -> Option<(ServerTls, u16, u64, bool)> {
    if let Some(hit) = cache_lookup::stale_fallback(cache_ctx) {
        match cache_lookup::write_h1_stale(&mut client_stream, hit, status, security, true).await {
            Some((len, st, _, false)) => return Some((client_stream, st, len, true)),
            // 書けなかった
            Some(_) => return None,
            // 本文を読めなかった: エラーを返す
            None => {}
        }
    }
    let err_buf = if status == 504 {
        ERR_MSG_GATEWAY_TIMEOUT.to_vec()
    } else {
        ERR_MSG_BAD_GATEWAY.to_vec()
    };
    let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
    Some((client_stream, status, 0, true))
}

// ====================
// HTTP プロキシ（コネクションプール対応）
// ====================

pub(super) async fn proxy_http_pooled(
    mut client_stream: ServerTls,
    target: &ProxyTarget,
    security: &SecurityConfig,
    compression: &CompressionConfig,
    buffering_config: &buffering::BufferingConfig,
    client_encoding: crate::config::ClientAccept,
    pool_key: &str,
    request: Vec<u8>,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
    client_wants_close: bool,
    cache_ctx: Option<&mut CacheSaveContext>,
    wasm_modules: crate::wasm_plugin_config::WasmModules,
) -> Option<(ServerTls, u16, u64, bool)> {
    // セキュリティ設定からタイムアウトを取得
    let connect_timeout = Duration::from_secs(security.backend_connect_timeout_secs);

    let is_grpc_req = request_bytes_indicate_grpc(&request);
    // F-177: 再送できるのは本文を受信し終えた冪等な要求だけ（本文をクライアントから流す要求は
    // 再送できない）。1 回目は要求の複製（要求バッファのプールから取る）を送り、原本を再送用に残す。
    let replayable =
        !is_chunked && content_length <= initial_body.len() && request_line_is_idempotent(&request);
    let mut request_holder = Some(request);
    let mut cache_ctx = cache_ctx;
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let req = match request_holder.as_deref() {
            Some(original) if replayable && attempt == 1 => {
                let mut copy = request_buf_get(original.len());
                copy.extend_from_slice(original);
                copy
            }
            _ => request_holder.take().unwrap_or_default(),
        };
        // プールから接続を取得、または新規作成
        let pooled = if attempt == 1 {
            HTTP_POOL.with(|p| p.borrow_mut().get(pool_key))
        } else {
            None
        };
        let from_pool = pooled.is_some();
        let mut backend_stream = match pooled {
            Some(stream) => stream,
            None => {
                // 新規接続を作成（事前解決済み SocketAddr があればブロッキング DNS を迂回）
                let addr = target.conn_addr(); // F-41/F-170: スタック上に構築（ヒープ確保なし、UDS 対応。TCP は従来と不変）
                let addr = addr.as_str();
                let connect_result = timeout(connect_timeout, connect_target(target, addr)).await;

                match connect_result {
                    Ok(Ok(stream)) => {
                        let _ = stream.set_nodelay(true);
                        stream
                    }
                    Ok(Err(e)) => {
                        crate::metrics::record_upstream_connect_error(
                            crate::metrics::UpstreamConnectError::Error,
                        );
                        crate::logging::log_throttled!(
                            error,
                            "Proxy connect error to {}: {}",
                            addr,
                            e
                        );
                        return write_gateway_error(
                            client_stream,
                            502,
                            cache_ctx.as_deref(),
                            security,
                        )
                        .await;
                    }
                    Err(_) => {
                        crate::metrics::record_upstream_connect_error(
                            crate::metrics::UpstreamConnectError::Timeout,
                        );
                        crate::logging::log_throttled!(error, "Proxy connect timeout to {}", addr);
                        return write_gateway_error(
                            client_stream,
                            504,
                            cache_ctx.as_deref(),
                            security,
                        )
                        .await;
                    }
                }
            }
        };

        // セキュリティ設定からchunked最大サイズを取得
        let max_chunked = security.max_chunked_body_size as u64;

        // 圧縮が有効かどうかの事前判定
        // 注意: 実際のContent-Typeはレスポンス受信後に判定するため、ここでは設定の有効/無効のみ確認
        let compression_enabled = compression.enabled && client_encoding.accepts_any();

        // メトリクス用ホスト名
        let host_str_for_metrics = &target.host;

        // バッファリングが有効かどうか判定
        // F-97: Content-Type: application/grpc は Full バッファをバイパス（リクエスト行に含む）
        let buffering_enabled = !is_grpc_req && buffering_config.is_enabled();
        // F-285: Wasm の応答本文のフェーズを受けるモジュールがあれば本文を全部溜める（buffering = "full" の経路）
        #[cfg(feature = "wasm")]
        let wasm_body = super::compress::wasm_wants_response_body(&wasm_modules);
        #[cfg(not(feature = "wasm"))]
        let wasm_body = false;

        // `buffering` の適用を数える（圧縮の経路を通っても本文は溜める。HTTP/2・HTTP/3 と同じ。B-273）
        // adaptive は応答の長さを見て溜めると決めたときに数える（proxy_request_buffered。B-296）
        if buffering_enabled && buffering_config.mode == crate::buffering::BufferingMode::Full {
            record_buffering_used(host_str_for_metrics);
        }

        // リクエスト送信とレスポンス受信
        // kTLS 有効時は splice(2) を使用してゼロコピー転送
        // ただし、圧縮有効、キャッシュ保存が必要、またはバッファリング有効な場合はkTLSを迂回
        #[cfg(veil_ktls)]
        let result = {
            // キャッシュ保存が必要かどうか
            // キャッシュ保存が必要な場合はsplice転送を使用できない（ユーザー空間でボディをキャプチャする必要がある）
            let cache_save_needed = cache_ctx.is_some();

            // kTLS + splice 版を試みる条件:
            // - kTLS有効
            // - Content-Length転送（非chunked）
            // - 圧縮無効
            // - キャッシュ保存不要
            // - バッファリング無効
            // - WASMモジュール未設定（WASM有効時はユーザー空間でレスポンスヘッダーを操作する必要がある）
            #[cfg(feature = "wasm")]
            let wasm_modules_active = !wasm_modules.is_empty();
            #[cfg(not(feature = "wasm"))]
            let wasm_modules_active = false;
            // 平文の接続も同じ splice の経路（ユーザー空間へ読んで書き直さない）
            if (client_stream.is_ktls_recv_enabled() || client_stream.is_plain())
                && !is_chunked
                && !compression_enabled
                && !cache_save_needed
                && !buffering_enabled
                && !wasm_modules_active
                // CORS のあるルートは応答のヘッダを足すのでユーザー空間の経路（F-273）
                && security.cors_actual.is_none()
            {
                let splice_result = try_splice_proxy(
                    &client_stream,
                    &backend_stream,
                    &req,
                    content_length,
                    is_chunked,
                    initial_body,
                    client_wants_close,
                    security,
                )
                .await;

                if splice_result.is_some() {
                    splice_result
                } else {
                    // splice 版が失敗した場合は通常版にフォールバック
                    proxy_http_request_with_compression(
                        &mut client_stream,
                        &mut backend_stream,
                        req,
                        content_length,
                        is_chunked,
                        initial_body,
                        max_chunked,
                        compression,
                        client_encoding,
                        cache_ctx.as_deref_mut(),
                        security,
                        wasm_modules.clone(),
                        client_wants_close,
                    )
                    .await
                }
            } else if wasm_body || (buffering_enabled && !compression_enabled) {
                // バッファリング有効時（圧縮無効の場合のみ）
                debug!(
                    "Calling proxy_request_buffered for {} {}",
                    target.host, target.port
                );
                proxy_request_buffered(
                    &mut client_stream,
                    &mut backend_stream,
                    req,
                    content_length,
                    is_chunked,
                    initial_body,
                    max_chunked,
                    buffering_config,
                    // F-285: Wasm で書き換える前の本文をキャッシュへ置かない（ヒットはフィルタを通らない）
                    if wasm_body {
                        None
                    } else {
                        cache_ctx.as_deref_mut()
                    },
                    security,
                    #[cfg(feature = "wasm")]
                    &wasm_modules,
                    #[cfg(feature = "wasm")]
                    wasm_body.then_some((compression, client_encoding)),
                    host_str_for_metrics,
                    client_wants_close,
                )
                .await
            } else {
                debug!("Calling proxy_http_request_with_compression for {} {} (buffering_enabled={}, compression_enabled={})",
                       target.host, target.port, buffering_enabled, compression_enabled);
                // kTLS が無効、Chunked、圧縮有効、キャッシュ保存が必要、またはバッファリング無効の場合は通常版を使用
                proxy_http_request_with_compression(
                    &mut client_stream,
                    &mut backend_stream,
                    req,
                    content_length,
                    is_chunked,
                    initial_body,
                    max_chunked,
                    compression,
                    client_encoding,
                    cache_ctx.as_deref_mut(),
                    security,
                    wasm_modules.clone(),
                    client_wants_close,
                )
                .await
            }
        };

        #[cfg(not(veil_ktls))]
        let result = if wasm_body || (buffering_enabled && !compression_enabled) {
            // バッファリング有効時（圧縮無効の場合のみ）
            proxy_request_buffered(
                &mut client_stream,
                &mut backend_stream,
                req,
                content_length,
                is_chunked,
                initial_body,
                max_chunked,
                buffering_config,
                // F-285: Wasm で書き換える前の本文をキャッシュへ置かない（ヒットはフィルタを通らない）
                if wasm_body {
                    None
                } else {
                    cache_ctx.as_deref_mut()
                },
                security,
                #[cfg(feature = "wasm")]
                &wasm_modules,
                #[cfg(feature = "wasm")]
                wasm_body.then_some((compression, client_encoding)),
                host_str_for_metrics,
                client_wants_close,
            )
            .await
        } else {
            proxy_http_request_with_compression(
                &mut client_stream,
                &mut backend_stream,
                req,
                content_length,
                is_chunked,
                initial_body,
                max_chunked,
                compression,
                client_encoding,
                cache_ctx.as_deref_mut(),
                security,
                wasm_modules.clone(),
                client_wants_close,
            )
            .await
        };

        // F-177: プールから取り出した接続が応答の前に切れていた（応答未受信の 502 / エラー）なら、
        // クライアントへ未送信のうちに新しい接続で 1 回だけ再送する（冪等・本文受信済みの要求のみ）。
        if from_pool && replayable && attempt == 1 && matches!(result, None | Some((502, 0, _, _)))
        {
            crate::metrics::record_retry(target.conn_addr().as_str(), "stale_connection");
            continue;
        }
        if let Some(original) = request_holder.take() {
            request_buf_put(original);
        }

        return match result {
            Some((status_code, total, backend_wants_keep_alive, client_must_close)) => {
                // B-17: クライアントへ 1 バイトも送らないままバックエンド異常で終わった場合、
                // エラーページ（502/504）を即時送出してクローズする（従来はクライアントが
                // 自身のタイムアウトまでハングしていた）
                if total == 0 && status_code >= 500 {
                    return write_gateway_error(
                        client_stream,
                        status_code,
                        cache_ctx.as_deref(),
                        security,
                    )
                    .await;
                }
                // バックエンドがKeep-Aliveを許可している場合、プールに返却
                if backend_wants_keep_alive {
                    let max_idle = security.max_idle_connections_per_host;
                    let idle_timeout = security.idle_connection_timeout_secs;
                    HTTP_POOL.with(|p| {
                        p.borrow_mut()
                            .put(pool_key, backend_stream, max_idle, idle_timeout)
                    });
                }
                // 408 (body timeout) sends Connection: close — must actually close
                // B-17: 上流異常でボディが完結しなかった場合もクローズする
                let should_close = client_wants_close || status_code == 408 || client_must_close;
                Some((client_stream, status_code, total, should_close))
            }
            None => {
                // エラー発生時は接続を破棄
                write_gateway_error(client_stream, 502, cache_ctx.as_deref(), security).await
            }
        };
    }
}

// ====================
// H2C プロキシ (HTTP/2 over cleartext)
// ====================
//
// HTTP/2 Prior Knowledge モードでバックエンドに接続し、
// リクエストを転送します。gRPCバックエンドへの接続に適しています。
// ====================

/// [`proxy_h1_via_https_h2`] の結果。
#[cfg(feature = "http2")]
pub(super) enum H1ViaH2 {
    /// HTTP/2 で中継した（戻り値は `proxy_https_pooled` と同じ形）。
    Done(Option<(ServerTls, u16, u64, bool)>),
    /// 上流が ALPN で HTTP/1.1 を選んだ。クライアント接続を返すので HTTP/1.1 経路で続ける
    /// （ネゴシエーションに使った上流接続は TLS プールへ入れてある）。
    Http1(ServerTls),
}

// B-414: 上流の content-length を写す前に確かめる
/// HTTP/2 の上流の応答の `content-length`。行が無ければ `Ok(None)`、すべての行が同じ 10 進の数なら
/// `Ok(Some(n))`、読めない値・食い違う値があれば `Err(())`（RFC 9113 §8.1.1 の不正な応答）。
#[cfg(feature = "http2")]
fn h2_upstream_content_length(headers: &[(Bytes, Bytes)]) -> Result<Option<usize>, ()> {
    let mut found = None;
    for (n, v) in headers {
        if !n.eq_ignore_ascii_case(b"content-length") {
            continue;
        }
        let digits = v.trim_ascii();
        if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
            return Err(());
        }
        let parsed: usize = std::str::from_utf8(digits)
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or(())?;
        match found {
            Some(prev) if prev != parsed => return Err(()),
            _ => found = Some(parsed),
        }
    }
    Ok(found)
}

/// HTTP/2 の上流（h2c・TLS の h2）の応答を HTTP/1.1 の head（空行まで）にする。
///
/// ステータス行は `HTTP/1.1`、ホップバイホップと `remove_response_headers` を落とし、`add_response_headers`・Alt-Svc を足し
/// （B-267・B-271）、Wasm の `on_http_response_headers`（B-268）・CORS（F-273）をかける。HTTP/1.1 の上流の
/// `render_response_head` と同じ順。`framing` は送り方で決まる `Content-Length`・`Transfer-Encoding` の行（空なら上流の
/// `content-length` をそのまま使う）。`extra` は head に並べる追加のヘッダ（h2c の gRPC の trailers）。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn render_h2_upstream_head(
    status: u16,
    headers: &[(Bytes, Bytes)],
    extra: &[(Bytes, Bytes)],
    framing: &[u8],
    security: &SecurityConfig,
    #[cfg(feature = "wasm")] wasm_modules: &crate::wasm_plugin_config::WasmModules,
    client_wants_close: bool,
) -> Result<Vec<u8>, super::sendfile::HeadLocal> {
    let mut head = Vec::with_capacity(256);
    head.extend_from_slice(b"HTTP/1.1 ");
    let mut sb = itoa::Buffer::new();
    head.extend_from_slice(sb.format(status).as_bytes());
    head.push(b' ');
    head.extend_from_slice(status_reason_phrase(status).as_bytes());
    head.extend_from_slice(b"\r\n");
    // `[server] server_header_enabled` なら自分の `Server` にする（B-271）。`remove_response_headers` に
    // `Server` があれば自分のものも付けない（HTTP/2・HTTP/3・静的配信と同じ）
    let removes = !security.remove_response_headers.is_empty();
    let drop_upstream_server = crate::pool::append_own_server_line(&mut head, security);
    for (n, v) in headers.iter().chain(extra.iter()) {
        if (drop_upstream_server && n.eq_ignore_ascii_case(b"server"))
            || n.eq_ignore_ascii_case(b"connection")
            || n.eq_ignore_ascii_case(b"keep-alive")
            || n.eq_ignore_ascii_case(b"transfer-encoding")
            || n.eq_ignore_ascii_case(b"upgrade")
            || n.eq_ignore_ascii_case(b"proxy-connection")
            || (!framing.is_empty() && n.eq_ignore_ascii_case(b"content-length"))
            || (removes && security.removes_response_header(n))
        {
            continue;
        }
        head.extend_from_slice(n);
        head.extend_from_slice(b": ");
        head.extend_from_slice(v);
        head.extend_from_slice(b"\r\n");
    }
    for (name, value) in &security.add_response_headers {
        head.extend_from_slice(name.as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(value.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    // F-94: HTTP/3 広告（Alt-Svc）
    crate::pool::append_own_alt_svc_line(&mut head, security);
    #[cfg(feature = "wasm")]
    let mut head = super::sendfile::h1_wasm_response_head(head, status, wasm_modules).await?;
    head.extend_from_slice(framing);
    head.extend_from_slice(if client_wants_close {
        b"Connection: close\r\n\r\n"
    } else {
        b"Connection: keep-alive\r\n\r\n"
    });
    // `[route.traffic.cors]` の実際の応答のヘッダ（F-273。CORS の無いルートは分岐だけ）
    crate::route_cors::apply_h1(&mut head, security);
    Ok(head)
}

/// F-175: HTTP/1.1 で受けた要求（本文は受信済み）を、ALPN で HTTP/2 を選んだ HTTPS 上流の
/// 多重化接続（F-174）へ中継する。応答は届いた順にクライアントへ書く（`Content-Length` が
/// 無ければ chunked。トレーラーは chunked のトレーラー部へ）。圧縮対象の応答だけは集めて圧縮する。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn proxy_h1_via_https_h2(
    mut client_stream: ServerTls,
    target: &ProxyTarget,
    security: &SecurityConfig,
    compression: &CompressionConfig,
    client_encoding: crate::config::ClientAccept,
    method: &[u8],
    path: &[u8],
    headers: &[(Bytes, Bytes)],
    request_body: &[u8],
    client_wants_close: bool,
    tls_profile: crate::upstream_tls::TlsProfile,
    added: &AddedHeaders,
    // キャッシュの外れの要求の応答を取り込む（圧縮しない応答だけ。F-257）
    mut cache_ctx: Option<&mut CacheSaveContext>,
) -> H1ViaH2 {
    use crate::http2::upstream_mux::{HttpsOpen, UpResp};
    let addr = target.conn_addr();
    let key = crate::http_utils::PoolKeyStr::tls_addr(addr.as_str(), target.sni(), tls_profile);
    let opened = http2::upstream_mux::open_https(
        target,
        key.as_str(),
        tls_profile,
        Duration::from_secs(security.backend_connect_timeout_secs),
        Duration::from_secs(security.idle_connection_timeout_secs),
        method,
        path,
        target.host.as_bytes(),
        headers
            .iter()
            .filter(|(k, _)| forward_allowed(security, k) && added.keeps(k))
            .map(|(k, v)| (k.as_ref(), v.as_ref()))
            .chain(added.iter()),
        request_body.is_empty(),
    )
    .await;
    let mut stream = match opened {
        Ok(HttpsOpen::H2(s)) => s,
        Ok(HttpsOpen::Http1(conn)) => {
            if let Some(conn) = conn {
                HTTPS_POOL.with(|p| {
                    p.borrow_mut().put(
                        key.as_str(),
                        *conn,
                        security.max_idle_connections_per_host,
                        security.idle_connection_timeout_secs,
                    )
                });
            }
            return H1ViaH2::Http1(client_stream);
        }
        Err(status) => {
            return H1ViaH2::Done(
                write_gateway_error(client_stream, status, cache_ctx.as_deref(), security).await,
            );
        }
    };
    if !request_body.is_empty() {
        if let Some(tx) = &stream.req_tx {
            let _ = tx.send(Bytes::copy_from_slice(request_body)).await;
        }
    }
    stream.finish_request();

    // クライアントへ何も書いていないときの 502・504（stale-if-error の保存した応答があればそれを返す。B-492）
    macro_rules! gateway_error {
        ($status:expr) => {
            return H1ViaH2::Done(
                write_gateway_error(client_stream, $status, cache_ctx.as_deref(), security).await,
            )
        };
    }
    // 最終応答の head を待つ。
    let (status, mut resp_headers) = match timeout(READ_TIMEOUT, stream.resp_rx.recv()).await {
        Ok(Some(UpResp::Head { status, headers })) => (status, headers),
        Err(_) => gateway_error!(504),
        _ => gateway_error!(502),
    };
    // stale-if-error: 上流の 502・504 は中継せずに保存した応答を返す（B-492）
    if let Some(hit) = super::cache_lookup::gateway_error_stale(status, cache_ctx.as_deref()) {
        if let Some((len, st, _, must_close)) = super::cache_lookup::write_h1_stale(
            &mut client_stream,
            hit,
            status,
            security,
            client_wants_close,
        )
        .await
        {
            return H1ViaH2::Done(Some((
                client_stream,
                st,
                len,
                client_wants_close || must_close,
            )));
        }
    }
    // B-417: メソッドは大文字小文字を区別する（`head` は HEAD ではない。HTTP/1.1 の経路の `request_is_head` と揃える）
    let no_body = method == b"HEAD" || status == 204 || status == 304;
    let header = |hs: &[(Bytes, Bytes)], name: &[u8]| {
        hs.iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let content_type = header(&resp_headers, b"content-type");
    let is_grpc = content_type
        .as_deref()
        .is_some_and(|ct| ct.starts_with(b"application/grpc"));
    // B-414: 上流の content-length は、すべての行が同じ 10 進の数のときだけ使う（食い違い・読めない値は 502。
    // 以前は最初の行だけを読み、ほかの行もそのままクライアントへ写していた）
    let content_length: Option<usize> = match h2_upstream_content_length(&resp_headers) {
        Ok(v) => v,
        Err(()) => gateway_error!(502),
    };
    let should_compress = if is_grpc || no_body {
        None
    } else {
        compression.should_compress(
            client_encoding,
            content_type.as_deref(),
            content_length,
            header(&resp_headers, b"content-encoding").as_deref(),
        )
    };

    if let Some(enc) = should_compress {
        // 圧縮して返す応答は保存しない（F-257）
        let mut body: Vec<u8> = Vec::new();
        loop {
            match timeout(READ_TIMEOUT, stream.resp_rx.recv()).await {
                Ok(None) => break,
                Ok(Some(UpResp::Data(b))) => {
                    // B-281: 溜める上限を超えたら打ち切る
                    if body.len() + b.len() > crate::pool::MAX_COLLECTED_RESPONSE_BODY {
                        gateway_error!(502);
                    }
                    body.extend_from_slice(&b)
                }
                Ok(Some(UpResp::Trailers(_))) | Ok(Some(UpResp::Head { .. })) => {}
                Ok(Some(UpResp::Reset { .. })) => gateway_error!(502),
                Err(_) => gateway_error!(504),
            }
        }
        let compressed = compress_body_h2(&body, enc, compression);
        resp_headers.retain(|(n, _)| {
            !n.eq_ignore_ascii_case(b"content-length")
                && !n.eq_ignore_ascii_case(b"content-encoding")
        });
        resp_headers.push((
            Bytes::from_static(b"content-encoding"),
            Bytes::from_static(enc.as_header_value()),
        ));
        resp_headers.push((
            Bytes::from_static(b"vary"),
            Bytes::from_static(b"Accept-Encoding"),
        ));
        let mut framing = Vec::with_capacity(32);
        framing.extend_from_slice(b"Content-Length: ");
        let mut lb = itoa::Buffer::new();
        framing.extend_from_slice(lb.format(compressed.len()).as_bytes());
        framing.extend_from_slice(b"\r\n");
        // この経路は Wasm モジュールの無いルートだけ（呼び出し側の条件）なのでローカルレスポンスは無い
        let Some(head) = render_h2_upstream_head(
            status,
            &resp_headers,
            &[],
            &framing,
            security,
            #[cfg(feature = "wasm")]
            &super::h1::no_wasm_modules(),
            client_wants_close,
        )
        .await
        .ok() else {
            return H1ViaH2::Done(None);
        };
        let sent = compressed.len() as u64;
        return match timeout(
            WRITE_TIMEOUT,
            client_stream.write_all_vectored(head, compressed),
        )
        .await
        {
            Ok((Ok(()), _, _)) => {
                H1ViaH2::Done(Some((client_stream, status, sent, client_wants_close)))
            }
            _ => H1ViaH2::Done(None),
        };
    }

    // 長さ既知なら自分の Content-Length の行、本文なしなら上流のまま、不明なら chunked で書く。
    // B-414: 長さ既知でも上流の行は写さない（検証した 1 行だけにする）
    let chunked = !no_body && content_length.is_none();
    let mut cl_line = [0u8; 40];
    let framing: &[u8] = match content_length {
        _ if chunked => b"Transfer-Encoding: chunked\r\n",
        Some(n) if !no_body => {
            let mut nb = itoa::Buffer::new();
            let digits = nb.format(n).as_bytes();
            let mut len = 0;
            for part in [&b"Content-Length: "[..], digits, b"\r\n"] {
                cl_line[len..len + part.len()].copy_from_slice(part);
                len += part.len();
            }
            &cl_line[..len]
        }
        _ => b"",
    };
    let Some(head) = render_h2_upstream_head(
        status,
        &resp_headers,
        &[],
        framing,
        security,
        #[cfg(feature = "wasm")]
        &super::h1::no_wasm_modules(),
        client_wants_close,
    )
    .await
    .ok() else {
        return H1ViaH2::Done(None);
    };
    if !matches!(
        timeout(WRITE_TIMEOUT, client_stream.write_all(head)).await,
        Ok((Ok(_), _))
    ) {
        return H1ViaH2::Done(None);
    }
    if no_body {
        return H1ViaH2::Done(Some((client_stream, status, 0, client_wants_close)));
    }
    // キャッシュの外れ: 見出しと、送った本文を取り込む（区切りどおりに終わり、トレーラの無い応答だけ保存する。F-257）
    if let Some(ctx) = cache_ctx.as_deref_mut() {
        ctx.set_header_pairs(
            resp_headers.iter().map(|(n, v)| (n.as_ref(), v.as_ref())),
            status,
        );
    }
    let mut sent: u64 = 0;
    let mut trailers: Option<Vec<(Bytes, Bytes)>> = None;
    // B-414: 長さ既知の本文は宣言した長さを超えて書かない。超える DATA・足りないまま終わった本文は接続を閉じて
    // 切り詰めを示す（以前は DATA をそのまま書き、余りがクライアントの次の応答として読まれ得た）
    let declared = content_length.map(|n| n as u64);
    loop {
        match timeout(READ_TIMEOUT, stream.resp_rx.recv()).await {
            Ok(None) => break,
            Ok(Some(UpResp::Data(b))) => {
                if declared.is_some_and(|n| sent + b.len() as u64 > n) {
                    client_stream.skip_close_notify();
                    return H1ViaH2::Done(Some((client_stream, status, sent, true)));
                }
                sent += b.len() as u64;
                if let Some(ctx) = cache_ctx.as_deref_mut() {
                    ctx.append_body(&b);
                }
                let ok = if chunked {
                    let mut size = Vec::with_capacity(12);
                    push_chunk_size_line(&mut size, b.len());
                    let r = timeout(WRITE_TIMEOUT, client_stream.write_all_vectored(size, b)).await;
                    matches!(r, Ok((Ok(()), _, _)))
                        && matches!(
                            timeout(WRITE_TIMEOUT, client_stream.write_all(b"\r\n".to_vec())).await,
                            Ok((Ok(_), _))
                        )
                } else {
                    matches!(
                        timeout(WRITE_TIMEOUT, client_stream.write_all(b)).await,
                        Ok((Ok(_), _))
                    )
                };
                if !ok {
                    return H1ViaH2::Done(None);
                }
            }
            Ok(Some(UpResp::Trailers(t))) => trailers = Some(t),
            Ok(Some(UpResp::Head { .. })) => {}
            // 本文の途中で上流が失敗: 切り詰めた本文を正常終了に見せないよう接続を閉じる（close_notify も送らない。F-284）。
            Ok(Some(UpResp::Reset { .. })) | Err(_) => {
                client_stream.skip_close_notify();
                return H1ViaH2::Done(Some((client_stream, status, sent, true)));
            }
        }
    }
    if declared.is_some_and(|n| sent != n) {
        client_stream.skip_close_notify();
        return H1ViaH2::Done(Some((client_stream, status, sent, true)));
    }
    if chunked {
        let mut tail = Vec::with_capacity(64);
        tail.extend_from_slice(b"0\r\n");
        for (n, v) in trailers.iter().flatten() {
            tail.extend_from_slice(n);
            tail.extend_from_slice(b": ");
            tail.extend_from_slice(v);
            tail.extend_from_slice(b"\r\n");
        }
        tail.extend_from_slice(b"\r\n");
        if !matches!(
            timeout(WRITE_TIMEOUT, client_stream.write_all(tail)).await,
            Ok((Ok(_), _))
        ) {
            return H1ViaH2::Done(None);
        }
    }
    if trailers.is_none() {
        if let Some(ctx) = cache_ctx {
            ctx.save_to_cache().await;
        }
    }
    H1ViaH2::Done(Some((client_stream, status, sent, client_wants_close)))
}

// B-420
/// HTTP/1.1 で受けた要求の本文を、上流の h2c へ送るために全部読む（chunked は復号する。上限は Content-Length の
/// 要求が `max_request_body_size`、chunked が `max_chunked_body_size`、gRPC は `MAX_GRPC_BODY_SIZE` まで）。
/// 返す `bool` は本文の後ろまで読んだか（次の要求が来ている）。失敗はステータス（上限超過 413、それ以外 400）。
#[cfg(feature = "http2")]
async fn read_h2c_request_body(
    client_stream: &mut ServerTls,
    security: &SecurityConfig,
    headers: &[(Bytes, Bytes)],
    content_length: usize,
    is_chunked: bool,
    is_grpc: bool,
    initial_body: &[u8],
) -> Result<(Bytes, bool), u16> {
    // chunked はクライアントが 100 Continue を待つことがある（Content-Length の要求は h1 の経路が先に返している）
    if is_chunked && crate::http_utils::check_expect_continue(headers) {
        match timeout(
            WRITE_TIMEOUT,
            client_stream.write_all(HTTP_100_CONTINUE.to_vec()),
        )
        .await
        {
            Ok((Ok(_), _)) => {}
            _ => return Err(400),
        }
    }
    let mut limit = if is_chunked {
        security.max_chunked_body_size
    } else {
        security.max_request_body_size
    };
    if is_grpc {
        limit = limit.min(MAX_GRPC_BODY_SIZE);
    }
    let mut overflow = BytesMut::new();
    let body = super::h1::read_full_request_body(
        client_stream,
        content_length,
        is_chunked,
        initial_body,
        limit,
        &mut overflow,
    )
    .await?;
    Ok((body, !overflow.is_empty()))
}

/// H2C (HTTP/2 over cleartext) プロキシ
///
/// HTTP/2 Prior Knowledge モードでバックエンドに接続し、
/// リクエストを送信してレスポンスを受信します。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn proxy_h2c(
    mut client_stream: ServerTls,
    target: &ProxyTarget,
    security: &SecurityConfig,
    compression: &CompressionConfig,
    client_encoding: crate::config::ClientAccept,
    #[cfg(feature = "wasm")] wasm_modules: &crate::wasm_plugin_config::WasmModules,
    method: &[u8],
    path: &[u8],
    headers: &[(Bytes, Bytes)],
    request_body: &[u8],
    client_wants_close: bool,
    added: &AddedHeaders,
    // キャッシュの外れの要求の応答を取り込む（圧縮しない・トレーラの無い・Wasm で書き換えない応答だけ。F-257）
    mut cache_ctx: Option<&mut CacheSaveContext>,
) -> Option<(ServerTls, u16, u64, bool)> {
    // F-174: 多重化した上流 HTTP/2 接続（ワーカーごとのプール）を使う
    // （従来は要求ごとに TCP 接続 + HTTP/2 ハンドシェイクしていた）。
    let addr = target.conn_addr(); // F-41/F-170: スタック上に構築（ヒープ確保なし、UDS 対応。TCP は従来と不変）
    let key = crate::http_utils::PoolKeyStr::plain_addr(addr.as_str());
    let body = (!request_body.is_empty()).then(|| Bytes::copy_from_slice(request_body));
    let response = match http2::upstream_mux::request_h2c_buffered(
        target,
        key.as_str(),
        Duration::from_secs(security.backend_connect_timeout_secs),
        Duration::from_secs(security.idle_connection_timeout_secs),
        READ_TIMEOUT,
        method,
        path,
        target.host.as_bytes(),
        headers
            .iter()
            .filter(|(k, _)| forward_allowed(security, k) && added.keeps(k))
            .map(|(k, v)| (k.as_ref(), v.as_ref()))
            .chain(added.iter()),
        body,
        crate::http_utils::is_idempotent_method(method),
    )
    .await
    {
        Ok(resp) => resp,
        Err(status) => {
            crate::logging::log_throttled!(
                error,
                "H2C request error to {}: {}",
                addr.as_str(),
                status
            );
            return write_gateway_error(client_stream, status, cache_ctx.as_deref(), security)
                .await;
        }
    };

    // レスポンスをHTTP/1.1形式でクライアントに返す
    let status_code = response.status;
    let mut response = response;
    // stale-if-error: 上流の 502・504 は返さずに保存した応答を返す（B-492）
    if let Some(hit) = super::cache_lookup::gateway_error_stale(status_code, cache_ctx.as_deref()) {
        if let Some((len, st, _, must_close)) = super::cache_lookup::write_h1_stale(
            &mut client_stream,
            hit,
            status_code,
            security,
            client_wants_close,
        )
        .await
        {
            return Some((client_stream, st, len, client_wants_close || must_close));
        }
    }
    // キャッシュの外れ: Wasm・圧縮の前の上流の応答を取り込む。トレーラのある応答・Wasm で書き換える本文は保存しない
    // （当たりは Wasm を通らない）
    #[cfg(feature = "wasm")]
    if super::compress::wasm_wants_response_body(wasm_modules) {
        cache_ctx = None;
    }
    if !response.trailers.is_empty() {
        cache_ctx = None;
    }
    if let Some(ctx) = cache_ctx.as_deref_mut() {
        ctx.set_header_pairs(
            response
                .headers
                .iter()
                .map(|(n, v)| (n.as_ref(), v.as_ref())),
            status_code,
        );
        ctx.append_body(&response.body);
    }

    // F-285: Wasm の応答本文のフェーズ（受けるモジュールのあるルートだけ。圧縮の前。gRPC・本文の無い応答は除く）
    #[cfg(feature = "wasm")]
    if !(method == b"HEAD"
        || matches!(status_code, 100..=199 | 204 | 304)
        || response.headers.iter().any(|(n, v)| {
            n.eq_ignore_ascii_case(b"content-type") && v.starts_with(b"application/grpc")
        }))
        && super::compress::wasm_wants_response_body(wasm_modules)
    {
        let body = std::mem::take(&mut response.body);
        match super::compress::wasm_filter_response_body(wasm_modules, body).await {
            Ok(b) => response.body = b,
            Err(local) => {
                let out = super::h1::wasm_local_response_h1(&local, false);
                let len = out.len() as u64;
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(out)).await;
                return Some((client_stream, local.status_code, len, client_wants_close));
            }
        }
    }

    // B-270: HTTP/1.1・TLS の h2 の上流と同じく圧縮する（gRPC・本文の無い応答は除く）
    let header = |name: &[u8]| {
        response
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let content_type = header(b"content-type");
    let is_grpc = content_type
        .as_deref()
        .is_some_and(|ct| ct.starts_with(b"application/grpc"));
    // B-417: 大文字小文字を区別する
    let no_body = method == b"HEAD" || status_code == 204 || status_code == 304;
    let should_compress = if is_grpc || no_body || response.body.is_empty() {
        None
    } else {
        compression.should_compress(
            client_encoding,
            content_type.as_deref(),
            Some(response.body.len()),
            header(b"content-encoding").as_deref(),
        )
    };
    if let Some(enc) = should_compress {
        // 圧縮して返す応答は保存しない（F-257）
        cache_ctx = None;
        response.body = Bytes::from(compress_body_h2(&response.body, enc, compression));
        response
            .headers
            .retain(|(n, _)| !n.eq_ignore_ascii_case(b"content-encoding"));
        response.headers.push((
            Bytes::from_static(b"content-encoding"),
            Bytes::from_static(enc.as_header_value()),
        ));
        response.headers.push((
            Bytes::from_static(b"vary"),
            Bytes::from_static(b"Accept-Encoding"),
        ));
    }

    // Content-Length（本文の無い応答は上流の値を残す。HEAD の応答の長さは GET と同じ）
    let mut framing = Vec::with_capacity(32);
    if !no_body || !response.body.is_empty() {
        framing.extend_from_slice(b"Content-Length: ");
        let mut lb = itoa::Buffer::new();
        framing.extend_from_slice(lb.format(response.body.len()).as_bytes());
        framing.extend_from_slice(b"\r\n");
    }
    // トレーラーヘッダー（gRPC-status など）はレスポンスヘッダーとして転送する
    let head = render_h2_upstream_head(
        status_code,
        &response.headers,
        &response.trailers,
        &framing,
        security,
        #[cfg(feature = "wasm")]
        wasm_modules,
        client_wants_close,
    )
    .await;
    // 書いた後に保存の `await` があるので、応答の置き場はブロックの中で終わらせる（借りた局所変数は、移した後も
    // スコープの終わりまでフューチャに置き場が残る。F-257）
    let resp_size = {
        let mut http11_response = match head {
            Ok(h) => h,
            // 応答ヘッダの段のローカルレスポンス（B-300。本文は受信済みなので接続は続けられる）
            Err(resp) => {
                return super::sendfile::write_h1_wasm_local(
                    &mut client_stream,
                    &resp,
                    method == b"HEAD",
                )
                .await
                .map(|(st, n)| (client_stream, st, n, client_wants_close));
            }
        };
        if !no_body {
            http11_response.extend_from_slice(&response.body);
        }

        let resp_size = http11_response.len() as u64;

        // クライアントに送信
        let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(http11_response)).await;
        if !matches!(write_result, Ok((Ok(_), _))) {
            return None;
        }
        resp_size
    };
    if let Some(ctx) = cache_ctx {
        ctx.save_to_cache().await;
    }

    Some((client_stream, status_code, resp_size, client_wants_close))
}

/// 上流へ送る要求のヘッダ（Host の行の後ろ。`Transfer-Encoding`・`Connection` と終わりの空行は呼び出し側）を書く。
///
/// 生の転送（`handle_proxy`）と stale-while-revalidate の裏の更新（[`build_revalidation_request`]）で同じ方針にする
/// （B-371: 裏の更新はクライアントの Host・転送ヘッダ・削除を指定したヘッダをそのまま上流へ送り、その応答を
/// キャッシュしていた）。hop-by-hop・`Expect`・`remove_request_headers`・信頼しない送信元の転送ヘッダを除き、
/// `add_request_headers`・転送ヘッダ（クライアントの IP）・Via を足す。`drop_content_length` はクライアントの
/// `Content-Length` を送らないとき（chunked で送る・本文を送らない）。
#[inline(always)]
pub(crate) fn write_upstream_request_headers(
    request: &mut Vec<u8>,
    headers: &[(Bytes, Bytes)],
    security: &SecurityConfig,
    client_ip: &str,
    path_str: &str,
    drop_content_length: bool,
) {
    // B-232: 転送ヘッダの方針（信頼しない送信元の値を捨て、クライアントの IP を入れる）
    let fwd = ForwardedValues::new(security, client_ip);

    for (name, value) in headers {
        // host と connection ヘッダーは別途処理済みのためスキップ
        if name.eq_ignore_ascii_case(b"host") || name.eq_ignore_ascii_case(b"connection") {
            continue;
        }

        // RFC 7230 Section 6.1: Hop-by-hopヘッダーを削除
        // Connection, Keep-Alive, Proxy-Connection, TE, Trailer, Transfer-Encoding, Upgrade
        // これらのヘッダーはプロキシで終端され、バックエンドに転送してはならない
        if is_hop_by_hop_header(name) {
            continue;
        }

        // B-23（多層防御）: chunked 転送時にクライアント由来の Content-Length を
        // バックエンドへ渡さない。フレーミング分類（classify_request_framing）が CL+TE を
        // 既に 400 で拒否しているため通常ここには到達しないが、chunked では下で
        // `Transfer-Encoding: chunked` を再付与するため、万一 CL が残っても
        // バックエンドに CL+TE の曖昧メッセージを渡さないよう保険で除去する。
        if drop_content_length && name.eq_ignore_ascii_case(b"content-length") {
            continue;
        }

        // B-11: Expect: 100-continue はプロキシが終端する（自ら 100 Continue を応答し、
        // ボディを無条件に転送する）ため、バックエンドへは転送しない。転送すると
        // バックエンドが独自の 100 Continue 中間応答を返し、応答解析と競合する。
        if name.eq_ignore_ascii_case(b"expect") {
            continue;
        }

        // 設定で削除が指定されているヘッダーをスキップ
        // eq_ignore_ascii_case でアロケーションなしに大文字小文字無視比較
        if security
            .remove_request_headers
            .iter()
            .any(|h| name.eq_ignore_ascii_case(h.as_bytes()))
        {
            continue;
        }
        if fwd.as_ref().is_some_and(|f| !f.keeps(name)) {
            continue;
        }

        // Header Injection防止: ヘッダー名と値の検証
        // httparseによるパース後も、多層防御として再検証を行う
        // 不正な文字（CR, LF, NUL等）を含むヘッダーは除外
        if !is_valid_header_name(name) {
            warn!(
                "Invalid header name detected, skipping: {:?}",
                String::from_utf8_lossy(name)
            );
            continue;
        }
        if !is_valid_header_value(value) {
            warn!(
                "Invalid header value detected (possible header injection), skipping header: {:?}",
                String::from_utf8_lossy(name)
            );
            continue;
        }

        request.extend_from_slice(name);
        request.extend_from_slice(HEADER_COLON);
        request.extend_from_slice(value);
        request.extend_from_slice(HEADER_CRLF);
    }

    // 設定で追加が指定されているヘッダーを追加
    // 特殊変数の置換: $client_ip, $host, $request_uri
    //
    // F-168 P4: `host_str` の検索はループ不変なので外へ出し、値の展開は
    // `expand_request_header_value` の 1 パスで再利用バッファへ書き込む
    // （`String::replace` の 3 連鎖はプレースホルダが無い値でも中間 `String` を
    // 3 つ確保していた）。
    if !security.add_request_headers.is_empty() {
        let host_str = headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(b"host"))
            .map(|(_, v)| std::str::from_utf8(v).unwrap_or("-"))
            .unwrap_or("-");

        let mut scratch = crate::pool::header_value_scratch_get();
        for (header_name, header_value) in &security.add_request_headers {
            scratch.clear();
            expand_request_header_value(header_value, client_ip, host_str, path_str, &mut scratch);

            // Header Injection防止チェック
            if !is_valid_header_value(&scratch) {
                warn!("Invalid add_request_header value: {}", header_name);
                continue;
            }

            request.extend_from_slice(header_name.as_bytes());
            request.extend_from_slice(HEADER_COLON);
            request.extend_from_slice(&scratch);
            request.extend_from_slice(HEADER_CRLF);
        }
        crate::pool::header_value_scratch_put(scratch);
    }
    if let Some(f) = &fwd {
        for (name, value) in f.pairs() {
            request.extend_from_slice(name);
            request.extend_from_slice(HEADER_COLON);
            request.extend_from_slice(value);
            request.extend_from_slice(HEADER_CRLF);
        }
    }

    // Via ヘッダー追加（RFC 9110 §7.6.3）。B-484: 値は設定のロード時に作ったもの（以前は要求ごとに設定を読んだ）
    if let Some(via) = crate::pool::via_header_value(ViaProto::Http11) {
        request.extend_from_slice(b"Via: ");
        request.extend_from_slice(&via);
        request.extend_from_slice(HEADER_CRLF);
    }
}

/// stale-while-revalidate の裏の更新で上流へ送る要求（本文なし・`Connection: close`）。
///
/// B-371: 生の転送と同じく、上流の Host（`target.host`）・上流のパス（`path_prefix` を付ける）・ヘッダの方針
/// （[`write_upstream_request_headers`]）で組み立てる。
pub(crate) fn build_revalidation_request(
    method: &[u8],
    req_path: &[u8],
    prefix: &[u8],
    target: &ProxyTarget,
    headers: &[(Bytes, Bytes)],
    security: &SecurityConfig,
    client_ip: &str,
) -> Vec<u8> {
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    let final_path = compute_upstream_path(path_str, prefix, &target.path_prefix, false);
    let mut request = Vec::with_capacity(512);
    request.extend_from_slice(method);
    request.extend_from_slice(HEADER_SPACE);
    request.extend_from_slice(final_path.as_bytes());
    request.extend_from_slice(HEADER_HTTP11_HOST);
    request.extend_from_slice(target.host.as_bytes());
    if !target.is_default_port() {
        request.extend_from_slice(HEADER_PORT_COLON);
        let mut port_buf = itoa::Buffer::new();
        request.extend_from_slice(port_buf.format(target.port).as_bytes());
    }
    request.extend_from_slice(HEADER_CRLF);
    write_upstream_request_headers(&mut request, headers, security, client_ip, path_str, true);
    request.extend_from_slice(b"Connection: close\r\n\r\n");
    request
}

#[cfg(test)]
mod revalidation_request_tests {
    use super::*;

    /// B-414: HTTP/2 の上流の content-length は全部の行が同じ数のときだけ使う
    #[cfg(feature = "http2")]
    #[test]
    fn h2_upstream_content_length_rejects_conflicts() {
        let h = |n: &'static [u8], v: &'static [u8]| (Bytes::from_static(n), Bytes::from_static(v));
        assert_eq!(h2_upstream_content_length(&[]), Ok(None));
        assert_eq!(
            h2_upstream_content_length(&[h(b"content-length", b"5")]),
            Ok(Some(5))
        );
        assert_eq!(
            h2_upstream_content_length(&[h(b"content-length", b"5"), h(b"Content-Length", b" 5")]),
            Ok(Some(5))
        );
        for bad in [
            vec![h(b"content-length", b"5"), h(b"content-length", b"6")],
            vec![h(b"content-length", b"+5")],
            vec![h(b"content-length", b"5, 5")],
            vec![h(b"content-length", b"")],
            vec![h(b"content-length", b"99999999999999999999999999")],
        ] {
            assert_eq!(h2_upstream_content_length(&bad), Err(()), "{bad:?}");
        }
    }

    /// B-371: stale-while-revalidate の裏の更新は生の転送と同じ方針で組み立てる（上流の Host・path_prefix・
    /// remove_request_headers・信頼しない送信元の転送ヘッダを捨ててクライアントの IP・hop-by-hop を送らない）
    #[test]
    fn revalidation_request_follows_the_forwarding_policy() {
        let target = ProxyTarget::parse("http://10.0.0.5:8080/base").unwrap();
        let security = SecurityConfig {
            remove_request_headers: vec!["x-secret".to_string()],
            ..Default::default()
        };
        let h = |n: &'static [u8], v: &'static [u8]| (Bytes::from_static(n), Bytes::from_static(v));
        let headers = vec![
            h(b"Host", b"attacker.example"),
            h(b"X-Forwarded-For", b"6.6.6.6"),
            h(b"X-Secret", b"s3cret"),
            h(b"Connection", b"keep-alive"),
            h(b"Transfer-Encoding", b"chunked"),
            h(b"Content-Length", b"5"),
            h(b"Accept", b"*/*"),
        ];
        let req = build_revalidation_request(
            b"GET",
            b"/app/page?q=1",
            b"/app",
            &target,
            &headers,
            &security,
            "192.0.2.7",
        );
        let req = String::from_utf8(req).unwrap();
        assert!(
            req.starts_with("GET /base/page?q=1 HTTP/1.1\r\nHost: 10.0.0.5:8080\r\n"),
            "{req}"
        );
        assert!(req.contains("\r\nAccept: */*\r\n"), "{req}");
        assert!(req.contains("\r\nx-forwarded-for: 192.0.2.7\r\n"), "{req}");
        assert!(req.ends_with("\r\nConnection: close\r\n\r\n"), "{req}");
        for leaked in [
            "attacker.example",
            "6.6.6.6",
            "s3cret",
            "keep-alive",
            "Transfer-Encoding",
            "Content-Length",
        ] {
            assert!(!req.contains(leaked), "{leaked} leaked: {req}");
        }
        assert_eq!(req.matches("Connection:").count(), 1, "{req}");
    }
}
