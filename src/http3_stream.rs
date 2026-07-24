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
use crate::runtime::handle::AsRawFd;
use std::io;
use std::rc::Rc;
use std::time::Duration;

use bytes::Bytes;
use ftlog::{debug, warn};

use crate::config::UpstreamServer;
use crate::runtime::tcp::TcpStream;
use crate::{AcceptedEncoding, CompressionConfig};

// ============================================================================
// アクターモデル共通プリミティブの再エクスポート（F-116 で stream_channel へ抽出）
// ============================================================================
//
// `H3Notify` / `channel` / `Sender` / `Receiver` / `TrySendError` / `TryRecv` は
// HTTP/2 多重化と共有するため [`crate::stream_channel`] へ移設した。HTTP/3 側は
// 従来の呼び出し名（`H3Notify` 等）を保つため型エイリアス付きで再エクスポートする。
pub(crate) use crate::stream_channel::{
    channel, Notify as H3Notify, Receiver, Sender, TryRecv, TrySendError,
};

// ============================================================================
// バックエンド I/O 抽象（F-44: 平文 TCP / TLS バックエンドの全二重ストリーミング）
// ============================================================================

/// バックエンドへの接続。平文 TCP または TLS（rustls / kTLS）。
///
/// アップロード（リクエストボディ送信）とレスポンス受信は **同一タスク内で
/// `select_biased!` により並行駆動**されるため、read / write とも `&self` で
/// 呼べる必要がある。平文は io_uring `TcpStream` がもともと `&self` API。TLS は
/// [`TlsBackend`] が rustls セッションを `RefCell` で内包し、**借用を `.await` を
/// 跨いで保持しない**よう read / write の状態機械を実装する（thread-per-core 前提）。
pub(crate) enum BackendIo {
    /// 平文 TCP（従来経路）。
    Plain(TcpStream),
    /// TLS バックエンド（rustls ユーザー空間 or kTLS 移行済み）。
    Tls(Box<TlsBackend>),
}

impl BackendIo {
    /// 所有バッファへ読み取る（EAGAIN 時は POLL_ADD で待機、ビジースピンしない）。
    async fn read_into(&self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        match self {
            BackendIo::Plain(s) => read_tcp(s, buf).await,
            BackendIo::Tls(t) => t.read_into(buf).await,
        }
    }

    /// 所有バッファ（`Bytes`）を全量書き込む（部分書き込み・EAGAIN を処理）。
    async fn write_all(&self, data: Bytes) -> io::Result<()> {
        match self {
            BackendIo::Plain(s) => write_all_tcp(s, data).await,
            BackendIo::Tls(t) => t.write_all(data).await,
        }
    }
}

/// TLS バックエンドの全二重ラッパー（F-44）。
///
/// `KtlsClientStream` / `SimpleTlsClientStream` の I/O は `&mut self` を要求するため、
/// アップロードとレスポンス受信の同一タスク内並行駆動（`&self` 共有）ができない。
/// 本型はハンドシェイク済みストリームを `into_parts()` で分解して受け取り、
/// rustls セッションを `RefCell` に置いて read / write を `&self` で提供する。
///
/// **不変条件**: `RefCell` の借用は同期区間のみで完結し、`.await`（`readable()` /
/// `writable()`）を跨いで保持しない。single-thread executor 上でのみ使用する。
pub(crate) struct TlsBackend {
    /// 基盤 TCP ストリーム（`readable()` / `writable()` の POLL_ADD 待機に使用）。
    inner: TcpStream,
    /// ユーザー空間 rustls セッション。kTLS 移行済み（生ソケット I/O 可能）なら `None`。
    session: Option<RefCell<rustls::ClientConnection>>,
    /// rustls が復号済みの平文の退避バッファ（received_plaintext 上限溢れ防止兼リード供給源）。
    drained: RefCell<Vec<u8>>,
    /// 暗号文読み取りスクラッチ（確保再利用。借用は await を跨がないため take/replace で移動）。
    read_scratch: RefCell<Vec<u8>>,
    /// TLS レコード書き出しスクラッチ（同上）。
    write_scratch: RefCell<Vec<u8>>,
}

/// TLS スクラッチバッファサイズ（rustls の最大レコード長 16KB に合わせる）。
const TLS_SCRATCH: usize = 16 * 1024;

