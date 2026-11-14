//! メッシュの HTTP/1.1（F-219）。下流の 1 接続をキープアライブで処理し、要求ごとにルートを選んで上流へ送る。
//!
//! - 本文はストリーミングで転送する（Content-Length・chunked はそのまま中継し、chunked は終端だけを走査）。
//! - リトライ（Envoy の `retry_policy`）は、本文を全部受け取っている（本文が無いか、最初の読み取りに収まった）
//!   要求だけ。上流へ本文を流し始めた後は行わない。
//! - WebSocket（`Upgrade`）は 101 を返した後に両方向の転送へ切り替える。
//! - 上流が HTTP/2（クラスタの設定）なら多重化接続のストリームで送り、HTTP/1.1 の応答に直して返す。

use super::httpcore::{self, ResponseFlag};
use super::l7::{self, ConnCtx, Decision, Forward, LocalReply, ReqView};
use super::upstream::{self, Alpn, UpIo};
use crate::http_utils::{ChunkedDecoder, ChunkedFeedResult};
use crate::runtime::handle::AsRawFd;
use crate::runtime::io::{AsyncReadRent, AsyncWriteRentExt, BufferedReadState};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::io;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_HEADERS: usize = 100;
pub(super) const MAX_HEAD_BYTES: usize = 60 * 1024;
const READ_CHUNK: usize = 16 * 1024;
const IDLE_TIMEOUT: Duration = Duration::from_secs(3600);
/// リトライのために手元に置ける本文の上限（Envoy の per_request_buffer_limit の既定に近い）
const RETRY_BODY_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    None,
    Length(u64),
    Chunked,
    /// 応答だけ: 接続が閉じるまで
    UntilClose,
}

/// 下流の 1 接続を処理する。
pub async fn serve<S>(mut io: S, initial: Vec<u8>, mut ctx: Arc<ConnCtx>)
where
    S: AsyncReadRent
        + AsyncWriteRentExt
        + AsRawFd
        + BufferedReadState
        + super::tcp::PendingRead
        + Unpin
        + 'static,
{
    let mut acc = BytesMut::from(&initial[..]);
    loop {
        // 要求のヘッダを読む
        let head_end = loop {
            if let Some(i) = memchr::memmem::find(&acc, b"\r\n\r\n") {
                break i + 4;
            }
            if acc.len() > MAX_HEAD_BYTES {
                let _ = write_local(
                    &mut io,
                    &LocalReply::new(431, ResponseFlag::None),
                    false,
                    &ctx,
                    true,
                )
                .await;
                return;
            }
            let idle = acc.is_empty();
            if idle && super::state::state().is_draining(ctx.direction) {
                return;
            }
            let wait = if idle {
                IDLE_TIMEOUT
            } else {
                Duration::from_secs(60)
            };
            match read_more(&mut io, &mut acc, wait).await {
                Ok(true) => {}
                _ => return,
            }
        };
        // LDS の更新で設定が変わっていれば選び直す（HTTP でなくなったら閉じる）
        ctx = match ctx.refreshed() {
            Some(c) => c,
            None => return,
        };
        let head = acc.split_to(head_end).freeze();
        let mut hbuf = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut req = httparse::Request::new(&mut hbuf);
        let parsed = req.parse(&head);
        if !matches!(parsed, Ok(httparse::Status::Complete(_))) {
            let _ = write_local(
                &mut io,
                &LocalReply::new(400, ResponseFlag::None),
                false,
                &ctx,
                true,
            )
            .await;
            return;
        }
        let mut method = sub(&head, req.method.unwrap_or("GET").as_bytes());
        let raw_target = sub(&head, req.path.unwrap_or("/").as_bytes());
        // B-315: 要求先は origin 形式にしてから正規化・RBAC・JWT・ルートの照合と上流の要求行に使う
        // （データプレーンの HTTP/1.1 と同じ。RFC 9112 §3.2）。絶対形式（`GET http://x/admin`）をそのまま
        // 使うと `/admin` の DENY・notPaths に合わず、上流は `/admin` として処理していた（F1・F2）。
        // authority は絶対形式のものを使う（RFC 9112 §3.2.2）
        let (mut path, target_authority) =
            match crate::http_utils::origin_form_target(&method, &raw_target) {
                Some(r) if r.start == 0 => (raw_target, None),
                Some(r) => {
                    let scheme_len = if raw_target[..7].eq_ignore_ascii_case(b"http://") {
                        7
                    } else {
                        8
                    };
                    let auth = raw_target.slice(scheme_len..r.start);
                    (raw_target.slice(r), Some(auth))
                }
                None => {
                    let _ = write_local(
                        &mut io,
                        &LocalReply::new(400, ResponseFlag::None),
                        false,
                        &ctx,
                        true,
                    )
                    .await;
                    return;
                }
            };
        let http10 = req.version == Some(0);
        let Some(ReqHeadScan {
            mut headers,
            mut authority,
            framing,
            mut keep_alive,
            upgrade,
            expect_continue,
        }) = scan_request_headers(&head, req.headers, http10)
        else {
            let _ = write_local(
                &mut io,
                &LocalReply::new(400, ResponseFlag::None),
                false,
                &ctx,
                true,
            )
            .await;
            return;
        };
        if let Some(a) = target_authority {
            for (k, v) in headers.iter_mut() {
                if k.as_ref() == b"host" {
                    *v = a.clone();
                }
            }
            authority = a;
        }
        let upgrade = upgrade
            && headers
                .iter()
                .any(|(k, v)| k.as_ref() == b"upgrade" && v.eq_ignore_ascii_case(b"websocket"))
            && ctx.hcm.websocket;
        if super::state::state().is_draining(ctx.direction) {
            keep_alive = false;
        }
        super::stats::DOWNSTREAM_RQ_TOTAL.fetch_add(1, Ordering::Relaxed);
        let filtered = l7::request_filters(
            &ctx,
            &mut l7::ReqParts {
                method: &mut method,
                path: &mut path,
                authority: &mut authority,
                headers: &mut headers,
                scheme: if ctx.tls { b"https" } else { b"http" },
                eos: matches!(framing, Framing::None | Framing::Length(0)),
            },
        )
        .await;
        let view = ReqView {
            method: &method,
            path: &path,
            authority: &authority,
            headers: &headers,
            scheme: if ctx.tls { b"https" } else { b"http" },
        };
        let start = Instant::now();
        let start_sys = if ctx.hcm.access_logs.is_empty() {
            std::time::UNIX_EPOCH
        } else {
            std::time::SystemTime::now()
        };
        let proto: &str = if http10 { "HTTP/1.0" } else { "HTTP/1.1" };
        let is_grpc = view.is_grpc();
        let req_body_len = match framing {
            Framing::Length(n) => n,
            _ => 0,
        };
        let decision = match filtered {
            Ok((jwt, ran)) => l7::decide(&ctx, &view, jwt, ran),
            Err(lr) => Decision::Local(lr),
        };
        match decision {
            Decision::Local(lr) => {
                // 本文は読み捨てる（キープアライブを保つ）
                let ok = discard_body(&mut io, &mut acc, framing).await.is_ok();
                let keep = keep_alive && ok;
                let _ = write_local(&mut io, &lr, is_grpc, &ctx, !keep).await;
                let labels = ctx.labels(None, None, is_grpc);
                if ctx.stats {
                    super::stats::record_request(
                        &labels,
                        lr.status,
                        lr.grpc_status,
                        lr.flag,
                        start.elapsed().as_secs_f64() * 1000.0,
                        req_body_len,
                        lr.body.len() as u64,
                    );
                }
                l7::access_log(
                    &ctx,
                    &view,
                    start_sys,
                    proto,
                    lr.status,
                    lr.flag,
                    req_body_len,
                    lr.body.len() as u64,
                    start.elapsed().as_millis() as u64,
                    None,
                    None,
                    "",
                    None,
                );
                if !keep {
                    return;
                }
            }
            Decision::Forward(f) => {
                if let Some(d) = f.delay {
                    crate::runtime::time::sleep(d).await;
                }
                if expect_continue {
                    let _ = io
                        .write_all(b"HTTP/1.1 100 Continue\r\n\r\n".to_vec())
                        .await;
                }
                let origin = view.header("origin");
                let r = forward(
                    &mut io,
                    &mut acc,
                    &ctx,
                    &f,
                    &method,
                    &path,
                    &authority,
                    &headers,
                    framing,
                    keep_alive,
                    upgrade,
                    is_grpc,
                    origin.as_ref(),
                )
                .await;
                let mut labels = ctx.labels(r.peer.clone(), r.svc.clone(), is_grpc);
                if ctx.direction != super::model::Direction::Inbound {
                    labels.peer_principal = r.up_principal.clone();
                }
                if ctx.stats {
                    super::stats::record_request(
                        &labels,
                        r.status,
                        r.grpc,
                        r.flag,
                        start.elapsed().as_secs_f64() * 1000.0,
                        r.req_bytes,
                        r.resp_bytes,
                    );
                }
                l7::access_log(
                    &ctx,
                    &view,
                    start_sys,
                    proto,
                    r.status,
                    r.flag,
                    r.req_bytes,
                    r.resp_bytes,
                    start.elapsed().as_millis() as u64,
                    r.upstream,
                    r.cluster.as_deref(),
                    &f.route().name,
                    r.upstream_ms,
                );
                l7::finish_span(&ctx, &f, &view, r.status, r.cluster.as_deref());
                #[cfg(feature = "mesh-linkerd")]
                if ctx.hcm.linkerd {
                    super::linkerd::metrics::record_http(
                        &ctx,
                        &super::linkerd::metrics::HttpDone {
                            view: &view,
                            upstream: r.upstream,
                            status: r.status,
                            grpc: r.grpc,
                            latency: start.elapsed(),
                            resp_bytes: r.resp_bytes,
                            ep_labels: r.ep_labels.as_deref(),
                            server_id: r.server_id.as_deref(),
                        },
                    );
                }
                if !r.keep_alive {
                    return;
                }
            }
        }
    }
}

