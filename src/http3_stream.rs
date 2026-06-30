//! # HTTP/3 ストリーミングプロキシ機構（F-32）
//!
//! HTTP/3 のリクエスト／レスポンスボディを **オンメモリに全溜めせず**、フレーム単位で
//! バックエンド⇔クライアント間をストリーミング転送するための機構を提供する。
//!
//! ## アクターモデル（thread-per-core・ロックフリー）
//!
//! quiche の `Connection` / `h3::Connection` は `Send` でなく、UDP I/O・ACK・フロー制御を
//! 単一スレッドの **メインループ**（[`crate::http3_server`] の `run_http3_server_async`）が
//! 専有して駆動する。そのため「バックエンドへの TCP I/O」をメインループ内で同期的に行うと
//! QUIC コネクション全体（他ストリームの ACK 含む）が停止してしまう。
//!
//! 本モジュールは次の 2 アクターを **単一スレッド非同期チャネル + Notify** で接続する:
//!
//! - **メインループ（QUIC/H3 アクター）**: `conn.send/recv`・`h3.poll/send_response/send_body/
//!   recv_body` を唯一駆動する。レスポンスは [`Receiver`]`<`[`RespMsg`]`>` から受け取って
//!   `send_body`、リクエストボディは `recv_body` して [`Sender`]`<`[`bytes::Bytes`]`>` へ流す。
//! - **バックエンドタスク（TCP I/O アクター）**: [`crate::runtime::tcp::TcpStream`]（io_uring）で
//!   バックエンドへ非同期接続し、リクエスト head 送出 → リクエストボディを chunked 逐次転送 →
//!   レスポンス head/body を逐次受信して [`RespMsg`] としてメインループへ送る。
//!
//! チャネルは [`Rc`]`<`[`RefCell`]`>` ベースで **アトミック・ロックを一切使わない**
//! （同一スレッド内の瞬間的 borrow のみ。本クレートの `ConnectionMap` 等と同方針）。
//! 有界チャネルにより、クライアント遅延 → レスポンスチャネル満杯 → バックエンド read 停止、
//! バックエンド遅延 → リクエストチャネル満杯 → `recv_body` 停止 → QUIC フロー制御で
//! クライアント送信停止、という **バックプレッシャ**が双方向に自然伝播する。プロセスの
//! ヒープ保持は「並行ストリーム数 × 1 ストリームあたり有界バッファ」に収まり、**RSS は
//! 総ペイロードサイズに比例しない**。
//!
//! ボディは [`bytes::Bytes`]（参照カウント）でアクター境界を越えて受け渡し、ディープコピーを
//! しない（quiche の `send_body`/`recv_body` が内部で行うコピーは quiche API 由来の不可避分のみ）。

#![cfg(feature = "http3")]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::io;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use bytes::Bytes;
use ftlog::{debug, warn};

use crate::config::UpstreamServer;
use crate::runtime::tcp::TcpStream;
use crate::{AcceptedEncoding, CompressionConfig};

// ============================================================================
// H3Notify — メインループを起こすための単一スレッド通知
// ============================================================================

struct NotifyInner {
    /// 通知済みフラグ（消費されるまで保持されるので取りこぼさない）。
    notified: bool,
    /// メインループ（待機側）の Waker。
    waker: Option<Waker>,
}

/// バックエンドタスク → メインループの起床通知。
///
/// メインループは UDP 受信（`recv_gro_async`）を待ちつつ本 Notify も `select` する。
/// バックエンドタスクがレスポンスチャネルへ push した／リクエストチャネルを消費した際に
/// [`notify`](H3Notify::notify) を呼ぶと、メインループが起きて `drive_proxy_streams` を回す。
#[derive(Clone)]
pub(crate) struct H3Notify {
    inner: Rc<RefCell<NotifyInner>>,
}

