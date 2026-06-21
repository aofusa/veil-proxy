//! プロキシ処理モジュール
//!
//! すべてのプロキシハンドリングロジックを担当します。
//! HTTP/1.1、HTTP/2、WebSocket、ファイル配信などに対応。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::net::SocketAddr;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use monoio::buf::{IoBuf, IoBufMut, IoVecBuf, IoVecBufMut};
use monoio::io::{AsyncReadRent, AsyncWriteRentExt};
use monoio::net::TcpStream;
use monoio::time::timeout;
use monoio::fs::OpenOptions;
use ftlog::{info, error, warn, debug};
use httparse::{Request, Status};
use memchr::memchr3;
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};
use crate::config::*;
use crate::logging::*;
use crate::metrics::*;
use crate::pool::*;
use crate::http_utils::*;
use crate::constants::*;
use crate::upstream::*;
use crate::buffering;
use crate::cache;
use crate::routing;
use crate::server::spawn_background_revalidation;

#[cfg(feature = "ktls")]
use crate::ktls_rustls::{RustlsAcceptor, KtlsServerStream, KtlsClientStream, SplicePipe};
#[cfg(not(feature = "ktls"))]
use crate::simple_tls;
#[cfg(not(feature = "ktls"))]
use crate::simple_tls::{SimpleTlsServerStream, SimpleTlsClientStream};
#[cfg(feature = "http2")]
use crate::http2;
#[cfg(feature = "grpc")]
use crate::grpc;
#[cfg(feature = "wasm")]
use crate::wasm;

// ServerTls型エイリアス（main.rsから再エクスポート）
#[cfg(feature = "ktls")]
use crate::ktls_rustls::KtlsServerStream as ServerTls;
#[cfg(not(feature = "ktls"))]
use crate::simple_tls::SimpleTlsServerStream as ServerTls;

// ====================
// 接続処理
// ====================

// ====================
// 共通セキュリティチェック（HTTP/1.1, HTTP/2, HTTP/3 共用）
// ====================
//
// プロトコル非依存のセキュリティチェック関数群。
// 各プロトコルハンドラーから呼び出されます。

/// セキュリティチェック結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityCheckResult {
    /// 許可（処理を継続）
    Allowed,
    /// IP拒否（403 Forbidden）
    IpDenied,
    /// メソッド不許可（405 Method Not Allowed）
    MethodNotAllowed,
    /// レート制限超過（429 Too Many Requests）
    RateLimitExceeded,
    /// リクエストサイズ超過（413 Request Entity Too Large）
    RequestTooLarge,
}

impl SecurityCheckResult {
    /// HTTPステータスコードに変換
    #[inline]
    pub fn status_code(&self) -> u16 {
        match self {
            SecurityCheckResult::Allowed => 200,
            SecurityCheckResult::IpDenied => 403,
            SecurityCheckResult::MethodNotAllowed => 405,
            SecurityCheckResult::RateLimitExceeded => 429,
            SecurityCheckResult::RequestTooLarge => 413,
        }
    }
    
    /// エラーメッセージを取得
    #[inline]
    pub fn message(&self) -> &'static [u8] {
        match self {
            SecurityCheckResult::Allowed => b"OK",
            SecurityCheckResult::IpDenied => b"Forbidden",
            SecurityCheckResult::MethodNotAllowed => b"Method Not Allowed",
            SecurityCheckResult::RateLimitExceeded => b"Too Many Requests",
            SecurityCheckResult::RequestTooLarge => b"Request Entity Too Large",
        }
    }
}

/// 統合セキュリティチェック（すべてのプロトコル共用）
/// 
/// ## チェック項目
/// 1. IP制限（allowed_ips, denied_ips）
/// 2. HTTPメソッド制限（allowed_methods）
/// 3. レートリミット（rate_limit_requests_per_min）
/// 4. ボディサイズ制限（max_request_body_size）
/// 
/// ## パフォーマンス
/// 設定がデフォルトの場合、has_security_checks() で早期リターンし、
/// オーバーヘッドを最小化。
#[cfg(any(feature = "http2", feature = "http3"))]
#[inline]
pub fn check_security(
    security: &SecurityConfig,
    client_ip: &str,
    method: &[u8],
    content_length: usize,
    is_chunked: bool,
) -> SecurityCheckResult {
    // IP制限チェック
    let ip_filter = security.ip_filter();
    if ip_filter.is_configured() && !ip_filter.is_allowed(client_ip) {
        return SecurityCheckResult::IpDenied;
    }
    
    // 許可メソッドチェック
    if !security.allowed_methods.is_empty() {
        let method_str = std::str::from_utf8(method).unwrap_or("GET");
        let is_allowed = security.allowed_methods.iter()
            .any(|m| m.eq_ignore_ascii_case(method_str));
        if !is_allowed {
            return SecurityCheckResult::MethodNotAllowed;
        }
    }
    
    // レートリミットチェック
    if security.rate_limit_requests_per_min > 0 {
        if !check_rate_limit(client_ip, security.rate_limit_requests_per_min) {
            return SecurityCheckResult::RateLimitExceeded;
        }
    }
    
    // ボディサイズ制限（chunked以外）
    if !is_chunked && content_length > security.max_request_body_size {
        return SecurityCheckResult::RequestTooLarge;
    }
    
    SecurityCheckResult::Allowed
}

// ====================
// HTTP/2 ハンドラー
// ====================
//
// HTTP/2 (RFC 7540) 接続を処理します。
// ALPN ネゴシエーションで h2 が選択された場合に呼び出されます。
// HTTP/1.1 と同等のセキュリティ機能とルーティングをサポート。

/// I/Oエラーが接続終了を示すものかどうかを判定
/// 
/// kTLSを使用している場合、クライアントが正常に接続を閉じた後でも
/// 次のフレーム読み込み時に以下のエラーが発生することがあります:
/// 
/// - EIO (os error 5): Input/output error - kTLS特有のエラー
///   kTLSではTLSレコードの処理がカーネル空間で行われるため、
///   クライアントがTLS close_notifyを送信せずに接続を閉じた場合や、
///   タイミングによってこのエラーが発生します。
/// - ConnectionReset: 接続がリセットされた（RST受信）
/// - BrokenPipe: パイプが壊れた（相手側が閉じた後の書き込み試行）
/// - UnexpectedEof: 予期しないEOF（相手側が閉じた）
/// - ConnectionAborted: 接続が中断された
/// 
/// **重要**: これらのエラーはクライアントが接続を閉じた場合の正常な動作であり、
/// サーバー側の問題ではありません。リクエスト処理は正常に完了しています。
/// ログには警告として出力しますが、接続は正常終了として扱います。
#[cfg_attr(not(feature = "http2"), allow(dead_code))]
#[inline]
fn build_sub_path(base: &str, remaining: &str) -> String {
    if remaining.is_empty() {
        if base.is_empty() {
            "/".to_string()
        } else {
            let mut s = String::with_capacity(base.len() + 1);
            s.push_str(base);
            s.push('/');
            s
        }
    } else if remaining.starts_with('/') {
        if base.is_empty() {
            remaining.to_string()
        } else {
            let mut s = String::with_capacity(base.len() + remaining.len());
            s.push_str(base);
            s.push_str(remaining);
            s
        }
    } else if base.is_empty() {
        let mut s = String::with_capacity(1 + remaining.len());
        s.push('/');
        s.push_str(remaining);
        s
    } else {
        let mut s = String::with_capacity(base.len() + 1 + remaining.len());
        s.push_str(base);
        s.push('/');
        s.push_str(remaining);
        s
    }
}

fn is_connection_closed_error(e: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    
    match e.kind() {
        ErrorKind::ConnectionReset => true,
        ErrorKind::BrokenPipe => true,
        ErrorKind::UnexpectedEof => true,
        ErrorKind::ConnectionAborted => true,
        _ => {
            // kTLS使用時のEIO (os error 5) をチェック
            // これはkTLS特有の動作であり、クライアントが接続を閉じた後に
            // 次のフレームを読み込もうとした際に発生します。
            // リクエスト処理自体は正常に完了しているため、問題ありません。
            if let Some(os_error) = e.raw_os_error() {
                // EIO = 5 (Linux)
                os_error == 5
            } else {
                false
            }
        }
    }
}

/// HTTP/2 リクエストを処理
/// 
/// HTTP/2 コネクションのメインループを実行し、各ストリームのリクエストを処理します。
/// HTTP/1.1 と同等のセキュリティチェック、ルーティング、プロキシ機能をサポート。
#[cfg(feature = "http2")]
async fn handle_http2_connection<S>(
    tls_stream: S,
    client_ip: &str,
) where
    S: monoio::io::AsyncReadRent + monoio::io::AsyncWriteRentExt + Unpin,
{
    use http2::Http2Connection;
    
    // HTTP/2 設定をCURRENT_CONFIGから取得（ホットリロード対応）
    let config = CURRENT_CONFIG.load();
    let settings = config.http2_config.to_http2_settings();
    
    // HTTP/2 コネクションを作成
    let mut conn = Http2Connection::new(tls_stream, settings);
    
    // ハンドシェイク（プリフェース確認 + SETTINGS 交換）
    if let Err(e) = conn.handshake().await {
        warn!("[HTTP/2] Handshake error: {}", e);
        return;
    }
    
    debug!("[HTTP/2] Connection established from {}", client_ip);
    
    // アクティブ接続メトリクスの自動管理（Dropで自動デクリメント）
    let mut connection_metric = ActiveConnectionMetric::new(true);
    
    // カスタムリクエストハンドラーを使用してメインループ実行
    let result = handle_http2_requests(&mut conn, client_ip, &mut connection_metric).await;
    
    if let Err(e) = result {
        warn!("[HTTP/2] Connection error: {}", e);
    }
    
    debug!("[HTTP/2] Connection closed from {}", client_ip);
}

/// HTTP/2 メインループ（カスタムリクエスト処理）
#[cfg(feature = "http2")]
async fn handle_http2_requests<S>(
    conn: &mut http2::Http2Connection<S>,
    client_ip: &str,
    connection_metric: &mut ActiveConnectionMetric,
) -> Result<(), http2::Http2Error>
where
    S: monoio::io::AsyncReadRent + monoio::io::AsyncWriteRentExt + Unpin,
{
    use http2::Http2Error;
    use std::io;
    
    loop {
        // フレームを読み込み
        let frame = match conn.read_frame().await {
            Ok(f) => f,
            Err(Http2Error::ConnectionClosed) => break,
            Err(Http2Error::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(Http2Error::Io(ref e)) if is_connection_closed_error(e) => {
                // クライアントが接続を閉じた場合に発生するエラー
                // kTLS使用時はEIO (os error 5) が発生することがある
                // 
                // 注意: このエラーはクライアントが正常に接続を閉じた場合の動作であり、
                // サーバー側の問題ではありません。リクエスト処理は正常に完了しています。
                // HTTP/2では、クライアントがレスポンス受信後にGOAWAYを送信せずに
                // 接続を閉じることがあり、その場合に次のフレーム読み込みでこのエラーが発生します。
                debug!(
                    "[HTTP/2] Connection closed by client (expected behavior): {} (client: {})",
                    e, client_ip
                );
                break;
            }
            Err(e) => {
                // その他のエラー時は GOAWAY を送信
                let _ = conn.send_goaway(e.error_code(), e.to_string().as_bytes()).await;
                return Err(e);
            }
        };
        
        // フレームを処理
        match conn.process_frame(frame).await {
            Ok(Some(req)) => {
                // リクエストが完了 - HTTP/1.1と同様のロジックで処理
                let stream_id = req.stream_id;
                
                // ストリーム情報を取得
                let (method, path, authority, body_len) = {
                    if let Some(stream) = conn.get_stream(stream_id) {
                        let method = stream.method().map(|m| m.to_vec()).unwrap_or_else(|| b"GET".to_vec());
                        let path = stream.path().map(|p| p.to_vec()).unwrap_or_else(|| b"/".to_vec());
                        // :authority を取得、見つからない場合は host ヘッダーにフォールバック
                        let authority = stream.authority()
                            .map(|a| a.to_vec())
                            .or_else(|| {
                                // :authority が無い場合は host ヘッダーを確認
                                stream.request_headers.iter()
                                    .find(|h| h.name.eq_ignore_ascii_case(b"host"))
                                    .map(|h| h.value.clone())
                            })
                            .unwrap_or_default();
                        let body_len = stream.request_body.len();
                        (method, path, authority, body_len)
                    } else {
                        continue;
                    }
                };
                
                // メトリクス: 最初のリクエストでホスト名を取得し、インクリメント
                if let Ok(host_str) = std::str::from_utf8(&authority) {
                    connection_metric.set_host(host_str.to_string());
                } else {
                    connection_metric.set_host("unknown".to_string());
                }
                
                // 処理時間計測開始
                let start_instant = Instant::now();
                
                // HTTP/2 リクエスト処理
                let result = handle_http2_single_request(
                    conn,
                    stream_id,
                    &method,
                    &path,
                    &authority,
                    body_len,
                    client_ip,
                ).await;
                
                // User-Agentを取得
                let user_agent: Box<[u8]> = if let Some(stream) = conn.get_stream(stream_id) {
                    stream.request_headers.iter()
                        .find(|h| h.name.eq_ignore_ascii_case(b"user-agent"))
                        .map(|h| Box::from(h.value.clone()))
                        .unwrap_or_else(|| Box::from([] as [u8; 0]))
                } else {
                    Box::from([] as [u8; 0])
                };
                
                // アクセスログ出力（log_access内でrecord_request_metricsも呼ばれるため、個別の呼び出しは不要）
                let (status, resp_size) = result.unwrap_or((500, 0));
                log_access(&method, &authority, &path, &user_agent, body_len as u64, status, resp_size, start_instant);
            }
            Ok(None) => {
                // フレーム処理完了、次のフレームへ
            }
            Err(e) => {
                if e.should_goaway() {
                    let _ = conn.send_goaway(e.error_code(), e.to_string().as_bytes()).await;
                    return Err(e);
                } else if let Some(id) = e.rst_stream_id() {
                    let _ = conn.send_rst_stream(id, e.error_code()).await;
                }
            }
        }
        
        // クリーンアップ
        conn.cleanup_closed();
    }
    
    Ok(())
}

/// HTTP/2 単一リクエスト処理
#[cfg(feature = "http2")]
async fn handle_http2_single_request<S>(
    conn: &mut http2::Http2Connection<S>,
    stream_id: u32,
    method: &[u8],
    path: &[u8],
    authority: &[u8],
    body_len: usize,
    client_ip: &str,
) -> Option<(u16, u64)>
where
    S: monoio::io::AsyncReadRent + monoio::io::AsyncWriteRentExt + Unpin,
{
    // メトリクスエンドポイントの処理（設定可能なパス）
    {
        let config = CURRENT_CONFIG.load();
        let prom_config = &config.prometheus_config;
        
        let path_str = std::str::from_utf8(path).unwrap_or("/");
        if prom_config.enabled 
            && path_str == prom_config.path 
            && method == b"GET" 
        {
            // IPアドレス制限チェック
            if !prom_config.is_ip_allowed(client_ip) {
                let server_guard = get_server_header_guard();
                let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
                if let Some(ref g) = server_guard {
                    headers.push(g.as_header());
                }
                let _ = conn.send_response(stream_id, 403, &headers, Some(b"Forbidden")).await;
                return Some((403, 9));
            }
            
            let body = encode_prometheus_metrics();
            let server_guard = get_server_header_guard();
            let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(3);
            headers.push((b"content-type", b"text/plain; version=0.0.4; charset=utf-8"));
            if let Some(ref g) = server_guard {
                headers.push(g.as_header());
            }
            if let Err(e) = conn.send_response(stream_id, 200, &headers, Some(&body)).await {
                warn!("[HTTP/2] Metrics response error: {}", e);
                return None;
            }
            return Some((200, body.len() as u64));
        }
    }
    
    // Backend選択（統合ルーティング）
    let config = CURRENT_CONFIG.load();

    // HTTP/2 ヘッダーをバイト列ペアとして収集
    // conn の borrow 解放のため Vec<u8> にコピー（String アロケーション・HashMap 不要）
    let h2_headers_store: Vec<(Vec<u8>, Vec<u8>)> = if let Some(stream) = conn.get_stream(stream_id) {
        stream.request_headers.iter()
            .map(|h| (h.name.clone(), h.value.clone()))
            .collect()
    } else {
        Vec::new()
    };
    let headers_raw: Vec<(&[u8], &[u8])> = h2_headers_store.iter()
        .map(|(k, v)| (k.as_slice(), v.as_slice()))
        .collect();

    // パス/クエリ分離（スキャンを1回に統一）
    let query_start_pos = path.iter().position(|&b| b == b'?');
    let raw_query: &[u8] = query_start_pos.map(|i| &path[i + 1..]).unwrap_or(b"");
    // パスからクエリ部分を除去
    let path_without_query = query_start_pos.map(|i| &path[..i]).unwrap_or(path);
    
    // クライアントIPをSocketAddrに変換
    let client_socket_addr = if let Ok(addr) = client_ip.parse::<SocketAddr>() {
        addr
    } else {
        // IPアドレスのみの場合、ポート80を仮定
        if let Ok(ip) = client_ip.parse::<std::net::IpAddr>() {
            SocketAddr::new(ip, 80)
        } else {
            // パースに失敗した場合はデフォルト
            SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)), 0)
        }
    };
    
    let backend_result = find_backend_unified(
        authority,
        path_without_query,
        method,
        &headers_raw,
        raw_query,
        &client_socket_addr,
        config.route.as_slice(),
        &config.upstream_groups,
    )
    .or_else(|| {
        // authority が空でない場合、デフォルトルートを検索
        if !authority.is_empty() {
            debug!("[HTTP/2] No route found for authority '{}', trying default routes",
                   String::from_utf8_lossy(authority));
            find_backend_unified(
                b"",
                path_without_query,
                method,
                &headers_raw,
                raw_query,
                &client_socket_addr,
                config.route.as_slice(),
                &config.upstream_groups,
            )
        } else {
            None
        }
    });
    
    let (prefix, backend) = match backend_result {
        Some(b) => b,
        None => {
            warn!(
                "[HTTP/2] No backend found for authority='{}' path='{}'",
                String::from_utf8_lossy(authority),
                String::from_utf8_lossy(path)
            );
            let server_guard = get_server_header_guard();
            let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
            if let Some(ref g) = server_guard {
                headers.push(g.as_header());
            }
            let _ = conn.send_response(stream_id, 404, &headers, Some(b"Not Found")).await;
            return Some((404, 9));
        }
    };
    
    // セキュリティチェック（共通関数を使用）
    let security = backend.security();
    let check_result = check_security(security, client_ip, method, body_len, false);
    
    if check_result != SecurityCheckResult::Allowed {
        let status = check_result.status_code();
        let msg = check_result.message();
        let server_guard = get_server_header_guard();
        let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
        if let Some(ref g) = server_guard {
            headers.push(g.as_header());
        }
        let _ = conn.send_response(stream_id, status, &headers, Some(msg)).await;
        return Some((status, msg.len() as u64));
    }
    
    // WASMモジュールの適用
    #[cfg(feature = "wasm")]
    let wasm_modules_to_apply: Vec<String> = {
        let config = CURRENT_CONFIG.load();
        if let Some(ref wasm_engine) = config.wasm_filter_engine {
            let path_str = std::str::from_utf8(path).unwrap_or("/");
            let method_str = std::str::from_utf8(method).unwrap_or("GET");
            
            let modules_to_apply = if let Some(backend_modules) = backend.modules() {
                backend_modules.to_vec()
            } else {
                // ルートレベルのmodulesが指定されていない場合は、WASMモジュールを適用しない
                Vec::new()
            };
            
            if !modules_to_apply.is_empty() {
                // HTTP/2のヘッダーを取得
                let headers_vec: Vec<(Vec<u8>, Vec<u8>)> = if let Some(stream) = conn.get_stream(stream_id) {
                    stream.request_headers.iter()
                        .map(|h| (h.name.clone(), h.value.clone()))
                        .collect()
                } else {
                    Vec::new()
                };

                let wasm_result = wasm_engine.clone().on_request_headers_with_modules_async(
                    modules_to_apply.clone(),
                    path_str.to_string(),
                    method_str.to_string(),
                    headers_vec,
                    client_ip.to_string(),
                    body_len == 0, // end_of_stream
                ).await;

                match wasm_result {
                    crate::wasm::FilterResult::LocalResponse(resp) => {
                        // ローカルレスポンスを返送
                        let server_guard = get_server_header_guard();
                        let mut headers: Vec<(&[u8], &[u8])> = resp.headers.iter()
                            .map(|(k, v)| (k.as_slice(), v.as_slice()))
                            .collect();
                        if let Some(ref g) = server_guard {
                            headers.push(g.as_header());
                        }
                        
                        let _ = conn.send_response(stream_id, resp.status_code, &headers, Some(&resp.body)).await;
                        // ライフサイクルコールバック: リクエスト完了
                        crate::wasm::on_request_complete_async(wasm_engine.clone(), modules_to_apply.to_vec()).await;
                        return Some((resp.status_code, resp.body.len() as u64));
                    }
                    crate::wasm::FilterResult::Pause => {
                        warn!("WASM module requested pause, but async operations are not yet supported");
                    }
                    crate::wasm::FilterResult::Continue { .. } => {
                        // ヘッダー変更はHTTP/2では複雑なため、現時点ではスキップ
                        // 将来的に実装可能
                    }
                }
            }
            modules_to_apply
        } else {
            Vec::new()
        }
    };
    
    // Accept-Encoding を取得
    let client_encoding = if let Some(stream) = conn.get_stream(stream_id) {
        stream.request_headers.iter()
            .find(|h| h.name.eq_ignore_ascii_case(b"accept-encoding"))
            .map(|h| AcceptedEncoding::parse(&h.value))
            .unwrap_or(AcceptedEncoding::Identity)
    } else {
        AcceptedEncoding::Identity
    };
    
    // Backend処理
    let result = match backend {
        Backend::Proxy(upstream_group, _, compression, _buffering, _cache, _) => {
            handle_http2_proxy(conn, stream_id, &upstream_group, &compression, client_encoding, method, path, &prefix, client_ip).await
        }
        Backend::MemoryFile(data, mime_type, security, _) => {
            // ファイル完全一致チェック
            let path_str = std::str::from_utf8(path).unwrap_or("/");
            let prefix_str = std::str::from_utf8(&prefix).unwrap_or("");
            
            let remainder = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
                &path_str[prefix_str.len()..]
            } else {
                ""
            };
            
            let clean_remainder = remainder.trim_matches('/');
            if !clean_remainder.is_empty() {
                let server_guard = get_server_header_guard();
                let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
                if let Some(ref g) = server_guard {
                    headers.push(g.as_header());
                }
                let _ = conn.send_response(stream_id, 404, &headers, Some(b"Not Found")).await;
                Some((404, 9))
            } else {
                let server_guard = get_server_header_guard();
                let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(4);
                headers.push((b"content-type", mime_type.as_bytes()));
                if let Some(ref g) = server_guard {
                    headers.push(g.as_header());
                }
                
                // セキュリティヘッダー追加
                for (k, v) in &security.add_response_headers {
                    headers.push((k.as_bytes(), v.as_bytes()));
                }

                if let Err(e) = conn.send_response(stream_id, 200, &headers, Some(&data)).await {
                    warn!("[HTTP/2] Memory file response error: {}", e);
                    None
                } else {
                    Some((200, data.len() as u64))
                }
            }
        }
        Backend::SendFile(base_path, is_dir, index_file, security, _cache, _open_file_cache_config, _) => {
            handle_http2_sendfile(conn, stream_id, &base_path, is_dir, index_file.as_deref(), path, &prefix, &security).await
        }
        Backend::Redirect(redirect_url, status_code, preserve_path, _) => {
            handle_http2_redirect(conn, stream_id, &redirect_url, status_code, preserve_path, path, &prefix).await
        }
    };
    
    // WASMライフサイクルコールバック: リクエスト完了
    #[cfg(feature = "wasm")]
    {
        if !wasm_modules_to_apply.is_empty() {
            let config = CURRENT_CONFIG.load();
            if let Some(ref wasm_engine) = config.wasm_filter_engine {
                crate::wasm::on_request_complete(wasm_engine, &wasm_modules_to_apply);
            }
        }
    }
    
    result
}

