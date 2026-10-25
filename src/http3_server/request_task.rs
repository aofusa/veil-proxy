//! バッファ経路の要求タスク（メインループから切り離す。B-97）
//!
//! F-233 で `src/http3_server.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// B-97: バッファ経路の要求タスク（メインループから切り離す）
// ====================

/// B-97: バッファ経路（静的配信・メトリクス・gRPC・Wasm 適用・h2c 上流・`buffering = "full"` 等）の
/// 要求 1 件を処理するタスクの文脈。
///
/// 従来はメインループが `handle_request` を await しており、上流 I/O・offload・Wasm の実行中は
/// **そのワーカーの全 HTTP/3 接続の受信・ACK・送信が止まっていた**。ストリーミング経路
/// （F-32）と同じアクターモデルに揃え、タスクは quiche に一切触れず、応答を
/// [`crate::http3_stream::RespMsg`] としてチャネルへ流す。quiche への送出（フロー制御・
/// StreamBlocked の保留）はメインループの `drive_proxy_streams` が行う。
pub(super) struct H3BufferedTask {
    /// ログ用のストリーム ID。
    pub(super) stream_id: u64,
    pub(super) peer_addr: SocketAddr,
    pub(super) client_ip: crate::http_utils::IpStr,
    /// 応答の送信端。応答を送り切った時点で `None`（drop = fin）にし、アクセスログ・
    /// Wasm `on_log` の完了を待たずに fin を出せるようにする。
    pub(super) resp_tx: Option<crate::http3_stream::Sender<crate::http3_stream::RespMsg>>,
    pub(super) notify: crate::http3_stream::ConnWaker,
    /// ルートの `[route.traffic.cors]` の実際の応答に足すヘッダ（F-273。ルートの解決の後に入れ、応答の head に足す）
    pub(super) cors: Option<std::sync::Arc<crate::route_cors::CorsActual>>,
}

/// バッファ経路の要求 Future を即座に 1 回だけ poll する（B-97）。
///
/// キャッシュに当たる静的配信・ローカル応答のように await せずに完了する要求は、ここで
/// 応答をチャネルへ積み終え、同じイテレーションの `drive_proxy_streams` で送出される
/// （タスクとして spawn すると、メインループのイテレーション末尾の `yield_now` まで実行されず、
/// 応答の送出が 1 イテレーション遅れて `h3_file` 3B で約 12% 退行した）。
///
/// 待ちに入った Future は `FuturesUnordered` に残り、起床するとメインループの select の
/// アームで poll される。ここでの poll は no-op Waker で行うが、`FuturesUnordered` は
/// 各 Future に自前の Waker を渡すため取りこぼしはない（select のアームが実 Waker で
/// poll し直してから待機に入る）。
pub(super) fn poll_buffered_once<F: std::future::Future<Output = ()>>(
    futs: &mut futures::stream::FuturesUnordered<F>,
) {
    use futures::StreamExt;
    let mut cx = std::task::Context::from_waker(futures::task::noop_waker_ref());
    while let std::task::Poll::Ready(Some(())) = futs.poll_next_unpin(&mut cx) {}
}

/// メインループの select アーム: 起床したバッファ経路の要求 Future を進める（B-97）。
///
/// 1 件でも完了したら `Ready`。完了しない進捗（応答断片の送出）は Future 内の
/// `ConnWaker::notify` が notify アームを起こす。空のときは `Pending` のまま（`Ready(None)`
/// で select を空回りさせない）。
pub(super) fn poll_buffered_arm<F: std::future::Future<Output = ()>>(
    futs: &mut futures::stream::FuturesUnordered<F>,
    cx: &mut std::task::Context<'_>,
) -> std::task::Poll<()> {
    use futures::StreamExt;
    let mut completed = false;
    while let std::task::Poll::Ready(Some(())) = futs.poll_next_unpin(cx) {
        completed = true;
    }
    if completed {
        std::task::Poll::Ready(())
    } else {
        std::task::Poll::Pending
    }
}

/// バッファ経路の要求タスク本体。
pub(super) async fn buffered_request_task(
    mut task: H3BufferedTask,
    headers: Vec<h3::Header>,
    body: Bytes,
) {
    if let Err(e) = task.handle_request(&headers, &body).await {
        // 送信失敗（クライアントがストリームをリセット）など。応答を送れていなければ
        // メインループが送信端の close を見て 502 を返す。
        debug!(
            "[HTTP/3] buffered request on stream {} ended: {}",
            task.stream_id, e
        );
    }
    task.finish();
}

/// 借用ヘッダ列を 1 回の確保に詰めて `RespHeaders` にする（ヘッダごとの確保をしない）。
///
/// `:status` は除き、`content_length` が `Some` なら `content-length` を末尾に足す。
/// 要求本文をストリーミングで受けるハンドラの起動パラメータ（F-209 #5）。
#[cfg(feature = "handler")]
pub(super) struct H3HandlerStream {
    pub(super) entry: std::sync::Arc<crate::api::handler::HandlerEntry>,
    // F-270: ルートの応答の圧縮
    pub(super) comp: Option<std::sync::Arc<crate::config::CompressionConfig>>,
    pub(super) req: crate::api::request::Request,
    pub(super) max_request_body: u64,
    /// `[route.traffic.cors]` の実際の応答に足すヘッダ（F-273。メインループが応答の head に足す）
    pub(super) cors: Option<std::sync::Arc<crate::route_cors::CorsActual>>,
    pub(super) log: H3AccessLog,
}

/// アクセスログに要る要求の情報（タスクがメインループの借用を持たないよう所有で持つ）。
#[cfg(feature = "handler")]
pub(super) struct H3AccessLog {
    pub(super) method: Bytes,
    pub(super) authority: Bytes,
    pub(super) path: Bytes,
    pub(super) user_agent: Bytes,
    pub(super) client_ip: crate::http_utils::IpStr,
}

/// 要求本文をストリーミングで受けるハンドラのタスク（F-209 #5）。メインループが `req_rx` へ流す本文を
/// ハンドラの受信ストリームへ中継しながらハンドラを呼び、応答を `RespMsg` でメインループへ返す
/// （HTTP/2 の `h2_api_handler` と同じ形。両方向が同時に進むので gRPC の双方向が全二重になる）。
#[cfg(feature = "handler")]
pub(super) async fn h3_api_stream_task(
    h: Box<H3HandlerStream>,
    req_rx: crate::http3_stream::Receiver<Bytes>,
    resp_tx: crate::http3_stream::Sender<crate::http3_stream::RespMsg>,
    notify: crate::http3_stream::ConnWaker,
) {
    let start = Instant::now();
    let H3HandlerStream {
        entry,
        comp,
        mut req,
        log,
        ..
    } = *h;
    let (mut body_tx, slot) = crate::api::stream::request_body_channel(REQ_CHAN_CAP);
    req.body_stream = Some(slot);
    let req_size = std::cell::Cell::new(0u64);
    let req_size_ref = &req_size;
    let rx = &req_rx;
    let notify_ref = &notify;
    // `body_tx` は move で持たせる（中継が終わったら drop して受信ストリームを終える）。
    // ハンドラとこの中継はどちらもチャネルの待ちだけで、起こされていない側を poll しても syscall は出ない。
    let pump = async move {
        use futures::SinkExt;
        while let Some(chunk) = rx.recv().await {
            req_size_ref.set(req_size_ref.get() + chunk.len() as u64);
            // 消費したらメインループを起こして残りの本文を受け取らせる（QUIC のフロー制御の補充）
            notify_ref.notify();
            if body_tx.send(chunk).await.is_err() {
                // ハンドラが受信ストリームを捨てた: 残りは読み捨てる（クライアントを止めない）
                while let Some(c) = rx.recv().await {
                    req_size_ref.set(req_size_ref.get() + c.len() as u64);
                    notify_ref.notify();
                }
                break;
            }
        }
    };
    let work = h3_api_respond(&entry, comp.as_deref(), req, &resp_tx, &notify);
    let (status, sent) =
        match futures::future::select(std::pin::pin!(work), std::pin::pin!(pump)).await {
            futures::future::Either::Left((r, _pump)) => r,
            futures::future::Either::Right(((), work)) => work.await,
        };
    drop(resp_tx);
    notify.notify();
    log_access(
        &log.method,
        &log.authority,
        &log.path,
        &log.user_agent,
        req_size.get(),
        status,
        sent,
        start,
        log.client_ip.as_str(),
        "",
    );
}

