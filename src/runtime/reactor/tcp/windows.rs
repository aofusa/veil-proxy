//! 非同期 TcpListener / TcpStream（reactor バックエンド、Windows/Winsock 実装）
//!
//! `unix.rs` と同一の公開 API（型・メソッドシグネチャ）を提供する。実装方式も同じ
//! try-first パターン: 非ブロッキング Winsock 呼び出しを先に試し、`WSAEWOULDBLOCK`
//! なら readiness を登録して `Pending` を返し、起床後に再試行する。
//!
//! Winsock は `errno`/`GetLastError` ではなく `WSAGetLastError()` にエラーコードを積む
//! ため、`io::Error::last_os_error()`（`GetLastError` ベース）ではなく本ファイルの
//! `last_wsa_error()` を必ず使う。`io::Error::from_raw_os_error` は標準ライブラリが
//! Windows では WSA エラーコードも正しく `ErrorKind`（`WouldBlock` 等）へマップするため、
//! `is_would_block()` はプラットフォーム共通のまま使い回せる。

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once, Weak};
use std::task::{Context, Poll};

use windows_sys::Win32::Networking::WinSock::{
    self, closesocket, connect as ws_connect, getpeername, getsockname, getsockopt, ioctlsocket,
    recv, send, setsockopt, shutdown as ws_shutdown, socket, WSADuplicateSocketW, WSAGetLastError,
    WSASocketW, WSAStartup, AF_INET, AF_INET6, FIONBIO, FROM_PROTOCOL_INFO, INVALID_SOCKET,
    IPPROTO_TCP, SD_BOTH, SD_RECEIVE, SD_SEND, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6, SOCKET,
    SOCKET_ERROR, SOCK_STREAM, SOL_SOCKET, SO_ERROR, SO_EXCLUSIVEADDRUSE,
    TCP_NODELAY as WIN_TCP_NODELAY, WSADATA, WSAEINPROGRESS, WSAEWOULDBLOCK, WSAPROTOCOL_INFOW,
    WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED,
};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;

use crate::runtime::buf::{IoBuf, IoBufMut};
use crate::runtime::executor::{coop_proceed, register_read, register_write, unregister};
use crate::runtime::handle::{win, AsRawFd, RawFd};

/// `WSAStartup` を一度だけ実行する。プロセス内のどの Winsock API 呼び出しよりも
/// 先に済ませる必要があるため、ソケット生成の入口（`create_nonblocking_socket`）で
/// 呼ぶ（`Once` でプロセス生涯 1 回のみ実行される）。
fn ensure_wsa_started() {
    static INIT: Once = Once::new();
    INIT.call_once(|| unsafe {
        let mut data: WSADATA = std::mem::zeroed();
        // 2.2 を要求する（現行 Windows は常に対応）。
        WSAStartup(0x0202, &mut data);
    });
    // `WSACleanup` はプロセス終了時に OS が後始末するため明示的には呼ばない
    // （複数スレッドが同時に Winsock を使う本プロセスの構造上、参照カウント管理が
    // 煩雑になるだけで実利が無いため）。
}

/// 直近の Winsock 呼び出しのエラーを `io::Error` として取得する。
///
/// `GetLastError`（`io::Error::last_os_error()`）ではなく `WSAGetLastError()` を
/// 使う必要がある点が Winsock API の注意点（別のエラーコード空間）。
#[inline]
fn last_wsa_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

/// `EAGAIN`/`EWOULDBLOCK` 相当（`WSAEWOULDBLOCK`）か判定する。
///
/// `io::Error::from_raw_os_error` は Windows でも WSA エラーコードを
/// `ErrorKind::WouldBlock` へマップするため、Unix 版と同じ判定式を使い回せる。
#[inline]
fn is_would_block(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::WouldBlock
}

fn sockaddr_to_storage(addr: &SocketAddr) -> (Vec<u8>, i32) {
    match addr {
        SocketAddr::V4(v4) => {
            let mut sin: SOCKADDR_IN = unsafe { std::mem::zeroed() };
            sin.sin_family = AF_INET;
            sin.sin_port = v4.port().to_be();
            sin.sin_addr.S_un.S_addr = u32::from_ne_bytes(v4.ip().octets());
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    &sin as *const _ as *const u8,
                    std::mem::size_of::<SOCKADDR_IN>(),
                )
            }
            .to_vec();
            (bytes, std::mem::size_of::<SOCKADDR_IN>() as i32)
        }
        SocketAddr::V6(v6) => {
            let mut sin6: SOCKADDR_IN6 = unsafe { std::mem::zeroed() };
            sin6.sin6_family = AF_INET6;
            sin6.sin6_port = v6.port().to_be();
            sin6.sin6_addr.u.Byte = v6.ip().octets();
            sin6.sin6_flowinfo = v6.flowinfo();
            // union フィールドへの書き込みは安全（読み取りのみ unsafe）。
            sin6.Anonymous.sin6_scope_id = v6.scope_id();
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    &sin6 as *const _ as *const u8,
                    std::mem::size_of::<SOCKADDR_IN6>(),
                )
            }
            .to_vec();
            (bytes, std::mem::size_of::<SOCKADDR_IN6>() as i32)
        }
    }
}

