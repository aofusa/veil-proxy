//! FreeBSD capsicum の capability mode 下の上流接続ブローカー（F-182）
//!
//! capability mode（`cap_enter(2)`）では `connect(2)` と名前解決ができない。`cap_enter` の前に
//! 自身を再実行してサンドボックス外の **接続ブローカー** を起動し、起動時に確定した許可リストの
//! 上流にだけ非ブロッキング `connect(2)` を開始させ、そのソケットを `SCM_RIGHTS` で受け取る。
//! 接続完了（writable + `SO_ERROR`）の待機と、以後の TLS・HTTP は本体が従来どおり行う。
//!
//! - 本体からブローカーへ渡すのは許可リストの **添字だけ**（アドレス・ホスト名・パスの文字列は
//!   受け付けない）。本体が乗っ取られても、設定に書かれた上流以外へは接続できない。
//! - 要求ごとに返信用の socketpair を作り、その片端を要求に添えて送る。共有の制御ソケットを全スレッドで
//!   使っても応答を取り違えず、reactor の fd ごとの待機者も衝突しない。
//! - 代行のコストは新規の上流接続のときだけ（上流は接続プール・多重化で再利用する）。
//!
//! プロトコル・サーバのループ・クライアントは unix 共通のコードで、Linux の単体テストでも検証する。
//! ブローカーの起動・プロセスの防御・本体側のグローバル状態は FreeBSD 専用。

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// ブローカーとして起動するための隠し引数（`run()` の先頭で判定する）。
#[cfg(target_os = "freebsd")]
pub const BROKER_ARG: &str = "__veil_connect_broker";

/// 要求メッセージの magic（"VECB"）。
const REQ_MAGIC: u32 = 0x4243_4556;
/// 初期化メッセージ（許可リストの先頭）。
const HELLO: &[u8] = b"VEILCB1";
/// 初期化メッセージ（許可リストの終端）。
const END: &[u8] = b"END";
/// 初期化完了の応答。
const READY: &[u8] = b"READY";
/// 許可リスト 1 エントリの上限（種別 1 バイト + 文字列）。
const MAX_ENTRY_LEN: usize = 4096;
/// 許可リストの件数の上限。
const MAX_ENTRIES: u32 = 65_536;
/// ホスト名の再解決の間隔（`runtime::dns::CACHE_TTL` と同じ）。
const RESOLVE_REFRESH: Duration = Duration::from_secs(30);
/// 未解決のホスト名を再試行する間隔。
const RESOLVE_RETRY: Duration = Duration::from_secs(1);

/// 許可リストの 1 エントリ（接続先）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    /// IP リテラル
    Addr(SocketAddr),
    /// ホスト名（`"host:port"`。ブローカーが解決する）
    Host(String),
    /// UDS パス
    Unix(PathBuf),
}

impl Target {
    /// `"host:port"` を IP リテラルならそのまま、ホスト名なら `Host` にする。
    pub fn from_host_port(s: &str) -> Target {
        match s.parse::<SocketAddr>() {
            Ok(a) => Target::Addr(a),
            Err(_) => Target::Host(s.to_string()),
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut v = Vec::new();
        match self {
            Target::Addr(a) => {
                v.push(1);
                v.extend_from_slice(a.to_string().as_bytes());
            }
            Target::Host(h) => {
                v.push(2);
                v.extend_from_slice(h.as_bytes());
            }
            Target::Unix(p) => {
                v.push(3);
                v.extend_from_slice(p.as_os_str().as_bytes());
            }
        }
        v
    }

    fn decode(buf: &[u8]) -> Option<Target> {
        let (&kind, rest) = buf.split_first()?;
        if rest.is_empty() || rest.contains(&0) {
            return None;
        }
        match kind {
            1 => std::str::from_utf8(rest)
                .ok()?
                .parse()
                .ok()
                .map(Target::Addr),
            2 => {
                let s = std::str::from_utf8(rest).ok()?;
                let (host, port) = s.rsplit_once(':')?;
                if host.is_empty() || port.parse::<u16>().is_err() {
                    return None;
                }
                Some(Target::Host(s.to_string()))
            }
            3 => Some(Target::Unix(PathBuf::from(std::ffi::OsStr::from_bytes(
                rest,
            )))),
            _ => None,
        }
    }
}

/// 許可リスト（本体側）。添字はブローカーへ送った順番。
#[derive(Default)]
pub struct Allowlist {
    entries: Vec<Target>,
    by_addr: HashMap<SocketAddr, u32>,
    by_host: HashMap<String, u32>,
    by_unix: HashMap<PathBuf, u32>,
}

impl Allowlist {
    pub fn new() -> Self {
        Self::default()
    }

