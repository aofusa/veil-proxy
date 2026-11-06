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
    client_encoding: crate::config::ClientAccept,
    cache_ctx: Option<&mut CacheSaveContext>,
    security: &SecurityConfig,
    wasm_modules: crate::wasm_plugin_config::WasmModules,
) -> Option<(u16, u64, bool, bool)> {
    // HEAD の応答は本文を持たない（Content-Length があっても読まない。B-245）
    let is_head = request_is_head(&request);
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
    client_encoding: crate::config::ClientAccept,
    mut cache_ctx: Option<&mut CacheSaveContext>,
    security: &SecurityConfig,
    wasm_modules: crate::wasm_plugin_config::WasmModules,
    is_head: bool,
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
                // 注意: 圧縮時はキャッシュ保存をスキップ（圧縮後のデータをキャッシュするには追加実装が必要）
                // リクエストごとに通る経路なので info ではなく debug（既定レベルで出さない）。
                ftlog::debug!(
                    "[Compression] Initializing compressed transfer with {:?}",
                    encoding
                );
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
                    status_code,
                    #[cfg(feature = "wasm")]
                    &wasm_modules,
                )
                .await
                {
                    Some((total, reusable)) => (total, status_code, reusable, false),
                    // 本文が途中で止まった・クライアントへ書けない: 上流接続は捨て、クライアントも閉じる
                    None => (0, 502, false, true),
                };
            } else {
                // 圧縮無効: そのまま転送（キャッシュ保存対応）
                // B-244: 最初の読み込みのうち本文の終端までだけを送る（後ろは次の応答であり得る）
                let start = response_body_start(body_kind, body_start);

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
                    ctx.append_body(&body_start[..start.send]);
                }

                // ヘッダーを修正（remove_response_headers・add_response_headers・Alt-Svc・Wasm・CORS）。1 本のバッファへ
                // 直接書く（以前は Wasm のあるルートでヘッダの行ごとに `format!` と `Vec` を作って連結していた）
                let modified_headers = match finish_h1_upstream_head(
                    &accumulated[..header_len],
                    status_code,
                    security,
                    #[cfg(feature = "wasm")]
                    &wasm_modules,
                )
                .await
                {
                    Ok(h) => h,
                    // 応答ヘッダの段のローカルレスポンス（B-300）。上流の本文は読まないので上流の接続は戻さない
                    Err(resp) => {
                        return match super::sendfile::write_h1_wasm_local(
                            client_stream,
                            &resp,
                            false,
                        )
                        .await
                        {
                            Some((st, n)) => (total + n, st, false, false),
                            None => (total, status_code, false, true),
                        };
                    }
                };

                // 修正したヘッダー + 初期ボディを送信
                // F-59: 平文接続では 1 回の SENDMSG（scatter-gather）で送出
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

                // 残りのボディを転送（キャッシュキャプチャ対応）。区切りどおりに終わったときだけ
                // 上流接続を戻し、キャッシュに保存する（B-244。B-162 の小さい応答もここで保存する）
                let outcome = transfer_response_body_rest(
                    backend_stream,
                    client_stream,
                    body_kind,
                    start,
                    cache_ctx,
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

/// F-285: 応答本文の Wasm フェーズを受けるモジュールがあるか（無いルートは本文を溜めない）。
#[cfg(feature = "wasm")]
pub(crate) fn wasm_wants_response_body(
    wasm_modules: &crate::wasm_plugin_config::WasmModules,
) -> bool {
    if wasm_modules.is_empty() {
        return false;
    }
    CURRENT_CONFIG
        .load()
        .wasm_filter_engine
        .as_ref()
        .is_some_and(|e| e.wants_response_body(wasm_modules))
}

/// F-285: 溜めた応答本文へ Wasm の `on_http_response_body` を `end_of_stream = true` で 1 回かける
/// （HTTP/3 の F-132 と同じ）。`Err` はモジュールのローカル応答（`send_local_response`）。
#[cfg(feature = "wasm")]
pub(crate) async fn wasm_filter_response_body(
    wasm_modules: &crate::wasm_plugin_config::WasmModules,
    body: Bytes,
) -> Result<Bytes, crate::wasm::LocalResponse> {
    let config = CURRENT_CONFIG.load();
    let Some(engine) = config.wasm_filter_engine.as_ref() else {
        return Ok(body);
    };
    match crate::wasm::http_executor::apply_wasm_response_body(engine, wasm_modules, body, true)
        .await
    {
        crate::wasm::http_executor::WasmBodyOutcome::Continue(b) => Ok(b),
        crate::wasm::http_executor::WasmBodyOutcome::LocalResponse(r) => Err(r),
    }
}

/// HTTP/1.1 の応答ヘッダ（空行まで）の本文の区切りを `Content-Length: len` に置き換える（`Transfer-Encoding`・
/// 元の `Content-Length` を除く。本文を書き換えた・chunked を溜めて解いたとき。F-285）
#[cfg(feature = "wasm")]
pub(super) fn set_h1_content_length(head: &mut Vec<u8>, len: usize) {
    if !head.ends_with(b"\r\n\r\n") {
        return;
    }
    let mut out = Vec::with_capacity(head.len() + 24);
    let mut first = true;
    for line in head[..head.len() - 2].split_inclusive(|&b| b == b'\n') {
        if !first {
            let name = line.split(|&b| b == b':').next().unwrap_or(b"");
            let name = name.trim_ascii();
            if name.eq_ignore_ascii_case(b"content-length")
                || name.eq_ignore_ascii_case(b"transfer-encoding")
            {
                continue;
            }
        }
        first = false;
        out.extend_from_slice(line);
    }
    out.extend_from_slice(b"Content-Length: ");
    let mut n = itoa::Buffer::new();
    out.extend_from_slice(n.format(len).as_bytes());
    out.extend_from_slice(b"\r\n\r\n");
    *head = out;
}

/// HTTP/1.1 の応答ヘッダへ `Content-Encoding` と `Vary: Accept-Encoding` を足す（F-285 の溜めた本文の圧縮）
#[cfg(feature = "wasm")]
pub(super) fn add_h1_content_encoding(head: &mut Vec<u8>, enc: AcceptedEncoding) {
    let name: &[u8] = match enc {
        AcceptedEncoding::Zstd => b"zstd",
        AcceptedEncoding::Brotli => b"br",
        AcceptedEncoding::Gzip => b"gzip",
        AcceptedEncoding::Deflate => b"deflate",
        AcceptedEncoding::Identity => return,
    };
    if !head.ends_with(b"\r\n\r\n") {
        return;
    }
    head.truncate(head.len() - 2);
    head.extend_from_slice(b"Content-Encoding: ");
    head.extend_from_slice(name);
    head.extend_from_slice(b"\r\nVary: Accept-Encoding\r\n\r\n");
}

/// 空行まで含む HTTP/1.1 の応答ヘッダに Wasm の `on_http_response_headers` をかける（モジュールが無ければそのまま）
#[cfg(feature = "wasm")]
async fn wasm_on_full_head(
    head: Vec<u8>,
    status: u16,
    wasm_modules: &crate::wasm_plugin_config::WasmModules,
) -> Result<Vec<u8>, super::sendfile::HeadLocal> {
    if wasm_modules.is_empty() || !head.ends_with(b"\r\n\r\n") {
        return Ok(head);
    }
    let mut lines = head;
    lines.truncate(lines.len() - 2);
    let mut out = super::sendfile::h1_wasm_response_head(lines, status, wasm_modules).await?;
    out.extend_from_slice(b"\r\n");
    Ok(out)
}

/// 上流の応答のヘッダ（ステータス行から空行まで）をクライアントへ返す形にし（`render_response_head`）、Wasm の
/// `on_http_response_headers`（B-268）と CORS（F-273）をかける。`buffering = "full"` の経路が使う（B-267）。
pub(super) async fn finish_h1_upstream_head(
    raw: &[u8],
    status: u16,
    security: &SecurityConfig,
    #[cfg(feature = "wasm")] wasm_modules: &crate::wasm_plugin_config::WasmModules,
) -> Result<Vec<u8>, super::sendfile::HeadLocal> {
    let head = render_response_head(raw, status, security);
    #[cfg(feature = "wasm")]
    let head = wasm_on_full_head(head, status, wasm_modules).await?;
    let mut head = head;
    // `[route.traffic.cors]` の実際の応答のヘッダ（F-273。CORS の無いルートは分岐だけ）
    crate::route_cors::apply_h1(&mut head, security);
    Ok(head)
}

/// 上流の応答のヘッダ（ステータス行から空行まで）を、クライアントへ返す形にする（WASM のフィルタが無いとき）。
///
/// ステータス行は `HTTP/1.1 <code> <reason>` に直し、`remove_response_headers` を除き、`add_response_headers` と
/// Alt-Svc を足す。確保は返すバッファ 1 本だけ。解析できなければ受け取ったまま返す（従来どおり）。
pub(super) fn render_response_head(raw: &[u8], status: u16, security: &SecurityConfig) -> Vec<u8> {
    let mut out = Vec::new();
    render_response_head_into(raw, status, security, &mut out);
    out
}

/// [`render_response_head`] と同じだが、`out` の末尾へ書く（呼び出し側が使い回すバッファで確保を避ける）。
pub(super) fn render_response_head_into(
    raw: &[u8],
    status: u16,
    security: &SecurityConfig,
    out: &mut Vec<u8>,
) {
    let mut storage = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut storage);
    if response.parse(raw).is_err() {
        out.extend_from_slice(raw);
        return;
    }
    let added: usize = security
        .add_response_headers
        .iter()
        .map(|(k, v)| k.len() + v.len() + 4)
        .sum();
    let alt_svc = get_alt_svc_guard().filter(|_| {
        security.remove_response_headers.is_empty() || !security.removes_response_header(b"alt-svc")
    });
    out.reserve(
        raw.len() + added + alt_svc.as_ref().map(|g| g.value().len() + 11).unwrap_or(0) + 16,
    );
    out.extend_from_slice(b"HTTP/1.1 ");
    let mut code = itoa::Buffer::new();
    out.extend_from_slice(code.format(status).as_bytes());
    out.push(b' ');
    out.extend_from_slice(status_code_to_reason(status).as_bytes());
    out.extend_from_slice(b"\r\n");
    // `[server] server_header_enabled` なら自分の `Server` にする（B-271。`remove_response_headers` の `Server` は自分のものも消す）
    let own_server = crate::pool::append_own_server_line(out, security);
    for header in response.headers.iter() {
        if security
            .remove_response_headers
            .iter()
            .any(|h| header.name.as_bytes().eq_ignore_ascii_case(h.as_bytes()))
            || (own_server && header.name.eq_ignore_ascii_case("server"))
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
}

/// 圧縮してレスポンスを転送
/// 戻り値: (転送バイト数, 上流接続を再利用できるか)。本文を区切りどおりに読み切れなければ何も送らず
/// `None`（呼び出し側が 502 にする。途中までを圧縮して完結した応答に見せない。B-244）
#[cfg(feature = "compression")]
pub(super) async fn transfer_compressed_response<R: AsyncReader + AsyncReadRent + Unpin>(
    client_stream: &mut ServerTls,
    backend_stream: &mut R,
    original_headers: &[u8],
    initial_body: &[u8],
    body_kind: ResponseBody,
    encoding: AcceptedEncoding,
    compression: &CompressionConfig,
    backend_wants_keep_alive: bool,
    security: &SecurityConfig,
    status_code: u16,
    #[cfg(feature = "wasm")] wasm_modules: &crate::wasm_plugin_config::WasmModules,
) -> Option<(u64, bool)> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    let mut total = 0u64;

    // 1. まず全てのボディデータを収集（ストリーミングは将来の改善）。chunked は復号する
    // （以前は chunked の枠ごと圧縮していた）
    let Some((body_data, extra)) =
        collect_response_body(backend_stream, body_kind, initial_body).await
    else {
        warn!("Backend response body incomplete before compression");
        return None;
    };
    let backend_wants_keep_alive =
        backend_wants_keep_alive && !extra && !matches!(body_kind, ResponseBody::UntilClose);

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
                .await
                .map(|t| (t, backend_wants_keep_alive));
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
                .await
                .map(|t| (t, backend_wants_keep_alive));
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
                    .await
                    .map(|t| (t, backend_wants_keep_alive));
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
                .await
                .map(|t| (t, backend_wants_keep_alive));
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
                .await
                .map(|t| (t, backend_wants_keep_alive));
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
                    .await
                    .map(|t| (t, backend_wants_keep_alive));
                }
            }
        }
        AcceptedEncoding::Identity => {
            // 圧縮なし（ここには来ないはず）
            body_data
        }
    };

    // 3. 新しいヘッダーを構築
    let new_headers = build_compressed_headers(
        original_headers,
        Some(encoding),
        compressed_body.len(),
        security,
    );
    // Wasm の `on_http_response_headers` は圧縮した後のヘッダに（B-268。区切りの行は元のまま残る）
    #[cfg(feature = "wasm")]
    let new_headers = match wasm_on_full_head(new_headers, status_code, wasm_modules).await {
        Ok(h) => h,
        // 応答ヘッダの段のローカルレスポンス（B-300。上流の本文は読み切っている）
        Err(resp) => {
            return super::sendfile::write_h1_wasm_local(client_stream, &resp, false)
                .await
                .map(|(_, n)| (n, backend_wants_keep_alive));
        }
    };
    #[cfg(not(feature = "wasm"))]
    let _ = status_code;

    // 4. ヘッダー送信（所有権を移動、clone 不要）
    let new_headers_len = new_headers.len();
    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(new_headers)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        // クライアントへ書けない: 呼び出し側はクライアント接続を閉じる
        return None;
    }
    total += new_headers_len as u64;

    // 5. 圧縮済みボディ送信（所有権を移動、clone 不要）
    let compressed_len = compressed_body.len();
    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(compressed_body)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        // クライアントへ書けない: 呼び出し側はクライアント接続を閉じる
        return None;
    }
    total += compressed_len as u64;

    Some((total, backend_wants_keep_alive))
}

