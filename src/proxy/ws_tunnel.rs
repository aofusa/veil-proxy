//! HTTP/2・HTTP/3 の上の WebSocket（拡張 CONNECT。RFC 8441・RFC 9220。F-288）
//!
//! `:method = CONNECT`・`:protocol = websocket` のストリームを受け、上流へ HTTP/1.1 の `Upgrade: websocket` を
//! 張って、ストリームの本文（DATA）と上流の TCP を双方向に中継する。HTTP/2 と HTTP/3 で共有する。
//!
//! - ルーティング・セキュリティの検査は GET として行う（RFC 8441 §5）。Proxy-Wasm の要求ヘッダの段も
//!   HTTP/1.1 の WebSocket と同じく通す。
//! - 上流は常に HTTP/1.1（`use_h2c`・ALPN の h2 の上流でも）。拡張 CONNECT のクライアントは
//!   `Sec-WebSocket-Key` を送らないので、プロキシが作って上流の `Sec-WebSocket-Accept` を検査する。
//!   101 はクライアントへ 200 にする（`Sec-WebSocket-Protocol`・`Sec-WebSocket-Extensions` は残す）。
//!   101 以外は本文ごと返す（2xx は「トンネルができた」と読まれるので 502 にする）。
//! - 中継はストリームの要求チャネル（`req_rx`）と上流の読み取り可能を同時に待つ（`futures::join!` を
//!   使わない。F-173）。フロー制御は既存の消費連動（HTTP/2 のウィンドウ補充・HTTP/3 の quiche のクレジット）。
//! - クライアントの END_STREAM・fin は上流の `shutdown(Write)`、上流の EOF はクライアントへの END_STREAM・fin。
//!   エラー・アイドルの満了（`websocket_idle_timeout_secs`）・graceful shutdown はストリームのリセット
//!   （RST_STREAM(CANCEL)・H3_REQUEST_CANCELLED。切り詰めを正常な終わりに見せない。B-86）。
//!
//! WebSocket の経路なので、要求ごとの確保（ヘッダの小文字化・中継の断片）は許している（AGENTS.md の
//! ホットパス規則の例外。WebSocket の無い要求は `:protocol` の有無の分岐 1 つだけ）。

use super::*;

/// RFC 6455 §1.3 の GUID
const WS_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// トンネルの要求（HTTP/2・HTTP/3 の要求から作る。疑似ヘッダは別に持つ）
pub(crate) struct TunnelRequest {
    /// `:path`（正規化済み）
    pub(crate) path: Bytes,
    /// `:authority`
    pub(crate) authority: Bytes,
    /// `:protocol`
    pub(crate) protocol: Bytes,
    /// 疑似ヘッダ以外のヘッダ（名前は小文字）
    pub(crate) headers: Vec<(Bytes, Bytes)>,
    pub(crate) client_ip: std::rc::Rc<str>,
    pub(crate) client_addr: SocketAddr,
    /// 平文リスナーの `enable_secure_redirect`（F-200。HTTP/2 だけ）
    pub(crate) redirect_https: Option<u16>,
    /// TLS で受けたか（ハンドラの `Request::is_tls`）
    #[cfg(feature = "handler")]
    pub(crate) tls: bool,
    /// 要求の受信の時刻（HTTP/3 のアクセスログ。HTTP/2 は要求のタスクが書く）
    #[cfg(feature = "http3")]
    pub(crate) start: Instant,
}

/// 応答の送り先（HTTP/2 のストリームか HTTP/3 のストリーム）
pub(crate) enum TunnelOut {
    #[cfg(feature = "http2")]
    H2 {
        tx: crate::stream_channel::Sender<H2RespMsg>,
        notify: crate::stream_channel::Notify,
    },
    #[cfg(feature = "http3")]
    H3 {
        tx: crate::stream_channel::Sender<crate::http3_stream::RespMsg>,
        notify: crate::http3_stream::ConnWaker,
    },
}

