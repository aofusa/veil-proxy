//! プロキシ処理モジュール
//!
//! すべてのプロキシハンドリングロジックを担当します。
//! HTTP/1.1、HTTP/2、WebSocket、ファイル配信などに対応。

use std::sync::Arc;

use crate::runtime::buf::{IoBuf, IoBufMut};
use crate::runtime::io::OpenOptions;
use crate::runtime::io::{AsyncReadRent, AsyncWriteRentExt, IoVecBuf, IoVecBufMut};
use crate::runtime::tcp::TcpStream;
use crate::runtime::time::timeout;
#[cfg(feature = "http2")]
use bytes::Bytes;
use ftlog::{debug, error, info, warn};
use httparse::{Request, Status};
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::buffering;
use crate::cache;
use crate::config::*;
use crate::constants::*;
use crate::http_utils::*;
use crate::logging::*;
use crate::metrics::*;
use crate::pool::*;
use crate::upstream::*;
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};

use crate::server::spawn_background_revalidation;

#[cfg(feature = "http2")]
use crate::http2;
#[cfg(feature = "ktls")]
use crate::ktls_rustls::{KtlsServerStream, RustlsAcceptor, SplicePipe};
#[cfg(not(feature = "ktls"))]
use crate::simple_tls;

// ServerTls型エイリアス（main.rsから再エクスポート）
#[cfg(feature = "ktls")]
use crate::ktls_rustls::KtlsServerStream as ServerTls;
#[cfg(not(feature = "ktls"))]
use crate::simple_tls::SimpleTlsServerStream as ServerTls;

// ====================
// 接続処理
// ====================

/// HTTPS コネクションプールキー（`tls_insecure` 設定毎に分離しプール汚染を防ぐ）
#[inline]
fn https_pool_key(host: &str, port: u16, sni: &str, tls_insecure: bool) -> String {
    let tag = if tls_insecure { "insecure" } else { "verify" };
    format!("{}:{}:{}:{}", host, port, sni, tag)
}

/// HTTPS コネクションプールキー（SNI なし）
#[inline]
fn https_pool_key_no_sni(host: &str, port: u16, tls_insecure: bool) -> String {
    let tag = if tls_insecure { "insecure" } else { "verify" };
    format!("{}:{}:{}", host, port, tag)
}

/// プロキシ起動時刻（F-21: 管理API /stats 用）
#[cfg(feature = "admin")]
static PROXY_START_TIME: once_cell::sync::Lazy<std::time::Instant> =
    once_cell::sync::Lazy::new(std::time::Instant::now);

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
/// 管理 API: 設定情報をJSON形式で返す（F-21: GET /__admin/config）
///
/// secret フィールドは "***" にマスクする。
#[cfg(feature = "admin")]
fn build_admin_config_json(config: &crate::config::RuntimeConfig) -> String {
    let admin = &config.admin_config;
    let prom = &config.prometheus_config;

    // secret をマスク
    let secret_masked = if admin.secret.is_empty() { "" } else { "***" };

    // allowed_ips を JSON 配列に変換
    let allowed_ips_json = {
        let mut s = String::from("[");
        for (i, ip) in admin.allowed_ips.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push('"');
            s.push_str(ip);
            s.push('"');
        }
        s.push(']');
        s
    };

    format!(
        "{{\"admin\":{{\"enabled\":{},\"path_prefix\":\"{}\",\"secret\":\"{}\",\"allowed_ips\":{}}},\"prometheus\":{{\"enabled\":{},\"path\":\"{}\"}}}}",
        admin.enabled,
        admin.path_prefix,
        secret_masked,
        allowed_ips_json,
        prom.enabled,
        prom.path,
    )
}

/// 管理 API: キャッシュ Purge リクエストを処理する（F-20）
///
/// クエリパラメータをパースし、キャッシュマネージャーの purge メソッドを呼ぶ。
/// 認証は呼び出し側で済ませておくこと。
///
/// 対応パラメータ:
/// - `?key=<url-encoded-path>` : 完全一致 Purge（PURGE メソッド時は path 自体）
/// - `?prefix=/api/`           : プレフィックス Purge
/// - `?pattern=/static/*.css`  : glob パターン Purge
/// - `?all=true`               : 全 Purge
///
/// PURGE メソッドの場合は、リクエストパス自体をプレフィックス Purge 対象にする。
///
/// # Returns
/// HTTP/1.1 レスポンス（バイト列）
#[cfg(feature = "admin")]
fn handle_cache_purge(path_with_query: &str, is_purge_method: bool) -> Vec<u8> {
    // クエリ文字列を分離
    let (path_part, query) = match path_with_query.split_once('?') {
        Some((p, q)) => (p, q),
        None => (path_with_query, ""),
    };

    // クエリパラメータを解析
    let mut key_param: Option<String> = None;
    let mut prefix_param: Option<String> = None;
    let mut pattern_param: Option<String> = None;
    let mut all_param = false;
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "key" => key_param = Some(url_decode(v)),
            "prefix" => prefix_param = Some(url_decode(v)),
            "pattern" => pattern_param = Some(url_decode(v)),
            "all" => all_param = v.eq_ignore_ascii_case("true") || v == "1",
            _ => {}
        }
    }

    #[cfg(feature = "cache")]
    {
        let manager = match cache::get_global_cache() {
            Some(m) => m,
            None => {
                return b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
            }
        };

        let purged: usize = if all_param {
            manager.purge_all()
        } else if let Some(prefix) = prefix_param {
            manager.purge_by_prefix(&prefix)
        } else if let Some(pattern) = pattern_param {
            manager.purge_by_pattern(&pattern)
        } else if let Some(key) = key_param {
            // key はパス（プレフィックス一致として扱う）
            manager.purge_by_prefix(&key)
        } else if is_purge_method {
            // PURGE メソッド: リクエストパスをプレフィックス Purge
            manager.purge_by_prefix(path_part)
        } else {
            // パラメータ不足
            return b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec();
        };

        let body = format!("{{\"purged\":{}}}", purged);
        let mut response = Vec::with_capacity(128 + body.len());
        response.extend_from_slice(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: ",
        );
        let mut num_buf = itoa::Buffer::new();
        response.extend_from_slice(num_buf.format(body.len()).as_bytes());
        response.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
        response.extend_from_slice(body.as_bytes());
        response
    }

    #[cfg(not(feature = "cache"))]
    {
        let _ = (
            path_part,
            key_param,
            prefix_param,
            pattern_param,
            all_param,
            is_purge_method,
        );
        b"HTTP/1.1 501 Not Implemented\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
    }
}

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
        let is_allowed = security
            .allowed_methods
            .iter()
            .any(|m| m.eq_ignore_ascii_case(method_str));
        if !is_allowed {
            return SecurityCheckResult::MethodNotAllowed;
        }
    }

    // レートリミットチェック
    if security.rate_limit_requests_per_min > 0
        && !check_rate_limit(client_ip, security.rate_limit_requests_per_min)
    {
        return SecurityCheckResult::RateLimitExceeded;
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

/// 上流へ転送するリクエストパスを構築する。
///
/// - `preserve_full_path = true`（gRPC）: ルート `/*` プレフィックスを除去しない。
///   `/grpc.Service/*` マッチで `/UnaryCall` だけ残すと上流が UNIMPLEMENTED になる（B-39/B-40）。
/// - それ以外: `prefix` を剥がし `target_path_prefix` を前置（従来どおり）。
#[inline]
fn compute_upstream_path(
    path_str: &str,
    prefix: &[u8],
    target_path_prefix: &str,
    preserve_full_path: bool,
) -> String {
    if preserve_full_path {
        return if path_str.is_empty() {
            "/".to_string()
        } else {
            path_str.to_string()
        };
    }
    let sub_path = if prefix.is_empty() {
        path_str.to_string()
    } else {
        let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
        if let Some(remaining) = path_str.strip_prefix(prefix_str) {
            let base = target_path_prefix.trim_end_matches('/');
            build_sub_path(base, remaining)
        } else {
            path_str.to_string()
        }
    };
    if sub_path.is_empty() {
        "/".to_string()
    } else {
        sub_path
    }
}

/// Content-Type が application/grpc* かどうか（ホップバイホップ転送前の判定用）。
/// 注意: `application/grpc-web*` も prefix 一致で true（gRPC-Web 経路でも使う）。
#[inline]
fn header_pair_is_grpc(name: &[u8], value: &[u8]) -> bool {
    name.eq_ignore_ascii_case(b"content-type") && value.starts_with(b"application/grpc")
}

/// ネイティブ gRPC Content-Type（`application/grpc` / `+proto` 等）かどうか。
/// gRPC-Web（`application/grpc-web*`）は除外する（F-112）。
///
/// ホットパス外: 単体テスト・fuzz_api から利用。本番 HTTP/1.1 経路は H2C ブリッジのため
/// 現時点では拒否ポリシーに配線しない（将来切替用の純関数）。
#[inline]
pub(crate) fn is_native_grpc_content_type(value: &[u8]) -> bool {
    // "application/grpc" = 16 bytes。それ未満は不一致。
    if value.len() < 16 {
        return false;
    }
    if !value[..16].eq_ignore_ascii_case(b"application/grpc") {
        return false;
    }
    // "application/grpc-web..." を除外（16 文字目以降が "-web" で始まる）
    if value.len() >= 20 && value[16..20].eq_ignore_ascii_case(b"-web") {
        return false;
    }
    // 残りは境界: 終端 / `;` / `+` のみ許可（誤マッチ防止）
    if value.len() == 16 {
        return true;
    }
    matches!(value[16], b';' | b'+' | b' ' | b'\t')
}

/// 既に組み立てた HTTP リクエストバイト列に `Content-Type: application/grpc` が含まれるか。
/// ホットパス: ヘッダ終端までの線形スキャンのみ（アロケーションなし）。
fn request_bytes_indicate_grpc(request: &[u8]) -> bool {
    let head_end = request
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(request.len());
    let head = &request[..head_end];
    let mut i = 0;
    while i + 13 <= head.len() {
        if head[i..i + 13].eq_ignore_ascii_case(b"content-type:") {
            let rest = &head[i + 13..];
            let line_end = rest
                .iter()
                .position(|&b| b == b'\r' || b == b'\n')
                .unwrap_or(rest.len());
            let value = rest[..line_end].trim_ascii_start();
            if value.len() >= 16 && value[..16].eq_ignore_ascii_case(b"application/grpc") {
                return true;
            }
        }
        i += 1;
    }
    false
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
async fn handle_http2_connection<S>(tls_stream: S, client_ip: &str)
where
    S: crate::runtime::io::AsyncReadRent
        + crate::runtime::io::AsyncWriteRentExt
        + AsRawFd
        + crate::runtime::io::BufferedReadState
        + Unpin,
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

/// HTTP/2 メインループ（F-116: ストリーム多重化アクターモデル）
///
/// コネクション（`Http2Connection`）は本メインループが専有駆動し、リクエストごとの
/// ルーティング・セキュリティ・WASM・バックエンド往復は per-stream タスク
/// （[`h2_request_task`]）へ切り出す。タスクは conn に一切触れず、レスポンスは
/// [`H2RespMsg`] チャネルで、リクエストボディは [`Bytes`] チャネルでメインループと通信する。
/// これにより遅いバックエンドのストリームが他ストリームの応答送出をブロックしない。
#[cfg(feature = "http2")]
async fn handle_http2_requests<S>(
    conn: &mut http2::Http2Connection<S>,
    client_ip: &str,
    connection_metric: &mut ActiveConnectionMetric,
) -> Result<(), http2::Http2Error>
where
    S: crate::runtime::io::AsyncReadRent
        + crate::runtime::io::AsyncWriteRentExt
        + AsRawFd
        + crate::runtime::io::BufferedReadState
        + Unpin,
{
    use http2::frame::Frame;
    use http2::Http2Error;

    let notify = crate::stream_channel::Notify::new();
    let spawner = h2_task_spawner();
    let mut streams: std::collections::HashMap<u32, H2ActiveStream> =
        std::collections::HashMap::new();

    loop {
        // 1. 各ストリームのレスポンスを送出（HPACK エンコードは送信順にここでのみ行う）。
        drive_h2_streams(conn, &mut streams).await?;
        // 2. 1 イテレーション 1 回のフラッシュ（複数ストリームの write_buf 合流を 1 回で送出）。
        conn.flush_write_buf().await?;

        // 3. 読み込みバッファに溜まった完全フレームを I/O なしで連続処理。
        let mut processed_any = false;
        loop {
            let frame = match conn.try_read_frame_buffered() {
                Ok(Some(f)) => {
                    processed_any = true;
                    f
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = conn
                        .send_goaway(e.error_code(), e.to_string().as_bytes())
                        .await;
                    return Err(e);
                }
            };

            // リクエスト方向ストリーミング中のストリームへの DATA はタスクの req チャネルへ流す。
            if let Frame::Data {
                stream_id,
                end_stream,
                data,
            } = frame
            {
                let is_streaming = streams
                    .get(&stream_id)
                    .map(|s| s.req_streaming && !s.req_eof)
                    .unwrap_or(false);
                if is_streaming {
                    let data_len = data.len();
                    // フロー制御・状態遷移・content-length 検証（recv ウィンドウの補充は
                    // しない。補充は drive が req チャネルへ転送できた消費連動で行う）。
                    if let Err(e) = conn.recv_data_for_streaming(stream_id, end_stream, data_len) {
                        if e.should_goaway() {
                            let _ = conn
                                .send_goaway(e.error_code(), e.to_string().as_bytes())
                                .await;
                            return Err(e);
                        } else if let Some(id) = e.rst_stream_id() {
                            let _ = conn.send_rst_stream(id, e.error_code()).await;
                        }
                        streams.remove(&stream_id);
                        // 未転送分のクレジットが conn レベルに残らないよう補充する。
                        conn.replenish_recv_window(stream_id);
                        // 直後の process_frame が write_all（write_buf 空の不変条件）を使うため、
                        // 積んだ WINDOW_UPDATE を先に送出する。
                        conn.flush_write_buf().await?;
                        continue;
                    }
                    if let Some(st) = streams.get_mut(&stream_id) {
                        st.req_bytes_total = st.req_bytes_total.saturating_add(data_len as u64);
                        if st.max_request_body > 0 && st.req_bytes_total > st.max_request_body {
                            // ボディ上限超過: RST_STREAM で打ち切りタスクを停止（チャネル drop）。
                            conn.flush_write_buf().await?;
                            let _ = conn
                                .send_rst_stream(stream_id, http2::Http2ErrorCode::EnhanceYourCalm)
                                .await;
                            streams.remove(&stream_id);
                            // 打ち切りで消費されないクレジットを conn レベルへ返す。
                            conn.replenish_recv_window(stream_id);
                            // 直後の process_frame が write_all（write_buf 空の不変条件）を使うため、
                            // 積んだ WINDOW_UPDATE を先に送出する。
                            conn.flush_write_buf().await?;
                            continue;
                        }
                        if data_len > 0 {
                            // ゼロコピー: 受信フレームの所有バッファをそのまま Bytes 化して転送。
                            st.req_pending.push_back(Bytes::from(data));
                        }
                        if end_stream {
                            st.req_eof = true;
                        }
                    }
                    continue;
                }

                // 通常（バッファ）経路の DATA は process_frame が request_body へ蓄積する。
                match conn
                    .process_frame(Frame::Data {
                        stream_id,
                        end_stream,
                        data,
                    })
                    .await
                {
                    Ok(Some(req)) => {
                        h2_spawn_for_request(
                            conn,
                            req.stream_id,
                            req.body_pending,
                            &mut streams,
                            &notify,
                            &spawner,
                            client_ip,
                            connection_metric,
                        );
                    }
                    Ok(None) => {}
                    Err(e) => {
                        if e.should_goaway() {
                            let _ = conn
                                .send_goaway(e.error_code(), e.to_string().as_bytes())
                                .await;
                            return Err(e);
                        } else if let Some(id) = e.rst_stream_id() {
                            let _ = conn.send_rst_stream(id, e.error_code()).await;
                            streams.remove(&id);
                        }
                    }
                }
                continue;
            }

            // RST_STREAM 受信ストリームは streams から除去（チャネル drop でタスク停止）。
            let rst_target = if let Frame::RstStream { stream_id, .. } = &frame {
                Some(*stream_id)
            } else {
                None
            };

            match conn.process_frame(frame).await {
                Ok(Some(req)) => {
                    if let Some(st) = streams.get_mut(&req.stream_id) {
                        // 既にタスク起動済みのストリーム: リクエスト方向ストリーミング中に
                        // トレイラー（END_STREAM 付き HEADERS / CONTINUATION 完了）で終端した
                        // 場合、process_frame は body_pending=false の ProcessedRequest を返す。
                        // 新規タスクは起動せず、req チャネルの EOF（sender drop）へ変換する
                        // （さもないとバックエンドが終端チャンクを受け取れずハングする）。
                        if st.req_streaming && !req.body_pending {
                            st.req_eof = true;
                        }
                    } else {
                        h2_spawn_for_request(
                            conn,
                            req.stream_id,
                            req.body_pending,
                            &mut streams,
                            &notify,
                            &spawner,
                            client_ip,
                            connection_metric,
                        );
                    }
                }
                Ok(None) => {
                    if let Some(id) = rst_target {
                        if let Some(st) = streams.remove(&id) {
                            // ストリーミング中の打ち切りは未転送クレジットを conn レベルへ返す。
                            if st.req_streaming {
                                conn.replenish_recv_window(id);
                                // 直後の process_frame が write_all（write_buf 空の不変条件）を
                                // 使うため、積んだ WINDOW_UPDATE を先に送出する。
                                conn.flush_write_buf().await?;
                            }
                        }
                    }
                }
                Err(e) => {
                    if e.should_goaway() {
                        let _ = conn
                            .send_goaway(e.error_code(), e.to_string().as_bytes())
                            .await;
                        return Err(e);
                    } else if let Some(id) = e.rst_stream_id() {
                        let _ = conn.send_rst_stream(id, e.error_code()).await;
                        if let Some(st) = streams.remove(&id) {
                            if st.req_streaming {
                                conn.replenish_recv_window(id);
                                // 直後の process_frame が write_all（write_buf 空の不変条件）を
                                // 使うため、積んだ WINDOW_UPDATE を先に送出する。
                                conn.flush_write_buf().await?;
                            }
                        }
                    }
                }
            }
        }

        conn.cleanup_closed();

        // 4. 入力取得 / 待機。
        //
        // (a) TLS 復号済み平文がストリーム内部に滞留している場合は POLLIN では通知されないため、
        //     待機せず `fill_read_buf` で能動的に読み出して再ループする（さもないと既読データが
        //     残ったまま POLLIN を待ってデッドロック / CPU スピンする）。
        // (b) 本周でフレームを処理した場合（WINDOW_UPDATE で送信ウィンドウが回復した等）は待機せず
        //     drive へ戻り、回復したウィンドウで pending_body を直ちに送出する。
        // (c) それ以外（idle / `read_buf` 内の分割フレーム待ち）は `select2` で待機する。分割フレーム
        //     の続きは POLLIN で発火し、タスク出力は notify で発火する（Notify は取りこぼさない）。
        let need_fill = if conn.has_stream_buffered_read_data() {
            true
        } else if processed_any {
            // 進捗ありだが追加入力はまだ。drive へ戻る（fill しない）。
            continue;
        } else {
            // idle: 可読 or notify を待つ。可読なら fill する。
            h2_select_readable_or_notify(conn.raw_fd(), &notify).await
        };

        if need_fill {
            match conn.fill_read_buf().await {
                Ok(_) => {}
                Err(Http2Error::ConnectionClosed) => break,
                Err(Http2Error::Io(ref e)) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(Http2Error::Io(ref e)) if is_connection_closed_error(e) => {
                    debug!(
                        "[HTTP/2] Connection closed by client (expected): {} (client: {})",
                        e, client_ip
                    );
                    break;
                }
                Err(e) => {
                    let _ = conn
                        .send_goaway(e.error_code(), e.to_string().as_bytes())
                        .await;
                    return Err(e);
                }
            }
        }
    }

    Ok(())
}

/// per-stream タスクからメインループへ送るレスポンス断片（F-116）。
///
/// ボディ終端は **送信端（[`crate::stream_channel::Sender`]）の drop** で表す
/// （メインループは EOF を検出して END_STREAM を送出する）。HPACK エンコードは
/// 接続状態を進めるため、[`H2RespMsg::Head`]/[`H2RespMsg::Trailers`] のエンコードは
/// **メインループが送信順に**行う（タスクは名前/値ペアのまま渡す）。
#[cfg(feature = "http2")]
enum H2RespMsg {
    /// レスポンス head（ステータス + ヘッダ）。最初に 1 回だけ送られる。
    Head {
        status: u16,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        end_stream: bool,
    },
    /// レスポンスボディ断片（ゼロコピー）。
    Body(Bytes),
    /// gRPC トレイラー等（END_STREAM 付き HEADERS）。gRPC 経路でのみ生成・送出する。
    #[cfg(feature = "grpc")]
    Trailers(Vec<(Vec<u8>, Vec<u8>)>),
    /// head 送出後のバックエンドエラー等でストリームをリセットする（RST_STREAM エラーコード）。
    Reset(u32),
}

/// per-stream リクエストタスクへ conn 非依存で引き渡すリクエスト情報（F-116）。
#[cfg(feature = "http2")]
struct H2RequestCtx {
    method: Vec<u8>,
    path: Vec<u8>,
    /// `:authority`（無ければ host ヘッダー、いずれも無ければ空）。
    authority: Vec<u8>,
    /// 疑似ヘッダー `:method`/`:path`/`:authority` を除いたヘッダー（`:scheme` 等の残余含む）。
    headers: Vec<crate::http2::hpack::HeaderField>,
    /// 完了済みリクエストボディ（バッファ経路。ストリーミング経路では空）。
    body: Bytes,
    client_ip: Box<str>,
    start: Instant,
}

/// メインループ側のストリーム状態（F-116）。
#[cfg(feature = "http2")]
struct H2ActiveStream {
    /// レスポンス断片の受信端。
    resp_rx: crate::stream_channel::Receiver<H2RespMsg>,
    /// リクエストボディ断片の送信端（ストリーミング経路のみ。EOF で None）。
    req_tx: Option<crate::stream_channel::Sender<Bytes>>,
    /// req チャネルへ未投入のボディ（満杯時の溢れ）。
    req_pending: std::collections::VecDeque<Bytes>,
    /// リクエスト方向ストリーミング対象か。
    req_streaming: bool,
    /// クライアント END_STREAM 受信済み。
    req_eof: bool,
    /// 受信済みリクエストボディ累計（`max_request_body_size` 強制用）。
    req_bytes_total: u64,
    /// リクエストボディ上限（0 = 無制限）。
    max_request_body: u64,
    /// HEADERS 送出済み。
    head_sent: bool,
    /// END_STREAM 送出済み（レスポンス完了）。
    end_sent: bool,
    /// フロー制御ウィンドウ待ちの未送信ボディ残 `(buf, 送信済みオフセット)`。
    pending_body: Option<(Bytes, usize)>,
}

/// per-stream リクエストタスクのスポーナ型（F-46: 型付きタスクプールで Box 確保を回避）。
#[cfg(feature = "http2")]
type H2TaskSpawner = std::rc::Rc<
    dyn Fn(
        H2RequestCtx,
        Option<crate::stream_channel::Receiver<Bytes>>,
        crate::stream_channel::Sender<H2RespMsg>,
        crate::stream_channel::Notify,
    ),
>;

/// HTTP/2 ワーカースレッド用の per-stream タスクスポーナを作成する。
#[cfg(feature = "http2")]
fn h2_task_spawner() -> H2TaskSpawner {
    let pool = crate::runtime::TaskPool::new();
    std::rc::Rc::new(move |ctx, req_rx, resp_tx, notify| {
        pool.spawn(h2_request_task(ctx, req_rx, resp_tx, notify));
    })
}

/// `write_buf` がこのサイズを超えたら drive 中に明示フラッシュする（F-116）。
#[cfg(feature = "http2")]
const WRITE_BUF_FLUSH_THRESHOLD: usize = 128 * 1024;
/// レスポンスチャネル容量（HTTP/3 と同水準）。
#[cfg(feature = "http2")]
const H2_RESP_CHANNEL_CAP: usize = 4;
/// リクエストボディチャネル容量。
#[cfg(feature = "http2")]
const H2_REQ_CHANNEL_CAP: usize = 4;

/// HEADERS 完了時（`ProcessedRequest`）にストリームタスクを起動する（F-116）。
///
/// `body_pending=false` は END_STREAM 済み（バッファ経路）。`body_pending=true` は
/// ボディ継続で、ストリーミング適格ならリクエストボディチャネル付きで起動する。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
fn h2_spawn_for_request<S>(
    conn: &mut http2::Http2Connection<S>,
    stream_id: u32,
    body_pending: bool,
    streams: &mut std::collections::HashMap<u32, H2ActiveStream>,
    notify: &crate::stream_channel::Notify,
    spawner: &H2TaskSpawner,
    client_ip: &str,
    connection_metric: &mut ActiveConnectionMetric,
) where
    S: crate::runtime::io::AsyncReadRent + crate::runtime::io::AsyncWriteRentExt + Unpin,
{
    // 既にタスク起動済みのストリーム（body_pending の二重通知）はスキップ。
    if streams.contains_key(&stream_id) {
        return;
    }

    // ストリーミング適格判定 + リクエストボディ上限をルーティング 1 回で取得する
    // （適格判定と上限取得で find_backend_unified を二重実行しない）。
    let plan = if body_pending {
        h2_route_streaming_plan(conn, stream_id, client_ip)
    } else {
        None
    };
    let streaming = plan.is_some();
    if body_pending && !streaming {
        // 非適格: 何もしない。DATA は process_frame が request_body へ蓄積し、
        // END_STREAM 受信時に body_pending=false の ProcessedRequest で本関数が再度呼ばれる。
        return;
    }
    let max_request_body = plan.unwrap_or(0);

    let parts = match conn.take_request_parts(stream_id) {
        Some(p) => p,
        None => return,
    };

    // authority（host フォールバック）を解決。
    let authority = parts.authority.clone().unwrap_or_else(|| {
        parts
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(b"host"))
            .map(|h| h.value.clone())
            .unwrap_or_default()
    });

    if let Ok(host_str) = std::str::from_utf8(&authority) {
        connection_metric.set_host(host_str.to_string());
    } else {
        connection_metric.set_host("unknown".to_string());
    }

    let ctx = H2RequestCtx {
        method: parts.method,
        path: parts.path,
        authority,
        headers: parts.headers,
        body: parts.body.freeze(),
        client_ip: Box::from(client_ip),
        start: Instant::now(),
    };

    let (resp_tx, resp_rx) = crate::stream_channel::channel::<H2RespMsg>(H2_RESP_CHANNEL_CAP);
    let (req_tx, req_rx) = if streaming {
        let (tx, rx) = crate::stream_channel::channel::<Bytes>(H2_REQ_CHANNEL_CAP);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    spawner(ctx, req_rx, resp_tx, notify.clone());

    streams.insert(
        stream_id,
        H2ActiveStream {
            resp_rx,
            req_tx,
            req_pending: std::collections::VecDeque::new(),
            req_streaming: streaming,
            req_eof: false,
            req_bytes_total: 0,
            max_request_body,
            head_sent: false,
            end_sent: false,
            pending_body: None,
        },
    );
}

/// 2 つの Future（ソケット可読 / notify）を自前 `poll_fn` で race する（futures 依存を増やさない）。
///
/// 戻り値 `true` = 可読、`false` = notify。両 Future ともキャンセル安全
/// （`wait_readable_fd` は POLL_ADD、`notify.wait` はフラグ待ち）。
#[cfg(feature = "http2")]
async fn h2_select_readable_or_notify(fd: RawFd, notify: &crate::stream_channel::Notify) -> bool {
    use std::future::poll_fn;
    use std::future::Future;
    use std::task::Poll;

    let readable = crate::runtime::tcp::wait_readable_fd(fd);
    let wait = notify.wait();
    let mut readable = std::pin::pin!(readable);
    let mut wait = std::pin::pin!(wait);
    poll_fn(move |cx| {
        if readable.as_mut().poll(cx).is_ready() {
            return Poll::Ready(true);
        }
        if wait.as_mut().poll(cx).is_ready() {
            return Poll::Ready(false);
        }
        Poll::Pending
    })
    .await
}

/// 全ストリームのレスポンスを 1 回駆動する（F-116）。
///
/// 各ストリームで pending_body 再送 → resp_rx を try_recv して
/// HEADERS/DATA/Trailers/Reset を送出。ウィンドウ枯渇時は pending_body へ保留。
/// チャネル EOF（sender drop）+ 未完了なら空 END_STREAM DATA でクリーンに閉じる。
#[cfg(feature = "http2")]
async fn drive_h2_streams<S>(
    conn: &mut http2::Http2Connection<S>,
    streams: &mut std::collections::HashMap<u32, H2ActiveStream>,
) -> Result<(), http2::Http2Error>
where
    S: crate::runtime::io::AsyncReadRent + crate::runtime::io::AsyncWriteRentExt + Unpin,
{
    use crate::stream_channel::{TryRecv, TrySendError};

    let mut done: Vec<u32> = Vec::new();
    let ids: Vec<u32> = streams.keys().copied().collect();

    for sid in ids {
        // --- リクエストボディを req チャネルへ流す（バックプレッシャ考慮） ---
        {
            let st = streams.get_mut(&sid).unwrap();
            let mut transferred = false;
            while st.req_tx.is_some() {
                let item = match st.req_pending.pop_front() {
                    Some(it) => it,
                    None => break,
                };
                match st.req_tx.as_ref().unwrap().try_send(item) {
                    Ok(()) => transferred = true,
                    Err(TrySendError::Full(it)) => {
                        st.req_pending.push_front(it);
                        break;
                    }
                    Err(TrySendError::Closed(_)) => {
                        st.req_pending.clear();
                        st.req_tx = None;
                    }
                }
            }
            if st.req_tx.is_some() && st.req_pending.is_empty() && st.req_eof {
                // クライアント END_STREAM 済み・全量投入済み → sender drop で EOF 伝播。
                st.req_tx = None;
            }
            // recv ウィンドウの消費連動補充（F-116 バックプレッシャ）: req チャネルへ転送
            // できた分だけクライアントへ新規クレジットを渡す。受信時に即補充すると
            // バックエンドが遅い場合に req_pending が際限なく成長する（旧 F-32 経路の
            // 「バックエンド書き込み後に補充」と同じ消費連動タイミング）。
            if transferred {
                conn.replenish_recv_window(sid);
            }
        }

        // --- pending_body（ウィンドウ待ち残）を先に再送 ---
        {
            let st = streams.get_mut(&sid).unwrap();
            if let Some((buf, off)) = st.pending_body.take() {
                let queued = conn.queue_data_frames(sid, &buf[off..], false)?;
                let new_off = off + queued;
                if conn.pending_write_len() > WRITE_BUF_FLUSH_THRESHOLD {
                    conn.flush_write_buf().await?;
                }
                if new_off < buf.len() {
                    // まだウィンドウ不足 → 再保留してこのストリームは以降スキップ。
                    st.pending_body = Some((buf, new_off));
                    continue;
                }
            }
        }

        // --- resp_rx を送出可能な限りドレイン ---
        loop {
            let recv = {
                let st = streams.get_mut(&sid).unwrap();
                st.resp_rx.try_recv()
            };
            let msg = match recv {
                TryRecv::Item(m) => m,
                TryRecv::Empty => break,
                TryRecv::Closed => {
                    let st = streams.get_mut(&sid).unwrap();
                    if st.head_sent && !st.end_sent {
                        // EOF: head 送出済みで END_STREAM 未送なら空 DATA で閉じる。
                        conn.queue_data_frames(sid, &[], true)?;
                        st.end_sent = true;
                    } else if !st.head_sent {
                        // タスクが Head を送らずに終了（想定外の異常終了）: クライアントを
                        // 宙吊りにしないよう RST_STREAM で明示的に打ち切る。
                        conn.flush_write_buf().await?;
                        let _ = conn
                            .send_rst_stream(sid, http2::Http2ErrorCode::InternalError)
                            .await;
                    }
                    done.push(sid);
                    break;
                }
            };

            match msg {
                H2RespMsg::Head {
                    status,
                    headers,
                    end_stream,
                } => {
                    let hv: Vec<(&[u8], &[u8])> = headers
                        .iter()
                        .map(|(k, v)| (k.as_slice(), v.as_slice()))
                        .collect();
                    conn.send_headers_buffered_end(sid, status, &hv, end_stream)
                        .await?;
                    let st = streams.get_mut(&sid).unwrap();
                    st.head_sent = true;
                    if end_stream {
                        st.end_sent = true;
                    }
                }
                H2RespMsg::Body(bytes) => {
                    let queued = conn.queue_data_frames(sid, &bytes, false)?;
                    if conn.pending_write_len() > WRITE_BUF_FLUSH_THRESHOLD {
                        conn.flush_write_buf().await?;
                    }
                    if queued < bytes.len() {
                        let st = streams.get_mut(&sid).unwrap();
                        st.pending_body = Some((bytes, queued));
                        break;
                    }
                }
                #[cfg(feature = "grpc")]
                H2RespMsg::Trailers(_trailers) => {
                    // トレイラー送出は write_all を伴うため先に合流バッファをフラッシュ。
                    conn.flush_write_buf().await?;
                    {
                        let mut grpc_status = 0u32;
                        let mut grpc_message: Option<String> = None;
                        for (name, value) in &_trailers {
                            if name == b"grpc-status" {
                                if let Ok(s) = std::str::from_utf8(value) {
                                    grpc_status = s.trim().parse().unwrap_or(0);
                                }
                            } else if name == b"grpc-message" {
                                grpc_message =
                                    std::str::from_utf8(value).ok().map(|s| s.to_string());
                            }
                        }
                        let _ = conn
                            .send_grpc_trailers(sid, grpc_status, grpc_message.as_deref())
                            .await;
                    }
                    let st = streams.get_mut(&sid).unwrap();
                    st.head_sent = true;
                    st.end_sent = true;
                }
                H2RespMsg::Reset(code) => {
                    conn.flush_write_buf().await?;
                    let _ = conn
                        .send_rst_stream(sid, http2::Http2ErrorCode::from_u32(code))
                        .await;
                    let st = streams.get_mut(&sid).unwrap();
                    st.end_sent = true;
                    done.push(sid);
                    break;
                }
            }
        }
    }

    for sid in done {
        if let Some(st) = streams.remove(&sid) {
            // タスクが早期終了したストリーミングストリームは req_pending が未転送のまま
            // 残り得る。そのクレジットが conn レベルに滞留しないよう補充する。
            if st.req_streaming {
                conn.replenish_recv_window(sid);
            }
        }
    }
    Ok(())
}

/// リクエスト方向ストリーミングの適格判定（F-32 の条件を厳密に踏襲）。
///
/// 条件: Proxy バックエンド + WASM モジュール非適用 + 非 gRPC + バッファリング非 Full +
/// 上流 use_h2c 以外 + セキュリティ許可 + サーバー選択成功。conn は借用のみ（変更しない）。
/// 適格なら `Some(max_request_body)`（0 = 無制限）を返す。
#[cfg(feature = "http2")]
fn h2_route_streaming_plan<S>(
    conn: &http2::Http2Connection<S>,
    stream_id: u32,
    client_ip: &str,
) -> Option<u64>
where
    S: crate::runtime::io::AsyncReadRent + crate::runtime::io::AsyncWriteRentExt + Unpin,
{
    let stream = conn.get_stream(stream_id)?;
    let method = stream
        .method()
        .map(|m| m.to_vec())
        .unwrap_or_else(|| b"GET".to_vec());
    let path = stream
        .path()
        .map(|p| p.to_vec())
        .unwrap_or_else(|| b"/".to_vec());
    let authority = stream
        .authority()
        .map(|a| a.to_vec())
        .or_else(|| {
            stream
                .request_headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case(b"host"))
                .map(|h| h.value.clone())
        })
        .unwrap_or_default();

    let h2_headers_store: Vec<(Vec<u8>, Vec<u8>)> = stream
        .request_headers
        .iter()
        .map(|h| (h.name.clone(), h.value.clone()))
        .collect();
    let headers_raw: Vec<(&[u8], &[u8])> = h2_headers_store
        .iter()
        .map(|(k, v)| (k.as_slice(), v.as_slice()))
        .collect();

    let config = CURRENT_CONFIG.load();
    let query_start = path.iter().position(|&b| b == b'?');
    let raw_query: &[u8] = query_start.map(|i| &path[i + 1..]).unwrap_or(b"");
    let path_wo_query = query_start.map(|i| &path[..i]).unwrap_or(&path[..]);

    let client_socket_addr = h2_client_socket_addr(client_ip);

    let backend_result = find_backend_unified(
        &authority,
        path_wo_query,
        &method,
        &headers_raw,
        raw_query,
        &client_socket_addr,
        config.route.as_slice(),
        &config.upstream_groups,
    )
    .or_else(|| {
        if !authority.is_empty() {
            find_backend_unified(
                b"",
                path_wo_query,
                &method,
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

    let (_prefix, backend, _rc) = backend_result?;
    let (upstream_group, security, buffering) = match &backend {
        Backend::Proxy(ug, sec, _comp, buf, _cache, modules) => {
            if modules.as_ref().is_some_and(|m| !m.is_empty()) {
                return None;
            }
            (ug.clone(), sec.clone(), buf.clone())
        }
        _ => return None,
    };

    // gRPC はトレイラー処理のため専用経路（非ストリーミング）。
    let is_grpc = h2_headers_store.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case(b"content-type")
            && value
                .get(..16)
                .map(|p| p.eq_ignore_ascii_case(b"application/grpc"))
                .unwrap_or(false)
    });
    if is_grpc {
        return None;
    }
    if buffering.mode == crate::buffering::BufferingMode::Full {
        return None;
    }
    if check_security(&security, client_ip, &method, 0, true) != SecurityCheckResult::Allowed {
        return None;
    }
    let server = upstream_group.select(client_ip)?;
    if server.target.use_h2c {
        return None;
    }
    Some(security.max_request_body_size as u64)
}

/// クライアント IP 文字列を `SocketAddr` に変換する（ルーティング入力）。
#[cfg(feature = "http2")]
fn h2_client_socket_addr(client_ip: &str) -> SocketAddr {
    if let Ok(addr) = client_ip.parse::<SocketAddr>() {
        addr
    } else if let Ok(ip) = client_ip.parse::<std::net::IpAddr>() {
        SocketAddr::new(ip, 80)
    } else {
        SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)), 0)
    }
}

