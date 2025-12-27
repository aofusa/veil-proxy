//! JWKS (JSON Web Key Set) handling
//!
//! Supports fetching and caching JWKS from external URLs

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use once_cell::sync::Lazy;

/// JWKS response from OIDC provider
#[derive(Debug, Clone, Deserialize)]
pub struct Jwks {
    pub keys: Vec<Jwk>,
}

/// JSON Web Key
#[derive(Debug, Clone, Deserialize)]
pub struct Jwk {
    /// Key type (RSA, EC, etc.)
    pub kty: String,
    /// Key ID
    #[serde(default)]
    pub kid: Option<String>,
    /// Algorithm
    #[serde(default)]
    pub alg: Option<String>,
    /// Key use: sig or enc
    #[serde(default)]
    pub use_type: Option<String>,
    
    // RSA parameters
    /// RSA modulus (base64url)
    #[serde(default)]
    pub n: Option<String>,
    /// RSA exponent (base64url)
    #[serde(default)]
    pub e: Option<String>,
}

impl Jwks {
    /// Parse JWKS from JSON bytes
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
    
    /// Find key by kid
    pub fn find_key(&self, kid: &str) -> Option<&Jwk> {
        self.keys.iter().find(|k| k.kid.as_deref() == Some(kid))
    }
    
    /// Get first RSA key (fallback when no kid specified)
    pub fn first_rsa_key(&self) -> Option<&Jwk> {
        self.keys.iter().find(|k| k.kty == "RSA")
    }
}

/// JWKS cache with TTL tracking
pub struct JwksCache {
    /// Cached JWKS keyed by URL
    entries: HashMap<String, CacheEntry>,
}

struct CacheEntry {
    jwks: Jwks,
    fetched_at: u64,  // nanoseconds since epoch
    ttl_secs: u64,
}

/// Get current time in nanoseconds since epoch
fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

impl JwksCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }
    
    /// Get cached JWKS if still valid (現在時刻を自動取得)
    pub fn get(&self, url: &str) -> Option<&Jwks> {
        let now_nanos = now_nanos();
        if let Some(entry) = self.entries.get(url) {
            let age_secs = (now_nanos - entry.fetched_at) / 1_000_000_000;
            if age_secs < entry.ttl_secs {
                return Some(&entry.jwks);
            }
        }
        None
    }
    
    /// Store JWKS in cache (現在時刻を自動取得)
    pub fn set(&mut self, url: String, jwks: Jwks, ttl_secs: u64) {
        let now_nanos = now_nanos();
        self.entries.insert(url, CacheEntry {
            jwks,
            fetched_at: now_nanos,
            ttl_secs,
        });
    }
    
    /// Check if URL needs refresh (現在時刻を自動取得)
    pub fn needs_refresh(&self, url: &str) -> bool {
        self.get(url).is_none()
    }
}

/// Global JWKS cache shared across all HTTP contexts
static GLOBAL_JWKS_CACHE: Lazy<Mutex<JwksCache>> = Lazy::new(|| {
    Mutex::new(JwksCache::new())
});

impl JwksCache {
    /// Get JWKS from global cache
    pub fn get_global(url: &str) -> Option<Jwks> {
        let cache = GLOBAL_JWKS_CACHE.lock().ok()?;
        cache.get(url).cloned()
    }
    
    /// Store JWKS in global cache
    pub fn set_global(url: String, jwks: Jwks, ttl_secs: u64) {
        if let Ok(mut cache) = GLOBAL_JWKS_CACHE.lock() {
            cache.set(url, jwks, ttl_secs);
        }
    }
    
    /// Check if global cache needs refresh
    pub fn needs_refresh_global(url: &str) -> bool {
        if let Ok(cache) = GLOBAL_JWKS_CACHE.lock() {
            cache.needs_refresh(url)
        } else {
            true  // ロック失敗時はリフレッシュが必要とみなす
        }
    }
}