impl TunnelOut {
    /// メトリクスのラベル（`veil_proxy_websocket_tunnels{protocol}`）
    fn protocol_label(&self) -> &'static str {
        match self {
            #[cfg(feature = "http2")]
            TunnelOut::H2 { .. } => "h2",
            #[cfg(feature = "http3")]
            TunnelOut::H3 { .. } => "h3",
        }
    }

    /// HTTP/3 のストリームか（ハンドラの `Request::version`）
    #[cfg(feature = "handler")]
    pub(crate) fn is_h3(&self) -> bool {
        match self {
            #[cfg(feature = "http2")]
            TunnelOut::H2 { .. } => false,
            #[cfg(feature = "http3")]
            TunnelOut::H3 { .. } => true,
        }
    }

    /// メインループを起こす（要求の本文を受け取ったのでウィンドウ・クレジットを補充してもらう）
    pub(crate) fn notify(&self) {
        match self {
            #[cfg(feature = "http2")]
            TunnelOut::H2 { notify, .. } => notify.notify(),
            #[cfg(feature = "http3")]
            TunnelOut::H3 { notify, .. } => notify.notify(),
        }
    }

    /// 応答の head を送る（`end` なら本文なし）。`security` の応答ヘッダの方針は呼び出し側でかけてある。
    /// `own_server` が false なら `[server] server_header_enabled` の `server` を付けない。
    pub(crate) async fn head(
        &self,
        status: u16,
        headers: Vec<(Bytes, Bytes)>,
        own_server: bool,
        end: bool,
    ) -> Result<(), ()> {
        match self {
            #[cfg(feature = "http2")]
            TunnelOut::H2 { tx, notify } => {
                let mut headers = headers;
                if own_server {
                    if let Some(g) = get_server_header_guard() {
                        headers.retain(|(n, _)| !n.eq_ignore_ascii_case(b"server"));
                        headers.insert(0, (Bytes::from_static(b"server"), g.value_bytes()));
                    }
                }
                h2_send(
                    tx,
                    notify,
                    H2RespMsg::Head {
                        status,
                        headers,
                        end_stream: end,
                    },
                )
                .await
            }
            #[cfg(feature = "http3")]
            TunnelOut::H3 { tx, notify } => {
                // HTTP/3 のストリームは送り手（`tx`）を捨てたときに fin で閉じるので `end` は要らない
                let _ = end;
                let r = tx
                    .send(crate::http3_stream::RespMsg::Head {
                        status,
                        headers,
                        own_server,
                    })
                    .await
                    .map_err(|_| ());
                notify.notify();
                r
            }
        }
    }

    /// 本文の断片を送る
    pub(crate) async fn body(&self, b: Bytes) -> Result<(), ()> {
        match self {
            #[cfg(feature = "http2")]
            TunnelOut::H2 { tx, notify } => h2_send(tx, notify, H2RespMsg::Body(b)).await,
            #[cfg(feature = "http3")]
            TunnelOut::H3 { tx, notify } => {
                let r = tx
                    .send(crate::http3_stream::RespMsg::Body(b))
                    .await
                    .map_err(|_| ());
                notify.notify();
                r
            }
        }
    }

    /// ストリームをリセットする（HTTP/2: RST_STREAM(CANCEL)、HTTP/3: H3_REQUEST_CANCELLED。head の後）
    pub(crate) async fn reset(&self) {
        match self {
            #[cfg(feature = "http2")]
            TunnelOut::H2 { tx, notify } => {
                let _ = h2_send(
                    tx,
                    notify,
                    H2RespMsg::Reset(http2::Http2ErrorCode::Cancel as u32),
                )
                .await;
            }
            #[cfg(feature = "http3")]
            TunnelOut::H3 { tx, notify } => {
                // head の後の Error はストリームのリセット（H3_REQUEST_CANCELLED）
                let _ = tx
                    .send(crate::http3_stream::RespMsg::Error { status: 502 })
                    .await;
                notify.notify();
            }
        }
    }

    /// 応答のチャネルに空きがある（または閉じた）か
    fn poll_ready(&self, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        match self {
            #[cfg(feature = "http2")]
            TunnelOut::H2 { tx, .. } => tx.poll_ready(cx),
            #[cfg(feature = "http3")]
            TunnelOut::H3 { tx, .. } => tx.poll_ready(cx),
        }
    }

    /// エラー応答（本文付き）
    async fn error(&self, status: u16, msg: &'static [u8]) -> (u16, u64) {
        let headers = vec![
            (
                Bytes::from_static(b"content-type"),
                Bytes::from_static(b"text/plain"),
            ),
            (
                Bytes::from_static(b"content-length"),
                Bytes::from(msg.len().to_string()),
            ),
        ];
        if self.head(status, headers, true, false).await.is_ok() {
            let _ = self.body(Bytes::from_static(msg)).await;
        }
        (status, msg.len() as u64)
    }
}

