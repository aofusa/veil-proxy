//! アップストリーム選択・ヘルスチェックモジュール
//!
//! バックエンド検索、条件マッチング、ヘルスチェック関数を提供します。

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream as StdTcpStream};
use std::sync::Arc;
use std::time::Duration;
use ftlog::{info, warn, debug};
use crate::config::*;
use crate::constants::*;
use crate::pool::*;
use crate::routing;

#[cfg(feature = "ktls")]
use crate::ktls_rustls::RustlsConnector;

/// 同期的な健康チェックを実行
/// 
/// TCP 接続して HTTP GET リクエストを送信し、レスポンスをチェック。
/// TLS接続もサポート（use_tls=true時）。
pub(crate) fn perform_health_check(
    addr: &str,
    host: &str,
    path: &str,
    use_tls: bool,
    verify_cert: bool,
    timeout: Duration,
    healthy_statuses: &[u16],
) -> bool {
    use std::net::TcpStream as StdTcpStream;
    use std::io::{Read, Write, ErrorKind};
    use rustls::{ClientConfig, ClientConnection, RootCertStore};
    use rustls::pki_types::ServerName;
    use std::sync::Arc;
    
    // TCP 接続
    let mut tcp_stream = match StdTcpStream::connect_timeout(
        &addr.parse().unwrap_or_else(|_| std::net::SocketAddr::from(([127, 0, 0, 1], 80))),
        timeout,
    ) {
        Ok(s) => s,
        Err(_) => return false,
    };
    
    let _ = tcp_stream.set_read_timeout(Some(timeout));
    let _ = tcp_stream.set_write_timeout(Some(timeout));
    
    // TLS接続の場合
    if use_tls {
        // rustls クライアント設定
        let config: Arc<ClientConfig> = if verify_cert {
            // 証明書検証を有効化（デフォルトのルート証明書ストアを使用）
            let mut root_store = RootCertStore::empty();
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            Arc::new(ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth())
        } else {
            // 証明書検証を完全に無効化（自己署名証明書を許可）
            use rustls::client::danger::{ServerCertVerifier, HandshakeSignatureValid, ServerCertVerified};
            use rustls::pki_types::{CertificateDer, UnixTime};
            use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};

            #[derive(Debug)]
            struct NoVerify;

            impl ServerCertVerifier for NoVerify {
                fn verify_server_cert(
                    &self, _: &CertificateDer, _: &[CertificateDer],
                    _: &rustls::pki_types::ServerName, _: &[u8], _: UnixTime,
                ) -> Result<ServerCertVerified, TlsError> {
                    Ok(ServerCertVerified::assertion())
                }
                fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsError> {
                    Ok(HandshakeSignatureValid::assertion())
                }
                fn verify_tls13_signature(&self, _: &[u8], _: &CertificateDer, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsError> {
                    Ok(HandshakeSignatureValid::assertion())
                }
                fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
                    vec![
                        SignatureScheme::RSA_PKCS1_SHA256, SignatureScheme::RSA_PKCS1_SHA384,
                        SignatureScheme::RSA_PKCS1_SHA512, SignatureScheme::ECDSA_NISTP256_SHA256,
                        SignatureScheme::ECDSA_NISTP384_SHA384, SignatureScheme::RSA_PSS_SHA256,
                        SignatureScheme::RSA_PSS_SHA384, SignatureScheme::RSA_PSS_SHA512,
                    ]
                }
            }

            Arc::new(ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerify))
                .with_no_client_auth())
        };
        
        // SNI名を決定
        let server_name = match ServerName::try_from(host.to_string()) {
            Ok(name) => name,
            Err(_) => return false,
        };
        
        // TLS接続を確立
        let mut tls_conn = match ClientConnection::new(config, server_name) {
            Ok(conn) => conn,
            Err(_) => return false,
        };
        
        // ハンドシェイクを実行（同期）
        while tls_conn.is_handshaking() {
            match tls_conn.complete_io(&mut tcp_stream) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    // 非ブロッキングI/Oの場合は短い待機
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(_) => return false,
            }
        }
        
        // rustls::Streamを使用して読み書き
        let mut stream = rustls::Stream::new(&mut tls_conn, &mut tcp_stream);
        
        // HTTP リクエスト送信
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: HealthCheck/1.0\r\n\r\n",
            path, host
        );
        
        if stream.write_all(request.as_bytes()).is_err() {
            return false;
        }
        
        // レスポンス読み取り
        let mut response = [0u8; 1024];
        let n = match stream.read(&mut response) {
            Ok(n) if n > 0 => n,
            _ => return false,
        };
        
        // ステータスコードを抽出
        let response_str = String::from_utf8_lossy(&response[..n]);
        if let Some(status_line) = response_str.lines().next() {
            // "HTTP/1.1 200 OK" のようなパターン
            let parts: Vec<&str> = status_line.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(status_code) = parts[1].parse::<u16>() {
                    return healthy_statuses.contains(&status_code);
                }
            }
        }
        
        false
    } else {
        // HTTP接続（既存の実装）
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: HealthCheck/1.0\r\n\r\n",
            path, host
        );
        
        if tcp_stream.write_all(request.as_bytes()).is_err() {
            return false;
        }
        
        // レスポンス読み取り
        let mut response = [0u8; 1024];
        let n = match tcp_stream.read(&mut response) {
            Ok(n) if n > 0 => n,
            _ => return false,
        };
        
        // ステータスコードを抽出
        let response_str = String::from_utf8_lossy(&response[..n]);
        if let Some(status_line) = response_str.lines().next() {
            // "HTTP/1.1 200 OK" のようなパターン
            let parts: Vec<&str> = status_line.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(status_code) = parts[1].parse::<u16>() {
                    return healthy_statuses.contains(&status_code);
                }
            }
        }
        
        false
    }
}


