//! 非同期 TcpListener / TcpStream（reactor バックエンド）
//!
//! `runtime::uring::tcp` と同一の公開 API（型・メソッドシグネチャ）を提供する。
//! 実装方式は「try-first」: まず非ブロッキング syscall（`accept4`/`read`/`write`/
//! `sendmsg`/`connect`）を試し、`EAGAIN` なら oneshot readiness を登録して `Pending` を
//! 返し、起床後に再試行する。
//!
//! io_uring 版と異なり、readiness モデルではカーネルが Future 保有のバッファを非同期に
//! 参照し続けることが構造的に無い（syscall は Future 起床後に同期実行する）ため、
//! `OpGuard`/detach に相当する後始末機構は不要である。各 Future の `Drop` は特別な処理を
//! 必要としない（登録済み Waker を残しても、後続の再登録で上書きされるか、無関係になった
//! タスクへの無害な spurious wake になるのみ）。

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::os::unix::io::{AsRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::runtime::buf::{IoBuf, IoBufMut};
use crate::runtime::executor::{register_read, register_write, unregister};

// SO_* ソケットオプション
const TCP_NODELAY: libc::c_int = 1;

// ====================
// ソケットアドレス変換ユーティリティ（uring 版と同一実装）
// ====================

fn sockaddr_to_storage(addr: &SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let len = match addr {
        SocketAddr::V4(v4) => {
            let sin = unsafe { &mut *(&mut storage as *mut _ as *mut libc::sockaddr_in) };
            sin.sin_family = libc::AF_INET as _;
            sin.sin_port = v4.port().to_be();
            sin.sin_addr.s_addr = u32::from_ne_bytes(v4.ip().octets());
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t
        }
        SocketAddr::V6(v6) => {
            let sin6 = unsafe { &mut *(&mut storage as *mut _ as *mut libc::sockaddr_in6) };
            sin6.sin6_family = libc::AF_INET6 as _;
            sin6.sin6_port = v6.port().to_be();
            sin6.sin6_addr.s6_addr = v6.ip().octets();
            sin6.sin6_flowinfo = v6.flowinfo();
            sin6.sin6_scope_id = v6.scope_id();
            std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t
        }
    };
    (storage, len)
}

fn storage_to_sockaddr(storage: &libc::sockaddr_storage) -> io::Result<SocketAddr> {
    match storage.ss_family as libc::c_int {
        libc::AF_INET => {
            let sin = unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
            let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            let port = u16::from_be(sin.sin_port);
            Ok(SocketAddr::V4(std::net::SocketAddrV4::new(ip, port)))
        }
        libc::AF_INET6 => {
            let sin6 = unsafe { &*(storage as *const _ as *const libc::sockaddr_in6) };
            let ip = std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            let port = u16::from_be(sin6.sin6_port);
            Ok(SocketAddr::V6(std::net::SocketAddrV6::new(
                ip,
                port,
                sin6.sin6_flowinfo,
                sin6.sin6_scope_id,
            )))
        }
        family => Err(io::Error::other(format!(
            "unsupported address family: {}",
            family
        ))),
    }
}

/// ノンブロッキングソケットを作成する（`O_NONBLOCK | O_CLOEXEC`）。
///
/// macOS は send(2) の `MSG_NOSIGNAL` を持たないため、代わりにソケット単位の
/// `SO_NOSIGPIPE` を生成直後に設定する（`WriteFuture`/`SendMsgFuture` 側は
/// macOS では send フラグを 0 にする。設計 docs/artifacts/f125_windows_macos_design.md
/// の macOS 節 2 を参照）。設定失敗は非致命（SIGPIPE は既定で無視されない環境もあるが、
/// プロセス全体で `signal(SIGPIPE, SIG_IGN)` する既存挙動と重複しても害はない）。
fn create_nonblocking_socket(domain: libc::c_int) -> io::Result<RawFd> {
    // macOS は `socket(2)` の type 引数に `SOCK_NONBLOCK`/`SOCK_CLOEXEC` を受け付けない
    // （libc にも定義が無い）。生の `SOCK_STREAM` で作成し `fcntl` で 2 段設定する
    // （accept 側フォールバックと同じ理由。設計 docs/artifacts/f125_windows_macos_design.md
    // の macOS 節 1）。他 OS は 1 syscall で完結させる。
    #[cfg(target_os = "macos")]
    let fd = {
        let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        unsafe {
            libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        fd
    };
    #[cfg(not(target_os = "macos"))]
    let fd = unsafe {
        libc::socket(
            domain,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(target_os = "macos")]
    set_so_nosigpipe(fd);
    Ok(fd)
}

/// `SO_NOSIGPIPE` を設定する（macOS 専用）。send(2) 時の `MSG_NOSIGNAL` 相当を
/// ソケットオプションとして事前設定し、対向クローズ済みソケットへの書き込みで
/// プロセスが SIGPIPE を受け取らないようにする。
#[cfg(target_os = "macos")]
fn set_so_nosigpipe(fd: RawFd) {
    let optval: libc::c_int = 1;
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            &optval as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

/// `EAGAIN`/`EWOULDBLOCK` か判定する。
#[inline]
fn is_would_block(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::WouldBlock
}

// ====================
// TcpListener
// ====================

/// 非同期 TCP リスナー。
pub struct TcpListener {
    fd: RawFd,
    /// AF_UNIX リスナーかどうか（F-164）。`accept`/`accept_batch` の peer アドレス
    /// 取得を分岐させるための唯一のフラグ（`uring::tcp::TcpListener` と同じ設計）。
    is_unix: bool,
}

impl TcpListener {
    /// アドレスにバインドしてリッスンを開始する。
    pub fn bind(addr: impl std::net::ToSocketAddrs) -> io::Result<Self> {
        Self::bind_impl(addr, false)
    }

    /// SO_REUSEPORT を設定してバインドする。
    pub fn bind_reuse_port(addr: impl std::net::ToSocketAddrs) -> io::Result<Self> {
        Self::bind_impl(addr, true)
    }

    fn bind_impl(addr: impl std::net::ToSocketAddrs, reuse_port: bool) -> io::Result<Self> {
        let addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address"))?;

        let domain = if addr.is_ipv6() {
            libc::AF_INET6
        } else {
            libc::AF_INET
        };
        let fd = create_nonblocking_socket(domain)?;

        let optval: libc::c_int = 1;
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
            if reuse_port {
                // FreeBSD の `SO_REUSEPORT` は複数 bind を許すのみでカーネル分散を行わない
                // （Linux の `SO_REUSEPORT` 相当の負荷分散は `SO_REUSEPORT_LB`、FreeBSD 12+）。
                // thread-per-core の accept 分散を成立させるには LB 版が必要なため、
                // FreeBSD では `SO_REUSEPORT_LB` を使う（設計ドキュメント 3.3 節）。
                #[cfg(target_os = "freebsd")]
                let reuseport_opt = libc::SO_REUSEPORT_LB;
                #[cfg(not(target_os = "freebsd"))]
                let reuseport_opt = libc::SO_REUSEPORT;
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    reuseport_opt,
                    &optval as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
        }

        let (storage, len) = sockaddr_to_storage(&addr);
        let ret = unsafe { libc::bind(fd, &storage as *const _ as *const libc::sockaddr, len) };
        if ret < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }

        let ret = unsafe { libc::listen(fd, 1024) };
        if ret < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }

        Ok(Self { fd, is_unix: false })
    }

    /// 既存の AF_UNIX リスナー fd から `TcpListener` を作る（F-164）。
    ///
    /// `server::bind_unix_listener` が 1 度だけ bind+listen した共有 fd を、
    /// 各ワーカーが `dup(2)` した自分専用の fd で呼び出す想定（fd はワーカーごとに
    /// 独立に close される）。AF_UNIX には `SO_REUSEPORT` が無いため、TCP の
    /// `bind_reuse_port` とは異なる経路で各ワーカーの `TcpListener` を用意する。
    ///
    /// # Safety
    /// `fd` は listen 済みの有効な AF_UNIX ソケット fd であり、この `TcpListener`
    /// が所有権を持つこと（Drop で close される）。
    pub unsafe fn from_raw_fd_unix(fd: RawFd) -> Self {
        Self { fd, is_unix: true }
    }

    /// 新しい接続を非同期で受け入れる。
    pub fn accept(&self) -> Accept<'_> {
        Accept {
            listener_fd: self.fd,
            is_unix: self.is_unix,
            _marker: std::marker::PhantomData,
        }
    }

    /// ローカルアドレスを取得する。
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let ret = unsafe {
            libc::getsockname(
                self.fd,
                &mut storage as *mut _ as *mut libc::sockaddr,
                &mut len,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        storage_to_sockaddr(&storage)
    }

    /// F-155: nginx の `multi_accept` 相当。バックログに滞留している接続を
    /// `max_batch` 件を上限に、非ブロッキングで一括受理する（`Future` ではない同期
    /// メソッド。syscall のみでブロックしないため、ホットパスの一部として呼んでよい）。
    ///
    /// 呼び出し側（`entry.rs` のワーカー accept ループ）は、まず `accept().await` で
    /// shutdown チェック用の 1 秒タイムアウト付きで 1 件目を待ち、受理できたら本メソッドで
    /// 残りのバックログを引き上げる想定。`max_batch` 件で必ずループを抜けて呼び出し側へ
    /// 制御を返すため、バックログがどれだけ積み上がっていても他 fd の処理や shutdown
    /// チェックを飢餓させない、協調的な設計になっている。
    ///
    /// ホットパスなので中間 `Vec` 等のバッファは確保しない。受理した接続はその場で
    /// `on_conn` コールバックへ渡す（コールバック方式にしている理由そのもの）。
    ///
    /// `raw_accept_one` の accept4/accept ロジックは `Accept::poll` と共用する
    /// （挙動は完全に同一。macOS の `accept`+`fcntl`+`set_so_nosigpipe` フォールバック、
    /// fd をリークしないよう先に `TcpStream` を構築してから `storage_to_sockaddr` する
    /// 順序を含む）。
    ///
    /// `storage_to_sockaddr` が失敗した場合（未対応アドレスファミリ）は、その 1 件のみ
    /// 破棄して次へ進む（fd は `TcpStream` の `Drop` で close 済みのためリークしない）。
    /// `Accept::poll` は単発 accept なので即座に `Err` を返して呼び出し側へ委ねるが、
    /// `accept_batch` はバックログ一括処理という性質上、1 件の異常でそれ以降の正常な
    /// 接続まで巻き込んで受理を中断する理由が無いため、あえて異なる方針を採る。
    ///
    /// `EINTR` は内側でリトライする。`EWOULDBLOCK`/`EAGAIN` でバックログが尽きたら、
    /// その時点までの受理件数を `Ok` で返す。それ以外のエラーは、それまでに受理した分は
    /// 既に `on_conn` へ渡した上で `Err` を返す。
    pub fn accept_batch<F>(&self, max_batch: usize, mut on_conn: F) -> io::Result<usize>
    where
        F: FnMut(TcpStream, SocketAddr),
    {
        let mut count = 0usize;
        while count < max_batch {
            match raw_accept_one(self.fd) {
                Ok(Some((fd, storage))) => {
                    // 先に TcpStream を構築する: アドレス変換が失敗しても Drop 経由で
                    // fd がクローズされ、リークしない（`Accept::poll` と同じ順序）。
                    let stream = TcpStream { fd };
                    // F-164: AF_UNIX は sockaddr_un を SocketAddr へ変換できないため、
                    // プレースホルダを返す（IP ブロックリスト・アクセスログはこの値を見る）。
                    if self.is_unix {
                        on_conn(stream, SocketAddr::from(([127, 0, 0, 1], 0)));
                        count += 1;
                        continue;
                    }
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
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        unregister(self.fd);
        unsafe { libc::close(self.fd) };
    }
}

impl AsRawFd for TcpListener {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

// ====================
// Accept Future
// ====================

/// `accept4`（macOS では `accept`+`fcntl`+`set_so_nosigpipe` フォールバック）を
/// 1 回だけ非ブロッキングで試みる、`Accept::poll` と `TcpListener::accept_batch` の
/// 共通ヘルパ。アドレス変換（`storage_to_sockaddr`）は行わず、生の `RawFd` と
/// `sockaddr_storage` を返す（呼び出し側が変換失敗時の後始末方針をそれぞれ選べる
/// ようにするため。`Accept::poll` は即座にエラーを返す一方、`accept_batch` は
/// その 1 件のみ破棄してバックログの受理を続ける）。
///
/// - 成功: `Ok(Some((fd, storage)))`
/// - `EINTR`: 内部でリトライする（呼び出し側からは見えない）
/// - `EWOULDBLOCK`/`EAGAIN`: バックログが尽きたことを示す `Ok(None)`
/// - それ以外のエラー: `Err(e)`
fn raw_accept_one(listener_fd: RawFd) -> io::Result<Option<(RawFd, libc::sockaddr_storage)>> {
    loop {
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        // macOS には `accept4(2)` が無いため、`accept(2)` +
        // `fcntl(F_SETFL, O_NONBLOCK)` + `fcntl(F_SETFD, FD_CLOEXEC)` へフォールバック
        // する（設計 docs/artifacts/f125_windows_macos_design.md の macOS 節 1）。
        // 他 OS は従来どおり `accept4` 1 syscall で完結させる。
        #[cfg(target_os = "macos")]
        let fd = unsafe {
            libc::accept(
                listener_fd,
                &mut storage as *mut _ as *mut libc::sockaddr,
                &mut len,
            )
        };
        #[cfg(not(target_os = "macos"))]
        let fd = unsafe {
            libc::accept4(
                listener_fd,
                &mut storage as *mut _ as *mut libc::sockaddr,
                &mut len,
                libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            )
        };
        if fd >= 0 {
            #[cfg(target_os = "macos")]
            {
                unsafe {
                    libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                }
                set_so_nosigpipe(fd);
            }
            return Ok(Some((fd, storage)));
        }
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if is_would_block(&e) {
            return Ok(None);
        }
        return Err(e);
    }
}

/// accept Future（`accept4` の try-first ラッパ）。
pub struct Accept<'a> {
    listener_fd: RawFd,
    /// AF_UNIX リスナーかどうか（F-164）。`true` の場合、peer アドレスは
    /// `sockaddr_un` を持たないプレースホルダ `127.0.0.1:0` を返す。
    is_unix: bool,
    _marker: std::marker::PhantomData<&'a TcpListener>,
}

impl<'a> Future for Accept<'a> {
    type Output = io::Result<(TcpStream, SocketAddr)>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match raw_accept_one(self.listener_fd)? {
            Some((fd, storage)) => {
                // 先に TcpStream を構築する: アドレス変換が失敗しても Drop 経由で
                // fd がクローズされ、リークしない。
                let stream = TcpStream { fd };
                let peer_addr = if self.is_unix {
                    SocketAddr::from(([127, 0, 0, 1], 0))
                } else {
                    storage_to_sockaddr(&storage)?
                };
                Poll::Ready(Ok((stream, peer_addr)))
            }
            None => {
                register_read(self.listener_fd, cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

// ====================
// TcpStream
// ====================

/// 非同期 TCP ストリーム。
pub struct TcpStream {
    pub(crate) fd: RawFd,
}

impl TcpStream {
    /// raw fd から作成する。
    ///
    /// # Safety
    /// `fd` は有効な非ブロッキングソケット FD であること。
    pub unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Self { fd }
    }

    /// アドレスに非同期で接続する。
    pub fn connect(addr: SocketAddr) -> Connect {
        Connect {
            addr,
            fd: -1,
            registered: false,
        }
    }

    /// 文字列アドレス（"host:port"）から接続する。
    ///
    /// DNS 解決はブロッキングで行う（コールドパスのみ）。
    pub async fn connect_str(addr: &str) -> io::Result<TcpStream> {
        use std::net::ToSocketAddrs;
        let socket_addr = addr
            .to_socket_addrs()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address resolved"))?;
        TcpStream::connect(socket_addr).await
    }

    /// バッファに非同期で読み込む。バッファの所有権を取り、完了時に `(Result<usize>, T)` を返す。
    ///
    /// `veil_aio`（F-127、FreeBSD `--features aio`）有効時は POSIX AIO（`aio_read` +
    /// `EVFILT_AIO` 完了通知）経路を使う（`reactor::aio::AioReadFuture`）。無効時は
    /// 現行の readiness（try-first `read(2)`）経路のまま、無改変・ゼロコストで動作する。
    #[cfg(not(veil_aio))]
    pub fn read<T: IoBufMut>(&self, buf: T) -> ReadFuture<T> {
        ReadFuture {
            fd: self.fd,
            buf: Some(buf),
        }
    }

    /// バッファに非同期で読み込む（AIO 版。上の `veil_aio` 節参照）。
    #[cfg(veil_aio)]
    pub fn read<T: IoBufMut>(&self, buf: T) -> crate::runtime::reactor::aio::AioReadFuture<T> {
        crate::runtime::reactor::aio::AioReadFuture::new(self.fd, buf)
    }

    /// バッファを非同期で書き込む。バッファの所有権を取り、完了時に `(Result<usize>, T)` を返す。
    ///
    /// `veil_aio` 有効時は POSIX AIO（`aio_write` + `EVFILT_AIO`）経路を使う。
    #[cfg(not(veil_aio))]
    pub fn write<T: IoBuf>(&self, buf: T) -> WriteFuture<T> {
        WriteFuture {
            fd: self.fd,
            buf: Some(buf),
        }
    }

    /// バッファを非同期で書き込む（AIO 版。上の `veil_aio` 節参照）。
    #[cfg(veil_aio)]
    pub fn write<T: IoBuf>(&self, buf: T) -> crate::runtime::reactor::aio::AioWriteFuture<T> {
        crate::runtime::reactor::aio::AioWriteFuture::new(self.fd, buf)
    }

    /// 2 つの不連続バッファを 1 回の `sendmsg`（scatter-gather）で書き込む（F-59 互換）。
    pub fn writev2<A: IoBuf, B: IoBuf>(&self, a: A, b: B, skip: usize) -> SendMsgFuture<A, B> {
        SendMsgFuture {
            fd: self.fd,
            bufs: Some((a, b)),
            skip,
        }
    }

    /// 2 つの不連続バッファを全量書き込む。
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
                            "sendmsg returned zero",
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

    /// 読み取り可能になるまで待つ。
    pub fn readable(&self) -> Readable<'_> {
        Readable {
            fd: self.fd,
            _marker: std::marker::PhantomData,
        }
    }

    /// 書き込み可能になるまで待つ。
    pub fn writable(&self) -> Writable<'_> {
        Writable {
            fd: self.fd,
            _marker: std::marker::PhantomData,
        }
    }

    /// TCP_NODELAY を設定する。
    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        let optval: libc::c_int = if nodelay { 1 } else { 0 };
        let ret = unsafe {
            libc::setsockopt(
                self.fd,
                libc::IPPROTO_TCP,
                TCP_NODELAY,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// `TCP_NOPUSH` を設定する（F-155、FreeBSD 専用）。
    ///
    /// Linux の `TCP_CORK` に相当する FreeBSD のソケットオプション。有効化すると
    /// 小さいセグメントの即時送出を抑制し、可能な限り MSS 一杯までパケットを
    /// まとめてから送出する。無効化（クリア）した瞬間に溜まっていたデータが
    /// 即座にフラッシュされる。
    #[cfg(target_os = "freebsd")]
    pub fn set_nopush(&self, enable: bool) -> io::Result<()> {
        let optval: libc::c_int = if enable { 1 } else { 0 };
        let ret = unsafe {
            libc::setsockopt(
                self.fd,
                libc::IPPROTO_TCP,
                libc::TCP_NOPUSH,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// `TCP_NOPUSH` を有効化し、drop 時に必ず解除する RAII ガードを返す（F-155、
    /// FreeBSD 専用）。
    ///
    /// 有効化（`setsockopt` 呼び出し）に失敗した場合は `None` を返す
    /// （ホットパス上のエラーとして扱わず、呼び出し側はガード無しで従来通り送信を
    /// 続けてよい）。
    ///
    /// 返るガードは `&self` のライフタイムに縛られる。ガードの `Drop` は fd へ
    /// `setsockopt` するため、`TcpStream` が先に drop（fd が close）されると
    /// 別用途に再利用された fd を触りうる。借用で縛ることでこれをコンパイル時に防ぐ。
    #[cfg(target_os = "freebsd")]
    pub fn nopush_guard(&self) -> Option<NoPushGuard<'_>> {
        self.set_nopush(true).ok()?;
        Some(NoPushGuard {
            fd: self.fd,
            _marker: std::marker::PhantomData,
        })
    }

    /// ピアアドレスを取得する。
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let ret = unsafe {
            libc::getpeername(
                self.fd,
                &mut storage as *mut _ as *mut libc::sockaddr,
                &mut len,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        storage_to_sockaddr(&storage)
    }

    /// ローカルアドレスを取得する。
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let ret = unsafe {
            libc::getsockname(
                self.fd,
                &mut storage as *mut _ as *mut libc::sockaddr,
                &mut len,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        storage_to_sockaddr(&storage)
    }

    /// ソケットをシャットダウンする。
    pub fn shutdown(&self, how: std::net::Shutdown) -> io::Result<()> {
        let how = match how {
            std::net::Shutdown::Read => libc::SHUT_RD,
            std::net::Shutdown::Write => libc::SHUT_WR,
            std::net::Shutdown::Both => libc::SHUT_RDWR,
        };
        let ret = unsafe { libc::shutdown(self.fd, how) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

// ====================
// AsyncReadRent / AsyncWriteRent 実装
// ====================

impl crate::runtime::io::AsyncReadRent for TcpStream {
    #[cfg(not(veil_aio))]
    fn read<T: crate::runtime::buf::IoBufMut>(
        &mut self,
        buf: T,
    ) -> impl std::future::Future<Output = crate::runtime::io::BufResult<usize, T>> {
        let fd = self.fd;
        ReadFuture { fd, buf: Some(buf) }
    }

    #[cfg(veil_aio)]
    fn read<T: crate::runtime::buf::IoBufMut>(
        &mut self,
        buf: T,
    ) -> impl std::future::Future<Output = crate::runtime::io::BufResult<usize, T>> {
        crate::runtime::reactor::aio::AioReadFuture::new(self.fd, buf)
    }
}

impl crate::runtime::io::AsyncWriteRent for TcpStream {
    #[cfg(not(veil_aio))]
    fn write<T: crate::runtime::buf::IoBuf>(
        &mut self,
        buf: T,
    ) -> impl std::future::Future<Output = crate::runtime::io::BufResult<usize, T>> {
        let fd = self.fd;
        WriteFuture { fd, buf: Some(buf) }
    }

    #[cfg(veil_aio)]
    fn write<T: crate::runtime::buf::IoBuf>(
        &mut self,
        buf: T,
    ) -> impl std::future::Future<Output = crate::runtime::io::BufResult<usize, T>> {
        crate::runtime::reactor::aio::AioWriteFuture::new(self.fd, buf)
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
            unsafe { libc::close(self.fd) };
        }
    }
}

impl AsRawFd for TcpStream {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

/// `TCP_NOPUSH` の RAII ガード（F-155、FreeBSD 専用）。
///
/// `TcpStream::nopush_guard` が返す。保持している間は `TCP_NOPUSH` が有効
/// （コルク状態）であり、`Drop` で必ず `TCP_NOPUSH` を解除する（`setsockopt` の
/// 失敗は無視する。解除自体が失敗しても後続の通信を止めるべきではないため）。
///
/// **解除漏れ防止**: 呼び出し側が明示的に解除を呼び忘れると、最後の小さな
/// セグメントがカーネル内に溜まったまま送出されない「Nagle の罠」に類する問題に
/// 直結する。本ガードは `Drop` で解除するため、正常終了・早期 `return`・
/// エラー伝播・（async 関数内であれば）Future の中断のいずれの経路でも、
/// スコープを抜ける際に必ず `TCP_NOPUSH` がクリアされる。
///
/// **panic 安全性**: `Drop::drop` は unwind 中にも呼ばれるため、途中で panic が
/// 発生してスタック巻き戻しが起きた場合でも `TCP_NOPUSH` は解除される。
///
/// ライフタイム `'a` は生成元の `TcpStream` の借用であり、fd が close された後に
/// `Drop` が走ることをコンパイル時に防ぐ（`TcpStream::nopush_guard` の doc 参照）。
#[cfg(target_os = "freebsd")]
pub struct NoPushGuard<'a> {
    fd: RawFd,
    _marker: std::marker::PhantomData<&'a TcpStream>,
}

#[cfg(target_os = "freebsd")]
impl Drop for NoPushGuard<'_> {
    fn drop(&mut self) {
        let optval: libc::c_int = 0;
        // SAFETY: fd は本ガードの生成元 TcpStream が close するまで有効であることを
        // 呼び出し側（TcpStream::nopush_guard）が保証する（ガードは TcpStream より
        // 長生きしない使い方を前提とする）。setsockopt の失敗は意図的に無視する
        // （doc 参照: 解除失敗で後続処理を止めない）。
        unsafe {
            libc::setsockopt(
                self.fd,
                libc::IPPROTO_TCP,
                libc::TCP_NOPUSH,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
}

// ====================
// Connect Future
// ====================

/// connect Future（非ブロッキング `connect(2)` → writable 待ち → `SO_ERROR` 確認）。
pub struct Connect {
    addr: SocketAddr,
    fd: RawFd,
    registered: bool,
}

impl Connect {
    /// 接続失敗時の後始末: 登録済みなら FdTable から除去してから close する。
    ///
    /// unregister を省くと閉じた fd の FdRecord（known_to_kernel/armed）が残り、
    /// OS が同じ fd 番号を再利用した際に stale な状態を引き継いでしまう。
    fn fail(&mut self, e: io::Error) -> io::Error {
        let fd = self.fd;
        self.fd = -1;
        if self.registered {
            unregister(fd);
            self.registered = false;
        }
        unsafe { libc::close(fd) };
        e
    }
}

impl Future for Connect {
    type Output = io::Result<TcpStream>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.fd < 0 {
            let domain = if self.addr.is_ipv6() {
                libc::AF_INET6
            } else {
                libc::AF_INET
            };
            let fd = match create_nonblocking_socket(domain) {
                Ok(fd) => fd,
                Err(e) => return Poll::Ready(Err(e)),
            };
            self.fd = fd;

            let (storage, len) = sockaddr_to_storage(&self.addr);
            let ret =
                unsafe { libc::connect(fd, &storage as *const _ as *const libc::sockaddr, len) };
            if ret == 0 {
                // 即座に接続完了（ローカルソケット等）。
                let fd = self.fd;
                self.fd = -1;
                return Poll::Ready(Ok(TcpStream { fd }));
            }
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINPROGRESS) {
                unsafe { libc::close(fd) };
                self.fd = -1;
                return Poll::Ready(Err(e));
            }
        }

        // 接続完了（POLLOUT/エラー）を poll(2) で確認してから SO_ERROR を読む。
        //
        // 「一度 register 済みなら起床＝完了」と見なしてはならない: タスクの起床は
        // この fd の writable イベント以外でも起こる（例: `timeout(CONNECT_TIMEOUT,
        // connect)` は select 系のためタイマー起床時にも内側の Connect を再 poll する）。
        // 接続未完了のソケットは SO_ERROR が 0 を返すため、readiness を確認せずに
        // SO_ERROR だけ見ると「未接続ソケットを接続成功として返す」誤判定になる。
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
        if ret < 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Poll::Ready(Err(self.fail(e)));
            }
            // EINTR: readiness 未確定として待機継続する。
        }
        if ret <= 0 || pfd.revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) == 0 {
            // 接続未完了: writable 待ちを（再）登録して待機する。
            register_write(self.fd, cx.waker().clone());
            self.registered = true;
            return Poll::Pending;
        }

        // 接続完了（または失敗）: SO_ERROR で接続結果を確認する。
        let mut err: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let ret = unsafe {
            libc::getsockopt(
                self.fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &mut err as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if ret < 0 {
            let e = io::Error::last_os_error();
            return Poll::Ready(Err(self.fail(e)));
        }
        if err != 0 {
            let e = io::Error::from_raw_os_error(err);
            return Poll::Ready(Err(self.fail(e)));
        }

        let fd = self.fd;
        self.fd = -1;
        Poll::Ready(Ok(TcpStream { fd }))
    }
}

impl Drop for Connect {
    fn drop(&mut self) {
        if self.fd >= 0 {
            if self.registered {
                unregister(self.fd);
            }
            unsafe { libc::close(self.fd) };
        }
    }
}

// ====================
// Read Future
// ====================

/// 読み込み Future（非ブロッキング `read(2)` の try-first ラッパ）。
pub struct ReadFuture<T: IoBufMut> {
    fd: RawFd,
    buf: Option<T>,
}

impl<T: IoBufMut> Future for ReadFuture<T> {
    type Output = (io::Result<usize>, T);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        loop {
            let buf = this
                .buf
                .as_mut()
                .expect("ReadFuture polled after completion");
            let ret = unsafe {
                libc::read(
                    this.fd,
                    buf.write_ptr() as *mut libc::c_void,
                    buf.bytes_total(),
                )
            };
            if ret >= 0 {
                // SAFETY: カーネルが ret バイトを初期化した。
                unsafe { buf.set_init(ret as usize) };
                let buf = this.buf.take().expect("buffer present at completion");
                return Poll::Ready((Ok(ret as usize), buf));
            }
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if is_would_block(&e) {
                register_read(this.fd, cx.waker().clone());
                return Poll::Pending;
            }
            let buf = this.buf.take().expect("buffer present on error");
            return Poll::Ready((Err(e), buf));
        }
    }
}

// ====================
// Write Future
// ====================

/// 書き込み Future（非ブロッキング `write(2)` の try-first ラッパ）。
pub struct WriteFuture<T: IoBuf> {
    fd: RawFd,
    buf: Option<T>,
}

impl<T: IoBuf> Future for WriteFuture<T> {
    type Output = (io::Result<usize>, T);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        loop {
            let buf = this
                .buf
                .as_ref()
                .expect("WriteFuture polled after completion");
            // macOS には `MSG_NOSIGNAL` が無い（SIGPIPE 抑止は生成時の `SO_NOSIGPIPE` で
            // 代替済み）ため send フラグを 0 にする。他 OS は従来どおり。
            #[cfg(target_os = "macos")]
            const SEND_FLAGS: libc::c_int = 0;
            #[cfg(not(target_os = "macos"))]
            const SEND_FLAGS: libc::c_int = libc::MSG_NOSIGNAL;
            let ret = unsafe {
                libc::send(
                    this.fd,
                    buf.read_ptr() as *const libc::c_void,
                    buf.bytes_init(),
                    SEND_FLAGS,
                )
            };
            if ret >= 0 {
                let buf = this.buf.take().expect("buffer present at completion");
                return Poll::Ready((Ok(ret as usize), buf));
            }
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if is_would_block(&e) {
                register_write(this.fd, cx.waker().clone());
                return Poll::Pending;
            }
            let buf = this.buf.take().expect("buffer present on error");
            return Poll::Ready((Err(e), buf));
        }
    }
}

// ====================
// SendMsg (scatter-gather) Future（F-59 互換）
// ====================

/// scatter-gather 書き込み Future（`sendmsg(2)` の try-first ラッパ）。
pub struct SendMsgFuture<A: IoBuf, B: IoBuf> {
    fd: RawFd,
    bufs: Option<(A, B)>,
    skip: usize,
}

impl<A: IoBuf, B: IoBuf> Future for SendMsgFuture<A, B> {
    type Output = (io::Result<usize>, A, B);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        loop {
            let (a, b) = this
                .bufs
                .as_ref()
                .expect("SendMsgFuture polled after completion");
            let (a_ptr, a_len) = (a.read_ptr(), a.bytes_init());
            let (b_ptr, b_len) = (b.read_ptr(), b.bytes_init());
            let skip = this.skip;
            debug_assert!(skip < a_len + b_len);

            let mut iovecs = [libc::iovec {
                iov_base: std::ptr::null_mut(),
                iov_len: 0,
            }; 2];
            let mut iov_count = 0usize;
            if skip < a_len {
                iovecs[iov_count] = libc::iovec {
                    iov_base: unsafe { a_ptr.add(skip) } as *mut libc::c_void,
                    iov_len: a_len - skip,
                };
                iov_count += 1;
                if b_len > 0 {
                    iovecs[iov_count] = libc::iovec {
                        iov_base: b_ptr as *mut libc::c_void,
                        iov_len: b_len,
                    };
                    iov_count += 1;
                }
            } else {
                let b_skip = skip - a_len;
                iovecs[iov_count] = libc::iovec {
                    iov_base: unsafe { b_ptr.add(b_skip) } as *mut libc::c_void,
                    iov_len: b_len - b_skip,
                };
                iov_count += 1;
            }

            let mut msghdr: libc::msghdr = unsafe { std::mem::zeroed() };
            msghdr.msg_iov = iovecs.as_mut_ptr();
            msghdr.msg_iovlen = iov_count as _;

            // macOS には `MSG_NOSIGNAL` が無いため send フラグを 0 にする（`WriteFuture` と
            // 同じ理由。生成時の `SO_NOSIGPIPE` で SIGPIPE 抑止済み）。
            #[cfg(target_os = "macos")]
            const SENDMSG_FLAGS: libc::c_int = 0;
            #[cfg(not(target_os = "macos"))]
            const SENDMSG_FLAGS: libc::c_int = libc::MSG_NOSIGNAL;
            let ret = unsafe { libc::sendmsg(this.fd, &msghdr, SENDMSG_FLAGS) };
            if ret >= 0 {
                let (a, b) = this.bufs.take().expect("buffers present at completion");
                return Poll::Ready((Ok(ret as usize), a, b));
            }
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if is_would_block(&e) {
                register_write(this.fd, cx.waker().clone());
                return Poll::Pending;
            }
            let (a, b) = this.bufs.take().expect("buffers present on error");
            return Poll::Ready((Err(e), a, b));
        }
    }
}

// ====================
// Readable / Writable Future
// ====================

/// 読み取り可能まで待つ Future。
pub struct Readable<'a> {
    fd: RawFd,
    _marker: std::marker::PhantomData<&'a TcpStream>,
}

impl<'a> Future for Readable<'a> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // F-141: kqueue バックエンドでは、直前の `EVFILT_READ` 起床がこの fd を
        // readable と報告済みなら、確認用の `poll(2)` syscall を省略する
        // （`executor::take_read_hint` の doc 参照。consume-once のため、無関係な
        // 後続呼び出しに古いヒントが漏れることはない）。
        #[cfg(veil_poller_kqueue)]
        if crate::runtime::executor::take_read_hint(self.fd) > 0 {
            return Poll::Ready(Ok(()));
        }
        // POLLIN/EPOLLIN 相当を即座に確認するため 0 バイト peek は行わず、まず fd の
        // readiness を epoll に問い合わせる（poll(2) を使い syscall 1 発で判定する）。
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
        if ret > 0 && pfd.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
            return Poll::Ready(Ok(()));
        }
        register_read(self.fd, cx.waker().clone());
        Poll::Pending
    }
}

/// 書き込み可能まで待つ Future。
pub struct Writable<'a> {
    fd: RawFd,
    _marker: std::marker::PhantomData<&'a TcpStream>,
}

impl<'a> Future for Writable<'a> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // F-155: kqueue バックエンドでは、直前の `EVFILT_WRITE` 起床がこの fd を
        // writable と報告済みなら、確認用の `poll(2)` syscall を省略する
        // （`executor::take_write_hint` の doc 参照。consume-once のため、無関係な
        // 後続呼び出しに古いヒントが漏れることはない）。
        #[cfg(veil_poller_kqueue)]
        if crate::runtime::executor::take_write_hint(self.fd) > 0 {
            return Poll::Ready(Ok(()));
        }
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
        if ret > 0 && pfd.revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) != 0 {
            return Poll::Ready(Ok(()));
        }
        register_write(self.fd, cx.waker().clone());
        Poll::Pending
    }
}

// ====================
// 汎用 FD 待機 Future（UDP 等、任意の FD に使用）
// ====================

/// 任意の FD が読み込み可能になるまで待つ Future。
pub struct ReadableFd {
    fd: RawFd,
}

impl Future for ReadableFd {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // F-141: `Readable::poll` と同じ理由で、kqueue の直近ヒントがあれば
        // 確認用 `poll(2)` syscall を省略する（UDP の `wait_readable_fd` 経路で使われる
        // ため、`QuicUdpSocket` の recv 系ループがこの恩恵を受ける）。
        #[cfg(veil_poller_kqueue)]
        if crate::runtime::executor::take_read_hint(self.fd) > 0 {
            return Poll::Ready(Ok(()));
        }
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
        if ret > 0 && pfd.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
            return Poll::Ready(Ok(()));
        }
        register_read(self.fd, cx.waker().clone());
        Poll::Pending
    }
}

