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

use crate::runtime::handle::AsRawFd;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
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
// F-151: バックエンド通知の per-connection 化
// ============================================================================

/// 接続 ID を持ち回るための共有ハンドル（F-151 レビュー修正）。
///
/// `quiche::ConnectionId<'static>` は内部が `Vec<u8>` のため、素の `clone()` は
/// 呼び出しのたびにヒープ確保（malloc）を伴う。ダーティキュー/タイマーヒープ/起床キューは
/// いずれも「同じ接続 ID を何度も複製して受け渡す」ホットパス（レスポンスボディの
/// チャンクごと・バックエンド通知のたびに発生し得る）であるため、`Rc` で包んで
/// `clone()` を参照カウント +1 のみ（malloc なし）に落とす。`HashMap`（`ConnectionMap`）の
/// キー自体は従来どおり素の `ConnectionId<'static>` のままでよく、ルックアップ時は
/// `&**key`（`Rc` → `ConnectionId` の deref）で行う。
pub(crate) type ConnKey = Rc<quiche::ConnectionId<'static>>;

/// メインループが drain する「起床した接続」の共有キュー。
///
/// バックエンドタスクが [`ConnWaker::notify`] を呼ぶ際に自分の接続 ID をここへ積む。
/// メインループは `select` から戻るたびにこのキューを drain し、積まれた接続だけを
/// ダーティ化する（従来は 1 本の `H3Notify` で起こすだけで「どの接続が進んだか」が
/// 分からず全接続を駆動していた）。`Rc<RefCell<..>>` は本クレートの `ConnectionMap` と
/// 同じ単一スレッド・ロックフリー方針。
///
/// F-161: 要素は `(ConnKey, Rc<Cell<bool>>)`。第 2 要素は当該接続の `ConnWaker::queued`
/// フラグ（全クローンで共有）で、drain 時に `false` へ戻すことでコアレッシングする
/// （詳細は [`ConnWaker`] のドキュメントを参照）。
pub(crate) type WakeQueue = Rc<RefCell<VecDeque<(ConnKey, Rc<Cell<bool>>)>>>;

/// バックエンドタスク → メインループの起床通知を per-connection 化したラッパー（F-151）。
///
/// 内部の `H3Notify` はそのままメインループの `select` 待機対象として使う（起床自体は
/// 引き続き 1 本の Notify で行う）。`notify()` を呼ぶ**前**に自 cid を [`WakeQueue`] へ
/// push することで、メインループがどの接続の何が進んだかを知り、全接続ではなく
/// 起こされた接続だけをダーティ化できるようにする。
///
/// F-161: レスポンスボディのチャンクごとに `notify()` が呼ばれるが、メインループが
/// drain するまでは 2 回目以降の push は完全な無駄（`mark_dirty` は `dirty` フラグにより
/// 2 回目以降は何もしない）。`queued`（接続ごとに共有する `Rc<Cell<bool>>`）で
/// 「既に `wake_queue` へ自 cid を積んである」ことを表し、積んである間は push を省く。
#[derive(Clone)]
pub(crate) struct ConnWaker {
    /// 自接続の ID（`ConnectionMap` のキーと同じ値を指す `Rc` ハンドル）。
    cid: ConnKey,
    /// メインループが drain する共有起床キュー。
    wake_queue: WakeQueue,
    /// メインループの select を起こす実体。
    notify: H3Notify,
    /// F-161: 既に `wake_queue` へ自 cid を積んであるか（接続内の全クローンで共有）。
    /// メインループが drain するときに false へ戻す。
    queued: Rc<Cell<bool>>,
}

impl ConnWaker {
    pub(crate) fn new(cid: ConnKey, wake_queue: WakeQueue, notify: H3Notify) -> Self {
        Self {
            cid,
            wake_queue,
            notify,
            queued: Rc::new(Cell::new(false)),
        }
    }

    /// メインループを起こす。呼び出し前に自 cid を起床キューへ積むため、メインループ側は
    /// 「どの接続が起こされたか」を取りこぼさずに把握できる。
    ///
    /// `self.cid.clone()`（`Rc::clone`）は参照カウント +1 のみで malloc を伴わない
    /// （`ConnKey` の意図どおり）。レスポンスボディのチャンクごとに呼ばれ得るホットパスの
    /// ため、ここで `ConnectionId` のディープコピーが発生しないことが重要。
    ///
    /// F-161: `queued` が既に `true` なら（前回の notify がまだ drain されていなければ）
    /// push を省く。`Cell::replace` で読み取りと設定を 1 操作にし、二重 push を防ぐ。
    /// `H3Notify::notify()` 自体は内部で bool コアレッシング済みのため常に呼んでよい
    /// （安価）。
    pub(crate) fn notify(&self) {
        if !self.queued.replace(true) {
            self.wake_queue
                .borrow_mut()
                .push_back((self.cid.clone(), self.queued.clone()));
        }
        self.notify.notify();
    }
}

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
    /// 基盤ソケットの fd（B-93 の生存確認に使う）。
    fn raw_fd(&self) -> crate::runtime::handle::RawFd {
        match self {
            BackendIo::Plain(s) => s.as_raw_fd(),
            BackendIo::Tls(t) => t.inner.as_raw_fd(),
        }
    }

    /// B-104: プールへ返してよい状態か（TLS は復号済みで未消費の平文が残っていないこと）。
    fn is_clean(&self) -> bool {
        match self {
            BackendIo::Plain(_) => true,
            BackendIo::Tls(t) => t.drained.borrow().is_empty(),
        }
    }

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
                            // 復号済み平文を都度退避する。rustls は受信平文が 16KB
                            // （DEFAULT_RECEIVED_PLAINTEXT_LIMIT）を超えると次の read_tls を
                            // "received plaintext buffer full" で拒否するため、1 回の生 read
                            // （最大 16KB の暗号文 = 複数レコード）をまとめて投入すると
                            // 平文を排出しないまま上限に達してエラーになる
                            // （macOS 実機の E2E で 1.2MB のアップロード折り返しが途中で切れた）。
                            let mut d = self.drained.borrow_mut();
                            drain_plaintext(&mut d, &mut conn.reader());
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
    let n = unsafe {
        libc::read(
            fd as libc::c_int,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len() as _,
        )
    };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// `libc::write` ラッパー（ノンブロッキング fd 用）。
fn raw_fd_write(fd: crate::runtime::handle::RawFd, buf: &[u8]) -> io::Result<usize> {
    let n = unsafe {
        libc::write(
            fd as libc::c_int,
            buf.as_ptr() as *const libc::c_void,
            buf.len() as _,
        )
    };
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
    let connector = if insecure {
        crate::config::get_tls_connector_insecure()
    } else {
        crate::config::get_tls_connector()
    };
    let stream = connector.connect(tcp, sni).await?;
    Ok(backend_io_from_client_tls(stream))
}

/// ハンドシェイク済みのクライアント TLS ストリームを全二重ラッパーにする。
fn backend_io_from_client_tls(stream: crate::pool::ClientTls) -> BackendIo {
    #[cfg(veil_ktls)]
    {
        let (inner, session, _mode, drained) = stream.into_parts();
        BackendIo::Tls(Box::new(TlsBackend::new(inner, session, drained)))
    }
    #[cfg(not(veil_ktls))]
    {
        let (inner, session, drained) = stream.into_parts();
        BackendIo::Tls(Box::new(TlsBackend::new(inner, Some(session), drained)))
    }
}

