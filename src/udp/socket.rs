//! # QUIC 用 UDP ソケット
//!
//! monoio と統合した UDP ソケット実装。
//! Linux GSO/GRO をサポートして高スループットを実現。
//!
//! ## 改善点 (v2)
//! - sendmsg/recvmsg を使用した正しい GSO/GRO 実装
//! - UDP_SEGMENT CMSG によるカーネルレベル GSO
//! - UDP_GRO CMSG によるカーネルレベル GRO
//! - 非ブロッキング I/O 統合

#![allow(unused_imports)]

use std::io;
use std::net::SocketAddr;
use std::os::unix::io::AsRawFd;

// 非ブロッキング UDP ソケット（std::net::UdpSocket ラッパー）
// monoio::net::udp::UdpSocket を削除し、std を使用する
use crate::runtime::tcp::{wait_readable_fd, wait_writable_fd};
use std::net::UdpSocket;

// F-115 第2段: sendmmsg の先頭メッセージ恒久エラーを skip する際の warn ログに使用。
#[cfg(target_os = "linux")]
use ftlog::warn;

/// GSO セグメントサイズ（QUIC パケットの典型的なサイズ）
const GSO_SEGMENT_SIZE: usize = 1200;

/// 受信バッファサイズ
const RECV_BUFFER_SIZE: usize = 65536;

/// CMSG バッファサイズ（UDP_SEGMENT + UDP_GRO 用）
const CMSG_BUFFER_SIZE: usize = 128;

/// F-115 第2段: 1 回の recvmmsg で受ける最大データグラム数。
/// 多接続（-c100）で異なるフローもまとめて 1 syscall で掻き出すためのバッチ幅。
pub const MMSG_RECV_BATCH: usize = 16;

/// F-115 第2段: 1 回の sendmmsg に載せる最大メッセージ数（GSO バッチ = 1 メッセージ）。
pub const MMSG_SEND_BATCH: usize = 16;

/// GSO 送信結果
#[derive(Debug)]
pub struct GsoSendResult {
    /// 送信されたバイト数
    pub bytes_sent: usize,
    /// GSO が使用されたかどうか
    pub gso_used: bool,
}

/// GRO 受信結果
#[derive(Debug)]
pub struct GroRecvResult {
    /// 受信したバイト数
    pub bytes_received: usize,
    /// 送信元アドレス
    pub from: SocketAddr,
    /// GRO セグメントサイズ（GRO 使用時）
    pub gro_segment_size: Option<u16>,
}

/// QUIC 用 UDP ソケット
pub struct QuicUdpSocket {
    /// 内部 UDP ソケット
    socket: UdpSocket,
    /// GSO 有効化フラグ
    gso_enabled: bool,
    /// GRO 有効化フラグ
    gro_enabled: bool,
    /// ローカルアドレス
    local_addr: SocketAddr,
}