/// 任意の FD が書き込み可能になるまで待つ Future。
pub struct WritableFd {
    fd: RawFd,
}

impl Future for WritableFd {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // F-155: `Writable::poll` と同じ理由で、kqueue の直近ヒントがあれば
        // 確認用 `poll(2)` syscall を省略する。
        #[cfg(veil_poller_kqueue)]
        if crate::runtime::executor::take_write_hint(self.fd) > 0 {
            return Poll::Ready(Ok(()));
        }
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
        if ret > 0 && pfd.revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) != 0 {
            return Poll::Ready(Ok(()));
        }
        register_write(self.fd, cx.waker().clone());
        Poll::Pending
    }
}

/// 任意の FD が読み込み可能になるまで待つ。
pub fn wait_readable_fd(fd: RawFd) -> ReadableFd {
    ReadableFd { fd }
}

/// 任意の FD が書き込み可能になるまで待つ。
pub fn wait_writable_fd(fd: RawFd) -> WritableFd {
    WritableFd { fd }
}

// ====================
// テスト
// ====================

#[cfg(test)]
mod tests {
    use super::*;

    /// F-164: AF_UNIX リスナー fd から `TcpListener` を作り、accept が成功すること
    /// （peer_addr はプレースホルダ `127.0.0.1:0` を返す）。reactor（epoll/kqueue）
    /// バックエンドでの `Accept::poll` の `is_unix` 分岐を検証する
    /// （`uring::tcp` 側の同名テストと対になる。F-145 の「reactor はテストの
    /// 空白地帯」の教訓により、両バックエンドに同じテストを置く）。
    #[test]
    // 理由付き allow: テストのソケットパス後始末（起動/イベントループ外のテストコード）。
    #[allow(clippy::disallowed_methods)]
    fn test_unix_listener_accept_placeholder_peer_addr() {
        let mut path = std::env::temp_dir();
        path.push(format!("veil-f164-reactor-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let listener = std::os::unix::net::UnixListener::bind(&path).expect("unix bind");
        listener.set_nonblocking(true).expect("set_nonblocking");
        let fd = listener.as_raw_fd();
        // TcpListener が fd の所有権を持つため、std 側の Drop による二重 close を
        // 避ける（所有権を明示的に移す）。
        std::mem::forget(listener);
        let uds_listener = unsafe { TcpListener::from_raw_fd_unix(fd) };

        let connect_path = path.clone();
        let client = std::thread::spawn(move || {
            std::os::unix::net::UnixStream::connect(&connect_path).expect("client connect")
        });

        let peer_addr = crate::runtime::block_on(async move {
            let (_stream, peer_addr) = uds_listener.accept().await.expect("accept");
            peer_addr
        });

        client.join().expect("client thread join");
        let _ = std::fs::remove_file(&path);

        assert_eq!(peer_addr, SocketAddr::from(([127, 0, 0, 1], 0)));
    }
}