/// 拡張 CONNECT の要求を処理する（HTTP/2・HTTP/3 の要求のタスクから呼ぶ）。戻り値 `(status, 応答のバイト数,
/// 要求のバイト数)`。`status` はアクセスログに書く値（トンネルができたら 200）。
pub(crate) async fn serve(
    req: TunnelRequest,
    req_rx: Option<crate::stream_channel::Receiver<Bytes>>,
    out: TunnelOut,
) -> (u16, u64, u64) {
    // F-200: 平文リスナーの HTTPS リダイレクト（HTTP/2 の通常の要求と同じ）
    if let Some(port) = req.redirect_https {
        let host = std::str::from_utf8(&req.authority).unwrap_or("");
        let location = crate::config::https_redirect_url(
            host,
            std::str::from_utf8(&req.path).unwrap_or("/"),
            port,
        );
        let headers = vec![(Bytes::from_static(b"location"), Bytes::from(location))];
        let _ = out.head(301, headers, true, true).await;
        return (301, 0, 0);
    }

    let headers_raw: Vec<(&[u8], &[u8])> = req
        .headers
        .iter()
        .map(|(n, v)| (n.as_ref(), v.as_ref()))
        .collect();
    let (path_wo_query, raw_query) = crate::l7::split_query(&req.path);
    // RFC 8441 §5: ルートの照合・検査は GET の要求として行う
    let Some((prefix, backend, _comp)) = crate::l7::resolve_route(
        &req.authority,
        path_wo_query,
        b"GET",
        &headers_raw,
        raw_query,
        &req.client_addr,
        true,
    ) else {
        let r = out.error(404, b"Not Found").await;
        return (r.0, r.1, 0);
    };
    drop(headers_raw);

    // ハンドラのルート（F-262 の WebSocket ハンドラを HTTP/2・HTTP/3 のストリームの上で動かす）
    #[cfg(feature = "handler")]
    if let Backend::Handler(entry, security, modules, comp) = &backend {
        let check = check_security(security, &req.client_ip, b"GET", 0, false);
        if check != SecurityCheckResult::Allowed {
            let r = out.error(check.status_code(), check.message()).await;
            return (r.0, r.1, 0);
        }
        // B-317: ハンドラのルートでも Proxy-Wasm の要求ヘッダの段を通す（HTTP/1.1 の WebSocket と同じ）。
        // 以前は RFC 8441・9220 の拡張 CONNECT だけ Wasm の認可を飛ばしてハンドラを呼んでいた（F16）
        #[cfg(feature = "wasm")]
        let (req, wasm) = {
            let mut wasm = match wasm_request_headers(&req, modules.as_ref(), &out).await {
                Ok(w) => w,
                Err(r) => return (r.0, r.1, 0),
            };
            let mut req = req;
            if let Some(h) = wasm.headers.take() {
                req.headers = h;
            }
            (req, wasm)
        };
        #[cfg(not(feature = "wasm"))]
        let _ = modules;
        let result =
            crate::api::dispatch::serve_extended_connect(&req, entry, comp.as_deref(), req_rx, out)
                .await;
        #[cfg(feature = "wasm")]
        if let Some(engine) = wasm.engine {
            crate::wasm::on_request_complete_async(engine, wasm.modules).await;
        }
        return result;
    }

    let Backend::Proxy(upstream_group, security, _, _, _, modules) = &backend else {
        // WebSocket のトンネルは Proxy のルートだけ（HTTP/1.1 は他のルートへアップグレードを渡さない。B-307）
        let r = out
            .error(
                400,
                b"Bad Request: WebSocket is not supported on this route",
            )
            .await;
        return (r.0, r.1, 0);
    };
    if !req.protocol.eq_ignore_ascii_case(b"websocket") {
        let r = out
            .error(501, b"Not Implemented: unsupported :protocol")
            .await;
        return (r.0, r.1, 0);
    }
    let check = check_security(security, &req.client_ip, b"GET", 0, false);
    if check != SecurityCheckResult::Allowed {
        let r = out.error(check.status_code(), check.message()).await;
        return (r.0, r.1, 0);
    }

    // Proxy-Wasm の要求ヘッダの段（HTTP/1.1 の WebSocket と同じ。モジュールの無いルートは分岐だけ）
    #[cfg(feature = "wasm")]
    let wasm = match wasm_request_headers(&req, modules.as_ref(), &out).await {
        Ok(w) => w,
        Err(r) => return (r.0, r.1, 0),
    };
    #[cfg(not(feature = "wasm"))]
    let _ = modules;
    #[cfg(feature = "wasm")]
    let headers: &[(Bytes, Bytes)] = wasm.headers.as_deref().unwrap_or(&req.headers);
    #[cfg(not(feature = "wasm"))]
    let headers: &[(Bytes, Bytes)] = &req.headers;

    let Some(server) = upstream_group.select(&req.client_ip) else {
        crate::metrics::record_upstream_connect_error(
            crate::metrics::UpstreamConnectError::NoHealthy,
        );
        crate::logging::log_throttled!(error, "No healthy upstream servers for WebSocket");
        let r = out.error(502, b"Bad Gateway").await;
        return (r.0, r.1, 0);
    };
    server.acquire();
    let result = proxy_tunnel(
        &req,
        headers,
        &server.target,
        upstream_group.tls_profile(),
        security,
        &prefix,
        req_rx,
        &out,
    )
    .await;
    server.release();

    #[cfg(feature = "wasm")]
    if let Some(engine) = wasm.engine {
        crate::wasm::on_request_complete_async(engine, wasm.modules).await;
    }
    result
}

/// Proxy-Wasm の要求ヘッダの段の結果
#[cfg(feature = "wasm")]
struct WasmPhase {
    /// モジュールが書き換えたヘッダ（書き換えなし・モジュールなしは None）
    headers: Option<Vec<(Bytes, Bytes)>>,
    /// 終わったら `on_request_complete` を呼ぶエンジンとモジュール
    engine: Option<Arc<crate::wasm::FilterEngine>>,
    modules: crate::wasm_plugin_config::WasmModules,
}

