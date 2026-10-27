//! kTLS + splice(2) の HTTP プロキシ
//!
//! F-233 で `src/proxy.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// kTLS + splice(2) によるHTTPプロキシ（高速版）
// ====================
//
// kTLS が有効な場合、splice(2) を使用してカーネル空間で直接
// データを転送します。HTTPバックエンド（平文）への接続で効果的です。
//
// 注意: Chunked 転送の場合は終端検出のためユーザー空間での
// 処理が必要なため、splice は使用しません。
// ====================

/// kTLS + splice によるボディ転送（Content-Length固定長のみ）
///
/// `src → pipe → dst` の 2 段 splice を **io_uring（IORING_OP_SPLICE）の非同期 Future**
/// で発行し、カーネル内ゼロコピー転送を行う（F-39）。同期 `libc::splice` は使用しない。
/// データ/空きが無い場合は `WouldBlock` が返るため、`readable()` / `writable()`
/// （POLL_ADD）で待機してから再試行する（L4 の `forward_direction_splice` と同方式）。
///
/// pipe に取り込んだ n バイトは dst のバックプレッシャに追従して**必ず全量ドレイン**
/// してから次のチャンクへ進む（pipe 内残データと `remaining` のずれによるデータ損失を防ぐ）。
#[cfg(all(veil_ktls, target_os = "linux"))]
pub(super) async fn splice_body_transfer(
    src_stream: &TcpStream,
    dst_stream: &TcpStream,
    pipe: &SplicePipe,
    mut remaining: usize,
) -> u64 {
    use crate::runtime::splice::{splice as iouring_splice, splice_more as iouring_splice_more};
    use std::os::unix::io::AsRawFd;

    let src_fd = src_stream.as_raw_fd();
    let dst_fd = dst_stream.as_raw_fd();
    let mut total = 0u64;

    // 設定に基づいてチャンクサイズを決定
    let chunk_size_config = {
        let config = CURRENT_CONFIG.load();
        match config.performance.chunk_size_mode {
            ChunkSizeMode::Dynamic => calculate_optimal_chunk_size(remaining as u64),
            ChunkSizeMode::Manual => config.performance.manual_chunk_size,
        }
    };

    'outer: while remaining > 0 {
        let chunk_size = remaining.min(chunk_size_config);

        // Step 1: src → pipe（io_uring 非同期 splice）
        let n = match iouring_splice(src_fd, pipe.write_fd(), chunk_size).await {
            Ok(0) => break, // src EOF
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // src にデータが無い → 読み取り可能になるまで待機（POLL_ADD）
                // B-17: 無応答の上流で永久待機しないよう READ_TIMEOUT で打ち切る
                match timeout(READ_TIMEOUT, src_stream.readable()).await {
                    Ok(Ok(())) => {}
                    _ => break,
                }
                continue;
            }
            Err(e) => {
                warn!("splice body transfer error (src→pipe): {}", e);
                break;
            }
        };

        // Step 2: pipe → dst（n バイトを全量ドレイン。dst のバックプレッシャに追従）
        //
        // B-25: SPLICE_F_MORE（splice_more）は「このチャンクの後に確実にデータが続く」
        // 中間チャンクのみに付与する。最終チャンクに付与すると、dst が kTLS ソケットの
        // 場合に 16KiB 未満の最終部分 TLS レコードがカーネル内で保留されたまま
        // フラッシュされず、クライアントが応答完了を永遠に待つハングになる。
        let more_follows = remaining > n;
        let mut moved = 0usize;
        while moved < n {
            let splice_step = if more_follows {
                iouring_splice_more(pipe.read_fd(), dst_fd, n - moved)
            } else {
                iouring_splice(pipe.read_fd(), dst_fd, n - moved)
            };
            match splice_step.await {
                Ok(0) => break 'outer, // dst クローズ
                Ok(m) => moved += m,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // dst 送信バッファ満杯 → 書き込み可能になるまで待機（POLL_ADD）
                    // B-17: 受信停止したクライアントで永久待機しないよう WRITE_TIMEOUT で打ち切る
                    match timeout(WRITE_TIMEOUT, dst_stream.writable()).await {
                        Ok(Ok(())) => {}
                        _ => break 'outer,
                    }
                }
                Err(e) => {
                    warn!("splice body transfer error (pipe→dst): {}", e);
                    break 'outer;
                }
            }
        }

        total += n as u64;
        remaining = remaining.saturating_sub(n);
    }

    total
}

