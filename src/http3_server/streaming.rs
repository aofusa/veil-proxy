//! ストリーミング用の関数（要求の head の構築・パスの計算・ストリームの駆動。F-32）
//!
//! F-233 で `src/http3_server.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// F-32: ストリーミング用フリー関数（リクエスト head 構築・パス計算・ストリーム駆動）
// ====================

/// プレフィックス除去 + `path_prefix` 連結でバックエンドへ送るパスを構築する。
/// `handle_proxy` と同一ロジック（挙動を一致させるため共有）。
pub(super) fn compute_backend_path(target: &ProxyTarget, req_path: &[u8], prefix: &[u8]) -> String {
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    compute_upstream_request_path(path_str, prefix, &target.path_prefix, false)
}

/// 上流リクエストパスを構築する。
///
/// - `preserve_full_path = true`（gRPC）: ルート `/*` プレフィックスを除去せずフルパスを返す（B-39）
/// - それ以外: `prefix` を剥がし `target_path_prefix` を前置
pub(super) fn compute_upstream_request_path(
    path_str: &str,
    prefix: &[u8],
    target_path_prefix: &str,
    preserve_full_path: bool,
) -> String {
    if preserve_full_path {
        return if path_str.is_empty() {
            "/".to_string()
        } else {
            path_str.to_string()
        };
    }

    let sub_path = if prefix.is_empty() {
        path_str.to_string()
    } else {
        let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
        if let Some(remaining) = path_str.strip_prefix(prefix_str) {
            let base = target_path_prefix.trim_end_matches('/');
            if remaining.is_empty() {
                if base.is_empty() {
                    "/".to_string()
                } else {
                    format!("{}/", base)
                }
            } else if remaining.starts_with('/') {
                if base.is_empty() {
                    remaining.to_string()
                } else {
                    format!("{}{}", base, remaining)
                }
            } else if base.is_empty() {
                format!("/{}", remaining)
            } else {
                format!("{}/{}", base, remaining)
            }
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

/// リクエストヘッダが gRPC（`application/grpc*`）かどうかを判定する。
#[cfg(feature = "grpc")]
pub(super) fn header_pairs_indicate_grpc(headers: &[(Vec<u8>, Vec<u8>)]) -> bool {
    headers.iter().any(|(n, v)| {
        n.eq_ignore_ascii_case(b"content-type") && crate::grpc::headers::is_grpc_content_type(v)
    })
}

/// gRPC over H3 の初期応答ヘッダから、trailers / 擬似ヘッダ / 重複を除外する（B-41）。
///
/// `grpc-status` / `grpc-message` は `send_additional_headers` の trailers 専用にし、
/// 初期 HEADERS には載せない。
#[cfg(feature = "grpc")]
pub(super) fn filter_h3_grpc_initial_headers<'a>(
    headers: &'a [(&'a [u8], &'a [u8])],
) -> impl Iterator<Item = (&'a [u8], &'a [u8])> + 'a {
    headers.iter().copied().filter(|(name, _)| {
        *name != b":status"
            && !name.eq_ignore_ascii_case(b"content-type")
            && !name.eq_ignore_ascii_case(b"grpc-status")
            && !name.eq_ignore_ascii_case(b"grpc-message")
            && !name.eq_ignore_ascii_case(b"content-length")
    })
}

/// ホップバイホップヘッダを除き、必要なら CL/CE を除いたうえで trailers をマージする（B-39）。
pub(super) fn merge_response_headers_and_trailers(
    headers: &[(Vec<u8>, Vec<u8>)],
    trailers: &[(Vec<u8>, Vec<u8>)],
    skip_content_length_encoding: bool,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut owned: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(headers.len() + trailers.len());
    for (name, value) in headers {
        if name.eq_ignore_ascii_case(b"connection")
            || name.eq_ignore_ascii_case(b"transfer-encoding")
            || name.eq_ignore_ascii_case(b"keep-alive")
        {
            continue;
        }
        if skip_content_length_encoding
            && (name.eq_ignore_ascii_case(b"content-length")
                || name.eq_ignore_ascii_case(b"content-encoding"))
        {
            continue;
        }
        owned.push((name.clone(), value.clone()));
    }
    for (name, value) in trailers {
        if !owned.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
            owned.push((name.clone(), value.clone()));
        }
    }
    owned
}