/// F-175: ALPN で HTTP/1.1 を選んだ HTTPS 上流の接続（ネゴシエーション済み）をワーカーの
/// 上流プールへ入れる（直後の HTTP/1.1 の要求で使う）。
#[cfg(feature = "http2")]
pub(crate) fn h3_pool_put_client_tls(
    key: &str,
    stream: crate::pool::ClientTls,
    max_idle: usize,
    idle_timeout_secs: u64,
) {
    h3_pool_put(
        key,
        backend_io_from_client_tls(stream),
        max_idle.max(1),
        idle_timeout_secs.max(1),
    );
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
    /// trailers（gRPC の grpc-status 等）。送出と同時にストリームを fin で閉じる（B-97）。
    #[cfg(feature = "grpc")]
    Trailers(RespHeaders),
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
    /// B-104: 上流接続プールのホストあたり最大アイドル接続数（`[security] max_idle_connections_per_host`）。
    pub pool_max_idle: usize,
    /// B-104: プールのアイドルタイムアウト秒（`[security] idle_connection_timeout_secs`）。
    pub pool_idle_timeout_secs: u64,
    /// HEAD 要求か（応答に Content-Length があっても本文を読まない）。
    pub no_response_body: bool,
    /// F-177: 再利用した接続が応答前に失敗したとき、本文の無い要求を 1 回だけ再送するか。
    pub retry_idempotent: bool,
    /// F-171: 上流が HTTP/2（h2c）なら `Some`。このとき `request_head` は使わない。
    #[cfg(feature = "http2")]
    pub h2: Option<H2Upstream>,
}

/// F-171: HTTP/2 上流へ送る要求（HTTP/1.1 の `request_head` の代わり）。
#[cfg(feature = "http2")]
pub(crate) struct H2Upstream {
    pub method: Bytes,
    /// 上流へ送る `:path`（gRPC はフルパスのまま）。
    pub path: Bytes,
    pub authority: Bytes,
    /// 転送する要求ヘッダ（疑似ヘッダを除く。ホップバイホップは上流クライアントが落とす）。
    pub headers: RespHeaders,
    /// 上流への接続タイムアウト。
    pub connect_timeout: Duration,
}

/// バックエンドタスクを起動するスポーナ（F-46: 型付きタスクプール）。
///
/// リクエストごとに spawn される最ホットなタスクのため、`Box<dyn Future>` 確保を
/// 型付きプール（[`crate::runtime::TaskPool`]）で排除する。タスクの具象 Future 型
/// （`async fn` の匿名型）はモジュール外から命名できないため、プールをクロージャに
/// 閉じ込めて `Rc<dyn Fn>` として配布する（クロージャは HTTP/3 ワーカースレッドごとに
/// 1 回だけ作られ、spawn 呼び出しは動的ディスパッチ 1 回 + プールスロット再利用のみ）。
pub(crate) type BackendSpawner =
    Rc<dyn Fn(BackendTaskParams, Receiver<Bytes>, Sender<RespMsg>, ConnWaker)>;

/// HTTP/3 ワーカースレッド用のバックエンドタスクスポーナを作成する。
pub(crate) fn backend_task_spawner() -> BackendSpawner {
    let pool = crate::runtime::TaskPool::new();
    Rc::new(move |params, req_body_rx, resp_tx, notify| {
        pool.spawn(backend_task(params, req_body_rx, resp_tx, notify));
    })
}

// ============================================================================
// B-104: HTTP/3 ワーカーの上流接続プール
// ============================================================================

/// プールに置いた上流接続 1 本。
struct H3PooledBackend {
    io: BackendIo,
    /// プールへ返した時刻（アイドル時間の起点。B-93 の生存確認の閾値判定に使う）。
    returned_at: std::time::Instant,
    idle_timeout_secs: u64,
}

thread_local! {
    /// B-104: HTTP/3 ワーカースレッドの上流接続プール。キーは `PoolKeyStr`
    /// （平文 = 接続先、TLS = 接続先 + SNI + 検証有無）。
    ///
    /// HTTP/1.1・HTTP/2 のワーカーは `HTTP_POOL` / `HTTPS_POOL` を使うが、HTTP/3 の
    /// ストリーミング経路は全二重の TLS ラッパー（[`TlsBackend`]）を使うため型が異なり、
    /// HTTP/3 ワーカーは別スレッドでもあるので、専用のプールを持つ。
    static H3_BACKEND_POOL: RefCell<HashMap<String, VecDeque<H3PooledBackend>>> =
        RefCell::new(HashMap::new());
}

/// プールから再利用可能な接続を取り出す（B-93 の生存確認込み）。
fn h3_pool_get(key: &str) -> Option<BackendIo> {
    H3_BACKEND_POOL.with(|p| {
        let mut pool = p.borrow_mut();
        let queue = pool.get_mut(key)?;
        while let Some(entry) = queue.pop_front() {
            // 平文は未読データ（前応答の残骸）があれば捨てる。TLS は NewSessionTicket 等の
            // TLS レコードが残り得るので未読データを理由に捨てない（HTTPS_POOL と同じ規則）。
            let reject_unread = matches!(entry.io, BackendIo::Plain(_));
            if crate::pool::pooled_conn_reusable(
                entry.returned_at,
                entry.idle_timeout_secs,
                entry.io.raw_fd(),
                reject_unread,
            ) {
                crate::metrics::record_connection_pool_hit(key);
                return Some(entry.io);
            }
        }
        crate::metrics::record_connection_pool_miss(key);
        None
    })
}

/// 接続をプールへ返す。既存キーへの返却は確保なし（新規キーのときだけ `to_string()`）。
fn h3_pool_put(key: &str, io: BackendIo, max_idle: usize, idle_timeout_secs: u64) {
    if max_idle == 0 || idle_timeout_secs == 0 {
        return;
    }
    H3_BACKEND_POOL.with(|p| {
        let mut pool = p.borrow_mut();
        let entry = H3PooledBackend {
            io,
            returned_at: std::time::Instant::now(),
            idle_timeout_secs,
        };
        if let Some(queue) = pool.get_mut(key) {
            while queue.len() >= max_idle {
                queue.pop_front();
            }
            queue.push_back(entry);
            crate::metrics::set_connection_pool_size(key, queue.len());
            return;
        }
        let mut queue = VecDeque::new();
        queue.push_back(entry);
        crate::metrics::set_connection_pool_size(key, queue.len());
        pool.insert(key.to_string(), queue);
    });
}

/// 応答を読み終えたときの上流接続の扱い（B-104）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RespEnd {
    /// 応答の終端をちょうど読み切った（次の要求に使える）。
    Reusable,
    /// EOF 終端・余剰データ・クライアント切断などで、接続を再利用できない。
    Close,
}

/// バックエンドストリーミングタスク本体。
///
/// メインループ（`process_h3_events`）から [`BackendSpawner`] 経由で起動され、当該リクエストの
/// バックエンド往復を独立タスクとして駆動する。タスクは `connections` を一切触らず、
/// チャネル経由でのみメインループと通信する（quiche の非 Send 制約を満たす）。
async fn backend_task(
    mut params: BackendTaskParams,
    req_body_rx: Receiver<Bytes>,
    resp_tx: Sender<RespMsg>,
    notify: ConnWaker,
) {
    params.server.acquire();
    #[cfg(feature = "http2")]
    let outcome = if params.h2.is_some() {
        if params.use_tls {
            // F-175: HTTPS 上流は ALPN で HTTP/2 を試し、HTTP/1.1 なら従来の経路へ。
            match open_h2_tls(&params).await {
                Ok(Some(stream)) => {
                    relay_h2_stream(&params, stream, &req_body_rx, &resp_tx, &notify).await
                }
                Ok(None) => {
                    let mut head = ReqHead::Raw(std::mem::take(&mut params.request_head));
                    run_backend_task(&params, &mut head, &req_body_rx, &resp_tx, &notify).await
                }
                Err(status) => Err(status),
            }
        } else {
            run_h2_task(&params, &req_body_rx, &resp_tx, &notify).await
        }
    } else {
        let mut head = ReqHead::Raw(std::mem::take(&mut params.request_head));
        run_backend_task(&params, &mut head, &req_body_rx, &resp_tx, &notify).await
    };
    #[cfg(not(feature = "http2"))]
    let outcome = {
        let mut head = ReqHead::Raw(std::mem::take(&mut params.request_head));
        run_backend_task(&params, &mut head, &req_body_rx, &resp_tx, &notify).await
    };
    params.server.release();

    if let Err(status) = outcome {
        // head 送出前のエラーはそのステータスで応答し、送出後のエラーはメインループが
        // ストリームをリセットする（切り詰めた本文を fin で閉じて成功に見せない）。
        let _ = resp_tx.send(RespMsg::Error { status }).await;
    }
    // resp_tx / req_body_rx はここで drop → メインループへ fin（EOF）伝播。
    notify.notify();
}