fn sub(head: &Bytes, part: &[u8]) -> Bytes {
    if part.is_empty() {
        return Bytes::new();
    }
    let start = part.as_ptr() as usize - head.as_ptr() as usize;
    head.slice(start..start + part.len())
}

/// 要求のヘッダを走査した結果。
struct ReqHeadScan {
    headers: Vec<(Bytes, Bytes)>,
    authority: Bytes,
    framing: Framing,
    keep_alive: bool,
    upgrade: bool,
    expect_continue: bool,
}

/// 要求のヘッダを走査する（名前は小文字にして `head` の切り出しで持つ）。400 にする要求は `None`。
///
/// B-315: 本文の区切りが曖昧な要求（CL と TE の両方・複数の CL・最後が chunked でない TE・不正な CL）は
/// 400 にする。接続を使い回す上流へそのまま送ると、veil と上流で要求の切れ目が食い違う（F13）。
/// B-384: Host が複数ある要求も 400（RFC 9112 §3.2）。最後の Host で経路・RBAC を決めて全部を上流へ
/// 送ると、最初の Host を使う上流では許可していない仮想ホストに届く。
fn scan_request_headers(
    head: &Bytes,
    fields: &[httparse::Header<'_>],
    http10: bool,
) -> Option<ReqHeadScan> {
    let mut headers: Vec<(Bytes, Bytes)> = Vec::with_capacity(fields.len() + 8);
    let mut authority = Bytes::new();
    let mut framing = Framing::None;
    let mut te_chunked = false;
    let mut keep_alive = !http10;
    let mut upgrade = false;
    let mut expect_continue = false;
    let mut bad_framing = false;
    let mut hosts = 0u32;
    for h in fields.iter() {
        let name = Bytes::from(h.name.to_ascii_lowercase());
        let value = sub(head, h.value);
        match name.as_ref() {
            b"host" => {
                hosts += 1;
                authority = value.clone();
            }
            b"content-length" => match crate::http_utils::parse_content_length(&value) {
                // B-315: 数字だけを受ける（`+5` 等を上流と違う長さで読まない）
                Some(n) => framing = Framing::Length(n),
                None => bad_framing = true,
            },
            b"transfer-encoding" => {
                if value.windows(7).any(|w| w.eq_ignore_ascii_case(b"chunked")) {
                    te_chunked = true;
                }
            }
            b"connection" => {
                if value.windows(5).any(|w| w.eq_ignore_ascii_case(b"close")) {
                    keep_alive = false;
                }
                if value
                    .windows(10)
                    .any(|w| w.eq_ignore_ascii_case(b"keep-alive"))
                    && http10
                {
                    keep_alive = true;
                }
                if value.windows(7).any(|w| w.eq_ignore_ascii_case(b"upgrade")) {
                    upgrade = true;
                }
            }
            b"expect" => expect_continue = value.eq_ignore_ascii_case(b"100-continue"),
            _ => {}
        }
        headers.push((name, value));
    }
    if bad_framing
        || hosts > 1
        || crate::http_utils::classify_request_framing(
            headers.iter().map(|(k, v)| (k.as_ref(), v.as_ref())),
        )
        .is_err()
    {
        return None;
    }
    if te_chunked {
        framing = Framing::Chunked;
    }
    Some(ReqHeadScan {
        headers,
        authority,
        framing,
        keep_alive,
        upgrade,
        expect_continue,
    })
}

/// 読めたら true、EOF・エラー・期限切れは false。
async fn read_more<S: AsyncReadRent>(
    io: &mut S,
    acc: &mut BytesMut,
    wait: Duration,
) -> io::Result<bool> {
    let buf: Vec<u8> = Vec::with_capacity(READ_CHUNK);
    match crate::runtime::timeout(wait, io.read(buf)).await {
        Ok((Ok(0), _)) => Ok(false),
        Ok((Ok(n), buf)) => {
            acc.extend_from_slice(&buf[..n]);
            Ok(true)
        }
        Ok((Err(e), _)) => Err(e),
        Err(_) => Ok(false),
    }
}

/// ローカル応答を書く（gRPC の要求には Trailers-Only 相当で grpc-status を返す）。
async fn write_local<S: AsyncWriteRentExt>(
    io: &mut S,
    lr: &LocalReply,
    is_grpc: bool,
    ctx: &ConnCtx,
    close: bool,
) -> io::Result<()> {
    let mut b = BytesMut::with_capacity(256 + lr.body.len());
    let status = if is_grpc && lr.grpc_status.is_some() {
        200
    } else {
        lr.status
    };
    status_line(&mut b, status);
    for (k, v) in &lr.headers {
        put_header(&mut b, k, v);
    }
    if is_grpc {
        if let Some(g) = lr.grpc_status {
            let mut it = itoa::Buffer::new();
            put_header(&mut b, b"content-type", b"application/grpc");
            put_header(&mut b, b"grpc-status", it.format(g).as_bytes());
            put_header(&mut b, b"grpc-message", &lr.body);
            put_header(&mut b, b"content-length", b"0");
        }
    }
    if !(is_grpc && lr.grpc_status.is_some()) {
        let mut it = itoa::Buffer::new();
        put_header(
            &mut b,
            b"content-length",
            it.format(lr.body.len()).as_bytes(),
        );
        if !lr.body.is_empty() {
            put_header(&mut b, b"content-type", b"text/plain");
        }
    }
    put_header(&mut b, b"date", httpdate_now().as_bytes());
    if ctx.hcm.linkerd {
        // プロキシが作った応答の印（本家の linkerd2-proxy と同じ）
        put_header(
            &mut b,
            b"l5d-proxy-error",
            httpcore::reason(lr.status).as_bytes(),
        );
    } else {
        put_header(&mut b, b"server", &ctx.hcm.server_name);
    }
    if close {
        put_header(&mut b, b"connection", b"close");
    }
    b.put_slice(b"\r\n");
    if !(is_grpc && lr.grpc_status.is_some()) {
        b.put_slice(&lr.body);
    }
    let (r, _) = io.write_all(b.freeze()).await;
    r.map(|_| ())
}

fn status_line(b: &mut BytesMut, status: u16) {
    let mut it = itoa::Buffer::new();
    b.put_slice(b"HTTP/1.1 ");
    b.put_slice(it.format(status).as_bytes());
    b.put_u8(b' ');
    b.put_slice(httpcore::reason(status).as_bytes());
    b.put_slice(b"\r\n");
}

fn put_header(b: &mut BytesMut, k: &[u8], v: &[u8]) {
    b.put_slice(k);
    b.put_slice(b": ");
    b.put_slice(v);
    b.put_slice(b"\r\n");
}

/// RFC 7231 の日付（秒単位でキャッシュ）。
pub fn httpdate_now() -> String {
    thread_local! {
        static CACHE: std::cell::RefCell<(u64, String)> = const { std::cell::RefCell::new((0, String::new())) };
    }
    let now = httpcore::unix_secs();
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.0 != now || c.1.is_empty() {
            let dt = time::OffsetDateTime::from_unix_timestamp(now as i64)
                .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
            const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
            const MONTHS: [&str; 12] = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ];
            c.1 = format!(
                "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
                DAYS[dt.weekday().number_days_from_monday() as usize],
                dt.day(),
                MONTHS[dt.month() as usize - 1],
                dt.year(),
                dt.hour(),
                dt.minute(),
                dt.second()
            );
            c.0 = now;
        }
        c.1.clone()
    })
}