/// HTTP/1.1 リクエスト head（リクエストライン + ヘッダ、**末尾の空行は含めない**）を構築する。
///
/// ボディフレーミング（`Transfer-Encoding: chunked` か無しか）と末尾の空行は、**実際に
/// ボディデータが来たか**をバックエンドタスクが判定してから付与する（HTTP/3 では HEADERS
/// 受信時点でボディ有無が確定しないため。例: h3 クライアントが HEADERS と fin を別送する GET は
/// `more_frames=true` でもボディなし）。B-104: `Connection: close` は付けない（HTTP/1.1 の既定の
/// キープアライブで上流接続をワーカーのプールへ返して再利用する）。
pub(super) fn build_h1_request_head(
    target: &ProxyTarget,
    method: &[u8],
    final_path: &str,
    headers: &[h3::Header],
) -> Vec<u8> {
    let mut req = Vec::with_capacity(512);
    req.extend_from_slice(method);
    req.push(b' ');
    req.extend_from_slice(final_path.as_bytes());
    req.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    req.extend_from_slice(target.host.as_bytes());
    if !target.is_default_port() {
        req.push(b':');
        let mut port_buf = itoa::Buffer::new();
        req.extend_from_slice(port_buf.format(target.port).as_bytes());
    }
    req.extend_from_slice(b"\r\n");

    for header in headers {
        let name = header.name();
        // B-11: expect はプロキシが終端する（ボディを無条件転送するため、バックエンドに
        // 100 Continue 中間応答を出させない）。
        if name.starts_with(b":")
            || name.eq_ignore_ascii_case(b"connection")
            || name.eq_ignore_ascii_case(b"keep-alive")
            || name.eq_ignore_ascii_case(b"transfer-encoding")
            || name.eq_ignore_ascii_case(b"content-length")
            || name.eq_ignore_ascii_case(b"expect")
        {
            continue;
        }
        req.extend_from_slice(name);
        req.extend_from_slice(b": ");
        req.extend_from_slice(header.value());
        req.extend_from_slice(b"\r\n");
    }

    // ボディフレーミングと末尾空行はタスク側で付与する。
    req
}

/// 1 ストリームの駆動結果。`(did_work, done)`。`did_work` は F-151 のダーティ集合維持判定
/// （進捗があれば呼び出し側がダーティのまま再投入する）、`done` が `true` なら呼び出し側が
/// `proxy_streams` から除去する。
pub(super) fn drive_proxy_stream(
    h3: &mut h3::Connection,
    conn: &mut quiche::Connection,
    stream_id: u64,
    ps: &mut ProxyStream,
) -> (bool, bool) {
    let pump_work = drive_request_pump(h3, conn, stream_id, ps);
    let flush_work = drive_response_flush(h3, conn, stream_id, ps);
    // 完了条件: レスポンス fin 送出済み かつ リクエスト側クローズ済み。
    let done = ps.resp_fin_sent && ps.req_tx.is_none() && ps.req_pending.is_empty();
    (pump_work || flush_work, done)
}