// ====================
// Backend選択
// ====================

/// 条件マッチング関数
/// 
/// すべての条件をANDで結合して評価します。
pub(crate) fn matches_conditions(
    conditions: &RouteConditions,
    host: &[u8],
    path: &[u8],
    method: &[u8],
    headers: &HashMap<String, String>,
    query: &HashMap<String, String>,
    source_ip: &SocketAddr,
) -> bool {
    // host条件のチェック
    // 条件が指定されていない場合は、すべてのホストにマッチ（デフォルト）
    if let Some(ref host_pattern) = conditions.host {
        let host_str = match std::str::from_utf8(host) {
            Ok(s) => {
                // ポート番号を除去
                if let Some(colon_pos) = s.find(':') {
                    &s[..colon_pos]
                } else {
                    s
                }
            },
            Err(_) => return false,
        };
        
        if !matches_wildcard(host_pattern, host_str) {
            return false;
        }
    }
    // host条件がNoneの場合は、すべてのホストにマッチ（デフォルト動作）
    
    // path条件のチェック
    // 条件が指定されていない場合は、すべてのパスにマッチ（デフォルト）
    if let Some(ref path_pattern) = conditions.path {
        if !matches_path_pattern(path_pattern, path) {
            return false;
        }
    }
    // path条件がNoneの場合は、すべてのパスにマッチ（デフォルト動作）
    
    // header条件のチェック
    // 条件が指定されていない場合は、すべてのヘッダーにマッチ（デフォルト）
    if let Some(ref header_map) = conditions.header {
        for (key, value_pattern) in header_map {
            let key_lower = key.to_lowercase();
            let header_value = headers.get(&key_lower).map(|s| s.as_str()).unwrap_or("");
            if !matches_wildcard(value_pattern, header_value) {
                return false;
            }
        }
    }
    // header条件がNoneの場合は、すべてのヘッダーにマッチ（デフォルト動作）
    
    // method条件のチェック
    // 条件が指定されていない場合は、すべてのメソッドにマッチ（デフォルト）
    if let Some(ref methods) = conditions.method {
        let method_str = std::str::from_utf8(method).unwrap_or("");
        if !methods.iter().any(|m| m.eq_ignore_ascii_case(method_str)) {
            return false;
        }
    }
    // method条件がNoneの場合は、すべてのメソッドにマッチ（デフォルト動作）
    
    // query条件のチェック
    // 条件が指定されていない場合は、すべてのクエリパラメータにマッチ（デフォルト）
    if let Some(ref query_map) = conditions.query {
        for (key, value_pattern) in query_map {
            let query_value = query.get(key).map(|s| s.as_str()).unwrap_or("");
            if !matches_wildcard(value_pattern, query_value) {
                return false;
            }
        }
    }
    // query条件がNoneの場合は、すべてのクエリパラメータにマッチ（デフォルト動作）
    
    // source_ip条件のチェック
    // 条件が指定されていない場合は、すべてのIPアドレスにマッチ（デフォルト）
    if let Some(ref ip_ranges) = conditions.source_ip {
        if !matches_cidr(source_ip, ip_ranges) {
            return false;
        }
    }
    // source_ip条件がNoneの場合は、すべてのIPアドレスにマッチ（デフォルト動作）
    
    // すべての条件がマッチした、または条件が指定されていない場合はtrue
    true
}

