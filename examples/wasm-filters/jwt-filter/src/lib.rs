//! JWT Authentication Proxy-Wasm Filter
//!
//! Validates JWT tokens with support for:
//! - HS256 (HMAC-SHA256) with static secrets
//! - RS256 (RSA-SHA256) with JWKS URL fetching
//! - Claims validation (exp, iss, aud)
//! - Claims-to-headers mapping

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use proxy_wasm::traits::*;
use proxy_wasm::types::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod crypto;
pub mod jwks;

use crypto::VerifyResult;
use jwks::{Jwks, JwksCache};

proxy_wasm::main! {{
    proxy_wasm::set_log_level(LogLevel::Info);
    proxy_wasm::set_root_context(|_| -> Box<dyn RootContext> {
        Box::new(JwtFilterRoot::new())
    });
}}

/// JWT Filter Configuration
#[derive(Debug, Clone, Deserialize)]
pub struct JwtConfig {
    /// Expected issuer (iss claim)
    #[serde(default)]
    pub issuer: Option<String>,
    
    /// Expected audience (aud claim)
    #[serde(default)]
    pub audience: Option<String>,
    
    /// JWKS URL for RS256 keys
    #[serde(default)]
    pub jwks_url: Option<String>,
    
    /// JWKS cache TTL in seconds
    #[serde(default = "default_jwks_ttl")]
    pub jwks_cache_ttl_secs: u64,
    
    /// Allowed algorithms
    #[serde(default = "default_algorithms")]
    pub algorithms: Vec<String>,
    
    /// Static secrets for HS256 (kid -> base64 secret)
    #[serde(default)]
    pub static_keys: HashMap<String, String>,
    
    /// Header name for token extraction
    #[serde(default = "default_header_name")]
    pub header_name: String,
    
    /// Claims to forward as headers (claim -> header name)
    #[serde(default)]
    pub claims_to_headers: HashMap<String, String>,
    
    /// Skip authentication for these paths
    #[serde(default)]
    pub skip_paths: Vec<String>,
    
    /// Upstream name for JWKS fetching
    #[serde(default = "default_jwks_upstream")]
    pub jwks_upstream: String,
    
    /// Error message level (detailed or simplified)
    #[serde(default = "default_error_level")]
    pub error_level: String,
    
    /// Clock skew tolerance in seconds (for iat validation)
    #[serde(default = "default_clock_skew")]
    pub clock_skew_tolerance_secs: u64,
    
    /// Validate nbf claim
    #[serde(default = "default_true")]
    pub validate_nbf: bool,
    
    /// Validate iat claim
    #[serde(default = "default_false")]
    pub validate_iat: bool,
}

fn default_jwks_ttl() -> u64 { 3600 }
fn default_algorithms() -> Vec<String> { vec!["RS256".to_string(), "HS256".to_string()] }
fn default_header_name() -> String { "Authorization".to_string() }
fn default_jwks_upstream() -> String { "jwks".to_string() }
fn default_error_level() -> String { "simplified".to_string() }
fn default_clock_skew() -> u64 { 300 }  // 5分
fn default_true() -> bool { true }
fn default_false() -> bool { false }

impl Default for JwtConfig {
    fn default() -> Self {
        Self {
            issuer: None,
            audience: None,
            jwks_url: None,
            jwks_cache_ttl_secs: 3600,
            algorithms: vec!["RS256".to_string(), "HS256".to_string()],
            static_keys: HashMap::new(),
            header_name: "Authorization".to_string(),
            claims_to_headers: HashMap::new(),
            skip_paths: vec!["/health".to_string(), "/metrics".to_string()],
            jwks_upstream: "jwks".to_string(),
            error_level: "simplified".to_string(),
            clock_skew_tolerance_secs: 300,
            validate_nbf: true,
            validate_iat: false,
        }
    }
}