/// per-stream リクエストタスク本体（F-116）。conn には一切触れない。
#[cfg(feature = "http2")]
async fn h2_request_task(
    ctx: H2RequestCtx,
    req_rx: Option<crate::stream_channel::Receiver<Bytes>>,
    resp_tx: crate::stream_channel::Sender<H2RespMsg>,
    notify: crate::stream_channel::Notify,
) {
    let user_agent: Vec<u8> = ctx
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(b"user-agent"))
        .map(|h| h.value.clone())
        .unwrap_or_default();

    let (status, resp_size, req_size) = if let Some(rx) = req_rx {
        h2_serve_streaming(&ctx, &rx, &resp_tx, &notify).await
    } else {
        h2_serve_buffered(&ctx, &resp_tx, &notify).await
    };

    if status != 0 {
        log_access(
            &ctx.method,
            &ctx.authority,
            &ctx.path,
            &user_agent,
            req_size,
            status,
            resp_size,
            ctx.start,
            &ctx.client_ip,
            "",
        );
    }
    // resp_tx / req_rx はここで drop → メインループへ EOF 伝播。
    notify.notify();
}

/// レスポンスチャネルへ送り、メインループへ通知する。receiver 切断時は `Err(())`。
#[cfg(feature = "http2")]
#[inline]
async fn h2_send(
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
    msg: H2RespMsg,
) -> Result<(), ()> {
    let r = resp_tx.send(msg).await;
    notify.notify();
    r
}

/// サーバー/Alt-Svc 等の共通レスポンスヘッダを所有ベクタで構築する。
#[cfg(feature = "http2")]
fn h2_base_headers(add_alt_svc: bool) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut headers: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(4);
    if let Some(ref g) = get_server_header_guard() {
        let (n, v) = g.as_header();
        headers.push((n.to_vec(), v.to_vec()));
    }
    if add_alt_svc {
        if let Some(g) = get_alt_svc_guard() {
            let (n, v) = g.as_header();
            headers.push((n.to_vec(), v.to_vec()));
        }
    }
    headers
}

/// 完結した（頭+本体一括の）レスポンスをメッセージ化して送る。戻り値 `(status, body_len)`。
#[cfg(feature = "http2")]
async fn h2_emit_full(
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
    status: u16,
    headers: Vec<(Vec<u8>, Vec<u8>)>,
    body: Vec<u8>,
) -> (u16, u64) {
    let len = body.len() as u64;
    let empty = body.is_empty();
    if h2_send(
        resp_tx,
        notify,
        H2RespMsg::Head {
            status,
            headers,
            end_stream: empty,
        },
    )
    .await
    .is_err()
    {
        return (status, len);
    }
    if !empty {
        let _ = h2_send(resp_tx, notify, H2RespMsg::Body(Bytes::from(body))).await;
    }
    (status, len)
}

/// エラー応答（Server ヘッダ付き）をメッセージ化して送る。戻り値 `(status, body_len)`。
#[cfg(feature = "http2")]
async fn h2_emit_error(
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
    status: u16,
    msg: &[u8],
) -> (u16, u64) {
    h2_emit_full(
        resp_tx,
        notify,
        status,
        h2_base_headers(false),
        msg.to_vec(),
    )
    .await
}

/// バッファ経路（END_STREAM 済み）の 1 リクエストを処理してレスポンスを送出する（F-116）。
///
/// 戻り値 `(status, resp_size, req_size)`。`status == 0` はクライアント切断（ログ不要）。
#[cfg(feature = "http2")]
async fn h2_serve_buffered(
    ctx: &H2RequestCtx,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64, u64) {
    let req_size = ctx.body.len() as u64;
    let (status, resp_size) = h2_dispatch(ctx, resp_tx, notify).await;
    (status, resp_size, req_size)
}