    /// 1 エントリを足す（重複は無視）。
    pub fn insert(&mut self, t: Target) {
        let idx = self.entries.len() as u32;
        let added = match &t {
            Target::Addr(a) => self.by_addr.try_insert_compat(*a, idx),
            Target::Host(h) => self.by_host.try_insert_compat(h.clone(), idx),
            Target::Unix(p) => self.by_unix.try_insert_compat(p.clone(), idx),
        };
        if added {
            self.entries.push(t);
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn index_of_addr(&self, a: &SocketAddr) -> Option<u32> {
        self.by_addr.get(a).copied()
    }

    pub fn index_of_host(&self, host_port: &str) -> Option<u32> {
        self.by_host.get(host_port).copied()
    }

    pub fn index_of_unix(&self, p: &Path) -> Option<u32> {
        self.by_unix.get(p).copied()
    }

    /// `"host:port"`（IP リテラル可）または `"unix:<path>"` の表記で引く。
    pub fn index_of_conn_addr(&self, addr: &str) -> Option<u32> {
        if let Some(p) = addr.strip_prefix("unix:") {
            return self.index_of_unix(Path::new(p));
        }
        match addr.parse::<SocketAddr>() {
            Ok(a) => self.index_of_addr(&a),
            Err(_) => self.index_of_host(addr),
        }
    }
}

/// `HashMap::try_insert` の安定版代替（既にあれば何もしない）。
trait TryInsertCompat<K, V> {
    fn try_insert_compat(&mut self, k: K, v: V) -> bool;
}

impl<K: std::hash::Hash + Eq, V> TryInsertCompat<K, V> for HashMap<K, V> {
    fn try_insert_compat(&mut self, k: K, v: V) -> bool {
        match self.entry(k) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(v);
                true
            }
        }
    }
}

/// 許可リストに無い宛先への接続要求のエラー。
#[cfg(target_os = "freebsd")]
pub fn not_allowed() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "upstream is not in the capability-mode connect allowlist (fixed at startup)",
    )
}

// ====================
// SCM_RIGHTS つきの送受信
// ====================

/// 制御メッセージ用のバッファ（`cmsghdr` の整列を満たすため u64 で確保する）。
#[repr(C)]
struct CmsgBuf([u64; 8]);

/// `data` と（あれば）`fd` 1 本を送る。
fn send_with_fd(sock: RawFd, data: &[u8], fd: Option<RawFd>, flags: libc::c_int) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    let mut cbuf = CmsgBuf([0; 8]);
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if let Some(fd) = fd {
        // SAFETY: cbuf は CMSG_SPACE(sizeof(int)) より大きく、cmsghdr の整列（u64）を満たす。
        // CMSG_FIRSTHDR は msg_control/msg_controllen を設定した後に呼ぶ。
        unsafe {
            let space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
            debug_assert!(space <= std::mem::size_of::<CmsgBuf>());
            msg.msg_control = cbuf.0.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = space as _;
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(c) as *mut RawFd, fd);
        }
    }
    loop {
        let n = unsafe { libc::sendmsg(sock, &msg, flags | libc::MSG_NOSIGNAL) };
        if n >= 0 {
            if n as usize != data.len() {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "short sendmsg"));
            }
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// 受信した fd（最大 1 本）。
struct Received {
    len: usize,
    fd: Option<RawFd>,
}

/// 1 メッセージを受け取る。fd は最大 1 本まで受け付け、それ以外（2 本以上・SCM_RIGHTS 以外の
/// 制御メッセージ・`MSG_CTRUNC`・`MSG_TRUNC`）は受け取った fd をすべて閉じて `InvalidData`。
fn recv_with_fd(sock: RawFd, buf: &mut [u8], flags: libc::c_int) -> io::Result<Received> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let mut cbuf = CmsgBuf([0; 8]);
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.0.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = std::mem::size_of::<CmsgBuf>() as _;
    let n = loop {
        let n = unsafe { libc::recvmsg(sock, &mut msg, flags | libc::MSG_CMSG_CLOEXEC) };
        if n >= 0 {
            break n as usize;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    };
    // 受け取った fd をすべて集める（検査で弾く場合も閉じ漏らさない）。
    let mut fds = [-1 as RawFd; 8];
    let mut nfds = 0usize;
    let mut bad = (msg.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC)) != 0;
    // SAFETY: msg は recvmsg が埋めた制御メッセージ領域を指す。CMSG_* はその範囲内だけを辿る。
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c) as *const RawFd;
                let hdr = libc::CMSG_LEN(0) as usize;
                let count =
                    ((*c).cmsg_len as usize).saturating_sub(hdr) / std::mem::size_of::<RawFd>();
                for i in 0..count {
                    let fd = std::ptr::read_unaligned(data.add(i));
                    if nfds < fds.len() {
                        fds[nfds] = fd;
                        nfds += 1;
                    } else {
                        libc::close(fd);
                        bad = true;
                    }
                }
            } else {
                bad = true;
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    if bad || nfds > 1 {
        for &fd in &fds[..nfds] {
            unsafe { libc::close(fd) };
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected control message",
        ));
    }
    Ok(Received {
        len: n,
        fd: (nfds == 1).then_some(fds[0]),
    })
}

