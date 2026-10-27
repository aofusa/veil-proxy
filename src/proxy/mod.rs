//! プロキシ処理モジュール
//!
//! すべてのプロキシハンドリングロジックを担当します。
//! HTTP/1.1、HTTP/2、WebSocket、ファイル配信などに対応。

use std::sync::Arc;

use crate::runtime::buf::{IoBuf, IoBufMut};
use crate::runtime::io::OpenOptions;
use crate::runtime::io::{AsyncReadRent, AsyncWriteRentExt, IoVecBuf, IoVecBufMut};
use crate::runtime::tcp::TcpStream;
use crate::runtime::time::{timeout, timeout_idle};
use bytes::{Bytes, BytesMut};
use ftlog::{debug, error, warn};
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
use crate::runtime::handle::{AsRawFd, RawFd};

use crate::server::spawn_background_revalidation;

#[cfg(feature = "http2")]
use crate::http2;
#[cfg(veil_ktls)]
use crate::ktls_rustls::{KtlsServerStream, RustlsAcceptor};
// SplicePipe は splice(2)（Linux 専用）用。FreeBSD kTLS は splice を使わずバッファ転送へ
// フォールバックするため Linux 限定で import する（F-126）。
#[cfg(all(veil_ktls, target_os = "linux"))]
use crate::ktls_rustls::SplicePipe;
#[cfg(not(veil_ktls))]
use crate::simple_tls;

// ServerTls型エイリアス（main.rsから再エクスポート）
#[cfg(veil_ktls)]
use crate::ktls_rustls::KtlsServerStream as ServerTls;
#[cfg(not(veil_ktls))]
use crate::simple_tls::SimpleTlsServerStream as ServerTls;

// F-233: 機能ごとのファイル（子モジュールは `use super::*;` で親の名前を見る）
#[cfg(feature = "http2")]
mod h2;
#[cfg(feature = "http2")]
pub(crate) use self::h2::*;
#[cfg(feature = "http2")]
mod h2_request_body;
#[cfg(feature = "http2")]
use self::h2_request_body::*;
mod detect;
pub use self::detect::*;
mod h1;
use self::h1::*;
mod websocket;
use self::websocket::*;
mod forward;
use self::forward::*;
mod buffered;
use self::buffered::*;
mod compress;
use self::compress::*;
#[cfg(veil_ktls)]
mod splice;
#[cfg(veil_ktls)]
use self::splice::*;
mod https;
use self::https::*;
mod transfer;
pub(crate) use self::transfer::collect_response_body_raw;
use self::transfer::*;
mod sendfile;
use self::sendfile::*;
// ====================
// 接続処理
// ====================

/// バックエンドへの TCP 接続を確立する（F-155: 事前解決済み `SocketAddr` の活用）。
///
/// `target.socket_addr` が `Some`（`host` が IP アドレスリテラルで設定ロード時に
/// 解決済み）の場合はそれを使って直接 `TcpStream::connect` する。`None`（`host` が
/// ホスト名）の場合のみ、従来どおり `addr`（`HostPortStr` 経由で構築済みの
/// `"host:port"` 文字列）を使って `TcpStream::connect_str` の同期名前解決経路へ
/// フォールバックする。呼び出し側で `timeout(..)` を被せる方式は変更しない
/// （エラー種別・502/504 分岐・ログは呼び出し元がこれまでどおり処理する）。
///
/// ホットパスのため追加のアロケーション・ヒープボックス化は一切行わない。
#[inline]
pub(crate) async fn connect_target(target: &ProxyTarget, addr: &str) -> io::Result<TcpStream> {
    if let Some(sock_addr) = target.socket_addr {
        return TcpStream::connect(sock_addr).await;
    }
    // F-170: UDS バックエンド。`target.unix_path` があるので `connect_str` の
    // `unix:` 接頭辞判定を経由せず直接 `connect_unix` を呼ぶ（`connect_str` 側でも
    // `unix:` を扱えるため二重に安全だが、ホットパスで余計な文字列走査をしない）。
    //
    // **`#[cfg(unix)]` は必須**（B-69 クラス）: `unix_path` は非 unix でも
    // 存在する（常に `None`）フィールドなので cfg を付けないとこの分岐が
    // Windows でもコンパイル対象になり、`reactor::tcp::windows::TcpStream` に
    // 無い `connect_unix` を呼んで**クロスビルドだけが壊れる**（Linux の
    // 単体・統合・E2E はすべて通過する）。非 unix では `unix_path` が常に `None`
    // のため、素通りして `connect_str` へ落ちる挙動で正しい（`connect_str` 側も
    // `unix:` を非対応エラーで弾く）。
    #[cfg(unix)]
    if let Some(path) = &target.unix_path {
        return TcpStream::connect_unix(std::path::Path::new(path.as_ref())).await;
    }
    TcpStream::connect_str(addr).await
}