/// HTTP/2 プロキシ処理（HTTP/1.1バックエンドへ変換）
#[cfg(feature = "http2")]
async fn handle_http2_proxy<S>(
    conn: &mut http2::Http2Connection<S>,
    stream_id: u32,
    upstream_group: &Arc<UpstreamGroup>,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    method: &[u8],
    req_path: &[u8],
    prefix: &[u8],
    client_ip: &str,
) -> Option<(u16, u64)>
where
    S: monoio::io::AsyncReadRent + monoio::io::AsyncWriteRentExt + Unpin,
{
    // サーバー選択
    let server = match upstream_group.select(client_ip) {
        Some(s) => s,
        None => {
            let server_guard = get_server_header_guard();
            let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
            if let Some(ref g) = server_guard {
                headers.push(g.as_header());
            }
            let _ = conn.send_response(stream_id, 502, &headers, Some(b"Bad Gateway")).await;
            return Some((502, 11));
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
            build_sub_path(base, remaining)
        } else {
            path_str.to_string()
        }
    };

    let final_path = if sub_path.is_empty() { "/" } else { &sub_path };

    // リクエストボディを取得
    let request_body = if let Some(stream) = conn.get_stream(stream_id) {
        stream.request_body.clone()
    } else {
        Vec::new()
    };
    
    // HTTP/1.1 リクエスト構築（プール使用）
    let mut request = request_buf_get(1024);
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
    
    // リクエストヘッダーを追加（疑似ヘッダー以外）
    if let Some(stream) = conn.get_stream(stream_id) {
        for header in &stream.request_headers {
            // 疑似ヘッダーをスキップ
            if header.name.starts_with(b":") {
                continue;
            }
            // ホップバイホップヘッダーをスキップ
            if header.name.eq_ignore_ascii_case(b"connection") ||
               header.name.eq_ignore_ascii_case(b"keep-alive") ||
               header.name.eq_ignore_ascii_case(b"transfer-encoding") {
                continue;
            }
            request.extend_from_slice(&header.name);
            request.extend_from_slice(b": ");
            request.extend_from_slice(&header.value);
            request.extend_from_slice(b"\r\n");
        }
    }
    
    // Content-Length追加（ボディがある場合）
    if !request_body.is_empty() {
        request.extend_from_slice(b"Content-Length: ");
        let mut len_buf = itoa::Buffer::new();
        request.extend_from_slice(len_buf.format(request_body.len()).as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    
    request.extend_from_slice(b"Connection: keep-alive\r\n\r\n");
    request.extend_from_slice(&request_body);
    
    // バックエンドに接続して転送
    let addr = format!("{}:{}", target.host, target.port);
    
    let result = if target.use_h2c {
        // H2C (Prior Knowledge) プロキシ
        handle_http2_proxy_h2c(conn, stream_id, &addr, target, request_body, method, final_path.as_bytes()).await
    } else if target.use_tls {
        // HTTP/2 → HTTPS (HTTP/1.1)
        handle_http2_proxy_https(conn, stream_id, &addr, target.sni(), request, compression, client_encoding).await
    } else {
        // HTTP/2 → HTTP (HTTP/1.1)
        handle_http2_proxy_http(conn, stream_id, &addr, request, compression, client_encoding).await
    };
    
    server.release();
    result
}

/// HTTP/2 → HTTP/2 プロキシ (H2C)
#[cfg(feature = "http2")]
async fn handle_http2_proxy_h2c<S>(
    conn: &mut http2::Http2Connection<S>,
    stream_id: u32,
    addr: &str,
    target: &ProxyTarget,
    request_body: Vec<u8>,
    method: &[u8],
    path: &[u8],
) -> Option<(u16, u64)>
where
    S: monoio::io::AsyncReadRent + monoio::io::AsyncWriteRentExt + Unpin,
{
    // バックエンドに接続
    let connect_result = timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await;
    
    let backend_stream = match connect_result {
        Ok(Ok(stream)) => {
            let _ = stream.set_nodelay(true);
            stream
        }
        Ok(Err(e)) => {
            warn!("[HTTP/2] H2C backend connect error ({}): {}", addr, e);
            let server_guard = get_server_header_guard();
            let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
            if let Some(ref g) = server_guard {
                headers.push(g.as_header());
            }
            let _ = conn.send_response(stream_id, 502, &headers, Some(b"Bad Gateway")).await;
            return Some((502, 11));
        }
        Err(_) => {
            let server_guard = get_server_header_guard();
            let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
            if let Some(ref g) = server_guard {
                headers.push(g.as_header());
            }
            let _ = conn.send_response(stream_id, 504, &headers, Some(b"Gateway Timeout")).await;
            return Some((504, 15));
        }
    };

    // H2Cクライアント作成
    let settings = http2::Http2Settings::default();
    let mut h2c_client = http2::H2cClient::new(backend_stream, settings);

    // H2Cハンドシェイク
    if let Err(e) = h2c_client.handshake().await {
        warn!("[HTTP/2] H2C handshake error ({}): {}", addr, e);
        let server_guard = get_server_header_guard();
        let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
        if let Some(ref g) = server_guard {
            headers.push(g.as_header());
        }
        let _ = conn.send_response(stream_id, 502, &headers, Some(b"Bad Gateway")).await;
        return Some((502, 11));
    }

    // ヘッダーを抽出
    let headers_vec: Vec<(&[u8], &[u8])> = if let Some(stream) = conn.get_stream(stream_id) {
        stream.request_headers.iter()
            .filter(|h| !h.name.starts_with(b":")) // 疑似ヘッダーを除外
            .filter(|h| {
                // ホップバイホップヘッダーを除外
                !h.name.eq_ignore_ascii_case(b"connection") &&
                !h.name.eq_ignore_ascii_case(b"keep-alive") &&
                !h.name.eq_ignore_ascii_case(b"proxy-connection") &&
                !h.name.eq_ignore_ascii_case(b"transfer-encoding") &&
                !h.name.eq_ignore_ascii_case(b"te") &&
                !h.name.eq_ignore_ascii_case(b"upgrade")
            })
            .map(|h| (h.name.as_ref(), h.value.as_ref()))
            .collect()
    } else {
        Vec::new()
    };

    // リクエスト送信
    let body = if request_body.is_empty() { None } else { Some(request_body.as_slice()) };
    let authority = target.host.as_bytes();
    
    match h2c_client.send_request(method, path, authority, &headers_vec, body).await {
        Ok(h2c_resp) => {
            // レスポンスをクライアントに中継
            let mut headers: Vec<(&[u8], &[u8])> = h2c_resp.headers.iter()
                .map(|(k, v)| (k.as_slice(), v.as_slice()))
                .collect();
            
            let server_guard = get_server_header_guard();
            if let Some(ref g) = server_guard {
                headers.push(g.as_header());
            }
            
            let has_body = !h2c_resp.body.is_empty();
            let has_trailers = !h2c_resp.trailers.is_empty();
            
            // ヘッダーを送信
            if let Err(e) = conn.send_headers(stream_id, h2c_resp.status, &headers, !has_body && !has_trailers).await {
                warn!("[HTTP/2] H2C send headers error (stream_id={}): {}", stream_id, e);
                return None;
            }
            
            // ボディを送信
            if has_body {
                if let Err(e) = conn.send_data(stream_id, &h2c_resp.body, !has_trailers).await {
                    warn!("[HTTP/2] H2C send data error (stream_id={}): {}", stream_id, e);
                    return None;
                }
            }
            
            // トレイラーを送信
            if has_trailers {
                // 特別に gRPC トレイラーを送信
                #[cfg(feature = "grpc")]
                {
                    let mut grpc_status = 0;
                    let mut grpc_message = None;

                    for (name, value) in &h2c_resp.trailers {
                        if name == b"grpc-status" {
                            if let Ok(status_str) = std::str::from_utf8(value) {
                                grpc_status = status_str.parse().unwrap_or(0);
                            }
                        } else if name == b"grpc-message" {
                            grpc_message = std::str::from_utf8(value).ok();
                        }
                    }

                    if let Err(e) = conn.send_grpc_trailers(stream_id, grpc_status, grpc_message).await {
                        warn!("[HTTP/2] H2C send trailers error (stream_id={}): {}", stream_id, e);
                        return None;
                    }
                }
                #[cfg(not(feature = "grpc"))]
                {
                    // gRPC feature なしの場合、トレイラーをスキップ
                    let _ = &h2c_resp.trailers;
                }
            }
            
            Some((h2c_resp.status, h2c_resp.body.len() as u64))
        }
        Err(e) => {
            warn!("[HTTP/2] H2C request error ({}): {}", addr, e);
            let server_guard = get_server_header_guard();
            let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
            if let Some(ref g) = server_guard {
                headers.push(g.as_header());
            }
            let _ = conn.send_response(stream_id, 502, &headers, Some(b"Bad Gateway")).await;
            Some((502, 11))
        }
    }
}

/// HTTP/2 → HTTP/1.1 プロキシ（HTTPバックエンド）
#[cfg(feature = "http2")]
async fn handle_http2_proxy_http<S>(
    conn: &mut http2::Http2Connection<S>,
    stream_id: u32,
    addr: &str,
    request: Vec<u8>,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
) -> Option<(u16, u64)>
where
    S: monoio::io::AsyncReadRent + monoio::io::AsyncWriteRentExt + Unpin,
{
    // バックエンドに接続
    let connect_result = timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await;
    
    let mut backend = match connect_result {
        Ok(Ok(stream)) => {
            let _ = stream.set_nodelay(true);
            stream
        }
        Ok(Err(e)) => {
            warn!("[HTTP/2] Backend connect error: {}", e);
            let server_guard = get_server_header_guard();
            let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
            if let Some(ref g) = server_guard {
                headers.push(g.as_header());
            }
            let _ = conn.send_response(stream_id, 502, &headers, Some(b"Bad Gateway")).await;
            return Some((502, 11));
        }
        Err(_) => {
            let server_guard = get_server_header_guard();
            let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
            if let Some(ref g) = server_guard {
                headers.push(g.as_header());
            }
            let _ = conn.send_response(stream_id, 504, &headers, Some(b"Gateway Timeout")).await;
            return Some((504, 15));
        }
    };
    
    // リクエスト送信
    let (write_res, returned_request) = backend.write_all(request).await;
    request_buf_put(returned_request);
    if write_res.is_err() {
        let server_guard = get_server_header_guard();
        let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
        if let Some(ref g) = server_guard {
            headers.push(g.as_header());
        }
        let _ = conn.send_response(stream_id, 502, &headers, Some(b"Bad Gateway")).await;
        return Some((502, 11));
    }
    
    // レスポンス受信
    let mut response_buf = Vec::with_capacity(BUF_SIZE);
    
    loop {
        let buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, backend.read(buf)).await;
        
        let (res, mut returned_buf) = match read_result {
            Ok(r) => r,
            Err(_) => {
                let server_guard = get_server_header_guard();
                let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
                if let Some(ref g) = server_guard {
                    headers.push(g.as_header());
                }
                let _ = conn.send_response(stream_id, 504, &headers, Some(b"Gateway Timeout")).await;
                return Some((504, 15));
            }
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                break;
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                let server_guard = get_server_header_guard();
                let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
                if let Some(ref g) = server_guard {
                    headers.push(g.as_header());
                }
                let _ = conn.send_response(stream_id, 502, &headers, Some(b"Bad Gateway")).await;
                return Some((502, 11));
            }
        };
        
        returned_buf.set_valid_len(n);
        response_buf.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);
        
        // ヘッダーが完了したかチェック
        if let Some(parsed) = parse_http_response(&response_buf) {
            // HTTP/1.1 レスポンスを HTTP/2 に変換
            let status = parsed.status_code;
            let body_start = parsed.header_len;
            let body = &response_buf[body_start..];
            
            // レスポンスヘッダーを解析
            let mut headers_storage = [httparse::EMPTY_HEADER; 64];
            let mut resp = httparse::Response::new(&mut headers_storage);
            let _ = resp.parse(&response_buf);
            
            // Content-Type と Content-Encoding を取得
            let content_type = resp.headers.iter()
                .find(|h| h.name.eq_ignore_ascii_case("content-type"))
                .map(|h| h.value);
            let existing_encoding = resp.headers.iter()
                .find(|h| h.name.eq_ignore_ascii_case("content-encoding"))
                .map(|h| h.value);
            
            // Content-Length が chunked の場合は計算
            let final_body = if parsed.is_chunked {
                // Chunked レスポンスの場合、終端検出しながら読み込み
                let mut decoder = ChunkedDecoder::new_unlimited();
                let mut full_body = body.to_vec();
                decoder.feed(body);
                
                while !decoder.is_complete() {
                    let buf = buf_get();
                    let read_result = timeout(READ_TIMEOUT, backend.read(buf)).await;
                    let (res, mut returned_buf) = match read_result {
                        Ok(r) => r,
                        Err(_) => break,
                    };
                    
                    let n = match res {
                        Ok(0) => { buf_put(returned_buf); break; }
                        Ok(n) => n,
                        Err(_) => { buf_put(returned_buf); break; }
                    };
                    
                    returned_buf.set_valid_len(n);
                    full_body.extend_from_slice(returned_buf.as_valid_slice());
                    decoder.feed(returned_buf.as_valid_slice());
                    buf_put(returned_buf);
                }
                // Chunkedデコード: 生のボディを抽出
                decode_chunked_body(&full_body)
            } else if let Some(content_len) = parsed.content_length {
                // 残りのボディを読む
                let mut full_body = body.to_vec();
                while full_body.len() < content_len {
                    let buf = buf_get();
                    let read_result = timeout(READ_TIMEOUT, backend.read(buf)).await;
                    let (res, mut returned_buf) = match read_result {
                        Ok(r) => r,
                        Err(_) => break,
                    };
                    
                    let n = match res {
                        Ok(0) => { buf_put(returned_buf); break; }
                        Ok(n) => n,
                        Err(_) => { buf_put(returned_buf); break; }
                    };
                    
                    returned_buf.set_valid_len(n);
                    full_body.extend_from_slice(returned_buf.as_valid_slice());
                    buf_put(returned_buf);
                }
                full_body
            } else {
                body.to_vec()
            };
            
            // 圧縮すべきかどうかを判定
            let should_compress = compression.should_compress(
                client_encoding,
                content_type,
                Some(final_body.len()),
                existing_encoding,
            );
            
            // HTTP/2用のヘッダーを構築（ホップバイホップヘッダー除外）
            let server_guard = get_server_header_guard();
            let mut h2_headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(16);
            if let Some(ref g) = server_guard {
                h2_headers.push(g.as_header());
            }
            
            // 圧縮が有効な場合は Content-Encoding を追加（静的スライス、ゼロアロケーション）
            if let Some(enc) = should_compress {
                let encoding_name: &'static [u8] = match enc {
                    AcceptedEncoding::Zstd => b"zstd",
                    AcceptedEncoding::Brotli => b"br",
                    AcceptedEncoding::Gzip => b"gzip",
                    AcceptedEncoding::Deflate => b"deflate",
                    AcceptedEncoding::Identity => b"",
                };
                if !encoding_name.is_empty() {
                    h2_headers.push((b"content-encoding", encoding_name));
                    h2_headers.push((b"vary", b"Accept-Encoding"));
                }
            }
            
            for header in resp.headers.iter() {
                if header.name.is_empty() {
                    continue;
                }
                // ホップバイホップヘッダーを除外
                if header.name.eq_ignore_ascii_case("connection") ||
                   header.name.eq_ignore_ascii_case("keep-alive") ||
                   header.name.eq_ignore_ascii_case("transfer-encoding") ||
                   header.name.eq_ignore_ascii_case("upgrade") {
                    continue;
                }
                // 圧縮時は Content-Length と Content-Encoding をスキップ
                if should_compress.is_some() && (
                    header.name.eq_ignore_ascii_case("content-length") ||
                    header.name.eq_ignore_ascii_case("content-encoding")
                ) {
                    continue;
                }
                h2_headers.push((header.name.as_bytes(), header.value));
            }
            
            // 圧縮処理
            let response_body = if let Some(enc) = should_compress {
                compress_body_h2(&final_body, enc, compression)
            } else {
                final_body
            };
            
            // HTTP/2 レスポンス送信
            if let Err(e) = conn.send_response(stream_id, status, &h2_headers, Some(&response_body)).await {
                warn!("[HTTP/2] Response send error: {}", e);
                return None;
            }
            
            return Some((status, response_body.len() as u64));
        }
        
        // ヘッダーが大きすぎる
        if response_buf.len() > MAX_HEADER_SIZE {
            let server_guard = get_server_header_guard();
            let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
            if let Some(ref g) = server_guard {
                headers.push(g.as_header());
            }
            let _ = conn.send_response(stream_id, 502, &headers, Some(b"Bad Gateway")).await;
            return Some((502, 11));
        }
    }
    
    // ストリーム終了（空レスポンス）
    let server_guard = get_server_header_guard();
    let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
    if let Some(ref g) = server_guard {
        headers.push(g.as_header());
    }
    let _ = conn.send_response(stream_id, 502, &headers, Some(b"Bad Gateway")).await;
    Some((502, 11))
}

/// HTTP/2 → HTTP/1.1 プロキシ（HTTPSバックエンド）
#[cfg(feature = "http2")]
async fn handle_http2_proxy_https<S>(
    conn: &mut http2::Http2Connection<S>,
    stream_id: u32,
    addr: &str,
    sni: &str,
    request: Vec<u8>,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
) -> Option<(u16, u64)>
where
    S: monoio::io::AsyncReadRent + monoio::io::AsyncWriteRentExt + Unpin,
{
    // バックエンドに TCP 接続
    let connect_result = timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await;
    
    let backend_tcp = match connect_result {
        Ok(Ok(stream)) => {
            let _ = stream.set_nodelay(true);
            stream
        }
        Ok(Err(e)) => {
            warn!("[HTTP/2] Backend connect error: {}", e);
            let server_guard = get_server_header_guard();
            let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
            if let Some(ref g) = server_guard {
                headers.push(g.as_header());
            }
            let _ = conn.send_response(stream_id, 502, &headers, Some(b"Bad Gateway")).await;
            return Some((502, 11));
        }
        Err(_) => {
            let server_guard = get_server_header_guard();
            let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
            if let Some(ref g) = server_guard {
                headers.push(g.as_header());
            }
            let _ = conn.send_response(stream_id, 504, &headers, Some(b"Gateway Timeout")).await;
            return Some((504, 15));
        }
    };
    
    // TLS ハンドシェイク
    let connector = get_tls_connector();
    let tls_result = timeout(CONNECT_TIMEOUT, connector.connect(backend_tcp, sni)).await;
    
    let mut backend = match tls_result {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            warn!("[HTTP/2] TLS handshake error: {}", e);
            let server_guard = get_server_header_guard();
            let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
            if let Some(ref g) = server_guard {
                headers.push(g.as_header());
            }
            let _ = conn.send_response(stream_id, 502, &headers, Some(b"Bad Gateway")).await;
            return Some((502, 11));
        }
        Err(_) => {
            let server_guard = get_server_header_guard();
            let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
            if let Some(ref g) = server_guard {
                headers.push(g.as_header());
            }
            let _ = conn.send_response(stream_id, 504, &headers, Some(b"Gateway Timeout")).await;
            return Some((504, 15));
        }
    };
    
    // リクエスト送信
    let (write_res, returned_request) = backend.write_all(request).await;
    request_buf_put(returned_request);
    if write_res.is_err() {
        let server_guard = get_server_header_guard();
        let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
        if let Some(ref g) = server_guard {
            headers.push(g.as_header());
        }
        let _ = conn.send_response(stream_id, 502, &headers, Some(b"Bad Gateway")).await;
        return Some((502, 11));
    }
    
    // レスポンス受信（HTTP と同様）
    let mut response_buf = Vec::with_capacity(BUF_SIZE);
    
    loop {
        let buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, backend.read(buf)).await;
        
        let (res, mut returned_buf) = match read_result {
            Ok(r) => r,
            Err(_) => {
                let server_guard = get_server_header_guard();
                let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
                if let Some(ref g) = server_guard {
                    headers.push(g.as_header());
                }
                let _ = conn.send_response(stream_id, 504, &headers, Some(b"Gateway Timeout")).await;
                return Some((504, 15));
            }
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                break;
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                break;
            }
        };
        
        returned_buf.set_valid_len(n);
        response_buf.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);
        
        // ヘッダーが完了したかチェック
        if let Some(parsed) = parse_http_response(&response_buf) {
            let status = parsed.status_code;
            let body_start = parsed.header_len;
            let body = &response_buf[body_start..];
            
            // レスポンスヘッダーを解析
            let mut headers_storage = [httparse::EMPTY_HEADER; 64];
            let mut resp = httparse::Response::new(&mut headers_storage);
            let _ = resp.parse(&response_buf);
            
            // Content-Type と Content-Encoding を取得
            let content_type = resp.headers.iter()
                .find(|h| h.name.eq_ignore_ascii_case("content-type"))
                .map(|h| h.value);
            let existing_encoding = resp.headers.iter()
                .find(|h| h.name.eq_ignore_ascii_case("content-encoding"))
                .map(|h| h.value);
            
            // ボディを読む（Chunked対応）
            let final_body = if parsed.is_chunked {
                // Chunked レスポンスの場合、終端検出しながら読み込み
                let mut decoder = ChunkedDecoder::new_unlimited();
                let mut full_body = body.to_vec();
                decoder.feed(body);
                
                while !decoder.is_complete() {
                    let buf = buf_get();
                    let read_result = timeout(READ_TIMEOUT, backend.read(buf)).await;
                    let (res, mut returned_buf) = match read_result {
                        Ok(r) => r,
                        Err(_) => break,
                    };
                    
                    let n = match res {
                        Ok(0) => { buf_put(returned_buf); break; }
                        Ok(n) => n,
                        Err(_) => { buf_put(returned_buf); break; }
                    };
                    
                    returned_buf.set_valid_len(n);
                    full_body.extend_from_slice(returned_buf.as_valid_slice());
                    decoder.feed(returned_buf.as_valid_slice());
                    buf_put(returned_buf);
                }
                // Chunkedデコード: 生のボディを抽出
                decode_chunked_body(&full_body)
            } else if let Some(content_len) = parsed.content_length {
                let mut full_body = body.to_vec();
                while full_body.len() < content_len {
                    let buf = buf_get();
                    let read_result = timeout(READ_TIMEOUT, backend.read(buf)).await;
                    let (res, mut returned_buf) = match read_result {
                        Ok(r) => r,
                        Err(_) => break,
                    };
                    
                    let n = match res {
                        Ok(0) => { buf_put(returned_buf); break; }
                        Ok(n) => n,
                        Err(_) => { buf_put(returned_buf); break; }
                    };
                    
                    returned_buf.set_valid_len(n);
                    full_body.extend_from_slice(returned_buf.as_valid_slice());
                    buf_put(returned_buf);
                }
                full_body
            } else {
                body.to_vec()
            };
            
            // 圧縮すべきかどうかを判定
            let should_compress = compression.should_compress(
                client_encoding,
                content_type,
                Some(final_body.len()),
                existing_encoding,
            );
            
            // HTTP/2用のヘッダーを構築
            let server_guard = get_server_header_guard();
            let mut h2_headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(16);
            if let Some(ref g) = server_guard {
                h2_headers.push(g.as_header());
            }
            
            // 圧縮が有効な場合は Content-Encoding を追加（静的スライス、ゼロアロケーション）
            if let Some(enc) = should_compress {
                let encoding_name: &'static [u8] = match enc {
                    AcceptedEncoding::Zstd => b"zstd",
                    AcceptedEncoding::Brotli => b"br",
                    AcceptedEncoding::Gzip => b"gzip",
                    AcceptedEncoding::Deflate => b"deflate",
                    AcceptedEncoding::Identity => b"",
                };
                if !encoding_name.is_empty() {
                    h2_headers.push((b"content-encoding", encoding_name));
                    h2_headers.push((b"vary", b"Accept-Encoding"));
                }
            }

            for header in resp.headers.iter() {
                if header.name.is_empty() {
                    continue;
                }
                // ホップバイホップヘッダーを除外
                if header.name.eq_ignore_ascii_case("connection") ||
                   header.name.eq_ignore_ascii_case("keep-alive") ||
                   header.name.eq_ignore_ascii_case("transfer-encoding") {
                    continue;
                }
                // 圧縮時は Content-Length と Content-Encoding をスキップ
                if should_compress.is_some() && (
                    header.name.eq_ignore_ascii_case("content-length") ||
                    header.name.eq_ignore_ascii_case("content-encoding")
                ) {
                    continue;
                }
                h2_headers.push((header.name.as_bytes(), header.value));
            }
            
            // 圧縮処理
            let response_body = if let Some(enc) = should_compress {
                compress_body_h2(&final_body, enc, compression)
            } else {
                final_body
            };
            
            if let Err(e) = conn.send_response(stream_id, status, &h2_headers, Some(&response_body)).await {
                warn!("[HTTP/2] Response send error: {}", e);
                return None;
            }
            
            return Some((status, response_body.len() as u64));
        }
        
        if response_buf.len() > MAX_HEADER_SIZE {
            break;
        }
    }
    
    let server_guard = get_server_header_guard();
    let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
    if let Some(ref g) = server_guard {
        headers.push(g.as_header());
    }
    let _ = conn.send_response(stream_id, 502, &headers, Some(b"Bad Gateway")).await;
    Some((502, 11))
}


/// HTTP/2 用レスポンスボディ圧縮ヘルパー関数
///
/// バイト配列を受け取り、指定されたエンコーディングで圧縮して返します。
/// 圧縮に失敗した場合は元のデータをそのまま返します。
#[cfg(all(feature = "http2", feature = "compression"))]
fn compress_body_h2(body: &[u8], encoding: AcceptedEncoding, compression: &CompressionConfig) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    match encoding {
        AcceptedEncoding::Zstd => {
            match zstd::encode_all(std::io::Cursor::new(body), compression.zstd_level) {
                Ok(compressed) => compressed,
                Err(_) => body.to_vec(),
            }
        }
        AcceptedEncoding::Gzip => {
            let level = Compression::new(compression.gzip_level);
            let mut encoder = GzEncoder::new(Vec::with_capacity(body.len()), level);
            if encoder.write_all(body).is_err() {
                return body.to_vec();
            }
            encoder.finish().unwrap_or_else(|_| body.to_vec())
        }
        AcceptedEncoding::Brotli => {
            let mut compressed = Vec::with_capacity(body.len());
            let params = brotli::enc::BrotliEncoderParams {
                quality: compression.brotli_level as i32,
                ..Default::default()
            };
            let mut input = std::io::Cursor::new(body);
            if brotli::BrotliCompress(&mut input, &mut compressed, &params).is_err() {
                return body.to_vec();
            }
            compressed
        }
        AcceptedEncoding::Deflate => {
            use flate2::write::DeflateEncoder;
            let level = Compression::new(compression.gzip_level);
            let mut encoder = DeflateEncoder::new(Vec::with_capacity(body.len()), level);
            if encoder.write_all(body).is_err() {
                return body.to_vec();
            }
            encoder.finish().unwrap_or_else(|_| body.to_vec())
        }
        AcceptedEncoding::Identity => body.to_vec(),
    }
}

/// compression feature 無効時のスタブ
#[cfg(all(feature = "http2", not(feature = "compression")))]
#[inline]
fn compress_body_h2(body: &[u8], _encoding: AcceptedEncoding, _compression: &CompressionConfig) -> Vec<u8> {
    body.to_vec()
}

/// HTTP/2 ファイル配信
#[cfg(feature = "http2")]
async fn handle_http2_sendfile<S>(
    conn: &mut http2::Http2Connection<S>,
    stream_id: u32,
    base_path: &PathBuf,
    is_dir: bool,
    index_file: Option<&str>,
    req_path: &[u8],
    prefix: &[u8],
    security: &SecurityConfig,
) -> Option<(u16, u64)>
where
    S: monoio::io::AsyncReadRent + monoio::io::AsyncWriteRentExt + Unpin,
{
    let path_str_raw = std::str::from_utf8(req_path).unwrap_or("/");
    // クエリ文字列を除去してファイルパス解決に使用するパスのみを取り出す
    let path_str = if let Some(qpos) = path_str_raw.find('?') { &path_str_raw[..qpos] } else { path_str_raw };
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
        let server_guard = get_server_header_guard();
        let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
        if let Some(ref g) = server_guard {
            headers.push(g.as_header());
        }
        let _ = conn.send_response(stream_id, 403, &headers, Some(b"Forbidden")).await;
        return Some((403, 9));
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
            let server_guard = get_server_header_guard();
            let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
            if let Some(ref g) = server_guard {
                headers.push(g.as_header());
            }
            let _ = conn.send_response(stream_id, 404, &headers, Some(b"Not Found")).await;
            return Some((404, 9));
        }
        base_path.clone()
    };
    
    // ファイル読み込み（io_uring による非同期I/O でワーカースレッドをブロックしない）
    let data = match monoio::fs::read(&file_path).await {
        Ok(d) => d,
        Err(_) => {
            let server_guard = get_server_header_guard();
            let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(2);
            if let Some(ref g) = server_guard {
                headers.push(g.as_header());
            }
            let _ = conn.send_response(stream_id, 404, &headers, Some(b"Not Found")).await;
            return Some((404, 9));
        }
    };

    let mime_type = mime_guess::from_path(&file_path).first_or_octet_stream();
    let mime_str = mime_type.as_ref();
    
    let server_guard = get_server_header_guard();
    let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(4);
    headers.push((b"content-type", mime_str.as_bytes()));
    if let Some(ref g) = server_guard {
        headers.push(g.as_header());
    }
    
    // セキュリティヘッダー追加
    for (k, v) in &security.add_response_headers {
        headers.push((k.as_bytes(), v.as_bytes()));
    }

    if let Err(e) = conn.send_response(stream_id, 200, &headers, Some(&data)).await {
        warn!("[HTTP/2] File response error: {}", e);
        return None;
    }
    
    Some((200, data.len() as u64))
}

/// HTTP/2 リダイレクト処理
#[cfg(feature = "http2")]
async fn handle_http2_redirect<S>(
    conn: &mut http2::Http2Connection<S>,
    stream_id: u32,
    redirect_url: &str,
    status_code: u16,
    preserve_path: bool,
    req_path: &[u8],
    prefix: &[u8],
) -> Option<(u16, u64)>
where
    S: monoio::io::AsyncReadRent + monoio::io::AsyncWriteRentExt + Unpin,
{
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
    
    let server_guard = get_server_header_guard();
    let mut headers: Vec<(&[u8], &[u8])> = Vec::with_capacity(3);
    headers.push((b"location", final_url.as_bytes()));
    if let Some(ref g) = server_guard {
        headers.push(g.as_header());
    }
    
    if let Err(e) = conn.send_response(stream_id, status_code, &headers, None).await {
        warn!("[HTTP/2] Redirect response error: {}", e);
        return None;
    }
    
    Some((status_code, 0))
}

// ====================
// プロトコル検出機能（H2C対応）
// ====================

/// プロトコルタイプ
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolType {
    H2C,      // HTTP/2 Cleartext (Prior Knowledge)
    TLS,      // TLS (HTTPS)
    Http11,   // HTTP/1.1
    Unknown,  // 不明
}

/// プロトコル判別で読み取った初期データを保持し、
/// 以降の読み取りでそのデータを優先して返すラッパーストリーム。
/// これにより、TLSハンドシェイク等の後続処理でデータ欠落を防ぐ。
pub struct BufferedStream<S> {
    inner: S,
    buffer: Option<Vec<u8>>,
    pos: usize,
}

impl<S> BufferedStream<S> {
    pub fn new(inner: S, initial_data: Vec<u8>) -> Self {
        let buffer = if initial_data.is_empty() {
            None
        } else {
            Some(initial_data)
        };
        BufferedStream {
            inner,
            buffer,
            pos: 0,
        }
    }
}

impl<S: AsRawFd> AsRawFd for BufferedStream<S> {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl<S: AsyncReadRent + Unpin> AsyncReadRent for BufferedStream<S> {
    async fn read<T: IoBufMut>(&mut self, mut buf: T) -> monoio::BufResult<usize, T> {
        if let Some(ref b) = self.buffer {
            let remaining = b.len() - self.pos;
            let to_copy = std::cmp::min(remaining, buf.bytes_total());
            
            unsafe {
                let slice = std::slice::from_raw_parts_mut(buf.write_ptr(), buf.bytes_total());
                slice[..to_copy].copy_from_slice(&b[self.pos..self.pos + to_copy]);
                buf.set_init(to_copy);
            }
            
            self.pos += to_copy;
            if self.pos >= b.len() {
                self.buffer = None;
            }
            
            return (Ok(to_copy), buf);
        }
        
        self.inner.read(buf).await
    }

    async fn readv<T: IoVecBufMut>(&mut self, buf: T) -> monoio::BufResult<usize, T> {
        // 現在の実装では readv は使用しないため、バッファがある場合は未サポート
        if self.buffer.is_some() {
            return (Err(io::Error::new(io::ErrorKind::Other, "readv not supported for BufferedStream with data")), buf);
        }
        self.inner.readv(buf).await
    }
}

impl<S: monoio::io::AsyncWriteRent + Unpin> monoio::io::AsyncWriteRent for BufferedStream<S> {
    async fn write<T: IoBuf>(&mut self, buf: T) -> monoio::BufResult<usize, T> {
        self.inner.write(buf).await
    }

    async fn writev<T: IoVecBuf>(&mut self, buf: T) -> monoio::BufResult<usize, T> {
        self.inner.writev(buf).await
    }

    async fn flush(&mut self) -> io::Result<()> {
        self.inner.flush().await
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        self.inner.shutdown().await
    }
}


/// プロトコル検出とバッファ管理
/// 
/// 最初の数バイトを読み込んでプロトコルを判別します。
/// 読み込んだデータはバッファに保持し、後続の処理で再利用します。
/// 
/// 検出順序:
/// 1. HTTP/2プリフェース（24バイト固定）
/// 2. TLS ClientHello（最初の5バイト）
/// 3. HTTP/1.1リクエスト（最初の3バイトでメソッド検出）
/// 
/// エラーハンドリング:
/// - 読み込みエラー: Unknownを返し、空のバッファを返す
/// - タイムアウト: 短いタイムアウトを設定して再試行（将来の改善）
pub async fn detect_protocol_with_buffer(
    stream: &mut TcpStream,
) -> (ProtocolType, Vec<u8>) {
    use monoio::io::AsyncReadRent;
    
    // 最大24バイト（HTTP/2プリフェース長）を読み込む
    // タイムアウトを設定して、無応答接続を検出
    let mut accumulated = Vec::with_capacity(24);
    let start_time = std::time::Instant::now();
    let timeout_duration = Duration::from_millis(200);
    
    while accumulated.len() < 24 {
        let remaining_timeout = match timeout_duration.checked_sub(start_time.elapsed()) {
            Some(d) if d.as_millis() > 0 => d,
            _ => break,
        };
        
        // 重要: monoio (io_uring) の read を timeout() で直接ラップしてキャンセルすると、
        // カーネルが既に読み取りバッファへコピー済みのバイトが失われる。これにより後続の
        // TLS ハンドシェイクがストリーム不整合（received corrupt message / InvalidContentType）で
        // 失敗する（負荷時にスレッドがビジーで 200ms タイムアウトが発火すると顕在化）。
        //
        // これを避けるため、まず「読み取り可能になる」のを待つ（データを消費しないため
        // キャンセルしても安全）。読み取り可能になってから timeout なしで read する。read は
        // 即座に完了するためキャンセルされず、ソケットからバイトを取りこぼさない。
        // readable がタイムアウトした場合は何も消費していないため、accumulated のまま
        // 安全にフォールバックできる（空バッファなら TLS ハンドシェイクが新規に読み直す）。
        match timeout(remaining_timeout, stream.readable(false)).await {
            Ok(Ok(())) => {}
            _ => break, // タイムアウト or エラー（消費済みバイト無し）
        }

        let peek_buf = vec![0u8; 24 - accumulated.len()];
        let (result, returned_buf) = stream.read(peek_buf).await;

        match result {
            Ok(0) => break, // 接続終了
            Ok(n) => {
                accumulated.extend_from_slice(&returned_buf[..n]);
            }
            Err(_) => break, // エラー
        }
        
        // TLS 検出に必要な 5 バイトが溜まった時点でチェック（早期脱出）
        if accumulated.len() >= 5 {
            if accumulated[0] == 0x16 && accumulated[1] == 0x03 && (accumulated[2] >= 0x01 && accumulated[2] <= 0x03) {
                return (ProtocolType::TLS, accumulated);
            }
        }
        
        // HTTP/1.1 検出に必要な最低限のバイト数が溜まった時点でチェック（早期脱出）
        // GET / (5 bytes), POST (5 bytes) 等
        if accumulated.len() >= 5 {
            let methods = [b"GET ", b"POST", b"PUT ", b"DELE", b"HEAD", b"OPTI", b"CONN", b"TRAC", b"PATC"];
            for method in &methods {
                if accumulated.starts_with(*method) {
                    return (ProtocolType::Http11, accumulated);
                }
            }
        }
    }
    
    let n = accumulated.len();
    
    // HTTP/2プリフェース検出（24バイト固定）
    if n >= 24 {
        if &accumulated[..24] == b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n" {
            debug!("[Protocol Detection] H2C (HTTP/2 Cleartext) detected");
            return (ProtocolType::H2C, accumulated);
        }
    }
    
    // 溜まったデータで再度チェック（TLS/HTTP1.1 も念のため）
    if n >= 5 {
        if accumulated[0] == 0x16 && accumulated[1] == 0x03 && (accumulated[2] >= 0x01 && accumulated[2] <= 0x03) {
            debug!("[Protocol Detection] TLS detected");
            return (ProtocolType::TLS, accumulated);
        }
        
        let methods = [b"GET ", b"POST", b"PUT ", b"DELE", b"HEAD", b"OPTI", b"CONN", b"TRAC", b"PATC"];
        for method in &methods {
            if accumulated.starts_with(*method) {
                debug!("[Protocol Detection] HTTP/1.1 detected");
                return (ProtocolType::Http11, accumulated);
            }
        }
    }
    
    if n > 0 {
        debug!("[Protocol Detection] Unknown protocol ({} bytes): {:?}", n, String::from_utf8_lossy(&accumulated));
    }
    
    (ProtocolType::Unknown, accumulated)
}