/// 統合ルーティング評価関数（最適化版）
/// 
/// Phase 1-4最適化を適用:
/// - Phase 1: Host-based グループ化 (O(1) HashMap lookup)
/// - Phase 2: Path Radix Tree (matchit)
/// - Phase 3: CIDR Tree 最適化
/// - Phase 4: LRU キャッシュ
/// 
/// 候補ルートのみを評価することで、線形O(n)から大幅に削減
pub fn find_backend_unified(
    host: &[u8],
    path: &[u8],
    method: &[u8],
    headers: &HashMap<String, String>,
    query: &HashMap<String, String>,
    source_ip: &SocketAddr,
    routes: &[Route],
    upstream_groups: &Arc<HashMap<String, Arc<UpstreamGroup>>>,
) -> Option<(Box<[u8]>, Backend)> {
    // CURRENT_CONFIG から OptimizedRouter を取得
    let config = CURRENT_CONFIG.load();
    let optimized_router = &config.optimized_router;
    let host_str = std::str::from_utf8(host).unwrap_or("");
    let path_str = std::str::from_utf8(path).unwrap_or("");
    debug!("[Routing] find_backend_unified: host='{}', path='{}', method='{}'", 
           host_str, path_str, std::str::from_utf8(method).unwrap_or(""));

    // Phase 4: キャッシュチェック
    let cache_key = routing::RouteCacheKey::new(host, path, method, source_ip);
    if let Some(cached_result) = optimized_router.try_cache(&cache_key) {
        match cached_result {
            Some(route_idx) => {
                // キャッシュヒット: ルートが見つかっている
                if let Some(route) = routes.get(route_idx) {
                    // 条件が変わっていないか確認（header/query/methodは動的）
                    if matches_conditions(
                        &route.conditions,
                        host,
                        path,
                        method,
                        headers,
                        query,
                        source_ip,
                    ) {
                        if let Ok(backend) = load_backend(route, upstream_groups) {
                            let prefix = extract_path_prefix(route);
                            return Some((prefix, backend));
                        }
                    }
                }
            }
            None => {
                // キャッシュが「マッチなし」を示しているが、header/query条件は動的なため
                // フォールスルーして全ルートを再評価する
            }
        }
    }
    
    // キャッシュミス: OptimizedRouter を使用して候補を取得
    let host_str = String::from_utf8_lossy(host);
    let path_str = String::from_utf8_lossy(path);
    
    // Phase 1-3: 候補ルートを取得
    let candidates = optimized_router.get_candidates(&host_str, &path_str, source_ip);
    
    if candidates.is_empty() {
        // 候補がない場合はフォールバック（全ルート走査）
        // これはOptimizedRouterの構築が不完全な場合のセーフティネット
        return find_backend_linear(
            host, path, method, headers, query, source_ip,
            routes, upstream_groups, &cache_key, optimized_router
        );
    }
    
    // 候補ルートのみを評価（first-match）
    // 候補は既にソート済み（インデックス順）
    info!("[Routing] Candidates for host='{}' path='{}': {:?}", host_str, path_str, candidates);
    for &route_idx in &candidates {
        if let Some(route) = routes.get(route_idx) {
            // 残りの条件（header, method, query）を評価
            let matched = matches_remaining_conditions(
                &route.conditions,
                method,
                headers,
                query,
            );
            
            if matched {
                info!(
                    "[Routing] Matched route index: {} (path={:?} action={:?})",
                    route_idx,
                    route.conditions.path,
                    route.action
                );
                match load_backend(route, upstream_groups) {
                    Ok(backend) => {
                        let prefix = extract_path_prefix(route);
                        // キャッシュに保存
                        optimized_router.cache_result(cache_key, Some(route_idx));
                        return Some((prefix, backend));
                    }
                    Err(e) => {
                        warn!(
                            "Route[{}] load_backend failed: {} (action={:?})",
                            route_idx, e, route.action
                        );
                        continue;
                    }
                }
            }
        }
    }
    
    // 候補内でマッチしなかった場合
    info!(
        "No route matched in {} candidates: host='{}' path='{}' method='{}'",
        candidates.len(),
        host_str,
        path_str,
        String::from_utf8_lossy(method),
    );
    
    // キャッシュにマッチなしを保存
    debug!("[Routing] No match found for host='{}' path='{}'", host_str, path_str);
    optimized_router.cache_result(cache_key, None);
    None
}