impl JwtConfig {
    /// Whether the request path skips authentication. The query string is not part of the match, and a skip path
    /// matches only itself and the paths below it (`/public` matches `/public` and `/public/a`, not `/publications`).
    /// A path with characters outside `A-Z a-z 0-9 / - . _ ~` (percent-encoding, `;` parameters, backslashes) or
    /// with `.` / `..` segments is never skipped, because an upstream may decode or normalize it into a path outside
    /// the skip path.
    // B-438: 以前は文字列の前方一致で、`/public` が `/publications` 等も認証なしで通していた（waf-filter の
    // `is_path_whitelisted` と同じ判定にする）
    pub fn should_skip(&self, path: &str) -> bool {
        let path = path.split(['?', '#']).next().unwrap_or("");
        let plain = path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'.' | b'_' | b'~'));
        if !plain || path.split('/').any(|seg| seg == "." || seg == "..") {
            return false;
        }
        self.skip_paths.iter().any(|p| {
            !p.is_empty()
                && (path == p
                    || path
                        .strip_prefix(p.as_str())
                        .is_some_and(|rest| p.ends_with('/') || rest.starts_with('/')))
        })
    }
}

/// JWT Header
#[derive(Debug, Deserialize)]
struct JwtHeader {
    alg: String,
    #[serde(default)]
    kid: Option<String>,
}

/// JWT Claims
#[derive(Debug, Deserialize)]
struct JwtClaims {
    #[serde(default)]
    iss: Option<String>,
    #[serde(default)]
    aud: Option<serde_json::Value>,
    #[serde(default)]
    exp: Option<u64>,
    #[serde(default)]
    nbf: Option<u64>,  // Not Before
    #[serde(default)]
    iat: Option<u64>,  // Issued At
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

struct JwtFilterRoot {
    config: JwtConfig,
}

impl JwtFilterRoot {
    fn new() -> Self {
        Self {
            config: JwtConfig::default(),
        }
    }
}

impl Context for JwtFilterRoot {}

impl RootContext for JwtFilterRoot {
    fn on_configure(&mut self, plugin_configuration_size: usize) -> bool {
        if plugin_configuration_size == 0 {
            log::info!("[jwt] Using default configuration");
            return true;
        }

        if let Some(config_bytes) = self.get_plugin_configuration() {
            match serde_json::from_slice::<JwtConfig>(&config_bytes) {
                Ok(config) => {
                    log::info!(
                        "[jwt] Configuration loaded: issuer={:?}, jwks_url={:?}, algorithms={:?}",
                        config.issuer,
                        config.jwks_url,
                        config.algorithms
                    );
                    self.config = config;
                }
                Err(e) => {
                    log::error!("[jwt] Failed to parse configuration: {}", e);
                    return false;
                }
            }
        }
        true
    }

    fn get_type(&self) -> Option<ContextType> {
        Some(ContextType::HttpContext)
    }