/// `buf` は `[u8; 128]` の受け口（境界は 1 バイト）なので、`SOCKADDR_IN` などへの参照を作らず、境界の揃った
/// 値へ写してから読む（揃っていない参照は未定義動作。B-353）。
fn storage_to_sockaddr(buf: &[u8]) -> io::Result<SocketAddr> {
    if buf.len() >= std::mem::size_of::<SOCKADDR_IN>() {
        // sa_family は先頭の 2 バイト（ネイティブのバイト順）
        let family = u16::from_ne_bytes([buf[0], buf[1]]);
        if family == AF_INET {
            // SAFETY: 長さは確認済み。read_unaligned は境界を問わず、SOCKADDR_IN は任意のビットパターンが有効な POD。
            let sin = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SOCKADDR_IN) };
            let ip = std::net::Ipv4Addr::from(u32::from_be(unsafe { sin.sin_addr.S_un.S_addr }));
            let port = u16::from_be(sin.sin_port);
            return Ok(SocketAddr::V4(std::net::SocketAddrV4::new(ip, port)));
        }
        if family == AF_INET6 && buf.len() >= std::mem::size_of::<SOCKADDR_IN6>() {
            // SAFETY: 長さは確認済み。read_unaligned は境界を問わず、SOCKADDR_IN6 は任意のビットパターンが有効な POD。
            let sin6 = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SOCKADDR_IN6) };
            let ip = std::net::Ipv6Addr::from(unsafe { sin6.sin6_addr.u.Byte });
            let port = u16::from_be(sin6.sin6_port);
            let scope_id = unsafe { sin6.Anonymous.sin6_scope_id };
            return Ok(SocketAddr::V6(std::net::SocketAddrV6::new(
                ip,
                port,
                sin6.sin6_flowinfo,
                scope_id,
            )));
        }
    }
    Err(io::Error::other("unsupported address family"))
}

/// ノンブロッキングソケットを作成する。
pub(crate) fn create_nonblocking_socket(domain: i32) -> io::Result<RawFd> {
    ensure_wsa_started();
    let sock = unsafe { socket(domain, SOCK_STREAM, IPPROTO_TCP) };
    if sock == INVALID_SOCKET {
        return Err(last_wsa_error());
    }
    let mut nonblocking: u32 = 1;
    if unsafe { ioctlsocket(sock, FIONBIO, &mut nonblocking) } == SOCKET_ERROR {
        let e = last_wsa_error();
        unsafe { closesocket(sock) };
        return Err(e);
    }
    Ok(win::from_socket(sock))
}

/// 待ち受けのソケットを作る（`SO_EXCLUSIVEADDRUSE` → bind → listen）。
///
/// B-460: 以前は `SO_REUSEADDR` を付けてワーカーごとに同じポートへ bind していた。Windows では `SO_REUSEADDR` を
/// 付けたソケットのポートへ、同じく `SO_REUSEADDR` を付けた他の利用者のソケットも（特権なしで）bind でき、届く接続が
/// どちらかへ定まらずに渡るので、平文のリスナーの要求（Cookie・Authorization）を奪えた。`SO_EXCLUSIVEADDRUSE` は
/// 誰の重ねた bind も断る（Windows の既定ではポートの再利用に TIME_WAIT を待たない）
fn listen_exclusive(addr: &SocketAddr) -> io::Result<SOCKET> {
    let domain = if addr.is_ipv6() { AF_INET6 } else { AF_INET };
    let sock = win::to_socket(create_nonblocking_socket(domain as i32)?);
    let optval: i32 = 1;
    // SAFETY: sock は直前に作った有効なソケット。optval は i32 を指し長さはその大きさ
    let ret = unsafe {
        setsockopt(
            sock,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            &optval as *const _ as *const u8,
            std::mem::size_of::<i32>() as i32,
        )
    };
    if ret == SOCKET_ERROR {
        let e = last_wsa_error();
        // SAFETY: 所有しているソケットを 1 回だけ閉じる（誤りは先に読んだ）
        unsafe { closesocket(sock) };
        return Err(e);
    }
    let (storage, len) = sockaddr_to_storage(addr);
    // SAFETY: sock は有効なソケット。storage は sockaddr_to_storage が作った len バイトの SOCKADDR
    let ret = unsafe { WinSock::bind(sock, storage.as_ptr() as *const SOCKADDR, len) };
    if ret == SOCKET_ERROR {
        let e = last_wsa_error();
        // SAFETY: 同上
        unsafe { closesocket(sock) };
        return Err(e);
    }
    // SAFETY: sock は bind 済みの有効なソケット
    let ret = unsafe { WinSock::listen(sock, 1024) };
    if ret == SOCKET_ERROR {
        let e = last_wsa_error();
        // SAFETY: 同上
        unsafe { closesocket(sock) };
        return Err(e);
    }
    Ok(sock)
}

