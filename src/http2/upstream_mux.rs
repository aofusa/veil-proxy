//! # F-174: 多重化対応の上流 HTTP/2 クライアント（h2c / h2 over TLS）
//!
//! 上流への HTTP/2 接続 1 本を **1 つのアクタータスク**が所有し、複数の下流要求（ストリーム）で
//! 共有する。旧 `H2cClient`（1 接続 1 ストリームの直列利用、接続レベルのウィンドウのみ）を置き換える。
//!
//! - **ストリームを開く**（[`H2Mux::open`]）のは呼び出し側のタスクで、同期的に HPACK エンコードして
//!   HEADERS を接続の送信キューへ積む（HPACK の動的テーブルは送出順に依存するため、エンコードと
//!   キュー投入を同じ借用内で行う）。要求ヘッダを所有コピーしない。
//! - **アクター**は送信キューの書き出し、受信フレームの振り分け、要求本文の送信（接続・ストリーム
//!   両方のウィンドウを追跡。B-106）、WINDOW_UPDATE（下流が受け取ったぶんだけ補充）を行う。
//! - ソケット可読は `wait_readable_fd`（POLL_ADD）で待つ（`read` を select の負け側で drop すると
//!   既読データを失う。F-116）。TLS の復号済み平文は [`BufferedReadState`] で確認してから待つ。
//! - 応答は [`UpResp`] のチャネルで流す（`Head` → `Data`* → `Trailers`?、正常終了はチャネルの close）。
//!   チャネルは有界で、満杯の間は受信した DATA をストリームごとに保留し WINDOW_UPDATE を止める
//!   （= 上流の送信が止まる。メモリはストリームの受信ウィンドウで上限が決まる）。
//! - `SETTINGS_MAX_CONCURRENT_STREAMS` に達した接続・GOAWAY を受けた接続には新しいストリームを
//!   割り当てない（プールは同じ上流へ 2 本目の接続を張る）。
//! - 接続ごとに 1 タスクなので、上流がアイドルで接続を閉じても即座に検知してプールから外れる。

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::future::{poll_fn, Future};
use std::rc::Rc;
use std::task::Poll;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use ftlog::{debug, warn};

use crate::http2::frame::{Frame, FrameDecoder, FrameEncoder, FrameHeader};
use crate::http2::hpack::{HpackDecoder, HpackEncoder};
use crate::http2::settings::defaults;
use crate::http2::NameSlot;
use crate::runtime::handle::AsRawFd;
use crate::runtime::io::{AsyncReadRent, AsyncWriteRentExt, BufferedReadState};
use crate::stream_channel::{channel, Notify, Receiver, Sender, TryRecv, TrySendError};

/// ストリームの受信ウィンドウ（SETTINGS_INITIAL_WINDOW_SIZE として広告する）。
/// 1 ストリームが下流の遅さで保留できる量の上限でもある。
const STREAM_RECV_WINDOW: u32 = 256 * 1024;
/// 接続の受信ウィンドウ（開始時に WINDOW_UPDATE で既定の 65,535 から広げる）。
const CONN_RECV_WINDOW: u32 = 4 * 1024 * 1024;
/// 受け付ける応答ヘッダリストの上限。
const MAX_HEADER_LIST: u32 = 64 * 1024;
/// CONTINUATION を含むヘッダブロックの上限（超えたら接続エラー）。
const MAX_HEADER_BLOCK: usize = 256 * 1024;
/// 応答チャネルの容量（アイテム数）。
const RESP_CHAN_CAP: usize = 8;
/// 要求本文チャネルの容量（アイテム数）。
const REQ_CHAN_CAP: usize = 4;
/// SETTINGS 受信前に仮定する同時ストリーム上限（RFC は無制限だが、過大な並列で
/// REFUSED_STREAM を浴びないよう控えめにする）。
const DEFAULT_MAX_CONCURRENT: u32 = 100;
/// ストリーム ID の再利用打ち切り点（31bit 上限の手前）。
const STREAM_ID_LIMIT: u32 = 0x7000_0000;
/// 1 回の read で受け取る最大バイト数。
const READ_CHUNK: usize = 64 * 1024;

const ERR_NO_ERROR: u32 = 0x0;
const ERR_PROTOCOL: u32 = 0x1;
const ERR_FLOW_CONTROL: u32 = 0x3;
const ERR_REFUSED_STREAM: u32 = 0x7;
const ERR_CANCEL: u32 = 0x8;

/// 上流からの応答断片。正常終了はチャネルの close（`recv` が `None`）で表す。
#[derive(Debug)]
pub(crate) enum UpResp {
    /// 最終応答の head（1xx は捨てる）。
    Head {
        status: u16,
        headers: Vec<(Bytes, Bytes)>,
    },
    /// 本文断片（ゼロコピー）。
    Data(Bytes),
    /// トレーラー（gRPC の grpc-status 等）。この後チャネルが閉じる。
    Trailers(Vec<(Bytes, Bytes)>),
    /// ストリームの異常終了（RST_STREAM・GOAWAY・接続断）。
    /// `retryable` は上流が要求を処理していないことが確実な場合（REFUSED_STREAM、
    /// GOAWAY の last_stream_id より後、応答 head より前の接続断）。
    Reset { retryable: bool },
}

/// 開いたストリームのハンドル。drop するとアクターが RST_STREAM(CANCEL) を送る。
pub(crate) struct UpStream {
    /// 応答断片の受信端。
    pub resp_rx: Receiver<UpResp>,
    /// 要求本文の送信端（`open` で `end_stream = true` なら `None`）。drop で END_STREAM。
    pub req_tx: Option<Sender<Bytes>>,
    notify: Notify,
}

impl UpStream {
    /// 要求本文の終端を送る（送信端を閉じる）。
    pub fn finish_request(&mut self) {
        if self.req_tx.take().is_some() {
            self.notify.notify();
        }
    }
}

impl Drop for UpStream {
    fn drop(&mut self) {
        // 応答を読み切る前に捨てられた場合のキャンセル検知のためにアクターを起こす
        // （フィールドの drop はこの後だが、アクターが走るのは現在のタスクが譲った後）。
        self.notify.notify();
    }
}

// ============================================================================
// 接続の共有状態
// ============================================================================

/// ストリーム ID 用の軽量ハッシャ（クライアント ID は奇数の単調増加で衝突しない）。
#[derive(Default, Clone, Copy)]
struct IdHasher(u64);