/// H2Cサーバー接続処理
/// 
/// TLSなしでHTTP/2コネクションを確立し、リクエストを処理します。
#[cfg(feature = "http2")]
pub async fn handle_h2c_connection(
    stream: TcpStream,
    client_ip: &str,
    initial_data: Vec<u8>,
) {
    use http2::Http2Connection;
    
    // HTTP/2設定を取得
    let config = CURRENT_CONFIG.load();
    let settings = config.http2_config.to_http2_settings();
    
    // Http2ConnectionはTcpStreamでも動作可能
    // 既に読み込んだデータ（プリフェース）を初期バッファとして渡す
    // これにより、不要な再読み込みを回避できる
    let mut conn = Http2Connection::new_with_initial_buffer(stream, settings, initial_data);
    
    // ハンドシェイク（プリフェース確認 + SETTINGS 交換）
    // expect_preface()は初期バッファからプリフェースを読み取る
    if let Err(e) = conn.handshake().await {
        warn!("[H2C] Handshake error: {}", e);
        return;
    }
    
    debug!("[H2C] Connection established from {}", client_ip);
    
    // アクティブ接続メトリクスの自動管理（Dropで自動デクリメント）
    let mut connection_metric = ActiveConnectionMetric::new(true);
    
    // 既存のHTTP/2リクエスト処理を使用
    let result = handle_http2_requests(&mut conn, client_ip, &mut connection_metric).await;
    
    if let Err(e) = result {
        warn!("[H2C] Connection error: {}", e);
    }
    
    debug!("[H2C] Connection closed from {}", client_ip);
}

// kTLS 有効時の接続処理（rustls + ktls2）
#[cfg(feature = "ktls")]
pub async fn handle_connection(
    mut stream: TcpStream,
    acceptor: RustlsAcceptor,
    peer_addr: SocketAddr,
) {
    let mut initial_buffer = None;

    // H2Cが有効な場合、プロトコル検出を実行
    #[cfg(feature = "http2")]
    {
        let config = CURRENT_CONFIG.load();
        if config.h2c_enabled {
            let (protocol_type, initial_data) = detect_protocol_with_buffer(&mut stream).await;
            initial_buffer = Some(initial_data);

            
            match protocol_type {
                ProtocolType::H2C => {
                    // H2Cサーバーハンドラー
                    handle_h2c_connection(stream, &peer_addr.ip().to_string(), initial_buffer.take().unwrap()).await;
                    return;
                }
                ProtocolType::Http11 => {
                    // HTTP/1.1ハンドラー（平文接続）
                    // TLSハンドシェイクをスキップして、平文ストリームとして処理
                    let plain_stream = match acceptor.accept_plain(stream, initial_buffer.take()).await {
                        Ok(s) => s,
                        Err(e) => {
                            warn!("Failed to create plain stream: {}", e);
                            return;
                        }
                    };
                    let client_ip = peer_addr.ip().to_string();
                    handle_requests(plain_stream, &client_ip, peer_addr).await;
                    return;
                }
                ProtocolType::TLS => {
                    // TLSハンドシェイク（既存処理へ）
                }
                ProtocolType::Unknown => {
                    warn!("[H2C] Unknown protocol from {}, falling back to TLS", peer_addr);
                    // 初期データは保持したままTLSハンドシェイクへ
                }
            }
        }
    }
    
    // HTTP/2 が有効な場合のみ設定を読み込む
    #[cfg(feature = "http2")]
    let http2_enabled = {
        let config = CURRENT_CONFIG.load();
        config.http2_enabled
    };
    
    // TLSハンドシェイクにタイムアウトを設定
    // rustls でハンドシェイク後、ktls2 で kTLS を有効化
    let tls_result = timeout(CONNECT_TIMEOUT, acceptor.accept(stream, initial_buffer)).await;
    
    let tls_stream = match tls_result {
        Ok(Ok(tls)) => tls,
        Ok(Err(e)) => {
            warn!("TLS handshake error: {}", e);
            return;
        }
        Err(_) => {
            warn!("TLS handshake timeout");
            return;
        }
    };
    
    // クライアントIPアドレスを文字列に変換
    let client_ip = peer_addr.ip().to_string();

    // HTTP/2 が有効かつネゴシエートされた場合は HTTP/2 ハンドラーを使用
    #[cfg(feature = "http2")]
    if http2_enabled && tls_stream.is_http2() {
        handle_http2_connection(tls_stream, &client_ip).await;
        return;
    }

    // HTTP/1.1 ハンドラー
    handle_requests(tls_stream, &client_ip, peer_addr).await;
}

// kTLS 無効時の接続処理（rustls のみ）
#[cfg(not(feature = "ktls"))]
pub async fn handle_connection(
    mut stream: TcpStream,
    acceptor: simple_tls::SimpleTlsAcceptor,
    peer_addr: SocketAddr,
) {
    let mut initial_buffer = None;

    // H2Cが有効な場合、プロトコル検出を実行
    #[cfg(feature = "http2")]
    {
        let config = CURRENT_CONFIG.load();
        if config.h2c_enabled {
            let (protocol_type, initial_data) = detect_protocol_with_buffer(&mut stream).await;
            initial_buffer = Some(initial_data);

            
            match protocol_type {
                ProtocolType::H2C => {
                    // H2Cサーバーハンドラー
                    handle_h2c_connection(stream, &peer_addr.ip().to_string(), initial_buffer.take().unwrap()).await;
                    return;
                }
                ProtocolType::Http11 => {
                    // HTTP/1.1ハンドラー（平文接続）
                    // TLSハンドシェイクをスキップして、平文ストリームとして処理
                    let plain_stream = match acceptor.accept_plain(stream, initial_buffer.take()).await {
                        Ok(s) => s,
                        Err(e) => {
                            warn!("Failed to create plain stream: {}", e);
                            return;
                        }
                    };
                    let client_ip = peer_addr.ip().to_string();
                    handle_requests(plain_stream, &client_ip, peer_addr).await;
                    return;
                }
                ProtocolType::TLS => {
                    // TLSハンドシェイク（既存処理へ）
                }
                ProtocolType::Unknown => {
                    warn!("[H2C] Unknown protocol from {}, falling back to TLS", peer_addr);
                    // 初期データは保持したままTLSハンドシェイクへ
                }
            }
        }
    }
    
    // HTTP/2 が有効な場合のみ設定を読み込む
    #[cfg(feature = "http2")]
    let http2_enabled = {
        let config = CURRENT_CONFIG.load();
        config.http2_enabled
    };
    
    // TLSハンドシェイクにタイムアウトを設定
    let tls_result = timeout(CONNECT_TIMEOUT, acceptor.accept(stream, initial_buffer)).await;
    
    let tls_stream = match tls_result {
        Ok(Ok(tls)) => tls,
        Ok(Err(e)) => {
            warn!("TLS handshake error: {}", e);
            return;
        }
        Err(_) => {
            warn!("TLS handshake timeout");
            return;
        }
    };
    
    // クライアントIPアドレスを文字列に変換
    let client_ip = peer_addr.ip().to_string();

    // HTTP/2 が有効かつネゴシエートされた場合は HTTP/2 ハンドラーを使用
    #[cfg(feature = "http2")]
    if http2_enabled && tls_stream.is_http2() {
        handle_http2_connection(tls_stream, &client_ip).await;
        return;
    }

    // HTTP/1.1 ハンドラー
    handle_requests(tls_stream, &client_ip, peer_addr).await;
}

// ====================
// リクエスト処理ループ
// ====================

/// Lingering close: クライアントが送信途中のデータを一定時間読み捨ててからクローズする。
///
/// エラーレスポンス（431/414/413 等）を書き込んだ直後にクローズすると、ソケットの受信
/// バッファにクライアントが送信中のデータ（巨大ヘッダ/ボディ）が残っている場合、OS が
/// FIN ではなく RST を送出し、直前に書いたレスポンスがクライアントに届かないことがある
/// （負荷時に顕在化する「過大ヘッダで応答が空になる」フレーキーの原因）。受信データを
/// ドレインしてからクローズすることで RST を防ぎ、エラーレスポンスを確実に届ける。
/// nginx の lingering_close と同じ目的。
async fn lingering_drain_before_close(stream: &mut ServerTls) {
    // 全体上限はやや長めに取る。単一スレッドが高負荷でスケジュールされにくい状況でも、
    // クライアントが送信中の本文（最大 ~数百KB）を読み切ってからクローズできるようにする。
    // 上限途中でも「一定時間データが来ない＝受信バッファが空」になった時点で抜ける。
    let overall = Duration::from_millis(2000);
    let start = std::time::Instant::now();
    while start.elapsed() < overall {
        let buf = buf_get();
        match timeout(Duration::from_millis(200), stream.read(buf)).await {
            Ok((res, b)) => {
                buf_put(b);
                match res {
                    Ok(0) => break,   // EOF: クライアントが書き込みをクローズ
                    Ok(_) => {}       // 読み捨てて継続（データがある間は読み続ける）
                    Err(_) => break,  // 読み取りエラー
                }
            }
            Err(_) => break, // 200ms データ無し＝受信バッファは空。安全にクローズ可能
        }
    }
}

// 統一されたリクエスト処理ループ（型エイリアスを使用）
async fn handle_requests(
    mut tls_stream: ServerTls,
    client_ip: &str,
    peer_addr: SocketAddr,
) {
    let mut accumulated = Vec::with_capacity(BUF_SIZE);
    
    // アクティブ接続メトリクスの自動管理（Dropで自動デクリメント）
    let mut connection_metric = ActiveConnectionMetric::new(true);

    loop {
        // 読み込み（アイドルタイムアウト付き）
        let read_buf = buf_get();
        let read_result = timeout(IDLE_TIMEOUT, tls_stream.read(read_buf)).await;
        
        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => {
                // アイドルタイムアウト - 接続を閉じる
                return;
            }
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                return;
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                return;
            }
        };
        
        // 読み込んだデータを蓄積（SafeReadBufferの型安全なアクセス）
        returned_buf.set_valid_len(n);
        accumulated.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);


        // ヘッダーサイズ制限チェック（RFC 6585: 431 Request Header Fields Too Large）
        // ヘッダー終端 \r\n\r\n が見つかった場合はヘッダー部分のみをチェック。
        // まだヘッダーが完全でない場合は蓄積サイズで判定する。
        // URIが長すぎる場合は414 URI Too Long、それ以外のヘッダー超過は431
        let header_section_end = accumulated.windows(4).position(|w| w == b"\r\n\r\n");
        let header_check_size = header_section_end.map_or(accumulated.len(), |end| end + 4);
        if header_check_size > MAX_HEADER_SIZE {
            // リクエストラインの終端を探してURIサイズを確認
            let response_code = if let Some(line_end) = accumulated.windows(2).position(|w| w == b"\r\n") {
                // リクエストラインが完全に含まれている場合、URIサイズを確認
                let request_line = &accumulated[..line_end];
                // "GET /path HTTP/1.1" 形式から URI 部分を抽出
                let uri_too_long = request_line.iter()
                    .position(|&b| b == b' ')
                    .and_then(|method_end| {
                        let after_method = &request_line[method_end + 1..];
                        after_method.iter()
                            .rposition(|&b| b == b' ')
                            .map(|uri_end| uri_end)
                    })
                    .map_or(false, |uri_len| uri_len > MAX_HEADER_SIZE);

                if uri_too_long {
                    ERR_MSG_URI_TOO_LONG
                } else {
                    ERR_MSG_REQUEST_HEADER_TOO_LARGE
                }
            } else {
                // リクエストラインがまだ完全でない場合は431
                ERR_MSG_REQUEST_HEADER_TOO_LARGE
            };
            let err_buf = response_code.to_vec();
            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
            // クライアントは巨大ヘッダを送信中のため、ドレインしてからクローズして
            // RST による 431/414 レスポンスの消失を防ぐ（lingering close）。
            lingering_drain_before_close(&mut tls_stream).await;
            return;
        }


        // HTTPリクエストをパース
        let mut headers_storage = [httparse::EMPTY_HEADER; 64];
        let mut req = Request::new(&mut headers_storage);
        
        match req.parse(&accumulated) {
            Ok(Status::Complete(header_len)) => {
                // HTTPメソッド取得
                let method_bytes: Box<[u8]> = req.method
                    .map(|m| m.as_bytes().into())
                    .unwrap_or_else(|| Box::from(b"GET" as &[u8]));

                // 有効なHTTPメソッドのみ受け付ける（RFC 7231）
                const VALID_HTTP_METHODS: &[&[u8]] = &[
                    b"GET", b"HEAD", b"POST", b"PUT", b"DELETE",
                    b"CONNECT", b"OPTIONS", b"TRACE", b"PATCH",
                ];
                if !VALID_HTTP_METHODS.iter().any(|m| method_bytes.as_ref().eq_ignore_ascii_case(m)) {
                    drop(req);
                    let err_buf = ERR_MSG_METHOD_NOT_ALLOWED.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                    return;
                }

                // ヘッダー情報抽出
                let host_bytes: Box<[u8]> = req.headers.iter()
                    .find(|h| h.name.eq_ignore_ascii_case("host"))
                    .map(|h| Box::from(h.value))
                    .unwrap_or_else(|| Box::from([] as [u8; 0]));
                
                // メトリクス: 最初のリクエストでホスト名を取得し、インクリメント
                if let Ok(host_str) = std::str::from_utf8(&host_bytes) {
                    connection_metric.set_host(host_str.to_string());
                } else {
                    connection_metric.set_host("unknown".to_string());
                }
                
                let path_bytes: Box<[u8]> = req.path
                    .map(|p| p.as_bytes().into())
                    .unwrap_or_else(|| Box::from(b"/" as &[u8]));
                
                let user_agent: Box<[u8]> = req.headers.iter()
                    .find(|h| h.name.eq_ignore_ascii_case("user-agent"))
                    .map(|h| Box::from(h.value))
                    .unwrap_or_else(|| Box::from([] as [u8; 0]));
                
                // Content-Length ヘッダーの値を取得し、不正な値の場合は400 Bad Requestを返す
                // 複数の Content-Length ヘッダーは RFC 7230 Section 3.3.2 違反 → 400
                let cl_headers: Vec<_> = req.headers.iter()
                    .filter(|h| h.name.eq_ignore_ascii_case("content-length"))
                    .collect();

                if cl_headers.len() > 1 {
                    drop(req);
                    let err_buf = ERR_MSG_BAD_REQUEST.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                    return;
                }

                let content_length: usize = if let Some(cl_header) = cl_headers.first() {
                    match std::str::from_utf8(cl_header.value).ok().and_then(|s| s.trim().parse::<usize>().ok()) {
                        Some(len) => len,
                        None => {
                            // 不正な Content-Length 値 → 400 Bad Request
                            drop(req);
                            let err_buf = ERR_MSG_BAD_REQUEST.to_vec();
                            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                            return;
                        }
                    }
                } else {
                    0
                };

                // Transfer-Encoding: chunked チェック（改善版）
                let is_chunked: bool = req.headers.iter()
                    .find(|h| h.name.eq_ignore_ascii_case("transfer-encoding"))
                    .map(|h| is_chunked_encoding(h.value))
                    .unwrap_or(false);

                // Content-Length と Transfer-Encoding の競合は RFC 7230 Section 3.3.3 違反 → 400
                if content_length > 0 && is_chunked {
                    drop(req);
                    let err_buf = ERR_MSG_BAD_REQUEST.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                    return;
                }
                
                // Connection ヘッダーチェック（Keep-Alive / Upgrade対応）
                let connection_header: Option<&[u8]> = req.headers.iter()
                    .find(|h| h.name.eq_ignore_ascii_case("connection"))
                    .map(|h| h.value);
                
                let client_wants_close: bool = connection_header
                    .map(|v| v.eq_ignore_ascii_case(b"close"))
                    .unwrap_or(false);
                
                // WebSocket Upgrade チェック
                // Connection: upgrade と Upgrade: websocket の両方が必要
                let is_upgrade_connection: bool = connection_header
                    .map(|v| {
                        // "upgrade" または "keep-alive, upgrade" などのパターンに対応
                        v.to_ascii_lowercase()
                            .windows(7)
                            .any(|w| w == b"upgrade")
                    })
                    .unwrap_or(false);
                
                let is_websocket_upgrade: bool = req.headers.iter()
                    .find(|h| h.name.eq_ignore_ascii_case("upgrade"))
                    .map(|h| h.value.eq_ignore_ascii_case(b"websocket"))
                    .unwrap_or(false);
                
                let is_websocket: bool = is_upgrade_connection && is_websocket_upgrade;

                // ボディサイズ制限
                if !is_chunked && content_length > MAX_BODY_SIZE {
                    drop(req);
                    let err_buf = ERR_MSG_REQUEST_TOO_LARGE.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                    return;
                }

                let headers_for_proxy: Vec<(Box<[u8]>, Box<[u8]>)> = req.headers.iter()
                    .filter(|h| !h.name.is_empty())
                    .map(|h| (h.name.as_bytes().into(), h.value.into()))
                    .collect();
                
                // HTTP/1.1 Hostヘッダー必須チェック (RFC 7230 Section 5.4)
                // HTTP/1.1リクエストにはHostヘッダーが必須
                if validate_host_header(&headers_for_proxy, 1).is_err() {
                    drop(req);
                    let err_buf = ERR_MSG_BAD_REQUEST.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                    return;
                }
                
                let path_str = std::str::from_utf8(&path_bytes).unwrap_or("/");
                
                // Expect: 100-continue ヘッダーがある場合、ボディ送信前に 100 Continue を返す
                if content_length > 0 && check_expect_continue(&headers_for_proxy) {
                    // ボディサイズが制限内であることを確認済みなので 100 Continue を送信
                    let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(HTTP_100_CONTINUE.to_vec())).await;
                    if let Err(_) | Ok((Err(_), _)) = write_result {
                        return;
                    }
                }
                
                // メトリクスエンドポイントの処理（設定可能なパス）
                // Prometheusスクレイピング用の特別なパス
                {
                    let config = CURRENT_CONFIG.load();
                    let prom_config = &config.prometheus_config;
                    
                    // パスとメソッドをチェック
                    if prom_config.enabled 
                        && path_str == prom_config.path 
                        && method_bytes.as_ref() == b"GET" 
                    {
                        let start_instant = Instant::now();
                        
                        // IPアドレス制限チェック
                        if !prom_config.is_ip_allowed(client_ip) {
                            let err_buf = ERR_MSG_FORBIDDEN.to_vec();
                            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                            log_access(&method_bytes, &host_bytes, &path_bytes, &user_agent, 0, 403, 0, start_instant);
                            accumulated.clear();
                            return;
                        }
                        
                        let metrics_response = build_metrics_response();
                        let resp_size = metrics_response.len() as u64;
                        
                        let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(metrics_response)).await;
                        match write_result {
                            Ok((Ok(_), _)) => {
                                log_access(&method_bytes, &host_bytes, &path_bytes, &user_agent, 0, 200, resp_size, start_instant);
                            }
                            _ => {}
                        }
                        
                        // メトリクスエンドポイントは常に接続を閉じる
                        accumulated.clear();
                        return;
                    }
                }

                // Backend選択（統合ルーティング対応）
                // パス/クエリ分離（スキャンを1回に統一）
                let query_start_pos = path_bytes.iter().position(|&b| b == b'?');
                let path_without_query = query_start_pos
                    .map(|i| &path_bytes[..i])
                    .unwrap_or(&path_bytes);

                let config = CURRENT_CONFIG.load();
                // client_ipをSocketAddrに変換
                let client_socket_addr = if let Ok(addr) = client_ip.parse::<SocketAddr>() {
                    addr
                } else {
                    if let Ok(ip) = client_ip.parse::<std::net::IpAddr>() {
                        SocketAddr::new(ip, 80)
                    } else {
                        peer_addr
                    }
                };

                // ヘッダーをゼロコピーのバイト列スライスとして参照し、
                // クエリ文字列は生バイトのまま渡す（HashMap 割り当て不要）
                // req のドロップはルーティング完了後に行う
                let backend_result = {
                    let headers_raw: Vec<(&[u8], &[u8])> = req.headers.iter()
                        .filter(|h| !h.name.is_empty())
                        .map(|h| (h.name.as_bytes(), h.value))
                        .collect();
                    let raw_query: &[u8] = query_start_pos
                        .map(|i| &path_bytes[i + 1..])
                        .unwrap_or(b"");

                    find_backend_unified(
                        &host_bytes,
                        path_without_query,
                        &method_bytes,
                        &headers_raw,
                        raw_query,
                        &client_socket_addr,
                        config.route.as_slice(),
                        &config.upstream_groups,
                    )
                };
                // ルーティング完了後に req をドロップ（accumulated の borrow を解放）
                drop(req);

                let (prefix, backend) = match backend_result {
                    Some(b) => b,
                    None => {
                        let err_buf = ERR_MSG_NOT_FOUND.to_vec();
                        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                        return;
                    }
                };
                
                // セキュリティ設定を取得
                let security = backend.security();
                
                // IP制限チェック（deny → allow の順で評価）
                let ip_filter = security.ip_filter();
                if ip_filter.is_configured() && !ip_filter.is_allowed(client_ip) {
                    let err_buf = ERR_MSG_FORBIDDEN.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                    return;
                }
                
                // 許可メソッドチェック
                if !security.allowed_methods.is_empty() {
                    let method_str = std::str::from_utf8(&method_bytes).unwrap_or("GET");
                    let is_allowed = security.allowed_methods.iter()
                        .any(|m| m.eq_ignore_ascii_case(method_str));
                    if !is_allowed {
                        let err_buf = ERR_MSG_METHOD_NOT_ALLOWED.to_vec();
                        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                        return;
                    }
                }
                
                // ルートごとのボディサイズ制限（chunked以外）
                if !is_chunked && content_length > security.max_request_body_size {
                    let err_buf = ERR_MSG_REQUEST_TOO_LARGE.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                    return;
                }
                
                // レートリミットチェック
                if security.rate_limit_requests_per_min > 0 {
                    if !check_rate_limit(client_ip, security.rate_limit_requests_per_min) {
                        let err_buf = ERR_MSG_TOO_MANY_REQUESTS.to_vec();
                        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                        return;
                    }
                }

                // 初期ボディ（ヘッダー後のデータ）
                let initial_body: Vec<u8> = if header_len < accumulated.len() {
                    accumulated[header_len..].to_vec()
                } else {
                    Vec::new()
                };

                // WASMモジュールの適用
                // モジュールリストをローカル変数として保持（スレッドローカルを使わない、並行タスク間の干渉を防ぐ）
                #[cfg(feature = "wasm")]
                let modules_to_apply: Vec<String> = if let Some(backend_modules) = backend.modules() {
                    backend_modules.to_vec()
                } else {
                    Vec::new()
                };

                #[cfg(feature = "wasm")]
                let headers_for_proxy = {
                    let config = CURRENT_CONFIG.load();
                    if let Some(ref wasm_engine) = config.wasm_filter_engine {
                        // routes内で指定されたmodulesを優先
                        let path_str = std::str::from_utf8(&path_bytes).unwrap_or("/");
                        let method_str = std::str::from_utf8(&method_bytes).unwrap_or("GET");

                        // モジュールを実行
                        if !modules_to_apply.is_empty() {
                            let headers_vec: Vec<(Vec<u8>, Vec<u8>)> = headers_for_proxy.iter()
                                .map(|(k, v)| (k.to_vec(), v.to_vec()))
                                .collect();

                            let wasm_result = wasm_engine.clone().on_request_headers_with_modules_async(
                                modules_to_apply.clone(),
                                path_str.to_string(),
                                method_str.to_string(),
                                headers_vec,
                                client_ip.to_string(),
                                initial_body.is_empty() && !is_chunked, // end_of_stream
                            ).await;

                            match wasm_result {
                                crate::wasm::FilterResult::Continue { headers: modified_headers, .. } => {
                                    // 修正されたヘッダーを使用
                                    modified_headers.iter()
                                        .map(|(k, v)| (
                                            k.clone().into_boxed_slice(),
                                            v.clone().into_boxed_slice()
                                        ))
                                        .collect()
                                }
                                crate::wasm::FilterResult::LocalResponse(resp) => {
                                    // ローカルレスポンスを返送
                                    let status_line = format!("HTTP/1.1 {} {}\r\n", resp.status_code,
                                        match resp.status_code {
                                            200 => "OK",
                                            404 => "Not Found",
                                            403 => "Forbidden",
                                            500 => "Internal Server Error",
                                            _ => "Unknown",
                                        });
                                    let mut response = status_line.into_bytes();
                                    for (k, v) in &resp.headers {
                                        response.extend_from_slice(k);
                                        response.extend_from_slice(b": ");
                                        response.extend_from_slice(v);
                                        response.extend_from_slice(b"\r\n");
                                    }
                                    response.extend_from_slice(b"\r\n");
                                    response.extend_from_slice(&resp.body);

                                    let start_instant = Instant::now();
                                    let resp_size = response.len() as u64;
                                    let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(response)).await;
                                    match write_result {
                                        Ok((Ok(_), _)) => {
                                            log_access(&method_bytes, &host_bytes, &path_bytes, &user_agent, 0, resp.status_code, resp_size, start_instant);
                                            // WASMライフサイクルコールバック: リクエスト完了
                                            crate::wasm::on_request_complete_async(wasm_engine.clone(), modules_to_apply.clone()).await;
                                        }
                                        _ => {}
                                    }
                                    accumulated.clear();
                                    return;
                                }
                                crate::wasm::FilterResult::Pause => {
                                    // 非同期処理待ち（現在は未実装）
                                    warn!("WASM module requested pause, but async operations are not yet supported");
                                    headers_for_proxy
                                }
                            }
                        } else {
                            headers_for_proxy
                        }
                    } else {
                        headers_for_proxy
                    }
                };

                // 処理時間計測開始（Instant: モノトニック・高精度）
                let start_instant = Instant::now();

                // バッファクリア（次のリクエストに備える）
                accumulated.clear();

                // WebSocket Upgrade の場合は専用ハンドラーを使用
                if is_websocket {
                    // WebSocket はプロキシバックエンドでのみサポート
                    if let Backend::Proxy(ref upstream_group, ref security, _, _, _, _) = backend {
                        debug!("WebSocket upgrade request detected for path: {}", 
                              std::str::from_utf8(&path_bytes).unwrap_or("-"));
                        
                        // UpstreamGroup からサーバーを選択
                        let server = match upstream_group.select(client_ip) {
                            Some(s) => s,
                            None => {
                                error!("No healthy upstream servers for WebSocket");
                                let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                                let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                                return;
                            }
                        };
                        
                        server.acquire();
                        
                        // WebSocket プロキシ処理（双方向転送）
                        let ws_result = handle_websocket_proxy(
                            tls_stream,
                            &server.target,
                            security,
                            &method_bytes,
                            &path_bytes,
                            &prefix,
                            &headers_for_proxy,
                            &initial_body,
                        ).await;
                        
                        server.release();
                        
                        match ws_result {
                            Some((status, resp_size)) => {
                                log_access(&method_bytes, &host_bytes, &path_bytes, &user_agent, content_length as u64, status, resp_size, start_instant);
                                
                                // WASMライフサイクルコールバック: リクエスト完了
                                #[cfg(feature = "wasm")]
                                {
                                    if !modules_to_apply.is_empty() {
                                        let config = CURRENT_CONFIG.load();
                                        if let Some(ref wasm_engine) = config.wasm_filter_engine {
                                            crate::wasm::on_request_complete_async(wasm_engine.clone(), modules_to_apply.clone()).await;
                                        }
                                    }
                                }
                            }
                            None => {}
                        }
                        // WebSocket 接続終了後は HTTP 接続も終了
                        return;
                    } else {
                        // ファイルバックエンドでは WebSocket 非対応
                        let err_buf = ERR_MSG_BAD_REQUEST.to_vec();
                        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                        return;
                    }
                }

                // Backend処理
                let result = handle_backend(
                    tls_stream,
                    backend,
                    &method_bytes,
                    &path_bytes,
                    prefix,
                    content_length,
                    is_chunked,
                    &headers_for_proxy,
                    &initial_body,
                    client_wants_close,
                    {
                        #[cfg(feature = "wasm")]
                        { modules_to_apply.clone() }
                        #[cfg(not(feature = "wasm"))]
                        { Vec::new() }
                    },
                    client_ip,
                ).await;

                match result {
                    Some((stream_back, status, resp_size, should_close)) => {
                        log_access(&method_bytes, &host_bytes, &path_bytes, &user_agent, content_length as u64, status, resp_size, start_instant);

                        // WASMライフサイクルコールバック: リクエスト完了
                        #[cfg(feature = "wasm")]
                        {
                            if !modules_to_apply.is_empty() {
                                let config = CURRENT_CONFIG.load();
                                if let Some(ref wasm_engine) = config.wasm_filter_engine {
                                    crate::wasm::on_request_complete_async(wasm_engine.clone(), modules_to_apply.clone()).await;
                                }
                            }
                        }

                        tls_stream = stream_back;

                        // Connection: close が要求された場合、またはエラー時は接続を閉じる
                        if should_close {
                            return;
                        }
                        // Keep-Alive: ループを継続して次のリクエストを待機
                    }
                    None => {
                        return;
                    }
                }
            }
            Ok(Status::Partial) => {
                // データ不足、次の読み込みを待つ
                continue;
            }
            Err(_) => {
                let err_buf = ERR_MSG_BAD_REQUEST.to_vec();
                let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                return;
            }
        }
    }
}

// Backend処理
// ====================

// 統一された Backend 処理（型エイリアスを使用）
/// ローカル応答系バックエンド（File/Memory/Redirect 等）向けに、消費されない
/// リクエストボディをソケットから読み捨てる。
///
/// これらのバックエンドはリクエストボディを読まずにレスポンスを返す。keep-alive 接続で
/// ボディを読み捨てずに次のリクエストへ進むと、ソケットに残ったボディ（Content-Length
/// 本文や chunked の `5\r\nhello\r\n0\r\n\r\n` 等）が次のリクエストとして解釈され、
/// `400 Bad Request` を引き起こす（負荷時に顕在化する keep-alive desync の原因）。
///
/// `initial_body` は既にヘッダ読み取り時にソケットから消費済みの先頭バイト。残りを
/// ソケットから読み切る。完全にドレインできた場合のみ `true` を返す（接続再利用が安全）。
/// ドレインに失敗した場合は `false`（呼び出し側は接続を閉じる）。
async fn drain_request_body(
    stream: &mut ServerTls,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
) -> bool {
    if is_chunked {
        // DoS 対策の上限は MAX_BODY_SIZE を流用（リクエスト本文の上限）。
        let mut decoder = ChunkedDecoder::new(MAX_BODY_SIZE as u64);
        match decoder.feed(initial_body) {
            ChunkedFeedResult::Complete => return true,
            ChunkedFeedResult::SizeLimitExceeded => return false,
            ChunkedFeedResult::Continue => {}
        }
        loop {
            let buf = buf_get();
            match timeout(READ_TIMEOUT, stream.read(buf)).await {
                Ok((Ok(0), b)) => { buf_put(b); return false; } // EOF: 不完全
                Ok((Ok(n), mut b)) => {
                    b.set_valid_len(n);
                    let res = decoder.feed(b.as_valid_slice());
                    buf_put(b);
                    match res {
                        ChunkedFeedResult::Complete => return true,
                        ChunkedFeedResult::SizeLimitExceeded => return false,
                        ChunkedFeedResult::Continue => {}
                    }
                }
                _ => return false, // 読み取りエラー or タイムアウト
            }
        }
    } else {
        let mut remaining = content_length.saturating_sub(initial_body.len());
        while remaining > 0 {
            let buf = buf_get();
            match timeout(READ_TIMEOUT, stream.read(buf)).await {
                Ok((Ok(0), b)) => { buf_put(b); return false; } // EOF: 不完全
                Ok((Ok(n), b)) => {
                    buf_put(b);
                    remaining = remaining.saturating_sub(n);
                }
                _ => return false, // 読み取りエラー or タイムアウト
            }
        }
        true
    }
}