/// ハンドラを呼んで応答を `RespMsg` で送る。戻り値 `(status, 送った本文のバイト数)`。
#[cfg(feature = "handler")]
pub(super) async fn h3_api_respond(
    entry: &crate::api::handler::HandlerEntry,
    comp: Option<&crate::config::CompressionConfig>,
    req: crate::api::request::Request,
    resp_tx: &crate::http3_stream::Sender<crate::http3_stream::RespMsg>,
    notify: &crate::http3_stream::ConnWaker,
) -> (u16, u64) {
    use crate::api::stream::BodyChunk;
    use crate::http3_stream::RespMsg;
    use futures::StreamExt;
    let send = |msg: RespMsg| async move {
        let r = resp_tx.send(msg).await;
        notify.notify();
        r.is_ok()
    };
    let mut resp = crate::api::dispatch::call(entry, req, comp).await;
    crate::api::dispatch::h2_h3_headers(&mut resp);
    let status = resp.status_code();
    let stream = resp.take_stream();
    let headers: Vec<(&[u8], &[u8])> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_ref(), v.as_ref()))
        .collect();
    let Some(mut stream) = stream else {
        let body = resp.body().clone();
        let len = body.len() as u64;
        let head = RespMsg::Head {
            status,
            headers: pack_resp_headers(&headers, Some(body.len())),
        };
        if send(head).await && !body.is_empty() {
            send(RespMsg::Body(body)).await;
        }
        return (status, len);
    };
    if !send(RespMsg::Head {
        status,
        headers: pack_resp_headers(&headers, None),
    })
    .await
    {
        return (status, 0);
    }
    let mut sent = 0u64;
    let mut carry: Option<BodyChunk> = None;
    loop {
        let chunk = match carry.take() {
            Some(c) => c,
            None => match stream.next().await {
                Some(c) => c,
                None => break,
            },
        };
        let msg = match chunk {
            BodyChunk::Data(b) if b.is_empty() => continue,
            BodyChunk::Data(b) => {
                let b = crate::api::stream::coalesce(
                    b,
                    &mut stream,
                    crate::api::stream::COALESCE_MAX,
                    &mut carry,
                );
                sent += b.len() as u64;
                RespMsg::Body(b)
            }
            BodyChunk::Trailers(mut t) => {
                crate::api::dispatch::lowercase_names(&mut t);
                send(RespMsg::Trailers(t)).await;
                return (status, sent);
            }
            BodyChunk::Abort(reason) => {
                ftlog::debug!("[api] streaming response aborted: {}", reason);
                // head 送出後のエラーはストリームのリセット
                send(RespMsg::Error { status: 500 }).await;
                return (status, sent);
            }
        };
        if !send(msg).await {
            // クライアントがストリームを閉じた: ストリームを drop してハンドラ側へ伝える
            return (status, sent);
        }
    }
    (status, sent)
}

pub(super) fn pack_resp_headers(
    headers: &[(&[u8], &[u8])],
    content_length: Option<usize>,
) -> crate::http3_stream::RespHeaders {
    let mut len_buf = itoa::Buffer::new();
    let cl: Option<&str> = content_length.map(|n| len_buf.format(n));
    let keep = |name: &[u8]| name != b":status";
    let mut total = cl.map_or(0, |v| b"content-length".len() + v.len());
    let mut count = usize::from(cl.is_some());
    for (n, v) in headers.iter().filter(|(n, _)| keep(n)) {
        total += n.len() + v.len();
        count += 1;
    }
    let mut buf = BytesMut::with_capacity(total);
    for (n, v) in headers.iter().filter(|(n, _)| keep(n)) {
        buf.extend_from_slice(n);
        buf.extend_from_slice(v);
    }
    if let Some(v) = cl {
        buf.extend_from_slice(b"content-length");
        buf.extend_from_slice(v.as_bytes());
    }
    let buf = buf.freeze();
    let mut out = Vec::with_capacity(count);
    let mut off = 0;
    for (n, v) in headers.iter().filter(|(n, _)| keep(n)) {
        let name = buf.slice(off..off + n.len());
        off += n.len();
        let value = buf.slice(off..off + v.len());
        off += v.len();
        out.push((name, value));
    }
    if let Some(v) = cl {
        let name = buf.slice(off..off + b"content-length".len());
        off += b"content-length".len();
        out.push((name, buf.slice(off..off + v.len())));
    }
    out
}

/// F-171: HTTP/3 の要求ヘッダ（疑似ヘッダを除く）を 1 回の確保に詰めて所有ペアにする
/// （上流 HTTP/2 への転送用。ホップバイホップは上流クライアントが落とす）。
#[cfg(feature = "http2")]
pub(super) fn pack_h3_request_headers(
    headers: &[h3::Header],
    security: &SecurityConfig,
    added: &[(Vec<u8>, Vec<u8>)],
) -> crate::http3_stream::RespHeaders {
    // F-236（F4）: [route.security] の要求ヘッダの方針（削除・追加）を HTTP/2 の上流にも当てる
    let refs: Vec<(&[u8], &[u8])> = headers
        .iter()
        .filter(|h| !h.name().starts_with(b":"))
        .filter(|h| crate::proxy::forward_allowed(security, h.name()))
        .map(|h| (h.name(), h.value()))
        .chain(added.iter().map(|(n, v)| (n.as_slice(), v.as_slice())))
        .collect();
    pack_resp_headers(&refs, None)
}

