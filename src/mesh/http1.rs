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
use crate::runtime::handle::AsRawFd;
use crate::runtime::io::{AsyncReadRent, AsyncWriteRentExt, BufferedReadState};
use bytes::{BufMut, Bytes, BytesMut};
use std::io;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const MAX_HEADERS: usize = 100;
const MAX_HEAD_BYTES: usize = 60 * 1024;
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
pub async fn serve<S>(mut io: S, initial: Vec<u8>, ctx: Arc<ConnCtx>)
where
    S: AsyncReadRent + AsyncWriteRentExt + AsRawFd + BufferedReadState + Unpin + 'static,
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
        let method = sub(&head, req.method.unwrap_or("GET").as_bytes());
        let path = sub(&head, req.path.unwrap_or("/").as_bytes());
        let http10 = req.version == Some(0);
        let mut headers: Vec<(Bytes, Bytes)> = Vec::with_capacity(req.headers.len() + 8);
        let mut authority = Bytes::new();
        let mut framing = Framing::None;
        let mut te_chunked = false;
        let mut keep_alive = !http10;
        let mut upgrade = false;
        let mut expect_continue = false;
        for h in req.headers.iter() {
            let name = Bytes::from(h.name.to_ascii_lowercase());
            let value = sub(&head, h.value);
            match name.as_ref() {
                b"host" => authority = value.clone(),
                b"content-length" => {
                    if let Some(n) = std::str::from_utf8(&value)
                        .ok()
                        .and_then(|s| s.trim().parse().ok())
                    {
                        framing = Framing::Length(n);
                    }
                }
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
        if te_chunked {
            framing = Framing::Chunked;
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
        let view = ReqView {
            method: &method,
            path: &path,
            authority: &authority,
            headers: &headers,
            scheme: if ctx.tls { b"https" } else { b"http" },
        };
        let start = Instant::now();
        let is_grpc = view.is_grpc();
        let req_body_len = match framing {
            Framing::Length(n) => n,
            _ => 0,
        };
        match l7::decide(&ctx, &view) {
            Decision::Local(lr) => {
                // 本文は読み捨てる（キープアライブを保つ）
                let ok = discard_body(&mut io, &mut acc, framing).await.is_ok();
                let keep = keep_alive && ok;
                let _ = write_local(&mut io, &lr, is_grpc, &ctx, !keep).await;
                let labels = ctx.labels(None, None, is_grpc);
                super::stats::record_request(
                    &labels,
                    lr.status,
                    lr.grpc_status,
                    lr.flag,
                    start.elapsed().as_secs_f64() * 1000.0,
                    req_body_len,
                    lr.body.len() as u64,
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
                let labels = ctx.labels(r.peer.clone(), r.svc.clone(), is_grpc);
                super::stats::record_request(
                    &labels,
                    r.status,
                    r.grpc,
                    r.flag,
                    start.elapsed().as_secs_f64() * 1000.0,
                    r.req_bytes,
                    r.resp_bytes,
                );
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
    put_header(&mut b, b"server", &ctx.hcm.server_name);
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

/// chunked の終端の走査（中身はそのまま転送する）。
#[derive(Default)]
struct ChunkScanner {
    state: CState,
    size: u64,
    /// trailer の行が空だったか
    line_empty: bool,
}

#[derive(Default, PartialEq, Eq, Clone, Copy)]
enum CState {
    #[default]
    Size,
    SizeExt,
    SizeLf,
    Data,
    DataCr,
    DataLf,
    TrailerStart,
    Trailer,
    TrailerLf,
    Done,
}

impl ChunkScanner {
    /// `data` を走査し、本文の終わりが含まれていればその直後の位置を返す。
    fn feed(&mut self, data: &[u8]) -> Result<Option<usize>, ()> {
        let mut i = 0;
        while i < data.len() {
            let c = data[i];
            match self.state {
                CState::Size => match c {
                    b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F' => {
                        let d = (c as char).to_digit(16).unwrap_or(0) as u64;
                        self.size = self
                            .size
                            .checked_mul(16)
                            .ok_or(())?
                            .checked_add(d)
                            .ok_or(())?;
                    }
                    b';' | b' ' | b'\t' => self.state = CState::SizeExt,
                    b'\r' => self.state = CState::SizeLf,
                    _ => return Err(()),
                },
                CState::SizeExt => {
                    if c == b'\r' {
                        self.state = CState::SizeLf;
                    }
                }
                CState::SizeLf => {
                    if c != b'\n' {
                        return Err(());
                    }
                    self.state = if self.size == 0 {
                        CState::TrailerStart
                    } else {
                        CState::Data
                    };
                }
                CState::Data => {
                    let take = (self.size as usize).min(data.len() - i);
                    self.size -= take as u64;
                    i += take;
                    if self.size == 0 {
                        self.state = CState::DataCr;
                    }
                    continue;
                }
                CState::DataCr => {
                    if c != b'\r' {
                        return Err(());
                    }
                    self.state = CState::DataLf;
                }
                CState::DataLf => {
                    if c != b'\n' {
                        return Err(());
                    }
                    self.state = CState::Size;
                    self.size = 0;
                }
                CState::TrailerStart => {
                    if c == b'\r' {
                        self.line_empty = true;
                        self.state = CState::TrailerLf;
                    } else {
                        self.line_empty = false;
                        self.state = CState::Trailer;
                    }
                }
                CState::Trailer => {
                    if c == b'\r' {
                        self.state = CState::TrailerLf;
                    }
                }
                CState::TrailerLf => {
                    if c != b'\n' {
                        return Err(());
                    }
                    if self.line_empty {
                        self.state = CState::Done;
                        return Ok(Some(i + 1));
                    }
                    self.state = CState::TrailerStart;
                }
                CState::Done => return Ok(Some(i)),
            }
            i += 1;
        }
        Ok(None)
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
            let mut sc = ChunkScanner::default();
            loop {
                match sc.feed(acc).map_err(|_| io::Error::other("bad chunked"))? {
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
            let mut sc = ChunkScanner::default();
            loop {
                if acc.is_empty() && !read_more(r, acc, Duration::from_secs(300)).await? {
                    return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                }
                let end = sc
                    .feed(acc)
                    .map_err(|_| io::Error::other("bad chunked body"))?;
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
            let mut sc = ChunkScanner::default();
            match sc.feed(acc) {
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
            let mut framing = if http10 {
                Framing::UntilClose
            } else {
                Framing::UntilClose
            };
            let mut has_len = false;
            let mut keep = !http10;
            for h in resp.headers.iter() {
                let name = Bytes::from(h.name.to_ascii_lowercase());
                let value = sub(&head, h.value);
                match name.as_ref() {
                    b"content-length" => {
                        if let Some(n) = std::str::from_utf8(&value)
                            .ok()
                            .and_then(|s| s.trim().parse().ok())
                        {
                            framing = Framing::Length(n);
                            has_len = true;
                        }
                    }
                    b"transfer-encoding" => {
                        if value.windows(7).any(|w| w.eq_ignore_ascii_case(b"chunked")) {
                            framing = Framing::Chunked;
                        }
                    }
                    b"connection" => {
                        if value.windows(5).any(|w| w.eq_ignore_ascii_case(b"close")) {
                            keep = false;
                        }
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
    S: AsyncReadRent + AsyncWriteRentExt + AsRawFd + BufferedReadState + Unpin + 'static,
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
            Ok(p) => p,
            Err(flag) => {
                let lr = fail(503, flag);
                out.status = 503;
                out.flag = flag;
                let _ = discard_body(io, acc, framing).await;
                let _ = write_local(io, &lr, is_grpc, ctx, !keep_alive).await;
                return out;
            }
        };
        let downstream_h2 = false;
        if l7::upstream_is_h2(&picked.cfg, downstream_h2) && !upgrade {
            return forward_to_h2(
                io, acc, ctx, f, &picked, method, up_path, authority, headers, framing, keep_alive,
                is_grpc, out, origin,
            )
            .await;
        }
        let (req_headers, peer_req) =
            l7::upstream_request_headers(ctx, f, headers, attempt, upgrade);
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
                    let flag = if e.kind() == io::ErrorKind::TimedOut {
                        ResponseFlag::UpstreamConnectFailure
                    } else {
                        ResponseFlag::UpstreamConnectFailure
                    };
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
        let mut resp_headers = rh.headers;
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
        if !keep {
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
) -> Outcome
where
    S: AsyncReadRent + AsyncWriteRentExt + AsRawFd + BufferedReadState + Unpin + 'static,
{
    use crate::http2::upstream_mux::UpResp;
    let (req_headers, peer_req) = l7::upstream_request_headers(ctx, f, headers, 1, false);
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
            return out;
        }
    };
    let no_body = framing == Framing::None;
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
        return out;
    };
    // 本文: Content-Length はそのまま、chunked は中身を取り出して送る
    if !no_body {
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
            return out;
        }
    }
    let mut keep = keep_alive;
    let mut head_sent = false;
    let mut trailers_grpc = None;
    loop {
        match st.resp_rx.recv().await {
            Some(UpResp::Head {
                status,
                mut headers,
            }) => {
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
                return out;
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
    out
}

/// chunked の本文から中身だけを取り出して HTTP/2 のストリームへ送る。
async fn dechunk_into<S: AsyncReadRent>(
    io: &mut S,
    acc: &mut BytesMut,
    st: &crate::http2::upstream_mux::UpStream,
    total: &mut u64,
) -> bool {
    loop {
        // サイズ行
        let line_end = loop {
            if let Some(i) = memchr::memmem::find(acc, b"\r\n") {
                break i;
            }
            if !read_more(io, acc, Duration::from_secs(300))
                .await
                .unwrap_or(false)
            {
                return false;
            }
        };
        let line = acc.split_to(line_end + 2);
        let size_str = std::str::from_utf8(&line[..line_end]).unwrap_or("");
        let size_str = size_str.split(';').next().unwrap_or("").trim();
        let Ok(mut size) = usize::from_str_radix(size_str, 16) else {
            return false;
        };
        if size == 0 {
            // trailer を読み捨てる
            loop {
                if let Some(i) = memchr::memmem::find(acc, b"\r\n") {
                    let l = acc.split_to(i + 2);
                    if l.len() == 2 {
                        return true;
                    }
                    continue;
                }
                if !read_more(io, acc, Duration::from_secs(300))
                    .await
                    .unwrap_or(false)
                {
                    return false;
                }
            }
        }
        while size > 0 {
            if acc.is_empty()
                && !read_more(io, acc, Duration::from_secs(300))
                    .await
                    .unwrap_or(false)
            {
                return false;
            }
            let take = acc.len().min(size);
            let chunk = acc.split_to(take).freeze();
            size -= take;
            *total += take as u64;
            if let Some(tx) = &st.req_tx {
                if tx.send(chunk).await.is_err() {
                    return false;
                }
            }
        }
        while acc.len() < 2 {
            if !read_more(io, acc, Duration::from_secs(300))
                .await
                .unwrap_or(false)
            {
                return false;
            }
        }
        let _ = acc.split_to(2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_scanner_finds_end() {
        let body = b"4\r\nWiki\r\n5;ext=1\r\npedia\r\n0\r\nX-T: 1\r\n\r\nGET /next";
        let mut sc = ChunkScanner::default();
        let end = sc.feed(body).unwrap().unwrap();
        assert_eq!(&body[end..], b"GET /next");
        // 1 バイトずつでも同じ
        let mut sc = ChunkScanner::default();
        let mut found = None;
        for (i, b) in body.iter().enumerate() {
            if let Some(e) = sc.feed(std::slice::from_ref(b)).unwrap() {
                found = Some(i + e);
                break;
            }
        }
        assert_eq!(found, Some(end));
        let mut bad = ChunkScanner::default();
        assert!(bad.feed(b"zz\r\n").is_err());
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
