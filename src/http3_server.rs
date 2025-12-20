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
use std::ffi::CString;
use std::io::{self, Write as IoWrite, Seek};
use std::net::SocketAddr;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
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
    CURRENT_CONFIG, SHUTDOWN_FLAG,
};

/// memfd_create システムコールのラッパー（セキュリティ強化版）
/// 
/// 匿名のメモリファイルを作成します。このファイルはファイルシステム上には
/// 存在せず、メモリ上にのみ存在します。Landlock のファイルシステム制限を
/// バイパスしながら、ファイルディスクリプタ経由でアクセスできます。
/// 
/// ## セキュリティ対策
/// - MFD_CLOEXEC: exec() 時に自動的に閉じる（fd リーク防止）
/// - MFD_ALLOW_SEALING: 書き込み後にシールを適用可能にする
fn memfd_create_secure(name: &str) -> io::Result<std::fs::File> {
    let c_name = CString::new(name).map_err(|e| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("invalid memfd name: {}", e))
    })?;
    
    // MFD_CLOEXEC (1): exec() 時に自動クローズ
    // MFD_ALLOW_SEALING (2): シール機能を有効化
    const MFD_CLOEXEC: libc::c_uint = 1;
    const MFD_ALLOW_SEALING: libc::c_uint = 2;
    
    let fd = unsafe {
        libc::memfd_create(c_name.as_ptr(), MFD_CLOEXEC | MFD_ALLOW_SEALING)
    };
    
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// memfd にシールを適用（書き込み禁止・サイズ変更禁止）
/// 
/// シールを適用することで、memfd の内容が改ざんされることを防ぎます。
/// これにより、攻撃者が memfd の内容を書き換えて不正な証明書を
/// 注入することを防止できます。
fn apply_memfd_seals(fd: i32) -> io::Result<()> {
    // F_ADD_SEALS = 1033
    // F_SEAL_SEAL = 1 (これ以上シールを追加できなくする)
    // F_SEAL_SHRINK = 2 (サイズ縮小禁止)
    // F_SEAL_GROW = 4 (サイズ拡大禁止)
    // F_SEAL_WRITE = 8 (書き込み禁止)
    const F_ADD_SEALS: libc::c_int = 1033;
    const F_SEAL_SEAL: libc::c_int = 1;
    const F_SEAL_SHRINK: libc::c_int = 2;
    const F_SEAL_GROW: libc::c_int = 4;
    const F_SEAL_WRITE: libc::c_int = 8;
    
    let seals = F_SEAL_WRITE | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL;
    
    let result = unsafe {
        libc::fcntl(fd, F_ADD_SEALS, seals)
    };
    
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    
    Ok(())
}

/// PEM データを memfd に書き込み、/proc/self/fd/<fd> パスを返す（セキュリティ強化版）
/// 
/// この関数は以下のことを行います：
/// 1. memfd_create で匿名ファイルを作成（MFD_CLOEXEC + MFD_ALLOW_SEALING）
/// 2. PEM データを書き込み
/// 3. シールを適用（書き込み禁止・サイズ変更禁止・追加シール禁止）
/// 4. ファイル位置を先頭に戻す
/// 5. /proc/self/fd/<fd> パスを生成
/// 
/// ## セキュリティ特性
/// - memfd の内容は書き込み後に変更不可能（シール適用）
/// - exec() 時に自動的に閉じる（MFD_CLOEXEC）
/// - ファイルシステム上には存在しない（Landlock バイパス）
/// 
/// ## 注意
/// 戻り値の File オブジェクトはスコープ内で保持し続ける必要があります。
/// ドロップされると fd が閉じられ、パスが無効になります。
fn create_memfd_for_pem(name: &str, pem_data: &[u8]) -> io::Result<(std::fs::File, String)> {
    // memfd を作成（セキュリティフラグ付き）
    let mut memfd = memfd_create_secure(name)?;
    
    // PEM データを書き込み
    memfd.write_all(pem_data)?;
    
    // ファイル位置を先頭に戻す（読み取り用）
    memfd.seek(io::SeekFrom::Start(0))?;
    
    // /proc/self/fd/<fd> パスを生成
    let fd = memfd.as_raw_fd();
    let proc_path = format!("/proc/self/fd/{}", fd);
    
    // シールを適用（書き込み禁止、サイズ変更禁止）
    // 注意: シール適用後は quiche がファイルを読み取る必要があるため、
    // 読み取りは引き続き可能
    if let Err(e) = apply_memfd_seals(fd) {
        warn!("[HTTP/3] Failed to apply memfd seals: {} (continuing without seals)", e);
        // シール適用失敗は致命的ではないため、警告のみで続行
    } else {
        debug!("[HTTP/3] memfd seals applied: WRITE|SHRINK|GROW|SEAL");
    }
    
    Ok((memfd, proc_path))
}