/// `SOCK_SEQPACKET` の AF_UNIX socketpair（CLOEXEC）。
fn seqpacket_pair(nonblocking: bool) -> io::Result<(RawFd, RawFd)> {
    let mut sv = [-1 as RawFd; 2];
    let mut ty = libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC;
    if nonblocking {
        ty |= libc::SOCK_NONBLOCK;
    }
    if unsafe { libc::socketpair(libc::AF_UNIX, ty, 0, sv.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((sv[0], sv[1]))
}

/// `fd` が AF_UNIX のソケットか。
fn is_unix_socket(fd: RawFd) -> bool {
    let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let r = unsafe { libc::getsockname(fd, &mut ss as *mut _ as *mut libc::sockaddr, &mut len) };
    r == 0 && ss.ss_family as libc::c_int == libc::AF_UNIX
}

// ====================
// 初期化（許可リストの受け渡し）
// ====================

/// 本体 → ブローカー: 許可リストを送る。
fn send_allowlist(ctrl: RawFd, allow: &Allowlist) -> io::Result<()> {
    let mut hello = Vec::with_capacity(HELLO.len() + 4);
    hello.extend_from_slice(HELLO);
    hello.extend_from_slice(&(allow.entries.len() as u32).to_le_bytes());
    send_with_fd(ctrl, &hello, None, 0)?;
    for t in &allow.entries {
        let m = t.encode();
        if m.len() > MAX_ENTRY_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "connect allowlist entry too long",
            ));
        }
        send_with_fd(ctrl, &m, None, 0)?;
    }
    send_with_fd(ctrl, END, None, 0)
}

/// ブローカー側: 許可リストを受け取る。
fn recv_allowlist(ctrl: RawFd) -> io::Result<Vec<Target>> {
    let bad = |what: &str| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("broker setup: {}", what),
        )
    };
    let mut buf = vec![0u8; MAX_ENTRY_LEN + 1];
    let r = recv_with_fd(ctrl, &mut buf, 0)?;
    if let Some(fd) = r.fd {
        unsafe { libc::close(fd) };
        return Err(bad("unexpected fd"));
    }
    if r.len != HELLO.len() + 4 || &buf[..HELLO.len()] != HELLO {
        return Err(bad("bad hello"));
    }
    let mut cnt = [0u8; 4];
    cnt.copy_from_slice(&buf[HELLO.len()..HELLO.len() + 4]);
    let count = u32::from_le_bytes(cnt);
    if count > MAX_ENTRIES {
        return Err(bad("too many entries"));
    }
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let r = recv_with_fd(ctrl, &mut buf, 0)?;
        if let Some(fd) = r.fd {
            unsafe { libc::close(fd) };
            return Err(bad("unexpected fd"));
        }
        if r.len == 0 || r.len > MAX_ENTRY_LEN {
            return Err(bad("bad entry length"));
        }
        out.push(Target::decode(&buf[..r.len]).ok_or_else(|| bad("bad entry"))?);
    }
    let r = recv_with_fd(ctrl, &mut buf, 0)?;
    if r.fd.is_some() || &buf[..r.len] != END {
        if let Some(fd) = r.fd {
            unsafe { libc::close(fd) };
        }
        return Err(bad("missing end"));
    }
    Ok(out)
}

// ====================
// ブローカー（サーバ）
// ====================

/// 接続先（解決済みの形）。
enum Slot {
    Addr(SocketAddr),
    Host {
        name: String,
        addr: RwLock<Option<SocketAddr>>,
    },
    Unix {
        addr: libc::sockaddr_un,
        len: libc::socklen_t,
    },
}

/// ブローカーが持つ接続先の表（添字 = 許可リストの添字）。
pub(crate) struct Table {
    slots: Vec<Slot>,
}

impl Table {
    fn new(targets: Vec<Target>) -> io::Result<Table> {
        let mut slots = Vec::with_capacity(targets.len());
        for t in targets {
            slots.push(match t {
                Target::Addr(a) => Slot::Addr(a),
                Target::Host(h) => Slot::Host {
                    name: h,
                    addr: RwLock::new(None),
                },
                Target::Unix(p) => {
                    let (addr, len) = sockaddr_un(&p)?;
                    Slot::Unix { addr, len }
                }
            });
        }
        Ok(Table { slots })
    }

    /// ホスト名を解決する（`only_unresolved` なら未解決のものだけ）。
    // 理由付き allow: ブローカープロセスの解決スレッド（veil のデータプレーンの外）での同期 DNS。
    #[allow(clippy::disallowed_methods)]
    fn resolve_hosts(&self, only_unresolved: bool) {
        use std::net::ToSocketAddrs;
        for s in &self.slots {
            if let Slot::Host { name, addr } = s {
                if only_unresolved && addr.read().map(|a| a.is_some()).unwrap_or(false) {
                    continue;
                }
                match name.to_socket_addrs().map(|mut it| it.next()) {
                    Ok(Some(a)) => {
                        if let Ok(mut w) = addr.write() {
                            *w = Some(a);
                        }
                    }
                    // 解決できなかったら前の結果を保つ（一時的な DNS 障害で接続を止めない）。
                    _ => eprintln!("veil connect-broker: failed to resolve {}", name),
                }
            }
        }
    }

    fn has_hosts(&self) -> bool {
        self.slots.iter().any(|s| matches!(s, Slot::Host { .. }))
    }

