//! # QUIC 用 UDP ソケット
//!
//! monoio と統合した UDP ソケット実装。
//! Linux GSO/GRO をサポートして高スループットを実現。

#![allow(unused_imports)]

use std::io;
use std::net::SocketAddr;
use std::os::unix::io::AsRawFd;

use monoio::net::udp::UdpSocket;

/// GSO セグメントサイズ
const GSO_SEGMENT_SIZE: usize = 1200;

/// 受信バッファサイズ
const RECV_BUFFER_SIZE: usize = 65536;

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

    /// GSO/GRO を設定
    fn configure_gso_gro(&mut self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;

            let fd = self.socket.as_raw_fd();

            // UDP_SEGMENT (GSO) を有効化
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

    /// パケットを受信
    pub async fn recv_from(&self, buf: Vec<u8>) -> (io::Result<(usize, SocketAddr)>, Vec<u8>) {
        self.socket.recv_from(buf).await
    }

    /// パケットを送信
    pub async fn send_to(&self, buf: Vec<u8>, target: SocketAddr) -> (io::Result<usize>, Vec<u8>) {
        self.socket.send_to(buf, target).await
    }

    /// 複数パケットを GSO で送信 (Linux のみ)
    #[cfg(target_os = "linux")]
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
        let mut combined = Vec::with_capacity(packets.iter().map(|p| p.len()).sum());
        for packet in packets {
            combined.extend_from_slice(packet);
        }

        let (result, _) = self.socket.send_to(combined, target).await;
        result
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

    // 注: 実際のソケット操作（bind, recv, send）はmonoioランタイムが必要
    // これらは統合テストで実施することを推奨
}