impl QuicUdpSocket {
    /// 新しいソケットをバインド
    pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr)?;
        let local_addr = socket.local_addr()?;

        let mut instance = Self {
            socket,
            gso_enabled: false,
            gro_enabled: false,
            local_addr,
        };

        // GSO/GRO を設定
        instance.configure_gso_gro()?;

        Ok(instance)
    }

    /// SO_REUSEPORT を設定してバインド（HTTP/3 マルチスレッド対応）
    ///
    /// 複数ワーカースレッドが同じポートでリッスンし、
    /// カーネルがフローに基づいてパケットを分散します。
    /// GSO/GRO も同時に設定されます。
    #[cfg(target_os = "linux")]
    pub fn bind_reuseport(addr: SocketAddr) -> io::Result<Self> {
        use std::os::unix::io::FromRawFd;

        // ソケットを作成
        let domain = if addr.is_ipv4() {
            libc::AF_INET
        } else {
            libc::AF_INET6
        };

        let fd = unsafe {
            libc::socket(
                domain,
                libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // SO_REUSEADDR を設定
        let optval: libc::c_int = 1;
        let ret = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            unsafe { libc::close(fd) };
            return Err(io::Error::last_os_error());
        }

        // SO_REUSEPORT を設定
        let ret = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEPORT,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            unsafe { libc::close(fd) };
            return Err(io::Error::last_os_error());
        }

        // アドレスをバインド
        let ret = match addr {
            SocketAddr::V4(v4) => {
                let sin = libc::sockaddr_in {
                    sin_family: libc::AF_INET as libc::sa_family_t,
                    sin_port: v4.port().to_be(),
                    sin_addr: libc::in_addr {
                        s_addr: u32::from_ne_bytes(v4.ip().octets()),
                    },
                    sin_zero: [0; 8],
                };
                unsafe {
                    libc::bind(
                        fd,
                        &sin as *const _ as *const libc::sockaddr,
                        std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                    )
                }
            }
            SocketAddr::V6(v6) => {
                let sin6 = libc::sockaddr_in6 {
                    sin6_family: libc::AF_INET6 as libc::sa_family_t,
                    sin6_port: v6.port().to_be(),
                    sin6_flowinfo: v6.flowinfo(),
                    sin6_addr: libc::in6_addr {
                        s6_addr: v6.ip().octets(),
                    },
                    sin6_scope_id: v6.scope_id(),
                };
                unsafe {
                    libc::bind(
                        fd,
                        &sin6 as *const _ as *const libc::sockaddr,
                        std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                    )
                }
            }
        };
        if ret < 0 {
            unsafe { libc::close(fd) };
            return Err(io::Error::last_os_error());
        }

        // std::net::UdpSocket を作成（非ブロッキングモードで設定）
        let std_socket = unsafe { std::net::UdpSocket::from_raw_fd(fd) };
        let socket = std_socket;
        socket.set_nonblocking(true)?;
        let local_addr = socket.local_addr()?;

        let mut instance = Self {
            socket,
            gso_enabled: false,
            gro_enabled: false,
            local_addr,
        };

        // GSO/GRO を設定
        instance.configure_gso_gro()?;

        Ok(instance)
    }

    /// SO_REUSEPORT を設定してバインド（GSO/GRO の有効化オプション付き）
    ///
    /// `enable_gso_gro` が false の場合、GSO/GRO は無効化されますが、
    /// ソケットバッファサイズの増加は適用されます。
    #[cfg(target_os = "linux")]
    pub fn bind_reuseport_with_gso(addr: SocketAddr, enable_gso_gro: bool) -> io::Result<Self> {
        use std::os::unix::io::FromRawFd;

        // ソケットを作成
        let domain = if addr.is_ipv4() {
            libc::AF_INET
        } else {
            libc::AF_INET6
        };

        let fd = unsafe {
            libc::socket(
                domain,
                libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // SO_REUSEADDR を設定
        let reuseaddr: libc::c_int = 1;
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                &reuseaddr as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }

        // SO_REUSEPORT を設定
        let reuseport: libc::c_int = 1;
        let ret = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEPORT,
                &reuseport as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            unsafe { libc::close(fd) };
            return Err(io::Error::last_os_error());
        }

        // バインド
        let ret = match addr {
            SocketAddr::V4(v4) => {
                let sin = libc::sockaddr_in {
                    sin_family: libc::AF_INET as libc::sa_family_t,
                    sin_port: v4.port().to_be(),
                    sin_addr: libc::in_addr {
                        s_addr: u32::from_ne_bytes(v4.ip().octets()),
                    },
                    sin_zero: [0; 8],
                };
                unsafe {
                    libc::bind(
                        fd,
                        &sin as *const _ as *const libc::sockaddr,
                        std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                    )
                }
            }
            SocketAddr::V6(v6) => {
                let sin6 = libc::sockaddr_in6 {
                    sin6_family: libc::AF_INET6 as libc::sa_family_t,
                    sin6_port: v6.port().to_be(),
                    sin6_flowinfo: v6.flowinfo(),
                    sin6_addr: libc::in6_addr {
                        s6_addr: v6.ip().octets(),
                    },
                    sin6_scope_id: v6.scope_id(),
                };
                unsafe {
                    libc::bind(
                        fd,
                        &sin6 as *const _ as *const libc::sockaddr,
                        std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                    )
                }
            }
        };
        if ret < 0 {
            unsafe { libc::close(fd) };
            return Err(io::Error::last_os_error());
        }

        // std::net::UdpSocket を作成（非ブロッキングモードで設定）
        let std_socket = unsafe { std::net::UdpSocket::from_raw_fd(fd) };
        let socket = std_socket;
        socket.set_nonblocking(true)?;
        let local_addr = socket.local_addr()?;

        let mut instance = Self {
            socket,
            gso_enabled: false,
            gro_enabled: false,
            local_addr,
        };

        // GSO/GRO を条件付きで設定
        if enable_gso_gro {
            instance.configure_gso_gro()?;
        } else {
            // GSO/GRO は無効だが、バッファサイズは増加させる
            instance.configure_buffer_sizes()?;
        }

        Ok(instance)
    }

    /// GSO/GRO を設定
    fn configure_gso_gro(&mut self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            let fd = self.socket.as_raw_fd();

            // UDP_SEGMENT (GSO) を有効化
            // 注意: setsockopt での設定はデフォルト値。実際の GSO は
            // sendmsg の CMSG で指定する必要がある
            let gso_size: libc::c_int = GSO_SEGMENT_SIZE as libc::c_int;
            let result = unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_UDP,
                    libc::UDP_SEGMENT,
                    &gso_size as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            self.gso_enabled = result == 0;

            // UDP_GRO を有効化
            let gro_enabled: libc::c_int = 1;
            let result = unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_UDP,
                    libc::UDP_GRO,
                    &gro_enabled as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            self.gro_enabled = result == 0;

            // 受信バッファサイズを増加
            let recv_buf_size: libc::c_int = 2 * 1024 * 1024; // 2MB
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    &recv_buf_size as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }

            // 送信バッファサイズを増加
            let send_buf_size: libc::c_int = 2 * 1024 * 1024; // 2MB
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    &send_buf_size as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
        }

        Ok(())
    }

    /// バッファサイズのみを設定（GSO/GRO無効時用）
    #[cfg(target_os = "linux")]
    fn configure_buffer_sizes(&mut self) -> io::Result<()> {
        let fd = self.socket.as_raw_fd();

        // 受信バッファサイズを増加
        let recv_buf_size: libc::c_int = 2 * 1024 * 1024; // 2MB
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                &recv_buf_size as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }

        // 送信バッファサイズを増加
        let send_buf_size: libc::c_int = 2 * 1024 * 1024; // 2MB
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                &send_buf_size as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }

        Ok(())
    }

    /// パケットを受信（基本版、io_uring POLL_ADD で非同期待機）
    pub async fn recv_from(&self, mut buf: Vec<u8>) -> (io::Result<(usize, SocketAddr)>, Vec<u8>) {
        let fd = self.socket.as_raw_fd();
        loop {
            match self.socket.recv_from(&mut buf) {
                Ok((n, addr)) => return (Ok((n, addr)), buf),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if let Err(e) = wait_readable_fd(fd).await {
                        return (Err(e), buf);
                    }
                }
                Err(e) => return (Err(e), buf),
            }
        }
    }

    /// パケットを送信（基本版、io_uring POLL_ADD で非同期待機）
    pub async fn send_to(&self, buf: Vec<u8>, target: SocketAddr) -> (io::Result<usize>, Vec<u8>) {
        let fd = self.socket.as_raw_fd();
        loop {
            match self.socket.send_to(&buf, target) {
                Ok(n) => return (Ok(n), buf),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if let Err(e) = wait_writable_fd(fd).await {
                        return (Err(e), buf);
                    }
                }
                Err(e) => return (Err(e), buf),
            }
        }
    }

    /// パケットを送信（ゼロアロケーション版、スライス直渡し）
    ///
    /// `send_to` と異なり所有権付き `Vec` を要求せず、`&[u8]` をそのまま
    /// 非ブロッキング sendto する（EAGAIN 時は POLL_ADD で待機）。内部の
    /// `std::net::UdpSocket::send_to` はスライスのみを必要とするため、呼び出し側で
    /// 送信ごとに `to_vec()` する必要がない。io_uring SEND は使わず POLL_ADD のみのため
    /// 新規オペコードも増やさない。HTTP/3 の単一パケット送信（最も高頻度な経路）で使用。
    pub async fn send_to_slice_async(&self, data: &[u8], target: SocketAddr) -> io::Result<usize> {
        let fd = self.socket.as_raw_fd();
        loop {
            match self.socket.send_to(data, target) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    wait_writable_fd(fd).await?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// GSO を使用して複数パケットを効率的に送信
    ///
    /// この関数は sendmsg(2) と UDP_SEGMENT CMSG を使用して、
    /// カーネルレベルでパケットをセグメント化します。
    ///
    /// # 引数
    /// - `data`: 送信するデータ（複数パケットを結合済み）
    /// - `segment_size`: 各パケットのセグメントサイズ
    /// - `target`: 送信先アドレス
    ///
    /// # 戻り値
    /// - 送信されたバイト数と GSO 使用有無
    #[cfg(target_os = "linux")]
    pub fn send_with_gso_sync(
        &self,
        data: &[u8],
        segment_size: u16,
        target: SocketAddr,
    ) -> io::Result<GsoSendResult> {
        use std::mem::MaybeUninit;

        if !self.gso_enabled || data.len() <= segment_size as usize {
            // GSO 無効または単一パケットの場合は通常送信
            return self
                .send_single_sync(data, target)
                .map(|bytes| GsoSendResult {
                    bytes_sent: bytes,
                    gso_used: false,
                });
        }

        let fd = self.socket.as_raw_fd();

        // sockaddr を構築
        let (sockaddr, sockaddr_len) = socket_addr_to_raw(target);

        // iovec を構築
        let iov = libc::iovec {
            iov_base: data.as_ptr() as *mut libc::c_void,
            iov_len: data.len(),
        };

        // CMSG バッファを構築（UDP_SEGMENT 用）
        let mut cmsg_buf = [0u8; CMSG_BUFFER_SIZE];
        let cmsg_len = build_gso_cmsg(&mut cmsg_buf, segment_size)?;

        // msghdr を構築
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_name = &sockaddr as *const _ as *mut libc::c_void;
        msg.msg_namelen = sockaddr_len;
        msg.msg_iov = &iov as *const _ as *mut libc::iovec;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cmsg_len as _;
        msg.msg_flags = 0;

        // sendmsg を呼び出し
        let result = unsafe { libc::sendmsg(fd, &msg, 0) };

        if result < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(GsoSendResult {
            bytes_sent: result as usize,
            gso_used: true,
        })
    }

    /// 単一パケットを送信（同期版）
    #[cfg(target_os = "linux")]
    fn send_single_sync(&self, data: &[u8], target: SocketAddr) -> io::Result<usize> {
        let fd = self.socket.as_raw_fd();
        let (sockaddr, sockaddr_len) = socket_addr_to_raw(target);

        let result = unsafe {
            libc::sendto(
                fd,
                data.as_ptr() as *const libc::c_void,
                data.len(),
                0,
                &sockaddr as *const _ as *const libc::sockaddr,
                sockaddr_len,
            )
        };

        if result < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(result as usize)
    }

    /// GRO を使用してパケットを受信
    ///
    /// この関数は recvmsg(2) と UDP_GRO CMSG を使用して、
    /// カーネルレベルで結合されたパケットを受信します。
    ///
    /// # 引数
    /// - `buf`: 受信バッファ
    ///
    /// # 戻り値
    /// - 受信結果（バイト数、送信元アドレス、GRO セグメントサイズ）
    #[cfg(target_os = "linux")]
    pub fn recv_with_gro_sync(&self, buf: &mut [u8]) -> io::Result<GroRecvResult> {
        let fd = self.socket.as_raw_fd();

        // sockaddr バッファ
        let mut sockaddr_storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };

        // iovec を構築
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };

        // CMSG バッファ
        let mut cmsg_buf = [0u8; CMSG_BUFFER_SIZE];

        // msghdr を構築
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_name = &mut sockaddr_storage as *mut _ as *mut libc::c_void;
        msg.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cmsg_buf.len() as _;
        msg.msg_flags = 0;

        // recvmsg を呼び出し
        let result = unsafe { libc::recvmsg(fd, &mut msg, 0) };

        if result < 0 {
            return Err(io::Error::last_os_error());
        }

        // 送信元アドレスを解析
        let from = raw_to_socket_addr(&sockaddr_storage)?;

        // GRO セグメントサイズを解析
        let gro_segment_size = parse_gro_cmsg(&msg);

        Ok(GroRecvResult {
            bytes_received: result as usize,
            from,
            gro_segment_size,
        })
    }

    /// 複数パケットを GSO で送信（非同期ラッパー）
    ///
    /// monoio は sendmsg を直接サポートしていないため、
    /// 非ブロッキングソケットで同期 API を使用します。
    ///
    /// **非推奨**: `send_gso_async()` を使用してください。
    /// この関数は EAGAIN を適切に処理しません。
    #[cfg(target_os = "linux")]
    #[deprecated(
        since = "0.4.0",
        note = "Use send_gso_async() instead - this function does not handle EAGAIN properly"
    )]
    pub async fn send_gso(&self, packets: &[&[u8]], target: SocketAddr) -> io::Result<usize> {
        if !self.gso_enabled || packets.is_empty() {
            // GSO 無効または空の場合は個別送信
            let mut total = 0;
            for packet in packets {
                let buf = packet.to_vec();
                let (result, _) = self.send_to(buf, target).await;
                total += result?;
            }
            return Ok(total);
        }

        // パケットを結合
        let segment_size = packets.first().map(|p| p.len()).unwrap_or(GSO_SEGMENT_SIZE) as u16;
        let mut combined = Vec::with_capacity(packets.iter().map(|p| p.len()).sum());
        for packet in packets {
            combined.extend_from_slice(packet);
        }

        // GSO 付き送信
        let result = self.send_with_gso_sync(&combined, segment_size, target)?;
        Ok(result.bytes_sent)
    }

    /// 既に連結済みのバッファをそのまま GSO 送信する（非同期・EAGAIN 対応）。
    ///
    /// `send_gso_async` は呼び出し側が渡す `&[&[u8]]` を毎回 `combine_packets` で
    /// 新規 `Vec<u8>` へ再結合していたが、呼び出し元（HTTP/3 送信ループ）は元々
    /// 送信バッファへ連続追記して `offsets` で境界を記録しているため、その時点で
    /// 既に連結済みである。本関数は `combined` を **そのまま** `sendmsg` へ渡し、
    /// GSO 無効時のみ `offsets` を使って境界通りに個別送信する。中間 `Vec` 確保
    /// （パケット参照 Vec + 再結合 Vec の 2 本）とデータの再コピーを排除する。
    ///
    /// # 引数
    /// - `combined`: 呼び出し元の送信スクラッチに連結済みの全パケットデータ
    /// - `offsets`: `combined` 内の各パケットの (start, len)（GSO 無効時のみ使用）
    /// - `segment_size`: GSO セグメントサイズ（最終セグメントのみ短くてよい）
    /// - `target`: 送信先
    #[cfg(target_os = "linux")]
    pub async fn send_gso_combined_async(
        &self,
        combined: &[u8],
        offsets: &[(usize, usize)],
        segment_size: u16,
        target: SocketAddr,
    ) -> io::Result<usize> {
        if !self.gso_enabled || combined.is_empty() {
            // GSO 無効: 元のパケット境界通りに個別送信（追加確保なし）。
            let mut total = 0;
            for &(start, len) in offsets {
                total += self
                    .send_to_slice_async(&combined[start..start + len], target)
                    .await?;
            }
            return Ok(total);
        }

        // EAGAIN 対応ループ（再結合なしで combined をそのまま渡す）
        loop {
            match self.send_with_gso_sync(combined, segment_size, target) {
                Ok(result) => return Ok(result.bytes_sent),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    wait_writable_fd(self.socket.as_raw_fd()).await?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// GRO を使用した非同期受信（EAGAIN 対応）
    ///
    /// 非ブロッキングソケットで recvmsg を使用し、
    /// EAGAIN/EWOULDBLOCK 時は readable().await で待機します。
    ///
    /// # 引数
    /// - `buf`: 受信バッファ
    ///
    /// # 戻り値
    /// - 受信結果（バイト数、送信元アドレス、GRO セグメントサイズ）
    #[cfg(target_os = "linux")]
    pub async fn recv_gro_async(&self, buf: &mut [u8]) -> io::Result<GroRecvResult> {
        loop {
            match self.recv_with_gro_sync(buf) {
                Ok(result) => return Ok(result),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // ソケットが読み込み可能になるまで待機
                    wait_readable_fd(self.socket.as_raw_fd()).await?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// F-115 第2段: 非ブロッキング recvmmsg。1 syscall で最大 `MMSG_RECV_BATCH` 件の
    /// データグラムを掻き出し、受信できた件数を返す。
    ///
    /// - `flags = MSG_DONTWAIT`・`timeout = NULL` で呼ぶため、受信できるものが無ければ
    ///   EAGAIN（`WouldBlock`）を `Err` で返す（0 件は返らない）。呼び出し側は EAGAIN で
    ///   drain を打ち切り、既存の POLL_ADD（`wait_readable_fd`）待機に戻る。**新規 io_uring
    ///   オペコードは追加しない**（ホットパス絶対規則）。
    /// - カーネルが書き換える `msg_namelen` / `msg_controllen` / `msg_flags` / `msg_len` のみ
    ///   毎回リセットする（N=16 の書き込みのみ、ヒープ確保なし）。iovec/ポインタ配線は
    ///   `MmsgRecvScratch::new()` で 1 回だけ済ませてある。
    /// - i 番目の結果は `scratch.meta(i)`（長さ・送信元・GRO セグメントサイズ）と
    ///   `scratch.buf_mut(i)`（ゼロコピーで quiche へ渡すバッファ）で取り出す。
    #[cfg(target_os = "linux")]
    pub fn recv_mmsg_sync(&self, scratch: &mut MmsgRecvScratch) -> io::Result<usize> {
        let fd = self.socket.as_raw_fd();

        // カーネルが上書きするフィールドのみ毎回リセット（確保なし）。
        // 不変条件: msg_name/msg_iov/msg_control のポインタは new() で Box 固定アドレスへ
        // 配線済みなので触らない。長さフィールドだけカーネル入力上限へ戻す。
        for i in 0..MMSG_RECV_BATCH {
            let hdr = &mut scratch.hdrs[i].msg_hdr;
            hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            hdr.msg_controllen = CMSG_BUFFER_SIZE as _;
            hdr.msg_flags = 0;
            scratch.hdrs[i].msg_len = 0;
        }

        // 安全性: hdrs は Box<[mmsghdr; N]> の固定アドレス。各 mmsghdr のポインタ群は
        // 同一 scratch 内の Box（bufs/addrs/iovecs/cmsg_bufs）を指し、いずれも有効。
        // MSG_DONTWAIT なのでブロッキングしない。
        let n = unsafe {
            libc::recvmmsg(
                fd,
                scratch.hdrs.as_mut_ptr(),
                MMSG_RECV_BATCH as libc::c_uint,
                libc::MSG_DONTWAIT,
                std::ptr::null_mut(),
            )
        };

        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        // n は 1..=MMSG_RECV_BATCH（EAGAIN は上で Err 済み）。
        Ok(n as usize)
    }

    /// F-115 第2段: `entries` を sendmmsg でまとめて送出する（非ブロッキング + EAGAIN 待機）。
    ///
    /// - 各エントリの `data`（呼び出し側スクラッチに連結済みのパケット列）を iovec が
    ///   そのまま指すためゼロコピー。`segments > 1` のエントリのみ UDP_SEGMENT cmsg を付け、
    ///   カーネル GSO でセグメント分割させる（`segments == 1` は cmsg なし）。
    /// - sendmmsg のセマンティクス: k 件送って k+1 件目で失敗すると k(>0) を返す（errno は
    ///   取れない）。よって「送れた件数だけ先頭を進めて再呼び出し」し、-1 で初めて errno を
    ///   得る。EAGAIN（送信バッファ満杯）は `wait_writable_fd`（既存 POLL_ADD）で待って再開。
    ///   先頭メッセージの恒久エラー（EMSGSIZE 等）は当該メッセージのみ skip + warn して継続する
    ///   （QUIC 再送で回復する）。
    /// - cancel-safety: 唯一の await ポイントは `wait_writable_fd`。drop されてもカーネルへ
    ///   未確定の副作用は残さない（sendmmsg 自体は同期・非ブロッキング）。
    #[cfg(target_os = "linux")]
    pub async fn send_mmsg_async(
        &self,
        entries: &[SendmmsgEntry<'_>],
        scratch: &mut MmsgSendScratch,
    ) -> io::Result<()> {
        let count = entries.len().min(MMSG_SEND_BATCH);
        if count == 0 {
            return Ok(());
        }

        // 各エントリを mmsghdr へ変換（addr / iovec / cmsg を per-entry の Box 固定領域へ書く）。
        // ポインタ配線（msg_name/msg_iov/msg_control が addrs[i]/iovecs[i]/cmsg_bufs[i] を指す）は
        // new() 済み。ここでは中身と可変長フィールドのみ更新する（確保なし）。
        for (i, e) in entries.iter().take(count).enumerate() {
            let (sockaddr, sockaddr_len) = socket_addr_to_raw(e.dest);
            scratch.addrs[i] = sockaddr;
            scratch.iovecs[i].iov_base = e.data.as_ptr() as *mut libc::c_void;
            scratch.iovecs[i].iov_len = e.data.len();

            let hdr = &mut scratch.hdrs[i].msg_hdr;
            hdr.msg_namelen = sockaddr_len;
            if e.segments > 1 {
                // GSO: UDP_SEGMENT cmsg を per-entry バッファへ構築。
                let cmsg_len = build_gso_cmsg(&mut scratch.cmsg_bufs[i], e.seg_size)?;
                hdr.msg_controllen = cmsg_len as _;
            } else {
                // 単一パケット: cmsg なし（controllen=0 で control 領域を無視させる）。
                hdr.msg_controllen = 0;
            }
            scratch.hdrs[i].msg_len = 0;
        }

        let fd = self.socket.as_raw_fd();
        let mut sent = 0usize;
        while sent < count {
            let remaining = count - sent;
            // 安全性: hdrs[sent..] は Box 固定アドレスの連続 mmsghdr。各ポインタは同一 scratch の
            // Box を指す。MSG_DONTWAIT で非ブロッキング。
            let ret = unsafe {
                libc::sendmmsg(
                    fd,
                    scratch.hdrs[sent..].as_mut_ptr(),
                    remaining as libc::c_uint,
                    libc::MSG_DONTWAIT,
                )
            };

            if ret < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock {
                    // 送信バッファ満杯: 書き込み可能を待って再開（先頭 sent 件は送信済み）。
                    wait_writable_fd(fd).await?;
                    continue;
                }
                // 先頭メッセージの恒久エラー: 当該メッセージのみ skip して継続（QUIC 再送で回復）。
                warn!("[HTTP/3] sendmmsg entry skipped: {}", err);
                sent += 1;
                continue;
            }
            if ret == 0 {
                // 想定外（規格上 0 は返らない）。無限ループ回避のため 1 件進めて打ち切り側へ寄せる。
                sent += 1;
                continue;
            }
            sent += ret as usize;
        }

        Ok(())
    }

    /// GSO が有効かどうか
    pub fn gso_enabled(&self) -> bool {
        self.gso_enabled
    }

    /// GRO が有効かどうか
    pub fn gro_enabled(&self) -> bool {
        self.gro_enabled
    }

    /// ローカルアドレスを取得
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// 内部ソケットへの参照を取得
    pub fn inner(&self) -> &UdpSocket {
        &self.socket
    }

    /// 内部ソケットの raw fd を取得
    pub fn as_raw_fd(&self) -> i32 {
        self.socket.as_raw_fd()
    }
}

// ====================
// F-115 第2段: recvmmsg / sendmmsg スクラッチ
// ====================

/// F-115 第2段: recvmmsg 用スクラッチ。
///
/// mmsghdr / iovec / sockaddr_storage / cmsg バッファを **Box で固定アドレス化**し、
/// 各 mmsghdr のポインタ配線（msg_name/msg_iov/msg_control）は `new()` で 1 回だけ行う。
/// 以降の `recv_mmsg_sync` はカーネルが書き換える長さ/フラグフィールドのみリセットするため、
/// per-sweep のヒープ確保が発生しない（ホットパス絶対規則）。
///
/// 不変条件: 本構造体を move してもポインタは有効。Box の move はスタック上のポインタ値
/// （fat/thin）だけを移すもので、ヒープ確保の実アドレスは変わらないため、mmsghdr が保持する
/// 内部ポインタ（別 Box のヒープ領域を指す）は移動後も同じ有効アドレスを指す。
#[cfg(target_os = "linux")]
pub struct MmsgRecvScratch {
    /// MMSG_RECV_BATCH × RECV_BUFFER_SIZE(65536) の連続バッファ（GRO 集約対応）。
    bufs: Box<[u8]>,
    /// 送信元アドレス（per-msg）。
    addrs: Box<[libc::sockaddr_storage; MMSG_RECV_BATCH]>,
    /// 各 msg の iovec（bufs 内の自分の区画を指す）。
    iovecs: Box<[libc::iovec; MMSG_RECV_BATCH]>,
    /// 各 msg の cmsg 受信バッファ（UDP_GRO を per-msg で受ける）。
    cmsg_bufs: Box<[[u8; CMSG_BUFFER_SIZE]; MMSG_RECV_BATCH]>,
    /// recvmmsg へ渡す mmsghdr 配列。
    hdrs: Box<[libc::mmsghdr; MMSG_RECV_BATCH]>,
}

#[cfg(target_os = "linux")]
impl MmsgRecvScratch {
    /// スクラッチを確保し、mmsghdr のポインタ配線を 1 回だけ行う。
    pub fn new() -> Self {
        let bufs = vec![0u8; MMSG_RECV_BATCH * RECV_BUFFER_SIZE].into_boxed_slice();
        // 安全性: sockaddr_storage / iovec / mmsghdr はすべて POD（全ビットゼロが有効表現）。
        let addrs: Box<[libc::sockaddr_storage; MMSG_RECV_BATCH]> =
            Box::new(unsafe { std::mem::zeroed() });
        let iovecs: Box<[libc::iovec; MMSG_RECV_BATCH]> = Box::new(unsafe { std::mem::zeroed() });
        let cmsg_bufs: Box<[[u8; CMSG_BUFFER_SIZE]; MMSG_RECV_BATCH]> =
            Box::new([[0u8; CMSG_BUFFER_SIZE]; MMSG_RECV_BATCH]);
        let hdrs: Box<[libc::mmsghdr; MMSG_RECV_BATCH]> = Box::new(unsafe { std::mem::zeroed() });

        let mut scratch = Self {
            bufs,
            addrs,
            iovecs,
            cmsg_bufs,
            hdrs,
        };
        scratch.wire_pointers();
        scratch
    }

    /// mmsghdr / iovec を Box 固定アドレスへ配線する（new() から 1 回だけ呼ぶ）。
    fn wire_pointers(&mut self) {
        for i in 0..MMSG_RECV_BATCH {
            // 生ポインタは即座に借用を手放すため、フィールド間の可変借用衝突は起きない。
            let buf_ptr =
                unsafe { self.bufs.as_mut_ptr().add(i * RECV_BUFFER_SIZE) } as *mut libc::c_void;
            let addr_ptr = (&mut self.addrs[i] as *mut libc::sockaddr_storage) as *mut libc::c_void;
            let iov_ptr = &mut self.iovecs[i] as *mut libc::iovec;
            let cmsg_ptr = self.cmsg_bufs[i].as_mut_ptr() as *mut libc::c_void;

            self.iovecs[i].iov_base = buf_ptr;
            self.iovecs[i].iov_len = RECV_BUFFER_SIZE;

            let hdr = &mut self.hdrs[i].msg_hdr;
            hdr.msg_name = addr_ptr;
            hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            hdr.msg_iov = iov_ptr;
            hdr.msg_iovlen = 1;
            hdr.msg_control = cmsg_ptr;
            hdr.msg_controllen = CMSG_BUFFER_SIZE as _;
            hdr.msg_flags = 0;
        }
    }

    /// i 番目の受信メタ情報（受信バイト数・送信元・GRO セグメントサイズ）を返す。
    ///
    /// 借用を持たない値のみ返すため、続けて `buf_mut(i)` を可変借用できる（分割 2 段アクセス）。
    /// `msg_len` はカーネルが書いた受信バイト数（0..=RECV_BUFFER_SIZE の範囲）。
    pub fn meta(&self, i: usize) -> io::Result<(usize, SocketAddr, Option<u16>)> {
        let len = self.hdrs[i].msg_len as usize;
        let from = raw_to_socket_addr(&self.addrs[i])?;
        // カーネルは msg_controllen を実 cmsg 長へ書き換えているので、per-msg msghdr で GRO を解析。
        let gro = parse_gro_cmsg(&self.hdrs[i].msg_hdr);
        Ok((len, from, gro))
    }

    /// i 番目の受信バッファ（全域スライス）。呼び出し側で `meta` の len で切って quiche へ渡す。
    pub fn buf_mut(&mut self, i: usize) -> &mut [u8] {
        let start = i * RECV_BUFFER_SIZE;
        &mut self.bufs[start..start + RECV_BUFFER_SIZE]
    }
}

#[cfg(target_os = "linux")]
impl Default for MmsgRecvScratch {
    fn default() -> Self {
        Self::new()
    }
}

/// F-115 第2段: sendmmsg 用の送信ディスクリプタ。
///
/// `data` は呼び出し側スクラッチに連結済みのパケット列（GSO バッチ）を指す。iovec が
/// そのまま指すためコピーは発生しない。
#[cfg(target_os = "linux")]
pub struct SendmmsgEntry<'a> {
    /// 連結済みパケット列（batch のスライス）。
    pub data: &'a [u8],
    /// GSO セグメントサイズ。
    pub seg_size: u16,
    /// パケット数。1 なら UDP_SEGMENT cmsg を付けない。
    pub segments: u16,
    /// 送信先。
    pub dest: SocketAddr,
}

/// F-115 第2段: sendmmsg 用スクラッチ。
///
/// `MmsgRecvScratch` 同様、iovec/sockaddr/cmsg/mmsghdr を Box で固定アドレス化し、ポインタ配線を
/// `new()` で 1 回だけ行う。送信ごとに変わるのは addr の中身・iovec の指す先・cmsg 長のみで、
/// `send_mmsg_async` がそれらを in-place に更新する（per-sweep のヒープ確保なし）。
#[cfg(target_os = "linux")]
pub struct MmsgSendScratch {
    /// 送信先アドレス（per-entry）。
    addrs: Box<[libc::sockaddr_storage; MMSG_SEND_BATCH]>,
    /// 各エントリの iovec（entries[i].data を指す）。
    iovecs: Box<[libc::iovec; MMSG_SEND_BATCH]>,
    /// 各エントリの UDP_SEGMENT cmsg バッファ。
    cmsg_bufs: Box<[[u8; CMSG_BUFFER_SIZE]; MMSG_SEND_BATCH]>,
    /// sendmmsg へ渡す mmsghdr 配列。
    hdrs: Box<[libc::mmsghdr; MMSG_SEND_BATCH]>,
}

#[cfg(target_os = "linux")]
impl MmsgSendScratch {
    /// スクラッチを確保し、mmsghdr のポインタ配線を 1 回だけ行う。
    pub fn new() -> Self {
        // 安全性: いずれも POD（全ビットゼロが有効表現）。
        let addrs: Box<[libc::sockaddr_storage; MMSG_SEND_BATCH]> =
            Box::new(unsafe { std::mem::zeroed() });
        let iovecs: Box<[libc::iovec; MMSG_SEND_BATCH]> = Box::new(unsafe { std::mem::zeroed() });
        let cmsg_bufs: Box<[[u8; CMSG_BUFFER_SIZE]; MMSG_SEND_BATCH]> =
            Box::new([[0u8; CMSG_BUFFER_SIZE]; MMSG_SEND_BATCH]);
        let hdrs: Box<[libc::mmsghdr; MMSG_SEND_BATCH]> = Box::new(unsafe { std::mem::zeroed() });

        let mut scratch = Self {
            addrs,
            iovecs,
            cmsg_bufs,
            hdrs,
        };
        scratch.wire_pointers();
        scratch
    }

    /// mmsghdr のポインタを Box 固定アドレスへ配線する（new() から 1 回だけ）。
    /// msg_namelen / iov 内容 / cmsg 長 / addr 内容は送信ごとに `send_mmsg_async` が更新する。
    fn wire_pointers(&mut self) {
        for i in 0..MMSG_SEND_BATCH {
            let addr_ptr = (&mut self.addrs[i] as *mut libc::sockaddr_storage) as *mut libc::c_void;
            let iov_ptr = &mut self.iovecs[i] as *mut libc::iovec;
            let cmsg_ptr = self.cmsg_bufs[i].as_mut_ptr() as *mut libc::c_void;

            let hdr = &mut self.hdrs[i].msg_hdr;
            hdr.msg_name = addr_ptr;
            hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            hdr.msg_iov = iov_ptr;
            hdr.msg_iovlen = 1;
            hdr.msg_control = cmsg_ptr;
            hdr.msg_controllen = 0;
            hdr.msg_flags = 0;
        }
    }
}

#[cfg(target_os = "linux")]
impl Default for MmsgSendScratch {
    fn default() -> Self {
        Self::new()
    }
}

// ====================
// GSO/GRO CMSG ヘルパー関数
// ====================

/// UDP_SEGMENT 用の CMSG を構築
///
/// sendmsg(2) で使用する制御メッセージを構築します。
/// カーネルはこのセグメントサイズでデータを分割して送信します。
#[cfg(target_os = "linux")]
fn build_gso_cmsg(buf: &mut [u8], segment_size: u16) -> io::Result<usize> {
    // CMSG ヘッダサイズの計算
    let cmsg_space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<u16>() as u32) as usize };

    if buf.len() < cmsg_space {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "CMSG buffer too small",
        ));
    }

    // cmsghdr を構築
    let cmsg = buf.as_mut_ptr() as *mut libc::cmsghdr;
    unsafe {
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<u16>() as u32) as _;
        (*cmsg).cmsg_level = libc::SOL_UDP;
        (*cmsg).cmsg_type = libc::UDP_SEGMENT;

        // セグメントサイズをデータ領域に書き込み
        let data_ptr = libc::CMSG_DATA(cmsg) as *mut u16;
        *data_ptr = segment_size;
    }

    Ok(cmsg_space)
}