impl H3BufferedTask {
    /// 応答断片を 1 つ送ってメインループを起こす。
    async fn send_msg(&self, mut msg: crate::http3_stream::RespMsg) -> io::Result<()> {
        let tx = self
            .resp_tx
            .as_ref()
            .ok_or_else(|| io::Error::other("HTTP/3 response already sent"))?;
        // `[route.traffic.cors]` の実際の応答のヘッダ（F-273。CORS の無いルートは分岐だけ）
        if let (Some(c), crate::http3_stream::RespMsg::Head { headers, .. }) =
            (&self.cors, &mut msg)
        {
            c.apply_bytes(headers);
        }
        let r = tx.send(msg).await;
        self.notify.notify();
        r.map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "HTTP/3 stream closed"))
    }

    /// 送信端を閉じる（メインループは fin を送る。応答未送出なら 502）。
    fn finish(&mut self) {
        if self.resp_tx.take().is_some() {
            self.notify.notify();
        }
    }

    /// 応答（head + 本文）を送り、送信端を閉じる。
    async fn send_response(
        &mut self,
        status: u16,
        headers: &[(&[u8], &[u8])],
        body: Option<Bytes>,
    ) -> io::Result<()> {
        use crate::http3_stream::RespMsg;
        // B-46: 既に content-length がある（プロキシ応答）なら重複させない。
        let has_cl = headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(b"content-length"));
        let cl = if has_cl {
            None
        } else {
            body.as_ref().map(|b| b.len())
        };
        self.send_msg(RespMsg::Head {
            status,
            headers: pack_resp_headers(headers, cl),
        })
        .await?;
        if let Some(b) = body {
            if !b.is_empty() {
                self.send_msg(RespMsg::Body(b)).await?;
            }
        }
        self.finish();
        Ok(())
    }

    /// ハンドラのストリーミング応答を送り、送信端を閉じる（F-197）。戻り値は本文のバイト数。
    #[cfg(feature = "handler")]
    async fn send_api_stream(
        &mut self,
        status: u16,
        headers: &[(&[u8], &[u8])],
        mut stream: crate::api::stream::BodyStream,
    ) -> io::Result<usize> {
        use crate::api::stream::BodyChunk;
        use crate::http3_stream::RespMsg;
        use futures::StreamExt;
        self.send_msg(RespMsg::Head {
            status,
            headers: pack_resp_headers(headers, None),
        })
        .await?;
        let mut sent = 0usize;
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
                    sent += b.len();
                    self.send_msg(RespMsg::Body(b)).await?;
                }
                BodyChunk::Trailers(mut t) => {
                    crate::api::dispatch::lowercase_names(&mut t);
                    self.send_msg(RespMsg::Trailers(t)).await?;
                    break;
                }
                BodyChunk::Abort(reason) => {
                    ftlog::debug!("[api] streaming response aborted: {}", reason);
                    // head 送出後のエラーはストリームのリセット
                    self.send_msg(RespMsg::Error { status: 500 }).await?;
                    break;
                }
            }
        }
        self.finish();
        Ok(sent)
    }

    /// エラーレスポンス送信
    async fn send_error_response(&mut self, status: u16, body: &'static [u8]) -> io::Result<()> {
        self.send_response(
            status,
            &[(b"content-type", b"text/plain"), (b"server", b"veil/http3")],
            Some(Bytes::from_static(body)),
        )
        .await
    }

    /// gRPC レスポンスを送信（初期 HEADERS → 本文 → trailers）。
    ///
    /// trailers はメインループが `send_additional_headers(is_trailer_section=true, fin=true)` で
    /// 送る（B-41: `send_response` は初期応答専用で、2 回目に使うとストリームが閉じない）。
    #[cfg(feature = "grpc")]
    async fn send_grpc_response(
        &mut self,
        headers: &[(&[u8], &[u8])],
        body: Option<Bytes>,
        grpc_status: u32,
        grpc_message: Option<&str>,
    ) -> io::Result<()> {
        use crate::grpc::status::{GrpcStatus, GrpcStatusCode};
        use crate::http3_stream::RespMsg;

        // 初期ヘッダ（content-type）。grpc-status/message は trailers 専用。
        let mut initial: Vec<(&[u8], &[u8])> = Vec::with_capacity(headers.len() + 1);
        initial.push((b"content-type", b"application/grpc"));
        initial.extend(filter_h3_grpc_initial_headers(headers));
        self.send_msg(RespMsg::Head {
            status: 200,
            headers: pack_resp_headers(&initial, None),
        })
        .await?;
        if let Some(b) = body {
            if !b.is_empty() {
                self.send_msg(RespMsg::Body(b)).await?;
            }
        }

        let code = GrpcStatusCode::from_u8(grpc_status as u8).unwrap_or(GrpcStatusCode::Unknown);
        let status = if let Some(msg) = grpc_message {
            GrpcStatus::error(code, msg)
        } else {
            GrpcStatus::from_code(code)
        };
        let trailer_pairs = status.to_trailers();
        let trailer_refs: Vec<(&[u8], &[u8])> = trailer_pairs
            .iter()
            .map(|(n, v)| (n.as_slice(), v.as_slice()))
            .collect();
        self.send_msg(RespMsg::Trailers(pack_resp_headers(&trailer_refs, None)))
            .await?;
        self.finish();
        Ok(())
    }

    /// HTTP/3 リクエストを処理（完全版）
    ///
    /// HTTP/1.1と同等のルーティング・セキュリティ・プロキシ機能をサポート。
    /// `handle_request_impl` を呼び出し、**成功・失敗（`?` による早期 return）を問わず**
    /// 最後に一度だけ `on_log`（`finish_h3_wasm_lifecycle`）を呼ぶ。
    ///
    /// F-132: `handle_request_impl` 内には `self.send_response(...).await?` 等、`?` で早期 return
    /// する箇所が多数あり、そこに個別に `on_log` 呼び出しを仕込むと取りこぼしうる
    /// （WASM コンテキストのリークに直結する）。このラッパで囲むことで、
    /// 離脱点の数に関係なく **1 リクエストにつきちょうど 1 回** だけ呼ばれることを構造的に
    /// 保証する（`handle_request_impl` 側は `finish_h3_wasm_lifecycle` を呼ばない）。
    async fn handle_request(
        &mut self,
        headers: &[h3::Header],
        request_body: &Bytes,
    ) -> io::Result<()> {
        #[cfg(feature = "wasm")]
        let mut wasm_modules_to_apply: Option<
            Arc<Vec<crate::wasm_plugin_config::ModuleRef>>,
        > = None;

        let result = self
            .handle_request_impl(
                headers,
                request_body,
                #[cfg(feature = "wasm")]
                &mut wasm_modules_to_apply,
            )
            .await;

        #[cfg(feature = "wasm")]
        finish_h3_wasm_lifecycle(&wasm_modules_to_apply).await;

        result
    }

    async fn handle_request_impl(
        &mut self,
        headers: &[h3::Header],
        request_body: &Bytes,
        #[cfg(feature = "wasm")] wasm_modules_to_apply: &mut Option<
            Arc<Vec<crate::wasm_plugin_config::ModuleRef>>,
        >,
    ) -> io::Result<()> {
        // F-101: バッファ経路でもヘッダサイズ上限を enforce（classify を経由しないケース）
        let header_block = h3_request_header_block_size(headers);
        if header_block > MAX_HEADER_SIZE {
            let path_len = headers
                .iter()
                .find(|h| h.name() == b":path")
                .map(|h| h.value().len())
                .unwrap_or(0);
            if path_len > MAX_HEADER_SIZE {
                self.send_error_response(414, b"URI Too Long").await?;
            } else {
                self.send_error_response(431, b"Request Header Fields Too Large")
                    .await?;
            }
            return Ok(());
        }

        // ヘッダーを解析（`headers` を借用するだけで、リクエストごとに `Vec` へコピーしない）
        let mut method: Option<&[u8]> = None;
        let mut path: Option<&[u8]> = None;
        let mut authority: Option<&[u8]> = None;
        let mut content_length: usize = 0;
        let mut accept_encoding: Option<&[u8]> = None;
        let mut user_agent: &[u8] = &[];

        for header in headers {
            match header.name() {
                b":method" => method = Some(header.value()),
                b":path" => path = Some(header.value()),
                b":authority" => authority = Some(header.value()),
                b"content-length" => {
                    if let Ok(s) = std::str::from_utf8(header.value()) {
                        content_length = s.parse().unwrap_or(0);
                    }
                }
                name if name.eq_ignore_ascii_case(b"accept-encoding") => {
                    accept_encoding = Some(header.value());
                }
                name if name.eq_ignore_ascii_case(b"user-agent") => {
                    user_agent = header.value();
                }
                _ => {}
            }
        }

        // クライアントの Accept-Encoding を解析
        let client_encoding = accept_encoding
            .map(AcceptedEncoding::parse)
            .unwrap_or(AcceptedEncoding::Identity);

        let method: &[u8] = method.unwrap_or(b"GET");
        let path: &[u8] = path.unwrap_or(b"/");
        let authority: &[u8] = authority.unwrap_or_default();

        // 処理開始時刻（アクセスログ・メトリクスが無効なら時刻を読まない）
        let start_time = crate::logging::request_start_instant();

        debug!(
            "[HTTP/3] Request: {} {} (stream {})",
            String::from_utf8_lossy(method),
            String::from_utf8_lossy(path),
            self.stream_id
        );

        // F-97: :authority と Host が矛盾するリクエストを 400 で拒否
        let host_hdr = headers.iter().find_map(|h| {
            if h.name().eq_ignore_ascii_case(b"host") {
                Some(h.value())
            } else {
                None
            }
        });
        if crate::http_utils::authority_host_mismatch(authority, host_hdr) {
            self.send_error_response(400, b"Bad Request: :authority/Host mismatch")
                .await?;
            let user_agent_slice: &[u8] = if user_agent.is_empty() {
                &[]
            } else {
                user_agent
            };
            log_access(
                method,
                authority,
                path,
                user_agent_slice,
                content_length as u64,
                400,
                0,
                start_time,
                self.client_ip.as_str(),
                "",
            );
            return Ok(());
        }

        // gRPC リクエスト検出フラグ
        #[cfg(feature = "grpc")]
        let is_grpc = Http3Handler::is_grpc_request(headers);
        #[cfg(not(feature = "grpc"))]
        let _is_grpc = false;

        #[cfg(feature = "grpc")]
        if is_grpc {
            debug!(
                "[HTTP/3] gRPC request detected: {}",
                String::from_utf8_lossy(path)
            );
        }

        // メトリクスエンドポイント（設定可能なパス）
        {
            let config = CURRENT_CONFIG.load();
            let prom_config = &config.prometheus_config;

            let path_str = std::str::from_utf8(path).unwrap_or("/");
            if prom_config.enabled && path_str == prom_config.path && method == b"GET" {
                // IPアドレス制限チェック
                if !prom_config.is_ip_allowed(self.client_ip.as_str()) {
                    self.send_error_response(403, b"Forbidden").await?;
                    let user_agent_slice: &[u8] = if user_agent.is_empty() {
                        &[]
                    } else {
                        user_agent
                    };
                    log_access(
                        method,
                        authority,
                        path,
                        user_agent_slice,
                        request_body.len() as u64,
                        403,
                        9,
                        start_time,
                        self.client_ip.as_str(),
                        "",
                    );
                    return Ok(());
                }

                let body = encode_prometheus_metrics();
                self.send_response(
                    200,
                    &[
                        (b":status", b"200"),
                        (b"content-type", b"text/plain; version=0.0.4; charset=utf-8"),
                        (b"server", b"veil/http3"),
                    ],
                    Some(Bytes::copy_from_slice(&body)),
                )
                .await?;

                let user_agent_slice: &[u8] = if user_agent.is_empty() {
                    &[]
                } else {
                    user_agent
                };
                log_access(
                    method,
                    authority,
                    path,
                    user_agent_slice,
                    request_body.len() as u64,
                    200,
                    body.len() as u64,
                    start_time,
                    self.client_ip.as_str(),
                    "",
                );
                return Ok(());
            }
        }

        // バックエンド選択（統合ルーティング）
        let config = CURRENT_CONFIG.load();

        // ヘッダーをゼロコピーのバイト列スライスとして参照（HashMap 不要）
        let headers_raw: Vec<(&[u8], &[u8])> = headers
            .iter()
            .filter(|h| !h.name().starts_with(b":")) // 疑似ヘッダーを除外
            .map(|h| (h.name(), h.value()))
            .collect();

        // パス/クエリ分離（スキャンを1回に統一）
        let query_start_pos = path.iter().position(|&b| b == b'?');
        let raw_query: &[u8] = query_start_pos.map(|i| &path[i + 1..]).unwrap_or(b"");
        let path_without_query = query_start_pos.map(|i| &path[..i]).unwrap_or(path);

        let backend_result = crate::l7::resolve_route(
            authority,
            path_without_query,
            method,
            &headers_raw,
            raw_query,
            &self.peer_addr,
            true,
        );

        // F-169: File/MemoryFile バックエンドの圧縮ネゴシエーションに使う
        // （従来は破棄していたため、HTTP/3 の静的配信では圧縮が一切効いていなかった）。
        let (prefix, backend, route_compression) = match backend_result {
            Some(b) => b,
            None => {
                debug!(
                    "[HTTP/3] No backend found for authority='{}', path='{}'",
                    String::from_utf8_lossy(authority),
                    String::from_utf8_lossy(path)
                );

                // gRPC リクエストの場合は gRPC エラーレスポンスを返す
                #[cfg(feature = "grpc")]
                if is_grpc {
                    // UNIMPLEMENTED (12) - サービス/メソッドが見つからない
                    self.send_grpc_response(&[], None, 12, Some("Service not found"))
                        .await?;
                    let user_agent_slice: &[u8] = if user_agent.is_empty() {
                        &[]
                    } else {
                        user_agent
                    };
                    log_access(
                        method,
                        authority,
                        path,
                        user_agent_slice,
                        request_body.len() as u64,
                        200,
                        0,
                        start_time,
                        self.client_ip.as_str(),
                        "",
                    );
                    return Ok(());
                }

                // 未登録のサービスへの gRPC は Trailers-Only の UNIMPLEMENTED（F-209）
                #[cfg(feature = "grpc-handler")]
                if let Some(h) = crate::api::grpc::unknown_route_response(
                    headers.iter().map(|h| (h.name(), h.value())),
                    path,
                ) {
                    let pairs: Vec<(&[u8], &[u8])> =
                        h.iter().map(|(n, v)| (n.as_ref(), v.as_ref())).collect();
                    self.send_response(200, &pairs, None).await?;
                    log_access(
                        method,
                        authority,
                        path,
                        user_agent,
                        request_body.len() as u64,
                        200,
                        0,
                        start_time,
                        self.client_ip.as_str(),
                        "",
                    );
                    return Ok(());
                }
                self.send_error_response(404, b"Not Found").await?;
                let user_agent_slice: &[u8] = if user_agent.is_empty() {
                    &[]
                } else {
                    user_agent
                };
                log_access(
                    method,
                    authority,
                    path,
                    user_agent_slice,
                    request_body.len() as u64,
                    404,
                    9,
                    start_time,
                    self.client_ip.as_str(),
                    "",
                );
                return Ok(());
            }
        };

        // セキュリティチェック。本文は受信済みなので実際の長さでも上限を見る（HTTP/3 は content-length を
        // 省略できる。省略した要求が本文の上限を素通りしていた。B-132 の行列の試験で発見）
        let security = backend.security();
        let check_result = check_security(
            security,
            self.client_ip.as_str(),
            method,
            content_length.max(request_body.len()),
            false,
        );

        if check_result != SecurityCheckResult::Allowed {
            let status = check_result.status_code();
            let msg = check_result.message();
            self.send_error_response(status, msg).await?;
            let user_agent_slice: &[u8] = if user_agent.is_empty() {
                &[]
            } else {
                user_agent
            };
            log_access(
                method,
                authority,
                path,
                user_agent_slice,
                request_body.len() as u64,
                status,
                msg.len() as u64,
                start_time,
                self.client_ip.as_str(),
                "",
            );
            return Ok(());
        }
        // `[route.traffic.cors]` の実際の応答のヘッダ（F-273。以降の応答の head に `send_msg` が足す）
        self.cors = security.cors_actual.clone();

        // WASM モジュール適用（B-38: リクエストヘッダ変更 + レスポンスヘッダ変更）
        // F-132: `wasm_modules_to_apply` は呼び出し元（`handle_request`）が保持する out
        // パラメータ。ここで `Some` にセットしておけば、この後どの `?` で早期 return しても
        // 呼び出し元側で必ず一度だけ `on_log` が呼ばれる。
        #[cfg(feature = "wasm")]
        let mut wasm_request_headers: Option<Vec<(Vec<u8>, Vec<u8>)>> = None;
        // F-132: WASM on_request_body で書き換えられた本文（適用時のみ Some）。
        #[cfg(feature = "wasm")]
        let mut wasm_request_body_override: Option<Bytes> = None;
        #[cfg(feature = "wasm")]
        {
            let config = CURRENT_CONFIG.load();
            if let Some(ref wasm_engine) = config.wasm_filter_engine {
                let path_str = std::str::from_utf8(path).unwrap_or("/");
                let method_str = std::str::from_utf8(method).unwrap_or("GET");

                // F-43: モジュールリストは Arc 共有（リクエストごとの deep copy 排除）
                let modules_to_apply = if let Some(backend_modules) = backend.modules_arc() {
                    backend_modules.clone()
                } else {
                    crate::wasm::empty_wasm_modules()
                };

                if !modules_to_apply.is_empty() {
                    *wasm_modules_to_apply = Some(modules_to_apply.clone());

                    let headers_vec: Vec<(Vec<u8>, Vec<u8>)> = headers
                        .iter()
                        .filter(|h| !h.name().starts_with(b":"))
                        .map(|h| (h.name().to_vec(), h.value().to_vec()))
                        .collect();

                    let wasm_result = wasm_engine
                        .on_request_headers_with_modules(
                            &modules_to_apply,
                            &std::sync::Arc::from(path_str),
                            &std::sync::Arc::from(method_str),
                            headers_vec,
                            &std::sync::Arc::from(self.client_ip.as_str()),
                            request_body.is_empty(),
                        )
                        .await;

                    match wasm_result {
                        crate::wasm::FilterResult::LocalResponse(resp) => {
                            self.send_response(
                                resp.status_code,
                                &resp
                                    .headers
                                    .iter()
                                    .map(|(k, v)| (k.as_slice(), v.as_slice()))
                                    .collect::<Vec<_>>(),
                                Some(Bytes::copy_from_slice(&resp.body)),
                            )
                            .await?;
                            let user_agent_slice: &[u8] = if user_agent.is_empty() {
                                &[]
                            } else {
                                user_agent
                            };
                            log_access(
                                method,
                                authority,
                                path,
                                user_agent_slice,
                                request_body.len() as u64,
                                resp.status_code,
                                resp.body.len() as u64,
                                start_time,
                                self.client_ip.as_str(),
                                "",
                            );
                            // F-132: on_log は呼び出し元の `handle_request` ラッパが
                            // `?`/早期 return を問わず最後に一度だけ呼ぶ
                            // （`*wasm_modules_to_apply` は既にセット済み）。
                            return Ok(());
                        }
                        crate::wasm::FilterResult::Pause => {
                            warn!("WASM module requested pause, but async operations are not yet supported");
                        }
                        crate::wasm::FilterResult::Continue {
                            headers: modified, ..
                        } => {
                            // B-38: 変更後ヘッダを上流リクエストへ反映
                            wasm_request_headers = Some(modified);

                            // F-132: リクエストボディフィルタ（HTTP/3 は WASM 適用時に
                            // 必ず Decision::Buffer に落ちるためボディ全体がメモリ上にある。
                            // end_of_stream=true の 1 回呼びで実装できる）。
                            if !request_body.is_empty() {
                                match crate::wasm::http_executor::apply_wasm_request_body(
                                    wasm_engine,
                                    &modules_to_apply,
                                    request_body.clone(),
                                    true,
                                )
                                .await
                                {
                                    crate::wasm::http_executor::WasmBodyOutcome::Continue(b) => {
                                        wasm_request_body_override = Some(b);
                                    }
                                    crate::wasm::http_executor::WasmBodyOutcome::LocalResponse(
                                        resp,
                                    ) => {
                                        self.send_response(
                                            resp.status_code,
                                            &resp
                                                .headers
                                                .iter()
                                                .map(|(k, v)| (k.as_slice(), v.as_slice()))
                                                .collect::<Vec<_>>(),
                                            Some(Bytes::copy_from_slice(&resp.body)),
                                        )
                                        .await?;
                                        let user_agent_slice: &[u8] = if user_agent.is_empty() {
                                            &[]
                                        } else {
                                            user_agent
                                        };
                                        log_access(
                                            method,
                                            authority,
                                            path,
                                            user_agent_slice,
                                            request_body.len() as u64,
                                            resp.status_code,
                                            resp.body.len() as u64,
                                            start_time,
                                            self.client_ip.as_str(),
                                            "",
                                        );
                                        // F-132: on_log は呼び出し元の `handle_request`
                                        // ラッパが最後に一度だけ呼ぶ。
                                        return Ok(());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // バックエンド処理
        let (status, resp_size) = match backend {
            Backend::Proxy(upstream_group, security, path_compression, _buffering, _cache, _) => {
                debug!("[HTTP/3] Starting proxy request to upstream group");

                // HTTP/3専用圧縮設定を解決
                // 優先順位: パス設定 > HTTP/3設定 > デフォルト
                let config = CURRENT_CONFIG.load();
                let effective_compression =
                    resolve_http3_compression_config(&path_compression, &config.http3_config);

                // F-132: WASM on_request_body で書き換えられた本文があればそれを使う。
                #[cfg(feature = "wasm")]
                let effective_request_body: &Bytes =
                    wasm_request_body_override.as_ref().unwrap_or(request_body);
                #[cfg(not(feature = "wasm"))]
                let effective_request_body: &Bytes = request_body;

                let result = self
                    .handle_proxy(
                        &upstream_group,
                        &security,
                        &effective_compression,
                        client_encoding,
                        method,
                        path,
                        &prefix,
                        headers,
                        effective_request_body,
                        #[cfg(feature = "wasm")]
                        wasm_modules_to_apply.as_ref(),
                        #[cfg(feature = "wasm")]
                        wasm_request_headers.as_deref(),
                    )
                    .await
                    .unwrap_or((502, 11));
                debug!(
                    "[HTTP/3] Proxy request completed: status={}, size={}",
                    result.0, result.1
                );
                result
            }
            Backend::MemoryFile(data, mime_type, security, _) => {
                // パス完全一致チェック
                let path_str = std::str::from_utf8(path).unwrap_or("/");
                let prefix_str = std::str::from_utf8(&prefix).unwrap_or("");

                let remainder = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
                    &path_str[prefix_str.len()..]
                } else {
                    ""
                };

                let clean_remainder = remainder.trim_matches('/');
                if !clean_remainder.is_empty() {
                    self.send_error_response(404, b"Not Found").await?;
                    (404, 9)
                } else {
                    // F-132: h1/h2 と同様、静的配信（Backend::File 系）にも WASM
                    // on_response_headers を適用する。
                    let mut header_store: Vec<(Vec<u8>, Vec<u8>)> = vec![
                        (b"content-type".to_vec(), mime_type.as_bytes().to_vec()),
                        (b"server".to_vec(), b"veil/http3".to_vec()),
                    ];
                    for (k, v) in &security.add_response_headers {
                        header_store.push((k.as_bytes().to_vec(), v.as_bytes().to_vec()));
                    }

                    // F-169: HTTP/3 の File 系バックエンドは従来ここで圧縮設定
                    // （`route_compression`）を破棄しており、h2 と違って圧縮が
                    // 一切効いていなかった。h2 (`build_h2_compressed_file_response`)
                    // と同じネゴシエーションを適用する。MemoryFile はファイルシステム
                    // パスを持たないため圧縮結果キャッシュ（`cache::compressed`）は
                    // 使えない（毎回圧縮する。h2 側の MemoryFile 経路と同じ扱い）。
                    let file_compression =
                        resolve_http3_compression_config(&route_compression, &config.http3_config);
                    let should_compress = file_compression.should_compress(
                        client_encoding,
                        Some(mime_type.as_bytes()),
                        Some(data.len()),
                        None,
                    );

                    let response_body: Bytes = if let Some(enc) = should_compress {
                        let encoding_name: &[u8] = match enc {
                            AcceptedEncoding::Zstd => b"zstd",
                            AcceptedEncoding::Brotli => b"br",
                            AcceptedEncoding::Gzip => b"gzip",
                            AcceptedEncoding::Deflate => b"deflate",
                            AcceptedEncoding::Identity => b"",
                        };
                        if !encoding_name.is_empty() {
                            header_store
                                .push((b"content-encoding".to_vec(), encoding_name.to_vec()));
                            header_store.push((b"vary".to_vec(), b"Accept-Encoding".to_vec()));
                        }
                        Bytes::from(compress_body_h3(data.as_slice(), enc, &file_compression))
                    } else {
                        // Arc<Vec<u8>> の参照カウントクローンのみ（ディープコピー無し）。
                        Bytes::from_owner(ArcVecBytes(data.clone()))
                    };

                    #[cfg(feature = "wasm")]
                    if let Some(modules) = wasm_modules_to_apply.as_ref() {
                        header_store =
                            apply_h3_wasm_response_headers(modules, 200, header_store).await;
                    }

                    let resp_headers: Vec<(&[u8], &[u8])> = header_store
                        .iter()
                        .map(|(k, v)| (k.as_slice(), v.as_slice()))
                        .collect();

                    self.send_response(200, &resp_headers, Some(response_body.clone()))
                        .await?;
                    (200, response_body.len())
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
                // F-169: HTTP/3 専用圧縮設定を解決（Proxy 経路の `handle_proxy` と同じ
                // 優先順位: パス設定 > HTTP/3 設定 > デフォルト）。従来 SendFile は
                // `route_compression` を破棄しており圧縮が一切効いていなかった。
                let file_compression =
                    resolve_http3_compression_config(&route_compression, &config.http3_config);
                self.handle_sendfile(SendFileRequest {
                    base_path: &base_path,
                    is_dir,
                    index_file: index_file.as_deref(),
                    req_path: path,
                    prefix: &prefix,
                    security: &security,
                    compression: &file_compression,
                    client_encoding,
                    open_file_cache_config: open_file_cache_config.as_deref(),
                    canonical_base: canonical_base.as_deref(),
                    static_file_cache_config: static_file_cache_config.as_deref(),
                    #[cfg(feature = "wasm")]
                    wasm_modules: wasm_modules_to_apply.as_ref(),
                })
                .await
                .unwrap_or((404, 9))
            }
            Backend::Respond(r) => {
                // 固定応答（F-219）。ヘッダはスタック上の配列へ参照を並べる
                let mut hs: [(&[u8], &[u8]); 12] = [(&[], &[]); 12];
                let n = r.headers.len().min(hs.len() - 1);
                for (slot, (k, v)) in hs.iter_mut().zip(r.headers.iter().take(n)) {
                    *slot = (k.as_ref(), v.as_ref());
                }
                hs[n] = (b"server", b"veil/http3");
                match self
                    .send_response(r.status, &hs[..n + 1], Some(r.body.clone()))
                    .await
                {
                    Ok(()) => (r.status, r.body.len()),
                    Err(_) => (500, 0),
                }
            }
            Backend::Redirect(redirect_url, status_code, preserve_path, _, _) => self
                .handle_redirect(&redirect_url, status_code, preserve_path, path, &prefix)
                .await
                .unwrap_or((500, 0)),
            #[cfg(feature = "handler")]
            Backend::Handler(entry, _, _, comp) => {
                // ネイティブハンドラ（API ライブラリ、F-188）
                let mut hdrs: Vec<(Bytes, Bytes)> = Vec::with_capacity(headers.len() + 1);
                let mut has_host = false;
                for h in headers.iter().filter(|h| !h.name().starts_with(b":")) {
                    has_host |= h.name().eq_ignore_ascii_case(b"host");
                    hdrs.push((
                        Bytes::copy_from_slice(h.name()),
                        Bytes::copy_from_slice(h.value()),
                    ));
                }
                if !has_host && !authority.is_empty() {
                    hdrs.push((
                        Bytes::from_static(b"host"),
                        Bytes::copy_from_slice(authority),
                    ));
                }
                let req = crate::api::dispatch::build_request(
                    Bytes::copy_from_slice(method),
                    Bytes::copy_from_slice(path),
                    hdrs,
                    request_body.clone(),
                    self.client_ip.as_str(),
                    crate::api::request::HttpVersion::Http3,
                );
                let mut resp = crate::api::dispatch::call(&entry, req, comp.as_deref()).await;
                crate::api::dispatch::h2_h3_headers(&mut resp);
                let status = resp.status_code();
                if let Some(stream) = resp.take_stream() {
                    // ストリーミング応答（F-197）: head → 断片 → トレーラ（または fin）
                    let resp_headers: Vec<(&[u8], &[u8])> = resp
                        .headers()
                        .iter()
                        .map(|(k, v)| (k.as_ref(), v.as_ref()))
                        .collect();
                    let sent = self.send_api_stream(status, &resp_headers, stream).await?;
                    (status, sent)
                } else {
                    let resp_headers: Vec<(&[u8], &[u8])> = resp
                        .headers()
                        .iter()
                        .map(|(k, v)| (k.as_ref(), v.as_ref()))
                        .collect();
                    let body = resp.body().clone();
                    let len = body.len();
                    self.send_response(status, &resp_headers, Some(body))
                        .await?;
                    (status, len)
                }
            }
        };

        let user_agent_slice: &[u8] = if user_agent.is_empty() {
            &[]
        } else {
            user_agent
        };
        log_access(
            method,
            authority,
            path,
            user_agent_slice,
            request_body.len() as u64,
            status,
            resp_size as u64,
            start_time,
            self.client_ip.as_str(),
            "",
        );
        // F-132: on_log は呼び出し元の `handle_request` ラッパが最後に一度だけ呼ぶ。
        Ok(())
    }

    /// プロキシ処理（HTTP/1.1 または H2C バックエンドへの変換）
    ///
    /// - 通常: HTTP/1.1 で上流へ転送
    /// - `use_h2c`: H2C (Prior Knowledge) で上流へ転送（B-39: gRPC over HTTP/3）
    /// - WASM: リクエスト/レスポンスヘッダ変更を適用（B-38）
    async fn handle_proxy(
        &mut self,
        upstream_group: &Arc<UpstreamGroup>,
        // 上流接続プール（B-74 の H2C_POOL・B-104 の HTTP/1.1 プール）の max_idle/idle_timeout。
        security: &SecurityConfig,
        compression: &CompressionConfig,
        client_encoding: AcceptedEncoding,
        method: &[u8],
        req_path: &[u8],
        prefix: &[u8],
        headers: &[h3::Header],
        request_body: &Bytes,
        #[cfg(feature = "wasm")] wasm_modules: Option<
            &std::sync::Arc<Vec<crate::wasm_plugin_config::ModuleRef>>,
        >,
        #[cfg(feature = "wasm")] wasm_request_headers: Option<&[(Vec<u8>, Vec<u8>)]>,
    ) -> io::Result<(u16, usize)> {
        // サーバー選択（F-97: Consistent Hash header/cookie キー対応）
        let server = match upstream_group.select_with_header_fn(self.client_ip.as_str(), |name| {
            headers
                .iter()
                .find(|h| h.name().eq_ignore_ascii_case(name))
                .map(|h| h.value())
        }) {
            Some(s) => s,
            None => {
                self.send_error_response(502, b"Bad Gateway").await?;
                return Ok((502, 11));
            }
        };

        server.acquire();
        let target = &server.target;

        // リクエストパス構築
        let path_str = std::str::from_utf8(req_path).unwrap_or("/");
        let timeout_secs = 30;

        // 上流へ送るヘッダソース（WASM 変更後 or 生 H3 ヘッダ）
        #[cfg(feature = "wasm")]
        let header_pairs: Vec<(Vec<u8>, Vec<u8>)> = if let Some(ov) = wasm_request_headers {
            ov.iter()
                .filter(|(n, _)| {
                    !n.starts_with(b":")
                        && !n.eq_ignore_ascii_case(b"connection")
                        && !n.eq_ignore_ascii_case(b"keep-alive")
                        && !n.eq_ignore_ascii_case(b"transfer-encoding")
                })
                .cloned()
                .collect()
        } else {
            headers
                .iter()
                .filter(|h| {
                    !h.name().starts_with(b":")
                        && !h.name().eq_ignore_ascii_case(b"connection")
                        && !h.name().eq_ignore_ascii_case(b"keep-alive")
                        && !h.name().eq_ignore_ascii_case(b"transfer-encoding")
                })
                .map(|h| (h.name().to_vec(), h.value().to_vec()))
                .collect()
        };
        #[cfg(not(feature = "wasm"))]
        let header_pairs: Vec<(Vec<u8>, Vec<u8>)> = headers
            .iter()
            .filter(|h| {
                !h.name().starts_with(b":")
                    && !h.name().eq_ignore_ascii_case(b"connection")
                    && !h.name().eq_ignore_ascii_case(b"keep-alive")
                    && !h.name().eq_ignore_ascii_case(b"transfer-encoding")
            })
            .map(|h| (h.name().to_vec(), h.value().to_vec()))
            .collect();

        // F-236（F4）: [route.security] の要求ヘッダの方針（削除・追加）を、上流の種類（HTTP/1.1・h2c・
        // HTTPS の HTTP/2）に関わらず当てる（以前は HTTP/1.1 で受けた要求にだけ当てていた）
        let mut header_pairs = header_pairs;
        if !security.remove_request_headers.is_empty() {
            header_pairs.retain(|(n, _)| crate::proxy::forward_allowed(security, n));
        }
        if !security.add_request_headers.is_empty() {
            let authority = headers
                .iter()
                .find(|h| h.name() == b":authority")
                .map(|h| h.value())
                .unwrap_or_default();
            header_pairs.extend(crate::proxy::expanded_added_headers(
                security,
                self.client_ip.as_str(),
                authority,
                path_str,
            ));
        }

        // gRPC はサービス/メソッドのフルパスを保持（B-39）
        #[cfg(feature = "grpc")]
        let is_grpc_req = header_pairs_indicate_grpc(&header_pairs);
        #[cfg(not(feature = "grpc"))]
        let is_grpc_req = false;

        let final_path_owned =
            compute_upstream_request_path(path_str, prefix, &target.path_prefix, is_grpc_req);
        let final_path = final_path_owned.as_str();

        // B-39: H2C 上流（gRPC 等）
        let use_h2c = target.use_h2c || upstream_group.use_h2c();
        let proxy_result = if use_h2c {
            #[cfg(feature = "http2")]
            {
                proxy_to_h2c_backend_async(
                    target,
                    method,
                    final_path.as_bytes(),
                    &header_pairs,
                    request_body,
                    timeout_secs,
                    security,
                )
                .await
            }
            #[cfg(not(feature = "http2"))]
            {
                warn!("[HTTP/3] use_h2c requested but http2 feature disabled");
                Err(io::Error::other("H2C requires http2 feature"))
            }
        } else {
            // F-175: HTTPS 上流は ALPN で HTTP/2 を試す。上流が HTTP/1.1 を選んだら、
            // ネゴシエーションに使った接続をワーカーのプールへ入れて下の HTTP/1.1 経路で使う。
            #[cfg(feature = "http2")]
            let h2_result: Option<io::Result<BackendProxyResult>> = if target.h2_over_tls() {
                let key = crate::http_utils::PoolKeyStr::tls_addr(
                    target.conn_addr().as_str(),
                    target.sni(),
                    upstream_group.tls_profile(),
                );
                let body = (!request_body.is_empty()).then(|| request_body.clone());
                match crate::http2::upstream_mux::request_https_buffered(
                    target,
                    key.as_str(),
                    upstream_group.tls_profile(),
                    Duration::from_secs(security.backend_connect_timeout_secs),
                    Duration::from_secs(security.idle_connection_timeout_secs),
                    Duration::from_secs(timeout_secs),
                    method,
                    final_path.as_bytes(),
                    target.host.as_bytes(),
                    header_pairs
                        .iter()
                        .map(|(k, v)| (k.as_slice(), v.as_slice())),
                    body,
                    crate::http_utils::is_idempotent_method(method),
                )
                .await
                {
                    Ok(crate::http2::upstream_mux::HttpsBuffered::H2(r)) => {
                        Some(Ok(BackendProxyResult {
                            status_code: r.status,
                            body: Vec::from(r.body),
                            headers: r
                                .headers
                                .into_iter()
                                .map(|(k, v)| (Vec::from(k), Vec::from(v)))
                                .collect(),
                            trailers: r
                                .trailers
                                .into_iter()
                                .map(|(k, v)| (Vec::from(k), Vec::from(v)))
                                .collect(),
                        }))
                    }
                    Ok(crate::http2::upstream_mux::HttpsBuffered::Http1(conn)) => {
                        if let Some(conn) = conn {
                            crate::http3_stream::h3_pool_put_client_tls(
                                key.as_str(),
                                *conn,
                                security.max_idle_connections_per_host,
                                security.idle_connection_timeout_secs,
                            );
                        }
                        None
                    }
                    Err(status) => Some(Err(if status == 504 {
                        io::Error::new(io::ErrorKind::TimedOut, "upstream h2 timeout")
                    } else {
                        io::Error::other("upstream h2 request failed")
                    })),
                }
            } else {
                None
            };
            #[cfg(not(feature = "http2"))]
            let h2_result: Option<io::Result<BackendProxyResult>> = None;
            match h2_result {
                Some(r) => r,
                None => {
                    // HTTP/1.1 リクエスト構築
                    let mut request = Vec::with_capacity(1024 + request_body.len());
                    request.extend_from_slice(method);
                    request.extend_from_slice(b" ");
                    request.extend_from_slice(final_path.as_bytes());
                    request.extend_from_slice(b" HTTP/1.1\r\nHost: ");
                    request.extend_from_slice(target.host.as_bytes());

                    if !target.is_default_port() {
                        request.extend_from_slice(b":");
                        let mut port_buf = itoa::Buffer::new();
                        request.extend_from_slice(port_buf.format(target.port).as_bytes());
                    }
                    request.extend_from_slice(b"\r\n");

                    // F-236（F2）: 名前・値の検査、content-length・host の除去（本文から付け直す）
                    crate::proxy::write_h1_forward_headers(
                        &mut request,
                        header_pairs
                            .iter()
                            .map(|(n, v)| (n.as_slice(), v.as_slice())),
                        security,
                        &[],
                    );

                    if !request_body.is_empty() {
                        request.extend_from_slice(b"Content-Length: ");
                        let mut len_buf = itoa::Buffer::new();
                        request.extend_from_slice(len_buf.format(request_body.len()).as_bytes());
                        request.extend_from_slice(b"\r\n");
                    }

                    // B-104: `Connection: close` は付けない（上流接続はワーカーのプールで再利用する）。
                    request.extend_from_slice(b"\r\n");
                    request.extend_from_slice(request_body);

                    let cfg = crate::http3_stream::BufferedExchange {
                        target,
                        timeout_secs,
                        tls_profile: upstream_group.tls_profile(),
                        pool_max_idle: security.max_idle_connections_per_host,
                        pool_idle_timeout_secs: security.idle_connection_timeout_secs,
                        no_response_body: method.eq_ignore_ascii_case(b"HEAD"),
                        idempotent: crate::http_utils::is_idempotent_method(method),
                    };
                    crate::http3_stream::exchange_buffered(&cfg, Bytes::from(request))
                        .await
                        .map(|r| BackendProxyResult {
                            status_code: r.status,
                            body: r.body,
                            headers: r.headers,
                            trailers: Vec::new(),
                        })
                }
            }
        };

        server.release();

        match proxy_result {
            Ok(backend_result) => {
                let status_code = backend_result.status_code;
                #[cfg(feature = "wasm")]
                let mut body = backend_result.body;
                #[cfg(not(feature = "wasm"))]
                let body = backend_result.body;
                let trailers = backend_result.trailers;

                // B-38: WASM レスポンスヘッダフィルタ
                #[cfg(feature = "wasm")]
                let mut resp_header_store = backend_result.headers;
                #[cfg(feature = "wasm")]
                if let Some(modules) = wasm_modules {
                    if !modules.is_empty() {
                        resp_header_store =
                            apply_h3_wasm_response_headers(modules, status_code, resp_header_store)
                                .await;
                    }
                }
                #[cfg(not(feature = "wasm"))]
                let resp_header_store = backend_result.headers;

                // gRPC は圧縮ネゴシエーション/ボディフィルタ対象外（application/grpc、trailers は F-133）
                #[cfg(feature = "grpc")]
                let is_grpc_ct = resp_header_store
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(b"content-type"))
                    .map(|(_, v)| crate::grpc::headers::is_grpc_content_type(v))
                    .unwrap_or(false);
                #[cfg(not(feature = "grpc"))]
                let is_grpc_ct = false;

                // F-132: WASM レスポンスボディフィルタ（HTTP/3 は WASM 適用時に必ず
                // Decision::Buffer に落ちるためボディ全体がメモリ上にある。end_of_stream=true
                // の 1 回呼びで実装できる）。書き換えたら content-length を更新する（B-46）。
                #[cfg(feature = "wasm")]
                if !is_grpc_ct {
                    if let Some(modules) = wasm_modules {
                        if !modules.is_empty() {
                            let config = CURRENT_CONFIG.load();
                            if let Some(ref wasm_engine) = config.wasm_filter_engine {
                                match crate::wasm::http_executor::apply_wasm_response_body(
                                    wasm_engine,
                                    modules,
                                    Bytes::from(body),
                                    true,
                                )
                                .await
                                {
                                    crate::wasm::http_executor::WasmBodyOutcome::Continue(b) => {
                                        crate::wasm::http_executor::set_content_length_header(
                                            &mut resp_header_store,
                                            b.len(),
                                        );
                                        body = b.to_vec();
                                    }
                                    crate::wasm::http_executor::WasmBodyOutcome::LocalResponse(
                                        resp,
                                    ) => {
                                        let resp_headers: Vec<(&[u8], &[u8])> = resp
                                            .headers
                                            .iter()
                                            .map(|(k, v)| (k.as_slice(), v.as_slice()))
                                            .collect();
                                        self.send_response(
                                            resp.status_code,
                                            &resp_headers,
                                            Some(Bytes::copy_from_slice(&resp.body)),
                                        )
                                        .await?;
                                        return Ok((resp.status_code, resp.body.len()));
                                    }
                                }
                            }
                        }
                    }
                }

                // 圧縮判定
                let mut content_type: Option<&[u8]> = None;
                let mut existing_encoding: Option<&[u8]> = None;
                for (name, value) in &resp_header_store {
                    if name.eq_ignore_ascii_case(b"content-type") {
                        content_type = Some(value.as_slice());
                    } else if name.eq_ignore_ascii_case(b"content-encoding") {
                        existing_encoding = Some(value.as_slice());
                    }
                }

                let should_compress = if is_grpc_ct {
                    None
                } else {
                    compression.should_compress(
                        client_encoding,
                        content_type,
                        Some(body.len()),
                        existing_encoding,
                    )
                };

                // レスポンスヘッダ + H2C trailers をマージ（B-39）
                let owned_headers = merge_response_headers_and_trailers(
                    &resp_header_store,
                    &trailers,
                    should_compress.is_some(),
                );

                let response_body = if let Some(enc) = should_compress {
                    compress_body_h3(&body, enc, compression)
                } else {
                    body
                };
                let response_body = Bytes::from(response_body);

                let resp_headers: Vec<(&[u8], &[u8])> = owned_headers
                    .iter()
                    .map(|(n, v)| (n.as_slice(), v.as_slice()))
                    .collect();

                // gRPC: trailers 用 API で終端（status は既に headers にマージ済み）
                #[cfg(feature = "grpc")]
                if is_grpc_ct && !trailers.is_empty() {
                    let grpc_status = trailers
                        .iter()
                        .find(|(n, _)| n.eq_ignore_ascii_case(b"grpc-status"))
                        .and_then(|(_, v)| std::str::from_utf8(v).ok())
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(0);
                    let grpc_message = trailers
                        .iter()
                        .find(|(n, _)| n.eq_ignore_ascii_case(b"grpc-message"))
                        .and_then(|(_, v)| String::from_utf8(v.clone()).ok());
                    // ヘッダにマージ済み + ボディ送信。status 200 で trailers も送る。
                    self.send_grpc_response(
                        &resp_headers,
                        Some(response_body.clone()),
                        grpc_status,
                        grpc_message.as_deref(),
                    )
                    .await?;
                    return Ok((status_code, response_body.len()));
                }

                self.send_response(status_code, &resp_headers, Some(response_body.clone()))
                    .await?;
                Ok((status_code, response_body.len()))
            }
            Err(e) => {
                warn!("[HTTP/3] Async backend proxy error: {}", e);
                self.send_error_response(502, b"Bad Gateway").await?;
                Ok((502, 11))
            }
        }
    }

    /// ファイル配信
    async fn handle_sendfile(&mut self, req: SendFileRequest<'_>) -> io::Result<(u16, usize)> {
        let SendFileRequest {
            base_path,
            is_dir,
            index_file,
            req_path,
            prefix,
            security,
            compression,
            client_encoding,
            open_file_cache_config,
            canonical_base,
            static_file_cache_config,
            #[cfg(feature = "wasm")]
            wasm_modules,
        } = req;
        let path_str = std::str::from_utf8(req_path).unwrap_or("/");
        let prefix_str = std::str::from_utf8(prefix).unwrap_or("");

        // プレフィックス除去後のサブパス
        let sub_path = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
            &path_str[prefix_str.len()..]
        } else {
            path_str
        };

        let clean_sub = sub_path.trim_start_matches('/');

        // パストラバーサル防止
        if clean_sub.contains("..") {
            self.send_error_response(403, b"Forbidden").await?;
            return Ok((403, 9));
        }

        // ファイルパス構築（素の join のみ。ディレクトリかどうかの判定は下の
        // `get_file_info_with_config` の非同期呼び出しへ委譲する）。
        let full_path = if is_dir {
            let mut p = base_path.to_path_buf();
            if !clean_sub.is_empty() {
                p.push(clean_sub);
            }
            p
        } else {
            if !clean_sub.is_empty() {
                self.send_error_response(404, b"Not Found").await?;
                return Ok((404, 9));
            }
            base_path.to_path_buf()
        };

        // B-65（続き）: 従来は「メタデータ解決（offload 1）→ …index 解決…→ 本体読み込み
        // （offload 2）」で 1 リクエストあたり offload のクロススレッド往復が 2 回
        // 発生していた（`docs/backlog/bugs/B-65-freebsd-h2c-request-cost.md`）。
        // 当初の改修は `is_dir == false`（固定ファイルルート）でしか高速経路が使われず、
        // 実運用で一般的なディレクトリルートでは改善していなかった。
        //
        // `is_dir` の true/false に関わらずまず `get_static_file_with_content` を 1 回
        // 呼ぶ（両キャッシュヒット時は offload ゼロ）。ディレクトリルートの場合のみ
        // `containment` に「このルート自身の」`base_path` を渡し、高速経路（Linux）は
        // その `base_path` の dirfd に対してのみ open する（F-154:
        // `resolve::open_beneath_in_root`）ため、これ自体が per-route の封じ込めになり、
        // `readlink` 等の事後検査は不要（`cache::static_file` モジュール doc 参照）。
        // h1/h2 と同一方針。
        let content_cfg = cache::effective_static_content_cache_config(static_file_cache_config);
        let containment = is_dir.then_some(cache::RouteContainment {
            root: base_path,
            canonical_base,
        });
        let first_result = cache::get_static_file_with_content(
            &full_path,
            open_file_cache_config,
            &content_cfg,
            containment,
        )
        .await;

        // F-169: `served_path`（実際に配信するパス。ディレクトリルートで index に
        // フォールバックした場合は index 側）を圧縮結果キャッシュのキーに使う。
        // `full_path` の所有権をそのまま流用するため追加のクローンは発生しない
        // （h2 側 `h2_sendfile` と同じ方針）。
        let (data, mime_owned, served_path): (
            bytes::Bytes,
            std::sync::Arc<str>,
            std::path::PathBuf,
        ) = match first_result {
            Some(cache::StaticFileOutcome::File(info, data)) => (data, info.mime_type, full_path),
            Some(cache::StaticFileOutcome::Forbidden) => {
                self.send_error_response(403, b"Forbidden").await?;
                return Ok((403, 9));
            }
            Some(cache::StaticFileOutcome::Directory(file_info)) => {
                // ディレクトリの場合はインデックスファイルを解決してからもう一度呼ぶ
                // （h1/h2 と同一ロジック。封じ込め検査は既に上で通過済みのため index
                // パスにも同じ containment を渡す）。
                let filename = index_file.unwrap_or("index.html");
                let index_path = file_info.canonical_path.join(filename);
                match cache::get_static_file_with_content(
                    &index_path,
                    open_file_cache_config,
                    &content_cfg,
                    containment,
                )
                .await
                {
                    Some(cache::StaticFileOutcome::File(info, data)) => {
                        (data, info.mime_type, index_path)
                    }
                    Some(cache::StaticFileOutcome::Forbidden) | None => {
                        self.send_error_response(403, b"Forbidden").await?;
                        return Ok((403, 9));
                    }
                    Some(cache::StaticFileOutcome::Directory(_)) => {
                        // index ファイル自体がさらにディレクトリ（通常起こり得ないが、
                        // 安全側に倒して 403 とする）。
                        self.send_error_response(403, b"Forbidden").await?;
                        return Ok((403, 9));
                    }
                }
            }
            None => {
                // ファイルが開けない場合はキャッシュを無効化（HTTP/1.1・HTTP/2 と同様）。
                cache::invalidate_file_cache(&full_path);
                cache::invalidate_content_cache(&full_path);
                self.send_error_response(404, b"Not Found").await?;
                return Ok((404, 9));
            }
        };
        let mime_str: &str = &mime_owned;

        // F-169: 圧縮ネゴシエーション + 静的配信の圧縮結果キャッシュ
        // （`cache::compressed`、`content_cfg.enabled` = `static_file_cache` 有効時のみ）。
        // 従来この関数は圧縮設定自体を受け取っておらず、HTTP/3 の静的配信では
        // 圧縮が一切効いていなかった。
        let should_compress = compression.should_compress(
            client_encoding,
            Some(mime_str.as_bytes()),
            Some(data.len()),
            None,
        );
        let mut encoding_name: &[u8] = b"";
        let response_body: Bytes = if let Some(enc) = should_compress {
            encoding_name = match enc {
                AcceptedEncoding::Zstd => b"zstd",
                AcceptedEncoding::Brotli => b"br",
                AcceptedEncoding::Gzip => b"gzip",
                AcceptedEncoding::Deflate => b"deflate",
                AcceptedEncoding::Identity => b"",
            };
            if content_cfg.enabled {
                let level = cache::compressed::compression_level(enc, compression);
                cache::compressed::get_or_compress(&served_path, enc, level, &content_cfg, || {
                    compress_body_h3(&data, enc, compression)
                })
            } else {
                Bytes::from(compress_body_h3(&data, enc, compression))
            }
        } else {
            // Bytes::clone() は参照カウント増加のみ（ディープコピー無し）。
            data.clone()
        };

        // 応答ヘッダは借用のまま組み立てる（以前は `Vec<(Vec<u8>, Vec<u8>)>` で
        // リクエストごとに 7〜8 回確保していた）。所有バッファが要るのは F-132 の
        // WASM on_response_headers を適用するときだけ。
        let mut resp_headers: Vec<(&[u8], &[u8])> =
            Vec::with_capacity(4 + security.add_response_headers.len());
        resp_headers.push((b"content-type", mime_str.as_bytes()));
        resp_headers.push((b"server", b"veil/http3"));
        for (k, v) in &security.add_response_headers {
            resp_headers.push((k.as_bytes(), v.as_bytes()));
        }
        if !encoding_name.is_empty() {
            resp_headers.push((b"content-encoding", encoding_name));
            resp_headers.push((b"vary", b"Accept-Encoding"));
        }

        // F-132: h1/h2 と同様、静的配信にも WASM on_response_headers を適用する。
        #[cfg(feature = "wasm")]
        if let Some(modules) = wasm_modules {
            let header_store: Vec<(Vec<u8>, Vec<u8>)> = resp_headers
                .iter()
                .map(|(k, v)| (k.to_vec(), v.to_vec()))
                .collect();
            let header_store = apply_h3_wasm_response_headers(modules, 200, header_store).await;
            let owned: Vec<(&[u8], &[u8])> = header_store
                .iter()
                .map(|(k, v)| (k.as_slice(), v.as_slice()))
                .collect();
            self.send_response(200, &owned, Some(response_body.clone()))
                .await?;
            return Ok((200, response_body.len()));
        }

        self.send_response(200, &resp_headers, Some(response_body.clone()))
            .await?;
        Ok((200, response_body.len()))
    }

    /// リダイレクト処理
    async fn handle_redirect(
        &mut self,
        redirect_url: &str,
        status_code: u16,
        preserve_path: bool,
        req_path: &[u8],
        prefix: &[u8],
    ) -> io::Result<(u16, usize)> {
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

        if preserve_path && !sub_path.is_empty() {
            if final_url.ends_with('/') && sub_path.starts_with('/') {
                final_url.push_str(&sub_path[1..]);
            } else if !final_url.ends_with('/') && !sub_path.starts_with('/') {
                final_url.push('/');
                final_url.push_str(sub_path);
            } else {
                final_url.push_str(sub_path);
            }
        }

        self.send_response(
            status_code,
            &[
                (b"location", final_url.as_bytes()),
                (b"server", b"veil/http3"),
            ],
            None,
        )
        .await?;

        Ok((status_code, 0))
    }
}
