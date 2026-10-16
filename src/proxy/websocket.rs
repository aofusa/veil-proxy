//! WebSocket のプロキシ
//!
//! F-233 で `src/proxy.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// WebSocket プロキシ処理
// ====================
//
// WebSocket アップグレードリクエストを検出し、双方向転送を行います。
//
// フロー:
// 1. クライアントから Upgrade: websocket リクエストを受信
// 2. バックエンドに接続し、アップグレードリクエストを転送
// 3. バックエンドから 101 Switching Protocols を受信
// 4. クライアントに 101 を転送
// 5. 以降は双方向でバイトストリームを透過的に転送
// 6. どちらかが接続を閉じるまで継続
// ====================

/// WebSocket プロキシ処理（双方向転送）
///
/// HTTP Upgrade をバックエンドに転送し、成功後は双方向のバイト転送を行う。
/// WebSocket 接続が終了するまでブロックし、終了後はクライアント接続も閉じる。
///
/// # Returns
/// Some((status_code, bytes_transferred)) - 成功時
/// None - エラー時
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_websocket_proxy(
    client_stream: ServerTls,
    target: &ProxyTarget,
    tls_profile: crate::upstream_tls::TlsProfile,
    security: &SecurityConfig,
    method: &[u8],
    req_path: &[u8],
    prefix: &[u8],
    headers: &[(Bytes, Bytes)],
    initial_body: &[u8],
) -> Option<(u16, u64)> {
    let connect_timeout = Duration::from_secs(security.backend_connect_timeout_secs);

    // リクエストパス構築
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    let sub_path = if prefix.is_empty() {
        path_str.to_string()
    } else {
        let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
        if let Some(remaining) = path_str.strip_prefix(prefix_str) {
            let base = target.path_prefix.trim_end_matches('/');
            build_sub_path(base, remaining)
        } else {
            path_str.to_string()
        }
    };

    let final_path = if sub_path.is_empty() { "/" } else { &sub_path };

    // WebSocket アップグレードリクエスト構築（プール使用）
    // Connection: Upgrade と Upgrade: websocket を維持
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

    // すべてのヘッダーを転送（Host 以外）
    for (name, value) in headers {
        if name.eq_ignore_ascii_case(b"host") {
            continue;
        }

        if !is_valid_header_name(name) || !is_valid_header_value(value) {
            continue;
        }

        request.extend_from_slice(name);
        request.extend_from_slice(HEADER_COLON);
        request.extend_from_slice(value);
        request.extend_from_slice(HEADER_CRLF);
    }
    request.extend_from_slice(HEADER_CRLF);

    // 初期ボディがあれば追加
    if !initial_body.is_empty() {
        request.extend_from_slice(initial_body);
    }

    // WebSocketポーリング設定を取得
    let poll_config = security.websocket_poll_config();

    // バックエンドに接続
    if target.use_tls {
        // HTTPS バックエンドへの WebSocket
        handle_websocket_proxy_https(
            client_stream,
            target,
            tls_profile,
            connect_timeout,
            request,
            &poll_config,
        )
        .await
    } else {
        // HTTP バックエンドへの WebSocket
        handle_websocket_proxy_http(
            client_stream,
            target,
            connect_timeout,
            request,
            &poll_config,
        )
        .await
    }
}