impl TlsBackend {
    /// ハンドシェイク済みストリームの構成要素からラッパーを構築する。
    ///
    /// `session` が `None` の場合は kTLS 移行済みで、生ソケット I/O（io_uring）を使う。
    fn new(inner: TcpStream, session: Option<rustls::ClientConnection>, drained: Vec<u8>) -> Self {
        // 生 read/write（ノンブロッキング前提）を行うため O_NONBLOCK を保証する
        // （io_uring の CONNECT は O_NONBLOCK を保証しない。ktls_rustls::connect と同方針）。
        #[cfg(unix)]
        {
            let fd = inner.as_raw_fd();
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL, 0);
                if flags >= 0 && (flags & libc::O_NONBLOCK) == 0 {
                    libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
            }
        }
        Self {
            inner,
            session: session.map(RefCell::new),
            drained: RefCell::new(drained),
            read_scratch: RefCell::new(vec![0u8; TLS_SCRATCH]),
            write_scratch: RefCell::new(Vec::with_capacity(TLS_SCRATCH)),
        }
    }

    /// ドレイン済み平文を `buf` 先頭へコピーして返す（無ければ `None`）。
    fn copy_drained(&self, buf: &mut [u8]) -> Option<usize> {
        let mut d = self.drained.borrow_mut();
        if d.is_empty() {
            return None;
        }
        let n = d.len().min(buf.len());
        buf[..n].copy_from_slice(&d[..n]);
        d.drain(..n);
        Some(n)
    }

    /// 平文を読み取る。復号済みバッファ → rustls セッション → 生ソケットの順に供給する。
    async fn read_into(&self, mut buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        if let Some(n) = self.copy_drained(&mut buf) {
            return (Ok(n), buf);
        }
        let cell = match &self.session {
            // kTLS 移行済み: カーネルが復号するため生ソケット read でよい。
            None => return read_tcp(&self.inner, buf).await,
            Some(c) => c,
        };
        let fd = self.inner.as_raw_fd();
        loop {
            // rustls 内に滞留する平文を排出（借用は同期区間のみ）。
            {
                let mut conn = cell.borrow_mut();
                let mut d = self.drained.borrow_mut();
                drain_plaintext(&mut d, &mut conn.reader());
            }
            if let Some(n) = self.copy_drained(&mut buf) {
                return (Ok(n), buf);
            }

            // 暗号文を生ソケットから読み rustls へ供給する。
            let mut cipher = self.read_scratch.take();
            match raw_fd_read(fd, &mut cipher) {
                Ok(0) => {
                    self.read_scratch.replace(cipher);
                    return (Ok(0), buf); // EOF（close_notify なしも HTTP/1.1 では正常終了扱い）
                }
                Ok(n) => {
                    let res = {
                        let mut conn = cell.borrow_mut();
                        let mut consumed = 0;
                        let mut err = None;
                        while consumed < n {
                            match conn.read_tls(&mut &cipher[consumed..n]) {
                                Ok(0) => break,
                                Ok(r) => consumed += r,
                                Err(e) => {
                                    err = Some(e);
                                    break;
                                }
                            }
                            if let Err(e) = conn.process_new_packets() {
                                err = Some(io::Error::new(io::ErrorKind::InvalidData, e));
                                break;
                            }
                        }
                        err
                    };
                    self.read_scratch.replace(cipher);
                    if let Some(e) = res {
                        return (Err(e), buf);
                    }
                    // ループ先頭で平文を排出して返す。
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.read_scratch.replace(cipher);
                    if let Err(e) = self.inner.readable().await {
                        return (Err(e), buf);
                    }
                }
                Err(e) => {
                    self.read_scratch.replace(cipher);
                    return (Err(e), buf);
                }
            }
        }
    }

    /// 平文を全量書き込む（rustls で暗号化し TLS レコードを全て送出する）。
    async fn write_all(&self, data: Bytes) -> io::Result<()> {
        let cell = match &self.session {
            // kTLS 移行済み: カーネルが暗号化するため生ソケット write でよい。
            None => return write_all_tcp(&self.inner, data).await,
            Some(c) => c,
        };
        let mut off = 0;
        while off < data.len() {
            let n = {
                let mut conn = cell.borrow_mut();
                let mut w = conn.writer();
                std::io::Write::write(&mut w, &data[off..])?
            };
            off += n;
            // rustls 内部バッファ（既定 64KB）を溢れさせないよう都度フラッシュする。
            self.flush_tls(cell).await?;
            if n == 0 {
                // フラッシュ後も 1 バイトも受け付けない = 進捗なし。
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "rustls writer made no progress",
                ));
            }
        }
        self.flush_tls(cell).await
    }

    /// rustls が送出待ちの TLS レコードを全てソケットへ書き出す。
    async fn flush_tls(&self, cell: &RefCell<rustls::ClientConnection>) -> io::Result<()> {
        let fd = self.inner.as_raw_fd();
        loop {
            let mut out = self.write_scratch.take();
            out.clear();
            {
                let mut conn = cell.borrow_mut();
                if !conn.wants_write() {
                    self.write_scratch.replace(out);
                    return Ok(());
                }
                if let Err(e) = conn.write_tls(&mut out) {
                    self.write_scratch.replace(out);
                    return Err(e);
                }
            }
            let mut written = 0;
            while written < out.len() {
                match raw_fd_write(fd, &out[written..]) {
                    Ok(0) => {
                        self.write_scratch.replace(out);
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "backend TLS write returned 0",
                        ));
                    }
                    Ok(n) => written += n,
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        if let Err(e) = self.inner.writable().await {
                            self.write_scratch.replace(out);
                            return Err(e);
                        }
                    }
                    Err(e) => {
                        self.write_scratch.replace(out);
                        return Err(e);
                    }
                }
            }
            self.write_scratch.replace(out);
        }
    }
}

