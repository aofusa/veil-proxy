//! Distributed Cache Proxy-Wasm Filter
//!
//! Caches HTTP GET responses using external backends:
//! - Redis via Webdis HTTP API
//! - Memcached via HTTP API
//!
//! Features:
//! - Configurable backend selection
//! - Cache key generation from URL and headers
//! - Cache-Control header support
//! - Cache bypass header

use proxy_wasm::traits::*;
use proxy_wasm::types::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

mod memcached;
mod redis;

proxy_wasm::main! {{
    proxy_wasm::set_log_level(LogLevel::Info);
    proxy_wasm::set_root_context(|_| -> Box<dyn RootContext> {
        Box::new(CacheFilterRoot::new())
    });
}}

/// Cache backend type
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheBackend {
    Redis,
    Memcached,
}

impl Default for CacheBackend {
    fn default() -> Self {
        CacheBackend::Redis
    }
}

/// Cache filter configuration
#[derive(Debug, Clone, Deserialize)]
pub struct CacheConfig {
    /// Cache backend to use
    #[serde(default)]
    pub backend: CacheBackend,
    
    /// Redis configuration
    #[serde(default)]
    pub redis: RedisConfig,
    
    /// Memcached configuration
    #[serde(default)]
    pub memcached: MemcachedConfig,
    
    /// Default TTL in seconds
    #[serde(default = "default_ttl")]
    pub default_ttl_secs: u64,
    
    /// HTTP methods to cache
    #[serde(default = "default_cache_methods")]
    pub cache_methods: Vec<String>,
    
    /// Headers to include in cache key
    #[serde(default)]
    pub key_headers: Vec<String>,
    
    /// Header to bypass cache
    #[serde(default = "default_bypass_header")]
    pub bypass_header: String,
    
    /// Paths to skip caching
    #[serde(default)]
    pub skip_paths: Vec<String>,
    
    /// Upstream cluster name for cache backend
    #[serde(default = "default_upstream")]
    pub upstream: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RedisConfig {
    /// Webdis URL base (just the host, path is built dynamically)
    #[serde(default = "default_redis_host")]
    pub host: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemcachedConfig {
    /// Memcached HTTP API host
    #[serde(default = "default_memcached_host")]
    pub host: String,
}

fn default_ttl() -> u64 { 300 }
fn default_cache_methods() -> Vec<String> { vec!["GET".to_string(), "HEAD".to_string()] }
fn default_bypass_header() -> String { "X-Cache-Bypass".to_string() }
fn default_upstream() -> String { "cache".to_string() }
fn default_redis_host() -> String { "webdis".to_string() }
fn default_memcached_host() -> String { "memcached-http".to_string() }

impl Default for RedisConfig {
    fn default() -> Self {
        Self { host: "webdis".to_string() }
    }
}

impl Default for MemcachedConfig {
    fn default() -> Self {
        Self { host: "memcached-http".to_string() }
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            backend: CacheBackend::Redis,
            redis: RedisConfig::default(),
            memcached: MemcachedConfig::default(),
            default_ttl_secs: 300,
            cache_methods: vec!["GET".to_string(), "HEAD".to_string()],
            key_headers: vec!["Accept".to_string(), "Accept-Encoding".to_string()],
            bypass_header: "X-Cache-Bypass".to_string(),
            skip_paths: vec!["/health".to_string(), "/metrics".to_string()],
            upstream: "cache".to_string(),
        }
    }
}

impl CacheConfig {
    pub fn should_skip(&self, path: &str) -> bool {
        self.skip_paths.iter().any(|p| path.starts_with(p))
    }

