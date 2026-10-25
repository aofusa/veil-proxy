//! HTTP/2 のハンドラー（接続のアクター・ストリームの駆動・応答の送出）
//!
//! F-233 で `src/proxy.rs` から移した（中身は変えていない）。
//!
//! 3,000 行を超えるのは、1 つの接続のアクター（ストリームの表・駆動・要求ごとのタスク・上流の各経路）が
//! 互いの非公開の状態を使う 1 つのまとまりだから。これ以上分けるのは F-234（L7 の共通化）で段を外へ出してから。

use super::*;

// ====================
// HTTP/2 ハンドラー
// ====================
//
// HTTP/2 (RFC 7540) 接続を処理します。
// ALPN ネゴシエーションで h2 が選択された場合に呼び出されます。
// HTTP/1.1 と同等のセキュリティ機能とルーティングをサポート。
/// HTTP/2 リクエストを処理
///
/// HTTP/2 コネクションのメインループを実行し、各ストリームのリクエストを処理します。
/// HTTP/1.1 と同等のセキュリティチェック、ルーティング、プロキシ機能をサポート。
#[cfg(feature = "http2")]
pub(super) async fn handle_http2_connection<S>(tls_stream: S, client_ip: &str)
where
    S: crate::runtime::io::AsyncReadRent
        + crate::runtime::io::AsyncWriteRentExt
        + AsRawFd
        + crate::runtime::io::BufferedReadState
        + Unpin,
{
    use http2::Http2Connection;

    // HTTP/2 設定をCURRENT_CONFIGから取得（ホットリロード対応）
    let config = CURRENT_CONFIG.load();
    let settings = config.http2_config.to_http2_settings();

    // HTTP/2 コネクションを作成
    let mut conn = Http2Connection::new(tls_stream, settings);

    // ハンドシェイク（プリフェース確認 + SETTINGS 交換）
    if let Err(e) = conn.handshake().await {
        warn!("[HTTP/2] Handshake error: {}", e);
        return;
    }

    debug!("[HTTP/2] Connection established from {}", client_ip);

    // アクティブ接続メトリクスの自動管理（Dropで自動デクリメント）
    let mut connection_metric = ActiveConnectionMetric::new(true);

    // カスタムリクエストハンドラーを使用してメインループ実行
    let result = handle_http2_requests(&mut conn, client_ip, &mut connection_metric, None).await;

    if let Err(e) = result {
        warn!("[HTTP/2] Connection error: {}", e);
    }

    debug!("[HTTP/2] Connection closed from {}", client_ip);
}

/// HTTP/2 メインループ（F-116: ストリーム多重化アクターモデル）
///
/// コネクション（`Http2Connection`）は本メインループが専有駆動し、リクエストごとの
/// ルーティング・セキュリティ・WASM・バックエンド往復は per-stream タスク
/// （[`h2_request_task`]）へ切り出す。タスクは conn に一切触れず、レスポンスは
/// [`H2RespMsg`] チャネルで、リクエストボディは [`Bytes`] チャネルでメインループと通信する。
/// これにより遅いバックエンドのストリームが他ストリームの応答送出をブロックしない。
#[cfg(feature = "http2")]
pub(super) async fn handle_http2_requests<S>(
    conn: &mut http2::Http2Connection<S>,
    client_ip: &str,
    connection_metric: &mut ActiveConnectionMetric,
    // F-200: 平文リスナー（`listen_cleartext`）で `enable_secure_redirect = true` のとき、
    // すべての要求を https へ 301 で返す（TLS の接続は常に None）。値はリダイレクト先のポート（F-260）。
    redirect_https: Option<u16>,
) -> Result<(), http2::Http2Error>
where
    S: crate::runtime::io::AsyncReadRent
        + crate::runtime::io::AsyncWriteRentExt
        + AsRawFd
        + crate::runtime::io::BufferedReadState
        + Unpin,
{
    let svc = ProxyH2Service {
        spawner: h2_task_spawner(),
    };
    handle_http2_requests_with(conn, client_ip, connection_metric, redirect_https, &svc).await
}

/// HTTP/2 の per-stream 処理を差し替える口（F-222: メッシュのデータプレーンが自分のルーティングと上流で
/// 使う）。veil の通常のプロキシは [`ProxyH2Service`]（従来の処理そのもの。単相化で同じコードになる）。
#[cfg(feature = "http2")]
pub(crate) trait H2Service {
    /// 本文がまだ届いている途中の要求を、本文をチャネルで流す（ストリーミング）にするか。
    /// `Some(上限)` ならストリーミング（0 は上限なし）、`None` は本文を溜めてから処理する。
    fn streaming_plan<S>(
        &self,
        conn: &http2::Http2Connection<S>,
        stream_id: u32,
        client_ip: &str,
        client_socket_addr: SocketAddr,
    ) -> Option<u64>
    where
        S: crate::runtime::io::AsyncReadRent + crate::runtime::io::AsyncWriteRentExt + Unpin;

    /// 要求 1 つを処理するタスクを起動する。
    fn spawn(
        &self,
        ctx: H2RequestCtx,
        req_rx: Option<crate::stream_channel::Receiver<Bytes>>,
        resp_tx: crate::stream_channel::Sender<H2RespMsg>,
        notify: crate::stream_channel::Notify,
    );
}

/// 通常のプロキシの per-stream 処理（ルーティング・セキュリティ・WASM・バックエンド）。
#[cfg(feature = "http2")]
pub(super) struct ProxyH2Service {
    spawner: H2TaskSpawner,
}

#[cfg(feature = "http2")]
impl H2Service for ProxyH2Service {
    #[inline]
    fn streaming_plan<S>(
        &self,
        conn: &http2::Http2Connection<S>,
        stream_id: u32,
        client_ip: &str,
        client_socket_addr: SocketAddr,
    ) -> Option<u64>
    where
        S: crate::runtime::io::AsyncReadRent + crate::runtime::io::AsyncWriteRentExt + Unpin,
    {
        h2_route_streaming_plan(conn, stream_id, client_ip, client_socket_addr)
    }

    #[inline]
    fn spawn(
        &self,
        ctx: H2RequestCtx,
        req_rx: Option<crate::stream_channel::Receiver<Bytes>>,
        resp_tx: crate::stream_channel::Sender<H2RespMsg>,
        notify: crate::stream_channel::Notify,
    ) {
        (self.spawner)(ctx, req_rx, resp_tx, notify)
    }
}

/// [`handle_http2_requests`] の本体（per-stream 処理を `svc` で差し替えられる）。
#[cfg(feature = "http2")]
pub(crate) async fn handle_http2_requests_with<S, V>(
    conn: &mut http2::Http2Connection<S>,
    client_ip: &str,
    connection_metric: &mut ActiveConnectionMetric,
    redirect_https: Option<u16>,
    svc: &V,
) -> Result<(), http2::Http2Error>
where
    S: crate::runtime::io::AsyncReadRent
        + crate::runtime::io::AsyncWriteRentExt
        + AsRawFd
        + crate::runtime::io::BufferedReadState
        + Unpin,
    V: H2Service,
{
    use http2::frame::Frame;
    use http2::Http2Error;

    let notify = crate::stream_channel::Notify::new();
    // F-162: per-stream 固定費削減。`client_ip` の `Rc<str>` 化と `SocketAddr` 解決は
    // 接続あたり 1 回だけ行い、以降は `Rc::clone`（参照カウント +1）と値渡し（Copy）で使い回す。
    let client_ip_rc: std::rc::Rc<str> = std::rc::Rc::from(client_ip);
    let client_socket_addr = h2_client_socket_addr(client_ip);
    let mut streams: std::collections::HashMap<u32, H2ActiveStream> =
        std::collections::HashMap::new();
    // `drive_h2_streams` のストリーム ID 走査用バッファ（F-157）。コネクションごとに
    // 1 本持ち回して毎イテレーションのヒープ確保を避ける。
    let mut stream_ids: Vec<u32> = Vec::new();

    loop {
        // 1. 各ストリームのレスポンスを送出（HPACK エンコードは送信順にここでのみ行う）。
        drive_h2_streams(conn, &mut streams, &mut stream_ids).await?;
        // 2. 1 イテレーション 1 回のフラッシュ（複数ストリームの write_buf 合流を 1 回で送出）。
        conn.flush_write_buf().await?;

        // 3. 読み込みバッファに溜まった完全フレームを I/O なしで連続処理。
        let mut processed_any = false;
        loop {
            let frame = match conn.try_read_frame_buffered() {
                Ok(Some(f)) => {
                    processed_any = true;
                    f
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = conn
                        .send_goaway(e.error_code(), e.to_string().as_bytes())
                        .await;
                    return Err(e);
                }
            };

            // リクエスト方向ストリーミング中のストリームへの DATA はタスクの req チャネルへ流す。
            if let Frame::Data {
                stream_id,
                end_stream,
                data,
            } = frame
            {
                let is_streaming = streams
                    .get(&stream_id)
                    .map(|s| s.req_streaming && !s.req_eof)
                    .unwrap_or(false);
                if is_streaming {
                    let data_len = data.len();
                    // フロー制御・状態遷移・content-length 検証（recv ウィンドウの補充は
                    // しない。補充は drive が req チャネルへ転送できた消費連動で行う）。
                    if let Err(e) = conn.recv_data_for_streaming(stream_id, end_stream, data_len) {
                        if e.should_goaway() {
                            let _ = conn
                                .send_goaway(e.error_code(), e.to_string().as_bytes())
                                .await;
                            return Err(e);
                        } else if let Some(id) = e.rst_stream_id() {
                            let _ = conn.send_rst_stream(id, e.error_code()).await;
                        }
                        streams.remove(&stream_id);
                        // 未転送分のクレジットが conn レベルに残らないよう補充する。
                        conn.replenish_recv_window(stream_id);
                        // 直後の process_frame が write_all（write_buf 空の不変条件）を使うため、
                        // 積んだ WINDOW_UPDATE を先に送出する。
                        conn.flush_write_buf().await?;
                        continue;
                    }
                    if let Some(st) = streams.get_mut(&stream_id) {
                        st.req_bytes_total = st.req_bytes_total.saturating_add(data_len as u64);
                        if st.max_request_body > 0 && st.req_bytes_total > st.max_request_body {
                            // ボディ上限超過: RST_STREAM で打ち切りタスクを停止（チャネル drop）。
                            conn.flush_write_buf().await?;
                            let _ = conn
                                .send_rst_stream(stream_id, http2::Http2ErrorCode::EnhanceYourCalm)
                                .await;
                            streams.remove(&stream_id);
                            // 打ち切りで消費されないクレジットを conn レベルへ返す。
                            conn.replenish_recv_window(stream_id);
                            // 直後の process_frame が write_all（write_buf 空の不変条件）を使うため、
                            // 積んだ WINDOW_UPDATE を先に送出する。
                            conn.flush_write_buf().await?;
                            continue;
                        }
                        if st.req_tx.is_none() {
                            // B-130: 受け手（ハンドラ・上流への中継）が本文を読み終える前に閉じた。残りの本文は
                            // 捨てて、そのぶんの受信ウィンドウをすぐ返す（溜めると補充されずに接続全体の
                            // ウィンドウが尽き、同じ接続の他の要求が止まる）。
                            if data_len > 0 {
                                conn.replenish_recv_window(stream_id);
                                // 直後の process_frame は write_buf が空である前提（write_all）なので先に送る
                                conn.flush_write_buf().await?;
                            }
                        } else if data_len > 0 {
                            // ゼロコピー: 受信フレームの所有バッファをそのまま Bytes 化して転送。
                            st.req_pending.push_back(Bytes::from(data));
                        }
                        if end_stream {
                            st.req_eof = true;
                        }
                    }
                    continue;
                }

                // 通常（バッファ）経路の DATA は process_frame が request_body へ蓄積する。
                match conn
                    .process_frame(Frame::Data {
                        stream_id,
                        end_stream,
                        data,
                    })
                    .await
                {
                    Ok(Some(req)) => {
                        h2_spawn_for_request(
                            conn,
                            req.stream_id,
                            req.body_pending,
                            &mut streams,
                            &notify,
                            svc,
                            &client_ip_rc,
                            client_socket_addr,
                            connection_metric,
                            redirect_https,
                        );
                    }
                    Ok(None) => {}
                    Err(e) => {
                        if e.should_goaway() {
                            let _ = conn
                                .send_goaway(e.error_code(), e.to_string().as_bytes())
                                .await;
                            return Err(e);
                        } else if let Some(id) = e.rst_stream_id() {
                            let _ = conn.send_rst_stream(id, e.error_code()).await;
                            streams.remove(&id);
                        }
                    }
                }
                continue;
            }

            // RST_STREAM 受信ストリームは streams から除去（チャネル drop でタスク停止）。
            let rst_target = if let Frame::RstStream { stream_id, .. } = &frame {
                Some(*stream_id)
            } else {
                None
            };

            match conn.process_frame(frame).await {
                Ok(Some(req)) => {
                    if let Some(st) = streams.get_mut(&req.stream_id) {
                        // 既にタスク起動済みのストリーム: リクエスト方向ストリーミング中に
                        // トレイラー（END_STREAM 付き HEADERS / CONTINUATION 完了）で終端した
                        // 場合、process_frame は body_pending=false の ProcessedRequest を返す。
                        // 新規タスクは起動せず、req チャネルの EOF（sender drop）へ変換する
                        // （さもないとバックエンドが終端チャンクを受け取れずハングする）。
                        if st.req_streaming && !req.body_pending {
                            st.req_eof = true;
                        }
                    } else {
                        h2_spawn_for_request(
                            conn,
                            req.stream_id,
                            req.body_pending,
                            &mut streams,
                            &notify,
                            svc,
                            &client_ip_rc,
                            client_socket_addr,
                            connection_metric,
                            redirect_https,
                        );
                    }
                }
                Ok(None) => {
                    if let Some(id) = rst_target {
                        if let Some(st) = streams.remove(&id) {
                            // ストリーミング中の打ち切りは未転送クレジットを conn レベルへ返す。
                            if st.req_streaming {
                                conn.replenish_recv_window(id);
                                // 直後の process_frame が write_all（write_buf 空の不変条件）を
                                // 使うため、積んだ WINDOW_UPDATE を先に送出する。
                                conn.flush_write_buf().await?;
                            }
                        }
                    }
                }
                Err(e) => {
                    if e.should_goaway() {
                        let _ = conn
                            .send_goaway(e.error_code(), e.to_string().as_bytes())
                            .await;
                        return Err(e);
                    } else if let Some(id) = e.rst_stream_id() {
                        let _ = conn.send_rst_stream(id, e.error_code()).await;
                        if let Some(st) = streams.remove(&id) {
                            if st.req_streaming {
                                conn.replenish_recv_window(id);
                                // 直後の process_frame が write_all（write_buf 空の不変条件）を
                                // 使うため、積んだ WINDOW_UPDATE を先に送出する。
                                conn.flush_write_buf().await?;
                            }
                        }
                    }
                }
            }
        }

        conn.cleanup_closed();

        // 4. 入力取得 / 待機。
        //
        // (a) TLS 復号済み平文がストリーム内部に滞留している場合は POLLIN では通知されないため、
        //     待機せず `fill_read_buf` で能動的に読み出して再ループする（さもないと既読データが
        //     残ったまま POLLIN を待ってデッドロック / CPU スピンする）。
        // (b) 本周でフレームを処理した場合（WINDOW_UPDATE で送信ウィンドウが回復した等）は待機せず
        //     drive へ戻り、回復したウィンドウで pending_body を直ちに送出する。
        // (c) それ以外（idle / `read_buf` 内の分割フレーム待ち）は `select2` で待機する。分割フレーム
        //     の続きは POLLIN で発火し、タスク出力は notify で発火する（Notify は取りこぼさない）。
        let need_fill = if conn.has_stream_buffered_read_data() {
            true
        } else if processed_any {
            // 進捗ありだが追加入力はまだ。drive へ戻る（fill しない）。
            continue;
        } else if streams.is_empty() && !conn.has_partial_frame() {
            // 処理中のストリームも分割フレームの続きも無い＝次の要求を待っているだけ。
            // アイドルタイムアウト（B-119: 以前は無期限に待っていた）と graceful shutdown
            // （F-187: `expire_idle_timeouts` で即座に満了）で GOAWAY を送って閉じる。
            match timeout_idle(
                IDLE_TIMEOUT,
                true,
                h2_select_readable_or_notify(conn.raw_fd(), &notify),
            )
            .await
            {
                Ok(readable) => readable,
                Err(_) => {
                    conn.close_with_goaway(http2::Http2ErrorCode::NoError, b"")
                        .await;
                    break;
                }
            }
        } else {
            // 処理中のストリームがある: 可読 or notify を待つ。可読なら fill する。
            h2_select_readable_or_notify(conn.raw_fd(), &notify).await
        };

        if need_fill {
            match conn.fill_read_buf().await {
                Ok(_) => {}
                Err(Http2Error::ConnectionClosed) => break,
                Err(Http2Error::Io(ref e)) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(Http2Error::Io(ref e)) if is_connection_closed_error(e) => {
                    debug!(
                        "[HTTP/2] Connection closed by client (expected): {} (client: {})",
                        e, client_ip
                    );
                    break;
                }
                Err(e) => {
                    let _ = conn
                        .send_goaway(e.error_code(), e.to_string().as_bytes())
                        .await;
                    return Err(e);
                }
            }
        }
    }

    Ok(())
}

/// per-stream タスクからメインループへ送るレスポンス断片（F-116）。
///
/// ボディ終端は **送信端（[`crate::stream_channel::Sender`]）の drop** で表す
/// （メインループは EOF を検出して END_STREAM を送出する）。HPACK エンコードは
/// 接続状態を進めるため、[`H2RespMsg::Head`]/[`H2RespMsg::Trailers`] のエンコードは
/// **メインループが送信順に**行う（タスクは名前/値ペアのまま渡す）。
#[cfg(feature = "http2")]
pub(crate) enum H2RespMsg {
    /// レスポンス head（ステータス + ヘッダ）。最初に 1 回だけ送られる。
    ///
    /// F-166/F-165(B-2/A4): ヘッダは `Vec<(Bytes, Bytes)>`。`Bytes` は
    /// `AsRef<[u8]>` を実装し `send_headers_buffered_end` は既にジェネリクス化済みのため
    /// 送信側は無改修。定数ヘッダ（server/alt-svc）は `Bytes::from_static`/参照カウント
    /// clone で構築でき、`h2_proxy_h2c` はバックエンド応答の `Bytes` をムーブするだけで
    /// 済み、従来の `(k.clone(), v.clone())` ディープコピーが消える。
    Head {
        status: u16,
        headers: Vec<(Bytes, Bytes)>,
        end_stream: bool,
    },
    /// レスポンスボディ断片（ゼロコピー）。
    Body(Bytes),
    /// gRPC トレイラー等（END_STREAM 付き HEADERS）。gRPC 経路でのみ生成・送出する。
    #[cfg(feature = "grpc")]
    Trailers(Vec<(Bytes, Bytes)>),
    /// ハンドラのストリーミング応答のトレーラ（全フィールドをそのまま送る。F-197）。
    #[cfg(feature = "handler")]
    HandlerTrailers(Vec<(Bytes, Bytes)>),
    /// head 送出後のバックエンドエラー等でストリームをリセットする（RST_STREAM エラーコード）。
    Reset(u32),
}