/// F-171: HTTP/2 上流（多重化接続のストリーム）との全二重中継。
///
/// 下流（HTTP/3）の要求本文は届いた順に上流ストリームへ流し、上流の応答
/// （head・DATA・トレーラー）は届いた順にメインループへ流す。両方向を同じタスクで並行に
/// 進めるので、クライアントストリーミング・双方向ストリーミングの gRPC が成立する
/// （応答の送出は要求の完了を待たない）。圧縮対象の応答（非 gRPC）だけは本文を集めてから
/// 圧縮する（HTTP/1.1 経路と同じ）。
#[cfg(feature = "http2")]
async fn run_h2_task(
    params: &BackendTaskParams,
    req_body_rx: &Receiver<Bytes>,
    resp_tx: &Sender<RespMsg>,
    notify: &ConnWaker,
) -> Result<(), u16> {
    use crate::http2::upstream_mux::open_h2c;
    let h2 = params.h2.as_ref().ok_or(502u16)?;
    let target = &params.server.target;
    let addr = target.conn_addr();
    let key = crate::http_utils::PoolKeyStr::plain_addr(addr.as_str());
    let stream = open_h2c(
        target,
        key.as_str(),
        h2.connect_timeout,
        Duration::from_secs(params.pool_idle_timeout_secs),
        &h2.method,
        &h2.path,
        &h2.authority,
        h2.headers.iter().map(|(n, v)| (n.as_ref(), v.as_ref())),
        !params.has_request_body,
    )
    .await?;
    relay_h2_stream(params, stream, req_body_rx, resp_tx, notify).await
}

/// F-175: HTTPS 上流で HTTP/2 のストリームを開く。上流が HTTP/1.1 を選んだら `Ok(None)`
/// （ネゴシエーションに使った接続はワーカーのプールへ入れ、HTTP/1.1 経路で使う）。
#[cfg(feature = "http2")]
async fn open_h2_tls(
    params: &BackendTaskParams,
) -> Result<Option<crate::http2::upstream_mux::UpStream>, u16> {
    use crate::http2::upstream_mux::{open_https, HttpsOpen};
    let h2 = params.h2.as_ref().ok_or(502u16)?;
    let target = &params.server.target;
    let addr = target.conn_addr();
    let key =
        crate::http_utils::PoolKeyStr::tls_addr(addr.as_str(), &params.sni, params.tls_insecure);
    match open_https(
        target,
        key.as_str(),
        params.tls_insecure,
        h2.connect_timeout,
        Duration::from_secs(params.pool_idle_timeout_secs),
        &h2.method,
        &h2.path,
        &h2.authority,
        h2.headers.iter().map(|(n, v)| (n.as_ref(), v.as_ref())),
        !params.has_request_body,
    )
    .await?
    {
        HttpsOpen::H2(s) => Ok(Some(s)),
        HttpsOpen::Http1(conn) => {
            if let Some(conn) = conn {
                h3_pool_put_client_tls(
                    key.as_str(),
                    *conn,
                    params.pool_max_idle,
                    params.pool_idle_timeout_secs,
                );
            }
            Ok(None)
        }
    }
}

/// F-171 / F-175: 開いた上流 HTTP/2 ストリームと下流（HTTP/3）を全二重に中継する。
#[cfg(feature = "http2")]
async fn relay_h2_stream(
    params: &BackendTaskParams,
    mut stream: crate::http2::upstream_mux::UpStream,
    req_body_rx: &Receiver<Bytes>,
    resp_tx: &Sender<RespMsg>,
    notify: &ConnWaker,
) -> Result<(), u16> {
    use crate::http2::upstream_mux::UpResp;
    let read_timeout = Duration::from_secs(params.timeout_secs);

    // 要求方向: 送信端を所有して流し、終わったら drop（= END_STREAM）。
    let req_tx = stream.req_tx.take();
    let upload = async move {
        let Some(tx) = req_tx else {
            return;
        };
        while let Some(chunk) = req_body_rx.recv().await {
            if tx.send(chunk).await.is_err() {
                // 上流がストリームを閉じた（早期応答・リセット）。残りは応答側で判定する。
                return;
            }
            // 要求チャネルに空きができた → メインループの recv_body を再開させる。
            notify.notify();
        }
    };

    // 応答方向。
    let resp_rx = &stream.resp_rx;
    let download = async move {
        let mut head_sent = false;
        loop {
            let msg = match crate::runtime::time::timeout(read_timeout, resp_rx.recv()).await {
                Ok(m) => m,
                Err(_) => {
                    warn!("[HTTP/3] h2 upstream response timeout");
                    return Err(if head_sent { 502 } else { 504 });
                }
            };
            match msg {
                None => return Ok(()),
                Some(UpResp::Head { status, headers }) => {
                    let content_type = headers
                        .iter()
                        .find(|(n, _)| n.as_ref() == b"content-type")
                        .map(|(_, v)| v.clone());
                    let is_grpc = content_type
                        .as_deref()
                        .is_some_and(|ct| ct.starts_with(b"application/grpc"));
                    let should_compress = if is_grpc || params.no_response_body {
                        None
                    } else {
                        let content_length = headers
                            .iter()
                            .find(|(n, _)| n.as_ref() == b"content-length")
                            .and_then(|(_, v)| std::str::from_utf8(v).ok())
                            .and_then(|v| v.trim().parse().ok());
                        let content_encoding = headers
                            .iter()
                            .find(|(n, _)| n.as_ref() == b"content-encoding")
                            .map(|(_, v)| v.as_ref());
                        params.compression.should_compress(
                            params.client_encoding,
                            content_type.as_deref(),
                            content_length,
                            content_encoding,
                        )
                    };
                    if let Some(enc) = should_compress {
                        return h2_relay_compressed(
                            resp_rx,
                            status,
                            headers,
                            enc,
                            &params.compression,
                            read_timeout,
                            resp_tx,
                        )
                        .await;
                    }
                    if resp_tx
                        .send(RespMsg::Head { status, headers })
                        .await
                        .is_err()
                    {
                        return Ok(()); // クライアント切断（stream の drop で上流へ CANCEL）。
                    }
                    head_sent = true;
                    if resp_rx.is_finished() {
                        // HEADERS で終わった応答（gRPC の trailers-only 等）。すぐ送信端を閉じて
                        // メインループに HEADERS へ fin を載せさせる。
                        return Ok(());
                    }
                }
                Some(UpResp::Data(b)) => {
                    if !head_sent {
                        return Err(502);
                    }
                    if resp_tx.send(RespMsg::Body(b)).await.is_err() {
                        return Ok(());
                    }
                }
                Some(UpResp::Trailers(trailers)) => {
                    #[cfg(feature = "grpc")]
                    {
                        if resp_tx.send(RespMsg::Trailers(trailers)).await.is_err() {
                            return Ok(());
                        }
                    }
                    #[cfg(not(feature = "grpc"))]
                    drop(trailers);
                }
                Some(UpResp::Reset { .. }) => {
                    debug!("[HTTP/3] h2 upstream stream reset");
                    return Err(502);
                }
            }
            notify.notify();
        }
    };

    let mut upload = std::pin::pin!(futures::FutureExt::fuse(upload));
    let mut download = std::pin::pin!(futures::FutureExt::fuse(download));
    loop {
        futures::select_biased! {
            r = download => return r,
            _ = upload => {}
        }
    }
}

