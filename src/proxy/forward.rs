//! プロキシ処理（HTTP の上流・接続プール・h2c の上流）
//!
//! F-233 で `proxy_orig.rs` から移した（中身は変えていない）。

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
    cache_config: &cache::CacheConfig,
    method: &[u8],
    req_path: &[u8],
    prefix: &[u8],
    content_length: usize,
    is_chunked: bool,
    headers: &[(Bytes, Bytes)],
    initial_body: &[u8],
    client_wants_close: bool,
    wasm_modules: Arc<Vec<crate::wasm_plugin_config::ModuleRef>>,
    client_ip: &str,
) -> Option<(ServerTls, u16, u64, bool)> {
    // クライアントの Accept-Encoding を解析
    let client_encoding = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(b"accept-encoding"))
        .map(|(_, value)| AcceptedEncoding::parse(value))
        .unwrap_or(AcceptedEncoding::Identity);

    // ホスト名を取得
    let host_str = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(b"host"))
        .and_then(|(_, v)| std::str::from_utf8(v).ok())
        .unwrap_or("unknown");

    // RFC 7230 Section 4.3: TE ヘッダーを解析
    // クライアントがtrailersをサポートしているかを判定
    let _client_supports_trailers = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(b"te"))
        .map(|(_, value)| parse_te_header(value).supports_trailers)
        .unwrap_or(false);

    let path_str = std::str::from_utf8(req_path).unwrap_or("/");

    // ===================
    // キャッシュヒット判定
    // ===================
    if cache_config.enabled {
        // キャッシュ対象かチェック
        if cache_config.is_cacheable_method(method) && !cache_config.should_bypass(path_str) {
            // キャッシュキー生成（key_headers を使用）
            let query = path_str.find('?').map(|i| &path_str[i + 1..]);
            let path_only = path_str
                .find('?')
                .map(|i| &path_str[..i])
                .unwrap_or(path_str);

            // key_headers からVaryキー用のヘッダー値を抽出
            let vary_key_headers = if !cache_config.key_headers.is_empty() {
                let extracted =
                    extract_vary_headers_for_cache_key(headers, &cache_config.key_headers);
                if extracted.is_empty() {
                    None
                } else {
                    Some(extracted)
                }
            } else {
                None
            };

            if let Some(cache_key) = cache::CacheKey::from_request(
                method,
                host_str,
                path_only,
                query,
                cache_config.include_query,
                vary_key_headers.as_deref(), // key_headers に基づくVaryキー
            ) {
                // グローバルキャッシュからエントリを取得
                if let Some(cache_manager) = cache::get_global_cache() {
                    // 有効なエントリを取得
                    let (cached_entry, is_stale) =
                        if let Some(entry) = cache_manager.get(&cache_key) {
                            (Some(entry), false)
                        } else if cache_config.stale_while_revalidate {
                            // 期限切れでもstale-while-revalidate期間内なら使用
                            // デフォルトで60秒のstale期間を許容
                            if let Some(entry) = cache_manager.get_stale(&cache_key, 60) {
                                debug!("Using stale cache entry for {} {}", host_str, path_str);
                                (Some(entry), true)
                            } else {
                                (None, false)
                            }
                        } else {
                            (None, false)
                        };

                    if let Some(cached_entry) = cached_entry {
                        // キャッシュヒット！
                        debug!(
                            "Cache {} for {} {}",
                            if is_stale { "STALE" } else { "HIT" },
                            host_str,
                            path_str
                        );
                        record_cache_hit(host_str);

                        // ETag/If-None-Match 検証（304レスポンス）
                        if cache_config.enable_etag {
                            if let Some(client_etag) =
                                cache::CachePolicy::get_if_none_match(headers)
                            {
                                if let Some(ref cached_etag) = cached_entry.etag {
                                    // ETagが一致すれば304 Not Modifiedを返す
                                    let client_etag_str =
                                        std::str::from_utf8(client_etag).unwrap_or("");
                                    if etag_matches(client_etag_str, cached_etag) {
                                        debug!("ETag match, returning 304 Not Modified");
                                        let response = build_304_response(
                                            &cached_entry,
                                            client_wants_close,
                                            is_stale,
                                        );
                                        match timeout(
                                            WRITE_TIMEOUT,
                                            client_stream.write_all(response),
                                        )
                                        .await
                                        {
                                            Ok((Ok(_), _)) => {
                                                return Some((
                                                    client_stream,
                                                    304,
                                                    0,
                                                    client_wants_close,
                                                ));
                                            }
                                            _ => {
                                                return None;
                                            }
                                        }
                                    }
                                }
                            }

                            // If-Modified-Since 検証（304レスポンス）
                            if let Some(client_ims) =
                                cache::CachePolicy::get_if_modified_since(headers)
                            {
                                if let Some(ref cached_lm) = cached_entry.last_modified {
                                    let client_ims_str =
                                        std::str::from_utf8(client_ims).unwrap_or("");
                                    if last_modified_matches(client_ims_str, cached_lm) {
                                        debug!(
                                            "If-Modified-Since match, returning 304 Not Modified"
                                        );
                                        let response = build_304_response(
                                            &cached_entry,
                                            client_wants_close,
                                            is_stale,
                                        );
                                        match timeout(
                                            WRITE_TIMEOUT,
                                            client_stream.write_all(response),
                                        )
                                        .await
                                        {
                                            Ok((Ok(_), _)) => {
                                                return Some((
                                                    client_stream,
                                                    304,
                                                    0,
                                                    client_wants_close,
                                                ));
                                            }
                                            _ => {
                                                return None;
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // stale-while-revalidate: バックグラウンド更新タスクをスポーン
                        if is_stale {
                            spawn_background_revalidation(
                                cache_key.clone(),
                                upstream_group.clone(),
                                security.clone(),
                                method.to_vec(),
                                req_path.to_vec(),
                                prefix.to_vec(),
                                headers.to_vec(),
                            );
                        }

                        // キャッシュからレスポンスを返す
                        // メモリキャッシュの場合（ボディは bytes::Bytes をゼロコピーで送出）
                        if let Some(body_data) = cached_entry.memory_body() {
                            let body = body_data.clone(); // O(1) refcount、memcpy なし
                            let body_len = body.len();
                            // ヘッダーのみ構築し、ボディは連結せずゼロコピーのまま
                            // F-59: ヘッダ + ボディを 1 回の SENDMSG（scatter-gather）で送出
                            // （平文接続。kTLS/rustls は内部で 2 回書き込みへフォールバック）
                            let headers = build_cached_response_headers(
                                &cached_entry,
                                client_wants_close,
                                is_stale,
                            );
                            match timeout(
                                WRITE_TIMEOUT,
                                client_stream.write_all_vectored(headers, body),
                            )
                            .await
                            {
                                Ok((Ok(()), _, _)) => {
                                    return Some((
                                        client_stream,
                                        cached_entry.status_code,
                                        body_len as u64,
                                        client_wants_close,
                                    ));
                                }
                                _ => {
                                    return None;
                                }
                            }
                        }
                        // ディスクキャッシュの場合
                        else if let Some(disk_path) = cached_entry.disk_path() {
                            debug!("Serving from disk cache: {:?}", disk_path);
                            match serve_from_disk_cache(
                                &mut client_stream,
                                &cached_entry,
                                disk_path,
                                client_wants_close,
                                is_stale,
                            )
                            .await
                            {
                                Some((status_code, body_size)) => {
                                    return Some((
                                        client_stream,
                                        status_code,
                                        body_size,
                                        client_wants_close,
                                    ));
                                }
                                None => {
                                    // ディスク読み込み失敗、キャッシュエントリを無効化してバックエンドに転送
                                    warn!("Failed to read disk cache: {:?}", disk_path);
                                    cache_manager.invalidate(&cache_key);
                                }
                            }
                        }
                    } else {
                        debug!("Cache MISS for {} {}", host_str, path_str);
                        record_cache_miss(host_str);
                    }
                }
            }
        }
    }

    // キャッシュ保存コンテキストを作成（キャッシュ有効かつキャッシュ可能な場合）
    let mut cache_save_ctx: Option<CacheSaveContext> = None;
    if cache_config.enabled
        && cache_config.is_cacheable_method(method)
        && !cache_config.should_bypass(path_str)
    {
        let query = path_str.find('?').map(|i| &path_str[i + 1..]);
        let path_only = path_str
            .find('?')
            .map(|i| &path_str[..i])
            .unwrap_or(path_str);

        // key_headers からVaryキー用のヘッダー値を抽出（保存時も同じキーを使用）
        let vary_key_headers = if !cache_config.key_headers.is_empty() {
            let extracted = extract_vary_headers_for_cache_key(headers, &cache_config.key_headers);
            if extracted.is_empty() {
                None
            } else {
                Some(extracted)
            }
        } else {
            None
        };

        if let Some(cache_key) = cache::CacheKey::from_request(
            method,
            host_str,
            path_only,
            query,
            cache_config.include_query,
            vary_key_headers.as_deref(), // key_headers に基づくVaryキー
        ) {
            // キャッシュ保存用コンテキストを作成
            let max_capture = cache_config.max_memory_size.min(10 * 1024 * 1024); // 最大10MB
            cache_save_ctx = Some(CacheSaveContext::new(
                cache_key,
                host_str.to_string(),
                max_capture,
            ));
        }
    }

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
            error!("No healthy upstream servers available");
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((client_stream, 502, 0, true));
        }
    };

    // 接続カウンターを増加（Least Connections 用）
    server.acquire();

    // F-06: リクエスト結果記録用にサーバーのインデックスと開始時刻を記録
    let resilience_server_idx = upstream_group
        .servers
        .iter()
        .position(|s| std::ptr::eq(s, server));
    let resilience_start = std::time::Instant::now();

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
        if is_chunked && name.eq_ignore_ascii_case(b"content-length") {
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

    // Via ヘッダー追加 (RFC 7230 Section 5.7.1)
    // プロキシ経由のリクエストに Via ヘッダーを追加
    {
        let config = CURRENT_CONFIG.load();
        if config.performance.via_header_enabled {
            let hostname = config
                .performance
                .via_header_hostname
                .as_deref()
                .unwrap_or("veil");
            // Via: 1.1 <hostname>
            request.extend_from_slice(b"Via: 1.1 ");
            request.extend_from_slice(hostname.as_bytes());
            request.extend_from_slice(HEADER_CRLF);
        }
    }

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
        )
        .await
        {
            H1ViaH2::Done(r) => {
                server.release();
                // F-06: HTTP/1.1 経路と同じくサーキットブレーカー・異常検知へ反映する。
                if let Some(idx) = resilience_server_idx {
                    let success = matches!(&r, Some((_, status, _, _)) if *status < 500);
                    let latency_ms = resilience_start.elapsed().as_millis() as u64;
                    upstream_group.record_outcome(idx, success, latency_ms);
                }
                return r;
            }
            H1ViaH2::Http1(c) => c,
        }
    } else {
        client_stream
    };

    let result = if target.use_tls {
        // HTTPS接続（キャッシュ保存はHTTPのみサポート、HTTPSは別途実装が必要）
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
                proxy_h2c(
                    client_stream,
                    target,
                    security,
                    method,
                    final_path.as_bytes(),
                    headers,
                    initial_body,
                    client_wants_close,
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
                cache_save_ctx.as_mut(),
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
            cache_save_ctx.as_mut(),
            wasm_modules,
        )
        .await
    };

    // 接続カウンターを減少（Least Connections 用）
    server.release();

    // F-06: リクエスト結果をサーキットブレーカー・異常検知へ反映
    if let Some(idx) = resilience_server_idx {
        let latency_ms = resilience_start.elapsed().as_millis() as u64;
        // 5xx をバックエンド障害として扱う
        let success = match &result {
            Some((_, status, _, _)) => *status < 500,
            None => false,
        };
        upstream_group.record_outcome(idx, success, latency_ms);
        #[cfg(feature = "metrics")]
        {
            if let Some(s) = upstream_group.servers.get(idx) {
                if let Some(cb) = &s.circuit_breaker {
                    crate::metrics::set_circuit_breaker_state(
                        &upstream_group.name,
                        cb.state_code(),
                    );
                }
                if s.is_ejected() {
                    // F-170: 接続先表記（conn_addr）で識別する。TCP は従来と不変。
                    let addr = s.target.conn_addr();
                    crate::metrics::set_outlier_ejected(&upstream_group.name, addr.as_str(), true);
                }
            }
        }
    }

    // stale-if-error: バックエンドエラー時にstaleキャッシュを返す
    if cache_config.stale_if_error {
        if let Some((mut client_stream, status_code, _, _)) = result {
            // バックエンドエラー（502, 504）の場合
            if status_code == 502 || status_code == 504 {
                // staleキャッシュを確認
                #[cfg(feature = "cache")]
                let _cache_key_opt = cache_save_ctx.as_ref().map(|c| c.key.clone());
                #[cfg(not(feature = "cache"))]
                let _cache_key_opt: Option<cache::CacheKey> = None;
                if let Some(cache_key) = _cache_key_opt {
                    if let Some(cache_manager) = cache::get_global_cache() {
                        // 最大1時間のstaleキャッシュを許容
                        if let Some(stale_entry) = cache_manager.get_stale(&cache_key, 3600) {
                            debug!("stale-if-error: serving stale cache for {}", host_str);

                            // staleキャッシュを返す（ボディは bytes::Bytes をゼロコピーで送出）
                            // F-59: ヘッダ + ボディを 1 回の SENDMSG（scatter-gather）で送出
                            if let Some(body_data) = stale_entry.memory_body() {
                                let body = body_data.clone(); // O(1) refcount、memcpy なし
                                let body_len = body.len();
                                let headers = build_cached_response_headers(
                                    &stale_entry,
                                    client_wants_close,
                                    true,
                                );
                                match timeout(
                                    WRITE_TIMEOUT,
                                    client_stream.write_all_vectored(headers, body),
                                )
                                .await
                                {
                                    Ok((Ok(()), _, _)) => {
                                        return Some((
                                            client_stream,
                                            stale_entry.status_code,
                                            body_len as u64,
                                            client_wants_close,
                                        ));
                                    }
                                    _ => {
                                        return None;
                                    }
                                }
                            } else if let Some(disk_path) = stale_entry.disk_path() {
                                if let Some((code, size)) = serve_from_disk_cache(
                                    &mut client_stream,
                                    &stale_entry,
                                    disk_path,
                                    client_wants_close,
                                    true,
                                )
                                .await
                                {
                                    return Some((client_stream, code, size, client_wants_close));
                                }
                            }
                        }
                    }
                }
            }
            // staleキャッシュがない場合は元のエラーレスポンスをそのまま返す
            return Some((client_stream, status_code, 0, client_wants_close));
        }
        return result;
    }

    result
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
    client_encoding: AcceptedEncoding,
    pool_key: &str,
    request: Vec<u8>,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
    client_wants_close: bool,
    cache_ctx: Option<&mut CacheSaveContext>,
    wasm_modules: Arc<Vec<crate::wasm_plugin_config::ModuleRef>>,
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
                        error!("Proxy connect error to {}: {}", addr, e);
                        let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                        let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                        return Some((client_stream, 502, 0, true));
                    }
                    Err(_) => {
                        error!("Proxy connect timeout to {}", addr);
                        let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
                        let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                        return Some((client_stream, 504, 0, true));
                    }
                }
            }
        };

        // セキュリティ設定からchunked最大サイズを取得
        let max_chunked = security.max_chunked_body_size as u64;

        // 圧縮が有効かどうかの事前判定
        // 注意: 実際のContent-Typeはレスポンス受信後に判定するため、ここでは設定の有効/無効のみ確認
        let compression_enabled =
            compression.enabled && client_encoding != AcceptedEncoding::Identity;

        // メトリクス用ホスト名
        let host_str_for_metrics = &target.host;

        // バッファリングが有効かどうか判定
        // F-97: Content-Type: application/grpc は Full バッファをバイパス（リクエスト行に含む）
        let buffering_enabled = !is_grpc_req
            && buffering_config.is_enabled()
            && buffering_config.should_buffer(Some(content_length));

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
            if client_stream.is_ktls_enabled()
                && !is_chunked
                && !compression_enabled
                && !cache_save_needed
                && !buffering_enabled
                && !wasm_modules_active
            {
                let splice_result = try_splice_proxy(
                    &client_stream,
                    &backend_stream,
                    &req,
                    content_length,
                    is_chunked,
                    initial_body,
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
                    )
                    .await
                }
            } else if buffering_enabled && !compression_enabled {
                // バッファリング有効時（圧縮無効の場合のみ）
                debug!(
                    "Calling proxy_request_buffered for {} {}",
                    target.host, target.port
                );
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
                    cache_ctx.as_deref_mut(),
                    security,
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
                )
                .await
            }
        };

        #[cfg(not(veil_ktls))]
        let result = if buffering_enabled && !compression_enabled {
            // バッファリング有効時（圧縮無効の場合のみ）
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
                cache_ctx.as_deref_mut(),
                security,
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
            )
            .await
        };

        // F-177: プールから取り出した接続が応答の前に切れていた（応答未受信の 502 / エラー）なら、
        // クライアントへ未送信のうちに新しい接続で 1 回だけ再送する（冪等・本文受信済みの要求のみ）。
        if from_pool && replayable && attempt == 1 && matches!(result, None | Some((502, 0, _, _)))
        {
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
                let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                Some((client_stream, 502, 0, true))
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
    client_encoding: AcceptedEncoding,
    method: &[u8],
    path: &[u8],
    headers: &[(Bytes, Bytes)],
    request_body: &[u8],
    client_wants_close: bool,
    tls_profile: crate::upstream_tls::TlsProfile,
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
        headers.iter().map(|(k, v)| (k.as_ref(), v.as_ref())),
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
            let err_buf = if status == 504 {
                ERR_MSG_GATEWAY_TIMEOUT.to_vec()
            } else {
                ERR_MSG_BAD_GATEWAY.to_vec()
            };
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return H1ViaH2::Done(Some((client_stream, status, 0, true)));
        }
    };
    if !request_body.is_empty() {
        if let Some(tx) = &stream.req_tx {
            let _ = tx.send(Bytes::copy_from_slice(request_body)).await;
        }
    }
    stream.finish_request();

    let bad_gateway = |mut c: ServerTls| async move {
        let _ = timeout(WRITE_TIMEOUT, c.write_all(ERR_MSG_BAD_GATEWAY.to_vec())).await;
        H1ViaH2::Done(Some((c, 502, 0, true)))
    };
    // 最終応答の head を待つ。
    let (status, mut resp_headers) = match timeout(READ_TIMEOUT, stream.resp_rx.recv()).await {
        Ok(Some(UpResp::Head { status, headers })) => (status, headers),
        Err(_) => {
            let _ = timeout(
                WRITE_TIMEOUT,
                client_stream.write_all(ERR_MSG_GATEWAY_TIMEOUT.to_vec()),
            )
            .await;
            return H1ViaH2::Done(Some((client_stream, 504, 0, true)));
        }
        _ => return bad_gateway(client_stream).await,
    };
    let no_body = method.eq_ignore_ascii_case(b"HEAD") || status == 204 || status == 304;
    let header = |hs: &[(Bytes, Bytes)], name: &[u8]| {
        hs.iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let content_type = header(&resp_headers, b"content-type");
    let is_grpc = content_type
        .as_deref()
        .is_some_and(|ct| ct.starts_with(b"application/grpc"));
    let content_length: Option<usize> = header(&resp_headers, b"content-length").and_then(|v| {
        std::str::from_utf8(&v)
            .ok()
            .and_then(|s| s.trim().parse().ok())
    });
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

    // 応答 head（ホップバイホップは HTTP/2 には無いが、念のため落とす）。
    let write_head = |status: u16, hs: &[(Bytes, Bytes)], framing: &[u8]| {
        let mut head = Vec::with_capacity(256);
        head.extend_from_slice(b"HTTP/1.1 ");
        let mut sb = itoa::Buffer::new();
        head.extend_from_slice(sb.format(status).as_bytes());
        head.push(b' ');
        head.extend_from_slice(status_reason_phrase(status).as_bytes());
        head.extend_from_slice(b"\r\n");
        for (n, v) in hs {
            if n.eq_ignore_ascii_case(b"connection")
                || n.eq_ignore_ascii_case(b"keep-alive")
                || n.eq_ignore_ascii_case(b"transfer-encoding")
                || n.eq_ignore_ascii_case(b"upgrade")
                || n.eq_ignore_ascii_case(b"proxy-connection")
            {
                continue;
            }
            head.extend_from_slice(n);
            head.extend_from_slice(b": ");
            head.extend_from_slice(v);
            head.extend_from_slice(b"\r\n");
        }
        head.extend_from_slice(framing);
        head.extend_from_slice(if client_wants_close {
            b"Connection: close\r\n\r\n"
        } else {
            b"Connection: keep-alive\r\n\r\n"
        });
        head
    };

    if let Some(enc) = should_compress {
        let mut body: Vec<u8> = Vec::new();
        loop {
            match timeout(READ_TIMEOUT, stream.resp_rx.recv()).await {
                Ok(None) => break,
                Ok(Some(UpResp::Data(b))) => body.extend_from_slice(&b),
                Ok(Some(UpResp::Trailers(_))) | Ok(Some(UpResp::Head { .. })) => {}
                Ok(Some(UpResp::Reset { .. })) => return bad_gateway(client_stream).await,
                Err(_) => {
                    let _ = timeout(
                        WRITE_TIMEOUT,
                        client_stream.write_all(ERR_MSG_GATEWAY_TIMEOUT.to_vec()),
                    )
                    .await;
                    return H1ViaH2::Done(Some((client_stream, 504, 0, true)));
                }
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
        let head = write_head(status, &resp_headers, &framing);
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

    // 長さ既知（または本文なし）ならそのまま、不明なら chunked で書く。
    let chunked = !no_body && content_length.is_none();
    let framing: &[u8] = if chunked {
        b"Transfer-Encoding: chunked\r\n"
    } else {
        b""
    };
    let head = write_head(status, &resp_headers, framing);
    if !matches!(
        timeout(WRITE_TIMEOUT, client_stream.write_all(head)).await,
        Ok((Ok(_), _))
    ) {
        return H1ViaH2::Done(None);
    }
    if no_body {
        return H1ViaH2::Done(Some((client_stream, status, 0, client_wants_close)));
    }
    let mut sent: u64 = 0;
    let mut trailers: Option<Vec<(Bytes, Bytes)>> = None;
    loop {
        match timeout(READ_TIMEOUT, stream.resp_rx.recv()).await {
            Ok(None) => break,
            Ok(Some(UpResp::Data(b))) => {
                sent += b.len() as u64;
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
            // 本文の途中で上流が失敗: 切り詰めた本文を正常終了に見せないよう接続を閉じる。
            Ok(Some(UpResp::Reset { .. })) | Err(_) => {
                return H1ViaH2::Done(Some((client_stream, status, sent, true)));
            }
        }
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
    H1ViaH2::Done(Some((client_stream, status, sent, client_wants_close)))
}

/// H2C (HTTP/2 over cleartext) プロキシ
///
/// HTTP/2 Prior Knowledge モードでバックエンドに接続し、
/// リクエストを送信してレスポンスを受信します。
#[cfg(feature = "http2")]
pub(super) async fn proxy_h2c(
    mut client_stream: ServerTls,
    target: &ProxyTarget,
    security: &SecurityConfig,
    method: &[u8],
    path: &[u8],
    headers: &[(Bytes, Bytes)],
    request_body: &[u8],
    client_wants_close: bool,
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
        headers.iter().map(|(k, v)| (k.as_ref(), v.as_ref())),
        body,
        crate::http_utils::is_idempotent_method(method),
    )
    .await
    {
        Ok(resp) => resp,
        Err(status) => {
            error!("H2C request error to {}: {}", addr.as_str(), status);
            let err_buf = if status == 504 {
                ERR_MSG_GATEWAY_TIMEOUT.to_vec()
            } else {
                ERR_MSG_BAD_GATEWAY.to_vec()
            };
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((client_stream, status, 0, true));
        }
    };

    // レスポンスをHTTP/1.1形式でクライアントに返す
    let status_code = response.status;
    let mut http11_response = Vec::with_capacity(512 + response.body.len());

    // ステータス行
    http11_response.extend_from_slice(b"HTTP/1.1 ");
    let mut status_buf = itoa::Buffer::new();
    http11_response.extend_from_slice(status_buf.format(status_code).as_bytes());
    http11_response.extend_from_slice(b" ");
    http11_response.extend_from_slice(status_reason_phrase(status_code).as_bytes());
    http11_response.extend_from_slice(b"\r\n");

    // レスポンスヘッダー
    for (name, value) in &response.headers {
        // ホップバイホップヘッダーはスキップ
        if name.eq_ignore_ascii_case(b"connection")
            || name.eq_ignore_ascii_case(b"transfer-encoding")
            || name.eq_ignore_ascii_case(b"keep-alive")
        {
            continue;
        }
        http11_response.extend_from_slice(name);
        http11_response.extend_from_slice(b": ");
        http11_response.extend_from_slice(value);
        http11_response.extend_from_slice(b"\r\n");
    }

    // トレーラーヘッダー（gRPC-status など）をレスポンスヘッダーとして転送
    for (name, value) in &response.trailers {
        http11_response.extend_from_slice(name);
        http11_response.extend_from_slice(b": ");
        http11_response.extend_from_slice(value);
        http11_response.extend_from_slice(b"\r\n");
    }

    // Content-Length
    http11_response.extend_from_slice(b"Content-Length: ");
    http11_response.extend_from_slice(status_buf.format(response.body.len()).as_bytes());
    http11_response.extend_from_slice(b"\r\n");

    // Connection ヘッダー
    if client_wants_close {
        http11_response.extend_from_slice(b"Connection: close\r\n");
    } else {
        http11_response.extend_from_slice(b"Connection: keep-alive\r\n");
    }

    http11_response.extend_from_slice(b"\r\n");

    // ボディ
    http11_response.extend_from_slice(&response.body);

    let resp_size = http11_response.len() as u64;

    // クライアントに送信
    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(http11_response)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return None;
    }

    Some((client_stream, status_code, resp_size, client_wants_close))
}

/// ディスクキャッシュからレスポンスを提供
///
/// 戻り値: Some((status_code, body_size)) または None（エラー時）
pub(super) async fn serve_from_disk_cache(
    client_stream: &mut ServerTls,
    cached_entry: &cache::CacheEntry,
    disk_path: &std::path::Path,
    client_wants_close: bool,
    is_stale: bool,
) -> Option<(u16, u64)> {
    // ディスクからボディを読み込み。whole-file 読み込みはイベントループをブロックするため、
    // ブロッキングオフロードで専用ワーカースレッドへ退避する（F-29 完全非同期化）。
    let disk_path_owned = disk_path.to_path_buf();
    // 理由付き allow: 同期 FS は offload 閉包内（専用ワーカースレッド）で実行され、イベントループを塞がない。
    #[allow(clippy::disallowed_methods)]
    let body_data =
        match crate::runtime::offload::offload(move || std::fs::read(disk_path_owned)).await {
            Ok(data) => data,
            Err(e) => {
                error!("Failed to read disk cache file: {}", e);
                return None;
            }
        };

    // レスポンスを構築
    let response = build_cached_response(cached_entry, &body_data, client_wants_close, is_stale);

    match timeout(WRITE_TIMEOUT, client_stream.write_all(response)).await {
        Ok((Ok(_), _)) => Some((cached_entry.status_code, body_data.len() as u64)),
        _ => None,
    }
}