/// recvmsg から UDP_GRO セグメントサイズを解析
///
/// カーネルが GRO で結合したパケットの元のセグメントサイズを取得します。
#[cfg(target_os = "linux")]
fn parse_gro_cmsg(msg: &libc::msghdr) -> Option<u16> {
    // CMSG を走査
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(msg) };

    while !cmsg.is_null() {
        let cmsg_ref = unsafe { &*cmsg };

        if cmsg_ref.cmsg_level == libc::SOL_UDP && cmsg_ref.cmsg_type == libc::UDP_GRO {
            // GRO セグメントサイズを読み取り
            let data_ptr = unsafe { libc::CMSG_DATA(cmsg) as *const u16 };
            return Some(unsafe { *data_ptr });
        }

        cmsg = unsafe { libc::CMSG_NXTHDR(msg, cmsg) };
    }

    None
}

// ====================
// SocketAddr 変換ヘルパー
// ====================

/// SocketAddr を libc sockaddr に変換
#[cfg(target_os = "linux")]
fn socket_addr_to_raw(addr: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };

    match addr {
        SocketAddr::V4(v4) => {
            let sin = &mut storage as *mut _ as *mut libc::sockaddr_in;
            unsafe {
                (*sin).sin_family = libc::AF_INET as libc::sa_family_t;
                (*sin).sin_port = v4.port().to_be();
                (*sin).sin_addr.s_addr = u32::from_ne_bytes(v4.ip().octets());
            }
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        }
        SocketAddr::V6(v6) => {
            let sin6 = &mut storage as *mut _ as *mut libc::sockaddr_in6;
            unsafe {
                (*sin6).sin6_family = libc::AF_INET6 as libc::sa_family_t;
                (*sin6).sin6_port = v6.port().to_be();
                (*sin6).sin6_flowinfo = v6.flowinfo();
                (*sin6).sin6_addr.s6_addr = v6.ip().octets();
                (*sin6).sin6_scope_id = v6.scope_id();
            }
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            )
        }
    }
}