/// per-stream リクエストタスクへ conn 非依存で引き渡すリクエスト情報（F-116）。
#[cfg(feature = "http2")]
pub(crate) struct H2RequestCtx {
    /// デコーダのアリーナ由来 `Bytes`（参照カウント clone のみ、R2/F-165）。
    pub(crate) method: Bytes,
    pub(crate) path: Bytes,
    /// `:authority`（無ければ host ヘッダー、いずれも無ければ空）。
    pub(crate) authority: Bytes,
    /// 疑似ヘッダー `:method`/`:path`/`:authority` を除いたヘッダー（`:scheme` 等の残余含む）。
    pub(crate) headers: Vec<crate::http2::hpack::HeaderField>,
    /// 完了済みリクエストボディ（バッファ経路。ストリーミング経路では空）。
    pub(crate) body: Bytes,
    /// リクエストトレイラー（gRPC 等。通常のリクエストでは空、F-133）。wasm feature
    /// 無効時は誰も読まないためフィールド自体を無くす（dead_code 回避）。
    #[cfg(feature = "wasm")]
    trailers: Vec<crate::http2::hpack::HeaderField>,
    /// 接続あたり 1 回だけ確保して `Rc::clone`（参照カウント +1）で使い回す（F-162）。
    pub(crate) client_ip: std::rc::Rc<str>,
    /// `client_ip` から解決した `SocketAddr`。接続確立時に 1 回だけ解決する（F-162）。
    pub(crate) client_socket_addr: SocketAddr,
    /// 平文リスナーの `enable_secure_redirect`（F-200）。`Some(port)` なら要求を https の `port` へ 301 で返す（F-260）。
    pub(crate) redirect_https: Option<u16>,
    pub(crate) start: Instant,
}

/// メインループ側のストリーム状態（F-116）。
#[cfg(feature = "http2")]
pub(super) struct H2ActiveStream {
    /// レスポンス断片の受信端。
    resp_rx: crate::stream_channel::Receiver<H2RespMsg>,
    /// リクエストボディ断片の送信端（ストリーミング経路のみ。EOF で None）。
    req_tx: Option<crate::stream_channel::Sender<Bytes>>,
    /// req チャネルへ未投入のボディ（満杯時の溢れ）。
    req_pending: std::collections::VecDeque<Bytes>,
    /// リクエスト方向ストリーミング対象か。
    req_streaming: bool,
    /// クライアント END_STREAM 受信済み。
    req_eof: bool,
    /// 受信済みリクエストボディ累計（`max_request_body_size` 強制用）。
    req_bytes_total: u64,
    /// リクエストボディ上限（0 = 無制限）。
    max_request_body: u64,
    /// HEADERS 送出済み。
    head_sent: bool,
    /// END_STREAM 送出済み（レスポンス完了）。
    end_sent: bool,
    /// フロー制御ウィンドウ待ちの未送信ボディ残 `(buf, 送信済みオフセット)`。
    pending_body: Option<(Bytes, usize)>,
    /// `pending_body` を送り切ったら END_STREAM を付けて終える（応答の最後の断片だった）。
    end_with_pending: bool,
    /// 最後の断片かを見るために先読みしたメッセージ（次の周回で最初に処理する）。
    peeked: Option<H2RespMsg>,
}

/// per-stream リクエストタスクのスポーナ型（F-46: 型付きタスクプールで Box 確保を回避）。
#[cfg(feature = "http2")]
pub(super) type H2TaskSpawner = std::rc::Rc<
    dyn Fn(
        H2RequestCtx,
        Option<crate::stream_channel::Receiver<Bytes>>,
        crate::stream_channel::Sender<H2RespMsg>,
        crate::stream_channel::Notify,
    ),
>;

/// HTTP/2 ワーカースレッド用の per-stream タスクスポーナを作成する。
///
/// F-158（A-1'）: **readiness reactor バックエンドに限り** `pool.spawn()` ではなく
/// `pool.spawn_inline()` を使う。タスク本体（`h2_request_task`）が実際に pend する
/// await を 1 つも持たない場合（静的キャッシュヒット等）、その場でタスク自身の実 Waker で
/// 1 回 poll し、`Poll::Ready` まで進めばエグゼキュータへ一切登録しない。これにより
/// 「spawn → メインループへ戻る → 1 周回後にエグゼキュータがタスクを実行する」という
/// 強制往復が 1 リクエストにつき 1 回消える。`Poll::Pending` の場合は `spawn()` と全く
/// 同じ状態でエグゼキュータに残るため、戻り値（完了したか）はここでは無視してよい。
///
/// ## なぜ io_uring では使わないのか（実測に基づく）
///
/// **この最適化は readiness reactor 固有のコストを消すものであり、io_uring では
/// 逆に退行する。** 交互 A/B の実測:
///
/// - FreeBSD 14.3 aarch64（kqueue）: 3B レスポンスで中央値 **+27.3%**
///   （10/10 ラウンド勝ち・分布は完全分離）。54KB は +0.8% で退行なし。
/// - Linux x86_64（io_uring）: `h2c_file` で中央値 **−4.6%**（12 ラウンド中 11 で
///   ベースラインが勝ち）。`h2c_proxy` も −3.8%。
///
/// 理由は待機の実装差にある。reactor では `h2_select_readable_or_notify` が
/// `Readable::poll` を通り、**kqueue ヒントが無いときは同期 `poll(2)` を 1 回発行する**
/// （この `poll(2)` フォールバック自体は F-141/F-155 の実測により削除禁止。
/// AGENTS.md 参照）。インライン初回 poll はこの往復ごと消せるため大きく効く。
/// 一方 io_uring では同じ待機が `IORING_OP_POLL_ADD` の SQE 1 本であり、他の SQE と
/// まとめて submit されるため**消せる往復コストがそもそも小さい**。残るのは
/// インライン poll 側の固定費（スロット確保・Waker 構築・`EXEC_STATE` の追加借用）と
/// SQE バッチングの乱れだけになり、差し引きで退行する。
///
/// **したがって io_uring 側にはこの機構を実装せず**（`runtime::uring::executor` は
/// F-158 以前と 1 バイトも変わらない）、`veil_rt_reactor` のときだけ `spawn_inline` を
/// 呼ぶ。AGENTS.md の「io_uring パスのロジックは変えない」「Linux io_uring 経路の
/// 非劣化保証」の両方を満たす。
#[cfg(feature = "http2")]
pub(super) fn h2_task_spawner() -> H2TaskSpawner {
    let pool = crate::runtime::TaskPool::new();
    std::rc::Rc::new(move |ctx, req_rx, resp_tx, notify| {
        let fut = h2_request_task(ctx, req_rx, resp_tx, notify);
        #[cfg(veil_rt_reactor)]
        {
            let _ = pool.spawn_inline(fut);
        }
        #[cfg(not(veil_rt_reactor))]
        {
            pool.spawn(fut);
        }
    })
}

/// `write_buf` がこのサイズを超えたら drive 中に明示フラッシュする（F-116）。
#[cfg(feature = "http2")]
pub(super) const WRITE_BUF_FLUSH_THRESHOLD: usize = 128 * 1024;
/// レスポンスチャネル容量（HTTP/3 と同水準）。
#[cfg(feature = "http2")]
pub(super) const H2_RESP_CHANNEL_CAP: usize = 4;
/// リクエストボディチャネル容量。
#[cfg(feature = "http2")]
pub(super) const H2_REQ_CHANNEL_CAP: usize = 4;

/// HEADERS 完了時（`ProcessedRequest`）にストリームタスクを起動する（F-116）。
///
/// `body_pending=false` は END_STREAM 済み（バッファ経路）。`body_pending=true` は
/// ボディ継続で、ストリーミング適格ならリクエストボディチャネル付きで起動する。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
pub(super) fn h2_spawn_for_request<S, V>(
    conn: &mut http2::Http2Connection<S>,
    stream_id: u32,
    body_pending: bool,
    streams: &mut std::collections::HashMap<u32, H2ActiveStream>,
    notify: &crate::stream_channel::Notify,
    svc: &V,
    client_ip: &std::rc::Rc<str>,
    client_socket_addr: SocketAddr,
    connection_metric: &mut ActiveConnectionMetric,
    redirect_https: Option<u16>,
) where
    S: crate::runtime::io::AsyncReadRent + crate::runtime::io::AsyncWriteRentExt + Unpin,
    V: H2Service,
{
    // 既にタスク起動済みのストリーム（body_pending の二重通知）はスキップ。
    if streams.contains_key(&stream_id) {
        return;
    }

    // ストリーミング適格判定 + リクエストボディ上限をルーティング 1 回で取得する
    // （適格判定と上限取得で find_backend_unified を二重実行しない）。
    // リダイレクトだけを返す接続（F-200）は本文を読む必要が無いのでストリーミングにしない。
    let plan = if body_pending && redirect_https.is_none() {
        svc.streaming_plan(conn, stream_id, client_ip, client_socket_addr)
    } else {
        None
    };
    let streaming = plan.is_some();
    if body_pending && !streaming {
        // 非適格: 何もしない。DATA は process_frame が request_body へ蓄積し、
        // END_STREAM 受信時に body_pending=false の ProcessedRequest で本関数が再度呼ばれる。
        return;
    }
    let max_request_body = plan.unwrap_or(0);

    let parts = match conn.take_request_parts(stream_id) {
        Some(p) => p,
        None => return,
    };

    // authority（host フォールバック）を解決。
    let authority = parts.authority.clone().unwrap_or_else(|| {
        parts
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(b"host"))
            .map(|h| h.value.clone())
            .unwrap_or_default()
    });

    if let Ok(host_str) = std::str::from_utf8(&authority) {
        connection_metric.set_host(host_str);
    } else {
        connection_metric.set_host("unknown");
    }

    let ctx = H2RequestCtx {
        method: parts.method,
        path: parts.path,
        authority,
        headers: parts.headers,
        body: parts.body.freeze(),
        #[cfg(feature = "wasm")]
        trailers: parts.trailers,
        client_ip: std::rc::Rc::clone(client_ip),
        client_socket_addr,
        redirect_https,
        // アクセスログ・メトリクスが無効なら時刻を読まない（ストリームごとの固定費）。
        start: crate::logging::request_start_instant(),
    };

    let (resp_tx, resp_rx) = crate::stream_channel::channel::<H2RespMsg>(H2_RESP_CHANNEL_CAP);
    let (req_tx, req_rx) = if streaming {
        let (tx, rx) = crate::stream_channel::channel::<Bytes>(H2_REQ_CHANNEL_CAP);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    // A-1': spawner はタスクを `spawn_inline` するため、この呼び出し自体が
    // `h2_request_task` を完了まで（ペンドせずに済む限り）その場で実行する可能性がある。
    // つまりタスクが Head/Body を `resp_tx` へ送出し終えるのが、`streams.insert` より
    // **前**になり得る。これは安全: `resp_rx`（受信端）はこの関数のローカル変数として
    // 保持したままであり、他のタスク/コネクションが同時に読み出すことは無い
    // （thread-per-core・単一スレッドのため、この関数呼び出し自体が他コードと
    // 競合しない）。`streams.insert` はこの直後に行われ、以降 `drive_h2_streams` が
    // `resp_rx.try_recv()` でチャネルに既に積まれたメッセージを回収する。チャネルは
    // 容量 `H2_RESP_CHANNEL_CAP`（4）の有界チャネルであり、それを超える送出は
    // タスク側が Pending で止まる（= 通常の spawn 経路にフォールバックする）ため、
    // 送出順序を先取りしても取りこぼしは発生しない。
    svc.spawn(ctx, req_rx, resp_tx, notify.clone());

    streams.insert(
        stream_id,
        H2ActiveStream {
            resp_rx,
            req_tx,
            req_pending: std::collections::VecDeque::new(),
            req_streaming: streaming,
            req_eof: false,
            req_bytes_total: 0,
            max_request_body,
            head_sent: false,
            end_sent: false,
            pending_body: None,
            end_with_pending: false,
            peeked: None,
        },
    );
}

/// 2 つの Future（ソケット可読 / notify）を自前 `poll_fn` で race する（futures 依存を増やさない）。
///
/// 戻り値 `true` = 可読、`false` = notify。両 Future ともキャンセル安全
/// （`wait_readable_fd` は POLL_ADD、`notify.wait` はフラグ待ち）。
#[cfg(feature = "http2")]
pub(super) async fn h2_select_readable_or_notify(
    fd: RawFd,
    notify: &crate::stream_channel::Notify,
) -> bool {
    use std::future::poll_fn;
    use std::future::Future;
    use std::task::Poll;

    let readable = crate::runtime::tcp::wait_readable_fd(fd);
    let wait = notify.wait();
    let mut readable = std::pin::pin!(readable);
    let mut wait = std::pin::pin!(wait);
    poll_fn(move |cx| {
        if readable.as_mut().poll(cx).is_ready() {
            return Poll::Ready(true);
        }
        if wait.as_mut().poll(cx).is_ready() {
            return Poll::Ready(false);
        }
        Poll::Pending
    })
    .await
}

/// 全ストリームのレスポンスを 1 回駆動する（F-116）。
///
/// 各ストリームで pending_body 再送 → resp_rx を try_recv して
/// HEADERS/DATA/Trailers/Reset を送出。ウィンドウ枯渇時は pending_body へ保留。
/// チャネル EOF（sender drop）+ 未完了なら空 END_STREAM DATA でクリーンに閉じる。
#[cfg(feature = "http2")]
pub(super) async fn drive_h2_streams<S>(
    conn: &mut http2::Http2Connection<S>,
    streams: &mut std::collections::HashMap<u32, H2ActiveStream>,
    stream_ids: &mut Vec<u32>,
) -> Result<(), http2::Http2Error>
where
    S: crate::runtime::io::AsyncReadRent + crate::runtime::io::AsyncWriteRentExt + Unpin,
{
    use crate::stream_channel::{TryRecv, TrySendError};

    let mut done: Vec<u32> = Vec::new();

    // ストリーム ID の走査用バッファ（F-157）。毎イテレーションの
    // `collect::<Vec<u32>>()` によるヒープ確保を消すため、呼び出し元
    // （`handle_http2_requests`）が持つ**コネクションごと**のバッファを借りて再利用する。
    //
    // スレッドローカルにしてはならない: このループは `.await` をまたぐため、
    // 同一スレッド上の別コネクションの `drive_h2_streams` が割り込んで
    // 同じバッファを `clear()` し、走査中の ID 列を壊す。
    stream_ids.clear();
    stream_ids.extend(streams.keys().copied());

    for &sid in stream_ids.iter() {
        // ストリームの表は、このループの後（`done` の処理）でだけ取り除く。ループの間は同じ
        // エントリを指し続けるので、1 回だけ引いて使い回す（`conn` とは別の借用）。
        let Some(st) = streams.get_mut(&sid) else {
            continue;
        };
        // --- リクエストボディを req チャネルへ流す（バックプレッシャ考慮） ---
        {
            let mut transferred = false;
            while st.req_tx.is_some() {
                let item = match st.req_pending.pop_front() {
                    Some(it) => it,
                    None => break,
                };
                match st.req_tx.as_ref().unwrap().try_send(item) {
                    Ok(()) => transferred = true,
                    Err(TrySendError::Full(it)) => {
                        st.req_pending.push_front(it);
                        break;
                    }
                    Err(TrySendError::Closed) => {
                        // B-130: 受け手が閉じた。溜めていた本文を捨て、そのぶんのウィンドウを返す
                        st.req_pending.clear();
                        st.req_tx = None;
                        transferred = true;
                    }
                }
            }
            if st.req_tx.is_some() && st.req_pending.is_empty() && st.req_eof {
                // クライアント END_STREAM 済み・全量投入済み → sender drop で EOF 伝播。
                st.req_tx = None;
            }
            // recv ウィンドウの消費連動補充（F-116 バックプレッシャ）: req チャネルへ転送
            // できた分だけクライアントへ新規クレジットを渡す。受信時に即補充すると
            // バックエンドが遅い場合に req_pending が際限なく成長する（旧 F-32 経路の
            // 「バックエンド書き込み後に補充」と同じ消費連動タイミング）。
            if transferred {
                conn.replenish_recv_window(sid);
            }
        }

        // --- pending_body（ウィンドウ待ち残）を先に再送 ---
        {
            if let Some((buf, off)) = st.pending_body.take() {
                // 本体の持ち主ごと渡す（FreeBSD の平文 h2c は共有メモリの本体を sendfile(2) で送る）
                let end = st.end_with_pending;
                let queued = conn.queue_data_frames_bytes(sid, &buf.slice(off..), end)?;
                let new_off = off + queued;
                if conn.pending_write_len() > WRITE_BUF_FLUSH_THRESHOLD {
                    conn.flush_write_buf().await?;
                }
                if new_off < buf.len() {
                    // まだウィンドウ不足 → 再保留してこのストリームは以降スキップ。
                    st.pending_body = Some((buf, new_off));
                    continue;
                }
                if end {
                    // 最後の断片を送り切った（END_STREAM はその DATA フレームに付けた）
                    st.end_sent = true;
                    done.push(sid);
                    continue;
                }
            }
        }

        // --- resp_rx を送出可能な限りドレイン ---
        loop {
            let recv = match st.peeked.take() {
                Some(m) => TryRecv::Item(m),
                None => st.resp_rx.try_recv(),
            };
            let msg = match recv {
                TryRecv::Item(m) => m,
                TryRecv::Empty => break,
                TryRecv::Closed => {
                    if st.head_sent && !st.end_sent {
                        // EOF: head 送出済みで END_STREAM 未送なら空 DATA で閉じる。
                        conn.queue_data_frames(sid, &[], true)?;
                        st.end_sent = true;
                    } else if !st.head_sent {
                        // タスクが Head を送らずに終了（想定外の異常終了）: クライアントを
                        // 宙吊りにしないよう RST_STREAM で明示的に打ち切る。
                        conn.flush_write_buf().await?;
                        let _ = conn
                            .send_rst_stream(sid, http2::Http2ErrorCode::InternalError)
                            .await;
                    }
                    done.push(sid);
                    break;
                }
            };

            match msg {
                H2RespMsg::Head {
                    status,
                    headers,
                    end_stream,
                } => {
                    // F-157/A-3: `send_headers_buffered_end` は `AsRef<[u8]>` の名前/値ペア列を
                    // 直接受け付けるようジェネリクス化済みのため、`headers: Vec<(Vec<u8>, Vec<u8>)>`
                    // を `&[u8]` へ変換する中間 `Vec<(&[u8], &[u8])>` の `collect()`
                    // （1 レスポンスにつき 1 回のヒープ確保）が不要になった。
                    conn.send_headers_buffered_end(sid, status, &headers, end_stream)
                        .await?;
                    st.head_sent = true;
                    if end_stream {
                        st.end_sent = true;
                    }
                }
                H2RespMsg::Body(bytes) => {
                    // 次のメッセージを先読みし、送り手が閉じていればこの断片が最後: END_STREAM を
                    // 最後の DATA フレームに付ける（空の DATA フレームを別に送らない。1 応答につき
                    // フレーム 1 つと、TCP のセグメント・相手の起床が 1 回減る）。
                    let last = match st.resp_rx.try_recv() {
                        TryRecv::Closed => true,
                        TryRecv::Item(m) => {
                            st.peeked = Some(m);
                            false
                        }
                        TryRecv::Empty => false,
                    };
                    let last = last && st.head_sent && !st.end_sent;
                    let queued = conn.queue_data_frames_bytes(sid, &bytes, last)?;
                    if conn.pending_write_len() > WRITE_BUF_FLUSH_THRESHOLD {
                        conn.flush_write_buf().await?;
                    }
                    if queued < bytes.len() {
                        st.pending_body = Some((bytes, queued));
                        st.end_with_pending = last;
                        break;
                    }
                    if last {
                        st.end_sent = true;
                        done.push(sid);
                        break;
                    }
                }
                #[cfg(feature = "grpc")]
                H2RespMsg::Trailers(_trailers) => {
                    // トレイラー送出は write_all を伴うため先に合流バッファをフラッシュ。
                    conn.flush_write_buf().await?;
                    {
                        let mut grpc_status = 0u32;
                        let mut grpc_message: Option<String> = None;
                        for (name, value) in &_trailers {
                            if name.as_ref() == b"grpc-status" as &[u8] {
                                if let Ok(s) = std::str::from_utf8(value) {
                                    grpc_status = s.trim().parse().unwrap_or(0);
                                }
                            } else if name.as_ref() == b"grpc-message" as &[u8] {
                                grpc_message =
                                    std::str::from_utf8(value).ok().map(|s| s.to_string());
                            }
                        }
                        let _ = conn
                            .send_grpc_trailers(sid, grpc_status, grpc_message.as_deref())
                            .await;
                    }
                    st.head_sent = true;
                    st.end_sent = true;
                }
                #[cfg(feature = "handler")]
                H2RespMsg::HandlerTrailers(trailers) => {
                    conn.flush_write_buf().await?;
                    let _ = conn.send_trailers(sid, &trailers).await;
                    st.head_sent = true;
                    st.end_sent = true;
                }
                H2RespMsg::Reset(code) => {
                    conn.flush_write_buf().await?;
                    let _ = conn
                        .send_rst_stream(sid, http2::Http2ErrorCode::from_u32(code))
                        .await;
                    st.end_sent = true;
                    done.push(sid);
                    break;
                }
            }
        }
    }

    for sid in done {
        if let Some(st) = streams.remove(&sid) {
            // タスクが早期終了したストリーミングストリームは req_pending が未転送のまま
            // 残り得る。そのクレジットが conn レベルに滞留しないよう補充する。
            if st.req_streaming {
                conn.replenish_recv_window(sid);
                // 応答を送り終えたのに要求本文がまだ届いている（上限超過で早期に応答した等）:
                // RST_STREAM(NO_ERROR) で送信を止めてもらう（RFC 9113 §8.1）。
                if !st.req_eof && st.end_sent {
                    conn.flush_write_buf().await?;
                    let _ = conn
                        .send_rst_stream(sid, http2::Http2ErrorCode::NoError)
                        .await;
                }
            }
        }
    }
    Ok(())
}