async fn handle_backend(
    mut tls_stream: ServerTls,
    backend: Backend,
    method: &[u8],
    req_path: &[u8],
    prefix: Box<[u8]>,
    content_length: usize,
    is_chunked: bool,
    headers: &[(Box<[u8]>, Box<[u8]>)],
    initial_body: &[u8],
    client_wants_close: bool,
    wasm_modules: Vec<String>,
    client_ip: &str,
) -> Option<(ServerTls, u16, u64, bool)> {
    // Proxy バックエンドはリクエストボディを上流へ転送して消費する。それ以外（File/Memory/
    // Redirect 等のローカル応答）はボディを読まないため、keep-alive 接続でボディが次の
    // リクエストに混入して 400 desync を起こす。ローカル応答の前に残りのボディを読み捨てる。
    if !matches!(backend, Backend::Proxy(..)) && (is_chunked || content_length > initial_body.len()) {
        if !drain_request_body(&mut tls_stream, content_length, is_chunked, initial_body).await {
            // ドレイン失敗（接続が汚染されている可能性）→ 接続を閉じる
            return Some((tls_stream, 400, 0, true));
        }
    }
    match backend {
        Backend::Proxy(upstream_group, security, compression, buffering, cache, _) => {
            handle_proxy(
                tls_stream,
                &upstream_group,
                &security,
                &compression,
                &buffering,
                &cache,
                method,
                req_path,
                &prefix,
                content_length,
                is_chunked,
                headers,
                initial_body,
                client_wants_close,
                wasm_modules,
                client_ip
            ).await
        }
        Backend::MemoryFile(data, mime_type, security, _) => {
            // ファイル完全一致チェック
            // MemoryFileはファイル指定なので、プレフィックス以降にパスがあれば404
            let path_str = std::str::from_utf8(req_path).unwrap_or("/");
            let prefix_str = std::str::from_utf8(&prefix).unwrap_or("");
            
            let remainder = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
                &path_str[prefix_str.len()..]
            } else {
                ""
            };
            
            let clean_remainder = remainder.trim_matches('/');
            if !clean_remainder.is_empty() {
                // ファイル指定なのにさらにパスが続いている場合は404
                let err_buf = ERR_MSG_NOT_FOUND.to_vec();
                let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                return Some((tls_stream, 404, 0, true));
            }
            
            // Keep-Alive対応: クライアントの要求に応じてConnectionヘッダーを動的に生成
            let mut header = Vec::with_capacity(HEADER_BUF_CAPACITY);
            header.extend_from_slice(HTTP_200_PREFIX);
            header.extend_from_slice(mime_type.as_bytes());
            header.extend_from_slice(CONTENT_LENGTH_HEADER);
            let mut num_buf = itoa::Buffer::new();
            header.extend_from_slice(num_buf.format(data.len()).as_bytes());
            header.extend_from_slice(b"\r\n");
            
            // 追加レスポンスヘッダー（セキュリティヘッダーなど）
            for (header_name, header_value) in &security.add_response_headers {
                header.extend_from_slice(header_name.as_bytes());
                header.extend_from_slice(b": ");
                header.extend_from_slice(header_value.as_bytes());
                header.extend_from_slice(b"\r\n");
            }
            
            // WASMレスポンスヘッダーフィルタを適用
            #[cfg(feature = "wasm")]
            let header = {
                ftlog::debug!("[WASM Response] MemoryFile: wasm_modules count = {}", wasm_modules.len());
                if !wasm_modules.is_empty() {
                    let config = CURRENT_CONFIG.load();
                    if let Some(ref wasm_engine) = config.wasm_filter_engine {
                        // 現在のヘッダーをVec<(Vec<u8>, Vec<u8>)>形式に変換
                        let header_str = String::from_utf8_lossy(&header);
                        let current_headers: Vec<(Vec<u8>, Vec<u8>)> = header_str.lines()
                            .skip(1) // ステータス行をスキップ
                            .filter_map(|line| {
                                let line_trimmed = line.trim_end_matches("\r\n").trim_end_matches("\r");
                                if line_trimmed.is_empty() {
                                    return None;
                                }
                                let colon_pos = line_trimmed.find(':')?;
                                let name = line_trimmed[..colon_pos].as_bytes().to_vec();
                                let value = line_trimmed[colon_pos+1..].trim_start().as_bytes().to_vec();
                                Some((name, value))
                            })
                            .collect();

                        // WASMフィルタを実行（レスポンスヘッダー処理）
                        let wasm_result = wasm_engine.clone().on_response_headers_with_modules_async(
                            wasm_modules.clone(),
                            200,
                            current_headers,
                            true, // end_of_stream
                        ).await;

                        match wasm_result {
                            crate::wasm::FilterResult::Continue { headers: modified_headers, .. } => {
                                // WASMから修正されたヘッダーで再構築
                                let mut new_header = Vec::with_capacity(HEADER_BUF_CAPACITY);
                                new_header.extend_from_slice(HTTP_200_PREFIX);
                                new_header.extend_from_slice(mime_type.as_bytes());
                                new_header.extend_from_slice(CONTENT_LENGTH_HEADER);
                                let mut num_buf = itoa::Buffer::new();
                                new_header.extend_from_slice(num_buf.format(data.len()).as_bytes());
                                new_header.extend_from_slice(b"\r\n");

                                // WASMから返されたヘッダーを追加
                                for (name, value) in modified_headers {
                                    new_header.extend_from_slice(&name);
                                    new_header.extend_from_slice(b": ");
                                    new_header.extend_from_slice(&value);
                                    new_header.extend_from_slice(b"\r\n");
                                }
                                new_header
                            }
                            _ => header,
                        }
                    } else {
                        header
                    }
                } else {
                    header
                }
            };

            // Connection header を追加（headerをmutableにする）
            let mut header = header;
            if client_wants_close {
                header.extend_from_slice(b"Connection: close\r\n\r\n");
            } else {
                header.extend_from_slice(b"Connection: keep-alive\r\n\r\n");
            }
            
            // ヘッダー送信（タイムアウト付き）
            let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(header)).await;
            match write_result {
                Ok((Ok(_), _)) => {}
                _ => return None,
            }
            
            // ボディ送信（タイムアウト付き）
            let data_len = data.len() as u64;
            let data_buf = data.to_vec();
            let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(data_buf)).await;
            match write_result {
                Ok((Ok(_), _)) => {
                    Some((tls_stream, 200, data_len, client_wants_close))
                }
                _ => None,
            }
        }
        Backend::SendFile(base_path, is_dir, index_file, security, _cache, open_file_cache_config, _) => {
            // Range ヘッダーを抽出 (RFC 7233)
            let range_header = headers.iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(b"range"))
                .map(|(_, v)| v.as_ref());
            handle_sendfile(tls_stream, &base_path, is_dir, index_file.as_deref(), req_path, &prefix, client_wants_close, &security, range_header, open_file_cache_config.as_deref(), wasm_modules).await
        }
        Backend::Redirect(redirect_url, status_code, preserve_path, _) => {
            handle_redirect(tls_stream, &redirect_url, status_code, preserve_path, req_path, &prefix, client_wants_close).await
        }
    }
}

// ====================
// リダイレクト処理
// ====================
//
// 設定されたURLへのHTTPリダイレクトを返します。
// ステータスコード: 301, 302, 303, 307, 308 をサポート
//
// 特殊変数:
// - $request_uri: 元のリクエストURI
// - $host: リクエストのHostヘッダー
// - $path: 元のパス（prefix除去後）
// ====================

/// リダイレクトレスポンスを生成して送信
async fn handle_redirect(
    mut tls_stream: ServerTls,
    redirect_url: &str,
    status_code: u16,
    preserve_path: bool,
    req_path: &[u8],
    prefix: &[u8],
    client_wants_close: bool,
) -> Option<(ServerTls, u16, u64, bool)> {
    // リダイレクト先URLを構築
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
    
    // preserve_path が true の場合、元のパスを追加
    if preserve_path && !sub_path.is_empty() {
        // URLにすでにパスがある場合は結合
        if final_url.ends_with('/') && sub_path.starts_with('/') {
            final_url.push_str(&sub_path[1..]);
        } else if !final_url.ends_with('/') && !sub_path.starts_with('/') {
            final_url.push('/');
            final_url.push_str(sub_path);
        } else {
            final_url.push_str(sub_path);
        }
    }
    
    // ステータス行を構築
    let status_line = match status_code {
        301 => "HTTP/1.1 301 Moved Permanently\r\n",
        302 => "HTTP/1.1 302 Found\r\n",
        303 => "HTTP/1.1 303 See Other\r\n",
        307 => "HTTP/1.1 307 Temporary Redirect\r\n",
        308 => "HTTP/1.1 308 Permanent Redirect\r\n",
        _ => "HTTP/1.1 301 Moved Permanently\r\n",
    };
    
    // レスポンス構築
    let mut response = Vec::with_capacity(256 + final_url.len());
    response.extend_from_slice(status_line.as_bytes());
    response.extend_from_slice(b"Location: ");
    response.extend_from_slice(final_url.as_bytes());
    response.extend_from_slice(b"\r\nContent-Length: 0\r\n");
    
    if client_wants_close {
        response.extend_from_slice(b"Connection: close\r\n\r\n");
    } else {
        response.extend_from_slice(b"Connection: keep-alive\r\n\r\n");
    }
    
    // レスポンス送信
    let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(response)).await;
    match write_result {
        Ok((Ok(_), _)) => {
            Some((tls_stream, status_code, 0, client_wants_close))
        }
        _ => None,
    }
}


// ====================
// WebSocket プロキシ処理
// ====================
//
// WebSocket アップグレードリクエストを検出し、双方向転送を行います。
//
// フロー:
// 1. クライアントから Upgrade: websocket リクエストを受信
// 2. バックエンドに接続し、アップグレードリクエストを転送
// 3. バックエンドから 101 Switching Protocols を受信
// 4. クライアントに 101 を転送
// 5. 以降は双方向でバイトストリームを透過的に転送
// 6. どちらかが接続を閉じるまで継続
// ====================

/// WebSocket プロキシ処理（双方向転送）
/// 
/// HTTP Upgrade をバックエンドに転送し、成功後は双方向のバイト転送を行う。
/// WebSocket 接続が終了するまでブロックし、終了後はクライアント接続も閉じる。
/// 
/// # Returns
/// Some((status_code, bytes_transferred)) - 成功時
/// None - エラー時
async fn handle_websocket_proxy(
    client_stream: ServerTls,
    target: &ProxyTarget,
    security: &SecurityConfig,
    method: &[u8],
    req_path: &[u8],
    prefix: &[u8],
    headers: &[(Box<[u8]>, Box<[u8]>)],
    initial_body: &[u8],
) -> Option<(u16, u64)> {
    let connect_timeout = Duration::from_secs(security.backend_connect_timeout_secs);
    
    // リクエストパス構築
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    let sub_path = if prefix.is_empty() {
        path_str.to_string()
    } else {
        let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
        if path_str.starts_with(prefix_str) {
            let remaining = &path_str[prefix_str.len()..];
            let base = target.path_prefix.trim_end_matches('/');
            build_sub_path(base, remaining)
        } else {
            path_str.to_string()
        }
    };

    let final_path = if sub_path.is_empty() { "/" } else { &sub_path };

    // WebSocket アップグレードリクエスト構築（プール使用）
    // Connection: Upgrade と Upgrade: websocket を維持
    let mut request = request_buf_get(1024);
    request.extend_from_slice(method);
    request.extend_from_slice(HEADER_SPACE);
    request.extend_from_slice(final_path.as_bytes());
    request.extend_from_slice(HEADER_HTTP11_HOST);
    request.extend_from_slice(target.host.as_bytes());
    
    if !target.is_default_port() {
        request.extend_from_slice(HEADER_PORT_COLON);
        let mut port_buf = itoa::Buffer::new();
        request.extend_from_slice(port_buf.format(target.port).as_bytes());
    }
    
    request.extend_from_slice(HEADER_CRLF);
    
    // すべてのヘッダーを転送（Host 以外）
    for (name, value) in headers {
        if name.eq_ignore_ascii_case(b"host") {
            continue;
        }
        
        if !is_valid_header_name(name) || !is_valid_header_value(value) {
            continue;
        }
        
        request.extend_from_slice(name);
        request.extend_from_slice(HEADER_COLON);
        request.extend_from_slice(value);
        request.extend_from_slice(HEADER_CRLF);
    }
    request.extend_from_slice(HEADER_CRLF);
    
    // 初期ボディがあれば追加
    if !initial_body.is_empty() {
        request.extend_from_slice(initial_body);
    }
    
    // WebSocketポーリング設定を取得
    let poll_config = security.websocket_poll_config();
    
    // バックエンドに接続
    if target.use_tls {
        // HTTPS バックエンドへの WebSocket
        handle_websocket_proxy_https(client_stream, target, connect_timeout, request, &poll_config).await
    } else {
        // HTTP バックエンドへの WebSocket
        handle_websocket_proxy_http(client_stream, target, connect_timeout, request, &poll_config).await
    }
}

/// HTTP バックエンドへの WebSocket プロキシ
async fn handle_websocket_proxy_http(
    mut client_stream: ServerTls,
    target: &ProxyTarget,
    connect_timeout: Duration,
    request: Vec<u8>,
    poll_config: &WebSocketPollConfig,
) -> Option<(u16, u64)> {
    // バックエンドに接続
    let addr = format!("{}:{}", target.host, target.port);
    let connect_result = timeout(connect_timeout, TcpStream::connect(&addr)).await;
    
    let mut backend_stream = match connect_result {
        Ok(Ok(stream)) => {
            let _ = stream.set_nodelay(true);
            stream
        }
        Ok(Err(e)) => {
            error!("WebSocket proxy connect error: {}", e);
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((502, 0));
        }
        Err(_) => {
            let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((504, 0));
        }
    };
    
    // アップグレードリクエストを送信
    let (write_res, returned_request) = backend_stream.write_all(request).await;
    request_buf_put(returned_request);
    if write_res.is_err() {
        let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
        let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
        return Some((502, 0));
    }
    
    // バックエンドからのレスポンスを読み取り
    let mut response_buf = Vec::with_capacity(4096);
    let status_code;
    
    loop {
        let buf = buf_get();
        let (res, mut returned_buf) = backend_stream.read(buf).await;
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                return Some((502, 0));
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                return Some((502, 0));
            }
        };
        
        returned_buf.set_valid_len(n);
        response_buf.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);
        
        // レスポンスヘッダーを解析
        if let Some(parsed) = parse_http_response(&response_buf) {
            status_code = parsed.status_code;
            
            // クライアントにレスポンスを転送
            let resp_data = response_buf.clone();
            let (write_res, _) = client_stream.write_all(resp_data).await;
            if write_res.is_err() {
                return None;
            }
            
            // 101 Switching Protocols の場合は双方向転送開始
            if status_code == 101 {
                debug!("WebSocket upgrade successful, starting bidirectional transfer");
                let total = websocket_bidirectional_transfer(&mut client_stream, &mut backend_stream, poll_config).await;
                return Some((101, total));
            } else {
                // アップグレード失敗（通常の HTTP レスポンス）
                return Some((status_code, response_buf.len() as u64));
            }
        }
        
        // ヘッダーが大きすぎる
        if response_buf.len() > MAX_HEADER_SIZE {
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((502, 0));
        }
    }
}

/// HTTPS バックエンドへの WebSocket プロキシ
async fn handle_websocket_proxy_https(
    mut client_stream: ServerTls,
    target: &ProxyTarget,
    connect_timeout: Duration,
    request: Vec<u8>,
    poll_config: &WebSocketPollConfig,
) -> Option<(u16, u64)> {
    // バックエンドに TCP 接続
    let addr = format!("{}:{}", target.host, target.port);
    let connect_result = timeout(connect_timeout, TcpStream::connect(&addr)).await;
    
    let backend_tcp = match connect_result {
        Ok(Ok(stream)) => {
            let _ = stream.set_nodelay(true);
            stream
        }
        Ok(Err(e)) => {
            error!("WebSocket proxy connect error: {}", e);
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((502, 0));
        }
        Err(_) => {
            let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((504, 0));
        }
    };
    
    // TLS 接続
    let connector = get_tls_connector();
    let tls_result = timeout(connect_timeout, connector.connect(backend_tcp, &target.host)).await;
    
    let mut backend_stream = match tls_result {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            error!("WebSocket TLS connect error: {}", e);
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((502, 0));
        }
        Err(_) => {
            let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((504, 0));
        }
    };
    
    // アップグレードリクエストを送信
    let (write_res, returned_request) = backend_stream.write_all(request).await;
    request_buf_put(returned_request);
    if write_res.is_err() {
        let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
        let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
        return Some((502, 0));
    }
    
    // バックエンドからのレスポンスを読み取り
    let mut response_buf = Vec::with_capacity(4096);
    let status_code;
    
    loop {
        let buf = buf_get();
        let (res, mut returned_buf) = backend_stream.read(buf).await;
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                return Some((502, 0));
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                return Some((502, 0));
            }
        };
        
        returned_buf.set_valid_len(n);
        response_buf.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);
        
        // レスポンスヘッダーを解析
        if let Some(parsed) = parse_http_response(&response_buf) {
            status_code = parsed.status_code;
            
            // クライアントにレスポンスを転送
            let resp_data = response_buf.clone();
            let (write_res, _) = client_stream.write_all(resp_data).await;
            if write_res.is_err() {
                return None;
            }
            
            // 101 Switching Protocols の場合は双方向転送開始
            if status_code == 101 {
                debug!("WebSocket upgrade successful (TLS), starting bidirectional transfer");
                let total = websocket_bidirectional_transfer_tls(&mut client_stream, &mut backend_stream, poll_config).await;
                return Some((101, total));
            } else {
                // アップグレード失敗
                return Some((status_code, response_buf.len() as u64));
            }
        }
        
        // ヘッダーが大きすぎる
        if response_buf.len() > MAX_HEADER_SIZE {
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((502, 0));
        }
    }
}

/// WebSocket 双方向転送（HTTP バックエンド）
/// 
/// クライアント ⇔ バックエンド間でデータを双方向に転送。
/// monoio の select! 相当を手動で実装し、どちらの方向も待機。
/// 
/// ## ポーリングモード
/// 
/// ### Fixed モード
/// 設定されたタイムアウト値を固定で使用。
/// 低レイテンシが最優先の場合（リアルタイムゲームなど）に推奨。
/// 
/// ### Adaptive モード（デフォルト）
/// データ転送があればタイムアウトをリセット（初期値に戻す）。
/// アイドル時はバックオフ方式でタイムアウトを延長（最大値まで）。
/// CPU効率とレイテンシのバランスを取る場合に推奨。
/// 
/// ## 将来的な改善
/// 
/// monoio が epoll/io_uring ベースのselect風APIをサポートした場合、
/// イベント駆動型の実装に移行することで、さらなる効率化が可能。
async fn websocket_bidirectional_transfer(
    client: &mut ServerTls,
    backend: &mut TcpStream,
    poll_config: &WebSocketPollConfig,
) -> u64 {
    let mut total = 0u64;
    
    // 現在のタイムアウト値（Adaptive モードで動的に変更）
    let mut current_timeout_ms = poll_config.initial_timeout_ms;
    
    loop {
        let poll_timeout = Duration::from_millis(current_timeout_ms);
        let mut had_activity = false;
        
        // クライアント → バックエンド
        let client_buf = buf_get();
        let read_result = timeout(poll_timeout, client.read(client_buf)).await;
        
        match read_result {
            Ok((Ok(0), buf)) => {
                buf_put(buf);
                break; // クライアントが接続を閉じた
            }
            Ok((Ok(n), mut buf)) => {
                buf.set_valid_len(n);
                let write_buf = buf.into_truncated();
                let (write_res, returned) = backend.write_all(write_buf).await;
                buf_put_vec(returned);
                if write_res.is_err() {
                    break;
                }
                total += n as u64;
                had_activity = true;
            }
            Ok((Err(_), buf)) => {
                buf_put(buf);
                break;
            }
            Err(_) => {
                // タイムアウト - 反対方向をチェック
            }
        }
        
        // バックエンド → クライアント
        let backend_buf = buf_get();
        let read_result = timeout(poll_timeout, backend.read(backend_buf)).await;
        
        match read_result {
            Ok((Ok(0), buf)) => {
                buf_put(buf);
                break; // バックエンドが接続を閉じた
            }
            Ok((Ok(n), mut buf)) => {
                buf.set_valid_len(n);
                let write_buf = buf.into_truncated();
                let (write_res, returned) = client.write_all(write_buf).await;
                buf_put_vec(returned);
                if write_res.is_err() {
                    break;
                }
                total += n as u64;
                had_activity = true;
            }
            Ok((Err(_), buf)) => {
                buf_put(buf);
                break;
            }
            Err(_) => {
                // タイムアウト - ループ継続
            }
        }
        
        // Adaptive モードでのタイムアウト調整
        if poll_config.mode == WebSocketPollMode::Adaptive {
            if had_activity {
                // データ転送があった場合: タイムアウトをリセット（初期値に戻す）
                current_timeout_ms = poll_config.initial_timeout_ms;
            } else {
                // タイムアウトした場合: バックオフ（最大値まで延長）
                let new_timeout = (current_timeout_ms as f64 * poll_config.backoff_multiplier) as u64;
                current_timeout_ms = new_timeout.min(poll_config.max_timeout_ms);
            }
        }
        // Fixed モードでは current_timeout_ms は変更されない
    }
    
    total
}

/// WebSocket 双方向転送（HTTPS バックエンド）
/// 
/// HTTP版と同様のポーリングモード（Fixed/Adaptive）をサポート。
/// 詳細は `websocket_bidirectional_transfer` のドキュメントを参照。
async fn websocket_bidirectional_transfer_tls(
    client: &mut ServerTls,
    backend: &mut ClientTls,
    poll_config: &WebSocketPollConfig,
) -> u64 {
    let mut total = 0u64;
    
    // 現在のタイムアウト値（Adaptive モードで動的に変更）
    let mut current_timeout_ms = poll_config.initial_timeout_ms;
    
    loop {
        let poll_timeout = Duration::from_millis(current_timeout_ms);
        let mut had_activity = false;
        
        // クライアント → バックエンド
        let client_buf = buf_get();
        let read_result = timeout(poll_timeout, client.read(client_buf)).await;
        
        match read_result {
            Ok((Ok(0), buf)) => {
                buf_put(buf);
                break;
            }
            Ok((Ok(n), mut buf)) => {
                buf.set_valid_len(n);
                let write_buf = buf.into_truncated();
                let (write_res, returned) = backend.write_all(write_buf).await;
                buf_put_vec(returned);
                if write_res.is_err() {
                    break;
                }
                total += n as u64;
                had_activity = true;
            }
            Ok((Err(_), buf)) => {
                buf_put(buf);
                break;
            }
            Err(_) => {}
        }
        
        // バックエンド → クライアント
        let backend_buf = buf_get();
        let read_result = timeout(poll_timeout, backend.read(backend_buf)).await;
        
        match read_result {
            Ok((Ok(0), buf)) => {
                buf_put(buf);
                break;
            }
            Ok((Ok(n), mut buf)) => {
                buf.set_valid_len(n);
                let write_buf = buf.into_truncated();
                let (write_res, returned) = client.write_all(write_buf).await;
                buf_put_vec(returned);
                if write_res.is_err() {
                    break;
                }
                total += n as u64;
                had_activity = true;
            }
            Ok((Err(_), buf)) => {
                buf_put(buf);
                break;
            }
            Err(_) => {}
        }
        
        // Adaptive モードでのタイムアウト調整
        if poll_config.mode == WebSocketPollMode::Adaptive {
            if had_activity {
                // データ転送があった場合: タイムアウトをリセット（初期値に戻す）
                current_timeout_ms = poll_config.initial_timeout_ms;
            } else {
                // タイムアウトした場合: バックオフ（最大値まで延長）
                let new_timeout = (current_timeout_ms as f64 * poll_config.backoff_multiplier) as u64;
                current_timeout_ms = new_timeout.min(poll_config.max_timeout_ms);
            }
        }
        // Fixed モードでは current_timeout_ms は変更されない
    }
    
    total
}

// ====================
// プロキシ処理
// ====================
//
// バックエンドコネクションプールを使用して、接続を再利用します。
// Connection: keep-alive をバックエンドに送信し、レスポンスの
// Connection ヘッダーに基づいて接続をプールに返却します。
// ====================