/// リクエストボディ pump: `recv_body` → req チャネル（フロー制御 + バックプレッシャ）。
///
/// F-151: 戻り値は「1 件でも進捗があったか」。フロー制御で完全にブロックされた
/// （`Full`/`is_full`）だけの呼び出しは `false`（進捗なし）とし、ダーティ集合からの
/// ビジーループ再投入を避ける。それ以外の状態遷移（送出・EOF 検出・破棄）は `true` とし、
/// B-12 の不変条件（イベントが残っているのに誰も見なくなる経路を作らない）を安全側に倒す。
pub(super) fn drive_request_pump(
    h3: &mut h3::Connection,
    conn: &mut quiche::Connection,
    stream_id: u64,
    ps: &mut ProxyStream,
) -> bool {
    use crate::http3_stream::TrySendError;
    let mut did_work = false;
    // バックエンドタスクが終了して本文が不要になった（チャネルが満杯のまま受信側が
    // 閉じた場合も含む。満杯だと try_send に到達せず Closed を観測できない）。
    if ps.req_tx.as_ref().is_some_and(|t| t.is_closed()) && !ps.req_too_large {
        abandon_request_body(conn, stream_id, ps);
        return true;
    }
    let tx = match &ps.req_tx {
        Some(t) => t,
        None => return did_work,
    };

    // ボディ上限超過済みなら何もしない（応答 flush 側で 413 + リセット）。
    if ps.req_too_large {
        return did_work;
    }

    // 1. 未投入ボディ（初回バッチ/溢れ分）を先に流す（ゼロコピー: clone せず move）。
    while let Some(front) = ps.req_pending.pop_front() {
        match tx.try_send(front) {
            Ok(()) => did_work = true,
            Err(TrySendError::Full(item)) => {
                ps.req_pending.push_front(item); // バックプレッシャ: recv_body も止める。
                return did_work;
            }
            Err(TrySendError::Closed) => {
                abandon_request_body(conn, stream_id, ps);
                return true;
            }
        }
    }

    // 2. quiche から recv_body してチャネルへ（容量がある間だけ = バックプレッシャ）。
    if ps.req_readable {
        loop {
            if tx.is_full() {
                return did_work; // データを quiche に残す → フロー制御でクライアント送信が止まる。
            }
            let mut buf = BytesMut::with_capacity(REQ_RECV_CHUNK);
            let spare = buf.spare_capacity_mut();
            // SAFETY: recv_body は read バイトのみ初期化。advance_mut で len に反映。
            let spare_u8 = unsafe {
                std::slice::from_raw_parts_mut(spare.as_mut_ptr() as *mut u8, spare.len())
            };
            match h3.recv_body(conn, stream_id, spare_u8) {
                Ok(n) if n > 0 => {
                    did_work = true;
                    unsafe { buf.advance_mut(n) };
                    ps.req_bytes_total += n as u64;
                    // ボディ上限チェック（0 = 無制限）。
                    if ps.max_request_body > 0 && ps.req_bytes_total > ps.max_request_body {
                        ps.req_too_large = true;
                        ps.req_tx = None; // バックエンドタスクを中断。
                        ps.req_pending.clear();
                        // クライアントの送信を止める。
                        let _ = conn.stream_shutdown(stream_id, quiche::Shutdown::Read, 0);
                        return true;
                    }
                    match tx.try_send(buf.freeze()) {
                        Ok(()) => continue,
                        Err(TrySendError::Full(b)) => {
                            ps.req_pending.push_back(b);
                            return did_work;
                        }
                        Err(TrySendError::Closed) => {
                            abandon_request_body(conn, stream_id, ps);
                            return true;
                        }
                    }
                }
                Ok(_) | Err(h3::Error::Done) => {
                    ps.req_readable = false;
                    break;
                }
                Err(e) => {
                    debug!("[HTTP/3] recv_body (stream) error: {}", e);
                    ps.req_readable = false;
                    break;
                }
            }
        }
    }

    // B-12: fin を含む最終データを本 pump の `recv_body`（`h3.poll()` の外）で消費した場合、
    // h3 の `Finished` イベントは内部キューに積まれるが、`poll` はパケット受信時にしか
    // 呼ばれないため、クライアントが送信を終えると新規パケットが来ず永久に取り出されない
    // （EOF 未伝播 → バックエンドタスクが待機 → レスポンス無し → QUIC アイドルタイムアウト）。
    // トランスポート層の `stream_finished`（fin 受信済みかつ全データ消費済み）を直接確認して
    // EOF を伝播する。
    if !ps.req_eof_seen && conn.stream_finished(stream_id) {
        ps.req_eof_seen = true;
        did_work = true; // 新規に EOF を検出 = 進捗（response flush 側が最終処理できるように）。
    }

    // 3. クライアント END_STREAM 受信かつ全消化なら送信端を閉じて EOF 伝播。
    if ps.req_eof_seen && ps.req_pending.is_empty() && !ps.req_readable {
        ps.req_tx = None;
        did_work = true;
    }

    did_work
}