/// compression feature 無効時のスタブ（`should_compress` が常に `None` なので呼ばれない）
#[cfg(not(feature = "compression"))]
#[allow(clippy::too_many_arguments)]
pub(super) async fn transfer_compressed_response<R: AsyncReader + AsyncReadRent + Unpin>(
    _client_stream: &mut ServerTls,
    _backend_stream: &mut R,
    _original_headers: &[u8],
    _initial_body: &[u8],
    _body_kind: ResponseBody,
    _encoding: AcceptedEncoding,
    _compression: &CompressionConfig,
    _backend_wants_keep_alive: bool,
    _security: &SecurityConfig,
    _status_code: u16,
    #[cfg(feature = "wasm")] _wasm_modules: &crate::wasm_plugin_config::WasmModules,
) -> Option<(u64, bool)> {
    None
}

/// 圧縮失敗時のフォールバック（非圧縮で送信）。クライアントへ書けなければ `None`。
/// 本文は集めた（chunked は復号した）ものなので、長さのヘッダを付け直す。
#[cfg(feature = "compression")]
pub(super) async fn transfer_uncompressed_fallback(
    client_stream: &mut ServerTls,
    original_headers: &[u8],
    body_data: &[u8],
    security: &SecurityConfig,
) -> Option<u64> {
    let head = build_compressed_headers(original_headers, None, body_data.len(), security);
    let total = (head.len() + body_data.len()) as u64;
    let write_result = timeout(
        WRITE_TIMEOUT,
        client_stream.write_all_vectored(head, body_data.to_vec()),
    )
    .await;
    if !matches!(write_result, Ok((Ok(()), _, _))) {
        return None;
    }
    Some(total)
}