async fn handle_proxy(
    mut client_stream: ServerTls,
    upstream_group: &UpstreamGroup,
    security: &SecurityConfig,
    compression: &CompressionConfig,
    buffering_config: &buffering::BufferingConfig,
    cache_config: &cache::CacheConfig,
    method: &[u8],
    req_path: &[u8],
    prefix: &[u8],
    content_length: usize,
    is_chunked: bool,
    headers: &[(Box<[u8]>, Box<[u8]>)],
    initial_body: &[u8],
    client_wants_close: bool,
    wasm_modules: Vec<String>,
    client_ip: &str,
) -> Option<(ServerTls, u16, u64, bool)> {
    // クライアントの Accept-Encoding を解析
    let client_encoding = headers.iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(b"accept-encoding"))
        .map(|(_, value)| AcceptedEncoding::parse(value))
        .unwrap_or(AcceptedEncoding::Identity);
    
    // ホスト名を取得
    let host_str = headers.iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(b"host"))
        .and_then(|(_, v)| std::str::from_utf8(v).ok())
        .unwrap_or("unknown");
    
    // RFC 7230 Section 4.3: TE ヘッダーを解析
    // クライアントがtrailersをサポートしているかを判定
    let _client_supports_trailers = headers.iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(b"te"))
        .map(|(_, value)| parse_te_header(value).supports_trailers)
        .unwrap_or(false);
    
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    
    // ===================
    // キャッシュヒット判定
    // ===================
    if cache_config.enabled {
        // キャッシュ対象かチェック
        if cache_config.is_cacheable_method(method) && !cache_config.should_bypass(path_str) {
            // キャッシュキー生成（key_headers を使用）
            let query = path_str.find('?').map(|i| &path_str[i+1..]);
            let path_only = path_str.find('?').map(|i| &path_str[..i]).unwrap_or(path_str);
            
            // key_headers からVaryキー用のヘッダー値を抽出
            let vary_key_headers = if !cache_config.key_headers.is_empty() {
                let extracted = extract_vary_headers_for_cache_key(headers, &cache_config.key_headers);
                if extracted.is_empty() {
                    None
                } else {
                    Some(extracted)
                }
            } else {
                None
            };
            
            if let Some(cache_key) = cache::CacheKey::from_request(
                method,
                host_str,
                path_only,
                query,
                cache_config.include_query,
                vary_key_headers.as_deref(), // key_headers に基づくVaryキー
            ) {
                // グローバルキャッシュからエントリを取得
                if let Some(cache_manager) = cache::get_global_cache() {
                    // 有効なエントリを取得
                    let (cached_entry, is_stale) = if let Some(entry) = cache_manager.get(&cache_key) {
                        (Some(entry), false)
                    } else if cache_config.stale_while_revalidate {
                        // 期限切れでもstale-while-revalidate期間内なら使用
                        // デフォルトで60秒のstale期間を許容
                        if let Some(entry) = cache_manager.get_stale(&cache_key, 60) {
                            debug!("Using stale cache entry for {} {}", host_str, path_str);
                            (Some(entry), true)
                        } else {
                            (None, false)
                        }
                    } else {
                        (None, false)
                    };
                    
                    if let Some(cached_entry) = cached_entry {
                        // キャッシュヒット！
                        debug!("Cache {} for {} {}", if is_stale { "STALE" } else { "HIT" }, host_str, path_str);
                        record_cache_hit(host_str);
                        
                        // ETag/If-None-Match 検証（304レスポンス）
                        if cache_config.enable_etag {
                            if let Some(client_etag) = cache::CachePolicy::get_if_none_match(headers) {
                                if let Some(ref cached_etag) = cached_entry.etag {
                                    // ETagが一致すれば304 Not Modifiedを返す
                                    let client_etag_str = std::str::from_utf8(client_etag).unwrap_or("");
                                    if etag_matches(client_etag_str, cached_etag) {
                                        debug!("ETag match, returning 304 Not Modified");
                                        let response = build_304_response(&cached_entry, client_wants_close, is_stale);
                                        match timeout(WRITE_TIMEOUT, client_stream.write_all(response)).await {
                                            Ok((Ok(_), _)) => {
                                                return Some((client_stream, 304, 0, client_wants_close));
                                            }
                                            _ => {
                                                return None;
                                            }
                                        }
                                    }
                                }
                            }
                            
                            // If-Modified-Since 検証（304レスポンス）
                            if let Some(client_ims) = cache::CachePolicy::get_if_modified_since(headers) {
                                if let Some(ref cached_lm) = cached_entry.last_modified {
                                    let client_ims_str = std::str::from_utf8(client_ims).unwrap_or("");
                                    if last_modified_matches(client_ims_str, cached_lm) {
                                        debug!("If-Modified-Since match, returning 304 Not Modified");
                                        let response = build_304_response(&cached_entry, client_wants_close, is_stale);
                                        match timeout(WRITE_TIMEOUT, client_stream.write_all(response)).await {
                                            Ok((Ok(_), _)) => {
                                                return Some((client_stream, 304, 0, client_wants_close));
                                            }
                                            _ => {
                                                return None;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        
                        // stale-while-revalidate: バックグラウンド更新タスクをスポーン
                        if is_stale {
                            spawn_background_revalidation(
                                cache_key.clone(),
                                upstream_group.clone(),
                                security.clone(),
                                method.to_vec(),
                                req_path.to_vec(),
                                prefix.to_vec(),
                                headers.to_vec(),
                            );
                        }
                        
                        // キャッシュからレスポンスを返す
                        // メモリキャッシュの場合
                        if let Some(body_data) = cached_entry.memory_body() {
                            let response = build_cached_response(&cached_entry, body_data, client_wants_close, is_stale);
                            
                            match timeout(WRITE_TIMEOUT, client_stream.write_all(response)).await {
                                Ok((Ok(_), _)) => {
                                    return Some((client_stream, cached_entry.status_code, body_data.len() as u64, client_wants_close));
                                }
                                _ => {
                                    return None;
                                }
                            }
                        }
                        // ディスクキャッシュの場合
                        else if let Some(disk_path) = cached_entry.disk_path() {
                            debug!("Serving from disk cache: {:?}", disk_path);
                            match serve_from_disk_cache(&mut client_stream, &cached_entry, disk_path, client_wants_close, is_stale).await {
                                Some((status_code, body_size)) => {
                                    return Some((client_stream, status_code, body_size, client_wants_close));
                                }
                                None => {
                                    // ディスク読み込み失敗、キャッシュエントリを無効化してバックエンドに転送
                                    warn!("Failed to read disk cache: {:?}", disk_path);
                                    cache_manager.invalidate(&cache_key);
                                }
                            }
                        }
                    } else {
                        debug!("Cache MISS for {} {}", host_str, path_str);
                        record_cache_miss(host_str);
                    }
                }
            }
        }
    }
    
    // キャッシュ保存コンテキストを作成（キャッシュ有効かつキャッシュ可能な場合）
    let mut cache_save_ctx: Option<CacheSaveContext> = None;
    if cache_config.enabled && cache_config.is_cacheable_method(method) && !cache_config.should_bypass(path_str) {
        let query = path_str.find('?').map(|i| &path_str[i+1..]);
        let path_only = path_str.find('?').map(|i| &path_str[..i]).unwrap_or(path_str);
        
        // key_headers からVaryキー用のヘッダー値を抽出（保存時も同じキーを使用）
        let vary_key_headers = if !cache_config.key_headers.is_empty() {
            let extracted = extract_vary_headers_for_cache_key(headers, &cache_config.key_headers);
            if extracted.is_empty() {
                None
            } else {
                Some(extracted)
            }
        } else {
            None
        };
        
        if let Some(cache_key) = cache::CacheKey::from_request(
            method,
            host_str,
            path_only,
            query,
            cache_config.include_query,
            vary_key_headers.as_deref(), // key_headers に基づくVaryキー
        ) {
            // キャッシュ保存用コンテキストを作成
            let max_capture = cache_config.max_memory_size.min(10 * 1024 * 1024); // 最大10MB
            cache_save_ctx = Some(CacheSaveContext::new(cache_key, host_str.to_string(), max_capture));
        }
    }
    
    // バッファリングモードのログ出力（デバッグ用）
    if buffering_config.is_enabled() {
        debug!("Buffering enabled for {} {} (mode={:?})", 
               host_str, path_str, buffering_config.mode);
    }
    
    // ロードバランシング: UpstreamGroup からサーバーを選択
    let server = match upstream_group.select(client_ip) {
        Some(s) => s,
        None => {
            // 利用可能なサーバーがない
            error!("No healthy upstream servers available");
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((client_stream, 502, 0, true));
        }
    };
    
    // 接続カウンターを増加（Least Connections 用）
    server.acquire();
    
    let target = &server.target;
    // コネクションプールキーの生成
    // HTTPS接続でSNI名が設定されている場合は、異なるSNI名は異なるプールとして扱う
    let pool_key = if target.use_tls && target.sni_name.is_some() {
        format!("{}:{}:{}", target.host, target.port, target.sni())
    } else {
        format!("{}:{}", target.host, target.port)
    };
    
    // リクエストパス構築
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    let sub_path = if prefix.is_empty() {
        path_str.to_string()
    } else {
        let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
        if path_str.starts_with(prefix_str) {
            let remaining = &path_str[prefix_str.len()..];
            let base = target.path_prefix.trim_end_matches('/');
            build_sub_path(base, remaining)
        } else {
            path_str.to_string()
        }
    };

    let final_path = if sub_path.is_empty() { "/" } else { &sub_path };

    // HTTPリクエスト構築（プール使用）
    // 定数バイト列を使用してアロケーションを削減
    let mut request = request_buf_get(1024);
    request.extend_from_slice(method);
    request.extend_from_slice(HEADER_SPACE);
    request.extend_from_slice(final_path.as_bytes());
    request.extend_from_slice(HEADER_HTTP11_HOST);
    request.extend_from_slice(target.host.as_bytes());
    
    if !target.is_default_port() {
        request.extend_from_slice(HEADER_PORT_COLON);
        let mut port_buf = itoa::Buffer::new();
        request.extend_from_slice(port_buf.format(target.port).as_bytes());
    }
    
    request.extend_from_slice(HEADER_CRLF);
    
    for (name, value) in headers {
        // host と connection ヘッダーは別途処理済みのためスキップ
        if name.eq_ignore_ascii_case(b"host") || name.eq_ignore_ascii_case(b"connection") {
            continue;
        }

        // RFC 7230 Section 6.1: Hop-by-hopヘッダーを削除
        // Connection, Keep-Alive, Proxy-Connection, TE, Trailer, Transfer-Encoding, Upgrade
        // これらのヘッダーはプロキシで終端され、バックエンドに転送してはならない
        if is_hop_by_hop_header(name) {
            continue;
        }

        // 設定で削除が指定されているヘッダーをスキップ
        // eq_ignore_ascii_case でアロケーションなしに大文字小文字無視比較
        if security.remove_request_headers.iter().any(|h| name.eq_ignore_ascii_case(h.as_bytes())) {
            continue;
        }
        
        // Header Injection防止: ヘッダー名と値の検証
        // httparseによるパース後も、多層防御として再検証を行う
        // 不正な文字（CR, LF, NUL等）を含むヘッダーは除外
        if !is_valid_header_name(name) {
            warn!("Invalid header name detected, skipping: {:?}", 
                  String::from_utf8_lossy(name));
            continue;
        }
        if !is_valid_header_value(value) {
            warn!("Invalid header value detected (possible header injection), skipping header: {:?}", 
                  String::from_utf8_lossy(name));
            continue;
        }
        
        request.extend_from_slice(name);
        request.extend_from_slice(HEADER_COLON);
        request.extend_from_slice(value);
        request.extend_from_slice(HEADER_CRLF);
    }
    
    // 設定で追加が指定されているヘッダーを追加
    // 特殊変数の置換: $client_ip, $host, $request_uri
    for (header_name, header_value) in &security.add_request_headers {
        // 特殊変数を置換
        let host_str = headers.iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(b"host"))
            .map(|(_, v)| std::str::from_utf8(v).unwrap_or("-"))
            .unwrap_or("-");
        
        let value_replaced = header_value
            .replace("$client_ip", client_ip)
            .replace("$host", host_str)
            .replace("$request_uri", path_str);
        
        // Header Injection防止チェック
        if !is_valid_header_value(value_replaced.as_bytes()) {
            warn!("Invalid add_request_header value: {}", header_name);
            continue;
        }
        
        request.extend_from_slice(header_name.as_bytes());
        request.extend_from_slice(HEADER_COLON);
        request.extend_from_slice(value_replaced.as_bytes());
        request.extend_from_slice(HEADER_CRLF);
    }
    
    // Via ヘッダー追加 (RFC 7230 Section 5.7.1)
    // プロキシ経由のリクエストに Via ヘッダーを追加
    {
        let config = CURRENT_CONFIG.load();
        if config.performance.via_header_enabled {
            let hostname = config.performance.via_header_hostname
                .as_deref()
                .unwrap_or("veil");
            // Via: 1.1 <hostname>
            request.extend_from_slice(b"Via: 1.1 ");
            request.extend_from_slice(hostname.as_bytes());
            request.extend_from_slice(HEADER_CRLF);
        }
    }
    
    // chunked リクエストはボディを chunked フレームのままバックエンドへ転送する。
    // 上のループで Transfer-Encoding を hop-by-hop ヘッダとして除去しているため、
    // chunked の場合はここで再付与しないと、バックエンドはボディ長を判別できず本文を
    // 読まないまま応答する。その結果、残った chunked フレーム
    // （例: `5\r\nhello\r\n0\r\n\r\n`）が keep-alive 接続上で次のリクエストとして
    // 解釈され、`400 Bad Request` の desync を引き起こす（負荷時に顕在化）。
    if is_chunked {
        request.extend_from_slice(b"Transfer-Encoding: chunked\r\n");
    }

    // バックエンドにはKeep-Aliveを要求
    request.extend_from_slice(HEADER_CONNECTION_KEEPALIVE_END);

    let result = if target.use_tls {
        // HTTPS接続（キャッシュ保存はHTTPのみサポート、HTTPSは別途実装が必要）
        // 設定ファイルの tls_insecure、または環境変数 VEIL_TLS_INSECURE で証明書検証スキップを制御
        let tls_insecure = upstream_group.tls_insecure() 
            || std::env::var("VEIL_TLS_INSECURE").map(|v| v == "1" || v == "true").unwrap_or(false);
        proxy_https_pooled(client_stream, target, security, compression, buffering_config, client_encoding, &pool_key, request, content_length, is_chunked, initial_body, client_wants_close, tls_insecure, wasm_modules).await
    } else if target.use_h2c || upstream_group.use_h2c() {
        // H2C (HTTP/2 over cleartext) 接続
        #[cfg(feature = "http2")]
        {
            // gRPCメッセージサイズ制限チェック
            let is_grpc = headers.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case(b"content-type") && value.starts_with(b"application/grpc")
            });
            if is_grpc && content_length > MAX_GRPC_BODY_SIZE {
                let err_buf = ERR_MSG_REQUEST_TOO_LARGE.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                Some((client_stream, 413, 0, true))
            } else {
                proxy_h2c(
                    client_stream,
                    target,
                    security,
                    method,
                    final_path.as_bytes(),
                    headers,
                    initial_body,
                    client_wants_close
                ).await
            }
        }
        #[cfg(not(feature = "http2"))]
        {
            // HTTP/2 feature が無効な場合はHTTP/1.1にフォールバック
            warn!("H2C requested but http2 feature not enabled, falling back to HTTP/1.1");
            proxy_http_pooled(client_stream, target, security, compression, buffering_config, client_encoding, &pool_key, request, content_length, is_chunked, initial_body, client_wants_close, cache_save_ctx.as_mut(), wasm_modules.clone()).await
        }
    } else {
        // HTTP接続（キャッシュ保存・バッファリング対応）
        proxy_http_pooled(client_stream, target, security, compression, buffering_config, client_encoding, &pool_key, request, content_length, is_chunked, initial_body, client_wants_close, cache_save_ctx.as_mut(), wasm_modules).await
    };
    
    // 接続カウンターを減少（Least Connections 用）
    server.release();
    
    // stale-if-error: バックエンドエラー時にstaleキャッシュを返す
    if cache_config.stale_if_error {
        if let Some((mut client_stream, status_code, _, _)) = result {
            // バックエンドエラー（502, 504）の場合
            if status_code == 502 || status_code == 504 {
                // staleキャッシュを確認
                #[cfg(feature = "cache")]
                let _cache_key_opt = cache_save_ctx.as_ref().map(|c| c.key.clone());
                #[cfg(not(feature = "cache"))]
                let _cache_key_opt: Option<cache::CacheKey> = None;
                if let Some(cache_key) = _cache_key_opt {
                    if let Some(cache_manager) = cache::get_global_cache() {
                        // 最大1時間のstaleキャッシュを許容
                        if let Some(stale_entry) = cache_manager.get_stale(&cache_key, 3600) {
                            debug!("stale-if-error: serving stale cache for {}", host_str);
                            
                            // staleキャッシュを返す
                            if let Some(body_data) = stale_entry.memory_body() {
                                let response = build_cached_response(&stale_entry, body_data, client_wants_close, true);
                                match timeout(WRITE_TIMEOUT, client_stream.write_all(response)).await {
                                    Ok((Ok(_), _)) => {
                                        return Some((client_stream, stale_entry.status_code, body_data.len() as u64, client_wants_close));
                                    }
                                    _ => {
                                        return None;
                                    }
                                }
                            } else if let Some(disk_path) = stale_entry.disk_path() {
                                match serve_from_disk_cache(&mut client_stream, &stale_entry, disk_path, client_wants_close, true).await {
                                    Some((code, size)) => {
                                        return Some((client_stream, code, size, client_wants_close));
                                    }
                                    None => {}
                                }
                            }
                        }
                    }
                }
            }
            // staleキャッシュがない場合は元のエラーレスポンスをそのまま返す
            return Some((client_stream, status_code, 0, client_wants_close));
        }
        return result;
    }
    
    result
}

// ====================
// HTTP プロキシ（コネクションプール対応）
// ====================

async fn proxy_http_pooled(
    mut client_stream: ServerTls,
    target: &ProxyTarget,
    security: &SecurityConfig,
    compression: &CompressionConfig,
    buffering_config: &buffering::BufferingConfig,
    client_encoding: AcceptedEncoding,
    pool_key: &str,
    request: Vec<u8>,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
    client_wants_close: bool,
    cache_ctx: Option<&mut CacheSaveContext>,
    wasm_modules: Vec<String>,
) -> Option<(ServerTls, u16, u64, bool)> {
    // セキュリティ設定からタイムアウトを取得
    let connect_timeout = Duration::from_secs(security.backend_connect_timeout_secs);
    
    // プールから接続を取得、または新規作成
    let mut backend_stream = match HTTP_POOL.with(|p| p.borrow_mut().get(pool_key)) {
        Some(stream) => stream,
        None => {
            // 新規接続を作成
            let addr = format!("{}:{}", target.host, target.port);
            let connect_result = timeout(connect_timeout, TcpStream::connect(&addr)).await;
            
            match connect_result {
                Ok(Ok(stream)) => {
                    let _ = stream.set_nodelay(true);
                    stream
                }
                Ok(Err(e)) => {
                    error!("Proxy connect error to {}: {}", addr, e);
                    let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                    return Some((client_stream, 502, 0, true));
                }
                Err(_) => {
                    error!("Proxy connect timeout to {}", addr);
                    let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                    return Some((client_stream, 504, 0, true));
                }
            }
        }
    };

    // セキュリティ設定からchunked最大サイズを取得
    let max_chunked = security.max_chunked_body_size as u64;
    
    // 圧縮が有効かどうかの事前判定
    // 注意: 実際のContent-Typeはレスポンス受信後に判定するため、ここでは設定の有効/無効のみ確認
    let compression_enabled = compression.enabled && client_encoding != AcceptedEncoding::Identity;
    
    // メトリクス用ホスト名
    let host_str_for_metrics = &target.host;
    
    // バッファリングが有効かどうか判定
    let buffering_enabled = buffering_config.is_enabled() && buffering_config.should_buffer(Some(content_length));
    
    // リクエスト送信とレスポンス受信
    // kTLS 有効時は splice(2) を使用してゼロコピー転送
    // ただし、圧縮有効、キャッシュ保存が必要、またはバッファリング有効な場合はkTLSを迂回
    #[cfg(feature = "ktls")]
    let result = {
        // キャッシュ保存が必要かどうか
        // キャッシュ保存が必要な場合はsplice転送を使用できない（ユーザー空間でボディをキャプチャする必要がある）
        let cache_save_needed = cache_ctx.is_some();
        
        // kTLS + splice 版を試みる条件:
        // - kTLS有効
        // - Content-Length転送（非chunked）
        // - 圧縮無効
        // - キャッシュ保存不要
        // - バッファリング無効
        // - WASMモジュール未設定（WASM有効時はユーザー空間でレスポンスヘッダーを操作する必要がある）
        #[cfg(feature = "wasm")]
        let wasm_modules_active = !wasm_modules.is_empty();
        #[cfg(not(feature = "wasm"))]
        let wasm_modules_active = false;
        if client_stream.is_ktls_enabled() && !is_chunked && !compression_enabled && !cache_save_needed && !buffering_enabled && !wasm_modules_active {
            let splice_result = proxy_http_request_splice(
                &client_stream,
                &backend_stream,
                &request,
                content_length,
                is_chunked,
                initial_body,
            ).await;
            
            if splice_result.is_some() {
                splice_result
            } else {
                // splice 版が失敗した場合は通常版にフォールバック
                proxy_http_request_with_compression(
                    &mut client_stream,
                    &mut backend_stream,
                    request,
                    content_length,
                    is_chunked,
                    initial_body,
                    max_chunked,
                    compression,
                    client_encoding,
                    cache_ctx,
                    security,
                    wasm_modules,
                ).await
            }
        } else if buffering_enabled && !compression_enabled {
            // バッファリング有効時（圧縮無効の場合のみ）
            debug!("Calling proxy_request_buffered for {} {}", target.host, target.port);
            record_buffering_used(&host_str_for_metrics);
            proxy_request_buffered(
                &mut client_stream,
                &mut backend_stream,
                request,
                content_length,
                is_chunked,
                initial_body,
                max_chunked,
                buffering_config,
                cache_ctx,
                security,
            ).await
        } else {
            debug!("Calling proxy_http_request_with_compression for {} {} (buffering_enabled={}, compression_enabled={})",
                   target.host, target.port, buffering_enabled, compression_enabled);
            // kTLS が無効、Chunked、圧縮有効、キャッシュ保存が必要、またはバッファリング無効の場合は通常版を使用
            proxy_http_request_with_compression(
                &mut client_stream,
                &mut backend_stream,
                request,
                content_length,
                is_chunked,
                initial_body,
                max_chunked,
                compression,
                client_encoding,
                cache_ctx,
                security,
                wasm_modules,
            ).await
        }
    };
    
    #[cfg(not(feature = "ktls"))]
    let result = if buffering_enabled && !compression_enabled {
        // バッファリング有効時（圧縮無効の場合のみ）
        record_buffering_used(&host_str_for_metrics);
        proxy_request_buffered(
            &mut client_stream,
            &mut backend_stream,
            request,
            content_length,
            is_chunked,
            initial_body,
            max_chunked,
            buffering_config,
            cache_ctx,
            security,
        ).await
    } else {
                proxy_http_request_with_compression(
                    &mut client_stream,
                    &mut backend_stream,
                    request,
                    content_length,
                    is_chunked,
                    initial_body,
                    max_chunked,
                    compression,
                    client_encoding,
                    cache_ctx,
                    security,
                    wasm_modules,
                ).await
    };

    match result {
        Some((status_code, total, backend_wants_keep_alive)) => {
            // バックエンドがKeep-Aliveを許可している場合、プールに返却
            if backend_wants_keep_alive {
                let max_idle = security.max_idle_connections_per_host;
                let idle_timeout = security.idle_connection_timeout_secs;
                HTTP_POOL.with(|p| p.borrow_mut().put(pool_key.to_string(), backend_stream, max_idle, idle_timeout));
            }
            // 408 (body timeout) sends Connection: close — must actually close
            let should_close = client_wants_close || status_code == 408;
            Some((client_stream, status_code, total, should_close))
        }
        None => {
            // エラー発生時は接続を破棄
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            Some((client_stream, 502, 0, true))
        }
    }
}

// ====================
// H2C プロキシ (HTTP/2 over cleartext)
// ====================
//
// HTTP/2 Prior Knowledge モードでバックエンドに接続し、
// リクエストを転送します。gRPCバックエンドへの接続に適しています。
// ====================

/// H2C (HTTP/2 over cleartext) プロキシ
/// 
/// HTTP/2 Prior Knowledge モードでバックエンドに接続し、
/// リクエストを送信してレスポンスを受信します。
#[cfg(feature = "http2")]
async fn proxy_h2c(
    mut client_stream: ServerTls,
    target: &ProxyTarget,
    security: &SecurityConfig,
    method: &[u8],
    path: &[u8],
    headers: &[(Box<[u8]>, Box<[u8]>)],
    request_body: &[u8],
    client_wants_close: bool,
) -> Option<(ServerTls, u16, u64, bool)> {
    let connect_timeout = Duration::from_secs(security.backend_connect_timeout_secs);
    
    // バックエンドに接続
    let addr = format!("{}:{}", target.host, target.port);
    let connect_result = timeout(connect_timeout, TcpStream::connect(&addr)).await;
    
    let backend_stream = match connect_result {
        Ok(Ok(stream)) => {
            let _ = stream.set_nodelay(true);
            stream
        }
        Ok(Err(e)) => {
            error!("H2C connect error to {}: {}", addr, e);
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((client_stream, 502, 0, true));
        }
        Err(_) => {
            error!("H2C connect timeout to {}", addr);
            let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((client_stream, 504, 0, true));
        }
    };

    // H2Cクライアントを作成
    let settings = http2::Http2Settings::default();
    let mut h2c_client = http2::H2cClient::new(backend_stream, settings);

    // HTTP/2 ハンドシェイク
    if let Err(e) = h2c_client.handshake().await {
        error!("H2C handshake error: {}", e);
        let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
        let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
        return Some((client_stream, 502, 0, true));
    }

    // ヘッダーを変換 (Box<[u8]> -> &[u8])
    let headers_ref: Vec<(&[u8], &[u8])> = headers.iter()
        .map(|(k, v)| (k.as_ref(), v.as_ref()))
        .collect();

    // リクエストを送信
    let body = if request_body.is_empty() { None } else { Some(request_body) };
    let authority = target.host.as_bytes();
    
    let response = match h2c_client.send_request(method, path, authority, &headers_ref, body).await {
        Ok(resp) => resp,
        Err(e) => {
            error!("H2C request error: {}", e);
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            return Some((client_stream, 502, 0, true));
        }
    };

    // レスポンスをHTTP/1.1形式でクライアントに返す
    let status_code = response.status;
    let mut http11_response = Vec::with_capacity(512 + response.body.len());
    
    // ステータス行
    http11_response.extend_from_slice(b"HTTP/1.1 ");
    let mut status_buf = itoa::Buffer::new();
    http11_response.extend_from_slice(status_buf.format(status_code).as_bytes());
    http11_response.extend_from_slice(b" ");
    http11_response.extend_from_slice(status_reason_phrase(status_code).as_bytes());
    http11_response.extend_from_slice(b"\r\n");

    // レスポンスヘッダー
    for (name, value) in &response.headers {
        // ホップバイホップヘッダーはスキップ
        if name.eq_ignore_ascii_case(b"connection")
            || name.eq_ignore_ascii_case(b"transfer-encoding")
            || name.eq_ignore_ascii_case(b"keep-alive")
        {
            continue;
        }
        http11_response.extend_from_slice(name);
        http11_response.extend_from_slice(b": ");
        http11_response.extend_from_slice(value);
        http11_response.extend_from_slice(b"\r\n");
    }

    // トレーラーヘッダー（gRPC-status など）をレスポンスヘッダーとして転送
    for (name, value) in &response.trailers {
        http11_response.extend_from_slice(name);
        http11_response.extend_from_slice(b": ");
        http11_response.extend_from_slice(value);
        http11_response.extend_from_slice(b"\r\n");
    }

    // Content-Length
    http11_response.extend_from_slice(b"Content-Length: ");
    http11_response.extend_from_slice(status_buf.format(response.body.len()).as_bytes());
    http11_response.extend_from_slice(b"\r\n");

    // Connection ヘッダー
    if client_wants_close {
        http11_response.extend_from_slice(b"Connection: close\r\n");
    } else {
        http11_response.extend_from_slice(b"Connection: keep-alive\r\n");
    }

    http11_response.extend_from_slice(b"\r\n");

    // ボディ
    http11_response.extend_from_slice(&response.body);

    let resp_size = http11_response.len() as u64;

    // クライアントに送信
    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(http11_response)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return None;
    }

    Some((client_stream, status_code, resp_size, client_wants_close))
}



/// ディスクキャッシュからレスポンスを提供
/// 
/// 戻り値: Some((status_code, body_size)) または None（エラー時）
async fn serve_from_disk_cache(
    client_stream: &mut ServerTls,
    cached_entry: &cache::CacheEntry,
    disk_path: &std::path::Path,
    client_wants_close: bool,
    is_stale: bool,
) -> Option<(u16, u64)> {
    // ディスクからボディを読み込み（monoio::fs使用）
    let body_data = match monoio::fs::File::open(disk_path).await {
        Ok(file) => {
            let file_size = cached_entry.body_size as usize;
            let mut buf = Vec::with_capacity(file_size);
            #[allow(clippy::uninit_vec)]
            unsafe { buf.set_len(file_size); }
            
            match file.read_exact_at(buf, 0).await {
                (Ok(_), data) => data,
                (Err(e), _) => {
                    error!("Failed to read disk cache file: {}", e);
                    return None;
                }
            }
        }
        Err(e) => {
            error!("Failed to open disk cache file: {}", e);
            return None;
        }
    };
    
    // レスポンスを構築
    let response = build_cached_response(cached_entry, &body_data, client_wants_close, is_stale);
    
    match timeout(WRITE_TIMEOUT, client_stream.write_all(response)).await {
        Ok((Ok(_), _)) => Some((cached_entry.status_code, body_data.len() as u64)),
        _ => None,
    }
}


// ====================
// バッファリング結果型
// ====================

/// バッファリングされたボディ結果
enum BufferedBodyResult {
    /// メモリ内にバッファリング
    Memory(Vec<u8>),
    /// ディスクにスピルオーバー
    Disk {
        path: std::path::PathBuf,
        size: u64,
    },
    /// バッファサイズ制限超過 (507 Insufficient Storage)
    LimitExceeded,
    /// バッファリング失敗（ストリーミングにフォールバック）
    Failed,
}

impl BufferedBodyResult {
    /// サイズを取得
    #[allow(dead_code)]
    fn size(&self) -> u64 {
        match self {
            BufferedBodyResult::Memory(data) => data.len() as u64,
            BufferedBodyResult::Disk { size, .. } => *size,
            BufferedBodyResult::LimitExceeded => 0,
            BufferedBodyResult::Failed => 0,
        }
    }
}

// ====================
// HTTPリクエスト送信とレスポンス受信（バッファリング対応版）
// ====================
//
// バッファリングが有効な場合、バックエンドからのレスポンス全体を
// メモリにバッファリングしてからクライアントに転送します。
// これにより、バックエンド接続を早期に解放し、低速クライアントによる
// バックエンドスレッド占有を防止します。
// ====================

/// バッファリング転送でHTTPリクエストを処理
/// 
/// バックエンドからレスポンス全体を受信してバッファに格納し、
/// バックエンド接続を解放してからクライアントへ送信します。
/// 
/// 戻り値: Option<(status_code, response_size, backend_wants_keep_alive)>
/// バッファリング転送でリクエストを処理
async fn proxy_request_buffered<R>(
    client_stream: &mut ServerTls,
    backend_stream: &mut R,
    request: Vec<u8>,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
    max_chunked_body_size: u64,
    buffering_config: &buffering::BufferingConfig,
    cache_ctx: Option<&mut CacheSaveContext>,
    security: &SecurityConfig,
) -> Option<(u16, u64, bool)>
where R: AsyncReader + AsyncWriter + Unpin + monoio::io::AsyncReadRent + monoio::io::AsyncWriteRentExt {
    // 1. リクエストヘッダー送信
    let write_result = timeout(WRITE_TIMEOUT, backend_stream.write_all(request)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return None;
    }

    // 2. リクエストボディ送信
    if !initial_body.is_empty() {
        let body_buf = initial_body.to_vec();
        let write_result = timeout(WRITE_TIMEOUT, backend_stream.write_all(body_buf)).await;
        if !matches!(write_result, Ok((Ok(_), _))) {
            return None;
        }
    }

    // 3. 残りのリクエストボディを転送
    if is_chunked {
        match transfer_chunked_body(client_stream, backend_stream, initial_body, max_chunked_body_size).await {
            ChunkedTransferResult::Complete => {}
            ChunkedTransferResult::Failed => return None,
            ChunkedTransferResult::SizeLimitExceeded => return None,
        }
    } else {
        let remaining = content_length.saturating_sub(initial_body.len());
        if remaining > 0 {
            let body_timeout = Duration::from_secs(security.client_body_timeout_secs);
            match timeout(body_timeout, transfer_exact_bytes(client_stream, backend_stream, remaining)).await {
                Ok(transferred) if transferred >= remaining as u64 => {}
                Ok(_) => return None,
                Err(_) => {
                    let _ = client_stream.write_all(ERR_MSG_REQUEST_TIMEOUT.to_vec()).await;
                    return Some((408, 0, false));
                }
            }
        }
    }

    // 4. レスポンスを受信してバッファリング
    let buffered = receive_and_buffer_response(backend_stream, buffering_config, cache_ctx).await;
    
    match buffered {
        Some((status_code, headers_data, body_result, backend_wants_keep_alive)) => {
            // 5. バッファからクライアントへ送信
            let mut total = 0u64;
            
            // buffer_headers の設定に応じてヘッダーとボディを送信
            if buffering_config.buffer_headers {
                // buffer_headers = true: ヘッダーとボディを結合して送信（デフォルト動作）
                // これにより、クライアントへの書き込み回数を削減
                match body_result {
                    BufferedBodyResult::Memory(body_data) => {
                        // ヘッダーとボディを結合（headers_data を move してコピー回数を削減）
                        let mut combined = headers_data;
                        combined.extend_from_slice(&body_data);
                        let combined_len = combined.len();

                        let write_result = timeout(
                            Duration::from_secs(buffering_config.client_write_timeout_secs),
                            client_stream.write_all(combined)
                        ).await;

                        if matches!(write_result, Ok((Ok(_), _))) {
                            total = combined_len as u64;
                        }
                    }
                    BufferedBodyResult::Disk { path, size } => {
                        // ヘッダーを先に送信（所有権を移動、clone 不要）
                        let headers_len = headers_data.len();
                        let write_result = timeout(
                            Duration::from_secs(buffering_config.client_write_timeout_secs),
                            client_stream.write_all(headers_data)
                        ).await;

                        if !matches!(write_result, Ok((Ok(_), _))) {
                            let _ = monoio::fs::remove_file(&path).await;
                            return Some((status_code, 0, false));
                        }

                        total = headers_len as u64;

                        // ディスクから読み込んでクライアントに送信
                        match send_disk_buffer_to_client(client_stream, &path, size, buffering_config.client_write_timeout_secs).await {
                            Some(sent) => {
                                total += sent;
                            }
                            None => {
                                let _ = monoio::fs::remove_file(&path).await;
                                return Some((status_code, total, false));
                            }
                        }
                        let _ = monoio::fs::remove_file(&path).await;
                    }
                    BufferedBodyResult::Failed => {
                        // ヘッダーのみ送信（所有権を移動、clone 不要）
                        let headers_len = headers_data.len();
                        let write_result = timeout(
                            Duration::from_secs(buffering_config.client_write_timeout_secs),
                            client_stream.write_all(headers_data)
                        ).await;
                        if matches!(write_result, Ok((Ok(_), _))) {
                            total = headers_len as u64;
                        }
                        return Some((status_code, total, false));
                    }
                    BufferedBodyResult::LimitExceeded => {
                        // 507 Insufficient Storage を送信
                        let err_buf = ERR_MSG_INSUFFICIENT_STORAGE.to_vec();
                        let _ = timeout(
                            Duration::from_secs(buffering_config.client_write_timeout_secs),
                            client_stream.write_all(err_buf)
                        ).await;
                        // 507 エラー時は接続を閉じる (should_close = true, backend keep-alive = false)
                        return Some((507, 0, false));
                    }
                }
            } else {
                // buffer_headers = false: ヘッダーを先に送信し、ボディは別途送信
                // ヘッダー送信（所有権を移動、clone 不要）
                let headers_len = headers_data.len();
                let write_result = timeout(
                    Duration::from_secs(buffering_config.client_write_timeout_secs),
                    client_stream.write_all(headers_data)
                ).await;

                if !matches!(write_result, Ok((Ok(_), _))) {
                    // ディスクファイルがあればクリーンアップ（非同期削除）
                    if let BufferedBodyResult::Disk { ref path, .. } = body_result {
                        let _ = monoio::fs::remove_file(path).await;
                    }
                    return Some((status_code, 0, false));
                }

                total = headers_len as u64;

                // ボディ送信（メモリまたはディスクから）
                match body_result {
                    BufferedBodyResult::Memory(body_data) => {
                        if !body_data.is_empty() {
                            let body_len = body_data.len();
                            let write_result = timeout(
                                Duration::from_secs(buffering_config.client_write_timeout_secs),
                                client_stream.write_all(body_data)
                            ).await;

                            if !matches!(write_result, Ok((Ok(_), _))) {
                                return Some((status_code, total, false));
                            }

                            total += body_len as u64;
                        }
                    }
                    BufferedBodyResult::Disk { path, size } => {
                        // ディスクから読み込んでクライアントに送信
                        match send_disk_buffer_to_client(client_stream, &path, size, buffering_config.client_write_timeout_secs).await {
                            Some(sent) => {
                                total += sent;
                            }
                            None => {
                                let _ = monoio::fs::remove_file(&path).await;
                                return Some((status_code, total, false));
                            }
                        }
                        let _ = monoio::fs::remove_file(&path).await;
                    }
                    BufferedBodyResult::Failed => {
                        return Some((status_code, total, false));
                    }
                    BufferedBodyResult::LimitExceeded => {
                        // すでにヘッダー送信済みのため、507を返すことはできないので接続を閉じる
                        return Some((status_code, total, true));
                    }
                }
            }
            
            Some((status_code, total, backend_wants_keep_alive))
        }
        None => None,
    }
}

/// バックエンドからレスポンスを受信してバッファリング
/// 
/// 戻り値: Option<(status_code, headers_data, body_result, backend_wants_keep_alive)>
async fn receive_and_buffer_response<R>(
    backend_stream: &mut R,
    buffering_config: &buffering::BufferingConfig,
    mut cache_ctx: Option<&mut CacheSaveContext>,
) -> Option<(u16, Vec<u8>, BufferedBodyResult, bool)> 
where R: AsyncReadRent + Unpin {
    let mut accumulated = Vec::with_capacity(BUF_SIZE);
    
    // ヘッダー読み取り
    loop {
        let read_buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
        
        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => return None,
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                return None;
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                return None;
            }
        };
        
        returned_buf.set_valid_len(n);
        accumulated.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);
        
        // ヘッダーが完全に受信されたかチェック
        if let Some(parsed) = parse_http_response(&accumulated) {
            let status_code = parsed.status_code;
            let backend_wants_keep_alive = !parsed.is_connection_close;
            
            let header_len = parsed.header_len;
            let body_start = accumulated[header_len..].to_vec();
            let headers_data = accumulated[..header_len].to_vec();
            
            // キャッシュコンテキストにヘッダーを設定
            if let Some(ref mut ctx) = cache_ctx {
                let mut headers_storage = [httparse::EMPTY_HEADER; 64];
                let mut response = httparse::Response::new(&mut headers_storage);
                if response.parse(&headers_data).is_ok() {
                    let headers: Vec<(Box<[u8]>, Box<[u8]>)> = response.headers.iter()
                        .map(|h| (h.name.as_bytes().into(), h.value.into()))
                        .collect();
                    ctx.set_headers(headers, status_code);
                }
            }
            
            // ボディをバッファリング
            let body_result = buffer_response_body_with_spillover(
                backend_stream,
                parsed.content_length,
                parsed.is_chunked,
                body_start,
                buffering_config,
                cache_ctx,
            ).await;
            
            return Some((status_code, headers_data, body_result, backend_wants_keep_alive));
        }
        
        // ヘッダーが大きすぎる場合は中止
        if accumulated.len() > MAX_HEADER_SIZE {
            return None;
        }
    }
}