    pub fn should_cache_method(&self, method: &str) -> bool {
        self.cache_methods.iter().any(|m| m.eq_ignore_ascii_case(method))
    }
}

/// Cache operation type
#[derive(Debug, Clone, Copy, PartialEq)]
enum CacheOp {
    Get,
    Set,
}

struct CacheFilterRoot {
    config: CacheConfig,
}

impl CacheFilterRoot {
    fn new() -> Self {
        Self {
            config: CacheConfig::default(),
        }
    }
}

impl Context for CacheFilterRoot {}

impl RootContext for CacheFilterRoot {
    fn on_configure(&mut self, plugin_configuration_size: usize) -> bool {
        if plugin_configuration_size == 0 {
            log::info!("[cache] Using default configuration (Redis backend)");
            return true;
        }

        if let Some(config_bytes) = self.get_plugin_configuration() {
            match serde_json::from_slice::<CacheConfig>(&config_bytes) {
                Ok(config) => {
                    log::info!(
                        "[cache] Configuration loaded: backend={:?}, ttl={}s",
                        config.backend,
                        config.default_ttl_secs
                    );
                    self.config = config;
                }
                Err(e) => {
                    log::error!("[cache] Failed to parse configuration: {}", e);
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
        Some(Box::new(CacheFilter {
            context_id,
            config: self.config.clone(),
            cache_key: None,
            pending_op: None,
            cached_response: None,
        }))
    }
}

struct CacheFilter {
    context_id: u32,
    config: CacheConfig,
    cache_key: Option<String>,
    pending_op: Option<CacheOp>,
    cached_response: Option<CachedResponse>,
}

#[derive(Debug, Clone)]
struct CachedResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Context for CacheFilter {
    fn on_http_call_response(&mut self, _token_id: u32, _num_headers: usize, body_size: usize, _num_trailers: usize) {
        let op = self.pending_op.take();
        
        match op {
            Some(CacheOp::Get) => {
                if let Some(body) = self.get_http_call_response_body(0, body_size) {
                    let cached_value = match self.config.backend {
                        CacheBackend::Redis => redis::parse_get_response(&body),
                        CacheBackend::Memcached => memcached::parse_get_response(&body),
                    };

                    if let Some(value) = cached_value {
                        log::info!("[cache:{}] Cache HIT for key {:?}", 
                            self.context_id, self.cache_key);
                        
                        // Parse cached response and serve it
                        if let Some(cached) = self.parse_cached_response(&value) {
                            self.add_http_response_header("X-Cache", "HIT");
                            self.send_http_response(
                                cached.status as u32,
                                cached.headers.iter()
                                    .map(|(k, v)| (k.as_str(), v.as_str()))
                                    .collect(),
                                Some(&cached.body),
                            );
                            return;
                        }
                    }

                    log::debug!("[cache:{}] Cache MISS for key {:?}", 
                        self.context_id, self.cache_key);
                }
                
                // Cache miss - continue to upstream
                self.resume_http_request();
            }
            Some(CacheOp::Set) => {
                log::debug!("[cache:{}] Cache SET completed for key {:?}", 
                    self.context_id, self.cache_key);
            }
            None => {}
        }
    }
}

impl HttpContext for CacheFilter {
    fn on_http_request_headers(&mut self, _num_headers: usize, _end_of_stream: bool) -> Action {
        // Get method and path
        let method = self.get_http_request_header(":method").unwrap_or_default();
        let path = self.get_http_request_header(":path").unwrap_or_default();

        // Check if cacheable
        if !self.config.should_cache_method(&method) {
            return Action::Continue;
        }

        if self.config.should_skip(&path) {
            return Action::Continue;
        }

        // Check bypass header
        if self.get_http_request_header(&self.config.bypass_header).is_some() {
            log::debug!("[cache:{}] Cache bypassed via header", self.context_id);
            return Action::Continue;
        }

        // Generate cache key
        let cache_key = self.generate_cache_key(&method, &path);
        self.cache_key = Some(cache_key.clone());

        // Try to get from cache
        if self.fetch_from_cache(&cache_key) {
            self.pending_op = Some(CacheOp::Get);
            return Action::Pause;
        }

        Action::Continue
    }

    fn on_http_response_headers(&mut self, _num_headers: usize, _end_of_stream: bool) -> Action {
        // Add cache status header
        self.add_http_response_header("X-Cache", "MISS");
        
        // Check if response is cacheable
        if let Some(cache_control) = self.get_http_response_header("cache-control") {
            if cache_control.contains("no-store") || cache_control.contains("private") {
                log::debug!("[cache:{}] Response not cacheable (Cache-Control)", self.context_id);
                self.cache_key = None;
                return Action::Continue;
            }
        }

        // Only cache successful responses
        if let Some(status) = self.get_http_response_header(":status") {
            if let Ok(code) = status.parse::<u16>() {
                if code < 200 || code >= 300 {
                    self.cache_key = None;
                }
            }
        }

        Action::Continue
    }

    fn on_http_response_body(&mut self, body_size: usize, end_of_stream: bool) -> Action {
        if !end_of_stream {
            return Action::Continue;
        }

        // Cache the response if we have a key
        if let Some(ref cache_key) = self.cache_key.clone() {
            if let Some(body) = self.get_http_response_body(0, body_size) {
                // Get status and headers
                let status = self.get_http_response_header(":status")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(200);
                
                let headers: Vec<(String, String)> = vec![
                    ("content-type".to_string(), 
                     self.get_http_response_header("content-type").unwrap_or_default()),
                ];

                let cached = CachedResponse { status, headers, body };
                
                // Store in cache
                let serialized = self.serialize_cached_response(&cached);
                self.store_in_cache(cache_key, &serialized, self.config.default_ttl_secs);
            }
        }

        Action::Continue
    }

    fn on_log(&mut self) {
        log::debug!("[cache:{}] Request completed", self.context_id);
    }
}

impl CacheFilter {
    /// Generate cache key from request
    fn generate_cache_key(&self, method: &str, path: &str) -> String {
        let mut key_parts = vec![method.to_string(), path.to_string()];
        
        // Add configured headers to key
        for header_name in &self.config.key_headers {
            if let Some(value) = self.get_http_request_header(header_name) {
                key_parts.push(format!("{}={}", header_name, value));
            }
        }
        
        // Simple hash of key parts
        let key_str = key_parts.join("|");
        format!("veil:cache:{:x}", simple_hash(&key_str))
    }

    /// Fetch from cache backend
    fn fetch_from_cache(&self, key: &str) -> bool {
        let path = match self.config.backend {
            CacheBackend::Redis => redis::build_get_path(key),
            CacheBackend::Memcached => memcached::build_get_path(key),
        };

        let headers = vec![
            (":method", "GET"),
            (":path", &path),
            (":authority", match self.config.backend {
                CacheBackend::Redis => &self.config.redis.host,
                CacheBackend::Memcached => &self.config.memcached.host,
            }),
        ];

        match self.dispatch_http_call(
            &self.config.upstream,
            headers,
            None,
            vec![],
            Duration::from_secs(1),
        ) {
            Ok(_) => true,
            Err(e) => {
                log::error!("[cache:{}] Failed to fetch from cache: {:?}", self.context_id, e);
                false
            }
        }
    }

    /// Store in cache backend
    fn store_in_cache(&self, key: &str, value: &str, ttl_secs: u64) {
        let (path, body) = match self.config.backend {
            CacheBackend::Redis => (
                redis::build_set_path(key, value, Some(ttl_secs)),
                None,
            ),
            CacheBackend::Memcached => (
                "/set".to_string(),
                Some(memcached::build_set_body(key, value, ttl_secs)),
            ),
        };

        let method = match self.config.backend {
            CacheBackend::Redis => "GET",
            CacheBackend::Memcached => "POST",
        };

        let headers = vec![
            (":method", method),
            (":path", &path),
            (":authority", match self.config.backend {
                CacheBackend::Redis => &self.config.redis.host,
                CacheBackend::Memcached => &self.config.memcached.host,
            }),
            ("content-type", "application/json"),
        ];

        if let Err(e) = self.dispatch_http_call(
            &self.config.upstream,
            headers,
            body.as_deref(),
            vec![],
            Duration::from_secs(1),
        ) {
            log::error!("[cache:{}] Failed to store in cache: {:?}", self.context_id, e);
        }
        // Note: Store is fire-and-forget, no need to track pending_op
    }

    /// Serialize cached response
    fn serialize_cached_response(&self, cached: &CachedResponse) -> String {
        // Simple JSON format
        let headers_json: Vec<String> = cached.headers.iter()
            .map(|(k, v)| format!(r#"["{}","{}"]"#, k, v))
            .collect();
        
        let body_b64 = base64_encode(&cached.body);
        
        format!(
            r#"{{"status":{},"headers":[{}],"body":"{}"}}"#,
            cached.status,
            headers_json.join(","),
            body_b64
        )
    }

    /// Parse cached response from serialized format
    fn parse_cached_response(&self, value: &str) -> Option<CachedResponse> {
        #[derive(Deserialize)]
        struct CachedJson {
            status: u16,
            headers: Vec<(String, String)>,
            body: String,
        }
        
        let parsed: CachedJson = serde_json::from_str(value).ok()?;
        let body = base64_decode(&parsed.body)?;
        
        Some(CachedResponse {
            status: parsed.status,
            headers: parsed.headers,
            body,
        })
    }
}

/// Simple hash function for cache keys
fn simple_hash(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325; // FNV offset basis
    for byte in s.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3); // FNV prime
    }
    hash
}

/// Simple base64 encoding
fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).map(|&b| b as u32).unwrap_or(0);
        let b2 = chunk.get(2).map(|&b| b as u32).unwrap_or(0);
        
        let n = (b0 << 16) | (b1 << 8) | b2;
        
        result.push(CHARS[((n >> 18) & 63) as usize] as char);
        result.push(CHARS[((n >> 12) & 63) as usize] as char);
        
        if chunk.len() > 1 {
            result.push(CHARS[((n >> 6) & 63) as usize] as char);
        } else {
            result.push('=');
        }
        
        if chunk.len() > 2 {
            result.push(CHARS[(n & 63) as usize] as char);
        } else {
            result.push('=');
        }
    }
    