/// ルートのモジュールの `on_request_headers` を呼ぶ。ローカル応答を返したらそれを送って `Err`。
#[cfg(feature = "wasm")]
async fn wasm_request_headers(
    req: &TunnelRequest,
    modules: Option<&Arc<Vec<crate::wasm_plugin_config::ModuleRef>>>,
    out: &TunnelOut,
) -> Result<WasmPhase, (u16, u64)> {
    let none = || WasmPhase {
        headers: None,
        engine: None,
        modules: crate::wasm::empty_wasm_modules(),
    };
    let Some(modules) = modules.filter(|m| !m.is_empty()) else {
        return Ok(none());
    };
    let config = CURRENT_CONFIG.load();
    let Some(engine) = config.wasm_filter_engine.clone() else {
        return Ok(none());
    };
    let modules = crate::wasm_plugin_config::WasmModules::for_request(modules.clone());
    let headers_vec: Vec<(Vec<u8>, Vec<u8>)> = req
        .headers
        .iter()
        .map(|(n, v)| (n.to_vec(), v.to_vec()))
        .collect();
    let result = engine
        .clone()
        .on_request_headers_with_modules_async(
            modules.clone(),
            Arc::from(std::str::from_utf8(&req.path).unwrap_or("/")),
            Arc::from("GET"),
            headers_vec,
            Arc::from(&*req.client_ip),
            // B-320: 拡張 CONNECT のトンネルは本文の段を走らせないので、本文を待つ Pause を決めずに
            // 通さないよう、本文が無いものとして渡す（F57）
            true,
        )
        .await;
    match result {
        crate::wasm::FilterResult::Continue { headers, .. } => Ok(WasmPhase {
            headers: Some(
                headers
                    .into_iter()
                    .map(|(n, v)| (Bytes::from(n.to_ascii_lowercase()), Bytes::from(v)))
                    .collect(),
            ),
            engine: Some(engine),
            modules,
        }),
        crate::wasm::FilterResult::LocalResponse(resp) => {
            let headers: Vec<(Bytes, Bytes)> = resp
                .headers
                .into_iter()
                .map(|(n, v)| (Bytes::from(n.to_ascii_lowercase()), Bytes::from(v)))
                .collect();
            let len = resp.body.len() as u64;
            let empty = resp.body.is_empty();
            if out
                .head(resp.status_code, headers, true, empty)
                .await
                .is_ok()
                && !empty
            {
                let _ = out.body(Bytes::from(resp.body)).await;
            }
            crate::wasm::on_request_complete_async(engine, modules).await;
            Err((resp.status_code, len))
        }
    }
}

/// `Sec-WebSocket-Key` と、上流が返すべき `Sec-WebSocket-Accept` を作る
fn ws_key_pair() -> Option<([u8; 24], [u8; 28])> {
    use crate::tls_provider::SecureRandom as _;
    let mut raw = [0u8; 16];
    crate::tls_provider::SystemRandom::new()
        .fill(&mut raw)
        .ok()?;
    let mut key = [0u8; 24];
    base64_into(&raw, &mut key);
    let digest = crate::tls_provider::sha1_legacy(&[&key, WS_GUID]);
    let mut accept = [0u8; 28];
    base64_into(&digest, &mut accept);
    Some((key, accept))
}

/// 標準の base64（パディング付き）を `out`（長さ `src.len().div_ceil(3) * 4`）へ書く
fn base64_into(src: &[u8], out: &mut [u8]) {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for (c, o) in src.chunks(3).zip(out.chunks_mut(4)) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        o[0] = T[(n >> 18) as usize & 63];
        o[1] = T[(n >> 12) as usize & 63];
        o[2] = if c.len() > 1 {
            T[(n >> 6) as usize & 63]
        } else {
            b'='
        };
        o[3] = if c.len() > 2 {
            T[n as usize & 63]
        } else {
            b'='
        };
    }
}

/// 上流へ送る HTTP/1.1 のアップグレードの要求を組む
fn build_upgrade_request(
    target: &ProxyTarget,
    final_path: &str,
    key: &[u8],
    headers: &[(Bytes, Bytes)],
    security: &SecurityConfig,
    added: &AddedHeaders,
) -> Vec<u8> {
    let mut r = request_buf_get(1024);
    r.extend_from_slice(b"GET ");
    r.extend_from_slice(final_path.as_bytes());
    r.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    r.extend_from_slice(target.host.as_bytes());
    if !target.is_default_port() {
        r.push(b':');
        let mut port_buf = itoa::Buffer::new();
        r.extend_from_slice(port_buf.format(target.port).as_bytes());
    }
    r.extend_from_slice(
        b"\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: ",
    );
    r.extend_from_slice(key);
    r.extend_from_slice(b"\r\n");
    // 鍵・版はこちらが付けた。`te`・`content-length` 等のホップバイホップは `write_h1_forward_headers` が落とす
    write_h1_forward_headers(
        &mut r,
        headers
            .iter()
            .filter(|(n, _)| {
                !n.eq_ignore_ascii_case(b"sec-websocket-key")
                    && !n.eq_ignore_ascii_case(b"sec-websocket-version")
                    && !n.eq_ignore_ascii_case(b"sec-websocket-accept")
                    && !n.eq_ignore_ascii_case(b"te")
            })
            .map(|(n, v)| (n.as_ref(), v.as_ref())),
        security,
        added,
    );
    r.extend_from_slice(b"\r\n");
    r
}