    /// 添字の接続先へ非ブロッキング connect を開始し、接続中のソケットを返す。
    fn connect(&self, idx: u32) -> Result<RawFd, i32> {
        let slot = self.slots.get(idx as usize).ok_or(libc::EINVAL)?;
        let (domain, ss, len) = match slot {
            Slot::Addr(a) => sockaddr_in(a),
            Slot::Host { addr, .. } => {
                let a = addr
                    .read()
                    .ok()
                    .and_then(|a| *a)
                    .ok_or(libc::EHOSTUNREACH)?;
                sockaddr_in(&a)
            }
            Slot::Unix { addr, len } => {
                let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
                // SAFETY: sockaddr_un は sockaddr_storage に収まる。
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        addr as *const libc::sockaddr_un as *const u8,
                        &mut ss as *mut libc::sockaddr_storage as *mut u8,
                        std::mem::size_of::<libc::sockaddr_un>(),
                    )
                };
                (libc::AF_UNIX, ss, *len)
            }
        };
        let fd = unsafe {
            libc::socket(
                domain,
                libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
            )
        };
        if fd < 0 {
            return Err(errno());
        }
        let r = unsafe { libc::connect(fd, &ss as *const _ as *const libc::sockaddr, len) };
        if r == 0 {
            return Ok(fd);
        }
        let e = errno();
        if e == libc::EINPROGRESS {
            return Ok(fd);
        }
        unsafe { libc::close(fd) };
        Err(e)
    }
}

fn errno() -> i32 {
    io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

fn sockaddr_in(a: &SocketAddr) -> (libc::c_int, libc::sockaddr_storage, libc::socklen_t) {
    let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    match a {
        SocketAddr::V4(v4) => {
            let sin = libc::sockaddr_in {
                #[cfg(any(target_os = "freebsd", target_os = "macos"))]
                sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: v4.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(v4.ip().octets()),
                },
                sin_zero: [0; 8],
            };
            // SAFETY: sockaddr_in は sockaddr_storage に収まる。
            unsafe { std::ptr::write(&mut ss as *mut _ as *mut libc::sockaddr_in, sin) };
            (
                libc::AF_INET,
                ss,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        }
        SocketAddr::V6(v6) => {
            let mut sin6: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            #[cfg(any(target_os = "freebsd", target_os = "macos"))]
            {
                sin6.sin6_len = std::mem::size_of::<libc::sockaddr_in6>() as u8;
            }
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_port = v6.port().to_be();
            sin6.sin6_flowinfo = v6.flowinfo();
            sin6.sin6_addr.s6_addr = v6.ip().octets();
            sin6.sin6_scope_id = v6.scope_id();
            // SAFETY: sockaddr_in6 は sockaddr_storage に収まる。
            unsafe { std::ptr::write(&mut ss as *mut _ as *mut libc::sockaddr_in6, sin6) };
            (
                libc::AF_INET6,
                ss,
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            )
        }
    }
}

fn sockaddr_un(p: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let b = p.as_os_str().as_bytes();
    if b.len() >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unix socket path too long: {}", p.display()),
        ));
    }
    for (i, &c) in b.iter().enumerate() {
        addr.sun_path[i] = c as libc::c_char;
    }
    let len = (std::mem::size_of::<libc::sa_family_t>() + b.len() + 1) as libc::socklen_t;
    #[cfg(any(target_os = "freebsd", target_os = "macos"))]
    {
        addr.sun_len = len as u8;
    }
    Ok((addr, len))
}

/// 要求を 1 件処理する。不正な要求は受け取った fd を閉じて捨てる（応答しない）。
fn handle_request(table: &Table, buf: &[u8], reply: Option<RawFd>) {
    let Some(reply) = reply else {
        return;
    };
    let valid = buf.len() == 8
        && u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) == REQ_MAGIC
        && is_unix_socket(reply);
    if valid {
        let idx = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        match table.connect(idx) {
            Ok(fd) => {
                let _ = send_with_fd(reply, &0i32.to_le_bytes(), Some(fd), libc::MSG_DONTWAIT);
                unsafe { libc::close(fd) };
            }
            Err(e) => {
                let _ = send_with_fd(reply, &e.to_le_bytes(), None, libc::MSG_DONTWAIT);
            }
        }
    }
    unsafe { libc::close(reply) };
}

/// ブローカーのメインループ。制御ソケットが EOF になったら（本体の終了）戻る。
pub(crate) fn serve(ctrl: RawFd, table: &Table) -> io::Result<()> {
    let mut buf = [0u8; 16];
    loop {
        match recv_with_fd(ctrl, &mut buf, 0) {
            Ok(r) if r.len == 0 && r.fd.is_none() => return Ok(()),
            Ok(r) => handle_request(table, &buf[..r.len], r.fd),
            // 不正な制御メッセージ（fd は閉じ済み）は捨てて続ける。
            Err(e) if e.kind() == io::ErrorKind::InvalidData => continue,
            Err(e) => return Err(e),
        }
    }
}

/// ホスト名の解決スレッド（未解決は `RESOLVE_RETRY` ごと、解決済みは `RESOLVE_REFRESH` ごと）。
// 理由付き allow: ブローカープロセスの専用スレッド（veil のデータプレーンの外）での待機。
#[allow(clippy::disallowed_methods)]
fn spawn_resolver(table: std::sync::Arc<Table>) -> io::Result<()> {
    if !table.has_hosts() {
        return Ok(());
    }
    std::thread::Builder::new()
        .name("connect-broker-resolver".into())
        .spawn(move || {
            let mut last_full = Instant::now();
            loop {
                std::thread::sleep(RESOLVE_RETRY);
                if last_full.elapsed() >= RESOLVE_REFRESH {
                    table.resolve_hosts(false);
                    last_full = Instant::now();
                } else {
                    table.resolve_hosts(true);
                }
            }
        })
        .map(|_| ())
}