/// プロキシ起動時刻（F-21: 管理API /stats 用）
#[cfg(feature = "admin")]
static PROXY_START_TIME: once_cell::sync::Lazy<std::time::Instant> =
    once_cell::sync::Lazy::new(std::time::Instant::now);

// ====================
// 共通セキュリティチェック（HTTP/1.1, HTTP/2, HTTP/3 共用）
// ====================
//
// プロトコル非依存のセキュリティチェックは src/l7.rs（F-234）。ここは各プロトコルハンドラーが
// 使う名前を再エクスポートする（`crate::proxy::check_security` 等の既存のパスを保つ）。
pub use crate::l7::{check_security, SecurityCheckResult};

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

// ====================
// 共通のヘルパー（HTTP/1.1・HTTP/2・WebSocket で使う上流のパスの組み立て・gRPC の判定等）
// ====================

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

/// F-177: HTTP/1.1 の要求バイト列の先頭（メソッド）が冪等か。
fn request_line_is_idempotent(request: &[u8]) -> bool {
    let method_end = request.iter().position(|&b| b == b' ').unwrap_or(0);
    crate::http_utils::is_idempotent_method(&request[..method_end])
}

// HTTP/1.1・HTTP/2 の両方から使う（F-233 の分割で http2 の子に入っていた）
/// 管理 API の `GET {path_prefix}/vds`（F-211）。VDS が無効・未対応のビルドでは `{"enabled":false}`。
#[cfg(feature = "admin")]
fn admin_vds_json() -> String {
    #[cfg(feature = "vds")]
    return crate::vds::status_json();
    #[cfg(not(feature = "vds"))]
    return "{\"enabled\":false}".to_string();
}

/// スレッドローカルの zstd 圧縮コンテキストを使い回して `body` を圧縮する。
/// 失敗したら `None`（呼び出し側が無圧縮へフォールバックする）。
///
/// B-99: HTTP/1.1 のプロキシ圧縮経路（`transfer_compressed_response` /
/// `transfer_https_compressed_response`）は F-169 の後も `zstd::encode_all`（呼び出しごとに
/// コンテキストを確保・初期化するワンショット API）のままで、54KB の zstd レベル 3 に
/// 1 リクエストあたり約 1.6ms の CPU を使っていた（静的配信の 6 倍）。HTTP/2・HTTP/3 と
/// 同じくこの関数でコンテキストを使い回す。
#[cfg(feature = "compression")]
fn zstd_compress_reused(body: &[u8], level: i32) -> Option<Vec<u8>> {
    use std::cell::RefCell;

    thread_local! {
        static ZSTD_COMPRESSOR: RefCell<Option<zstd::bulk::Compressor<'static>>> =
            const { RefCell::new(None) };
    }

    ZSTD_COMPRESSOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        let compressor = match slot.as_mut() {
            Some(c) => c,
            None => slot.get_or_insert(zstd::bulk::Compressor::new(level).ok()?),
        };
        compressor.set_compression_level(level).ok()?;
        compressor.compress(body).ok()
    })
}

// ====================
// 上流へ転送する要求ヘッダの方針（F-236 の監査の F1・F2・F4）
// ====================

/// `[route.security] remove_request_headers` に入っていない（上流へ送ってよい）名前か。
/// 設定が空なら分岐 1 個で抜ける。
#[inline]
pub(crate) fn forward_allowed(security: &SecurityConfig, name: &[u8]) -> bool {
    security.remove_request_headers.is_empty()
        || !security
            .remove_request_headers
            .iter()
            .any(|h| name.eq_ignore_ascii_case(h.as_bytes()))
}

/// 上流への要求に足すヘッダ（`[route.security] add_request_headers` と既定の転送ヘッダ。B-232）。
///
/// 転送ヘッダはクライアントの IP をスタックに持つだけで、要求ごとに確保しない（`add_request_headers` が無ければ
/// `user` も空のまま）。受けた要求のヘッダを送るかは `keeps` で決める。
pub(crate) struct AddedHeaders {
    user: Vec<(Vec<u8>, Vec<u8>)>,
    fwd: Option<ForwardedValues>,
}