/// リクエスト方向ストリーミングの適格判定（F-32 の条件を厳密に踏襲）。
///
/// 条件: Proxy バックエンド + WASM モジュール非適用 + 非 gRPC + バッファリング非 Full +
/// 上流 use_h2c 以外 + セキュリティ許可 + サーバー選択成功。conn は借用のみ（変更しない）。
/// 適格なら `Some(max_request_body)`（0 = 無制限）を返す。
#[cfg(feature = "http2")]
pub(super) fn h2_route_streaming_plan<S>(
    conn: &http2::Http2Connection<S>,
    stream_id: u32,
    client_ip: &str,
    client_socket_addr: SocketAddr,
) -> Option<u64>
where
    S: crate::runtime::io::AsyncReadRent + crate::runtime::io::AsyncWriteRentExt + Unpin,
{
    let stream = conn.get_stream(stream_id)?;
    let method = stream
        .method()
        .map(|m| m.to_vec())
        .unwrap_or_else(|| b"GET".to_vec());
    let path = stream
        .path()
        .map(|p| p.to_vec())
        .unwrap_or_else(|| b"/".to_vec());
    let authority = stream
        .authority()
        .map(|a| a.to_vec())
        .or_else(|| {
            stream
                .request_headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case(b"host"))
                .map(|h| h.value.to_vec())
        })
        .unwrap_or_default();

    // `Bytes` の参照カウント clone のみ（デコーダのアリーナ由来なのでコピーなし、R2/F-165）。
    let h2_headers_store: Vec<(Bytes, Bytes)> = stream
        .request_headers
        .iter()
        .map(|h| (h.name.clone(), h.value.clone()))
        .collect();
    let headers_raw: Vec<(&[u8], &[u8])> = h2_headers_store
        .iter()
        .map(|(k, v)| (k.as_ref(), v.as_ref()))
        .collect();

    let query_start = path.iter().position(|&b| b == b'?');
    let raw_query: &[u8] = query_start.map(|i| &path[i + 1..]).unwrap_or(b"");
    let path_wo_query = query_start.map(|i| &path[..i]).unwrap_or(&path[..]);

    let backend_result = crate::l7::resolve_route(
        &authority,
        path_wo_query,
        &method,
        &headers_raw,
        raw_query,
        &client_socket_addr,
        false,
    );

    let (_prefix, backend, _rc) = backend_result?;
    // ストリーミング受信のハンドラ（F-197）: 本文の上限は掛けない（gRPC はメッセージ単位で検査する）。
    #[cfg(feature = "handler")]
    if let Backend::Handler(entry, _, _, _) = &backend {
        return entry.streaming_request.then_some(0);
    }
    let (upstream_group, security, buffering) = match &backend {
        Backend::Proxy(ug, sec, _comp, buf, _cache, modules) => {
            if modules
                .as_ref()
                .is_some_and(|m: &Arc<Vec<crate::wasm_plugin_config::ModuleRef>>| !m.is_empty())
            {
                return None;
            }
            (ug.clone(), sec.clone(), buf.clone())
        }
        _ => return None,
    };

    let is_grpc = h2_headers_store.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case(b"content-type")
            && value
                .get(..16)
                .map(|p| p.eq_ignore_ascii_case(b"application/grpc"))
                .unwrap_or(false)
    });
    // gRPC は buffering = full をバイパスする（F-97）。
    if buffering.mode == crate::buffering::BufferingMode::Full && !is_grpc {
        return None;
    }
    if check_security(&security, client_ip, &method, 0, true) != SecurityCheckResult::Allowed {
        return None;
    }
    let server = upstream_group.select(client_ip)?;
    // F-171: h2c 上流は多重化接続（F-174）のストリームで全二重に中継する。gRPC を HTTP/1.1
    // 上流へ送る構成は従来どおりバッファ経路（トレーラーを HTTP/1.1 で受けられない）。
    let h2_upstream =
        server.target.use_h2c || upstream_group.use_h2c() || server.target.h2_over_tls();
    if is_grpc && !h2_upstream {
        return None;
    }
    Some(security.max_request_body_size as u64)
}

/// クライアント IP 文字列を `SocketAddr` に変換する（ルーティング入力）。
#[cfg(feature = "http2")]
pub(super) fn h2_client_socket_addr(client_ip: &str) -> SocketAddr {
    if let Ok(addr) = client_ip.parse::<SocketAddr>() {
        addr
    } else if let Ok(ip) = client_ip.parse::<std::net::IpAddr>() {
        SocketAddr::new(ip, 80)
    } else {
        SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)), 0)
    }
}

/// per-stream リクエストタスク本体（F-116）。conn には一切触れない。
#[cfg(feature = "http2")]
pub(super) async fn h2_request_task(
    ctx: H2RequestCtx,
    req_rx: Option<crate::stream_channel::Receiver<Bytes>>,
    resp_tx: crate::stream_channel::Sender<H2RespMsg>,
    notify: crate::stream_channel::Notify,
) {
    let user_agent: Bytes = ctx
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(b"user-agent"))
        .map(|h| h.value.clone())
        .unwrap_or_default();

    let (status, resp_size, req_size) = if let Some(rx) = req_rx {
        h2_serve_streaming(&ctx, &rx, &resp_tx, &notify).await
    } else {
        h2_serve_buffered(&ctx, &resp_tx, &notify).await
    };

    if status != 0 {
        log_access(
            &ctx.method,
            &ctx.authority,
            &ctx.path,
            &user_agent,
            req_size,
            status,
            resp_size,
            ctx.start,
            &ctx.client_ip,
            "",
        );
    }
    // resp_tx / req_rx はここで drop → メインループへ EOF 伝播。
    notify.notify();
}

/// レスポンスチャネルへ送り、メインループへ通知する。receiver 切断時は `Err(())`。
#[cfg(feature = "http2")]
#[inline]
pub(super) async fn h2_send(
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
    msg: H2RespMsg,
) -> Result<(), ()> {
    let r = resp_tx.send(msg).await.map_err(|_| ());
    notify.notify();
    r
}

/// サーバー/Alt-Svc 等の共通レスポンスヘッダを所有ベクタで構築する。
///
/// F-166: 名前は静的定数（`Bytes::from_static`）、値は Guard から参照カウント
/// `clone()`（`value_bytes()`）で取得する。**リクエストごとの `Vec<u8>` 確保はゼロ**
/// （外側の `Vec` 自体は呼び出しごとに 1 回のみ確保。中身の名前/値はヒープ確保なし）。
#[cfg(feature = "http2")]
pub(super) fn h2_base_headers(add_alt_svc: bool) -> Vec<(Bytes, Bytes)> {
    // 呼び出し側はこの戻り値へさらに content-type / location / 上流ヘッダ等を push する。
    // 容量を 2 に切り詰めると **リクエストごとに 1 回 realloc** が発生する
    // （`alloc-stats` の reallocs/req が 2.31 → 3.34 に増えた形で実測された）。
    // 従来どおり 4 を確保しておく。
    let mut headers: Vec<(Bytes, Bytes)> = Vec::with_capacity(4);
    if let Some(ref g) = get_server_header_guard() {
        headers.push((Bytes::from_static(b"server"), g.value_bytes()));
    }
    if add_alt_svc {
        if let Some(g) = get_alt_svc_guard() {
            headers.push((Bytes::from_static(b"alt-svc"), g.value_bytes()));
        }
    }
    headers
}

/// 完結した（頭+本体一括の）レスポンスをメッセージ化して送る。戻り値 `(status, body_len)`。
#[cfg(feature = "http2")]
pub(super) async fn h2_emit_full(
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
    status: u16,
    headers: Vec<(Bytes, Bytes)>,
    body: Bytes,
) -> (u16, u64) {
    let len = body.len() as u64;
    let empty = body.is_empty();
    if h2_send(
        resp_tx,
        notify,
        H2RespMsg::Head {
            status,
            headers,
            end_stream: empty,
        },
    )
    .await
    .is_err()
    {
        return (status, len);
    }
    if !empty {
        let _ = h2_send(resp_tx, notify, H2RespMsg::Body(body)).await;
    }
    (status, len)
}

/// 要求本文の残りを読み捨てる。戻り値は読み捨てたバイト数。
///
/// 消費するたびに `notify` でメインループを起こす。起こさないと `req_pending` の残りがチャネルへ
/// 補充されず、recv ウィンドウも補充されないので、ウィンドウを超える本文を送るクライアントが
/// 止まったまま終わらない（reactor では他に起床の契機が無く必ず固まる。B-123）。
#[cfg(feature = "http2")]
pub(super) async fn h2_drain_req(
    req_rx: &crate::stream_channel::Receiver<Bytes>,
    notify: &crate::stream_channel::Notify,
) -> u64 {
    let mut n = 0u64;
    while let Some(c) = req_rx.recv().await {
        n += c.len() as u64;
        notify.notify();
    }
    n
}

/// エラー応答（Server ヘッダ付き）をメッセージ化して送る。戻り値 `(status, body_len)`。
#[cfg(feature = "http2")]
pub(super) async fn h2_emit_error(
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
    status: u16,
    msg: &[u8],
) -> (u16, u64) {
    h2_emit_full(
        resp_tx,
        notify,
        status,
        h2_base_headers(false),
        Bytes::copy_from_slice(msg),
    )
    .await
}

/// バッファ経路（END_STREAM 済み）の 1 リクエストを処理してレスポンスを送出する（F-116）。
///
/// 戻り値 `(status, resp_size, req_size)`。`status == 0` はクライアント切断（ログ不要）。
#[cfg(feature = "http2")]
pub(super) async fn h2_serve_buffered(
    ctx: &H2RequestCtx,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64, u64) {
    let req_size = ctx.body.len() as u64;
    let (status, resp_size) = h2_dispatch(ctx, resp_tx, notify).await;
    (status, resp_size, req_size)
}