/// 初期化（許可リストの受信・初回の名前解決・READY の返信）をして要求を処理し続ける。
pub(crate) fn run_server(ctrl: RawFd) -> io::Result<()> {
    let targets = recv_allowlist(ctrl)?;
    let table = std::sync::Arc::new(Table::new(targets)?);
    table.resolve_hosts(false);
    spawn_resolver(std::sync::Arc::clone(&table))?;
    send_with_fd(ctrl, READY, None, 0)?;
    serve(ctrl, &table)
}

// ====================
// クライアント（本体側）
// ====================

/// 本体側のクライアント。制御ソケットは全スレッドで共有する（SEQPACKET の送信は原子的）。
pub struct Client {
    ctrl: RawFd,
    allow: Allowlist,
}

impl Client {
    pub fn allowlist(&self) -> &Allowlist {
        &self.allow
    }

    /// 接続要求を送り、返信を受け取る側の fd（非ブロッキング）を返す。
    /// 制御ソケットの送信バッファが一杯なら `WouldBlock`。
    pub fn request(&self, idx: u32) -> io::Result<RawFd> {
        let (mine, theirs) = seqpacket_pair(true)?;
        let mut msg = [0u8; 8];
        msg[..4].copy_from_slice(&REQ_MAGIC.to_le_bytes());
        msg[4..].copy_from_slice(&idx.to_le_bytes());
        let r = send_with_fd(self.ctrl, &msg, Some(theirs), libc::MSG_DONTWAIT);
        unsafe { libc::close(theirs) };
        match r {
            Ok(()) => Ok(mine),
            Err(e) => {
                unsafe { libc::close(mine) };
                Err(e)
            }
        }
    }

    /// 返信を受け取る。成功なら接続中（非ブロッキング connect 開始済み）のソケット。
    /// まだ届いていなければ `WouldBlock`。ブローカーが要求を捨てた場合は EOF で `ConnectionAborted`。
    pub fn recv_reply(reply: RawFd) -> io::Result<RawFd> {
        let mut buf = [0u8; 8];
        let r = recv_with_fd(reply, &mut buf, libc::MSG_DONTWAIT)?;
        if r.len != 4 {
            if let Some(fd) = r.fd {
                unsafe { libc::close(fd) };
            }
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "connect broker rejected the request",
            ));
        }
        let e = i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        match (e, r.fd) {
            (0, Some(fd)) => Ok(fd),
            (0, None) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "connect broker returned no socket",
            )),
            (e, fd) => {
                if let Some(fd) = fd {
                    unsafe { libc::close(fd) };
                }
                Err(io::Error::from_raw_os_error(e))
            }
        }
    }

    /// 同期版（専用スレッド用: ヘルスチェック・WASM の外部呼び出し・OpenTelemetry）。
    /// 接続完了まで `timeout` 以内で待ち、接続済みの非ブロッキングソケットを返す。
    // 理由付き allow: 専用スレッド（イベントループ外）からのみ呼ぶ同期 API。
    #[allow(clippy::disallowed_methods)]
    pub fn connect_blocking(&self, idx: u32, timeout: Duration) -> io::Result<RawFd> {
        let deadline = Instant::now() + timeout;
        let remaining = |d: Instant| -> io::Result<i32> {
            let now = Instant::now();
            if now >= d {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "connect timed out"));
            }
            Ok((d - now).as_millis().clamp(1, i32::MAX as u128) as i32)
        };
        let reply = loop {
            match self.request(idx) {
                Ok(fd) => break fd,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    remaining(deadline)?;
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => return Err(e),
            }
        };
        let res = (|| {
            wait_fd(reply, libc::POLLIN, remaining(deadline)?)?;
            Client::recv_reply(reply)
        })();
        unsafe { libc::close(reply) };
        let fd = res?;
        let done = (|| {
            wait_fd(fd, libc::POLLOUT, remaining(deadline)?)?;
            take_so_error(fd)
        })();
        match done {
            Ok(()) => Ok(fd),
            Err(e) => {
                unsafe { libc::close(fd) };
                Err(e)
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        unsafe { libc::close(self.ctrl) };
    }
}

/// `poll(2)` で `events` を最大 `ms` ミリ秒待つ。
fn wait_fd(fd: RawFd, events: libc::c_short, ms: i32) -> io::Result<()> {
    let mut p = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    loop {
        let r = unsafe { libc::poll(&mut p, 1, ms) };
        if r > 0 {
            return Ok(());
        }
        if r == 0 {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "connect timed out"));
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// 接続の結果（`SO_ERROR`）を確かめる。
pub fn take_so_error(fd: RawFd) -> io::Result<()> {
    let mut err: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            &mut err as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    if err != 0 {
        return Err(io::Error::from_raw_os_error(err));
    }
    Ok(())
}

// ====================
// FreeBSD: 起動・本体側のグローバル状態・ブローカーの防御
// ====================

#[cfg(target_os = "freebsd")]
static CLIENT: std::sync::OnceLock<Client> = std::sync::OnceLock::new();

/// 起動済みのブローカーのクライアント（未起動なら `None`。接続のたびに 1 回の読み出し）。
#[cfg(target_os = "freebsd")]
#[inline]
pub fn client() -> Option<&'static Client> {
    CLIENT.get()
}