/// 既定の転送ヘッダの値（B-232）。
#[derive(Clone, Copy)]
pub(crate) struct ForwardedValues {
    ip: crate::http_utils::IpStr,
    /// 送信元が `trusted_proxies` に入る（受けた転送ヘッダを残す）
    trusted: bool,
    add_xff: bool,
}

/// 転送ヘッダの名前か（信頼しない送信元から受けたものは上流へ送らない）。
#[inline]
fn is_forwarded_header(name: &[u8]) -> bool {
    // 先頭の文字で大半を外す（要求ごとに全ヘッダへ掛かる）
    match name.first() {
        Some(b'x' | b'X') => {
            name.eq_ignore_ascii_case(b"x-forwarded-for")
                || name.eq_ignore_ascii_case(b"x-real-ip")
                || name.eq_ignore_ascii_case(b"x-forwarded-proto")
        }
        Some(b'f' | b'F') => name.eq_ignore_ascii_case(b"forwarded"),
        _ => false,
    }
}

impl ForwardedValues {
    /// ルートの方針と送信元から作る（`passthrough` なら `None`）。
    pub(crate) fn new(security: &SecurityConfig, client_ip: &str) -> Option<Self> {
        if security.forwarded_headers == crate::config::ForwardedHeadersMode::Passthrough {
            return None;
        }
        let ip = crate::http_utils::IpStr::from_ip_text(client_ip)?;
        // nginx・Envoy と同じく既定で足すのは X-Forwarded-For だけ（X-Real-IP は add_request_headers で足す）
        let add_xff = security.add_request_headers.is_empty()
            || !security
                .add_request_headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("x-forwarded-for"));
        Some(Self {
            ip,
            trusted: security.is_trusted_proxy(client_ip),
            add_xff,
        })
    }

    /// 受けた要求のヘッダ `name` を上流へ送ってよいか。
    #[inline]
    pub(crate) fn keeps(&self, name: &[u8]) -> bool {
        self.trusted || !is_forwarded_header(name)
    }

    /// 足すヘッダ（`X-Forwarded-For`）。
    pub(crate) fn pairs(&self) -> impl Iterator<Item = (&[u8], &[u8])> + Clone {
        self.add_xff
            .then_some((&b"x-forwarded-for"[..], self.ip.as_str().as_bytes()))
            .into_iter()
    }
}

impl AddedHeaders {
    /// 何も足さない（受けたヘッダの方針も変えない）。HTTP/3 のバッファ経路だけが使う
    #[cfg(feature = "http3")]
    pub(crate) const NONE: AddedHeaders = AddedHeaders {
        user: Vec::new(),
        fwd: None,
    };

    /// 受けた要求のヘッダ `name` を上流へ送ってよいか（転送ヘッダの方針）。
    #[inline]
    pub(crate) fn keeps(&self, name: &[u8]) -> bool {
        self.fwd.as_ref().is_none_or(|f| f.keeps(name))
    }

    /// 足すヘッダ（`add_request_headers` の展開、続けて転送ヘッダ）。
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&[u8], &[u8])> + Clone {
        self.user
            .iter()
            .map(|(n, v)| (n.as_slice(), v.as_slice()))
            .chain(self.fwd.iter().flat_map(|f| f.pairs()))
    }
}

/// `[route.security] add_request_headers` を展開する（`$client_ip`・`$host`・`$request_uri`）。設定が無ければ
/// 確保しない（空の `Vec`）。値に CR・LF・NUL が入るもの（展開した値も含む）は送らない。既定の転送ヘッダ
/// （`forwarded_headers`。B-232）も持つ。
pub(crate) fn expanded_added_headers(
    security: &SecurityConfig,
    client_ip: &str,
    host: &[u8],
    request_uri: &str,
) -> AddedHeaders {
    let fwd = ForwardedValues::new(security, client_ip);
    if security.add_request_headers.is_empty() {
        return AddedHeaders {
            user: Vec::new(),
            fwd,
        };
    }
    let host = std::str::from_utf8(host).unwrap_or("-");
    let mut out = Vec::with_capacity(security.add_request_headers.len());
    for (name, value) in &security.add_request_headers {
        let mut v = Vec::with_capacity(value.len());
        expand_request_header_value(value, client_ip, host, request_uri, &mut v);
        if is_valid_header_name(name.as_bytes()) && is_valid_header_value(&v) {
            out.push((name.as_bytes().to_ascii_lowercase(), v));
        }
    }
    AddedHeaders { user: out, fwd }
}