/// バッファ経路のルーティング + ディスパッチ。戻り値 `(status, resp_size)`。
#[cfg(feature = "http2")]
pub(super) async fn h2_dispatch(
    ctx: &H2RequestCtx,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64) {
    let method = &ctx.method[..];
    let path = &ctx.path[..];
    let client_ip: &str = &ctx.client_ip;

    // F-200: 平文リスナーの HTTPS リダイレクト。
    if let Some(port) = ctx.redirect_https {
        let host = std::str::from_utf8(&ctx.authority).unwrap_or("");
        let location =
            crate::config::https_redirect_url(host, std::str::from_utf8(path).unwrap_or("/"), port);
        let mut headers = h2_base_headers(false);
        headers.push((Bytes::from_static(b"location"), Bytes::from(location)));
        return h2_emit_full(resp_tx, notify, 301, headers, Bytes::new()).await;
    }

    // 非疑似ヘッダーのみ（ルーティング・転送用）。
    let headers_raw: Vec<(&[u8], &[u8])> = ctx
        .headers
        .iter()
        .filter(|h| !h.name.starts_with(b":"))
        .map(|h| (h.name.as_ref(), h.value.as_ref()))
        .collect();

    // メトリクスエンドポイント。
    {
        let config = CURRENT_CONFIG.load();
        let prom = &config.prometheus_config;
        let path_str = std::str::from_utf8(path).unwrap_or("/");
        if prom.enabled && path_str == prom.path && method == b"GET" {
            if !prom.is_ip_allowed(client_ip) {
                return h2_emit_error(resp_tx, notify, 403, b"Forbidden").await;
            }
            let body = encode_prometheus_metrics();
            let mut headers = h2_base_headers(false);
            headers.push((
                Bytes::from_static(b"content-type"),
                Bytes::from_static(b"text/plain; version=0.0.4; charset=utf-8"),
            ));
            return h2_emit_full(resp_tx, notify, 200, headers, Bytes::from(body)).await;
        }
    }

    // 管理 API（B-29）。
    #[cfg(feature = "admin")]
    if let Some((status, headers, body)) = h2_admin_response(method, path, client_ip, &headers_raw)
    {
        return h2_emit_full(resp_tx, notify, status, headers, Bytes::from(body)).await;
    }

    let query_start = path.iter().position(|&b| b == b'?');
    let raw_query: &[u8] = query_start.map(|i| &path[i + 1..]).unwrap_or(b"");
    let path_wo_query = query_start.map(|i| &path[..i]).unwrap_or(path);
    let authority = &ctx.authority[..];

    let backend_result = crate::l7::resolve_route(
        authority,
        path_wo_query,
        method,
        &headers_raw,
        raw_query,
        &ctx.client_socket_addr,
        true,
    );

    let (prefix, backend, route_compression) = match backend_result {
        Some(b) => b,
        None => {
            // 未登録のサービスへの gRPC は Trailers-Only の UNIMPLEMENTED（F-209）
            #[cfg(feature = "grpc-handler")]
            if let Some(h) = crate::api::grpc::unknown_route_response(
                ctx.headers.iter().map(|h| (&h.name[..], &h.value[..])),
                path,
            ) {
                let mut headers = h2_base_headers(false);
                headers.extend(h);
                return h2_emit_full(resp_tx, notify, 200, headers, Bytes::new()).await;
            }
            return h2_emit_error(resp_tx, notify, 404, b"Not Found").await;
        }
    };

    // セキュリティチェック。
    let security = backend.security();
    let check_result = check_security(security, client_ip, method, ctx.body.len(), false);
    if check_result != SecurityCheckResult::Allowed {
        let status = check_result.status_code();
        let msg = check_result.message();
        return h2_emit_error(resp_tx, notify, status, msg).await;
    }

    // WASM リクエストフィルタ。
    //
    // F-133: `on_request_headers` が返した変更後ヘッダを保持する（変更があった場合のみ Some）。
    // 従来 HTTP/2 経路は `Continue { .. } => {}` で**変更後ヘッダを捨てていた**ため、
    // WASM によるリクエストヘッダ変更が上流に一切反映されなかった（HTTP/3 経路は
    // `wasm_request_headers` として反映済み。HTTP/1.1 経路も反映済み）。
    #[cfg(feature = "wasm")]
    let mut wasm_request_headers_override: Option<Vec<crate::http2::hpack::HeaderField>> = None;
    #[cfg(feature = "wasm")]
    let wasm_modules_to_apply: Arc<Vec<crate::wasm_plugin_config::ModuleRef>> = {
        let config = CURRENT_CONFIG.load();
        if let Some(ref wasm_engine) = config.wasm_filter_engine {
            let path_str = std::str::from_utf8(path).unwrap_or("/");
            let method_str = std::str::from_utf8(method).unwrap_or("GET");
            let modules_to_apply = if let Some(backend_modules) = backend.modules_arc() {
                backend_modules.clone()
            } else {
                crate::wasm::empty_wasm_modules()
            };
            if !modules_to_apply.is_empty() {
                // WASM ホスト API 境界は `Vec<u8>` 所有ペアを要求するため、ここでのみ
                // `Bytes` → `Vec<u8>` へコピーする（guest 側は独立コピーを要するため
                // 避けられない境界コスト。ホットパス本体〈R1/R2〉には影響しない）。
                let headers_vec: Vec<(Vec<u8>, Vec<u8>)> = ctx
                    .headers
                    .iter()
                    .map(|h| (h.name.to_vec(), h.value.to_vec()))
                    .collect();
                let wasm_result = wasm_engine
                    .clone()
                    .on_request_headers_with_modules_async(
                        modules_to_apply.clone(),
                        Arc::from(path_str),
                        Arc::from(method_str),
                        headers_vec,
                        Arc::from(client_ip),
                        ctx.body.is_empty(),
                    )
                    .await;
                match wasm_result {
                    crate::wasm::FilterResult::LocalResponse(resp) => {
                        // resp.headers を所有権ムーブで変換（`.clone()` 不要、F-166）。
                        let mut headers: Vec<(Bytes, Bytes)> = resp
                            .headers
                            .into_iter()
                            .map(|(k, v)| (Bytes::from(k), Bytes::from(v)))
                            .collect();
                        for (n, v) in h2_base_headers(false) {
                            headers.push((n, v));
                        }
                        let (st, sz) = h2_emit_full(
                            resp_tx,
                            notify,
                            resp.status_code,
                            headers,
                            Bytes::from(resp.body),
                        )
                        .await;
                        crate::wasm::on_request_complete_async(
                            wasm_engine.clone(),
                            modules_to_apply.clone(),
                        )
                        .await;
                        return (st, sz);
                    }
                    crate::wasm::FilterResult::Pause => {
                        warn!("WASM module requested pause, but async operations are not yet supported");
                    }
                    crate::wasm::FilterResult::Continue { headers, .. } => {
                        // F-133: 変更後ヘッダを上流リクエストへ反映する（下の ctx 差し替え）。
                        wasm_request_headers_override = Some(
                            headers
                                .into_iter()
                                .map(|(name, value)| crate::http2::hpack::HeaderField {
                                    name: Bytes::from(name),
                                    value: Bytes::from(value),
                                })
                                .collect(),
                        );
                    }
                }
            }
            modules_to_apply
        } else {
            crate::wasm::empty_wasm_modules()
        }
    };

    // gRPC リクエストトレイラー WASM フィルタ（F-133）。トレイラー無しの一般的なリクエストは
    // `ctx.trailers` が空のため、この分岐だけで即座に抜ける（コスト増なし）。
    #[cfg(feature = "wasm")]
    if !ctx.trailers.is_empty() && !wasm_modules_to_apply.is_empty() {
        let config = CURRENT_CONFIG.load();
        if let Some(ref wasm_engine) = config.wasm_filter_engine {
            let trailers_vec: Vec<(Vec<u8>, Vec<u8>)> = ctx
                .trailers
                .iter()
                .map(|h| (h.name.to_vec(), h.value.to_vec()))
                .collect();
            let wasm_result = wasm_engine
                .on_request_trailers_with_modules(&wasm_modules_to_apply, trailers_vec)
                .await;
            match wasm_result {
                crate::wasm::FilterResult::LocalResponse(resp) => {
                    // resp.headers を所有権ムーブで変換（`.clone()` 不要、F-166）。
                    let mut headers: Vec<(Bytes, Bytes)> = resp
                        .headers
                        .into_iter()
                        .map(|(k, v)| (Bytes::from(k), Bytes::from(v)))
                        .collect();
                    for (n, v) in h2_base_headers(false) {
                        headers.push((n, v));
                    }
                    return h2_emit_full(
                        resp_tx,
                        notify,
                        resp.status_code,
                        headers,
                        Bytes::from(resp.body),
                    )
                    .await;
                }
                crate::wasm::FilterResult::Pause => {
                    warn!(
                        "WASM module requested pause on request trailers, but async operations are not yet supported"
                    );
                }
                // F-133 調査結果: リクエストトレイラー自体が上流バックエンドへ転送される
                // 経路が存在しない（`h2_proxy_h2c` は `h2c_client.send_request` に
                // method/path/authority/headers/body しか渡せず、リクエストトレイラーの
                // 送信 API が無い。H1/HTTPS バックエンド経路も同様）。したがって
                // WASM がここで返した変更後トレイラーには反映先が無く、意図的に破棄する
                // （元の `ctx.trailers` を使っても上流へは送られないため、変更後を
                // 使っても使わなくても観測可能な挙動差は無い）。将来リクエストトレイラー
                // 転送を実装する際は、ここで変更後トレイラーを保持して反映すること。
                crate::wasm::FilterResult::Continue { .. } => {}
            }
        }
    }

    // F-133: WASM がリクエストヘッダを変更した場合のみ、ヘッダを差し替えた `H2RequestCtx` を
    // ローカルに構築して `ctx` をシャドウイングする（ホットパス絶対規則: WASM 未適用時は
    // `None` 分岐のみでコスト増なし。`body` は `Bytes` の clone のため参照カウント増加のみ）。
    #[cfg(feature = "wasm")]
    let modified_ctx;
    #[cfg(feature = "wasm")]
    let ctx = if let Some(headers) = wasm_request_headers_override {
        modified_ctx = H2RequestCtx {
            method: ctx.method.clone(),
            path: ctx.path.clone(),
            authority: ctx.authority.clone(),
            headers,
            body: ctx.body.clone(),
            trailers: ctx.trailers.clone(),
            client_ip: ctx.client_ip.clone(),
            client_socket_addr: ctx.client_socket_addr,
            redirect_https: ctx.redirect_https,
            start: ctx.start,
        };
        &modified_ctx
    } else {
        ctx
    };

    // Accept-Encoding。
    let client_encoding = ctx
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(b"accept-encoding"))
        .map(|h| AcceptedEncoding::parse(&h.value))
        .unwrap_or(AcceptedEncoding::Identity);

    let result = match backend {
        Backend::Proxy(upstream_group, security, compression, _buffering, _cache, _) => {
            h2_proxy(
                ctx,
                &upstream_group,
                &compression,
                client_encoding,
                &prefix,
                &security,
                #[cfg(feature = "wasm")]
                &wasm_modules_to_apply,
                resp_tx,
                notify,
            )
            .await
        }
        Backend::MemoryFile(data, mime_type, security, _) => {
            let path_str = std::str::from_utf8(path).unwrap_or("/");
            let prefix_str = std::str::from_utf8(&prefix).unwrap_or("");
            let remainder = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
                &path_str[prefix_str.len()..]
            } else {
                ""
            };
            if !remainder.trim_matches('/').is_empty() {
                h2_emit_error(resp_tx, notify, 404, b"Not Found").await
            } else {
                let (built_headers, response_body) = build_h2_compressed_file_response(
                    Bytes::from_owner(ArcVecBytes(data)),
                    mime_type.as_ref(),
                    &security,
                    &route_compression,
                    client_encoding,
                    // MemoryFile はファイルシステムパスを持たない（config 由来の
                    // インメモリ内容）ため、圧縮結果キャッシュのキーが作れない。
                    None,
                );
                #[cfg(feature = "wasm")]
                let header_store =
                    apply_h2_wasm_response_headers(&wasm_modules_to_apply, 200, built_headers)
                        .await;
                #[cfg(not(feature = "wasm"))]
                let header_store = built_headers;
                h2_emit_full(resp_tx, notify, 200, header_store, response_body).await
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
            h2_sendfile(
                ctx,
                &base_path,
                is_dir,
                index_file.as_deref(),
                &prefix,
                &security,
                &route_compression,
                client_encoding,
                open_file_cache_config.as_deref(),
                canonical_base.as_deref(),
                static_file_cache_config.as_deref(),
                #[cfg(feature = "wasm")]
                &wasm_modules_to_apply,
                resp_tx,
                notify,
            )
            .await
        }
        Backend::Respond(r) => {
            // 固定応答（F-219）
            let mut headers = h2_base_headers(false);
            headers.extend(r.headers.iter().cloned());
            h2_emit_full(resp_tx, notify, r.status, headers, r.body.clone()).await
        }
        Backend::Redirect(redirect_url, status_code, preserve_path, _, security) => {
            h2_redirect(
                ctx,
                &redirect_url,
                status_code,
                preserve_path,
                &prefix,
                &security,
                resp_tx,
                notify,
            )
            .await
        }
        #[cfg(feature = "handler")]
        Backend::Handler(entry, security, _, comp) => {
            let (status, size, _) = h2_api_handler(
                ctx,
                &entry,
                comp.as_deref(),
                &security,
                None,
                resp_tx,
                notify,
            )
            .await;
            (status, size)
        }
    };

    #[cfg(feature = "wasm")]
    {
        if !wasm_modules_to_apply.is_empty() {
            let config = CURRENT_CONFIG.load();
            if let Some(ref wasm_engine) = config.wasm_filter_engine {
                crate::wasm::on_request_complete_async(
                    wasm_engine.clone(),
                    wasm_modules_to_apply.clone(),
                )
                .await;
            }
        }
    }

    result
}

/// HTTP/2 のネイティブハンドラ（API ライブラリ、F-188）。戻り値 `(status, resp_size, req_size)`。
///
/// `req_rx` があればストリーミング受信のルート（F-197）: 届いた本文断片をハンドラの
/// `Request::take_body_stream()` へ流しながら、応答（ストリーミング応答なら断片とトレーラ）を送る。
/// 本文の中継と応答の送出は同じタスクの中で `select` し、双方向ストリーム（gRPC の bidi）を成り立たせる。
#[cfg(all(feature = "http2", feature = "handler"))]
pub(super) async fn h2_api_handler(
    ctx: &H2RequestCtx,
    entry: &crate::api::handler::HandlerEntry,
    comp: Option<&crate::config::CompressionConfig>,
    security: &SecurityConfig,
    req_rx: Option<&crate::stream_channel::Receiver<Bytes>>,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64, u64) {
    let mut headers: Vec<(Bytes, Bytes)> = Vec::with_capacity(ctx.headers.len() + 1);
    let mut has_host = false;
    for h in ctx.headers.iter().filter(|h| !h.name.starts_with(b":")) {
        has_host |= h.name.eq_ignore_ascii_case(b"host");
        headers.push((h.name.clone(), h.value.clone()));
    }
    if !has_host && !ctx.authority.is_empty() {
        headers.push((Bytes::from_static(b"host"), ctx.authority.clone()));
    }
    let mut req = crate::api::dispatch::build_request(
        ctx.method.clone(),
        ctx.path.clone(),
        headers,
        ctx.body.clone(),
        &ctx.client_ip,
        crate::api::request::HttpVersion::Http2,
    );
    let Some(rx) = req_rx else {
        let (status, size) = h2_api_respond(entry, comp, security, req, resp_tx, notify).await;
        return (status, size, ctx.body.len() as u64);
    };

    let (mut body_tx, slot) = crate::api::stream::request_body_channel(H2_REQ_CHANNEL_CAP);
    req.body_stream = Some(slot);
    let req_size = std::cell::Cell::new(0u64);
    let req_size_ref = &req_size;
    // `body_tx` は move で持たせる（中継が終わったら drop して受信ストリームを終える）
    let pump = async move {
        let req_size = req_size_ref;
        use futures::SinkExt;
        while let Some(chunk) = rx.recv().await {
            req_size.set(req_size.get() + chunk.len() as u64);
            if body_tx.send(chunk).await.is_err() {
                // ハンドラが受信ストリームを捨てた: 残りは読み捨てる（クライアントを止めない）。
                req_size.set(req_size.get() + h2_drain_req(rx, notify).await);
                break;
            }
            // 消費したらメインループを起こして残りの補充と WINDOW_UPDATE をさせる（B-123）
            notify.notify();
        }
        // body_tx の drop で受信ストリームが終わる
    };
    let work = h2_api_respond(entry, comp, security, req, resp_tx, notify);
    let (status, size) =
        match futures::future::select(std::pin::pin!(work), std::pin::pin!(pump)).await {
            futures::future::Either::Left((r, _pump)) => r,
            futures::future::Either::Right(((), work)) => work.await,
        };
    (status, size, req_size.get())
}

/// ハンドラを呼んで応答を送る（全量・ストリーミングの両方）。戻り値 `(status, resp_size)`。
#[cfg(all(feature = "http2", feature = "handler"))]
pub(super) async fn h2_api_respond(
    entry: &crate::api::handler::HandlerEntry,
    comp: Option<&crate::config::CompressionConfig>,
    security: &SecurityConfig,
    req: crate::api::request::Request,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64) {
    let mut resp = crate::api::dispatch::call(entry, req, comp).await;
    crate::api::dispatch::h2_h3_headers(&mut resp);
    // `[route.traffic.cors]` の実際の応答のヘッダ（F-273。CORS の無いルートは分岐だけ）
    crate::route_cors::apply_bytes(&mut resp.headers, security);
    let status = resp.status_code();
    let crate::api::response::Response {
        headers: resp_headers,
        body,
        stream,
        ..
    } = resp;
    let mut out = h2_base_headers(false);
    out.extend(resp_headers);
    let Some(mut stream) = stream else {
        return h2_emit_full(resp_tx, notify, status, out, body).await;
    };
    if h2_send(
        resp_tx,
        notify,
        H2RespMsg::Head {
            status,
            headers: out,
            end_stream: false,
        },
    )
    .await
    .is_err()
    {
        return (status, 0);
    }
    use crate::api::stream::BodyChunk;
    use futures::StreamExt;
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
                H2RespMsg::Body(b)
            }
            BodyChunk::Trailers(mut t) => {
                crate::api::dispatch::lowercase_names(&mut t);
                let _ = h2_send(resp_tx, notify, H2RespMsg::HandlerTrailers(t)).await;
                return (status, sent);
            }
            BodyChunk::Abort(reason) => {
                ftlog::debug!("[api] streaming response aborted: {}", reason);
                let _ = h2_send(
                    resp_tx,
                    notify,
                    H2RespMsg::Reset(http2::Http2ErrorCode::InternalError as u32),
                )
                .await;
                return (status, sent);
            }
        };
        if h2_send(resp_tx, notify, msg).await.is_err() {
            // クライアントがストリームを閉じた: ストリームを drop してハンドラ側へ伝える
            return (status, sent);
        }
    }
    // 送信端の drop で END_STREAM
    (status, sent)
}