    result
}

/// Simple base64 decoding
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const DECODE: [i8; 128] = [
        -1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,
        -1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,
        -1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,62,-1,-1,-1,63,
        52,53,54,55,56,57,58,59,60,61,-1,-1,-1,-1,-1,-1,
        -1, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9,10,11,12,13,14,
        15,16,17,18,19,20,21,22,23,24,25,-1,-1,-1,-1,-1,
        -1,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,
        41,42,43,44,45,46,47,48,49,50,51,-1,-1,-1,-1,-1,
    ];
    
    let bytes: Vec<u8> = s.bytes()
        .filter(|&b| b != b'=' && (b as usize) < 128 && DECODE[b as usize] >= 0)
        .collect();
    
    let mut result = Vec::new();
    
    for chunk in bytes.chunks(4) {
        if chunk.len() < 2 {
            break;
        }
        
        let b0 = DECODE[chunk[0] as usize] as u32;
        let b1 = DECODE[chunk[1] as usize] as u32;
        let b2 = chunk.get(2).map(|&b| DECODE[b as usize] as u32).unwrap_or(0);
        let b3 = chunk.get(3).map(|&b| DECODE[b as usize] as u32).unwrap_or(0);
        
        let n = (b0 << 18) | (b1 << 12) | (b2 << 6) | b3;
        
        result.push((n >> 16) as u8);
        if chunk.len() > 2 {
            result.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            result.push(n as u8);
        }
    }
    
    Some(result)
}
