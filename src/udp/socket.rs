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

use monoio::net::udp::UdpSocket;

/// GSO セグメントサイズ（QUIC パケットの典型的なサイズ）
const GSO_SEGMENT_SIZE: usize = 1200;

/// 受信バッファサイズ
const RECV_BUFFER_SIZE: usize = 65536;

/// CMSG バッファサイズ（UDP_SEGMENT + UDP_GRO 用）
const CMSG_BUFFER_SIZE: usize = 128;

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
            libc::socket(domain, libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0) 
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
        
        // std::net::UdpSocket を作成し、monoio の UdpSocket に変換
        let std_socket = unsafe { std::net::UdpSocket::from_raw_fd(fd) };
        let socket = UdpSocket::from_std(std_socket)?;
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

    /// パケットを受信（基本版）
    pub async fn recv_from(&self, buf: Vec<u8>) -> (io::Result<(usize, SocketAddr)>, Vec<u8>) {
        self.socket.recv_from(buf).await
    }

    /// パケットを送信（基本版）
    pub async fn send_to(&self, buf: Vec<u8>, target: SocketAddr) -> (io::Result<usize>, Vec<u8>) {
        self.socket.send_to(buf, target).await
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
            return self.send_single_sync(data, target).map(|bytes| GsoSendResult {
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
        msg.msg_controllen = cmsg_len;
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
        msg.msg_controllen = cmsg_buf.len();
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
    #[deprecated(since = "0.4.0", note = "Use send_gso_async() instead - this function does not handle EAGAIN properly")]
    pub async fn send_gso(&self, packets: &[&[u8]], target: SocketAddr) -> io::Result<usize> {
        if !self.gso_enabled || packets.is_empty() {
            // GSO 無効または空の場合は個別送信
            let mut total = 0;
            for packet in packets {
                let buf = packet.to_vec();
                let (result, _) = self.socket.send_to(buf, target).await;
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

    /// GSO を使用した非同期送信（EAGAIN 対応）
    /// 
    /// monoio は sendmsg を直接サポートしていないため、
    /// 非ブロッキングソケットで同期 API を使用し、
    /// EAGAIN/EWOULDBLOCK 時は writable().await で待機します。
    /// 
    /// # 引数
    /// - `packets`: 送信するパケットのスライス
    /// - `target`: 送信先アドレス
    /// 
    /// # 戻り値
    /// - 送信されたバイト数
    #[cfg(target_os = "linux")]
    pub async fn send_gso_async(&self, packets: &[&[u8]], target: SocketAddr) -> io::Result<usize> {
        if !self.gso_enabled || packets.is_empty() {
            // GSO 無効または空の場合は個別送信
            return self.send_packets_individually(packets, target).await;
        }

        // パケットを結合
        let segment_size = packets.first().map(|p| p.len()).unwrap_or(GSO_SEGMENT_SIZE) as u16;
        let combined = Self::combine_packets(packets);

        // EAGAIN 対応ループ
        loop {
            match self.send_with_gso_sync(&combined, segment_size, target) {
                Ok(result) => return Ok(result.bytes_sent),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // ソケットが書き込み可能になるまで待機
                    self.socket.writable(false).await?;
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
                    self.socket.readable(false).await?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// パケットを個別に送信（GSO 無効時のフォールバック）
    async fn send_packets_individually(&self, packets: &[&[u8]], target: SocketAddr) -> io::Result<usize> {
        let mut total = 0;
        for packet in packets {
            let buf = packet.to_vec();
            let (result, _) = self.socket.send_to(buf, target).await;
            total += result?;
        }
        Ok(total)
    }

    /// パケットを結合
    fn combine_packets(packets: &[&[u8]]) -> Vec<u8> {
        let total_len: usize = packets.iter().map(|p| p.len()).sum();
        let mut combined = Vec::with_capacity(total_len);
        for packet in packets {
            combined.extend_from_slice(packet);
        }
        combined
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
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<u16>() as u32) as usize;
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
            (storage, std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t)
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
            (storage, std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t)
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
        assert!(GSO_SEGMENT_SIZE > 0);
        assert!(GSO_SEGMENT_SIZE <= 65535); // UDPペイロード最大
    }

    #[test]
    fn test_recv_buffer_size() {
        // 受信バッファサイズは十分な大きさ
        assert_eq!(RECV_BUFFER_SIZE, 65536);
        assert!(RECV_BUFFER_SIZE >= GSO_SEGMENT_SIZE);
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
}