/// 圧縮用にヘッダーを書き換え
// compression feature 有効時のみ呼び出される
#[cfg(feature = "compression")]
pub(super) fn build_compressed_headers(
    original_headers: &[u8],
    encoding: Option<AcceptedEncoding>,
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
    // `[server] server_header_enabled` なら自分の `Server` にする（B-271。`remove_response_headers` の `Server` は自分のものも消す）
    let own_server = crate::pool::append_own_server_line(&mut new_headers, security);

    // 元のヘッダーをコピー（Content-Length, Content-Encoding, Transfer-Encoding を除く）
    // 削除対象のヘッダーも除外
    // アロケーションなしで大文字小文字無視ヘッダー比較
    for header in response.headers.iter() {
        let n = header.name.as_bytes();
        if n.eq_ignore_ascii_case(b"content-length")
            || n.eq_ignore_ascii_case(b"content-encoding")
            || n.eq_ignore_ascii_case(b"transfer-encoding")
            || (own_server && n.eq_ignore_ascii_case(b"server"))
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

    // Content-Encoding を追加（`None` は圧縮に失敗して素のまま送る本文。長さだけ付け直す）
    if let Some(encoding) = encoding {
        new_headers.extend_from_slice(b"Content-Encoding: ");
        new_headers.extend_from_slice(encoding.as_header_value());
        new_headers.extend_from_slice(b"\r\n");
    }

    // Content-Length を追加（圧縮後のサイズ）
    new_headers.extend_from_slice(b"Content-Length: ");
    let mut len_buf = itoa::Buffer::new();
    new_headers.extend_from_slice(len_buf.format(compressed_length).as_bytes());
    new_headers.extend_from_slice(b"\r\n");

    // Vary ヘッダーを追加（キャッシュ制御）
    if encoding.is_some() {
        new_headers.extend_from_slice(b"Vary: Accept-Encoding\r\n");
    }

    // 追加するヘッダーを追加
    for (header_name, header_value) in &security.add_response_headers {
        new_headers.extend_from_slice(header_name.as_bytes());
        new_headers.extend_from_slice(b": ");
        new_headers.extend_from_slice(header_value.as_bytes());
        new_headers.extend_from_slice(b"\r\n");
    }
    // F-94: HTTP/3 広告（Alt-Svc）
    crate::pool::append_own_alt_svc_line(&mut new_headers, security);

    // ヘッダー終端
    new_headers.extend_from_slice(b"\r\n");
    // `[route.traffic.cors]` の実際の応答のヘッダ（F-273）
    crate::route_cors::apply_h1(&mut new_headers, security);

    new_headers
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