impl H3Notify {
    pub(crate) fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(NotifyInner {
                notified: false,
                waker: None,
            })),
        }
    }

    /// メインループを起こす。待機していなければフラグだけ立て、次回 `wait()` で即 Ready になる。
    pub(crate) fn notify(&self) {
        let waker = {
            let mut g = self.inner.borrow_mut();
            g.notified = true;
            g.waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
    }

    /// 通知が来るまで（または既に来ていれば即座に）完了する Future を返す。
    ///
    /// 完了時にフラグを消費する。`select` の負け arm として drop されても副作用はない。
    pub(crate) async fn wait(&self) {
        poll_fn(|cx: &mut Context<'_>| {
            let mut g = self.inner.borrow_mut();
            if g.notified {
                g.notified = false;
                Poll::Ready(())
            } else {
                g.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }
}

// ============================================================================
// 単一スレッド SPSC 非同期チャネル（ロック・アトミックなし）
// ============================================================================

struct Shared<T> {
    queue: VecDeque<T>,
    cap: usize,
    sender_closed: bool,
    receiver_closed: bool,
    recv_waker: Option<Waker>,
    send_waker: Option<Waker>,
}

/// 送信端。
pub(crate) struct Sender<T> {
    sh: Rc<RefCell<Shared<T>>>,
}

/// 受信端。
pub(crate) struct Receiver<T> {
    sh: Rc<RefCell<Shared<T>>>,
}

/// [`Sender::try_send`] の失敗種別。
pub(crate) enum TrySendError<T> {
    /// 容量上限（バックプレッシャ）。アイテムは返却される。
    Full(T),
    /// 受信端が drop 済み。アイテムは返却される。
    Closed(#[allow(dead_code)] T),
}

/// [`Receiver::try_recv`] の結果。
pub(crate) enum TryRecv<T> {
    /// アイテムを取り出した。
    Item(T),
    /// 今は空（送信端は生存）。
    Empty,
    /// 送信端が閉じ、キューも空（= EOF）。
    Closed,
}

/// 容量 `cap`（アイテム数）の有界チャネルを作る。
pub(crate) fn channel<T>(cap: usize) -> (Sender<T>, Receiver<T>) {
    let sh = Rc::new(RefCell::new(Shared {
        queue: VecDeque::new(),
        cap,
        sender_closed: false,
        receiver_closed: false,
        recv_waker: None,
        send_waker: None,
    }));
    (Sender { sh: sh.clone() }, Receiver { sh })
}

impl<T> Sender<T> {
    /// 非ブロッキング送信。容量満杯なら [`TrySendError::Full`]。
    pub(crate) fn try_send(&self, item: T) -> Result<(), TrySendError<T>> {
        let waker = {
            let mut s = self.sh.borrow_mut();
            if s.receiver_closed {
                return Err(TrySendError::Closed(item));
            }
            if s.queue.len() >= s.cap {
                return Err(TrySendError::Full(item));
            }
            s.queue.push_back(item);
            s.recv_waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
        Ok(())
    }

    /// キューが容量上限に達しているか（バックプレッシャ判定）。
    pub(crate) fn is_full(&self) -> bool {
        let s = self.sh.borrow();
        s.queue.len() >= s.cap
    }

    /// 容量が空くまで待ってから送信する。受信端が閉じていれば `Err(())`。
    pub(crate) async fn send(&self, item: T) -> Result<(), ()> {
        let mut item = Some(item);
        poll_fn(|cx: &mut Context<'_>| {
            let waker = {
                let mut s = self.sh.borrow_mut();
                if s.receiver_closed {
                    return Poll::Ready(Err(()));
                }
                if s.queue.len() < s.cap {
                    s.queue
                        .push_back(item.take().expect("send polled after completion"));
                    s.recv_waker.take()
                } else {
                    s.send_waker = Some(cx.waker().clone());
                    return Poll::Pending;
                }
            };
            if let Some(w) = waker {
                w.wake();
            }
            Poll::Ready(Ok(()))
        })
        .await
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let waker = {
            let mut s = self.sh.borrow_mut();
            s.sender_closed = true;
            s.recv_waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
    }
}

impl<T> Receiver<T> {
    /// 非ブロッキング受信。
    pub(crate) fn try_recv(&self) -> TryRecv<T> {
        let (item, waker) = {
            let mut s = self.sh.borrow_mut();
            match s.queue.pop_front() {
                Some(x) => (Some(x), s.send_waker.take()),
                None => {
                    if s.sender_closed {
                        return TryRecv::Closed;
                    }
                    return TryRecv::Empty;
                }
            }
        };
        if let Some(w) = waker {
            w.wake();
        }
        // item は Some 確定。
        TryRecv::Item(item.unwrap())
    }

    /// アイテムが来るまで待つ。送信端が閉じてキューも空なら `None`。
    pub(crate) async fn recv(&self) -> Option<T> {
        poll_fn(|cx: &mut Context<'_>| {
            let (item, waker) = {
                let mut s = self.sh.borrow_mut();
                match s.queue.pop_front() {
                    Some(x) => (x, s.send_waker.take()),
                    None => {
                        if s.sender_closed {
                            return Poll::Ready(None);
                        }
                        s.recv_waker = Some(cx.waker().clone());
                        return Poll::Pending;
                    }
                }
            };
            if let Some(w) = waker {
                w.wake();
            }
            Poll::Ready(Some(item))
        })
        .await
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let waker = {
            let mut s = self.sh.borrow_mut();
            s.receiver_closed = true;
            s.send_waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
    }
}

// ============================================================================
// レスポンスメッセージ（バックエンドタスク → メインループ）
// ============================================================================

/// レスポンスヘッダ（疑似ヘッダ `:status` 以外）。`(name, value)` の所有ペア。
pub(crate) type RespHeaders = Vec<(Bytes, Bytes)>;

/// バックエンドタスクがメインループへ送るレスポンス断片。
///
/// ボディ終端は **送信端（[`Sender`]）の drop** で表す（メインループは
/// [`TryRecv::Closed`] を fin として扱う）。
pub(crate) enum RespMsg {
    /// レスポンス head（ステータス + ヘッダ）。最初に 1 回だけ送られる。
    Head { status: u16, headers: RespHeaders },
    /// レスポンスボディ断片（ゼロコピー）。
    Body(Bytes),
    /// バックエンドエラー（head 送出前なら指定ステータスを返し、送出後はストリームをリセット）。
    Error { status: u16 },
}

// ============================================================================
// バックエンドストリーミングタスク
// ============================================================================

/// バックエンドタスクの起動パラメータ（メインループの `classify` で構築）。
pub(crate) struct BackendTaskParams {
    /// 選択済みアップストリームサーバ（`acquire`/`release` のためクローンを保持）。
    pub server: UpstreamServer,
    /// 完成済み HTTP/1.1 リクエスト head（リクエストライン + ヘッダ + 空行）。ボディは含まない。
    pub request_head: Vec<u8>,
    /// リクエストボディを chunked で転送するか（`true` のとき head は `Transfer-Encoding: chunked`）。
    pub has_request_body: bool,
    /// レスポンス圧縮設定。
    pub compression: CompressionConfig,
    /// クライアントの受理エンコーディング。
    pub client_encoding: AcceptedEncoding,
    /// 接続/読み取りタイムアウト秒。
    pub timeout_secs: u64,
    /// リクエストボディ上限（0 = 無制限）。メインループ側の `ProxyStream` が強制する。
    pub max_request_body: u64,
}

/// バックエンドストリーミングタスクを起動する。
///
/// メインループ（`process_h3_events`）から `crate::runtime::spawn` で呼ばれ、当該リクエストの
/// バックエンド往復を独立タスクとして駆動する。タスクは `connections` を一切触らず、
/// チャネル経由でのみメインループと通信する（quiche の非 Send 制約を満たす）。
pub(crate) fn spawn_backend_task(
    params: BackendTaskParams,
    req_body_rx: Receiver<Bytes>,
    resp_tx: Sender<RespMsg>,
    notify: H3Notify,
) {
    crate::runtime::spawn(async move {
        let server = params.server;
        server.acquire();
        let outcome = run_backend_task(
            &server,
            params.request_head,
            params.has_request_body,
            &params.compression,
            params.client_encoding,
            params.timeout_secs,
            &req_body_rx,
            &resp_tx,
            &notify,
        )
        .await;
        server.release();

        if let Err(status) = outcome {
            // head 送出前のエラーはステータスを通知（送出後は resp_tx drop で fin）。
            let _ = resp_tx.send(RespMsg::Error { status }).await;
        }
        // resp_tx / req_body_rx はここで drop → メインループへ fin（EOF）伝播。
        notify.notify();
    });
}

/// バックエンド往復本体。`Err(status)` は **head 未送出時のみ** のエラー（指定ステータスを返す）。
#[allow(clippy::too_many_arguments)]
async fn run_backend_task(
    server: &UpstreamServer,
    request_head: Vec<u8>,
    has_request_body: bool,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    timeout_secs: u64,
    req_body_rx: &Receiver<Bytes>,
    resp_tx: &Sender<RespMsg>,
    notify: &H3Notify,
) -> Result<(), u16> {
    let target = &server.target;
    let addr = format!("{}:{}", target.host, target.port);

    // --- 非同期接続（タイムアウト付き） ---
    let connect = TcpStream::connect_str(&addr);
    let backend =
        match crate::runtime::time::timeout(Duration::from_secs(timeout_secs), connect).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                warn!("[HTTP/3] streaming backend connect error: {}", e);
                return Err(502);
            }
            Err(_) => {
                warn!("[HTTP/3] streaming backend connect timeout");
                return Err(504);
            }
        };
    let _ = backend.set_nodelay(true);

    // --- リクエスト head + ボディフレーミングの確定 ---
    // `request_head` は `...Connection: close\r\n`（末尾空行なし・ボディフレーミングなし）。
    // ボディ有無は HEADERS 受信時点では確定しない（h3 クライアントは HEADERS と fin を別送する
    // ため、ボディのない GET でも `more_frames=true`）。そこで **最初のボディ断片が実際に届くか**
    // を見てから framing を確定する: 届けば `Transfer-Encoding: chunked`、届かなければボディなし。
    let mut head = request_head;
    let first_chunk = if has_request_body {
        req_body_rx.recv().await
    } else {
        None
    };

    match first_chunk {
        Some(first) => {
            // 実ボディあり → chunked 逐次転送。
            head.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
            if let Err(e) = write_all(&backend, Bytes::from(head)).await {
                warn!("[HTTP/3] streaming backend head write error: {}", e);
                return Err(502);
            }
            if let Err(e) = send_backend_chunk(&backend, first).await {
                warn!("[HTTP/3] streaming backend body write error: {}", e);
                return Err(502);
            }
            notify.notify();
            // クライアント側 END_STREAM（送信端 drop / 明示クローズ）まで逐次転送。
            while let Some(chunk) = req_body_rx.recv().await {
                // 書き込み完了まで次フレームを読まない（バックプレッシャ）。
                if let Err(e) = send_backend_chunk(&backend, chunk).await {
                    warn!("[HTTP/3] streaming backend body write error: {}", e);
                    return Err(502);
                }
                notify.notify();
            }
            // 終端チャンク。
            if let Err(e) = write_all(&backend, Bytes::from_static(b"0\r\n\r\n")).await {
                warn!("[HTTP/3] streaming backend terminator write error: {}", e);
                return Err(502);
            }
        }
        None => {
            // ボディなし（GET 等、または more_frames=true でも実データ無し） → 空行で head 終端。
            head.extend_from_slice(b"\r\n");
            if let Err(e) = write_all(&backend, Bytes::from(head)).await {
                warn!("[HTTP/3] streaming backend head write error: {}", e);
                return Err(502);
            }
        }
    }

    // --- レスポンス受信（head → body 逐次） ---
    stream_response(
        &backend,
        compression,
        client_encoding,
        timeout_secs,
        resp_tx,
        notify,
    )
    .await
}

/// バックエンドレスポンスを head→body の順で受信し、メインループへ逐次転送する。
async fn stream_response(
    backend: &TcpStream,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    timeout_secs: u64,
    resp_tx: &Sender<RespMsg>,
    notify: &H3Notify,
) -> Result<(), u16> {
    // 読み取りバッファ（所有権ベース read のため都度払い出し→受け取り）。
    let mut read_buf = vec![0u8; RESP_READ_CHUNK];
    let mut head_buf: Vec<u8> = Vec::with_capacity(4096);
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);

    // --- ヘッダ終端まで読む ---
    let header_end;
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(504);
        }
        let (res, buf) = read_backend(backend, read_buf).await;
        read_buf = buf;
        let n = match res {
            Ok(0) => {
                warn!("[HTTP/3] streaming backend closed before response headers");
                return Err(502);
            }
            Ok(n) => n,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => {
                warn!("[HTTP/3] streaming backend read error: {}", e);
                return Err(502);
            }
        };
        head_buf.extend_from_slice(&read_buf[..n]);
        if let Some(pos) = find_header_end(&head_buf) {
            header_end = pos;
            break;
        }
        if head_buf.len() > MAX_RESP_HEADER {
            warn!("[HTTP/3] streaming backend response headers too large");
            return Err(502);
        }
    }

    let status = parse_status_code(&head_buf[..header_end]).unwrap_or(502);
    let parsed = parse_response_headers(&head_buf[..header_end]);
    // ヘッダ終端（\r\n\r\n）以降は先読みしたボディ断片。
    let leftover = Bytes::copy_from_slice(&head_buf[header_end + 4..]);

    // 圧縮判定（content-type / 既存エンコーディング / 既知長）。
    let should_compress = compression.should_compress(
        client_encoding,
        parsed.content_type.as_deref(),
        parsed.content_length,
        parsed.content_encoding.as_deref(),
    );

    if let Some(enc) = should_compress {
        // 圧縮はボディ全体が必要 → バッファ経路（HTTP/2 第1フェーズと同方針）。
        return stream_response_compressed(
            backend,
            status,
            parsed,
            leftover,
            read_buf,
            enc,
            compression,
            deadline,
            resp_tx,
        )
        .await;
    }

    // --- head 送出（非圧縮ストリーミング） ---
    let mut headers = parsed.headers;
    // 長さ既知ならそのまま転送（クライアントへ content-length 提示）。chunked/EOF は length 削除。
    if parsed.is_chunked {
        // chunked のデータ長は不定 → content-length は付けない（quiche がストリーム長を管理）。
        headers.retain(|(n, _)| !n.eq_ignore_ascii_case(b"content-length"));
    }
    if resp_tx
        .send(RespMsg::Head { status, headers })
        .await
        .is_err()
    {
        return Ok(()); // クライアント切断。
    }
    notify.notify();

    // --- body 逐次転送 ---
    match parsed.framing {
        Framing::Length(total) => {
            stream_body_length(
                backend, leftover, read_buf, total, deadline, resp_tx, notify,
            )
            .await
        }
        Framing::Chunked => {
            stream_body_chunked(backend, leftover, read_buf, deadline, resp_tx, notify).await
        }
        Framing::Eof => {
            stream_body_eof(backend, leftover, read_buf, deadline, resp_tx, notify).await
        }
    }
}

