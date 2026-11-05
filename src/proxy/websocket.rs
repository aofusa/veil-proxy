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
    content_length: usize,
    client_ip: &str,
    host: &[u8],
) -> Option<(u16, u64)> {
    let connect_timeout = Duration::from_secs(security.backend_connect_timeout_secs);

    // リクエストパス構築
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    let final_path = ws_upstream_path(target, path_str, prefix);
    let final_path = final_path.as_str();

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

    // ヘッダーを転送する。Host（上で書いた）・Connection と Upgrade 以外のホップバイホップ・
    // `remove_request_headers` に入っている名前・不正な名前と値は送らず、`add_request_headers` を足す
    // （以前は削除・追加の指定が WebSocket の経路にだけ効いていなかった。F-236 の再走査の F2）。
    let added = expanded_added_headers(security, client_ip, host, path_str);
    // B-307: `Connection` は `Upgrade` だけにし、`Connection` に挙がったホップバイホップのヘッダは送らない
    request.extend_from_slice(b"Connection: Upgrade\r\n");
    for (name, value) in headers {
        if name.eq_ignore_ascii_case(b"host")
            || name.eq_ignore_ascii_case(b"connection")
            || (!name.eq_ignore_ascii_case(b"upgrade")
                && crate::http_utils::connection_lists(headers, name))
            || name.eq_ignore_ascii_case(b"keep-alive")
            || name.eq_ignore_ascii_case(b"transfer-encoding")
            || name.eq_ignore_ascii_case(b"proxy-connection")
            || name.eq_ignore_ascii_case(b"te")
            || !forward_allowed(security, name)
            || !added.keeps(name)
        {
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
    for (name, value) in added.iter() {
        request.extend_from_slice(name);
        request.extend_from_slice(HEADER_COLON);
        request.extend_from_slice(value);
        request.extend_from_slice(HEADER_CRLF);
    }
    request.extend_from_slice(HEADER_CRLF);

    // 要求の本文（Content-Length の分）だけをハンドシェイクと一緒に送る。それより後ろのバイトは
    // 上流が 101 を返したときだけ送る（WebSocket のフレーム）。上流がアップグレードしなければ捨てる
    // （以前はハンドシェイクの後ろに全部付けて送り、アップグレードしない上流では veil が経路・検査を
    // 通していない次の要求として処理された。F-236 の再走査の F6）。
    let body_len = content_length.min(initial_body.len());
    request.extend_from_slice(&initial_body[..body_len]);
    let after_upgrade = &initial_body[body_len..];

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
            after_upgrade,
            &poll_config,
            security,
        )
        .await
    } else {
        // HTTP バックエンドへの WebSocket
        handle_websocket_proxy_http(
            client_stream,
            target,
            connect_timeout,
            request,
            after_upgrade,
            &poll_config,
            security,
        )
        .await
    }
}

/// WebSocket の上流へ送るパス（ルートの接頭辞を外して上流の `path_prefix` を前に付ける。HTTP/1.1 と
/// HTTP/2・HTTP/3 の拡張 CONNECT（F-288）で共有）
pub(super) fn ws_upstream_path(target: &ProxyTarget, path_str: &str, prefix: &[u8]) -> String {
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
    if sub_path.is_empty() {
        "/".to_string()
    } else {
        sub_path
    }
}

/// HTTP バックエンドへの WebSocket プロキシ
pub(super) async fn handle_websocket_proxy_http(
    mut client_stream: ServerTls,
    target: &ProxyTarget,
    connect_timeout: Duration,
    request: Vec<u8>,
    after_upgrade: &[u8],
    poll_config: &WebSocketPollConfig,
    security: &SecurityConfig,
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
            crate::metrics::record_upstream_connect_error(
                crate::metrics::UpstreamConnectError::Error,
            );
            crate::logging::log_throttled!(error, "WebSocket proxy connect error: {}", e);
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

    websocket_handshake_and_relay(
        &mut client_stream,
        &mut backend_stream,
        request,
        after_upgrade,
        security,
        poll_config.idle_timeout,
    )
    .await
}