/// F-171: 圧縮対象の h2 応答を最後まで集めて圧縮し、head + 本文として送る。
#[cfg(feature = "http2")]
async fn h2_relay_compressed(
    resp_rx: &Receiver<crate::http2::upstream_mux::UpResp>,
    status: u16,
    mut headers: RespHeaders,
    enc: AcceptedEncoding,
    compression: &CompressionConfig,
    read_timeout: Duration,
    resp_tx: &Sender<RespMsg>,
) -> Result<(), u16> {
    use crate::http2::upstream_mux::UpResp;
    let mut body: Vec<u8> = Vec::new();
    loop {
        match crate::runtime::time::timeout(read_timeout, resp_rx.recv()).await {
            Err(_) => return Err(504),
            Ok(None) => break,
            Ok(Some(UpResp::Data(b))) => body.extend_from_slice(&b),
            Ok(Some(UpResp::Trailers(_))) | Ok(Some(UpResp::Head { .. })) => {}
            Ok(Some(UpResp::Reset { .. })) => return Err(502),
        }
    }
    let compressed = crate::http3_server::compress_body_h3(&body, enc, compression);
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
        .is_ok()
    {
        let _ = resp_tx.send(RespMsg::Body(Bytes::from(compressed))).await;
    }
    Ok(())
}

/// 新規に上流へ接続する（TLS ならハンドシェイクまで）。
async fn connect_backend(params: &BackendTaskParams, addr: &str) -> Result<BackendIo, u16> {
    connect_backend_io(
        addr,
        params.timeout_secs,
        params.use_tls,
        &params.sni,
        params.tls_insecure,
    )
    .await
}

/// 新規に上流へ接続する（ストリーミング経路・バッファ経路で共用）。
async fn connect_backend_io(
    addr: &str,
    timeout_secs: u64,
    use_tls: bool,
    sni: &str,
    tls_insecure: bool,
) -> Result<BackendIo, u16> {
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

    // F-44: TLS バックエンドはハンドシェイクして全二重 TLS ラッパーで包む。
    if !use_tls {
        return Ok(BackendIo::Plain(tcp));
    }
    match crate::runtime::time::timeout(
        Duration::from_secs(timeout_secs),
        tls_connect(tcp, sni, tls_insecure),
    )
    .await
    {
        Ok(Ok(b)) => Ok(b),
        Ok(Err(e)) => {
            warn!("[HTTP/3] streaming backend TLS handshake error: {}", e);
            Err(502)
        }
        Err(_) => {
            warn!("[HTTP/3] streaming backend TLS handshake timeout");
            Err(504)
        }
    }
}

/// リクエスト head の状態（F-177 の再送で head を作り直さずに使い回すため）。
enum ReqHead {
    /// 末尾空行・ボディフレーミング未付与の head（初回）。
    Raw(Vec<u8>),
    /// 本文なしで送った完成済み head（再送時は参照カウントの複製だけで再送できる）。
    NoBody(Bytes),
}

/// バックエンド往復本体。`Err(status)` は **head 未送出時のみ** のエラー（指定ステータスを返す）。
///
/// B-104: 上流接続はワーカーのプールから取り出し、応答の終端をちょうど読み切れたときだけ返す。
/// F-177: プールから取り出した接続が応答の 1 バイト目より前に失敗し、要求に本文が無い
/// （＝再送しても副作用が増えない形で要求を作り直せる）場合に限り、新規接続で 1 回だけ再送する。
async fn run_backend_task(
    params: &BackendTaskParams,
    head: &mut ReqHead,
    req_body_rx: &Receiver<Bytes>,
    resp_tx: &Sender<RespMsg>,
    notify: &ConnWaker,
) -> Result<(), u16> {
    let target = &params.server.target;
    let addr = target.conn_addr(); // F-41/F-170: スタック上に構築（UDS 対応、TCP は不変）
    let addr = addr.as_str();
    let pool_key = if params.use_tls {
        crate::http_utils::PoolKeyStr::tls_addr(addr, &params.sni, params.tls_insecure)
    } else {
        crate::http_utils::PoolKeyStr::plain_addr(addr)
    };

    let mut pooled = h3_pool_get(pool_key.as_str());
    loop {
        let reused = pooled.is_some();
        let backend = match pooled.take() {
            Some(b) => b,
            None => connect_backend(params, addr).await?,
        };
        let before_head = Cell::new(true);
        let result = exchange(
            &backend,
            params,
            head,
            req_body_rx,
            resp_tx,
            notify,
            &before_head,
        )
        .await;
        match result {
            Ok(RespEnd::Reusable) if backend.is_clean() => {
                h3_pool_put(
                    pool_key.as_str(),
                    backend,
                    params.pool_max_idle,
                    params.pool_idle_timeout_secs,
                );
                return Ok(());
            }
            Ok(_) => return Ok(()),
            Err(status)
                if reused
                    && before_head.get()
                    && !params.has_request_body
                    && params.retry_idempotent =>
            {
                // F-177: 再利用した接続が応答前に失敗 → 新規接続で 1 回だけ再送。
                debug!(
                    "[HTTP/3] pooled backend connection failed before the response ({}); retrying on a fresh connection",
                    status
                );
                continue;
            }
            Err(status) => return Err(status),
        }
    }
}

/// 1 回の要求・応答のやり取り。
#[allow(clippy::too_many_arguments)]
async fn exchange(
    backend: &BackendIo,
    params: &BackendTaskParams,
    head: &mut ReqHead,
    req_body_rx: &Receiver<Bytes>,
    resp_tx: &Sender<RespMsg>,
    notify: &ConnWaker,
    before_head: &Cell<bool>,
) -> Result<RespEnd, u16> {
    // --- リクエスト head + ボディフレーミングの確定 ---
    // `request_head` は末尾空行なし・ボディフレーミングなし（B-104 で `Connection: close` は外した）。
    // ボディ有無は HEADERS 受信時点では確定しない（h3 クライアントは HEADERS と fin を別送する
    // ため、ボディのない GET でも `more_frames=true`）。そこで **最初のボディ断片が実際に届くか**
    // を見てから framing を確定する: 届けば `Transfer-Encoding: chunked`、届かなければボディなし。
    let first_chunk = if params.has_request_body {
        req_body_rx.recv().await
    } else {
        None
    };

    let respond = |backend| {
        stream_response(
            backend,
            &params.compression,
            params.client_encoding,
            params.timeout_secs,
            params.no_response_body,
            resp_tx,
            notify,
            before_head,
        )
    };

    match first_chunk {
        Some(first) => {
            // 実ボディあり → chunked 逐次転送（本文ありの要求は再送しないので head は使い切る）。
            let mut raw = match std::mem::replace(head, ReqHead::NoBody(Bytes::new())) {
                ReqHead::Raw(v) => v,
                ReqHead::NoBody(b) => b[..b.len().saturating_sub(2)].to_vec(),
            };
            raw.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
            if let Err(e) = backend.write_all(Bytes::from(raw)).await {
                warn!("[HTTP/3] streaming backend head write error: {}", e);
                return Err(502);
            }
            if let Err(e) = send_backend_chunk(backend, first).await {
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
                    send_backend_chunk(backend, chunk).await?;
                    notify.notify();
                }
                // 終端チャンク。
                backend.write_all(Bytes::from_static(b"0\r\n\r\n")).await?;
                Ok::<(), io::Error>(())
            };
            let respond = respond(backend);

            let mut upload_done = false;
            let mut upload = std::pin::pin!(futures::FutureExt::fuse(upload));
            let mut respond = std::pin::pin!(futures::FutureExt::fuse(respond));
            loop {
                futures::select_biased! {
                    r = respond => {
                        // B-104: 本文を送り切る前に応答が完了した（早期応答）接続は、上流が
                        // 残りの本文をどう扱うか分からないので再利用しない。
                        return r.map(|end| if upload_done { end } else { RespEnd::Close });
                    }
                    u = upload => {
                        match u {
                            Ok(()) => upload_done = true,
                            Err(e) => {
                                // レスポンス完結後にバックエンドがリクエスト読み取りを
                                // 打ち切るのは合法。ここでは中断せずレスポンス側の完了・
                                // エラー判定に委ねる（upload_done は false のまま＝再利用しない）。
                                debug!(
                                    "[HTTP/3] streaming backend body write error: {} (response still in flight)",
                                    e
                                );
                            }
                        }
                        // アップロード完了後はレスポンス側のみを待つ
                        //（fuse 済みのため以降 select から除外される）。
                    }
                }
            }
        }
        None => {
            // ボディなし（GET 等、または more_frames=true でも実データ無し） → 空行で head 終端。
            let bytes = match std::mem::replace(head, ReqHead::NoBody(Bytes::new())) {
                ReqHead::Raw(mut v) => {
                    v.extend_from_slice(b"\r\n");
                    Bytes::from(v)
                }
                ReqHead::NoBody(b) => b,
            };
            *head = ReqHead::NoBody(bytes.clone());
            if let Err(e) = backend.write_all(bytes).await {
                warn!("[HTTP/3] streaming backend head write error: {}", e);
                return Err(502);
            }
            // --- レスポンス受信（head → body 逐次） ---
            respond(backend).await
        }
    }
}

