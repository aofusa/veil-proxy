//! # HTTP/3 サーバー (monoio + quiche ベース)
//!
//! monoio (io_uring) と Cloudflare quiche を使用した HTTP/3 サーバー実装。
//! thread-per-core モデルで、各コネクションを独立した非同期タスクで処理します。
//!
//! ## 設計ポイント
//!
//! - **io_uring 活用**: monoio の UdpSocket で高効率な UDP I/O
//! - **コネクションごとのタスク分離**: monoio::spawn で各接続を独立管理
//! - **タイマー管理**: quiche::timeout() と monoio::time::sleep の連携
//! - **H3 インスタンスの永続化**: QPACK 動的テーブル等の状態を維持

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use monoio::net::udp::UdpSocket;
use quiche::{h3, Config, ConnectionId};
use quiche::h3::NameValue;
use ring::rand::*;

use ftlog::{info, warn, error, debug};

/// HTTP/3 サーバー設定
#[derive(Clone)]
pub struct Http3ServerConfig {
    /// TLS 証明書パス
    pub cert_path: String,
    /// TLS 秘密鍵パス
    pub key_path: String,
    /// 最大アイドルタイムアウト（ミリ秒）
    pub max_idle_timeout: u64,
    /// 最大 UDP ペイロードサイズ
    pub max_udp_payload_size: u64,
    /// 初期最大データサイズ
    pub initial_max_data: u64,
    /// 初期最大ストリームデータサイズ（双方向）
    pub initial_max_stream_data_bidi_local: u64,
    /// 初期最大ストリームデータサイズ（双方向リモート）
    pub initial_max_stream_data_bidi_remote: u64,
    /// 初期最大ストリームデータサイズ（単方向）
    pub initial_max_stream_data_uni: u64,
    /// 初期最大双方向ストリーム数
    pub initial_max_streams_bidi: u64,
    /// 初期最大単方向ストリーム数
    pub initial_max_streams_uni: u64,
}

impl Default for Http3ServerConfig {
    fn default() -> Self {
        Self {
            cert_path: String::new(),
            key_path: String::new(),
            max_idle_timeout: 30000,
            max_udp_payload_size: 1350,
            initial_max_data: 10_000_000,
            initial_max_stream_data_bidi_local: 1_000_000,
            initial_max_stream_data_bidi_remote: 1_000_000,
            initial_max_stream_data_uni: 1_000_000,
            initial_max_streams_bidi: 100,
            initial_max_streams_uni: 100,
        }
    }
}

/// HTTP/3 コネクションハンドラー
/// 
/// quiche::Connection と h3::Connection をセットで保持し、
/// コネクションの寿命の間、同一のインスタンスを維持します。
struct Http3Handler {
    /// QUIC コネクション
    conn: quiche::Connection,
    /// HTTP/3 コネクション (確立後に Some)
    h3_conn: Option<h3::Connection>,
    /// リモートアドレス
    peer_addr: SocketAddr,
    /// 部分的なレスポンス（ストリーム ID → (ボディ, 書き込み済みバイト数)）
    partial_responses: HashMap<u64, (Vec<u8>, usize)>,
}

impl Http3Handler {
    /// 新しいハンドラーを作成
    fn new(conn: quiche::Connection, peer_addr: SocketAddr) -> Self {
        Self {
            conn,
            h3_conn: None,
            peer_addr,
            partial_responses: HashMap::new(),
        }
    }

    /// HTTP/3 コネクションを初期化（QUIC 確立後）
    fn init_h3(&mut self) -> io::Result<()> {
        if self.h3_conn.is_none() && self.conn.is_established() {
            let h3_config = h3::Config::new()
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            let h3 = h3::Connection::with_transport(&mut self.conn, &h3_config)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            self.h3_conn = Some(h3);
            info!("[HTTP/3] HTTP/3 connection established from {}", self.peer_addr);
        }
        Ok(())
    }