/// chunked の本文のフレーミングが不正（下流の要求なら 400 で閉じる。B-369）。
#[derive(Debug)]
struct BadChunked;

impl std::fmt::Display for BadChunked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("bad chunked body")
    }
}

impl std::error::Error for BadChunked {}

fn is_bad_chunked(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|x| x.is::<BadChunked>())
}

/// chunked の本文の終端を探す（中身はそのまま転送する）。終端が `data` の中にあればその直後の位置を返す。
///
/// B-369: 要求・応答とも厳格に読む（RFC 9112 §7.1。行末は CRLF だけ・サイズの無い行やチャンク拡張・トレーラの中の
/// LF は不正・フレーミングの量に上限）。フレーミングをそのまま相手へ送るので、緩く読むと相手と本文の終わりの
/// 解釈がずれ、後ろのバイトが相手の側で veil の通していない別の要求（応答）になる（以前の走査は拡張・トレーラの
/// 中の裸の LF と空のサイズの行を受け入れていた）。
fn chunked_end(dec: &mut ChunkedDecoder, data: &[u8]) -> io::Result<Option<usize>> {
    match dec.feed_counted(data) {
        (ChunkedFeedResult::Complete, end) => Ok(Some(end)),
        (ChunkedFeedResult::Continue, _) => Ok(None),
        (ChunkedFeedResult::Invalid | ChunkedFeedResult::SizeLimitExceeded, _) => {
            Err(io::Error::new(io::ErrorKind::InvalidData, BadChunked))
        }
    }
}

