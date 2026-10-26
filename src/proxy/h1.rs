//! HTTP/1.1 の要求処理ループ・Backend の処理・リダイレクト
//!
//! F-233 で `src/proxy.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// リクエスト処理ループ
// ====================

/// Lingering close: クライアントが送信途中のデータを一定時間読み捨ててからクローズする。
///
/// エラーレスポンス（431/414/413 等）を書き込んだ直後にクローズすると、ソケットの受信
/// バッファにクライアントが送信中のデータ（巨大ヘッダ/ボディ）が残っている場合、OS が
/// FIN ではなく RST を送出し、直前に書いたレスポンスがクライアントに届かないことがある
/// （負荷時に顕在化する「過大ヘッダで応答が空になる」フレーキーの原因）。受信データを
/// ドレインしてからクローズすることで RST を防ぎ、エラーレスポンスを確実に届ける。
/// nginx の lingering_close と同じ目的。
pub(super) async fn lingering_drain_before_close(stream: &mut ServerTls) {
    // 全体上限はやや長めに取る。単一スレッドが高負荷でスケジュールされにくい状況でも、
    // クライアントが送信中の本文（最大 ~数百KB）を読み切ってからクローズできるようにする。
    // 上限途中でも「一定時間データが来ない＝受信バッファが空」になった時点で抜ける。
    let overall = Duration::from_millis(2000);
    let start = std::time::Instant::now();
    while start.elapsed() < overall {
        let buf = buf_get();
        match timeout(Duration::from_millis(200), stream.read(buf)).await {
            Ok((res, b)) => {
                buf_put(b);
                match res {
                    Ok(0) => break,  // EOF: クライアントが書き込みをクローズ
                    Ok(_) => {}      // 読み捨てて継続（データがある間は読み続ける）
                    Err(_) => break, // 読み取りエラー
                }
            }
            Err(_) => break, // 200ms データ無し＝受信バッファは空。安全にクローズ可能
        }
    }
}

/// `add_request_headers` の値テンプレートを 1 パスで展開して `out` へ追記する（F-168 P4）。
///
/// 対応するプレースホルダは `$client_ip` / `$host` / `$request_uri` の 3 つ。
/// プレースホルダを含まない値（大半のケース）は `memchr` で `$` が 1 つも見つからず、
/// スライスをそのまま追記するだけで中間 `String` を作らない。認識できない `$` は
/// そのまま出力へ残す。
///
/// **`String::replace` の 3 連鎖との差異**: 旧実装は `$client_ip` → `$host` →
/// `$request_uri` の順に置換していたため、**先に埋め込まれた値の中に後続の
/// プレースホルダ文字列が含まれていると、それも置換されてしまう**
/// （例: クライアント IP が `$host` を含む文字列だった場合）。本関数は入力を
/// 1 回走査するだけなので、展開結果に対する再置換は起こらない。こちらが正しい挙動。
pub(super) fn expand_request_header_value(
    template: &str,
    client_ip: &str,
    host: &str,
    request_uri: &str,
    out: &mut Vec<u8>,
) {
    let bytes = template.as_bytes();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let rest = &bytes[pos..];
        let Some(dollar) = memchr::memchr(b'$', rest) else {
            out.extend_from_slice(rest);
            return;
        };
        out.extend_from_slice(&rest[..dollar]);
        pos += dollar;
        let tail = &bytes[pos..];
        // 長い名前から順に照合する（`$host` が `$hostname` の接頭辞になるような
        // 取り違えは無いが、将来プレースホルダを増やしたときのために順序を固定する）。
        if tail.starts_with(b"$request_uri") {
            out.extend_from_slice(request_uri.as_bytes());
            pos += b"$request_uri".len();
        } else if tail.starts_with(b"$client_ip") {
            out.extend_from_slice(client_ip.as_bytes());
            pos += b"$client_ip".len();
        } else if tail.starts_with(b"$host") {
            out.extend_from_slice(host.as_bytes());
            pos += b"$host".len();
        } else {
            // 認識できないプレースホルダ（末尾の裸の `$` を含む）はそのまま残す。
            out.push(b'$');
            pos += 1;
        }
    }
}

/// `haystack` 内の部分スライス `needle` の (開始オフセット, 長さ) を求める。
///
/// `needle` は必ず `haystack` の内部を指している（借用元が同一である）ことを前提とする。
/// httparse の解析結果（`Request::method`/`path`/各 `Header::value`）はすべて `req.parse()`
/// に渡した元バッファのサブスライスであり、この前提を満たす。ポインタ演算のみで完結し、
/// コピーもヒープ確保も発生しない。
#[inline]
pub(super) fn slice_range(haystack: &[u8], needle: &[u8]) -> (usize, usize) {
    let start = needle.as_ptr() as usize - haystack.as_ptr() as usize;
    debug_assert!(start <= haystack.len());
    debug_assert!(start + needle.len() <= haystack.len());
    (start, needle.len())
}

/// キープアライブの次の要求の読み方（F-205）。
pub(super) enum KeepaliveRead {
    /// 従来: 読み取りバッファへ読み、蓄積へ写す
    Buf(io::Result<usize>, SafeReadBuffer),
    /// 蓄積バッファへ直接読んだ（io_uring）
    #[cfg(veil_rt_uring)]
    Direct(io::Result<usize>, BytesMut),
}

/// 応答を返した直後に、次の要求を蓄積バッファへ直接読みにいく（F-205、io_uring）。`[performance]
/// keepalive_direct_read_ms`（既定 20ms）の間に届けば `Some`。届かなければ読みを取り消して `None`
/// （バッファは返す。呼び出し側はバッファを持たない従来の待ち方に戻る。B-101）。
///
/// RECV 1 回で受ける（`POLL_ADD` → `RECV` の 2 操作を 1 回に）。期限では ASYNC_CANCEL を投げて CQE を待つので、
/// 取り消しと行き違いで届いたデータも失わない。
///
/// epoll では入れていない: `read(2)` を先に試す形を測ったところ、外れたときの空打ちが当たりで省ける
/// `poll(2)`・`epoll_ctl` を上回り、`tools/api-bench` で −1.4〜−2.5% だった（F-205 のチケット）。
/// 平文の接続だけ（TLS は復号の状態があるので従来どおり）。
#[cfg(veil_rt_uring)]
pub(super) async fn keepalive_direct_read(
    tls_stream: &ServerTls,
) -> Option<(io::Result<usize>, BytesMut)> {
    let wait = CURRENT_CONFIG.load().performance.keepalive_direct_read();
    if wait.is_zero()
        || !tls_stream.is_plain()
        || crate::runtime::io::BufferedReadState::has_buffered_read_data(tls_stream)
    {
        return None;
    }
    let buf = crate::pool::accum_buf_get();
    let (res, buf) = tls_stream.get_ref().read_within(buf, wait).await;
    match res {
        Some(r) => Some((r, buf)),
        None => {
            crate::pool::accum_buf_put(buf);
            None
        }
    }
}