    /// HTTP/3 イベントを処理
    fn process_h3_events(&mut self) -> io::Result<()> {
        // 処理するリクエストを収集
        let mut pending_requests: Vec<(u64, Vec<h3::Header>)> = Vec::new();
        
        if let Some(ref mut h3_conn) = self.h3_conn {
            loop {
                match h3_conn.poll(&mut self.conn) {
                    Ok((stream_id, h3::Event::Headers { list, more_frames: _ })) => {
                        pending_requests.push((stream_id, list));
                    }
                    Ok((stream_id, h3::Event::Data)) => {
                        // リクエストボディを読み込み
                        let mut body = vec![0u8; 4096];
                        while let Ok(read) = h3_conn.recv_body(&mut self.conn, stream_id, &mut body) {
                            if read == 0 {
                                break;
                            }
                        }
                    }
                    Ok((_stream_id, h3::Event::Finished)) => {}
                    Ok((_stream_id, h3::Event::Reset(_))) => {}
                    Ok((_flow_id, h3::Event::GoAway)) => {}
                    Ok((_, h3::Event::PriorityUpdate)) => {}
                    Err(h3::Error::Done) => break,
                    Err(e) => {
                        warn!("[HTTP/3] h3 poll error: {}", e);
                        break;
                    }
                }
            }
        }

        // リクエストを処理
        for (stream_id, headers) in pending_requests {
            self.handle_request(stream_id, &headers)?;
        }

        // 部分的なレスポンスを送信
        self.flush_partial_responses()?;

        Ok(())
    }

    /// HTTP/3 リクエストを処理
    fn handle_request(&mut self, stream_id: u64, headers: &[h3::Header]) -> io::Result<()> {
        let h3_conn = match &mut self.h3_conn {
            Some(h3) => h3,
            None => return Ok(()),
        };

        let mut method = None;
        let mut path = None;

        for header in headers {
            match header.name() {
                b":method" => method = Some(header.value()),
                b":path" => path = Some(header.value()),
                _ => {}
            }
        }

        let method = method.unwrap_or(b"GET");
        let path = path.unwrap_or(b"/");

        debug!(
            "[HTTP/3] Request: {} {} (stream {})",
            String::from_utf8_lossy(method),
            String::from_utf8_lossy(path),
            stream_id
        );

        // シンプルなレスポンス
        let body = b"HTTP/3 OK\n";
        let response_headers = vec![
            h3::Header::new(b":status", b"200"),
            h3::Header::new(b"content-type", b"text/plain"),
            h3::Header::new(b"content-length", body.len().to_string().as_bytes()),
            h3::Header::new(b"server", b"zerocopy-server/http3"),
            h3::Header::new(b"alt-svc", b"clear"),
        ];

        // ヘッダーを送信
        match h3_conn.send_response(&mut self.conn, stream_id, &response_headers, false) {
            Ok(()) => {}
            Err(h3::Error::StreamBlocked) => return Ok(()),
            Err(e) => {
                warn!("[HTTP/3] send_response error: {}", e);
                return Ok(());
            }
        }

        // ボディを送信
        match h3_conn.send_body(&mut self.conn, stream_id, body, true) {
            Ok(_) => {}
            Err(h3::Error::Done) => {
                self.partial_responses.insert(stream_id, (body.to_vec(), 0));
            }
            Err(e) => {
                warn!("[HTTP/3] send_body error: {}", e);
            }
        }

        Ok(())
    }

    /// 部分的なレスポンスをフラッシュ
    fn flush_partial_responses(&mut self) -> io::Result<()> {
        let h3_conn = match &mut self.h3_conn {
            Some(h3) => h3,
            None => return Ok(()),
        };

        let mut completed = Vec::new();
        for (&stream_id, (body, written)) in &mut self.partial_responses {
            if *written < body.len() {
                match h3_conn.send_body(&mut self.conn, stream_id, &body[*written..], true) {
                    Ok(sent) => {
                        *written += sent;
                        if *written >= body.len() {
                            completed.push(stream_id);
                        }
                    }
                    Err(h3::Error::Done) => {}
                    Err(e) => {
                        warn!("[HTTP/3] send_body error: {}", e);
                        completed.push(stream_id);
                    }
                }
            } else {
                completed.push(stream_id);
            }
        }
        for stream_id in completed {
            self.partial_responses.remove(&stream_id);
        }

        Ok(())
    }
}