/// バックエンドタスクが要求本文を必要としなくなった（早期応答・バックエンド切断で
/// req チャネルの受信側が閉じた）とき、要求ストリームの受信を打ち切る。
///
/// 以前は `req_tx` を落とすだけで `recv_body` を呼ばなくなっていたため、未受信の本文が
/// quiche に溜まって QUIC のフロー制御ウィンドウが補充されず、**本文を送り切ってから応答を
/// 読むクライアントは送信が止まったまま応答を受け取れず、アイドルタイムアウトまで停止した**
/// （B-68 の 30 秒待ちの直接の原因）。RFC 9114 §4.1.1 に従い `STOP_SENDING(H3_NO_ERROR)` で
/// 送信停止を求める（quiche は以後の受信データを破棄し、クライアントは応答を読める）。
/// クライアントが既に本文を送り切っている（fin 受信済み）場合は何もしない。
pub(super) fn abandon_request_body(
    conn: &mut quiche::Connection,
    stream_id: u64,
    ps: &mut ProxyStream,
) {
    /// RFC 9114 §8.1 H3_NO_ERROR
    const H3_NO_ERROR: u64 = 0x100;
    ps.req_pending.clear();
    ps.req_tx = None;
    if !ps.req_eof_seen && !conn.stream_finished(stream_id) {
        let _ = conn.stream_shutdown(stream_id, quiche::Shutdown::Read, H3_NO_ERROR);
    }
}

/// レスポンス flush: resp チャネル → `send_response`/`send_body`（フロー制御 + 部分送信保持）。
///
/// F-151: 戻り値は「1 件でも進捗があったか」（ヘッダ/ボディ/fin を実際に送出できた、または
/// エラー応答で完了させた）。`Blocked`（quiche 側フロー制御で 0 バイトも送れなかった）は
/// 進捗なしとして扱い、ビジーループ再投入を避ける。
pub(super) fn drive_response_flush(
    h3: &mut h3::Connection,
    conn: &mut quiche::Connection,
    stream_id: u64,
    ps: &mut ProxyStream,
) -> bool {
    use crate::http3_stream::{RespMsg, TryRecv};

    if ps.resp_fin_sent {
        return false;
    }

    let mut did_work = false;

    // ボディ上限超過 → 413 を返して終了（応答未開始時のみ）。
    if ps.req_too_large && !ps.resp_started {
        send_simple_h3_error(h3, conn, stream_id, 413);
        ps.resp_fin_sent = true;
        return true;
    }

    // 0. 保留中の fin を再送。
    if ps.need_fin {
        if try_send_h3_fin(h3, conn, stream_id) {
            ps.resp_fin_sent = true;
            ps.need_fin = false;
            did_work = true;
        }
        return did_work;
    }

    // 1. StreamBlocked で保留した head を再送。
    if let Some((status, headers)) = ps.head_pending.take() {
        match send_h3_head(h3, conn, stream_id, status, &headers) {
            HeadSend::Sent => {
                ps.resp_started = true;
                did_work = true;
            }
            HeadSend::Blocked => {
                ps.head_pending = Some((status, headers));
                return did_work;
            }
            HeadSend::Error => {
                ps.resp_fin_sent = true;
                return true;
            }
        }
    }

    // 2. 部分送信のボディ断片を flush。
    if let Some((buf, off, fin)) = ps.body_pending.take() {
        match send_h3_body(h3, conn, stream_id, &buf, off, fin) {
            BodySend::Done => {
                did_work = true;
                if fin {
                    ps.resp_fin_sent = true;
                    return true;
                }
            }
            BodySend::Partial(new_off) => {
                did_work = true; // 一部でも送れたので進捗あり。
                ps.body_pending = Some((buf, new_off, fin));
                return did_work;
            }
            BodySend::Blocked => {
                ps.body_pending = Some((buf, off, fin));
                return did_work;
            }
            BodySend::Error => {
                ps.resp_fin_sent = true;
                return true;
            }
        }
    }

    // 2b. StreamBlocked で保留した trailers を再送（gRPC。B-97）。
    #[cfg(any(feature = "grpc", feature = "handler"))]
    if let Some(trailers) = ps.trailers_pending.take() {
        return match send_h3_trailers(h3, conn, stream_id, &trailers) {
            HeadSend::Blocked => {
                ps.trailers_pending = Some(trailers);
                did_work
            }
            HeadSend::Sent | HeadSend::Error => {
                ps.resp_fin_sent = true;
                true
            }
        };
    }

    // 3. チャネルを排出して送出。
    loop {
        if ps.head_pending.is_some() || ps.body_pending.is_some() {
            return did_work;
        }
        match ps.resp_rx.try_recv() {
            TryRecv::Item(RespMsg::Head { status, headers }) => {
                // 本文の無い応答（リダイレクト・HEAD 等）は HEADERS に fin を載せる。
                let fin = ps.resp_rx.is_finished();
                match send_h3_head_fin(h3, conn, stream_id, status, &headers, fin) {
                    HeadSend::Sent => {
                        ps.resp_started = true;
                        did_work = true;
                        if fin {
                            ps.resp_fin_sent = true;
                            return true;
                        }
                    }
                    HeadSend::Blocked => {
                        ps.head_pending = Some((status, headers));
                        return did_work;
                    }
                    HeadSend::Error => {
                        ps.resp_fin_sent = true;
                        return true;
                    }
                }
            }
            TryRecv::Item(RespMsg::Body(b)) => {
                // 最後の断片なら終端を同じ送出へ載せる（空の fin を別途送らない）。
                let fin = ps.resp_rx.is_finished();
                match send_h3_body(h3, conn, stream_id, &b, 0, fin) {
                    BodySend::Done => {
                        did_work = true;
                        if fin {
                            ps.resp_fin_sent = true;
                            return true;
                        }
                    }
                    BodySend::Partial(off) => {
                        did_work = true;
                        ps.body_pending = Some((b, off, fin));
                        return did_work;
                    }
                    BodySend::Blocked => {
                        ps.body_pending = Some((b, 0, fin));
                        return did_work;
                    }
                    BodySend::Error => {
                        ps.resp_fin_sent = true;
                        return true;
                    }
                }
            }
            #[cfg(any(feature = "grpc", feature = "handler"))]
            TryRecv::Item(RespMsg::Trailers(trailers)) => {
                return match send_h3_trailers(h3, conn, stream_id, &trailers) {
                    HeadSend::Blocked => {
                        ps.trailers_pending = Some(trailers);
                        did_work
                    }
                    HeadSend::Sent | HeadSend::Error => {
                        ps.resp_fin_sent = true;
                        true
                    }
                };
            }
            TryRecv::Item(RespMsg::Error { status }) => {
                if !ps.resp_started {
                    send_simple_h3_error(h3, conn, stream_id, status);
                } else {
                    // 応答途中のエラー: ストリームをリセット。
                    let _ = conn.stream_shutdown(stream_id, quiche::Shutdown::Write, 0x10c);
                }
                ps.resp_fin_sent = true;
                return true;
            }
            TryRecv::Closed => {
                // バックエンド完了 → fin 送出。
                if ps.resp_started {
                    if try_send_h3_fin(h3, conn, stream_id) {
                        ps.resp_fin_sent = true;
                    } else {
                        ps.need_fin = true;
                    }
                } else {
                    // head を一度も生成できなかった → 502。
                    send_simple_h3_error(h3, conn, stream_id, 502);
                    ps.resp_fin_sent = true;
                }
                return true;
            }
            TryRecv::Empty => return did_work,
        }
    }
}