/// バッファ経路の Proxy 処理（H1/HTTPS/H2C バックエンド）。戻り値 `(status, resp_size)`。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn h2_proxy(
    ctx: &H2RequestCtx,
    upstream_group: &Arc<UpstreamGroup>,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    prefix: &[u8],
    security: &SecurityConfig,
    #[cfg(feature = "wasm")] wasm_modules: &Arc<Vec<crate::wasm_plugin_config::ModuleRef>>,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64) {
    let method = &ctx.method[..];
    let req_path = &ctx.path[..];
    let client_ip: &str = &ctx.client_ip;

    // Consistent Hash キー解決。
    let hash_key_owned: Option<String> = match &upstream_group.algorithm {
        crate::config::LoadBalanceAlgorithm::ConsistentHash {
            hash_key: crate::config::HashKey::Header(name),
        } => ctx
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name.as_bytes()))
            .and_then(|h| std::str::from_utf8(&h.value).ok())
            .map(|s| s.to_string()),
        crate::config::LoadBalanceAlgorithm::ConsistentHash {
            hash_key: crate::config::HashKey::Cookie(name),
        } => ctx
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(b"cookie"))
            .and_then(|h| std::str::from_utf8(&h.value).ok())
            .and_then(|c| {
                c.split(';').find_map(|part| {
                    let part = part.trim();
                    part.split_once('=').and_then(|(k, v)| {
                        if k.trim().eq_ignore_ascii_case(name) {
                            Some(v.trim().to_string())
                        } else {
                            None
                        }
                    })
                })
            }),
        _ => None,
    };

    let server = match upstream_group.select_with_key(client_ip, hash_key_owned.as_deref(), None) {
        Some(s) => s,
        None => return h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await,
    };
    server.acquire();
    let target = &server.target;

    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    let preserve_grpc_path = ctx
        .headers
        .iter()
        .any(|h| header_pair_is_grpc(&h.name, &h.value));
    let final_path_owned =
        compute_upstream_path(path_str, prefix, &target.path_prefix, preserve_grpc_path);
    let final_path = final_path_owned.as_str();

    // H2C バックエンドは HPACK 応答のため専用処理。
    if target.use_h2c || upstream_group.use_h2c() {
        let addr = target.conn_addr(); // F-170: UDS 対応（TCP は従来と不変）
        let addr = addr.as_str();
        let result = h2_proxy_h2c(
            ctx,
            addr,
            target,
            method,
            final_path.as_bytes(),
            security,
            #[cfg(feature = "wasm")]
            wasm_modules,
            resp_tx,
            notify,
        )
        .await;
        server.release();
        return result;
    }

    // F-175: HTTPS 上流は ALPN で HTTP/2 を試す（多重化接続）。上流が HTTP/1.1 を選べば
    // ネゴシエーションに使った接続を TLS プールへ入れて、下の HTTP/1.1 経路で使う。
    if target.h2_over_tls() {
        let tls_profile = upstream_group.tls_profile();
        let addr = target.conn_addr();
        let key = crate::http_utils::PoolKeyStr::tls_addr(addr.as_str(), target.sni(), tls_profile);
        let is_grpc = ctx
            .headers
            .iter()
            .any(|h| header_pair_is_grpc(&h.name, &h.value));
        let added = expanded_added_headers(
            security,
            client_ip,
            &ctx.authority,
            std::str::from_utf8(req_path).unwrap_or("/"),
        );
        let headers_iter = ctx
            .headers
            .iter()
            .filter(|h| is_grpc || !h.name.eq_ignore_ascii_case(b"te"))
            .filter(|h| forward_allowed(security, &h.name))
            .map(|h| (h.name.as_ref(), h.value.as_ref()))
            .chain(added.iter().map(|(n, v)| (n.as_slice(), v.as_slice())));
        let body = (!ctx.body.is_empty()).then(|| ctx.body.clone());
        match http2::upstream_mux::request_https_buffered(
            target,
            key.as_str(),
            tls_profile,
            Duration::from_secs(security.backend_connect_timeout_secs),
            Duration::from_secs(security.idle_connection_timeout_secs),
            READ_TIMEOUT,
            method,
            final_path.as_bytes(),
            target.host.as_bytes(),
            headers_iter,
            body,
            crate::http_utils::is_idempotent_method(method),
        )
        .await
        {
            Ok(http2::upstream_mux::HttpsBuffered::H2(resp)) => {
                let result = h2_emit_upstream_response(
                    final_path.as_bytes(),
                    target,
                    resp,
                    Some((compression, client_encoding)),
                    security,
                    #[cfg(feature = "wasm")]
                    wasm_modules,
                    resp_tx,
                    notify,
                )
                .await;
                server.release();
                return result;
            }
            Ok(http2::upstream_mux::HttpsBuffered::Http1(conn)) => {
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
            }
            Err(status) => {
                server.release();
                let msg: &[u8] = if status == 504 {
                    b"Gateway Timeout"
                } else {
                    b"Bad Gateway"
                };
                return h2_emit_error(resp_tx, notify, status, msg).await;
            }
        }
    }

    // H1/HTTPS バックエンドへの HTTP/1.1 リクエストを構築。
    let mut request = request_buf_get(1024);
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
    let added = expanded_added_headers(
        security,
        client_ip,
        &ctx.authority,
        std::str::from_utf8(req_path).unwrap_or("/"),
    );
    write_h1_forward_headers(
        &mut request,
        ctx.headers
            .iter()
            .map(|h| (h.name.as_ref(), h.value.as_ref())),
        security,
        &added,
    );
    if !ctx.body.is_empty() {
        request.extend_from_slice(b"Content-Length: ");
        let mut len_buf = itoa::Buffer::new();
        request.extend_from_slice(len_buf.format(ctx.body.len()).as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    request.extend_from_slice(b"Connection: keep-alive\r\n\r\n");
    request.extend_from_slice(&ctx.body);

    let addr = target.conn_addr(); // F-170: UDS 対応（TCP は従来と不変）
    let addr = addr.as_str();

    let result = if target.use_tls {
        h2_proxy_https(
            ctx,
            addr,
            target.sni(),
            request,
            compression,
            client_encoding,
            security,
            upstream_group.tls_profile(),
            resp_tx,
            notify,
        )
        .await
    } else {
        h2_proxy_http(
            ctx,
            addr,
            request,
            compression,
            client_encoding,
            security,
            resp_tx,
            notify,
        )
        .await
    };
    server.release();
    result
}

/// バックエンド接続の EADDRNOTAVAIL 一時的失敗を指数バックオフでリトライして吸収する（B-44 第2段）。
///
/// F-116 のストリーム多重化により、スレッドあたり同時 ~250 ストリームが独立にバックエンド
/// 接続を新規 connect するようになった。プール上限引き上げ（B-44 第1段）後も、
/// コールドスタート/構成切り替え直後の**接続ストーム**（同時 ~1000 ストリームの一斉新規
/// connect）では `EADDRNOTAVAIL` が数秒間バースト的に発生し 502 化する。切り分けの結果、
/// 素の `connect(2)`（Python 実測）では同一条件で再現せず、TIME_WAIT も枯渇水準に達して
/// いない（カーネルのポート枯渇ではない）ため、veil の io_uring CONNECT 経路特有の事象
/// （バックエンド nginx の SYN/accept キュー飽和時の失敗が `EADDRNOTAVAIL` として表面化）
/// と推定している。nginx の `proxy_next_upstream` 相当の一時的失敗リトライとして、
/// `EADDRNOTAVAIL` の場合のみ 10ms → 40ms → 160ms の指数バックオフを挟み最大 4 試行
/// （リトライ 3 回）する（それ以外の connect エラーは従来どおり即時返却）。
///
/// このリトライは **接続失敗時のみ実行されるコールドパス**（ホットパスは初回 connect の
/// 成功経路）であり、追加アロケーションなし・待機は既存の io_uring タイマー
/// （`crate::runtime::timer::sleep`）による非同期待機のため、ホットパス絶対規則
/// （同期待機禁止・不要なアロケーション禁止）に反しない。
///
/// 戻り値: 接続成功で `Ok(TcpStream)`（成功時に `set_nodelay(true)` 済み）。
/// `timeout` 到達時は呼び出し側で 504 に区別できるよう `io::ErrorKind::TimedOut` を返す。
/// それ以外の connect エラー（最終リトライ失敗を含む）はそのまま返す（呼び出し側で 502）。
#[cfg(feature = "http2")]
pub(super) async fn connect_backend_with_retry(addr: &str) -> io::Result<TcpStream> {
    const BACKOFF_MS: [u64; 3] = [10, 40, 160];
    let mut attempt = 0usize;
    loop {
        match timeout(CONNECT_TIMEOUT, TcpStream::connect_str(addr)).await {
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            // `ErrorKind::AddrNotAvailable` は Unix の `EADDRNOTAVAIL` / Windows の
            // `WSAEADDRNOTAVAIL` の両方へ標準ライブラリがマップするため、raw errno 比較
            // より移植性が高い（libc クレートは Windows 向けに errno 定数を定義しない）。
            Ok(Err(e)) if e.kind() == io::ErrorKind::AddrNotAvailable && attempt < 3 => {
                crate::runtime::timer::sleep(Duration::from_millis(BACKOFF_MS[attempt])).await;
                attempt += 1;
            }
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(io::Error::from(io::ErrorKind::TimedOut)),
        }
    }
}

/// ホストごとの新規 connect 並行数の上限（スレッドごと、B-44 第3段）。
#[cfg(feature = "http2")]
pub(super) const MAX_CONCURRENT_CONNECTS_PER_HOST: usize = 64;

/// ホストごとの新規 connect 並行数ゲート（B-44 第3段）。
///
/// 第2段のリトライ（10/40/160ms、最大 4 試行）で 5xx は約 1/3 に減ったが根絶できなかった。
/// h2load の反復切り替え時に in-flight だった ~900 接続がタスクキャンセルで破棄され、
/// 次反復の冒頭で数百規模の connect ストームが再発する。失敗 → 502 → クライアントが即座に
/// 次リクエストを発行、の自己持続でストームが数秒継続し、バックオフ合計 210ms では
/// 吸収できない。そこで **バックエンドへの新規 connect の同時実行数をホストごとに制限**する
/// 構造的修正を行う（Envoy の upstream circuit breaker `max_connections`/pending queue 相当）。
/// 新規 connect は 64/スレッドの波で進み、完了したリクエストの接続が返却され次第、
/// 待機ストリームは再利用側で満たされるため、EADDRNOTAVAIL の発生源
/// （数百規模の一斉 connect）自体が消える。
///
/// ゲートは **プールミス時のみ** 作動するコールドパスであり、プールヒット
/// （ホットパス）はゲートに一切触れない（ホットパス絶対規則に反しない）。
/// thread-per-core 構成のためスレッドローカルでロック不要。
#[cfg(feature = "http2")]
pub(super) struct ConnectGate {
    /// このスレッド上で進行中の新規 connect 数。
    in_flight: std::cell::Cell<usize>,
    /// スロット解放・プール返却を待つ待機者（待機者ごとの `Notify` を積む）。
    ///
    /// 設計原案はゲート共有の単一 `Notify` だったが、`Notify` は waker を 1 つしか
    /// 保持しないため、複数待機者では後着の poll が先着の waker を上書きし、
    /// 起こされない待機者が永久に停止する（lost wakeup）。待機者ごとに `Notify` を
    /// 積み、解放時に全員へブロードキャストする方式とする（キャンセル済み待機者の
    /// エントリが残っていても他の待機者を巻き込まない）。
    waiters: std::cell::RefCell<std::collections::VecDeque<crate::stream_channel::Notify>>,
}

#[cfg(feature = "http2")]
impl ConnectGate {
    fn new() -> Self {
        Self {
            in_flight: std::cell::Cell::new(0),
            waiters: std::cell::RefCell::new(std::collections::VecDeque::new()),
        }
    }

    /// 待機者全員を起こす（ブロードキャスト）。待機者はループ先頭でプール再取得を
    /// 試みるため、新規 connect ではなく返却された接続の再利用が優先される。
    fn notify_waiters(&self) {
        // wake() によるタスク再スケジュールが RefCell の再入借用と衝突しないよう、
        // キューを取り出してから借用を解放して通知する。
        let drained = std::mem::take(&mut *self.waiters.borrow_mut());
        for waiter in drained {
            waiter.notify();
        }
    }
}

/// [`ConnectGate`] の in_flight スロットを保持する RAII ガード（B-44 第3段）。
///
/// connect の成功・失敗・タイムアウト・タスクキャンセル（Future drop）のいずれの経路でも
/// Drop で必ずスロットを解放し、待機者を起こす（解放漏れなし）。
#[cfg(feature = "http2")]
pub(super) struct ConnectPermit {
    gate: std::rc::Rc<ConnectGate>,
}

#[cfg(feature = "http2")]
impl ConnectPermit {
    /// 空きスロットがあれば確保して permit を返す。満杯なら `None`。
    fn try_acquire(gate: &std::rc::Rc<ConnectGate>) -> Option<Self> {
        if gate.in_flight.get() < MAX_CONCURRENT_CONNECTS_PER_HOST {
            gate.in_flight.set(gate.in_flight.get() + 1);
            Some(Self { gate: gate.clone() })
        } else {
            None
        }
    }
}

#[cfg(feature = "http2")]
impl Drop for ConnectPermit {
    fn drop(&mut self) {
        self.gate.in_flight.set(self.gate.in_flight.get() - 1);
        self.gate.notify_waiters();
    }
}

#[cfg(feature = "http2")]
thread_local! {
    /// 接続先ホスト（addr）→ 新規 connect ゲート（B-44 第3段）。
    /// エントリは初回のプールミス時に生成される（コールドパス）。
    static CONNECT_GATES: std::cell::RefCell<
        std::collections::HashMap<String, std::rc::Rc<ConnectGate>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// ホストのゲートを取得する（なければ生成、コールドパス）。
/// `Rc` クローンはゲート取得時のみで、プールヒット経路はここに到達しない。
#[cfg(feature = "http2")]
pub(super) fn connect_gate(host: &str) -> std::rc::Rc<ConnectGate> {
    CONNECT_GATES.with(|g| {
        let mut map = g.borrow_mut();
        if let Some(gate) = map.get(host) {
            gate.clone()
        } else {
            let gate = std::rc::Rc::new(ConnectGate::new());
            map.insert(host.to_string(), gate.clone());
            gate
        }
    })
}

/// プール返却直後にゲート待機者を起こし、再利用の機会を与える（B-44 第3段）。
///
/// ゲート未生成（そのホストで新規 connect の競合が起きていない）または待機者ゼロなら
/// ハッシュ参照のみの no-op。`CONNECT_GATES` の借用は `Rc` クローンの同期区間で解放し、
/// 起こされたタスク側の再入借用と衝突しない。
#[cfg(feature = "http2")]
pub(super) fn notify_connect_gate_waiters(host: &str) {
    let gate = CONNECT_GATES.with(|g| g.borrow().get(host).cloned());
    if let Some(gate) = gate {
        gate.notify_waiters();
    }
}

/// ゲート付きバックエンド接続取得の結果（B-44 第3段）。
#[cfg(feature = "http2")]
pub(super) enum GateAcquire<P> {
    /// ゲート通過待ちの間にプールへ返却された接続を再利用した。
    Pooled(P),
    /// ゲートを通過して新規に確立した接続。
    Fresh(TcpStream),
}

/// プールミス時のバックエンド接続取得（新規 connect 並行数ゲート付き、B-44 第3段）。
///
/// ループ先頭で毎回プール（`pool_get`）を再試行し、ゲート通過待ちの間に返却された
/// 接続があれば新規 connect よりも優先して再利用する（これがストームを構造的に消す本質）。
/// プールミスかつ in_flight < [`MAX_CONCURRENT_CONNECTS_PER_HOST`] ならスロットを確保して
/// [`connect_backend_with_retry`]（第2段のリトライは安全網として維持）へ進み、満杯なら
/// スロット解放またはプール返却の通知を待って再ループする。
///
/// `RefCell` の借用はいずれも同期区間のみで、`.await` を跨いで保持しない。
/// 待機者の `Notify` 確保（`Rc` 1 個）は初回待機時のみで、以降の再待機では再利用する。
#[cfg(feature = "http2")]
pub(super) async fn acquire_backend_conn<P>(
    addr: &str,
    mut pool_get: impl FnMut() -> Option<P>,
) -> io::Result<GateAcquire<P>> {
    let gate = connect_gate(addr);
    let mut waiter: Option<crate::stream_channel::Notify> = None;
    loop {
        // ゲート通過待ちの間に返却された接続の再利用を最優先する
        if let Some(stream) = pool_get() {
            return Ok(GateAcquire::Pooled(stream));
        }
        if let Some(_permit) = ConnectPermit::try_acquire(&gate) {
            // 成功・失敗・キャンセルのすべての経路で permit の Drop がスロットを解放する
            return connect_backend_with_retry(addr)
                .await
                .map(GateAcquire::Fresh);
        }
        // 満杯: スロット解放かプール返却を待つ
        let w = waiter.get_or_insert_with(crate::stream_channel::Notify::new);
        gate.waiters.borrow_mut().push_back(w.clone());
        w.wait().await;
    }
}

/// H1 バックエンド（平文）へのプロキシ（プール再利用付き、B-28）。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn h2_proxy_http(
    _ctx: &H2RequestCtx,
    addr: &str,
    request: Vec<u8>,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    security: &SecurityConfig,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64) {
    // F-177: プールの接続が応答の前に切れていたら、冪等な要求に限り新しい接続で 1 回だけ再送する。
    // 要求バッファは write_all が返すものを使い回す（コピーしない）。
    let idempotent = request_line_is_idempotent(&request);
    let mut request = request;
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let first = attempt == 1;
        let pool_get = || {
            if first {
                HTTP_POOL.with(|p| p.borrow_mut().get(addr))
            } else {
                None
            }
        };
        let (mut backend, from_pool) = match pool_get() {
            Some(stream) => (stream, true),
            None => {
                // プールミス: 新規 connect 並行数ゲート経由で取得（B-44 第3段）
                match acquire_backend_conn(addr, pool_get).await {
                    Ok(GateAcquire::Pooled(stream)) => (stream, true),
                    Ok(GateAcquire::Fresh(stream)) => (stream, false),
                    Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                        request_buf_put(request);
                        return h2_emit_error(resp_tx, notify, 504, b"Gateway Timeout").await;
                    }
                    Err(e) => {
                        warn!("[HTTP/2] Backend connect error: {}", e);
                        request_buf_put(request);
                        return h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
                    }
                }
            }
        };
        let retry = from_pool && idempotent && first;

        let (write_res, returned_request) = backend.write_all(request).await;
        request = returned_request;
        if write_res.is_err() {
            if retry {
                continue;
            }
            request_buf_put(request);
            return h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
        }

        let (status, sent, reusable) = h2_relay_backend_response(
            &mut backend,
            compression,
            client_encoding,
            security,
            resp_tx,
            notify,
            retry,
        )
        .await;
        if status == 0 && retry {
            continue;
        }
        request_buf_put(request);
        if reusable {
            HTTP_POOL.with(|p| {
                p.borrow_mut().put(
                    addr,
                    backend,
                    security.max_idle_connections_per_host,
                    security.idle_connection_timeout_secs,
                )
            });
            // 返却した接続をゲート待機者に再利用させる（B-44 第3段）
            notify_connect_gate_waiters(addr);
        }
        return (status, sent);
    }
}

/// HTTPS バックエンドへのプロキシ（TLS プール再利用付き、B-28）。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn h2_proxy_https(
    _ctx: &H2RequestCtx,
    addr: &str,
    sni: &str,
    request: Vec<u8>,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    security: &SecurityConfig,
    tls_profile: crate::upstream_tls::TlsProfile,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64) {
    let pool_key = crate::http_utils::PoolKeyStr::tls_addr(addr, sni, tls_profile);
    // F-177: プールの接続が応答の前に切れていたら、冪等な要求に限り新しい接続で 1 回だけ再送する。
    let idempotent = request_line_is_idempotent(&request);
    let mut request = request;
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let first = attempt == 1;
        let pool_get = || {
            if first {
                HTTPS_POOL.with(|p| p.borrow_mut().get(pool_key.as_str()))
            } else {
                None
            }
        };
        let (mut backend, from_pool) = match pool_get() {
            Some(stream) => (stream, true),
            None => {
                // プールミス: 新規 connect 並行数ゲート経由で取得（B-44 第3段）
                let acquired = match acquire_backend_conn(addr, pool_get).await {
                    Ok(acquired) => acquired,
                    Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                        request_buf_put(request);
                        return h2_emit_error(resp_tx, notify, 504, b"Gateway Timeout").await;
                    }
                    Err(e) => {
                        warn!("[HTTP/2] Backend connect error: {}", e);
                        request_buf_put(request);
                        return h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
                    }
                };
                match acquired {
                    GateAcquire::Pooled(stream) => (stream, true),
                    GateAcquire::Fresh(backend_tcp) => {
                        let tls_result = {
                            let connector = crate::config::tls_connector(tls_profile);
                            timeout(CONNECT_TIMEOUT, connector.connect(backend_tcp, sni)).await
                        };
                        match tls_result {
                            Ok(Ok(stream)) => (stream, false),
                            Ok(Err(e)) => {
                                warn!("[HTTP/2] TLS handshake error: {}", e);
                                request_buf_put(request);
                                return h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
                            }
                            Err(_) => {
                                request_buf_put(request);
                                return h2_emit_error(resp_tx, notify, 504, b"Gateway Timeout")
                                    .await;
                            }
                        }
                    }
                }
            }
        };
        let retry = from_pool && idempotent && first;

        let (write_res, returned_request) = backend.write_all(request).await;
        request = returned_request;
        if write_res.is_err() {
            if retry {
                continue;
            }
            request_buf_put(request);
            return h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
        }

        let (status, sent, reusable) = h2_relay_backend_response(
            &mut backend,
            compression,
            client_encoding,
            security,
            resp_tx,
            notify,
            retry,
        )
        .await;
        if status == 0 && retry {
            continue;
        }
        request_buf_put(request);
        if reusable {
            HTTPS_POOL.with(|p| {
                p.borrow_mut().put(
                    pool_key.as_str(),
                    backend,
                    security.max_idle_connections_per_host,
                    security.idle_connection_timeout_secs,
                )
            });
            // 返却した接続をゲート待機者に再利用させる（ゲートは addr 単位、B-44 第3段）
            notify_connect_gate_waiters(addr);
        }
        return (status, sent);
    }
}

