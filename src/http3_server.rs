//! # HTTP/3 サーバー (quiche ベース)
//!
//! Cloudflare quiche を使用した HTTP/3 サーバー実装。
//! mio を使用してイベントループを実行し、
//! monoio ワーカースレッドとは別のスレッドで動作します。

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use mio::net::UdpSocket;
use mio::{Events, Interest, Poll, Token};
use quiche::{h3, Config, ConnectionId};
use quiche::h3::NameValue;
use ring::rand::*;

use ftlog::{info, warn, error, debug};

/// HTTP/3 サーバー設定
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

/// クライアント接続情報
struct Client {
    /// QUIC コネクション
    conn: quiche::Connection,
    /// HTTP/3 コネクション
    http3_conn: Option<h3::Connection>,
    /// 部分的なリクエスト（ストリーム ID → ヘッダー）
    #[allow(dead_code)]
    partial_requests: HashMap<u64, Vec<h3::Header>>,
    /// 部分的なレスポンス（ストリーム ID → (ボディ, 書き込み済みバイト数)）
    partial_responses: HashMap<u64, (Vec<u8>, usize)>,
}

/// HTTP/3 サーバーを起動
pub fn run_http3_server(
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

    // HTTP/3 設定
    let h3_config = h3::Config::new()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;

    // UDP ソケットを作成
    let mut socket = UdpSocket::bind(bind_addr)?;
    
    info!("[HTTP/3] Server listening on {} (QUIC/UDP)", bind_addr);

    // mio イベントループを設定
    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(1024);
    
    const SOCKET_TOKEN: Token = Token(0);
    poll.registry().register(&mut socket, SOCKET_TOKEN, Interest::READABLE)?;

    // クライアント管理
    let mut clients: HashMap<ConnectionId<'static>, Client> = HashMap::new();
    let mut recv_buf = vec![0u8; 65536];
    let mut send_buf = vec![0u8; 1350];

    // 乱数生成器
    let rng = SystemRandom::new();

    loop {
        // タイムアウト計算
        let timeout = clients.values()
            .filter_map(|c| c.conn.timeout())
            .min()
            .unwrap_or(Duration::from_millis(100));

        poll.poll(&mut events, Some(timeout))?;

        // タイムアウト処理
        for client in clients.values_mut() {
            client.conn.on_timeout();
        }

        // 受信イベント処理
        'recv: loop {
            let (len, from) = match socket.recv_from(&mut recv_buf) {
                Ok(v) => v,
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break 'recv,
                Err(e) => {
                    error!("[HTTP/3] recv_from error: {}", e);
                    break 'recv;
                }
            };

            let pkt_buf = &mut recv_buf[..len];

            // パケットヘッダーを解析
            let hdr = match quiche::Header::from_slice(pkt_buf, quiche::MAX_CONN_ID_LEN) {
                Ok(v) => v,
                Err(e) => {
                    warn!("[HTTP/3] Invalid packet header: {}", e);
                    continue 'recv;
                }
            };

            // 既存コネクションを検索
            let conn_id = if !clients.contains_key(&hdr.dcid) {
                if hdr.ty != quiche::Type::Initial {
                    debug!("[HTTP/3] Non-initial packet for unknown connection");
                    continue 'recv;
                }

                // 新規コネクション
                let mut scid = [0u8; quiche::MAX_CONN_ID_LEN];
                rng.fill(&mut scid).map_err(|_| io::Error::new(io::ErrorKind::Other, "RNG error"))?;
                let scid = ConnectionId::from_ref(&scid).into_owned();

                let local_addr = socket.local_addr()?;
                let conn = quiche::accept(
                    &scid,
                    None,
                    local_addr,
                    from,
                    &mut quic_config,
                )
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

                info!("[HTTP/3] New connection from {}", from);

                clients.insert(scid.clone(), Client {
                    conn,
                    http3_conn: None,
                    partial_requests: HashMap::new(),
                    partial_responses: HashMap::new(),
                });

                scid
            } else {
                hdr.dcid.into_owned()
            };

            let client = clients.get_mut(&conn_id).unwrap();

            // パケットを処理
            let recv_info = quiche::RecvInfo {
                from,
                to: socket.local_addr()?,
            };

            match client.conn.recv(pkt_buf, recv_info) {
                Ok(_) => {}
                Err(e) => {
                    warn!("[HTTP/3] recv error: {}", e);
                    continue 'recv;
                }
            }

            // HTTP/3 ハンドシェイク完了チェック
            if client.http3_conn.is_none() && client.conn.is_established() {
                match h3::Connection::with_transport(&mut client.conn, &h3_config) {
                    Ok(h3) => {
                        info!("[HTTP/3] HTTP/3 connection established from {}", from);
                        client.http3_conn = Some(h3);
                    }
                    Err(e) => {
                        error!("[HTTP/3] h3 setup error: {}", e);
                        continue 'recv;
                    }
                }
            }

            // HTTP/3 イベント処理
            if let Some(ref mut h3_conn) = client.http3_conn {
                loop {
                    match h3_conn.poll(&mut client.conn) {
                        Ok((stream_id, h3::Event::Headers { list, more_frames })) => {
                            handle_request(
                                h3_conn,
                                &mut client.conn,
                                stream_id,
                                &list,
                                more_frames,
                                &mut client.partial_responses,
                            );
                        }
                        Ok((stream_id, h3::Event::Data)) => {
                            // リクエストボディを読み込み（今は無視）
                            let mut body = vec![0u8; 4096];
                            while let Ok(read) = h3_conn.recv_body(&mut client.conn, stream_id, &mut body) {
                                if read == 0 {
                                    break;
                                }
                            }
                        }
                        Ok((_stream_id, h3::Event::Finished)) => {
                            // ストリーム終了
                        }
                        Ok((_stream_id, h3::Event::Reset(_))) => {
                            // ストリームリセット
                        }
                        Ok((_flow_id, h3::Event::GoAway)) => {
                            // GOAWAY 受信
                        }
                        Ok((_, h3::Event::PriorityUpdate)) => {}
                        Err(h3::Error::Done) => break,
                        Err(e) => {
                            warn!("[HTTP/3] h3 poll error: {}", e);
                            break;
                        }
                    }
                }
            }

            // 部分的なレスポンスを送信
            if let Some(ref mut h3_conn) = client.http3_conn {
                let mut completed = Vec::new();
                for (&stream_id, (body, written)) in &mut client.partial_responses {
                    if *written < body.len() {
                        match h3_conn.send_body(&mut client.conn, stream_id, &body[*written..], true) {
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
                    client.partial_responses.remove(&stream_id);
                }
            }
        }

        // 送信処理
        for client in clients.values_mut() {
            loop {
                let (write, send_info) = match client.conn.send(&mut send_buf) {
                    Ok(v) => v,
                    Err(quiche::Error::Done) => break,
                    Err(e) => {
                        error!("[HTTP/3] send error: {}", e);
                        client.conn.close(false, 0x1, b"send error").ok();
                        break;
                    }
                };

                if let Err(e) = socket.send_to(&send_buf[..write], send_info.to) {
                    if e.kind() != io::ErrorKind::WouldBlock {
                        error!("[HTTP/3] send_to error: {}", e);
                    }
                }
            }
        }

        // クローズ済みコネクションを削除
        clients.retain(|_, c| {
            if c.conn.is_closed() {
                info!("[HTTP/3] Connection closed");
                false
            } else {
                true
            }
        });
    }
}

/// HTTP/3 リクエストを処理
fn handle_request(
    h3_conn: &mut h3::Connection,
    conn: &mut quiche::Connection,
    stream_id: u64,
    headers: &[h3::Header],
    _has_body: bool,
    partial_responses: &mut HashMap<u64, (Vec<u8>, usize)>,
) {
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
    match h3_conn.send_response(conn, stream_id, &response_headers, false) {
        Ok(()) => {}
        Err(h3::Error::StreamBlocked) => {
            // ブロックされた場合は後で再試行
            return;
        }
        Err(e) => {
            warn!("[HTTP/3] send_response error: {}", e);
            return;
        }
    }

    // ボディを送信
    match h3_conn.send_body(conn, stream_id, body, true) {
        Ok(_) => {}
        Err(h3::Error::Done) => {
            // 後で再試行
            partial_responses.insert(stream_id, (body.to_vec(), 0));
        }
        Err(e) => {
            warn!("[HTTP/3] send_body error: {}", e);
        }
    }
}