// 統一されたリクエスト処理ループ（型エイリアスを使用）
pub(super) async fn handle_requests(
    mut tls_stream: ServerTls,
    client_ip: &str,
    peer_addr: SocketAddr,
) {
    // B-101: 要求の蓄積バッファは、データが届いてからスレッドローカルのプールで借りる
    // （接続ごとに 64KB を確保して接続の寿命まで抱えない）。
    let mut accumulated = BytesMut::new();

    // アクティブ接続メトリクスの自動管理（Dropで自動デクリメント）
    let mut connection_metric = ActiveConnectionMetric::new(true);
    // 接続直後の 1 回目の読み取りか（待機先行は 2 回目以降のみ。ServerTls::wait_next_request 参照）
    let mut first_read = true;
    // B-129: 蓄積に次の要求のヘッダが揃っている（パイプライン）
    let mut pipelined_ready = false;
    // ネイティブハンドラの呼び出しで要求をまたいで使い回す状態（F-202）
    #[cfg(feature = "handler")]
    let mut api_cache = crate::api::dispatch::CallCache::default();

    loop {
        // 応答を返した直後（蓄積が空）は次のリクエストがまだ届いていないのが普通なので、
        // readiness を待ってから読み取りバッファを借りる（B-101: アイドル接続にバッファを
        // 持たせない。kqueue では先読みの EAGAIN と確認用 poll(2) も省ける）。
        // B-129: 前の要求の後ろに次の要求（パイプライン）が丸ごと届いていれば、読まずに処理する
        // （読みにいくと、もう送られてこないデータを待ち続ける）
        if !std::mem::take(&mut pipelined_ready) {
            let wait_first = !first_read && accumulated.is_empty();
            first_read = false;
            // F-187: 蓄積が空＝次の要求を待っているだけ。graceful shutdown ではこの待ちを即座に
            // 打ち切って接続を閉じる（要求の途中まで届いている接続は待つ）。
            let idle = accumulated.is_empty();
            if idle && accumulated.capacity() > 0 {
                crate::pool::accum_buf_put(std::mem::take(&mut accumulated));
            }
            let read_result = timeout_idle(IDLE_TIMEOUT, idle, async {
                if wait_first {
                    // F-205: 応答の直後は、次の要求を蓄積バッファへ直接読みにいく（届けば 1 回の操作で済む）
                    #[cfg(veil_rt_uring)]
                    if let Some((res, buf)) = keepalive_direct_read(&tls_stream).await {
                        return Ok::<_, io::Error>(KeepaliveRead::Direct(res, buf));
                    }
                    tls_stream.wait_next_request().await?;
                }
                let (res, buf) = tls_stream.read(buf_get()).await;
                Ok::<_, io::Error>(KeepaliveRead::Buf(res, buf))
            })
            .await;

            match read_result {
                Ok(Ok(KeepaliveRead::Buf(res, mut returned_buf))) => match res {
                    Ok(n) if n > 0 => {
                        // 読み込んだデータを蓄積（SafeReadBufferの型安全なアクセス）
                        returned_buf.set_valid_len(n);
                        if accumulated.capacity() == 0 {
                            accumulated = crate::pool::accum_buf_get();
                        }
                        accumulated.extend_from_slice(returned_buf.as_valid_slice());
                        buf_put(returned_buf);
                    }
                    // 0 = 相手が閉じた・読み取りエラー
                    _ => {
                        buf_put(returned_buf);
                        return;
                    }
                },
                // F-205: 蓄積バッファへ直接読んだ（写しが要らない）
                #[cfg(veil_rt_uring)]
                Ok(Ok(KeepaliveRead::Direct(res, buf))) => match res {
                    Ok(n) if n > 0 => accumulated = buf,
                    _ => {
                        crate::pool::accum_buf_put(buf);
                        return;
                    }
                },
                // アイドルタイムアウト・待機中のエラー - 接続を閉じる
                Ok(Err(_)) | Err(_) => return,
            }
        }

        // ヘッダーサイズ制限チェック（RFC 6585: 431 Request Header Fields Too Large）
        // ヘッダー終端 \r\n\r\n が見つかった場合はヘッダー部分のみをチェック。
        // まだヘッダーが完全でない場合は蓄積サイズで判定する。
        // URIが長すぎる場合は414 URI Too Long、それ以外のヘッダー超過は431
        let header_section_end = accumulated.windows(4).position(|w| w == b"\r\n\r\n");
        let header_check_size = header_section_end.map_or(accumulated.len(), |end| end + 4);
        if header_check_size > MAX_HEADER_SIZE {
            // リクエストラインの終端を探してURIサイズを確認
            let response_code =
                if let Some(line_end) = accumulated.windows(2).position(|w| w == b"\r\n") {
                    // リクエストラインが完全に含まれている場合、URIサイズを確認
                    let request_line = &accumulated[..line_end];
                    // "GET /path HTTP/1.1" 形式から URI 部分を抽出
                    let uri_too_long = request_line
                        .iter()
                        .position(|&b| b == b' ')
                        .and_then(|method_end| {
                            let after_method = &request_line[method_end + 1..];
                            after_method.iter().rposition(|&b| b == b' ')
                        })
                        .is_some_and(|uri_len| uri_len > MAX_HEADER_SIZE);

                    if uri_too_long {
                        ERR_MSG_URI_TOO_LONG
                    } else {
                        ERR_MSG_REQUEST_HEADER_TOO_LARGE
                    }
                } else {
                    // リクエストラインがまだ完全でない場合は431
                    ERR_MSG_REQUEST_HEADER_TOO_LARGE
                };
            let err_buf = response_code.to_vec();
            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
            // クライアントは巨大ヘッダを送信中のため、ドレインしてからクローズして
            // RST による 431/414 レスポンスの消失を防ぐ（lingering close）。
            lingering_drain_before_close(&mut tls_stream).await;
            return;
        }

        // HTTPリクエストをパース
        let mut headers_storage = [httparse::EMPTY_HEADER; 64];
        let mut req = Request::new(&mut headers_storage);

        match req.parse(&accumulated) {
            Ok(Status::Complete(header_len)) => {
                // 各フィールドの (offset, len) を req の借用中（accumulated への参照が
                // 生きている間）に確定する。以後は accumulated を可変利用するため、
                // ここでオフセットをすべて集め終えたら直ちに req をドロップする。
                let method_range = req.method.map(|m| slice_range(&accumulated, m.as_bytes()));
                let host_range = req
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case("host"))
                    .map(|h| slice_range(&accumulated, h.value));
                let path_range = req.path.map(|p| slice_range(&accumulated, p.as_bytes()));
                let user_agent_range = req
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case("user-agent"))
                    .map(|h| slice_range(&accumulated, h.value));
                // F-261: HTTP/1.0 の要求はアップグレードできない（ハンドラの判定にだけ使う）
                #[cfg(feature = "handler")]
                let http10 = req.version == Some(0);
                // ヘッダの位置はスタックの配列に控える（要求ごとに Vec を確保しない。F-202）。
                // httparse の格納先と同じ 64 個が上限。ヘッダ部は上の検査で MAX_HEADER_SIZE（8KB）以下なので
                // 位置は u16 に収まる（配列を 512 バイトに抑え、要求ごとの初期化を小さくする）。
                // await をまたがないので Future は大きくならない。
                let mut header_ranges = [[0u16; 4]; 64];
                let mut header_count = 0;
                for h in req.headers.iter().filter(|h| !h.name.is_empty()) {
                    let (no, nl) = slice_range(&accumulated, h.name.as_bytes());
                    let (vo, vl) = slice_range(&accumulated, h.value);
                    header_ranges[header_count] = [no as u16, nl as u16, vo as u16, vl as u16];
                    header_count += 1;
                }

                // req（accumulated への借用）はここで使い終わる（以後 accumulated を可変で使う）。

                // ヘッダー領域を Bytes として切り出す（参照カウント共有・memcpy なし）。
                // split_to 後は accumulated にボディの続きだけが残り、読み込みバッファとして
                // 再利用できる。
                let header_bytes: Bytes = accumulated.split_to(header_len).freeze();

                // ヘッダー一覧を Bytes ペアとして構築（`Bytes::slice` は参照カウント +1 の
                // みでコピーを伴わない）。
                let headers_for_proxy: Vec<(Bytes, Bytes)> = header_ranges[..header_count]
                    .iter()
                    .map(|&[no, nl, vo, vl]| {
                        let (no, nl, vo, vl) = (no as usize, nl as usize, vo as usize, vl as usize);
                        (
                            header_bytes.slice(no..no + nl),
                            header_bytes.slice(vo..vo + vl),
                        )
                    })
                    .collect();

                // HTTPメソッド取得
                let method_bytes: Bytes = method_range
                    .map(|(off, len)| header_bytes.slice(off..off + len))
                    .unwrap_or_else(|| Bytes::from_static(b"GET"));

                // 有効なHTTPメソッドのみ受け付ける（RFC 7231）
                const VALID_HTTP_METHODS: &[&[u8]] = &[
                    b"GET", b"HEAD", b"POST", b"PUT", b"DELETE", b"CONNECT", b"OPTIONS", b"TRACE",
                    b"PATCH",
                ];
                if !VALID_HTTP_METHODS
                    .iter()
                    .any(|m| method_bytes.as_ref().eq_ignore_ascii_case(m))
                {
                    let err_buf = ERR_MSG_METHOD_NOT_ALLOWED.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                    return;
                }

                // ヘッダー情報抽出
                let host_bytes: Bytes = host_range
                    .map(|(off, len)| header_bytes.slice(off..off + len))
                    .unwrap_or_else(Bytes::new);

                // メトリクス: 最初のリクエストでホスト名を取得し、インクリメント
                if let Ok(host_str) = std::str::from_utf8(&host_bytes) {
                    connection_metric.set_host(host_str);
                } else {
                    connection_metric.set_host("unknown");
                }

                let path_bytes: Bytes = path_range
                    .map(|(off, len)| header_bytes.slice(off..off + len))
                    .unwrap_or_else(|| Bytes::from_static(b"/"));
                // 要求先の形式（RFC 9112 §3.2）: 絶対形式（`http://host/path`）は origin 形式に直し、
                // それ以外（`/` で始まらないもの。CONNECT と OPTIONS の `*` を除く）は 400 にする。
                // そのあとパスを正規化し、ルートの照合・検査・上流への要求に同じパスを使う
                // （以前は生のまま照合して上流へ送り、`/%61dmin`・`/public/../admin` でルートの制限を
                // 素通りできた。F-236 の再走査の F7）。
                let path_bytes: Bytes =
                    match crate::http_utils::origin_form_target(&method_bytes, &path_bytes) {
                        Some(r) => {
                            let p = path_bytes.slice(r);
                            match crate::path_norm::normalize_request_path(&p) {
                                Some(n) => Bytes::from(n),
                                None => p,
                            }
                        }
                        None => {
                            let err_buf = ERR_MSG_BAD_REQUEST.to_vec();
                            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                            return;
                        }
                    };

                let user_agent: Bytes = user_agent_range
                    .map(|(off, len)| header_bytes.slice(off..off + len))
                    .unwrap_or_else(Bytes::new);

                // Content-Length ヘッダーの値を取得し、不正な値の場合は400 Bad Requestを返す
                // 複数の Content-Length ヘッダーは RFC 7230 Section 3.3.2 違反 → 400
                // 確保せずに 1 つ目と 2 つ目の有無だけを見る（F-202）
                let mut cl_iter = headers_for_proxy
                    .iter()
                    .filter(|(name, _)| name.eq_ignore_ascii_case(b"content-length"));
                let cl_first = cl_iter.next();
                if cl_iter.next().is_some() {
                    let err_buf = ERR_MSG_BAD_REQUEST.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                    return;
                }

                let content_length: usize = if let Some((_, cl_value)) = cl_first {
                    match std::str::from_utf8(cl_value)
                        .ok()
                        .and_then(|s| s.trim().parse::<usize>().ok())
                    {
                        Some(len) => len,
                        None => {
                            // 不正な Content-Length 値 → 400 Bad Request
                            let err_buf = ERR_MSG_BAD_REQUEST.to_vec();
                            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                            return;
                        }
                    }
                } else {
                    0
                };

                // B-23: リクエストフレーミングを RFC 7230 §3.3.3 準拠で分類し、HTTP リクエスト
                // スマグリング（CL.TE / TE.CL）要因を転送前に一律 400 で拒否する。
                // 従来は `content_length > 0 && is_chunked` のみを弾いており、
                // `Content-Length: 0` + `Transfer-Encoding: chunked`（CL の値に依らない CL.TE）や
                // 最終エンコーディングが chunked でない TE を取りこぼしていた。
                let is_chunked: bool = match classify_request_framing(
                    headers_for_proxy
                        .iter()
                        .map(|(k, v)| (k.as_ref(), v.as_ref())),
                ) {
                    Ok(RequestFraming::Chunked) => true,
                    Ok(RequestFraming::ContentLength) => false,
                    Err(_) => {
                        let err_buf = ERR_MSG_BAD_REQUEST.to_vec();
                        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                        return;
                    }
                };

                // Connection ヘッダーチェック（Keep-Alive / Upgrade対応）
                let connection_header: Option<&[u8]> = headers_for_proxy
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(b"connection"))
                    .map(|(_, value)| value.as_ref());

                let client_wants_close: bool = connection_header
                    .map(|v| v.eq_ignore_ascii_case(b"close"))
                    .unwrap_or(false);

                // WebSocket Upgrade チェック
                // Connection: upgrade と Upgrade: websocket の両方が必要
                let is_upgrade_connection: bool = connection_header
                    .map(|v| {
                        // "upgrade" または "keep-alive, upgrade" などのパターンに対応
                        v.to_ascii_lowercase().windows(7).any(|w| w == b"upgrade")
                    })
                    .unwrap_or(false);

                let is_websocket_upgrade: bool = headers_for_proxy
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(b"upgrade"))
                    .map(|(_, value)| value.eq_ignore_ascii_case(b"websocket"))
                    .unwrap_or(false);

                let is_websocket: bool = is_upgrade_connection && is_websocket_upgrade;

                // F-266: ライブラリのビルドでは、上限を超える本文は経路を見てから判断する（ストリーミング受信の
                // ハンドラは本文を溜めないので通す）。本体のビルドは今までどおりここで 413
                #[cfg(feature = "handler")]
                let defer_size = !is_chunked && content_length > MAX_BODY_SIZE;
                #[cfg(not(feature = "handler"))]
                let defer_size = false;
                #[cfg(feature = "handler")]
                let expect_continue = check_expect_continue(&headers_for_proxy);

                // ボディサイズ制限
                if !is_chunked && content_length > MAX_BODY_SIZE && !defer_size {
                    let err_buf = ERR_MSG_REQUEST_TOO_LARGE.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                    return;
                }

                // HTTP/1.1 Hostヘッダー必須チェック (RFC 7230 Section 5.4)
                // HTTP/1.1リクエストにはHostヘッダーが必須
                if validate_host_header(&headers_for_proxy, 1).is_err() {
                    let err_buf = ERR_MSG_BAD_REQUEST.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                    return;
                }

                let path_str = std::str::from_utf8(&path_bytes).unwrap_or("/");

                // Expect: 100-continue ヘッダーがある場合、ボディ送信前に 100 Continue を返す
                if content_length > 0 && !defer_size && check_expect_continue(&headers_for_proxy) {
                    // ボディサイズが制限内であることを確認済みなので 100 Continue を送信
                    let write_result = timeout(
                        WRITE_TIMEOUT,
                        tls_stream.write_all(HTTP_100_CONTINUE.to_vec()),
                    )
                    .await;
                    if let Err(_) | Ok((Err(_), _)) = write_result {
                        return;
                    }
                }

                // メトリクスエンドポイントの処理（設定可能なパス）
                // Prometheusスクレイピング用の特別なパス
                {
                    let config = CURRENT_CONFIG.load();
                    let prom_config = &config.prometheus_config;

                    // パスとメソッドをチェック
                    if prom_config.enabled
                        && path_str == prom_config.path
                        && method_bytes.as_ref() == b"GET"
                    {
                        let start_instant = Instant::now();

                        // IPアドレス制限チェック
                        if !prom_config.is_ip_allowed(client_ip) {
                            let err_buf = ERR_MSG_FORBIDDEN.to_vec();
                            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                            log_access(
                                &method_bytes,
                                &host_bytes,
                                &path_bytes,
                                &user_agent,
                                0,
                                403,
                                0,
                                start_instant,
                                client_ip,
                                "",
                            );
                            accumulated.clear();
                            return;
                        }

                        let metrics_response = build_metrics_response();
                        let resp_size = metrics_response.len() as u64;

                        let write_result =
                            timeout(WRITE_TIMEOUT, tls_stream.write_all(metrics_response)).await;
                        if let Ok((Ok(_), _)) = write_result {
                            log_access(
                                &method_bytes,
                                &host_bytes,
                                &path_bytes,
                                &user_agent,
                                0,
                                200,
                                resp_size,
                                start_instant,
                                client_ip,
                                "",
                            );
                        }

                        // メトリクスエンドポイントは常に接続を閉じる
                        accumulated.clear();
                        return;
                    }
                }

                // 管理 API エンドポイントの処理（F-20: キャッシュ Purge）
                // PURGE メソッド、または admin.path_prefix 配下の /cache/purge を処理する
                #[cfg(feature = "admin")]
                {
                    let config = CURRENT_CONFIG.load();
                    let admin_config = &config.admin_config;
                    let is_purge_method = method_bytes.as_ref() == b"PURGE";
                    // パフォーマンス最適化: 事前計算済みプレフィックスを使用（format! 排除）
                    let is_admin_purge_path =
                        path_str.starts_with(&admin_config.cache_purge_prefix);

                    if admin_config.enabled && (is_purge_method || is_admin_purge_path) {
                        let start_instant = Instant::now();

                        // IP制限チェック
                        let response = if !admin_config.is_ip_allowed(client_ip) {
                            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                        } else {
                            // Authorization ヘッダーを取得
                            let auth = headers_for_proxy.iter().find_map(|(name, value)| {
                                if name.eq_ignore_ascii_case(b"authorization") {
                                    std::str::from_utf8(value).ok()
                                } else {
                                    None
                                }
                            });

                            if !admin_config.check_auth(auth) {
                                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                            } else {
                                handle_cache_purge(path_str, is_purge_method)
                            }
                        };

                        let resp_size = response.len() as u64;
                        let status = if response.starts_with(b"HTTP/1.1 200") {
                            200
                        } else if response.starts_with(b"HTTP/1.1 401") {
                            401
                        } else if response.starts_with(b"HTTP/1.1 403") {
                            403
                        } else {
                            400
                        };
                        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(response)).await;
                        log_access(
                            &method_bytes,
                            &host_bytes,
                            &path_bytes,
                            &user_agent,
                            0,
                            status,
                            resp_size,
                            start_instant,
                            client_ip,
                            "",
                        );
                        accumulated.clear();
                        return;
                    }
                }

                // 管理 API エンドポイントの処理（F-21: /config, /stats, /reload, /tls/reload）
                #[cfg(feature = "admin")]
                {
                    let config = CURRENT_CONFIG.load();
                    let admin_config = &config.admin_config;

                    if admin_config.enabled
                        && path_str.starts_with(&admin_config.path_prefix)
                        && method_bytes.as_ref() != b"PURGE"
                        && !path_str.starts_with(&admin_config.cache_purge_prefix)
                    {
                        // GET /__admin/config, GET /__admin/stats,
                        // POST /__admin/reload, POST /__admin/tls/reload のみを処理
                        let path_suffix = &path_str[admin_config.path_prefix.len()..];
                        let is_known_endpoint = matches!(
                            (method_bytes.as_ref(), path_suffix),
                            (b"GET", "/config")
                                | (b"GET", "/stats")
                                | (b"GET", "/vds")
                                | (b"POST", "/reload")
                                | (b"POST", "/tls/reload")
                        );

                        if is_known_endpoint {
                            let start_instant = Instant::now();

                            // IP制限チェック
                            let response: Vec<u8> = if !admin_config.is_ip_allowed(client_ip) {
                                b"HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"error\":\"403\"}".to_vec()
                            } else {
                                // Authorization ヘッダーを取得
                                let auth = headers_for_proxy.iter().find_map(|(name, value)| {
                                    if name.eq_ignore_ascii_case(b"authorization") {
                                        std::str::from_utf8(value).ok()
                                    } else {
                                        None
                                    }
                                });

                                if !admin_config.check_auth(auth) {
                                    b"HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"error\":\"401\"}".to_vec()
                                } else {
                                    // 認証成功: エンドポイントに応じた処理
                                    match (method_bytes.as_ref(), path_suffix) {
                                        (b"GET", "/config") => {
                                            // 設定情報をJSON形式で返す（secret はマスク）
                                            let body = build_admin_config_json(&config);
                                            let mut resp = format!(
                                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                                body.len()
                                            ).into_bytes();
                                            resp.extend_from_slice(body.as_bytes());
                                            resp
                                        }
                                        (b"GET", "/stats") => {
                                            // 起動からの経過時間を返す
                                            // PROXY_START_TIME を初回アクセスで初期化
                                            let uptime_secs = PROXY_START_TIME.elapsed().as_secs();
                                            let body = format!("{{\"uptime_secs\":{}}}", uptime_secs);
                                            let mut resp = format!(
                                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                                body.len()
                                            ).into_bytes();
                                            resp.extend_from_slice(body.as_bytes());
                                            resp
                                        }
                                        (b"GET", "/vds") => {
                                            // F-211: VDS の状態（接続・適用中の版・最後の NACK の理由・更新回数）
                                            let body = admin_vds_json();
                                            let mut resp = format!(
                                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                                body.len()
                                            ).into_bytes();
                                            resp.extend_from_slice(body.as_bytes());
                                            resp
                                        }
                                        (b"POST", "/reload") => {
                                            // 設定リロードフラグを立てる
                                            use std::sync::atomic::Ordering;
                                            RELOAD_FLAG.store(true, Ordering::Relaxed);
                                            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10\r\nConnection: close\r\n\r\n{\"ok\":true}".to_vec()
                                        }
                                        (b"POST", "/tls/reload") => {
                                            // TLS証明書リロードフラグを立てる
                                            use std::sync::atomic::Ordering;
                                            TLS_RELOAD_FLAG.store(true, Ordering::Relaxed);
                                            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10\r\nConnection: close\r\n\r\n{\"ok\":true}".to_vec()
                                        }
                                        _ => {
                                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                                        }
                                    }
                                }
                            };

                            let resp_size = response.len() as u64;
                            let status = if response.starts_with(b"HTTP/1.1 200") {
                                200
                            } else if response.starts_with(b"HTTP/1.1 401") {
                                401
                            } else if response.starts_with(b"HTTP/1.1 403") {
                                403
                            } else {
                                400
                            };
                            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(response)).await;
                            log_access(
                                &method_bytes,
                                &host_bytes,
                                &path_bytes,
                                &user_agent,
                                0,
                                status,
                                resp_size,
                                start_instant,
                                client_ip,
                                "",
                            );
                            accumulated.clear();
                            return;
                        }
                    }
                }

                // Backend選択（統合ルーティング対応）
                // パス/クエリ分離（スキャンを1回に統一）
                let query_start_pos = path_bytes.iter().position(|&b| b == b'?');
                let path_without_query = query_start_pos
                    .map(|i| &path_bytes[..i])
                    .unwrap_or(&path_bytes);

                // ルーティングの送信元条件に使うアドレス。`client_ip` は呼び出し側で
                // `peer_addr.ip()` から作った文字列なので、毎リクエスト文字列から
                // パースし直さず（DTrace で `read_ipv4_addr` がホットスポットに出ていた）
                // `peer_addr` をそのまま使う（送信元条件が見るのは IP のみ）。
                let client_socket_addr = peer_addr;

                // ヘッダーをゼロコピーのバイト列スライスとして参照し、
                // クエリ文字列は生バイトのまま渡す（HashMap 割り当て不要）
                // headers_for_proxy は既に accumulated から切り出し済みの Bytes なので、
                // ここでは参照を並べるだけで新たなコピーは発生しない。
                let backend_result = {
                    // 16 個まではスタックの配列に並べる（要求ごとに Vec を確保しない。F-202）。それより多いときだけ確保する。
                    let mut raw_buf: [(&[u8], &[u8]); 16] = [(&[], &[]); 16];
                    let raw_vec: Vec<(&[u8], &[u8])>;
                    let headers_raw: &[(&[u8], &[u8])] = if headers_for_proxy.len() <= raw_buf.len()
                    {
                        for (slot, (k, v)) in raw_buf.iter_mut().zip(headers_for_proxy.iter()) {
                            *slot = (k.as_ref(), v.as_ref());
                        }
                        &raw_buf[..headers_for_proxy.len()]
                    } else {
                        raw_vec = headers_for_proxy
                            .iter()
                            .map(|(k, v)| (k.as_ref(), v.as_ref()))
                            .collect();
                        &raw_vec
                    };
                    let raw_query: &[u8] =
                        query_start_pos.map(|i| &path_bytes[i + 1..]).unwrap_or(b"");

                    crate::l7::resolve_route(
                        &host_bytes,
                        path_without_query,
                        &method_bytes,
                        headers_raw,
                        raw_query,
                        &client_socket_addr,
                        true,
                    )
                };

                let (prefix, backend, _route_compression) = match backend_result {
                    Some(b) => b,
                    None => {
                        let err_buf = ERR_MSG_NOT_FOUND.to_vec();
                        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                        return;
                    }
                };

                // F-261: ハンドラのルートの WebSocket の要求は 400 にせず、通常のハンドラの経路へ進める
                // （ハンドラが 101 を返して接続を引き取る）。先読みの残りはパイプラインの分として取っておく。
                #[cfg(feature = "handler")]
                let is_websocket = is_websocket && !matches!(backend, Backend::Handler(..));

                // セキュリティ設定を取得
                let security = backend.security();

                // F-266: ストリーミング受信のハンドラ（`Multipart` 等）は本文の大きさを見ない（HTTP/2 と同じ。
                // multipart はパートごとの上限を見る）
                #[cfg(feature = "handler")]
                let checked_length = match &backend {
                    Backend::Handler(e, _, _, _) if e.streaming_request => 0,
                    _ => content_length,
                };
                #[cfg(not(feature = "handler"))]
                let checked_length = content_length;
                #[cfg(feature = "handler")]
                if defer_size {
                    if checked_length != 0 {
                        let err_buf = ERR_MSG_REQUEST_TOO_LARGE.to_vec();
                        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                        return;
                    }
                    if expect_continue {
                        let write_result = timeout(
                            WRITE_TIMEOUT,
                            tls_stream.write_all(HTTP_100_CONTINUE.to_vec()),
                        )
                        .await;
                        if let Err(_) | Ok((Err(_), _)) = write_result {
                            return;
                        }
                    }
                }
                // IP 制限・許可メソッド・本文の大きさ（chunked 以外）・レート制限（F-234: 3 経路で共通の検査と順）
                let check = check_security(
                    security,
                    client_ip,
                    &method_bytes,
                    checked_length,
                    is_chunked,
                );
                if let Some(resp) = check.http1_response() {
                    let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(resp.to_vec())).await;
                    // 本文をまだ受け取り切っていない要求（413 等）は、未読のまま閉じると RST になり、
                    // 書いた応答がクライアントに届かないことがある（macOS で再現）。読み捨ててから閉じる（B-102）
                    if is_chunked || content_length > accumulated.len() {
                        lingering_drain_before_close(&mut tls_stream).await;
                    }
                    return;
                }

                // 初期ボディ（ヘッダー後のデータ）
                // ヘッダー領域は既に `accumulated.split_to(header_len)` で切り出し済みのため、
                // 残っている accumulated 全体がそのままボディの先頭部分になる
                // （header_len によるオフセット計算は不要）。
                // B-129: 本文の長さが決まっている要求（chunked・WebSocket 以外）は、本文より後ろのデータを
                // 次の要求（パイプライン）として残す（以前は本文の先頭として扱い、次の要求を失っていた）
                // chunked の要求は、読み取り済みの範囲に終端があればその直後で切る（以前は切らず、終端より
                // 後ろのバイト＝veil が経路・検査を通していない次の要求を上流へそのまま送っていた。
                // F-236 の再走査の F1）。不正なチャンク形式は 400 で閉じる（F5）。
                let chunked_end = if is_chunked && !is_websocket {
                    let mut dec = ChunkedDecoder::new_request(0);
                    match dec.feed_counted(&accumulated) {
                        (ChunkedFeedResult::Complete, k) => Some(k),
                        (ChunkedFeedResult::Invalid, _) => {
                            let _ = timeout(
                                WRITE_TIMEOUT,
                                tls_stream.write_all(ERR_MSG_BAD_REQUEST.to_vec()),
                            )
                            .await;
                            return;
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                // 本文を読む経路（ローカル応答の読み捨て・Wasm の本文の段・ハンドラ）が本文の終わりより後ろまで
                // 読んだ分も、ここへ足して次の要求として扱う（B-186）
                let mut pipelined: BytesMut = if let Some(k) = chunked_end {
                    accumulated.split_off(k)
                } else if !is_chunked && !is_websocket && accumulated.len() > content_length {
                    accumulated.split_off(content_length)
                } else {
                    BytesMut::new()
                };
                let initial_body: Vec<u8> = accumulated.to_vec();

                // WASMモジュールの適用
                // モジュールリストをローカル変数として保持（スレッドローカルを使わない、並行タスク間の干渉を防ぐ）
                // F-43: モジュールリストは Arc 共有（リクエストごとの deep copy 排除）
                #[cfg(feature = "wasm")]
                let modules_to_apply: Arc<
                    Vec<crate::wasm_plugin_config::ModuleRef>,
                > = if let Some(backend_modules) = backend.modules_arc() {
                    backend_modules.clone()
                } else {
                    crate::wasm::empty_wasm_modules()
                };

                #[cfg(feature = "wasm")]
                let headers_for_proxy = {
                    let config = CURRENT_CONFIG.load();
                    if let Some(ref wasm_engine) = config.wasm_filter_engine {
                        // routes内で指定されたmodulesを優先
                        let path_str = std::str::from_utf8(&path_bytes).unwrap_or("/");
                        let method_str = std::str::from_utf8(&method_bytes).unwrap_or("GET");

                        // モジュールを実行
                        if !modules_to_apply.is_empty() {
                            let headers_vec: Vec<(Vec<u8>, Vec<u8>)> = headers_for_proxy
                                .iter()
                                .map(|(k, v)| (k.to_vec(), v.to_vec()))
                                .collect();

                            let wasm_result = wasm_engine
                                .clone()
                                .on_request_headers_with_modules_async(
                                    modules_to_apply.clone(),
                                    Arc::from(path_str),
                                    Arc::from(method_str),
                                    headers_vec,
                                    Arc::from(client_ip),
                                    content_length == 0 && !is_chunked, // end_of_stream
                                )
                                .await;

                            match wasm_result {
                                crate::wasm::FilterResult::Continue {
                                    headers: modified_headers,
                                    ..
                                } => {
                                    // 修正されたヘッダーを使用
                                    // F-43/F-168: 所有権ムーブで変換（deep copy しない）。
                                    // `Bytes::from(Vec<u8>)` はバッファの所有権を引き継ぐだけで
                                    // memcpy を伴わない。
                                    modified_headers
                                        .into_iter()
                                        .map(|(k, v)| (Bytes::from(k), Bytes::from(v)))
                                        .collect()
                                }
                                crate::wasm::FilterResult::LocalResponse(resp) => {
                                    // ローカルレスポンスを返送。F-271: 本文を読み切れたら接続を続ける
                                    // （以前は毎回閉じていた）
                                    let start_instant = Instant::now();
                                    let response = wasm_local_response_h1(&resp);
                                    let resp_size = response.len() as u64;
                                    let write_result =
                                        timeout(WRITE_TIMEOUT, tls_stream.write_all(response))
                                            .await;
                                    if !matches!(write_result, Ok((Ok(_), _))) {
                                        return;
                                    }
                                    log_access(
                                        &method_bytes,
                                        &host_bytes,
                                        &path_bytes,
                                        &user_agent,
                                        0,
                                        resp.status_code,
                                        resp_size,
                                        start_instant,
                                        client_ip,
                                        "",
                                    );
                                    // WASMライフサイクルコールバック: リクエスト完了
                                    crate::wasm::on_request_complete_async(
                                        wasm_engine.clone(),
                                        modules_to_apply.clone(),
                                    )
                                    .await;
                                    accumulated.clear();
                                    if client_wants_close
                                        || is_websocket
                                        || !drain_request_body(
                                            &mut tls_stream,
                                            content_length,
                                            is_chunked && chunked_end.is_none(),
                                            &initial_body,
                                            &mut pipelined,
                                        )
                                        .await
                                    {
                                        return;
                                    }
                                    if !pipelined.is_empty() {
                                        accumulated.extend_from_slice(&pipelined);
                                        pipelined_ready =
                                            accumulated.windows(4).any(|w| w == b"\r\n\r\n");
                                    }
                                    continue;
                                }
                                crate::wasm::FilterResult::Pause => {
                                    // 非同期処理待ち（現在は未実装）
                                    warn!("WASM module requested pause, but async operations are not yet supported");
                                    headers_for_proxy
                                }
                            }
                        } else {
                            headers_for_proxy
                        }
                    } else {
                        headers_for_proxy
                    }
                };

                // F-271: 要求本文のフェーズ（本文を読むモジュール＝`allow_request_body_read` があり、本文のある
                // 要求だけ）。本文を上限付きで読み切って `on_request_body` を呼び、結果の本文を Content-Length 付きで
                // バックエンドへ渡す（chunked は外す）。Wasm の無いルートは `is_empty` の分岐だけ
                #[cfg(feature = "wasm")]
                let (initial_body, content_length, is_chunked, headers_for_proxy) =
                    if modules_to_apply.is_empty()
                        || is_websocket
                        || !(is_chunked || content_length > 0)
                    {
                        (initial_body, content_length, is_chunked, headers_for_proxy)
                    } else {
                        let config = CURRENT_CONFIG.load();
                        match config.wasm_filter_engine.as_ref() {
                            Some(wasm_engine)
                                if wasm_engine.wants_request_body(&modules_to_apply) =>
                            {
                                let limit = if is_chunked {
                                    security.max_chunked_body_size
                                } else {
                                    security.max_request_body_size
                                };
                                let body = match read_full_request_body(
                                    &mut tls_stream,
                                    content_length,
                                    is_chunked,
                                    &initial_body,
                                    limit,
                                    &mut pipelined,
                                )
                                .await
                                {
                                    Ok(b) => b,
                                    Err(status) => {
                                        let err: &[u8] = if status == 413 {
                                            ERR_MSG_REQUEST_TOO_LARGE
                                        } else {
                                            ERR_MSG_BAD_REQUEST
                                        };
                                        let _ = timeout(
                                            WRITE_TIMEOUT,
                                            tls_stream.write_all(err.to_vec()),
                                        )
                                        .await;
                                        // 未読の本文を残して閉じると RST で応答が届かないことがある（B-102）
                                        lingering_drain_before_close(&mut tls_stream).await;
                                        return;
                                    }
                                };
                                let method_arc: Arc<str> =
                                    Arc::from(std::str::from_utf8(&method_bytes).unwrap_or("GET"));
                                let path_arc: Arc<str> =
                                    Arc::from(std::str::from_utf8(&path_bytes).unwrap_or("/"));
                                let ip_arc: Arc<str> = Arc::from(client_ip);
                                let headers_vec: Vec<(Vec<u8>, Vec<u8>)> = headers_for_proxy
                                    .iter()
                                    .map(|(k, v)| (k.to_vec(), v.to_vec()))
                                    .collect();
                                let meta = crate::wasm::RequestMeta {
                                    method: &method_arc,
                                    path: &path_arc,
                                    headers: &headers_vec,
                                    client_ip: &ip_arc,
                                };
                                match crate::wasm::http_executor::apply_wasm_request_body(
                                    wasm_engine,
                                    &modules_to_apply,
                                    body,
                                    true,
                                    Some(&meta),
                                )
                                .await
                                {
                                    crate::wasm::http_executor::WasmBodyOutcome::Continue(b) => {
                                        let mut headers = headers_for_proxy;
                                        set_body_framing_headers(&mut headers, b.len());
                                        (b.to_vec(), b.len(), false, headers)
                                    }
                                    crate::wasm::http_executor::WasmBodyOutcome::LocalResponse(
                                        resp,
                                    ) => {
                                        let start_instant = Instant::now();
                                        let response = wasm_local_response_h1(&resp);
                                        let resp_size = response.len() as u64;
                                        let write_result =
                                            timeout(WRITE_TIMEOUT, tls_stream.write_all(response))
                                                .await;
                                        if !matches!(write_result, Ok((Ok(_), _))) {
                                            return;
                                        }
                                        log_access(
                                            &method_bytes,
                                            &host_bytes,
                                            &path_bytes,
                                            &user_agent,
                                            content_length as u64,
                                            resp.status_code,
                                            resp_size,
                                            start_instant,
                                            client_ip,
                                            "",
                                        );
                                        crate::wasm::on_request_complete_async(
                                            wasm_engine.clone(),
                                            modules_to_apply.clone(),
                                        )
                                        .await;
                                        accumulated.clear();
                                        if client_wants_close {
                                            return;
                                        }
                                        if !pipelined.is_empty() {
                                            accumulated.extend_from_slice(&pipelined);
                                            pipelined_ready =
                                                accumulated.windows(4).any(|w| w == b"\r\n\r\n");
                                        }
                                        continue;
                                    }
                                }
                            }
                            _ => (initial_body, content_length, is_chunked, headers_for_proxy),
                        }
                    };

                // 処理時間計測開始（アクセスログ・メトリクスが無効なら時刻を読まない）
                let start_instant = crate::logging::request_start_instant();

                // バッファクリア（次のリクエストに備える）
                accumulated.clear();

                // WebSocket Upgrade の場合は専用ハンドラーを使用
                if is_websocket {
                    // WebSocket はプロキシバックエンドでのみサポート
                    if let Backend::Proxy(ref upstream_group, ref security, _, _, _, _) = backend {
                        debug!(
                            "WebSocket upgrade request detected for path: {}",
                            std::str::from_utf8(&path_bytes).unwrap_or("-")
                        );

                        // UpstreamGroup からサーバーを選択
                        let server = match upstream_group.select(client_ip) {
                            Some(s) => s,
                            None => {
                                error!("No healthy upstream servers for WebSocket");
                                let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                                let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                                return;
                            }
                        };

                        server.acquire();

                        // WebSocket プロキシ処理（双方向転送）
                        let ws_result = handle_websocket_proxy(
                            tls_stream,
                            &server.target,
                            upstream_group.tls_profile(),
                            security,
                            &method_bytes,
                            &path_bytes,
                            &prefix,
                            &headers_for_proxy,
                            &initial_body,
                            content_length,
                            client_ip,
                            &host_bytes,
                        )
                        .await;

                        server.release();

                        if let Some((status, resp_size)) = ws_result {
                            log_access(
                                &method_bytes,
                                &host_bytes,
                                &path_bytes,
                                &user_agent,
                                content_length as u64,
                                status,
                                resp_size,
                                start_instant,
                                client_ip,
                                "",
                            );

                            // WASMライフサイクルコールバック: リクエスト完了
                            #[cfg(feature = "wasm")]
                            {
                                if !modules_to_apply.is_empty() {
                                    let config = CURRENT_CONFIG.load();
                                    if let Some(ref wasm_engine) = config.wasm_filter_engine {
                                        crate::wasm::on_request_complete_async(
                                            wasm_engine.clone(),
                                            modules_to_apply.clone(),
                                        )
                                        .await;
                                    }
                                }
                            }
                        }
                        // WebSocket 接続終了後は HTTP 接続も終了
                        return;
                    } else {
                        // ファイルバックエンドでは WebSocket 非対応
                        let err_buf = ERR_MSG_BAD_REQUEST.to_vec();
                        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                        return;
                    }
                }

                // F-261: ハンドラがアップグレードを返してよい要求か（`Connection: upgrade` の要求だけ
                // ヘッダを見る。それ以外の要求は bool の代入だけ）
                #[cfg(feature = "handler")]
                {
                    api_cache.upgrade_allowed = is_upgrade_connection
                        && !http10
                        && !is_chunked
                        && content_length == 0
                        && headers_for_proxy
                            .iter()
                            .any(|(n, _)| n.eq_ignore_ascii_case(b"upgrade"));
                }

                // Backend処理
                let result = handle_backend(
                    tls_stream,
                    backend,
                    &method_bytes,
                    &path_bytes,
                    prefix,
                    content_length,
                    is_chunked,
                    headers_for_proxy,
                    &initial_body,
                    client_wants_close,
                    #[cfg(feature = "handler")]
                    &mut api_cache,
                    {
                        #[cfg(feature = "wasm")]
                        {
                            modules_to_apply.clone()
                        }
                        #[cfg(not(feature = "wasm"))]
                        {
                            no_wasm_modules()
                        }
                    },
                    client_ip,
                    &mut pipelined,
                )
                .await;

                match result {
                    Some((stream_back, status, resp_size, should_close)) => {
                        log_access(
                            &method_bytes,
                            &host_bytes,
                            &path_bytes,
                            &user_agent,
                            content_length as u64,
                            status,
                            resp_size,
                            start_instant,
                            client_ip,
                            "",
                        );

                        // WASMライフサイクルコールバック: リクエスト完了
                        #[cfg(feature = "wasm")]
                        {
                            if !modules_to_apply.is_empty() {
                                let config = CURRENT_CONFIG.load();
                                if let Some(ref wasm_engine) = config.wasm_filter_engine {
                                    crate::wasm::on_request_complete_async(
                                        wasm_engine.clone(),
                                        modules_to_apply.clone(),
                                    )
                                    .await;
                                }
                            }
                        }

                        tls_stream = stream_back;

                        // F-261: ハンドラが 101 を書いた。接続（と先読みの残り）をハンドラの関数へ渡し、
                        // 終わったら閉じる（この接続のタスクのまま待つので同時接続数に数え続ける）
                        #[cfg(feature = "handler")]
                        if status == 101 {
                            if let Some(on_upgrade) = api_cache.pending_upgrade.take() {
                                crate::api::upgrade::run(
                                    on_upgrade,
                                    tls_stream,
                                    pipelined,
                                    client_ip,
                                    &mut api_cache,
                                )
                                .await;
                                return;
                            }
                        }

                        // Connection: close が要求された場合、またはエラー時は接続を閉じる
                        if should_close {
                            return;
                        }
                        // B-129: 届いている次の要求（パイプライン）を蓄積に戻す
                        if !pipelined.is_empty() {
                            accumulated.extend_from_slice(&pipelined);
                            pipelined_ready = accumulated.windows(4).any(|w| w == b"\r\n\r\n");
                        }
                        // Keep-Alive: ループを継続して次のリクエストを待機
                    }
                    None => {
                        return;
                    }
                }
            }
            Ok(Status::Partial) => {
                // データ不足、次の読み込みを待つ
                continue;
            }
            Err(_) => {
                let err_buf = ERR_MSG_BAD_REQUEST.to_vec();
                let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                return;
            }
        }
    }
}