/// H2C バックエンドへのプロキシ（F-106 プール再利用 + gRPC トレイラー）。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn h2_proxy_h2c(
    _ctx: &H2RequestCtx,
    addr: &str,
    target: &ProxyTarget,
    method: &[u8],
    path: &[u8],
    security: &SecurityConfig,
    #[cfg(feature = "wasm")] wasm_modules: &Arc<Vec<crate::wasm_plugin_config::ModuleRef>>,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64) {
    let ctx = _ctx;
    let is_grpc_upstream = ctx
        .headers
        .iter()
        .any(|h| header_pair_is_grpc(&h.name, &h.value));
    // F-166/F-165(A2): フィルタ済みイテレータを直接渡す（中間 `Vec` の `collect()` を回避）。
    // ホップバイホップ・`te`（gRPC 以外）の除外は上流クライアント側（F-174）でも行う。
    let added = expanded_added_headers(
        security,
        &ctx.client_ip,
        &ctx.authority,
        std::str::from_utf8(&ctx.path).unwrap_or("/"),
    );
    let headers_iter = ctx
        .headers
        .iter()
        .filter(|h| is_grpc_upstream || !h.name.eq_ignore_ascii_case(b"te"))
        .filter(|h| forward_allowed(security, &h.name))
        .map(|h| (h.name.as_ref(), h.value.as_ref()))
        .chain(added.iter().map(|(n, v)| (n.as_slice(), v.as_slice())));

    // F-174: 多重化した上流 HTTP/2 接続（ワーカーごとのプール）でストリームを開く。
    let key = crate::http_utils::PoolKeyStr::plain_addr(addr);
    let body = (!ctx.body.is_empty()).then(|| ctx.body.clone());
    let h2c_resp = match http2::upstream_mux::request_h2c_buffered(
        target,
        key.as_str(),
        Duration::from_secs(security.backend_connect_timeout_secs),
        Duration::from_secs(security.idle_connection_timeout_secs),
        READ_TIMEOUT,
        method,
        path,
        target.host.as_bytes(),
        headers_iter,
        body,
        crate::http_utils::is_idempotent_method(method),
    )
    .await
    {
        Ok(resp) => resp,
        Err(status) => {
            let msg: &[u8] = if status == 504 {
                b"Gateway Timeout"
            } else {
                b"Bad Gateway"
            };
            return h2_emit_error(resp_tx, notify, status, msg).await;
        }
    };
    h2_emit_upstream_response(
        path,
        target,
        h2c_resp,
        None,
        security,
        #[cfg(feature = "wasm")]
        wasm_modules,
        resp_tx,
        notify,
    )
    .await
}

/// 上流 HTTP/2 からバッファ型で受けた応答を下流（HTTP/2）へ送る（h2c / TLS 上の h2 共通）。
///
/// `compress` が `Some` なら、gRPC 以外の応答に圧縮ネゴシエーションを適用する（F-175: HTTPS
/// 上流が HTTP/2 になっても HTTP/1.1 経路と同じく圧縮する）。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn h2_emit_upstream_response(
    path: &[u8],
    target: &ProxyTarget,
    h2c_resp: http2::upstream_mux::H2cResponse,
    compress: Option<(&CompressionConfig, AcceptedEncoding)>,
    security: &SecurityConfig,
    #[cfg(feature = "wasm")] wasm_modules: &Arc<Vec<crate::wasm_plugin_config::ModuleRef>>,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64) {
    // path / target は gRPC メトリクスにだけ使う。
    #[cfg(not(feature = "grpc"))]
    let _ = (path, target);
    let mut h2c_resp = h2c_resp;
    // F-166(B-2)/F-165(A3/A4): バックエンド応答の `Bytes` ヘッダをそのままムーブする
    // （従来の `(k.clone(), v.clone())` ディープコピーを排除）。
    let mut header_store: Vec<(Bytes, Bytes)> = std::mem::take(&mut h2c_resp.headers);
    #[cfg(feature = "wasm")]
    {
        header_store =
            apply_h2_wasm_response_headers(wasm_modules, h2c_resp.status, header_store).await;
    }
    if let Some((compression, client_encoding)) = compress {
        let header = |name: &[u8]| {
            header_store
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        };
        let content_type = header(b"content-type");
        let is_grpc = content_type
            .as_deref()
            .is_some_and(|ct| ct.starts_with(b"application/grpc"));
        let should = if is_grpc || h2c_resp.body.is_empty() {
            None
        } else {
            compression.should_compress(
                client_encoding,
                content_type.as_deref(),
                Some(h2c_resp.body.len()),
                header(b"content-encoding").as_deref(),
            )
        };
        if let Some(enc) = should {
            h2c_resp.body = Bytes::from(compress_body_h2(&h2c_resp.body, enc, compression));
            header_store.retain(|(n, _)| {
                !n.eq_ignore_ascii_case(b"content-length")
                    && !n.eq_ignore_ascii_case(b"content-encoding")
            });
            header_store.push((
                Bytes::from_static(b"content-encoding"),
                Bytes::from_static(enc.as_header_value()),
            ));
            header_store.push((
                Bytes::from_static(b"vary"),
                Bytes::from_static(b"Accept-Encoding"),
            ));
        }
    }
    for (n, v) in h2_base_headers(true) {
        header_store.push((n, v));
    }
    // `[route.traffic.cors]` の実際の応答のヘッダ（F-273。CORS の無いルートは分岐だけ）
    crate::route_cors::apply_bytes(&mut header_store, security);

    let has_body = !h2c_resp.body.is_empty();
    let has_trailers = !h2c_resp.trailers.is_empty();
    let status = h2c_resp.status;
    let body_len = h2c_resp.body.len() as u64;

    if h2_send(
        resp_tx,
        notify,
        H2RespMsg::Head {
            status,
            headers: header_store,
            end_stream: !has_body && !has_trailers,
        },
    )
    .await
    .is_err()
    {
        return (status, 0);
    }

    if has_body
        && h2_send(resp_tx, notify, H2RespMsg::Body(h2c_resp.body))
            .await
            .is_err()
    {
        return (status, body_len);
    }

    if has_trailers {
        #[cfg(feature = "grpc")]
        {
            // gRPC トレイラー WASM フィルタ（F-133）: `grpc-status`/`grpc-message` の
            // 書き換えを許可する。`wasm_modules` が空なら早期 return（コスト増なし）。
            #[cfg(feature = "wasm")]
            let trailers = apply_h2_wasm_response_trailers(wasm_modules, h2c_resp.trailers).await;
            #[cfg(not(feature = "wasm"))]
            let trailers = h2c_resp.trailers;
            let mut grpc_status = 0u32;
            for (name, value) in &trailers {
                if name.as_ref() == b"grpc-status" as &[u8] {
                    if let Ok(s) = std::str::from_utf8(value) {
                        grpc_status = s.trim().parse().unwrap_or(0);
                    }
                }
            }
            // F-09: gRPC リクエストメトリクスを記録。
            let grpc_method = std::str::from_utf8(path).unwrap_or("");
            let mut status_buf = itoa::Buffer::new();
            let status_str = status_buf.format(grpc_status);
            crate::metrics::record_grpc_request(grpc_method, status_str, &target.host);
            let _ = h2_send(resp_tx, notify, H2RespMsg::Trailers(trailers)).await;
        }
        #[cfg(not(feature = "grpc"))]
        {
            // gRPC feature 無効時はトレイラーをスキップ。
            let _ = &h2c_resp.trailers;
        }
    }

    (status, body_len)
}

/// バックエンド HTTP/1.1 レスポンスを受信して [`H2RespMsg`] としてメインループへ流す（F-116）。
///
/// 戻り値 `(status, sent, reusable)`。`reusable` はバックエンド接続をプールへ返せるか
/// （CL 全量消費 + 非 `Connection: close`。chunked/EOF/エラーは false）。
///
/// F-177: `retry_on_empty` が `true` のとき、応答の 1 バイト目より前に EOF / エラーになったら
/// 下流へエラーを送らずに `(0, 0, false)` を返す（呼び出し側が新しい接続で 1 回だけ再送する）。
#[cfg(feature = "http2")]
pub(super) async fn h2_relay_backend_response<B>(
    backend: &mut B,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    security: &SecurityConfig,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
    retry_on_empty: bool,
) -> (u16, u64, bool)
where
    B: crate::runtime::io::AsyncReadRent + Unpin,
{
    // プールバッファを最後まで保持し、確実に返却する RAII ラッパー。
    // ヘッダーが 1 回目の read で完結した場合（大半のケース）は、このバッファを
    // そのままヘッダー/ボディの参照元として使い続け、`Vec` への確保・コピーを避ける。
    struct PooledBuf(Option<SafeReadBuffer>);
    impl PooledBuf {
        #[inline(always)]
        fn as_slice(&self) -> &[u8] {
            self.0
                .as_ref()
                .expect("PooledBuf holds a buffer until drop")
                .as_valid_slice()
        }

        /// バッファを消費し、有効データのみへ truncate した `Vec<u8>` を取り出す。
        ///
        /// プールへは返却しない（呼び出し側が `Bytes::from` で所有権を引き取り、
        /// ボディをゼロコピーで配る＝F-165 R4）。内部で `Option` を `take()` する
        /// ため、その後 `self` が drop されても `Drop::drop` は何もしない
        /// （`buf_put` は呼ばれない）。
        #[inline(always)]
        fn into_vec(mut self) -> Vec<u8> {
            self.0
                .take()
                .expect("PooledBuf holds a buffer until into_vec")
                .into_truncated()
        }
    }
    impl Drop for PooledBuf {
        fn drop(&mut self) {
            if let Some(b) = self.0.take() {
                buf_put(b);
            }
        }
    }

    // レスポンスデータの所有元。ヘッダーが複数回の read にまたがる稀なケースのみ
    // `Heap` に蓄積する（従来どおりの挙動）。
    enum RespData {
        Pooled(PooledBuf),
        Heap(Vec<u8>),
    }
    impl RespData {
        #[inline(always)]
        fn as_slice(&self) -> &[u8] {
            match self {
                RespData::Pooled(b) => b.as_slice(),
                RespData::Heap(v) => v.as_slice(),
            }
        }

        /// `self` を消費し、`body_start` 以降をボディとして `Bytes` で取り出す
        /// （F-165 R4）。`Bytes::from(Vec<u8>)` はヒープ確保をそのまま引き継ぐ
        /// ゼロコピー変換で、続く `.slice()` も参照カウント共有のみで memcpy
        /// しない。`RespData::Pooled` の場合、内部バッファはプール
        /// （`BUF_POOL`）へは返却されず、最後の `Bytes` 参照が落ちたときに
        /// 解放される（ヘッダー分のコピー削減と引き換えに、この 1 バッファ分は
        /// プールへ戻らない）。
        #[inline(always)]
        fn into_body_bytes(self, body_start: usize) -> Bytes {
            let full: Bytes = match self {
                RespData::Pooled(b) => Bytes::from(b.into_vec()),
                RespData::Heap(v) => Bytes::from(v),
            };
            full.slice(body_start..)
        }
    }

    // 1 回目の read はプールバッファへ直接行い、コピーせずその場でパースを試みる。
    // このパース結果をそのまま持ち回り、後段で同じヘッダーを二重に
    // parse_http_response し直さない（ヘッダーが 1 回で揃う大半のケースでの
    // 解析回数を「据え置き」、コピー削減分を無駄にしない）。
    let (data, parsed) = {
        let buf = buf_get();
        let (res, mut returned_buf) = match timeout(READ_TIMEOUT, backend.read(buf)).await {
            Ok(r) => r,
            Err(_) => {
                let (s, sz) = h2_emit_error(resp_tx, notify, 504, b"Gateway Timeout").await;
                return (s, sz, false);
            }
        };
        let n = match res {
            Ok(n) if n > 0 => n,
            _ => {
                buf_put(returned_buf);
                if retry_on_empty {
                    return (0, 0, false);
                }
                let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
                return (s, sz, false);
            }
        };
        returned_buf.set_valid_len(n);

        match parse_http_response(returned_buf.as_valid_slice()) {
            Some(parsed) => (RespData::Pooled(PooledBuf(Some(returned_buf))), parsed),
            None => {
                // ヘッダーがこの read だけでは完結しなかった（稀）:
                // 従来どおり Vec へ蓄積し、揃うまで追加の read を繰り返す。
                let mut heap = Vec::with_capacity(BUF_SIZE);
                heap.extend_from_slice(returned_buf.as_valid_slice());
                buf_put(returned_buf);

                let parsed = loop {
                    if let Some(parsed) = parse_http_response(&heap) {
                        break parsed;
                    }
                    if heap.len() > MAX_HEADER_SIZE {
                        let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
                        return (s, sz, false);
                    }

                    let buf = buf_get();
                    let read_result = timeout(READ_TIMEOUT, backend.read(buf)).await;
                    let (res, mut returned_buf) = match read_result {
                        Ok(r) => r,
                        Err(_) => {
                            let (s, sz) =
                                h2_emit_error(resp_tx, notify, 504, b"Gateway Timeout").await;
                            return (s, sz, false);
                        }
                    };
                    let n = match res {
                        Ok(0) => {
                            buf_put(returned_buf);
                            let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
                            return (s, sz, false);
                        }
                        Ok(n) => n,
                        Err(_) => {
                            buf_put(returned_buf);
                            let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
                            return (s, sz, false);
                        }
                    };
                    returned_buf.set_valid_len(n);
                    heap.extend_from_slice(returned_buf.as_valid_slice());
                    buf_put(returned_buf);
                };

                (RespData::Heap(heap), parsed)
            }
        }
    };

    {
        let status = parsed.status_code;
        let body_start = parsed.header_len;
        let response_buf = data.as_slice();
        let body = &response_buf[body_start..];

        let mut headers_storage = [httparse::EMPTY_HEADER; 64];
        let mut resp = httparse::Response::new(&mut headers_storage);
        let _ = resp.parse(response_buf);
        let content_type = resp
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("content-type"))
            .map(|h| h.value);
        let existing_encoding = resp
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("content-encoding"))
            .map(|h| h.value);

        let stream_compress_hint = compression.should_compress(
            client_encoding,
            content_type,
            parsed.content_length,
            existing_encoding,
        );

        // 非圧縮 + CL 既知 + 非 chunked → 逐次ストリーミング。
        if stream_compress_hint.is_none() && !parsed.is_chunked {
            if let Some(content_len) = parsed.content_length {
                let mut headers = h2_base_headers(true);
                for header in resp.headers.iter() {
                    if header.name.is_empty() {
                        continue;
                    }
                    if header.name.eq_ignore_ascii_case("connection")
                        || header.name.eq_ignore_ascii_case("keep-alive")
                        || header.name.eq_ignore_ascii_case("transfer-encoding")
                        || header.name.eq_ignore_ascii_case("upgrade")
                    {
                        continue;
                    }
                    headers.push((
                        Bytes::copy_from_slice(header.name.as_bytes()),
                        Bytes::copy_from_slice(header.value),
                    ));
                }
                // ヘッダー抽出（上のループ）が済んだ時点で `data`（= `response_buf`/
                // `body` の借用元）はもう参照されないため、ここで `Bytes` へ消費
                // 変換してゼロコピーでボディを渡す（F-165 R4）。
                // `[route.traffic.cors]` の実際の応答のヘッダ（F-273。CORS の無いルートは分岐だけ）
                crate::route_cors::apply_bytes(&mut headers, security);
                let initial_body = data.into_body_bytes(body_start);
                let (sent, ok) = h2_stream_body_cl(
                    resp_tx,
                    notify,
                    status,
                    headers,
                    backend,
                    initial_body,
                    content_len,
                )
                .await;
                let reusable = ok && sent == content_len as u64 && !parsed.is_connection_close;
                return (status, sent, reusable);
            }
        }

        // 非圧縮 + chunked → ゼロコピー逐次デコード転送。
        if stream_compress_hint.is_none() && parsed.is_chunked {
            let mut headers = h2_base_headers(true);
            for header in resp.headers.iter() {
                if header.name.is_empty() {
                    continue;
                }
                if header.name.eq_ignore_ascii_case("connection")
                    || header.name.eq_ignore_ascii_case("keep-alive")
                    || header.name.eq_ignore_ascii_case("transfer-encoding")
                    || header.name.eq_ignore_ascii_case("upgrade")
                    || header.name.eq_ignore_ascii_case("content-length")
                {
                    continue;
                }
                headers.push((
                    Bytes::copy_from_slice(header.name.as_bytes()),
                    Bytes::copy_from_slice(header.value),
                ));
            }
            // 同上（F-165 R4）: ヘッダー抽出後に `data` を `Bytes` へ消費変換する。
            // `[route.traffic.cors]` の実際の応答のヘッダ（F-273）
            crate::route_cors::apply_bytes(&mut headers, security);
            let initial_body = data.into_body_bytes(body_start);
            let sent =
                h2_stream_body_chunked(resp_tx, notify, status, headers, backend, initial_body)
                    .await;
            return (status, sent, false);
        }

        // 圧縮あり / 長さ不明 → 全読み込み後に（必要なら圧縮して）送信。
        let mut backend_reusable = false;
        let final_body = if parsed.is_chunked {
            let mut decoder = ChunkedDecoder::new_unlimited();
            let mut full_body = body.to_vec();
            decoder.feed(body);
            while !decoder.is_complete() {
                let buf = buf_get();
                let (res, mut returned_buf) = match timeout(READ_TIMEOUT, backend.read(buf)).await {
                    Ok(r) => r,
                    Err(_) => break,
                };
                let n = match res {
                    Ok(0) => {
                        buf_put(returned_buf);
                        break;
                    }
                    Ok(n) => n,
                    Err(_) => {
                        buf_put(returned_buf);
                        break;
                    }
                };
                returned_buf.set_valid_len(n);
                full_body.extend_from_slice(returned_buf.as_valid_slice());
                decoder.feed(returned_buf.as_valid_slice());
                buf_put(returned_buf);
            }
            decode_chunked_body(&full_body)
        } else if let Some(content_len) = parsed.content_length {
            let mut full_body = body.to_vec();
            while full_body.len() < content_len {
                let buf = buf_get();
                let (res, mut returned_buf) = match timeout(READ_TIMEOUT, backend.read(buf)).await {
                    Ok(r) => r,
                    Err(_) => break,
                };
                let n = match res {
                    Ok(0) => {
                        buf_put(returned_buf);
                        break;
                    }
                    Ok(n) => n,
                    Err(_) => {
                        buf_put(returned_buf);
                        break;
                    }
                };
                returned_buf.set_valid_len(n);
                full_body.extend_from_slice(returned_buf.as_valid_slice());
                buf_put(returned_buf);
            }
            backend_reusable = full_body.len() == content_len;
            full_body
        } else {
            body.to_vec()
        };

        let should_compress = compression.should_compress(
            client_encoding,
            content_type,
            Some(final_body.len()),
            existing_encoding,
        );

        let mut headers = h2_base_headers(true);
        if let Some(enc) = should_compress {
            let encoding_name: &'static [u8] = match enc {
                AcceptedEncoding::Zstd => b"zstd",
                AcceptedEncoding::Brotli => b"br",
                AcceptedEncoding::Gzip => b"gzip",
                AcceptedEncoding::Deflate => b"deflate",
                AcceptedEncoding::Identity => b"",
            };
            if !encoding_name.is_empty() {
                headers.push((
                    Bytes::from_static(b"content-encoding"),
                    Bytes::from_static(encoding_name),
                ));
                headers.push((
                    Bytes::from_static(b"vary"),
                    Bytes::from_static(b"Accept-Encoding"),
                ));
            }
        }
        for header in resp.headers.iter() {
            if header.name.is_empty() {
                continue;
            }
            if header.name.eq_ignore_ascii_case("connection")
                || header.name.eq_ignore_ascii_case("keep-alive")
                || header.name.eq_ignore_ascii_case("transfer-encoding")
                || header.name.eq_ignore_ascii_case("upgrade")
            {
                continue;
            }
            if should_compress.is_some()
                && (header.name.eq_ignore_ascii_case("content-length")
                    || header.name.eq_ignore_ascii_case("content-encoding"))
            {
                continue;
            }
            // 応答ヘッダはパース元バッファ（プールへ返却される）を参照しているため、
            // `Bytes::copy_from_slice` による 1 回のコピーが必要（従来の `to_vec()` と同等）。
            headers.push((
                Bytes::copy_from_slice(header.name.as_bytes()),
                Bytes::copy_from_slice(header.value),
            ));
        }

        let response_body = if let Some(enc) = should_compress {
            compress_body_h2(&final_body, enc, compression)
        } else {
            final_body
        };
        // `[route.traffic.cors]` の実際の応答のヘッダ（F-273）
        crate::route_cors::apply_bytes(&mut headers, security);
        let (status2, sent) =
            h2_emit_full(resp_tx, notify, status, headers, Bytes::from(response_body)).await;
        (
            status2,
            sent,
            backend_reusable && !parsed.is_connection_close,
        )
    }
}