/// ブローカーを起動し、許可リストを渡して準備完了まで待つ（**`cap_enter` 前** に呼ぶ）。
///
/// 特権降格の後に呼ぶので、ブローカーは本体と同じ（降格後の）利用者で動く。準備ができたら
/// 監視スレッドを起動する（ブローカーが死んだら上流へつなげないまま動かさず終了コード 1 で終わる）。
#[cfg(target_os = "freebsd")]
pub fn start(allow: Allowlist) -> io::Result<()> {
    use std::os::fd::FromRawFd;
    use std::process::{Command, Stdio};

    let (mine, theirs) = seqpacket_pair(false)?;
    // SAFETY: theirs は socketpair が返した所有権のある fd。Stdio へ移す。
    let child_end = unsafe { std::os::fd::OwnedFd::from_raw_fd(theirs) };
    let exe = std::env::current_exe()?;
    let spawned = Command::new(exe)
        .arg(BROKER_ARG)
        .stdin(Stdio::from(child_end))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn();
    let child = match spawned {
        Ok(c) => c,
        Err(e) => {
            unsafe { libc::close(mine) };
            return Err(e);
        }
    };
    let ready = (|| {
        send_allowlist(mine, &allow)?;
        wait_fd(mine, libc::POLLIN, 10_000)?;
        let mut buf = [0u8; 16];
        let r = recv_with_fd(mine, &mut buf, libc::MSG_DONTWAIT)?;
        if let Some(fd) = r.fd {
            unsafe { libc::close(fd) };
        }
        if &buf[..r.len] != READY {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "connect broker did not become ready",
            ));
        }
        Ok(())
    })();
    if let Err(e) = ready {
        unsafe { libc::close(mine) };
        return Err(e);
    }
    let pid = child.id();
    let entries = allow.len();
    CLIENT.set(Client { ctrl: mine, allow }).map_err(|_| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "connect broker already started",
        )
    })?;
    spawn_monitor(mine)?;
    ftlog::info!(
        "connect broker started (pid {}, {} allowed upstream targets)",
        pid,
        entries
    );
    Ok(())
}

/// ブローカーの終了を監視する。ブローカーは READY の後に制御ソケットへ何も書かないので、
/// 読み取り可能になるのは EOF（ブローカーの終了）のときだけ。
#[cfg(target_os = "freebsd")]
fn spawn_monitor(ctrl: RawFd) -> io::Result<()> {
    std::thread::Builder::new()
        .name("veil-connect-broker-monitor".into())
        .spawn(move || {
            let mut p = libc::pollfd {
                fd: ctrl,
                events: libc::POLLIN,
                revents: 0,
            };
            loop {
                let r = unsafe { libc::poll(&mut p, 1, -1) };
                if r > 0 {
                    break;
                }
                if r < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    break;
                }
            }
            if crate::config::SHUTDOWN_FLAG.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let msg = "connect broker exited; upstream connections are no longer possible under \
                       capability mode, exiting";
            ftlog::error!("{}", msg);
            eprintln!("{}", msg);
            std::process::exit(1);
        })
        .map(|_| ())
}

/// ブローカープロセスのエントリポイント（`run()` の先頭で隠し引数を見て呼ぶ）。終了コードを返す。
#[cfg(target_os = "freebsd")]
pub fn broker_main() -> i32 {
    if let Err(e) = harden() {
        eprintln!("veil connect-broker: hardening failed: {}", e);
        return 1;
    }
    match run_server(libc::STDIN_FILENO) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("veil connect-broker: {}", e);
            1
        }
    }
}