// Backend処理
// ====================

// 統一された Backend 処理（型エイリアスを使用）
/// ローカル応答系バックエンド（File/Memory/Redirect 等）向けに、消費されない
/// リクエストボディをソケットから読み捨てる。
///
/// これらのバックエンドはリクエストボディを読まずにレスポンスを返す。keep-alive 接続で
/// ボディを読み捨てずに次のリクエストへ進むと、ソケットに残ったボディ（Content-Length
/// 本文や chunked の `5\r\nhello\r\n0\r\n\r\n` 等）が次のリクエストとして解釈され、
/// `400 Bad Request` を引き起こす（負荷時に顕在化する keep-alive desync の原因）。
///
/// `initial_body` は既にヘッダ読み取り時にソケットから消費済みの先頭バイト。残りを
/// ソケットから読み切る。完全にドレインできた場合のみ `true` を返す（接続再利用が安全）。
/// ドレインに失敗した場合は `false`（呼び出し側は接続を閉じる）。
///
/// 本文の終わりより後ろまで読んだ分（同じセグメントで届いた次の要求＝パイプライン）は `overflow` の後ろに
/// 足す（B-186。以前は捨てていたので、接続を続けると次の要求が消えた）。
pub(super) async fn drain_request_body(
    stream: &mut ServerTls,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
    overflow: &mut BytesMut,
) -> bool {
    if is_chunked {
        // DoS 対策の上限は MAX_BODY_SIZE を流用（リクエスト本文の上限）。
        let mut decoder = ChunkedDecoder::new_request(MAX_BODY_SIZE as u64);
        match decoder.feed(initial_body) {
            ChunkedFeedResult::Complete => return true,
            ChunkedFeedResult::SizeLimitExceeded | ChunkedFeedResult::Invalid => return false,
            ChunkedFeedResult::Continue => {}
        }
        loop {
            let buf = buf_get();
            match timeout(READ_TIMEOUT, stream.read(buf)).await {
                Ok((Ok(0), b)) => {
                    buf_put(b);
                    return false;
                } // EOF: 不完全
                Ok((Ok(n), mut b)) => {
                    b.set_valid_len(n);
                    let data = b.as_valid_slice();
                    let (res, used) = decoder.feed_counted(data);
                    if matches!(res, ChunkedFeedResult::Complete) {
                        overflow.extend_from_slice(&data[used..]);
                    }
                    buf_put(b);
                    match res {
                        ChunkedFeedResult::Complete => return true,
                        ChunkedFeedResult::SizeLimitExceeded | ChunkedFeedResult::Invalid => {
                            return false
                        }
                        ChunkedFeedResult::Continue => {}
                    }
                }
                _ => return false, // 読み取りエラー or タイムアウト
            }
        }
    } else {
        let mut remaining = content_length.saturating_sub(initial_body.len());
        while remaining > 0 {
            let buf = buf_get();
            match timeout(READ_TIMEOUT, stream.read(buf)).await {
                Ok((Ok(0), b)) => {
                    buf_put(b);
                    return false;
                } // EOF: 不完全
                Ok((Ok(n), mut b)) => {
                    if n > remaining {
                        b.set_valid_len(n);
                        overflow.extend_from_slice(&b.as_valid_slice()[remaining..]);
                    }
                    buf_put(b);
                    remaining = remaining.saturating_sub(n);
                }
                _ => return false, // 読み取りエラー or タイムアウト
            }
        }
        true
    }
}