/// head 送出の結果。
pub(super) enum HeadSend {
    Sent,
    Blocked,
    Error,
}

/// body 送出の結果。
pub(super) enum BodySend {
    Done,
    Partial(usize),
    Blocked,
    Error,
}

/// レスポンス head（`:status` + ヘッダ）を `send_response(fin=false)` で送る。
pub(super) fn send_h3_head(
    h3: &mut h3::Connection,
    conn: &mut quiche::Connection,
    stream_id: u64,
    status: u16,
    headers: &[(Bytes, Bytes)],
) -> HeadSend {
    send_h3_head_fin(h3, conn, stream_id, status, headers, false)
}

/// レスポンス head を送る。`fin` なら HEADERS でストリームを閉じる（本文の無い応答）。
pub(super) fn send_h3_head_fin(
    h3: &mut h3::Connection,
    conn: &mut quiche::Connection,
    stream_id: u64,
    status: u16,
    headers: &[(Bytes, Bytes)],
    fin: bool,
) -> HeadSend {
    let mut status_buf = itoa::Buffer::new();
    let status_str = status_buf.format(status);
    // 借用ヘッダ（`HeaderRef`）で渡す（`h3::Header::new` は名前と値を毎回 `Vec` にコピーする）。
    let mut h3_headers: Vec<h3::HeaderRef<'_>> = Vec::with_capacity(headers.len() + 2);
    h3_headers.push(h3::HeaderRef::new(b":status", status_str.as_bytes()));
    h3_headers.push(h3::HeaderRef::new(b"server", b"veil/http3"));
    for (name, value) in headers {
        if name.eq_ignore_ascii_case(b":status") || name.eq_ignore_ascii_case(b"server") {
            continue;
        }
        h3_headers.push(h3::HeaderRef::new(name, value));
    }
    match h3.send_response(conn, stream_id, &h3_headers, fin) {
        Ok(()) => HeadSend::Sent,
        Err(h3::Error::StreamBlocked) => HeadSend::Blocked,
        Err(e) => {
            warn!("[HTTP/3] streaming send_response error: {}", e);
            HeadSend::Error
        }
    }
}