/// HTTP/2・HTTP/3 で受けた要求のヘッダを、HTTP/1.1 の上流への要求に書く。
///
/// 送らないもの: 疑似ヘッダ・ホップバイホップ（connection・keep-alive・transfer-encoding・upgrade・proxy-connection）・
/// `content-length`（本文から付け直す）・`host`（`:authority` から作った Host を先に書いている）・`expect`（プロキシが終端する）・
/// `remove_request_headers`・名前が token でないもの・値に CR・LF・NUL を含むもの（HPACK・QPACK は任意のバイトを
/// 運べるので、HTTP/1.1 では要求の密輸になる。F1・F2）。最後に `add_request_headers` を足す（F4）。
#[cfg(any(feature = "http2", feature = "http3"))]
pub(crate) fn write_h1_forward_headers<'a>(
    out: &mut Vec<u8>,
    headers: impl Iterator<Item = (&'a [u8], &'a [u8])>,
    security: &SecurityConfig,
    added: &AddedHeaders,
) {
    for (name, value) in headers {
        if name.starts_with(b":")
            || name.eq_ignore_ascii_case(b"connection")
            || name.eq_ignore_ascii_case(b"keep-alive")
            || name.eq_ignore_ascii_case(b"transfer-encoding")
            || name.eq_ignore_ascii_case(b"upgrade")
            || name.eq_ignore_ascii_case(b"proxy-connection")
            || name.eq_ignore_ascii_case(b"content-length")
            || name.eq_ignore_ascii_case(b"host")
            || name.eq_ignore_ascii_case(b"expect")
            || !forward_allowed(security, name)
            || !added.keeps(name)
            || !is_valid_header_name(name)
            || !is_valid_header_value(value)
        {
            continue;
        }
        out.extend_from_slice(name);
        out.extend_from_slice(b": ");
        out.extend_from_slice(value);
        out.extend_from_slice(b"\r\n");
    }
    for (name, value) in added.iter() {
        out.extend_from_slice(name);
        out.extend_from_slice(b": ");
        out.extend_from_slice(value);
        out.extend_from_slice(b"\r\n");
    }
}

// ====================
// ロギング
// ====================

// log_access は crate::logging モジュールに移動しました。

// ====================
// ユニットテスト
// ====================

#[cfg(test)]
mod path_tests {
    use super::*;

    /// B-232: 信頼しない送信元の転送ヘッダは捨ててクライアントの IP を入れる。trusted_proxies からは残して
    /// X-Forwarded-For を足す。passthrough は以前のまま
    #[cfg(any(feature = "http2", feature = "http3"))]
    #[test]
    fn forwarded_headers_policy() {
        let hdrs = || -> Vec<(&'static [u8], &'static [u8])> {
            vec![
                (b"X-Forwarded-For", b"1.2.3.4"),
                (b"x-real-ip", b"1.2.3.4"),
                (b"X-Forwarded-Proto", b"https"),
                (b"forwarded", b"for=1.2.3.4"),
                (b"x-other", b"v"),
            ]
        };
        let write = |sec: &SecurityConfig, ip: &str| {
            let added = expanded_added_headers(sec, ip, b"h", "/");
            let mut out = Vec::new();
            write_h1_forward_headers(&mut out, hdrs().into_iter(), sec, &added);
            String::from_utf8(out).unwrap()
        };
        let mut sec = SecurityConfig::default();
        assert_eq!(
            write(&sec, "198.51.100.7"),
            "x-other: v\r\nx-forwarded-for: 198.51.100.7\r\n"
        );
        sec.trusted_proxies = vec!["10.0.0.0/8".into()];
        assert_eq!(
            write(&sec, "10.1.2.3"),
            "X-Forwarded-For: 1.2.3.4\r\nx-real-ip: 1.2.3.4\r\nX-Forwarded-Proto: https\r\n\
             forwarded: for=1.2.3.4\r\nx-other: v\r\nx-forwarded-for: 10.1.2.3\r\n"
        );
        // 信頼リストの外は捨てる（IPv4 射影も IPv4 として見る）
        assert!(write(&sec, "::ffff:10.1.2.3").starts_with("X-Forwarded-For: 1.2.3.4"));
        assert!(!write(&sec, "192.0.2.9").contains("1.2.3.4"));
        sec.forwarded_headers = crate::config::ForwardedHeadersMode::Passthrough;
        assert_eq!(
            write(&sec, "192.0.2.9"),
            "X-Forwarded-For: 1.2.3.4\r\nx-real-ip: 1.2.3.4\r\nX-Forwarded-Proto: https\r\n\
             forwarded: for=1.2.3.4\r\nx-other: v\r\n"
        );
    }