/// HTTP バックエンドへの WebSocket プロキシ
pub(super) async fn handle_websocket_proxy_http(
    mut client_stream: ServerTls,
    target: &ProxyTarget,
    connect_timeout: Duration,
    request: Vec<u8>,
    poll_config: &WebSocketPollConfig,
) -> Option<(u16, u64)> {
    // バックエンドに接続
    let addr = target.conn_addr(); // F-41/F-170: スタック上に構築（ヒープ確保なし、UDS 対応。TCP は従来と不変）
    let addr = addr.as_str();
    let connect_result = timeout(connect_timeout, connect_target(target, addr)).await;

    let mut backend_stream = match connect_result {
        Ok(Ok(stream)) => {
            let _ = stream.set_nodelay(true);
            stream
        }
        Ok(Err(e)) => {
            error!("WebSocket proxy connect error: {}", e);
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((502, 0));
        }
        Err(_) => {
            let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((504, 0));
        }
    };

    // アップグレードリクエストを送信
    let (write_res, returned_request) = backend_stream.write_all(request).await;
    request_buf_put(returned_request);
    if write_res.is_err() {
        let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
        let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
        return Some((502, 0));
    }

    // バックエンドからのレスポンスを読み取り
    let mut response_buf = Vec::with_capacity(4096);
    let status_code;

    loop {
        // B-17: ハンドシェイク応答の読取にタイムアウトを設け、無応答上流で
        // クライアントを待たせない
        let buf = buf_get();
        let read_result = timeout(BACKEND_HEADER_TIMEOUT, backend_stream.read(buf)).await;
        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => {
                let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                return Some((504, 0));
            }
        };

        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                return Some((502, 0));
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                return Some((502, 0));
            }
        };

        returned_buf.set_valid_len(n);
        response_buf.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);

        // レスポンスヘッダーを解析
        if let Some(parsed) = parse_http_response(&response_buf) {
            status_code = parsed.status_code;

            // クライアントにレスポンスを転送
            let resp_data = response_buf.clone();
            let (write_res, _) = client_stream.write_all(resp_data).await;
            if write_res.is_err() {
                return None;
            }

            // 101 Switching Protocols の場合は双方向転送開始
            if status_code == 101 {
                debug!("WebSocket upgrade successful, starting bidirectional transfer");
                let total = websocket_bidirectional_transfer(
                    &mut client_stream,
                    &mut backend_stream,
                    poll_config,
                )
                .await;
                return Some((101, total));
            } else {
                // アップグレード失敗（通常の HTTP レスポンス）
                return Some((status_code, response_buf.len() as u64));
            }
        }

        // ヘッダーが大きすぎる
        if response_buf.len() > MAX_HEADER_SIZE {
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((502, 0));
        }
    }
}

/// HTTPS バックエンドへの WebSocket プロキシ
pub(super) async fn handle_websocket_proxy_https(
    mut client_stream: ServerTls,
    target: &ProxyTarget,
    tls_profile: crate::upstream_tls::TlsProfile,
    connect_timeout: Duration,
    request: Vec<u8>,
    poll_config: &WebSocketPollConfig,
) -> Option<(u16, u64)> {
    // バックエンドに TCP 接続（事前解決済み SocketAddr があればブロッキング DNS を迂回）
    let addr = target.conn_addr(); // F-41/F-170: スタック上に構築（ヒープ確保なし、UDS 対応。TCP は従来と不変）
    let addr = addr.as_str();
    let connect_result = timeout(connect_timeout, connect_target(target, addr)).await;

    let backend_tcp = match connect_result {
        Ok(Ok(stream)) => {
            let _ = stream.set_nodelay(true);
            stream
        }
        Ok(Err(e)) => {
            error!("WebSocket proxy connect error: {}", e);
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((502, 0));
        }
        Err(_) => {
            let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((504, 0));
        }
    };

    // TLS 接続
    let connector = crate::config::tls_connector(tls_profile);
    let tls_result = timeout(
        connect_timeout,
        connector.connect(backend_tcp, &target.host),
    )
    .await;

    let mut backend_stream = match tls_result {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            error!("WebSocket TLS connect error: {}", e);
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((502, 0));
        }
        Err(_) => {
            let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((504, 0));
        }
    };

    // アップグレードリクエストを送信
    let (write_res, returned_request) = backend_stream.write_all(request).await;
    request_buf_put(returned_request);
    if write_res.is_err() {
        let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
        let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
        return Some((502, 0));
    }

    // バックエンドからのレスポンスを読み取り
    let mut response_buf = Vec::with_capacity(4096);
    let status_code;

    loop {
        // B-17: ハンドシェイク応答の読取にタイムアウトを設け、無応答上流で
        // クライアントを待たせない
        let buf = buf_get();
        let read_result = timeout(BACKEND_HEADER_TIMEOUT, backend_stream.read(buf)).await;
        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => {
                let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                return Some((504, 0));
            }
        };

        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                return Some((502, 0));
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                return Some((502, 0));
            }
        };

        returned_buf.set_valid_len(n);
        response_buf.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);

        // レスポンスヘッダーを解析
        if let Some(parsed) = parse_http_response(&response_buf) {
            status_code = parsed.status_code;

            // クライアントにレスポンスを転送
            let resp_data = response_buf.clone();
            let (write_res, _) = client_stream.write_all(resp_data).await;
            if write_res.is_err() {
                return None;
            }

            // 101 Switching Protocols の場合は双方向転送開始
            if status_code == 101 {
                debug!("WebSocket upgrade successful (TLS), starting bidirectional transfer");
                let total = websocket_bidirectional_transfer_tls(
                    &mut client_stream,
                    &mut backend_stream,
                    poll_config,
                )
                .await;
                return Some((101, total));
            } else {
                // アップグレード失敗
                return Some((status_code, response_buf.len() as u64));
            }
        }

        // ヘッダーが大きすぎる
        if response_buf.len() > MAX_HEADER_SIZE {
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((502, 0));
        }
    }
}