/// trailers を `send_additional_headers(is_trailer_section=true, fin=true)` で送る（B-41/B-97）。
///
/// `Error` のときは空ボディ + fin でストリームを閉じ、クライアントのハングを防ぐ。
#[cfg(any(feature = "grpc", feature = "handler"))]
pub(super) fn send_h3_trailers(
    h3: &mut h3::Connection,
    conn: &mut quiche::Connection,
    stream_id: u64,
    trailers: &[(Bytes, Bytes)],
) -> HeadSend {
    let refs: Vec<h3::HeaderRef<'_>> = trailers
        .iter()
        .map(|(n, v)| h3::HeaderRef::new(n, v))
        .collect();
    match h3.send_additional_headers(conn, stream_id, &refs, true, true) {
        Ok(()) => HeadSend::Sent,
        Err(h3::Error::StreamBlocked) => HeadSend::Blocked,
        Err(e) => {
            warn!(
                "[HTTP/3] gRPC trailers send_additional_headers error: {}",
                e
            );
            if let Err(e2) = h3.send_body(conn, stream_id, &[], true) {
                debug!("[HTTP/3] gRPC trailers fin fallback error: {:?}", e2);
            }
            HeadSend::Error
        }
    }
}

/// ボディ断片を `send_body` で送る（`off` から）。`fin` は最後の断片に終端を載せる。
pub(super) fn send_h3_body(
    h3: &mut h3::Connection,
    conn: &mut quiche::Connection,
    stream_id: u64,
    buf: &[u8],
    off: usize,
    fin: bool,
) -> BodySend {
    if off >= buf.len() && !fin {
        return BodySend::Done;
    }
    match h3.send_body(conn, stream_id, &buf[off..], fin) {
        Ok(n) => {
            let new_off = off + n;
            if new_off >= buf.len() {
                BodySend::Done
            } else {
                BodySend::Partial(new_off)
            }
        }
        Err(h3::Error::Done) => BodySend::Blocked, // 送信バッファ/フロー制御で送れない。
        Err(e) => {
            warn!("[HTTP/3] streaming send_body error: {}", e);
            BodySend::Error
        }
    }
}

/// 空ボディ + fin を送る。`true` で fin 送出完了、`false` でブロック（再試行）。
pub(super) fn try_send_h3_fin(
    h3: &mut h3::Connection,
    conn: &mut quiche::Connection,
    stream_id: u64,
) -> bool {
    match h3.send_body(conn, stream_id, b"", true) {
        Ok(_) => true,
        Err(h3::Error::Done) => false,
        Err(e) => {
            debug!("[HTTP/3] streaming fin send error: {}", e);
            true // これ以上どうにもならないため完了扱い。
        }
    }
}

/// 簡易エラーレスポンス（head + 小ボディ + fin）を送る。
pub(super) fn send_simple_h3_error(
    h3: &mut h3::Connection,
    conn: &mut quiche::Connection,
    stream_id: u64,
    status: u16,
) {
    let mut status_buf = itoa::Buffer::new();
    let status_str = status_buf.format(status);
    let body: &[u8] = match status {
        413 => b"Payload Too Large",
        502 => b"Bad Gateway",
        504 => b"Gateway Timeout",
        _ => b"Error",
    };
    let mut len_buf = itoa::Buffer::new();
    let h3_headers = [
        h3::Header::new(b":status", status_str.as_bytes()),
        h3::Header::new(b"server", b"veil/http3"),
        h3::Header::new(b"content-type", b"text/plain"),
        h3::Header::new(b"content-length", len_buf.format(body.len()).as_bytes()),
    ];
    match h3.send_response(conn, stream_id, &h3_headers, false) {
        Ok(()) => {
            let _ = h3.send_body(conn, stream_id, body, true);
        }
        Err(e) => debug!("[HTTP/3] streaming error response send failed: {}", e),
    }
}