impl std::hash::Hasher for IdHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 << 8) | b as u64;
        }
    }
    #[inline]
    fn write_u32(&mut self, n: u32) {
        self.0 = (n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

type StreamMap = HashMap<u32, StreamSlot, std::hash::BuildHasherDefault<IdHasher>>;

/// アクター側のストリーム状態。
struct StreamSlot {
    resp_tx: Sender<UpResp>,
    /// 要求本文の受信端（本文なし・送信完了で `None`）。
    req_rx: Option<Receiver<Bytes>>,
    /// ウィンドウ待ちの本文断片（`(buf, 送信済みオフセット)`）。
    req_chunk: Option<(Bytes, usize)>,
    /// END_STREAM を送ったか。
    local_closed: bool,
    /// END_STREAM（または RST_STREAM）を受けたか。
    remote_closed: bool,
    /// ストリームの送信ウィンドウ。
    send_window: i64,
    /// 応答チャネルが満杯の間に受けた断片。
    resp_queue: VecDeque<UpResp>,
    /// 下流へ渡したが WINDOW_UPDATE していない受信バイト数。
    recv_delivered: u32,
    /// 最終応答の head を受けたか。
    head_seen: bool,
    /// アクターが RST_STREAM を送る/受けたなど、以後フレームを送らない。
    reset: bool,
}

struct Inner {
    hpack_enc: HpackEncoder,
    fenc: FrameEncoder,
    /// 送信キュー（アクターが書き出す）。
    out: Vec<u8>,
    next_id: u32,
    streams: StreamMap,
    peer_max_concurrent: u32,
    peer_initial_window: i64,
    peer_max_frame: usize,
    conn_send_window: i64,
    /// 下流へ渡したが WINDOW_UPDATE していない受信バイト数（接続レベル）。
    conn_recv_delivered: u32,
    /// GOAWAY 受信済み（新規ストリーム不可）。
    goaway: bool,
}

struct Shared {
    inner: RefCell<Inner>,
    /// アクターを起こす。
    notify: Notify,
    /// アクターが終了した（接続断・アイドル切断）。
    dead: Cell<bool>,
    /// `:scheme`（TLS 上の h2 なら `https`）。
    https: bool,
}

/// 上流 HTTP/2 接続のハンドル（プールが保持し、`open` でストリームを割り当てる）。
#[derive(Clone)]
pub(crate) struct H2Mux {
    sh: Rc<Shared>,
}

/// 上流へ転送しない要求ヘッダか（疑似ヘッダ・ホップバイホップ・Host・Expect・Content-Length）。
///
/// `te` は HTTP/2 では `trailers` 以外を送ってはならない（RFC 9113 §8.2.2）。
#[inline]
fn skip_request_header(name: &[u8], value: &[u8]) -> bool {
    name.starts_with(b":")
        || name.eq_ignore_ascii_case(b"connection")
        || name.eq_ignore_ascii_case(b"keep-alive")
        || name.eq_ignore_ascii_case(b"proxy-connection")
        || name.eq_ignore_ascii_case(b"transfer-encoding")
        || name.eq_ignore_ascii_case(b"upgrade")
        || name.eq_ignore_ascii_case(b"host")
        || name.eq_ignore_ascii_case(b"expect")
        || name.eq_ignore_ascii_case(b"content-length")
        || (name.eq_ignore_ascii_case(b"te") && !value.eq_ignore_ascii_case(b"trailers"))
}

impl H2Mux {
    /// 接続済みトランスポートで HTTP/2 を開始し、アクターを spawn する。
    ///
    /// プリフェース・SETTINGS・接続ウィンドウの WINDOW_UPDATE は送信キューの先頭に積むだけで、
    /// サーバの SETTINGS を待たずに直ちにストリームを開ける（RFC 9113 §3.4）。
    pub(crate) fn start<S>(io: S, https: bool, idle_timeout: Duration) -> Self
    where
        S: AsyncReadRent + AsyncWriteRentExt + AsRawFd + BufferedReadState + Unpin + 'static,
    {
        let fenc = FrameEncoder::new(defaults::MAX_FRAME_SIZE);
        let mut out = Vec::with_capacity(256);
        out.extend_from_slice(defaults::CONNECTION_PREFACE);
        out.extend_from_slice(&fenc.encode_settings(
            &[
                (0x2, 0),                  // ENABLE_PUSH
                (0x4, STREAM_RECV_WINDOW), // INITIAL_WINDOW_SIZE
                (0x6, MAX_HEADER_LIST),    // MAX_HEADER_LIST_SIZE
            ],
            false,
        ));
        out.extend_from_slice(
            &fenc.encode_window_update(0, CONN_RECV_WINDOW - defaults::CONNECTION_WINDOW_SIZE),
        );
        let sh = Rc::new(Shared {
            inner: RefCell::new(Inner {
                hpack_enc: HpackEncoder::new(defaults::HEADER_TABLE_SIZE as usize),
                fenc,
                out,
                next_id: 1,
                streams: StreamMap::default(),
                peer_max_concurrent: DEFAULT_MAX_CONCURRENT,
                peer_initial_window: defaults::INITIAL_WINDOW_SIZE as i64,
                peer_max_frame: defaults::MAX_FRAME_SIZE as usize,
                conn_send_window: defaults::CONNECTION_WINDOW_SIZE as i64,
                conn_recv_delivered: 0,
                goaway: false,
            }),
            notify: Notify::new(),
            dead: Cell::new(false),
            https,
        });
        crate::runtime::spawn(run_actor(io, sh.clone(), idle_timeout));
        Self { sh }
    }

    /// 新しいストリームを割り当てられるか（接続が生きていて、GOAWAY 前で、同時数に余裕がある）。
    pub(crate) fn has_capacity(&self) -> bool {
        if self.sh.dead.get() {
            return false;
        }
        let g = self.sh.inner.borrow();
        !g.goaway && (g.streams.len() as u32) < g.peer_max_concurrent && g.next_id < STREAM_ID_LIMIT
    }

    /// 接続が終了しているか（プールから外す）。
    pub(crate) fn is_dead(&self) -> bool {
        self.sh.dead.get()
    }

    /// ストリームを開いて HEADERS を送信キューへ積む。割り当てられなければ `None`。
    ///
    /// `end_stream` が `true` なら本文なし（HEADERS に END_STREAM）。`false` なら返り値の
    /// `req_tx` で本文を送り、送信端を閉じる（[`UpStream::finish_request`]）と END_STREAM になる。
    pub(crate) fn open<'h, I>(
        &self,
        method: &[u8],
        path: &[u8],
        authority: &[u8],
        headers: I,
        end_stream: bool,
    ) -> Option<UpStream>
    where
        I: Iterator<Item = (&'h [u8], &'h [u8])> + Clone,
    {
        if !self.has_capacity() {
            return None;
        }
        let mut g = self.sh.inner.borrow_mut();
        let id = g.next_id;

        // HTTP/2 はヘッダ名の小文字必須（RFC 9113 §8.2）。既に小文字の名前は借用のまま。
        let mut lowered: Vec<NameSlot<'h>> = Vec::new();
        for (name, value) in headers.clone() {
            if skip_request_header(name, value) {
                continue;
            }
            if name.iter().any(|b| b.is_ascii_uppercase()) {
                let mut buf = crate::pool::lowered_header_name_buf_get();
                buf.extend_from_slice(name);
                buf.make_ascii_lowercase();
                lowered.push(NameSlot::Owned(buf));
            } else {
                lowered.push(NameSlot::Borrowed(name));
            }
        }
        let scheme: &[u8] = if self.sh.https { b"https" } else { b"http" };
        let mut list: Vec<(&[u8], &[u8], bool)> = Vec::with_capacity(lowered.len() + 4);
        list.push((b":method", method, false));
        list.push((b":scheme", scheme, false));
        list.push((b":authority", authority, false));
        list.push((b":path", path, false));
        let mut li = 0usize;
        for (name, value) in headers {
            if skip_request_header(name, value) {
                continue;
            }
            list.push((lowered[li].as_slice(), value, false));
            li += 1;
        }
        let block = match g.hpack_enc.encode(&list) {
            Ok(b) => b,
            Err(e) => {
                warn!("[h2-upstream] HPACK encode error: {}", e);
                return None;
            }
        };
        drop(list);
        for slot in lowered {
            if let NameSlot::Owned(buf) = slot {
                crate::pool::lowered_header_name_buf_put(buf);
            }
        }

        // HEADERS（+ 必要なら CONTINUATION）を送信キューへ。
        let max = g.peer_max_frame;
        let inner = &mut *g;
        if block.len() <= max {
            inner
                .fenc
                .encode_headers_into(&mut inner.out, id, &block, end_stream, true, None);
        } else {
            inner.fenc.encode_headers_into(
                &mut inner.out,
                id,
                &block[..max],
                end_stream,
                false,
                None,
            );
            let mut off = max;
            while off < block.len() {
                let end = (off + max).min(block.len());
                let frame =
                    inner
                        .fenc
                        .encode_continuation(id, &block[off..end], end == block.len());
                inner.out.extend_from_slice(&frame);
                off = end;
            }
        }
        g.next_id += 2;

        let (resp_tx, resp_rx) = channel::<UpResp>(RESP_CHAN_CAP);
        let (req_tx, req_rx) = if end_stream {
            (None, None)
        } else {
            let (t, r) = channel::<Bytes>(REQ_CHAN_CAP);
            (Some(t), Some(r))
        };
        let send_window = g.peer_initial_window;
        g.streams.insert(
            id,
            StreamSlot {
                resp_tx,
                req_rx,
                req_chunk: None,
                local_closed: end_stream,
                remote_closed: false,
                send_window,
                resp_queue: VecDeque::new(),
                recv_delivered: 0,
                head_seen: false,
                reset: false,
            },
        );
        drop(g);
        self.sh.notify.notify();
        Some(UpStream {
            resp_rx,
            req_tx,
            notify: self.sh.notify.clone(),
        })
    }
}

// ============================================================================
// アクター
// ============================================================================

/// 受信側の状態（アクターのローカル。共有しない）。
struct RecvState {
    buf: Vec<u8>,
    start: usize,
    decoder: FrameDecoder,
    hpack_dec: HpackDecoder,
    /// CONTINUATION 組み立て中のヘッダブロック `(stream_id, end_stream, block)`。
    cont: Option<(u32, bool, Vec<u8>)>,
}

/// 接続エラー（アクターを終了させる）。
struct ConnError;

async fn run_actor<S>(mut io: S, sh: Rc<Shared>, idle_timeout: Duration)
where
    S: AsyncReadRent + AsyncWriteRentExt + AsRawFd + BufferedReadState + Unpin,
{
    let fd = io.as_raw_fd();
    let mut hpack_dec = HpackDecoder::new(defaults::HEADER_TABLE_SIZE as usize);
    hpack_dec.set_max_header_list_size(MAX_HEADER_LIST as usize);
    let mut rs = RecvState {
        buf: Vec::with_capacity(READ_CHUNK),
        start: 0,
        decoder: FrameDecoder::new(defaults::MAX_FRAME_SIZE),
        hpack_dec,
        cont: None,
    };
    let mut scratch: Vec<u8> = Vec::with_capacity(READ_CHUNK);
    let mut ids: Vec<u32> = Vec::new();
    let mut idle_since: Option<Instant> = None;
    let mut eof = false;

    let _result: Result<(), ConnError> = async {
        loop {
            // 1. 受信済みの完全フレームを処理し、ストリームを駆動して送信キューを作る。
            {
                let mut g = sh.inner.borrow_mut();
                while let Some(frame) = next_frame(&mut rs)? {
                    handle_frame(&mut g, &mut rs, frame)?;
                }
                pump_streams(&mut g, &mut ids);
                if g.streams.is_empty() {
                    if idle_since.is_none() {
                        idle_since = Some(Instant::now());
                    }
                } else {
                    idle_since = None;
                }
            }

            // 2. 送信キューを書き出す（書き出し中に積まれたものは次周回）。
            let out = {
                let mut g = sh.inner.borrow_mut();
                if g.out.is_empty() {
                    None
                } else {
                    Some(std::mem::take(&mut g.out))
                }
            };
            if let Some(out) = out {
                let (res, mut returned) = io.write_all(out).await;
                if let Err(e) = res {
                    debug!("[h2-upstream] write error: {}", e);
                    return Err(ConnError);
                }
                returned.clear();
                let mut g = sh.inner.borrow_mut();
                if g.out.is_empty() {
                    // 容量を保持したまま戻す（周回ごとの確保をしない）。
                    g.out = returned;
                }
                continue;
            }

            if eof {
                return Err(ConnError);
            }

            // 3. アイドル切断（ストリームが無いまま idle_timeout を過ぎた）。
            let idle_deadline = idle_since.map(|t| t + idle_timeout);
            if let Some(dl) = idle_deadline {
                if Instant::now() >= dl {
                    // 先に dead にしてから書く（書き出しの待ちの間にストリームを割り当てさせない）。
                    sh.dead.set(true);
                    let frame = sh.inner.borrow().fenc.encode_goaway(0, ERR_NO_ERROR, &[]);
                    let _ = io.write_all(frame).await;
                    return Ok(());
                }
            }

            // 4. 待つ: ソケット可読 / 新規ストリーム・キャンセル / 要求本文 / 応答チャネルの空き。
            if !io.has_buffered_read_data() {
                let woke = wait_for_work(&sh, fd, idle_deadline).await;
                if !woke {
                    continue; // 本文・チャネルの空き・notify・タイマー → 1 へ。
                }
            }

            // 5. 読む（可読になっている / TLS の復号済み平文がある）。
            let (res, buf) = io.read(std::mem::take(&mut scratch)).await;
            scratch = buf;
            match res {
                Ok(0) => {
                    eof = true;
                }
                Ok(n) => {
                    if rs.start > 0 && rs.start == rs.buf.len() {
                        rs.buf.clear();
                        rs.start = 0;
                    }
                    rs.buf.extend_from_slice(&scratch[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    debug!("[h2-upstream] read error: {}", e);
                    return Err(ConnError);
                }
            }
            scratch.clear();
        }
    }
    .await;

    // 終了: 以後は割り当てない。進行中のストリームへ異常終了を伝える。
    sh.dead.set(true);
    let mut g = sh.inner.borrow_mut();
    for (_, mut st) in g.streams.drain() {
        if !st.remote_closed {
            // 接続断では上流が処理したかどうか分からない（再送可否は呼び出し側がメソッドで判断）。
            st.resp_queue.push_back(UpResp::Reset { retryable: false });
        }
        // 保留中の断片を渡せるだけ渡す（チャネルが満杯なら諦める）。
        while let Some(msg) = st.resp_queue.pop_front() {
            if st.resp_tx.try_send(msg).is_err() {
                break;
            }
        }
    }
}

/// 待機。`true` = ソケット可読（読みに行く）、`false` = それ以外の起床（駆動し直す）。
async fn wait_for_work(
    sh: &Shared,
    fd: crate::runtime::handle::RawFd,
    idle: Option<Instant>,
) -> bool {
    let readable = crate::runtime::tcp::wait_readable_fd(fd);
    let notified = sh.notify.wait();
    let sleep =
        idle.map(|dl| crate::runtime::time::sleep(dl.saturating_duration_since(Instant::now())));
    let mut readable = std::pin::pin!(readable);
    let mut notified = std::pin::pin!(notified);
    let mut sleep = std::pin::pin!(sleep);
    poll_fn(|cx| {
        if readable.as_mut().poll(cx).is_ready() {
            return Poll::Ready(true);
        }
        if notified.as_mut().poll(cx).is_ready() {
            return Poll::Ready(false);
        }
        if let Some(s) = sleep.as_mut().as_pin_mut() {
            if s.poll(cx).is_ready() {
                return Poll::Ready(false);
            }
        }
        // ストリームごとの起床条件を登録する（要求本文の到着 / 応答チャネルの空き / キャンセル）。
        let mut g = sh.inner.borrow_mut();
        let conn_open = g.conn_send_window > 0;
        for st in g.streams.values_mut() {
            if st.resp_tx.is_closed() {
                return Poll::Ready(false);
            }
            if !st.resp_queue.is_empty() && st.resp_tx.poll_ready(cx).is_ready() {
                return Poll::Ready(false);
            }
            if st.req_chunk.is_none() && !st.reset && conn_open && st.send_window > 0 {
                if let Some(rx) = &st.req_rx {
                    match rx.poll_recv(cx) {
                        Poll::Ready(Some(b)) => {
                            st.req_chunk = Some((b, 0));
                            return Poll::Ready(false);
                        }
                        Poll::Ready(None) => return Poll::Ready(false),
                        Poll::Pending => {}
                    }
                }
            }
        }
        Poll::Pending
    })
    .await
}

/// 受信バッファから完全フレームを 1 つ取り出す。
fn next_frame(rs: &mut RecvState) -> Result<Option<Frame>, ConnError> {
    let avail = rs.buf.len() - rs.start;
    if avail < FrameHeader::SIZE {
        compact(rs);
        return Ok(None);
    }
    let header = rs.decoder.decode_header(&rs.buf[rs.start..]).map_err(|e| {
        debug!("[h2-upstream] frame header error: {}", e);
        ConnError
    })?;
    let total = FrameHeader::SIZE + header.length as usize;
    if avail < total {
        compact(rs);
        return Ok(None);
    }
    let payload = &rs.buf[rs.start + FrameHeader::SIZE..rs.start + total];
    let frame = rs.decoder.decode(&header, payload).map_err(|e| {
        debug!("[h2-upstream] frame decode error: {}", e);
        ConnError
    })?;
    rs.start += total;
    Ok(Some(frame))
}

/// 消費済みの先頭を詰める（未消費が少ないときだけコピーが走る）。
fn compact(rs: &mut RecvState) {
    if rs.start == 0 {
        return;
    }
    if rs.start == rs.buf.len() {
        rs.buf.clear();
    } else {
        rs.buf.drain(..rs.start);
    }
    rs.start = 0;
}

fn rst(g: &mut Inner, id: u32, code: u32) {
    let frame = g.fenc.encode_rst_stream(id, code);
    g.out.extend_from_slice(&frame);
}

/// 受信フレーム 1 つを処理する。
fn handle_frame(g: &mut Inner, rs: &mut RecvState, frame: Frame) -> Result<(), ConnError> {
    // CONTINUATION の途中に他のフレームが来たら接続エラー（RFC 9113 §6.10）。
    if rs.cont.is_some() && !matches!(frame, Frame::Continuation { .. }) {
        return Err(ConnError);
    }
    match frame {
        Frame::Settings {
            ack: false,
            settings,
        } => {
            for (id, value) in settings {
                match id {
                    0x1 => g.hpack_enc.set_max_table_size(value as usize),
                    0x3 => g.peer_max_concurrent = value,
                    0x4 => {
                        if value > i32::MAX as u32 {
                            return Err(ConnError);
                        }
                        let delta = value as i64 - g.peer_initial_window;
                        g.peer_initial_window = value as i64;
                        for st in g.streams.values_mut() {
                            st.send_window += delta;
                        }
                    }
                    0x5 => {
                        g.fenc.set_max_frame_size(value);
                        g.peer_max_frame = value as usize;
                    }
                    _ => {}
                }
            }
            let ack = g.fenc.encode_settings_ack();
            g.out.extend_from_slice(&ack);
        }
        Frame::Settings { ack: true, .. } => {}
        Frame::Ping { ack: false, data } => {
            let pong = g.fenc.encode_ping(&data, true);
            g.out.extend_from_slice(&pong);
        }
        Frame::Ping { ack: true, .. } => {}
        Frame::WindowUpdate {
            stream_id,
            increment,
        } => {
            if stream_id == 0 {
                g.conn_send_window += increment as i64;
                if g.conn_send_window > i32::MAX as i64 {
                    return Err(ConnError);
                }
            } else if let Some(st) = g.streams.get_mut(&stream_id) {
                st.send_window += increment as i64;
                if st.send_window > i32::MAX as i64 {
                    st.reset = true;
                    st.remote_closed = true;
                    st.resp_queue.push_back(UpResp::Reset { retryable: false });
                    rst(g, stream_id, ERR_FLOW_CONTROL);
                }
            }
        }
        Frame::Headers {
            stream_id,
            end_stream,
            end_headers,
            header_block,
            ..
        } => {
            if end_headers {
                on_headers(g, rs, stream_id, end_stream, &header_block)?;
            } else {
                rs.cont = Some((stream_id, end_stream, header_block));
            }
        }
        Frame::Continuation {
            stream_id,
            end_headers,
            header_block,
        } => {
            let Some((cid, end_stream, mut block)) = rs.cont.take() else {
                return Err(ConnError);
            };
            if cid != stream_id || block.len() + header_block.len() > MAX_HEADER_BLOCK {
                return Err(ConnError);
            }
            block.extend_from_slice(&header_block);
            if end_headers {
                on_headers(g, rs, stream_id, end_stream, &block)?;
            } else {
                rs.cont = Some((cid, end_stream, block));
            }
        }
        Frame::Data {
            stream_id,
            end_stream,
            data,
        } => {
            let len = data.len() as u32;
            match g.streams.get_mut(&stream_id) {
                Some(st) if !st.reset => {
                    if !data.is_empty() {
                        st.resp_queue.push_back(UpResp::Data(Bytes::from(data)));
                    }
                    if end_stream {
                        st.remote_closed = true;
                    }
                }
                _ => {
                    // 閉じた/キャンセルしたストリーム宛て: 接続ウィンドウだけ即座に返す。
                    g.conn_recv_delivered += len;
                }
            }
        }
        Frame::RstStream {
            stream_id,
            error_code,
        } => {
            if let Some(st) = g.streams.get_mut(&stream_id) {
                if !st.remote_closed {
                    st.resp_queue.push_back(UpResp::Reset {
                        retryable: error_code == ERR_REFUSED_STREAM,
                    });
                }
                st.reset = true;
                st.remote_closed = true;
                st.local_closed = true;
                st.req_rx = None;
                st.req_chunk = None;
            }
        }
        Frame::GoAway {
            last_stream_id,
            error_code,
            ..
        } => {
            debug!(
                "[h2-upstream] GOAWAY last_stream_id={} error={}",
                last_stream_id, error_code
            );
            g.goaway = true;
            for (&id, st) in g.streams.iter_mut() {
                if id > last_stream_id && !st.remote_closed {
                    // 上流は処理していない（RFC 9113 §6.8）→ 再送してよい。
                    st.resp_queue.push_back(UpResp::Reset { retryable: true });
                    st.reset = true;
                    st.remote_closed = true;
                    st.local_closed = true;
                    st.req_rx = None;
                    st.req_chunk = None;
                }
            }
        }
        Frame::PushPromise { .. } => {
            // ENABLE_PUSH=0 を広告している。
            let frame = g.fenc.encode_goaway(0, ERR_PROTOCOL, &[]);
            g.out.extend_from_slice(&frame);
            return Err(ConnError);
        }
        Frame::Priority { .. } | Frame::Unknown { .. } => {}
    }
    Ok(())
}

/// 応答 HEADERS（head / 1xx / トレーラー）を処理する。HPACK の状態を保つため、閉じた
/// ストリーム宛てでも必ずデコードする。
fn on_headers(
    g: &mut Inner,
    rs: &mut RecvState,
    stream_id: u32,
    end_stream: bool,
    block: &[u8],
) -> Result<(), ConnError> {
    let fields = rs.hpack_dec.decode(block).map_err(|e| {
        debug!("[h2-upstream] HPACK decode error: {}", e);
        ConnError
    })?;
    let Some(st) = g.streams.get_mut(&stream_id) else {
        return Ok(());
    };
    if st.reset {
        return Ok(());
    }
    if !st.head_seen {
        let mut status: u16 = 0;
        let mut headers = Vec::with_capacity(fields.len());
        for f in fields {
            if f.name.as_ref() == b":status" {
                status = std::str::from_utf8(&f.value)
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
            } else if !f.name.starts_with(b":") {
                headers.push((f.name, f.value));
            }
        }
        if (100..200).contains(&status) {
            // 1xx（100 Continue / 103 Early Hints）は転送しない。最終応答を待つ。
            return Ok(());
        }
        if status == 0 {
            st.reset = true;
            st.remote_closed = true;
            st.resp_queue.push_back(UpResp::Reset { retryable: false });
            rst(g, stream_id, ERR_PROTOCOL);
            return Ok(());
        }
        st.head_seen = true;
        st.resp_queue.push_back(UpResp::Head { status, headers });
    } else {
        let trailers = fields.into_iter().map(|f| (f.name, f.value)).collect();
        st.resp_queue.push_back(UpResp::Trailers(trailers));
    }
    if end_stream {
        st.remote_closed = true;
    }
    Ok(())
}

/// 全ストリームを 1 回駆動する: 応答断片の受け渡し・WINDOW_UPDATE・要求本文の送信・完了と
/// キャンセルの片付け。`ids` は走査用の再利用バッファ。
fn pump_streams(g: &mut Inner, ids: &mut Vec<u32>) {
    ids.clear();
    ids.extend(g.streams.keys().copied());
    let mut conn_delivered: u32 = 0;
    for &id in ids.iter() {
        let max_frame = g.peer_max_frame;
        let mut conn_window = g.conn_send_window;
        let inner_out = &mut g.out;
        let fenc = &g.fenc;
        let Some(st) = g.streams.get_mut(&id) else {
            continue;
        };

        // 下流が応答ハンドルを捨てた → キャンセル。
        if st.resp_tx.is_closed() {
            if !st.reset && !(st.local_closed && st.remote_closed) {
                inner_out.extend_from_slice(&fenc.encode_rst_stream(id, ERR_CANCEL));
            }
            // 保留していた DATA の接続ウィンドウは返す。
            for msg in st.resp_queue.drain(..) {
                if let UpResp::Data(b) = msg {
                    conn_delivered += b.len() as u32;
                }
            }
            g.streams.remove(&id);
            continue;
        }

        // 応答断片を下流へ渡す（チャネルが空いている分だけ）。
        let mut delivered: u32 = 0;
        while let Some(msg) = st.resp_queue.pop_front() {
            let n = if let UpResp::Data(b) = &msg {
                b.len() as u32
            } else {
                0
            };
            match st.resp_tx.try_send(msg) {
                Ok(()) => delivered += n,
                Err(TrySendError::Full(msg)) => {
                    st.resp_queue.push_front(msg);
                    break;
                }
                Err(TrySendError::Closed(_)) => break,
            }
        }
        conn_delivered += delivered;
        if delivered > 0 && !st.remote_closed {
            st.recv_delivered += delivered;
            if st.recv_delivered >= STREAM_RECV_WINDOW / 2 {
                inner_out.extend_from_slice(&fenc.encode_window_update(id, st.recv_delivered));
                st.recv_delivered = 0;
            }
        }

        // 要求本文を送る（接続・ストリーム両方のウィンドウの範囲で）。
        while !st.local_closed && !st.reset {
            if st.req_chunk.is_none() {
                match st.req_rx.as_ref().map(|rx| rx.try_recv()) {
                    Some(TryRecv::Item(b)) => st.req_chunk = Some((b, 0)),
                    Some(TryRecv::Empty) => break,
                    Some(TryRecv::Closed) | None => {
                        // 本文終端 → 空 DATA + END_STREAM（ウィンドウを消費しない）。
                        st.req_rx = None;
                        fenc.encode_data_into(inner_out, id, &[], true);
                        st.local_closed = true;
                        break;
                    }
                }
            }
            let Some((buf, off)) = st.req_chunk.as_mut() else {
                break;
            };
            let window = st.send_window.min(conn_window);
            if window <= 0 {
                break;
            }
            let n = (buf.len() - *off).min(window as usize).min(max_frame);
            if n > 0 {
                fenc.encode_data_into(inner_out, id, &buf[*off..*off + n], false);
                *off += n;
                st.send_window -= n as i64;
                conn_window -= n as i64;
            }
            if *off >= buf.len() {
                st.req_chunk = None;
            }
        }
        g.conn_send_window = conn_window;

        // 完了: 応答を最後まで渡し、受信側が閉じた。
        if st.remote_closed && st.resp_queue.is_empty() {
            if !st.local_closed && !st.reset {
                // 上流が要求本文を読み切る前に応答を完了させた → 本文の送信を止める。
                g.out
                    .extend_from_slice(&g.fenc.encode_rst_stream(id, ERR_NO_ERROR));
            }
            // resp_tx の drop でチャネルが閉じる（= 正常終了 / Reset 通知済み）。
            g.streams.remove(&id);
        }
    }
    g.conn_recv_delivered += conn_delivered;
    if g.conn_recv_delivered >= CONN_RECV_WINDOW / 2 {
        let n = g.conn_recv_delivered;
        let frame = g.fenc.encode_window_update(0, n);
        g.out.extend_from_slice(&frame);
        g.conn_recv_delivered = 0;
    }
}

// ============================================================================
// プール（ワーカースレッドごと）
// ============================================================================

thread_local! {
    /// 上流 HTTP/2 接続のプール。キーは `PoolKeyStr`（平文 = 接続先、TLS = 接続先 + SNI + 検証有無）。
    static H2_MUX_POOL: RefCell<HashMap<String, Vec<H2Mux>>> = RefCell::new(HashMap::new());
}

/// プール内の接続でストリームを開く（空きのある接続が無ければ `None`）。死んだ接続は外す。
pub(crate) fn pool_open<'h, I>(
    key: &str,
    method: &[u8],
    path: &[u8],
    authority: &[u8],
    headers: I,
    end_stream: bool,
) -> Option<UpStream>
where
    I: Iterator<Item = (&'h [u8], &'h [u8])> + Clone,
{
    H2_MUX_POOL.with(|p| {
        let mut pool = p.borrow_mut();
        let conns = pool.get_mut(key)?;
        conns.retain(|m| !m.is_dead());
        for m in conns.iter() {
            if let Some(s) = m.open(method, path, authority, headers.clone(), end_stream) {
                crate::metrics::record_connection_pool_hit(key);
                return Some(s);
            }
        }
        crate::metrics::record_connection_pool_miss(key);
        None
    })
}

/// 新しく張った接続をプールへ登録する（同時ストリームの上限に達したら次の接続を足す）。
pub(crate) fn pool_insert(key: &str, mux: H2Mux) {
    H2_MUX_POOL.with(|p| {
        let mut pool = p.borrow_mut();
        if let Some(conns) = pool.get_mut(key) {
            conns.retain(|m| !m.is_dead());
            conns.push(mux);
            crate::metrics::set_connection_pool_size(key, conns.len());
            return;
        }
        crate::metrics::set_connection_pool_size(key, 1);
        pool.insert(key.to_string(), vec![mux]);
    });
}

// ============================================================================
// バッファ型の要求（本文を全部送って応答を全部受ける）
// ============================================================================

/// バッファ型で受けた上流 HTTP/2 応答。
pub struct H2cResponse {
    /// ステータスコード
    pub status: u16,
    /// レスポンスヘッダー（疑似ヘッダを除く）
    pub headers: Vec<(Bytes, Bytes)>,
    /// レスポンスボディ
    pub body: Bytes,
    /// トレーラー（gRPC の grpc-status 等）
    pub trailers: Vec<(Bytes, Bytes)>,
}

/// [`collect_response`] の失敗。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CollectError {
    /// ストリームが異常終了した。`retryable` は上流が処理していないことが確実
    /// （REFUSED_STREAM・GOAWAY の範囲外）か、冪等な要求が応答 head より前に失敗した。
    Reset { retryable: bool },
    /// 応答断片の間隔が `read_timeout` を超えた。
    Timeout,
}

/// ストリームの応答を最後まで受けて 1 つにまとめる。`read_timeout` は断片ごとの待ち時間の上限。
pub(crate) async fn collect_response(
    stream: &UpStream,
    idempotent: bool,
    read_timeout: Duration,
) -> Result<H2cResponse, CollectError> {
    let mut resp = H2cResponse {
        status: 0,
        headers: Vec::new(),
        body: Bytes::new(),
        trailers: Vec::new(),
    };
    // 本文が 1 断片なら所有権の移動だけで済ませる（2 断片目から連結する）。
    let mut joined: Option<BytesMut> = None;
    loop {
        let msg = match crate::runtime::time::timeout(read_timeout, stream.resp_rx.recv()).await {
            Ok(m) => m,
            Err(_) => return Err(CollectError::Timeout),
        };
        match msg {
            Some(UpResp::Head { status, headers }) => {
                resp.status = status;
                resp.headers = headers;
            }
            Some(UpResp::Data(b)) => match joined.as_mut() {
                Some(j) => j.extend_from_slice(&b),
                None if resp.body.is_empty() => resp.body = b,
                None => {
                    let mut j = BytesMut::with_capacity(resp.body.len() + b.len());
                    j.extend_from_slice(&resp.body);
                    j.extend_from_slice(&b);
                    joined = Some(j);
                }
            },
            Some(UpResp::Trailers(t)) => resp.trailers = t,
            Some(UpResp::Reset { retryable }) => {
                return Err(CollectError::Reset {
                    retryable: retryable || (idempotent && resp.status == 0),
                });
            }
            None => break,
        }
    }
    if let Some(j) = joined {
        resp.body = j.freeze();
    }
    if resp.status == 0 {
        return Err(CollectError::Reset {
            retryable: idempotent,
        });
    }
    Ok(resp)
}

// ============================================================================
// h2c（平文 HTTP/2 prior knowledge）上流
// ============================================================================

/// h2c 上流でストリームを開く（プールの接続 → 無ければ新規接続）。失敗時は返すべき
/// ステータス（502 = 接続失敗、504 = 接続タイムアウト）。
///
/// `idle_timeout` が 0 ならプールせず、ストリームが終わった時点で接続を閉じる。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn open_h2c<'h, I>(
    target: &crate::config::ProxyTarget,
    key: &str,
    connect_timeout: Duration,
    idle_timeout: Duration,
    method: &[u8],
    path: &[u8],
    authority: &[u8],
    headers: I,
    end_stream: bool,
) -> Result<UpStream, u16>
where
    I: Iterator<Item = (&'h [u8], &'h [u8])> + Clone,
{
    if let Some(s) = pool_open(key, method, path, authority, headers.clone(), end_stream) {
        return Ok(s);
    }
    let addr = target.conn_addr();
    let tcp = match crate::runtime::time::timeout(
        connect_timeout,
        crate::proxy::connect_target(target, addr.as_str()),
    )
    .await
    {
        Ok(Ok(tcp)) => tcp,
        Ok(Err(e)) => {
            warn!("[h2-upstream] connect error ({}): {}", addr.as_str(), e);
            return Err(502);
        }
        Err(_) => {
            warn!("[h2-upstream] connect timeout ({})", addr.as_str());
            return Err(504);
        }
    };
    let _ = tcp.set_nodelay(true);
    let mux = H2Mux::start(tcp, false, idle_timeout);
    let stream = mux
        .open(method, path, authority, headers, end_stream)
        .ok_or(502u16)?;
    if !idle_timeout.is_zero() {
        pool_insert(key, mux);
    }
    Ok(stream)
}