/// 同じソケットを指す別の記述子を作る（ワーカーごとのリスナー。各々が自分の記述子を閉じ、最後の 1 つが閉じたときに
/// ソケットが閉じる）。ノンブロッキングにして返す
fn duplicate_listen_socket(base: SOCKET) -> io::Result<SOCKET> {
    // SAFETY: WSAPROTOCOL_INFOW は整数と配列だけの POD で、全ビット 0 が有効な初期値
    let mut info: WSAPROTOCOL_INFOW = unsafe { std::mem::zeroed() };
    // SAFETY: base は生きている待ち受けのソケット。info は WSADuplicateSocketW が書く出力の領域。複製先は自分のプロセス
    if unsafe { WSADuplicateSocketW(base, GetCurrentProcessId(), &mut info) } == SOCKET_ERROR {
        return Err(last_wsa_error());
    }
    // SAFETY: info は直前の WSADuplicateSocketW が満たした値。子プロセスへ継承させない
    let sock = unsafe {
        WSASocketW(
            FROM_PROTOCOL_INFO,
            FROM_PROTOCOL_INFO,
            FROM_PROTOCOL_INFO,
            &info,
            0,
            WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    if sock == INVALID_SOCKET {
        return Err(last_wsa_error());
    }
    let mut nonblocking: u32 = 1;
    // SAFETY: sock は直前に作った有効なソケット
    if unsafe { ioctlsocket(sock, FIONBIO, &mut nonblocking) } == SOCKET_ERROR {
        let e = last_wsa_error();
        // SAFETY: 所有しているソケットを 1 回だけ閉じる（誤りは先に読んだ）
        unsafe { closesocket(sock) };
        return Err(e);
    }
    Ok(sock)
}

/// 同じアドレスの全ワーカーのリスナーが複製元にする待ち受けのソケット（最後のリスナーが落ちたら閉じる。B-460）
struct SharedListenSocket {
    sock: SOCKET,
    /// どれかのワーカーがこのソケットで `accept` を呼んでいる（B-480。[`raw_accept_one`] が取る番）
    accepting: AtomicBool,
}

impl Drop for SharedListenSocket {
    fn drop(&mut self) {
        // SAFETY: sock はこの値だけが所有する複製元の記述子。ワーカーのリスナーは自分の複製を別に閉じる
        unsafe { closesocket(self.sock) };
    }
}

/// アドレスごとの複製元（起動・リロード時のリスナー作成だけが触るコールドパス）
static SHARED_LISTEN_SOCKETS: Mutex<Vec<(SocketAddr, Weak<SharedListenSocket>)>> =
    Mutex::new(Vec::new());

// ====================
// TcpListener
// ====================

/// 非同期 TCP リスナー。
pub struct TcpListener {
    fd: RawFd,
    /// `bind_reuse_port` のリスナーが複製元を生かしておく（B-460）。受理の番もここで取る（B-480）
    shared: Option<Arc<SharedListenSocket>>,
}

impl TcpListener {
    pub fn bind(addr: impl std::net::ToSocketAddrs) -> io::Result<Self> {
        let addr = resolve_listen_addr(addr)?;
        Ok(Self {
            fd: win::from_socket(listen_exclusive(&addr)?),
            shared: None,
        })
    }

    /// Windows には Linux 的な `SO_REUSEPORT`（カーネル分散）が無いため、同じアドレスの全ワーカーで 1 本の
    /// 待ち受けのソケット（`SO_EXCLUSIVEADDRUSE`）を共有し、各ワーカーにはその複製を返す。接続はその時に
    /// 受理したワーカーへ渡る（thread-per-core の accept 分散は保証されない。F-125）。B-460: 以前は各ワーカーが
    /// `SO_REUSEADDR` で同じポートへ bind しており、他の利用者のソケットも重ねて bind できた。複製での受理は
    /// 同時に 1 つのワーカーだけが行う（B-480。[`raw_accept_one`]）
    pub fn bind_reuse_port(addr: impl std::net::ToSocketAddrs) -> io::Result<Self> {
        let addr = resolve_listen_addr(addr)?;
        // ポート 0 は呼び出しごとに別のポート（共有しない）
        if addr.port() == 0 {
            return Self::bind(addr);
        }
        let shared = {
            let mut table = SHARED_LISTEN_SOCKETS
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            table.retain(|(_, w)| w.strong_count() > 0);
            match table
                .iter()
                .find(|(a, _)| *a == addr)
                .and_then(|(_, w)| w.upgrade())
            {
                Some(s) => s,
                None => {
                    let s = Arc::new(SharedListenSocket {
                        sock: listen_exclusive(&addr)?,
                        accepting: AtomicBool::new(false),
                    });
                    table.push((addr, Arc::downgrade(&s)));
                    s
                }
            }
        };
        Ok(Self {
            fd: win::from_socket(duplicate_listen_socket(shared.sock)?),
            shared: Some(shared),
        })
    }

    pub fn accept(&self) -> Accept<'_> {
        Accept { listener: self }
    }

    /// バックログに溜まっている接続を最大 `max_batch` 件までまとめて受理する
    /// （F-155 の nginx `multi_accept` 相当。unix 版と同じインタフェース・同じ規律）。
    ///
    /// 1 周回 1 接続にしないことでバックログを引き上げるが、**上限で必ず抜けて
    /// イベントループへ戻る**協調的設計を崩さないこと。受理できた件数を返し、
    /// `0` は「今は接続が無い」（`WSAEWOULDBLOCK`）を意味する。
    ///
    /// アドレス変換に失敗した接続は**その 1 件だけ破棄**して受理を続ける
    /// （`TcpStream` を先に構築してあるため `Drop` でソケットが閉じられ、リークしない）。
    pub fn accept_batch<F>(&self, max_batch: usize, mut on_conn: F) -> io::Result<usize>
    where
        F: FnMut(TcpStream, SocketAddr),
    {
        let mut count = 0usize;
        while count < max_batch {
            match raw_accept_one(self) {
                Ok(Some((fd, storage))) => {
                    // 先に TcpStream を構築する: アドレス変換が失敗しても Drop 経由で
                    // ソケットがクローズされ、リークしない（`Accept::poll` と同じ順序）。
                    let stream = TcpStream { fd };
                    match storage_to_sockaddr(&storage) {
                        Ok(peer_addr) => {
                            on_conn(stream, peer_addr);
                            count += 1;
                        }
                        Err(_) => {
                            // この 1 件のみ破棄して次へ進む（doc コメント参照）。
                            drop(stream);
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(count)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        let sock = win::to_socket(self.fd);
        let mut buf = [0u8; 128];
        let mut len = buf.len() as i32;
        let ret = unsafe { getsockname(sock, buf.as_mut_ptr() as *mut SOCKADDR, &mut len) };
        if ret == SOCKET_ERROR {
            return Err(last_wsa_error());
        }
        storage_to_sockaddr(&buf)
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        unregister(self.fd);
        // 自分の記述子だけを閉じる（共有の複製元は `shared` の最後の参照が落ちたときに閉じる）
        unsafe { closesocket(win::to_socket(self.fd)) };
    }
}

/// リスナーのアドレスを 1 つに決める
#[allow(clippy::disallowed_methods)] // 起動・リロード時のリスナー作成のみ（ホットパス外。呼び出し側は解決済みの SocketAddr を渡す）
fn resolve_listen_addr(addr: impl std::net::ToSocketAddrs) -> io::Result<SocketAddr> {
    addr.to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address"))
}

impl AsRawFd for TcpListener {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

// ====================
// Accept Future
// ====================

pub struct Accept<'a> {
    listener: &'a TcpListener,
}

/// 共有の待ち受けのソケットで `accept` を呼ぶ番（B-480）。落とすと返す
struct AcceptTurn<'a>(&'a AtomicBool);

impl Drop for AcceptTurn<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// `accept` を 1 回だけ非ブロッキングで試みる、`Accept::poll` と
/// `TcpListener::accept_batch` の共通ヘルパ（unix 版 `raw_accept_one` と同じ役割）。
///
/// アドレス変換（`storage_to_sockaddr`）は行わず、生の `RawFd` と
/// `sockaddr` バッファを返す（呼び出し側が変換失敗時の後始末方針をそれぞれ選べる
/// ようにするため。`Accept::poll` は即座にエラーを返す一方、`accept_batch` は
/// その 1 件のみ破棄してバックログの受理を続ける）。
///
/// 受理したソケットの非ブロッキング化（`FIONBIO`）は**ここ 1 箇所**で行う
/// （F-155 の「accept の作法を二重管理しない」方針）。
///
/// - 成功: `Ok(Some((fd, storage)))`
/// - `WSAEWOULDBLOCK`（バックログが尽きた）、または同じ待ち受けのソケットで別のワーカーが受理している最中: `Ok(None)`
/// - それ以外のエラー: `Err(e)`
///
/// B-480: 同じ待ち受けのソケットの複製を持つワーカー同士は、同時に `accept` を呼ばない。mswsock の非ブロッキングの
/// `accept` は、接続が並んでいるかを確かめてから待つ形の要求を出す。その間に別のスレッドが同じ接続を受理すると、
/// 次の接続が来るまで `accept` の中で止まり、そのワーカーが受理済みの接続ごと止まっていた。番を取れなかった
/// ワーカーは readiness を待ち直す（接続が残っていれば WSAPoll がすぐに起こす）
fn raw_accept_one(listener: &TcpListener) -> io::Result<Option<(RawFd, [u8; 128])>> {
    let _turn = match &listener.shared {
        Some(shared) => {
            if shared
                .accepting
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                return Ok(None);
            }
            Some(AcceptTurn(&shared.accepting))
        }
        None => None,
    };
    let listener_sock = win::to_socket(listener.fd);
    let mut buf = [0u8; 128];
    let mut len = buf.len() as i32;
    let accepted =
        unsafe { WinSock::accept(listener_sock, buf.as_mut_ptr() as *mut SOCKADDR, &mut len) };
    if accepted != INVALID_SOCKET {
        let mut nonblocking: u32 = 1;
        unsafe { ioctlsocket(accepted, FIONBIO, &mut nonblocking) };
        return Ok(Some((win::from_socket(accepted), buf)));
    }
    let e = last_wsa_error();
    if is_would_block(&e) {
        return Ok(None);
    }
    Err(e)
}

impl<'a> Future for Accept<'a> {
    type Output = io::Result<(TcpStream, SocketAddr)>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match raw_accept_one(self.listener) {
            Ok(Some((fd, buf))) => {
                // 先に TcpStream を構築してからアドレス変換する（変換失敗時も
                // Drop でソケットが閉じられ、リークしない）。
                let stream = TcpStream { fd };
                let peer_addr = storage_to_sockaddr(&buf)?;
                Poll::Ready(Ok((stream, peer_addr)))
            }
            Ok(None) => {
                register_read(self.listener.fd, cx.waker().clone());
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

// ====================
// TcpStream
// ====================

pub struct TcpStream {
    pub(crate) fd: RawFd,
}

impl TcpStream {
    /// # Safety
    /// `fd` は有効な非ブロッキングソケットハンドルであること。
    pub unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Self { fd }
    }

    pub fn connect(addr: SocketAddr) -> Connect {
        Connect {
            addr,
            fd: crate::runtime::handle::INVALID_FD,
            registered: false,
        }
    }

    pub async fn connect_str(addr: &str) -> io::Result<TcpStream> {
        // F-170: UDS バックエンド表記（`unix:<path>`）は Windows では非対応（B-69 の
        // 教訓どおり windows.rs にも同じ分岐を必ず入れる。挙動は「対応しない」旨の
        // エラーを返すのみで、TCP 経路の命令列は無変更）。
        if addr.starts_with("unix:") {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unix domain socket backends are not supported on this platform",
            ));
        }
        // B-107: ホスト名は offload で解決する（同期 getaddrinfo でイベントループを止めない）。
        // B-180: 解決したアドレスを順に試す（localhost が ::1 と 127.0.0.1 の両方になる等）。
        crate::runtime::dns::connect_each(addr, TcpStream::connect).await
    }

    pub fn read<T: IoBufMut>(&self, buf: T) -> ReadFuture<T> {
        ReadFuture {
            fd: self.fd,
            buf: Some(buf),
        }
    }

    pub fn write<T: IoBuf>(&self, buf: T) -> WriteFuture<T> {
        WriteFuture {
            fd: self.fd,
            buf: Some(buf),
        }
    }

    /// 2 つの不連続バッファを書き込む。Windows は `WSASend`（`WSABUF` scatter-gather）を
    /// 使う（Unix 版の `sendmsg` に相当）。
    pub fn writev2<A: IoBuf, B: IoBuf>(&self, a: A, b: B, skip: usize) -> SendMsgFuture<A, B> {
        SendMsgFuture {
            fd: self.fd,
            bufs: Some((a, b)),
            skip,
        }
    }

    pub async fn write_all_vectored<A: IoBuf, B: IoBuf>(
        &self,
        a: A,
        b: B,
    ) -> (io::Result<()>, A, B) {
        let total = a.bytes_init() + b.bytes_init();
        let mut sent = 0usize;
        let (mut a, mut b) = (a, b);
        while sent < total {
            let (res, ra, rb) = self.writev2(a, b, sent).await;
            a = ra;
            b = rb;
            match res {
                Ok(0) => {
                    return (
                        Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "WSASend returned zero",
                        )),
                        a,
                        b,
                    );
                }
                Ok(n) => sent += n,
                Err(e) => return (Err(e), a, b),
            }
        }
        (Ok(()), a, b)
    }

    pub fn readable(&self) -> Readable<'_> {
        Readable {
            fd: self.fd,
            _marker: std::marker::PhantomData,
        }
    }

    /// `unix.rs` の `readable_lazy` に対応する（B-69: reactor/tcp のメソッドは両方に足す）。
    /// WSAPoll には readiness ヒントが無いため通常の `readable` と同じ。
    pub fn readable_lazy(&self) -> Readable<'_> {
        self.readable()
    }

    /// `unix.rs` の `readable_confirmed` に対応する（B-69）。WSAPoll にはヒントが無く、常に確かめる。
    pub fn readable_confirmed(&self) -> Readable<'_> {
        self.readable()
    }

    pub fn writable(&self) -> Writable<'_> {
        Writable {
            fd: self.fd,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        let optval: i32 = if nodelay { 1 } else { 0 };
        let ret = unsafe {
            setsockopt(
                win::to_socket(self.fd),
                IPPROTO_TCP,
                WIN_TCP_NODELAY,
                &optval as *const _ as *const u8,
                std::mem::size_of::<i32>() as i32,
            )
        };
        if ret == SOCKET_ERROR {
            return Err(last_wsa_error());
        }
        Ok(())
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        let mut buf = [0u8; 128];
        let mut len = buf.len() as i32;
        let ret = unsafe {
            getpeername(
                win::to_socket(self.fd),
                buf.as_mut_ptr() as *mut SOCKADDR,
                &mut len,
            )
        };
        if ret == SOCKET_ERROR {
            return Err(last_wsa_error());
        }
        storage_to_sockaddr(&buf)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        let mut buf = [0u8; 128];
        let mut len = buf.len() as i32;
        let ret = unsafe {
            getsockname(
                win::to_socket(self.fd),
                buf.as_mut_ptr() as *mut SOCKADDR,
                &mut len,
            )
        };
        if ret == SOCKET_ERROR {
            return Err(last_wsa_error());
        }
        storage_to_sockaddr(&buf)
    }

    pub fn shutdown(&self, how: std::net::Shutdown) -> io::Result<()> {
        let how = match how {
            std::net::Shutdown::Read => SD_RECEIVE,
            std::net::Shutdown::Write => SD_SEND,
            std::net::Shutdown::Both => SD_BOTH,
        };
        let ret = unsafe { ws_shutdown(win::to_socket(self.fd), how) };
        if ret == SOCKET_ERROR {
            return Err(last_wsa_error());
        }
        // Linux/BSD ではローカルの SHUT_RD で同じソケットの待機中 poll が即座に
        // readable（EOF）になるが、Windows の WSAPoll はローカルの SD_RECEIVE を
        // イベントとして報告しない。待機中の read が idle timeout まで起きないため
        // （L4 の B-57: 片方向の close で対向方向の read を解除する経路が Windows だけ
        // 30 秒止まっていた）、読み取り待機者を明示的に起こして recv を再試行させる
        // （recv は WSAESHUTDOWN を返し、読み取り側が終了する）。
        if how != SD_SEND {
            crate::runtime::reactor::executor::wake_all_readers(self.fd);
        }
        Ok(())
    }
}