/// 本文を読み捨てる（ローカル応答のあと接続を保つため）。
async fn discard_body<S: AsyncReadRent>(
    io: &mut S,
    acc: &mut BytesMut,
    framing: Framing,
) -> io::Result<()> {
    match framing {
        Framing::None | Framing::UntilClose => Ok(()),
        Framing::Length(mut n) => loop {
            let take = (acc.len() as u64).min(n) as usize;
            let _ = acc.split_to(take);
            n -= take as u64;
            if n == 0 {
                return Ok(());
            }
            if n > 1024 * 1024 {
                return Err(io::Error::other("body too large to discard"));
            }
            if !read_more(io, acc, Duration::from_secs(30)).await? {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
        },
        Framing::Chunked => {
            // 読み捨てる本文は Content-Length と同じく 1MiB まで
            let mut dec = ChunkedDecoder::new_request(1024 * 1024);
            loop {
                match chunked_end(&mut dec, acc)? {
                    Some(end) => {
                        let _ = acc.split_to(end);
                        return Ok(());
                    }
                    None => acc.clear(),
                }
                if !read_more(io, acc, Duration::from_secs(30)).await? {
                    return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                }
            }
        }
    }
}

/// 下流の本文を上流へ流す（`acc` は読み済みの分。本文より後ろは残す＝パイプライン）。
async fn copy_req_body<R, W>(
    r: &mut R,
    acc: &mut BytesMut,
    w: &mut W,
    framing: Framing,
) -> io::Result<u64>
where
    R: AsyncReadRent,
    W: AsyncWriteRentExt,
{
    let mut total = 0u64;
    match framing {
        Framing::None | Framing::UntilClose => Ok(0),
        Framing::Length(mut n) => {
            while n > 0 {
                if acc.is_empty() && !read_more(r, acc, Duration::from_secs(300)).await? {
                    return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                }
                let take = (acc.len() as u64).min(n) as usize;
                let chunk = acc.split_to(take).freeze();
                n -= take as u64;
                total += take as u64;
                let (res, _) = w.write_all(chunk).await;
                res?;
            }
            Ok(total)
        }
        Framing::Chunked => {
            let mut dec = ChunkedDecoder::new_request(0);
            loop {
                if acc.is_empty() && !read_more(r, acc, Duration::from_secs(300)).await? {
                    return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                }
                let end = chunked_end(&mut dec, acc)?;
                let take = end.unwrap_or(acc.len());
                let chunk = acc.split_to(take).freeze();
                total += take as u64;
                let (res, _) = w.write_all(chunk).await;
                res?;
                if end.is_some() {
                    return Ok(total);
                }
            }
        }
    }
}

/// 本文が全部手元にあれば、その範囲（リトライで再送できる）。
fn buffered_body(acc: &BytesMut, framing: Framing) -> Option<usize> {
    match framing {
        Framing::None | Framing::UntilClose => Some(0),
        Framing::Length(n) => {
            (n as usize <= acc.len() && n as usize <= RETRY_BODY_LIMIT).then_some(n as usize)
        }
        Framing::Chunked => {
            let mut dec = ChunkedDecoder::new_request(0);
            match chunked_end(&mut dec, acc) {
                Ok(Some(end)) if end <= RETRY_BODY_LIMIT => Some(end),
                _ => None,
            }
        }
    }
}

pub(crate) struct Outcome {
    pub status: u16,
    pub grpc: Option<u32>,
    pub flag: ResponseFlag,
    pub keep_alive: bool,
    pub req_bytes: u64,
    pub resp_bytes: u64,
    pub peer: Option<Arc<super::peer::Workload>>,
    pub svc: Option<Arc<super::stats::ServiceLabels>>,
    /// アクセスログ用（上流のアドレス・クラスタ・上流の所要時間）
    pub upstream: Option<std::net::SocketAddr>,
    pub cluster: Option<Arc<super::model::ClusterCfg>>,
    pub upstream_ms: Option<u64>,
    /// 選んだエンドポイントの観測のラベルと相手の ID（Linkerd のメトリクス）
    pub ep_labels: Option<Arc<str>>,
    pub server_id: Option<Arc<str>>,
    /// 上流の ID（outbound の destination_principal）
    pub up_principal: Option<Arc<str>>,
}

/// 上流の応答のヘッダ。
struct RespHead {
    status: u16,
    headers: Vec<(Bytes, Bytes)>,
    framing: Framing,
    upstream_keep_alive: bool,
}

async fn read_resp_head(up: &mut UpIo, buf: &mut BytesMut, head_req: bool) -> io::Result<RespHead> {
    loop {
        if let Some(i) = memchr::memmem::find(buf, b"\r\n\r\n") {
            let head = buf.split_to(i + 4).freeze();
            let mut hbuf = [httparse::EMPTY_HEADER; MAX_HEADERS];
            let mut resp = httparse::Response::new(&mut hbuf);
            match resp.parse(&head) {
                Ok(httparse::Status::Complete(_)) => {}
                _ => return Err(io::Error::other("bad upstream response")),
            }
            let status = resp.code.unwrap_or(502);
            if (100..200).contains(&status) && status != 101 {
                // 1xx は捨てて次を待つ
                continue;
            }
            let http10 = resp.version == Some(0);
            let mut headers = Vec::with_capacity(resp.headers.len() + 4);
            // 長さの指定が無ければ接続を閉じるまで（HTTP/1.0 も同じ）
            let mut framing = Framing::UntilClose;
            let mut has_len = false;
            let mut keep = !http10;
            for h in resp.headers.iter() {
                let name = Bytes::from(h.name.to_ascii_lowercase());
                let value = sub(&head, h.value);
                match name.as_ref() {
                    b"content-length" => {
                        // B-315: 要求と同じく数字だけを受ける（不正な値は接続を閉じるまでの本文として扱う）
                        if let Some(n) = crate::http_utils::parse_content_length(&value) {
                            framing = Framing::Length(n);
                            has_len = true;
                        } else {
                            keep = false;
                        }
                    }
                    b"transfer-encoding" => {
                        if value.windows(7).any(|w| w.eq_ignore_ascii_case(b"chunked")) {
                            framing = Framing::Chunked;
                        }
                    }
                    b"connection" if value.windows(5).any(|w| w.eq_ignore_ascii_case(b"close")) => {
                        keep = false;
                    }
                    _ => {}
                }
                headers.push((name, value));
            }
            let _ = has_len;
            if head_req || status == 204 || status == 304 || status == 101 {
                framing = Framing::None;
            }
            if framing == Framing::UntilClose {
                keep = false;
            }
            return Ok(RespHead {
                status,
                headers,
                framing,
                upstream_keep_alive: keep,
            });
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err(io::Error::other("upstream response head too large"));
        }
        let b: Vec<u8> = Vec::with_capacity(READ_CHUNK);
        let (r, b) = up.read(b).await;
        match r {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Ok(n) => buf.extend_from_slice(&b[..n]),
            Err(e) => return Err(e),
        }
    }
}

/// 上流への要求ヘッダ部（要求行 + ヘッダ）。
fn request_head(
    method: &[u8],
    path: &[u8],
    headers: &[(Bytes, Bytes)],
    host: &[u8],
    host_set: bool,
) -> Bytes {
    let mut b = BytesMut::with_capacity(256 + headers.len() * 48);
    b.put_slice(method);
    b.put_u8(b' ');
    b.put_slice(path);
    b.put_slice(b" HTTP/1.1\r\n");
    let mut wrote_host = false;
    for (k, v) in headers {
        if k.as_ref() == b"host" {
            if host_set {
                continue;
            }
            wrote_host = true;
        }
        put_header(&mut b, k, v);
    }
    if host_set || !wrote_host {
        put_header(&mut b, b"host", host);
    }
    b.put_slice(b"\r\n");
    b.freeze()
}

#[allow(clippy::too_many_arguments)]
async fn forward<S>(
    io: &mut S,
    acc: &mut BytesMut,
    ctx: &ConnCtx,
    f: &Forward,
    method: &Bytes,
    path: &Bytes,
    authority: &Bytes,
    headers: &[(Bytes, Bytes)],
    framing: Framing,
    keep_alive: bool,
    upgrade: bool,
    is_grpc: bool,
    origin: Option<&Bytes>,
) -> Outcome
where
    S: AsyncReadRent
        + AsyncWriteRentExt
        + AsRawFd
        + BufferedReadState
        + super::tcp::PendingRead
        + Unpin
        + 'static,
{
    let svc = f
        .cluster
        .cfg
        .load()
        .as_ref()
        .as_ref()
        .and_then(|c| c.svc_labels.clone());
    let mut out = Outcome {
        status: 0,
        grpc: None,
        flag: ResponseFlag::None,
        keep_alive,
        req_bytes: 0,
        resp_bytes: 0,
        peer: None,
        svc,
        upstream: None,
        cluster: None,
        upstream_ms: None,
        ep_labels: None,
        server_id: None,
        up_principal: None,
    };
    let fail = |status: u16, flag: ResponseFlag| LocalReply::new(status, flag);
    let retry = f.action.retry.clone().or_else(|| f.vhost().retry.clone());
    let max_attempts = 1 + retry.as_ref().map_or(0, |r| r.num_retries);
    let body_len = buffered_body(acc, framing);
    let replayable = body_len.is_some();
    let new_path = l7::rewritten_path(f, path);
    let up_path: &[u8] = new_path.as_deref().unwrap_or(path);
    let host_rw: Option<Bytes> = f.action.host_rewrite.clone();
    let mut tried: Vec<std::net::SocketAddr> = Vec::new();
    let deadline = f.action.timeout.map(|t| Instant::now() + t);
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let picked = match upstream::pick(&f.cluster, ctx.orig_dst, f.hash, &tried) {
            // B-375: auto_sni のクラスタは上流へ送る Host を TLS の名前にする
            Ok(p) => p.with_request_host(host_rw.as_ref().unwrap_or(authority)),
            Err(flag) => {
                let lr = fail(503, flag);
                out.status = 503;
                out.flag = flag;
                let _ = discard_body(io, acc, framing).await;
                let _ = write_local(io, &lr, is_grpc, ctx, !keep_alive).await;
                return out;
            }
        };
        out.upstream = Some(picked.addr);
        out.ep_labels = picked.endpoint().and_then(|e| e.metric_labels.clone());
        out.server_id = picked
            .tls
            .as_ref()
            .filter(|t| t.mesh_mtls && t.transport.is_some())
            .map(|t| Arc::<str>::from(t.sni.as_str()));
        out.up_principal = picked
            .tls
            .as_ref()
            .filter(|t| t.mesh_mtls)
            .and_then(|t| t.principal.clone());
        out.cluster = Some(picked.cfg.clone());
        let downstream_h2 = false;
        // Linkerd: 相手のプロキシが HTTP/2 を受けられる（ProtocolHint.H2）なら昇格して送る（`l5d-orig-proto`）
        let up_h2 = (l7::upstream_is_h2(&picked.cfg, downstream_h2)
            || (ctx.hcm.linkerd && picked.endpoint().is_some_and(|e| e.h2_upgrade)))
            && !upgrade;
        let _admission = match upstream::admit(&f.cluster, &picked.cfg, up_h2) {
            Ok(a) => a,
            Err(flag) => {
                out.status = 503;
                out.flag = flag;
                let _ = discard_body(io, acc, framing).await;
                let _ = write_local(io, &fail(503, flag), is_grpc, ctx, !keep_alive).await;
                return out;
            }
        };
        if attempt == 1 && !f.action.mirrors.is_empty() {
            if let Some(n) = body_len {
                super::mirror::spawn_h1(
                    f,
                    ctx,
                    method,
                    up_path,
                    authority,
                    headers,
                    Bytes::copy_from_slice(&acc[..n]),
                    framing == Framing::Chunked,
                );
            }
        }
        if up_h2 {
            // 本文を全部読めていて試行が残っていればリトライできる（Linkerd の昇格した HTTP/2 も同じ）
            let again = H2Again {
                retry: retry.as_deref().filter(|_| attempt < max_attempts),
                body_len: body_len.filter(|_| framing != Framing::Chunked),
                deadline,
                attempt,
            };
            match forward_to_h2(
                io, acc, ctx, f, &picked, method, up_path, authority, headers, framing, keep_alive,
                is_grpc, out, origin, again,
            )
            .await
            {
                H2Result::Done(o) => return o,
                H2Result::Retry(o) => {
                    out = o;
                    tried.push(picked.addr);
                    continue;
                }
            }
        }
        let (req_headers, peer_req) = l7::upstream_request_headers(
            ctx,
            f,
            headers,
            attempt,
            upgrade,
            framing == Framing::Chunked,
        );
        let host: Bytes = host_rw.clone().unwrap_or_else(|| authority.clone());
        let head = request_head(method, up_path, &req_headers, &host, host_rw.is_some());
        picked.begin();
        let started = Instant::now();
        // 接続（プールから、無ければ新しく）
        let (mut up, uses, reused) = match upstream::h1_take(&picked) {
            Some((u, n)) => (u, n, true),
            None => match upstream::connect(&picked, Alpn::H1).await {
                Ok(u) => (u, 0, false),
                Err(e) => {
                    picked.end(false, true);
                    ftlog::debug!("[mesh] connect to {} failed: {}", picked.addr, e);
                    tried.push(picked.addr);
                    let can_retry = retry
                        .as_ref()
                        .is_some_and(|r| r.on_connect_failure || r.on_reset_before_request)
                        && attempt < max_attempts
                        && replayable;
                    if can_retry {
                        continue;
                    }
                    // Envoy は接続のタイムアウトも UF（上流への接続の失敗）
                    let flag = ResponseFlag::UpstreamConnectFailure;
                    out.status = 503;
                    out.flag = if attempt > 1 {
                        ResponseFlag::UpstreamRetryLimit
                    } else {
                        flag
                    };
                    out.peer = l7::peer_workload(ctx, peer_req, None, picked.endpoint());
                    let _ = discard_body(io, acc, framing).await;
                    let _ = write_local(io, &fail(503, out.flag), is_grpc, ctx, !keep_alive).await;
                    return out;
                }
            },
        };
        // 要求を送る
        let send = async {
            let (r, _) = up.write_all(head.clone()).await;
            r?;
            match body_len {
                Some(n) if n > 0 => {
                    let body = Bytes::copy_from_slice(&acc[..n]);
                    let (r, _) = up.write_all(body).await;
                    r?;
                    Ok::<u64, io::Error>(n as u64)
                }
                Some(_) => Ok(0),
                None => copy_req_body(io, acc, &mut up, framing).await,
            }
        };
        let sent = match deadline {
            Some(d) => crate::runtime::timeout(d.saturating_duration_since(Instant::now()), send)
                .await
                .unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "timeout"))),
            None => send.await,
        };
        if let Err(e) = sent {
            if is_bad_chunked(&e) {
                // B-369: 下流の本文のフレーミングが不正。途中まで送った上流の接続は捨て、400 で閉じる
                // （上流の失敗には数えない）
                picked.end(false, false);
                out.status = 400;
                out.flag = ResponseFlag::DownstreamProtocolError;
                out.keep_alive = false;
                let _ = write_local(io, &fail(400, out.flag), is_grpc, ctx, true).await;
                return out;
            }
            picked.end(false, true);
            tried.push(picked.addr);
            // 再利用した接続が既に切れていた（送信前に気付けない）なら本文を再送できるときだけやり直す
            if replayable
                && (reused
                    || retry
                        .as_ref()
                        .is_some_and(|r| r.on_reset || r.on_reset_before_request))
                && attempt < max_attempts + reused as u32
            {
                continue;
            }
            ftlog::debug!("[mesh] send to {} failed: {}", picked.addr, e);
            out.status = 503;
            out.flag = ResponseFlag::UpstreamConnectionTermination;
            out.keep_alive = false;
            let _ = write_local(io, &fail(503, out.flag), is_grpc, ctx, true).await;
            return out;
        }
        out.req_bytes = sent.unwrap_or(0);
        // 応答のヘッダ
        let mut rbuf = BytesMut::with_capacity(READ_CHUNK);
        let per_try = retry.as_ref().and_then(|r| r.per_try_timeout);
        let wait = match (per_try, deadline) {
            (Some(p), Some(d)) => Some(p.min(d.saturating_duration_since(Instant::now()))),
            (Some(p), None) => Some(p),
            (None, Some(d)) => Some(d.saturating_duration_since(Instant::now())),
            (None, None) => None,
        };
        let head_res = match wait {
            Some(w) => match crate::runtime::timeout(
                w,
                read_resp_head(&mut up, &mut rbuf, method.as_ref() == b"HEAD"),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "upstream timeout")),
            },
            None => read_resp_head(&mut up, &mut rbuf, method.as_ref() == b"HEAD").await,
        };
        let rh = match head_res {
            Ok(h) => h,
            Err(e) => {
                picked.end(false, true);
                tried.push(picked.addr);
                let timed_out = e.kind() == io::ErrorKind::TimedOut;
                let can_retry = replayable
                    && attempt < max_attempts + (reused && !timed_out) as u32
                    && (reused
                        || retry.as_ref().is_some_and(|r| {
                            r.on_reset || (timed_out && r.per_try_timeout.is_some())
                        }));
                if can_retry {
                    continue;
                }
                let (status, flag) = if timed_out {
                    (504, ResponseFlag::UpstreamTimeout)
                } else {
                    (503, ResponseFlag::UpstreamConnectionTermination)
                };
                out.status = status;
                out.flag = flag;
                out.keep_alive = false;
                out.peer = l7::peer_workload(ctx, peer_req, None, picked.endpoint());
                let _ = write_local(io, &fail(status, flag), is_grpc, ctx, true).await;
                return out;
            }
        };
        let grpc_hdr = rh
            .headers
            .iter()
            .find(|(k, _)| k.as_ref() == b"grpc-status")
            .and_then(|(_, v)| std::str::from_utf8(v).ok()?.parse().ok());
        if let Some(rp) = &retry {
            if attempt < max_attempts && replayable && l7::retry_on_status(rp, rh.status, grpc_hdr)
            {
                picked.end(rh.status >= 500, matches!(rh.status, 502..=504));
                tried.push(picked.addr);
                // 応答の本文は読まずに接続ごと捨てる
                continue;
            }
        }
        let upstream_ms = started.elapsed().as_millis() as u64;
        out.upstream_ms = Some(upstream_ms);
        let mut resp_headers = rh.headers;
        if let Err(lr) = f.response_filters(rh.status, &mut resp_headers).await {
            // Wasm のローカル応答（B-300）。上流の本文は読まないので上流の接続は捨てる
            picked.end(false, false);
            out.status = lr.status;
            out.keep_alive = false;
            out.peer = l7::peer_workload(ctx, peer_req, None, picked.endpoint());
            let _ = write_local(io, &lr, is_grpc, ctx, true).await;
            return out;
        }
        let peer_resp = l7::downstream_response_headers(
            ctx,
            Some(f),
            &mut resp_headers,
            Some(upstream_ms),
            origin,
        );
        out.peer = l7::peer_workload(ctx, peer_req, peer_resp, picked.endpoint());
        out.status = rh.status;
        out.grpc = grpc_hdr;
        // 下流へヘッダ
        let mut keep = keep_alive && rh.status != 101;
        let mut hb = BytesMut::with_capacity(256 + resp_headers.len() * 48);
        status_line(&mut hb, rh.status);
        for (k, v) in &resp_headers {
            if k.as_ref() == b"connection" || k.as_ref() == b"keep-alive" {
                continue;
            }
            if rh.status == 101 && k.as_ref() == b"upgrade" {
                put_header(&mut hb, k, v);
                continue;
            }
            if httpcore::is_hop_by_hop(k)
                && !(k.as_ref() == b"transfer-encoding" && rh.framing == Framing::Chunked)
            {
                continue;
            }
            put_header(&mut hb, k, v);
        }
        if rh.status == 101 {
            put_header(&mut hb, b"connection", b"upgrade");
        } else if rh.framing == Framing::UntilClose {
            // 長さの分からない応答は chunked で返す（下流の接続を保てる）
            put_header(&mut hb, b"transfer-encoding", b"chunked");
        }
        // 101 は `connection: upgrade` だけ（close を並べると WebSocket のクライアントが拒否する）
        if !keep && rh.status != 101 {
            put_header(&mut hb, b"connection", b"close");
        }
        hb.put_slice(b"\r\n");
        let (wr, _) = io.write_all(hb.freeze()).await;
        if wr.is_err() {
            picked.end(false, false);
            out.flag = ResponseFlag::DownstreamConnectionTermination;
            out.keep_alive = false;
            return out;
        }
        if rh.status == 101 {
            // WebSocket: 残っている分を渡してから両方向の転送
            if !rbuf.is_empty() {
                let (r, _) = io.write_all(rbuf.split().freeze()).await;
                let _ = r;
            }
            if !acc.is_empty() {
                let (r, _) = up.write_all(acc.split().freeze()).await;
                let _ = r;
            }
            let (a, b) = super::tcp::relay(io, &mut up, Duration::from_secs(3600)).await;
            picked.end(false, false);
            out.req_bytes += a;
            out.resp_bytes += b;
            out.keep_alive = false;
            return out;
        }
        // 本文
        let body_res = copy_resp_body(&mut up, &mut rbuf, io, rh.framing).await;
        picked.end(rh.status >= 500, matches!(rh.status, 502..=504));
        match body_res {
            Ok(n) => {
                out.resp_bytes = n;
                if rh.upstream_keep_alive && rbuf.is_empty() {
                    upstream::h1_put(&picked, up, uses + 1);
                }
            }
            Err(_) => {
                keep = false;
                out.flag = ResponseFlag::UpstreamConnectionTermination;
            }
        }
        // 本文を全部手元で送った要求は、下流の読み残しを捨てる
        if let Some(n) = body_len {
            let _ = acc.split_to(n.min(acc.len()));
        }
        out.keep_alive = keep;
        return out;
    }
}

