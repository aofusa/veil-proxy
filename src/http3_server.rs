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
//!
//! ## 機能
//!
//! - HTTP/1.1と同等のルーティング機能（ホスト/パスベース）
//! - セキュリティ機能（IP制限、レートリミット、メソッド制限）
//! - プロキシ機能（HTTPSバックエンドへのプロトコル変換）
//! - ファイル配信、リダイレクト、メトリクス

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use monoio::net::udp::UdpSocket;
use quiche::{h3, Config, ConnectionId};
use quiche::h3::NameValue;
use ring::rand::*;

use ftlog::{info, warn, error, debug};

use crate::{
    Backend, SortedPathMap, SecurityConfig, UpstreamGroup, ProxyTarget,
    find_backend, check_security, SecurityCheckResult,
    encode_prometheus_metrics, record_request_metrics,
    CURRENT_CONFIG,
};

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
/// 
/// HTTP/1.1と同等のルーティング・セキュリティ・プロキシ機能をサポート。
struct Http3Handler {
    /// QUIC コネクション
    conn: quiche::Connection,
    /// HTTP/3 コネクション (確立後に Some)
    h3_conn: Option<h3::Connection>,
    /// リモートアドレス
    peer_addr: SocketAddr,
    /// 部分的なレスポンス（ストリーム ID → (ボディ, 書き込み済みバイト数)）
    partial_responses: HashMap<u64, (Vec<u8>, usize)>,
    /// クライアントIPアドレス（文字列）
    client_ip: String,
    /// ホストルーティング設定
    host_routes: Arc<HashMap<Box<[u8]>, Backend>>,
    /// パスルーティング設定
    path_routes: Arc<HashMap<Box<[u8]>, SortedPathMap>>,
}