/// HTTPS バックエンドへの WebSocket プロキシ
pub(super) async fn handle_websocket_proxy_https(
    mut client_stream: ServerTls,
    target: &ProxyTarget,
    tls_profile: crate::upstream_tls::TlsProfile,
    connect_timeout: Duration,
    request: Vec<u8>,
    after_upgrade: &[u8],
    poll_config: &WebSocketPollConfig,
    security: &SecurityConfig,
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
            crate::metrics::record_upstream_connect_error(
                crate::metrics::UpstreamConnectError::Error,
            );
            crate::logging::log_throttled!(error, "WebSocket proxy connect error: {}", e);
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
            crate::metrics::record_upstream_connect_error(
                crate::metrics::UpstreamConnectError::Tls,
            );
            crate::logging::log_throttled!(error, "WebSocket TLS connect error: {}", e);
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

    websocket_handshake_and_relay(
        &mut client_stream,
        &mut backend_stream,
        request,
        after_upgrade,
        security,
        poll_config.idle_timeout,
    )
    .await
}

/// 上流へアップグレードの要求を送り、応答を返す。101 なら双方向の中継に移る（B-306・B-307）。
///
/// - 1xx（101 以外）は読み捨てる。
/// - 応答の head は通常のプロキシと同じく書き直す（`remove_response_headers`・`add_response_headers`・`Server`・Alt-Svc）。
/// - 101 以外は本文（`Content-Length`・chunked・接続の終わりまで）も返してから閉じる（以前は head を読んだ時点で受け取って
///   いたバイトだけを返し、本文を切り詰めていた）。
async fn websocket_handshake_and_relay<B: RelayIo>(
    client_stream: &mut ServerTls,
    backend_stream: &mut B,
    request: Vec<u8>,
    after_upgrade: &[u8],
    security: &SecurityConfig,
    idle_timeout: Option<Duration>,
) -> Option<(u16, u64)> {
    let (write_res, returned_request) =
        match timeout(WRITE_TIMEOUT, backend_stream.write_all(request)).await {
            Ok(r) => r,
            Err(_) => {
                let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                return Some((504, 0));
            }
        };
    request_buf_put(returned_request);
    if write_res.is_err() {
        let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
        let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
        return Some((502, 0));
    }

    // 上流の応答の head を読む（B-17: 時間切れで無応答の上流にクライアントを待たせない）
    let mut response_buf = Vec::with_capacity(4096);
    let parsed = loop {
        let buf = buf_get();
        let (res, mut returned_buf) =
            match timeout(BACKEND_HEADER_TIMEOUT, backend_stream.read(buf)).await {
                Ok(result) => result,
                Err(_) => {
                    let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                    return Some((504, 0));
                }
            };
        let n = match res {
            Ok(n) if n > 0 => n,
            _ => {
                buf_put(returned_buf);
                let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                return Some((502, 0));
            }
        };
        returned_buf.set_valid_len(n);
        response_buf.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);

        // B-307: 101 以外の 1xx は最終の応答ではない
        crate::http_utils::drain_interim_responses(&mut response_buf);
        if let Some(parsed) = parse_http_response(&response_buf) {
            break parsed;
        }
        // B-234: ヘッダが 65 個以上・不正な応答は待たずに 502
        if response_buf.len() > MAX_HEADER_SIZE
            || crate::http_utils::response_head_invalid(&response_buf)
        {
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((502, 0));
        }
    };
    let status_code = parsed.status_code;
    let head = super::compress::render_response_head(
        &response_buf[..parsed.header_len],
        status_code,
        security,
    );
    let rest = &response_buf[parsed.header_len..];

    if status_code == 101 {
        debug!("WebSocket upgrade successful, starting bidirectional transfer");
        let mut out = head;
        // 101 の後ろに上流のフレームが続いて届いていれば一緒に送る
        out.extend_from_slice(rest);
        let early = rest.len() as u64;
        match timeout(WRITE_TIMEOUT, client_stream.write_all(out)).await {
            Ok((Ok(_), _)) => {}
            _ => return None,
        }
        // ハンドシェイクと同じ読み取りで届いていたクライアントのフレーム
        if !after_upgrade.is_empty() {
            match timeout(
                WRITE_TIMEOUT,
                backend_stream.write_all(after_upgrade.to_vec()),
            )
            .await
            {
                Ok((Ok(_), _)) => {}
                _ => return Some((101, early)),
            }
        }
        let total = websocket_relay(client_stream, backend_stream, idle_timeout).await;
        return Some((101, early + after_upgrade.len() as u64 + total));
    }

    // アップグレードしなかった: 応答全体を返して閉じる（B-307）
    let mut sent = rest.len() as u64;
    let mut out = head;
    out.extend_from_slice(rest);
    match timeout(WRITE_TIMEOUT, client_stream.write_all(out)).await {
        Ok((Ok(_), _)) => {}
        _ => return None,
    }
    // 本文の残り
    enum Framing {
        Length(u64),
        Chunked(crate::http_utils::ChunkedDecoder),
        UntilClose,
        Done,
    }
    let no_body = (100..200).contains(&status_code) || status_code == 204 || status_code == 304;
    let mut framing = if no_body {
        Framing::Done
    } else if parsed.is_chunked {
        let mut d = crate::http_utils::ChunkedDecoder::new(0);
        match d.feed_counted(rest).0 {
            crate::http_utils::ChunkedFeedResult::Continue => Framing::Chunked(d),
            _ => Framing::Done,
        }
    } else if let Some(cl) = parsed.content_length {
        let cl = cl as u64;
        if sent >= cl {
            Framing::Done
        } else {
            Framing::Length(cl - sent)
        }
    } else {
        Framing::UntilClose
    };
    while !matches!(framing, Framing::Done) {
        let buf = buf_get();
        let (res, mut buf) = match timeout(READ_TIMEOUT, backend_stream.read(buf)).await {
            Ok(r) => r,
            Err(_) => break,
        };
        let n = match res {
            Ok(n) if n > 0 => n,
            _ => {
                buf_put(buf);
                break;
            }
        };
        buf.set_valid_len(n);
        let mut take = n;
        framing = match framing {
            Framing::Length(left) => {
                take = (left.min(n as u64)) as usize;
                if left <= n as u64 {
                    Framing::Done
                } else {
                    Framing::Length(left - n as u64)
                }
            }
            Framing::Chunked(mut d) => {
                let (r, used) = d.feed_counted(&buf.as_valid_slice()[..n]);
                take = used;
                match r {
                    crate::http_utils::ChunkedFeedResult::Continue => Framing::Chunked(d),
                    _ => Framing::Done,
                }
            }
            other => other,
        };
        buf.set_valid_len(take);
        match timeout(WRITE_TIMEOUT, client_stream.write_all(buf.into_truncated())).await {
            Ok((Ok(_), returned)) => buf_put_vec(returned),
            _ => break,
        }
        sent += take as u64;
    }
    Some((status_code, sent))
}