    fn create_http_context(&self, context_id: u32) -> Option<Box<dyn HttpContext>> {
        Some(Box::new(JwtFilter {
            context_id,
            config: self.config.clone(),
            pending_jwks_fetch: false,
            cached_token: None,
        }))
    }
}

struct JwtFilter {
    context_id: u32,
    config: JwtConfig,
    pending_jwks_fetch: bool,
    cached_token: Option<String>,
}

impl Context for JwtFilter {
    fn on_http_call_response(&mut self, _token_id: u32, _num_headers: usize, body_size: usize, _num_trailers: usize) {
        self.pending_jwks_fetch = false;
        
        if let Some(body) = self.get_http_call_response_body(0, body_size) {
            if let Some(jwks) = Jwks::from_bytes(&body) {
                log::info!("[jwt:{}] JWKS fetched with {} keys", self.context_id, jwks.keys.len());
                
                // キャッシュに保存
                if let Some(ref jwks_url) = self.config.jwks_url {
                    JwksCache::set_global(
                        jwks_url.clone(),
                        jwks.clone(),
                        self.config.jwks_cache_ttl_secs
                    );
                }
                
                // Re-verify token with new JWKS
                if let Some(ref token) = self.cached_token.take() {
                    match self.verify_token_with_jwks(token, &jwks) {
                        Ok(()) => {
                            // 検証成功 - クレームをヘッダに追加
                            let parts: Vec<&str> = token.split('.').collect();
                            if parts.len() == 3 {
                                if let Some(claims) = self.decode_claims(parts[1]) {
                                    self.add_claims_headers(&claims);
                                }
                            }
                            self.resume_http_request();
                            return;
                        }
                        Err(e) => {
                            log::warn!("[jwt:{}] Token verification failed: {}", self.context_id, e);
                        }
                    }
                }
            } else {
                log::error!("[jwt:{}] Failed to parse JWKS response", self.context_id);
            }
        }
        
        // Failed to verify
        self.send_unauthorized("Invalid token");
    }
}

impl HttpContext for JwtFilter {
    fn on_http_request_headers(&mut self, _num_headers: usize, _end_of_stream: bool) -> Action {
        // B-439: クレームを写すヘッダは、クライアントが送ったものを先に消す（認証を飛ばすパスでも）。以前は検証した値を
        // 足すだけで、クライアントの `X-User-ID: alice` 等が先頭の値（クレームが無ければ唯一の値）として上流へ届いた
        if !self.strip_claim_headers() {
            self.send_http_response(
                500,
                vec![("content-type", "text/plain")],
                Some(b"Internal Server Error"),
            );
            return Action::Pause;
        }

        // Check skip paths
        if let Some(path) = self.get_http_request_header(":path") {
            if self.config.should_skip(&path) {
                log::debug!("[jwt:{}] Path skipped: {}", self.context_id, path);
                return Action::Continue;
            }
        }

        // Extract token
        let token = match self.extract_token() {
            Some(t) => t,
            None => {
                self.send_unauthorized("Missing authentication token");
                return Action::Pause;
            }
        };

        // Parse JWT
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 {
            self.send_unauthorized("Invalid token format");
            return Action::Pause;
        }

        // Decode header
        let header = match self.decode_header(parts[0]) {
            Some(h) => h,
            None => {
                self.send_unauthorized("Invalid token header");
                return Action::Pause;
            }
        };

        // Check algorithm
        if !self.config.algorithms.contains(&header.alg) {
            self.send_unauthorized("Unsupported algorithm");
            return Action::Pause;
        }

        // Decode claims
        let claims = match self.decode_claims(parts[1]) {
            Some(c) => c,
            None => {
                self.send_unauthorized("Invalid token claims");
                return Action::Pause;
            }
        };

        // Validate claims
        if let Err(msg) = self.validate_claims(&claims) {
            self.send_unauthorized(&msg);
            return Action::Pause;
        }

        // Verify signature
        let header_payload = format!("{}.{}", parts[0], parts[1]);
        
        match header.alg.as_str() {
            "HS256" => {
                if let Some(result) = self.verify_hs256(&header, &header_payload, parts[2]) {
                    if !matches!(result, VerifyResult::Valid) {
                        self.send_unauthorized("Invalid signature");
                        return Action::Pause;
                    }
                } else {
                    self.send_unauthorized("Key not found");
                    return Action::Pause;
                }
            }
            "RS256" => {
                // Check if we need to fetch JWKS
                if let Some(ref jwks_url) = self.config.jwks_url {
                    // キャッシュをチェック
                    if let Some(jwks) = JwksCache::get_global(jwks_url) {
                        log::debug!("[jwt:{}] JWKS found in cache", self.context_id);
                        
                        // キャッシュから取得したJWKSで検証
                        match self.verify_token_with_jwks(&token, &jwks) {
                            Ok(()) => {
                                // 検証成功 - クレームをヘッダに追加
                                if let Some(claims) = self.decode_claims(parts[1]) {
                                    self.add_claims_headers(&claims);
                                }
                                return Action::Continue;
                            }
                            Err(_) => {
                                // キャッシュのJWKSで検証失敗 - 新しいJWKSを取得
                                log::debug!("[jwt:{}] Token verification failed with cached JWKS, fetching new", self.context_id);
                            }
                        }
                    }
                    
                    self.cached_token = Some(token.clone());
                    
                    // Try to fetch JWKS
                    if self.fetch_jwks() {
                        self.pending_jwks_fetch = true;
                        return Action::Pause;
                    } else {
                        self.send_unauthorized("Failed to fetch JWKS");
                        return Action::Pause;
                    }
                } else {
                    self.send_unauthorized("RS256 requires JWKS URL");
                    return Action::Pause;
                }
            }
            _ => {
                self.send_unauthorized("Unsupported algorithm");
                return Action::Pause;
            }
        }

        // Add claims as headers
        self.add_claims_headers(&claims);

        Action::Continue
    }