/// WebSocket 双方向転送（HTTP バックエンド）
///
/// クライアント ⇔ バックエンド間でデータを双方向に転送。
/// 2 方向の読み取りを、タイムアウト付きで交互に待つ（select の代わり）。
///
/// ## ポーリングモード
///
/// ### Fixed モード
/// 設定されたタイムアウト値を固定で使用。
/// 低レイテンシが最優先の場合（リアルタイムゲームなど）に推奨。
///
/// ### Adaptive モード（デフォルト）
/// データ転送があればタイムアウトをリセット（初期値に戻す）。
/// アイドル時はバックオフ方式でタイムアウトを延長（最大値まで）。
/// CPU効率とレイテンシのバランスを取る場合に推奨。
///
/// ## 将来的な改善
///
/// 2 方向の読み取りを `FuturesUnordered` 等で同時に待つイベント駆動型に移行すれば、
/// タイムアウトの周回が無くなる（同時待ちで `futures::join!` を使わない。F-173）。
pub(super) async fn websocket_bidirectional_transfer(
    client: &mut ServerTls,
    backend: &mut TcpStream,
    poll_config: &WebSocketPollConfig,
) -> u64 {
    let mut total = 0u64;

    // 現在のタイムアウト値（Adaptive モードで動的に変更）
    let mut current_timeout_ms = poll_config.initial_timeout_ms;

    loop {
        let poll_timeout = Duration::from_millis(current_timeout_ms);
        let mut had_activity = false;

        // クライアント → バックエンド
        let client_buf = buf_get();
        let read_result = timeout(poll_timeout, client.read(client_buf)).await;

        match read_result {
            Ok((Ok(0), buf)) => {
                buf_put(buf);
                break; // クライアントが接続を閉じた
            }
            Ok((Ok(n), mut buf)) => {
                buf.set_valid_len(n);
                let write_buf = buf.into_truncated();
                let (write_res, returned) = backend.write_all(write_buf).await;
                buf_put_vec(returned);
                if write_res.is_err() {
                    break;
                }
                total += n as u64;
                had_activity = true;
            }
            Ok((Err(_), buf)) => {
                buf_put(buf);
                break;
            }
            Err(_) => {
                // タイムアウト - 反対方向をチェック
            }
        }

        // バックエンド → クライアント
        let backend_buf = buf_get();
        let read_result = timeout(poll_timeout, backend.read(backend_buf)).await;

        match read_result {
            Ok((Ok(0), buf)) => {
                buf_put(buf);
                break; // バックエンドが接続を閉じた
            }
            Ok((Ok(n), mut buf)) => {
                buf.set_valid_len(n);
                let write_buf = buf.into_truncated();
                let (write_res, returned) = client.write_all(write_buf).await;
                buf_put_vec(returned);
                if write_res.is_err() {
                    break;
                }
                total += n as u64;
                had_activity = true;
            }
            Ok((Err(_), buf)) => {
                buf_put(buf);
                break;
            }
            Err(_) => {
                // タイムアウト - ループ継続
            }
        }

        // Adaptive モードでのタイムアウト調整
        if poll_config.mode == WebSocketPollMode::Adaptive {
            if had_activity {
                // データ転送があった場合: タイムアウトをリセット（初期値に戻す）
                current_timeout_ms = poll_config.initial_timeout_ms;
            } else {
                // タイムアウトした場合: バックオフ（最大値まで延長）
                let new_timeout =
                    (current_timeout_ms as f64 * poll_config.backoff_multiplier) as u64;
                current_timeout_ms = new_timeout.min(poll_config.max_timeout_ms);
            }
        }
        // Fixed モードでは current_timeout_ms は変更されない
    }

    total
}