// ====================
// AsyncReadRent / AsyncWriteRent 実装
// ====================

impl crate::runtime::io::AsyncReadRent for TcpStream {
    fn read<T: crate::runtime::buf::IoBufMut>(
        &mut self,
        buf: T,
    ) -> impl std::future::Future<Output = crate::runtime::io::BufResult<usize, T>> {
        let fd = self.fd;
        ReadFuture { fd, buf: Some(buf) }
    }
}

impl crate::runtime::io::AsyncWriteRent for TcpStream {
    fn write<T: crate::runtime::buf::IoBuf>(
        &mut self,
        buf: T,
    ) -> impl std::future::Future<Output = crate::runtime::io::BufResult<usize, T>> {
        let fd = self.fd;
        WriteFuture { fd, buf: Some(buf) }
    }

    fn shutdown(&mut self) -> impl std::future::Future<Output = std::io::Result<()>> {
        let result = TcpStream::shutdown(self, std::net::Shutdown::Both);
        async move { result }
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unregister(self.fd);
            unsafe { closesocket(win::to_socket(self.fd)) };
        }
    }
}

impl AsRawFd for TcpStream {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

// ====================
// Connect Future
// ====================

pub struct Connect {
    addr: SocketAddr,
    fd: RawFd,
    registered: bool,
}

impl Connect {
    fn fail(&mut self, e: io::Error) -> io::Error {
        let fd = self.fd;
        self.fd = crate::runtime::handle::INVALID_FD;
        if self.registered {
            unregister(fd);
            self.registered = false;
        }
        unsafe { closesocket(win::to_socket(fd)) };
        e
    }
}

impl Future for Connect {
    type Output = io::Result<TcpStream>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.fd < 0 {
            let domain = if self.addr.is_ipv6() {
                AF_INET6
            } else {
                AF_INET
            };
            let fd = match create_nonblocking_socket(domain as i32) {
                Ok(fd) => fd,
                Err(e) => return Poll::Ready(Err(e)),
            };
            self.fd = fd;

            let (storage, len) = sockaddr_to_storage(&self.addr);
            let sock = win::to_socket(fd);
            let ret = unsafe { ws_connect(sock, storage.as_ptr() as *const SOCKADDR, len) };
            if ret == 0 {
                let fd = self.fd;
                self.fd = crate::runtime::handle::INVALID_FD;
                return Poll::Ready(Ok(TcpStream { fd }));
            }
            let e = last_wsa_error();
            if e.raw_os_error() != Some(WSAEINPROGRESS) && e.raw_os_error() != Some(WSAEWOULDBLOCK)
            {
                unsafe { closesocket(sock) };
                self.fd = crate::runtime::handle::INVALID_FD;
                return Poll::Ready(Err(e));
            }
        }