/// 上流の応答のヘッダをクライアント向け（HTTP/2・HTTP/3。名前は小文字）にする。ホップバイホップと
/// `Sec-WebSocket-Accept`（ハンドシェイクはプロキシで終わる）を落とし、ルートの応答ヘッダの方針をかける
/// （B-307 の HTTP/1.1 と同じ）。`tunnel` なら本文の長さのヘッダも落とす（200 の後はトンネルのバイト列）。
fn client_headers(
    raw_head: &[u8],
    security: &SecurityConfig,
    tunnel: bool,
) -> (Vec<(Bytes, Bytes)>, Option<Bytes>, bool) {
    let mut storage = [httparse::EMPTY_HEADER; 64];
    let mut resp = httparse::Response::new(&mut storage);
    let mut out: Vec<(Bytes, Bytes)> = Vec::new();
    let mut accept = None;
    let mut upgrade_ws = false;
    if resp.parse(raw_head).is_err() {
        return (out, None, false);
    }
    for h in resp.headers.iter() {
        let name = h.name.as_bytes();
        if name.eq_ignore_ascii_case(b"sec-websocket-accept") {
            accept = Some(Bytes::copy_from_slice(h.value.trim_ascii()));
            continue;
        }
        if name.eq_ignore_ascii_case(b"upgrade") {
            upgrade_ws |= h
                .value
                .split(|&b| b == b',')
                .any(|t| t.trim_ascii().eq_ignore_ascii_case(b"websocket"));
            continue;
        }
        if name.eq_ignore_ascii_case(b"connection")
            || name.eq_ignore_ascii_case(b"keep-alive")
            || name.eq_ignore_ascii_case(b"proxy-connection")
            || name.eq_ignore_ascii_case(b"transfer-encoding")
            || name.eq_ignore_ascii_case(b"te")
            || (tunnel && name.eq_ignore_ascii_case(b"content-length"))
            || !is_valid_header_name(name)
            || !is_valid_header_value(h.value)
        {
            continue;
        }
        out.push((
            Bytes::from(name.to_ascii_lowercase()),
            Bytes::copy_from_slice(h.value),
        ));
    }
    security.apply_response_headers(&mut out);
    (out, accept, upgrade_ws)
}

/// 上流へ接続してハンドシェイクし、中継する
#[allow(clippy::too_many_arguments)]
async fn proxy_tunnel(
    req: &TunnelRequest,
    headers: &[(Bytes, Bytes)],
    target: &ProxyTarget,
    tls_profile: crate::upstream_tls::TlsProfile,
    security: &SecurityConfig,
    prefix: &[u8],
    req_rx: Option<crate::stream_channel::Receiver<Bytes>>,
    out: &TunnelOut,
) -> (u16, u64, u64) {
    let connect_timeout = Duration::from_secs(security.backend_connect_timeout_secs);
    let addr = target.conn_addr();
    let tcp = match timeout(connect_timeout, connect_target(target, addr.as_str())).await {
        Ok(Ok(s)) => {
            let _ = s.set_nodelay(true);
            s
        }
        Ok(Err(e)) => {
            crate::metrics::record_upstream_connect_error(
                crate::metrics::UpstreamConnectError::Error,
            );
            crate::logging::log_throttled!(error, "WebSocket proxy connect error: {}", e);
            let r = out.error(502, b"Bad Gateway").await;
            return (r.0, r.1, 0);
        }
        Err(_) => {
            let r = out.error(504, b"Gateway Timeout").await;
            return (r.0, r.1, 0);
        }
    };
    if target.use_tls {
        let connector = crate::config::tls_connector(tls_profile);
        let mut tls = match timeout(connect_timeout, connector.connect(tcp, &target.host)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                crate::metrics::record_upstream_connect_error(
                    crate::metrics::UpstreamConnectError::Tls,
                );
                crate::logging::log_throttled!(error, "WebSocket TLS connect error: {}", e);
                let r = out.error(502, b"Bad Gateway").await;
                return (r.0, r.1, 0);
            }
            Err(_) => {
                let r = out.error(504, b"Gateway Timeout").await;
                return (r.0, r.1, 0);
            }
        };
        handshake_and_relay(
            req, headers, target, security, prefix, &mut tls, req_rx, out,
        )
        .await
    } else {
        let mut tcp = tcp;
        handshake_and_relay(
            req, headers, target, security, prefix, &mut tcp, req_rx, out,
        )
        .await
    }
}