/// 非圧縮・CL 既知ボディを [`H2RespMsg::Body`] として逐次転送する。戻り値 `(sent, ok)`。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn h2_stream_body_cl<B>(
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
    status: u16,
    headers: Vec<(Bytes, Bytes)>,
    backend: &mut B,
    initial_body: Bytes,
    content_length: usize,
) -> (u64, bool)
where
    B: crate::runtime::io::AsyncReadRent + Unpin,
{
    let empty = content_length == 0;
    if h2_send(
        resp_tx,
        notify,
        H2RespMsg::Head {
            status,
            headers,
            end_stream: empty,
        },
    )
    .await
    .is_err()
    {
        return (0, false);
    }
    if empty {
        return (0, true);
    }

    let mut remaining = content_length;
    let init_len = initial_body.len().min(remaining);
    if init_len > 0 {
        // ゼロコピー: `initial_body` は `RespData::into_body_bytes` が
        // `Bytes::from(Vec<u8>)` で引き継いだ所有データなので、`.slice()`
        // （参照カウント共有）で送るだけで memcpy しない（F-165 R4）。
        if h2_send(
            resp_tx,
            notify,
            H2RespMsg::Body(initial_body.slice(0..init_len)),
        )
        .await
        .is_err()
        {
            return ((content_length - remaining) as u64, false);
        }
        remaining -= init_len;
    }

    while remaining > 0 {
        let buf = body_buf_get();
        let (res, mut returned_buf) = match timeout(READ_TIMEOUT, backend.read(buf)).await {
            Ok(r) => r,
            Err(_) => {
                // タイムアウト: HEADERS 送出済みのため RST_STREAM で打ち切る（旧 send_rst_stream 相当）。
                let _ = h2_send(resp_tx, notify, H2RespMsg::Reset(2)).await;
                return ((content_length - remaining) as u64, false);
            }
        };
        let n = match res {
            Ok(0) => {
                // content-length 未達でバックエンド切断: 空 END_STREAM で閉じる（graceful）。
                body_buf_put(returned_buf);
                return ((content_length - remaining) as u64, false);
            }
            Ok(n) => n,
            Err(_) => {
                body_buf_put(returned_buf);
                let _ = h2_send(resp_tx, notify, H2RespMsg::Reset(2)).await;
                return ((content_length - remaining) as u64, false);
            }
        };
        let take = n.min(remaining);
        // 読み込んだ n バイトを丸ごとゼロコピーで切り出す（split_to().freeze()）。
        // content-length を超えて読めた場合（take < n、稀）のみさらに
        // `.slice()` で先頭 take バイトへ絞る（これも参照カウント複製のみ）。
        let frozen = returned_buf.split_to(n).freeze();
        let chunk = if take == n {
            frozen
        } else {
            frozen.slice(0..take)
        };
        let send_ok = h2_send(resp_tx, notify, H2RespMsg::Body(chunk))
            .await
            .is_ok();
        // `chunk` は上の送出で消費済み。`returned_buf`（残り部分）は同じ確保を
        // 参照カウントで共有しているだけの別オブジェクトなので、送出後に
        // ここで初めてプールへ返却を試みる（body_buf_put が一意所有かどうかを
        // 判定し、まだ他所から参照されていればプールへは戻さず drop する）。
        body_buf_put(returned_buf);
        if !send_ok {
            return ((content_length - remaining) as u64, false);
        }
        remaining -= take;
    }
    (content_length as u64, true)
}

/// 非圧縮・chunked ボディを `next_data_span` でデコードしつつ逐次転送する。戻り値 `sent`。
#[cfg(feature = "http2")]
pub(super) async fn h2_stream_body_chunked<B>(
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
    status: u16,
    headers: Vec<(Bytes, Bytes)>,
    backend: &mut B,
    initial_body: Bytes,
) -> u64
where
    B: crate::runtime::io::AsyncReadRent + Unpin,
{
    if h2_send(
        resp_tx,
        notify,
        H2RespMsg::Head {
            status,
            headers,
            end_stream: false,
        },
    )
    .await
    .is_err()
    {
        return 0;
    }

    let mut decoder = crate::http_utils::ChunkedDecoder::new_unlimited();
    let mut sent: u64 = 0;

    match h2_drain_chunked_to_msg(resp_tx, notify, &mut decoder, &initial_body, &mut sent).await {
        H2ChunkDrain::Done => return sent,
        H2ChunkDrain::NeedMore => {}
    }

    loop {
        let buf = body_buf_get();
        let (res, mut returned_buf) = match timeout(READ_TIMEOUT, backend.read(buf)).await {
            Ok(r) => r,
            Err(_) => {
                let _ = h2_send(resp_tx, notify, H2RespMsg::Reset(2)).await;
                return sent;
            }
        };
        let n = match res {
            Ok(0) => {
                // 終端チャンク前に切断: 空 END_STREAM で閉じる（graceful）。
                body_buf_put(returned_buf);
                return sent;
            }
            Ok(n) => n,
            Err(_) => {
                body_buf_put(returned_buf);
                let _ = h2_send(resp_tx, notify, H2RespMsg::Reset(2)).await;
                return sent;
            }
        };
        // 読み込んだ n バイトをゼロコピーで切り出す（split_to().freeze()）。
        let frozen = returned_buf.split_to(n).freeze();
        let drain =
            h2_drain_chunked_to_msg(resp_tx, notify, &mut decoder, &frozen, &mut sent).await;
        drop(frozen);
        // 送出（内部で複数チャンクへ分割されうる）が終わった後にプールへの
        // 返却を試みる（h2_stream_body_cl と同じ理由。body_buf_put 参照）。
        body_buf_put(returned_buf);
        match drain {
            H2ChunkDrain::Done => return sent,
            H2ChunkDrain::NeedMore => {}
        }
    }
}

/// chunked デコード 1 バッファ分の結果。
#[cfg(feature = "http2")]
pub(super) enum H2ChunkDrain {
    NeedMore,
    Done,
}

/// 1 入力バッファ分の chunked をデコードして [`H2RespMsg::Body`] として送出する。
#[cfg(feature = "http2")]
pub(super) async fn h2_drain_chunked_to_msg(
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
    decoder: &mut crate::http_utils::ChunkedDecoder,
    data: &Bytes,
    sent: &mut u64,
) -> H2ChunkDrain {
    let mut pos = 0;
    while pos < data.len() {
        let span = decoder.next_data_span(&data[pos..]);
        if span.data_len > 0 {
            let start = pos + span.data_start;
            // ゼロコピー: `data` は呼び出し側が所有する `Bytes`（プール由来の
            // `BytesMut::split_to().freeze()` または初回ヘッダー読み込みバッファ）
            // への参照カウント共有スライス（F-165 R4、以前は毎回 `Bytes::copy_from_slice`）。
            let chunk = data.slice(start..start + span.data_len);
            if h2_send(resp_tx, notify, H2RespMsg::Body(chunk))
                .await
                .is_err()
            {
                return H2ChunkDrain::Done;
            }
            *sent += span.data_len as u64;
        }
        pos += span.consumed;
        if span.complete || span.limit_exceeded {
            return H2ChunkDrain::Done;
        }
        if span.consumed == 0 {
            break;
        }
    }
    H2ChunkDrain::NeedMore
}

/// SendFile バックエンドのファイル配信をメッセージ化する（F-116）。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn h2_sendfile(
    ctx: &H2RequestCtx,
    base_path: &Path,
    is_dir: bool,
    index_file: Option<&str>,
    prefix: &[u8],
    security: &SecurityConfig,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    open_file_cache_config: Option<&cache::OpenFileCacheConfig>,
    canonical_base: Option<&Path>,
    static_file_cache_config: Option<&cache::StaticContentCacheRouteConfig>,
    #[cfg(feature = "wasm")] wasm_modules: &Arc<Vec<crate::wasm_plugin_config::ModuleRef>>,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64) {
    let path_str_raw = std::str::from_utf8(&ctx.path).unwrap_or("/");
    let path_str = if let Some(qpos) = path_str_raw.find('?') {
        &path_str_raw[..qpos]
    } else {
        path_str_raw
    };
    let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
    let remainder: &str = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
        &path_str[prefix_str.len()..]
    } else {
        path_str
    };

    if remainder.contains("..") {
        return h2_emit_error(resp_tx, notify, 403, b"Forbidden").await;
    }

    let full_path = if is_dir {
        let sub_path = remainder.trim_start_matches('/');
        let mut p = base_path.to_path_buf();
        if !sub_path.is_empty() {
            p.push(sub_path);
        }
        p
    } else {
        let clean_remainder = remainder.trim_matches('/');
        if !clean_remainder.is_empty() {
            return h2_emit_error(resp_tx, notify, 404, b"Not Found").await;
        }
        base_path.to_path_buf()
    };

    let content_cfg = cache::effective_static_content_cache_config(static_file_cache_config);

    // B-65（続き）: 従来は「メタデータ解決（offload 1）→ …検査… → 本体読み込み
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
    // `readlink` 等の事後検査は不要（`static_file.rs` モジュール doc 参照）。
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

    // F-169: 圧縮結果キャッシュのキーに使う「実際に配信するパス」（ディレクトリ
    // ルートで index ファイルへフォールバックした場合は index 側のパス）を
    // `full_path` の所有権をそのまま流用して求める（参照渡しの検索は既に完了して
    // いるため `full_path` 自体はこの後不要 = 追加のクローンなしで済む）。
    let (mime_type, data, served_path) = match first_result {
        Some(cache::StaticFileOutcome::File(info, data)) => (info.mime_type, data, full_path),
        Some(cache::StaticFileOutcome::Forbidden) => {
            return h2_emit_error(resp_tx, notify, 403, b"Forbidden").await;
        }
        Some(cache::StaticFileOutcome::Directory(file_info)) => {
            // ディレクトリの場合はインデックスファイルを解決してからもう一度呼ぶ
            // （封じ込め検査は既に上で通過済みのため index パスにも同じ containment
            // を渡す）。
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
                    (info.mime_type, data, index_path)
                }
                Some(cache::StaticFileOutcome::Forbidden) | None => {
                    return h2_emit_error(resp_tx, notify, 403, b"Forbidden").await;
                }
                Some(cache::StaticFileOutcome::Directory(_)) => {
                    // index ファイル自体がさらにディレクトリ（通常起こり得ないが、
                    // 安全側に倒して 403 とする）。
                    return h2_emit_error(resp_tx, notify, 403, b"Forbidden").await;
                }
            }
        }
        None => {
            // ファイルが開けない場合はキャッシュを無効化（HTTP/1.1 と同様）。
            cache::invalidate_file_cache(&full_path);
            cache::invalidate_content_cache(&full_path);
            return h2_emit_error(resp_tx, notify, 404, b"Not Found").await;
        }
    };

    let (built_headers, response_body) = build_h2_compressed_file_response(
        data,
        &mime_type,
        security,
        compression,
        client_encoding,
        Some((served_path.as_path(), &content_cfg)),
    );
    #[cfg(feature = "wasm")]
    let header_store = apply_h2_wasm_response_headers(wasm_modules, 200, built_headers).await;
    #[cfg(not(feature = "wasm"))]
    let header_store = built_headers;
    h2_emit_full(resp_tx, notify, 200, header_store, response_body).await
}

/// リダイレクトレスポンスをメッセージ化する（F-116）。
#[cfg(feature = "http2")]
pub(super) async fn h2_redirect(
    ctx: &H2RequestCtx,
    redirect_url: &str,
    status_code: u16,
    preserve_path: bool,
    prefix: &[u8],
    security: &SecurityConfig,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64) {
    let path_str = std::str::from_utf8(&ctx.path).unwrap_or("/");
    let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
    let sub_path = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
        &path_str[prefix_str.len()..]
    } else {
        path_str
    };
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

    let mut headers = h2_base_headers(false);
    headers.push((
        Bytes::from_static(b"location"),
        Bytes::from(final_url.into_bytes()),
    ));
    // `[route.traffic.cors]` の実際の応答のヘッダ（F-273）
    crate::route_cors::apply_bytes(&mut headers, security);
    if h2_send(
        resp_tx,
        notify,
        H2RespMsg::Head {
            status: status_code,
            headers,
            end_stream: true,
        },
    )
    .await
    .is_err()
    {
        return (status_code, 0);
    }
    (status_code, 0)
}

/// [`h2_relay_h2_streaming`] の結果。
#[cfg(feature = "http2")]
pub(super) enum H2Relay {
    /// 中継した（`(status, resp_size, req_size)`）。
    Done((u16, u64, u64)),
    /// HTTPS 上流が ALPN で HTTP/1.1 を選んだ（F-175）。ネゴシエーションに使った接続があれば渡す。
    Http1(Option<Box<ClientTls>>),
}

/// F-171 / F-175: HTTP/2 で受けた要求を上流 HTTP/2（h2c、または TLS 上の h2）の多重化接続の
/// ストリーム（F-174）へ全二重に中継する。
///
/// 下流の DATA は届いた順に上流ストリームへ、上流の HEADERS / DATA / トレーラーは届いた順に
/// 下流へ流す（双方向ストリーミング gRPC が成立する）。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn h2_relay_h2_streaming(
    ctx: &H2RequestCtx,
    req_rx: &crate::stream_channel::Receiver<Bytes>,
    target: &ProxyTarget,
    tls_profile: crate::upstream_tls::TlsProfile,
    prefix: &[u8],
    security: &SecurityConfig,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> H2Relay {
    use crate::http2::upstream_mux::{HttpsOpen, UpResp};
    let method = &ctx.method[..];
    let is_grpc = ctx
        .headers
        .iter()
        .any(|h| header_pair_is_grpc(&h.name, &h.value));
    let path_str = std::str::from_utf8(&ctx.path).unwrap_or("/");
    let final_path = compute_upstream_path(path_str, prefix, &target.path_prefix, is_grpc);
    let addr = target.conn_addr();
    let added = expanded_added_headers(security, &ctx.client_ip, &ctx.authority, path_str);
    let headers_iter = ctx
        .headers
        .iter()
        .filter(|h| is_grpc || !h.name.eq_ignore_ascii_case(b"te"))
        .filter(|h| forward_allowed(security, &h.name))
        .map(|h| (h.name.as_ref(), h.value.as_ref()))
        .chain(added.iter().map(|(n, v)| (n.as_slice(), v.as_slice())));
    let connect_timeout = Duration::from_secs(security.backend_connect_timeout_secs);
    let idle_timeout = Duration::from_secs(security.idle_connection_timeout_secs);
    let opened = if target.use_tls {
        let key = crate::http_utils::PoolKeyStr::tls_addr(addr.as_str(), target.sni(), tls_profile);
        match http2::upstream_mux::open_https(
            target,
            key.as_str(),
            tls_profile,
            connect_timeout,
            idle_timeout,
            method,
            final_path.as_bytes(),
            target.host.as_bytes(),
            headers_iter,
            false,
        )
        .await
        {
            Ok(HttpsOpen::H2(s)) => Ok(s),
            Ok(HttpsOpen::Http1(conn)) => return H2Relay::Http1(conn),
            Err(status) => Err(status),
        }
    } else {
        let key = crate::http_utils::PoolKeyStr::plain_addr(addr.as_str());
        http2::upstream_mux::open_h2c(
            target,
            key.as_str(),
            connect_timeout,
            idle_timeout,
            method,
            final_path.as_bytes(),
            target.host.as_bytes(),
            headers_iter,
            false,
        )
        .await
    };
    let mut stream = match opened {
        Ok(s) => s,
        Err(status) => {
            h2_drain_req(req_rx, notify).await;
            let msg: &[u8] = if status == 504 {
                b"Gateway Timeout"
            } else {
                b"Bad Gateway"
            };
            let (s, sz) = h2_emit_error(resp_tx, notify, status, msg).await;
            return H2Relay::Done((s, sz, 0));
        }
    };

    let req_size = std::cell::Cell::new(0u64);
    let req_tx = stream.req_tx.take();
    let initial = ctx.body.clone();
    let upload = async {
        let Some(tx) = req_tx else {
            return;
        };
        if !initial.is_empty() {
            req_size.set(initial.len() as u64);
            if tx.send(initial).await.is_err() {
                return;
            }
        }
        while let Some(chunk) = req_rx.recv().await {
            req_size.set(req_size.get() + chunk.len() as u64);
            if tx.send(chunk).await.is_err() {
                // 上流がストリームを閉じた（早期応答・リセット）。下流の残りは捨てる。
                h2_drain_req(req_rx, notify).await;
                return;
            }
            notify.notify();
        }
        // tx の drop で END_STREAM。
    };

    let resp_rx = &stream.resp_rx;
    let download = async {
        let mut status = 0u16;
        let mut sent = 0u64;
        loop {
            let msg = match timeout(READ_TIMEOUT, resp_rx.recv()).await {
                Ok(m) => m,
                Err(_) => {
                    warn!("[HTTP/2] h2c upstream response timeout");
                    if status == 0 {
                        return h2_emit_error(resp_tx, notify, 504, b"Gateway Timeout").await;
                    }
                    let _ = h2_send(resp_tx, notify, H2RespMsg::Reset(0x2)).await;
                    return (status, sent);
                }
            };
            match msg {
                None => {
                    if status == 0 {
                        return h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
                    }
                    // 送信端の drop でメインループが END_STREAM を送る。
                    return (status, sent);
                }
                Some(UpResp::Head {
                    status: s,
                    mut headers,
                }) => {
                    status = s;
                    headers.extend(h2_base_headers(true));
                    // `[route.traffic.cors]` の実際の応答のヘッダ（F-273）
                    crate::route_cors::apply_bytes(&mut headers, security);
                    // 上流が HEADERS で終わった（gRPC の trailers-only 応答など）なら、下流にも
                    // END_STREAM 付きの HEADERS 1 枚で返す（空 DATA で閉じ直さない）。
                    let end_stream = resp_rx.is_finished();
                    let head = H2RespMsg::Head {
                        status: s,
                        headers,
                        end_stream,
                    };
                    if h2_send(resp_tx, notify, head).await.is_err() || end_stream {
                        return (status, sent);
                    }
                }
                Some(UpResp::Data(b)) => {
                    if status == 0 {
                        return h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
                    }
                    sent += b.len() as u64;
                    if h2_send(resp_tx, notify, H2RespMsg::Body(b)).await.is_err() {
                        return (status, sent);
                    }
                }
                Some(UpResp::Trailers(trailers)) => {
                    #[cfg(feature = "grpc")]
                    {
                        if let Some(code) = trailers
                            .iter()
                            .find(|(n, _)| n.as_ref() == b"grpc-status")
                            .and_then(|(_, v)| std::str::from_utf8(v).ok())
                        {
                            crate::metrics::record_grpc_request(
                                &final_path,
                                code.trim(),
                                &target.host,
                            );
                        }
                        let _ = h2_send(resp_tx, notify, H2RespMsg::Trailers(trailers)).await;
                    }
                    #[cfg(not(feature = "grpc"))]
                    drop(trailers);
                }
                Some(UpResp::Reset { .. }) => {
                    if status == 0 {
                        return h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
                    }
                    // 応答途中の上流エラー: 切り詰めた本文を正常終了に見せない。
                    let _ = h2_send(resp_tx, notify, H2RespMsg::Reset(0x2)).await;
                    return (status, sent);
                }
            }
        }
    };

    let mut upload = std::pin::pin!(futures::FutureExt::fuse(upload));
    let mut download = std::pin::pin!(futures::FutureExt::fuse(download));
    loop {
        futures::select_biased! {
            (status, sent) = download => return H2Relay::Done((status, sent, req_size.get())),
            _ = upload => {}
        }
    }
}