        // writable（接続完了）を WSAPoll 相当の 0 タイムアウトチェックで確認してから
        // SO_ERROR を読む（`Readable`/`Writable` と同じ即時チェックパターン）。
        let sock = win::to_socket(self.fd);
        let mut pfd = WinSock::WSAPOLLFD {
            fd: sock,
            events: WinSock::POLLWRNORM,
            revents: 0,
        };
        let ret = unsafe { WinSock::WSAPoll(&mut pfd, 1, 0) };
        if ret == SOCKET_ERROR {
            let e = last_wsa_error();
            return Poll::Ready(Err(self.fail(e)));
        }
        if ret <= 0
            || pfd.revents & (WinSock::POLLWRNORM | WinSock::POLLERR | WinSock::POLLHUP) == 0
        {
            register_write(self.fd, cx.waker().clone());
            self.registered = true;
            return Poll::Pending;
        }

        let mut err: i32 = 0;
        let mut len = std::mem::size_of::<i32>() as i32;
        let ret = unsafe {
            getsockopt(
                sock,
                SOL_SOCKET,
                SO_ERROR,
                &mut err as *mut _ as *mut u8,
                &mut len,
            )
        };
        if ret == SOCKET_ERROR {
            let e = last_wsa_error();
            return Poll::Ready(Err(self.fail(e)));
        }
        if err != 0 {
            let e = io::Error::from_raw_os_error(err);
            return Poll::Ready(Err(self.fail(e)));
        }

