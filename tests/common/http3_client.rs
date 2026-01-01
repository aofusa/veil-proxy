//! HTTP/3テストクライアント
//!
//! quicheを使用したHTTP/3クライアントの実装

#[cfg(feature = "http3")]
use std::net::{SocketAddr, UdpSocket};
#[cfg(feature = "http3")]
use std::time::{Duration, Instant};
#[cfg(feature = "http3")]
use quiche::{Config, Connection, ConnectionId};
#[cfg(feature = "http3")]
use quiche::h3;
#[cfg(feature = "http3")]
use quiche::h3::NameValue;
#[cfg(feature = "http3")]
use aws_lc_rs::rand::{SecureRandom, SystemRandom};

/// HTTP/3テストクライアント
#[cfg(feature = "http3")]
pub struct Http3TestClient {
    conn: Connection,
    socket: UdpSocket,
    peer_addr: SocketAddr,
    local_addr: SocketAddr,
    h3_conn: Option<h3::Connection>,
}

#[cfg(feature = "http3")]
impl Http3TestClient {
    /// 新しいHTTP/3クライアントを作成
    pub fn new(server_addr: SocketAddr) -> Result<Self, Box<dyn std::error::Error>> {
        // UDPソケットを作成
        let socket = UdpSocket::bind("127.0.0.1:0")?;
        let local_addr = socket.local_addr()?;
        socket.set_read_timeout(Some(Duration::from_millis(500)))?;
        socket.set_write_timeout(Some(Duration::from_secs(5)))?;
        socket.connect(server_addr)?;
        
        // QUIC設定
        let mut config = Config::new(quiche::PROTOCOL_VERSION)?;
        config.set_application_protos(quiche::h3::APPLICATION_PROTOCOL)?;
        config.set_initial_max_data(10_000_000);
        config.set_initial_max_stream_data_bidi_local(1_000_000);
        config.set_initial_max_stream_data_bidi_remote(1_000_000);
        config.set_initial_max_stream_data_uni(1_000_000);
        config.set_initial_max_streams_bidi(100);
        config.set_initial_max_streams_uni(100);
        config.set_max_idle_timeout(5000);
        config.set_max_recv_udp_payload_size(1350);
        config.set_max_send_udp_payload_size(1350);
        config.set_disable_active_migration(true);
        config.verify_peer(false); // テスト用に証明書検証を無効化
        
        // コネクションIDを生成
        let mut scid = [0u8; quiche::MAX_CONN_ID_LEN];
        let rng = SystemRandom::new();
        rng.fill(&mut scid)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "Failed to generate connection ID"))?;
        let scid = ConnectionId::from_ref(&scid);
        
        // 接続を開始
        let conn = quiche::connect(
            Some("localhost"),
            &scid,
            local_addr,
            server_addr,
            &mut config,
        )?;
        
        Ok(Self {
            conn,
            socket,
            peer_addr: server_addr,
            local_addr,
            h3_conn: None,
        })
    }
    
    /// ハンドシェイクを完了
    pub fn handshake(&mut self, timeout: Duration) -> Result<(), Box<dyn std::error::Error>> {
        let mut buf = [0u8; 65535];
        let mut out = [0u8; 1350];
        let start = Instant::now();
        
        // 初期パケットを送信
        let (write, _) = self.conn.send(&mut out)?;
        if write > 0 {
            self.socket.send(&out[..write])?;
        }
        
        // ハンドシェイクを完了
        while !self.conn.is_established() {
            if start.elapsed() > timeout {
                return Err("Handshake timeout".into());
            }
            
            // パケットを受信
            match self.socket.recv(&mut buf) {
                Ok(len) => {
                    let recv_info = quiche::RecvInfo {
                        from: self.peer_addr,
                        to: self.local_addr,
                    };
                    match self.conn.recv(&mut buf[..len], recv_info) {
                        Ok(_) => {}
                        Err(quiche::Error::Done) => {}
                        Err(e) => return Err(format!("Recv error: {}", e).into()),
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(format!("Socket recv error: {}", e).into()),
            }
            
            // パケットを送信
            loop {
                match self.conn.send(&mut out) {
                    Ok((write, _)) if write > 0 => {
                        self.socket.send(&out[..write])?;
                    }
                    Ok(_) => break,
                    Err(quiche::Error::Done) => break,
                    Err(e) => return Err(format!("Send error: {}", e).into()),
                }
            }
            
            // タイマー処理
            if let Some(timeout) = self.conn.timeout() {
                if start.elapsed() > timeout {
                    self.conn.on_timeout();
                }
            }
        }
        
        // HTTP/3接続を確立
        let h3_config = h3::Config::new()?;
        let h3_conn = h3::Connection::with_transport(&mut self.conn, &h3_config)?;
        self.h3_conn = Some(h3_conn);
        
        Ok(())
    }
    
    /// HTTP/3リクエストを送信
    pub fn send_request(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        let h3_conn = self.h3_conn.as_mut()
            .ok_or("HTTP/3 connection not established")?;
        
        // ヘッダーを構築
        let mut h3_headers = vec![
            h3::Header::new(b":method", method.as_bytes()),
            h3::Header::new(b":path", path.as_bytes()),
            h3::Header::new(b":scheme", b"https"),
            h3::Header::new(b":authority", b"localhost"),
        ];
        
        for (name, value) in headers {
            h3_headers.push(h3::Header::new(name.as_bytes(), value.as_bytes()));
        }
        
        // リクエストを送信
        let stream_id = h3_conn.send_request(&mut self.conn, &h3_headers, body.is_some())?;
        
        // ボディを送信
        if let Some(body) = body {
            h3_conn.send_body(&mut self.conn, stream_id, body, true)?;
        }
        
        // パケットを送信
        let mut out = [0u8; 1350];
        loop {
            match self.conn.send(&mut out) {
                Ok((write, _)) if write > 0 => {
                    self.socket.send(&out[..write])?;
                }
                Ok(_) => break,
                Err(quiche::Error::Done) => break,
                Err(e) => return Err(format!("Send error: {}", e).into()),
            }
        }
        
        Ok(stream_id)
    }
    
    /// レスポンスを受信
    pub fn recv_response(
        &mut self,
        stream_id: u64,
        timeout: Duration,
    ) -> Result<(Vec<u8>, u16), Box<dyn std::error::Error>> {
        let mut buf = [0u8; 65535];
        let mut out = [0u8; 1350];
        let mut response_body = Vec::new();
        let mut status_code = 200u16;
        let start = Instant::now();
        
        loop {
            if start.elapsed() > timeout {
                return Err("Response timeout".into());
            }
            
            // パケットを受信
            match self.socket.recv(&mut buf) {
                Ok(len) => {
                    let recv_info = quiche::RecvInfo {
                        from: self.peer_addr,
                        to: self.local_addr,
                    };
                    match self.conn.recv(&mut buf[..len], recv_info) {
                        Ok(_) => {}
                        Err(quiche::Error::Done) => {}
                        Err(e) => return Err(format!("Recv error: {}", e).into()),
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(format!("Socket recv error: {}", e).into()),
            }
            
            // HTTP/3レスポンスを処理
            let h3_conn = self.h3_conn.as_mut()
                .ok_or("HTTP/3 connection not established")?;
            
            // イベントをポーリング
            loop {
                match h3_conn.poll(&mut self.conn) {
                    Ok((id, h3::Event::Headers { list, .. })) if id == stream_id => {
                        // ステータスコードを取得
                        for header in &list {
                            if header.name() == b":status" {
                                if let Ok(s) = std::str::from_utf8(header.value()) {
                                    if let Ok(code) = s.parse::<u16>() {
                                        status_code = code;
                                    }
                                }
                            }
                        }
                    }
                    Ok((id, h3::Event::Finished { .. })) if id == stream_id => {
                        // ストリームが終了
                        return Ok((response_body, status_code));
                    }
                    Ok(_) => {
                        // 他のストリームのイベントは無視
                    }
                    Err(h3::Error::Done) => break,
                    Err(e) => return Err(format!("H3 poll error: {}", e).into()),
                }
            }
            
            // ボディを受信
            let mut body_buf = [0u8; 4096];
            loop {
                match h3_conn.recv_body(&mut self.conn, stream_id, &mut body_buf) {
                    Ok(len) => {
                        if len > 0 {
                            response_body.extend_from_slice(&body_buf[..len]);
                        } else {
                            // データがない場合は終了
                            break;
                        }
                    }
                    Err(h3::Error::Done) => break,
                    Err(e) => return Err(format!("H3 body recv error: {}", e).into()),
                }
            }
            
            // レスポンスボディが空でない場合、または既にステータスコードが設定されている場合は返す
            if !response_body.is_empty() || status_code != 200 {
                return Ok((response_body, status_code));
            }
            
            // タイマー処理
            if let Some(timeout) = self.conn.timeout() {
                if start.elapsed() > timeout {
                    self.conn.on_timeout();
                }
            }
            
            // パケットを送信
            loop {
                match self.conn.send(&mut out) {
                    Ok((write, _)) if write > 0 => {
                        self.socket.send(&out[..write])?;
                    }
                    Ok(_) => break,
                    Err(quiche::Error::Done) => break,
                    Err(e) => return Err(format!("Send error: {}", e).into()),
                }
            }
        }
    }
    
    /// 接続を閉じる
    pub fn close(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.conn.close(true, 0x00, b"test complete")?;
        let mut out = [0u8; 1350];
        loop {
            match self.conn.send(&mut out) {
                Ok((write, _)) if write > 0 => {
                    self.socket.send(&out[..write])?;
                }
                Ok(_) => break,
                Err(quiche::Error::Done) => break,
                Err(_) => break,
            }
        }
        Ok(())
    }
}