// ====================
// kTLS + splice(2) によるHTTPプロキシ転送
// ====================
//
// kTLS が有効な場合、以下のフローでゼロコピー転送を実現：
//
// [リクエスト] クライアント(kTLS) → splice → バックエンド(TCP)
//   1. ヘッダー: raw_read で読み取り → パース → raw_write で送信
//   2. ボディ(Content-Length): splice(2) でゼロコピー転送
//   3. ボディ(Chunked): 通常の転送（終端検出が必要）
//
// [レスポンス] バックエンド(TCP) → splice → クライアント(kTLS)
//   1. ヘッダー: raw_read で読み取り → パース → raw_write で送信
//   2. ボディ(Content-Length): splice(2) でゼロコピー転送
//   3. ボディ(Chunked): 通常の転送（終端検出が必要）
// ====================

/// kTLS + splice を使用したHTTPプロキシリクエスト処理
///
/// Content-Length が指定されている場合はボディ転送に splice を使用。
/// Chunked 転送の場合は通常の転送を使用。
/// splice ベースの kTLS プロキシ転送を試みる（F-126 クロスプラットフォーム分岐点）。
///
/// splice(2) は Linux 専用のため、Linux では [`proxy_http_request_splice`] を呼ぶ。
/// FreeBSD 等の非 Linux では `None` を返し、呼び出し側のバッファ転送
/// （`proxy_http_request_with_compression`）へフォールバックさせる。FreeBSD kTLS は
/// カーネル暗号化自体は有効だが、プロキシのデータ転送は通常の write 経路を用いる。
#[cfg(all(veil_ktls, target_os = "linux"))]
pub(super) async fn try_splice_proxy(
    client_stream: &KtlsServerStream,
    backend_stream: &TcpStream,
    request: &[u8],
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
) -> Option<(u16, u64, bool, bool)> {
    proxy_http_request_splice(
        client_stream,
        backend_stream,
        request,
        content_length,
        is_chunked,
        initial_body,
    )
    .await
}

/// splice 非対応プラットフォーム（FreeBSD 等）向けのフォールバック: 常に `None` を返し
/// 呼び出し側のバッファ転送経路へ委ねる（F-126）。
#[cfg(all(veil_ktls, not(target_os = "linux")))]
pub(super) async fn try_splice_proxy(
    _client_stream: &KtlsServerStream,
    _backend_stream: &TcpStream,
    _request: &[u8],
    _content_length: usize,
    _is_chunked: bool,
    _initial_body: &[u8],
) -> Option<(u16, u64, bool, bool)> {
    None
}