/// 上流のアップグレードの要求を送り、応答を返す。101 なら 200 を返して中継する。
#[allow(clippy::too_many_arguments)]
async fn handshake_and_relay<B: RelayIo>(
    req: &TunnelRequest,
    headers: &[(Bytes, Bytes)],
    target: &ProxyTarget,
    security: &SecurityConfig,
    prefix: &[u8],
    backend: &mut B,
    req_rx: Option<crate::stream_channel::Receiver<Bytes>>,
    out: &TunnelOut,
) -> (u16, u64, u64) {
    let Some((key, expected_accept)) = ws_key_pair() else {
        let r = out.error(500, b"Internal Server Error").await;
        return (r.0, r.1, 0);
    };
    let path_str = std::str::from_utf8(&req.path).unwrap_or("/");
    let final_path = ws_upstream_path(target, path_str, prefix);
    let added = expanded_added_headers(security, &req.client_ip, &req.authority, path_str);
    let request = build_upgrade_request(target, &final_path, &key, headers, security, &added);

    let (write_res, returned) = match timeout(WRITE_TIMEOUT, backend.write_all(request)).await {
        Ok(r) => r,
        Err(_) => {
            let r = out.error(504, b"Gateway Timeout").await;
            return (r.0, r.1, 0);
        }
    };
    request_buf_put(returned);
    if write_res.is_err() {
        let r = out.error(502, b"Bad Gateway").await;
        return (r.0, r.1, 0);
    }

    // 上流の応答の head を読む（B-17: 無応答の上流に待たせない）
    let mut response_buf = Vec::with_capacity(4096);
    let parsed = loop {
        let buf = buf_get();
        let (res, mut buf) = match timeout(BACKEND_HEADER_TIMEOUT, backend.read(buf)).await {
            Ok(r) => r,
            Err(_) => {
                let r = out.error(504, b"Gateway Timeout").await;
                return (r.0, r.1, 0);
            }
        };
        let n = match res {
            Ok(n) if n > 0 => n,
            _ => {
                buf_put(buf);
                let r = out.error(502, b"Bad Gateway").await;
                return (r.0, r.1, 0);
            }
        };
        buf.set_valid_len(n);
        response_buf.extend_from_slice(buf.as_valid_slice());
        buf_put(buf);
        // 101 以外の 1xx は最終の応答ではない（B-307）
        crate::http_utils::drain_interim_responses(&mut response_buf);
        if let Some(p) = parse_http_response(&response_buf) {
            break p;
        }
        if response_buf.len() > MAX_HEADER_SIZE
            || crate::http_utils::response_head_invalid(&response_buf)
        {
            let r = out.error(502, b"Bad Gateway").await;
            return (r.0, r.1, 0);
        }
    };
    let status = parsed.status_code;
    let own_server = !security.removes_response_header(b"server");
    let rest = Bytes::copy_from_slice(&response_buf[parsed.header_len..]);

    if status == 101 {
        let (resp_headers, accept, upgrade_ws) =
            client_headers(&response_buf[..parsed.header_len], security, true);
        // 上流が本当に WebSocket へ切り替えたか（鍵はプロキシが作った）
        if !upgrade_ws || accept.as_deref() != Some(&expected_accept[..]) {
            debug!("WebSocket upstream returned an invalid Sec-WebSocket-Accept");
            let r = out.error(502, b"Bad Gateway").await;
            return (r.0, r.1, 0);
        }
        if out
            .head(200, resp_headers, own_server, false)
            .await
            .is_err()
        {
            return (0, 0, 0);
        }
        let early = rest.len() as u64;
        if !rest.is_empty() && out.body(rest).await.is_err() {
            return (200, 0, 0);
        }
        let idle = security.websocket_poll_config().idle_timeout;
        let (down, up) = relay(req_rx, out, backend, idle).await;
        return (200, early + down, up);
    }

    // アップグレードしなかった: 応答を本文ごと返す。2xx は「トンネルができた」と読まれるので 502（RFC 8441 §5）
    if (200..300).contains(&status) {
        let r = out.error(502, b"Bad Gateway").await;
        return (r.0, r.1, 0);
    }
    let (resp_headers, _, _) = client_headers(&response_buf[..parsed.header_len], security, false);
    let no_body = (100..200).contains(&status) || status == 204 || status == 304;
    if out
        .head(status, resp_headers, own_server, no_body)
        .await
        .is_err()
        || no_body
    {
        return (status, 0, 0);
    }
    let sent = forward_body(backend, out, parsed, rest).await;
    (status, sent, 0)
}

/// アップグレードしなかった応答の本文を返す（`Content-Length`・chunked・接続の終わりまで）。
/// chunked は本文だけを取り出して送る（HTTP/2・HTTP/3 には chunked が無い）。
async fn forward_body<B: RelayIo>(
    backend: &mut B,
    out: &TunnelOut,
    parsed: ParsedResponse,
    rest: Bytes,
) -> u64 {
    enum Framing {
        Length(u64),
        Chunked(crate::http_utils::ChunkedDecoder),
        UntilClose,
        Done,
    }
    let mut chunked_out = Vec::new();
    let mut sent = 0u64;
    let mut framing = if parsed.is_chunked {
        Framing::Chunked(crate::http_utils::ChunkedDecoder::new(0))
    } else if let Some(cl) = parsed.content_length {
        Framing::Length(cl as u64)
    } else {
        Framing::UntilClose
    };
    let mut pending = rest;
    loop {
        if !pending.is_empty() {
            let data = std::mem::take(&mut pending);
            let (piece, next) = match framing {
                Framing::Length(left) => {
                    let take = (left.min(data.len() as u64)) as usize;
                    let next = if left <= data.len() as u64 {
                        Framing::Done
                    } else {
                        Framing::Length(left - data.len() as u64)
                    };
                    (data.slice(..take), next)
                }
                Framing::Chunked(mut d) => {
                    // 本文の部分だけを取り出す（フレーミングのバイトは捨てる）
                    chunked_out.clear();
                    let mut input = &data[..];
                    let mut next = None;
                    while !input.is_empty() {
                        let span = d.next_data_span(input);
                        chunked_out.extend_from_slice(
                            &input[span.data_start..span.data_start + span.data_len],
                        );
                        if span.complete || span.limit_exceeded {
                            next = Some(Framing::Done);
                            break;
                        }
                        if span.consumed == 0 {
                            break;
                        }
                        input = &input[span.consumed..];
                    }
                    (
                        Bytes::copy_from_slice(&chunked_out),
                        next.unwrap_or(Framing::Chunked(d)),
                    )
                }
                other => (data, other),
            };
            framing = next;
            if !piece.is_empty() {
                sent += piece.len() as u64;
                if out.body(piece).await.is_err() {
                    return sent;
                }
            }
        }
        if matches!(framing, Framing::Done) {
            return sent;
        }
        let buf = buf_get();
        let (res, mut buf) = match timeout(READ_TIMEOUT, backend.read(buf)).await {
            Ok(r) => r,
            Err(_) => {
                out.reset().await;
                return sent;
            }
        };
        match res {
            Ok(n) if n > 0 => {
                buf.set_valid_len(n);
                pending = Bytes::copy_from_slice(buf.as_valid_slice());
                buf_put(buf);
            }
            _ => {
                buf_put(buf);
                // 長さの決まった本文が途中で切れたら、正常な終わりに見せない
                if !matches!(framing, Framing::UntilClose) {
                    out.reset().await;
                }
                return sent;
            }
        }
    }
}