pub(super) async fn handle_backend(
    mut tls_stream: ServerTls,
    backend: Backend,
    method: &Bytes,
    req_path: &Bytes,
    prefix: Arc<[u8]>,
    content_length: usize,
    is_chunked: bool,
    // 所有で受け取る（ネイティブハンドラは複製せずに Request へ渡す。F-202）
    headers: Vec<(Bytes, Bytes)>,
    initial_body: &[u8],
    client_wants_close: bool,
    #[cfg(feature = "handler")] api_cache: &mut crate::api::dispatch::CallCache,
    wasm_modules: Arc<Vec<crate::wasm_plugin_config::ModuleRef>>,
    client_ip: &str,
    // 本文を読み過ぎた分（次の要求）を足す先（B-186。本文を読み終えたら呼び出し側が蓄積へ戻す）
    pipelined: &mut BytesMut,
) -> Option<(ServerTls, u16, u64, bool)> {
    // Proxy バックエンドはリクエストボディを上流へ転送して消費する。それ以外（File/Memory/
    // Redirect 等のローカル応答）はボディを読まないため、keep-alive 接続でボディが次の
    // リクエストに混入して 400 desync を起こす。ローカル応答の前に残りのボディを読み捨てる。
    if !backend_consumes_request_body(&backend)
        && (is_chunked || content_length > initial_body.len())
        && !drain_request_body(
            &mut tls_stream,
            content_length,
            is_chunked,
            initial_body,
            pipelined,
        )
        .await
    {
        // ドレイン失敗（接続が汚染されている可能性）→ 接続を閉じる
        return Some((tls_stream, 400, 0, true));
    }
    match backend {
        Backend::Proxy(upstream_group, security, compression, buffering, cache, _) => {
            // handle_proxy のフューチャは約 19KB あり、インラインで待つと handle_backend の
            // フューチャ全体が 22KB になる。handle_backend は毎リクエスト生成・ムーブされるため、
            // 静的配信のリクエストでもこの 22KB の memcpy を払っていた（FreeBSD のプロファイルで
            // memcpy が最大のホットスポット）。プロキシ経路だけを Box に逃がし、他の経路の
            // フューチャを約 10KB に縮める。
            Box::pin(handle_proxy(
                tls_stream,
                &upstream_group,
                &security,
                &compression,
                &buffering,
                &cache,
                method,
                req_path,
                &prefix,
                content_length,
                is_chunked,
                &headers,
                initial_body,
                client_wants_close,
                wasm_modules,
                client_ip,
            ))
            .await
        }
        Backend::MemoryFile(data, mime_type, security, _) => {
            // ファイル完全一致チェック
            // MemoryFileはファイル指定なので、プレフィックス以降にパスがあれば404
            let path_str = std::str::from_utf8(req_path).unwrap_or("/");
            let prefix_str = std::str::from_utf8(&prefix).unwrap_or("");

            let remainder = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
                &path_str[prefix_str.len()..]
            } else {
                ""
            };

            let clean_remainder = remainder.trim_matches('/');
            if !clean_remainder.is_empty() {
                // ファイル指定なのにさらにパスが続いている場合は404
                let err_buf = ERR_MSG_NOT_FOUND.to_vec();
                let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                return Some((tls_stream, 404, 0, true));
            }

            // Keep-Alive対応: クライアントの要求に応じてConnectionヘッダーを動的に生成
            let mut header = Vec::with_capacity(HEADER_BUF_CAPACITY);
            header.extend_from_slice(HTTP_200_PREFIX);
            header.extend_from_slice(mime_type.as_bytes());
            header.extend_from_slice(CONTENT_LENGTH_HEADER);
            let mut num_buf = itoa::Buffer::new();
            header.extend_from_slice(num_buf.format(data.len()).as_bytes());
            header.extend_from_slice(b"\r\n");

            // 追加レスポンスヘッダー（セキュリティヘッダーなど）
            for (header_name, header_value) in &security.add_response_headers {
                header.extend_from_slice(header_name.as_bytes());
                header.extend_from_slice(b": ");
                header.extend_from_slice(header_value.as_bytes());
                header.extend_from_slice(b"\r\n");
            }
            // `[route.traffic.cors]` の実際の応答のヘッダ（F-273）
            crate::route_cors::append_h1_lines(&mut header, &security);
            // F-94: HTTP/3 広告（Alt-Svc）
            append_alt_svc_header_line(&mut header);

            // WASMレスポンスヘッダーフィルタを適用（後段で Connection ヘッダーを追記するため mut）
            #[cfg(feature = "wasm")]
            let mut header = {
                ftlog::debug!(
                    "[WASM Response] MemoryFile: wasm_modules count = {}",
                    wasm_modules.len()
                );
                if !wasm_modules.is_empty() {
                    let config = CURRENT_CONFIG.load();
                    if let Some(ref wasm_engine) = config.wasm_filter_engine {
                        // 現在のヘッダーをVec<(Vec<u8>, Vec<u8>)>形式に変換
                        let header_str = String::from_utf8_lossy(&header);
                        let current_headers: Vec<(Vec<u8>, Vec<u8>)> = header_str
                            .lines()
                            .skip(1) // ステータス行をスキップ
                            .filter_map(|line| {
                                let line_trimmed =
                                    line.trim_end_matches("\r\n").trim_end_matches("\r");
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

                        // WASMフィルタを実行（レスポンスヘッダー処理）
                        let wasm_result = wasm_engine
                            .clone()
                            .on_response_headers_with_modules_async(
                                wasm_modules.clone(),
                                200,
                                current_headers,
                                true, // end_of_stream
                            )
                            .await;

                        match wasm_result {
                            crate::wasm::FilterResult::Continue {
                                headers: modified_headers,
                                ..
                            } => {
                                // WASMから修正されたヘッダーで再構築
                                let mut new_header = Vec::with_capacity(HEADER_BUF_CAPACITY);
                                new_header.extend_from_slice(HTTP_200_PREFIX);
                                new_header.extend_from_slice(mime_type.as_bytes());
                                new_header.extend_from_slice(CONTENT_LENGTH_HEADER);
                                let mut num_buf = itoa::Buffer::new();
                                new_header.extend_from_slice(num_buf.format(data.len()).as_bytes());
                                new_header.extend_from_slice(b"\r\n");

                                // WASMから返されたヘッダーを追加
                                for (name, value) in modified_headers {
                                    new_header.extend_from_slice(&name);
                                    new_header.extend_from_slice(b": ");
                                    new_header.extend_from_slice(&value);
                                    new_header.extend_from_slice(b"\r\n");
                                }
                                new_header
                            }
                            _ => header,
                        }
                    } else {
                        header
                    }
                } else {
                    header
                }
            };

            // Connection header を追加（header は上で `let mut` 済み）
            if client_wants_close {
                header.extend_from_slice(b"Connection: close\r\n\r\n");
            } else {
                header.extend_from_slice(b"Connection: keep-alive\r\n\r\n");
            }

            // ヘッダー送信（タイムアウト付き）
            let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(header)).await;
            match write_result {
                Ok((Ok(_), _)) => {}
                _ => return None,
            }

            // ボディ送信（タイムアウト付き）
            let data_len = data.len() as u64;
            let data_buf = data.to_vec();
            let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(data_buf)).await;
            match write_result {
                Ok((Ok(_), _)) => Some((tls_stream, 200, data_len, client_wants_close)),
                _ => None,
            }
        }
        Backend::SendFile(
            base_path,
            is_dir,
            index_file,
            security,
            _cache,
            open_file_cache_config,
            canonical_base,
            static_file_cache_config,
            _,
        ) => {
            // Range ヘッダーを抽出 (RFC 7233)
            let range_header = headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(b"range"))
                .map(|(_, v)| v.as_ref());
            handle_sendfile(
                tls_stream,
                &base_path,
                is_dir,
                index_file.as_deref(),
                req_path,
                &prefix,
                client_wants_close,
                &security,
                range_header,
                open_file_cache_config.as_deref(),
                canonical_base.as_deref(),
                wasm_modules,
                static_file_cache_config.as_deref(),
            )
            .await
        }
        Backend::Respond(r) => {
            // 固定応答（F-219）。応答全体はロード時に組み立て済み（Bytes の参照カウントだけ）
            let full = if client_wants_close {
                &r.h1_close
            } else {
                &r.h1_keepalive
            };
            let out = if method.as_ref() == b"HEAD" {
                full.slice(..full.len() - r.body.len())
            } else {
                full.clone()
            };
            match timeout(WRITE_TIMEOUT, tls_stream.write_all(out)).await {
                Ok((Ok(_), _)) => Some((
                    tls_stream,
                    r.status,
                    r.body.len() as u64,
                    client_wants_close,
                )),
                _ => None,
            }
        }
        Backend::Redirect(redirect_url, status_code, preserve_path, _, security) => {
            handle_redirect(
                tls_stream,
                &redirect_url,
                status_code,
                preserve_path,
                req_path,
                &prefix,
                client_wants_close,
                &security,
            )
            .await
        }
        #[cfg(feature = "handler")]
        Backend::Handler(entry, security, _, comp) => {
            Box::pin(handle_api_handler(
                tls_stream,
                &entry,
                comp.as_deref(),
                &security,
                method,
                req_path,
                content_length,
                is_chunked,
                headers,
                initial_body,
                client_wants_close,
                client_ip,
                api_cache,
                pipelined,
            ))
            .await
        }
    }
}