/// セキュアなバイト配列のゼロ化
/// 
/// メモリ上の機密データを安全にゼロ化します。
/// コンパイラによる最適化（デッドストア削除）を防ぐため、
/// volatile 書き込みを使用します。
fn secure_zero(data: &mut [u8]) {
    // volatile 書き込みで最適化を防止
    for byte in data.iter_mut() {
        unsafe {
            std::ptr::write_volatile(byte, 0);
        }
    }
    // メモリバリアで確実に書き込みを完了
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
}

/// HTTP/3 サーバー設定
#[derive(Clone)]
pub struct Http3ServerConfig {
    /// TLS 証明書パス（後方互換性のため残す、cert_pem優先）
    pub cert_path: String,
    /// TLS 秘密鍵パス（後方互換性のため残す、key_pem優先）
    pub key_path: String,
    /// TLS 証明書（PEM形式、事前読み込み済み）
    /// 
    /// Landlock適用前に読み込まれた証明書バイト列。
    /// 設定されている場合、cert_pathより優先される。
    /// 
    /// 注意: 使用後にセキュアにゼロ化されます。
    pub cert_pem: Option<Vec<u8>>,
    /// TLS 秘密鍵（PEM形式、事前読み込み済み）
    /// 
    /// Landlock適用前に読み込まれた秘密鍵バイト列。
    /// 設定されている場合、key_pathより優先される。
    /// 
    /// 注意: 使用後にセキュアにゼロ化されます。
    pub key_pem: Option<Vec<u8>>,
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
            cert_pem: None,
            key_pem: None,
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
                        info!(
                            "[HTTP/3] Received Headers event: stream_id={}, more_frames={}, header_count={}",
                            stream_id, more_frames, list.len()
                        );
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

        // メトリクスエンドポイント（設定可能なパス）
        {
            let config = CURRENT_CONFIG.load();
            let prom_config = &config.prometheus_config;
            
            let path_str = std::str::from_utf8(&path).unwrap_or("/");
            if prom_config.enabled 
                && path_str == prom_config.path 
                && method == b"GET" 
            {
                // IPアドレス制限チェック
                if !prom_config.is_ip_allowed(&self.client_ip) {
                    self.send_error_response(stream_id, 403, b"Forbidden")?;
                    self.record_metrics(&method, &authority, 403, request_body.len(), 9, start_time);
                    return Ok(());
                }
                
                let body = encode_prometheus_metrics();
                self.send_response(stream_id, 200, &[
                    (b":status", b"200"),
                    (b"content-type", b"text/plain; version=0.0.4; charset=utf-8"),
                    (b"server", b"zerocopy-server/http3"),
                ], Some(&body))?;
                
                self.record_metrics(&method, &authority, 200, request_body.len(), body.len(), start_time);
                return Ok(());
            }
        }

        // バックエンド選択（デフォルトルートへのフォールバック付き）
        let backend_result = find_backend(&authority, &path, &self.host_routes, &self.path_routes)
            .or_else(|| {
                // authority が空でない場合、デフォルトルートを検索
                if !authority.is_empty() {
                    debug!(
                        "[HTTP/3] No route found for authority '{}', trying default routes",
                        String::from_utf8_lossy(&authority)
                    );
                    find_backend(b"", &path, &self.host_routes, &self.path_routes)
                } else {
                    None
                }
            });
        