/// パスプレフィックスを抽出
#[inline]
pub(crate) fn extract_path_prefix(route: &Route) -> Box<[u8]> {
    if let Some(ref path_pattern) = route.conditions.path {
        if let Some(prefix_str) = path_pattern.strip_suffix("/*") {
            prefix_str.as_bytes().into()
        } else {
            path_pattern.as_bytes().into()
        }
    } else {
        Box::new([])
    }
}

/// 残りの条件（host/path/source_ip以外）のみをチェック
/// 
/// OptimizedRouterで既にhost/path/source_ipはフィルタ済み
#[inline]
pub(crate) fn matches_remaining_conditions(
    conditions: &RouteConditions,
    method: &[u8],
    headers: &HashMap<String, String>,
    query: &HashMap<String, String>,
) -> bool {
    // header条件のチェック
    if let Some(ref header_map) = conditions.header {
        for (key, value_pattern) in header_map {
            let key_lower = key.to_lowercase();
            let header_value = headers.get(&key_lower).map(|s| s.as_str()).unwrap_or("");
            if !matches_wildcard(value_pattern, header_value) {
                return false;
            }
        }
    }
    
    // method条件のチェック
    if let Some(ref methods) = conditions.method {
        let method_str = std::str::from_utf8(method).unwrap_or("");
        if !methods.iter().any(|m| m.eq_ignore_ascii_case(method_str)) {
            return false;
        }
    }
    
    // query条件のチェック
    if let Some(ref query_map) = conditions.query {
        for (key, value_pattern) in query_map {
            let query_value = query.get(key).map(|s| s.as_str()).unwrap_or("");
            if !matches_wildcard(value_pattern, query_value) {
                return false;
            }
        }
    }
    
    true
}

/// フォールバック用線形探索（セーフティネット）
pub(crate) fn find_backend_linear(
    host: &[u8],
    path: &[u8],
    method: &[u8],
    headers: &HashMap<String, String>,
    query: &HashMap<String, String>,
    source_ip: &SocketAddr,
    routes: &[Route],
    upstream_groups: &Arc<HashMap<String, Arc<UpstreamGroup>>>,
    cache_key: &routing::RouteCacheKey,
    optimized_router: &routing::OptimizedRouter,
) -> Option<(Box<[u8]>, Backend)> {
    // 配列の順序で評価（first-match）
    for (i, route) in routes.iter().enumerate() {
        let matched = matches_conditions(
            &route.conditions,
            host,
            path,
            method,
            headers,
            query,
            source_ip,
        );
        
        if matched {
            debug!(
                "Route[{}] matched (linear fallback): host={:?} path={:?} method={:?}",
                i,
                route.conditions.host,
                route.conditions.path,
                route.conditions.method
            );
            match load_backend(route, upstream_groups) {
                Ok(backend) => {
                    let prefix = extract_path_prefix(route);
                    // キャッシュに保存
                    optimized_router.cache_result(cache_key.clone(), Some(i));
                    return Some((prefix, backend));
                }
                Err(e) => {
                    warn!(
                        "Route[{}] load_backend failed: {} (action={:?})",
                        i, e, route.action
                    );
                    continue;
                }
            }
        }
    }
    
    debug!(
        "No route matched (linear fallback): host='{}' path='{}' method='{}' routes_count={}",
        String::from_utf8_lossy(host),
        String::from_utf8_lossy(path),
        String::from_utf8_lossy(method),
        routes.len()
    );
    
    // キャッシュにマッチなしを保存
    optimized_router.cache_result(cache_key.clone(), None);
    None
}