/// h2c 上流へバッファ型で要求する（本文を全部送り、応答を全部受ける）。
///
/// 上流が処理していないことが確実な失敗（REFUSED_STREAM・GOAWAY）と、冪等な要求の応答前の失敗は、
/// 新しいストリームで 1 回だけ再送する（F-177）。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn request_h2c_buffered<'h, I>(
    target: &crate::config::ProxyTarget,
    key: &str,
    connect_timeout: Duration,
    idle_timeout: Duration,
    read_timeout: Duration,
    method: &[u8],
    path: &[u8],
    authority: &[u8],
    headers: I,
    body: Option<Bytes>,
    idempotent: bool,
) -> Result<H2cResponse, u16>
where
    I: Iterator<Item = (&'h [u8], &'h [u8])> + Clone,
{
    let body = body.filter(|b| !b.is_empty());
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut stream = open_h2c(
            target,
            key,
            connect_timeout,
            idle_timeout,
            method,
            path,
            authority,
            headers.clone(),
            body.is_none(),
        )
        .await?;
        if let (Some(b), Some(tx)) = (&body, &stream.req_tx) {
            // 送信失敗（ストリームが既に閉じた）は応答側で判定する。
            let _ = tx.send(b.clone()).await;
        }
        stream.finish_request();
        match collect_response(&stream, idempotent, read_timeout).await {
            Ok(resp) => return Ok(resp),
            Err(CollectError::Reset { retryable: true }) if attempt == 1 => {
                debug!("[h2-upstream] stream failed before processing; retrying once");
                continue;
            }
            Err(CollectError::Reset { .. }) => return Err(502),
            Err(CollectError::Timeout) => return Err(504),
        }
    }
}