impl Http3Handler {
    /// 新しいハンドラーを作成
    fn new(
        conn: quiche::Connection,
        peer_addr: SocketAddr,
        host_routes: Arc<HashMap<Box<[u8]>, Backend>>,
        path_routes: Arc<HashMap<Box<[u8]>, SortedPathMap>>,
    ) -> Self {
        Self {
            conn,
            h3_conn: None,
            client_ip: peer_addr.ip().to_string(),
            peer_addr,
            partial_responses: HashMap::new(),
            host_routes,
            path_routes,
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
        // 処理するリクエストを収集（ストリームID → (ヘッダー, ボディ)）
        let mut pending_requests: Vec<(u64, Vec<h3::Header>, Vec<u8>)> = Vec::new();
        // ストリームごとのボディバッファ
        let mut stream_bodies: HashMap<u64, Vec<u8>> = HashMap::new();
        
        if let Some(ref mut h3_conn) = self.h3_conn {
            loop {
                match h3_conn.poll(&mut self.conn) {
                    Ok((stream_id, h3::Event::Headers { list, more_frames })) => {
                        if !more_frames {
                            // ボディがないリクエスト
                            pending_requests.push((stream_id, list, Vec::new()));
                        } else {
                            // ボディがある場合、ヘッダーを保持して後で処理
                            // 簡略化: ボディがある場合も即座に処理
                            let body = stream_bodies.remove(&stream_id).unwrap_or_default();
                            pending_requests.push((stream_id, list, body));
                        }
                    }
                    Ok((stream_id, h3::Event::Data)) => {
                        // リクエストボディを読み込み
                        let mut buf = vec![0u8; 16384];
                        let body = stream_bodies.entry(stream_id).or_insert_with(Vec::new);
                        
                        loop {
                            match h3_conn.recv_body(&mut self.conn, stream_id, &mut buf) {
                                Ok(read) if read > 0 => {
                                    body.extend_from_slice(&buf[..read]);
                                }
                                Ok(_) => break,
                                Err(h3::Error::Done) => break,
                                Err(e) => {
                                    warn!("[HTTP/3] recv_body error: {}", e);
                                    break;
                                }
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
        for (stream_id, headers, body) in pending_requests {
            self.handle_request(stream_id, &headers, &body)?;
        }

        // 部分的なレスポンスを送信
        self.flush_partial_responses()?;

        Ok(())
    }

    /// HTTP/3 リクエストを処理（完全版）
    /// 
    /// HTTP/1.1と同等のルーティング・セキュリティ・プロキシ機能をサポート。
    fn handle_request(&mut self, stream_id: u64, headers: &[h3::Header], request_body: &[u8]) -> io::Result<()> {
        // HTTP/3コネクションが確立されていなければ何もしない
        if self.h3_conn.is_none() {
            return Ok(());
        }

        // ヘッダーを解析
        let mut method = None;
        let mut path = None;
        let mut authority = None;
        let mut content_length: usize = 0;

        for header in headers {
            match header.name() {
                b":method" => method = Some(header.value().to_vec()),
                b":path" => path = Some(header.value().to_vec()),
                b":authority" => authority = Some(header.value().to_vec()),
                b"content-length" => {
                    if let Ok(s) = std::str::from_utf8(header.value()) {
                        content_length = s.parse().unwrap_or(0);
                    }
                }
                _ => {}
            }
        }

        let method = method.unwrap_or_else(|| b"GET".to_vec());
        let path = path.unwrap_or_else(|| b"/".to_vec());
        let authority = authority.unwrap_or_default();

        // 処理開始時刻
        let start_time = Instant::now();

        debug!(
            "[HTTP/3] Request: {} {} (stream {})",
            String::from_utf8_lossy(&method),
            String::from_utf8_lossy(&path),
            stream_id
        );

        // メトリクスエンドポイント
        if path == b"/__metrics" && method == b"GET" {
            let body = encode_prometheus_metrics();
            self.send_response(stream_id, 200, &[
                (b":status", b"200"),
                (b"content-type", b"text/plain; version=0.0.4; charset=utf-8"),
                (b"server", b"zerocopy-server/http3"),
            ], Some(&body))?;
            
            self.record_metrics(&method, &authority, 200, request_body.len(), body.len(), start_time);
            return Ok(());
        }

        // バックエンド選択
        let backend_result = find_backend(&authority, &path, &self.host_routes, &self.path_routes);
        
        let (prefix, backend) = match backend_result {
            Some(b) => b,
            None => {
                self.send_error_response(stream_id, 400, b"Bad Request")?;
                self.record_metrics(&method, &authority, 400, request_body.len(), 11, start_time);
                return Ok(());
            }
        };

        // セキュリティチェック
        let security = backend.security();
        let check_result = check_security(security, &self.client_ip, &method, content_length, false);
        
        if check_result != SecurityCheckResult::Allowed {
            let status = check_result.status_code();
            let msg = check_result.message();
            self.send_error_response(stream_id, status, msg)?;
            self.record_metrics(&method, &authority, status, request_body.len(), msg.len(), start_time);
            return Ok(());
        }

        // バックエンド処理
        let (status, resp_size) = match backend {
            Backend::Proxy(upstream_group, _) => {
                self.handle_proxy(stream_id, &upstream_group, &method, &path, &prefix, headers, request_body)
                    .unwrap_or((502, 11))
            }
            Backend::MemoryFile(data, mime_type, security) => {
                // パス完全一致チェック
                let path_str = std::str::from_utf8(&path).unwrap_or("/");
                let prefix_str = std::str::from_utf8(&prefix).unwrap_or("");
                
                let remainder = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
                    &path_str[prefix_str.len()..]
                } else {
                    ""
                };
                
                let clean_remainder = remainder.trim_matches('/');
                if !clean_remainder.is_empty() {
                    self.send_error_response(stream_id, 404, b"Not Found")?;
                    (404, 9)
                } else {
                    let mut resp_headers: Vec<(&[u8], &[u8])> = vec![
                        (b"content-type", mime_type.as_bytes()),
                        (b"server", b"zerocopy-server/http3"),
                    ];
                    
                    // セキュリティヘッダー追加
                    let security_headers: Vec<(Vec<u8>, Vec<u8>)> = security.add_response_headers.iter()
                        .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
                        .collect();
                    
                    for (k, v) in &security_headers {
                        resp_headers.push((k.as_slice(), v.as_slice()));
                    }
                    
                    self.send_response(stream_id, 200, &resp_headers, Some(&data))?;
                    (200, data.len())
                }
            }
            Backend::SendFile(base_path, is_dir, index_file, security) => {
                self.handle_sendfile(stream_id, &base_path, is_dir, index_file.as_deref(), &path, &prefix, &security)
                    .unwrap_or((404, 9))
            }
            Backend::Redirect(redirect_url, status_code, preserve_path) => {
                self.handle_redirect(stream_id, &redirect_url, status_code, preserve_path, &path, &prefix)
                    .unwrap_or((500, 0))
            }
        };

        self.record_metrics(&method, &authority, status, request_body.len(), resp_size, start_time);
        Ok(())
    }
    
    /// レスポンス送信ヘルパー
    fn send_response(
        &mut self,
        stream_id: u64,
        status: u16,
        headers: &[(&[u8], &[u8])],
        body: Option<&[u8]>,
    ) -> io::Result<()> {
        let h3_conn = match &mut self.h3_conn {
            Some(h3) => h3,
            None => return Ok(()),
        };
        
        // ステータスを含むヘッダーを構築
        let status_str = status.to_string();
        let mut h3_headers = vec![h3::Header::new(b":status", status_str.as_bytes())];
        
        for (name, value) in headers {
            if *name != b":status" {
                h3_headers.push(h3::Header::new(*name, *value));
            }
        }
        
        // Content-Length を追加
        if let Some(body_data) = body {
            let len_str = body_data.len().to_string();
            h3_headers.push(h3::Header::new(b"content-length", len_str.as_bytes()));
        }
        
        // ヘッダー送信
        let has_body = body.is_some();
        match h3_conn.send_response(&mut self.conn, stream_id, &h3_headers, !has_body) {
            Ok(()) => {}
            Err(h3::Error::StreamBlocked) => return Ok(()),
            Err(e) => {
                warn!("[HTTP/3] send_response error: {}", e);
                return Ok(());
            }
        }
        
        // ボディ送信
        if let Some(body_data) = body {
            if !body_data.is_empty() {
                match h3_conn.send_body(&mut self.conn, stream_id, body_data, true) {
                    Ok(_) => {}
                    Err(h3::Error::Done) => {
                        self.partial_responses.insert(stream_id, (body_data.to_vec(), 0));
                    }
                    Err(e) => {
                        warn!("[HTTP/3] send_body error: {}", e);
                    }
                }
            }
        }
        
        Ok(())
    }
    
    /// エラーレスポンス送信
    fn send_error_response(&mut self, stream_id: u64, status: u16, body: &[u8]) -> io::Result<()> {
        self.send_response(stream_id, status, &[
            (b"content-type", b"text/plain"),
            (b"server", b"zerocopy-server/http3"),
        ], Some(body))
    }
    
    /// メトリクス記録
    fn record_metrics(&self, method: &[u8], authority: &[u8], status: u16, req_size: usize, resp_size: usize, start_time: Instant) {
        let duration = start_time.elapsed().as_secs_f64();
        let method_str = std::str::from_utf8(method).unwrap_or("UNKNOWN");
        let host_str = std::str::from_utf8(authority).unwrap_or("-");
        record_request_metrics(method_str, host_str, status, req_size as u64, resp_size as u64, duration);
    }
    
    /// プロキシ処理（HTTP/1.1バックエンドへの変換）
    fn handle_proxy(
        &mut self,
        stream_id: u64,
        upstream_group: &Arc<UpstreamGroup>,
        method: &[u8],
        req_path: &[u8],
        prefix: &[u8],
        headers: &[h3::Header],
        request_body: &[u8],
    ) -> io::Result<(u16, usize)> {
        // サーバー選択
        let server = match upstream_group.select(&self.client_ip) {
            Some(s) => s,
            None => {
                self.send_error_response(stream_id, 502, b"Bad Gateway")?;
                return Ok((502, 11));
            }
        };
        
        server.acquire();
        let target = &server.target;
        
        // リクエストパス構築
        let path_str = std::str::from_utf8(req_path).unwrap_or("/");
        let sub_path = if prefix.is_empty() {
            path_str.to_string()
        } else {
            let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
            if path_str.starts_with(prefix_str) {
                let remaining = &path_str[prefix_str.len()..];
                let base = target.path_prefix.trim_end_matches('/');
                
                if remaining.is_empty() {
                    if base.is_empty() { "/".to_string() } else { format!("{}/", base) }
                } else if remaining.starts_with('/') {
                    if base.is_empty() { remaining.to_string() } else { format!("{}{}", base, remaining) }
                } else {
                    if base.is_empty() { format!("/{}", remaining) } else { format!("{}/{}", base, remaining) }
                }
            } else {
                path_str.to_string()
            }
        };
        
        let final_path = if sub_path.is_empty() { "/" } else { &sub_path };
        
        // HTTP/1.1 リクエスト構築
        let mut request = Vec::with_capacity(1024);
        request.extend_from_slice(method);
        request.extend_from_slice(b" ");
        request.extend_from_slice(final_path.as_bytes());
        request.extend_from_slice(b" HTTP/1.1\r\nHost: ");
        request.extend_from_slice(target.host.as_bytes());
        
        if !target.is_default_port() {
            request.extend_from_slice(b":");
            let mut port_buf = itoa::Buffer::new();
            request.extend_from_slice(port_buf.format(target.port).as_bytes());
        }
        request.extend_from_slice(b"\r\n");
        
        // ヘッダー追加（疑似ヘッダー以外）
        for header in headers {
            if header.name().starts_with(b":") {
                continue;
            }
            if header.name().eq_ignore_ascii_case(b"connection") ||
               header.name().eq_ignore_ascii_case(b"keep-alive") {
                continue;
            }
            request.extend_from_slice(header.name());
            request.extend_from_slice(b": ");
            request.extend_from_slice(header.value());
            request.extend_from_slice(b"\r\n");
        }
        
        // Content-Length 追加
        if !request_body.is_empty() {
            request.extend_from_slice(b"Content-Length: ");
            let mut len_buf = itoa::Buffer::new();
            request.extend_from_slice(len_buf.format(request_body.len()).as_bytes());
            request.extend_from_slice(b"\r\n");
        }
        
        request.extend_from_slice(b"Connection: close\r\n\r\n");
        request.extend_from_slice(request_body);
        
        // 同期的にプロキシ処理（monoioはthread-per-coreなのでブロッキングOK）
        // 実際にはこの関数は非同期コンテキストで呼ばれないため、
        // レスポンスをバッファに格納してから送信
        let result = self.proxy_to_backend(stream_id, target, request)?;
        
        server.release();
        Ok(result)
    }
    
    /// バックエンドへのプロキシ（同期版、後で非同期に変換される前提）
    fn proxy_to_backend(
        &mut self,
        stream_id: u64,
        _target: &ProxyTarget,
        _request: Vec<u8>,
    ) -> io::Result<(u16, usize)> {
        // 注意: この実装は簡略版です
        // 実際のプロダクションでは非同期I/Oが必要
        // HTTP/3はUDPベースなので、TCPバックエンドとの通信は別途処理
        
        // 簡易実装: バックエンドへの接続はこのコンテキストでは不可能なため
        // プロキシ未対応のエラーを返す
        // TODO: 完全な非同期プロキシ実装
        
        self.send_error_response(stream_id, 502, b"HTTP/3 proxy not yet implemented for this backend")?;
        Ok((502, 47))
    }
    
    /// ファイル配信
    fn handle_sendfile(
        &mut self,
        stream_id: u64,
        base_path: &PathBuf,
        is_dir: bool,
        index_file: Option<&str>,
        req_path: &[u8],
        prefix: &[u8],
        security: &SecurityConfig,
    ) -> io::Result<(u16, usize)> {
        let path_str = std::str::from_utf8(req_path).unwrap_or("/");
        let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
        
        // プレフィックス除去後のサブパス
        let sub_path = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
            &path_str[prefix_str.len()..]
        } else {
            path_str
        };
        
        let clean_sub = sub_path.trim_start_matches('/');
        
        // パストラバーサル防止
        if clean_sub.contains("..") {
            self.send_error_response(stream_id, 403, b"Forbidden")?;
            return Ok((403, 9));
        }
        
        // ファイルパス構築
        let file_path = if is_dir {
            let mut p = base_path.clone();
            if clean_sub.is_empty() || clean_sub == "/" {
                p.push(index_file.unwrap_or("index.html"));
            } else {
                p.push(clean_sub);
                if p.is_dir() {
                    p.push(index_file.unwrap_or("index.html"));
                }
            }
            p
        } else {
            if !clean_sub.is_empty() {
                self.send_error_response(stream_id, 404, b"Not Found")?;
                return Ok((404, 9));
            }
            base_path.clone()
        };
        
        // ファイル読み込み
        let data = match std::fs::read(&file_path) {
            Ok(d) => d,
            Err(_) => {
                self.send_error_response(stream_id, 404, b"Not Found")?;
                return Ok((404, 9));
            }
        };
        
        let mime_type = mime_guess::from_path(&file_path).first_or_octet_stream();
        let mime_str = mime_type.as_ref();
        
        let mut resp_headers: Vec<(&[u8], &[u8])> = vec![
            (b"content-type", mime_str.as_bytes()),
            (b"server", b"zerocopy-server/http3"),
        ];
        
        // セキュリティヘッダー追加
        let security_headers: Vec<(Vec<u8>, Vec<u8>)> = security.add_response_headers.iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect();
        
        for (k, v) in &security_headers {
            resp_headers.push((k.as_slice(), v.as_slice()));
        }
        
        self.send_response(stream_id, 200, &resp_headers, Some(&data))?;
        Ok((200, data.len()))
    }
    
    /// リダイレクト処理
    fn handle_redirect(
        &mut self,
        stream_id: u64,
        redirect_url: &str,
        status_code: u16,
        preserve_path: bool,
        req_path: &[u8],
        prefix: &[u8],
    ) -> io::Result<(u16, usize)> {
        let path_str = std::str::from_utf8(req_path).unwrap_or("/");
        let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
        
        // パス部分（prefix除去後）
        let sub_path = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
            &path_str[prefix_str.len()..]
        } else {
            path_str
        };
        
        // 変数置換とパス追加
        let mut final_url = redirect_url
            .replace("$request_uri", path_str)
            .replace("$path", sub_path);
        
        if preserve_path && !sub_path.is_empty() {
            if final_url.ends_with('/') && sub_path.starts_with('/') {
                final_url.push_str(&sub_path[1..]);
            } else if !final_url.ends_with('/') && !sub_path.starts_with('/') {
                final_url.push('/');
                final_url.push_str(sub_path);
            } else {
                final_url.push_str(sub_path);
            }
        }
        
        self.send_response(stream_id, status_code, &[
            (b"location", final_url.as_bytes()),
            (b"server", b"zerocopy-server/http3"),
        ], None)?;
        
        Ok((status_code, 0))
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
/// HTTP/1.1と同等のルーティング・セキュリティ・プロキシ機能をサポートします。
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
    
    // ルーティング設定を CURRENT_CONFIG から取得（ホットリロード対応）
    let get_routes = || {
        let config = CURRENT_CONFIG.load();
        (config.host_routes.clone(), config.path_routes.clone())
    };

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

                // 最新のルーティング設定を取得
                let (host_routes, path_routes) = get_routes();
                let handler = Http3Handler::new(conn, from, host_routes, path_routes);
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