/// 上流の応答本文を下流へ（長さ不明は chunked に直す）。
async fn copy_resp_body<W: AsyncWriteRentExt>(
    up: &mut UpIo,
    rbuf: &mut BytesMut,
    w: &mut W,
    framing: Framing,
) -> io::Result<u64> {
    match framing {
        Framing::None => Ok(0),
        Framing::Length(_) | Framing::Chunked => copy_req_body(up, rbuf, w, framing).await,
        Framing::UntilClose => {
            let mut total = 0u64;
            loop {
                if rbuf.is_empty() {
                    let b: Vec<u8> = Vec::with_capacity(READ_CHUNK);
                    let (r, b) = up.read(b).await;
                    match r {
                        Ok(0) => break,
                        Ok(n) => rbuf.extend_from_slice(&b[..n]),
                        Err(e) => return Err(e),
                    }
                }
                let chunk = rbuf.split();
                total += chunk.len() as u64;
                let mut c = BytesMut::with_capacity(chunk.len() + 16);
                c.put_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
                c.put_slice(&chunk);
                c.put_slice(b"\r\n");
                let (r, _) = w.write_all(c.freeze()).await;
                r?;
            }
            let (r, _) = w.write_all(Bytes::from_static(b"0\r\n\r\n")).await;
            r?;
            Ok(total)
        }
    }
}