impl Default for JwksCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    fn create_test_jwk(kid: Option<&str>) -> Jwk {
        Jwk {
            kty: "RSA".to_string(),
            kid: kid.map(|s| s.to_string()),
            alg: Some("RS256".to_string()),
            use_type: Some("sig".to_string()),
            n: Some("test_n".to_string()),
            e: Some("AQAB".to_string()),
        }
    }
    
    fn create_test_jwks() -> Jwks {
        Jwks {
            keys: vec![
                create_test_jwk(Some("key1")),
                create_test_jwk(Some("key2")),
                create_test_jwk(None),
            ],
        }
    }
    
    #[test]
    fn test_jwks_from_bytes_valid() {
        let json = r#"{"keys":[{"kty":"RSA","kid":"key1","alg":"RS256","n":"test_n","e":"AQAB"}]}"#;
        let jwks = Jwks::from_bytes(json.as_bytes());
        assert!(jwks.is_some());
        let jwks = jwks.unwrap();
        assert_eq!(jwks.keys.len(), 1);
        assert_eq!(jwks.keys[0].kid, Some("key1".to_string()));
    }
    
    #[test]
    fn test_jwks_from_bytes_invalid() {
        let json = "invalid json";
        let jwks = Jwks::from_bytes(json.as_bytes());
        assert!(jwks.is_none());
    }
    
    #[test]
    fn test_jwks_find_key_by_kid() {
        let jwks = create_test_jwks();
        let key = jwks.find_key("key1");
        assert!(key.is_some());
        assert_eq!(key.unwrap().kid, Some("key1".to_string()));
    }
    
    #[test]
    fn test_jwks_find_key_not_found() {
        let jwks = create_test_jwks();
        let key = jwks.find_key("nonexistent");
        assert!(key.is_none());
    }
    
    #[test]
    fn test_jwks_first_rsa_key() {
        let jwks = create_test_jwks();
        let key = jwks.first_rsa_key();
        assert!(key.is_some());
        assert_eq!(key.unwrap().kty, "RSA");
    }
    
    #[test]
    fn test_jwks_cache_get_set() {
        let mut cache = JwksCache::new();
        let jwks = create_test_jwks();
        let url = "https://example.com/jwks".to_string();
        
        // Set cache
        cache.set(url.clone(), jwks.clone(), 3600);
        
        // Get cache (should be valid immediately)
        let cached = cache.get(&url);
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().keys.len(), 3);
    }
    
    #[test]
    fn test_jwks_cache_ttl_expired() {
        let mut cache = JwksCache::new();
        let jwks = create_test_jwks();
        let url = "https://example.com/jwks".to_string();
        
        // Set cache with very short TTL
        cache.set(url.clone(), jwks, 0);  // TTL = 0 seconds
        
        // Wait a bit and check (should be expired)
        std::thread::sleep(std::time::Duration::from_millis(100));
        let cached = cache.get(&url);
        
        // TTL=0なので、即座に期限切れになるはず
        // ただし、時刻取得の精度により、わずかな遅延がある可能性がある
        // そのため、Noneであることを確認する（期限切れ）
        assert!(cached.is_none(), "Cache should be expired with TTL=0");
    }
    
    #[test]
    fn test_jwks_cache_needs_refresh() {
        let mut cache = JwksCache::new();
        let jwks = create_test_jwks();
        let url = "https://example.com/jwks".to_string();
        
        // Initially needs refresh
        assert!(cache.needs_refresh(&url));
        
        // Set cache
        cache.set(url.clone(), jwks, 3600);
        
        // Should not need refresh immediately
        assert!(!cache.needs_refresh(&url));
    }
    
    #[test]
    fn test_jwks_cache_global_shared() {
        let jwks = create_test_jwks();
        let url = "https://example.com/jwks".to_string();
        
        // Set in global cache
        JwksCache::set_global(url.clone(), jwks.clone(), 3600);
        
        // Get from global cache
        let cached = JwksCache::get_global(&url);
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().keys.len(), 3);
    }
    
    #[test]
    fn test_jwks_cache_multiple_urls() {
        let mut cache = JwksCache::new();
        let jwks1 = create_test_jwks();
        let jwks2 = Jwks { keys: vec![create_test_jwk(Some("different"))] };
        
        let url1 = "https://example.com/jwks1".to_string();
        let url2 = "https://example.com/jwks2".to_string();
        
        cache.set(url1.clone(), jwks1, 3600);
        cache.set(url2.clone(), jwks2, 3600);
        
        let cached1 = cache.get(&url1);
        let cached2 = cache.get(&url2);
        
        assert!(cached1.is_some());
        assert!(cached2.is_some());
        assert_eq!(cached1.unwrap().keys.len(), 3);
        assert_eq!(cached2.unwrap().keys.len(), 1);
    }
}