/// リクエスト本文を自分で読むバックエンドか（それ以外はローカル応答の前に読み捨てる）。
#[inline]
pub(super) fn backend_consumes_request_body(backend: &Backend) -> bool {
    match backend {
        Backend::Proxy(..) => true,
        #[cfg(feature = "handler")]
        Backend::Handler(..) => true,
        _ => false,
    }
}

/// HTTP/1.1 のネイティブハンドラ（API ライブラリ、F-188）: 本文を上限付きで読み、ハンドラを呼んで
/// 応答を書く。戻り値は他のバックエンドと同じ `(stream, status, body_len, close)`。
#[cfg(feature = "handler")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_api_handler(
    mut tls_stream: ServerTls,
    entry: &crate::api::handler::HandlerEntry,
    comp: Option<&crate::config::CompressionConfig>,
    security: &SecurityConfig,
    method: &Bytes,
    req_path: &Bytes,
    content_length: usize,
    is_chunked: bool,
    headers: Vec<(Bytes, Bytes)>,
    initial_body: &[u8],
    client_wants_close: bool,
    client_ip: &str,
    api_cache: &mut crate::api::dispatch::CallCache,
    overflow: &mut BytesMut,
) -> Option<(ServerTls, u16, u64, bool)> {
    // F-266: ストリーミング受信のハンドラ（`Multipart` 等）は本文を待たずに呼び、届いた順に渡す。
    // chunked は従来どおり全部を読んでから 1 つの断片で渡す（`take_body_stream` の既定）
    if entry.streaming_request && !is_chunked && content_length > 0 {
        return handle_api_streaming(
            tls_stream,
            entry,
            comp,
            security,
            method,
            req_path,
            content_length,
            headers,
            initial_body,
            client_wants_close,
            client_ip,
            api_cache,
            overflow,
        )
        .await;
    }
    let limit = crate::api::app::max_body_size();
    let body = match read_full_request_body(
        &mut tls_stream,
        content_length,
        is_chunked,
        initial_body,
        limit,
        overflow,
    )
    .await
    {
        Ok(b) => b,
        Err(status) => {
            let resp = crate::api::response::Response::error(
                status,
                if status == 413 {
                    "Payload Too Large"
                } else {
                    "Bad Request"
                },
            );
            let mut out = crate::api::dispatch::http1_head(&resp, false);
            out.extend_from_slice(&resp.body);
            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(out)).await;
            return Some((tls_stream, status, resp.body.len() as u64, true));
        }
    };
    // メソッド・パスは要求ヘッダの領域を指す Bytes（参照カウントだけ。F-202）
    let req = crate::api::dispatch::build_request(
        method.clone(),
        req_path.clone(),
        headers,
        body,
        client_ip,
        crate::api::request::HttpVersion::Http11,
    );
    let resp = crate::api::dispatch::call_with(entry, req, api_cache, comp).await;
    write_api_response(tls_stream, resp, security, client_wants_close, api_cache).await
}