/// バッファ経路のルーティング + ディスパッチ。戻り値 `(status, resp_size)`。
#[cfg(feature = "http2")]
async fn h2_dispatch(
    ctx: &H2RequestCtx,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64) {
    let method = &ctx.method[..];
    let path = &ctx.path[..];
    let client_ip: &str = &ctx.client_ip;

    // 非疑似ヘッダーのみ（ルーティング・転送用）。
    let headers_raw: Vec<(&[u8], &[u8])> = ctx
        .headers
        .iter()
        .filter(|h| !h.name.starts_with(b":"))
        .map(|h| (h.name.as_slice(), h.value.as_slice()))
        .collect();

    // メトリクスエンドポイント。
    {
        let config = CURRENT_CONFIG.load();
        let prom = &config.prometheus_config;
        let path_str = std::str::from_utf8(path).unwrap_or("/");
        if prom.enabled && path_str == prom.path && method == b"GET" {
            if !prom.is_ip_allowed(client_ip) {
                return h2_emit_error(resp_tx, notify, 403, b"Forbidden").await;
            }
            let body = encode_prometheus_metrics();
            let mut headers = h2_base_headers(false);
            headers.push((
                b"content-type".to_vec(),
                b"text/plain; version=0.0.4; charset=utf-8".to_vec(),
            ));
            return h2_emit_full(resp_tx, notify, 200, headers, body).await;
        }
    }

    // 管理 API（B-29）。
    #[cfg(feature = "admin")]
    if let Some((status, headers, body)) = h2_admin_response(method, path, client_ip, &headers_raw)
    {
        return h2_emit_full(resp_tx, notify, status, headers, body).await;
    }

    let config = CURRENT_CONFIG.load();
    let query_start = path.iter().position(|&b| b == b'?');
    let raw_query: &[u8] = query_start.map(|i| &path[i + 1..]).unwrap_or(b"");
    let path_wo_query = query_start.map(|i| &path[..i]).unwrap_or(path);
    let client_socket_addr = h2_client_socket_addr(client_ip);
    let authority = &ctx.authority[..];

    let backend_result = find_backend_unified(
        authority,
        path_wo_query,
        method,
        &headers_raw,
        raw_query,
        &client_socket_addr,
        config.route.as_slice(),
        &config.upstream_groups,
    )
    .or_else(|| {
        if !authority.is_empty() {
            find_backend_unified(
                b"",
                path_wo_query,
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

    let (prefix, backend, route_compression) = match backend_result {
        Some(b) => b,
        None => {
            return h2_emit_error(resp_tx, notify, 404, b"Not Found").await;
        }
    };

    // セキュリティチェック。
    let security = backend.security();
    let check_result = check_security(security, client_ip, method, ctx.body.len(), false);
    if check_result != SecurityCheckResult::Allowed {
        let status = check_result.status_code();
        let msg = check_result.message();
        return h2_emit_error(resp_tx, notify, status, msg).await;
    }

    // WASM リクエストフィルタ。
    #[cfg(feature = "wasm")]
    let wasm_modules_to_apply: Arc<Vec<String>> = {
        let config = CURRENT_CONFIG.load();
        if let Some(ref wasm_engine) = config.wasm_filter_engine {
            let path_str = std::str::from_utf8(path).unwrap_or("/");
            let method_str = std::str::from_utf8(method).unwrap_or("GET");
            let modules_to_apply = if let Some(backend_modules) = backend.modules_arc() {
                backend_modules.clone()
            } else {
                crate::wasm::empty_wasm_modules()
            };
            if !modules_to_apply.is_empty() {
                let headers_vec: Vec<(Vec<u8>, Vec<u8>)> = ctx
                    .headers
                    .iter()
                    .map(|h| (h.name.clone(), h.value.clone()))
                    .collect();
                let wasm_result = wasm_engine
                    .clone()
                    .on_request_headers_with_modules_async(
                        modules_to_apply.clone(),
                        Arc::from(path_str),
                        Arc::from(method_str),
                        headers_vec,
                        Arc::from(client_ip),
                        ctx.body.is_empty(),
                    )
                    .await;
                match wasm_result {
                    crate::wasm::FilterResult::LocalResponse(resp) => {
                        let mut headers: Vec<(Vec<u8>, Vec<u8>)> = resp
                            .headers
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        for (n, v) in h2_base_headers(false) {
                            headers.push((n, v));
                        }
                        let (st, sz) =
                            h2_emit_full(resp_tx, notify, resp.status_code, headers, resp.body)
                                .await;
                        crate::wasm::on_request_complete_async(
                            wasm_engine.clone(),
                            modules_to_apply.clone(),
                        )
                        .await;
                        return (st, sz);
                    }
                    crate::wasm::FilterResult::Pause => {
                        warn!("WASM module requested pause, but async operations are not yet supported");
                    }
                    crate::wasm::FilterResult::Continue { .. } => {}
                }
            }
            modules_to_apply
        } else {
            crate::wasm::empty_wasm_modules()
        }
    };

    // Accept-Encoding。
    let client_encoding = ctx
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(b"accept-encoding"))
        .map(|h| AcceptedEncoding::parse(&h.value))
        .unwrap_or(AcceptedEncoding::Identity);

    let result = match backend {
        Backend::Proxy(upstream_group, security, compression, _buffering, _cache, _) => {
            h2_proxy(
                ctx,
                &upstream_group,
                &compression,
                client_encoding,
                &prefix,
                &security,
                #[cfg(feature = "wasm")]
                &wasm_modules_to_apply,
                resp_tx,
                notify,
            )
            .await
        }
        Backend::MemoryFile(data, mime_type, security, _) => {
            let path_str = std::str::from_utf8(path).unwrap_or("/");
            let prefix_str = std::str::from_utf8(&prefix).unwrap_or("");
            let remainder = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
                &path_str[prefix_str.len()..]
            } else {
                ""
            };
            if !remainder.trim_matches('/').is_empty() {
                h2_emit_error(resp_tx, notify, 404, b"Not Found").await
            } else {
                let (built_headers, response_body) = build_h2_compressed_file_response(
                    &data,
                    mime_type.as_ref(),
                    &security,
                    &route_compression,
                    client_encoding,
                );
                #[cfg(feature = "wasm")]
                let header_store =
                    apply_h2_wasm_response_headers(&wasm_modules_to_apply, 200, built_headers)
                        .await;
                #[cfg(not(feature = "wasm"))]
                let header_store = built_headers;
                h2_emit_full(resp_tx, notify, 200, header_store, response_body).await
            }
        }
        Backend::SendFile(
            base_path,
            is_dir,
            index_file,
            security,
            _cache,
            open_file_cache_config,
            _,
        ) => {
            h2_sendfile(
                ctx,
                &base_path,
                is_dir,
                index_file.as_deref(),
                &prefix,
                &security,
                &route_compression,
                client_encoding,
                open_file_cache_config.as_deref(),
                #[cfg(feature = "wasm")]
                &wasm_modules_to_apply,
                resp_tx,
                notify,
            )
            .await
        }
        Backend::Redirect(redirect_url, status_code, preserve_path, _) => {
            h2_redirect(
                ctx,
                &redirect_url,
                status_code,
                preserve_path,
                &prefix,
                resp_tx,
                notify,
            )
            .await
        }
    };

    #[cfg(feature = "wasm")]
    {
        if !wasm_modules_to_apply.is_empty() {
            let config = CURRENT_CONFIG.load();
            if let Some(ref wasm_engine) = config.wasm_filter_engine {
                crate::wasm::on_request_complete_async(
                    wasm_engine.clone(),
                    wasm_modules_to_apply.clone(),
                )
                .await;
            }
        }
    }

    result
}

/// バッファ経路の Proxy 処理（H1/HTTPS/H2C バックエンド）。戻り値 `(status, resp_size)`。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
async fn h2_proxy(
    ctx: &H2RequestCtx,
    upstream_group: &Arc<UpstreamGroup>,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    prefix: &[u8],
    security: &SecurityConfig,
    #[cfg(feature = "wasm")] wasm_modules: &Arc<Vec<String>>,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64) {
    let method = &ctx.method[..];
    let req_path = &ctx.path[..];
    let client_ip: &str = &ctx.client_ip;

    // Consistent Hash キー解決。
    let hash_key_owned: Option<String> = match &upstream_group.algorithm {
        crate::config::LoadBalanceAlgorithm::ConsistentHash {
            hash_key: crate::config::HashKey::Header(name),
        } => ctx
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name.as_bytes()))
            .and_then(|h| std::str::from_utf8(&h.value).ok())
            .map(|s| s.to_string()),
        crate::config::LoadBalanceAlgorithm::ConsistentHash {
            hash_key: crate::config::HashKey::Cookie(name),
        } => ctx
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(b"cookie"))
            .and_then(|h| std::str::from_utf8(&h.value).ok())
            .and_then(|c| {
                c.split(';').find_map(|part| {
                    let part = part.trim();
                    part.split_once('=').and_then(|(k, v)| {
                        if k.trim().eq_ignore_ascii_case(name) {
                            Some(v.trim().to_string())
                        } else {
                            None
                        }
                    })
                })
            }),
        _ => None,
    };

    let server = match upstream_group.select_with_key(client_ip, hash_key_owned.as_deref(), None) {
        Some(s) => s,
        None => return h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await,
    };
    server.acquire();
    let target = &server.target;

    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    let preserve_grpc_path = ctx
        .headers
        .iter()
        .any(|h| header_pair_is_grpc(&h.name, &h.value));
    let final_path_owned =
        compute_upstream_path(path_str, prefix, &target.path_prefix, preserve_grpc_path);
    let final_path = final_path_owned.as_str();

    // H2C バックエンドは HPACK 応答のため専用処理。
    if target.use_h2c || upstream_group.use_h2c() {
        let addr = HostPortStr::new(&target.host, target.port);
        let addr = addr.as_str();
        let result = h2_proxy_h2c(
            ctx,
            addr,
            target,
            method,
            final_path.as_bytes(),
            security,
            #[cfg(feature = "wasm")]
            wasm_modules,
            resp_tx,
            notify,
        )
        .await;
        server.release();
        return result;
    }

    // H1/HTTPS バックエンドへの HTTP/1.1 リクエストを構築。
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
    for header in ctx.headers.iter() {
        if header.name.starts_with(b":") {
            continue;
        }
        if header.name.eq_ignore_ascii_case(b"connection")
            || header.name.eq_ignore_ascii_case(b"keep-alive")
            || header.name.eq_ignore_ascii_case(b"transfer-encoding")
        {
            continue;
        }
        request.extend_from_slice(&header.name);
        request.extend_from_slice(b": ");
        request.extend_from_slice(&header.value);
        request.extend_from_slice(b"\r\n");
    }
    if !ctx.body.is_empty() {
        request.extend_from_slice(b"Content-Length: ");
        let mut len_buf = itoa::Buffer::new();
        request.extend_from_slice(len_buf.format(ctx.body.len()).as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    request.extend_from_slice(b"Connection: keep-alive\r\n\r\n");
    request.extend_from_slice(&ctx.body);

    let addr = HostPortStr::new(&target.host, target.port);
    let addr = addr.as_str();

    let result = if target.use_tls {
        h2_proxy_https(
            ctx,
            addr,
            target.sni(),
            request,
            compression,
            client_encoding,
            security,
            upstream_group.tls_insecure(),
            resp_tx,
            notify,
        )
        .await
    } else {
        h2_proxy_http(
            ctx,
            addr,
            request,
            compression,
            client_encoding,
            security,
            resp_tx,
            notify,
        )
        .await
    };
    server.release();
    result
}

/// バックエンド接続の EADDRNOTAVAIL 一時的失敗を指数バックオフでリトライして吸収する（B-44 第2段）。
///
/// F-116 のストリーム多重化により、スレッドあたり同時 ~250 ストリームが独立にバックエンド
/// 接続を新規 connect するようになった。プール上限引き上げ（B-44 第1段）後も、
/// コールドスタート/構成切り替え直後の**接続ストーム**（同時 ~1000 ストリームの一斉新規
/// connect）では `EADDRNOTAVAIL` が数秒間バースト的に発生し 502 化する。切り分けの結果、
/// 素の `connect(2)`（Python 実測）では同一条件で再現せず、TIME_WAIT も枯渇水準に達して
/// いない（カーネルのポート枯渇ではない）ため、veil の io_uring CONNECT 経路特有の事象
/// （バックエンド nginx の SYN/accept キュー飽和時の失敗が `EADDRNOTAVAIL` として表面化）
/// と推定している。nginx の `proxy_next_upstream` 相当の一時的失敗リトライとして、
/// `EADDRNOTAVAIL` の場合のみ 10ms → 40ms → 160ms の指数バックオフを挟み最大 4 試行
/// （リトライ 3 回）する（それ以外の connect エラーは従来どおり即時返却）。
///
/// このリトライは **接続失敗時のみ実行されるコールドパス**（ホットパスは初回 connect の
/// 成功経路）であり、追加アロケーションなし・待機は既存の io_uring タイマー
/// （`crate::runtime::timer::sleep`）による非同期待機のため、ホットパス絶対規則
/// （同期待機禁止・不要なアロケーション禁止）に反しない。
///
/// 戻り値: 接続成功で `Ok(TcpStream)`（成功時に `set_nodelay(true)` 済み）。
/// `timeout` 到達時は呼び出し側で 504 に区別できるよう `io::ErrorKind::TimedOut` を返す。
/// それ以外の connect エラー（最終リトライ失敗を含む）はそのまま返す（呼び出し側で 502）。
#[cfg(feature = "http2")]
async fn connect_backend_with_retry(addr: &str) -> io::Result<TcpStream> {
    const BACKOFF_MS: [u64; 3] = [10, 40, 160];
    let mut attempt = 0usize;
    loop {
        match timeout(CONNECT_TIMEOUT, TcpStream::connect_str(addr)).await {
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Ok(Err(e)) if e.raw_os_error() == Some(libc::EADDRNOTAVAIL) && attempt < 3 => {
                crate::runtime::timer::sleep(Duration::from_millis(BACKOFF_MS[attempt])).await;
                attempt += 1;
            }
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(io::Error::from(io::ErrorKind::TimedOut)),
        }
    }
}

/// ホストごとの新規 connect 並行数の上限（スレッドごと、B-44 第3段）。
#[cfg(feature = "http2")]
const MAX_CONCURRENT_CONNECTS_PER_HOST: usize = 64;

/// ホストごとの新規 connect 並行数ゲート（B-44 第3段）。
///
/// 第2段のリトライ（10/40/160ms、最大 4 試行）で 5xx は約 1/3 に減ったが根絶できなかった。
/// h2load の反復切り替え時に in-flight だった ~900 接続がタスクキャンセルで破棄され、
/// 次反復の冒頭で数百規模の connect ストームが再発する。失敗 → 502 → クライアントが即座に
/// 次リクエストを発行、の自己持続でストームが数秒継続し、バックオフ合計 210ms では
/// 吸収できない。そこで **バックエンドへの新規 connect の同時実行数をホストごとに制限**する
/// 構造的修正を行う（Envoy の upstream circuit breaker `max_connections`/pending queue 相当）。
/// 新規 connect は 64/スレッドの波で進み、完了したリクエストの接続が返却され次第、
/// 待機ストリームは再利用側で満たされるため、EADDRNOTAVAIL の発生源
/// （数百規模の一斉 connect）自体が消える。
///
/// ゲートは **プールミス時のみ** 作動するコールドパスであり、プールヒット
/// （ホットパス）はゲートに一切触れない（ホットパス絶対規則に反しない）。
/// thread-per-core 構成のためスレッドローカルでロック不要。
#[cfg(feature = "http2")]
struct ConnectGate {
    /// このスレッド上で進行中の新規 connect 数。
    in_flight: std::cell::Cell<usize>,
    /// スロット解放・プール返却を待つ待機者（待機者ごとの `Notify` を積む）。
    ///
    /// 設計原案はゲート共有の単一 `Notify` だったが、`Notify` は waker を 1 つしか
    /// 保持しないため、複数待機者では後着の poll が先着の waker を上書きし、
    /// 起こされない待機者が永久に停止する（lost wakeup）。待機者ごとに `Notify` を
    /// 積み、解放時に全員へブロードキャストする方式とする（キャンセル済み待機者の
    /// エントリが残っていても他の待機者を巻き込まない）。
    waiters: std::cell::RefCell<std::collections::VecDeque<crate::stream_channel::Notify>>,
}

#[cfg(feature = "http2")]
impl ConnectGate {
    fn new() -> Self {
        Self {
            in_flight: std::cell::Cell::new(0),
            waiters: std::cell::RefCell::new(std::collections::VecDeque::new()),
        }
    }

    /// 待機者全員を起こす（ブロードキャスト）。待機者はループ先頭でプール再取得を
    /// 試みるため、新規 connect ではなく返却された接続の再利用が優先される。
    fn notify_waiters(&self) {
        // wake() によるタスク再スケジュールが RefCell の再入借用と衝突しないよう、
        // キューを取り出してから借用を解放して通知する。
        let drained = std::mem::take(&mut *self.waiters.borrow_mut());
        for waiter in drained {
            waiter.notify();
        }
    }
}

/// [`ConnectGate`] の in_flight スロットを保持する RAII ガード（B-44 第3段）。
///
/// connect の成功・失敗・タイムアウト・タスクキャンセル（Future drop）のいずれの経路でも
/// Drop で必ずスロットを解放し、待機者を起こす（解放漏れなし）。
#[cfg(feature = "http2")]
struct ConnectPermit {
    gate: std::rc::Rc<ConnectGate>,
}

#[cfg(feature = "http2")]
impl ConnectPermit {
    /// 空きスロットがあれば確保して permit を返す。満杯なら `None`。
    fn try_acquire(gate: &std::rc::Rc<ConnectGate>) -> Option<Self> {
        if gate.in_flight.get() < MAX_CONCURRENT_CONNECTS_PER_HOST {
            gate.in_flight.set(gate.in_flight.get() + 1);
            Some(Self { gate: gate.clone() })
        } else {
            None
        }
    }
}

#[cfg(feature = "http2")]
impl Drop for ConnectPermit {
    fn drop(&mut self) {
        self.gate.in_flight.set(self.gate.in_flight.get() - 1);
        self.gate.notify_waiters();
    }
}

#[cfg(feature = "http2")]
thread_local! {
    /// 接続先ホスト（addr）→ 新規 connect ゲート（B-44 第3段）。
    /// エントリは初回のプールミス時に生成される（コールドパス）。
    static CONNECT_GATES: std::cell::RefCell<
        std::collections::HashMap<String, std::rc::Rc<ConnectGate>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// ホストのゲートを取得する（なければ生成、コールドパス）。
/// `Rc` クローンはゲート取得時のみで、プールヒット経路はここに到達しない。
#[cfg(feature = "http2")]
fn connect_gate(host: &str) -> std::rc::Rc<ConnectGate> {
    CONNECT_GATES.with(|g| {
        let mut map = g.borrow_mut();
        if let Some(gate) = map.get(host) {
            gate.clone()
        } else {
            let gate = std::rc::Rc::new(ConnectGate::new());
            map.insert(host.to_string(), gate.clone());
            gate
        }
    })
}

/// プール返却直後にゲート待機者を起こし、再利用の機会を与える（B-44 第3段）。
///
/// ゲート未生成（そのホストで新規 connect の競合が起きていない）または待機者ゼロなら
/// ハッシュ参照のみの no-op。`CONNECT_GATES` の借用は `Rc` クローンの同期区間で解放し、
/// 起こされたタスク側の再入借用と衝突しない。
#[cfg(feature = "http2")]
fn notify_connect_gate_waiters(host: &str) {
    let gate = CONNECT_GATES.with(|g| g.borrow().get(host).cloned());
    if let Some(gate) = gate {
        gate.notify_waiters();
    }
}

/// ゲート付きバックエンド接続取得の結果（B-44 第3段）。
#[cfg(feature = "http2")]
enum GateAcquire<P> {
    /// ゲート通過待ちの間にプールへ返却された接続を再利用した。
    Pooled(P),
    /// ゲートを通過して新規に確立した接続。
    Fresh(TcpStream),
}

/// プールミス時のバックエンド接続取得（新規 connect 並行数ゲート付き、B-44 第3段）。
///
/// ループ先頭で毎回プール（`pool_get`）を再試行し、ゲート通過待ちの間に返却された
/// 接続があれば新規 connect よりも優先して再利用する（これがストームを構造的に消す本質）。
/// プールミスかつ in_flight < [`MAX_CONCURRENT_CONNECTS_PER_HOST`] ならスロットを確保して
/// [`connect_backend_with_retry`]（第2段のリトライは安全網として維持）へ進み、満杯なら
/// スロット解放またはプール返却の通知を待って再ループする。
///
/// `RefCell` の借用はいずれも同期区間のみで、`.await` を跨いで保持しない。
/// 待機者の `Notify` 確保（`Rc` 1 個）は初回待機時のみで、以降の再待機では再利用する。
#[cfg(feature = "http2")]
async fn acquire_backend_conn<P>(
    addr: &str,
    mut pool_get: impl FnMut() -> Option<P>,
) -> io::Result<GateAcquire<P>> {
    let gate = connect_gate(addr);
    let mut waiter: Option<crate::stream_channel::Notify> = None;
    loop {
        // ゲート通過待ちの間に返却された接続の再利用を最優先する
        if let Some(stream) = pool_get() {
            return Ok(GateAcquire::Pooled(stream));
        }
        if let Some(_permit) = ConnectPermit::try_acquire(&gate) {
            // 成功・失敗・キャンセルのすべての経路で permit の Drop がスロットを解放する
            return connect_backend_with_retry(addr)
                .await
                .map(GateAcquire::Fresh);
        }
        // 満杯: スロット解放かプール返却を待つ
        let w = waiter.get_or_insert_with(crate::stream_channel::Notify::new);
        gate.waiters.borrow_mut().push_back(w.clone());
        w.wait().await;
    }
}

/// H1 バックエンド（平文）へのプロキシ（プール再利用付き、B-28）。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
async fn h2_proxy_http(
    _ctx: &H2RequestCtx,
    addr: &str,
    request: Vec<u8>,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    security: &SecurityConfig,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64) {
    let mut backend = match HTTP_POOL.with(|p| p.borrow_mut().get(addr)) {
        Some(stream) => stream,
        None => {
            // プールミス: 新規 connect 並行数ゲート経由で取得（B-44 第3段）
            match acquire_backend_conn(addr, || HTTP_POOL.with(|p| p.borrow_mut().get(addr))).await
            {
                Ok(GateAcquire::Pooled(stream) | GateAcquire::Fresh(stream)) => stream,
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                    let (s, sz) = h2_emit_error(resp_tx, notify, 504, b"Gateway Timeout").await;
                    return (s, sz);
                }
                Err(e) => {
                    warn!("[HTTP/2] Backend connect error: {}", e);
                    let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
                    return (s, sz);
                }
            }
        }
    };

    let (write_res, returned_request) = backend.write_all(request).await;
    request_buf_put(returned_request);
    if write_res.is_err() {
        return h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
    }

    let (status, sent, reusable) =
        h2_relay_backend_response(&mut backend, compression, client_encoding, resp_tx, notify)
            .await;
    if reusable {
        HTTP_POOL.with(|p| {
            p.borrow_mut().put(
                addr.to_string(),
                backend,
                security.max_idle_connections_per_host,
                security.idle_connection_timeout_secs,
            )
        });
        // 返却した接続をゲート待機者に再利用させる（B-44 第3段）
        notify_connect_gate_waiters(addr);
    }
    (status, sent)
}

/// HTTPS バックエンドへのプロキシ（TLS プール再利用付き、B-28）。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
async fn h2_proxy_https(
    _ctx: &H2RequestCtx,
    addr: &str,
    sni: &str,
    request: Vec<u8>,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    security: &SecurityConfig,
    tls_insecure: bool,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64) {
    let pool_key = format!(
        "{}:{}:{}",
        addr,
        sni,
        if tls_insecure { "insecure" } else { "verify" }
    );

    let mut backend = match HTTPS_POOL.with(|p| p.borrow_mut().get(&pool_key)) {
        Some(stream) => stream,
        None => {
            // プールミス: 新規 connect 並行数ゲート経由で取得（B-44 第3段）
            let acquired = match acquire_backend_conn(addr, || {
                HTTPS_POOL.with(|p| p.borrow_mut().get(&pool_key))
            })
            .await
            {
                Ok(acquired) => acquired,
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                    return h2_emit_error(resp_tx, notify, 504, b"Gateway Timeout").await;
                }
                Err(e) => {
                    warn!("[HTTP/2] Backend connect error: {}", e);
                    return h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
                }
            };
            match acquired {
                GateAcquire::Pooled(stream) => stream,
                GateAcquire::Fresh(backend_tcp) => {
                    let tls_result = if tls_insecure {
                        let connector = get_tls_connector_insecure();
                        timeout(CONNECT_TIMEOUT, connector.connect(backend_tcp, sni)).await
                    } else {
                        let connector = get_tls_connector();
                        timeout(CONNECT_TIMEOUT, connector.connect(backend_tcp, sni)).await
                    };
                    match tls_result {
                        Ok(Ok(stream)) => stream,
                        Ok(Err(e)) => {
                            warn!("[HTTP/2] TLS handshake error: {}", e);
                            return h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
                        }
                        Err(_) => {
                            return h2_emit_error(resp_tx, notify, 504, b"Gateway Timeout").await;
                        }
                    }
                }
            }
        }
    };

    let (write_res, returned_request) = backend.write_all(request).await;
    request_buf_put(returned_request);
    if write_res.is_err() {
        return h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
    }

    let (status, sent, reusable) =
        h2_relay_backend_response(&mut backend, compression, client_encoding, resp_tx, notify)
            .await;
    if reusable {
        HTTPS_POOL.with(|p| {
            p.borrow_mut().put(
                pool_key,
                backend,
                security.max_idle_connections_per_host,
                security.idle_connection_timeout_secs,
            )
        });
        // 返却した接続をゲート待機者に再利用させる（ゲートは addr 単位、B-44 第3段）
        notify_connect_gate_waiters(addr);
    }
    (status, sent)
}

/// H2C バックエンドへのプロキシ（F-106 プール再利用 + gRPC トレイラー）。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
async fn h2_proxy_h2c(
    _ctx: &H2RequestCtx,
    addr: &str,
    target: &ProxyTarget,
    method: &[u8],
    path: &[u8],
    security: &SecurityConfig,
    #[cfg(feature = "wasm")] wasm_modules: &Arc<Vec<String>>,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64) {
    let ctx = _ctx;
    let from_pool;
    let mut h2c_client = match H2C_POOL.with(|p| p.borrow_mut().get(addr)) {
        Some(client) => {
            from_pool = true;
            client
        }
        None => {
            from_pool = false;
            match h2c_connect_and_handshake(addr).await {
                Ok(client) => client,
                Err(status) => {
                    let msg: &[u8] = if status == 504 {
                        b"Gateway Timeout"
                    } else {
                        b"Bad Gateway"
                    };
                    return h2_emit_error(resp_tx, notify, status, msg).await;
                }
            }
        }
    };

    let is_grpc_upstream = ctx
        .headers
        .iter()
        .any(|h| header_pair_is_grpc(&h.name, &h.value));
    let headers_vec: Vec<(&[u8], &[u8])> = ctx
        .headers
        .iter()
        .filter(|h| !h.name.starts_with(b":"))
        .filter(|h| {
            !h.name.eq_ignore_ascii_case(b"connection")
                && !h.name.eq_ignore_ascii_case(b"keep-alive")
                && !h.name.eq_ignore_ascii_case(b"proxy-connection")
                && !h.name.eq_ignore_ascii_case(b"transfer-encoding")
                && !h.name.eq_ignore_ascii_case(b"upgrade")
                && (is_grpc_upstream || !h.name.eq_ignore_ascii_case(b"te"))
        })
        .map(|h| (h.name.as_slice(), h.value.as_slice()))
        .collect();

    let body: Option<&[u8]> = if ctx.body.is_empty() {
        None
    } else {
        Some(&ctx.body)
    };
    let authority = target.host.as_bytes();

    let mut send_result = h2c_client
        .send_request(method, path, authority, &headers_vec, body)
        .await;
    if send_result.is_err() && from_pool {
        if let Ok(fresh) = h2c_connect_and_handshake(addr).await {
            h2c_client = fresh;
            send_result = h2c_client
                .send_request(method, path, authority, &headers_vec, body)
                .await;
        }
    }

    match send_result {
        Ok(h2c_resp) => {
            if h2c_client.is_reusable() {
                let max_idle = security.max_idle_connections_per_host;
                let idle_timeout = security.idle_connection_timeout_secs;
                H2C_POOL.with(|p| {
                    p.borrow_mut()
                        .put(addr.to_string(), h2c_client, max_idle, idle_timeout)
                });
            }

            let mut header_store: Vec<(Vec<u8>, Vec<u8>)> = h2c_resp
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            #[cfg(feature = "wasm")]
            {
                header_store =
                    apply_h2_wasm_response_headers(wasm_modules, h2c_resp.status, header_store)
                        .await;
            }
            for (n, v) in h2_base_headers(true) {
                header_store.push((n, v));
            }

            let has_body = !h2c_resp.body.is_empty();
            let has_trailers = !h2c_resp.trailers.is_empty();
            let status = h2c_resp.status;
            let body_len = h2c_resp.body.len() as u64;

            if h2_send(
                resp_tx,
                notify,
                H2RespMsg::Head {
                    status,
                    headers: header_store,
                    end_stream: !has_body && !has_trailers,
                },
            )
            .await
            .is_err()
            {
                return (status, 0);
            }

            if has_body
                && h2_send(resp_tx, notify, H2RespMsg::Body(Bytes::from(h2c_resp.body)))
                    .await
                    .is_err()
            {
                return (status, body_len);
            }

            if has_trailers {
                #[cfg(feature = "grpc")]
                {
                    let mut grpc_status = 0u32;
                    for (name, value) in &h2c_resp.trailers {
                        if name == b"grpc-status" {
                            if let Ok(s) = std::str::from_utf8(value) {
                                grpc_status = s.trim().parse().unwrap_or(0);
                            }
                        }
                    }
                    // F-09: gRPC リクエストメトリクスを記録。
                    let grpc_method = std::str::from_utf8(path).unwrap_or("");
                    let mut status_buf = itoa::Buffer::new();
                    let status_str = status_buf.format(grpc_status);
                    crate::metrics::record_grpc_request(grpc_method, status_str, &target.host);
                    let _ = h2_send(resp_tx, notify, H2RespMsg::Trailers(h2c_resp.trailers)).await;
                }
                #[cfg(not(feature = "grpc"))]
                {
                    // gRPC feature 無効時はトレイラーをスキップ。
                    let _ = &h2c_resp.trailers;
                }
            }

            (status, body_len)
        }
        Err(e) => {
            warn!("[HTTP/2] H2C request error ({}): {}", addr, e);
            h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await
        }
    }
}

/// バックエンド HTTP/1.1 レスポンスを受信して [`H2RespMsg`] としてメインループへ流す（F-116）。
///
/// 戻り値 `(status, sent, reusable)`。`reusable` はバックエンド接続をプールへ返せるか
/// （CL 全量消費 + 非 `Connection: close`。chunked/EOF/エラーは false）。
#[cfg(feature = "http2")]
async fn h2_relay_backend_response<B>(
    backend: &mut B,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64, bool)
where
    B: crate::runtime::io::AsyncReadRent + Unpin,
{
    let mut response_buf = Vec::with_capacity(BUF_SIZE);

    loop {
        let buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, backend.read(buf)).await;
        let (res, mut returned_buf) = match read_result {
            Ok(r) => r,
            Err(_) => {
                let (s, sz) = h2_emit_error(resp_tx, notify, 504, b"Gateway Timeout").await;
                return (s, sz, false);
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
                let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
                return (s, sz, false);
            }
        };
        returned_buf.set_valid_len(n);
        response_buf.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);

        if let Some(parsed) = parse_http_response(&response_buf) {
            let status = parsed.status_code;
            let body_start = parsed.header_len;
            let body = &response_buf[body_start..];

            let mut headers_storage = [httparse::EMPTY_HEADER; 64];
            let mut resp = httparse::Response::new(&mut headers_storage);
            let _ = resp.parse(&response_buf);
            let content_type = resp
                .headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case("content-type"))
                .map(|h| h.value);
            let existing_encoding = resp
                .headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case("content-encoding"))
                .map(|h| h.value);

            let stream_compress_hint = compression.should_compress(
                client_encoding,
                content_type,
                parsed.content_length,
                existing_encoding,
            );

            // 非圧縮 + CL 既知 + 非 chunked → 逐次ストリーミング。
            if stream_compress_hint.is_none() && !parsed.is_chunked {
                if let Some(content_len) = parsed.content_length {
                    let mut headers = h2_base_headers(true);
                    for header in resp.headers.iter() {
                        if header.name.is_empty() {
                            continue;
                        }
                        if header.name.eq_ignore_ascii_case("connection")
                            || header.name.eq_ignore_ascii_case("keep-alive")
                            || header.name.eq_ignore_ascii_case("transfer-encoding")
                            || header.name.eq_ignore_ascii_case("upgrade")
                        {
                            continue;
                        }
                        headers.push((header.name.as_bytes().to_vec(), header.value.to_vec()));
                    }
                    let (sent, ok) = h2_stream_body_cl(
                        resp_tx,
                        notify,
                        status,
                        headers,
                        backend,
                        body,
                        content_len,
                    )
                    .await;
                    let reusable = ok && sent == content_len as u64 && !parsed.is_connection_close;
                    return (status, sent, reusable);
                }
            }

            // 非圧縮 + chunked → ゼロコピー逐次デコード転送。
            if stream_compress_hint.is_none() && parsed.is_chunked {
                let mut headers = h2_base_headers(true);
                for header in resp.headers.iter() {
                    if header.name.is_empty() {
                        continue;
                    }
                    if header.name.eq_ignore_ascii_case("connection")
                        || header.name.eq_ignore_ascii_case("keep-alive")
                        || header.name.eq_ignore_ascii_case("transfer-encoding")
                        || header.name.eq_ignore_ascii_case("upgrade")
                        || header.name.eq_ignore_ascii_case("content-length")
                    {
                        continue;
                    }
                    headers.push((header.name.as_bytes().to_vec(), header.value.to_vec()));
                }
                let sent =
                    h2_stream_body_chunked(resp_tx, notify, status, headers, backend, body).await;
                return (status, sent, false);
            }

            // 圧縮あり / 長さ不明 → 全読み込み後に（必要なら圧縮して）送信。
            let mut backend_reusable = false;
            let final_body = if parsed.is_chunked {
                let mut decoder = ChunkedDecoder::new_unlimited();
                let mut full_body = body.to_vec();
                decoder.feed(body);
                while !decoder.is_complete() {
                    let buf = buf_get();
                    let (res, mut returned_buf) =
                        match timeout(READ_TIMEOUT, backend.read(buf)).await {
                            Ok(r) => r,
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
                    full_body.extend_from_slice(returned_buf.as_valid_slice());
                    decoder.feed(returned_buf.as_valid_slice());
                    buf_put(returned_buf);
                }
                decode_chunked_body(&full_body)
            } else if let Some(content_len) = parsed.content_length {
                let mut full_body = body.to_vec();
                while full_body.len() < content_len {
                    let buf = buf_get();
                    let (res, mut returned_buf) =
                        match timeout(READ_TIMEOUT, backend.read(buf)).await {
                            Ok(r) => r,
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
                    full_body.extend_from_slice(returned_buf.as_valid_slice());
                    buf_put(returned_buf);
                }
                backend_reusable = full_body.len() == content_len;
                full_body
            } else {
                body.to_vec()
            };

            let should_compress = compression.should_compress(
                client_encoding,
                content_type,
                Some(final_body.len()),
                existing_encoding,
            );

            let mut headers = h2_base_headers(true);
            if let Some(enc) = should_compress {
                let encoding_name: &'static [u8] = match enc {
                    AcceptedEncoding::Zstd => b"zstd",
                    AcceptedEncoding::Brotli => b"br",
                    AcceptedEncoding::Gzip => b"gzip",
                    AcceptedEncoding::Deflate => b"deflate",
                    AcceptedEncoding::Identity => b"",
                };
                if !encoding_name.is_empty() {
                    headers.push((b"content-encoding".to_vec(), encoding_name.to_vec()));
                    headers.push((b"vary".to_vec(), b"Accept-Encoding".to_vec()));
                }
            }
            for header in resp.headers.iter() {
                if header.name.is_empty() {
                    continue;
                }
                if header.name.eq_ignore_ascii_case("connection")
                    || header.name.eq_ignore_ascii_case("keep-alive")
                    || header.name.eq_ignore_ascii_case("transfer-encoding")
                    || header.name.eq_ignore_ascii_case("upgrade")
                {
                    continue;
                }
                if should_compress.is_some()
                    && (header.name.eq_ignore_ascii_case("content-length")
                        || header.name.eq_ignore_ascii_case("content-encoding"))
                {
                    continue;
                }
                headers.push((header.name.as_bytes().to_vec(), header.value.to_vec()));
            }

            let response_body = if let Some(enc) = should_compress {
                compress_body_h2(&final_body, enc, compression)
            } else {
                final_body
            };
            let (status2, sent) =
                h2_emit_full(resp_tx, notify, status, headers, response_body).await;
            return (
                status2,
                sent,
                backend_reusable && !parsed.is_connection_close,
            );
        }

        if response_buf.len() > MAX_HEADER_SIZE {
            let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
            return (s, sz, false);
        }
    }

    let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
    (s, sz, false)
}

/// 非圧縮・CL 既知ボディを [`H2RespMsg::Body`] として逐次転送する。戻り値 `(sent, ok)`。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
async fn h2_stream_body_cl<B>(
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
    status: u16,
    headers: Vec<(Vec<u8>, Vec<u8>)>,
    backend: &mut B,
    initial_body: &[u8],
    content_length: usize,
) -> (u64, bool)
where
    B: crate::runtime::io::AsyncReadRent + Unpin,
{
    let empty = content_length == 0;
    if h2_send(
        resp_tx,
        notify,
        H2RespMsg::Head {
            status,
            headers,
            end_stream: empty,
        },
    )
    .await
    .is_err()
    {
        return (0, false);
    }
    if empty {
        return (0, true);
    }

    let mut remaining = content_length;
    let init_len = initial_body.len().min(remaining);
    if init_len > 0 {
        if h2_send(
            resp_tx,
            notify,
            H2RespMsg::Body(Bytes::copy_from_slice(&initial_body[..init_len])),
        )
        .await
        .is_err()
        {
            return ((content_length - remaining) as u64, false);
        }
        remaining -= init_len;
    }

    while remaining > 0 {
        let buf = buf_get();
        let (res, mut returned_buf) = match timeout(READ_TIMEOUT, backend.read(buf)).await {
            Ok(r) => r,
            Err(_) => {
                // タイムアウト: HEADERS 送出済みのため RST_STREAM で打ち切る（旧 send_rst_stream 相当）。
                let _ = h2_send(resp_tx, notify, H2RespMsg::Reset(2)).await;
                return ((content_length - remaining) as u64, false);
            }
        };
        let n = match res {
            Ok(0) => {
                // content-length 未達でバックエンド切断: 空 END_STREAM で閉じる（graceful）。
                buf_put(returned_buf);
                return ((content_length - remaining) as u64, false);
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                let _ = h2_send(resp_tx, notify, H2RespMsg::Reset(2)).await;
                return ((content_length - remaining) as u64, false);
            }
        };
        returned_buf.set_valid_len(n);
        let take = n.min(remaining);
        let chunk = Bytes::copy_from_slice(&returned_buf.as_valid_slice()[..take]);
        buf_put(returned_buf);
        if h2_send(resp_tx, notify, H2RespMsg::Body(chunk))
            .await
            .is_err()
        {
            return ((content_length - remaining) as u64, false);
        }
        remaining -= take;
    }
    (content_length as u64, true)
}