/// WebSocket 双方向転送（HTTPS バックエンド）
///
/// HTTP版と同様のポーリングモード（Fixed/Adaptive）をサポート。
/// 詳細は `websocket_bidirectional_transfer` のドキュメントを参照。
pub(super) async fn websocket_bidirectional_transfer_tls(
    client: &mut ServerTls,
    backend: &mut ClientTls,
    poll_config: &WebSocketPollConfig,
) -> u64 {
    let mut total = 0u64;

    // 現在のタイムアウト値（Adaptive モードで動的に変更）
    let mut current_timeout_ms = poll_config.initial_timeout_ms;

    loop {
        let poll_timeout = Duration::from_millis(current_timeout_ms);
        let mut had_activity = false;

        // クライアント → バックエンド
        let client_buf = buf_get();
        let read_result = timeout(poll_timeout, client.read(client_buf)).await;

        match read_result {
            Ok((Ok(0), buf)) => {
                buf_put(buf);
                break;
            }
            Ok((Ok(n), mut buf)) => {
                buf.set_valid_len(n);
                let write_buf = buf.into_truncated();
                let (write_res, returned) = backend.write_all(write_buf).await;
                buf_put_vec(returned);
                if write_res.is_err() {
                    break;
                }
                total += n as u64;
                had_activity = true;
            }
            Ok((Err(_), buf)) => {
                buf_put(buf);
                break;
            }
            Err(_) => {}
        }

        // バックエンド → クライアント
        let backend_buf = buf_get();
        let read_result = timeout(poll_timeout, backend.read(backend_buf)).await;

        match read_result {
            Ok((Ok(0), buf)) => {
                buf_put(buf);
                break;
            }
            Ok((Ok(n), mut buf)) => {
                buf.set_valid_len(n);
                let write_buf = buf.into_truncated();
                let (write_res, returned) = client.write_all(write_buf).await;
                buf_put_vec(returned);
                if write_res.is_err() {
                    break;
                }
                total += n as u64;
                had_activity = true;
            }
            Ok((Err(_), buf)) => {
                buf_put(buf);
                break;
            }
            Err(_) => {}
        }

        // Adaptive モードでのタイムアウト調整
        if poll_config.mode == WebSocketPollMode::Adaptive {
            if had_activity {
                // データ転送があった場合: タイムアウトをリセット（初期値に戻す）
                current_timeout_ms = poll_config.initial_timeout_ms;
            } else {
                // タイムアウトした場合: バックオフ（最大値まで延長）
                let new_timeout =
                    (current_timeout_ms as f64 * poll_config.backoff_multiplier) as u64;
                current_timeout_ms = new_timeout.min(poll_config.max_timeout_ms);
            }
        }
        // Fixed モードでは current_timeout_ms は変更されない
    }

    total
}