/// ディスクバッファをクライアントに送信
async fn send_disk_buffer_to_client(
    client_stream: &mut ServerTls,
    path: &std::path::Path,
    size: u64,
    timeout_secs: u64,
) -> Option<u64> {
    // ディスクから読み込み
    let file = match monoio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) => {
            error!("Failed to open disk buffer: {}", e);
            return None;
        }
    };
    
    let mut buf = Vec::with_capacity(size as usize);
    #[allow(clippy::uninit_vec)]
    unsafe { buf.set_len(size as usize); }
    
    let (res, data) = file.read_exact_at(buf, 0).await;
    if res.is_err() {
        error!("Failed to read disk buffer");
        return None;
    }
    
    // クライアントに送信
    let write_result = timeout(
        Duration::from_secs(timeout_secs),
        client_stream.write_all(data)
    ).await;
    
    match write_result {
        Ok((Ok(_), _)) => Some(size),
        _ => None,
    }
}

/// レスポンスボディをバッファリング（ディスクスピルオーバー対応）
async fn buffer_response_body_with_spillover<R>(
    backend_stream: &mut R,
    content_length: Option<usize>,
    is_chunked: bool,
    initial_body: Vec<u8>,
    buffering_config: &buffering::BufferingConfig,
    mut cache_ctx: Option<&mut CacheSaveContext>,
) -> BufferedBodyResult 
where R: AsyncReadRent + Unpin {
    let mut body = initial_body;
    
    debug!("buffer_response_body_with_spillover: content_length={:?}, is_chunked={}, initial_body_len={}, max_mem={}, max_disk={}", 
          content_length, is_chunked, body.len(), buffering_config.max_memory_buffer, buffering_config.max_disk_buffer);
    
    // キャッシュコンテキストに初期ボディをキャプチャ
    if let Some(ref mut ctx) = cache_ctx {
        ctx.append_body(&body);
    }
    
    if let Some(cl) = content_length {
        // Content-Length 転送
        let cl_usize = cl as usize;
        let remaining = cl.saturating_sub(body.len());
        
        // バッファサイズ制限チェック (メモリ)
        if cl_usize > buffering_config.max_memory_buffer {
            // ディスクスピルオーバー
            if let Some(ref disk_path) = buffering_config.disk_buffer_path {
                // max_disk_buffer 制限チェック
                if cl > buffering_config.max_disk_buffer {
                    ftlog::warn!("Response size {} exceeds max_disk_buffer {}, aborting buffer", 
                           cl, buffering_config.max_disk_buffer);
                    return BufferedBodyResult::LimitExceeded;
                }
                
                debug!("Response size {} exceeds memory limit {}, spilling to disk (max_disk={})", 
                      cl, buffering_config.max_memory_buffer, buffering_config.max_disk_buffer);
                
                // まず残りのデータをメモリに読み込み
                if remaining > 0 {
                    let additional = buffer_exact_bytes(backend_stream, remaining, &mut cache_ctx).await;
                    body.extend(additional);
                }
                
                // ディスクに書き込み
                let key = format!("buffer_{}", std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos());
                
                match buffering::disk_buffer::write_to_disk(disk_path, key.as_bytes(), body).await {
                    Ok(path) => {
                        let size = cl as u64;
                        return BufferedBodyResult::Disk { path, size };
                    }
                    Err(e) => {
                        error!("Failed to write disk buffer: {}", e);
                        return BufferedBodyResult::Failed;
                    }
                }
            } else {
                // ディスクなし: 可能な範囲でメモリにバッファリング
                if remaining > 0 {
                    let max_additional = buffering_config.max_memory_buffer.saturating_sub(body.len());
                    if max_additional > 0 {
                        let additional = buffer_exact_bytes(backend_stream, max_additional, &mut cache_ctx).await;
                        body.extend(additional);
                    }
                }
                warn!("Response truncated: memory limit exceeded and no disk buffer configured");
            }
        } else {
            // メモリ制限内
            if remaining > 0 {
                let additional = buffer_exact_bytes(backend_stream, remaining, &mut cache_ctx).await;
                body.extend(additional);
            }
        }
    } else if is_chunked {
        // Chunked 転送
        let mut decoder = ChunkedDecoder::new_unlimited();
        decoder.feed(&body);
        
        if decoder.is_complete() {
            if let Some(ctx) = cache_ctx {
                ctx.save_to_cache();
            }
            return BufferedBodyResult::Memory(body);
        }
        
        loop {
            // バッファサイズ制限チェック
            if body.len() >= buffering_config.max_memory_buffer {
                // ディスクスピルオーバー（Chunked）
                if let Some(ref disk_path) = buffering_config.disk_buffer_path {
                    debug!("Chunked response exceeds memory limit, spilling to disk");
                    
                    // 残りを読み込み続ける
                    let mut overflow = Vec::new();
                    let max_disk = buffering_config.max_disk_buffer;
                    let mut total_size = body.len();
                    let mut size_exceeded = false;
                    
                    loop {
                        // max_disk_buffer 制限チェック
                        if total_size as u64 > max_disk as u64 {
                            ftlog::warn!("Chunked response exceeds max_disk_buffer {}, aborting buffer", max_disk);
                            size_exceeded = true;
                            break;
                        }
                        
                        let read_buf = buf_get();
                        let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
                        
                        let (res, mut returned_buf) = match read_result {
                            Ok(result) => result,
                            Err(_) => break,
                        };
                        
                        let n = match res {
                            Ok(0) => {
                                buf_put(returned_buf);
                                break;
                            }
                            Ok(n) => n,
                            Err(_) => {
                                buf_put(returned_buf);
                                break;
                            }
                        };
                        
                        returned_buf.set_valid_len(n);
                        let chunk = returned_buf.as_valid_slice();
                        let feed_result = decoder.feed(chunk);
                        
                        if let Some(ref mut ctx) = cache_ctx {
                            ctx.append_body(chunk);
                        }
                        
                        overflow.extend_from_slice(chunk);
                        total_size += n;
                        buf_put(returned_buf);
                        
                        if feed_result == ChunkedFeedResult::Complete {
                            break;
                        }
                    }
                    
                    if size_exceeded {
                        return BufferedBodyResult::LimitExceeded;
                    }
                    
                    // 全体をディスクに書き込み
                    body.extend(overflow);
                    let key = format!("buffer_chunked_{}", std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos());
                    
                    let size = body.len() as u64;
                    match buffering::disk_buffer::write_to_disk(disk_path, key.as_bytes(), body).await {
                        Ok(path) => {
                            if let Some(ctx) = cache_ctx {
                                ctx.save_to_cache();
                            }
                            return BufferedBodyResult::Disk { path, size };
                        }
                        Err(e) => {
                            error!("Failed to write chunked disk buffer: {}", e);
                            return BufferedBodyResult::Failed;
                        }
                    }
                }
                break;
            }
            
            let read_buf = buf_get();
            let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
            
            let (res, mut returned_buf) = match read_result {
                Ok(result) => result,
                Err(_) => break,
            };
            
            let n = match res {
                Ok(0) => {
                    buf_put(returned_buf);
                    break;
                }
                Ok(n) => n,
                Err(_) => {
                    buf_put(returned_buf);
                    break;
                }
            };
            
            returned_buf.set_valid_len(n);
            let chunk = returned_buf.as_valid_slice();
            let feed_result = decoder.feed(chunk);
            
            if let Some(ref mut ctx) = cache_ctx {
                ctx.append_body(chunk);
            }
            
            body.extend_from_slice(chunk);
            buf_put(returned_buf);
            
            if feed_result == ChunkedFeedResult::Complete {
                break;
            }
        }
    } else {
        // Content-Length も Chunked もない場合: EOF まで読み取り（ブラインドバッファリング）
        loop {
            // メモリサイズ制限チェック
            if body.len() >= buffering_config.max_memory_buffer {
                // ディスクスピルオーバー（ブラインド）
                if let Some(ref disk_path) = buffering_config.disk_buffer_path {
                    let mut overflow = Vec::new();
                    let max_disk = buffering_config.max_disk_buffer;
                    let mut total_size = body.len();
                    let mut size_exceeded = false;
                    
                    loop {
                        if total_size as u64 > max_disk as u64 {
                            ftlog::warn!("Blind response exceeds max_disk_buffer {}, aborting buffer", max_disk);
                            size_exceeded = true;
                            break;
                        }
                        
                        let read_buf = buf_get();
                        let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
                        let (res, mut returned_buf) = match read_result {
                            Ok(result) => result,
                            Err(_) => break,
                        };
                        
                        let n = match res {
                            Ok(0) => { buf_put(returned_buf); break; }
                            Ok(n) => n,
                            Err(_) => { buf_put(returned_buf); break; }
                        };
                        
                        returned_buf.set_valid_len(n);
                        let chunk = returned_buf.as_valid_slice();
                        if let Some(ref mut ctx) = cache_ctx {
                            ctx.append_body(chunk);
                        }
                        overflow.extend_from_slice(chunk);
                        total_size += n;
                        buf_put(returned_buf);
                    }
                    
                    if size_exceeded {
                        return BufferedBodyResult::LimitExceeded;
                    }
                    
                    body.extend(overflow);
                    let key = format!("buffer_blind_{}", std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos());
                    let size = body.len() as u64;
                    match buffering::disk_buffer::write_to_disk(disk_path, key.as_bytes(), body).await {
                        Ok(path) => {
                            if let Some(ctx) = cache_ctx {
                                ctx.save_to_cache();
                            }
                            return BufferedBodyResult::Disk { path, size };
                        }
                        Err(e) => {
                            error!("Failed to write blind disk buffer: {}", e);
                            return BufferedBodyResult::Failed;
                        }
                    }
                }
                break;
            }
            
            let read_buf = buf_get();
            let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
            let (res, mut returned_buf) = match read_result {
                Ok(result) => result,
                Err(_) => break,
            };
            
            let n = match res {
                Ok(0) => { buf_put(returned_buf); break; }
                Ok(n) => n,
                Err(_) => { buf_put(returned_buf); break; }
            };
            
            returned_buf.set_valid_len(n);
            let chunk = returned_buf.as_valid_slice();
            if let Some(ref mut ctx) = cache_ctx {
                ctx.append_body(chunk);
            }
            body.extend_from_slice(chunk);
            buf_put(returned_buf);
        }
    }
    
    // キャッシュに保存
    if let Some(ctx) = cache_ctx {
        ctx.save_to_cache();
    }
    
    BufferedBodyResult::Memory(body)
}

/// バックエンドから正確なバイト数を読み取りバッファに格納
async fn buffer_exact_bytes<R>(
    backend_stream: &mut R,
    mut remaining: usize,
    cache_ctx: &mut Option<&mut CacheSaveContext>,
) -> Vec<u8> 
where R: AsyncReadRent + Unpin {
    let mut result = Vec::with_capacity(remaining);
    
    while remaining > 0 {
        let read_buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
        
        let (res, mut returned_buf) = match read_result {
            Ok(r) => r,
            Err(_) => break,
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                break;
            }
            Ok(n) => n.min(remaining),
            Err(_) => {
                buf_put(returned_buf);
                break;
            }
        };
        
        returned_buf.set_valid_len(n);
        let chunk = &returned_buf.as_valid_slice()[..n];
        
        // キャッシュコンテキストにキャプチャ
        if let Some(ref mut ctx) = cache_ctx {
            ctx.append_body(chunk);
        }
        
        result.extend_from_slice(chunk);
        buf_put(returned_buf);
        remaining = remaining.saturating_sub(n);
    }
    
    result
}

// ====================
// HTTPリクエスト送信とレスポンス受信（圧縮対応版）
// ====================
//
// 圧縮設定が有効な場合、バックエンドからのレスポンスを動的に圧縮して
// クライアントに転送します。
// 
// 圧縮判定:
// 1. compression.enabled が true
// 2. クライアントが Accept-Encoding で圧縮をサポート
// 3. Content-Type が圧縮対象
// 4. Content-Length が min_size 以上
// 5. バックエンドのレスポンスが未圧縮
// ====================

/// HTTPリクエストを送信してレスポンスを受信（圧縮対応版）
/// 戻り値: Option<(status_code, response_size, backend_wants_keep_alive)>
async fn proxy_http_request_with_compression(
    client_stream: &mut ServerTls,
    backend_stream: &mut TcpStream,
    request: Vec<u8>,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
    max_chunked_body_size: u64,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    cache_ctx: Option<&mut CacheSaveContext>,
    security: &SecurityConfig,
    wasm_modules: Vec<String>,
) -> Option<(u16, u64, bool)> {
    // 1. リクエストヘッダー送信（タイムアウト付き）
    let write_result = timeout(WRITE_TIMEOUT, backend_stream.write_all(request)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return None;
    }

    // 2. リクエストボディ送信
    if !initial_body.is_empty() {
        let body_buf = initial_body.to_vec();
        let write_result = timeout(WRITE_TIMEOUT, backend_stream.write_all(body_buf)).await;
        if !matches!(write_result, Ok((Ok(_), _))) {
            return None;
        }
    }

    // 3. 残りのリクエストボディを転送
    if is_chunked {
        // Chunked転送の場合（DoS対策: ルートごとのmax_chunked_body_sizeで制限）
        match transfer_chunked_body(client_stream, backend_stream, initial_body, max_chunked_body_size).await {
            ChunkedTransferResult::Complete => {}
            ChunkedTransferResult::Failed => return None,
            ChunkedTransferResult::SizeLimitExceeded => {
                return None;
            }
        }
    } else {
        // Content-Length転送の場合
        let remaining = content_length.saturating_sub(initial_body.len());
        if remaining > 0 {
            let body_timeout = Duration::from_secs(security.client_body_timeout_secs);
            match timeout(body_timeout, transfer_exact_bytes(client_stream, backend_stream, remaining)).await {
                Ok(transferred) if transferred >= remaining as u64 => {}
                Ok(_) => return None,
                Err(_) => {
                    let _ = client_stream.write_all(ERR_MSG_REQUEST_TIMEOUT.to_vec()).await;
                    return Some((408, 0, false));
                }
            }
        }
    }

    // 4. レスポンスを受信して転送（圧縮対応、キャッシュ保存対応）
    let (total, status_code, backend_wants_keep_alive) =
        transfer_response_with_compression(backend_stream, client_stream, compression, client_encoding, cache_ctx, security, wasm_modules).await;

    Some((status_code, total, backend_wants_keep_alive))
}

// ====================
// レスポンス転送（圧縮対応版）
// ====================

/// レスポンスヘッダーを解析し、必要に応じて圧縮してクライアントに転送
/// キャッシュコンテキストが指定されている場合、レスポンスボディをキャプチャしてキャッシュに保存
async fn transfer_response_with_compression(
    backend_stream: &mut TcpStream,
    client_stream: &mut ServerTls,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    mut cache_ctx: Option<&mut CacheSaveContext>,
    security: &SecurityConfig,
    wasm_modules: Vec<String>,
) -> (u64, u16, bool) {
    let mut accumulated = Vec::with_capacity(BUF_SIZE);
    let mut total = 0u64;
    let mut status_code = 502u16;
    // 初期値false: エラー時はKeep-Aliveを無効化
    let mut backend_wants_keep_alive = false;

    // ヘッダー読み取り用バッファ
    loop {
        let read_buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
        
        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => {
                return (total, status_code, backend_wants_keep_alive);
            }
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                return (total, status_code, backend_wants_keep_alive);
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                return (total, status_code, backend_wants_keep_alive);
            }
        };
        
        returned_buf.set_valid_len(n);
        accumulated.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);
        
        // ヘッダーが完全に受信されたかチェック
        if let Some(parsed) = parse_http_response(&accumulated) {
            status_code = parsed.status_code;
            backend_wants_keep_alive = !parsed.is_connection_close;
            
            let header_len = parsed.header_len;
            let body_start = &accumulated[header_len..];
            
            // Content-Type と Content-Encoding を取得
            let content_type = extract_header_value(&accumulated[..header_len], b"content-type");
            let existing_encoding = extract_header_value(&accumulated[..header_len], b"content-encoding");
            
            // 圧縮すべきか判定
            let should_compress = compression.should_compress(
                client_encoding,
                content_type,
                parsed.content_length,
                existing_encoding,
            );
            
            if let Some(encoding) = should_compress {
                // 圧縮有効: ヘッダーを書き換えて圧縮転送
                // 注意: 圧縮時はキャッシュ保存をスキップ（圧縮後のデータをキャッシュするには追加実装が必要）
                info!("[Compression] Initializing compressed transfer with {:?}", encoding);
                let result = transfer_compressed_response(
                    client_stream,
                    backend_stream,
                    &accumulated[..header_len],
                    body_start,
                    parsed.content_length,
                    parsed.is_chunked,
                    encoding,
                    compression,
                    backend_wants_keep_alive,
                    security,
                ).await;
                
                return (result.0, status_code, result.1);
            } else {
                // 圧縮無効: そのまま転送（キャッシュ保存対応）
                
                // キャッシュコンテキストがある場合、ヘッダーを設定
                if let Some(ref mut ctx) = cache_ctx {
                    // ヘッダーを解析してキャッシュコンテキストに保存
                    let mut headers_storage = [httparse::EMPTY_HEADER; 64];
                    let mut response = httparse::Response::new(&mut headers_storage);
                    if response.parse(&accumulated[..header_len]).is_ok() {
                        let headers: Vec<(Box<[u8]>, Box<[u8]>)> = response.headers.iter()
                            .map(|h| (h.name.as_bytes().into(), h.value.into()))
                            .collect();
                        ctx.set_headers(headers, status_code);
                    }
                    
                    // 初期ボディをキャプチャ
                    ctx.append_body(body_start);
                }
                
                // ヘッダーを修正（security.add_response_headersを追加、remove_response_headersを削除）
                let mut modified_headers = accumulated[..header_len].to_vec();
                
                // ヘッダーをパースして操作
                let mut headers_storage = [httparse::EMPTY_HEADER; 64];
                let mut response = httparse::Response::new(&mut headers_storage);
                if response.parse(&modified_headers).is_ok() {
                    let mut new_header_lines = Vec::new();
                    
                    // ステータス行を追加
                    let status_line = format!("HTTP/1.1 {} {}\r\n", 
                        status_code, 
                        status_code_to_reason(status_code));
                    new_header_lines.push(status_line.into_bytes());
                    
                    // 既存のヘッダーを追加（削除対象を除外）
                    // eq_ignore_ascii_case でアロケーションなしに比較
                    for header in response.headers.iter() {
                        if !security.remove_response_headers.iter()
                            .any(|h| header.name.as_bytes().eq_ignore_ascii_case(h.as_bytes()))
                        {
                            new_header_lines.push(format!("{}: {}\r\n",
                                header.name,
                                std::str::from_utf8(header.value).unwrap_or("")).into_bytes());
                        }
                    }
                    
                    // 追加するヘッダーを追加
                    for (header_name, header_value) in &security.add_response_headers {
                        new_header_lines.push(format!("{}: {}\r\n", header_name, header_value).into_bytes());
                    }
                    
                    // WASMレスポンスヘッダーフィルタを適用
                    #[cfg(feature = "wasm")]
                    {
                        if !wasm_modules.is_empty() {
                            let config = CURRENT_CONFIG.load();
                            if let Some(ref wasm_engine) = config.wasm_filter_engine {
                                // 現在のヘッダーをVec<(Vec<u8>, Vec<u8>)>形式に変換
                                let current_headers: Vec<(Vec<u8>, Vec<u8>)> = new_header_lines.iter()
                                    .skip(1) // ステータス行をスキップ
                                    .filter_map(|line| {
                                        let line_str = std::str::from_utf8(line).ok()?;
                                        let line_trimmed = line_str.trim_end_matches("\r\n");
                                        if line_trimmed.is_empty() {
                                            return None;
                                        }
                                        let colon_pos = line_trimmed.find(':')?;
                                        let name = line_trimmed[..colon_pos].as_bytes().to_vec();
                                        let value = line_trimmed[colon_pos+1..].trim_start().as_bytes().to_vec();
                                        Some((name, value))
                                    })
                                    .collect();

                                // WASMフィルタを実行（レスポンスヘッダー処理）
                                let wasm_result = wasm_engine.clone().on_response_headers_with_modules_async(
                                    wasm_modules.clone(),
                                    status_code,
                                    current_headers,
                                    true, // end_of_stream
                                ).await;

                                match wasm_result {
                                    crate::wasm::FilterResult::Continue { headers: modified_headers, .. } => {
                                        // WASMから修正されたヘッダーで置き換え
                                        new_header_lines.clear();

                                        // ステータス行を再追加
                                        let status_line = format!("HTTP/1.1 {} {}\r\n",
                                            status_code,
                                            status_code_to_reason(status_code));
                                        new_header_lines.push(status_line.into_bytes());

                                        // WASMから返されたヘッダーを追加
                                        for (name, value) in modified_headers {
                                            let mut line = Vec::with_capacity(name.len() + value.len() + 4);
                                            line.extend_from_slice(&name);
                                            line.extend_from_slice(b": ");
                                            line.extend_from_slice(&value);
                                            line.extend_from_slice(b"\r\n");
                                            new_header_lines.push(line);
                                        }
                                    }
                                    crate::wasm::FilterResult::LocalResponse(_) => {
                                        // レスポンスヘッダー処理ではLocalResponseは無視
                                        // （すでにバックエンドレスポンスを受信しているため）
                                    }
                                    crate::wasm::FilterResult::Pause => {
                                        // 非同期処理待ち（現在は未実装）
                                    }
                                }
                            }
                        }
                    }
                    
                    // ヘッダー終了マーカーを追加
                    new_header_lines.push(b"\r\n".to_vec());
                    
                    // 結合
                    modified_headers = new_header_lines.into_iter().flatten().collect();
                }
                
                // 修正したヘッダーを送信
                let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(modified_headers)).await;
                if !matches!(write_result, Ok((Ok(_), _))) {
                    return (total, status_code, false);
                }
                total += header_len as u64;
                
                // 初期ボディを送信
                if !body_start.is_empty() {
                    let body_data = body_start.to_vec();
                    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(body_data)).await;
                    if !matches!(write_result, Ok((Ok(_), _))) {
                        return (total, status_code, false);
                    }
                    total += body_start.len() as u64;
                }
                
                // 残りのボディを転送（キャッシュキャプチャ対応）
                let body_remaining = if let Some(cl) = parsed.content_length {
                    cl.saturating_sub(body_start.len())
                } else if parsed.is_chunked {
                    // Chunked の場合は終端まで転送
                    usize::MAX
                } else {
                    0
                };
                
                if body_remaining > 0 {
                    let transferred = transfer_response_body_with_cache(
                        backend_stream,
                        client_stream,
                        parsed.content_length,
                        parsed.is_chunked,
                        body_start,
                        cache_ctx,
                    ).await;
                    total += transferred;
                }
                
                return (total, status_code, backend_wants_keep_alive);
            }
        }
        
        // ヘッダーが大きすぎる場合は中止
        if accumulated.len() > MAX_HEADER_SIZE {
            return (0, 502, false);
        }
    }
}


/// 圧縮してレスポンスを転送
/// 戻り値: (転送バイト数, backend_wants_keep_alive)
#[cfg(feature = "compression")]
async fn transfer_compressed_response(
    client_stream: &mut ServerTls,
    backend_stream: &mut TcpStream,
    original_headers: &[u8],
    initial_body: &[u8],
    content_length: Option<usize>,
    is_chunked: bool,
    encoding: AcceptedEncoding,
    compression: &CompressionConfig,
    backend_wants_keep_alive: bool,
    security: &SecurityConfig,
) -> (u64, bool) {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    
    let mut total = 0u64;
    
    // 1. まず全てのボディデータを収集（ストリーミングは将来の改善）
    let mut body_data = initial_body.to_vec();
    
    if let Some(cl) = content_length {
        let remaining = cl.saturating_sub(initial_body.len());
        if remaining > 0 {
            let mut remaining_to_read = remaining;
            while remaining_to_read > 0 {
                let read_buf = buf_get();
                let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
                
                let (res, mut returned_buf) = match read_result {
                    Ok(result) => result,
                    Err(_) => {
                        return (total, false);
                    }
                };
                
                let n = match res {
                    Ok(0) => {
                        buf_put(returned_buf);
                        break;
                    }
                    Ok(n) => n.min(remaining_to_read),
                    Err(_) => {
                        buf_put(returned_buf);
                        return (total, false);
                    }
                };
                
                returned_buf.set_valid_len(n);
                body_data.extend_from_slice(returned_buf.as_valid_slice());
                buf_put(returned_buf);
                remaining_to_read = remaining_to_read.saturating_sub(n);
            }
        }
    } else if is_chunked {
        // Chunked の場合はデコードして収集
        let mut decoder = ChunkedDecoder::new_unlimited();
        
        // 初期ボディをデコーダにフィード
        let initial_result = decoder.feed(initial_body);
        if initial_result == ChunkedFeedResult::Complete {
            // 初期ボディで完了（本来はデコード済みボディが必要だが、簡略化）
        } else {
            // 残りを読み取り
            loop {
                let read_buf = buf_get();
                let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
                
                let (res, mut returned_buf) = match read_result {
                    Ok(result) => result,
                    Err(_) => {
                        // タイムアウト
                        break;
                    }
                };
                
                let n = match res {
                    Ok(0) => {
                        buf_put(returned_buf);
                        break;
                    }
                    Ok(n) => n,
                    Err(_) => {
                        buf_put(returned_buf);
                        break;
                    }
                };
                
                returned_buf.set_valid_len(n);
                let chunk = returned_buf.as_valid_slice();
                body_data.extend_from_slice(chunk);
                let feed_result = decoder.feed(chunk);
                buf_put(returned_buf);
                
                if feed_result == ChunkedFeedResult::Complete {
                    break;
                }
            }
        }
    }
    
    // 2. ボディを圧縮
    let compressed_body = match encoding {
        AcceptedEncoding::Zstd => {
            match zstd::encode_all(std::io::Cursor::new(&body_data), compression.zstd_level) {
                Ok(compressed) => compressed,
                Err(_) => {
                    return transfer_uncompressed_fallback(
                        client_stream,
                        original_headers,
                        &body_data,
                    ).await;
                }
            }
        }
        AcceptedEncoding::Gzip => {
            let level = Compression::new(compression.gzip_level);
            let mut encoder = GzEncoder::new(Vec::new(), level);
            if encoder.write_all(&body_data).is_err() {
                // 圧縮失敗: 非圧縮で送信
                return transfer_uncompressed_fallback(
                    client_stream,
                    original_headers,
                    &body_data,
                ).await;
            }
            match encoder.finish() {
                Ok(data) => data,
                Err(_) => {
                    return transfer_uncompressed_fallback(
                        client_stream,
                        original_headers,
                        &body_data,
                    ).await;
                }
            }
        }
        AcceptedEncoding::Brotli => {
            let mut compressed = Vec::new();
            let params = brotli::enc::BrotliEncoderParams {
                quality: compression.brotli_level as i32,
                ..Default::default()
            };
            let mut input = std::io::Cursor::new(&body_data);
            if brotli::BrotliCompress(&mut input, &mut compressed, &params).is_err() {
                return transfer_uncompressed_fallback(
                    client_stream,
                    original_headers,
                    &body_data,
                ).await;
            }
            compressed
        }
        AcceptedEncoding::Deflate => {
            let level = Compression::new(compression.gzip_level);
            let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), level);
            if encoder.write_all(&body_data).is_err() {
                return transfer_uncompressed_fallback(
                    client_stream,
                    original_headers,
                    &body_data,
                ).await;
            }
            match encoder.finish() {
                Ok(data) => data,
                Err(_) => {
                    return transfer_uncompressed_fallback(
                        client_stream,
                        original_headers,
                        &body_data,
                    ).await;
                }
            }
        }
        AcceptedEncoding::Identity => {
            // 圧縮なし（ここには来ないはず）
            body_data
        }
    };

    // 3. 新しいヘッダーを構築
    let new_headers = build_compressed_headers(
        original_headers,
        encoding,
        compressed_body.len(),
        security,
    );
    
    // 4. ヘッダー送信（所有権を移動、clone 不要）
    let new_headers_len = new_headers.len();
    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(new_headers)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return (total, false);
    }
    total += new_headers_len as u64;

    // 5. 圧縮済みボディ送信（所有権を移動、clone 不要）
    let compressed_len = compressed_body.len();
    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(compressed_body)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return (total, false);
    }
    total += compressed_len as u64;

    (total, backend_wants_keep_alive)
}

/// compression feature 無効時のスタブ
#[cfg(not(feature = "compression"))]
async fn transfer_compressed_response(
    client_stream: &mut ServerTls,
    _backend_stream: &mut TcpStream,
    original_headers: &[u8],
    initial_body: &[u8],
    _content_length: Option<usize>,
    _is_chunked: bool,
    _encoding: AcceptedEncoding,
    _compression: &CompressionConfig,
    backend_wants_keep_alive: bool,
    _security: &SecurityConfig,
) -> (u64, bool) {
    transfer_uncompressed_fallback(client_stream, original_headers, initial_body).await
}