/// 非圧縮・chunked ボディを `next_data_span` でデコードしつつ逐次転送する。戻り値 `sent`。
#[cfg(feature = "http2")]
async fn h2_stream_body_chunked<B>(
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
    status: u16,
    headers: Vec<(Vec<u8>, Vec<u8>)>,
    backend: &mut B,
    initial_body: &[u8],
) -> u64
where
    B: crate::runtime::io::AsyncReadRent + Unpin,
{
    if h2_send(
        resp_tx,
        notify,
        H2RespMsg::Head {
            status,
            headers,
            end_stream: false,
        },
    )
    .await
    .is_err()
    {
        return 0;
    }

    let mut decoder = crate::http_utils::ChunkedDecoder::new_unlimited();
    let mut sent: u64 = 0;

    match h2_drain_chunked_to_msg(resp_tx, notify, &mut decoder, initial_body, &mut sent).await {
        H2ChunkDrain::Done => return sent,
        H2ChunkDrain::NeedMore => {}
    }

    loop {
        let buf = buf_get();
        let (res, mut returned_buf) = match timeout(READ_TIMEOUT, backend.read(buf)).await {
            Ok(r) => r,
            Err(_) => {
                let _ = h2_send(resp_tx, notify, H2RespMsg::Reset(2)).await;
                return sent;
            }
        };
        let n = match res {
            Ok(0) => {
                // 終端チャンク前に切断: 空 END_STREAM で閉じる（graceful）。
                buf_put(returned_buf);
                return sent;
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                let _ = h2_send(resp_tx, notify, H2RespMsg::Reset(2)).await;
                return sent;
            }
        };
        returned_buf.set_valid_len(n);
        let drain = h2_drain_chunked_to_msg(
            resp_tx,
            notify,
            &mut decoder,
            returned_buf.as_valid_slice(),
            &mut sent,
        )
        .await;
        buf_put(returned_buf);
        match drain {
            H2ChunkDrain::Done => return sent,
            H2ChunkDrain::NeedMore => {}
        }
    }
}

/// chunked デコード 1 バッファ分の結果。
#[cfg(feature = "http2")]
enum H2ChunkDrain {
    NeedMore,
    Done,
}

/// 1 入力バッファ分の chunked をデコードして [`H2RespMsg::Body`] として送出する。
#[cfg(feature = "http2")]
async fn h2_drain_chunked_to_msg(
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
    decoder: &mut crate::http_utils::ChunkedDecoder,
    data: &[u8],
    sent: &mut u64,
) -> H2ChunkDrain {
    let mut pos = 0;
    while pos < data.len() {
        let span = decoder.next_data_span(&data[pos..]);
        if span.data_len > 0 {
            let start = pos + span.data_start;
            let chunk = Bytes::copy_from_slice(&data[start..start + span.data_len]);
            if h2_send(resp_tx, notify, H2RespMsg::Body(chunk))
                .await
                .is_err()
            {
                return H2ChunkDrain::Done;
            }
            *sent += span.data_len as u64;
        }
        pos += span.consumed;
        if span.complete || span.limit_exceeded {
            return H2ChunkDrain::Done;
        }
        if span.consumed == 0 {
            break;
        }
    }
    H2ChunkDrain::NeedMore
}

/// SendFile バックエンドのファイル配信をメッセージ化する（F-116）。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
async fn h2_sendfile(
    ctx: &H2RequestCtx,
    base_path: &Path,
    is_dir: bool,
    index_file: Option<&str>,
    prefix: &[u8],
    security: &SecurityConfig,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    open_file_cache_config: Option<&cache::OpenFileCacheConfig>,
    #[cfg(feature = "wasm")] wasm_modules: &Arc<Vec<String>>,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64) {
    let path_str_raw = std::str::from_utf8(&ctx.path).unwrap_or("/");
    let path_str = if let Some(qpos) = path_str_raw.find('?') {
        &path_str_raw[..qpos]
    } else {
        path_str_raw
    };
    let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
    let remainder: &str = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
        &path_str[prefix_str.len()..]
    } else {
        path_str
    };

    if remainder.contains("..") {
        return h2_emit_error(resp_tx, notify, 403, b"Forbidden").await;
    }

    let full_path = if is_dir {
        let sub_path = remainder.trim_start_matches('/');
        let mut p = base_path.to_path_buf();
        if !sub_path.is_empty() {
            p.push(sub_path);
        }
        p
    } else {
        let clean_remainder = remainder.trim_matches('/');
        if !clean_remainder.is_empty() {
            return h2_emit_error(resp_tx, notify, 404, b"Not Found").await;
        }
        base_path.to_path_buf()
    };

    // OpenFileCache経由でファイル情報を取得（canonicalize/metadata/mime_guessをキャッシュ、
    // HTTP/1.1 の handle_sendfile と同一経路。キャッシュミス時は offload でブロッキング解決）。
    let file_info = match cache::get_file_info_with_config(&full_path, open_file_cache_config).await
    {
        Some(info) => info,
        None => return h2_emit_error(resp_tx, notify, 404, b"Not Found").await,
    };

    // ディレクトリルートの場合は base_path からの canonical パス封じ込め検査。
    if is_dir {
        if let Some(base_info) =
            cache::get_file_info_with_config(base_path, open_file_cache_config).await
        {
            if !file_info
                .canonical_path
                .starts_with(&base_info.canonical_path)
            {
                return h2_emit_error(resp_tx, notify, 403, b"Forbidden").await;
            }
        }
    }

    // ディレクトリの場合はインデックスファイルを解決する。
    let (final_path, mime_type) = if !file_info.is_file {
        let filename = index_file.unwrap_or("index.html");
        let index_path = file_info.canonical_path.join(filename);
        match cache::get_file_info_with_config(&index_path, open_file_cache_config).await {
            Some(idx_info) if idx_info.is_file => {
                (idx_info.canonical_path.clone(), idx_info.mime_type.clone())
            }
            _ => return h2_emit_error(resp_tx, notify, 403, b"Forbidden").await,
        }
    } else {
        (
            file_info.canonical_path.clone(),
            file_info.mime_type.clone(),
        )
    };

    let data = match crate::runtime::io::read(&final_path).await {
        Ok(d) => d,
        Err(_) => {
            // ファイルが開けない場合はキャッシュを無効化（HTTP/1.1 と同様）。
            cache::invalidate_file_cache(&full_path);
            return h2_emit_error(resp_tx, notify, 404, b"Not Found").await;
        }
    };

    let (built_headers, response_body) = build_h2_compressed_file_response(
        &data,
        &mime_type,
        security,
        compression,
        client_encoding,
    );
    #[cfg(feature = "wasm")]
    let header_store = apply_h2_wasm_response_headers(wasm_modules, 200, built_headers).await;
    #[cfg(not(feature = "wasm"))]
    let header_store = built_headers;
    h2_emit_full(resp_tx, notify, 200, header_store, response_body).await
}

/// リダイレクトレスポンスをメッセージ化する（F-116）。
#[cfg(feature = "http2")]
async fn h2_redirect(
    ctx: &H2RequestCtx,
    redirect_url: &str,
    status_code: u16,
    preserve_path: bool,
    prefix: &[u8],
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64) {
    let path_str = std::str::from_utf8(&ctx.path).unwrap_or("/");
    let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
    let sub_path = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
        &path_str[prefix_str.len()..]
    } else {
        path_str
    };
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

    let mut headers = h2_base_headers(false);
    headers.push((b"location".to_vec(), final_url.into_bytes()));
    if h2_send(
        resp_tx,
        notify,
        H2RespMsg::Head {
            status: status_code,
            headers,
            end_stream: true,
        },
    )
    .await
    .is_err()
    {
        return (status_code, 0);
    }
    (status_code, 0)
}