        let fd = self.fd;
        self.fd = crate::runtime::handle::INVALID_FD;
        Poll::Ready(Ok(TcpStream { fd }))
    }
}

impl Drop for Connect {
    fn drop(&mut self) {
        if self.fd >= 0 {
            if self.registered {
                unregister(self.fd);
            }
            unsafe { closesocket(win::to_socket(self.fd)) };
        }
    }
}

// ====================
// Read Future
// ====================

pub struct ReadFuture<T: IoBufMut> {
    fd: RawFd,
    buf: Option<T>,
}

impl<T: IoBufMut> Future for ReadFuture<T> {
    type Output = (io::Result<usize>, T);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // B-332: try-first の I/O が成功し続けるタスクにワーカーを占有させない
        if !coop_proceed(cx) {
            return Poll::Pending;
        }
        let this = unsafe { self.get_unchecked_mut() };
        let buf = this
            .buf
            .as_mut()
            .expect("ReadFuture polled after completion");
        let ret = unsafe {
            recv(
                win::to_socket(this.fd),
                buf.write_ptr(),
                buf.bytes_total() as i32,
                0,
            )
        };
        if ret >= 0 {
            unsafe { buf.set_init(ret as usize) };
            let buf = this.buf.take().expect("buffer present at completion");
            return Poll::Ready((Ok(ret as usize), buf));
        }
        let e = last_wsa_error();
        if is_would_block(&e) {
            register_read(this.fd, cx.waker().clone());
            return Poll::Pending;
        }
        let buf = this.buf.take().expect("buffer present on error");
        Poll::Ready((Err(e), buf))
    }
}

// ====================
// Write Future
// ====================

pub struct WriteFuture<T: IoBuf> {
    fd: RawFd,
    buf: Option<T>,
}

impl<T: IoBuf> Future for WriteFuture<T> {
    type Output = (io::Result<usize>, T);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // B-332: try-first の I/O が成功し続けるタスクにワーカーを占有させない
        if !coop_proceed(cx) {
            return Poll::Pending;
        }
        let this = unsafe { self.get_unchecked_mut() };
        let buf = this
            .buf
            .as_ref()
            .expect("WriteFuture polled after completion");
        let ret = unsafe {
            send(
                win::to_socket(this.fd),
                buf.read_ptr(),
                buf.bytes_init() as i32,
                0,
            )
        };
        if ret >= 0 {
            let buf = this.buf.take().expect("buffer present at completion");
            return Poll::Ready((Ok(ret as usize), buf));
        }
        let e = last_wsa_error();
        if is_would_block(&e) {
            register_write(this.fd, cx.waker().clone());
            return Poll::Pending;
        }
        let buf = this.buf.take().expect("buffer present on error");
        Poll::Ready((Err(e), buf))
    }
}

// ====================
// SendMsg (scatter-gather) Future（`WSASend` 版）
// ====================

pub struct SendMsgFuture<A: IoBuf, B: IoBuf> {
    fd: RawFd,
    bufs: Option<(A, B)>,
    skip: usize,
}