#[cfg(all(veil_ktls, target_os = "linux"))]
pub(super) async fn proxy_http_request_splice(
    client_stream: &KtlsServerStream,
    backend_stream: &TcpStream,
    request: &[u8],
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
) -> Option<(u16, u64, bool, bool)> {
    // 設定に基づいてパイプを取得または作成
    let per_stream_pipe_enabled = {
        let config = CURRENT_CONFIG.load();
        config.performance.per_stream_pipe_enabled
    };

    // パイプ取得: ストリーム毎の新規パイプ or スレッドローカルプールから checkout（B-16）
    //
    // どちらの経路も所有権ベースのため、await を跨いで RefCell の借用が残らない。
    // プール checkout は Drop 時に残データ検査つきで返却される（src/pool.rs）。
    let per_stream_pipe: Option<SplicePipe> = if per_stream_pipe_enabled {
        // ストリーム毎に新規パイプを作成（高並行性環境向け）
        match SplicePipe::new() {
            Ok(p) => Some(p),
            Err(e) => {
                warn!(
                    "Failed to create per-stream splice pipe: {}, falling back to pooled pipe",
                    e
                );
                None
            }
        }
    } else {
        None
    };
    let pooled_pipe = if per_stream_pipe.is_none() {
        get_splice_pipe()
    } else {
        None
    };
    let pipe: &SplicePipe = match (per_stream_pipe.as_ref(), pooled_pipe.as_deref()) {
        (Some(p), _) => p,
        (None, Some(p)) => p,
        (None, None) => {
            warn!("splice pipe not available, falling back to normal transfer");
            return None;
        }
    };

    // kTLS が有効でない場合はフォールバック
    if !client_stream.is_ktls_enabled() {
        return None;
    }

    let client_tcp = client_stream.get_ref();

    // 1. リクエストヘッダーをバックエンドに送信（raw_write）
    if let Err(e) = async_raw_write_all(backend_stream, request).await {
        warn!("Failed to send request header: {}", e);
        return None;
    }

    // 2. 初期ボディがあれば送信
    if !initial_body.is_empty() {
        if let Err(e) = async_raw_write_all(backend_stream, initial_body).await {
            warn!("Failed to send initial body: {}", e);
            return None;
        }
    }

    // 3. 残りのリクエストボディを転送
    let remaining_body = content_length.saturating_sub(initial_body.len());
    if remaining_body > 0 {
        if is_chunked {
            // Chunked 転送はフォールバック（終端検出が必要）
            return None;
        }

        // Content-Length の場合: splice でゼロコピー転送
        // kTLS クライアント → バックエンド TCP
        let transferred =
            splice_body_transfer(client_tcp, backend_stream, pipe, remaining_body).await;

        if transferred < remaining_body as u64 {
            warn!(
                "Request body transfer incomplete: {} < {}",
                transferred, remaining_body
            );
            return None;
        }
    }

    // 4. レスポンスを受信して転送（splice 使用）
    let result = splice_transfer_response_ktls(backend_stream, client_stream, pipe).await;

    Some(result)
}