/// libc sockaddr_storage を SocketAddr に変換
#[cfg(target_os = "linux")]
fn raw_to_socket_addr(storage: &libc::sockaddr_storage) -> io::Result<SocketAddr> {
    match storage.ss_family as libc::c_int {
        libc::AF_INET => {
            let sin = storage as *const _ as *const libc::sockaddr_in;
            let sin_ref = unsafe { &*sin };
            let ip = std::net::Ipv4Addr::from(sin_ref.sin_addr.s_addr.to_ne_bytes());
            let port = u16::from_be(sin_ref.sin_port);
            Ok(SocketAddr::V4(std::net::SocketAddrV4::new(ip, port)))
        }
        libc::AF_INET6 => {
            let sin6 = storage as *const _ as *const libc::sockaddr_in6;
            let sin6_ref = unsafe { &*sin6 };
            let ip = std::net::Ipv6Addr::from(sin6_ref.sin6_addr.s6_addr);
            let port = u16::from_be(sin6_ref.sin6_port);
            Ok(SocketAddr::V6(std::net::SocketAddrV6::new(
                ip,
                port,
                sin6_ref.sin6_flowinfo,
                sin6_ref.sin6_scope_id,
            )))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Unknown address family",
        )),
    }
}

/// 受信バッファを作成
pub fn create_recv_buffer() -> Vec<u8> {
    vec![0u8; RECV_BUFFER_SIZE]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    // ====================
    // 定数テスト
    // ====================

    #[test]
    fn test_gso_segment_size() {
        // GSO セグメントサイズは適切な値
        // 通常のQUICパケットサイズに合わせて設定
        assert_eq!(GSO_SEGMENT_SIZE, 1200);
        const _: () = assert!(GSO_SEGMENT_SIZE > 0);
        const _: () = assert!(GSO_SEGMENT_SIZE <= 65535); // UDPペイロード最大
    }

    #[test]
    fn test_recv_buffer_size() {
        // 受信バッファサイズは十分な大きさ
        assert_eq!(RECV_BUFFER_SIZE, 65536);
        const _: () = assert!(RECV_BUFFER_SIZE >= GSO_SEGMENT_SIZE);
    }

    // ====================
    // create_recv_buffer テスト
    // ====================

    #[test]
    fn test_create_recv_buffer() {
        // 受信バッファの作成
        let buf = create_recv_buffer();

        assert_eq!(buf.len(), RECV_BUFFER_SIZE);
        assert!(buf.iter().all(|&b| b == 0)); // ゼロ初期化
    }

    #[test]
    fn test_create_recv_buffer_capacity() {
        // 容量も正しく設定されている
        let buf = create_recv_buffer();

        assert!(buf.capacity() >= RECV_BUFFER_SIZE);
    }

    // ====================
    // SocketAddr テスト
    // ====================

    #[test]
    fn test_socket_addr_v4() {
        // IPv4アドレスの作成
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);

        assert_eq!(addr.port(), 8080);
        assert!(addr.is_ipv4());
    }

    #[test]
    fn test_socket_addr_any() {
        // 任意アドレス (0.0.0.0)
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

        assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
    }

    // ====================
    // CMSG テスト
    // ====================

    #[cfg(target_os = "linux")]
    #[test]
    fn test_build_gso_cmsg() {
        let mut buf = [0u8; CMSG_BUFFER_SIZE];
        let segment_size: u16 = 1200;

        let result = build_gso_cmsg(&mut buf, segment_size);
        assert!(result.is_ok());

        let cmsg_len = result.unwrap();
        assert!(cmsg_len > 0);
        assert!(cmsg_len <= CMSG_BUFFER_SIZE);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_socket_addr_conversion_v4() {
        let original = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 12345);
        let (storage, _) = socket_addr_to_raw(original);
        let recovered = raw_to_socket_addr(&storage).unwrap();

        assert_eq!(original, recovered);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_socket_addr_conversion_v6() {
        use std::net::Ipv6Addr;
        let original = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 54321);
        let (storage, _) = socket_addr_to_raw(original);
        let recovered = raw_to_socket_addr(&storage).unwrap();

        assert_eq!(original, recovered);
    }

    // 注: 実際のソケット操作（bind, recv, send）はmonoioランタイムが必要
    // これらは統合テストで実施することを推奨

    // ====================
    // F-115 第2段: recvmmsg / sendmmsg スクラッチのテスト
    // ====================

    /// ループバック UDP ペアで recvmmsg が複数データグラムを 1 回で受け、
    /// 空ソケットでは EAGAIN（WouldBlock）を返すことを検証する（設計書 §4）。
    #[cfg(target_os = "linux")]
    #[test]
    fn test_recv_mmsg_sync_multiple_and_eagain() {
        let server = QuicUdpSocket::bind_reuseport_with_gso(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            false,
        )
        .expect("bind server");
        let server_addr = server.local_addr();

        // 送信元 client（テスト専用の同期 UDP ソケット。ホットパスではない）。
        let client = std::net::UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .expect("bind client");

        let mut scratch = MmsgRecvScratch::new();

        // 空のソケットでは EAGAIN（WouldBlock）が返る（0 件は返らない）。
        let empty = server.recv_mmsg_sync(&mut scratch);
        assert!(matches!(empty, Err(ref e) if e.kind() == io::ErrorKind::WouldBlock));

        // 複数データグラムを送信。
        let payloads: [&[u8]; 3] = [b"alpha", b"bravo-two", b"charlie-three-3333"];
        for p in payloads.iter() {
            client.send_to(p, server_addr).expect("send");
        }

        // recvmmsg で掻き出す（loopback 配送遅延に備え WouldBlock は有限回リトライ）。
        let mut received: Vec<Vec<u8>> = Vec::new();
        let mut attempts = 0;
        while received.len() < payloads.len() && attempts < 100_000 {
            attempts += 1;
            match server.recv_mmsg_sync(&mut scratch) {
                Ok(n) => {
                    assert!((1..=MMSG_RECV_BATCH).contains(&n));
                    for i in 0..n {
                        let (len, from, _gro) = scratch.meta(i).expect("meta");
                        // 送信元は client（localhost）。
                        assert_eq!(from.ip(), server_addr.ip());
                        received.push(scratch.buf_mut(i)[..len].to_vec());
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("recv_mmsg_sync error: {e}"),
            }
        }

        assert_eq!(received.len(), payloads.len());
        // 同一送信元・ローカルループバックのため送出順で届く。
        for (p, r) in payloads.iter().zip(received.iter()) {
            assert_eq!(&r[..], *p);
        }
    }

    /// sendmmsg スクラッチの mmsghdr ポインタ配線（new() で 1 回）を検証する（設計書 §4）。
    #[cfg(target_os = "linux")]
    #[test]
    fn test_mmsg_send_scratch_wiring() {
        let scratch = MmsgSendScratch::new();
        for i in 0..MMSG_SEND_BATCH {
            let hdr = &scratch.hdrs[i].msg_hdr;
            // 配線先はいずれも自分の Box 固定領域を指す（非 NULL）。
            assert!(!hdr.msg_name.is_null());
            assert!(!hdr.msg_iov.is_null());
            assert!(!hdr.msg_control.is_null());
            assert_eq!(hdr.msg_iovlen, 1);
            // msg_name が addrs[i]、msg_control が cmsg_bufs[i] を指すこと。
            assert_eq!(
                hdr.msg_name as *const libc::sockaddr_storage,
                &scratch.addrs[i] as *const libc::sockaddr_storage
            );
            assert_eq!(hdr.msg_control as *const u8, scratch.cmsg_bufs[i].as_ptr());
        }
    }

    /// recvmmsg スクラッチの iovec が bufs 内の自分の区画を指すことを検証する。
    #[cfg(target_os = "linux")]
    #[test]
    fn test_mmsg_recv_scratch_wiring() {
        let scratch = MmsgRecvScratch::new();
        for i in 0..MMSG_RECV_BATCH {
            let iov = &scratch.iovecs[i];
            assert_eq!(iov.iov_len, RECV_BUFFER_SIZE);
            let expected = scratch.bufs[i * RECV_BUFFER_SIZE..].as_ptr();
            assert_eq!(iov.iov_base as *const u8, expected);
            // mmsghdr が iovecs[i] を指すこと。
            let hdr = &scratch.hdrs[i].msg_hdr;
            assert_eq!(
                hdr.msg_iov as *const libc::iovec,
                &scratch.iovecs[i] as *const libc::iovec
            );
        }
    }
}