    /// F-236 の F1・F4: HTTP/2・HTTP/3 の要求を HTTP/1.1 へ書くとき、密輸になるもの・削除の指定を落とし、追加を足す
    #[cfg(any(feature = "http2", feature = "http3"))]
    #[test]
    fn h1_forward_headers_apply_policy_and_drop_smuggling() {
        let sec = SecurityConfig {
            remove_request_headers: vec!["x-internal-user".to_string()],
            add_request_headers: [("x-real-ip".to_string(), "$client_ip".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let added = expanded_added_headers(&sec, "192.0.2.1", b"example.com", "/p");
        let hdrs: Vec<(&[u8], &[u8])> = vec![
            (b":method", b"GET"),
            (b"x-ok", b"1"),
            (b"X-Internal-User", b"admin"),
            (b"content-length", b"0"),
            (b"transfer-encoding", b"chunked"),
            (b"x-evil", b"a\r\nTransfer-Encoding: chunked"),
            (b"bad name", b"v"),
        ];
        let mut out = Vec::new();
        write_h1_forward_headers(&mut out, hdrs.into_iter(), &sec, &added);
        let s = String::from_utf8(out).unwrap();
        // B-232: add_request_headers の x-real-ip を使い、既定の転送ヘッダは X-Forwarded-For だけ足す
        assert_eq!(
            s,
            "x-ok: 1\r\nx-real-ip: 192.0.2.1\r\nx-forwarded-for: 192.0.2.1\r\n"
        );
        assert!(forward_allowed(&sec, b"x-ok"));
        assert!(!forward_allowed(&sec, b"x-INTERNAL-user"));
    }

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

#[cfg(test)]
mod add_request_header_tests {
    use super::*;

    /// テスト用ヘルパ: 展開結果を `String` で受け取る。
    fn expand(template: &str) -> String {
        let mut out = Vec::new();
        expand_request_header_value(template, "10.0.0.1", "example.test", "/a/b?q=1", &mut out);
        String::from_utf8(out).expect("展開結果は UTF-8")
    }

    #[test]
    fn f168_p4_no_placeholder_is_passed_through() {
        assert_eq!(expand("https"), "https");
        assert_eq!(expand(""), "");
    }

    #[test]
    fn f168_p4_each_placeholder_expands() {
        assert_eq!(expand("$client_ip"), "10.0.0.1");
        assert_eq!(expand("$host"), "example.test");
        assert_eq!(expand("$request_uri"), "/a/b?q=1");
    }

    #[test]
    fn f168_p4_mixed_and_adjacent_placeholders() {
        assert_eq!(
            expand("ip=$client_ip; host=$host; uri=$request_uri"),
            "ip=10.0.0.1; host=example.test; uri=/a/b?q=1"
        );
        // 連続（区切り無し）でも取り違えない
        assert_eq!(expand("$host$request_uri"), "example.test/a/b?q=1");
    }

    #[test]
    fn f168_p4_unknown_placeholder_is_kept() {
        assert_eq!(expand("$foo"), "$foo");
        assert_eq!(expand("a$foo$hostb"), "a$fooexample.testb");
    }

    #[test]
    fn f168_p4_trailing_dollar_is_kept() {
        // 末尾が裸の `$` でも境界外アクセスせずそのまま残す
        assert_eq!(expand("value$"), "value$");
        assert_eq!(expand("$"), "$");
        // プレースホルダ名の途中で切れている場合もそのまま
        assert_eq!(expand("$hos"), "$hos");
        assert_eq!(expand("$client_i"), "$client_i");
    }

    #[test]
    fn f168_p4_expanded_value_is_not_re_replaced() {
        // 旧実装（String::replace の 3 連鎖）は client_ip の中の "$host" まで
        // 置換してしまっていた。1 パス展開ではそれが起きないことを固定する。
        let mut out = Vec::new();
        expand_request_header_value("$client_ip", "x$host", "H", "/u", &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "x$host");
    }
}