/// kTLS + splice によるレスポンス転送
///
/// バックエンド(TCP) からヘッダーを読み取り、パースしてクライアント(kTLS)に送信。
/// ボディは Content-Length の場合は splice、Chunked の場合は通常転送。
/// 戻り値: (ステータス, 転送バイト数, backend_wants_keep_alive, client_must_close)
#[cfg(all(veil_ktls, target_os = "linux"))]
pub(super) async fn splice_transfer_response_ktls(
    backend_stream: &TcpStream,
    client_stream: &KtlsServerStream,
    pipe: &SplicePipe,
) -> (u16, u64, bool, bool) {
    let client_tcp = client_stream.get_ref();

    let mut total = 0u64;
    let mut status_code = 502u16;
    let mut accumulated = Vec::with_capacity(4096);
    let mut backend_wants_keep_alive: bool;

    // ヘッダー読み取り用バッファ
    let mut header_buf = [0u8; 8192];

    // 1. ヘッダーを読み取り（raw_read + パース）
    loop {
        // バックエンドからヘッダーを読み取り
        // B-17: ヘッダー読取は専用の短いタイムアウトで打ち切り、504 へ即変換する
        let n = match timeout(
            BACKEND_HEADER_TIMEOUT,
            async_raw_read(backend_stream, &mut header_buf),
        )
        .await
        {
            Ok(Ok(0)) => {
                // B-17: ヘッダー完了前の EOF は 502 として即時応答する
                return (status_code, total, false, true);
            }
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                warn!("Failed to read response header: {}", e);
                return (status_code, total, false, true);
            }
            Err(_) => {
                warn!("Backend response header read timeout (splice)");
                return (504, total, false, true);
            }
        };

        accumulated.extend_from_slice(&header_buf[..n]);

        // B-11: バックエンド由来の 1xx 中間応答を読み捨てる。
        drain_interim_responses(&mut accumulated);

        // ヘッダーが完全に受信されたかチェック
        if let Some(parsed) = parse_http_response(&accumulated) {
            status_code = parsed.status_code;
            backend_wants_keep_alive = !parsed.is_connection_close;

            let header_len = parsed.header_len;
            let body_start_len = accumulated.len().saturating_sub(header_len);

            // ヘッダー + 初期ボディをクライアントに送信（raw_write）
            if let Err(e) = async_raw_write_all(client_tcp, &accumulated).await {
                warn!("Failed to send response header: {}", e);
                return (status_code, total, false, true);
            }
            total += accumulated.len() as u64;

            // B-17: ボディが完結しなかった場合にクライアント接続をクローズするためのフラグ
            let mut client_must_close = false;

            // ボディ転送
            if parsed.is_chunked {
                // Chunked 転送: 通常の方法で転送（終端検出が必要）
                // レスポンス受信時は制限なし（バックエンドを信頼）
                let mut chunked_decoder = ChunkedDecoder::new_unlimited();

                // 初期ボディ部分をデコーダにフィード
                if body_start_len > 0
                    && chunked_decoder.feed(&accumulated[header_len..])
                        == ChunkedFeedResult::Complete
                {
                    // 初期ボディで完了
                    return (status_code, total, backend_wants_keep_alive, false);
                }

                // 残りの Chunked ボディを転送
                loop {
                    // B-17: 無応答の上流で永久待機しないよう READ_TIMEOUT で打ち切る
                    let n = match timeout(
                        READ_TIMEOUT,
                        async_raw_read(backend_stream, &mut header_buf),
                    )
                    .await
                    {
                        Ok(Ok(0)) => {
                            // B-17: 終端チャンク前の EOF はクライアントを待たせないようクローズ
                            backend_wants_keep_alive = false;
                            client_must_close = true;
                            break;
                        }
                        Ok(Ok(n)) => n,
                        Ok(Err(_)) | Err(_) => {
                            backend_wants_keep_alive = false;
                            client_must_close = true;
                            break;
                        }
                    };

                    let feed_result = chunked_decoder.feed(&header_buf[..n]);

                    if (async_raw_write_all(client_tcp, &header_buf[..n]).await).is_err() {
                        backend_wants_keep_alive = false;
                        client_must_close = true;
                        break;
                    }
                    total += n as u64;

                    if feed_result == ChunkedFeedResult::Complete {
                        break;
                    }
                }
            } else if let Some(content_length) = parsed.content_length {
                // Content-Length 転送: splice でゼロコピー
                let remaining = content_length.saturating_sub(body_start_len);

                if remaining > 0 {
                    let transferred =
                        splice_body_transfer(backend_stream, client_tcp, pipe, remaining).await;

                    total += transferred;

                    if transferred < remaining as u64 {
                        // B-17: CL 未達のままの終了はクライアントを待たせないようクローズ
                        backend_wants_keep_alive = false;
                        client_must_close = true;
                    }
                }
            } else {
                // Content-Length も Chunked もない場合: 接続クローズまで読み取り
                // この場合は Keep-Alive 不可
                backend_wants_keep_alive = false;

                loop {
                    // B-17: 無応答の上流で永久待機しないよう READ_TIMEOUT で打ち切る
                    let n = match timeout(
                        READ_TIMEOUT,
                        async_raw_read(backend_stream, &mut header_buf),
                    )
                    .await
                    {
                        Ok(Ok(0)) => break,
                        Ok(Ok(n)) => n,
                        Ok(Err(_)) | Err(_) => break,
                    };

                    if (async_raw_write_all(client_tcp, &header_buf[..n]).await).is_err() {
                        break;
                    }
                    total += n as u64;
                }

                // 接続クローズが終端のためクライアント側もクローズが必要
                client_must_close = true;
            }

            return (
                status_code,
                total,
                backend_wants_keep_alive,
                client_must_close,
            );
        }

        // B-17: レスポンスヘッダーの上限超過は 502 で即時応答する
        // B-234: ヘッダが 65 個以上・不正な応答は、上限か読み取りのタイムアウトまで待たずに 502
        if accumulated.len() > MAX_RESPONSE_HEADER_SIZE
            || crate::http_utils::response_head_invalid(&accumulated)
        {
            warn!(
                "Backend response header too large (> {} bytes, splice) or invalid",
                MAX_RESPONSE_HEADER_SIZE
            );
            return (502, 0, false, true);
        }
    }
}