/// 中継する片側（B-306）。読み取り可能の待ちに使う TCP と、TLS が手元に抱えた未読の平文の有無
pub(super) trait RelayIo:
    crate::runtime::io::AsyncReadRent
    + crate::runtime::io::AsyncWriteRentExt
    + crate::runtime::io::BufferedReadState
{
    fn relay_tcp(&self) -> &TcpStream;
}

impl RelayIo for TcpStream {
    #[inline]
    fn relay_tcp(&self) -> &TcpStream {
        self
    }
}

impl RelayIo for ServerTls {
    #[inline]
    fn relay_tcp(&self) -> &TcpStream {
        self.get_ref()
    }
}

impl RelayIo for ClientTls {
    #[inline]
    fn relay_tcp(&self) -> &TcpStream {
        self.get_ref()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    A,
    B,
}

/// 片側の読み取り可能の待ちを起こした印（どちらの待ちが起きたかを親に伝える。F-173: 起きていない側を
/// poll し直さない＝reactor で `poll(2)` を空打ちしない）
struct SideWaker {
    woken: std::sync::atomic::AtomicBool,
    parent: futures::task::AtomicWaker,
}

impl std::task::Wake for SideWaker {
    fn wake(self: std::sync::Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &std::sync::Arc<Self>) {
        self.woken.store(true, std::sync::atomic::Ordering::Release);
        self.parent.wake();
    }
}

/// 両側の読み取り可能を同時に待つ（読み取り可能の待ちは drop しても受信したデータを失わない。B-306）
async fn ready_either(
    a: &TcpStream,
    b: &TcpStream,
    wakers: &(
        std::sync::Arc<SideWaker>,
        std::task::Waker,
        std::sync::Arc<SideWaker>,
        std::task::Waker,
    ),
    prefer_b: bool,
) -> io::Result<Side> {
    use std::future::Future;
    use std::sync::atomic::Ordering;
    use std::task::{Context, Poll};
    let (fa, wa, fb, wb) = wakers;
    // 最初は両方を poll する
    fa.woken.store(true, Ordering::Relaxed);
    fb.woken.store(true, Ordering::Relaxed);
    let mut ra = std::pin::pin!(a.readable());
    let mut rb = std::pin::pin!(b.readable());
    std::future::poll_fn(|cx| {
        fa.parent.register(cx.waker());
        fb.parent.register(cx.waker());
        let order = if prefer_b {
            [Side::B, Side::A]
        } else {
            [Side::A, Side::B]
        };
        for side in order {
            let (flag, waker, fut) = match side {
                Side::A => (fa, wa, ra.as_mut()),
                Side::B => (fb, wb, rb.as_mut()),
            };
            if flag.woken.swap(false, Ordering::AcqRel) {
                if let Poll::Ready(r) = fut.poll(&mut Context::from_waker(waker)) {
                    return Poll::Ready(r.map(|()| side));
                }
            }
        }
        Poll::Pending
    })
    .await
}

/// `from` から 1 回読んで `to` へ全部書く（読めたバイト数。終わり・エラー・時間切れは None）
async fn relay_once<R: RelayIo, W: RelayIo>(
    from: &mut R,
    to: &mut W,
    wait: Duration,
) -> Option<u64> {
    let buf = buf_get();
    // 読み取り可能になった側（または TLS が平文を抱えている側）なので普通はすぐ返る。TLS のレコードの途中で
    // 止まった相手は時間切れで閉じる（閉じるので、取り消しで受信データを失っても構わない）
    let (res, mut buf) = match timeout(wait, from.read(buf)).await {
        Ok(r) => r,
        Err(_) => return None,
    };
    let n = match res {
        Ok(0) | Err(_) => {
            buf_put(buf);
            return None;
        }
        Ok(n) => n,
    };
    buf.set_valid_len(n);
    // 読まない相手で中継が止まり続けないよう、書き込みに時間の上限（B-306）
    match timeout(WRITE_TIMEOUT, to.write_all(buf.into_truncated())).await {
        Ok((Ok(_), returned)) => {
            buf_put_vec(returned);
            Some(n as u64)
        }
        Ok((Err(_), returned)) => {
            buf_put_vec(returned);
            None
        }
        Err(_) => None,
    }
}

/// アイドルの上限が無いときの 1 回の待ちの長さ（満了したら待ち直す）
const RELAY_WAIT_CHUNK: Duration = Duration::from_secs(3600);

/// WebSocket（アップグレード後のトンネル）の双方向の中継（B-306）。
///
/// 両側の読み取り可能を同時に待ち、起きた側から読んで反対側へ書く（以前は両側を短い時間切れで交互に読み、
/// io_uring では時間切れで drop した読み取りが受信済みのデータを捨て得た）。待っている間は SQE もタイマーの
/// 周回も出さない。`idle_timeout`（`websocket_idle_timeout_secs`）は両方向の最後の活動から数え、graceful
/// shutdown（F-187）は待ちを満了させてトンネルを閉じる（B-308）。
pub(super) async fn websocket_relay<A: RelayIo, B: RelayIo>(
    client: &mut A,
    backend: &mut B,
    idle_timeout: Option<Duration>,
) -> u64 {
    let _tunnel = crate::metrics::websocket_tunnel_opened("http/1.1");
    let wakers = {
        let mk = || {
            let f = std::sync::Arc::new(SideWaker {
                woken: std::sync::atomic::AtomicBool::new(true),
                parent: futures::task::AtomicWaker::new(),
            });
            let w = std::task::Waker::from(f.clone());
            (f, w)
        };
        let (fa, wa) = mk();
        let (fb, wb) = mk();
        (fa, wa, fb, wb)
    };
    let wait = idle_timeout.unwrap_or(RELAY_WAIT_CHUNK);
    let mut total = 0u64;
    let mut prefer_b = false;
    loop {
        let side = if client.has_buffered_read_data() {
            Side::A
        } else if backend.has_buffered_read_data() {
            Side::B
        } else {
            let started = std::time::Instant::now();
            match crate::runtime::time::timeout_idle(
                wait,
                true,
                ready_either(client.relay_tcp(), backend.relay_tcp(), &wakers, prefer_b),
            )
            .await
            {
                Ok(Ok(side)) => side,
                // 待ちのエラーは読み取りで確かめる（EOF・エラーで閉じる）
                Ok(Err(_)) => {
                    if prefer_b {
                        Side::B
                    } else {
                        Side::A
                    }
                }
                Err(_) => {
                    // 上限が無く、満了まで待った（graceful shutdown ではない）なら待ち直す
                    if idle_timeout.is_none() && started.elapsed() >= wait {
                        continue;
                    }
                    debug!("WebSocket tunnel idle or shutting down; closing");
                    break;
                }
            }
        };
        // 公平に: 次は反対側を先に見る
        prefer_b = side == Side::A;
        let moved = match side {
            Side::A => relay_once(client, backend, wait).await,
            Side::B => relay_once(backend, client, wait).await,
        };
        match moved {
            Some(n) => total += n,
            None => break,
        }
    }
    total
}