// ============================================================================
// TLS 上の HTTP/2（F-175）
// ============================================================================

/// 上流が ALPN で HTTP/1.1 を選んだ結果を覚えておく時間（この間は `"auto"` でも h2 を試さない）。
const H1_ONLY_TTL: Duration = Duration::from_secs(300);

thread_local! {
    /// 上流が ALPN で HTTP/1.1 を選んだ接続先（`PoolKeyStr` → 記録時刻）。
    static H1_ONLY: RefCell<HashMap<String, Instant>> = RefCell::new(HashMap::new());
}

/// `"auto"` の上流が HTTP/1.1 しか話さないと分かっているか（F-175）。
pub(crate) fn https_known_h1(key: &str) -> bool {
    H1_ONLY.with(|m| {
        m.borrow()
            .get(key)
            .is_some_and(|t| t.elapsed() < H1_ONLY_TTL)
    })
}

fn remember_h1(key: &str) {
    H1_ONLY.with(|m| {
        let mut m = m.borrow_mut();
        match m.get_mut(key) {
            Some(t) => *t = Instant::now(),
            None => {
                m.insert(key.to_string(), Instant::now());
            }
        }
    });
}

/// [`open_https`] の結果。
pub(crate) enum HttpsOpen {
    /// HTTP/2 のストリームを開いた。
    H2(UpStream),
    /// 上流は HTTP/1.1。ALPN のために新しく張った接続があれば `Some`（呼び出し側の HTTP/1.1
    /// 経路で使う。接続を無駄にしない）。上流ごとに一度きりの経路なので `Box` で持つ。
    Http1(Option<Box<crate::pool::ClientTls>>),
}