// Helper functions for condition matching

/// ワイルドカードパターンマッチング（シンプルな実装）
/// 
/// パターン例:
/// - "example.com" → 完全一致
/// - "*.example.com" → サブドメインにマッチ（例: "api.example.com", "www.example.com"）
/// - "api.*.com" → サポートしない（先頭または末尾のみ）
fn matches_wildcard(pattern: &str, text: &str) -> bool {
    if pattern == text {
        return true;
    }
    
    // 先頭ワイルドカード: "*.example.com"
    if let Some(rest) = pattern.strip_prefix("*.") {
        if text.ends_with(rest) {
            // サブドメインのチェック（少なくとも1つのドットが必要）
            let subdomain = &text[..text.len() - rest.len()];
            return !subdomain.is_empty() && !subdomain.contains('.');
        }
    }
    
    // 末尾ワイルドカード: "api.*"
    if let Some(rest) = pattern.strip_suffix(".*") {
        if text.starts_with(rest) {
            // ドメイン部分のチェック
            let domain = &text[rest.len()..];
            return !domain.is_empty() && domain.starts_with('.');
        }
    }
    
    false
}

/// パスパターンマッチング（ワイルドカード対応）
/// 
/// パターン例:
/// - "/api" → 完全一致
/// - "/api/*" → "/api/" で始まるすべてのパスにマッチ
/// - "/api/v2/*" → "/api/v2/" で始まるすべてのパスにマッチ
fn matches_path_pattern(pattern: &str, path: &[u8]) -> bool {
    let path_str = match std::str::from_utf8(path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    
    // 完全一致
    if pattern == path_str {
        return true;
    }
    
    // ワイルドカードパターン: "/api/*"
    if let Some(prefix) = pattern.strip_suffix("/*") {
        return path_str.starts_with(prefix) && 
               (path_str.len() == prefix.len() || path_str.as_bytes()[prefix.len()] == b'/');
    }
    
    // プレフィックス一致（末尾スラッシュなしでもマッチ）
    if path_str.starts_with(pattern) {
        // パターンが完全一致、または次の文字がスラッシュ
        let remaining = &path_str[pattern.len()..];
        return remaining.is_empty() || remaining.starts_with('/');
    }
    
    false
}

/// ソースIPがCIDR範囲に含まれるかチェック
fn matches_cidr(ip: &SocketAddr, cidr_ranges: &[String]) -> bool {
    use std::net::IpAddr;
    
    let ip_addr = ip.ip();
    
    for cidr in cidr_ranges {
        // シンプルなCIDRマッチング（IPv4のみ対応）
        if let Some((network_str, prefix_len_str)) = cidr.split_once('/') {
            if let (Ok(network), Ok(prefix_len)) = (
                network_str.parse::<IpAddr>(),
                prefix_len_str.parse::<u8>()
            ) {
                if let (IpAddr::V4(network_v4), IpAddr::V4(ip_v4)) = (network, ip_addr) {
                    let mask = !((1u32 << (32 - prefix_len)) - 1);
                    let network_u32 = u32::from_be_bytes(network_v4.octets());
                    let ip_u32 = u32::from_be_bytes(ip_v4.octets());
                    if (network_u32 & mask) == (ip_u32 & mask) {
                        return true;
                    }
                }
            }
        } else {
            // CIDR表記なし（完全一致）
            if let Ok(parsed_ip) = cidr.parse::<IpAddr>() {
                if parsed_ip == ip_addr {
                    return true;
                }
            }
        }
    }
    
    false
}