impl<A: IoBuf, B: IoBuf> Future for SendMsgFuture<A, B> {
    type Output = (io::Result<usize>, A, B);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // B-332: try-first の I/O が成功し続けるタスクにワーカーを占有させない
        if !coop_proceed(cx) {
            return Poll::Pending;
        }
        let this = unsafe { self.get_unchecked_mut() };
        let (a, b) = this
            .bufs
            .as_ref()
            .expect("SendMsgFuture polled after completion");
        let (a_ptr, a_len) = (a.read_ptr(), a.bytes_init());
        let (b_ptr, b_len) = (b.read_ptr(), b.bytes_init());
        let skip = this.skip;
        debug_assert!(skip < a_len + b_len);

        let mut wsabufs = [WinSock::WSABUF {
            len: 0,
            buf: std::ptr::null_mut(),
        }; 2];
        let mut count = 0u32;
        if skip < a_len {
            wsabufs[count as usize] = WinSock::WSABUF {
                len: (a_len - skip) as u32,
                buf: unsafe { a_ptr.add(skip) as *mut u8 },
            };
            count += 1;
            if b_len > 0 {
                wsabufs[count as usize] = WinSock::WSABUF {
                    len: b_len as u32,
                    buf: b_ptr as *mut u8,
                };
                count += 1;
            }
        } else {
            let b_skip = skip - a_len;
            wsabufs[count as usize] = WinSock::WSABUF {
                len: (b_len - b_skip) as u32,
                buf: unsafe { b_ptr.add(b_skip) as *mut u8 },
            };
            count += 1;
        }

        let mut sent: u32 = 0;
        let ret = unsafe {
            WinSock::WSASend(
                win::to_socket(this.fd),
                wsabufs.as_ptr(),
                count,
                &mut sent,
                0,
                std::ptr::null_mut(),
                None,
            )
        };
        if ret == 0 {
            let (a, b) = this.bufs.take().expect("buffers present at completion");
            return Poll::Ready((Ok(sent as usize), a, b));
        }
        let e = last_wsa_error();
        if is_would_block(&e) {
            register_write(this.fd, cx.waker().clone());
            return Poll::Pending;
        }
        let (a, b) = this.bufs.take().expect("buffers present on error");
        Poll::Ready((Err(e), a, b))
    }
}

// ====================
// Readable / Writable Future
// ====================

fn poll_ready_now(fd: RawFd, want_write: bool) -> bool {
    let mut pfd = WinSock::WSAPOLLFD {
        fd: win::to_socket(fd),
        events: if want_write {
            WinSock::POLLWRNORM
        } else {
            WinSock::POLLRDNORM
        },
        revents: 0,
    };
    let ret = unsafe { WinSock::WSAPoll(&mut pfd, 1, 0) };
    let mask = if want_write {
        WinSock::POLLWRNORM | WinSock::POLLERR | WinSock::POLLHUP
    } else {
        WinSock::POLLRDNORM | WinSock::POLLERR | WinSock::POLLHUP
    };
    ret > 0 && pfd.revents & mask != 0
}

pub struct Readable<'a> {
    fd: RawFd,
    _marker: std::marker::PhantomData<&'a TcpStream>,
}

impl<'a> Future for Readable<'a> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // B-332: try-first の I/O が成功し続けるタスクにワーカーを占有させない
        if !coop_proceed(cx) {
            return Poll::Pending;
        }
        if poll_ready_now(self.fd, false) {
            return Poll::Ready(Ok(()));
        }
        register_read(self.fd, cx.waker().clone());
        Poll::Pending
    }
}

pub struct Writable<'a> {
    fd: RawFd,
    _marker: std::marker::PhantomData<&'a TcpStream>,
}

impl<'a> Future for Writable<'a> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if poll_ready_now(self.fd, true) {
            return Poll::Ready(Ok(()));
        }
        register_write(self.fd, cx.waker().clone());
        Poll::Pending
    }
}

// ====================
// 汎用 FD 待機 Future（任意のソケットハンドルに使用）
// ====================

pub struct ReadableFd {
    fd: RawFd,
}

impl Future for ReadableFd {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if poll_ready_now(self.fd, false) {
            return Poll::Ready(Ok(()));
        }
        register_read(self.fd, cx.waker().clone());
        Poll::Pending
    }
}

pub struct WritableFd {
    fd: RawFd,
}

impl Future for WritableFd {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if poll_ready_now(self.fd, true) {
            return Poll::Ready(Ok(()));
        }
        register_write(self.fd, cx.waker().clone());
        Poll::Pending
    }
}

pub fn wait_readable_fd(fd: RawFd) -> ReadableFd {
    ReadableFd { fd }
}