/// HTTPS 上流で HTTP/2 のストリームを開く（F-175）。プールに空きのある接続があればそれを、
/// 無ければ ALPN `h2, http/1.1` で新しく接続する。上流が HTTP/1.1 を選べば
/// `HttpsOpen::Http1`（`http2 = "on"` なら 502）。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn open_https<'h, I>(
    target: &crate::config::ProxyTarget,
    key: &str,
    insecure: bool,
    connect_timeout: Duration,
    idle_timeout: Duration,
    method: &[u8],
    path: &[u8],
    authority: &[u8],
    headers: I,
    end_stream: bool,
) -> Result<HttpsOpen, u16>
where
    I: Iterator<Item = (&'h [u8], &'h [u8])> + Clone,
{
    use crate::config::UpstreamHttp2;
    if let Some(s) = pool_open(key, method, path, authority, headers.clone(), end_stream) {
        return Ok(HttpsOpen::H2(s));
    }
    if target.http2 == UpstreamHttp2::Auto && https_known_h1(key) {
        return Ok(HttpsOpen::Http1(None));
    }
    let addr = target.conn_addr();
    let tcp = match crate::runtime::time::timeout(
        connect_timeout,
        crate::proxy::connect_target(target, addr.as_str()),
    )
    .await
    {
        Ok(Ok(tcp)) => tcp,
        Ok(Err(e)) => {
            warn!("[h2-upstream] connect error ({}): {}", addr.as_str(), e);
            return Err(502);
        }
        Err(_) => {
            warn!("[h2-upstream] connect timeout ({})", addr.as_str());
            return Err(504);
        }
    };
    let _ = tcp.set_nodelay(true);
    let connector = crate::config::get_tls_connector_h2(insecure);
    let tls =
        match crate::runtime::time::timeout(connect_timeout, connector.connect(tcp, target.sni()))
            .await
        {
            Ok(Ok(tls)) => tls,
            Ok(Err(e)) => {
                warn!(
                    "[h2-upstream] TLS handshake error ({}): {}",
                    addr.as_str(),
                    e
                );
                return Err(502);
            }
            Err(_) => {
                warn!("[h2-upstream] TLS handshake timeout ({})", addr.as_str());
                return Err(504);
            }
        };
    if !tls.negotiated_h2() {
        if target.http2 == UpstreamHttp2::On {
            warn!(
                "[h2-upstream] {} did not negotiate h2 (http2 = \"on\")",
                addr.as_str()
            );
            return Err(502);
        }
        remember_h1(key);
        return Ok(HttpsOpen::Http1(Some(Box::new(tls))));
    }
    let mux = H2Mux::start(tls, true, idle_timeout);
    let stream = mux
        .open(method, path, authority, headers, end_stream)
        .ok_or(502u16)?;
    if !idle_timeout.is_zero() {
        pool_insert(key, mux);
    }
    Ok(HttpsOpen::H2(stream))
}