/// rustls セッションに滞留する復号済み平文を `dst` の未初期化スペアへ排出する。
fn drain_plaintext(dst: &mut Vec<u8>, rd: &mut dyn std::io::Read) {
    loop {
        dst.reserve(TLS_SCRATCH);
        let spare = dst.spare_capacity_mut();
        // SAFETY: read は書き込んだバイト数のみ返し、set_len はその分だけ伸ばす。
        let sbuf =
            unsafe { std::slice::from_raw_parts_mut(spare.as_mut_ptr() as *mut u8, spare.len()) };
        match rd.read(sbuf) {
            Ok(0) => break,
            Ok(n) => unsafe { dst.set_len(dst.len() + n) },
            Err(_) => break, // WouldBlock = 平文なし
        }
    }
}

/// `libc::read` ラッパー（ノンブロッキング fd 用）。
fn raw_fd_read(fd: crate::runtime::handle::RawFd, buf: &mut [u8]) -> io::Result<usize> {
    let n = unsafe { libc::read(fd as libc::c_int, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as _) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// `libc::write` ラッパー（ノンブロッキング fd 用）。
fn raw_fd_write(fd: crate::runtime::handle::RawFd, buf: &[u8]) -> io::Result<usize> {
    let n = unsafe { libc::write(fd as libc::c_int, buf.as_ptr() as *const libc::c_void, buf.len() as _) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// TLS ハンドシェイクを実行して [`BackendIo::Tls`] を構築する（F-44）。
///
/// kTLS ビルドでは `RustlsConnector`（設定に応じて kTLS 移行を試行）、非 kTLS ビルドでは
/// `SimpleTlsConnector` を使う。`insecure` はアップストリーム設定 `tls_insecure` に対応する。
async fn tls_connect(tcp: TcpStream, sni: &str, insecure: bool) -> io::Result<BackendIo> {
    #[cfg(veil_ktls)]
    {
        let connector = if insecure {
            crate::config::get_tls_connector_insecure()
        } else {
            crate::config::get_tls_connector()
        };
        let stream = connector.connect(tcp, sni).await?;
        let (inner, session, _mode, drained) = stream.into_parts();
        Ok(BackendIo::Tls(Box::new(TlsBackend::new(
            inner, session, drained,
        ))))
    }
    #[cfg(not(veil_ktls))]
    {
        let connector = if insecure {
            crate::config::get_tls_connector_insecure()
        } else {
            crate::config::get_tls_connector()
        };
        let stream = connector.connect(tcp, sni).await?;
        let (inner, session, drained) = stream.into_parts();
        Ok(BackendIo::Tls(Box::new(TlsBackend::new(
            inner,
            Some(session),
            drained,
        ))))
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
    /// TLS バックエンドか（F-44: `https://` アップストリーム）。
    pub use_tls: bool,
    /// TLS の SNI / 証明書検証に使うサーバ名（`sni_name` 設定またはホスト名）。
    pub sni: String,
    /// 証明書検証をスキップするか（アップストリーム設定 `tls_insecure`）。
    pub tls_insecure: bool,
}

/// バックエンドタスクを起動するスポーナ（F-46: 型付きタスクプール）。
///
/// リクエストごとに spawn される最ホットなタスクのため、`Box<dyn Future>` 確保を
/// 型付きプール（[`crate::runtime::TaskPool`]）で排除する。タスクの具象 Future 型
/// （`async fn` の匿名型）はモジュール外から命名できないため、プールをクロージャに
/// 閉じ込めて `Rc<dyn Fn>` として配布する（クロージャは HTTP/3 ワーカースレッドごとに
/// 1 回だけ作られ、spawn 呼び出しは動的ディスパッチ 1 回 + プールスロット再利用のみ）。
pub(crate) type BackendSpawner =
    Rc<dyn Fn(BackendTaskParams, Receiver<Bytes>, Sender<RespMsg>, H3Notify)>;

/// HTTP/3 ワーカースレッド用のバックエンドタスクスポーナを作成する。
pub(crate) fn backend_task_spawner() -> BackendSpawner {
    let pool = crate::runtime::TaskPool::new();
    Rc::new(move |params, req_body_rx, resp_tx, notify| {
        pool.spawn(backend_task(params, req_body_rx, resp_tx, notify));
    })
}

/// バックエンドストリーミングタスク本体。
///
/// メインループ（`process_h3_events`）から [`BackendSpawner`] 経由で起動され、当該リクエストの
/// バックエンド往復を独立タスクとして駆動する。タスクは `connections` を一切触らず、
/// チャネル経由でのみメインループと通信する（quiche の非 Send 制約を満たす）。
async fn backend_task(
    params: BackendTaskParams,
    req_body_rx: Receiver<Bytes>,
    resp_tx: Sender<RespMsg>,
    notify: H3Notify,
) {
    let server = params.server;
    server.acquire();
    let outcome = run_backend_task(
        &server,
        params.request_head,
        params.has_request_body,
        &params.compression,
        params.client_encoding,
        params.timeout_secs,
        params.use_tls,
        &params.sni,
        params.tls_insecure,
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
    use_tls: bool,
    sni: &str,
    tls_insecure: bool,
    req_body_rx: &Receiver<Bytes>,
    resp_tx: &Sender<RespMsg>,
    notify: &H3Notify,
) -> Result<(), u16> {
    let target = &server.target;
    let addr = crate::http_utils::HostPortStr::new(&target.host, target.port); // F-41
    let addr = addr.as_str();

    // --- 非同期接続（タイムアウト付き） ---
    let connect = TcpStream::connect_str(addr);
    let tcp = match crate::runtime::time::timeout(Duration::from_secs(timeout_secs), connect).await
    {
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
    let _ = tcp.set_nodelay(true);

    // --- F-44: TLS バックエンドはハンドシェイクして全二重 TLS ラッパーで包む ---
    let backend = if use_tls {
        let insecure = tls_insecure;
        match crate::runtime::time::timeout(
            Duration::from_secs(timeout_secs),
            tls_connect(tcp, sni, insecure),
        )
        .await
        {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => {
                warn!("[HTTP/3] streaming backend TLS handshake error: {}", e);
                return Err(502);
            }
            Err(_) => {
                warn!("[HTTP/3] streaming backend TLS handshake timeout");
                return Err(504);
            }
        }
    } else {
        BackendIo::Plain(tcp)
    };

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
            if let Err(e) = backend.write_all(Bytes::from(head)).await {
                warn!("[HTTP/3] streaming backend head write error: {}", e);
                return Err(502);
            }
            if let Err(e) = send_backend_chunk(&backend, first).await {
                warn!("[HTTP/3] streaming backend body write error: {}", e);
                return Err(502);
            }
            notify.notify();

            // B-12: 残りのリクエストボディ送信とレスポンス受信を**並行**に駆動する。
            //
            // 逐次（全ボディ送信 → レスポンス受信）だと、リクエスト完了前にレスポンスを
            // 返し始めるバックエンド（エコー・早期エラー応答等）で
            //   バックエンドの送信バッファ満杯 → バックエンドがリクエスト読み取り停止
            //   → 本タスクの write ブロック → req チャネル満杯 → QUIC フロー制御で
            //   クライアント送信停止
            // という双方向デッドロックに陥り、QUIC アイドルタイムアウトまで完全停止する
            // （成立はカーネルのソケットバッファ自動調整量に依存するため間欠的）。
            //
            // 両 Future は同一タスク内で &TcpStream を共有インターリーブする（L4 の
            // bidirectional_forward と同方式）。レスポンス完了時はアップロード側を
            // 打ち切ってよい（バックエンドが応答を完結させた = 残りボディは不要）。
            let upload = async {
                // クライアント側 END_STREAM（送信端 drop / 明示クローズ）まで逐次転送。
                while let Some(chunk) = req_body_rx.recv().await {
                    // 書き込み完了まで次フレームを読まない（バックプレッシャ）。
                    send_backend_chunk(&backend, chunk).await?;
                    notify.notify();
                }
                // 終端チャンク。
                backend.write_all(Bytes::from_static(b"0\r\n\r\n")).await?;
                Ok::<(), io::Error>(())
            };
            let respond = stream_response(
                &backend,
                compression,
                client_encoding,
                timeout_secs,
                resp_tx,
                notify,
            );

            let mut upload = std::pin::pin!(futures::FutureExt::fuse(upload));
            let mut respond = std::pin::pin!(futures::FutureExt::fuse(respond));
            loop {
                futures::select_biased! {
                    r = respond => return r,
                    u = upload => {
                        if let Err(e) = u {
                            // レスポンス完結後にバックエンドがリクエスト読み取りを
                            // 打ち切るのは合法（Connection: close 等）。ここでは中断せず
                            // レスポンス側の完了・エラー判定に委ねる。
                            debug!(
                                "[HTTP/3] streaming backend body write error: {} (response still in flight)",
                                e
                            );
                        }
                        // アップロード完了後はレスポンス側のみを待つ
                        //（fuse 済みのため以降 select から除外される）。
                    }
                }
            }
        }
        None => {
            // ボディなし（GET 等、または more_frames=true でも実データ無し） → 空行で head 終端。
            head.extend_from_slice(b"\r\n");
            if let Err(e) = backend.write_all(Bytes::from(head)).await {
                warn!("[HTTP/3] streaming backend head write error: {}", e);
                return Err(502);
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
    }
}

/// バックエンドレスポンスを head→body の順で受信し、メインループへ逐次転送する。
async fn stream_response(
    backend: &BackendIo,
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
        let (res, buf) = backend.read_into(read_buf).await;
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
        // B-11: 1xx 中間応答（101 以外）は読み捨てて最終応答を待つ（バッファ内に後続の
        // 最終応答ヘッドが既に届いている場合があるため、読み取りせずに再検査する）。
        if let Some(pos) = drain_interim_and_find_header_end(&mut head_buf) {
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
    backend: &BackendIo,
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
        let (res, buf) = backend.read_into(read_buf).await;
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
    backend: &BackendIo,
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
        let (res, buf) = backend.read_into(read_buf).await;
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
    backend: &BackendIo,
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
        let (res, buf) = backend.read_into(read_buf).await;
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
    backend: &BackendIo,
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
        let (res, buf) = backend.read_into(read_buf).await;
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
async fn read_tcp(backend: &TcpStream, mut buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
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
async fn write_all_tcp(backend: &TcpStream, mut buf: Bytes) -> io::Result<()> {
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
async fn send_backend_chunk(backend: &BackendIo, data: Bytes) -> io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    let mut header = Vec::with_capacity(18);
    push_chunk_size_line(&mut header, data.len());
    backend.write_all(Bytes::from(header)).await?;
    backend.write_all(data).await?;
    backend.write_all(Bytes::from_static(b"\r\n")).await?;
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

/// 先頭の 1xx 中間応答（101 以外）を読み捨てた上でヘッダ終端位置を返す（B-11）。
///
/// バックエンドが 100 Continue / 103 Early Hints 等の中間応答を最終応答より先に
/// 送ってきた場合、そのヘッドを drain して最終応答の解析に進む（1xx にボディはない）。
fn drain_interim_and_find_header_end(head_buf: &mut Vec<u8>) -> Option<usize> {
    loop {
        let pos = find_header_end(head_buf)?;
        let status = parse_status_code(&head_buf[..pos]).unwrap_or(502);
        if (100..=199).contains(&status) && status != 101 {
            head_buf.drain(..pos + 4);
            continue;
        }
        return Some(pos);
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

    // NOTE: channel / Notify の単体テストは抽出先の [`crate::stream_channel`] に移設した（F-116）。

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

    // B-11: 1xx 中間応答の読み捨て（ストリーミング経路）
    #[test]
    fn drain_interim_and_find_header_end_skips_100() {
        let mut buf =
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec();
        let pos = drain_interim_and_find_header_end(&mut buf).expect("final head");
        assert_eq!(parse_status_code(&buf[..pos]), Some(200));
    }

    #[test]
    fn drain_interim_and_find_header_end_waits_for_final() {
        // 中間応答のみ到着 → None（呼び出し側が次の read を待つ）。
        let mut buf = b"HTTP/1.1 100 Continue\r\n\r\n".to_vec();
        assert!(drain_interim_and_find_header_end(&mut buf).is_none());
        assert!(buf.is_empty());
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