/// 圧縮失敗時のフォールバック（非圧縮で送信）
async fn transfer_uncompressed_fallback(
    client_stream: &mut ServerTls,
    original_headers: &[u8],
    body_data: &[u8],
) -> (u64, bool) {
    let mut total = 0u64;

    // ヘッダー送信（monoio は所有権を要求するため to_vec、clone は不要）
    let headers_len = original_headers.len();
    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(original_headers.to_vec())).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return (total, false);
    }
    total += headers_len as u64;

    // ボディ送信
    let body_len = body_data.len();
    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(body_data.to_vec())).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return (total, false);
    }
    total += body_len as u64;

    (total, true)
}


/// 圧縮用にヘッダーを書き換え
fn build_compressed_headers(
    original_headers: &[u8],
    encoding: AcceptedEncoding,
    compressed_length: usize,
    security: &SecurityConfig,
) -> Vec<u8> {
    let mut headers_storage = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers_storage);
    
    if response.parse(original_headers).is_err() {
        return original_headers.to_vec();
    }
    
    let status_code = response.code.unwrap_or(200);
    let reason = status_code_to_reason(status_code);
    
    let mut new_headers = Vec::with_capacity(original_headers.len() + 64);
    
    // ステータス行
    new_headers.extend_from_slice(b"HTTP/1.1 ");
    let mut code_buf = itoa::Buffer::new();
    new_headers.extend_from_slice(code_buf.format(status_code).as_bytes());
    new_headers.extend_from_slice(b" ");
    new_headers.extend_from_slice(reason.as_bytes());
    new_headers.extend_from_slice(b"\r\n");
    
    // 元のヘッダーをコピー（Content-Length, Content-Encoding, Transfer-Encoding を除く）
    // 削除対象のヘッダーも除外
    // アロケーションなしで大文字小文字無視ヘッダー比較
    for header in response.headers.iter() {
        let n = header.name.as_bytes();
        if n.eq_ignore_ascii_case(b"content-length")
            || n.eq_ignore_ascii_case(b"content-encoding")
            || n.eq_ignore_ascii_case(b"transfer-encoding")
            || security.remove_response_headers.iter().any(|h| n.eq_ignore_ascii_case(h.as_bytes()))
        {
            continue;
        }
        new_headers.extend_from_slice(header.name.as_bytes());
        new_headers.extend_from_slice(b": ");
        new_headers.extend_from_slice(header.value);
        new_headers.extend_from_slice(b"\r\n");
    }
    
    // Content-Encoding を追加
    new_headers.extend_from_slice(b"Content-Encoding: ");
    new_headers.extend_from_slice(encoding.as_header_value());
    new_headers.extend_from_slice(b"\r\n");
    
    // Content-Length を追加（圧縮後のサイズ）
    new_headers.extend_from_slice(b"Content-Length: ");
    let mut len_buf = itoa::Buffer::new();
    new_headers.extend_from_slice(len_buf.format(compressed_length).as_bytes());
    new_headers.extend_from_slice(b"\r\n");
    
    // Vary ヘッダーを追加（キャッシュ制御）
    new_headers.extend_from_slice(b"Vary: Accept-Encoding\r\n");
    
    // 追加するヘッダーを追加
    for (header_name, header_value) in &security.add_response_headers {
        new_headers.extend_from_slice(header_name.as_bytes());
        new_headers.extend_from_slice(b": ");
        new_headers.extend_from_slice(header_value.as_bytes());
        new_headers.extend_from_slice(b"\r\n");
    }
    
    // ヘッダー終端
    new_headers.extend_from_slice(b"\r\n");
    
    new_headers
}

/// レスポンスボディを転送（キャッシュキャプチャ対応版）
/// 
/// キャッシュコンテキストが指定されている場合、ボディをキャプチャしてキャッシュに保存します。
async fn transfer_response_body_with_cache(
    backend_stream: &mut TcpStream,
    client_stream: &mut ServerTls,
    content_length: Option<usize>,
    is_chunked: bool,
    initial_body: &[u8],
    mut cache_ctx: Option<&mut CacheSaveContext>,
) -> u64 {
    let mut total = 0u64;
    
    if let Some(cl) = content_length {
        let remaining = cl.saturating_sub(initial_body.len());
        if remaining > 0 {
            let transferred = transfer_exact_bytes_from_backend_with_cache(
                backend_stream, 
                client_stream, 
                remaining,
                cache_ctx,
            ).await;
            total += transferred;
        }
    } else if is_chunked {
        // Chunked 転送（キャッシュキャプチャ対応）
        let mut decoder = ChunkedDecoder::new_unlimited();
        decoder.feed(initial_body);
        
        if decoder.is_complete() {
            // 転送完了後にキャッシュに保存
            if let Some(ctx) = cache_ctx {
                ctx.save_to_cache();
            }
            return total;
        }
        
        loop {
            let read_buf = buf_get();
            let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
            
            let (res, mut returned_buf) = match read_result {
                Ok(result) => result,
                Err(_) => break,
            };
            
            let n = match res {
                Ok(0) => {
                    buf_put(returned_buf);
                    break;
                }
                Ok(n) => n,
                Err(_) => {
                    buf_put(returned_buf);
                    break;
                }
            };
            
            returned_buf.set_valid_len(n);
            let chunk = returned_buf.as_valid_slice();
            let feed_result = decoder.feed(chunk);
            
            // キャッシュコンテキストにボディをキャプチャ
            if let Some(ref mut ctx) = cache_ctx {
                ctx.append_body(chunk);
            }
            
            // クライアントに転送
            let chunk_data = chunk.to_vec();
            let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(chunk_data)).await;
            buf_put(returned_buf);
            
            if !matches!(write_result, Ok((Ok(_), _))) {
                break;
            }
            total += n as u64;
            
            if feed_result == ChunkedFeedResult::Complete {
                break;
            }
        }
        
        // 転送完了後にキャッシュに保存
        if let Some(ctx) = cache_ctx {
            ctx.save_to_cache();
        }
    }
    
    total
}

/// バックエンドから正確なバイト数を読み取りクライアントに転送（キャッシュキャプチャ対応版）
async fn transfer_exact_bytes_from_backend_with_cache(
    backend_stream: &mut TcpStream,
    client_stream: &mut ServerTls,
    mut remaining: usize,
    mut cache_ctx: Option<&mut CacheSaveContext>,
) -> u64 {
    let mut total = 0u64;
    
    while remaining > 0 {
        let read_buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
        
        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => break,
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                break;
            }
            Ok(n) => n.min(remaining),
            Err(_) => {
                buf_put(returned_buf);
                break;
            }
        };
        
        returned_buf.set_valid_len(n);
        let chunk = returned_buf.as_valid_slice();
        
        // キャッシュコンテキストにボディをキャプチャ
        if let Some(ref mut ctx) = cache_ctx {
            ctx.append_body(&chunk[..n]);
        }
        
        // クライアントに転送
        let chunk_data = chunk[..n].to_vec();
        let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(chunk_data)).await;
        buf_put(returned_buf);
        
        if !matches!(write_result, Ok((Ok(_), _))) {
            break;
        }
        
        total += n as u64;
        remaining = remaining.saturating_sub(n);
    }
    
    // 転送完了後にキャッシュに保存
    if let Some(ctx) = cache_ctx {
        ctx.save_to_cache();
    }
    
    total
}

// ====================
// kTLS + splice(2) によるHTTPプロキシ（高速版）
// ====================
//
// kTLS が有効な場合、splice(2) を使用してカーネル空間で直接
// データを転送します。HTTPバックエンド（平文）への接続で効果的です。
//
// 注意: Chunked 転送の場合は終端検出のためユーザー空間での
// 処理が必要なため、splice は使用しません。
// ====================

/// kTLS + splice によるボディ転送（Content-Length固定長のみ）
///
/// FD間でsplice(2)を使用してゼロコピー転送を行います。
/// 非ブロッキングソケットに対応し、WouldBlockの場合は待機します。
///
/// splice(2) によるボディ転送（固定長）
#[cfg(feature = "ktls")]
async fn splice_body_transfer(
    src_stream: &TcpStream,
    dst_stream: &TcpStream,
    pipe: &SplicePipe,
    mut remaining: usize,
) -> u64 {
    use std::os::unix::io::AsRawFd;
    
    let src_fd = src_stream.as_raw_fd();
    let dst_fd = dst_stream.as_raw_fd();
    let mut total = 0u64;
    
    // 設定に基づいてチャンクサイズを決定
    let chunk_size_config = {
        let config = CURRENT_CONFIG.load();
        match config.performance.chunk_size_mode {
            ChunkSizeMode::Dynamic => calculate_optimal_chunk_size(remaining as u64),
            ChunkSizeMode::Manual => config.performance.manual_chunk_size,
        }
    };
    
    while remaining > 0 {
        let chunk_size = remaining.min(chunk_size_config);
        
        match pipe.transfer(src_fd, dst_fd, chunk_size) {
            Ok(0) => break,
            Ok(n) => {
                total += n as u64;
                remaining = remaining.saturating_sub(n);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // 読み取り可能になるまで待機
                if let Err(_) = src_stream.readable(false).await {
                    break;
                }
            }
            Err(e) => {
                warn!("splice body transfer error: {}", e);
                break;
            }
        }
    }
    
    total
}

// ====================
// kTLS + splice(2) によるHTTPプロキシ転送
// ====================
//
// kTLS が有効な場合、以下のフローでゼロコピー転送を実現：
//
// [リクエスト] クライアント(kTLS) → splice → バックエンド(TCP)
//   1. ヘッダー: raw_read で読み取り → パース → raw_write で送信
//   2. ボディ(Content-Length): splice(2) でゼロコピー転送
//   3. ボディ(Chunked): 通常の転送（終端検出が必要）
//
// [レスポンス] バックエンド(TCP) → splice → クライアント(kTLS)
//   1. ヘッダー: raw_read で読み取り → パース → raw_write で送信
//   2. ボディ(Content-Length): splice(2) でゼロコピー転送
//   3. ボディ(Chunked): 通常の転送（終端検出が必要）
// ====================

/// kTLS + splice を使用したHTTPプロキシリクエスト処理
///
/// Content-Length が指定されている場合はボディ転送に splice を使用。
/// Chunked 転送の場合は通常の転送を使用。
#[cfg(feature = "ktls")]
async fn proxy_http_request_splice(
    client_stream: &KtlsServerStream,
    backend_stream: &TcpStream,
    request: &[u8],
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
) -> Option<(u16, u64, bool)> {
    // 設定に基づいてパイプを取得または作成
    let per_stream_pipe_enabled = {
        let config = CURRENT_CONFIG.load();
        config.performance.per_stream_pipe_enabled
    };
    
    // パイプ取得: ストリーム毎の新規パイプ or スレッドローカル再利用
    // 
    // 重要: 以下のunused_assignments警告は意図的に抑制しています。
    // 理由: Rustの所有権システムでは、参照が有効な間は元の値を保持する必要があります。
    //       `pipe` は借用参照であり、その参照元である `per_stream_pipe` または
    //       `thread_local_pipe_ref` が関数終了まで生存する必要があります。
    //       これらの変数への代入は「読み取られない」ように見えますが、
    //       実際にはライフタイム延長のために必須です。
    //
    // 参照: 実装評価レポート validation_report.md (2025-12-29)
    #[allow(unused_assignments)]
    let mut per_stream_pipe: Option<SplicePipe> = None;
    #[allow(unused_assignments)]
    let mut thread_local_pipe_ref = None;
    
    // #[allow(unused_assignments)] は以下のif-elseブロック内の代入にも適用
    // コンパイラはこれらを「読み取られない」と判断しますが、
    // 実際には所有権保持のために必須です
    let pipe: &SplicePipe = if per_stream_pipe_enabled {
        // ストリーム毎に新規パイプを作成（高並行性環境向け）
        match SplicePipe::new() {
            Ok(p) => {
                per_stream_pipe = Some(p);
                // thread_local_pipe_ref への代入は不要（per_stream_pipe が所有権を持つ）
                per_stream_pipe.as_ref().unwrap()
            }
            Err(e) => {
                warn!("Failed to create per-stream splice pipe: {}, falling back to thread-local", e);
                // フォールバック: スレッドローカルパイプを使用
                thread_local_pipe_ref = Some(get_splice_pipe());
                // per_stream_pipe への代入は不要（thread_local_pipe_ref が所有権を持つ）
                match thread_local_pipe_ref.as_ref().and_then(|r| r.as_ref()) {
                    Some(p) => p,
                    None => {
                        warn!("splice pipe not available, falling back to normal transfer");
                        return None;
                    }
                }
            }
        }
    } else {
        // スレッドローカルパイプを再利用（メモリ効率重視）
        // per_stream_pipe への代入は不要（thread_local_pipe_ref が所有権を持つ）
        thread_local_pipe_ref = Some(get_splice_pipe());
        match thread_local_pipe_ref.as_ref().and_then(|r| r.as_ref()) {
            Some(p) => p,
            None => {
                warn!("splice pipe not available, falling back to normal transfer");
                return None;
            }
        }
    };
    
    // kTLS が有効でない場合はフォールバック
    if !client_stream.is_ktls_enabled() {
        return None;
    }
    
    let client_tcp = client_stream.get_ref();
    
    // 1. リクエストヘッダーをバックエンドに送信（raw_write）
    if let Err(e) = async_raw_write_all(backend_stream, request).await {
        warn!("Failed to send request header: {}", e);
        return None;
    }
    
    // 2. 初期ボディがあれば送信
    if !initial_body.is_empty() {
        if let Err(e) = async_raw_write_all(backend_stream, initial_body).await {
            warn!("Failed to send initial body: {}", e);
            return None;
        }
    }
    
    // 3. 残りのリクエストボディを転送
    let remaining_body = content_length.saturating_sub(initial_body.len());
    if remaining_body > 0 {
        if is_chunked {
            // Chunked 転送はフォールバック（終端検出が必要）
            return None;
        }
        
        // Content-Length の場合: splice でゼロコピー転送
        // kTLS クライアント → バックエンド TCP
        let transferred = splice_body_transfer(
            client_tcp,
            backend_stream,
            pipe,
            remaining_body,
        ).await;
        
        if transferred < remaining_body as u64 {
            warn!("Request body transfer incomplete: {} < {}", transferred, remaining_body);
            return None;
        }
    }
    
    // 4. レスポンスを受信して転送（splice 使用）
    let result = splice_transfer_response_ktls(
        backend_stream,
        client_stream,
        pipe,
    ).await;
    
    Some(result)
}

/// kTLS + splice によるレスポンス転送
///
/// バックエンド(TCP) からヘッダーを読み取り、パースしてクライアント(kTLS)に送信。
/// ボディは Content-Length の場合は splice、Chunked の場合は通常転送。
#[cfg(feature = "ktls")]
async fn splice_transfer_response_ktls(
    backend_stream: &TcpStream,
    client_stream: &KtlsServerStream,
    pipe: &SplicePipe,
) -> (u16, u64, bool) {
    let client_tcp = client_stream.get_ref();
    
    let mut total = 0u64;
    let mut status_code = 502u16;
    let mut accumulated = Vec::with_capacity(4096);
    let mut backend_wants_keep_alive: bool;
    
    // ヘッダー読み取り用バッファ
    let mut header_buf = [0u8; 8192];
    
    // 1. ヘッダーを読み取り（raw_read + パース）
    loop {
        // バックエンドからヘッダーを読み取り
        let n = match async_raw_read(backend_stream, &mut header_buf).await {
            Ok(0) => {
                // EOF
                return (status_code, total, false);
            }
            Ok(n) => n,
            Err(e) => {
                warn!("Failed to read response header: {}", e);
                return (status_code, total, false);
            }
        };
        
        accumulated.extend_from_slice(&header_buf[..n]);
        
        // ヘッダーが完全に受信されたかチェック
        if let Some(parsed) = parse_http_response(&accumulated) {
            status_code = parsed.status_code;
            backend_wants_keep_alive = !parsed.is_connection_close;
            
            let header_len = parsed.header_len;
            let body_start_len = accumulated.len().saturating_sub(header_len);
            
            // ヘッダー + 初期ボディをクライアントに送信（raw_write）
            if let Err(e) = async_raw_write_all(client_tcp, &accumulated).await {
                warn!("Failed to send response header: {}", e);
                return (status_code, total, false);
            }
            total += accumulated.len() as u64;
            
            // ボディ転送
            if parsed.is_chunked {
                // Chunked 転送: 通常の方法で転送（終端検出が必要）
                // レスポンス受信時は制限なし（バックエンドを信頼）
                let mut chunked_decoder = ChunkedDecoder::new_unlimited();
                
                // 初期ボディ部分をデコーダにフィード
                if body_start_len > 0 {
                    if chunked_decoder.feed(&accumulated[header_len..]) == ChunkedFeedResult::Complete {
                        // 初期ボディで完了
                        return (status_code, total, backend_wants_keep_alive);
                    }
                }
                
                // 残りの Chunked ボディを転送
                loop {
                    let n = match async_raw_read(backend_stream, &mut header_buf).await {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => {
                            backend_wants_keep_alive = false;
                            break;
                        }
                    };
                    
                    let feed_result = chunked_decoder.feed(&header_buf[..n]);
                    
                    if let Err(_) = async_raw_write_all(client_tcp, &header_buf[..n]).await {
                        backend_wants_keep_alive = false;
                        break;
                    }
                    total += n as u64;
                    
                    if feed_result == ChunkedFeedResult::Complete {
                        break;
                    }
                }
            } else if let Some(content_length) = parsed.content_length {
                // Content-Length 転送: splice でゼロコピー
                let remaining = content_length.saturating_sub(body_start_len);
                
                if remaining > 0 {
                    let transferred = splice_body_transfer(
                        backend_stream,
                        client_tcp,
                        pipe,
                        remaining,
                    ).await;
                    
                    total += transferred;
                    
                    if transferred < remaining as u64 {
                        backend_wants_keep_alive = false;
                    }
                }
            } else {
                // Content-Length も Chunked もない場合: 接続クローズまで読み取り
                // この場合は Keep-Alive 不可
                backend_wants_keep_alive = false;
                
                loop {
                    let n = match async_raw_read(backend_stream, &mut header_buf).await {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    
                    if let Err(_) = async_raw_write_all(client_tcp, &header_buf[..n]).await {
                        break;
                    }
                    total += n as u64;
                }
            }
            
            return (status_code, total, backend_wants_keep_alive);
        }
        
        // ヘッダーが大きすぎる場合は中止
        if accumulated.len() > MAX_HEADER_SIZE {
            warn!("Response header too large");
            return (502, 0, false);
        }
    }
}

// ====================
// HTTPS プロキシ（コネクションプール対応）
// ====================

/// バックエンドへの新規 HTTPS（TCP+TLS）接続を確立する。
///
/// 成功時は確立済みの `ClientTls` を返す。失敗時は `(ステータスコード, クライアントへ
/// 返すエラーメッセージ)` を返す（接続エラー=502 / タイムアウト=504）。クライアントへの
/// 書き込みは呼び出し側が行う。
async fn connect_https_backend_fresh(
    target: &ProxyTarget,
    connect_timeout: Duration,
    tls_insecure: bool,
) -> Result<ClientTls, (u16, &'static [u8])> {
    let addr = format!("{}:{}", target.host, target.port);
    let backend_tcp = match timeout(connect_timeout, TcpStream::connect(&addr)).await {
        Ok(Ok(stream)) => {
            let _ = stream.set_nodelay(true);
            stream
        }
        Ok(Err(e)) => {
            error!("Proxy connect error to {}: {}", addr, e);
            return Err((502, ERR_MSG_BAD_GATEWAY));
        }
        Err(_) => {
            error!("Proxy connect timeout to {}", addr);
            return Err((504, ERR_MSG_GATEWAY_TIMEOUT));
        }
    };

    // TLS接続（タイムアウト付き）
    // SNI名を使用（sni_nameが設定されていればそれを使用、なければhostを使用）
    // tls_insecure が true の場合、証明書検証をスキップ
    let sni = target.sni();
    let tls_result = if tls_insecure {
        let connector = get_tls_connector_insecure();
        timeout(connect_timeout, connector.connect(backend_tcp, sni)).await
    } else {
        let connector = get_tls_connector();
        timeout(connect_timeout, connector.connect(backend_tcp, sni)).await
    };

    match tls_result {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(e)) => {
            error!("TLS connect error to {} (SNI: {}): {}", target.host, sni, e);
            Err((502, ERR_MSG_BAD_GATEWAY))
        }
        Err(_) => {
            error!("TLS connect timeout to {} (SNI: {})", target.host, sni);
            Err((504, ERR_MSG_GATEWAY_TIMEOUT))
        }
    }
}

async fn proxy_https_pooled(
    mut client_stream: ServerTls,
    target: &ProxyTarget,
    security: &SecurityConfig,
    compression: &CompressionConfig,
    buffering_config: &buffering::BufferingConfig,
    client_encoding: AcceptedEncoding,
    pool_key: &str,
    request: Vec<u8>,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
    client_wants_close: bool,
    tls_insecure: bool,
    wasm_modules: Vec<String>,
) -> Option<(ServerTls, u16, u64, bool)> {
    // セキュリティ設定からタイムアウトを取得
    let connect_timeout = Duration::from_secs(security.backend_connect_timeout_secs);
    // セキュリティ設定からchunked最大サイズを取得
    let max_chunked = security.max_chunked_body_size as u64;
    // バッファリングが有効かどうか判定
    let buffering_enabled = buffering_config.is_enabled() && buffering_config.should_buffer(Some(content_length));

    // プールから取り出した keep-alive 接続は、バックエンド側の idle タイムアウト等で
    // 既に閉じられていることがある。その場合バックエンドからの最初の read が即座に EOF を
    // 返し、クライアントへ何も送信されないまま 502 相当で終わってしまう（負荷時に顕在化する
    // 「応答が空になる」フレーキーの原因）。クライアントへ未送信であれば、新規接続で透過的に
    // 一度だけリトライする。
    //
    // リトライが安全なのは「リクエストボディがクライアントストリームから未読でない」場合に
    // 限る（ボディ全体が initial_body 内にあり、chunked でない）。ボディをストリーム転送する
    // リクエストは再送できないためリトライしない。
    let replayable = !is_chunked && content_length <= initial_body.len();
    // request / wasm_modules はリトライ時に再利用するため、リトライ可能な場合のみ複製を保持する。
    // （非リトライ要求では複製せず move するためホットパスに余分な割り当てを足さない）
    let mut request_holder = Some(request);

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        // 接続取得: 初回はプール優先。リトライ時は必ず新規接続。
        let pooled = if attempt == 1 {
            HTTPS_POOL.with(|p| p.borrow_mut().get(pool_key))
        } else {
            None
        };
        let (mut backend_stream, from_pool) = match pooled {
            Some(stream) => (stream, true),
            None => match connect_https_backend_fresh(target, connect_timeout, tls_insecure).await {
                Ok(stream) => (stream, false),
                Err((code, msg)) => {
                    let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(msg.to_vec())).await;
                    return Some((client_stream, code, 0, true));
                }
            },
        };

        // リトライ可能要求は複製を渡し（次の試行のため原本を保持）、それ以外は move する。
        let req = if replayable {
            request_holder.as_ref().map(|r| r.clone()).unwrap_or_default()
        } else {
            request_holder.take().unwrap_or_default()
        };
        // wasm_modules は通常空（割り当てなし）のため毎試行クローンしても実質コストは無い。
        let wasm_mods = wasm_modules.clone();

        let result = if buffering_enabled && (!compression.enabled || client_encoding == AcceptedEncoding::Identity) {
            let host_str_for_metrics = &target.host;
            record_buffering_used(&host_str_for_metrics);
            proxy_request_buffered(
                &mut client_stream,
                &mut backend_stream,
                req,
                content_length,
                is_chunked,
                initial_body,
                max_chunked,
                buffering_config,
                None,
                security,
            ).await
        } else {
            // リクエスト送信とレスポンス受信（圧縮対応）
            proxy_https_request_with_compression(
                &mut client_stream,
                &mut backend_stream,
                req,
                content_length,
                is_chunked,
                initial_body,
                max_chunked,
                compression,
                client_encoding,
                security,
                wasm_mods,
            ).await
        };

        match result {
            Some((status_code, total, backend_wants_keep_alive)) => {
                // プールから取り出した接続が応答前に死んでいた（total==0 かつ status は初期値 502 = レスポンス未受信）。
                // クライアントへ未送信のため、新規接続で一度だけ透過リトライ。死んだ接続はプールに戻さない。
                if from_pool && total == 0 && status_code == 502 && replayable && attempt < 2 {
                    continue;
                }
                // バックエンドがKeep-Aliveを許可している場合、プールに返却
                if backend_wants_keep_alive {
                    let max_idle = security.max_idle_connections_per_host;
                    let idle_timeout = security.idle_connection_timeout_secs;
                    HTTPS_POOL.with(|p| p.borrow_mut().put(pool_key.to_string(), backend_stream, max_idle, idle_timeout));
                }
                // 408 (body timeout) sends Connection: close — must actually close
                let should_close = client_wants_close || status_code == 408;
                return Some((client_stream, status_code, total, should_close));
            }
            None => {
                // プール接続でのエラーかつクライアントへ未送信なら新規接続でリトライ
                if from_pool && replayable && attempt < 2 {
                    continue;
                }
                // エラー発生時は接続を破棄
                let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                return Some((client_stream, 502, 0, true));
            }
        }
    }
}

/// HTTPSリクエストを送信してレスポンスを受信（圧縮対応版）
/// 戻り値: Option<(status_code, response_size, backend_wants_keep_alive)>
async fn proxy_https_request_with_compression(
    client_stream: &mut ServerTls,
    backend_stream: &mut ClientTls,
    request: Vec<u8>,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
    max_chunked_body_size: u64,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    security: &SecurityConfig,
    wasm_modules: Vec<String>,
) -> Option<(u16, u64, bool)> {
    // 1. リクエストヘッダー送信
    let write_result = timeout(WRITE_TIMEOUT, backend_stream.write_all(request)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return None;
    }

    // 2. リクエストボディ送信
    if !initial_body.is_empty() {
        let body_buf = initial_body.to_vec();
        let write_result = timeout(WRITE_TIMEOUT, backend_stream.write_all(body_buf)).await;
        if !matches!(write_result, Ok((Ok(_), _))) {
            return None;
        }
    }

    // 3. 残りのリクエストボディを転送
    if is_chunked {
        match transfer_chunked_body(client_stream, backend_stream, initial_body, max_chunked_body_size).await {
            ChunkedTransferResult::Complete => {}
            ChunkedTransferResult::Failed => return None,
            ChunkedTransferResult::SizeLimitExceeded => {
                return None;
            }
        }
    } else {
        let remaining = content_length.saturating_sub(initial_body.len());
        if remaining > 0 {
            let body_timeout = Duration::from_secs(security.client_body_timeout_secs);
            let timed_out = monoio::select! {
                _ = monoio::time::sleep(body_timeout) => {
                    true
                }
                transferred = transfer_exact_bytes(client_stream, backend_stream, remaining) => {
                    if transferred < remaining as u64 { return None; }
                    false
                }
            };
            if timed_out {
                let _ = client_stream.write_all(ERR_MSG_REQUEST_TIMEOUT.to_vec()).await;
                return Some((408, 0, false));
            }
        }
    }

    // 4. レスポンスを受信して転送（圧縮対応）
    let (total, status_code, backend_wants_keep_alive) =
        transfer_https_response_with_compression(backend_stream, client_stream, compression, client_encoding, security, wasm_modules).await;

    Some((status_code, total, backend_wants_keep_alive))
}