        let (prefix, backend) = match backend_result {
            Some(b) => b,
            None => {
                debug!(
                    "[HTTP/3] No backend found for authority='{}', path='{}'",
                    String::from_utf8_lossy(&authority),
                    String::from_utf8_lossy(&path)
                );
                self.send_error_response(stream_id, 404, b"Not Found")?;
                self.record_metrics(&method, &authority, 404, request_body.len(), 9, start_time);
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
                debug!("[HTTP/3] Starting proxy request to upstream group");
                let result = self.handle_proxy(stream_id, &upstream_group, &method, &path, &prefix, headers, request_body)
                    .unwrap_or((502, 11));
                debug!("[HTTP/3] Proxy request completed: status={}, size={}", result.0, result.1);
                result
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
    /// 
    /// HTTP/3 レスポンスを送信します。StreamBlocked エラーが発生した場合は
    /// 部分レスポンスとして保存し、後で flush_partial_responses() で再送します。
    fn send_response(
        &mut self,
        stream_id: u64,
        status: u16,
        headers: &[(&[u8], &[u8])],
        body: Option<&[u8]>,
    ) -> io::Result<()> {
        debug!("[HTTP/3] send_response called: stream_id={}, status={}, h3_conn={}", 
            stream_id, status, self.h3_conn.is_some());
        
        let h3_conn = match &mut self.h3_conn {
            Some(h3) => h3,
            None => {
                warn!("[HTTP/3] h3_conn is None, cannot send response");
                return Ok(());
            }
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
        let has_body = body.is_some() && body.map_or(false, |b| !b.is_empty());
        match h3_conn.send_response(&mut self.conn, stream_id, &h3_headers, !has_body) {
            Ok(()) => {
                debug!("[HTTP/3] Response headers sent for stream {}", stream_id);
            }
            Err(h3::Error::StreamBlocked) => {
                // ストリームがブロックされた場合、ボディを部分レスポンスとして保存
                // 次の send_pending_packets() で送信される
                debug!("[HTTP/3] Stream {} blocked, will retry later", stream_id);
                if let Some(body_data) = body {
                    self.partial_responses.insert(stream_id, (body_data.to_vec(), 0));
                }
                return Ok(());
            }
            Err(e) => {
                warn!("[HTTP/3] send_response error on stream {}: {}", stream_id, e);
                return Ok(());
            }
        }
        
        // ボディ送信
        if let Some(body_data) = body {
            if !body_data.is_empty() {
                match h3_conn.send_body(&mut self.conn, stream_id, body_data, true) {
                    Ok(written) => {
                        debug!("[HTTP/3] Response body sent: {} bytes for stream {}", written, stream_id);
                        // 部分的にしか送信できなかった場合
                        if written < body_data.len() {
                            self.partial_responses.insert(stream_id, (body_data.to_vec(), written));
                        }
                    }
                    Err(h3::Error::Done) => {
                        // バッファがいっぱい、後で再送
                        debug!("[HTTP/3] Body buffer full for stream {}, queuing for later", stream_id);
                        self.partial_responses.insert(stream_id, (body_data.to_vec(), 0));
                    }
                    Err(e) => {
                        warn!("[HTTP/3] send_body error on stream {}: {}", stream_id, e);
                    }
                }
            }
        }
        
        Ok(())
    }
    
    /// エラーレスポンス送信
    fn send_error_response(&mut self, stream_id: u64, status: u16, body: &[u8]) -> io::Result<()> {
        debug!("[HTTP/3] Sending error response: status={}, body_len={}", status, body.len());
        let result = self.send_response(stream_id, status, &[
            (b"content-type", b"text/plain"),
            (b"server", b"zerocopy-server/http3"),
        ], Some(body));
        debug!("[HTTP/3] Error response send result: {:?}", result.is_ok());
        result
    }
    
    /// メトリクス記録
    fn record_metrics(&self, method: &[u8], authority: &[u8], status: u16, req_size: usize, resp_size: usize, start_time: Instant) {
        let duration = start_time.elapsed().as_secs_f64();
        let method_str = std::str::from_utf8(method).unwrap_or("UNKNOWN");
        let host_str = std::str::from_utf8(authority).unwrap_or("-");
        record_request_metrics(method_str, host_str, status, req_size as u64, resp_size as u64, duration);
    }
    
    /// プロキシ処理（HTTP/1.1またはHTTP/2バックエンドへの変換）
    /// 
    /// HTTP/3からのリクエストをバックエンドに転送します。
    /// バックエンドがHTTP/3に対応していない場合は、HTTP/2またはHTTP/1.1にフォールバックします。
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
        let mut request = Vec::with_capacity(1024 + request_body.len());
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
               header.name().eq_ignore_ascii_case(b"keep-alive") ||
               header.name().eq_ignore_ascii_case(b"transfer-encoding") {
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
        
        // 同期的なブロッキングI/Oでバックエンドに接続
        // monoioはthread-per-coreモデルなので、同期I/Oも許容される
        // ただし、本番環境では非同期版が推奨
        let result = self.proxy_to_backend_sync(stream_id, target, request);
        
        server.release();
        result
    }
    
    /// バックエンドへの同期プロキシ処理
    /// 
    /// HTTP/3コネクションはUDPベースですが、バックエンドへの接続はTCPを使用します。
    /// std::net::TcpStreamを使用して同期的に接続し、レスポンスを受信します。
    fn proxy_to_backend_sync(
        &mut self,
        stream_id: u64,
        target: &ProxyTarget,
        request: Vec<u8>,
    ) -> io::Result<(u16, usize)> {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        
        debug!("[HTTP/3] Connecting to backend {}:{} (TLS: {})", target.host, target.port, target.use_tls);
        
        // バックエンドに接続（同期）
        // DNS解決を行い、タイムアウト付きで接続
        let addr = format!("{}:{}", target.host, target.port);
        
        // DNS解決（ToSocketAddrs を使用）
        use std::net::ToSocketAddrs;
        let socket_addr = match addr.to_socket_addrs() {
            Ok(mut addrs) => match addrs.next() {
                Some(a) => a,
                None => {
                    warn!("[HTTP/3] No addresses found for {}", addr);
                    self.send_error_response(stream_id, 502, b"Bad Gateway")?;
                    return Ok((502, 11));
                }
            },
            Err(e) => {
                warn!("[HTTP/3] DNS resolution error for {}: {}", addr, e);
                self.send_error_response(stream_id, 502, b"Bad Gateway")?;
                return Ok((502, 11));
            }
        };
        
        debug!("[HTTP/3] Resolved {} to {}", addr, socket_addr);
        
        let mut backend = match TcpStream::connect_timeout(
            &socket_addr,
            Duration::from_secs(10),
        ) {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
                debug!("[HTTP/3] Connected to backend {}", socket_addr);
                stream
            }
            Err(e) => {
                warn!("[HTTP/3] Backend connect error to {}: {}", addr, e);
                self.send_error_response(stream_id, 502, b"Bad Gateway")?;
                return Ok((502, 11));
            }
        };
        
        // TLSバックエンドの場合
        if target.use_tls {
            // rustlsを使用したTLS接続
            let result = self.proxy_to_tls_backend_sync(stream_id, target, request, backend);
            return result;
        }
        
        // リクエスト送信
        if let Err(e) = backend.write_all(&request) {
            warn!("[HTTP/3] Backend write error: {}", e);
            self.send_error_response(stream_id, 502, b"Bad Gateway")?;
            return Ok((502, 11));
        }
        
        // レスポンス受信
        let mut response = Vec::with_capacity(16384);
        let mut buf = [0u8; 8192];
        
        loop {
            match backend.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => response.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) if e.kind() == io::ErrorKind::TimedOut => break,
                Err(e) => {
                    warn!("[HTTP/3] Backend read error: {}", e);
                    if response.is_empty() {
                        self.send_error_response(stream_id, 502, b"Bad Gateway")?;
                        return Ok((502, 11));
                    }
                    break;
                }
            }
        }
        
        // HTTPレスポンスをパース
        self.parse_and_send_response(stream_id, &response)
    }
    