/// ハンドラの応答を HTTP/1.1 で書く（`handle_api_handler` とストリーミング受信の経路で共通）。
#[cfg(feature = "handler")]
async fn write_api_response(
    mut tls_stream: ServerTls,
    mut resp: crate::api::response::Response,
    security: &SecurityConfig,
    client_wants_close: bool,
    api_cache: &mut crate::api::dispatch::CallCache,
) -> Option<(ServerTls, u16, u64, bool)> {
    // F-261: 検査を通ったアップグレード（101）。ヘッダを書き、接続は呼び出し側のループが関数へ渡す
    if let Some(up) = resp.upgrade.take() {
        let mut head = crate::pool::request_buf_get(API_HEAD_ESTIMATE);
        crate::api::upgrade::write_http1_101(&mut head, &resp, &up.protocol);
        match timeout(WRITE_TIMEOUT, tls_stream.write_all(head)).await {
            Ok((Ok(_), buf)) => crate::pool::request_buf_put(buf),
            _ => return None,
        }
        api_cache.pending_upgrade = Some(up.on_upgrade);
        return Some((tls_stream, 101, 0, false));
    }
    // `[route.traffic.cors]` の実際の応答のヘッダ（F-273。CORS の無いルートは分岐だけ）
    crate::route_cors::apply_bytes(&mut resp.headers, security);
    let status = resp.status_code();
    let body = resp.body().clone();
    // ステータス行・ヘッダはスレッドローカルのプールのバッファに書き、小さな本文は同じバッファに続けて
    // 1 回の書き込みで送る（要求ごとの確保と、ヘッダ・本文の 2 回の書き込みを避ける。F-202）
    let inline_body = !resp.is_streaming() && body.len() <= API_INLINE_BODY_MAX;
    let mut head =
        crate::pool::request_buf_get(API_HEAD_ESTIMATE + if inline_body { body.len() } else { 0 });
    crate::api::dispatch::write_http1_head(&mut head, &resp, !client_wants_close);
    if inline_body {
        head.extend_from_slice(&body);
    }
    let streaming = resp.take_stream();
    match timeout(WRITE_TIMEOUT, tls_stream.write_all(head)).await {
        Ok((Ok(_), buf)) => crate::pool::request_buf_put(buf),
        _ => return None,
    }
    if let Some(stream) = streaming {
        let (tls_stream, sent) = write_api_chunked(tls_stream, stream).await?;
        return Some((tls_stream, status, sent, client_wants_close));
    }
    let len = body.len() as u64;
    if !inline_body {
        match timeout(WRITE_TIMEOUT, tls_stream.write_all(body)).await {
            Ok((Ok(_), _)) => {}
            _ => return None,
        }
    }
    Some((tls_stream, status, len, client_wants_close))
}