pub fn wait_writable_fd(fd: RawFd) -> WritableFd {
    WritableFd { fd }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// B-460: 同じアドレスの `bind_reuse_port` は 1 本の待ち受けを共有し（どの複製でも受理でき）、他のソケットは
    /// `SO_REUSEADDR` を付けても重ねて bind できない。全部閉じたらポートは空く
    // 理由付き allow: 試験で空いているポートを探し、受け手の無い同期の接続を張る（データプレーン外）
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn reuse_port_listeners_share_one_exclusive_socket() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let a = TcpListener::bind_reuse_port(addr).expect("first worker listener");
        let b = TcpListener::bind_reuse_port(addr).expect("second worker listener");
        assert_ne!(a.fd, b.fd, "each worker gets its own descriptor");
        assert_eq!(a.local_addr().unwrap(), addr);
        assert_eq!(b.local_addr().unwrap(), addr);

        // 別のソケットは SO_REUSEADDR を付けても重ねて bind できない
        let other = win::to_socket(create_nonblocking_socket(AF_INET as i32).unwrap());
        let optval: i32 = 1;
        // SAFETY: other は有効なソケット。optval は i32 を指し長さはその大きさ
        unsafe {
            setsockopt(
                other,
                SOL_SOCKET,
                windows_sys::Win32::Networking::WinSock::SO_REUSEADDR,
                &optval as *const _ as *const u8,
                std::mem::size_of::<i32>() as i32,
            )
        };
        let (storage, len) = sockaddr_to_storage(&addr);
        // SAFETY: other は有効なソケット。storage は len バイトの SOCKADDR
        let ret = unsafe { WinSock::bind(other, storage.as_ptr() as *const SOCKADDR, len) };
        let err = last_wsa_error();
        // SAFETY: 所有しているソケットを 1 回だけ閉じる
        unsafe { closesocket(other) };
        assert_eq!(ret, SOCKET_ERROR, "a co-bind must be refused");
        assert!(
            matches!(err.raw_os_error(), Some(10013) | Some(10048)),
            "WSAEACCES or WSAEADDRINUSE: {err}"
        );

        // どちらの複製でも受理できる（同じ待ち受けのソケット）
        let c1 = std::net::TcpStream::connect(addr).unwrap();
        let mut got = 0;
        for _ in 0..200 {
            got += a.accept_batch(4, |_, _| {}).unwrap();
            if got > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(got, 1, "the first duplicate accepts");
        let c2 = std::net::TcpStream::connect(addr).unwrap();
        got = 0;
        for _ in 0..200 {
            got += b.accept_batch(4, |_, _| {}).unwrap();
            if got > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(got, 1, "the second duplicate accepts from the same socket");

        drop(c1);
        drop(c2);

        // 1 本だけ閉じても待ち受けは残り、全部閉じたらポートが空く
        drop(a);
        let c3 = std::net::TcpStream::connect(addr).expect("the other duplicate still listens");
        drop(c3);
        drop(b);
        // 受理した接続が閉じ終えるまで SO_EXCLUSIVEADDRUSE の bind は断られ得る
        let mut again = None;
        for _ in 0..100 {
            if let Ok(l) = TcpListener::bind(addr) {
                again = Some(l);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            again.is_some(),
            "the port is free after every listener closed"
        );
    }

    /// B-480: 同じ待ち受けのソケットの複製で複数のスレッドが同時に受理しても、`accept` の中で止まらない。ワーカーと
    /// 同じく readiness を待ってからまとめて受理し、受理した接続に 1 バイト返す（受理の中で止まったスレッドは、
    /// 先に受理した接続に次の接続が来るまで応えない）
    // 理由付き allow: 試験のスレッド・同期の接続・待ち（データプレーン外）
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn concurrent_accepts_on_a_shared_socket_do_not_block() {
        use std::io::Read;
        use std::sync::atomic::AtomicUsize;
        use std::time::{Duration, Instant};

        const THREADS: usize = 4;
        const ROUNDS: usize = 100;
        const CONNS: usize = 8;
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();
        for _ in 0..THREADS {
            let listener = TcpListener::bind_reuse_port(addr).expect("worker listener");
            let stop = Arc::clone(&stop);
            let finished = Arc::clone(&finished);
            threads.push(std::thread::spawn(move || {
                // ワーカーと同じく、受理した接続はもう一度受理を試みた後で進める（受理のループは受理した接続の
                // タスクを走らせる前に次の accept を呼ぶ）。受理の中で止まると、受理済みの接続に応えない
                let mut accepted: Vec<TcpStream> = Vec::new();
                let mut fresh = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    let mut pfd = WinSock::WSAPOLLFD {
                        fd: win::to_socket(listener.fd),
                        events: WinSock::POLLRDNORM,
                        revents: 0,
                    };
                    // SAFETY: pfd は 1 要素の有効な配列
                    let ready = unsafe { WinSock::WSAPoll(&mut pfd, 1, 20) } > 0;
                    if ready || !accepted.is_empty() {
                        let _ = listener.accept_batch(31, |s, _| fresh.push(s));
                    }
                    for s in accepted.drain(..) {
                        // SAFETY: s.fd は受理した有効なソケット（送信の領域は空いている）
                        unsafe { send(win::to_socket(s.fd), b"x".as_ptr(), 1, 0) };
                    }
                    accepted.append(&mut fresh);
                }
                finished.fetch_add(1, Ordering::Release);
            }));
        }

        // 各回の接続は同時に張る（要求が同時に来たときと同じく、接続が並んだ状態で全スレッドが起きる）
        let mut unanswered = 0;
        for _ in 0..ROUNDS {
            unanswered += std::thread::scope(|scope| {
                let clients: Vec<_> = (0..CONNS)
                    .map(|_| {
                        scope.spawn(|| {
                            let mut c = std::net::TcpStream::connect(addr).expect("connect");
                            c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                            let mut b = [0u8; 1];
                            c.read_exact(&mut b).is_err()
                        })
                    })
                    .collect();
                clients
                    .into_iter()
                    .map(|c| usize::from(c.join().unwrap()))
                    .sum::<usize>()
            });
            if unanswered > 0 {
                break;
            }
        }

        // 受理の中で止まっているスレッドは接続を張って起こしてから終わらせる
        stop.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut wake = Vec::new();
        while finished.load(Ordering::Acquire) < THREADS && Instant::now() < deadline {
            if let Ok(c) = std::net::TcpStream::connect(addr) {
                wake.push(c);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            finished.load(Ordering::Acquire),
            THREADS,
            "a thread stayed inside accept"
        );
        for t in threads {
            t.join().unwrap();
        }
        drop(wake);
        assert_eq!(
            unanswered, 0,
            "a thread stopped inside accept and left the connections it had accepted unanswered"
        );
    }
}