/// リクエスト方向ストリーミング経路（F-32 統合）。戻り値 `(status, resp_size, req_size)`。
///
/// メインループから `req_rx` 経由で流れてくるボディを chunked でバックエンドへゼロコピー
/// 転送しつつ、レスポンスを [`H2RespMsg`] としてメインループへ流す。`status == 0` は
/// クライアント切断（ログ不要）。
#[cfg(feature = "http2")]
pub(super) async fn h2_serve_streaming(
    ctx: &H2RequestCtx,
    req_rx: &crate::stream_channel::Receiver<Bytes>,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64, u64) {
    // ルーティング（メインループで適格判定済み。ここでサーバー/パスを解決）。
    let method = &ctx.method[..];
    let path = &ctx.path[..];
    let client_ip: &str = &ctx.client_ip;

    let headers_raw: Vec<(&[u8], &[u8])> = ctx
        .headers
        .iter()
        .filter(|h| !h.name.starts_with(b":"))
        .map(|h| (h.name.as_ref(), h.value.as_ref()))
        .collect();

    let query_start = path.iter().position(|&b| b == b'?');
    let raw_query: &[u8] = query_start.map(|i| &path[i + 1..]).unwrap_or(b"");
    let path_wo_query = query_start.map(|i| &path[..i]).unwrap_or(path);
    let authority = &ctx.authority[..];

    let backend_result = crate::l7::resolve_route(
        authority,
        path_wo_query,
        method,
        &headers_raw,
        raw_query,
        &ctx.client_socket_addr,
        true,
    );

    // 適格判定が外れる（config 再読込レース等）場合は req をドレインして 502。
    let (prefix, upstream_group, compression, security) = match backend_result {
        Some((prefix, Backend::Proxy(ug, sec, comp, _buf, _cache, _mods), _rc)) => {
            (prefix, ug, comp, sec)
        }
        // ストリーミング受信のハンドラ（F-197）
        #[cfg(feature = "handler")]
        Some((_prefix, Backend::Handler(entry, sec, _, comp), _rc)) if entry.streaming_request => {
            return h2_api_handler(
                ctx,
                &entry,
                comp.as_deref(),
                &sec,
                Some(req_rx),
                resp_tx,
                notify,
            )
            .await;
        }
        // 下見の後に固定応答になった（JWT の期限が切れた等。F-219・F-220）
        Some((_prefix, Backend::Respond(r), _rc)) => {
            h2_drain_req(req_rx, notify).await;
            let mut headers = h2_base_headers(false);
            headers.extend(r.headers.iter().cloned());
            let (s, sz) = h2_emit_full(resp_tx, notify, r.status, headers, r.body.clone()).await;
            return (s, sz, 0);
        }
        _ => {
            h2_drain_req(req_rx, notify).await;
            let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
            return (s, sz, 0);
        }
    };

    // F-97: Consistent Hash の header/cookie キーに対応（バッファ経路と同じ選び方）。
    let server = match upstream_group.select_with_header_fn(client_ip, |name| {
        ctx.headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value.as_ref())
    }) {
        Some(s) => s,
        None => {
            h2_drain_req(req_rx, notify).await;
            let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
            return (s, sz, 0);
        }
    };

    // F-171 / F-175: 上流 HTTP/2（h2c、または ALPN で h2 を選んだ HTTPS 上流）は全二重で中継する。
    // HTTPS 上流が HTTP/1.1 を選んだら、ネゴシエーションに使った接続を下の HTTP/1.1 経路で使う。
    let mut preconnected: Option<Box<ClientTls>> = None;
    if server.target.use_h2c || upstream_group.use_h2c() || server.target.h2_over_tls() {
        server.acquire();
        let relayed = h2_relay_h2_streaming(
            ctx,
            req_rx,
            &server.target,
            upstream_group.tls_profile(),
            &prefix,
            &security,
            resp_tx,
            notify,
        )
        .await;
        server.release();
        match relayed {
            H2Relay::Done(r) => return r,
            H2Relay::Http1(conn) => preconnected = conn,
        }
    }

    let client_encoding = ctx
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(b"accept-encoding"))
        .map(|h| AcceptedEncoding::parse(&h.value))
        .unwrap_or(AcceptedEncoding::Identity);

    server.acquire();
    let target = &server.target;
    let use_tls = target.use_tls;
    let sni = target.sni().to_string();
    let tls_profile = upstream_group.tls_profile();
    let addr = target.conn_addr(); // F-170: UDS 対応（TCP は従来と不変）
    let addr = addr.as_str();

    // chunked リクエストヘッダ構築。
    let path_str = std::str::from_utf8(path).unwrap_or("/");
    let preserve_grpc_path = ctx
        .headers
        .iter()
        .any(|h| header_pair_is_grpc(&h.name, &h.value));
    let final_path_owned =
        compute_upstream_path(path_str, &prefix, &target.path_prefix, preserve_grpc_path);
    let final_path = final_path_owned.as_str();

    let mut request = request_buf_get(1024);
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
    let added = expanded_added_headers(&security, &ctx.client_ip, &ctx.authority, path_str);
    write_h1_forward_headers(
        &mut request,
        ctx.headers
            .iter()
            .map(|h| (h.name.as_ref(), h.value.as_ref())),
        &security,
        &added,
    );
    request.extend_from_slice(b"Transfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n");

    // F-175: ALPN で HTTP/1.1 を選んだ HTTPS 上流の接続（ネゴシエーション済み）をそのまま使う。
    if let Some(mut backend) = preconnected {
        let r = h2_run_streaming_upload(
            &mut *backend,
            request,
            req_rx,
            &compression,
            client_encoding,
            &security,
            resp_tx,
            notify,
        )
        .await;
        server.release();
        return r;
    }

    // バックエンド接続（事前解決済み SocketAddr があればブロッキング DNS を迂回）。
    let backend_tcp = match timeout(CONNECT_TIMEOUT, connect_target(target, addr)).await {
        Ok(Ok(s)) => s,
        _ => {
            server.release();
            request_buf_put(request);
            h2_drain_req(req_rx, notify).await;
            let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
            return (s, sz, 0);
        }
    };

    let (status, resp_size, req_size) = if use_tls {
        let tls_result = {
            let connector = crate::config::tls_connector(tls_profile);
            timeout(CONNECT_TIMEOUT, connector.connect(backend_tcp, &sni)).await
        };
        match tls_result {
            Ok(Ok(mut backend)) => {
                h2_run_streaming_upload(
                    &mut backend,
                    request,
                    req_rx,
                    &compression,
                    client_encoding,
                    &security,
                    resp_tx,
                    notify,
                )
                .await
            }
            _ => {
                request_buf_put(request);
                h2_drain_req(req_rx, notify).await;
                let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
                (s, sz, 0)
            }
        }
    } else {
        let mut backend = backend_tcp;
        h2_run_streaming_upload(
            &mut backend,
            request,
            req_rx,
            &compression,
            client_encoding,
            &security,
            resp_tx,
            notify,
        )
        .await
    };
    let _ = &security;
    server.release();
    (status, resp_size, req_size)
}

/// chunked リクエストヘッド送出 → req チャネルのボディ逐次転送 → レスポンスリレー。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn h2_run_streaming_upload<B>(
    backend: &mut B,
    request: Vec<u8>,
    req_rx: &crate::stream_channel::Receiver<Bytes>,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    security: &SecurityConfig,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64, u64)
where
    B: crate::runtime::io::AsyncReadRent + crate::runtime::io::AsyncWriteRentExt + Unpin,
{
    // ヘッダ送出。
    let (write_res, returned_request) = backend.write_all(request).await;
    request_buf_put(returned_request);
    if write_res.is_err() {
        h2_drain_req(req_rx, notify).await;
        let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
        return (s, sz, 0);
    }

    // リクエストボディを chunked で逐次転送。チャネルを消費したら notify で
    // メインループを起こし、req_pending の残りをチャネルへ補充させる（HTTP/3 と同方針。
    // これが無いとチャネル満杯 + メインループ待機で残ボディが流れずデッドロックする）。
    let mut req_size: u64 = 0;
    while let Some(chunk) = req_rx.recv().await {
        req_size = req_size.saturating_add(chunk.len() as u64);
        if send_backend_chunk(backend, chunk).await.is_err() {
            let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
            return (s, sz, req_size);
        }
        notify.notify();
    }
    // 終端チャンク。
    if backend_write_all_bytes(backend, Bytes::from_static(b"0\r\n\r\n"))
        .await
        .is_err()
    {
        let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
        return (s, sz, req_size);
    }

    let (status, sent, _reusable) = h2_relay_backend_response(
        backend,
        compression,
        client_encoding,
        security,
        resp_tx,
        notify,
        false,
    )
    .await;
    (status, sent, req_size)
}

/// HTTP/2 管理 API（B-29）。conn 非依存で `(status, headers, body)` を返す。
#[cfg(all(feature = "http2", feature = "admin"))]
pub(super) fn h2_admin_response(
    method: &[u8],
    path: &[u8],
    client_ip: &str,
    headers_raw: &[(&[u8], &[u8])],
) -> Option<(u16, Vec<(Bytes, Bytes)>, Vec<u8>)> {
    let config = CURRENT_CONFIG.load();
    let admin_config = &config.admin_config;
    if !admin_config.enabled {
        return None;
    }
    let path_str = std::str::from_utf8(path).unwrap_or("/");
    let auth = headers_raw.iter().find_map(|(name, value)| {
        if name.eq_ignore_ascii_case(b"authorization") {
            std::str::from_utf8(value).ok()
        } else {
            None
        }
    });

    let is_purge_method = method == b"PURGE";
    let is_admin_purge_path = path_str.starts_with(&admin_config.cache_purge_prefix);

    if is_purge_method || is_admin_purge_path {
        let (status, body) = if !admin_config.is_ip_allowed(client_ip) {
            (403, Vec::new())
        } else if !admin_config.check_auth(auth) {
            (401, Vec::new())
        } else {
            let resp = handle_cache_purge(path_str, is_purge_method);
            parse_http1_admin_response(&resp)
        };
        return Some((status, h2_base_headers(false), body));
    }

    if !path_str.starts_with(&admin_config.path_prefix)
        || method == b"PURGE"
        || path_str.starts_with(&admin_config.cache_purge_prefix)
    {
        return None;
    }

    let path_suffix = &path_str[admin_config.path_prefix.len()..];
    let is_known_endpoint = matches!(
        (method, path_suffix),
        (b"GET", "/config")
            | (b"GET", "/stats")
            | (b"GET", "/vds")
            | (b"POST", "/reload")
            | (b"POST", "/tls/reload")
    );
    if !is_known_endpoint {
        return None;
    }

    let (status, body) = if !admin_config.is_ip_allowed(client_ip) {
        (403, b"{\"error\":\"403\"}".to_vec())
    } else if !admin_config.check_auth(auth) {
        (401, b"{\"error\":\"401\"}".to_vec())
    } else {
        match (method, path_suffix) {
            (b"GET", "/config") => {
                let json = build_admin_config_json(&config);
                (200, json.into_bytes())
            }
            (b"GET", "/stats") => {
                let uptime_secs = PROXY_START_TIME.elapsed().as_secs();
                let json = format!("{{\"uptime_secs\":{}}}", uptime_secs);
                (200, json.into_bytes())
            }
            (b"GET", "/vds") => (200, admin_vds_json().into_bytes()),
            (b"POST", "/reload") => {
                use std::sync::atomic::Ordering;
                RELOAD_FLAG.store(true, Ordering::Relaxed);
                (200, b"{\"ok\":true}".to_vec())
            }
            (b"POST", "/tls/reload") => {
                use std::sync::atomic::Ordering;
                TLS_RELOAD_FLAG.store(true, Ordering::Relaxed);
                (200, b"{\"ok\":true}".to_vec())
            }
            _ => (404, Vec::new()),
        }
    };

    let mut headers = h2_base_headers(false);
    headers.push((
        Bytes::from_static(b"content-type"),
        Bytes::from_static(b"application/json"),
    ));
    Some((status, headers, body))
}

// ===== F-116: 再利用ヘルパー（従来実装を維持） =====

#[cfg(all(test, feature = "http2"))]
mod connect_gate_tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    // io_uring/epoll を許可しない環境（Docker ビルドサンドボックス・古いカーネル等）では
    // ランタイムドライバの生成が失敗し `block_on` が panic するため、ランタイムを要する
    // テストはスキップする（`src/l4/proxy.rs` の tests と同じパターン）。
    #[cfg(veil_rt_uring)]
    fn io_uring_available() -> bool {
        crate::runtime::ring::IoUring::new(8, 0).is_ok()
    }

    /// reactor（epoll）ビルドでは `epoll_create1` の成否をランタイム可用性の代替指標とする。
    #[cfg(veil_poller_epoll)]
    fn io_uring_available() -> bool {
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd >= 0 {
            unsafe { libc::close(fd) };
            true
        } else {
            false
        }
    }

    /// reactor（kqueue）ビルドでは `kqueue()` の成否をランタイム可用性の代替指標とする。
    #[cfg(veil_poller_kqueue)]
    fn io_uring_available() -> bool {
        let fd = unsafe { libc::kqueue() };
        if fd >= 0 {
            unsafe { libc::close(fd) };
            true
        } else {
            false
        }
    }

    /// reactor（WSAPoll）ビルド: WSAPoll はカーネル資源を事前確保しないため常に利用可能。
    #[cfg(veil_poller_wsapoll)]
    fn io_uring_available() -> bool {
        true
    }

    /// ConnectPermit の Drop が in_flight を確実に減算し、待機者へ通知すること（B-44 第3段）。
    #[test]
    fn test_connect_permit_drop_releases_and_notifies() {
        if !io_uring_available() {
            eprintln!(
                "io_uring unavailable; skipping test_connect_permit_drop_releases_and_notifies"
            );
            return;
        }

        let gate = Rc::new(ConnectGate::new());
        let waiter = crate::stream_channel::Notify::new();
        gate.waiters.borrow_mut().push_back(waiter.clone());

        let permit = ConnectPermit::try_acquire(&gate).expect("must acquire on empty gate");
        assert_eq!(gate.in_flight.get(), 1);

        drop(permit);
        assert_eq!(gate.in_flight.get(), 0, "Drop must decrement in_flight");
        assert!(
            gate.waiters.borrow().is_empty(),
            "Drop must drain and notify waiters"
        );
        // 通知フラグが立っている = wait() が即 Ready になる
        crate::runtime::block_on(async move {
            waiter.wait().await;
        });
    }

    /// 同時 65 個目の取得は wait に入り、スロット解放（permit Drop）後に進むこと（B-44 第3段）。
    #[test]
    fn test_gate_blocks_at_capacity_and_resumes_after_release() {
        if !io_uring_available() {
            eprintln!(
                "io_uring unavailable; skipping test_gate_blocks_at_capacity_and_resumes_after_release"
            );
            return;
        }

        crate::runtime::block_on(async {
            let gate = Rc::new(ConnectGate::new());
            // 64 スロットをすべて占有する
            let mut permits: Vec<ConnectPermit> = (0..MAX_CONCURRENT_CONNECTS_PER_HOST)
                .map(|_| ConnectPermit::try_acquire(&gate).expect("slot must be available"))
                .collect();
            assert_eq!(gate.in_flight.get(), MAX_CONCURRENT_CONNECTS_PER_HOST);
            assert!(
                ConnectPermit::try_acquire(&gate).is_none(),
                "65th acquisition must be rejected while the gate is full"
            );

            // 65 個目の取得タスク: acquire_backend_conn と同じ待機ループを実行する
            let progressed = Rc::new(Cell::new(false));
            let gate_task = gate.clone();
            let progressed_task = progressed.clone();
            crate::runtime::executor::spawn(async move {
                let mut waiter: Option<crate::stream_channel::Notify> = None;
                loop {
                    if let Some(_permit) = ConnectPermit::try_acquire(&gate_task) {
                        progressed_task.set(true);
                        return;
                    }
                    let w = waiter.get_or_insert_with(crate::stream_channel::Notify::new);
                    gate_task.waiters.borrow_mut().push_back(w.clone());
                    w.wait().await;
                }
            });

            // スポーンしたタスクに実行機会を与える（単一スレッドなので決定的）
            crate::runtime::timer::sleep(Duration::from_millis(10)).await;
            assert!(
                !progressed.get(),
                "the 65th acquirer must wait while the gate is full"
            );

            // 1 スロット解放 → permit の Drop が待機者を起こす
            permits.pop();
            crate::runtime::timer::sleep(Duration::from_millis(10)).await;
            assert!(
                progressed.get(),
                "the 65th acquirer must resume after a slot is released"
            );
        });
    }
}