/// バックエンドレスポンスを head→body の順で受信し、メインループへ逐次転送する。
///
/// `before_head` は、最終応答のヘッダをメインループへ送る直前まで `true`（F-177 の再送判定に使う）。
#[allow(clippy::too_many_arguments)]
async fn stream_response(
    backend: &BackendIo,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    timeout_secs: u64,
    no_response_body: bool,
    resp_tx: &Sender<RespMsg>,
    notify: &ConnWaker,
    before_head: &Cell<bool>,
) -> Result<RespEnd, u16> {
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
    let mut parsed = parse_response_headers(&head_buf[..header_end]);
    // ヘッダ終端（\r\n\r\n）以降は先読みしたボディ断片。
    let leftover = Bytes::copy_from_slice(&head_buf[header_end + 4..]);

    // B-104: 本文を持たない応答（HEAD への応答・204・304）は Content-Length があっても本文を
    // 読まない（キープアライブでは上流が閉じないため、読みに行くとタイムアウトまで止まる）。
    if no_response_body || status == 204 || status == 304 {
        parsed.framing = Framing::Length(0);
    }
    // 101（プロトコル切り替え）は HTTP/3 で中継できず、接続も HTTP/1.1 ではなくなる。
    let reusable_conn = parsed.keep_alive && status != 101;

    // 圧縮判定（content-type / 既存エンコーディング / 既知長）。本文の無い応答は圧縮しない。
    let should_compress = if matches!(parsed.framing, Framing::Length(0)) {
        None
    } else {
        compression.should_compress(
            client_encoding,
            parsed.content_type.as_deref(),
            parsed.content_length,
            parsed.content_encoding.as_deref(),
        )
    };

    let end = if let Some(enc) = should_compress {
        // 圧縮はボディ全体が必要 → バッファ経路（HTTP/2 第1フェーズと同方針）。
        before_head.set(false);
        stream_response_compressed(
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
        .await?
    } else {
        // --- head 送出（非圧縮ストリーミング） ---
        let framing = parsed.framing;
        let mut headers = parsed.headers;
        // 長さ既知ならそのまま転送（クライアントへ content-length 提示）。chunked/EOF は length 削除。
        if parsed.is_chunked {
            // chunked のデータ長は不定 → content-length は付けない（quiche がストリーム長を管理）。
            headers.retain(|(n, _)| !n.eq_ignore_ascii_case(b"content-length"));
        }
        before_head.set(false);
        if resp_tx
            .send(RespMsg::Head { status, headers })
            .await
            .is_err()
        {
            return Ok(RespEnd::Close); // クライアント切断。
        }
        notify.notify();

        // --- body 逐次転送 ---
        match framing {
            Framing::Length(total) => {
                stream_body_length(
                    backend, leftover, read_buf, total, deadline, resp_tx, notify,
                )
                .await?
            }
            Framing::Chunked => {
                stream_body_chunked(backend, leftover, read_buf, deadline, resp_tx, notify).await?
            }
            Framing::Eof => {
                stream_body_eof(backend, leftover, read_buf, deadline, resp_tx, notify).await?
            }
        }
    };
    Ok(if reusable_conn { end } else { RespEnd::Close })
}

/// 非圧縮・content-length 既知（または不明だが length フレーミング）のボディ転送。
async fn stream_body_length(
    backend: &BackendIo,
    leftover: Bytes,
    mut read_buf: Vec<u8>,
    total: usize,
    deadline: std::time::Instant,
    resp_tx: &Sender<RespMsg>,
    notify: &ConnWaker,
) -> Result<RespEnd, u16> {
    let mut sent = 0usize;
    // B-104: 宣言長を超えて届いたバイト（上流の誤り）があれば接続を再利用しない。
    let mut overrun = leftover.len() > total;
    if !leftover.is_empty() && total > 0 {
        let take = leftover.len().min(total);
        if send_body_bytes(resp_tx, notify, leftover.slice(0..take))
            .await
            .is_err()
        {
            return Ok(RespEnd::Close);
        }
        sent += take;
    }
    while sent < total {
        if std::time::Instant::now() >= deadline {
            // head 送出済み: 短い本文を正常終了（fin）で閉じるとクライアントは切り詰められた
            // 応答を成功と誤認する。Err を返してストリームをリセットさせる。
            return Err(504);
        }
        let (res, buf) = backend.read_into(read_buf).await;
        read_buf = buf;
        match res {
            Ok(0) => {
                warn!(
                    "[HTTP/3] streaming backend closed mid-body ({} of {} bytes)",
                    sent, total
                );
                return Err(502);
            }
            Ok(n) => {
                let take = n.min(total - sent);
                overrun |= n > take;
                let chunk = bytes_from_read(&read_buf, take);
                if send_body_bytes(resp_tx, notify, chunk).await.is_err() {
                    return Ok(RespEnd::Close);
                }
                sent += take;
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => {
                warn!("[HTTP/3] streaming backend read error mid-body: {}", e);
                return Err(502);
            }
        }
    }
    Ok(if overrun {
        RespEnd::Close
    } else {
        RespEnd::Reusable
    })
}

/// `drain_chunked` の結果。
enum ChunkedProgress {
    /// 終端に達していない（次の read を待つ）。
    More,
    /// 終端に達した。`exact` は入力を終端ちょうどまで消費したか（余剰があれば `false`）。
    Done { exact: bool },
    /// クライアントが切断した。
    ClientGone,
}

/// 非圧縮・chunked のボディ転送（`ChunkedDecoder::next_data_span` でゼロコピーデコード）。
async fn stream_body_chunked(
    backend: &BackendIo,
    leftover: Bytes,
    mut read_buf: Vec<u8>,
    deadline: std::time::Instant,
    resp_tx: &Sender<RespMsg>,
    notify: &ConnWaker,
) -> Result<RespEnd, u16> {
    use crate::http_utils::ChunkedDecoder;
    let mut decoder = ChunkedDecoder::new_unlimited();

    // 先読み分を先にデコード。
    if !leftover.is_empty() {
        match drain_chunked(&mut decoder, &leftover, resp_tx, notify).await {
            ChunkedProgress::More => {}
            ChunkedProgress::Done { exact } => return Ok(reuse_if(exact)),
            ChunkedProgress::ClientGone => return Ok(RespEnd::Close),
        }
    }

    loop {
        if std::time::Instant::now() >= deadline {
            return Err(504); // head 送出済み → リセット（切り詰めを成功に見せない）
        }
        let (res, buf) = backend.read_into(read_buf).await;
        read_buf = buf;
        match res {
            Ok(0) => {
                // 終端チャンク前の EOF = 本文の欠落。
                warn!("[HTTP/3] streaming backend closed before the last chunk");
                return Err(502);
            }
            Ok(n) => {
                // read_buf の先頭 n バイトを Bytes 化してデコード（span はこの Bytes のスライス）。
                let data = bytes_from_read(&read_buf, n);
                match drain_chunked(&mut decoder, &data, resp_tx, notify).await {
                    ChunkedProgress::More => {}
                    ChunkedProgress::Done { exact } => return Ok(reuse_if(exact)),
                    ChunkedProgress::ClientGone => return Ok(RespEnd::Close),
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => {
                warn!("[HTTP/3] streaming chunked read error: {}", e);
                return Err(502);
            }
        }
    }
}

#[inline]
fn reuse_if(exact: bool) -> RespEnd {
    if exact {
        RespEnd::Reusable
    } else {
        RespEnd::Close
    }
}

/// 1 入力バッファ分の chunked を `next_data_span` で逐次デコードしてストリーム送出する。
async fn drain_chunked(
    decoder: &mut crate::http_utils::ChunkedDecoder,
    data: &Bytes,
    resp_tx: &Sender<RespMsg>,
    notify: &ConnWaker,
) -> ChunkedProgress {
    let mut pos = 0;
    while pos < data.len() {
        let span = decoder.next_data_span(&data[pos..]);
        if span.data_len > 0 {
            // 入力 Bytes のサブスライス（ゼロコピー）。
            let start = pos + span.data_start;
            let chunk = data.slice(start..start + span.data_len);
            if send_body_bytes(resp_tx, notify, chunk).await.is_err() {
                return ChunkedProgress::ClientGone;
            }
        }
        if span.complete {
            return ChunkedProgress::Done {
                exact: pos + span.consumed == data.len(),
            };
        }
        if span.consumed == 0 {
            break; // これ以上進めない（次の read を待つ）。
        }
        pos += span.consumed;
    }
    ChunkedProgress::More
}

/// 非圧縮・EOF 終端（長さもチャンクも無い）のボディ転送。接続は再利用できない。
async fn stream_body_eof(
    backend: &BackendIo,
    leftover: Bytes,
    mut read_buf: Vec<u8>,
    deadline: std::time::Instant,
    resp_tx: &Sender<RespMsg>,
    notify: &ConnWaker,
) -> Result<RespEnd, u16> {
    if !leftover.is_empty() && send_body_bytes(resp_tx, notify, leftover).await.is_err() {
        return Ok(RespEnd::Close);
    }
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(504); // head 送出済み → リセット
        }
        let (res, buf) = backend.read_into(read_buf).await;
        read_buf = buf;
        match res {
            Ok(0) => break, // EOF 終端なので正常終了
            Ok(n) => {
                let chunk = bytes_from_read(&read_buf, n);
                if send_body_bytes(resp_tx, notify, chunk).await.is_err() {
                    return Ok(RespEnd::Close);
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => {
                warn!("[HTTP/3] streaming backend read error mid-body: {}", e);
                return Err(502);
            }
        }
    }
    Ok(RespEnd::Close)
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
) -> Result<RespEnd, u16> {
    // ボディ全体を読み取る（圧縮に必要）。
    let mut body: Vec<u8> = Vec::with_capacity(leftover.len().max(RESP_READ_CHUNK));
    let mut decoder = if parsed.is_chunked {
        Some(crate::http_utils::ChunkedDecoder::new_unlimited())
    } else {
        None
    };
    let mut remaining = parsed.content_length;
    let eof_framed = decoder.is_none() && remaining.is_none();

    // 先読み分。
    let mut exact = accumulate_body(&mut body, &mut decoder, &mut remaining, &leftover);
    let mut done =
        decoder.as_ref().map(|d| d.is_complete()).unwrap_or(false) || remaining == Some(0);

    while !done {
        if std::time::Instant::now() >= deadline {
            // head 未送出なので 504 をそのまま返せる（欠けた本文を圧縮して返さない）。
            return Err(504);
        }
        let (res, buf) = backend.read_into(read_buf).await;
        read_buf = buf;
        match res {
            Ok(0) => {
                // length / chunked フレーミングで終端前の EOF は本文の欠落。
                // EOF 終端（どちらも無い）なら正常終了。
                if !eof_framed {
                    warn!("[HTTP/3] streaming backend closed before the body completed");
                    return Err(502);
                }
                break;
            }
            Ok(n) => {
                let slice = Bytes::copy_from_slice(&read_buf[..n]);
                exact = accumulate_body(&mut body, &mut decoder, &mut remaining, &slice);
                done = decoder.as_ref().map(|d| d.is_complete()).unwrap_or(false)
                    || remaining == Some(0);
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => {
                warn!("[HTTP/3] streaming backend read error: {}", e);
                return Err(502);
            }
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
        return Ok(RespEnd::Close);
    }
    let _ = resp_tx.send(RespMsg::Body(Bytes::from(compressed))).await;
    Ok(if eof_framed {
        RespEnd::Close
    } else {
        reuse_if(exact)
    })
}

/// 圧縮経路用: 1 入力スライスをデコード（chunked）または素通し（length/eof）して body へ蓄積する。
///
/// 戻り値は「入力を余らせずに消費したか」（B-104: 終端の後ろに余剰バイトがあれば `false`）。
fn accumulate_body(
    body: &mut Vec<u8>,
    decoder: &mut Option<crate::http_utils::ChunkedDecoder>,
    remaining: &mut Option<usize>,
    data: &Bytes,
) -> bool {
    if let Some(dec) = decoder {
        let mut pos = 0;
        while pos < data.len() {
            let span = dec.next_data_span(&data[pos..]);
            if span.data_len > 0 {
                let start = pos + span.data_start;
                body.extend_from_slice(&data[start..start + span.data_len]);
            }
            if span.complete {
                return pos + span.consumed == data.len();
            }
            if span.consumed == 0 {
                break;
            }
            pos += span.consumed;
        }
        true
    } else if let Some(rem) = remaining {
        let take = data.len().min(*rem);
        body.extend_from_slice(&data[..take]);
        *rem -= take;
        take == data.len()
    } else {
        body.extend_from_slice(data);
        true
    }
}

// ============================================================================
// バッファ経路の上流往復（B-104 / B-105）
// ============================================================================

/// バッファ経路の上流往復の設定（`handle_request` から渡す）。
pub(crate) struct BufferedExchange<'a> {
    pub target: &'a crate::config::ProxyTarget,
    pub timeout_secs: u64,
    pub tls_insecure: bool,
    pub pool_max_idle: usize,
    pub pool_idle_timeout_secs: u64,
    /// HEAD 要求か（応答本文を読まない）。
    pub no_response_body: bool,
    /// F-177: 冪等な要求か（再利用した接続が応答前に失敗したとき 1 回だけ再送する）。
    pub idempotent: bool,
}

/// バッファ経路の上流応答（本文はフレーミングを外した完全な本文）。
pub(crate) struct BufferedResponse {
    pub status: u16,
    pub headers: Vec<(Vec<u8>, Vec<u8>)>,
    pub body: Vec<u8>,
}

/// 完成済みの HTTP/1.1 要求（head + 本文。`Connection: close` は付けない）を上流へ送り、
/// 応答全体を受け取る。
///
/// B-104: 上流接続は HTTP/3 ワーカーのプール（ストリーミング経路と共通）から取り出し、
/// 応答の終端をちょうど読み切ったときだけ返す。
/// B-105: HTTPS 上流も同じ非同期経路（`tls_connect`）で扱う。以前はスレッドを生成して
/// 同期 TLS で接続し、5ms 刻みでポーリングしていた。
/// F-177: 再利用した接続が応答の 1 バイト目より前に失敗し、要求が冪等なら、要求全体を
/// 保持しているので新規接続で 1 回だけ再送する。
pub(crate) async fn exchange_buffered(
    cfg: &BufferedExchange<'_>,
    request: Bytes,
) -> io::Result<BufferedResponse> {
    let target = cfg.target;
    let addr = target.conn_addr();
    let addr = addr.as_str();
    let sni = target.sni();
    let pool_key = if target.use_tls {
        crate::http_utils::PoolKeyStr::tls_addr(addr, sni, cfg.tls_insecure)
    } else {
        crate::http_utils::PoolKeyStr::plain_addr(addr)
    };

    let mut pooled = h3_pool_get(pool_key.as_str());
    loop {
        let reused = pooled.is_some();
        let backend = match pooled.take() {
            Some(b) => b,
            None => connect_backend_io(
                addr,
                cfg.timeout_secs,
                target.use_tls,
                sni,
                cfg.tls_insecure,
            )
            .await
            .map_err(|status| {
                io::Error::new(
                    if status == 504 {
                        io::ErrorKind::TimedOut
                    } else {
                        io::ErrorKind::ConnectionRefused
                    },
                    "backend connect failed",
                )
            })?,
        };
        let mut got_response_bytes = false;
        let result = buffered_round_trip(
            &backend,
            request.clone(),
            cfg.timeout_secs,
            cfg.no_response_body,
            &mut got_response_bytes,
        )
        .await;
        match result {
            Ok((resp, RespEnd::Reusable)) if backend.is_clean() => {
                h3_pool_put(
                    pool_key.as_str(),
                    backend,
                    cfg.pool_max_idle,
                    cfg.pool_idle_timeout_secs,
                );
                return Ok(resp);
            }
            Ok((resp, _)) => return Ok(resp),
            Err(e) if reused && !got_response_bytes && cfg.idempotent => {
                debug!(
                    "[HTTP/3] pooled backend connection failed before the response ({}); retrying on a fresh connection",
                    e
                );
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

/// 要求を送り、応答を最後まで読む（バッファ経路の 1 往復）。
async fn buffered_round_trip(
    backend: &BackendIo,
    request: Bytes,
    timeout_secs: u64,
    no_response_body: bool,
    got_response_bytes: &mut bool,
) -> io::Result<(BufferedResponse, RespEnd)> {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    let timed_out = || io::Error::new(io::ErrorKind::TimedOut, "backend response timeout");
    match crate::runtime::time::timeout(
        Duration::from_secs(timeout_secs),
        backend.write_all(request),
    )
    .await
    {
        Ok(r) => r?,
        Err(_) => return Err(timed_out()),
    }

    let mut read_buf = vec![0u8; RESP_READ_CHUNK];
    let mut head_buf: Vec<u8> = Vec::with_capacity(4096);
    let header_end = loop {
        if std::time::Instant::now() >= deadline {
            return Err(timed_out());
        }
        let (res, buf) = backend.read_into(read_buf).await;
        read_buf = buf;
        let n = match res {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "backend closed before response headers",
                ))
            }
            Ok(n) => n,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        };
        *got_response_bytes = true;
        head_buf.extend_from_slice(&read_buf[..n]);
        if let Some(pos) = drain_interim_and_find_header_end(&mut head_buf) {
            break pos;
        }
        if head_buf.len() > MAX_RESP_HEADER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "backend response headers too large",
            ));
        }
    };

    let status = parse_status_code(&head_buf[..header_end]).unwrap_or(502);
    let mut parsed = parse_response_headers(&head_buf[..header_end]);
    if no_response_body || status == 204 || status == 304 {
        parsed.framing = Framing::Length(0);
    }
    let reusable_conn = parsed.keep_alive && status != 101;
    let leftover = Bytes::copy_from_slice(&head_buf[header_end + 4..]);

    let mut decoder = if matches!(parsed.framing, Framing::Chunked) {
        Some(crate::http_utils::ChunkedDecoder::new_unlimited())
    } else {
        None
    };
    let mut remaining = match parsed.framing {
        Framing::Length(n) => Some(n),
        _ => None,
    };
    let eof_framed = matches!(parsed.framing, Framing::Eof);
    let mut body: Vec<u8> = Vec::with_capacity(remaining.unwrap_or(leftover.len()));
    let mut exact = accumulate_body(&mut body, &mut decoder, &mut remaining, &leftover);
    let mut done =
        decoder.as_ref().map(|d| d.is_complete()).unwrap_or(false) || remaining == Some(0);
    while !done {
        if std::time::Instant::now() >= deadline {
            return Err(timed_out());
        }
        let (res, buf) = backend.read_into(read_buf).await;
        read_buf = buf;
        match res {
            Ok(0) if eof_framed => break,
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "backend closed before the body completed",
                ))
            }
            Ok(n) => {
                let slice = Bytes::copy_from_slice(&read_buf[..n]);
                exact = accumulate_body(&mut body, &mut decoder, &mut remaining, &slice);
                done = decoder.as_ref().map(|d| d.is_complete()).unwrap_or(false)
                    || remaining == Some(0);
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }

    // 本文はフレーミングを外した完全な形で返すので、chunked の痕跡と（圧縮等で変わりうる）
    // 長さは呼び出し側が付け直す。Content-Length は本文長に合わせる。
    let headers: Vec<(Vec<u8>, Vec<u8>)> = parsed
        .headers
        .into_iter()
        .filter(|(n, _)| !n.eq_ignore_ascii_case(b"content-length") || no_response_body)
        .map(|(n, v)| (n.to_vec(), v.to_vec()))
        .collect();
    let end = if reusable_conn && !eof_framed && exact {
        RespEnd::Reusable
    } else {
        RespEnd::Close
    };
    Ok((
        BufferedResponse {
            status,
            headers,
            body,
        },
        end,
    ))
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
    notify: &ConnWaker,
    chunk: Bytes,
) -> Result<(), ()> {
    resp_tx.send(RespMsg::Body(chunk)).await.map_err(|_| ())?;
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
    crate::http_utils::push_chunk_size_line(&mut header, data.len());
    backend.write_all(Bytes::from(header)).await?;
    backend.write_all(data).await?;
    backend.write_all(Bytes::from_static(b"\r\n")).await?;
    Ok(())
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
    /// B-104: 上流がこの応答の後も接続を維持するか（HTTP/1.1 で `Connection: close` 無し、
    /// または HTTP/1.0 で `Connection: keep-alive`）。`false` なら接続をプールへ返さない。
    keep_alive: bool,
}

/// HTTP/1.1 レスポンスヘッダ部（ステータス行除く）をパースし、転送用ヘッダとフレーミングを返す。
fn parse_response_headers(header_bytes: &[u8]) -> ParsedHeaders {
    let mut headers: RespHeaders = Vec::new();
    let mut is_chunked = false;
    let mut content_length: Option<usize> = None;
    let mut content_type: Option<Bytes> = None;
    let mut content_encoding: Option<Bytes> = None;
    let http11 = header_bytes.starts_with(b"HTTP/1.1");
    let mut conn_close = false;
    let mut conn_keep_alive = false;

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
        if name.eq_ignore_ascii_case(b"connection") {
            // トークンのリスト（例: `keep-alive, Upgrade`）。
            for token in value.split(|&b| b == b',') {
                let token = token.trim_ascii();
                if token.eq_ignore_ascii_case(b"close") {
                    conn_close = true;
                } else if token.eq_ignore_ascii_case(b"keep-alive") {
                    conn_keep_alive = true;
                }
            }
            continue; // ホップバイホップ。
        }
        if name.eq_ignore_ascii_case(b"keep-alive") {
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
        keep_alive: !conn_close && (http11 || conn_keep_alive),
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
    fn b104_keep_alive_detection() {
        let h = parse_response_headers(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n");
        assert!(h.keep_alive);
        let h = parse_response_headers(b"HTTP/1.1 200 OK\r\nConnection: close\r\n");
        assert!(!h.keep_alive);
        let h = parse_response_headers(b"HTTP/1.1 200 OK\r\nConnection: Keep-Alive, Close\r\n");
        assert!(!h.keep_alive);
        let h = parse_response_headers(b"HTTP/1.0 200 OK\r\nContent-Length: 3\r\n");
        assert!(
            !h.keep_alive,
            "HTTP/1.0 closes unless keep-alive is requested"
        );
        let h = parse_response_headers(b"HTTP/1.0 200 OK\r\nConnection: keep-alive\r\n");
        assert!(h.keep_alive);
        // Connection ヘッダはクライアントへ転送しない（ホップバイホップ）。
        assert!(h
            .headers
            .iter()
            .all(|(n, _)| !n.eq_ignore_ascii_case(b"connection")));
    }

    #[test]
    fn b104_accumulate_body_reports_overrun() {
        let mut body = Vec::new();
        let mut dec = None;
        let mut rem = Some(3usize);
        assert!(accumulate_body(
            &mut body,
            &mut dec,
            &mut rem,
            &Bytes::from_static(b"abc")
        ));
        let mut rem = Some(3usize);
        let mut body = Vec::new();
        assert!(!accumulate_body(
            &mut body,
            &mut dec,
            &mut rem,
            &Bytes::from_static(b"abcd")
        ));
        assert_eq!(body, b"abc");

        let mut dec = Some(crate::http_utils::ChunkedDecoder::new_unlimited());
        let mut rem = None;
        let mut body = Vec::new();
        assert!(accumulate_body(
            &mut body,
            &mut dec,
            &mut rem,
            &Bytes::from_static(b"3\r\nabc\r\n0\r\n\r\n")
        ));
        assert_eq!(body, b"abc");
        let mut dec = Some(crate::http_utils::ChunkedDecoder::new_unlimited());
        let mut body = Vec::new();
        assert!(!accumulate_body(
            &mut body,
            &mut dec,
            &mut rem,
            &Bytes::from_static(b"3\r\nabc\r\n0\r\n\r\nX")
        ));
    }

    /// rustls の client/server をメモリ上でハンドシェイクさせる（テスト用）。
    /// docker build のサンドボックス等、io_uring が seccomp で拒否される環境では
    /// ランタイムを起動できない（`l4::proxy` のテストと同じ判定）。
    #[cfg(all(veil_rt_uring, target_os = "linux"))]
    fn runtime_available() -> bool {
        crate::runtime::ring::IoUring::new(8, 0).is_ok()
    }

    #[cfg(all(unix, not(all(veil_rt_uring, target_os = "linux"))))]
    fn runtime_available() -> bool {
        true
    }

    #[cfg(unix)]
    fn handshaked_pair() -> (rustls::ClientConnection, rustls::ServerConnection) {
        use std::sync::Arc;
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let provider = Arc::new(crate::tls_provider::provider::default_provider());
        let cert_der = ck.cert.der().clone();
        let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()),
        );
        let server_cfg = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert_der).unwrap();
        let client_cfg = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let mut client = rustls::ClientConnection::new(
            Arc::new(client_cfg),
            rustls::pki_types::ServerName::try_from("localhost").unwrap(),
        )
        .unwrap();
        let mut server = rustls::ServerConnection::new(Arc::new(server_cfg)).unwrap();
        while client.is_handshaking() || server.is_handshaking() {
            let mut buf = Vec::new();
            client.write_tls(&mut buf).unwrap();
            server.read_tls(&mut &buf[..]).unwrap();
            server.process_new_packets().unwrap();
            let mut buf = Vec::new();
            server.write_tls(&mut buf).unwrap();
            client.read_tls(&mut &buf[..]).unwrap();
            client.process_new_packets().unwrap();
        }
        (client, server)
    }

    /// 1 回の生 read に「16KB レコードの末尾 + 後続の小さいレコード群」が入っても
    /// `TlsBackend::read_into` がエラーにならず全量を返すこと（回帰テスト）。
    ///
    /// rustls の deframer は `read_tls` 1 回あたり最大 4KB しか取り込まないため、
    /// 生 read 1 回分（最大 16KB）を平文の排出なしに投入し続けると、16KB レコードが
    /// 完成した直後に小さいレコードの平文が積み増されて受信平文上限（16KB）を超え、
    /// 次の `read_tls` が "received plaintext buffer full" で失敗していた。
    /// HTTP/3 の TLS バックエンド経路はこのエラーで本文を途中終了させ、macOS の E2E
    /// （1.2MB のアップロード折り返し）で切り詰められた応答が 200 で返っていた。
    #[cfg(unix)]
    #[test]
    // 理由付き allow: テストコード（書き込みスレッドの完了待ちに同期 sleep を使う。データプレーン非経由）。
    #[allow(clippy::disallowed_methods)]
    fn tls_backend_read_survives_large_record_followed_by_small_records() {
        use std::io::Write as _;
        use std::os::unix::io::FromRawFd as _;

        if !runtime_available() {
            eprintln!("skip: async runtime unavailable (io_uring denied in this sandbox)");
            return;
        }

        let (client, mut server) = handshaked_pair();
        // 16KB の最大長レコード 1 本 + 200 バイトのレコード 80 本。
        let mut expected = Vec::new();
        let big: Vec<u8> = (0..16 * 1024).map(|i| (i % 251) as u8).collect();
        let mut cipher = Vec::new();
        server.writer().write_all(&big).unwrap();
        expected.extend_from_slice(&big);
        server.write_tls(&mut cipher).unwrap();
        for i in 0..80u8 {
            let small = [i; 200];
            server.writer().write_all(&small).unwrap();
            expected.extend_from_slice(&small);
            while server.wants_write() {
                server.write_tls(&mut cipher).unwrap();
            }
        }

        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: fds は 2 要素の有効な配列。
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(rc, 0, "socketpair failed");
        // 暗号文を一括で流し込む（受信側が 1 回の read で 16KB を取り込めるように）。
        // SAFETY: fds[1] は socketpair が返した所有 fd で、ここで一度だけ所有権を移す。
        let mut peer = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fds[1]) };
        let writer = std::thread::spawn(move || {
            peer.write_all(&cipher).unwrap();
            peer
        });
        std::thread::sleep(std::time::Duration::from_millis(100));

        // SAFETY: fds[0] は socketpair が返した所有 fd で、TlsBackend へ所有権を移す。
        let inner = unsafe { TcpStream::from_raw_fd(fds[0]) };
        let backend = TlsBackend::new(inner, Some(client), Vec::new());
        let got = crate::runtime::block_on(async move {
            let mut got = Vec::new();
            let mut buf = vec![0u8; 64 * 1024];
            while got.len() < expected.len() {
                let (res, b) = backend.read_into(buf).await;
                buf = b;
                let n = res.expect("TLS backend read must not fail");
                assert!(n > 0, "unexpected EOF after {} bytes", got.len());
                got.extend_from_slice(&buf[..n]);
            }
            assert_eq!(got, expected);
            got.len()
        });
        assert_eq!(got, 16 * 1024 + 80 * 200);
        drop(writer.join().unwrap());
    }

    #[test]
    fn push_chunk_size_line_hex() {
        let mut b = Vec::new();
        crate::http_utils::push_chunk_size_line(&mut b, 0);
        assert_eq!(b, b"0\r\n");
        let mut b = Vec::new();
        crate::http_utils::push_chunk_size_line(&mut b, 255);
        assert_eq!(b, b"ff\r\n");
        let mut b = Vec::new();
        crate::http_utils::push_chunk_size_line(&mut b, 7000);
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

    // F-161: 同一 ConnWaker から複数回 notify() してもキュー長が 1（コアレッシング）。
    #[test]
    fn conn_waker_notify_coalesces_until_drained() {
        let wake_queue: WakeQueue = Rc::new(RefCell::new(VecDeque::new()));
        let notify = H3Notify::new();
        let cid: ConnKey = Rc::new(quiche::ConnectionId::from_ref(&[1, 2, 3]).into_owned());
        let waker = ConnWaker::new(cid, wake_queue.clone(), notify);

        // 3 回 notify() してもキューには 1 エントリしか積まれない。
        waker.notify();
        waker.notify();
        waker.notify();
        assert_eq!(wake_queue.borrow().len(), 1);

        // drain（pop → flag=false）を模してから再度 notify() すると再びキューへ積まれる。
        let (_, queued) = wake_queue.borrow_mut().pop_front().expect("1 entry");
        queued.set(false);
        assert!(wake_queue.borrow().is_empty());

        waker.notify();
        assert_eq!(wake_queue.borrow().len(), 1);
    }
}