    fn on_log(&mut self) {
        log::debug!("[jwt:{}] Request completed", self.context_id);
    }
}

impl JwtFilter {
    fn extract_token(&self) -> Option<String> {
        let header_value = self.get_http_request_header(&self.config.header_name)?;
        
        // Support "Bearer <token>" format
        if header_value.starts_with("Bearer ") || header_value.starts_with("bearer ") {
            Some(header_value[7..].trim().to_string())
        } else {
            Some(header_value)
        }
    }

    fn decode_header(&self, header_b64: &str) -> Option<JwtHeader> {
        let bytes = URL_SAFE_NO_PAD.decode(header_b64).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn decode_claims(&self, claims_b64: &str) -> Option<JwtClaims> {
        let bytes = URL_SAFE_NO_PAD.decode(claims_b64).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn validate_claims(&self, claims: &JwtClaims) -> Result<(), String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        
        // Check expiration
        if let Some(exp) = claims.exp {
            if now > exp {
                return Err("Token expired".to_string());
            }
        }

        // Check not before (nbf)
        if self.config.validate_nbf {
            if let Some(nbf) = claims.nbf {
                if now < nbf {
                    return Err("Token not yet valid".to_string());
                }
            }
        }

        // Check issued at (iat) - 未来の時刻は無効
        if self.config.validate_iat {
            if let Some(iat) = claims.iat {
                // 時計のずれを考慮して、許容範囲を設ける
                if iat > now + self.config.clock_skew_tolerance_secs {
                    return Err("Token issued in the future".to_string());
                }
            }
        }

        // Check issuer
        if let Some(ref expected_iss) = self.config.issuer {
            match &claims.iss {
                Some(iss) if iss == expected_iss => {}
                _ => return Err("Invalid issuer".to_string()),
            }
        }

        // Check audience
        if let Some(ref expected_aud) = self.config.audience {
            let aud_matches = match &claims.aud {
                Some(serde_json::Value::String(s)) => s == expected_aud,
                Some(serde_json::Value::Array(arr)) => {
                    arr.iter().any(|v| v.as_str() == Some(expected_aud.as_str()))
                }
                _ => false,
            };
            
            if !aud_matches {
                return Err("Invalid audience".to_string());
            }
        }

        Ok(())
    }

    fn verify_hs256(&self, header: &JwtHeader, header_payload: &str, signature: &str) -> Option<VerifyResult> {
        // Find key by kid or use first available
        let secret_b64 = if let Some(ref kid) = header.kid {
            self.config.static_keys.get(kid)?
        } else {
            self.config.static_keys.values().next()?
        };

        let secret = URL_SAFE_NO_PAD.decode(secret_b64).ok()?;
        Some(crypto::verify_hs256(header_payload, signature, &secret))
    }

    fn verify_token_with_jwks(&self, token: &str, jwks: &Jwks) -> Result<(), String> {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 {
            return Err("Invalid format".to_string());
        }

        let header = self.decode_header(parts[0]).ok_or("Invalid header")?;
        let header_payload = format!("{}.{}", parts[0], parts[1]);

        // Find key
        let jwk = if let Some(ref kid) = header.kid {
            jwks.find_key(kid)
        } else {
            jwks.first_rsa_key()
        }.ok_or("Key not found")?;

        // Verify
        match crypto::verify_rs256(&header_payload, parts[2], jwk) {
            VerifyResult::Valid => Ok(()),
            _ => Err("Invalid signature".to_string()),
        }
    }

    fn fetch_jwks(&self) -> bool {
        let jwks_url = match &self.config.jwks_url {
            Some(url) => url,
            None => return false,
        };

        // Parse URL to get host and path
        let url_without_scheme = if jwks_url.starts_with("https://") {
            &jwks_url[8..]
        } else if jwks_url.starts_with("http://") {
            &jwks_url[7..]
        } else {
            jwks_url.as_str()
        };

        let (host, path) = match url_without_scheme.find('/') {
            Some(pos) => (&url_without_scheme[..pos], &url_without_scheme[pos..]),
            None => (url_without_scheme, "/"),
        };

        let headers = vec![
            (":method", "GET"),
            (":path", path),
            (":authority", host),
            ("accept", "application/json"),
        ];

        match self.dispatch_http_call(
            &self.config.jwks_upstream,
            headers,
            None,
            vec![],
            Duration::from_secs(5),
        ) {
            Ok(_) => {
                log::debug!("[jwt:{}] JWKS fetch dispatched to {}", self.context_id, jwks_url);
                true
            }
            Err(e) => {
                log::error!("[jwt:{}] Failed to dispatch JWKS fetch: {:?}", self.context_id, e);
                false
            }
        }
    }

    /// Remove every header named in `claims_to_headers` from the request. False when the host refuses (the
    /// request must not go on with headers the client may have set).
    fn strip_claim_headers(&self) -> bool {
        for header in self.config.claims_to_headers.values() {
            if let Err(e) =
                proxy_wasm::hostcalls::set_map_value(MapType::HttpRequestHeaders, header, None)
            {
                log::error!(
                    "[jwt:{}] Failed to remove the request header {}: {:?}",
                    self.context_id,
                    header,
                    e
                );
                return false;
            }
        }
        true
    }

    fn add_claims_headers(&self, claims: &JwtClaims) {
        for (claim, header) in &self.config.claims_to_headers {
            let value = match claim.as_str() {
                "sub" => claims.sub.as_ref().map(|s| s.to_string()),
                "email" => claims.email.as_ref().map(|s| s.to_string()),
                "iss" => claims.iss.as_ref().map(|s| s.to_string()),
                _ => claims.extra.get(claim).and_then(|v| {
                    match v {
                        serde_json::Value::String(s) => Some(s.clone()),
                        _ => Some(v.to_string()),
                    }
                }),
            };

            if let Some(v) = value {
                // 同じ名前のヘッダを置き換える（クライアントの値は `strip_claim_headers` で消してある）
                if let Err(e) = proxy_wasm::hostcalls::set_map_value(
                    MapType::HttpRequestHeaders,
                    header,
                    Some(&v),
                ) {
                    log::error!(
                        "[jwt:{}] Failed to set the request header {}: {:?}",
                        self.context_id,
                        header,
                        e
                    );
                }
            }
        }
    }

    fn send_unauthorized(&self, message: &str) {
        // ログには詳細なメッセージを記録
        log::warn!("[jwt:{}] Unauthorized: {}", self.context_id, message);
        
        // レスポンスには設定に応じたメッセージを返す
        let user_message = if self.config.error_level == "detailed" {
            message.to_string()
        } else {
            self.simplify_error_message(message)
        };
        
        self.send_http_response(
            401,
            vec![
                ("content-type", "application/json"),
                ("www-authenticate", "Bearer"),
            ],
            Some(format!(r#"{{"error":"unauthorized","message":"{}"}}"#, user_message).as_bytes()),
        );
    }
    
    fn simplify_error_message(&self, message: &str) -> String {
        // エラーメッセージを簡略化
        if message.contains("expired") {
            "Token expired".to_string()
        } else if message.contains("Invalid signature") {
            "Invalid token".to_string()
        } else if message.contains("Invalid issuer") {
            "Invalid token".to_string()
        } else if message.contains("Invalid audience") {
            "Invalid token".to_string()
        } else if message.contains("Missing") {
            "Authentication required".to_string()
        } else if message.contains("not yet valid") {
            "Token not yet valid".to_string()
        } else if message.contains("future") {
            "Invalid token".to_string()
        } else {
            "Invalid token".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    
    fn create_test_config() -> JwtConfig {
        JwtConfig {
            issuer: Some("https://example.com".to_string()),
            audience: Some("api.example.com".to_string()),
            jwks_url: None,
            jwks_cache_ttl_secs: 3600,
            algorithms: vec!["HS256".to_string(), "RS256".to_string()],
            static_keys: HashMap::new(),
            header_name: "Authorization".to_string(),
            claims_to_headers: HashMap::new(),
            skip_paths: vec!["/health".to_string()],
            jwks_upstream: "jwks".to_string(),
            error_level: "simplified".to_string(),
            clock_skew_tolerance_secs: 300,
            validate_nbf: true,
            validate_iat: false,
        }
    }
    
    fn create_test_filter() -> JwtFilter {
        JwtFilter {
            context_id: 1,
            config: create_test_config(),
            pending_jwks_fetch: false,
            cached_token: None,
        }
    }
    
    #[test]
    fn test_decode_header_valid() {
        let filter = create_test_filter();
        // Base64url encoded: {"alg":"HS256","typ":"JWT"}
        let header_b64 = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";
        let header = filter.decode_header(header_b64);
        assert!(header.is_some());
        let header = header.unwrap();
        assert_eq!(header.alg, "HS256");
    }
    
    #[test]
    fn test_decode_header_invalid() {
        let filter = create_test_filter();
        let header_b64 = "invalid_base64!!!";
        let header = filter.decode_header(header_b64);
        assert!(header.is_none());
    }
    
    #[test]
    fn test_decode_claims_valid() {
        let filter = create_test_filter();
        // Base64url encoded: {"sub":"1234567890","name":"John Doe","iat":1516239022}
        let claims_b64 = "eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ";
        let claims = filter.decode_claims(claims_b64);
        assert!(claims.is_some());
        let claims = claims.unwrap();
        assert_eq!(claims.sub, Some("1234567890".to_string()));
    }
    
    #[test]
    fn test_decode_claims_invalid() {
        let filter = create_test_filter();
        let claims_b64 = "invalid_base64!!!";
        let claims = filter.decode_claims(claims_b64);
        assert!(claims.is_none());
    }
    
    #[test]
    fn test_validate_claims_exp_valid() {
        let mut config = create_test_config();
        config.issuer = None;  // Disable issuer check
        config.audience = None;  // Disable audience check
        let filter = JwtFilter {
            context_id: 1,
            config,
            pending_jwks_fetch: false,
            cached_token: None,
        };
        
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        
        let claims = JwtClaims {
            exp: Some(now + 3600),  // 1 hour in future
            ..Default::default()
        };
        
        assert!(filter.validate_claims(&claims).is_ok());
    }
    
    #[test]
    fn test_validate_claims_exp_expired() {
        let filter = create_test_filter();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        
        let claims = JwtClaims {
            exp: Some(now - 3600),  // 1 hour in past
            ..Default::default()
        };
        
        let result = filter.validate_claims(&claims);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expired"));
    }
    
    #[test]
    fn test_validate_claims_nbf_valid() {
        let mut config = create_test_config();
        config.validate_nbf = true;
        config.issuer = None;  // Disable issuer check
        config.audience = None;  // Disable audience check
        let filter = JwtFilter {
            context_id: 1,
            config,
            pending_jwks_fetch: false,
            cached_token: None,
        };
        
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        
        let claims = JwtClaims {
            nbf: Some(now - 3600),  // 1 hour in past
            ..Default::default()
        };
        
        assert!(filter.validate_claims(&claims).is_ok());
    }
    
    #[test]
    fn test_validate_claims_nbf_not_yet_valid() {
        let mut config = create_test_config();
        config.validate_nbf = true;
        let filter = JwtFilter {
            context_id: 1,
            config,
            pending_jwks_fetch: false,
            cached_token: None,
        };
        
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        
        let claims = JwtClaims {
            nbf: Some(now + 3600),  // 1 hour in future
            ..Default::default()
        };
        
        let result = filter.validate_claims(&claims);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not yet valid"));
    }
    
    #[test]
    fn test_validate_claims_iss_valid() {
        let mut config = create_test_config();
        config.audience = None;  // Disable audience check
        let filter = JwtFilter {
            context_id: 1,
            config,
            pending_jwks_fetch: false,
            cached_token: None,
        };
        
        let claims = JwtClaims {
            iss: Some("https://example.com".to_string()),
            ..Default::default()
        };
        
        assert!(filter.validate_claims(&claims).is_ok());
    }
    
    #[test]
    fn test_validate_claims_iss_invalid() {
        let filter = create_test_filter();
        let claims = JwtClaims {
            iss: Some("https://different.com".to_string()),
            ..Default::default()
        };
        
        let result = filter.validate_claims(&claims);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid issuer"));
    }
    
    #[test]
    fn test_validate_claims_aud_string_valid() {
        let mut config = create_test_config();
        config.issuer = None;  // Disable issuer check
        let filter = JwtFilter {
            context_id: 1,
            config,
            pending_jwks_fetch: false,
            cached_token: None,
        };
        
        let claims = JwtClaims {
            aud: Some(serde_json::Value::String("api.example.com".to_string())),
            ..Default::default()
        };
        
        assert!(filter.validate_claims(&claims).is_ok());
    }
    
    #[test]
    fn test_validate_claims_aud_array_valid() {
        let mut config = create_test_config();
        config.issuer = None;  // Disable issuer check
        let filter = JwtFilter {
            context_id: 1,
            config,
            pending_jwks_fetch: false,
            cached_token: None,
        };
        
        let claims = JwtClaims {
            aud: Some(serde_json::Value::Array(vec![
                serde_json::Value::String("other.example.com".to_string()),
                serde_json::Value::String("api.example.com".to_string()),
            ])),
            ..Default::default()
        };
        
        assert!(filter.validate_claims(&claims).is_ok());
    }
    
    #[test]
    fn test_validate_claims_aud_invalid() {
        let mut config = create_test_config();
        config.issuer = None;  // Disable issuer check
        let filter = JwtFilter {
            context_id: 1,
            config,
            pending_jwks_fetch: false,
            cached_token: None,
        };
        
        let claims = JwtClaims {
            aud: Some(serde_json::Value::String("different.example.com".to_string())),
            ..Default::default()
        };
        
        let result = filter.validate_claims(&claims);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid audience"));
    }
    
    #[test]
    fn test_simplify_error_message() {
        let filter = create_test_filter();
        
        assert_eq!(filter.simplify_error_message("Token expired"), "Token expired");
        assert_eq!(filter.simplify_error_message("Invalid signature"), "Invalid token");
        assert_eq!(filter.simplify_error_message("Invalid issuer"), "Invalid token");
        assert_eq!(filter.simplify_error_message("Missing authentication token"), "Authentication required");
        assert_eq!(filter.simplify_error_message("Token not yet valid"), "Token not yet valid");
        assert_eq!(filter.simplify_error_message("Unknown error"), "Invalid token");
    }
    
    #[test]
    fn test_jwt_config_should_skip() {
        let config = create_test_config();
        assert!(config.should_skip("/health"));
        assert!(config.should_skip("/health/check"));
        assert!(!config.should_skip("/api/users"));
    }

    /// B-438: 認証を飛ばすパスはセグメントの境界で比べ、符号化・`.`・`..` を含むパスは飛ばさない
    #[test]
    fn skip_paths_respect_segment_boundaries() {
        let config = JwtConfig {
            skip_paths: vec!["/health".to_string(), "/public".to_string(), "/static/".to_string()],
            ..Default::default()
        };
        assert!(config.should_skip("/public"));
        assert!(config.should_skip("/public/a.css?v=1"));
        assert!(config.should_skip("/static/app.js"));
        assert!(config.should_skip("/health?probe=1"));
        assert!(!config.should_skip("/publications/drafts"));
        assert!(!config.should_skip("/healthz-admin"));
        assert!(!config.should_skip("/health/../admin"));
        assert!(!config.should_skip("/public/./../admin"));
        assert!(!config.should_skip("/public/%2e%2e/admin"));
        assert!(!config.should_skip("/public;x/../admin"));
        assert!(!config.should_skip("/public\\..\\admin"));
        assert!(!config.should_skip("/static"));
        assert!(!config.should_skip("/admin?next=/public"));
    }
}

impl Default for JwtClaims {
    fn default() -> Self {
        Self {
            iss: None,
            aud: None,
            exp: None,
            nbf: None,
            iat: None,
            sub: None,
            email: None,
            extra: HashMap::new(),
        }
    }
}