/// HTTP/1.1 の下流 → HTTP/2 の上流（クラスタが HTTP/2 を求める場合）。
#[allow(clippy::too_many_arguments)]
async fn forward_to_h2<S>(
    io: &mut S,
    acc: &mut BytesMut,
    ctx: &ConnCtx,
    f: &Forward,
    picked: &upstream::Picked,
    method: &Bytes,
    path: &[u8],
    authority: &Bytes,
    headers: &[(Bytes, Bytes)],
    framing: Framing,
    keep_alive: bool,
    is_grpc: bool,
    mut out: Outcome,
    origin: Option<&Bytes>,
    again: H2Again<'_>,
) -> H2Result
where
    S: AsyncReadRent
        + AsyncWriteRentExt
        + AsRawFd
        + BufferedReadState
        + super::tcp::PendingRead
        + Unpin
        + 'static,
{
    use crate::http2::upstream_mux::UpResp;
    let (mut req_headers, peer_req) =
        l7::upstream_request_headers(ctx, f, headers, again.attempt, false, false);
    if ctx.hcm.linkerd && !matches!(picked.cfg.proto, super::model::UpstreamProto::Http2) {
        req_headers.push((
            Bytes::from_static(b"l5d-orig-proto"),
            Bytes::from_static(b"HTTP/1.1"),
        ));
    }
    let host = f
        .action
        .host_rewrite
        .clone()
        .unwrap_or_else(|| authority.clone());
    let started = Instant::now();
    picked.begin();
    let mux = match upstream::h2_mux(picked).await {
        Ok(m) => m,
        Err(_) => {
            picked.end(false, true);
            if again.body_len.is_some()
                && again
                    .retry
                    .is_some_and(|r| r.on_connect_failure || r.on_reset_before_request)
            {
                return H2Result::Retry(out);
            }
            out.status = 503;
            out.flag = ResponseFlag::UpstreamConnectFailure;
            let _ = discard_body(io, acc, framing).await;
            let _ = write_local(
                io,
                &LocalReply::new(503, out.flag),
                is_grpc,
                ctx,
                !keep_alive,
            )
            .await;
            return H2Result::Done(out);
        }
    };
    let no_body = framing == Framing::None || again.body_len == Some(0);
    let Some(mut st) = mux.open(
        method,
        path,
        &host,
        req_headers.iter().map(|(k, v)| (k.as_ref(), v.as_ref())),
        no_body,
    ) else {
        picked.end(false, true);
        out.status = 503;
        out.flag = ResponseFlag::UpstreamConnectFailure;
        let _ = write_local(
            io,
            &LocalReply::new(503, out.flag),
            is_grpc,
            ctx,
            !keep_alive,
        )
        .await;
        return H2Result::Done(out);
    };
    // 本文: リトライできるときは読み込み済みの本文を複製して送る（応答を見るまで手元に残す）
    let replay = again.retry.is_some() && again.body_len.is_some();
    if replay {
        if let (Some(n), Some(tx)) = (again.body_len, &st.req_tx) {
            if n > 0 {
                out.req_bytes = n as u64;
                let _ = tx.send(Bytes::copy_from_slice(&acc[..n])).await;
            }
        }
        st.finish_request();
    }
    // 本文: Content-Length はそのまま、chunked は中身を取り出して送る
    if !no_body && !replay {
        let r = match framing {
            Framing::Length(mut n) => {
                let mut ok = true;
                while n > 0 {
                    if acc.is_empty()
                        && !read_more(io, acc, Duration::from_secs(300))
                            .await
                            .unwrap_or(false)
                    {
                        ok = false;
                        break;
                    }
                    let take = (acc.len() as u64).min(n) as usize;
                    let chunk = acc.split_to(take).freeze();
                    n -= take as u64;
                    out.req_bytes += take as u64;
                    if let Some(tx) = &st.req_tx {
                        if tx.send(chunk).await.is_err() {
                            ok = false;
                            break;
                        }
                    }
                }
                ok
            }
            Framing::Chunked => dechunk_into(io, acc, &st, &mut out.req_bytes).await,
            _ => true,
        };
        st.finish_request();
        if !r {
            picked.end(false, true);
            out.status = 400;
            out.keep_alive = false;
            return H2Result::Done(out);
        }
    }
    let mut keep = keep_alive;
    let mut head_sent = false;
    let mut trailers_grpc = None;
    loop {
        // 応答のヘッダまでのタイムアウト（ルートの timeout。Linkerd の HTTPRoute の timeouts.request も）
        let item = match again.deadline.filter(|_| !head_sent) {
            Some(d) => {
                let left = d.saturating_duration_since(Instant::now());
                match crate::runtime::time::timeout(left, st.resp_rx.recv()).await {
                    Ok(i) => i,
                    Err(_) => {
                        picked.end(false, true);
                        out.status = 504;
                        out.flag = ResponseFlag::UpstreamTimeout;
                        if let Some(n) = again.body_len.filter(|_| replay) {
                            acc.advance(n.min(acc.len()));
                        }
                        let _ = write_local(
                            io,
                            &LocalReply::new(504, out.flag),
                            is_grpc,
                            ctx,
                            !keep_alive,
                        )
                        .await;
                        out.keep_alive = keep_alive;
                        return H2Result::Done(out);
                    }
                }
            }
            None => st.resp_rx.recv().await,
        };
        // リトライ（応答のステータス・gRPC のステータス、接続の切断）
        if replay && !head_sent {
            let grpc = match &item {
                Some(UpResp::Head { headers, .. }) => headers
                    .iter()
                    .find(|(k, _)| k.as_ref() == b"grpc-status")
                    .and_then(|(_, v)| std::str::from_utf8(v).ok()?.parse().ok()),
                _ => None,
            };
            let again_now = match (&item, again.retry) {
                (Some(UpResp::Head { status, .. }), Some(r)) => {
                    l7::retry_on_status(r, *status, grpc)
                }
                (Some(UpResp::Reset { .. }) | None, Some(r)) => r.on_reset || r.on_refused_stream,
                _ => false,
            };
            if again_now {
                let st5 = matches!(&item, Some(UpResp::Head { status, .. }) if *status >= 500);
                picked.end(st5, !st5);
                return H2Result::Retry(out);
            }
            if let Some(n) = again.body_len {
                acc.advance(n.min(acc.len()));
            }
        }
        match item {
            Some(UpResp::Head {
                status,
                mut headers,
            }) => {
                if let Err(lr) = f.response_filters(status, &mut headers).await {
                    // Wasm のローカル応答（B-300）
                    picked.end(false, false);
                    out.status = lr.status;
                    out.keep_alive = false;
                    let _ = write_local(io, &lr, is_grpc, ctx, true).await;
                    return H2Result::Done(out);
                }
                let peer_resp = l7::downstream_response_headers(
                    ctx,
                    Some(f),
                    &mut headers,
                    Some(started.elapsed().as_millis() as u64),
                    origin,
                );
                out.peer = l7::peer_workload(ctx, peer_req.clone(), peer_resp, picked.endpoint());
                out.status = status;
                let mut hb = BytesMut::with_capacity(256);
                status_line(&mut hb, status);
                for (k, v) in &headers {
                    if httpcore::is_hop_by_hop(k) || k.as_ref() == b"content-length" {
                        continue;
                    }
                    put_header(&mut hb, k, v);
                }
                put_header(&mut hb, b"transfer-encoding", b"chunked");
                if !keep {
                    put_header(&mut hb, b"connection", b"close");
                }
                hb.put_slice(b"\r\n");
                if io.write_all(hb.freeze()).await.0.is_err() {
                    keep = false;
                    break;
                }
                head_sent = true;
            }
            Some(UpResp::Data(d)) => {
                out.resp_bytes += d.len() as u64;
                let mut c = BytesMut::with_capacity(d.len() + 16);
                c.put_slice(format!("{:x}\r\n", d.len()).as_bytes());
                c.put_slice(&d);
                c.put_slice(b"\r\n");
                if io.write_all(c.freeze()).await.0.is_err() {
                    keep = false;
                    break;
                }
            }
            Some(UpResp::Trailers(t)) => {
                trailers_grpc = t
                    .iter()
                    .find(|(k, _)| k.as_ref() == b"grpc-status")
                    .and_then(|(_, v)| std::str::from_utf8(v).ok()?.parse().ok());
                let mut c = BytesMut::from(&b"0\r\n"[..]);
                for (k, v) in &t {
                    put_header(&mut c, k, v);
                }
                c.put_slice(b"\r\n");
                let _ = io.write_all(c.freeze()).await;
                picked.end(out.status >= 500, false);
                out.grpc = trailers_grpc;
                out.keep_alive = keep;
                return H2Result::Done(out);
            }
            Some(UpResp::Reset { .. }) => {
                keep = false;
                out.flag = ResponseFlag::UpstreamConnectionTermination;
                if !head_sent {
                    out.status = 503;
                    let _ =
                        write_local(io, &LocalReply::new(503, out.flag), is_grpc, ctx, true).await;
                }
                break;
            }
            None => {
                if head_sent {
                    let _ = io.write_all(Bytes::from_static(b"0\r\n\r\n")).await;
                } else {
                    out.status = 503;
                    keep = false;
                    let _ = write_local(
                        io,
                        &LocalReply::new(503, ResponseFlag::UpstreamConnectionTermination),
                        is_grpc,
                        ctx,
                        true,
                    )
                    .await;
                }
                break;
            }
        }
    }
    picked.end(out.status >= 500, false);
    out.grpc = trailers_grpc;
    out.keep_alive = keep;
    H2Result::Done(out)
}