/// ストリーミング受信のハンドラ（F-266）: 本文（Content-Length）を読みながらハンドラを呼ぶ。読み取りと
/// ハンドラは同じタスクの `FuturesUnordered` で待つ（片方の起床でもう片方を poll しない。F-173）。
/// ハンドラが本文を読み終える前に全量の応答を返したら、残りは読まずに応答の後で接続を閉じる。
/// ストリーミング応答を返したら（要求本文を応答へ流すエコー等）、本文を読み終えるまで応答の断片を
/// 溜めて要求本文のチャネルを空け続け、読み終えてから送る（HTTP/1.1 の接続は読みと書きを同時にしない）。
#[cfg(feature = "handler")]
#[allow(clippy::too_many_arguments)]
async fn handle_api_streaming(
    mut tls_stream: ServerTls,
    entry: &crate::api::handler::HandlerEntry,
    comp: Option<&crate::config::CompressionConfig>,
    security: &SecurityConfig,
    method: &Bytes,
    req_path: &Bytes,
    content_length: usize,
    headers: Vec<(Bytes, Bytes)>,
    initial_body: &[u8],
    client_wants_close: bool,
    client_ip: &str,
    api_cache: &mut crate::api::dispatch::CallCache,
    overflow: &mut BytesMut,
) -> Option<(ServerTls, u16, u64, bool)> {
    use crate::api::stream::{BodyChunk, BodyStream};
    use futures::stream::{FuturesUnordered, StreamExt};
    use futures::FutureExt;
    use std::cell::RefCell;
    use std::rc::Rc;
    enum Ev {
        Resp(crate::api::response::Response),
        Pumped(bool),
        Held,
    }
    let (tx, slot) = crate::api::stream::request_body_channel(8);
    let mut req = crate::api::dispatch::build_request(
        method.clone(),
        req_path.clone(),
        headers,
        Bytes::new(),
        client_ip,
        crate::api::request::HttpVersion::Http11,
    );
    req.body_stream = Some(slot);
    let first = &initial_body[..initial_body.len().min(content_length)];
    let first = (!first.is_empty()).then(|| Bytes::copy_from_slice(first));
    let mut body_done = false;
    // 本文を読み終える前に返ったストリーミング応答の本文と、その間に溜めた断片
    let held_stream: Rc<RefCell<Option<BodyStream>>> = Rc::new(RefCell::new(None));
    let held: Rc<RefCell<Vec<BodyChunk>>> = Rc::new(RefCell::new(Vec::new()));
    let mut resp = {
        let mut set: FuturesUnordered<
            std::pin::Pin<Box<dyn std::future::Future<Output = Ev> + '_>>,
        > = FuturesUnordered::new();
        set.push(Box::pin(
            crate::api::dispatch::call_with(entry, req, api_cache, comp).map(Ev::Resp),
        ));
        set.push(Box::pin(
            pump_api_body(&mut tls_stream, content_length, first, tx, overflow).map(Ev::Pumped),
        ));
        let mut early: Option<crate::api::response::Response> = None;
        loop {
            match set.next().await {
                Some(Ev::Resp(mut r)) => {
                    if body_done || !r.is_streaming() {
                        break r;
                    }
                    *held_stream.borrow_mut() = r.take_stream();
                    early = Some(r);
                    let (hs, h) = (held_stream.clone(), held.clone());
                    set.push(Box::pin(async move {
                        loop {
                            let next =
                                futures::future::poll_fn(|cx| match hs.borrow_mut().as_mut() {
                                    Some(s) => s.poll_next_unpin(cx),
                                    None => std::task::Poll::Ready(None),
                                })
                                .await;
                            match next {
                                Some(c) => h.borrow_mut().push(c),
                                None => break Ev::Held,
                            }
                        }
                    }));
                }
                Some(Ev::Pumped(ok)) => {
                    body_done = ok;
                    if let Some(r) = early.take() {
                        break r;
                    }
                }
                Some(Ev::Held) => {}
                None => match early.take() {
                    Some(r) => break r,
                    None => return None,
                },
            }
        }
    };
    if let Some(rest) = held_stream.borrow_mut().take() {
        let before: Vec<BodyChunk> = std::mem::take(&mut *held.borrow_mut());
        resp.stream = Some(Box::pin(futures::stream::iter(before).chain(rest)));
    }
    write_api_response(
        tls_stream,
        resp,
        security,
        client_wants_close || !body_done,
        api_cache,
    )
    .await
}

/// 要求本文（Content-Length）を読みながらハンドラの本文のチャネルへ送る（F-266）。全部読めたら `true`。
/// 本文の終わりより後ろまで読んだ分（次の要求）は `overflow` の後ろに足す（B-186）。
#[cfg(feature = "handler")]
async fn pump_api_body(
    stream: &mut ServerTls,
    content_length: usize,
    first: Option<Bytes>,
    mut tx: futures::channel::mpsc::Sender<Bytes>,
    overflow: &mut BytesMut,
) -> bool {
    use futures::SinkExt;
    let mut got = 0usize;
    if let Some(b) = first {
        got += b.len();
        if tx.send(b).await.is_err() {
            return false;
        }
    }
    while got < content_length {
        let buf = buf_get();
        match timeout(READ_TIMEOUT, stream.read(buf)).await {
            Ok((Ok(n), mut b)) if n > 0 => {
                b.set_valid_len(n);
                let s = b.as_valid_slice();
                let want = content_length - got;
                let take = &s[..s.len().min(want)];
                got += take.len();
                let chunk = Bytes::copy_from_slice(take);
                if s.len() > want {
                    overflow.extend_from_slice(&s[want..]);
                }
                buf_put(b);
                if tx.send(chunk).await.is_err() {
                    return false;
                }
            }
            Ok((_, b)) => {
                buf_put(b);
                return false;
            }
            Err(_) => return false,
        }
    }
    true
}

/// Wasm のローカル応答（`send_local_response`）を HTTP/1.1 の応答にする（Content-Length が無ければ補う。F-62）
#[cfg(feature = "wasm")]
fn wasm_local_response_h1(resp: &crate::wasm::LocalResponse) -> Vec<u8> {
    let mut response = Vec::with_capacity(64 + resp.body.len() + resp.headers.len() * 32);
    let _ = std::io::Write::write_fmt(
        &mut response,
        format_args!(
            "HTTP/1.1 {} {}\r\n",
            resp.status_code,
            crate::http_utils::status_reason_phrase(resp.status_code)
        ),
    );
    for (k, v) in &resp.headers {
        response.extend_from_slice(k);
        response.extend_from_slice(b": ");
        response.extend_from_slice(v);
        response.extend_from_slice(b"\r\n");
    }
    // F-62: モジュールが Content-Length を設定していない場合は補完する（欠落するとクライアントは接続
    // クローズまでボディを待ち続け、TLS では close_notify 無しの EOF エラーになる）
    if !resp
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case(b"content-length"))
    {
        let _ = std::io::Write::write_fmt(
            &mut response,
            format_args!("Content-Length: {}\r\n", resp.body.len()),
        );
    }
    response.extend_from_slice(b"\r\n");
    response.extend_from_slice(&resp.body);
    response
}

/// 本文を差し替えた要求のヘッダ（Transfer-Encoding・Content-Length を外して新しい Content-Length を付ける。F-271）。
#[cfg(feature = "wasm")]
fn set_body_framing_headers(headers: &mut Vec<(Bytes, Bytes)>, len: usize) {
    headers.retain(|(k, _)| {
        !k.eq_ignore_ascii_case(b"content-length") && !k.eq_ignore_ascii_case(b"transfer-encoding")
    });
    headers.push((
        Bytes::from_static(b"Content-Length"),
        Bytes::from(len.to_string()),
    ));
}

/// `wasm` feature 無しのビルドで handle_backend に渡す空のモジュール一覧。要求ごとに `Arc` を作らないよう
/// スレッドごとに 1 つを使い回す（F-202）。
#[cfg(not(feature = "wasm"))]
pub(super) fn no_wasm_modules() -> Arc<Vec<crate::wasm_plugin_config::ModuleRef>> {
    thread_local! {
        static EMPTY: Arc<Vec<crate::wasm_plugin_config::ModuleRef>> = Arc::new(Vec::new());
    }
    EMPTY.with(Arc::clone)
}

/// HTTP/1.1 のハンドラ応答で、ヘッダと同じバッファに続けて 1 回で書く本文の上限（F-202）。
/// 要求バッファのプールの大きい方（既定 4KB）に収まる大きさ。超えるものはヘッダと別に書く（複製しない）。
#[cfg(feature = "handler")]
pub(super) const API_INLINE_BODY_MAX: usize = 3 * 1024;
/// ステータス行と既定のヘッダの見積もり（プールのバッファの選択に使う）。
#[cfg(feature = "handler")]
pub(super) const API_HEAD_ESTIMATE: usize = 256;

/// ハンドラのストリーミング応答を HTTP/1.1 の chunked で書く（F-197）。トレーラは最後のチャンクの後に
/// 書く。`Abort`・書き込み失敗は `None`（接続を閉じる）。戻り値は送った本文のバイト数。
#[cfg(feature = "handler")]
pub(super) async fn write_api_chunked(
    mut tls_stream: ServerTls,
    mut stream: crate::api::stream::BodyStream,
) -> Option<(ServerTls, u64)> {
    use crate::api::stream::BodyChunk;
    use futures::StreamExt;
    let mut sent = 0u64;
    let mut trailers: Vec<(Bytes, Bytes)> = Vec::new();
    let mut carry: Option<BodyChunk> = None;
    loop {
        let chunk = match carry.take() {
            Some(c) => c,
            None => match stream.next().await {
                Some(c) => c,
                None => break,
            },
        };
        match chunk {
            BodyChunk::Data(b) if b.is_empty() => {}
            BodyChunk::Data(b) => {
                let b = crate::api::stream::coalesce(
                    b,
                    &mut stream,
                    crate::api::stream::COALESCE_MAX,
                    &mut carry,
                );
                let mut prefix = Vec::with_capacity(12);
                let _ = std::io::Write::write_fmt(&mut prefix, format_args!("{:x}\r\n", b.len()));
                sent += b.len() as u64;
                for part in [Bytes::from(prefix), b, Bytes::from_static(b"\r\n")] {
                    match timeout(WRITE_TIMEOUT, tls_stream.write_all(part)).await {
                        Ok((Ok(_), _)) => {}
                        _ => return None,
                    }
                }
            }
            BodyChunk::Trailers(t) => {
                trailers = t;
                break;
            }
            BodyChunk::Abort(reason) => {
                ftlog::debug!("[api] streaming response aborted: {}", reason);
                return None;
            }
        }
    }
    let mut tail = Vec::with_capacity(5 + trailers.len() * 32);
    tail.extend_from_slice(b"0\r\n");
    for (n, v) in &trailers {
        tail.extend_from_slice(n);
        tail.extend_from_slice(b": ");
        tail.extend_from_slice(v);
        tail.extend_from_slice(b"\r\n");
    }
    tail.extend_from_slice(b"\r\n");
    match timeout(WRITE_TIMEOUT, tls_stream.write_all(tail)).await {
        Ok((Ok(_), _)) => Some((tls_stream, sent)),
        _ => None,
    }
}