/// 中継の待ちの結果
enum Ev {
    /// クライアントのデータ（None は要求の終わり）
    Client(Option<Bytes>),
    /// 上流が読み取り可能（またはエラー。読み取りで確かめる）
    Upstream,
}

/// 上流の読み取り可能の待ちを起こした印（起きていない待ちを poll し直さない。F-173: reactor で
/// `poll(2)` を空打ちしない）
struct UpWaker {
    woken: std::sync::atomic::AtomicBool,
    parent: futures::task::AtomicWaker,
}

impl std::task::Wake for UpWaker {
    fn wake(self: std::sync::Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &std::sync::Arc<Self>) {
        self.woken.store(true, std::sync::atomic::Ordering::Release);
        self.parent.wake();
    }
}

/// アイドルの上限が無いときの 1 回の待ちの長さ（満了したら待ち直す）
const RELAY_WAIT_CHUNK: Duration = Duration::from_secs(3600);

/// ストリームと上流の双方向の中継。戻り値 `(上流→クライアントのバイト数, クライアント→上流のバイト数)`。
///
/// 要求のチャネルと上流の読み取り可能を同時に待つ。上流は応答のチャネルに空きがあるときだけ読む
/// （クライアントが読まなければ上流の TCP のウィンドウで止まる）。上流の EOF は `out` を閉じて
/// END_STREAM・fin にし、エラー・アイドルの満了・graceful shutdown はストリームのリセットにする。
async fn relay<B: RelayIo>(
    mut req_rx: Option<crate::stream_channel::Receiver<Bytes>>,
    out: &TunnelOut,
    backend: &mut B,
    idle_timeout: Option<Duration>,
) -> (u64, u64) {
    use std::future::Future;
    use std::sync::atomic::Ordering;
    use std::task::{Context, Poll};

    let _tunnel = crate::metrics::websocket_tunnel_opened(out.protocol_label());
    let up_flag = std::sync::Arc::new(UpWaker {
        woken: std::sync::atomic::AtomicBool::new(true),
        parent: futures::task::AtomicWaker::new(),
    });
    let up_waker = std::task::Waker::from(up_flag.clone());
    let wait = idle_timeout.unwrap_or(RELAY_WAIT_CHUNK);
    let mut down = 0u64;
    let mut up = 0u64;
    loop {
        let buffered = backend.has_buffered_read_data();
        let started = std::time::Instant::now();
        let ev = {
            let rx = req_rx.as_ref();
            let tcp = backend.relay_tcp();
            up_flag.woken.store(true, Ordering::Relaxed);
            let mut readable = std::pin::pin!(tcp.readable_confirmed());
            let waiting = std::future::poll_fn(|cx: &mut Context<'_>| {
                if let Some(rx) = rx {
                    if let Poll::Ready(item) = rx.poll_recv(cx) {
                        return Poll::Ready(Ev::Client(item));
                    }
                }
                // 応答のチャネルに空きがあるときだけ上流を読む
                if out.poll_ready(cx).is_pending() {
                    return Poll::Pending;
                }
                if buffered {
                    return Poll::Ready(Ev::Upstream);
                }
                up_flag.parent.register(cx.waker());
                if up_flag.woken.swap(false, Ordering::AcqRel)
                    && readable
                        .as_mut()
                        .poll(&mut Context::from_waker(&up_waker))
                        .is_ready()
                {
                    return Poll::Ready(Ev::Upstream);
                }
                Poll::Pending
            });
            crate::runtime::time::timeout_idle(wait, true, waiting).await
        };
        match ev {
            Ok(Ev::Client(Some(data))) => {
                // 受け取った分のウィンドウ・クレジットを返してもらう
                out.notify();
                let n = data.len() as u64;
                if data.is_empty() {
                    continue;
                }
                match timeout(WRITE_TIMEOUT, backend.write_all(data)).await {
                    Ok((Ok(_), _)) => up += n,
                    _ => {
                        out.reset().await;
                        return (down, up);
                    }
                }
            }
            Ok(Ev::Client(None)) => {
                let finished = req_rx.as_ref().is_some_and(|r| r.is_finished());
                req_rx = None;
                if !finished {
                    // クライアントがストリームをリセットした・接続が切れた
                    let _ = backend.relay_tcp().shutdown(std::net::Shutdown::Both);
                    return (down, up);
                }
                // END_STREAM・fin: 上流へ送信の終わりを伝え、上流の残りを返し続ける
                backend.close_write().await;
            }
            Ok(Ev::Upstream) => {
                let buf = buf_get();
                let (res, mut buf) = match timeout(wait, backend.read(buf)).await {
                    Ok(r) => r,
                    Err(_) => {
                        out.reset().await;
                        return (down, up);
                    }
                };
                match res {
                    Ok(0) => {
                        buf_put(buf);
                        // 上流が閉じた: `out` を閉じて END_STREAM・fin（TCP の FIN に当たる。RFC 8441 §4）
                        return (down, up);
                    }
                    Ok(n) => {
                        buf.set_valid_len(n);
                        let data = Bytes::copy_from_slice(buf.as_valid_slice());
                        buf_put(buf);
                        down += n as u64;
                        if out.body(data).await.is_err() {
                            return (down, up);
                        }
                    }
                    Err(_) => {
                        buf_put(buf);
                        out.reset().await;
                        return (down, up);
                    }
                }
            }
            Err(_) => {
                // 上限が無く、満了まで待った（graceful shutdown ではない）なら待ち直す
                if idle_timeout.is_none() && started.elapsed() >= wait {
                    continue;
                }
                debug!("WebSocket tunnel idle or shutting down; resetting the stream");
                out.reset().await;
                return (down, up);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6455 §1.3 の例: 鍵 `dGhlIHNhbXBsZSBub25jZQ==` の Accept は `s3pPLMBiTxaQ9kYGzzhZRbK+xOo=`
    #[test]
    fn accept_key_matches_rfc6455_example() {
        let key = b"dGhlIHNhbXBsZSBub25jZQ==";
        let digest = crate::tls_provider::sha1_legacy(&[key, WS_GUID]);
        let mut accept = [0u8; 28];
        base64_into(&digest, &mut accept);
        assert_eq!(&accept, b"s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        let mut k = [0u8; 24];
        base64_into(b"the sample nonce", &mut k);
        assert_eq!(&k, key);
    }

    #[test]
    fn key_pair_is_random_and_consistent() {
        let (k1, a1) = ws_key_pair().unwrap();
        let (k2, _) = ws_key_pair().unwrap();
        assert_ne!(k1, k2);
        let digest = crate::tls_provider::sha1_legacy(&[&k1, WS_GUID]);
        let mut a = [0u8; 28];
        base64_into(&digest, &mut a);
        assert_eq!(a, a1);
    }

    #[test]
    fn upgrade_request_has_our_key_and_drops_client_handshake_fields() {
        let target = ProxyTarget::parse("http://127.0.0.1:9000").unwrap();
        let headers = vec![
            (
                Bytes::from_static(b"sec-websocket-key"),
                Bytes::from_static(b"evil"),
            ),
            (
                Bytes::from_static(b"sec-websocket-version"),
                Bytes::from_static(b"13"),
            ),
            (
                Bytes::from_static(b"sec-websocket-protocol"),
                Bytes::from_static(b"chat"),
            ),
            (
                Bytes::from_static(b"origin"),
                Bytes::from_static(b"https://a.example"),
            ),
        ];
        let security = SecurityConfig::default();
        let added = expanded_added_headers(&security, "127.0.0.1", b"a.example", "/ws/x");
        let r = build_upgrade_request(&target, "/x", b"KEY", &headers, &security, &added);
        let s = String::from_utf8(r).unwrap();
        assert!(
            s.starts_with("GET /x HTTP/1.1\r\nHost: 127.0.0.1:9000\r\n"),
            "{s}"
        );
        assert!(s.contains("Upgrade: websocket\r\n"));
        assert!(s.contains("Sec-WebSocket-Key: KEY\r\n"));
        assert!(!s.contains("evil"));
        assert_eq!(
            s.matches("Sec-WebSocket-Version").count() + s.matches("sec-websocket-version").count(),
            1
        );
        assert!(s.contains("sec-websocket-protocol: chat\r\n"));
        assert!(s.ends_with("\r\n\r\n"));
    }

    #[test]
    fn client_headers_keep_subprotocol_and_drop_hop_by_hop() {
        let raw = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
Sec-WebSocket-Accept: abc\r\nSec-WebSocket-Protocol: chat\r\nSec-WebSocket-Extensions: permessage-deflate\r\n\
Content-Length: 0\r\nX-Powered-By: php\r\n\r\n";
        let security = SecurityConfig {
            remove_response_headers: vec!["X-Powered-By".to_string()],
            ..Default::default()
        };
        let (h, accept, ws) = client_headers(raw, &security, true);
        assert!(ws);
        assert_eq!(accept.as_deref(), Some(&b"abc"[..]));
        let names: Vec<&[u8]> = h.iter().map(|(n, _)| n.as_ref()).collect();
        assert_eq!(
            names,
            vec![
                &b"sec-websocket-protocol"[..],
                &b"sec-websocket-extensions"[..]
            ]
        );
    }
}