/// リクエスト方向ストリーミング経路（F-32 統合）。戻り値 `(status, resp_size, req_size)`。
///
/// メインループから `req_rx` 経由で流れてくるボディを chunked でバックエンドへゼロコピー
/// 転送しつつ、レスポンスを [`H2RespMsg`] としてメインループへ流す。`status == 0` は
/// クライアント切断（ログ不要）。
#[cfg(feature = "http2")]
async fn h2_serve_streaming(
    ctx: &H2RequestCtx,
    req_rx: &crate::stream_channel::Receiver<Bytes>,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64, u64) {
    // ルーティング（メインループで適格判定済み。ここでサーバー/パスを解決）。
    let method = &ctx.method[..];
    let path = &ctx.path[..];
    let client_ip: &str = &ctx.client_ip;

    let headers_raw: Vec<(&[u8], &[u8])> = ctx
        .headers
        .iter()
        .filter(|h| !h.name.starts_with(b":"))
        .map(|h| (h.name.as_slice(), h.value.as_slice()))
        .collect();

    let config = CURRENT_CONFIG.load();
    let query_start = path.iter().position(|&b| b == b'?');
    let raw_query: &[u8] = query_start.map(|i| &path[i + 1..]).unwrap_or(b"");
    let path_wo_query = query_start.map(|i| &path[..i]).unwrap_or(path);
    let client_socket_addr = h2_client_socket_addr(client_ip);
    let authority = &ctx.authority[..];

    let backend_result = find_backend_unified(
        authority,
        path_wo_query,
        method,
        &headers_raw,
        raw_query,
        &client_socket_addr,
        config.route.as_slice(),
        &config.upstream_groups,
    )
    .or_else(|| {
        if !authority.is_empty() {
            find_backend_unified(
                b"",
                path_wo_query,
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

    // 適格判定が外れる（config 再読込レース等）場合は req をドレインして 502。
    let (prefix, upstream_group, compression, security) = match backend_result {
        Some((prefix, Backend::Proxy(ug, sec, comp, _buf, _cache, _mods), _rc)) => {
            (prefix, ug, comp, sec)
        }
        _ => {
            while req_rx.recv().await.is_some() {}
            let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
            return (s, sz, 0);
        }
    };

    let server = match upstream_group.select(client_ip) {
        Some(s) => s,
        None => {
            while req_rx.recv().await.is_some() {}
            let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
            return (s, sz, 0);
        }
    };

    let client_encoding = ctx
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(b"accept-encoding"))
        .map(|h| AcceptedEncoding::parse(&h.value))
        .unwrap_or(AcceptedEncoding::Identity);

    server.acquire();
    let target = &server.target;
    let use_tls = target.use_tls;
    let sni = target.sni().to_string();
    let tls_insecure = upstream_group.tls_insecure();
    let addr = HostPortStr::new(&target.host, target.port);
    let addr = addr.as_str();

    // chunked リクエストヘッダ構築。
    let path_str = std::str::from_utf8(path).unwrap_or("/");
    let preserve_grpc_path = ctx
        .headers
        .iter()
        .any(|h| header_pair_is_grpc(&h.name, &h.value));
    let final_path_owned =
        compute_upstream_path(path_str, &prefix, &target.path_prefix, preserve_grpc_path);
    let final_path = final_path_owned.as_str();

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
    for header in ctx.headers.iter() {
        if header.name.starts_with(b":") {
            continue;
        }
        if header.name.eq_ignore_ascii_case(b"connection")
            || header.name.eq_ignore_ascii_case(b"keep-alive")
            || header.name.eq_ignore_ascii_case(b"transfer-encoding")
            || header.name.eq_ignore_ascii_case(b"content-length")
        {
            continue;
        }
        request.extend_from_slice(&header.name);
        request.extend_from_slice(b": ");
        request.extend_from_slice(&header.value);
        request.extend_from_slice(b"\r\n");
    }
    request.extend_from_slice(b"Transfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n");

    // バックエンド接続。
    let backend_tcp = match timeout(CONNECT_TIMEOUT, TcpStream::connect_str(addr)).await {
        Ok(Ok(s)) => s,
        _ => {
            server.release();
            request_buf_put(request);
            while req_rx.recv().await.is_some() {}
            let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
            return (s, sz, 0);
        }
    };

    let (status, resp_size, req_size) = if use_tls {
        let tls_result = if tls_insecure {
            let connector = get_tls_connector_insecure();
            timeout(CONNECT_TIMEOUT, connector.connect(backend_tcp, &sni)).await
        } else {
            let connector = get_tls_connector();
            timeout(CONNECT_TIMEOUT, connector.connect(backend_tcp, &sni)).await
        };
        match tls_result {
            Ok(Ok(mut backend)) => {
                h2_run_streaming_upload(
                    &mut backend,
                    request,
                    req_rx,
                    &compression,
                    client_encoding,
                    resp_tx,
                    notify,
                )
                .await
            }
            _ => {
                request_buf_put(request);
                while req_rx.recv().await.is_some() {}
                let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
                (s, sz, 0)
            }
        }
    } else {
        let mut backend = backend_tcp;
        h2_run_streaming_upload(
            &mut backend,
            request,
            req_rx,
            &compression,
            client_encoding,
            resp_tx,
            notify,
        )
        .await
    };
    let _ = &security;
    server.release();
    (status, resp_size, req_size)
}

/// chunked リクエストヘッド送出 → req チャネルのボディ逐次転送 → レスポンスリレー。
#[cfg(feature = "http2")]
#[allow(clippy::too_many_arguments)]
async fn h2_run_streaming_upload<B>(
    backend: &mut B,
    request: Vec<u8>,
    req_rx: &crate::stream_channel::Receiver<Bytes>,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    resp_tx: &crate::stream_channel::Sender<H2RespMsg>,
    notify: &crate::stream_channel::Notify,
) -> (u16, u64, u64)
where
    B: crate::runtime::io::AsyncReadRent + crate::runtime::io::AsyncWriteRentExt + Unpin,
{
    // ヘッダ送出。
    let (write_res, returned_request) = backend.write_all(request).await;
    request_buf_put(returned_request);
    if write_res.is_err() {
        while req_rx.recv().await.is_some() {}
        let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
        return (s, sz, 0);
    }

    // リクエストボディを chunked で逐次転送。チャネルを消費したら notify で
    // メインループを起こし、req_pending の残りをチャネルへ補充させる（HTTP/3 と同方針。
    // これが無いとチャネル満杯 + メインループ待機で残ボディが流れずデッドロックする）。
    let mut req_size: u64 = 0;
    while let Some(chunk) = req_rx.recv().await {
        req_size = req_size.saturating_add(chunk.len() as u64);
        if send_backend_chunk(backend, chunk).await.is_err() {
            let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
            return (s, sz, req_size);
        }
        notify.notify();
    }
    // 終端チャンク。
    if backend_write_all_bytes(backend, Bytes::from_static(b"0\r\n\r\n"))
        .await
        .is_err()
    {
        let (s, sz) = h2_emit_error(resp_tx, notify, 502, b"Bad Gateway").await;
        return (s, sz, req_size);
    }

    let (status, sent, _reusable) =
        h2_relay_backend_response(backend, compression, client_encoding, resp_tx, notify).await;
    (status, sent, req_size)
}

/// HTTP/2 管理 API（B-29）。conn 非依存で `(status, headers, body)` を返す。
#[cfg(all(feature = "http2", feature = "admin"))]
fn h2_admin_response(
    method: &[u8],
    path: &[u8],
    client_ip: &str,
    headers_raw: &[(&[u8], &[u8])],
) -> Option<(u16, Vec<(Vec<u8>, Vec<u8>)>, Vec<u8>)> {
    let config = CURRENT_CONFIG.load();
    let admin_config = &config.admin_config;
    if !admin_config.enabled {
        return None;
    }
    let path_str = std::str::from_utf8(path).unwrap_or("/");
    let auth = headers_raw.iter().find_map(|(name, value)| {
        if name.eq_ignore_ascii_case(b"authorization") {
            std::str::from_utf8(value).ok()
        } else {
            None
        }
    });

    let is_purge_method = method == b"PURGE";
    let is_admin_purge_path = path_str.starts_with(&admin_config.cache_purge_prefix);

    if is_purge_method || is_admin_purge_path {
        let (status, body) = if !admin_config.is_ip_allowed(client_ip) {
            (403, Vec::new())
        } else if !admin_config.check_auth(auth) {
            (401, Vec::new())
        } else {
            let resp = handle_cache_purge(path_str, is_purge_method);
            parse_http1_admin_response(&resp)
        };
        return Some((status, h2_base_headers(false), body));
    }

    if !path_str.starts_with(&admin_config.path_prefix)
        || method == b"PURGE"
        || path_str.starts_with(&admin_config.cache_purge_prefix)
    {
        return None;
    }

    let path_suffix = &path_str[admin_config.path_prefix.len()..];
    let is_known_endpoint = matches!(
        (method, path_suffix),
        (b"GET", "/config") | (b"GET", "/stats") | (b"POST", "/reload") | (b"POST", "/tls/reload")
    );
    if !is_known_endpoint {
        return None;
    }

    let (status, body) = if !admin_config.is_ip_allowed(client_ip) {
        (403, b"{\"error\":\"403\"}".to_vec())
    } else if !admin_config.check_auth(auth) {
        (401, b"{\"error\":\"401\"}".to_vec())
    } else {
        match (method, path_suffix) {
            (b"GET", "/config") => {
                let json = build_admin_config_json(&config);
                (200, json.into_bytes())
            }
            (b"GET", "/stats") => {
                let uptime_secs = PROXY_START_TIME.elapsed().as_secs();
                let json = format!("{{\"uptime_secs\":{}}}", uptime_secs);
                (200, json.into_bytes())
            }
            (b"POST", "/reload") => {
                use std::sync::atomic::Ordering;
                RELOAD_FLAG.store(true, Ordering::Relaxed);
                (200, b"{\"ok\":true}".to_vec())
            }
            (b"POST", "/tls/reload") => {
                use std::sync::atomic::Ordering;
                TLS_RELOAD_FLAG.store(true, Ordering::Relaxed);
                (200, b"{\"ok\":true}".to_vec())
            }
            _ => (404, Vec::new()),
        }
    };

    let mut headers = h2_base_headers(false);
    headers.push((b"content-type".to_vec(), b"application/json".to_vec()));
    Some((status, headers, body))
}

// ===== F-116: 再利用ヘルパー（従来実装を維持） =====

// ====================
// F-32: HTTP/2 リクエスト方向ストリーミング
// ====================

/// chunked transfer-encoding のチャンクサイズ行（`<hex>\r\n`）を `buf` へ追記する。
///
/// `format!` を使わずに 16 進エンコードする（ホットパスのアロケーション/整形回避）。
#[cfg(feature = "http2")]
fn push_chunk_size_line(buf: &mut Vec<u8>, mut n: usize) {
    if n == 0 {
        buf.push(b'0');
    } else {
        let mut tmp = [0u8; 16];
        let mut i = tmp.len();
        while n > 0 {
            i -= 1;
            let d = (n & 0xf) as u8;
            tmp[i] = if d < 10 { b'0' + d } else { b'a' + (d - 10) };
            n >>= 4;
        }
        buf.extend_from_slice(&tmp[i..]);
    }
    buf.extend_from_slice(b"\r\n");
}

/// 所有バッファ（`Bytes`）をバックエンドへ全量書き込む（部分書き込みを正しく処理）。
///
/// 共通の `write_all`（io.rs）は単一 `write` 前提で部分書き込み時に誤動作するため、
/// ストリーミング経路では本ヘルパーで `write` をループ呼びする。`Bytes` は `advance` で
/// ゼロコピーに前進できるため、再アロケーション・再コピーは発生しない。
#[cfg(feature = "http2")]
async fn backend_write_all_bytes<B>(backend: &mut B, mut buf: Bytes) -> io::Result<()>
where
    B: crate::runtime::io::AsyncWriteRent,
{
    use bytes::Buf;
    while !buf.is_empty() {
        let len = buf.len();
        let (res, returned) = backend.write(buf).await;
        match res {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "backend write returned 0",
                ));
            }
            Ok(n) if n >= len => return Ok(()),
            Ok(n) => {
                let mut b = returned;
                b.advance(n);
                buf = b;
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                buf = returned;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// 1 つの DATA フレーム分のボディを chunked エンコードしてバックエンドへ送る。
///
/// チャンクサイズ行と終端 CRLF のみ小バッファを確保し、**ペイロード本体は受信フレームの
/// 所有バッファ（`Bytes`）をそのまま書き込む（ゼロコピー）**。`writev` 未実装のため 3 回の
/// `write` に分かれるが、ストリーミングバックエンドは Nagle 有効（nodelay 未設定）で
/// カーネルが結合するため実パケット数は抑えられる。
#[cfg(feature = "http2")]
async fn send_backend_chunk<B>(backend: &mut B, data: Bytes) -> io::Result<()>
where
    B: crate::runtime::io::AsyncWriteRent,
{
    if data.is_empty() {
        return Ok(());
    }
    let mut header = Vec::with_capacity(18);
    push_chunk_size_line(&mut header, data.len());
    backend_write_all_bytes(backend, Bytes::from(header)).await?;
    backend_write_all_bytes(backend, data).await?;
    backend_write_all_bytes(backend, Bytes::from_static(b"\r\n")).await?;
    Ok(())
}

/// HTTP/1.1 レスポンスバイト列からステータスコードとボディを抽出（管理 API Purge 用）
#[cfg(all(feature = "http2", feature = "admin"))]
fn parse_http1_admin_response(resp: &[u8]) -> (u16, Vec<u8>) {
    let status = if resp.starts_with(b"HTTP/1.1 200") {
        200
    } else if resp.starts_with(b"HTTP/1.1 401") {
        401
    } else if resp.starts_with(b"HTTP/1.1 403") {
        403
    } else if resp.starts_with(b"HTTP/1.1 503") {
        503
    } else if resp.starts_with(b"HTTP/1.1 501") {
        501
    } else {
        400
    };
    let body = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| resp[i + 4..].to_vec())
        .unwrap_or_default();
    (status, body)
}

/// HTTP/2 応答ヘッダーへ WASM レスポンスフィルタを適用（B-30）
#[cfg(all(feature = "http2", feature = "wasm"))]
async fn apply_h2_wasm_response_headers(
    wasm_modules: &Arc<Vec<String>>,
    status: u16,
    mut header_store: Vec<(Vec<u8>, Vec<u8>)>,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    if wasm_modules.is_empty() {
        return header_store;
    }
    let config = CURRENT_CONFIG.load();
    let Some(ref wasm_engine) = config.wasm_filter_engine else {
        return header_store;
    };

    let wasm_result = wasm_engine
        .clone()
        .on_response_headers_with_modules_async(
            wasm_modules.clone(),
            status,
            header_store.clone(),
            true,
        )
        .await;

    if let crate::wasm::FilterResult::Continue {
        headers: modified_headers,
        ..
    } = wasm_result
    {
        header_store = modified_headers;
    }
    header_store
}

/// HTTP/2 静的応答の圧縮ネゴシエーションとヘッダー構築（B-32）
#[cfg(feature = "http2")]
fn build_h2_compressed_file_response(
    data: &[u8],
    mime_type: &str,
    security: &SecurityConfig,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
) -> (Vec<(Vec<u8>, Vec<u8>)>, Vec<u8>) {
    let should_compress = compression.should_compress(
        client_encoding,
        Some(mime_type.as_bytes()),
        Some(data.len()),
        None,
    );

    let mut header_store: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(8);
    header_store.push((b"content-type".to_vec(), mime_type.as_bytes().to_vec()));
    if let Some(ref g) = get_server_header_guard() {
        let (n, v) = g.as_header();
        header_store.push((n.to_vec(), v.to_vec()));
    }
    for (k, v) in &security.add_response_headers {
        header_store.push((k.as_bytes().to_vec(), v.as_bytes().to_vec()));
    }
    // F-94: HTTP/3 広告（Alt-Svc）
    if let Some(g) = get_alt_svc_guard() {
        let (n, v) = g.as_header();
        header_store.push((n.to_vec(), v.to_vec()));
    }

    let response_body = if let Some(enc) = should_compress {
        let encoding_name: &[u8] = match enc {
            AcceptedEncoding::Zstd => b"zstd",
            AcceptedEncoding::Brotli => b"br",
            AcceptedEncoding::Gzip => b"gzip",
            AcceptedEncoding::Deflate => b"deflate",
            AcceptedEncoding::Identity => b"",
        };
        if !encoding_name.is_empty() {
            header_store.push((b"content-encoding".to_vec(), encoding_name.to_vec()));
            header_store.push((b"vary".to_vec(), b"Accept-Encoding".to_vec()));
        }
        compress_body_h2(data, enc, compression)
    } else {
        data.to_vec()
    };

    (header_store, response_body)
}

/// F-106: h2c バックエンドへ新規接続し HTTP/2 ハンドシェイクまで完了する。
/// 失敗時は送出すべきステータス（502=接続/ハンドシェイク失敗, 504=接続タイムアウト）を返す。
#[cfg(feature = "http2")]
async fn h2c_connect_and_handshake(
    addr: &str,
) -> Result<http2::H2cClient<crate::runtime::tcp::TcpStream>, u16> {
    let connect_result = timeout(CONNECT_TIMEOUT, TcpStream::connect_str(addr)).await;
    let backend_stream = match connect_result {
        Ok(Ok(stream)) => {
            let _ = stream.set_nodelay(true);
            stream
        }
        Ok(Err(e)) => {
            warn!("[HTTP/2] H2C backend connect error ({}): {}", addr, e);
            return Err(502);
        }
        Err(_) => {
            warn!("[HTTP/2] H2C backend connect timeout ({})", addr);
            return Err(504);
        }
    };
    let settings = http2::Http2Settings::default();
    let mut client = http2::H2cClient::new(backend_stream, settings);
    if let Err(e) = client.handshake().await {
        warn!("[HTTP/2] H2C handshake error ({}): {}", addr, e);
        return Err(502);
    }
    Ok(client)
}

/// HTTP/2 用レスポンスボディ圧縮ヘルパー関数
///
/// バイト配列を受け取り、指定されたエンコーディングで圧縮して返します。
/// 圧縮に失敗した場合は元のデータをそのまま返します。
#[cfg(all(feature = "http2", feature = "compression"))]
fn compress_body_h2(
    body: &[u8],
    encoding: AcceptedEncoding,
    compression: &CompressionConfig,
) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
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
fn compress_body_h2(
    body: &[u8],
    _encoding: AcceptedEncoding,
    _compression: &CompressionConfig,
) -> Vec<u8> {
    body.to_vec()
}

// ====================
// プロトコル検出機能（H2C対応）
// ====================

/// プロトコルタイプ
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolType {
    H2C,     // HTTP/2 Cleartext (Prior Knowledge)
    TLS,     // TLS (HTTPS)
    Http11,  // HTTP/1.1
    Unknown, // 不明
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

impl<S> crate::runtime::io::BufferedReadState for BufferedStream<S> {
    /// プロトコル検出時の先読みデータが未消費なら `true`（消費し切ると `buffer` は `None`）。
    /// F-116: HTTP/2 多重化メインループの可読待機前チェックに使う。
    #[inline]
    fn has_buffered_read_data(&self) -> bool {
        self.buffer.is_some()
    }
}

impl<S: AsyncReadRent + Unpin> AsyncReadRent for BufferedStream<S> {
    async fn read<T: IoBufMut>(&mut self, mut buf: T) -> crate::runtime::io::BufResult<usize, T> {
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

    async fn readv<T: IoVecBufMut>(&mut self, buf: T) -> crate::runtime::io::BufResult<usize, T> {
        // 現在の実装では readv は使用しないため、バッファがある場合は未サポート
        if self.buffer.is_some() {
            return (
                Err(io::Error::other(
                    "readv not supported for BufferedStream with data",
                )),
                buf,
            );
        }
        self.inner.readv(buf).await
    }
}

impl<S: crate::runtime::io::AsyncWriteRent + Unpin> crate::runtime::io::AsyncWriteRent
    for BufferedStream<S>
{
    async fn write<T: IoBuf>(&mut self, buf: T) -> crate::runtime::io::BufResult<usize, T> {
        self.inner.write(buf).await
    }

    async fn writev<T: IoVecBuf>(&mut self, buf: T) -> crate::runtime::io::BufResult<usize, T> {
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
pub async fn detect_protocol_with_buffer(stream: &mut TcpStream) -> (ProtocolType, Vec<u8>) {
    // ソケットバッファを MSG_PEEK で覗き見て（消費せず）プロトコルを判別する。
    //
    // 以前は io_uring RECV でバイトを消費し initial_data として持ち回していたが、
    // タイムアウトキャンセルや所有権の絡みでバイトが欠落/破損し、後続の TLS ハンドシェイクが
    // `received corrupt message / InvalidContentType` で失敗する不具合があった
    // （h2c 検出が有効な全接続で発生しうる）。MSG_PEEK ならバイトはソケットに残るため、
    // 判別後に TLS/H2C/HTTP1.1 各ハンドラがそのまま読み直せる（initial_data は常に空）。
    let fd = stream.as_raw_fd();
    let start_time = std::time::Instant::now();
    let timeout_duration = Duration::from_millis(200);
    let mut peeked = [0u8; 24];
    let mut n = 0usize;

    loop {
        let remaining_timeout = match timeout_duration.checked_sub(start_time.elapsed()) {
            Some(d) if d.as_millis() > 0 => d,
            _ => break,
        };

        // データ到着を待つ（MSG_PEEK は消費しないのでタイムアウトキャンセルしても安全）。
        match timeout(remaining_timeout, stream.readable()).await {
            Ok(Ok(())) => {}
            _ => break,
        }

        // 消費せずに覗き見る（毎回ソケットバッファの先頭から最大 24 バイト）。
        let ret = unsafe {
            libc::recv(
                fd,
                peeked.as_mut_ptr() as *mut libc::c_void,
                peeked.len(),
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        if ret < 0 {
            // 偽の readable 通知（EAGAIN）なら再試行、その他はフォールバック。
            if io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock {
                continue;
            }
            break;
        }
        if ret == 0 {
            break; // 接続終了
        }
        n = ret as usize;
        // TLS/HTTP1.1 は 5 バイト、H2C プリフェースは 24 バイトで判別可能。
        if n >= 24 || n >= 5 {
            break;
        }
    }

    let data = &peeked[..n];

    // HTTP/2 プリフェース検出（24バイト固定）。MSG_PEEK なので initial_data は空で返す。
    if n >= 24 && data == b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n" {
        debug!("[Protocol Detection] H2C (HTTP/2 Cleartext) detected");
        return (ProtocolType::H2C, Vec::new());
    }

    if n >= 5 {
        if data[0] == 0x16 && data[1] == 0x03 && (data[2] >= 0x01 && data[2] <= 0x03) {
            debug!("[Protocol Detection] TLS detected");
            return (ProtocolType::TLS, Vec::new());
        }

        let methods: [&[u8]; 9] = [
            b"GET ", b"POST", b"PUT ", b"DELE", b"HEAD", b"OPTI", b"CONN", b"TRAC", b"PATC",
        ];
        for method in &methods {
            if data.starts_with(method) {
                debug!("[Protocol Detection] HTTP/1.1 detected");
                return (ProtocolType::Http11, Vec::new());
            }
        }
    }

    if n > 0 {
        debug!(
            "[Protocol Detection] Unknown protocol ({} bytes): hex={:02x?}",
            n, data
        );
    }

    (ProtocolType::Unknown, Vec::new())
}

/// H2Cサーバー接続処理
///
/// TLSなしでHTTP/2コネクションを確立し、リクエストを処理します。
#[cfg(feature = "http2")]
pub async fn handle_h2c_connection(stream: TcpStream, client_ip: &str, initial_data: Vec<u8>) {
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
    #[cfg_attr(not(feature = "http2"), allow(unused_mut))] mut stream: TcpStream,
    acceptor: RustlsAcceptor,
    peer_addr: SocketAddr,
) {
    #[cfg_attr(not(feature = "http2"), allow(unused_mut))]
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
                    let client_ip = IpStr::new(peer_addr.ip());
                    handle_h2c_connection(
                        stream,
                        client_ip.as_str(),
                        initial_buffer.take().unwrap(),
                    )
                    .await;
                    return;
                }
                ProtocolType::Http11 => {
                    // HTTP/1.1ハンドラー（平文接続）
                    // TLSハンドシェイクをスキップして、平文ストリームとして処理
                    let plain_stream =
                        match acceptor.accept_plain(stream, initial_buffer.take()).await {
                            Ok(s) => s,
                            Err(e) => {
                                warn!("Failed to create plain stream: {}", e);
                                return;
                            }
                        };
                    let client_ip = IpStr::new(peer_addr.ip());
                    handle_requests(plain_stream, client_ip.as_str(), peer_addr).await;
                    return;
                }
                ProtocolType::TLS => {
                    // TLSハンドシェイク（既存処理へ）
                }
                ProtocolType::Unknown => {
                    warn!(
                        "[H2C] Unknown protocol from {}, falling back to TLS",
                        peer_addr
                    );
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

    // クライアントIPアドレスをスタックバッファへ変換（F-41: 接続ごとのヒープ確保排除）
    let client_ip = IpStr::new(peer_addr.ip());

    // HTTP/2 が有効かつネゴシエートされた場合は HTTP/2 ハンドラーを使用
    #[cfg(feature = "http2")]
    if http2_enabled && tls_stream.is_http2() {
        handle_http2_connection(tls_stream, client_ip.as_str()).await;
        return;
    }

    // HTTP/1.1 ハンドラー
    handle_requests(tls_stream, client_ip.as_str(), peer_addr).await;
}

// kTLS 無効時の接続処理（rustls のみ）
#[cfg(not(feature = "ktls"))]
#[cfg_attr(not(feature = "http2"), allow(unused_mut))]
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
                    let client_ip = IpStr::new(peer_addr.ip());
                    handle_h2c_connection(
                        stream,
                        client_ip.as_str(),
                        initial_buffer.take().unwrap(),
                    )
                    .await;
                    return;
                }
                ProtocolType::Http11 => {
                    // HTTP/1.1ハンドラー（平文接続）
                    // TLSハンドシェイクをスキップして、平文ストリームとして処理
                    let plain_stream =
                        match acceptor.accept_plain(stream, initial_buffer.take()).await {
                            Ok(s) => s,
                            Err(e) => {
                                warn!("Failed to create plain stream: {}", e);
                                return;
                            }
                        };
                    let client_ip = IpStr::new(peer_addr.ip());
                    handle_requests(plain_stream, client_ip.as_str(), peer_addr).await;
                    return;
                }
                ProtocolType::TLS => {
                    // TLSハンドシェイク（既存処理へ）
                }
                ProtocolType::Unknown => {
                    warn!(
                        "[H2C] Unknown protocol from {}, falling back to TLS",
                        peer_addr
                    );
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

    // クライアントIPアドレスをスタックバッファへ変換（F-41: 接続ごとのヒープ確保排除）
    let client_ip = IpStr::new(peer_addr.ip());

    // HTTP/2 が有効かつネゴシエートされた場合は HTTP/2 ハンドラーを使用
    #[cfg(feature = "http2")]
    if http2_enabled && tls_stream.is_http2() {
        handle_http2_connection(tls_stream, client_ip.as_str()).await;
        return;
    }

    // HTTP/1.1 ハンドラー
    handle_requests(tls_stream, client_ip.as_str(), peer_addr).await;
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
                    Ok(0) => break,  // EOF: クライアントが書き込みをクローズ
                    Ok(_) => {}      // 読み捨てて継続（データがある間は読み続ける）
                    Err(_) => break, // 読み取りエラー
                }
            }
            Err(_) => break, // 200ms データ無し＝受信バッファは空。安全にクローズ可能
        }
    }
}

// 統一されたリクエスト処理ループ（型エイリアスを使用）
// clippy::drop_non_drop 許容理由: `req` はヘッダバッファ（accumulated）への借用を保持する
// 非 Drop 型で、`drop(req)` は借用領域を明示的に終わらせて後続の可変利用を許すための
// 意図的な記述（`let _ =` より意図が明確なため維持する）。
#[allow(clippy::drop_non_drop)]
async fn handle_requests(mut tls_stream: ServerTls, client_ip: &str, peer_addr: SocketAddr) {
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
            let response_code =
                if let Some(line_end) = accumulated.windows(2).position(|w| w == b"\r\n") {
                    // リクエストラインが完全に含まれている場合、URIサイズを確認
                    let request_line = &accumulated[..line_end];
                    // "GET /path HTTP/1.1" 形式から URI 部分を抽出
                    let uri_too_long = request_line
                        .iter()
                        .position(|&b| b == b' ')
                        .and_then(|method_end| {
                            let after_method = &request_line[method_end + 1..];
                            after_method.iter().rposition(|&b| b == b' ')
                        })
                        .is_some_and(|uri_len| uri_len > MAX_HEADER_SIZE);

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
                let method_bytes: Box<[u8]> = req
                    .method
                    .map(|m| m.as_bytes().into())
                    .unwrap_or_else(|| Box::from(b"GET" as &[u8]));

                // 有効なHTTPメソッドのみ受け付ける（RFC 7231）
                const VALID_HTTP_METHODS: &[&[u8]] = &[
                    b"GET", b"HEAD", b"POST", b"PUT", b"DELETE", b"CONNECT", b"OPTIONS", b"TRACE",
                    b"PATCH",
                ];
                if !VALID_HTTP_METHODS
                    .iter()
                    .any(|m| method_bytes.as_ref().eq_ignore_ascii_case(m))
                {
                    drop(req);
                    let err_buf = ERR_MSG_METHOD_NOT_ALLOWED.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                    return;
                }

                // ヘッダー情報抽出
                let host_bytes: Box<[u8]> = req
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case("host"))
                    .map(|h| Box::from(h.value))
                    .unwrap_or_else(|| Box::from([] as [u8; 0]));

                // メトリクス: 最初のリクエストでホスト名を取得し、インクリメント
                if let Ok(host_str) = std::str::from_utf8(&host_bytes) {
                    connection_metric.set_host(host_str.to_string());
                } else {
                    connection_metric.set_host("unknown".to_string());
                }

                let path_bytes: Box<[u8]> = req
                    .path
                    .map(|p| p.as_bytes().into())
                    .unwrap_or_else(|| Box::from(b"/" as &[u8]));

                let user_agent: Box<[u8]> = req
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case("user-agent"))
                    .map(|h| Box::from(h.value))
                    .unwrap_or_else(|| Box::from([] as [u8; 0]));

                // Content-Length ヘッダーの値を取得し、不正な値の場合は400 Bad Requestを返す
                // 複数の Content-Length ヘッダーは RFC 7230 Section 3.3.2 違反 → 400
                let cl_headers: Vec<_> = req
                    .headers
                    .iter()
                    .filter(|h| h.name.eq_ignore_ascii_case("content-length"))
                    .collect();

                if cl_headers.len() > 1 {
                    drop(req);
                    let err_buf = ERR_MSG_BAD_REQUEST.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                    return;
                }

                let content_length: usize = if let Some(cl_header) = cl_headers.first() {
                    match std::str::from_utf8(cl_header.value)
                        .ok()
                        .and_then(|s| s.trim().parse::<usize>().ok())
                    {
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

                // B-23: リクエストフレーミングを RFC 7230 §3.3.3 準拠で分類し、HTTP リクエスト
                // スマグリング（CL.TE / TE.CL）要因を転送前に一律 400 で拒否する。
                // 従来は `content_length > 0 && is_chunked` のみを弾いており、
                // `Content-Length: 0` + `Transfer-Encoding: chunked`（CL の値に依らない CL.TE）や
                // 最終エンコーディングが chunked でない TE を取りこぼしていた。
                let is_chunked: bool = match classify_request_framing(
                    req.headers.iter().map(|h| (h.name.as_bytes(), h.value)),
                ) {
                    Ok(RequestFraming::Chunked) => true,
                    Ok(RequestFraming::ContentLength) => false,
                    Err(_) => {
                        drop(req);
                        let err_buf = ERR_MSG_BAD_REQUEST.to_vec();
                        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                        return;
                    }
                };

                // Connection ヘッダーチェック（Keep-Alive / Upgrade対応）
                let connection_header: Option<&[u8]> = req
                    .headers
                    .iter()
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
                        v.to_ascii_lowercase().windows(7).any(|w| w == b"upgrade")
                    })
                    .unwrap_or(false);

                let is_websocket_upgrade: bool = req
                    .headers
                    .iter()
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

                let headers_for_proxy: Vec<(Box<[u8]>, Box<[u8]>)> = req
                    .headers
                    .iter()
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
                    let write_result = timeout(
                        WRITE_TIMEOUT,
                        tls_stream.write_all(HTTP_100_CONTINUE.to_vec()),
                    )
                    .await;
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
                            log_access(
                                &method_bytes,
                                &host_bytes,
                                &path_bytes,
                                &user_agent,
                                0,
                                403,
                                0,
                                start_instant,
                                client_ip,
                                "",
                            );
                            accumulated.clear();
                            return;
                        }

                        let metrics_response = build_metrics_response();
                        let resp_size = metrics_response.len() as u64;

                        let write_result =
                            timeout(WRITE_TIMEOUT, tls_stream.write_all(metrics_response)).await;
                        if let Ok((Ok(_), _)) = write_result {
                            log_access(
                                &method_bytes,
                                &host_bytes,
                                &path_bytes,
                                &user_agent,
                                0,
                                200,
                                resp_size,
                                start_instant,
                                client_ip,
                                "",
                            );
                        }

                        // メトリクスエンドポイントは常に接続を閉じる
                        accumulated.clear();
                        return;
                    }
                }

                // 管理 API エンドポイントの処理（F-20: キャッシュ Purge）
                // PURGE メソッド、または admin.path_prefix 配下の /cache/purge を処理する
                #[cfg(feature = "admin")]
                {
                    let config = CURRENT_CONFIG.load();
                    let admin_config = &config.admin_config;
                    let is_purge_method = method_bytes.as_ref() == b"PURGE";
                    // パフォーマンス最適化: 事前計算済みプレフィックスを使用（format! 排除）
                    let is_admin_purge_path =
                        path_str.starts_with(&admin_config.cache_purge_prefix);

                    if admin_config.enabled && (is_purge_method || is_admin_purge_path) {
                        let start_instant = Instant::now();

                        // IP制限チェック
                        let response = if !admin_config.is_ip_allowed(client_ip) {
                            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                        } else {
                            // Authorization ヘッダーを取得
                            let auth = headers_for_proxy.iter().find_map(|(name, value)| {
                                if name.eq_ignore_ascii_case(b"authorization") {
                                    std::str::from_utf8(value).ok()
                                } else {
                                    None
                                }
                            });

                            if !admin_config.check_auth(auth) {
                                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                            } else {
                                handle_cache_purge(path_str, is_purge_method)
                            }
                        };

                        let resp_size = response.len() as u64;
                        let status = if response.starts_with(b"HTTP/1.1 200") {
                            200
                        } else if response.starts_with(b"HTTP/1.1 401") {
                            401
                        } else if response.starts_with(b"HTTP/1.1 403") {
                            403
                        } else {
                            400
                        };
                        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(response)).await;
                        log_access(
                            &method_bytes,
                            &host_bytes,
                            &path_bytes,
                            &user_agent,
                            0,
                            status,
                            resp_size,
                            start_instant,
                            client_ip,
                            "",
                        );
                        accumulated.clear();
                        return;
                    }
                }

                // 管理 API エンドポイントの処理（F-21: /config, /stats, /reload, /tls/reload）
                #[cfg(feature = "admin")]
                {
                    let config = CURRENT_CONFIG.load();
                    let admin_config = &config.admin_config;

                    if admin_config.enabled
                        && path_str.starts_with(&admin_config.path_prefix)
                        && method_bytes.as_ref() != b"PURGE"
                        && !path_str.starts_with(&admin_config.cache_purge_prefix)
                    {
                        // GET /__admin/config, GET /__admin/stats,
                        // POST /__admin/reload, POST /__admin/tls/reload のみを処理
                        let path_suffix = &path_str[admin_config.path_prefix.len()..];
                        let is_known_endpoint = matches!(
                            (method_bytes.as_ref(), path_suffix),
                            (b"GET", "/config")
                                | (b"GET", "/stats")
                                | (b"POST", "/reload")
                                | (b"POST", "/tls/reload")
                        );

                        if is_known_endpoint {
                            let start_instant = Instant::now();

                            // IP制限チェック
                            let response: Vec<u8> = if !admin_config.is_ip_allowed(client_ip) {
                                b"HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"error\":\"403\"}".to_vec()
                            } else {
                                // Authorization ヘッダーを取得
                                let auth = headers_for_proxy.iter().find_map(|(name, value)| {
                                    if name.eq_ignore_ascii_case(b"authorization") {
                                        std::str::from_utf8(value).ok()
                                    } else {
                                        None
                                    }
                                });

                                if !admin_config.check_auth(auth) {
                                    b"HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"error\":\"401\"}".to_vec()
                                } else {
                                    // 認証成功: エンドポイントに応じた処理
                                    match (method_bytes.as_ref(), path_suffix) {
                                        (b"GET", "/config") => {
                                            // 設定情報をJSON形式で返す（secret はマスク）
                                            let body = build_admin_config_json(&config);
                                            let mut resp = format!(
                                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                                body.len()
                                            ).into_bytes();
                                            resp.extend_from_slice(body.as_bytes());
                                            resp
                                        }
                                        (b"GET", "/stats") => {
                                            // 起動からの経過時間を返す
                                            // PROXY_START_TIME を初回アクセスで初期化
                                            let uptime_secs = PROXY_START_TIME.elapsed().as_secs();
                                            let body = format!("{{\"uptime_secs\":{}}}", uptime_secs);
                                            let mut resp = format!(
                                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                                body.len()
                                            ).into_bytes();
                                            resp.extend_from_slice(body.as_bytes());
                                            resp
                                        }
                                        (b"POST", "/reload") => {
                                            // 設定リロードフラグを立てる
                                            use std::sync::atomic::Ordering;
                                            RELOAD_FLAG.store(true, Ordering::Relaxed);
                                            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10\r\nConnection: close\r\n\r\n{\"ok\":true}".to_vec()
                                        }
                                        (b"POST", "/tls/reload") => {
                                            // TLS証明書リロードフラグを立てる
                                            use std::sync::atomic::Ordering;
                                            TLS_RELOAD_FLAG.store(true, Ordering::Relaxed);
                                            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10\r\nConnection: close\r\n\r\n{\"ok\":true}".to_vec()
                                        }
                                        _ => {
                                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                                        }
                                    }
                                }
                            };

                            let resp_size = response.len() as u64;
                            let status = if response.starts_with(b"HTTP/1.1 200") {
                                200
                            } else if response.starts_with(b"HTTP/1.1 401") {
                                401
                            } else if response.starts_with(b"HTTP/1.1 403") {
                                403
                            } else {
                                400
                            };
                            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(response)).await;
                            log_access(
                                &method_bytes,
                                &host_bytes,
                                &path_bytes,
                                &user_agent,
                                0,
                                status,
                                resp_size,
                                start_instant,
                                client_ip,
                                "",
                            );
                            accumulated.clear();
                            return;
                        }
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
                    let headers_raw: Vec<(&[u8], &[u8])> = req
                        .headers
                        .iter()
                        .filter(|h| !h.name.is_empty())
                        .map(|h| (h.name.as_bytes(), h.value))
                        .collect();
                    let raw_query: &[u8] =
                        query_start_pos.map(|i| &path_bytes[i + 1..]).unwrap_or(b"");

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

                let (prefix, backend, _route_compression) = match backend_result {
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
                    let is_allowed = security
                        .allowed_methods
                        .iter()
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
                if security.rate_limit_requests_per_min > 0
                    && !check_rate_limit(client_ip, security.rate_limit_requests_per_min)
                {
                    let err_buf = ERR_MSG_TOO_MANY_REQUESTS.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                    return;
                }

                // 初期ボディ（ヘッダー後のデータ）
                let initial_body: Vec<u8> = if header_len < accumulated.len() {
                    accumulated[header_len..].to_vec()
                } else {
                    Vec::new()
                };

                // WASMモジュールの適用
                // モジュールリストをローカル変数として保持（スレッドローカルを使わない、並行タスク間の干渉を防ぐ）
                // F-43: モジュールリストは Arc 共有（リクエストごとの deep copy 排除）
                #[cfg(feature = "wasm")]
                let modules_to_apply: Arc<Vec<String>> =
                    if let Some(backend_modules) = backend.modules_arc() {
                        backend_modules.clone()
                    } else {
                        crate::wasm::empty_wasm_modules()
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
                            let headers_vec: Vec<(Vec<u8>, Vec<u8>)> = headers_for_proxy
                                .iter()
                                .map(|(k, v)| (k.to_vec(), v.to_vec()))
                                .collect();

                            let wasm_result = wasm_engine
                                .clone()
                                .on_request_headers_with_modules_async(
                                    modules_to_apply.clone(),
                                    Arc::from(path_str),
                                    Arc::from(method_str),
                                    headers_vec,
                                    Arc::from(client_ip),
                                    initial_body.is_empty() && !is_chunked, // end_of_stream
                                )
                                .await;

                            match wasm_result {
                                crate::wasm::FilterResult::Continue {
                                    headers: modified_headers,
                                    ..
                                } => {
                                    // 修正されたヘッダーを使用
                                    // F-43: 所有権ムーブで変換（deep copy しない）
                                    modified_headers
                                        .into_iter()
                                        .map(|(k, v)| (k.into_boxed_slice(), v.into_boxed_slice()))
                                        .collect()
                                }
                                crate::wasm::FilterResult::LocalResponse(resp) => {
                                    // ローカルレスポンスを返送
                                    let status_line = format!(
                                        "HTTP/1.1 {} {}\r\n",
                                        resp.status_code,
                                        match resp.status_code {
                                            200 => "OK",
                                            404 => "Not Found",
                                            403 => "Forbidden",
                                            500 => "Internal Server Error",
                                            _ => "Unknown",
                                        }
                                    );
                                    let mut response = status_line.into_bytes();
                                    for (k, v) in &resp.headers {
                                        response.extend_from_slice(k);
                                        response.extend_from_slice(b": ");
                                        response.extend_from_slice(v);
                                        response.extend_from_slice(b"\r\n");
                                    }
                                    // F-62: モジュールが Content-Length を設定していない場合は
                                    // 補完する（欠落するとクライアントは接続クローズまで
                                    // ボディを待ち続け、TLS では close_notify 無しの EOF
                                    // エラーになる）
                                    let has_content_length = resp
                                        .headers
                                        .iter()
                                        .any(|(k, _)| k.eq_ignore_ascii_case(b"content-length"));
                                    if !has_content_length {
                                        response.extend_from_slice(
                                            format!("Content-Length: {}\r\n", resp.body.len())
                                                .as_bytes(),
                                        );
                                    }
                                    response.extend_from_slice(b"\r\n");
                                    response.extend_from_slice(&resp.body);

                                    let start_instant = Instant::now();
                                    let resp_size = response.len() as u64;
                                    let write_result =
                                        timeout(WRITE_TIMEOUT, tls_stream.write_all(response))
                                            .await;
                                    if let Ok((Ok(_), _)) = write_result {
                                        log_access(
                                            &method_bytes,
                                            &host_bytes,
                                            &path_bytes,
                                            &user_agent,
                                            0,
                                            resp.status_code,
                                            resp_size,
                                            start_instant,
                                            client_ip,
                                            "",
                                        );
                                        // WASMライフサイクルコールバック: リクエスト完了
                                        crate::wasm::on_request_complete_async(
                                            wasm_engine.clone(),
                                            modules_to_apply.clone(),
                                        )
                                        .await;
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
                        debug!(
                            "WebSocket upgrade request detected for path: {}",
                            std::str::from_utf8(&path_bytes).unwrap_or("-")
                        );

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
                        )
                        .await;

                        server.release();

                        if let Some((status, resp_size)) = ws_result {
                            log_access(
                                &method_bytes,
                                &host_bytes,
                                &path_bytes,
                                &user_agent,
                                content_length as u64,
                                status,
                                resp_size,
                                start_instant,
                                client_ip,
                                "",
                            );

                            // WASMライフサイクルコールバック: リクエスト完了
                            #[cfg(feature = "wasm")]
                            {
                                if !modules_to_apply.is_empty() {
                                    let config = CURRENT_CONFIG.load();
                                    if let Some(ref wasm_engine) = config.wasm_filter_engine {
                                        crate::wasm::on_request_complete_async(
                                            wasm_engine.clone(),
                                            modules_to_apply.clone(),
                                        )
                                        .await;
                                    }
                                }
                            }
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
                        {
                            modules_to_apply.clone()
                        }
                        #[cfg(not(feature = "wasm"))]
                        {
                            Arc::new(Vec::new())
                        }
                    },
                    client_ip,
                )
                .await;

                match result {
                    Some((stream_back, status, resp_size, should_close)) => {
                        log_access(
                            &method_bytes,
                            &host_bytes,
                            &path_bytes,
                            &user_agent,
                            content_length as u64,
                            status,
                            resp_size,
                            start_instant,
                            client_ip,
                            "",
                        );

                        // WASMライフサイクルコールバック: リクエスト完了
                        #[cfg(feature = "wasm")]
                        {
                            if !modules_to_apply.is_empty() {
                                let config = CURRENT_CONFIG.load();
                                if let Some(ref wasm_engine) = config.wasm_filter_engine {
                                    crate::wasm::on_request_complete_async(
                                        wasm_engine.clone(),
                                        modules_to_apply.clone(),
                                    )
                                    .await;
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
                Ok((Ok(0), b)) => {
                    buf_put(b);
                    return false;
                } // EOF: 不完全
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
                Ok((Ok(0), b)) => {
                    buf_put(b);
                    return false;
                } // EOF: 不完全
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
    wasm_modules: Arc<Vec<String>>,
    client_ip: &str,
) -> Option<(ServerTls, u16, u64, bool)> {
    // Proxy バックエンドはリクエストボディを上流へ転送して消費する。それ以外（File/Memory/
    // Redirect 等のローカル応答）はボディを読まないため、keep-alive 接続でボディが次の
    // リクエストに混入して 400 desync を起こす。ローカル応答の前に残りのボディを読み捨てる。
    if !matches!(backend, Backend::Proxy(..))
        && (is_chunked || content_length > initial_body.len())
        && !drain_request_body(&mut tls_stream, content_length, is_chunked, initial_body).await
    {
        // ドレイン失敗（接続が汚染されている可能性）→ 接続を閉じる
        return Some((tls_stream, 400, 0, true));
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
                client_ip,
            )
            .await
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
            // F-94: HTTP/3 広告（Alt-Svc）
            append_alt_svc_header_line(&mut header);

            // WASMレスポンスヘッダーフィルタを適用（後段で Connection ヘッダーを追記するため mut）
            #[cfg(feature = "wasm")]
            let mut header = {
                ftlog::debug!(
                    "[WASM Response] MemoryFile: wasm_modules count = {}",
                    wasm_modules.len()
                );
                if !wasm_modules.is_empty() {
                    let config = CURRENT_CONFIG.load();
                    if let Some(ref wasm_engine) = config.wasm_filter_engine {
                        // 現在のヘッダーをVec<(Vec<u8>, Vec<u8>)>形式に変換
                        let header_str = String::from_utf8_lossy(&header);
                        let current_headers: Vec<(Vec<u8>, Vec<u8>)> = header_str
                            .lines()
                            .skip(1) // ステータス行をスキップ
                            .filter_map(|line| {
                                let line_trimmed =
                                    line.trim_end_matches("\r\n").trim_end_matches("\r");
                                if line_trimmed.is_empty() {
                                    return None;
                                }
                                let colon_pos = line_trimmed.find(':')?;
                                let name = line_trimmed.as_bytes()[..colon_pos].to_vec();
                                let value = line_trimmed[colon_pos + 1..]
                                    .trim_start()
                                    .as_bytes()
                                    .to_vec();
                                Some((name, value))
                            })
                            .collect();

                        // WASMフィルタを実行（レスポンスヘッダー処理）
                        let wasm_result = wasm_engine
                            .clone()
                            .on_response_headers_with_modules_async(
                                wasm_modules.clone(),
                                200,
                                current_headers,
                                true, // end_of_stream
                            )
                            .await;

                        match wasm_result {
                            crate::wasm::FilterResult::Continue {
                                headers: modified_headers,
                                ..
                            } => {
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

            // Connection header を追加（header は上で `let mut` 済み）
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
                Ok((Ok(_), _)) => Some((tls_stream, 200, data_len, client_wants_close)),
                _ => None,
            }
        }
        Backend::SendFile(
            base_path,
            is_dir,
            index_file,
            security,
            _cache,
            open_file_cache_config,
            _,
        ) => {
            // Range ヘッダーを抽出 (RFC 7233)
            let range_header = headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(b"range"))
                .map(|(_, v)| v.as_ref());
            handle_sendfile(
                tls_stream,
                &base_path,
                is_dir,
                index_file.as_deref(),
                req_path,
                &prefix,
                client_wants_close,
                &security,
                range_header,
                open_file_cache_config.as_deref(),
                wasm_modules,
            )
            .await
        }
        Backend::Redirect(redirect_url, status_code, preserve_path, _) => {
            handle_redirect(
                tls_stream,
                &redirect_url,
                status_code,
                preserve_path,
                req_path,
                &prefix,
                client_wants_close,
            )
            .await
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
        Ok((Ok(_), _)) => Some((tls_stream, status_code, 0, client_wants_close)),
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
        if let Some(remaining) = path_str.strip_prefix(prefix_str) {
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
        handle_websocket_proxy_https(
            client_stream,
            target,
            connect_timeout,
            request,
            &poll_config,
        )
        .await
    } else {
        // HTTP バックエンドへの WebSocket
        handle_websocket_proxy_http(
            client_stream,
            target,
            connect_timeout,
            request,
            &poll_config,
        )
        .await
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
    let addr = HostPortStr::new(&target.host, target.port); // F-41: スタック上に構築（ヒープ確保なし）
    let addr = addr.as_str();
    let connect_result = timeout(connect_timeout, TcpStream::connect_str(addr)).await;

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
        // B-17: ハンドシェイク応答の読取にタイムアウトを設け、無応答上流で
        // クライアントを待たせない
        let buf = buf_get();
        let read_result = timeout(BACKEND_HEADER_TIMEOUT, backend_stream.read(buf)).await;
        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => {
                let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                return Some((504, 0));
            }
        };

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
                let total = websocket_bidirectional_transfer(
                    &mut client_stream,
                    &mut backend_stream,
                    poll_config,
                )
                .await;
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
    let addr = HostPortStr::new(&target.host, target.port); // F-41: スタック上に構築（ヒープ確保なし）
    let addr = addr.as_str();
    let connect_result = timeout(connect_timeout, TcpStream::connect_str(addr)).await;

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
    let tls_result = timeout(
        connect_timeout,
        connector.connect(backend_tcp, &target.host),
    )
    .await;

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
        // B-17: ハンドシェイク応答の読取にタイムアウトを設け、無応答上流で
        // クライアントを待たせない
        let buf = buf_get();
        let read_result = timeout(BACKEND_HEADER_TIMEOUT, backend_stream.read(buf)).await;
        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => {
                let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                return Some((504, 0));
            }
        };

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
                let total = websocket_bidirectional_transfer_tls(
                    &mut client_stream,
                    &mut backend_stream,
                    poll_config,
                )
                .await;
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
                let new_timeout =
                    (current_timeout_ms as f64 * poll_config.backoff_multiplier) as u64;
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
                let new_timeout =
                    (current_timeout_ms as f64 * poll_config.backoff_multiplier) as u64;
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
    wasm_modules: Arc<Vec<String>>,
    client_ip: &str,
) -> Option<(ServerTls, u16, u64, bool)> {
    // クライアントの Accept-Encoding を解析
    let client_encoding = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(b"accept-encoding"))
        .map(|(_, value)| AcceptedEncoding::parse(value))
        .unwrap_or(AcceptedEncoding::Identity);

    // ホスト名を取得
    let host_str = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(b"host"))
        .and_then(|(_, v)| std::str::from_utf8(v).ok())
        .unwrap_or("unknown");

    // RFC 7230 Section 4.3: TE ヘッダーを解析
    // クライアントがtrailersをサポートしているかを判定
    let _client_supports_trailers = headers
        .iter()
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
            let query = path_str.find('?').map(|i| &path_str[i + 1..]);
            let path_only = path_str
                .find('?')
                .map(|i| &path_str[..i])
                .unwrap_or(path_str);

            // key_headers からVaryキー用のヘッダー値を抽出
            let vary_key_headers = if !cache_config.key_headers.is_empty() {
                let extracted =
                    extract_vary_headers_for_cache_key(headers, &cache_config.key_headers);
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
                    let (cached_entry, is_stale) =
                        if let Some(entry) = cache_manager.get(&cache_key) {
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
                        debug!(
                            "Cache {} for {} {}",
                            if is_stale { "STALE" } else { "HIT" },
                            host_str,
                            path_str
                        );
                        record_cache_hit(host_str);

                        // ETag/If-None-Match 検証（304レスポンス）
                        if cache_config.enable_etag {
                            if let Some(client_etag) =
                                cache::CachePolicy::get_if_none_match(headers)
                            {
                                if let Some(ref cached_etag) = cached_entry.etag {
                                    // ETagが一致すれば304 Not Modifiedを返す
                                    let client_etag_str =
                                        std::str::from_utf8(client_etag).unwrap_or("");
                                    if etag_matches(client_etag_str, cached_etag) {
                                        debug!("ETag match, returning 304 Not Modified");
                                        let response = build_304_response(
                                            &cached_entry,
                                            client_wants_close,
                                            is_stale,
                                        );
                                        match timeout(
                                            WRITE_TIMEOUT,
                                            client_stream.write_all(response),
                                        )
                                        .await
                                        {
                                            Ok((Ok(_), _)) => {
                                                return Some((
                                                    client_stream,
                                                    304,
                                                    0,
                                                    client_wants_close,
                                                ));
                                            }
                                            _ => {
                                                return None;
                                            }
                                        }
                                    }
                                }
                            }

                            // If-Modified-Since 検証（304レスポンス）
                            if let Some(client_ims) =
                                cache::CachePolicy::get_if_modified_since(headers)
                            {
                                if let Some(ref cached_lm) = cached_entry.last_modified {
                                    let client_ims_str =
                                        std::str::from_utf8(client_ims).unwrap_or("");
                                    if last_modified_matches(client_ims_str, cached_lm) {
                                        debug!(
                                            "If-Modified-Since match, returning 304 Not Modified"
                                        );
                                        let response = build_304_response(
                                            &cached_entry,
                                            client_wants_close,
                                            is_stale,
                                        );
                                        match timeout(
                                            WRITE_TIMEOUT,
                                            client_stream.write_all(response),
                                        )
                                        .await
                                        {
                                            Ok((Ok(_), _)) => {
                                                return Some((
                                                    client_stream,
                                                    304,
                                                    0,
                                                    client_wants_close,
                                                ));
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
                        // メモリキャッシュの場合（ボディは bytes::Bytes をゼロコピーで送出）
                        if let Some(body_data) = cached_entry.memory_body() {
                            let body = body_data.clone(); // O(1) refcount、memcpy なし
                            let body_len = body.len();
                            // ヘッダーのみ構築し、ボディは連結せずゼロコピーのまま
                            // F-59: ヘッダ + ボディを 1 回の SENDMSG（scatter-gather）で送出
                            // （平文接続。kTLS/rustls は内部で 2 回書き込みへフォールバック）
                            let headers = build_cached_response_headers(
                                &cached_entry,
                                client_wants_close,
                                is_stale,
                            );
                            match timeout(
                                WRITE_TIMEOUT,
                                client_stream.write_all_vectored(headers, body),
                            )
                            .await
                            {
                                Ok((Ok(()), _, _)) => {
                                    return Some((
                                        client_stream,
                                        cached_entry.status_code,
                                        body_len as u64,
                                        client_wants_close,
                                    ));
                                }
                                _ => {
                                    return None;
                                }
                            }
                        }
                        // ディスクキャッシュの場合
                        else if let Some(disk_path) = cached_entry.disk_path() {
                            debug!("Serving from disk cache: {:?}", disk_path);
                            match serve_from_disk_cache(
                                &mut client_stream,
                                &cached_entry,
                                disk_path,
                                client_wants_close,
                                is_stale,
                            )
                            .await
                            {
                                Some((status_code, body_size)) => {
                                    return Some((
                                        client_stream,
                                        status_code,
                                        body_size,
                                        client_wants_close,
                                    ));
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
    if cache_config.enabled
        && cache_config.is_cacheable_method(method)
        && !cache_config.should_bypass(path_str)
    {
        let query = path_str.find('?').map(|i| &path_str[i + 1..]);
        let path_only = path_str
            .find('?')
            .map(|i| &path_str[..i])
            .unwrap_or(path_str);

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
            cache_save_ctx = Some(CacheSaveContext::new(
                cache_key,
                host_str.to_string(),
                max_capture,
            ));
        }
    }

    // バッファリングモードのログ出力（デバッグ用）
    if buffering_config.is_enabled() {
        debug!(
            "Buffering enabled for {} {} (mode={:?})",
            host_str, path_str, buffering_config.mode
        );
    }

    // ロードバランシング: UpstreamGroup からサーバーを選択
    // F-97: Consistent Hash の header:/cookie: をリクエストヘッダから解決
    let server = match upstream_group.select_with_header_fn(client_ip, |name| {
        headers
            .iter()
            .find(|(n, _)| n.as_ref().eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_ref())
    }) {
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

    // F-06: リクエスト結果記録用にサーバーのインデックスと開始時刻を記録
    let resilience_server_idx = upstream_group
        .servers
        .iter()
        .position(|s| std::ptr::eq(s, server));
    let resilience_start = std::time::Instant::now();

    let target = &server.target;
    // コネクションプールキーの生成
    // HTTPS: SNI と tls_insecure 毎に別プール（B-30: 検証設定の異なる接続の再利用を防ぐ）
    let tls_insecure = upstream_group.tls_insecure();
    let pool_key = if target.use_tls && target.sni_name.is_some() {
        https_pool_key(&target.host, target.port, target.sni(), tls_insecure)
    } else if target.use_tls {
        https_pool_key_no_sni(&target.host, target.port, tls_insecure)
    } else {
        format!("{}:{}", target.host, target.port)
    };

    // リクエストパス構築
    // gRPC はフルパス保持（/* プレフィックス除去で UNIMPLEMENTED → B-40）
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    let preserve_grpc_path = headers.iter().any(|(n, v)| header_pair_is_grpc(n, v));
    let final_path_owned =
        compute_upstream_path(path_str, prefix, &target.path_prefix, preserve_grpc_path);
    let final_path = final_path_owned.as_str();

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

        // B-23（多層防御）: chunked 転送時にクライアント由来の Content-Length を
        // バックエンドへ渡さない。フレーミング分類（classify_request_framing）が CL+TE を
        // 既に 400 で拒否しているため通常ここには到達しないが、chunked では下で
        // `Transfer-Encoding: chunked` を再付与するため、万一 CL が残っても
        // バックエンドに CL+TE の曖昧メッセージを渡さないよう保険で除去する。
        if is_chunked && name.eq_ignore_ascii_case(b"content-length") {
            continue;
        }

        // B-11: Expect: 100-continue はプロキシが終端する（自ら 100 Continue を応答し、
        // ボディを無条件に転送する）ため、バックエンドへは転送しない。転送すると
        // バックエンドが独自の 100 Continue 中間応答を返し、応答解析と競合する。
        if name.eq_ignore_ascii_case(b"expect") {
            continue;
        }

        // 設定で削除が指定されているヘッダーをスキップ
        // eq_ignore_ascii_case でアロケーションなしに大文字小文字無視比較
        if security
            .remove_request_headers
            .iter()
            .any(|h| name.eq_ignore_ascii_case(h.as_bytes()))
        {
            continue;
        }

        // Header Injection防止: ヘッダー名と値の検証
        // httparseによるパース後も、多層防御として再検証を行う
        // 不正な文字（CR, LF, NUL等）を含むヘッダーは除外
        if !is_valid_header_name(name) {
            warn!(
                "Invalid header name detected, skipping: {:?}",
                String::from_utf8_lossy(name)
            );
            continue;
        }
        if !is_valid_header_value(value) {
            warn!(
                "Invalid header value detected (possible header injection), skipping header: {:?}",
                String::from_utf8_lossy(name)
            );
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
        let host_str = headers
            .iter()
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
            let hostname = config
                .performance
                .via_header_hostname
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
        // 上流証明書検証は per-upstream の tls_insecure のみで制御（B-30: VEIL_TLS_INSECURE はクライアント向け）
        proxy_https_pooled(
            client_stream,
            target,
            security,
            compression,
            buffering_config,
            client_encoding,
            &pool_key,
            request,
            content_length,
            is_chunked,
            initial_body,
            client_wants_close,
            tls_insecure,
            wasm_modules,
        )
        .await
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
                    client_wants_close,
                )
                .await
            }
        }
        #[cfg(not(feature = "http2"))]
        {
            // HTTP/2 feature が無効な場合はHTTP/1.1にフォールバック
            warn!("H2C requested but http2 feature not enabled, falling back to HTTP/1.1");
            proxy_http_pooled(
                client_stream,
                target,
                security,
                compression,
                buffering_config,
                client_encoding,
                &pool_key,
                request,
                content_length,
                is_chunked,
                initial_body,
                client_wants_close,
                cache_save_ctx.as_mut(),
                wasm_modules.clone(),
            )
            .await
        }
    } else {
        // HTTP接続（キャッシュ保存・バッファリング対応）
        proxy_http_pooled(
            client_stream,
            target,
            security,
            compression,
            buffering_config,
            client_encoding,
            &pool_key,
            request,
            content_length,
            is_chunked,
            initial_body,
            client_wants_close,
            cache_save_ctx.as_mut(),
            wasm_modules,
        )
        .await
    };

    // 接続カウンターを減少（Least Connections 用）
    server.release();

    // F-06: リクエスト結果をサーキットブレーカー・異常検知へ反映
    if let Some(idx) = resilience_server_idx {
        let latency_ms = resilience_start.elapsed().as_millis() as u64;
        // 5xx をバックエンド障害として扱う
        let success = match &result {
            Some((_, status, _, _)) => *status < 500,
            None => false,
        };
        upstream_group.record_outcome(idx, success, latency_ms);
        #[cfg(feature = "metrics")]
        {
            if let Some(s) = upstream_group.servers.get(idx) {
                if let Some(cb) = &s.circuit_breaker {
                    crate::metrics::set_circuit_breaker_state(
                        &upstream_group.name,
                        cb.state_code(),
                    );
                }
                if s.is_ejected() {
                    crate::metrics::set_outlier_ejected(
                        &upstream_group.name,
                        &format!("{}:{}", s.target.host, s.target.port),
                        true,
                    );
                }
            }
        }
    }

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

                            // staleキャッシュを返す（ボディは bytes::Bytes をゼロコピーで送出）
                            // F-59: ヘッダ + ボディを 1 回の SENDMSG（scatter-gather）で送出
                            if let Some(body_data) = stale_entry.memory_body() {
                                let body = body_data.clone(); // O(1) refcount、memcpy なし
                                let body_len = body.len();
                                let headers = build_cached_response_headers(
                                    &stale_entry,
                                    client_wants_close,
                                    true,
                                );
                                match timeout(
                                    WRITE_TIMEOUT,
                                    client_stream.write_all_vectored(headers, body),
                                )
                                .await
                                {
                                    Ok((Ok(()), _, _)) => {
                                        return Some((
                                            client_stream,
                                            stale_entry.status_code,
                                            body_len as u64,
                                            client_wants_close,
                                        ));
                                    }
                                    _ => {
                                        return None;
                                    }
                                }
                            } else if let Some(disk_path) = stale_entry.disk_path() {
                                if let Some((code, size)) = serve_from_disk_cache(
                                    &mut client_stream,
                                    &stale_entry,
                                    disk_path,
                                    client_wants_close,
                                    true,
                                )
                                .await
                                {
                                    return Some((client_stream, code, size, client_wants_close));
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
    wasm_modules: Arc<Vec<String>>,
) -> Option<(ServerTls, u16, u64, bool)> {
    // セキュリティ設定からタイムアウトを取得
    let connect_timeout = Duration::from_secs(security.backend_connect_timeout_secs);

    // プールから接続を取得、または新規作成
    let mut backend_stream = match HTTP_POOL.with(|p| p.borrow_mut().get(pool_key)) {
        Some(stream) => stream,
        None => {
            // 新規接続を作成
            let addr = HostPortStr::new(&target.host, target.port); // F-41: スタック上に構築（ヒープ確保なし）
            let addr = addr.as_str();
            let connect_result = timeout(connect_timeout, TcpStream::connect_str(addr)).await;

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
    // F-97: Content-Type: application/grpc は Full バッファをバイパス（リクエスト行に含む）
    let is_grpc_req = request_bytes_indicate_grpc(&request);
    let buffering_enabled = !is_grpc_req
        && buffering_config.is_enabled()
        && buffering_config.should_buffer(Some(content_length));

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
        if client_stream.is_ktls_enabled()
            && !is_chunked
            && !compression_enabled
            && !cache_save_needed
            && !buffering_enabled
            && !wasm_modules_active
        {
            let splice_result = proxy_http_request_splice(
                &client_stream,
                &backend_stream,
                &request,
                content_length,
                is_chunked,
                initial_body,
            )
            .await;

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
                )
                .await
            }
        } else if buffering_enabled && !compression_enabled {
            // バッファリング有効時（圧縮無効の場合のみ）
            debug!(
                "Calling proxy_request_buffered for {} {}",
                target.host, target.port
            );
            record_buffering_used(host_str_for_metrics);
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
            )
            .await
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
            )
            .await
        }
    };

    #[cfg(not(feature = "ktls"))]
    let result = if buffering_enabled && !compression_enabled {
        // バッファリング有効時（圧縮無効の場合のみ）
        record_buffering_used(host_str_for_metrics);
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
        )
        .await
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
        )
        .await
    };

    match result {
        Some((status_code, total, backend_wants_keep_alive, client_must_close)) => {
            // B-17: クライアントへ 1 バイトも送らないままバックエンド異常で終わった場合、
            // エラーページ（502/504）を即時送出してクローズする（従来はクライアントが
            // 自身のタイムアウトまでハングしていた）
            if total == 0 && status_code >= 500 {
                let err_buf = if status_code == 504 {
                    ERR_MSG_GATEWAY_TIMEOUT.to_vec()
                } else {
                    ERR_MSG_BAD_GATEWAY.to_vec()
                };
                let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                return Some((client_stream, status_code, 0, true));
            }
            // バックエンドがKeep-Aliveを許可している場合、プールに返却
            if backend_wants_keep_alive {
                let max_idle = security.max_idle_connections_per_host;
                let idle_timeout = security.idle_connection_timeout_secs;
                HTTP_POOL.with(|p| {
                    p.borrow_mut()
                        .put(pool_key.to_string(), backend_stream, max_idle, idle_timeout)
                });
            }
            // 408 (body timeout) sends Connection: close — must actually close
            // B-17: 上流異常でボディが完結しなかった場合もクローズする
            let should_close = client_wants_close || status_code == 408 || client_must_close;
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
    let addr = HostPortStr::new(&target.host, target.port); // F-41: スタック上に構築（ヒープ確保なし）
    let addr = addr.as_str();
    let connect_result = timeout(connect_timeout, TcpStream::connect_str(addr)).await;

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
    let headers_ref: Vec<(&[u8], &[u8])> = headers
        .iter()
        .map(|(k, v)| (k.as_ref(), v.as_ref()))
        .collect();

    // リクエストを送信
    let body = if request_body.is_empty() {
        None
    } else {
        Some(request_body)
    };
    let authority = target.host.as_bytes();

    let response = match h2c_client
        .send_request(method, path, authority, &headers_ref, body)
        .await
    {
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
    // ディスクからボディを読み込み。whole-file 読み込みはイベントループをブロックするため、
    // ブロッキングオフロードで専用ワーカースレッドへ退避する（F-29 完全非同期化）。
    let disk_path_owned = disk_path.to_path_buf();
    // 理由付き allow: 同期 FS は offload 閉包内（専用ワーカースレッド）で実行され、イベントループを塞がない。
    #[allow(clippy::disallowed_methods)]
    let body_data =
        match crate::runtime::offload::offload(move || std::fs::read(disk_path_owned)).await {
            Ok(data) => data,
            Err(e) => {
                error!("Failed to read disk cache file: {}", e);
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
    Disk { path: std::path::PathBuf, size: u64 },
    /// バッファサイズ制限超過 (507 Insufficient Storage)
    LimitExceeded,
    /// バッファリング失敗（ストリーミングにフォールバック）
    Failed,
}

impl BufferedBodyResult {}

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
) -> Option<(u16, u64, bool, bool)>
where
    R: AsyncReader
        + AsyncWriter
        + Unpin
        + crate::runtime::io::AsyncReadRent
        + crate::runtime::io::AsyncWriteRentExt,
{
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
        match transfer_chunked_body(
            client_stream,
            backend_stream,
            initial_body,
            max_chunked_body_size,
        )
        .await
        {
            ChunkedTransferResult::Complete => {}
            ChunkedTransferResult::Failed => return None,
            ChunkedTransferResult::SizeLimitExceeded => return None,
        }
    } else {
        let remaining = content_length.saturating_sub(initial_body.len());
        if remaining > 0 {
            let body_timeout = Duration::from_secs(security.client_body_timeout_secs);
            match timeout(
                body_timeout,
                transfer_exact_bytes(client_stream, backend_stream, remaining),
            )
            .await
            {
                Ok(transferred) if transferred >= remaining as u64 => {}
                Ok(_) => return None,
                Err(_) => {
                    let _ = client_stream
                        .write_all(ERR_MSG_REQUEST_TIMEOUT.to_vec())
                        .await;
                    return Some((408, 0, false, true));
                }
            }
        }
    }

    // 4. レスポンスを受信してバッファリング
    let buffered = receive_and_buffer_response(backend_stream, buffering_config, cache_ctx).await;

    match buffered {
        Some((status_code, headers_data, body_result, backend_wants_keep_alive)) => {
            // B-17: ボディのバッファリングに失敗した場合、クライアントへは未送信のため
            // ヘッダーだけ送って（CL 分のボディを待たせて）ハングさせず、
            // None → 呼び出し元の 502 送出 + クローズに委ねる
            if matches!(body_result, BufferedBodyResult::Failed) {
                return None;
            }
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
                            client_stream.write_all(combined),
                        )
                        .await;

                        if matches!(write_result, Ok((Ok(_), _))) {
                            total = combined_len as u64;
                        }
                    }
                    BufferedBodyResult::Disk { path, size } => {
                        // ヘッダーを先に送信（所有権を移動、clone 不要）
                        let headers_len = headers_data.len();
                        let write_result = timeout(
                            Duration::from_secs(buffering_config.client_write_timeout_secs),
                            client_stream.write_all(headers_data),
                        )
                        .await;

                        if !matches!(write_result, Ok((Ok(_), _))) {
                            let _ = crate::runtime::io::remove_file(&path).await;
                            return Some((status_code, 0, false, true));
                        }

                        total = headers_len as u64;

                        // ディスクから読み込んでクライアントに送信
                        match send_disk_buffer_to_client(
                            client_stream,
                            &path,
                            size,
                            buffering_config.client_write_timeout_secs,
                        )
                        .await
                        {
                            Some(sent) => {
                                total += sent;
                            }
                            None => {
                                let _ = crate::runtime::io::remove_file(&path).await;
                                return Some((status_code, total, false, true));
                            }
                        }
                        let _ = crate::runtime::io::remove_file(&path).await;
                    }
                    BufferedBodyResult::Failed => {
                        // ヘッダーのみ送信（所有権を移動、clone 不要）
                        let headers_len = headers_data.len();
                        let write_result = timeout(
                            Duration::from_secs(buffering_config.client_write_timeout_secs),
                            client_stream.write_all(headers_data),
                        )
                        .await;
                        if matches!(write_result, Ok((Ok(_), _))) {
                            total = headers_len as u64;
                        }
                        return Some((status_code, total, false, true));
                    }
                    BufferedBodyResult::LimitExceeded => {
                        // 507 Insufficient Storage を送信
                        let err_buf = ERR_MSG_INSUFFICIENT_STORAGE.to_vec();
                        let _ = timeout(
                            Duration::from_secs(buffering_config.client_write_timeout_secs),
                            client_stream.write_all(err_buf),
                        )
                        .await;
                        // 507 エラー時は接続を閉じる (should_close = true, backend keep-alive = false)
                        return Some((507, 0, false, true));
                    }
                }
            } else {
                // buffer_headers = false: ヘッダーを先に送信し、ボディは別途送信
                // ヘッダー送信（所有権を移動、clone 不要）
                let headers_len = headers_data.len();
                let write_result = timeout(
                    Duration::from_secs(buffering_config.client_write_timeout_secs),
                    client_stream.write_all(headers_data),
                )
                .await;

                if !matches!(write_result, Ok((Ok(_), _))) {
                    // ディスクファイルがあればクリーンアップ（非同期削除）
                    if let BufferedBodyResult::Disk { ref path, .. } = body_result {
                        let _ = crate::runtime::io::remove_file(path).await;
                    }
                    return Some((status_code, 0, false, true));
                }

                total = headers_len as u64;

                // ボディ送信（メモリまたはディスクから）
                match body_result {
                    BufferedBodyResult::Memory(body_data) => {
                        if !body_data.is_empty() {
                            let body_len = body_data.len();
                            let write_result = timeout(
                                Duration::from_secs(buffering_config.client_write_timeout_secs),
                                client_stream.write_all(body_data),
                            )
                            .await;

                            if !matches!(write_result, Ok((Ok(_), _))) {
                                return Some((status_code, total, false, true));
                            }

                            total += body_len as u64;
                        }
                    }
                    BufferedBodyResult::Disk { path, size } => {
                        // ディスクから読み込んでクライアントに送信
                        match send_disk_buffer_to_client(
                            client_stream,
                            &path,
                            size,
                            buffering_config.client_write_timeout_secs,
                        )
                        .await
                        {
                            Some(sent) => {
                                total += sent;
                            }
                            None => {
                                let _ = crate::runtime::io::remove_file(&path).await;
                                return Some((status_code, total, false, true));
                            }
                        }
                        let _ = crate::runtime::io::remove_file(&path).await;
                    }
                    BufferedBodyResult::Failed => {
                        return Some((status_code, total, false, true));
                    }
                    BufferedBodyResult::LimitExceeded => {
                        // すでにヘッダー送信済みのため、507を返すことはできないので接続を閉じる
                        return Some((status_code, total, true, true));
                    }
                }
            }

            Some((status_code, total, backend_wants_keep_alive, false))
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
where
    R: AsyncReadRent + Unpin,
{
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

        // B-11: バックエンド由来の 1xx 中間応答を読み捨てる。
        drain_interim_responses(&mut accumulated);

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
                    let headers: Vec<(Box<[u8]>, Box<[u8]>)> = response
                        .headers
                        .iter()
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
            )
            .await;

            return Some((
                status_code,
                headers_data,
                body_result,
                backend_wants_keep_alive,
            ));
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
    // whole-file 読み込みはイベントループをブロックするためオフロードする（F-29）。
    let path_owned = path.to_path_buf();
    // 理由付き allow: 同期 FS は offload 閉包内（専用ワーカースレッド）で実行され、イベントループを塞がない。
    #[allow(clippy::disallowed_methods)]
    let data = match crate::runtime::offload::offload(move || std::fs::read(path_owned)).await {
        Ok(d) => d,
        Err(e) => {
            error!("Failed to read disk buffer: {}", e);
            return None;
        }
    };

    // クライアントに送信
    let write_result = timeout(
        Duration::from_secs(timeout_secs),
        client_stream.write_all(data),
    )
    .await;

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
where
    R: AsyncReadRent + Unpin,
{
    let mut body = initial_body;

    debug!("buffer_response_body_with_spillover: content_length={:?}, is_chunked={}, initial_body_len={}, max_mem={}, max_disk={}", 
          content_length, is_chunked, body.len(), buffering_config.max_memory_buffer, buffering_config.max_disk_buffer);

    // キャッシュコンテキストに初期ボディをキャプチャ
    if let Some(ref mut ctx) = cache_ctx {
        ctx.append_body(&body);
    }

    if let Some(cl) = content_length {
        // Content-Length 転送
        let cl_usize = cl;
        let remaining = cl.saturating_sub(body.len());

        // バッファサイズ制限チェック (メモリ)
        if cl_usize > buffering_config.max_memory_buffer {
            // ディスクスピルオーバー
            if let Some(ref disk_path) = buffering_config.disk_buffer_path {
                // max_disk_buffer 制限チェック
                if cl > buffering_config.max_disk_buffer {
                    ftlog::warn!(
                        "Response size {} exceeds max_disk_buffer {}, aborting buffer",
                        cl,
                        buffering_config.max_disk_buffer
                    );
                    return BufferedBodyResult::LimitExceeded;
                }

                debug!(
                    "Response size {} exceeds memory limit {}, spilling to disk (max_disk={})",
                    cl, buffering_config.max_memory_buffer, buffering_config.max_disk_buffer
                );

                // まず残りのデータをメモリに読み込み
                if remaining > 0 {
                    let additional =
                        buffer_exact_bytes(backend_stream, remaining, &mut cache_ctx).await;
                    body.extend(additional);
                }

                // ディスクに書き込み
                let key = format!(
                    "buffer_{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos()
                );

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
                    let max_additional = buffering_config
                        .max_memory_buffer
                        .saturating_sub(body.len());
                    if max_additional > 0 {
                        let additional =
                            buffer_exact_bytes(backend_stream, max_additional, &mut cache_ctx)
                                .await;
                        body.extend(additional);
                    }
                }
                warn!("Response truncated: memory limit exceeded and no disk buffer configured");
            }
        } else {
            // メモリ制限内
            if remaining > 0 {
                let additional =
                    buffer_exact_bytes(backend_stream, remaining, &mut cache_ctx).await;
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
                            ftlog::warn!(
                                "Chunked response exceeds max_disk_buffer {}, aborting buffer",
                                max_disk
                            );
                            size_exceeded = true;
                            break;
                        }

                        let read_buf = buf_get();
                        let read_result =
                            timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;

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
                    let key = format!(
                        "buffer_chunked_{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos()
                    );

                    let size = body.len() as u64;
                    match buffering::disk_buffer::write_to_disk(disk_path, key.as_bytes(), body)
                        .await
                    {
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
                            ftlog::warn!(
                                "Blind response exceeds max_disk_buffer {}, aborting buffer",
                                max_disk
                            );
                            size_exceeded = true;
                            break;
                        }

                        let read_buf = buf_get();
                        let read_result =
                            timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;
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
                    let key = format!(
                        "buffer_blind_{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos()
                    );
                    let size = body.len() as u64;
                    match buffering::disk_buffer::write_to_disk(disk_path, key.as_bytes(), body)
                        .await
                    {
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
where
    R: AsyncReadRent + Unpin,
{
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
    wasm_modules: Arc<Vec<String>>,
) -> Option<(u16, u64, bool, bool)> {
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
        match transfer_chunked_body(
            client_stream,
            backend_stream,
            initial_body,
            max_chunked_body_size,
        )
        .await
        {
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
            match timeout(
                body_timeout,
                transfer_exact_bytes(client_stream, backend_stream, remaining),
            )
            .await
            {
                Ok(transferred) if transferred >= remaining as u64 => {}
                Ok(_) => return None,
                Err(_) => {
                    let _ = client_stream
                        .write_all(ERR_MSG_REQUEST_TIMEOUT.to_vec())
                        .await;
                    return Some((408, 0, false, true));
                }
            }
        }
    }

    // 4. レスポンスを受信して転送（圧縮対応、キャッシュ保存対応）
    let (total, status_code, backend_wants_keep_alive, client_must_close) =
        transfer_response_with_compression(
            backend_stream,
            client_stream,
            compression,
            client_encoding,
            cache_ctx,
            security,
            wasm_modules,
        )
        .await;

    Some((
        status_code,
        total,
        backend_wants_keep_alive,
        client_must_close,
    ))
}

// ====================
// レスポンス転送（圧縮対応版）
// ====================

/// レスポンスヘッダーを解析し、必要に応じて圧縮してクライアントに転送
/// キャッシュコンテキストが指定されている場合、レスポンスボディをキャプチャしてキャッシュに保存
///
/// 戻り値: (転送バイト数, ステータス, backend_wants_keep_alive, client_must_close)
/// `client_must_close` は B-17: 上流異常（ヘッダー不完全・CL 未達 EOF 等）でクライアント
/// 接続を即時クローズすべき場合に true。
#[cfg_attr(not(feature = "wasm"), allow(unused_variables))]
async fn transfer_response_with_compression(
    backend_stream: &mut TcpStream,
    client_stream: &mut ServerTls,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    mut cache_ctx: Option<&mut CacheSaveContext>,
    security: &SecurityConfig,
    wasm_modules: Arc<Vec<String>>,
) -> (u64, u16, bool, bool) {
    let mut accumulated = Vec::with_capacity(BUF_SIZE);
    let mut total = 0u64;
    let mut status_code = 502u16;
    // 初期値false: エラー時はKeep-Aliveを無効化
    let mut backend_wants_keep_alive = false;

    // ヘッダー読み取り用バッファ
    loop {
        // B-17: ヘッダー読取は専用の短いタイムアウトで打ち切り、504 へ即変換する
        let read_buf = buf_get();
        let read_result = timeout(BACKEND_HEADER_TIMEOUT, backend_stream.read(read_buf)).await;

        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => {
                warn!("Backend response header read timeout");
                return (total, 504, false, true);
            }
        };

        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                // B-17: ヘッダー完了前の EOF は 502 として即時応答する
                return (total, status_code, backend_wants_keep_alive, true);
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                return (total, status_code, backend_wants_keep_alive, true);
            }
        };

        returned_buf.set_valid_len(n);
        accumulated.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);

        // B-11: バックエンド由来の 1xx 中間応答（100 Continue / 103 Early Hints 等）を
        // 読み捨てる（最終応答と誤認して転送するとクライアントが最終応答を待ち続ける）。
        drain_interim_responses(&mut accumulated);

        // ヘッダーが完全に受信されたかチェック
        if let Some(parsed) = parse_http_response(&accumulated) {
            status_code = parsed.status_code;
            backend_wants_keep_alive = !parsed.is_connection_close;

            let header_len = parsed.header_len;
            let body_start = &accumulated[header_len..];

            // Content-Type と Content-Encoding を取得
            let content_type = extract_header_value(&accumulated[..header_len], b"content-type");
            let existing_encoding =
                extract_header_value(&accumulated[..header_len], b"content-encoding");

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
                info!(
                    "[Compression] Initializing compressed transfer with {:?}",
                    encoding
                );
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
                )
                .await;

                return (result.0, status_code, result.1, false);
            } else {
                // 圧縮無効: そのまま転送（キャッシュ保存対応）

                // キャッシュコンテキストがある場合、ヘッダーを設定
                if let Some(ref mut ctx) = cache_ctx {
                    // ヘッダーを解析してキャッシュコンテキストに保存
                    let mut headers_storage = [httparse::EMPTY_HEADER; 64];
                    let mut response = httparse::Response::new(&mut headers_storage);
                    if response.parse(&accumulated[..header_len]).is_ok() {
                        let headers: Vec<(Box<[u8]>, Box<[u8]>)> = response
                            .headers
                            .iter()
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
                    let status_line = format!(
                        "HTTP/1.1 {} {}\r\n",
                        status_code,
                        status_code_to_reason(status_code)
                    );
                    new_header_lines.push(status_line.into_bytes());

                    // 既存のヘッダーを追加（削除対象を除外）
                    // eq_ignore_ascii_case でアロケーションなしに比較
                    for header in response.headers.iter() {
                        if !security
                            .remove_response_headers
                            .iter()
                            .any(|h| header.name.as_bytes().eq_ignore_ascii_case(h.as_bytes()))
                        {
                            new_header_lines.push(
                                format!(
                                    "{}: {}\r\n",
                                    header.name,
                                    std::str::from_utf8(header.value).unwrap_or("")
                                )
                                .into_bytes(),
                            );
                        }
                    }

                    // 追加するヘッダーを追加
                    for (header_name, header_value) in &security.add_response_headers {
                        new_header_lines
                            .push(format!("{}: {}\r\n", header_name, header_value).into_bytes());
                    }
                    // F-94: HTTP/3 広告（Alt-Svc）
                    if let Some(g) = get_alt_svc_guard() {
                        let mut line = Vec::with_capacity(16 + g.value().len());
                        line.extend_from_slice(b"Alt-Svc: ");
                        line.extend_from_slice(g.value());
                        line.extend_from_slice(b"\r\n");
                        new_header_lines.push(line);
                    }

                    // WASMレスポンスヘッダーフィルタを適用
                    #[cfg(feature = "wasm")]
                    {
                        if !wasm_modules.is_empty() {
                            let config = CURRENT_CONFIG.load();
                            if let Some(ref wasm_engine) = config.wasm_filter_engine {
                                // 現在のヘッダーをVec<(Vec<u8>, Vec<u8>)>形式に変換
                                let current_headers: Vec<(Vec<u8>, Vec<u8>)> = new_header_lines
                                    .iter()
                                    .skip(1) // ステータス行をスキップ
                                    .filter_map(|line| {
                                        let line_str = std::str::from_utf8(line).ok()?;
                                        let line_trimmed = line_str.trim_end_matches("\r\n");
                                        if line_trimmed.is_empty() {
                                            return None;
                                        }
                                        let colon_pos = line_trimmed.find(':')?;
                                        let name = line_trimmed.as_bytes()[..colon_pos].to_vec();
                                        let value = line_trimmed[colon_pos + 1..]
                                            .trim_start()
                                            .as_bytes()
                                            .to_vec();
                                        Some((name, value))
                                    })
                                    .collect();

                                // WASMフィルタを実行（レスポンスヘッダー処理）
                                let wasm_result = wasm_engine
                                    .clone()
                                    .on_response_headers_with_modules_async(
                                        wasm_modules.clone(),
                                        status_code,
                                        current_headers,
                                        true, // end_of_stream
                                    )
                                    .await;

                                match wasm_result {
                                    crate::wasm::FilterResult::Continue {
                                        headers: modified_headers,
                                        ..
                                    } => {
                                        // WASMから修正されたヘッダーで置き換え
                                        new_header_lines.clear();

                                        // ステータス行を再追加
                                        let status_line = format!(
                                            "HTTP/1.1 {} {}\r\n",
                                            status_code,
                                            status_code_to_reason(status_code)
                                        );
                                        new_header_lines.push(status_line.into_bytes());

                                        // WASMから返されたヘッダーを追加
                                        for (name, value) in modified_headers {
                                            let mut line =
                                                Vec::with_capacity(name.len() + value.len() + 4);
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

                // 修正したヘッダー + 初期ボディを送信
                // F-59: 平文接続では 1 回の SENDMSG（scatter-gather）で送出
                let body_data = body_start.to_vec();
                let write_result = timeout(
                    WRITE_TIMEOUT,
                    client_stream.write_all_vectored(modified_headers, body_data),
                )
                .await;
                if !matches!(write_result, Ok((Ok(()), _, _))) {
                    return (total, status_code, false, true);
                }
                total += header_len as u64;
                total += body_start.len() as u64;

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
                    )
                    .await;
                    total += transferred;

                    // B-17: Content-Length 宣言に満たないまま上流が終了した場合、
                    // クライアントは残りボディを待ち続けるため接続を即クローズして通知する
                    if parsed.content_length.is_some() && transferred < body_remaining as u64 {
                        warn!(
                            "Backend response body incomplete: {} < {}",
                            transferred, body_remaining
                        );
                        return (total, status_code, false, true);
                    }
                }

                return (total, status_code, backend_wants_keep_alive, false);
            }
        }

        // B-17: レスポンスヘッダーの上限超過は 502 で即時応答する
        if accumulated.len() > MAX_RESPONSE_HEADER_SIZE {
            warn!(
                "Backend response header too large (> {} bytes)",
                MAX_RESPONSE_HEADER_SIZE
            );
            return (0, 502, false, true);
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
    use flate2::write::GzEncoder;
    use flate2::Compression;
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
                    )
                    .await;
                }
            }
        }
        AcceptedEncoding::Gzip => {
            let level = Compression::new(compression.gzip_level);
            let mut encoder = GzEncoder::new(Vec::new(), level);
            if encoder.write_all(&body_data).is_err() {
                // 圧縮失敗: 非圧縮で送信
                return transfer_uncompressed_fallback(client_stream, original_headers, &body_data)
                    .await;
            }
            match encoder.finish() {
                Ok(data) => data,
                Err(_) => {
                    return transfer_uncompressed_fallback(
                        client_stream,
                        original_headers,
                        &body_data,
                    )
                    .await;
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
                return transfer_uncompressed_fallback(client_stream, original_headers, &body_data)
                    .await;
            }
            compressed
        }
        AcceptedEncoding::Deflate => {
            let level = Compression::new(compression.gzip_level);
            let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), level);
            if encoder.write_all(&body_data).is_err() {
                return transfer_uncompressed_fallback(client_stream, original_headers, &body_data)
                    .await;
            }
            match encoder.finish() {
                Ok(data) => data,
                Err(_) => {
                    return transfer_uncompressed_fallback(
                        client_stream,
                        original_headers,
                        &body_data,
                    )
                    .await;
                }
            }
        }
        AcceptedEncoding::Identity => {
            // 圧縮なし（ここには来ないはず）
            body_data
        }
    };

    // 3. 新しいヘッダーを構築
    let new_headers =
        build_compressed_headers(original_headers, encoding, compressed_body.len(), security);

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
    _backend_wants_keep_alive: bool,
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
    let write_result = timeout(
        WRITE_TIMEOUT,
        client_stream.write_all(original_headers.to_vec()),
    )
    .await;
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
// compression feature 有効時のみ呼び出される
#[cfg_attr(not(feature = "compression"), allow(dead_code))]
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
            || security
                .remove_response_headers
                .iter()
                .any(|h| n.eq_ignore_ascii_case(h.as_bytes()))
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
    // F-94: HTTP/3 広告（Alt-Svc）
    append_alt_svc_header_line(&mut new_headers);

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
            )
            .await;
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
/// `src → pipe → dst` の 2 段 splice を **io_uring（IORING_OP_SPLICE）の非同期 Future**
/// で発行し、カーネル内ゼロコピー転送を行う（F-39）。同期 `libc::splice` は使用しない。
/// データ/空きが無い場合は `WouldBlock` が返るため、`readable()` / `writable()`
/// （POLL_ADD）で待機してから再試行する（L4 の `forward_direction_splice` と同方式）。
///
/// pipe に取り込んだ n バイトは dst のバックプレッシャに追従して**必ず全量ドレイン**
/// してから次のチャンクへ進む（pipe 内残データと `remaining` のずれによるデータ損失を防ぐ）。
#[cfg(feature = "ktls")]
async fn splice_body_transfer(
    src_stream: &TcpStream,
    dst_stream: &TcpStream,
    pipe: &SplicePipe,
    mut remaining: usize,
) -> u64 {
    use crate::runtime::splice::{splice as iouring_splice, splice_more as iouring_splice_more};
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

    'outer: while remaining > 0 {
        let chunk_size = remaining.min(chunk_size_config);

        // Step 1: src → pipe（io_uring 非同期 splice）
        let n = match iouring_splice(src_fd, pipe.write_fd(), chunk_size).await {
            Ok(0) => break, // src EOF
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // src にデータが無い → 読み取り可能になるまで待機（POLL_ADD）
                // B-17: 無応答の上流で永久待機しないよう READ_TIMEOUT で打ち切る
                match timeout(READ_TIMEOUT, src_stream.readable()).await {
                    Ok(Ok(())) => {}
                    _ => break,
                }
                continue;
            }
            Err(e) => {
                warn!("splice body transfer error (src→pipe): {}", e);
                break;
            }
        };

        // Step 2: pipe → dst（n バイトを全量ドレイン。dst のバックプレッシャに追従）
        //
        // B-25: SPLICE_F_MORE（splice_more）は「このチャンクの後に確実にデータが続く」
        // 中間チャンクのみに付与する。最終チャンクに付与すると、dst が kTLS ソケットの
        // 場合に 16KiB 未満の最終部分 TLS レコードがカーネル内で保留されたまま
        // フラッシュされず、クライアントが応答完了を永遠に待つハングになる。
        let more_follows = remaining > n;
        let mut moved = 0usize;
        while moved < n {
            let splice_step = if more_follows {
                iouring_splice_more(pipe.read_fd(), dst_fd, n - moved)
            } else {
                iouring_splice(pipe.read_fd(), dst_fd, n - moved)
            };
            match splice_step.await {
                Ok(0) => break 'outer, // dst クローズ
                Ok(m) => moved += m,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // dst 送信バッファ満杯 → 書き込み可能になるまで待機（POLL_ADD）
                    // B-17: 受信停止したクライアントで永久待機しないよう WRITE_TIMEOUT で打ち切る
                    match timeout(WRITE_TIMEOUT, dst_stream.writable()).await {
                        Ok(Ok(())) => {}
                        _ => break 'outer,
                    }
                }
                Err(e) => {
                    warn!("splice body transfer error (pipe→dst): {}", e);
                    break 'outer;
                }
            }
        }

        total += n as u64;
        remaining = remaining.saturating_sub(n);
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
) -> Option<(u16, u64, bool, bool)> {
    // 設定に基づいてパイプを取得または作成
    let per_stream_pipe_enabled = {
        let config = CURRENT_CONFIG.load();
        config.performance.per_stream_pipe_enabled
    };

    // パイプ取得: ストリーム毎の新規パイプ or スレッドローカルプールから checkout（B-16）
    //
    // どちらの経路も所有権ベースのため、await を跨いで RefCell の借用が残らない。
    // プール checkout は Drop 時に残データ検査つきで返却される（src/pool.rs）。
    let per_stream_pipe: Option<SplicePipe> = if per_stream_pipe_enabled {
        // ストリーム毎に新規パイプを作成（高並行性環境向け）
        match SplicePipe::new() {
            Ok(p) => Some(p),
            Err(e) => {
                warn!(
                    "Failed to create per-stream splice pipe: {}, falling back to pooled pipe",
                    e
                );
                None
            }
        }
    } else {
        None
    };
    let pooled_pipe = if per_stream_pipe.is_none() {
        get_splice_pipe()
    } else {
        None
    };
    let pipe: &SplicePipe = match (per_stream_pipe.as_ref(), pooled_pipe.as_deref()) {
        (Some(p), _) => p,
        (None, Some(p)) => p,
        (None, None) => {
            warn!("splice pipe not available, falling back to normal transfer");
            return None;
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
        let transferred =
            splice_body_transfer(client_tcp, backend_stream, pipe, remaining_body).await;

        if transferred < remaining_body as u64 {
            warn!(
                "Request body transfer incomplete: {} < {}",
                transferred, remaining_body
            );
            return None;
        }
    }

    // 4. レスポンスを受信して転送（splice 使用）
    let result = splice_transfer_response_ktls(backend_stream, client_stream, pipe).await;

    Some(result)
}

/// kTLS + splice によるレスポンス転送
///
/// バックエンド(TCP) からヘッダーを読み取り、パースしてクライアント(kTLS)に送信。
/// ボディは Content-Length の場合は splice、Chunked の場合は通常転送。
/// 戻り値: (ステータス, 転送バイト数, backend_wants_keep_alive, client_must_close)
#[cfg(feature = "ktls")]
async fn splice_transfer_response_ktls(
    backend_stream: &TcpStream,
    client_stream: &KtlsServerStream,
    pipe: &SplicePipe,
) -> (u16, u64, bool, bool) {
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
        // B-17: ヘッダー読取は専用の短いタイムアウトで打ち切り、504 へ即変換する
        let n = match timeout(
            BACKEND_HEADER_TIMEOUT,
            async_raw_read(backend_stream, &mut header_buf),
        )
        .await
        {
            Ok(Ok(0)) => {
                // B-17: ヘッダー完了前の EOF は 502 として即時応答する
                return (status_code, total, false, true);
            }
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                warn!("Failed to read response header: {}", e);
                return (status_code, total, false, true);
            }
            Err(_) => {
                warn!("Backend response header read timeout (splice)");
                return (504, total, false, true);
            }
        };

        accumulated.extend_from_slice(&header_buf[..n]);

        // B-11: バックエンド由来の 1xx 中間応答を読み捨てる。
        drain_interim_responses(&mut accumulated);

        // ヘッダーが完全に受信されたかチェック
        if let Some(parsed) = parse_http_response(&accumulated) {
            status_code = parsed.status_code;
            backend_wants_keep_alive = !parsed.is_connection_close;

            let header_len = parsed.header_len;
            let body_start_len = accumulated.len().saturating_sub(header_len);

            // ヘッダー + 初期ボディをクライアントに送信（raw_write）
            if let Err(e) = async_raw_write_all(client_tcp, &accumulated).await {
                warn!("Failed to send response header: {}", e);
                return (status_code, total, false, true);
            }
            total += accumulated.len() as u64;

            // B-17: ボディが完結しなかった場合にクライアント接続をクローズするためのフラグ
            let mut client_must_close = false;

            // ボディ転送
            if parsed.is_chunked {
                // Chunked 転送: 通常の方法で転送（終端検出が必要）
                // レスポンス受信時は制限なし（バックエンドを信頼）
                let mut chunked_decoder = ChunkedDecoder::new_unlimited();

                // 初期ボディ部分をデコーダにフィード
                if body_start_len > 0
                    && chunked_decoder.feed(&accumulated[header_len..])
                        == ChunkedFeedResult::Complete
                {
                    // 初期ボディで完了
                    return (status_code, total, backend_wants_keep_alive, false);
                }

                // 残りの Chunked ボディを転送
                loop {
                    // B-17: 無応答の上流で永久待機しないよう READ_TIMEOUT で打ち切る
                    let n = match timeout(
                        READ_TIMEOUT,
                        async_raw_read(backend_stream, &mut header_buf),
                    )
                    .await
                    {
                        Ok(Ok(0)) => {
                            // B-17: 終端チャンク前の EOF はクライアントを待たせないようクローズ
                            backend_wants_keep_alive = false;
                            client_must_close = true;
                            break;
                        }
                        Ok(Ok(n)) => n,
                        Ok(Err(_)) | Err(_) => {
                            backend_wants_keep_alive = false;
                            client_must_close = true;
                            break;
                        }
                    };

                    let feed_result = chunked_decoder.feed(&header_buf[..n]);

                    if (async_raw_write_all(client_tcp, &header_buf[..n]).await).is_err() {
                        backend_wants_keep_alive = false;
                        client_must_close = true;
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
                    let transferred =
                        splice_body_transfer(backend_stream, client_tcp, pipe, remaining).await;

                    total += transferred;

                    if transferred < remaining as u64 {
                        // B-17: CL 未達のままの終了はクライアントを待たせないようクローズ
                        backend_wants_keep_alive = false;
                        client_must_close = true;
                    }
                }
            } else {
                // Content-Length も Chunked もない場合: 接続クローズまで読み取り
                // この場合は Keep-Alive 不可
                backend_wants_keep_alive = false;

                loop {
                    // B-17: 無応答の上流で永久待機しないよう READ_TIMEOUT で打ち切る
                    let n = match timeout(
                        READ_TIMEOUT,
                        async_raw_read(backend_stream, &mut header_buf),
                    )
                    .await
                    {
                        Ok(Ok(0)) => break,
                        Ok(Ok(n)) => n,
                        Ok(Err(_)) | Err(_) => break,
                    };

                    if (async_raw_write_all(client_tcp, &header_buf[..n]).await).is_err() {
                        break;
                    }
                    total += n as u64;
                }

                // 接続クローズが終端のためクライアント側もクローズが必要
                client_must_close = true;
            }

            return (
                status_code,
                total,
                backend_wants_keep_alive,
                client_must_close,
            );
        }

        // B-17: レスポンスヘッダーの上限超過は 502 で即時応答する
        if accumulated.len() > MAX_RESPONSE_HEADER_SIZE {
            warn!(
                "Backend response header too large (> {} bytes, splice)",
                MAX_RESPONSE_HEADER_SIZE
            );
            return (502, 0, false, true);
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
    let addr = HostPortStr::new(&target.host, target.port); // F-41: スタック上に構築（ヒープ確保なし）
    let addr = addr.as_str();
    let backend_tcp = match timeout(connect_timeout, TcpStream::connect_str(addr)).await {
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
    wasm_modules: Arc<Vec<String>>,
) -> Option<(ServerTls, u16, u64, bool)> {
    // セキュリティ設定からタイムアウトを取得
    let connect_timeout = Duration::from_secs(security.backend_connect_timeout_secs);
    // セキュリティ設定からchunked最大サイズを取得
    let max_chunked = security.max_chunked_body_size as u64;
    // バッファリングが有効かどうか判定
    // F-97: gRPC は Full バッファをバイパス
    let is_grpc_req = request_bytes_indicate_grpc(&request);
    let buffering_enabled = !is_grpc_req
        && buffering_config.is_enabled()
        && buffering_config.should_buffer(Some(content_length));

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
            None => {
                match connect_https_backend_fresh(target, connect_timeout, tls_insecure).await {
                    Ok(stream) => (stream, false),
                    Err((code, msg)) => {
                        let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(msg.to_vec())).await;
                        return Some((client_stream, code, 0, true));
                    }
                }
            }
        };

        // リトライ可能要求は複製を渡し（次の試行のため原本を保持）、それ以外は move する。
        let req = if replayable {
            request_holder.clone().unwrap_or_default()
        } else {
            request_holder.take().unwrap_or_default()
        };
        // wasm_modules は通常空（割り当てなし）のため毎試行クローンしても実質コストは無い。
        let wasm_mods = wasm_modules.clone();

        let result = if buffering_enabled
            && (!compression.enabled || client_encoding == AcceptedEncoding::Identity)
        {
            let host_str_for_metrics = &target.host;
            record_buffering_used(host_str_for_metrics);
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
            )
            .await
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
            )
            .await
        };

        match result {
            Some((status_code, total, backend_wants_keep_alive, client_must_close)) => {
                // プールから取り出した接続が応答前に死んでいた（total==0 かつ status は初期値 502 = レスポンス未受信）。
                // クライアントへ未送信のため、新規接続で一度だけ透過リトライ。死んだ接続はプールに戻さない。
                if from_pool && total == 0 && status_code == 502 && replayable && attempt < 2 {
                    continue;
                }
                // B-17: クライアントへ 1 バイトも送らないままバックエンド異常で終わった場合、
                // エラーページ（502/504）を即時送出してクローズする
                if total == 0 && status_code >= 500 {
                    let err_buf = if status_code == 504 {
                        ERR_MSG_GATEWAY_TIMEOUT.to_vec()
                    } else {
                        ERR_MSG_BAD_GATEWAY.to_vec()
                    };
                    let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                    return Some((client_stream, status_code, 0, true));
                }
                // バックエンドがKeep-Aliveを許可している場合、プールに返却
                if backend_wants_keep_alive {
                    let max_idle = security.max_idle_connections_per_host;
                    let idle_timeout = security.idle_connection_timeout_secs;
                    HTTPS_POOL.with(|p| {
                        p.borrow_mut().put(
                            pool_key.to_string(),
                            backend_stream,
                            max_idle,
                            idle_timeout,
                        )
                    });
                }
                // 408 (body timeout) sends Connection: close — must actually close
                // B-17: 上流異常でボディが完結しなかった場合もクローズする
                let should_close = client_wants_close || status_code == 408 || client_must_close;
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
    wasm_modules: Arc<Vec<String>>,
) -> Option<(u16, u64, bool, bool)> {
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
        match transfer_chunked_body(
            client_stream,
            backend_stream,
            initial_body,
            max_chunked_body_size,
        )
        .await
        {
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
            let timed_out = futures::select_biased! {
                _ = futures::FutureExt::fuse(crate::runtime::time::sleep(body_timeout)) => {
                    true
                }
                transferred = futures::FutureExt::fuse(transfer_exact_bytes(client_stream, backend_stream, remaining)) => {
                    if transferred < remaining as u64 { return None; }
                    false
                }
            };
            if timed_out {
                let _ = client_stream
                    .write_all(ERR_MSG_REQUEST_TIMEOUT.to_vec())
                    .await;
                return Some((408, 0, false, true));
            }
        }
    }

    // 4. レスポンスを受信して転送（圧縮対応）
    let (total, status_code, backend_wants_keep_alive, client_must_close) =
        transfer_https_response_with_compression(
            backend_stream,
            client_stream,
            compression,
            client_encoding,
            security,
            wasm_modules,
        )
        .await;

    Some((
        status_code,
        total,
        backend_wants_keep_alive,
        client_must_close,
    ))
}

/// HTTPSレスポンス転送（圧縮対応版）
#[cfg_attr(not(feature = "wasm"), allow(unused_variables))]
async fn transfer_https_response_with_compression(
    backend_stream: &mut ClientTls,
    client_stream: &mut ServerTls,
    compression: &CompressionConfig,
    client_encoding: AcceptedEncoding,
    security: &SecurityConfig,
    wasm_modules: Arc<Vec<String>>,
) -> (u64, u16, bool, bool) {
    let mut accumulated = Vec::with_capacity(BUF_SIZE);
    let mut total = 0u64;
    let mut status_code = 502u16;
    // 初期値false: エラー時はKeep-Aliveを無効化
    let mut backend_wants_keep_alive = false;

    // ヘッダー読み取り用バッファ
    loop {
        // B-17: ヘッダー読取は専用の短いタイムアウトで打ち切り、504 へ即変換する
        let read_buf = buf_get();
        let read_result = timeout(BACKEND_HEADER_TIMEOUT, backend_stream.read(read_buf)).await;

        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => {
                warn!("Backend response timeout while reading headers");
                return (total, 504, false, true);
            }
        };

        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                warn!("Backend closed connection without sending response (read returned 0 bytes)");
                return (total, status_code, backend_wants_keep_alive, true);
            }
            Ok(n) => n,
            Err(e) => {
                buf_put(returned_buf);
                // kTLS使用時はEIO (os error 5) が発生することがある
                // これはバックエンドがTLS close_notifyを送信せずに接続を閉じた場合に発生
                if is_connection_closed_error(&e) {
                    debug!(
                        "Backend closed connection (kTLS EIO or connection reset): {}",
                        e
                    );
                } else {
                    warn!("Backend read error: {}", e);
                }
                return (total, status_code, backend_wants_keep_alive, true);
            }
        };

        returned_buf.set_valid_len(n);
        accumulated.extend_from_slice(returned_buf.as_valid_slice());
        buf_put(returned_buf);

        // B-11: バックエンド由来の 1xx 中間応答（100 Continue / 103 Early Hints 等）を
        // 読み捨てる（最終応答と誤認して転送するとクライアントが最終応答を待ち続ける）。
        drain_interim_responses(&mut accumulated);

        // ヘッダーが完全に受信されたかチェック
        if let Some(parsed) = parse_http_response(&accumulated) {
            status_code = parsed.status_code;
            backend_wants_keep_alive = !parsed.is_connection_close;

            let header_len = parsed.header_len;
            let body_start = &accumulated[header_len..];

            // Content-Type と Content-Encoding を取得
            let content_type = extract_header_value(&accumulated[..header_len], b"content-type");
            let existing_encoding =
                extract_header_value(&accumulated[..header_len], b"content-encoding");

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
                )
                .await;

                return (result.0, status_code, result.1, false);
            } else {
                // 圧縮無効: そのまま転送（ヘッダー追加処理）
                let mut modified_headers = accumulated[..header_len].to_vec();

                // ヘッダーをパースして操作
                let mut headers_storage = [httparse::EMPTY_HEADER; 64];
                let mut response = httparse::Response::new(&mut headers_storage);
                if response.parse(&modified_headers).is_ok() {
                    let mut new_header_lines = Vec::new();

                    // ステータス行を追加
                    let status_line = format!(
                        "HTTP/1.1 {} {}\r\n",
                        status_code,
                        status_code_to_reason(status_code)
                    );
                    new_header_lines.push(status_line.into_bytes());

                    // 既存のヘッダーを追加（削除対象を除外）
                    // eq_ignore_ascii_case でアロケーションなしに比較
                    for header in response.headers.iter() {
                        if !security
                            .remove_response_headers
                            .iter()
                            .any(|h| header.name.as_bytes().eq_ignore_ascii_case(h.as_bytes()))
                        {
                            new_header_lines.push(
                                format!(
                                    "{}: {}\r\n",
                                    header.name,
                                    std::str::from_utf8(header.value).unwrap_or("")
                                )
                                .into_bytes(),
                            );
                        }
                    }

                    // 追加するヘッダーを追加
                    for (header_name, header_value) in &security.add_response_headers {
                        new_header_lines
                            .push(format!("{}: {}\r\n", header_name, header_value).into_bytes());
                    }
                    // F-94: HTTP/3 広告（Alt-Svc）
                    if let Some(g) = get_alt_svc_guard() {
                        let mut line = Vec::with_capacity(16 + g.value().len());
                        line.extend_from_slice(b"Alt-Svc: ");
                        line.extend_from_slice(g.value());
                        line.extend_from_slice(b"\r\n");
                        new_header_lines.push(line);
                    }

                    // WASMレスポンスヘッダーフィルタを適用
                    #[cfg(feature = "wasm")]
                    {
                        if !wasm_modules.is_empty() {
                            let config = CURRENT_CONFIG.load();
                            if let Some(ref wasm_engine) = config.wasm_filter_engine {
                                let current_headers: Vec<(Vec<u8>, Vec<u8>)> = new_header_lines
                                    .iter()
                                    .skip(1)
                                    .filter_map(|line| {
                                        let line_str = std::str::from_utf8(line).ok()?;
                                        let line_trimmed = line_str.trim_end_matches("\r\n");
                                        if line_trimmed.is_empty() {
                                            return None;
                                        }
                                        let colon_pos = line_trimmed.find(':')?;
                                        let name = line_trimmed.as_bytes()[..colon_pos].to_vec();
                                        let value = line_trimmed[colon_pos + 1..]
                                            .trim_start()
                                            .as_bytes()
                                            .to_vec();
                                        Some((name, value))
                                    })
                                    .collect();

                                let wasm_result = wasm_engine
                                    .clone()
                                    .on_response_headers_with_modules_async(
                                        wasm_modules.clone(),
                                        status_code,
                                        current_headers,
                                        true,
                                    )
                                    .await;

                                if let crate::wasm::FilterResult::Continue {
                                    headers: modified_headers_wasm,
                                    ..
                                } = wasm_result
                                {
                                    new_header_lines.clear();
                                    let status_line = format!(
                                        "HTTP/1.1 {} {}\r\n",
                                        status_code,
                                        status_code_to_reason(status_code)
                                    );
                                    new_header_lines.push(status_line.into_bytes());
                                    for (name, value) in modified_headers_wasm {
                                        let mut line =
                                            Vec::with_capacity(name.len() + value.len() + 4);
                                        line.extend_from_slice(&name);
                                        line.extend_from_slice(b": ");
                                        line.extend_from_slice(&value);
                                        line.extend_from_slice(b"\r\n");
                                        new_header_lines.push(line);
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

                // F-59: ヘッダー + 初期ボディを平文接続では 1 回の SENDMSG で送出
                let body_data = body_start.to_vec();
                let write_result = timeout(
                    WRITE_TIMEOUT,
                    client_stream.write_all_vectored(modified_headers, body_data),
                )
                .await;
                if !matches!(write_result, Ok((Ok(()), _, _))) {
                    return (total, status_code, false, true);
                }
                total += header_len as u64;
                total += body_start.len() as u64;

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
                    )
                    .await;
                    total += transferred;

                    // B-17: Content-Length 宣言に満たないまま上流が終了した場合、
                    // クライアントは残りボディを待ち続けるため接続を即クローズして通知する
                    if parsed.content_length.is_some() && transferred < body_remaining as u64 {
                        warn!(
                            "Backend response body incomplete: {} < {}",
                            transferred, body_remaining
                        );
                        return (total, status_code, false, true);
                    }
                }

                return (total, status_code, backend_wants_keep_alive, false);
            }
        }

        // B-17: レスポンスヘッダーの上限超過は 502 で即時応答する
        if accumulated.len() > MAX_RESPONSE_HEADER_SIZE {
            warn!(
                "Backend response header too large (> {} bytes)",
                MAX_RESPONSE_HEADER_SIZE
            );
            return (0, 502, false, true);
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
    use flate2::write::GzEncoder;
    use flate2::Compression;
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
                    return transfer_uncompressed_fallback(
                        client_stream,
                        original_headers,
                        &body_data,
                    )
                    .await;
                }
            }
        }
        AcceptedEncoding::Gzip => {
            let level = Compression::new(compression.gzip_level);
            let mut encoder = GzEncoder::new(Vec::new(), level);
            if encoder.write_all(&body_data).is_err() {
                return transfer_uncompressed_fallback(client_stream, original_headers, &body_data)
                    .await;
            }
            match encoder.finish() {
                Ok(data) => data,
                Err(_) => {
                    return transfer_uncompressed_fallback(
                        client_stream,
                        original_headers,
                        &body_data,
                    )
                    .await;
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
                return transfer_uncompressed_fallback(client_stream, original_headers, &body_data)
                    .await;
            }
            compressed
        }
        AcceptedEncoding::Deflate => {
            let level = Compression::new(compression.gzip_level);
            let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), level);
            if encoder.write_all(&body_data).is_err() {
                return transfer_uncompressed_fallback(client_stream, original_headers, &body_data)
                    .await;
            }
            match encoder.finish() {
                Ok(data) => data,
                Err(_) => {
                    return transfer_uncompressed_fallback(
                        client_stream,
                        original_headers,
                        &body_data,
                    )
                    .await;
                }
            }
        }
        AcceptedEncoding::Identity => body_data,
    };

    // 3. 新しいヘッダーを構築
    let new_headers =
        build_compressed_headers(original_headers, encoding, compressed_body.len(), security);

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
    _backend_wants_keep_alive: bool,
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
            ChunkedFeedResult::SizeLimitExceeded => {
                return ChunkedTransferResult::SizeLimitExceeded
            }
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

#[cfg_attr(not(feature = "wasm"), allow(unused_variables))]
async fn handle_sendfile(
    mut tls_stream: ServerTls,
    base_path: &Path,
    is_dir: bool,
    index_filename: Option<&str>,
    req_path: &[u8],
    prefix: &[u8],
    client_wants_close: bool,
    security: &SecurityConfig,
    range_header: Option<&[u8]>, // RFC 7233 Range header support
    open_file_cache_config: Option<&cache::OpenFileCacheConfig>, // OpenFileCache設定（ルーティングごと）
    wasm_modules: Arc<Vec<String>>,
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
    let path_str = if let Some(qpos) = path_str_raw.find('?') {
        &path_str_raw[..qpos]
    } else {
        path_str_raw
    };
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
    let file_info = match cache::get_file_info_with_config(&full_path, open_file_cache_config).await
    {
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
        if let Some(base_info) =
            cache::get_file_info_with_config(base_path, open_file_cache_config).await
        {
            if !file_info
                .canonical_path
                .starts_with(&base_info.canonical_path)
            {
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
        match cache::get_file_info_with_config(&index_path, open_file_cache_config).await {
            Some(idx_info) if idx_info.is_file => (
                idx_info.canonical_path.clone(),
                idx_info.file_size,
                idx_info.mime_type.clone(),
            ),
            _ => {
                // インデックスファイルが存在しない場合は403 Forbidden
                let err_buf = ERR_MSG_FORBIDDEN.to_vec();
                let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                return Some((tls_stream, 403, 0, true));
            }
        }
    } else {
        (
            file_info.canonical_path.clone(),
            file_info.file_size,
            file_info.mime_type.clone(),
        )
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
            if let Some(first_range) = parsed.ranges.first() {
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
        // 数値→バイト列は itoa でスタックバッファに直接書き込む（String の一時確保を排除）。
        let mut num_buf = itoa::Buffer::new();
        header_buf.extend_from_slice(b"HTTP/1.1 206 Partial Content\r\nContent-Type: ");
        header_buf.extend_from_slice(mime_type.as_bytes());
        header_buf.extend_from_slice(b"\r\nAccept-Ranges: bytes\r\nContent-Range: bytes ");
        header_buf.extend_from_slice(num_buf.format(start).as_bytes());
        header_buf.extend_from_slice(b"-");
        header_buf.extend_from_slice(num_buf.format(end).as_bytes());
        header_buf.extend_from_slice(b"/");
        header_buf.extend_from_slice(num_buf.format(file_size).as_bytes());
        header_buf.extend_from_slice(b"\r\nContent-Length: ");
        header_buf.extend_from_slice(num_buf.format(content_length).as_bytes());
        header_buf.extend_from_slice(b"\r\n");
        (206u16, content_length)
    } else {
        // 通常のレスポンス
        header_buf.extend_from_slice(HTTP_200_PREFIX);
        header_buf.extend_from_slice(mime_type.as_bytes());
        header_buf.extend_from_slice(b"\r\nAccept-Ranges: bytes"); // Range サポートを通知
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
    // F-94: HTTP/3 広告（Alt-Svc）
    append_alt_svc_header_line(&mut header_buf);

    // WASMレスポンスヘッダーフィルタを適用（後段で Connection ヘッダーを追記するため mut）
    #[cfg(feature = "wasm")]
    let mut header_buf = {
        ftlog::info!(
            "[WASM Response] SendFile: wasm_modules count = {}",
            wasm_modules.len()
        );
        if !wasm_modules.is_empty() {
            let config = CURRENT_CONFIG.load();
            if let Some(ref wasm_engine) = config.wasm_filter_engine {
                // 現在のヘッダーをVec<(String, String)>形式に変換
                let header_str = String::from_utf8_lossy(&header_buf);
                let current_headers: Vec<(Vec<u8>, Vec<u8>)> = header_str
                    .lines()
                    .skip(1) // ステータス行をスキップ
                    .filter_map(|line| {
                        let line_trimmed = line.trim_end_matches("\r\n").trim_end_matches("\r");
                        if line_trimmed.is_empty() {
                            return None;
                        }
                        let colon_pos = line_trimmed.find(':')?;
                        let name = line_trimmed.as_bytes()[..colon_pos].to_vec();
                        let value = line_trimmed[colon_pos + 1..]
                            .trim_start()
                            .as_bytes()
                            .to_vec();
                        Some((name, value))
                    })
                    .collect();

                // WASMフィルタを実行（レスポンスヘッダー処理）
                let wasm_result = wasm_engine
                    .clone()
                    .on_response_headers_with_modules_async(
                        wasm_modules.clone(),
                        200,
                        current_headers,
                        true, // end_of_stream
                    )
                    .await;

                match wasm_result {
                    crate::wasm::FilterResult::Continue {
                        headers: modified_headers,
                        ..
                    } => {
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

    // Connection ヘッダーを追加（header_buf は上で `let mut` 済み）
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
            return handle_sendfile_zerocopy(
                tls_stream,
                &file,
                transfer_offset,
                transfer_length,
                client_wants_close,
                response_status,
            )
            .await;
        }
    }

    // kTLS が無効な場合は従来の read/write を使用
    handle_sendfile_userspace(
        tls_stream,
        &file,
        transfer_offset,
        transfer_length,
        client_wants_close,
        response_status,
    )
    .await
}

/// kTLS + sendfile によるゼロコピーファイル送信
///
/// kTLS が有効な場合に使用されます。
/// ファイルの内容をカーネル空間で直接 TLS 暗号化して送信します。
#[cfg(feature = "ktls")]
async fn handle_sendfile_zerocopy(
    tls_stream: ServerTls,
    file: &crate::runtime::io::File,
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
                    if (tls_stream.get_ref().writable().await).is_err() {
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
    file: &crate::runtime::io::File,
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

#[cfg(all(test, feature = "http2"))]
mod streaming_tests {
    use super::*;

    /// chunked チャンクサイズ行のエンコードが正しいこと（F-32 リクエストストリーミング）。
    #[test]
    fn test_push_chunk_size_line() {
        fn line(n: usize) -> Vec<u8> {
            let mut b = Vec::new();
            push_chunk_size_line(&mut b, n);
            b
        }
        assert_eq!(line(0), b"0\r\n");
        assert_eq!(line(1), b"1\r\n");
        assert_eq!(line(15), b"f\r\n");
        assert_eq!(line(16), b"10\r\n");
        assert_eq!(line(255), b"ff\r\n");
        assert_eq!(line(256), b"100\r\n");
        assert_eq!(line(65535), b"ffff\r\n");
        assert_eq!(line(16384), b"4000\r\n");
        // 既存バッファへの追記であること（前の内容を保持）
        let mut b = b"prefix".to_vec();
        push_chunk_size_line(&mut b, 16384);
        assert_eq!(b, b"prefix4000\r\n");
    }

    /// チャンクサイズ行が標準ライブラリの 16 進表現と一致すること（網羅確認）。
    #[test]
    fn test_push_chunk_size_line_matches_hex() {
        for n in [
            0usize, 1, 7, 9, 10, 16, 100, 1000, 4096, 16384, 65535, 65536, 1_048_576, 200_000,
        ] {
            let mut b = Vec::new();
            push_chunk_size_line(&mut b, n);
            let expected = format!("{:x}\r\n", n).into_bytes();
            assert_eq!(b, expected, "mismatch for n={}", n);
        }
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    /// B-40: gRPC はルート /* プレフィックスを剥がさずフルパスを維持する。
    #[test]
    fn test_b40_grpc_preserves_full_path() {
        let path = "/grpc.test.v1.TestService/UnaryCall";
        let prefix = b"/grpc.test.v1.TestService";
        let full = compute_upstream_path(path, prefix, "", true);
        assert_eq!(full, path);
        // 非 gRPC では従来どおり prefix 除去
        let stripped = compute_upstream_path(path, prefix, "", false);
        assert_eq!(stripped, "/UnaryCall");
    }

    #[test]
    fn test_b40_grpc_path_empty_becomes_slash() {
        assert_eq!(compute_upstream_path("", b"", "", true), "/");
    }

    #[test]
    fn test_header_pair_is_grpc() {
        assert!(header_pair_is_grpc(b"content-type", b"application/grpc"));
        assert!(header_pair_is_grpc(
            b"Content-Type",
            b"application/grpc+proto"
        ));
        assert!(!header_pair_is_grpc(b"content-type", b"text/plain"));
        assert!(!header_pair_is_grpc(b"accept", b"application/grpc"));
    }

    #[test]
    fn test_is_native_grpc_content_type() {
        // ネイティブ gRPC
        assert!(is_native_grpc_content_type(b"application/grpc"));
        assert!(is_native_grpc_content_type(b"application/grpc+proto"));
        assert!(is_native_grpc_content_type(b"application/grpc+json"));
        assert!(is_native_grpc_content_type(b"Application/GRPC"));
        assert!(is_native_grpc_content_type(
            b"application/grpc; charset=utf-8"
        ));
        // gRPC-Web は除外
        assert!(!is_native_grpc_content_type(b"application/grpc-web"));
        assert!(!is_native_grpc_content_type(b"application/grpc-web+proto"));
        assert!(!is_native_grpc_content_type(b"application/grpc-web-text"));
        // その他
        assert!(!is_native_grpc_content_type(b"text/plain"));
        assert!(!is_native_grpc_content_type(b"application/json"));
        assert!(!is_native_grpc_content_type(b"application/grp"));
    }

    #[test]
    fn test_request_bytes_indicate_grpc() {
        let grpc = b"POST /x HTTP/1.1\r\nHost: h\r\nContent-Type: application/grpc\r\n\r\n";
        assert!(request_bytes_indicate_grpc(grpc));
        let plain = b"POST /x HTTP/1.1\r\nHost: h\r\nContent-Type: text/plain\r\n\r\n";
        assert!(!request_bytes_indicate_grpc(plain));
        let mixed = b"POST /x HTTP/1.1\r\ncontent-type: Application/GRPC+proto\r\n\r\n";
        assert!(request_bytes_indicate_grpc(mixed));
    }
}

#[cfg(all(test, feature = "http2"))]
mod connect_gate_tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    // io_uring/epoll を許可しない環境（Docker ビルドサンドボックス・古いカーネル等）では
    // ランタイムドライバの生成が失敗し `block_on` が panic するため、ランタイムを要する
    // テストはスキップする（`src/l4/proxy.rs` の tests と同じパターン）。
    #[cfg(veil_rt_uring)]
    fn io_uring_available() -> bool {
        crate::runtime::ring::IoUring::new(8, 0).is_ok()
    }

    /// reactor（epoll）ビルドでは `epoll_create1` の成否をランタイム可用性の代替指標とする。
    #[cfg(veil_rt_reactor)]
    fn io_uring_available() -> bool {
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd >= 0 {
            unsafe { libc::close(fd) };
            true
        } else {
            false
        }
    }

    /// ConnectPermit の Drop が in_flight を確実に減算し、待機者へ通知すること（B-44 第3段）。
    #[test]
    fn test_connect_permit_drop_releases_and_notifies() {
        if !io_uring_available() {
            eprintln!(
                "io_uring unavailable; skipping test_connect_permit_drop_releases_and_notifies"
            );
            return;
        }

        let gate = Rc::new(ConnectGate::new());
        let waiter = crate::stream_channel::Notify::new();
        gate.waiters.borrow_mut().push_back(waiter.clone());

        let permit = ConnectPermit::try_acquire(&gate).expect("must acquire on empty gate");
        assert_eq!(gate.in_flight.get(), 1);

        drop(permit);
        assert_eq!(gate.in_flight.get(), 0, "Drop must decrement in_flight");
        assert!(
            gate.waiters.borrow().is_empty(),
            "Drop must drain and notify waiters"
        );
        // 通知フラグが立っている = wait() が即 Ready になる
        crate::runtime::block_on(async move {
            waiter.wait().await;
        });
    }

    /// 同時 65 個目の取得は wait に入り、スロット解放（permit Drop）後に進むこと（B-44 第3段）。
    #[test]
    fn test_gate_blocks_at_capacity_and_resumes_after_release() {
        if !io_uring_available() {
            eprintln!(
                "io_uring unavailable; skipping test_gate_blocks_at_capacity_and_resumes_after_release"
            );
            return;
        }

        crate::runtime::block_on(async {
            let gate = Rc::new(ConnectGate::new());
            // 64 スロットをすべて占有する
            let mut permits: Vec<ConnectPermit> = (0..MAX_CONCURRENT_CONNECTS_PER_HOST)
                .map(|_| ConnectPermit::try_acquire(&gate).expect("slot must be available"))
                .collect();
            assert_eq!(gate.in_flight.get(), MAX_CONCURRENT_CONNECTS_PER_HOST);
            assert!(
                ConnectPermit::try_acquire(&gate).is_none(),
                "65th acquisition must be rejected while the gate is full"
            );

            // 65 個目の取得タスク: acquire_backend_conn と同じ待機ループを実行する
            let progressed = Rc::new(Cell::new(false));
            let gate_task = gate.clone();
            let progressed_task = progressed.clone();
            crate::runtime::executor::spawn(async move {
                let mut waiter: Option<crate::stream_channel::Notify> = None;
                loop {
                    if let Some(_permit) = ConnectPermit::try_acquire(&gate_task) {
                        progressed_task.set(true);
                        return;
                    }
                    let w = waiter.get_or_insert_with(crate::stream_channel::Notify::new);
                    gate_task.waiters.borrow_mut().push_back(w.clone());
                    w.wait().await;
                }
            });

            // スポーンしたタスクに実行機会を与える（単一スレッドなので決定的）
            crate::runtime::timer::sleep(Duration::from_millis(10)).await;
            assert!(
                !progressed.get(),
                "the 65th acquirer must wait while the gate is full"
            );

            // 1 スロット解放 → permit の Drop が待機者を起こす
            permits.pop();
            crate::runtime::timer::sleep(Duration::from_millis(10)).await;
            assert!(
                progressed.get(),
                "the 65th acquirer must resume after a slot is released"
            );
        });
    }
}