/// 非圧縮・content-length 既知（または不明だが length フレーミング）のボディ転送。
async fn stream_body_length(
    backend: &TcpStream,
    leftover: Bytes,
    mut read_buf: Vec<u8>,
    total: usize,
    deadline: std::time::Instant,
    resp_tx: &Sender<RespMsg>,
    notify: &H3Notify,
) -> Result<(), u16> {
    let mut sent = 0usize;
    if !leftover.is_empty() {
        let take = leftover.len().min(total);
        if send_body_bytes(resp_tx, notify, leftover.slice(0..take))
            .await
            .is_err()
        {
            return Ok(());
        }
        sent += take;
    }
    while sent < total {
        if std::time::Instant::now() >= deadline {
            return Ok(()); // 既に head 送出済み → fin（resp_tx drop）で閉じる。
        }
        let (res, buf) = read_backend(backend, read_buf).await;
        read_buf = buf;
        match res {
            Ok(0) => break,
            Ok(n) => {
                let take = n.min(total - sent);
                let chunk = bytes_from_read(&read_buf, take);
                if send_body_bytes(resp_tx, notify, chunk).await.is_err() {
                    return Ok(());
                }
                sent += take;
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        }
    }
    Ok(())
}

/// 非圧縮・chunked のボディ転送（`ChunkedDecoder::next_data_span` でゼロコピーデコード）。
async fn stream_body_chunked(
    backend: &TcpStream,
    leftover: Bytes,
    mut read_buf: Vec<u8>,
    deadline: std::time::Instant,
    resp_tx: &Sender<RespMsg>,
    notify: &H3Notify,
) -> Result<(), u16> {
    use crate::http_utils::ChunkedDecoder;
    let mut decoder = ChunkedDecoder::new_unlimited();

    // 先読み分を先にデコード。
    if !leftover.is_empty() && drain_chunked(&mut decoder, &leftover, resp_tx, notify).await? {
        return Ok(());
    }

    loop {
        if std::time::Instant::now() >= deadline {
            return Ok(());
        }
        let (res, buf) = read_backend(backend, read_buf).await;
        read_buf = buf;
        match res {
            Ok(0) => break,
            Ok(n) => {
                // read_buf の先頭 n バイトを Bytes 化してデコード（span はこの Bytes のスライス）。
                let data = bytes_from_read(&read_buf, n);
                if drain_chunked(&mut decoder, &data, resp_tx, notify).await? {
                    break;
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => {
                debug!("[HTTP/3] streaming chunked read error: {}", e);
                break;
            }
        }
    }
    Ok(())
}

/// 1 入力バッファ分の chunked を `next_data_span` で逐次デコードしてストリーム送出する。
/// 終端（complete）に達したら `Ok(true)` を返す。
async fn drain_chunked(
    decoder: &mut crate::http_utils::ChunkedDecoder,
    data: &Bytes,
    resp_tx: &Sender<RespMsg>,
    notify: &H3Notify,
) -> Result<bool, u16> {
    let mut pos = 0;
    while pos < data.len() {
        let span = decoder.next_data_span(&data[pos..]);
        if span.data_len > 0 {
            // 入力 Bytes のサブスライス（ゼロコピー）。
            let start = pos + span.data_start;
            let chunk = data.slice(start..start + span.data_len);
            if send_body_bytes(resp_tx, notify, chunk).await.is_err() {
                return Ok(true); // クライアント切断 → 終了扱い。
            }
        }
        if span.complete {
            return Ok(true);
        }
        if span.consumed == 0 {
            break; // これ以上進めない（次の read を待つ）。
        }
        pos += span.consumed;
    }
    Ok(false)
}

/// 非圧縮・EOF 終端（`Connection: close`）のボディ転送。
async fn stream_body_eof(
    backend: &TcpStream,
    leftover: Bytes,
    mut read_buf: Vec<u8>,
    deadline: std::time::Instant,
    resp_tx: &Sender<RespMsg>,
    notify: &H3Notify,
) -> Result<(), u16> {
    if !leftover.is_empty() && send_body_bytes(resp_tx, notify, leftover).await.is_err() {
        return Ok(());
    }
    loop {
        if std::time::Instant::now() >= deadline {
            return Ok(());
        }
        let (res, buf) = read_backend(backend, read_buf).await;
        read_buf = buf;
        match res {
            Ok(0) => break,
            Ok(n) => {
                let chunk = bytes_from_read(&read_buf, n);
                if send_body_bytes(resp_tx, notify, chunk).await.is_err() {
                    return Ok(());
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        }
    }
    Ok(())
}

/// 圧縮経路: ボディ全体を読み切り、圧縮してから head + body を送る。
#[allow(clippy::too_many_arguments)]
async fn stream_response_compressed(
    backend: &TcpStream,
    status: u16,
    parsed: ParsedHeaders,
    leftover: Bytes,
    mut read_buf: Vec<u8>,
    enc: AcceptedEncoding,
    compression: &CompressionConfig,
    deadline: std::time::Instant,
    resp_tx: &Sender<RespMsg>,
) -> Result<(), u16> {
    // ボディ全体を読み取る（圧縮に必要）。
    let mut body: Vec<u8> = Vec::with_capacity(leftover.len().max(RESP_READ_CHUNK));
    let mut decoder = if parsed.is_chunked {
        Some(crate::http_utils::ChunkedDecoder::new_unlimited())
    } else {
        None
    };
    let mut remaining = parsed.content_length;

    // 先読み分。
    accumulate_body(&mut body, &mut decoder, &mut remaining, &leftover);
    let mut done =
        decoder.as_ref().map(|d| d.is_complete()).unwrap_or(false) || remaining == Some(0);

    while !done {
        if std::time::Instant::now() >= deadline {
            break;
        }
        let (res, buf) = read_backend(backend, read_buf).await;
        read_buf = buf;
        match res {
            Ok(0) => break,
            Ok(n) => {
                let slice = Bytes::copy_from_slice(&read_buf[..n]);
                accumulate_body(&mut body, &mut decoder, &mut remaining, &slice);
                done = decoder.as_ref().map(|d| d.is_complete()).unwrap_or(false)
                    || remaining == Some(0);
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        }
    }

    let compressed = crate::http3_server::compress_body_h3(&body, enc, compression);

    // ヘッダ調整: content-length / content-encoding を差し替え。
    let mut headers = parsed.headers;
    headers.retain(|(n, _)| {
        !n.eq_ignore_ascii_case(b"content-length") && !n.eq_ignore_ascii_case(b"content-encoding")
    });
    headers.push((
        Bytes::from_static(b"content-encoding"),
        Bytes::from_static(enc.as_header_value()),
    ));

    if resp_tx
        .send(RespMsg::Head { status, headers })
        .await
        .is_err()
    {
        return Ok(());
    }
    let _ = resp_tx.send(RespMsg::Body(Bytes::from(compressed))).await;
    Ok(())
}

/// 圧縮経路用: 1 入力スライスをデコード（chunked）または素通し（length/eof）して body へ蓄積。
fn accumulate_body(
    body: &mut Vec<u8>,
    decoder: &mut Option<crate::http_utils::ChunkedDecoder>,
    remaining: &mut Option<usize>,
    data: &Bytes,
) {
    if let Some(dec) = decoder {
        let mut pos = 0;
        while pos < data.len() {
            let span = dec.next_data_span(&data[pos..]);
            if span.data_len > 0 {
                let start = pos + span.data_start;
                body.extend_from_slice(&data[start..start + span.data_len]);
            }
            if span.complete || span.consumed == 0 {
                break;
            }
            pos += span.consumed;
        }
    } else if let Some(rem) = remaining {
        let take = data.len().min(*rem);
        body.extend_from_slice(&data[..take]);
        *rem -= take;
    } else {
        body.extend_from_slice(data);
    }
}

// ============================================================================
// 内部ヘルパー
// ============================================================================

/// レスポンス読み取り 1 回分のサイズ。
const RESP_READ_CHUNK: usize = 32 * 1024;
/// レスポンスヘッダの最大許容サイズ。
const MAX_RESP_HEADER: usize = 256 * 1024;

/// `read_buf` の先頭 `n` バイトをゼロコピー前提の `Bytes` に変換する。
///
/// アクター境界を越えて所有権を移すため、1 チャンク = 1 確保が必要（quiche `send_body` が
/// 内部コピーするのと同様、設計上不可避な確保）。`Vec` を切り詰めて `Bytes` 化することで
/// **追加のディープコピーは発生しない**（`Vec` → `Bytes` はバッファ移譲）。
#[inline]
fn bytes_from_read(read_buf: &[u8], n: usize) -> Bytes {
    let mut v = Vec::with_capacity(n);
    v.extend_from_slice(&read_buf[..n]);
    Bytes::from(v)
}

/// ボディ断片をレスポンスチャネルへ送る（送出後にメインループへ通知）。
#[inline]
async fn send_body_bytes(
    resp_tx: &Sender<RespMsg>,
    notify: &H3Notify,
    chunk: Bytes,
) -> Result<(), ()> {
    resp_tx.send(RespMsg::Body(chunk)).await?;
    notify.notify();
    Ok(())
}

/// io_uring RECV を発行し、EAGAIN 時は POLL_ADD（`readable()`）で読み取り可能を待って
/// からリトライする。**ビジースピンせず**イベントループへ制御を返すため、メインループ
/// （QUIC 駆動）が starve しない（io_uring RECV は無データ時に EAGAIN を返し得る）。
async fn read_backend(backend: &TcpStream, mut buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
    loop {
        let (res, b) = backend.read(buf).await;
        buf = b;
        match res {
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                if let Err(e) = backend.readable().await {
                    return (Err(e), buf);
                }
            }
            other => return (other, buf),
        }
    }
}

/// 所有バッファ（`Bytes`）をバックエンドへ全量書き込む（部分書き込みを処理）。
///
/// EAGAIN 時は POLL_ADD（`writable()`）で書き込み可能を待ってからリトライする。**ビジー
/// スピンしない**（大容量チャンクで送信バッファが埋まってもメインループを starve させない）。
async fn write_all(backend: &TcpStream, mut buf: Bytes) -> io::Result<()> {
    use bytes::Buf;
    while !buf.is_empty() {
        let len = buf.len();
        let (res, returned) = backend.write(buf).await;
        match res {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "backend write returned 0",
                ))
            }
            Ok(n) if n >= len => return Ok(()),
            Ok(n) => {
                let mut b = returned;
                b.advance(n);
                buf = b;
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                buf = returned;
                backend.writable().await?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// 1 つのリクエストボディフレームを chunked エンコードしてバックエンドへ送る。
///
/// チャンクサイズ行と終端 CRLF のみ小バッファを確保し、**ペイロード本体は受信フレームの
/// 所有バッファ（`Bytes`）をそのまま書き込む（ゼロコピー）**。
async fn send_backend_chunk(backend: &TcpStream, data: Bytes) -> io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    let mut header = Vec::with_capacity(18);
    push_chunk_size_line(&mut header, data.len());
    write_all(backend, Bytes::from(header)).await?;
    write_all(backend, data).await?;
    write_all(backend, Bytes::from_static(b"\r\n")).await?;
    Ok(())
}

/// chunked のチャンクサイズ行（`<hex>\r\n`）を `buf` へ追記する（`format!` を避ける）。
pub(crate) fn push_chunk_size_line(buf: &mut Vec<u8>, mut n: usize) {
    if n == 0 {
        buf.push(b'0');
    } else {
        let mut tmp = [0u8; 16];
        let mut i = tmp.len();
        while n > 0 {
            i -= 1;
            let d = (n & 0xf) as u8;
            tmp[i] = if d < 10 { b'0' + d } else { b'a' + (d - 10) };
            n >>= 4;
        }
        buf.extend_from_slice(&tmp[i..]);
    }
    buf.extend_from_slice(b"\r\n");
}

/// レスポンスボディのフレーミング種別。
enum Framing {
    /// Content-Length 既知。
    Length(usize),
    /// Transfer-Encoding: chunked。
    Chunked,
    /// 長さ不明（Connection: close で EOF 終端）。
    Eof,
}

/// パース済みレスポンスヘッダ。
struct ParsedHeaders {
    /// クライアントへ転送するヘッダ（ホップバイホップ除去済み）。
    headers: RespHeaders,
    framing: Framing,
    is_chunked: bool,
    content_length: Option<usize>,
    content_type: Option<Bytes>,
    content_encoding: Option<Bytes>,
}

/// HTTP/1.1 レスポンスヘッダ部（ステータス行除く）をパースし、転送用ヘッダとフレーミングを返す。
fn parse_response_headers(header_bytes: &[u8]) -> ParsedHeaders {
    let mut headers: RespHeaders = Vec::new();
    let mut is_chunked = false;
    let mut content_length: Option<usize> = None;
    let mut content_type: Option<Bytes> = None;
    let mut content_encoding: Option<Bytes> = None;

    // 最初の行（ステータス行）はスキップ。
    let after_status = memchr::memchr(b'\n', header_bytes)
        .map(|i| &header_bytes[i + 1..])
        .unwrap_or(&[]);

    for line in after_status.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let colon = match memchr::memchr(b':', line) {
            Some(c) => c,
            None => continue,
        };
        let name = &line[..colon];
        let value = line[colon + 1..]
            .strip_prefix(b" ")
            .unwrap_or(&line[colon + 1..]);

        if name.eq_ignore_ascii_case(b"transfer-encoding") {
            if value.eq_ignore_ascii_case(b"chunked")
                || value.to_ascii_lowercase().ends_with(b"chunked")
            {
                is_chunked = true;
            }
            continue; // HTTP/3 へは転送しない。
        }
        if name.eq_ignore_ascii_case(b"connection") || name.eq_ignore_ascii_case(b"keep-alive") {
            continue; // ホップバイホップ。
        }
        if name.eq_ignore_ascii_case(b"content-length") {
            if let Ok(s) = std::str::from_utf8(value) {
                content_length = s.trim().parse().ok();
            }
            // content-length は転送ヘッダにも残す（length フレーミングのクライアント提示用）。
        }
        if name.eq_ignore_ascii_case(b"content-type") {
            content_type = Some(Bytes::copy_from_slice(value));
        }
        if name.eq_ignore_ascii_case(b"content-encoding") {
            content_encoding = Some(Bytes::copy_from_slice(value));
        }
        headers.push((Bytes::copy_from_slice(name), Bytes::copy_from_slice(value)));
    }

    let framing = if is_chunked {
        Framing::Chunked
    } else if let Some(len) = content_length {
        Framing::Length(len)
    } else {
        Framing::Eof
    };

    ParsedHeaders {
        headers,
        framing,
        is_chunked,
        content_length,
        content_type,
        content_encoding,
    }
}

/// HTTP レスポンスのヘッダ終端（`\r\n\r\n`）位置を返す。
fn find_header_end(data: &[u8]) -> Option<usize> {
    let mut search_from = 0;
    while let Some(idx) = memchr::memchr(b'\r', &data[search_from..]) {
        let pos = search_from + idx;
        if data.len() >= pos + 4 && &data[pos..pos + 4] == b"\r\n\r\n" {
            return Some(pos);
        }
        search_from = pos + 1;
        if search_from >= data.len() {
            break;
        }
    }
    None
}

/// ステータス行からステータスコードをパースする。
fn parse_status_code(header: &[u8]) -> Option<u16> {
    let first_line = header.split(|&b| b == b'\n').next()?;
    let mut parts = first_line.split(|&b| b == b' ').filter(|s| !s.is_empty());
    let _http = parts.next()?;
    let code = parts.next()?;
    std::str::from_utf8(code).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同一スレッドで Future を 1 つ実行する最小ランタイム（テスト用）。
    fn block_on<F: std::future::Future>(mut fut: F) -> F::Output {
        use std::sync::Arc;
        use std::task::{Context, Poll, Wake, Waker};
        struct NoopWake;
        impl Wake for NoopWake {
            fn wake(self: Arc<Self>) {}
        }
        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        let mut fut = unsafe { std::pin::Pin::new_unchecked(&mut fut) };
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => {} // テストでは Pending を即リトライ（busy）しない設計のもののみ使用。
            }
        }
    }

    #[test]
    fn channel_try_send_recv_roundtrip() {
        let (tx, rx) = channel::<u32>(2);
        assert!(tx.try_send(1).is_ok());
        assert!(tx.try_send(2).is_ok());
        // 容量超過。
        assert!(matches!(tx.try_send(3), Err(TrySendError::Full(3))));
        assert!(matches!(rx.try_recv(), TryRecv::Item(1)));
        assert!(matches!(rx.try_recv(), TryRecv::Item(2)));
        assert!(matches!(rx.try_recv(), TryRecv::Empty));
        // 容量が空いたので送れる。
        assert!(tx.try_send(3).is_ok());
        assert!(matches!(rx.try_recv(), TryRecv::Item(3)));
    }

    #[test]
    fn channel_sender_drop_signals_eof() {
        let (tx, rx) = channel::<u32>(4);
        assert!(tx.try_send(7).is_ok());
        drop(tx);
        assert!(matches!(rx.try_recv(), TryRecv::Item(7)));
        // 送信端 drop 後、キュー空 → Closed（EOF）。
        assert!(matches!(rx.try_recv(), TryRecv::Closed));
    }

    #[test]
    fn channel_receiver_drop_blocks_send() {
        let (tx, rx) = channel::<u32>(1);
        drop(rx);
        assert!(matches!(tx.try_send(1), Err(TrySendError::Closed(1))));
    }

    #[test]
    fn channel_async_send_recv() {
        // 容量 1。send→recv→send→recv を単一スレッドで交互に進める。
        let (tx, rx) = channel::<u32>(1);
        block_on(async {
            tx.send(10).await.unwrap();
            assert_eq!(rx.recv().await, Some(10));
            tx.send(20).await.unwrap();
            assert_eq!(rx.recv().await, Some(20));
            drop(tx);
            assert_eq!(rx.recv().await, None);
        });
    }

    #[test]
    fn notify_sets_and_consumes_flag() {
        let n = H3Notify::new();
        n.notify();
        // 既に通知済みなら wait は即完了。
        block_on(n.wait());
        // 消費済み: 再 notify するまで Pending（busy ループ回避のためここでは検証のみ）。
        assert!(!n.inner.borrow().notified);
    }

    #[test]
    fn push_chunk_size_line_hex() {
        let mut b = Vec::new();
        push_chunk_size_line(&mut b, 0);
        assert_eq!(b, b"0\r\n");
        let mut b = Vec::new();
        push_chunk_size_line(&mut b, 255);
        assert_eq!(b, b"ff\r\n");
        let mut b = Vec::new();
        push_chunk_size_line(&mut b, 7000);
        assert_eq!(b, format!("{:x}\r\n", 7000).into_bytes());
    }

    #[test]
    fn find_header_end_works() {
        assert_eq!(find_header_end(b"HTTP/1.1 200 OK\r\n\r\nbody"), Some(15));
        assert_eq!(find_header_end(b"no terminator"), None);
        assert_eq!(
            find_header_end(b"A: b\r\nC: d\r\n\r\n"),
            Some(b"A: b\r\nC: d".len())
        );
    }

    #[test]
    fn parse_status_code_works() {
        assert_eq!(parse_status_code(b"HTTP/1.1 200 OK"), Some(200));
        assert_eq!(parse_status_code(b"HTTP/1.1 404 Not Found"), Some(404));
        assert_eq!(parse_status_code(b"HTTP/1.0 500"), Some(500));
        assert_eq!(parse_status_code(b"garbage"), None);
    }

    #[test]
    fn parse_response_headers_chunked() {
        let h = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\nConnection: close";
        let parsed = parse_response_headers(h);
        assert!(parsed.is_chunked);
        assert!(matches!(parsed.framing, Framing::Chunked));
        // transfer-encoding / connection は転送されない。
        assert!(parsed
            .headers
            .iter()
            .all(|(n, _)| !n.eq_ignore_ascii_case(b"transfer-encoding")
                && !n.eq_ignore_ascii_case(b"connection")));
        assert_eq!(parsed.content_type.as_deref(), Some(&b"text/plain"[..]));
    }

    #[test]
    fn parse_response_headers_length() {
        let h =
            b"HTTP/1.1 200 OK\r\nContent-Length: 1234\r\nContent-Type: application/octet-stream";
        let parsed = parse_response_headers(h);
        assert!(!parsed.is_chunked);
        assert_eq!(parsed.content_length, Some(1234));
        assert!(matches!(parsed.framing, Framing::Length(1234)));
        // content-length は転送ヘッダに残る。
        assert!(parsed
            .headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(b"content-length")));
    }

    #[test]
    fn parse_response_headers_eof() {
        let h = b"HTTP/1.1 200 OK\r\nServer: x";
        let parsed = parse_response_headers(h);
        assert!(matches!(parsed.framing, Framing::Eof));
        assert_eq!(parsed.content_length, None);
    }
}