/// HTTP/2 の上流への 1 回の試行のリトライの条件。
struct H2Again<'a> {
    /// リトライの方針（試行が残っているときだけ）
    retry: Option<&'a super::model::RetryPolicy>,
    /// 読み込み済みの本文の長さ（`acc` の先頭。chunked は除く）
    body_len: Option<usize>,
    /// 応答のヘッダまでの期限
    deadline: Option<Instant>,
    attempt: u32,
}

enum H2Result {
    Done(Outcome),
    /// 別のエンドポイントでやり直す（本文は `acc` に残っている）
    Retry(Outcome),
}

/// `acc` の先頭の chunked のフレーミングを読み進め、中身があれば切り出す（読んだ分は `acc` から捨てる）。
/// 戻り値は（中身, 終端に達したか）。終端より後ろ（次の要求）は `acc` に残す。
fn take_chunk_data(
    dec: &mut ChunkedDecoder,
    acc: &mut BytesMut,
) -> io::Result<(Option<Bytes>, bool)> {
    let span = dec.next_data_span(acc);
    if span.limit_exceeded {
        return Err(io::Error::new(io::ErrorKind::InvalidData, BadChunked));
    }
    if span.data_len == 0 {
        acc.advance(span.consumed);
        return Ok((None, span.complete));
    }
    // 中身はデコーダが返した直後に止まる
    debug_assert_eq!(span.consumed, span.data_start + span.data_len);
    acc.advance(span.data_start);
    Ok((Some(acc.split_to(span.data_len).freeze()), false))
}