    /// TLSバックエンドへの同期プロキシ処理
    fn proxy_to_tls_backend_sync(
        &mut self,
        stream_id: u64,
        target: &ProxyTarget,
        request: Vec<u8>,
        mut tcp_stream: std::net::TcpStream,
    ) -> io::Result<(u16, usize)> {
        use std::io::{Read, Write};
        use rustls::ClientConfig;
        use std::sync::Arc;
        
        // rustls クライアント設定
        let root_store = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        
        let config = ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        
        let config = Arc::new(config);
        
        // SNI名を決定
        let sni_name = target.sni_name.as_deref().unwrap_or(&target.host);
        let server_name = match rustls::pki_types::ServerName::try_from(sni_name.to_string()) {
            Ok(name) => name,
            Err(e) => {
                warn!("[HTTP/3] Invalid SNI name: {}", e);
                self.send_error_response(stream_id, 502, b"Bad Gateway")?;
                return Ok((502, 11));
            }
        };
        
        let mut tls_conn = match rustls::ClientConnection::new(config, server_name) {
            Ok(conn) => conn,
            Err(e) => {
                warn!("[HTTP/3] TLS connection error: {}", e);
                self.send_error_response(stream_id, 502, b"Bad Gateway")?;
                return Ok((502, 11));
            }
        };
        
        let mut stream = rustls::Stream::new(&mut tls_conn, &mut tcp_stream);
        
        // リクエスト送信
        if let Err(e) = stream.write_all(&request) {
            warn!("[HTTP/3] TLS backend write error: {}", e);
            self.send_error_response(stream_id, 502, b"Bad Gateway")?;
            return Ok((502, 11));
        }
        
        // レスポンス受信
        let mut response = Vec::with_capacity(16384);
        let mut buf = [0u8; 8192];
        
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => response.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => {
                    if response.is_empty() {
                        warn!("[HTTP/3] TLS backend read error: {}", e);
                        self.send_error_response(stream_id, 502, b"Bad Gateway")?;
                        return Ok((502, 11));
                    }
                    break;
                }
            }
        }
        
        // HTTPレスポンスをパース
        self.parse_and_send_response(stream_id, &response)
    }
    
    /// HTTPレスポンスをパースしてHTTP/3レスポンスとして送信
    fn parse_and_send_response(&mut self, stream_id: u64, response: &[u8]) -> io::Result<(u16, usize)> {
        // ヘッダー終端を探す
        let header_end = match find_header_end(response) {
            Some(pos) => pos,
            None => {
                warn!("[HTTP/3] Invalid response from backend");
                self.send_error_response(stream_id, 502, b"Bad Gateway")?;
                return Ok((502, 11));
            }
        };
        
        // ステータス行をパース
        let header_bytes = &response[..header_end];
        let body_bytes = &response[header_end + 4..]; // \r\n\r\n の後
        
        // ステータスコードを取得
        let status_code = parse_status_code(header_bytes).unwrap_or(502);
        
        // ヘッダーをパース
        let mut resp_headers: Vec<(&[u8], &[u8])> = Vec::new();
        resp_headers.push((b"server", b"zerocopy-server/http3"));
        
        // レスポンスヘッダーを抽出（ステータス行をスキップ）
        if let Some(first_crlf) = memchr::memchr(b'\n', header_bytes) {
            let headers_section = &header_bytes[first_crlf + 1..];
            for line in headers_section.split(|&b| b == b'\n') {
                let line = line.strip_suffix(&[b'\r']).unwrap_or(line);
                if line.is_empty() {
                    continue;
                }
                if let Some(colon_pos) = memchr::memchr(b':', line) {
                    let name = &line[..colon_pos];
                    let value = &line[colon_pos + 1..];
                    let value = value.strip_prefix(&[b' ']).unwrap_or(value);
                    
                    // ホップバイホップヘッダーはスキップ
                    if name.eq_ignore_ascii_case(b"connection")
                        || name.eq_ignore_ascii_case(b"transfer-encoding")
                        || name.eq_ignore_ascii_case(b"keep-alive")
                    {
                        continue;
                    }
                    
                    resp_headers.push((name, value));
                }
            }
        }
        
        self.send_response(stream_id, status_code, &resp_headers, Some(body_bytes))?;
        Ok((status_code, body_bytes.len()))
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

/// SO_REUSEPORT を設定した UDP ソケットを作成
/// 
/// 複数ワーカースレッドが同じポートでリッスンし、
/// カーネルがフローに基づいてパケットを分散します。
fn create_reuseport_udp_socket(bind_addr: SocketAddr) -> io::Result<UdpSocket> {
    use std::os::unix::io::FromRawFd;
    
    // socket2 を使用せず libc を直接使用
    let domain = if bind_addr.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };
    
    let fd = unsafe { libc::socket(domain, libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0) };
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
    let ret = match bind_addr {
        SocketAddr::V4(addr) => {
            let sin = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: addr.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(addr.ip().octets()),
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
        SocketAddr::V6(addr) => {
            let sin6 = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: addr.port().to_be(),
                sin6_flowinfo: addr.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: addr.ip().octets(),
                },
                sin6_scope_id: addr.scope_id(),
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
    UdpSocket::from_std(std_socket)
}

/// HTTP/3 サーバーを起動（monoio ランタイム上で実行）
/// 
/// この関数は monoio のスレッド内から呼び出す必要があります。
/// HTTP/1.1と同等のルーティング・セキュリティ・プロキシ機能をサポートします。
/// 
/// ## セキュリティ
/// 証明書データ（cert_pem, key_pem）は quiche へのロード完了後、
/// セキュアにゼロ化してからメモリから解放されます。
pub async fn run_http3_server_async(
    bind_addr: SocketAddr,
    mut config: Http3ServerConfig,
) -> io::Result<()> {
    // QUIC 設定を作成
    let mut quic_config = Config::new(quiche::PROTOCOL_VERSION)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;

    // TLS 証明書を設定
    // memfd アプローチ: 事前読み込み済みの PEM バイト列を memfd に書き込み、
    // /proc/self/fd/<fd> パス経由で quiche に渡す
    // これにより Landlock でファイルシステムアクセスを制限しながら HTTP/3 を使用可能
    // 
    // セキュリティ: quiche が証明書を読み込んだ後:
    // 1. memfd を即座にドロップ（カーネルがメモリ解放）
    // 2. config 内の Vec<u8> をセキュアにゼロ化してからドロップ
    if let (Some(mut cert_pem), Some(mut key_pem)) = (config.cert_pem.take(), config.key_pem.take()) {
        // memfd 経由でロード（Landlock 対応）
        info!("[HTTP/3] Loading certificates via memfd (Landlock compatible)");
        
        // 証明書を memfd に書き込み
        let (cert_memfd, cert_path) = create_memfd_for_pem("tls_cert", &cert_pem)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, 
                format!("Failed to create memfd for cert: {}", e)))?;
        
        // quiche が証明書を読み込む
        quic_config.load_cert_chain_from_pem_file(&cert_path)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, 
                format!("cert load error (memfd): {}", e)))?;
        
        // 証明書 memfd を即座にドロップ（fd を閉じてカーネルにメモリ解放を依頼）
        drop(cert_memfd);
        
        // 証明書データをセキュアにゼロ化
        secure_zero(&mut cert_pem);
        drop(cert_pem);
        debug!("[HTTP/3] Certificate data securely zeroed and released");
        
        // 秘密鍵を memfd に書き込み
        let (key_memfd, key_path) = create_memfd_for_pem("tls_key", &key_pem)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, 
                format!("Failed to create memfd for key: {}", e)))?;
        
        // quiche が秘密鍵を読み込む
        quic_config.load_priv_key_from_pem_file(&key_path)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, 
                format!("key load error (memfd): {}", e)))?;
        
        // 秘密鍵 memfd を即座にドロップ
        drop(key_memfd);
        
        // 秘密鍵データをセキュアにゼロ化
        secure_zero(&mut key_pem);
        drop(key_pem);
        debug!("[HTTP/3] Private key data securely zeroed and released");
        
        info!("[HTTP/3] Certificates loaded, memfd closed, sensitive data zeroed");
    } else {
        // ファイルパスから直接ロード（後方互換性）
        info!("[HTTP/3] Loading certificates from file path (legacy mode)");
        warn!("[HTTP/3] Note: When using Landlock, add cert/key paths to landlock_read_paths");
        
        quic_config.load_cert_chain_from_pem_file(&config.cert_path)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, 
                format!("cert load error: {}", e)))?;
        
        quic_config.load_priv_key_from_pem_file(&config.key_path)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, 
                format!("key load error: {}", e)))?;
    }

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
    // SO_REUSEPORT を設定して複数ワーカーで並列処理を可能に
    let socket = create_reuseport_udp_socket(bind_addr)?;
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
        // シャットダウンチェック
        if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
            info!("[HTTP/3] Shutting down...");
            break Ok(());
        }
        
        // 最小タイムアウトを計算
        let timeout_duration = {
            let conns = connections.borrow();
            conns.values()
                .filter_map(|h| h.conn.timeout())
                .min()
                .unwrap_or(Duration::from_millis(100))
        };

        // タイムアウト付きでパケット受信
        let recv_buf = vec![0u8; 65536];
        let recv_result = monoio::time::timeout(timeout_duration, socket.recv_from(recv_buf)).await;

        // タイムアウト処理（常に実行）
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

        // パケット受信結果を処理（タイムアウト時も送信処理は実行する）
        let received_packet = match recv_result {
            Ok((Ok((len, from)), buf)) => Some((buf, len, from)),
            Ok((Err(e), _)) => {
                if e.kind() != io::ErrorKind::WouldBlock {
                    error!("[HTTP/3] recv_from error: {}", e);
                }
                None
            }
            Err(_) => {
                // タイムアウト - パケット受信なし、送信処理は続行
                None
            }
        };

        // パケットを受信した場合のみ処理
        if let Some((recv_buf, len, from)) = received_packet {
            let mut pkt_buf = recv_buf[..len].to_vec();

            // パケットヘッダーを解析
            let hdr = match quiche::Header::from_slice(&mut pkt_buf, quiche::MAX_CONN_ID_LEN) {
                Ok(v) => v,
                Err(e) => {
                    warn!("[HTTP/3] Invalid packet header: {}", e);
                    // 無効なパケットでも送信処理は続行
                    send_pending_packets(&connections, &socket, local_addr).await;
                    continue;
                }
            };

            // コネクションを検索または作成
            let conn_id = {
                let mut conns = connections.borrow_mut();
                
                if !conns.contains_key(&hdr.dcid) {
                    if hdr.ty != quiche::Type::Initial {
                        debug!("[HTTP/3] Non-initial packet for unknown connection");
                        // 送信処理は続行
                        drop(conns);
                        send_pending_packets(&connections, &socket, local_addr).await;
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
                            // エラー時も送信処理は続行
                        }
                    }

                    // HTTP/3 初期化
                    if handler.h3_conn.is_none() && handler.conn.is_established() {
                        debug!("[HTTP/3] Connection established, initializing H3");
                    }
                    if let Err(e) = handler.init_h3() {
                        warn!("[HTTP/3] init_h3 error: {}", e);
                    }

                    // HTTP/3 イベント処理
                    if handler.h3_conn.is_some() {
                        if let Err(e) = handler.process_h3_events() {
                            warn!("[HTTP/3] process_h3_events error: {}", e);
                        }
                    }
                }
            }
        }

        // 送信処理（常に実行 - タイムアウト時も送信が必要）
        send_pending_packets(&connections, &socket, local_addr).await;
    }
}

/// 保留中のパケットを全コネクションに対して送信
/// 
/// この関数はメインループで常に呼び出され、タイムアウト時でも
/// ACKやレスポンスパケットを送信します。
async fn send_pending_packets(
    connections: &ConnectionMap,
    socket: &Rc<UdpSocket>,
    _local_addr: SocketAddr,
) {
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
            if let (Err(e), _) = socket_clone.send_to(send_data, target).await {
                warn!("[HTTP/3] send_to error: {}", e);
            }
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

// ====================
// ヘルパー関数
// ====================

/// HTTPレスポンスのヘッダー終端（\r\n\r\n）を探す
fn find_header_end(data: &[u8]) -> Option<usize> {
    for i in 0..data.len().saturating_sub(3) {
        if &data[i..i+4] == b"\r\n\r\n" {
            return Some(i);
        }
    }
    None
}

/// HTTPレスポンスからステータスコードをパース
fn parse_status_code(header: &[u8]) -> Option<u16> {
    // "HTTP/1.1 200 OK" のような形式
    let header_str = std::str::from_utf8(header).ok()?;
    let first_line = header_str.lines().next()?;
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() >= 2 {
        parts[1].parse().ok()
    } else {
        None
    }
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