/// [`request_https_buffered`] の結果。
pub(crate) enum HttpsBuffered {
    /// HTTP/2 で応答を受けた。
    H2(H2cResponse),
    /// 上流は HTTP/1.1（[`HttpsOpen::Http1`] と同じ）。
    Http1(Option<Box<crate::pool::ClientTls>>),
}

/// HTTPS 上流へバッファ型で要求する（F-175）。HTTP/2 なら応答を全部受けて返し、
/// 上流が HTTP/1.1 なら呼び出し側の HTTP/1.1 経路へ戻す。再送の規則は [`request_h2c_buffered`] と同じ。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn request_https_buffered<'h, I>(
    target: &crate::config::ProxyTarget,
    key: &str,
    insecure: bool,
    connect_timeout: Duration,
    idle_timeout: Duration,
    read_timeout: Duration,
    method: &[u8],
    path: &[u8],
    authority: &[u8],
    headers: I,
    body: Option<Bytes>,
    idempotent: bool,
) -> Result<HttpsBuffered, u16>
where
    I: Iterator<Item = (&'h [u8], &'h [u8])> + Clone,
{
    let body = body.filter(|b| !b.is_empty());
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut stream = match open_https(
            target,
            key,
            insecure,
            connect_timeout,
            idle_timeout,
            method,
            path,
            authority,
            headers.clone(),
            body.is_none(),
        )
        .await?
        {
            HttpsOpen::H2(s) => s,
            HttpsOpen::Http1(conn) => return Ok(HttpsBuffered::Http1(conn)),
        };
        if let (Some(b), Some(tx)) = (&body, &stream.req_tx) {
            let _ = tx.send(b.clone()).await;
        }
        stream.finish_request();
        match collect_response(&stream, idempotent, read_timeout).await {
            Ok(resp) => return Ok(HttpsBuffered::H2(resp)),
            Err(CollectError::Reset { retryable: true }) if attempt == 1 => continue,
            Err(CollectError::Reset { .. }) => return Err(502),
            Err(CollectError::Timeout) => return Err(504),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::runtime::tcp::TcpStream;
    use std::io::{Read, Write};
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

    /// (クライアント側の非ブロッキング `TcpStream`, サーバ側の同期 `UnixStream`)。
    fn pair() -> (TcpStream, UnixStream) {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: fds は 2 要素の有効な配列。
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(rc, 0);
        // SAFETY: socketpair が返した有効な fd。
        unsafe {
            let fl = libc::fcntl(fds[0], libc::F_GETFL);
            libc::fcntl(fds[0], libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        // SAFETY: socketpair が返した所有 fd をそれぞれ一度だけ所有権移動する。
        unsafe {
            (
                TcpStream::from_raw_fd(fds[0]),
                UnixStream::from_raw_fd(fds[1]),
            )
        }
    }

    /// テスト用の同期 HTTP/2 サーバ側の最小実装。
    struct Peer {
        s: UnixStream,
        enc: FrameEncoder,
        dec: FrameDecoder,
        hdec: HpackDecoder,
        henc: HpackEncoder,
    }

    impl Peer {
        fn new(mut s: UnixStream, settings: &[(u16, u32)]) -> Self {
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut preface = [0u8; 24];
            s.read_exact(&mut preface).unwrap();
            assert_eq!(&preface, defaults::CONNECTION_PREFACE);
            let enc = FrameEncoder::new(defaults::MAX_FRAME_SIZE);
            s.write_all(&enc.encode_settings(settings, false)).unwrap();
            Self {
                s,
                enc,
                dec: FrameDecoder::new(1 << 20),
                hdec: HpackDecoder::new(4096),
                henc: HpackEncoder::new(4096),
            }
        }

        fn read_frame(&mut self) -> Frame {
            let mut h = [0u8; 9];
            self.s.read_exact(&mut h).unwrap();
            let header = self.dec.decode_header(&h).unwrap();
            let mut payload = vec![0u8; header.length as usize];
            self.s.read_exact(&mut payload).unwrap();
            self.dec.decode(&header, &payload).unwrap()
        }

        /// クライアントの要求 HEADERS を読み、`(stream_id, end_stream, :path)` を返す
        /// （SETTINGS・WINDOW_UPDATE 等は読み飛ばす）。
        fn read_headers(&mut self) -> (u32, bool, Vec<u8>) {
            loop {
                if let Frame::Headers {
                    stream_id,
                    end_stream,
                    header_block,
                    ..
                } = self.read_frame()
                {
                    let fields = self.hdec.decode(&header_block).unwrap();
                    let path = fields
                        .iter()
                        .find(|f| f.name.as_ref() == b":path")
                        .map(|f| f.value.to_vec())
                        .unwrap_or_default();
                    return (stream_id, end_stream, path);
                }
            }
        }

        fn respond(&mut self, stream_id: u32, status: &[u8], body: &[u8]) {
            let block = self
                .henc
                .encode(&[
                    (b":status", status, false),
                    (b"content-type", b"text/plain", false),
                ])
                .unwrap();
            let mut out = Vec::new();
            self.enc
                .encode_headers_into(&mut out, stream_id, &block, body.is_empty(), true, None);
            if !body.is_empty() {
                self.enc.encode_data_into(&mut out, stream_id, body, true);
            }
            self.s.write_all(&out).unwrap();
        }
    }

    fn run<F: std::future::Future<Output = ()> + 'static>(f: F) {
        crate::runtime::block_on(f);
    }

    /// アクターの終了を待つ（タスクを残したままランタイムを抜けないため）。
    async fn wait_dead(m: &H2Mux) {
        for _ in 0..500 {
            if m.is_dead() {
                return;
            }
            crate::runtime::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("upstream actor did not exit");
    }

    /// B-106: ストリームの初期ウィンドウ（16KB）を超える本文を 2 ストリーム並行で送っても、
    /// 各ストリームがウィンドウを超えて送らず、WINDOW_UPDATE で再開し、両方とも
    /// 1 本の接続上で完了する（F-174 の多重化）。
    #[test]
    // 理由付き allow: テスト用の同期サーバスレッド（データプレーン非経由）。
    #[allow(clippy::disallowed_methods)]
    fn f174_multiplexes_and_respects_stream_window() {
        if !runtime_available() {
            eprintln!("skip: async runtime unavailable");
            return;
        }
        const WINDOW: u32 = 16 * 1024;
        const BODY: usize = 40 * 1024;
        let (client, server) = pair();
        let srv = std::thread::spawn(move || {
            let mut p = Peer::new(server, &[(0x4, WINDOW), (0x3, 10)]);
            let mut since_update: HashMap<u32, u32> = HashMap::new();
            let mut total: HashMap<u32, usize> = HashMap::new();
            let mut done = 0;
            let mut ids = Vec::new();
            while done < 2 {
                match p.read_frame() {
                    Frame::Headers { stream_id, .. } => ids.push(stream_id),
                    Frame::Data {
                        stream_id,
                        end_stream,
                        data,
                    } => {
                        let n = since_update.entry(stream_id).or_default();
                        *n += data.len() as u32;
                        assert!(*n <= WINDOW, "stream {} exceeded its window", stream_id);
                        *total.entry(stream_id).or_default() += data.len();
                        if *n == WINDOW {
                            let mut wu = p.enc.encode_window_update(stream_id, WINDOW);
                            wu.extend_from_slice(&p.enc.encode_window_update(0, WINDOW));
                            p.s.write_all(&wu).unwrap();
                            *n = 0;
                        }
                        if end_stream {
                            assert_eq!(total[&stream_id], BODY);
                            p.respond(stream_id, b"200", format!("ok-{}", stream_id).as_bytes());
                            done += 1;
                        }
                    }
                    _ => {}
                }
            }
            ids
        });
        run(async move {
            let mux = H2Mux::start(client, false, Duration::from_millis(200));
            let mut a = mux
                .open(b"POST", b"/a", b"up", std::iter::empty(), false)
                .unwrap();
            let mut b = mux
                .open(b"POST", b"/b", b"up", std::iter::empty(), false)
                .unwrap();
            let body = Bytes::from(vec![7u8; BODY]);
            async fn send(s: &UpStream, body: Bytes) {
                let tx = s.req_tx.as_ref().unwrap();
                for chunk in body.chunks(8 * 1024) {
                    tx.send(Bytes::copy_from_slice(chunk)).await.unwrap();
                }
            }
            futures::join!(send(&a, body.clone()), send(&b, body.clone()));
            a.finish_request();
            b.finish_request();
            let ra = collect_response(&a, false, Duration::from_secs(5))
                .await
                .unwrap();
            let rb = collect_response(&b, false, Duration::from_secs(5))
                .await
                .unwrap();
            assert_eq!(ra.status, 200);
            assert_eq!(ra.body.as_ref(), b"ok-1");
            assert_eq!(rb.body.as_ref(), b"ok-3");
            drop((a, b));
            wait_dead(&mux).await;
        });
        let mut ids = srv.join().unwrap();
        ids.sort();
        assert_eq!(ids, vec![1, 3], "both streams must share one connection");
    }

    /// REFUSED_STREAM は再送してよい失敗、それ以外の RST_STREAM は再送しない失敗になる。
    #[test]
    #[allow(clippy::disallowed_methods)]
    fn f174_refused_stream_is_retryable() {
        if !runtime_available() {
            eprintln!("skip: async runtime unavailable");
            return;
        }
        let (client, server) = pair();
        let srv = std::thread::spawn(move || {
            let mut p = Peer::new(server, &[]);
            let (id1, _, _) = p.read_headers();
            p.s.write_all(&p.enc.encode_rst_stream(id1, ERR_REFUSED_STREAM))
                .unwrap();
            let (id2, _, _) = p.read_headers();
            p.s.write_all(&p.enc.encode_rst_stream(id2, ERR_PROTOCOL))
                .unwrap();
            // クライアントが閉じるまで待つ。
            let mut buf = [0u8; 64];
            while matches!(p.s.read(&mut buf), Ok(n) if n > 0) {}
        });
        run(async move {
            let mux = H2Mux::start(client, false, Duration::from_millis(200));
            let s1 = mux
                .open(b"POST", b"/x", b"up", std::iter::empty(), true)
                .unwrap();
            assert_eq!(
                collect_response(&s1, false, Duration::from_secs(5))
                    .await
                    .err(),
                Some(CollectError::Reset { retryable: true })
            );
            let s2 = mux
                .open(b"POST", b"/y", b"up", std::iter::empty(), true)
                .unwrap();
            assert_eq!(
                collect_response(&s2, false, Duration::from_secs(5))
                    .await
                    .err(),
                Some(CollectError::Reset { retryable: false })
            );
            drop((s1, s2));
            wait_dead(&mux).await;
        });
        srv.join().unwrap();
    }

    /// SETTINGS_MAX_CONCURRENT_STREAMS に達した接続には新しいストリームを割り当てず、
    /// ストリームが終われば再び割り当てる。
    #[test]
    #[allow(clippy::disallowed_methods)]
    fn f174_respects_max_concurrent_streams() {
        if !runtime_available() {
            eprintln!("skip: async runtime unavailable");
            return;
        }
        let (client, server) = pair();
        let srv = std::thread::spawn(move || {
            let mut p = Peer::new(server, &[(0x3, 1)]);
            let (id1, _, _) = p.read_headers();
            p.respond(id1, b"200", b"one");
            let (id2, _, path) = p.read_headers();
            assert_eq!(path, b"/second");
            p.respond(id2, b"200", b"two");
            let mut buf = [0u8; 64];
            while matches!(p.s.read(&mut buf), Ok(n) if n > 0) {}
        });
        run(async move {
            let mux = H2Mux::start(client, false, Duration::from_millis(200));
            // サーバの SETTINGS（同時 1 本）が反映されるまで待つ。
            for _ in 0..100 {
                if mux.sh.inner.borrow().peer_max_concurrent == 1 {
                    break;
                }
                crate::runtime::time::sleep(Duration::from_millis(10)).await;
            }
            let s1 = mux
                .open(b"GET", b"/first", b"up", std::iter::empty(), true)
                .unwrap();
            assert!(
                mux.open(b"GET", b"/second", b"up", std::iter::empty(), true)
                    .is_none(),
                "second concurrent stream must not be allocated"
            );
            let r1 = collect_response(&s1, false, Duration::from_secs(5))
                .await
                .unwrap();
            assert_eq!(r1.body.as_ref(), b"one");
            drop(s1);
            let mut s2 = None;
            for _ in 0..100 {
                s2 = mux.open(b"GET", b"/second", b"up", std::iter::empty(), true);
                if s2.is_some() {
                    break;
                }
                crate::runtime::time::sleep(Duration::from_millis(10)).await;
            }
            let s2 = s2.expect("stream slot must be released");
            let r2 = collect_response(&s2, false, Duration::from_secs(5))
                .await
                .unwrap();
            assert_eq!(r2.body.as_ref(), b"two");
            drop(s2);
            wait_dead(&mux).await;
        });
        srv.join().unwrap();
    }

    /// 応答の途中でハンドルを捨てると RST_STREAM(CANCEL) を送り、接続は他のストリームに使える。
    #[test]
    #[allow(clippy::disallowed_methods)]
    fn f174_dropping_stream_sends_cancel() {
        if !runtime_available() {
            eprintln!("skip: async runtime unavailable");
            return;
        }
        let (client, server) = pair();
        let srv = std::thread::spawn(move || {
            let mut p = Peer::new(server, &[]);
            let (id1, _, _) = p.read_headers();
            // head だけ返して本文を保留する。
            let block = p.henc.encode(&[(b":status", b"200", false)]).unwrap();
            p.s.write_all(&p.enc.encode_headers(id1, &block, false, true, None))
                .unwrap();
            // 次の要求の HEADERS と RST_STREAM の順序は問わない。
            let mut cancelled = false;
            let mut next = None;
            while !cancelled || next.is_none() {
                match p.read_frame() {
                    Frame::RstStream {
                        stream_id,
                        error_code,
                    } => {
                        assert_eq!(stream_id, id1);
                        assert_eq!(error_code, ERR_CANCEL);
                        cancelled = true;
                    }
                    Frame::Headers {
                        stream_id,
                        header_block,
                        ..
                    } => {
                        p.hdec.decode(&header_block).unwrap();
                        next = Some(stream_id);
                    }
                    _ => {}
                }
            }
            p.respond(next.unwrap(), b"204", b"");
            let mut buf = [0u8; 64];
            while matches!(p.s.read(&mut buf), Ok(n) if n > 0) {}
        });
        run(async move {
            let mux = H2Mux::start(client, false, Duration::from_millis(200));
            let s1 = mux
                .open(b"GET", b"/slow", b"up", std::iter::empty(), true)
                .unwrap();
            match s1.resp_rx.recv().await {
                Some(UpResp::Head { status, .. }) => assert_eq!(status, 200),
                other => panic!("unexpected {:?}", other),
            }
            drop(s1);
            let s2 = mux
                .open(b"GET", b"/next", b"up", std::iter::empty(), true)
                .unwrap();
            let r2 = collect_response(&s2, false, Duration::from_secs(5))
                .await
                .unwrap();
            assert_eq!(r2.status, 204);
            drop(s2);
            wait_dead(&mux).await;
        });
        srv.join().unwrap();
    }
}