/// HTTPSレスポンス転送（圧縮対応版）
async fn transfer_https_response_with_compression(
    backend_stream: &mut ClientTls,
    client_stream: &mut ServerTls,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    security: &SecurityConfig,
    wasm_modules: Vec<String>,
) -> (u64, u16, bool) {
    let mut accumulated = Vec::with_capacity(BUF_SIZE);
    let mut total = 0u64;
    let mut status_code = 502u16;
    // 初期値false: エラー時はKeep-Aliveを無効化
    let mut backend_wants_keep_alive = false;

    // ヘッダー読み取り用バッファ
    loop {
        let read_buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
        
        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => {
                warn!("Backend response timeout while reading headers");
                return (total, status_code, backend_wants_keep_alive);
            }
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                warn!("Backend closed connection without sending response (read returned 0 bytes)");
                return (total, status_code, backend_wants_keep_alive);
            }
            Ok(n) => n,
            Err(e) => {
                buf_put(returned_buf);
                // kTLS使用時はEIO (os error 5) が発生することがある
                // これはバックエンドがTLS close_notifyを送信せずに接続を閉じた場合に発生
                if is_connection_closed_error(&e) {
                    debug!("Backend closed connection (kTLS EIO or connection reset): {}", e);
                } else {
                    warn!("Backend read error: {}", e);
                }
                return (total, status_code, backend_wants_keep_alive);
            }
        };
        
        returned_buf.set_valid_len(n);
        accumulated.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);
        
        // ヘッダーが完全に受信されたかチェック
        if let Some(parsed) = parse_http_response(&accumulated) {
            status_code = parsed.status_code;
            backend_wants_keep_alive = !parsed.is_connection_close;
            
            let header_len = parsed.header_len;
            let body_start = &accumulated[header_len..];
            
            // Content-Type と Content-Encoding を取得
            let content_type = extract_header_value(&accumulated[..header_len], b"content-type");
            let existing_encoding = extract_header_value(&accumulated[..header_len], b"content-encoding");
            
            // 圧縮すべきか判定
            let should_compress = compression.should_compress(
                client_encoding,
                content_type,
                parsed.content_length,
                existing_encoding,
            );
            
            if let Some(encoding) = should_compress {
                // 圧縮有効: ヘッダーを書き換えて圧縮転送
                let result = transfer_compressed_https_response(
                    client_stream,
                    backend_stream,
                    &accumulated[..header_len],
                    body_start,
                    parsed.content_length,
                    parsed.is_chunked,
                    encoding,
                    compression,
                    backend_wants_keep_alive,
                    security,
                ).await;
                
                return (result.0, status_code, result.1);
            } else {
                // 圧縮無効: そのまま転送（ヘッダー追加処理）
                let mut modified_headers = accumulated[..header_len].to_vec();
                
                // ヘッダーをパースして操作
                let mut headers_storage = [httparse::EMPTY_HEADER; 64];
                let mut response = httparse::Response::new(&mut headers_storage);
                if response.parse(&modified_headers).is_ok() {
                    let mut new_header_lines = Vec::new();
                    
                    // ステータス行を追加
                    let status_line = format!("HTTP/1.1 {} {}\r\n", 
                        status_code, 
                        status_code_to_reason(status_code));
                    new_header_lines.push(status_line.into_bytes());
                    
                    // 既存のヘッダーを追加（削除対象を除外）
                    // eq_ignore_ascii_case でアロケーションなしに比較
                    for header in response.headers.iter() {
                        if !security.remove_response_headers.iter()
                            .any(|h| header.name.as_bytes().eq_ignore_ascii_case(h.as_bytes()))
                        {
                            new_header_lines.push(format!("{}: {}\r\n",
                                header.name,
                                std::str::from_utf8(header.value).unwrap_or("")).into_bytes());
                        }
                    }
                    
                    // 追加するヘッダーを追加
                    for (header_name, header_value) in &security.add_response_headers {
                        new_header_lines.push(format!("{}: {}\r\n", header_name, header_value).into_bytes());
                    }

                    // WASMレスポンスヘッダーフィルタを適用
                    #[cfg(feature = "wasm")]
                    {
                        if !wasm_modules.is_empty() {
                            let config = CURRENT_CONFIG.load();
                            if let Some(ref wasm_engine) = config.wasm_filter_engine {
                                let current_headers: Vec<(Vec<u8>, Vec<u8>)> = new_header_lines.iter()
                                    .skip(1)
                                    .filter_map(|line| {
                                        let line_str = std::str::from_utf8(line).ok()?;
                                        let line_trimmed = line_str.trim_end_matches("\r\n");
                                        if line_trimmed.is_empty() { return None; }
                                        let colon_pos = line_trimmed.find(':')?;
                                        let name = line_trimmed[..colon_pos].as_bytes().to_vec();
                                        let value = line_trimmed[colon_pos+1..].trim_start().as_bytes().to_vec();
                                        Some((name, value))
                                    })
                                    .collect();

                                let wasm_result = wasm_engine.clone().on_response_headers_with_modules_async(
                                    wasm_modules.clone(),
                                    status_code,
                                    current_headers,
                                    true,
                                ).await;

                                match wasm_result {
                                    crate::wasm::FilterResult::Continue { headers: modified_headers_wasm, .. } => {
                                        new_header_lines.clear();
                                        let status_line = format!("HTTP/1.1 {} {}\r\n",
                                            status_code,
                                            status_code_to_reason(status_code));
                                        new_header_lines.push(status_line.into_bytes());
                                        for (name, value) in modified_headers_wasm {
                                            let mut line = Vec::with_capacity(name.len() + value.len() + 4);
                                            line.extend_from_slice(&name);
                                            line.extend_from_slice(b": ");
                                            line.extend_from_slice(&value);
                                            line.extend_from_slice(b"\r\n");
                                            new_header_lines.push(line);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }

                    // ヘッダー終了マーカーを追加
                    new_header_lines.push(b"\r\n".to_vec());

                    // 結合
                    modified_headers = new_header_lines.into_iter().flatten().collect();
                }

                let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(modified_headers)).await;
                if !matches!(write_result, Ok((Ok(_), _))) {
                    return (total, status_code, false);
                }
                total += header_len as u64;

                if !body_start.is_empty() {
                    let body_data = body_start.to_vec();
                    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(body_data)).await;
                    if !matches!(write_result, Ok((Ok(_), _))) {
                        return (total, status_code, false);
                    }
                    total += body_start.len() as u64;
                }
                
                // 残りのボディを転送
                let body_remaining = if let Some(cl) = parsed.content_length {
                    cl.saturating_sub(body_start.len())
                } else if parsed.is_chunked {
                    usize::MAX
                } else {
                    0
                };
                
                if body_remaining > 0 {
                    let transferred = transfer_https_response_body(
                        backend_stream,
                        client_stream,
                        parsed.content_length,
                        parsed.is_chunked,
                        body_start,
                    ).await;
                    total += transferred;
                }
                
                return (total, status_code, backend_wants_keep_alive);
            }
        }
        
        if accumulated.len() > MAX_HEADER_SIZE {
            return (0, 502, false);
        }
    }
}

/// 圧縮してHTTPSレスポンスを転送
#[cfg(feature = "compression")]
async fn transfer_compressed_https_response(
    client_stream: &mut ServerTls,
    backend_stream: &mut ClientTls,
    original_headers: &[u8],
    initial_body: &[u8],
    content_length: Option<usize>,
    is_chunked: bool,
    encoding: AcceptedEncoding,
    compression: &CompressionConfig,
    backend_wants_keep_alive: bool,
    security: &SecurityConfig,
) -> (u64, bool) {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    
    let mut total = 0u64;
    
    // 1. まず全てのボディデータを収集
    let mut body_data = initial_body.to_vec();
    
    if let Some(cl) = content_length {
        let remaining = cl.saturating_sub(initial_body.len());
        if remaining > 0 {
            let mut remaining_to_read = remaining;
            while remaining_to_read > 0 {
                let read_buf = buf_get();
                let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
                
                let (res, mut returned_buf) = match read_result {
                    Ok(result) => result,
                    Err(_) => {
                        return (total, false);
                    }
                };
                
                let n = match res {
                    Ok(0) => {
                        buf_put(returned_buf);
                        break;
                    }
                    Ok(n) => n.min(remaining_to_read),
                    Err(_) => {
                        buf_put(returned_buf);
                        return (total, false);
                    }
                };
                
                returned_buf.set_valid_len(n);
                body_data.extend_from_slice(returned_buf.as_valid_slice());
                buf_put(returned_buf);
                remaining_to_read = remaining_to_read.saturating_sub(n);
            }
        }
    } else if is_chunked {
        let mut decoder = ChunkedDecoder::new_unlimited();
        decoder.feed(initial_body);
        
        loop {
            let read_buf = buf_get();
            let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
            
            let (res, mut returned_buf) = match read_result {
                Ok(result) => result,
                Err(_) => {
                    // タイムアウト
                    break;
                }
            };
            
            let n = match res {
                Ok(0) => {
                    buf_put(returned_buf);
                    break;
                }
                Ok(n) => n,
                Err(_) => {
                    buf_put(returned_buf);
                    break;
                }
            };
            
            returned_buf.set_valid_len(n);
            let chunk = returned_buf.as_valid_slice();
            body_data.extend_from_slice(chunk);
            let feed_result = decoder.feed(chunk);
            buf_put(returned_buf);
            
            if feed_result == ChunkedFeedResult::Complete {
                break;
            }
        }
    }
    
    // 2. ボディを圧縮
    let compressed_body = match encoding {
        AcceptedEncoding::Zstd => {
            match zstd::encode_all(std::io::Cursor::new(&body_data), compression.zstd_level) {
                Ok(compressed) => compressed,
                Err(_) => {
                    return transfer_uncompressed_fallback(client_stream, original_headers, &body_data).await;
                }
            }
        }
        AcceptedEncoding::Gzip => {
            let level = Compression::new(compression.gzip_level);
            let mut encoder = GzEncoder::new(Vec::new(), level);
            if encoder.write_all(&body_data).is_err() {
                return transfer_uncompressed_fallback(client_stream, original_headers, &body_data).await;
            }
            match encoder.finish() {
                Ok(data) => data,
                Err(_) => {
                    return transfer_uncompressed_fallback(client_stream, original_headers, &body_data).await;
                }
            }
        }
        AcceptedEncoding::Brotli => {
            let mut compressed = Vec::new();
            let params = brotli::enc::BrotliEncoderParams {
                quality: compression.brotli_level as i32,
                ..Default::default()
            };
            let mut input = std::io::Cursor::new(&body_data);
            if brotli::BrotliCompress(&mut input, &mut compressed, &params).is_err() {
                return transfer_uncompressed_fallback(client_stream, original_headers, &body_data).await;
            }
            compressed
        }
        AcceptedEncoding::Deflate => {
            let level = Compression::new(compression.gzip_level);
            let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), level);
            if encoder.write_all(&body_data).is_err() {
                return transfer_uncompressed_fallback(client_stream, original_headers, &body_data).await;
            }
            match encoder.finish() {
                Ok(data) => data,
                Err(_) => {
                    return transfer_uncompressed_fallback(client_stream, original_headers, &body_data).await;
                }
            }
        }
        AcceptedEncoding::Identity => {
            body_data
        }
    };

    // 3. 新しいヘッダーを構築
    let new_headers = build_compressed_headers(original_headers, encoding, compressed_body.len(), security);

    // 4. ヘッダー送信（所有権を移動、clone 不要）
    let new_headers_len = new_headers.len();
    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(new_headers)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return (total, false);
    }
    total += new_headers_len as u64;

    // 5. 圧縮済みボディ送信（所有権を移動、clone 不要）
    let compressed_len = compressed_body.len();
    let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(compressed_body)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return (total, false);
    }
    total += compressed_len as u64;

    (total, backend_wants_keep_alive)
}

/// compression feature 無効時のスタブ
#[cfg(not(feature = "compression"))]
async fn transfer_compressed_https_response(
    client_stream: &mut ServerTls,
    _backend_stream: &mut ClientTls,
    original_headers: &[u8],
    initial_body: &[u8],
    _content_length: Option<usize>,
    _is_chunked: bool,
    _encoding: AcceptedEncoding,
    _compression: &CompressionConfig,
    backend_wants_keep_alive: bool,
    _security: &SecurityConfig,
) -> (u64, bool) {
    transfer_uncompressed_fallback(client_stream, original_headers, initial_body).await
}

/// HTTPSレスポンスボディを転送（圧縮なし）
async fn transfer_https_response_body(
    backend_stream: &mut ClientTls,
    client_stream: &mut ServerTls,
    content_length: Option<usize>,
    is_chunked: bool,
    initial_body: &[u8],
) -> u64 {
    let mut total = 0u64;
    
    if let Some(cl) = content_length {
        let remaining = cl.saturating_sub(initial_body.len());
        if remaining > 0 {
            let transferred = transfer_exact_bytes(backend_stream, client_stream, remaining).await;
            total += transferred;
        }
    } else if is_chunked {
        let mut decoder = ChunkedDecoder::new_unlimited();
        decoder.feed(initial_body);
        
        loop {
            let read_buf = buf_get();
            let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
            
            let (res, mut returned_buf) = match read_result {
                Ok(result) => result,
                Err(_) => break,
            };
            
            let n = match res {
                Ok(0) => {
                    buf_put(returned_buf);
                    break;
                }
                Ok(n) => n,
                Err(_) => {
                    buf_put(returned_buf);
                    break;
                }
            };
            
            returned_buf.set_valid_len(n);
            let chunk = returned_buf.as_valid_slice();
            let feed_result = decoder.feed(chunk);
            
            let chunk_data = chunk.to_vec();
            let write_result = timeout(WRITE_TIMEOUT, client_stream.write_all(chunk_data)).await;
            buf_put(returned_buf);
            
            if !matches!(write_result, Ok((Ok(_), _))) {
                break;
            }
            total += n as u64;
            
            if feed_result == ChunkedFeedResult::Complete {
                break;
            }
        }
    }
    
    total
}

// ====================
// 転送ヘルパー関数（ジェネリック版）
// ====================

/// 正確なバイト数を転送
async fn transfer_exact_bytes<R: AsyncReader, W: AsyncWriter>(
    reader: &mut R,
    writer: &mut W,
    mut remaining: usize,
) -> u64 {
    let mut total = 0u64;

    while remaining > 0 {
        let buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, reader.read_buf(buf)).await;
        
        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => return total,
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                break;
            }
            Ok(n) => n.min(remaining),
            Err(_) => {
                buf_put(returned_buf);
                break;
            }
        };
        
        // SafeReadBuffer の有効長を設定して書き込み用Vecに変換
        returned_buf.set_valid_len(n);
        let write_buf = returned_buf.into_truncated();
        
        let write_result = timeout(WRITE_TIMEOUT, writer.write_buf(write_buf)).await;
        match write_result {
            Ok((Ok(_), returned)) => {
                buf_put_vec(returned);
            }
            Ok((Err(_), returned)) => {
                buf_put_vec(returned);
                break;
            }
            Err(_) => break,
        }
        
        total += n as u64;
        remaining -= n;
    }
    
    total
}

// ====================
// kTLS + splice(2) によるゼロコピー転送（Linux 固有）
// ====================
//
// kTLS が有効な場合、splice(2) を使用してカーネル空間で直接
// データを転送します。これにより、ボディ転送時にユーザー空間への
// コピーが完全に不要になります。
//
// ## 実装状況
//
// - **ファイル送信（sendfile）**: kTLS有効時にゼロコピー対応 ✅
// - **プロキシ転送（splice）**: kTLS有効時 + Content-Length で対応 ✅
//
// ## プロキシ転送でのsplice使用
//
// libc::read/write を直接使用し、monoio の所有権ベース I/O を回避。
// 非同期待機は TcpStream::readable()/writable() を使用。
//
// ### 対応状況
// - Content-Length 転送: splice(2) でゼロコピー ✅
// - Chunked 転送: 通常転送（終端検出が必要なため）
//
// ## splice(2) の転送フロー
//
// [リクエスト] クライアント(kTLS) → splice → バックエンド(TCP)
//   1. ヘッダー: raw_read で読み取り → パース → raw_write で送信
//   2. ボディ: splice(2) でゼロコピー転送
//
// [レスポンス] バックエンド(TCP) → splice → クライアント(kTLS)
//   1. ヘッダー: raw_read で読み取り → パース → raw_write で送信
//   2. ボディ: splice(2) でゼロコピー転送
//
// 注意: splice(2) は少なくとも一方のFDがパイプである必要があります。
// ====================

/// Chunkedボディ転送の結果
#[derive(Debug, Clone, Copy, PartialEq)]
enum ChunkedTransferResult {
    /// 転送完了
    Complete,
    /// 転送失敗（I/Oエラー等）
    Failed,
    /// サイズ制限超過（DoS対策）
    SizeLimitExceeded,
}

/// Chunkedボディを転送（ステートマシンベース）
/// 
/// RFC 7230準拠のChunkedDecoderを使用して、トレーラーの有無に
/// かかわらず正確に終端を検出します。
/// 
/// DoS対策として、max_body_size を超えた場合は転送を中止します。
/// 
/// # Arguments
/// * `reader` - 読み取り元ストリーム
/// * `writer` - 書き込み先ストリーム
/// * `initial_body` - 初期ボディデータ（ヘッダー後に既に読み取り済みのデータ）
/// * `max_body_size` - 最大許容ボディサイズ（0の場合は制限なし）
async fn transfer_chunked_body<R: AsyncReader, W: AsyncWriter>(
    reader: &mut R,
    writer: &mut W,
    initial_body: &[u8],
    max_body_size: u64,
) -> ChunkedTransferResult {
    let mut decoder = ChunkedDecoder::new(max_body_size);
    
    // 初期ボディが既に終端を含んでいるかチェック
    if !initial_body.is_empty() {
        match decoder.feed(initial_body) {
            ChunkedFeedResult::Complete => return ChunkedTransferResult::Complete,
            ChunkedFeedResult::SizeLimitExceeded => return ChunkedTransferResult::SizeLimitExceeded,
            ChunkedFeedResult::Continue => {}
        }
    }
    
    loop {
        let buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, reader.read_buf(buf)).await;
        
        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => return ChunkedTransferResult::Failed,
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                break;
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                return ChunkedTransferResult::Failed;
            }
        };
        
        // SafeReadBuffer の有効長を設定
        returned_buf.set_valid_len(n);
        
        // ステートマシンにデータをフィード（型安全なアクセス）
        let feed_result = decoder.feed(returned_buf.as_valid_slice());
        
        // サイズ制限超過チェック
        if feed_result == ChunkedFeedResult::SizeLimitExceeded {
            buf_put(returned_buf);
            return ChunkedTransferResult::SizeLimitExceeded;
        }
        
        // バックエンドに転送（有効データのみを含むVecに変換）
        let write_buf = returned_buf.into_truncated();
        
        let write_result = timeout(WRITE_TIMEOUT, writer.write_buf(write_buf)).await;
        match write_result {
            Ok((Ok(_), returned)) => {
                buf_put_vec(returned);
            }
            Ok((Err(_), returned)) => {
                buf_put_vec(returned);
                return ChunkedTransferResult::Failed;
            }
            Err(_) => return ChunkedTransferResult::Failed,
        }
        
        // 終端チェック
        if feed_result == ChunkedFeedResult::Complete {
            return ChunkedTransferResult::Complete;
        }
    }
    
    ChunkedTransferResult::Failed
}

// ====================
// SendFile処理
// ====================
//
// kTLS + sendfile によるゼロコピー送信をサポートします。
//
// ## 通常の送信フロー（kTLS無効時）
//
// ファイル → ユーザー空間バッファ → TLS暗号化 → ネットワーク
// （2回のコピーが発生）
//
// ## ゼロコピー送信フロー（kTLS有効時）
//
// ファイル → カーネル空間でTLS暗号化 → NIC
// （ユーザー空間へのコピーなし）
//
// ### パフォーマンス効果
//
// - コンテキストスイッチの削減
// - メモリアクセスの削減（L3キャッシュミスの減少）
// - CPU使用率の低下（特に大きなファイル送信時）
//
// ### セキュリティ
//
// - ファイルの内容は変更されず、そのまま送信される
// - TLS暗号化はカーネル内で行われるため安全
// ====================

async fn handle_sendfile(
    mut tls_stream: ServerTls,
    base_path: &Path,
    is_dir: bool,
    index_filename: Option<&str>,
    req_path: &[u8],
    prefix: &[u8],
    client_wants_close: bool,
    security: &SecurityConfig,
    range_header: Option<&[u8]>,  // RFC 7233 Range header support
    open_file_cache_config: Option<&cache::OpenFileCacheConfig>,  // OpenFileCache設定（ルーティングごと）
    wasm_modules: Vec<String>,
) -> Option<(ServerTls, u16, u64, bool)> {
    // --- パス解決ロジック（Nginx風） ---
    // 
    // 1. ファイル指定（is_dir=false）: 完全一致のみ
    //    例: prefix="/robots.txt", path="./www/robots.txt"
    //    - リクエスト "/robots.txt" → OK（ファイルを返す）
    //    - リクエスト "/robots.txt/extra" → 404（ファイルの下には入れない）
    //
    // 2. ディレクトリ指定（is_dir=true）: プレフィックス除去後のパスを結合
    //    例: prefix="/static/", path="./www/assets/"
    //    - リクエスト "/static/css/style.css" → "./www/assets/css/style.css"
    //    - リクエスト "/static/" → "./www/assets/{index_filename}" (デフォルト: index.html)
    
    // Cow<str>を使用してパス処理のアロケーションを最小化
    let path_str_raw = std::str::from_utf8(req_path).unwrap_or("/");
    // クエリ文字列を除去してファイルパス解決に使用するパスのみを取り出す
    let path_str = if let Some(qpos) = path_str_raw.find('?') { &path_str_raw[..qpos] } else { path_str_raw };
    let prefix_str = std::str::from_utf8(prefix).unwrap_or("");

    // プレフィックスを除去して「残りパス」を取得（借用のみ、アロケーションなし）
    let remainder: &str = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
        &path_str[prefix_str.len()..]
    } else {
        path_str
    };
    
    // パストラバーサル防止（簡易チェック）
    if remainder.contains("..") {
        let err_buf = ERR_MSG_FORBIDDEN.to_vec();
        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
        return Some((tls_stream, 403, 0, true));
    }
    
    let full_path = if is_dir {
        // ケースA: ディレクトリへのルーティング（Alias動作）
        // config: path = "./www/static/"
        // req: /static/css/style.css → remainder: css/style.css
        // result: ./www/static/css/style.css
        let sub_path = remainder.trim_start_matches('/');
        let mut p = base_path.to_path_buf();
        if !sub_path.is_empty() {
            p.push(sub_path);
        }
        p
    } else {
        // ケースB: ファイルへの直接ルーティング（完全一致）
        // config: path = "./www/robots.txt"
        // req: /robots.txt → remainder: "" (OK)
        // req: /robots.txt/extra → remainder: "/extra" (NG → 404)
        
        let clean_remainder = remainder.trim_matches('/');
        if !clean_remainder.is_empty() {
            // ファイル指定なのにさらにパスが続いている場合は404
            let err_buf = ERR_MSG_NOT_FOUND.to_vec();
            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
            return Some((tls_stream, 404, 0, true));
        }
        base_path.to_path_buf()
    };

    // OpenFileCacheを使用してファイル情報を取得（キャッシュ優先）
    // これにより、canonicalize、metadata、mime_guessのシステムコールをキャッシュ
    // ルーティングごとの設定を考慮
    let file_info = match cache::get_file_info_with_config(&full_path, open_file_cache_config) {
        Some(info) => info,
        None => {
            let err_buf = ERR_MSG_NOT_FOUND.to_vec();
            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
            return Some((tls_stream, 404, 0, true));
        }
    };

    // ディレクトリの場合はセキュリティチェック
    if is_dir {
        // ベースパスのキャッシュ情報も取得（頻繁にアクセスされるため）
        if let Some(base_info) = cache::get_file_info_with_config(base_path, open_file_cache_config) {
            if !file_info.canonical_path.starts_with(&base_info.canonical_path) {
                let err_buf = ERR_MSG_FORBIDDEN.to_vec();
                let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                return Some((tls_stream, 403, 0, true));
            }
        }
    }

    // ディレクトリの場合はインデックスファイルを試す
    let (final_path, file_size, mime_type) = if !file_info.is_file {
        let filename = index_filename.unwrap_or("index.html");
        let index_path = file_info.canonical_path.join(filename);
        
        // インデックスファイルの情報をキャッシュから取得
        match cache::get_file_info_with_config(&index_path, open_file_cache_config) {
            Some(idx_info) if idx_info.is_file => {
                (idx_info.canonical_path.clone(), idx_info.file_size, idx_info.mime_type.clone())
            }
            _ => {
                // インデックスファイルが存在しない場合は403 Forbidden
                let err_buf = ERR_MSG_FORBIDDEN.to_vec();
                let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                return Some((tls_stream, 403, 0, true));
            }
        }
    } else {
        (file_info.canonical_path.clone(), file_info.file_size, file_info.mime_type.clone())
    };

    // ファイルを開く（非同期、実際のI/Oが必要）
    let file = match OpenOptions::new().read(true).open(&final_path).await {
        Ok(f) => f,
        Err(_) => {
            // ファイルが開けない場合はキャッシュを無効化
            cache::invalidate_file_cache(&full_path);
            let err_buf = ERR_MSG_NOT_FOUND.to_vec();
            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
            return Some((tls_stream, 404, 0, true));
        }
    };

    // キャッシュから取得したサイズとMIMEタイプを使用
    // （file.metadata()の呼び出しを省略）
    
    // RFC 7233 Range リクエスト処理
    let range_info: Option<(u64, u64)> = if let Some(range_bytes) = range_header {
        if let Some(parsed) = parse_range_header(range_bytes) {
            if let Some(ref first_range) = parsed.ranges.first() {
                match normalize_range(first_range, file_size) {
                    Some((start, end)) => Some((start, end)),
                    None => {
                        // 416 Range Not Satisfiable
                        let resp = build_range_not_satisfiable_response(file_size);
                        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(resp)).await;
                        return Some((tls_stream, 416, 0, true));
                    }
                }
            } else {
                None
            }
        } else {
            None // 不正なRange形式は無視して通常レスポンス
        }
    } else {
        None
    };
    
    // ヘッダー構築（Keep-Alive対応 + カスタムレスポンスヘッダー）
    let mut header_buf = Vec::with_capacity(HEADER_BUF_CAPACITY);
    
    // Range リクエストの場合は 206 Partial Content
    let (response_status, _response_content_length) = if let Some((start, end)) = range_info {
        let content_length = end - start + 1;
        header_buf.extend_from_slice(b"HTTP/1.1 206 Partial Content\r\nContent-Type: ");
        header_buf.extend_from_slice(mime_type.as_bytes());
        header_buf.extend_from_slice(b"\r\nAccept-Ranges: bytes\r\nContent-Range: bytes ");
        header_buf.extend_from_slice(start.to_string().as_bytes());
        header_buf.extend_from_slice(b"-");
        header_buf.extend_from_slice(end.to_string().as_bytes());
        header_buf.extend_from_slice(b"/");
        header_buf.extend_from_slice(file_size.to_string().as_bytes());
        header_buf.extend_from_slice(b"\r\nContent-Length: ");
        header_buf.extend_from_slice(content_length.to_string().as_bytes());
        header_buf.extend_from_slice(b"\r\n");
        (206u16, content_length)
    } else {
        // 通常のレスポンス
        header_buf.extend_from_slice(HTTP_200_PREFIX);
        header_buf.extend_from_slice(mime_type.as_bytes());
        header_buf.extend_from_slice(b"\r\nAccept-Ranges: bytes");  // Range サポートを通知
        header_buf.extend_from_slice(CONTENT_LENGTH_HEADER);
        let mut num_buf = itoa::Buffer::new();
        header_buf.extend_from_slice(num_buf.format(file_size).as_bytes());
        header_buf.extend_from_slice(b"\r\n");
        (200u16, file_size)
    };
    
    // 追加レスポンスヘッダー（セキュリティヘッダーなど）
    for (header_name, header_value) in &security.add_response_headers {
        header_buf.extend_from_slice(header_name.as_bytes());
        header_buf.extend_from_slice(b": ");
        header_buf.extend_from_slice(header_value.as_bytes());
        header_buf.extend_from_slice(b"\r\n");
    }
    
    // WASMレスポンスヘッダーフィルタを適用
    #[cfg(feature = "wasm")]
    let header_buf = {
        ftlog::info!("[WASM Response] SendFile: wasm_modules count = {}", wasm_modules.len());
        if !wasm_modules.is_empty() {
            let config = CURRENT_CONFIG.load();
            if let Some(ref wasm_engine) = config.wasm_filter_engine {
                // 現在のヘッダーをVec<(String, String)>形式に変換
                let header_str = String::from_utf8_lossy(&header_buf);
                let current_headers: Vec<(Vec<u8>, Vec<u8>)> = header_str.lines()
                    .skip(1) // ステータス行をスキップ
                    .filter_map(|line| {
                        let line_trimmed = line.trim_end_matches("\r\n").trim_end_matches("\r");
                        if line_trimmed.is_empty() {
                            return None;
                        }
                        let colon_pos = line_trimmed.find(':')?;
                        let name = line_trimmed[..colon_pos].as_bytes().to_vec();
                        let value = line_trimmed[colon_pos+1..].trim_start().as_bytes().to_vec();
                        Some((name, value))
                    })
                    .collect();

                // WASMフィルタを実行（レスポンスヘッダー処理）
                let wasm_result = wasm_engine.clone().on_response_headers_with_modules_async(
                    wasm_modules.clone(),
                    200,
                    current_headers,
                    true, // end_of_stream
                ).await;

                match wasm_result {
                    crate::wasm::FilterResult::Continue { headers: modified_headers, .. } => {
                        // WASMから修正されたヘッダーで再構築
                        let mut new_header = Vec::with_capacity(HEADER_BUF_CAPACITY);
                        new_header.extend_from_slice(HTTP_200_PREFIX);
                        new_header.extend_from_slice(mime_type.as_bytes());
                        new_header.extend_from_slice(CONTENT_LENGTH_HEADER);
                        let mut num_buf = itoa::Buffer::new();
                        new_header.extend_from_slice(num_buf.format(file_size).as_bytes());
                        new_header.extend_from_slice(b"\r\n");

                        // WASMから返されたヘッダーを追加
                        for (name, value) in modified_headers {
                            new_header.extend_from_slice(&name);
                            new_header.extend_from_slice(b": ");
                            new_header.extend_from_slice(&value);
                            new_header.extend_from_slice(b"\r\n");
                        }
                        new_header
                    }
                    _ => header_buf,
                }
            } else {
                header_buf
            }
        } else {
            header_buf
        }
    };

    // Connection ヘッダーを追加（headerをmutableにする）
    let mut header_buf = header_buf;
    if client_wants_close {
        header_buf.extend_from_slice(b"Connection: close\r\n\r\n");
    } else {
        header_buf.extend_from_slice(b"Connection: keep-alive\r\n\r\n");
    }

    // ヘッダー送信（タイムアウト付き）
    let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(header_buf)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return None;
    }

    // ファイル転送
    // Range リクエストの場合はオフセットと長さを調整
    let (transfer_offset, transfer_length) = if let Some((start, end)) = range_info {
        (start as i64, end - start + 1)
    } else {
        (0i64, file_size)
    };
    
    // kTLS が有効な場合は sendfile によるゼロコピー送信を使用
    #[cfg(feature = "ktls")]
    {
        if tls_stream.is_ktls_send_enabled() {
            return handle_sendfile_zerocopy(tls_stream, &file, transfer_offset, transfer_length, client_wants_close, response_status).await;
        }
    }
    
    // kTLS が無効な場合は従来の read/write を使用
    handle_sendfile_userspace(tls_stream, &file, transfer_offset, transfer_length, client_wants_close, response_status).await
}

/// kTLS + sendfile によるゼロコピーファイル送信
///
/// kTLS が有効な場合に使用されます。
/// ファイルの内容をカーネル空間で直接 TLS 暗号化して送信します。
#[cfg(feature = "ktls")]
async fn handle_sendfile_zerocopy(
    tls_stream: ServerTls,
    file: &monoio::fs::File,
    transfer_offset: i64,
    transfer_length: u64,
    client_wants_close: bool,
    response_status: u16,
) -> Option<(ServerTls, u16, u64, bool)> {
    use std::os::unix::io::AsRawFd;
    
    let file_fd = file.as_raw_fd();
    let mut offset: i64 = transfer_offset;
    let target_end = transfer_offset + transfer_length as i64;
    let mut total_sent = 0u64;
    
    // sendfile を使用してファイルをゼロコピー送信
    // 設定に基づいてチャンクサイズを決定
    let chunk_size_config = {
        let config = CURRENT_CONFIG.load();
        match config.performance.chunk_size_mode {
            ChunkSizeMode::Dynamic => calculate_optimal_chunk_size(transfer_length),
            ChunkSizeMode::Manual => config.performance.manual_chunk_size,
        }
    };
    
    while offset < target_end {
        let remaining = (target_end - offset) as u64;
        let chunk_size = (remaining as usize).min(chunk_size_config);
        
        // sendfile 実行
        match tls_stream.sendfile(file_fd, &mut offset, chunk_size) {
            Ok(0) => {
                // EOF
                break;
            }
            Ok(n) => {
                total_sent += n as u64;
            }
            Err(e) => {
                // EAGAIN/EWOULDBLOCK の場合は再試行（非同期ソケットの場合）
                if e.kind() == io::ErrorKind::WouldBlock {
                    // writable を待ってから再試行
                    if let Err(_) = tls_stream.get_ref().writable(false).await {
                        break;
                    }
                    continue;
                }
                error!("sendfile error: {}", e);
                break;
            }
        }
    }

    Some((tls_stream, response_status, total_sent, client_wants_close))
}

/// 従来の read/write によるファイル送信（ユーザー空間経由）
///
/// kTLS が無効な場合、または rustls 使用時に使用されます。
async fn handle_sendfile_userspace(
    mut tls_stream: ServerTls,
    file: &monoio::fs::File,
    transfer_offset: i64,
    transfer_length: u64,
    client_wants_close: bool,
    response_status: u16,
) -> Option<(ServerTls, u16, u64, bool)> {
    let mut total_sent = 0u64;
    let mut offset = transfer_offset as u64;
    let target_end = transfer_offset as u64 + transfer_length;
    
    while offset < target_end {
        let read_buf = buf_get();
        let (res, mut returned_buf) = file.read_at(read_buf, offset).await;
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                break;
            }
            Ok(n) => {
                // Range リクエストの場合、読み取りサイズを制限
                let remaining = (target_end - offset) as usize;
                n.min(remaining)
            }
            Err(e) => {
                buf_put(returned_buf);
                error!("File read error: {}", e);
                break;
            }
        };
        
        // SafeReadBuffer の有効長を設定して書き込み用Vecに変換
        returned_buf.set_valid_len(n);
        let write_buf = returned_buf.into_truncated();
        
        let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(write_buf)).await;
        match write_result {
            Ok((Ok(_), returned)) => {
                buf_put_vec(returned);
                total_sent += n as u64;
                offset += n as u64;
            }
            Ok((Err(_), returned)) => {
                buf_put_vec(returned);
                break;
            }
            Err(_) => break,
        }
    }

    Some((tls_stream, response_status, total_sent, client_wants_close))
}

// ====================
// ロギング
// ====================

// log_access は crate::logging モジュールに移動しました。

// ====================
// ユニットテスト
// ====================