/// ハンドラ用に HTTP/1.1 のリクエスト本文を全部読む（Content-Length / chunked、上限 `limit`）。
/// 失敗はステータス（上限超過 413、それ以外 400）。Wasm の要求本文のフェーズ（F-271）も使う。
/// 本文の終わりより後ろまで読んだ分（次の要求）は `overflow` の後ろに足す（B-186）。
#[cfg(any(feature = "handler", feature = "wasm"))]
pub(super) async fn read_full_request_body(
    stream: &mut ServerTls,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
    limit: usize,
    overflow: &mut BytesMut,
) -> Result<Bytes, u16> {
    if is_chunked {
        let mut decoder = ChunkedDecoder::new_request(limit as u64);
        let mut raw: Vec<u8> = initial_body.to_vec();
        let mut state = decoder.feed(initial_body);
        while matches!(state, ChunkedFeedResult::Continue) {
            let buf = buf_get();
            match timeout(READ_TIMEOUT, stream.read(buf)).await {
                Ok((Ok(n), mut b)) if n > 0 => {
                    b.set_valid_len(n);
                    let data = b.as_valid_slice();
                    let (res, used) = decoder.feed_counted(data);
                    raw.extend_from_slice(&data[..used]);
                    if matches!(res, ChunkedFeedResult::Complete) {
                        overflow.extend_from_slice(&data[used..]);
                    }
                    state = res;
                    buf_put(b);
                }
                Ok((_, b)) => {
                    buf_put(b);
                    return Err(400);
                }
                Err(_) => return Err(400),
            }
        }
        match state {
            ChunkedFeedResult::Complete => Ok(Bytes::from(decode_chunked_body(&raw))),
            ChunkedFeedResult::SizeLimitExceeded => Err(413),
            ChunkedFeedResult::Continue | ChunkedFeedResult::Invalid => Err(400),
        }
    } else {
        if content_length > limit {
            return Err(413);
        }
        let mut body = Vec::with_capacity(content_length);
        body.extend_from_slice(&initial_body[..initial_body.len().min(content_length)]);
        while body.len() < content_length {
            let buf = buf_get();
            match timeout(READ_TIMEOUT, stream.read(buf)).await {
                Ok((Ok(n), mut b)) if n > 0 => {
                    b.set_valid_len(n);
                    let want = content_length - body.len();
                    let got = b.as_valid_slice();
                    body.extend_from_slice(&got[..got.len().min(want)]);
                    if got.len() > want {
                        overflow.extend_from_slice(&got[want..]);
                    }
                    buf_put(b);
                }
                Ok((_, b)) => {
                    buf_put(b);
                    return Err(400);
                }
                Err(_) => return Err(400),
            }
        }
        Ok(Bytes::from(body))
    }
}

// ====================
// リダイレクト処理
// ====================
//
// 設定されたURLへのHTTPリダイレクトを返します。
// ステータスコード: 301, 302, 303, 307, 308 をサポート
//
// 特殊変数:
// - $request_uri: 元のリクエストURI
// - $host: リクエストのHostヘッダー
// - $path: 元のパス（prefix除去後）
// ====================

/// リダイレクトレスポンスを生成して送信
pub(super) async fn handle_redirect(
    mut tls_stream: ServerTls,
    redirect_url: &str,
    status_code: u16,
    preserve_path: bool,
    req_path: &[u8],
    prefix: &[u8],
    client_wants_close: bool,
    security: &SecurityConfig,
) -> Option<(ServerTls, u16, u64, bool)> {
    // リダイレクト先URLを構築
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    let prefix_str = std::str::from_utf8(prefix).unwrap_or("");

    // パス部分（prefix除去後）
    let sub_path = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
        &path_str[prefix_str.len()..]
    } else {
        path_str
    };

    // 変数置換とパス追加
    let mut final_url = redirect_url
        .replace("$request_uri", path_str)
        .replace("$path", sub_path);

    // preserve_path が true の場合、元のパスを追加
    if preserve_path && !sub_path.is_empty() {
        // URLにすでにパスがある場合は結合
        if final_url.ends_with('/') && sub_path.starts_with('/') {
            final_url.push_str(&sub_path[1..]);
        } else if !final_url.ends_with('/') && !sub_path.starts_with('/') {
            final_url.push('/');
            final_url.push_str(sub_path);
        } else {
            final_url.push_str(sub_path);
        }
    }

    // ステータス行を構築
    let status_line = match status_code {
        301 => "HTTP/1.1 301 Moved Permanently\r\n",
        302 => "HTTP/1.1 302 Found\r\n",
        303 => "HTTP/1.1 303 See Other\r\n",
        307 => "HTTP/1.1 307 Temporary Redirect\r\n",
        308 => "HTTP/1.1 308 Permanent Redirect\r\n",
        _ => "HTTP/1.1 301 Moved Permanently\r\n",
    };

    // レスポンス構築
    let mut response = Vec::with_capacity(256 + final_url.len());
    response.extend_from_slice(status_line.as_bytes());
    response.extend_from_slice(b"Location: ");
    response.extend_from_slice(final_url.as_bytes());
    response.extend_from_slice(b"\r\nContent-Length: 0\r\n");
    // `[route.traffic.cors]` の実際の応答のヘッダ（F-273）
    crate::route_cors::append_h1_lines(&mut response, security);

    if client_wants_close {
        response.extend_from_slice(b"Connection: close\r\n\r\n");
    } else {
        response.extend_from_slice(b"Connection: keep-alive\r\n\r\n");
    }

    // レスポンス送信
    let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(response)).await;
    match write_result {
        Ok((Ok(_), _)) => Some((tls_stream, status_code, 0, client_wants_close)),
        _ => None,
    }
}

#[cfg(all(test, unix))]
mod b186_overflow_tests {
    //! B-186: 本文を読む経路が本文の終わりより後ろまで読んだ分（次の要求）を呼び出し側へ返すこと。
    use super::*;
    use std::io::Write as _;
    use std::os::unix::io::FromRawFd as _;
    use std::os::unix::net::UnixStream;

    #[cfg(all(veil_rt_uring, target_os = "linux"))]
    fn runtime_available() -> bool {
        crate::runtime::ring::IoUring::new(8, 0).is_ok()
    }

    #[cfg(not(all(veil_rt_uring, target_os = "linux")))]
    fn runtime_available() -> bool {
        true
    }

    /// (サーバ側の非ブロッキング `TcpStream`, クライアント側の同期 `UnixStream`)
    fn pair() -> (crate::runtime::tcp::TcpStream, UnixStream) {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: fds は 2 要素の有効な配列
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(rc, 0);
        // SAFETY: socketpair が返した有効な fd
        unsafe {
            let fl = libc::fcntl(fds[0], libc::F_GETFL);
            libc::fcntl(fds[0], libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        // SAFETY: socketpair が返した所有 fd をそれぞれ一度だけ所有権移動する
        unsafe {
            (
                crate::runtime::tcp::TcpStream::from_raw_fd(fds[0]),
                UnixStream::from_raw_fd(fds[1]),
            )
        }
    }

    async fn server(stream: crate::runtime::tcp::TcpStream) -> ServerTls {
        #[cfg(veil_ktls)]
        let s = crate::ktls_rustls::accept_plain(stream, None).await;
        #[cfg(not(veil_ktls))]
        let s = crate::simple_tls::accept_plain(stream, None).await;
        s.expect("plain stream")
    }

    const NEXT: &[u8] = b"GET /next HTTP/1.1\r\nHost: a\r\n\r\n";

    /// 本文の残りと次の要求を 1 回で書く
    fn send_rest(mut c: &UnixStream, rest: &[u8]) {
        let mut buf = rest.to_vec();
        buf.extend_from_slice(NEXT);
        c.write_all(&buf).unwrap();
    }

    #[test]
    fn drain_request_body_returns_pipelined_bytes() {
        if !runtime_available() {
            return;
        }
        crate::runtime::block_on(async {
            // Content-Length
            let (s, c) = pair();
            let mut tls = server(s).await;
            send_rest(&c, b"lo world");
            let mut over = BytesMut::new();
            assert!(drain_request_body(&mut tls, 11, false, b"hel", &mut over).await);
            assert_eq!(&over[..], NEXT);
            // chunked（終端が次の要求と同じ読み取りに入る）
            let (s, c) = pair();
            let mut tls = server(s).await;
            send_rest(&c, b"lo\r\n0\r\n\r\n");
            let mut over = BytesMut::new();
            assert!(drain_request_body(&mut tls, 0, true, b"5\r\nhel", &mut over).await);
            assert_eq!(&over[..], NEXT);
        });
    }

    #[cfg(any(feature = "handler", feature = "wasm"))]
    #[test]
    fn read_full_request_body_returns_pipelined_bytes() {
        if !runtime_available() {
            return;
        }
        crate::runtime::block_on(async {
            let (s, c) = pair();
            let mut tls = server(s).await;
            send_rest(&c, b"lo world");
            let mut over = BytesMut::new();
            let body = read_full_request_body(&mut tls, 11, false, b"hel", 1024, &mut over)
                .await
                .unwrap();
            assert_eq!(&body[..], b"hello world");
            assert_eq!(&over[..], NEXT);
            let (s, c) = pair();
            let mut tls = server(s).await;
            send_rest(&c, b"lo\r\n0\r\n\r\n");
            let mut over = BytesMut::new();
            let body = read_full_request_body(&mut tls, 0, true, b"5\r\nhel", 1024, &mut over)
                .await
                .unwrap();
            assert_eq!(&body[..], b"hello");
            assert_eq!(&over[..], NEXT);
        });
    }

    #[cfg(feature = "handler")]
    #[test]
    fn pump_api_body_returns_pipelined_bytes() {
        use futures::StreamExt;
        if !runtime_available() {
            return;
        }
        crate::runtime::block_on(async {
            let (s, c) = pair();
            let mut tls = server(s).await;
            send_rest(&c, b"lo world");
            let (tx, slot) = crate::api::stream::request_body_channel(8);
            let mut rx = slot.lock().unwrap().take().unwrap();
            let mut over = BytesMut::new();
            let first = Some(Bytes::from_static(b"hel"));
            assert!(pump_api_body(&mut tls, 11, first, tx, &mut over).await);
            let mut got = Vec::new();
            while let Some(b) = rx.next().await {
                got.extend_from_slice(&b);
            }
            assert_eq!(&got[..], b"hello world");
            assert_eq!(&over[..], NEXT);
        });
    }
}