/// ブローカー自身の防御（fd・ptrace・子プロセス・ファイル書き込み・資源を絞る）。
#[cfg(target_os = "freebsd")]
fn harden() -> io::Result<()> {
    // 制御ソケット（0）と標準出力・標準エラー以外の fd を閉じる。
    unsafe { libc::closefrom(3) };
    // SIGPIPE で落ちない（返信先が先に閉じていても続ける。送信は MSG_NOSIGNAL も付ける）。
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    // ptrace とコアダンプを禁止する。本体が死んだら道連れにする。
    let mut trace = libc::PROC_TRACE_CTL_DISABLE;
    let mut sig = libc::SIGKILL;
    // SAFETY: procctl に渡すポインタは呼び出し中だけ有効なローカル変数。
    unsafe {
        if libc::procctl(
            libc::P_PID,
            0,
            libc::PROC_TRACE_CTL,
            &mut trace as *mut _ as *mut libc::c_void,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
        if libc::procctl(
            libc::P_PID,
            0,
            libc::PROC_PDEATHSIG_CTL,
            &mut sig as *mut _ as *mut libc::c_void,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    // fork/exec・ファイルへの書き込み・コアダンプを禁じ、fd の数を絞る。
    for (res, lim) in [
        (libc::RLIMIT_NPROC, 0),
        (libc::RLIMIT_FSIZE, 0),
        (libc::RLIMIT_CORE, 0),
        (libc::RLIMIT_NOFILE, 64),
    ] {
        let rl = libc::rlimit {
            rlim_cur: lim,
            rlim_max: lim,
        };
        if unsafe { libc::setrlimit(res, &rl) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn client_and_server(
        targets: Vec<Target>,
    ) -> (Client, std::thread::JoinHandle<io::Result<()>>) {
        let (mine, theirs) = seqpacket_pair(false).unwrap();
        let server = std::thread::spawn(move || {
            let r = run_server(theirs);
            unsafe { libc::close(theirs) };
            r
        });
        let mut allow = Allowlist::new();
        for t in targets {
            allow.insert(t);
        }
        send_allowlist(mine, &allow).unwrap();
        let mut buf = [0u8; 16];
        let r = recv_with_fd(mine, &mut buf, 0).unwrap();
        assert_eq!(&buf[..r.len], READY);
        (Client { ctrl: mine, allow }, server)
    }

    fn into_tcp(fd: RawFd) -> std::net::TcpStream {
        use std::os::fd::FromRawFd;
        let s = unsafe { std::net::TcpStream::from_raw_fd(fd) };
        s.set_nonblocking(false).unwrap();
        s
    }

    /// 許可リストの添字で TCP・ホスト名・UDS の上流へ接続できる。
    #[test]
    fn connects_to_allowed_targets() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("up.sock");
        let ul = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let (c, server) = client_and_server(vec![
            Target::Addr(l.local_addr().unwrap()),
            Target::Host(format!("localhost:{}", port)),
            Target::Unix(sock.clone()),
        ]);
        let a = c.allowlist();
        assert_eq!(
            a.index_of_conn_addr(&format!("127.0.0.1:{}", port)),
            Some(0)
        );
        assert_eq!(
            a.index_of_conn_addr(&format!("localhost:{}", port)),
            Some(1)
        );
        assert_eq!(
            a.index_of_conn_addr(&format!("unix:{}", sock.display())),
            Some(2)
        );
        assert_eq!(a.index_of_conn_addr("127.0.0.1:1"), None);

        for idx in [0, 1] {
            let fd = c.connect_blocking(idx, Duration::from_secs(5)).unwrap();
            let mut s = into_tcp(fd);
            let (mut peer, _) = l.accept().unwrap();
            s.write_all(b"ping").unwrap();
            let mut b = [0u8; 4];
            peer.read_exact(&mut b).unwrap();
            assert_eq!(&b, b"ping");
        }
        let fd = c.connect_blocking(2, Duration::from_secs(5)).unwrap();
        use std::os::fd::FromRawFd;
        let mut s = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
        s.set_nonblocking(false).unwrap();
        let (mut peer, _) = ul.accept().unwrap();
        s.write_all(b"uds").unwrap();
        let mut b = [0u8; 3];
        peer.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"uds");

        drop(c);
        server.join().unwrap().unwrap();
    }

    /// 範囲外の添字・接続拒否はエラーで返り、ブローカーは動き続ける。
    #[test]
    fn rejects_out_of_range_and_reports_errno() {
        // 拒否されるポート（bind して閉じたポート）
        let refused = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let (c, server) = client_and_server(vec![Target::Addr(refused)]);
        let e = c.connect_blocking(7, Duration::from_secs(5)).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EINVAL));
        let e = c.connect_blocking(0, Duration::from_secs(5)).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::ECONNREFUSED));
        // まだ要求を処理できる
        let e = c.connect_blocking(7, Duration::from_secs(5)).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EINVAL));
        drop(c);
        server.join().unwrap().unwrap();
    }

    /// 不正な要求（magic 違い・長さ違い・fd なし・fd が 2 本・fd がソケットでない）は捨てられ、
    /// 後続の正しい要求は処理される。
    #[test]
    fn drops_malformed_requests() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (c, server) = client_and_server(vec![Target::Addr(l.local_addr().unwrap())]);

        // magic 違い: 返信用ソケットは閉じられ EOF になる。
        let (mine, theirs) = seqpacket_pair(false).unwrap();
        send_with_fd(c.ctrl, &[0u8; 8], Some(theirs), 0).unwrap();
        unsafe { libc::close(theirs) };
        let mut b = [0u8; 8];
        let r = recv_with_fd(mine, &mut b, 0).unwrap();
        assert_eq!((r.len, r.fd), (0, None));
        unsafe { libc::close(mine) };

        // 長さ違い
        let (mine, theirs) = seqpacket_pair(false).unwrap();
        send_with_fd(c.ctrl, &REQ_MAGIC.to_le_bytes(), Some(theirs), 0).unwrap();
        unsafe { libc::close(theirs) };
        let r = recv_with_fd(mine, &mut b, 0).unwrap();
        assert_eq!((r.len, r.fd), (0, None));
        unsafe { libc::close(mine) };

        // fd なし
        let mut msg = [0u8; 8];
        msg[..4].copy_from_slice(&REQ_MAGIC.to_le_bytes());
        send_with_fd(c.ctrl, &msg, None, 0).unwrap();

        // fd がソケットでない（パイプ）
        let mut p = [0 as RawFd; 2];
        assert_eq!(unsafe { libc::pipe(p.as_mut_ptr()) }, 0);
        send_with_fd(c.ctrl, &msg, Some(p[1]), 0).unwrap();
        unsafe { libc::close(p[1]) };
        let mut pb = [0u8; 1];
        // ブローカーが閉じたので読み取りは EOF
        assert_eq!(unsafe { libc::read(p[0], pb.as_mut_ptr() as *mut _, 1) }, 0);
        unsafe { libc::close(p[0]) };

        // fd が 2 本
        let (m1, t1) = seqpacket_pair(false).unwrap();
        let (m2, t2) = seqpacket_pair(false).unwrap();
        {
            let mut iov = libc::iovec {
                iov_base: msg.as_ptr() as *mut libc::c_void,
                iov_len: msg.len(),
            };
            let mut cbuf = CmsgBuf([0; 8]);
            let mut mh: libc::msghdr = unsafe { std::mem::zeroed() };
            mh.msg_iov = &mut iov;
            mh.msg_iovlen = 1;
            unsafe {
                let space = libc::CMSG_SPACE((2 * std::mem::size_of::<RawFd>()) as u32) as usize;
                mh.msg_control = cbuf.0.as_mut_ptr() as *mut libc::c_void;
                mh.msg_controllen = space as _;
                let cm = libc::CMSG_FIRSTHDR(&mh);
                (*cm).cmsg_level = libc::SOL_SOCKET;
                (*cm).cmsg_type = libc::SCM_RIGHTS;
                (*cm).cmsg_len = libc::CMSG_LEN((2 * std::mem::size_of::<RawFd>()) as u32) as _;
                let d = libc::CMSG_DATA(cm) as *mut RawFd;
                std::ptr::write_unaligned(d, t1);
                std::ptr::write_unaligned(d.add(1), t2);
                assert_eq!(libc::sendmsg(c.ctrl, &mh, 0), 8);
                libc::close(t1);
                libc::close(t2);
            }
        }
        for m in [m1, m2] {
            let r = recv_with_fd(m, &mut b, 0).unwrap();
            assert_eq!((r.len, r.fd), (0, None));
            unsafe { libc::close(m) };
        }

        // 正しい要求は処理される
        let fd = c.connect_blocking(0, Duration::from_secs(5)).unwrap();
        let _s = into_tcp(fd);
        let _ = l.accept().unwrap();

        drop(c);
        server.join().unwrap().unwrap();
    }

    /// 許可リストの初期化メッセージの検査（壊れた HELLO・エントリは失敗）。
    #[test]
    fn setup_rejects_malformed_allowlist() {
        let (mine, theirs) = seqpacket_pair(false).unwrap();
        send_with_fd(mine, b"HELLO", None, 0).unwrap();
        assert!(recv_allowlist(theirs).is_err());
        unsafe { libc::close(mine) };
        unsafe { libc::close(theirs) };

        let (mine, theirs) = seqpacket_pair(false).unwrap();
        let mut hello = HELLO.to_vec();
        hello.extend_from_slice(&1u32.to_le_bytes());
        send_with_fd(mine, &hello, None, 0).unwrap();
        send_with_fd(mine, b"\x02nohostport", None, 0).unwrap();
        assert!(recv_allowlist(theirs).is_err());
        unsafe { libc::close(mine) };
        unsafe { libc::close(theirs) };

        assert_eq!(
            Target::decode(b"\x01127.0.0.1:80"),
            Some(Target::Addr("127.0.0.1:80".parse().unwrap()))
        );
        assert_eq!(
            Target::decode(b"\x02example.com:443"),
            Some(Target::Host("example.com:443".into()))
        );
        assert_eq!(
            Target::decode(b"\x03/run/up.sock"),
            Some(Target::Unix("/run/up.sock".into()))
        );
        assert_eq!(Target::decode(b"\x02:443"), None);
        assert_eq!(Target::decode(b"\x02h:99999"), None);
        assert_eq!(Target::decode(b"\x03a\0b"), None);
        assert_eq!(Target::decode(b"\x09x"), None);
        assert_eq!(Target::decode(b""), None);
    }

    /// 重複は 1 エントリにまとまり、`from_host_port` は IP リテラルを Addr にする。
    #[test]
    fn allowlist_dedupes_and_classifies() {
        let mut a = Allowlist::new();
        a.insert(Target::from_host_port("10.0.0.1:80"));
        a.insert(Target::from_host_port("10.0.0.1:80"));
        a.insert(Target::from_host_port("api.internal:8080"));
        a.insert(Target::from_host_port("[::1]:443"));
        assert_eq!(a.len(), 3);
        assert!(!a.is_empty());
        assert_eq!(a.index_of_conn_addr("10.0.0.1:80"), Some(0));
        assert_eq!(a.index_of_conn_addr("api.internal:8080"), Some(1));
        assert_eq!(a.index_of_conn_addr("[::1]:443"), Some(2));
    }
}