/// コネクション管理（Rc<RefCell> で共有）
type ConnectionMap = Rc<RefCell<HashMap<ConnectionId<'static>, Http3Handler>>>;

/// HTTP/3 サーバーを起動（monoio ランタイム上で実行）
/// 
/// この関数は monoio のスレッド内から呼び出す必要があります。
pub async fn run_http3_server_async(
    bind_addr: SocketAddr,
    config: Http3ServerConfig,
) -> io::Result<()> {
    // QUIC 設定を作成
    let mut quic_config = Config::new(quiche::PROTOCOL_VERSION)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;

    // TLS 証明書を設定
    quic_config.load_cert_chain_from_pem_file(&config.cert_path)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("cert load error: {}", e)))?;
    quic_config.load_priv_key_from_pem_file(&config.key_path)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("key load error: {}", e)))?;

    // QUIC パラメータを設定
    quic_config.set_max_idle_timeout(config.max_idle_timeout);
    quic_config.set_max_recv_udp_payload_size(config.max_udp_payload_size as usize);
    quic_config.set_max_send_udp_payload_size(config.max_udp_payload_size as usize);
    quic_config.set_initial_max_data(config.initial_max_data);
    quic_config.set_initial_max_stream_data_bidi_local(config.initial_max_stream_data_bidi_local);
    quic_config.set_initial_max_stream_data_bidi_remote(config.initial_max_stream_data_bidi_remote);
    quic_config.set_initial_max_stream_data_uni(config.initial_max_stream_data_uni);
    quic_config.set_initial_max_streams_bidi(config.initial_max_streams_bidi);
    quic_config.set_initial_max_streams_uni(config.initial_max_streams_uni);
    quic_config.set_disable_active_migration(true);
    quic_config.enable_early_data();

    // HTTP/3 用の ALPN を設定
    quic_config.set_application_protos(h3::APPLICATION_PROTOCOL)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;

    // 設定を Rc で共有（quiche::Config は Clone できないため）
    let quic_config = Rc::new(RefCell::new(quic_config));

    // UDP ソケットを作成（monoio io_uring ベース）
    let socket = UdpSocket::bind(bind_addr)?;
    let socket = Rc::new(socket);
    let local_addr = bind_addr;

    info!("[HTTP/3] Server listening on {} (QUIC/UDP, monoio io_uring)", bind_addr);

    // コネクション管理
    let connections: ConnectionMap = Rc::new(RefCell::new(HashMap::new()));

    // 乱数生成器
    let rng = SystemRandom::new();

    // メインループ: パケット受信とディスパッチ
    loop {
        // 最小タイムアウトを計算
        let timeout = {
            let conns = connections.borrow();
            conns.values()
                .filter_map(|h| h.conn.timeout())
                .min()
                .unwrap_or(Duration::from_millis(100))
        };

        // タイムアウト付きでパケット受信
        let recv_buf = vec![0u8; 65536];
        let recv_result = monoio::time::timeout(timeout, socket.recv_from(recv_buf)).await;

        // タイムアウト処理
        {
            let mut conns = connections.borrow_mut();
            let mut closed = Vec::new();
            for (cid, handler) in conns.iter_mut() {
                handler.conn.on_timeout();
                if handler.conn.is_closed() {
                    closed.push(cid.clone());
                }
            }
            for cid in closed {
                info!("[HTTP/3] Connection closed (timeout)");
                conns.remove(&cid);
            }
        }

        let (recv_buf, len, from) = match recv_result {
            Ok((Ok((len, from)), buf)) => (buf, len, from),
            Ok((Err(e), _)) => {
                if e.kind() != io::ErrorKind::WouldBlock {
                    error!("[HTTP/3] recv_from error: {}", e);
                }
                continue;
            }
            Err(_) => {
                // タイムアウト - ループを継続
                continue;
            }
        };

        let mut pkt_buf = recv_buf[..len].to_vec();

        // パケットヘッダーを解析
        let hdr = match quiche::Header::from_slice(&mut pkt_buf, quiche::MAX_CONN_ID_LEN) {
            Ok(v) => v,
            Err(e) => {
                warn!("[HTTP/3] Invalid packet header: {}", e);
                continue;
            }
        };

        // コネクションを検索または作成
        let conn_id = {
            let mut conns = connections.borrow_mut();
            
            if !conns.contains_key(&hdr.dcid) {
                if hdr.ty != quiche::Type::Initial {
                    debug!("[HTTP/3] Non-initial packet for unknown connection");
                    continue;
                }

                // 新規コネクション
                let mut scid = [0u8; quiche::MAX_CONN_ID_LEN];
                rng.fill(&mut scid)
                    .map_err(|_| io::Error::new(io::ErrorKind::Other, "RNG error"))?;
                let scid = ConnectionId::from_ref(&scid).into_owned();

                let mut config_ref = quic_config.borrow_mut();
                let conn = quiche::accept(
                    &scid,
                    None,
                    local_addr,
                    from,
                    &mut config_ref,
                )
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

                info!("[HTTP/3] New connection from {}", from);

                let handler = Http3Handler::new(conn, from);
                conns.insert(scid.clone(), handler);

                scid
            } else {
                hdr.dcid.into_owned()
            }
        };

        // パケットを処理
        {
            let mut conns = connections.borrow_mut();
            if let Some(handler) = conns.get_mut(&conn_id) {
                let recv_info = quiche::RecvInfo {
                    from,
                    to: local_addr,
                };

                // パケットを受信
                let mut pkt_buf_mut = pkt_buf.to_vec();
                match handler.conn.recv(&mut pkt_buf_mut, recv_info) {
                    Ok(_) => {}
                    Err(e) => {
                        warn!("[HTTP/3] recv error: {}", e);
                        continue;
                    }
                }

                // HTTP/3 初期化
                if let Err(e) = handler.init_h3() {
                    warn!("[HTTP/3] init_h3 error: {}", e);
                }

                // HTTP/3 イベント処理
                if let Err(e) = handler.process_h3_events() {
                    warn!("[HTTP/3] process_h3_events error: {}", e);
                }
            }
        }

        // 送信処理
        {
            let mut conns = connections.borrow_mut();
            let mut send_buf = vec![0u8; 1350];
            let mut closed = Vec::new();
            
            for (cid, handler) in conns.iter_mut() {
                loop {
                    let (write, send_info) = match handler.conn.send(&mut send_buf) {
                        Ok(v) => v,
                        Err(quiche::Error::Done) => break,
                        Err(e) => {
                            error!("[HTTP/3] send error: {}", e);
                            handler.conn.close(false, 0x1, b"send error").ok();
                            break;
                        }
                    };

                    let send_data = send_buf[..write].to_vec();
                    let socket_clone = socket.clone();
                    let target = send_info.to;
                    
                    // 非同期送信（spawn しない、直接 await）
                    // monoio の UdpSocket は send_to が async
                    let _ = socket_clone.send_to(send_data, target).await;
                }

                if handler.conn.is_closed() {
                    info!("[HTTP/3] Connection closed from {}", handler.peer_addr);
                    closed.push(cid.clone());
                }
            }

            for cid in closed {
                conns.remove(&cid);
            }
        }
    }
}

/// HTTP/3 サーバーを起動（同期ラッパー）
/// 
/// 別スレッドで monoio ランタイムを作成して実行します。
pub fn run_http3_server(
    bind_addr: SocketAddr,
    config: Http3ServerConfig,
) -> io::Result<()> {
    use monoio::RuntimeBuilder;

    let mut rt = RuntimeBuilder::<monoio::IoUringDriver>::new()
        .enable_timer()
        .build()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Runtime error: {}", e)))?;

    rt.block_on(async move {
        run_http3_server_async(bind_addr, config).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = Http3ServerConfig::default();
        assert_eq!(config.max_idle_timeout, 30000);
        assert_eq!(config.max_udp_payload_size, 1350);
    }
}