/// chunked の本文から中身だけを取り出して HTTP/2 のストリームへ送る。
///
/// B-392: フレーミングは厳格なデコーダで少しずつ読み、読み終えた分は `acc` から捨てる（サイズの行・拡張・
/// トレーラの量はデコーダの上限で抑える）。以前は CRLF が見つかるまで `acc` へ溜めて毎回先頭から探していたので、
/// 改行を送らない相手 1 つでメモリを使い切れた（探す費用も二乗で増えた）。
async fn dechunk_into<S: AsyncReadRent>(
    io: &mut S,
    acc: &mut BytesMut,
    st: &crate::http2::upstream_mux::UpStream,
    total: &mut u64,
) -> bool {
    let mut dec = ChunkedDecoder::new_request(0);
    loop {
        if acc.is_empty()
            && !read_more(io, acc, Duration::from_secs(300))
                .await
                .unwrap_or(false)
        {
            return false;
        }
        let Ok((data, complete)) = take_chunk_data(&mut dec, acc) else {
            return false;
        };
        if let Some(chunk) = data {
            *total += chunk.len() as u64;
            if let Some(tx) = &st.req_tx {
                if tx.send(chunk).await.is_err() {
                    return false;
                }
            }
        }
        if complete {
            return true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // B-392: HTTP/2 の上流へ送る chunked の本文は、読んだ分を捨てながら中身だけを取り出し、改行の無い
    // フレーミングは上限で打ち切る（溜め続けない）
    #[test]
    fn take_chunk_data_streams_and_bounds_framing() {
        let mut dec = ChunkedDecoder::new_request(0);
        let mut acc =
            BytesMut::from(&b"4\r\nWiki\r\n5;ext=1\r\npedia\r\n0\r\nX-T: 1\r\n\r\nGET /next"[..]);
        let mut got = Vec::new();
        loop {
            let (data, complete) = take_chunk_data(&mut dec, &mut acc).unwrap();
            if let Some(d) = data {
                got.extend_from_slice(&d);
            }
            if complete {
                break;
            }
            assert!(!acc.is_empty());
        }
        assert_eq!(got, b"Wikipedia");
        assert_eq!(&acc[..], b"GET /next");

        // 分割して届いても同じ中身になり、読んだフレーミングは手元に残らない
        let whole = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let mut dec = ChunkedDecoder::new_request(0);
        let mut acc = BytesMut::new();
        let mut got = Vec::new();
        let mut done = false;
        for b in whole.iter() {
            acc.extend_from_slice(&[*b]);
            while !acc.is_empty() && !done {
                let (data, complete) = take_chunk_data(&mut dec, &mut acc).unwrap();
                if let Some(d) = data {
                    got.extend_from_slice(&d);
                }
                done = complete;
            }
            assert!(acc.is_empty());
        }
        assert!(done);
        assert_eq!(got, b"Wikipedia");

        // 改行の無いサイズの行・トレーラは上限で不正になる
        let mut dec = ChunkedDecoder::new_request(0);
        let mut acc = BytesMut::from(&[b'a'; 4096][..]);
        assert!(take_chunk_data(&mut dec, &mut acc).is_err());
        let mut dec = ChunkedDecoder::new_request(0);
        let mut acc = BytesMut::from(&b"0\r\n"[..]);
        assert!(!take_chunk_data(&mut dec, &mut acc).unwrap().1);
        let mut failed = false;
        for _ in 0..1024 {
            acc.extend_from_slice(&[b'x'; 1024]);
            match take_chunk_data(&mut dec, &mut acc) {
                Ok((None, false)) => assert!(acc.is_empty()),
                Err(_) => {
                    failed = true;
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(failed);
    }

    // B-384: Host が複数ある要求・本文の区切りが曖昧な要求は 400（走査が None）
    #[test]
    fn scan_request_headers_rejects_duplicate_host() {
        fn scan(raw: &'static [u8]) -> Option<ReqHeadScan> {
            let head = Bytes::from_static(raw);
            let mut hbuf = [httparse::EMPTY_HEADER; MAX_HEADERS];
            let mut req = httparse::Request::new(&mut hbuf);
            assert!(matches!(
                req.parse(&head),
                Ok(httparse::Status::Complete(_))
            ));
            scan_request_headers(&head, req.headers, req.version == Some(0))
        }
        let ok = scan(b"GET / HTTP/1.1\r\nHost: public.example\r\n\r\n").unwrap();
        assert_eq!(&ok.authority[..], b"public.example");
        assert_eq!(ok.framing, Framing::None);
        assert!(ok.keep_alive);
        assert!(
            scan(b"GET / HTTP/1.1\r\nHost: admin.internal\r\nHost: public.example\r\n\r\n")
                .is_none()
        );
        assert!(scan(b"GET / HTTP/1.1\r\nhost: a\r\nHOST: a\r\n\r\n").is_none());
        let c = scan(b"POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
        assert_eq!(c.framing, Framing::Chunked);
        assert!(scan(
            b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n"
        )
        .is_none());
        assert!(scan(b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: +1\r\n\r\n").is_none());
    }

    #[test]
    fn chunked_end_finds_end_and_rejects_smuggling() {
        let body = b"4\r\nWiki\r\n5;ext=1\r\npedia\r\n0\r\nX-T: 1\r\n\r\nGET /next";
        let mut dec = ChunkedDecoder::new_request(0);
        let end = chunked_end(&mut dec, body).unwrap().unwrap();
        assert_eq!(&body[end..], b"GET /next");
        // 1 バイトずつでも同じ
        let mut dec = ChunkedDecoder::new_request(0);
        let mut found = None;
        for (i, b) in body.iter().enumerate() {
            if let Some(e) = chunked_end(&mut dec, std::slice::from_ref(b)).unwrap() {
                found = Some(i + e);
                break;
            }
        }
        assert_eq!(found, Some(end));
        // B-369: 上流が裸の LF を行末と読むと後ろが別の要求になる形・空のサイズの行は不正にする
        // （以前の走査はトレーラ・拡張の中の LF を内容として読み、後ろの GET /admin までを本文にしていた）
        for bad in [
            &b"zz\r\n"[..],
            b"0\r\n\nGET /admin HTTP/1.1\r\nHost: svc\r\n\r\n",
            b"2;\nxx\r\n0\r\n\r\n",
            b"\r\n",
        ] {
            let mut dec = ChunkedDecoder::new_request(0);
            let e = chunked_end(&mut dec, bad).unwrap_err();
            assert!(is_bad_chunked(&e), "{:?}", String::from_utf8_lossy(bad));
        }
        assert!(!is_bad_chunked(&io::Error::other("other")));
    }

    #[test]
    fn request_head_rewrites_host() {
        let h = vec![
            (Bytes::from_static(b"host"), Bytes::from_static(b"a")),
            (Bytes::from_static(b"x"), Bytes::from_static(b"1")),
        ];
        let head = request_head(b"GET", b"/p", &h, b"b", true);
        assert_eq!(&head[..], b"GET /p HTTP/1.1\r\nx: 1\r\nhost: b\r\n\r\n");
        let head = request_head(b"GET", b"/p", &h, b"a", false);
        assert_eq!(&head[..], b"GET /p HTTP/1.1\r\nhost: a\r\nx: 1\r\n\r\n");
    }

    #[test]
    fn buffered_body_detection() {
        let acc = BytesMut::from(&b"hello"[..]);
        assert_eq!(buffered_body(&acc, Framing::Length(5)), Some(5));
        assert_eq!(buffered_body(&acc, Framing::Length(6)), None);
        let acc = BytesMut::from(&b"0\r\n\r\n"[..]);
        assert_eq!(buffered_body(&acc, Framing::Chunked), Some(5));
    }
}
