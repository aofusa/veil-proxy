//! Routing Optimization Module
//!
//! Provides optimized routing data structures for high-performance route matching.
//!
//! # Optimizations
//!
//! - **Phase 1: Host-based Grouping** - O(1) host lookup using HashMap
//! - **Phase 2: Path Radix Tree** - O(k) path matching using matchit crate
//! - **Phase 3: CIDR Tree** - Efficient IP range matching using sorted structures
//! - **Phase 4: LRU Cache** - Cache route results for repeated requests
//!
//! # Example
//!
//! ```ignore
//! let router = OptimizedRouter::build(&routes);
//! let route_idx = router.find_route(host, path, method, headers, query, source_ip);
//! ```

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use lru::LruCache;
use std::num::NonZeroUsize;

// ====================
// Phase 1: Host-based Grouping
// ====================

/// Host-based route grouping for O(1) initial lookup
/// 
/// Routes are grouped by their host condition:
/// - Exact host matches go into `exact` HashMap
/// - Wildcard hosts (*.example.com) go into `wildcard` Vec
/// - Routes without host condition go into `any_host` Vec
#[derive(Debug)]
pub struct HostRouter {
    /// Exact host match: host -> list of route indices
    exact: HashMap<String, Vec<usize>>,
    /// Wildcard host patterns: (suffix, route indices)
    /// e.g., "*.example.com" stored as ("example.com", [indices])
    wildcard: Vec<(String, Vec<usize>)>,
    /// Routes without host condition (matches all hosts)
    any_host: Vec<usize>,
}

impl HostRouter {
    /// Build a new HostRouter from route conditions
    pub fn new() -> Self {
        Self {
            exact: HashMap::new(),
            wildcard: Vec::new(),
            any_host: Vec::new(),
        }
    }

    /// Add a route with its host condition
    pub fn add_route(&mut self, route_idx: usize, host_condition: Option<&str>) {
        match host_condition {
            None => {
                // No host condition - matches all hosts
                self.any_host.push(route_idx);
            }
            Some(pattern) => {
                if let Some(suffix) = pattern.strip_prefix("*.") {
                    // Wildcard pattern: *.example.com
                    if let Some(entry) = self.wildcard.iter_mut().find(|(s, _)| s == suffix) {
                        entry.1.push(route_idx);
                    } else {
                        self.wildcard.push((suffix.to_lowercase(), vec![route_idx]));
                    }
                } else if pattern.ends_with(".*") {
                    // Suffix wildcard: api.* - treat as wildcard
                    let prefix = pattern.strip_suffix(".*").unwrap_or(pattern);
                    // Store as special prefix pattern
                    let key = format!("__prefix__:{}", prefix.to_lowercase());
                    if let Some(entry) = self.wildcard.iter_mut().find(|(s, _)| s == &key) {
                        entry.1.push(route_idx);
                    } else {
                        self.wildcard.push((key, vec![route_idx]));
                    }
                } else {
                    // Exact match
                    self.exact
                        .entry(pattern.to_lowercase())
                        .or_insert_with(Vec::new)
                        .push(route_idx);
                }
            }
        }
    }

    /// Get candidate route indices for a given host
    /// Returns indices in priority order (exact -> wildcard -> any)
    pub fn get_candidates(&self, host: &str) -> Vec<usize> {
        let host_lower = host.to_lowercase();
        // Remove port if present
        let host_only = host_lower.split(':').next().unwrap_or(&host_lower);
        
        let mut candidates = Vec::new();
        
        // 1. Exact match (highest priority)
        if let Some(indices) = self.exact.get(host_only) {
            candidates.extend(indices.iter().copied());
        }
        
        // 2. Wildcard matches
        for (pattern, indices) in &self.wildcard {
            if pattern.starts_with("__prefix__:") {
                // Prefix pattern (api.*)
                let prefix = &pattern["__prefix__:".len()..];
                if host_only.starts_with(prefix) && host_only.len() > prefix.len() {
                    let rest = &host_only[prefix.len()..];
                    if rest.starts_with('.') {
                        candidates.extend(indices.iter().copied());
                    }
                }
            } else {
                // Suffix pattern (*.example.com)
                if host_only.ends_with(pattern) {
                    let prefix_len = host_only.len() - pattern.len();
                    if prefix_len > 0 && host_only.as_bytes()[prefix_len - 1] == b'.' {
                        // Check it's a valid subdomain (no extra dots)
                        let subdomain = &host_only[..prefix_len - 1];
                        if !subdomain.contains('.') {
                            candidates.extend(indices.iter().copied());
                        }
                    }
                }
            }
        }
        
        // 3. Any host (lowest priority, but always included)
        candidates.extend(self.any_host.iter().copied());
        
        candidates
    }

    /// Get exact match host count (for statistics)
    pub fn exact_count(&self) -> usize {
        self.exact.len()
    }

    /// Get wildcard pattern count (for statistics)
    pub fn wildcard_count(&self) -> usize {
        self.wildcard.len()
    }
}

impl Default for HostRouter {
    fn default() -> Self {
        Self::new()
    }
}

// ====================
// Phase 2: Path Radix Tree
// ====================

/// Path-based route matching using Radix Tree (matchit)
/// 
/// Provides O(k) path matching where k is the path length
#[derive(Debug)]
pub struct PathRouter {
    /// Radix tree router: path pattern -> route indices
    router: matchit::Router<Vec<usize>>,
    /// Routes without path condition (matches all paths)
    any_path: Vec<usize>,
    /// Original patterns for fallback matching
    patterns: Vec<(String, usize)>,
}

impl PathRouter {
    /// Build a new PathRouter
    pub fn new() -> Self {
        Self {
            router: matchit::Router::new(),
            any_path: Vec::new(),
            patterns: Vec::new(),
        }
    }

    /// Add a route with its path condition
    pub fn add_route(&mut self, route_idx: usize, path_condition: Option<&str>) {
        match path_condition {
            None => {
                // No path condition - matches all paths
                self.any_path.push(route_idx);
            }
            Some(pattern) => {
                // Store original pattern for fallback
                self.patterns.push((pattern.to_string(), route_idx));
                
                // Convert pattern to matchit format
                let matchit_pattern = self.convert_pattern(pattern);
                
                // Try to insert into router
                match self.router.at_mut(&matchit_pattern) {
                    Ok(matched) => {
                        // Pattern already exists, add to list
                        matched.value.push(route_idx);
                    }
                    Err(_) => {
                        // New pattern, insert
                        if self.router.insert(matchit_pattern.clone(), vec![route_idx]).is_err() {
                            // If insert fails (conflicting patterns), use fallback
                            // The pattern is already stored in self.patterns
                        }
                    }
                }
            }
        }
    }

    /// Convert our pattern format to matchit format
    /// /api/* -> /api/{*rest}
    fn convert_pattern(&self, pattern: &str) -> String {
        if let Some(prefix) = pattern.strip_suffix("/*") {
            format!("{}/{{*rest}}", prefix)
        } else {
            pattern.to_string()
        }
    }

    /// Get candidate route indices for a given path
    pub fn get_candidates(&self, path: &str) -> Vec<usize> {
        let mut candidates = Vec::new();
        
        // 1. Try radix tree match
        if let Ok(matched) = self.router.at(path) {
            candidates.extend(matched.value.iter().copied());
        }
        
        // 2. Fallback pattern matching for complex patterns
        for (pattern, idx) in &self.patterns {
            if !candidates.contains(idx) && self.matches_pattern(pattern, path) {
                candidates.push(*idx);
            }
        }
        
        // 3. Any path matches
        candidates.extend(self.any_path.iter().copied());
        
        candidates
    }

    /// Check if path matches pattern (fallback for complex patterns)
    fn matches_pattern(&self, pattern: &str, path: &str) -> bool {
        // Exact match
        if pattern == path {
            return true;
        }
        
        // Wildcard pattern: /api/*
        if let Some(prefix) = pattern.strip_suffix("/*") {
            return path.starts_with(prefix) && 
                   (path.len() == prefix.len() || path.as_bytes().get(prefix.len()) == Some(&b'/'));
        }
        
        // Prefix match
        if path.starts_with(pattern) {
            let remaining = &path[pattern.len()..];
            return remaining.is_empty() || remaining.starts_with('/');
        }
        
        false
    }

    /// Get path pattern count (for statistics)
    pub fn patterns_count(&self) -> usize {
        self.patterns.len()
    }
}

impl Default for PathRouter {
    fn default() -> Self {
        Self::new()
    }
}

// ====================
// Phase 3: CIDR Tree Optimization
// ====================

/// Efficient CIDR range matching
/// 
/// Uses sorted structures for faster IP range lookups
#[derive(Debug, Clone)]
pub struct CidrMatcher {
    /// IPv4 CIDR ranges: (network, prefix_len, route_indices)
    v4_ranges: Vec<(u32, u8, Vec<usize>)>,
    /// IPv6 CIDR ranges: (network, prefix_len, route_indices)
    v6_ranges: Vec<(u128, u8, Vec<usize>)>,
    /// Exact IP matches
    exact_ips: HashMap<IpAddr, Vec<usize>>,
    /// Routes without source_ip condition
    any_ip: Vec<usize>,
}

impl CidrMatcher {
    /// Create a new CidrMatcher
    pub fn new() -> Self {
        Self {
            v4_ranges: Vec::new(),
            v6_ranges: Vec::new(),
            exact_ips: HashMap::new(),
            any_ip: Vec::new(),
        }
    }

    /// Add a route with its source_ip condition
    pub fn add_route(&mut self, route_idx: usize, ip_ranges: Option<&[String]>) {
        match ip_ranges {
            None => {
                self.any_ip.push(route_idx);
            }
            Some(ranges) => {
                for cidr in ranges {
                    self.add_cidr(route_idx, cidr);
                }
            }
        }
    }

    /// Parse and add a CIDR range
    fn add_cidr(&mut self, route_idx: usize, cidr: &str) {
        if let Some((network_str, prefix_len_str)) = cidr.split_once('/') {
            // CIDR notation
            if let (Ok(network), Ok(prefix_len)) = (
                network_str.parse::<IpAddr>(),
                prefix_len_str.parse::<u8>()
            ) {
                match network {
                    IpAddr::V4(v4) => {
                        let network_u32 = u32::from_be_bytes(v4.octets());
                        // Find existing range or create new
                        if let Some(entry) = self.v4_ranges.iter_mut()
                            .find(|(n, p, _)| *n == network_u32 && *p == prefix_len)
                        {
                            if !entry.2.contains(&route_idx) {
                                entry.2.push(route_idx);
                            }
                        } else {
                            self.v4_ranges.push((network_u32, prefix_len, vec![route_idx]));
                        }
                    }
                    IpAddr::V6(v6) => {
                        let network_u128 = u128::from_be_bytes(v6.octets());
                        if let Some(entry) = self.v6_ranges.iter_mut()
                            .find(|(n, p, _)| *n == network_u128 && *p == prefix_len)
                        {
                            if !entry.2.contains(&route_idx) {
                                entry.2.push(route_idx);
                            }
                        } else {
                            self.v6_ranges.push((network_u128, prefix_len, vec![route_idx]));
                        }
                    }
                }
            }
        } else {
            // Exact IP
            if let Ok(ip) = cidr.parse::<IpAddr>() {
                self.exact_ips
                    .entry(ip)
                    .or_insert_with(Vec::new)
                    .push(route_idx);
            }
        }
    }

    /// Sort ranges by prefix length (most specific first)
    pub fn optimize(&mut self) {
        // Sort by prefix length descending (more specific first)
        self.v4_ranges.sort_by(|a, b| b.1.cmp(&a.1));
        self.v6_ranges.sort_by(|a, b| b.1.cmp(&a.1));
    }

    /// Check if an IP matches and get candidate routes
    pub fn get_candidates(&self, addr: &SocketAddr) -> Vec<usize> {
        let ip = addr.ip();
        let mut candidates = Vec::new();
        
        // 1. Check exact IP match
        if let Some(indices) = self.exact_ips.get(&ip) {
            candidates.extend(indices.iter().copied());
        }
        
        // 2. Check CIDR ranges
        match ip {
            IpAddr::V4(v4) => {
                let ip_u32 = u32::from_be_bytes(v4.octets());
                for (network, prefix_len, indices) in &self.v4_ranges {
                    let mask = if *prefix_len == 0 {
                        0
                    } else {
                        !((1u32 << (32 - prefix_len)) - 1)
                    };
                    if (ip_u32 & mask) == (*network & mask) {
                        for idx in indices {
                            if !candidates.contains(idx) {
                                candidates.push(*idx);
                            }
                        }
                    }
                }
            }
            IpAddr::V6(v6) => {
                let ip_u128 = u128::from_be_bytes(v6.octets());
                for (network, prefix_len, indices) in &self.v6_ranges {
                    let mask = if *prefix_len == 0 {
                        0
                    } else {
                        !((1u128 << (128 - prefix_len)) - 1)
                    };
                    if (ip_u128 & mask) == (*network & mask) {
                        for idx in indices {
                            if !candidates.contains(idx) {
                                candidates.push(*idx);
                            }
                        }
                    }
                }
            }
        }
        
        // 3. Any IP (routes without source_ip condition)
        candidates.extend(self.any_ip.iter().copied());
        
        candidates
    }
    
    /// Check if this matcher has any source_ip conditions
    pub fn has_conditions(&self) -> bool {
        !self.v4_ranges.is_empty() || !self.v6_ranges.is_empty() || !self.exact_ips.is_empty()
    }
}

impl Default for CidrMatcher {
    fn default() -> Self {
        Self::new()
    }
}

// ====================
// Phase 4: LRU Cache
// ====================

/// Cache key for route matching results
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RouteCacheKey {
    /// Host header value
    pub host: String,
    /// Request path
    pub path: String,
    /// HTTP method
    pub method: String,
    /// Source IP (as string for simplicity)
    pub source_ip: String,
}

impl RouteCacheKey {
    /// Create a new cache key
    pub fn new(host: &[u8], path: &[u8], method: &[u8], source_ip: &SocketAddr) -> Self {
        Self {
            host: String::from_utf8_lossy(host).to_string(),
            path: String::from_utf8_lossy(path).to_string(),
            method: String::from_utf8_lossy(method).to_string(),
            source_ip: source_ip.ip().to_string(),
        }
    }
}

/// LRU cache for route matching results
/// 
/// Caches the index of matched routes for repeated requests
pub struct RouteCache {
    /// LRU cache: key -> route index
    cache: std::sync::Mutex<LruCache<RouteCacheKey, Option<usize>>>,
    /// Cache hit counter
    hits: std::sync::atomic::AtomicU64,
    /// Cache miss counter
    misses: std::sync::atomic::AtomicU64,
}

impl RouteCache {
    /// Create a new RouteCache with specified capacity
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::new(1000).unwrap());
        Self {
            cache: std::sync::Mutex::new(LruCache::new(cap)),
            hits: std::sync::atomic::AtomicU64::new(0),
            misses: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Try to get a cached route index
    pub fn get(&self, key: &RouteCacheKey) -> Option<Option<usize>> {
        if let Ok(mut cache) = self.cache.lock() {
            if let Some(result) = cache.get(key) {
                self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Some(*result);
            }
        }
        self.misses.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        None
    }

    /// Cache a route matching result
    pub fn put(&self, key: RouteCacheKey, route_idx: Option<usize>) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.put(key, route_idx);
        }
    }

    /// Get cache statistics
    pub fn stats(&self) -> (u64, u64) {
        (
            self.hits.load(std::sync::atomic::Ordering::Relaxed),
            self.misses.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Clear the cache
    pub fn clear(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.clear();
        }
    }
}

impl std::fmt::Debug for RouteCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (hits, misses) = self.stats();
        f.debug_struct("RouteCache")
            .field("hits", &hits)
            .field("misses", &misses)
            .finish()
    }
}

// ====================
// Optimized Router (Combined)
// ====================

/// Combined optimized router using all optimization phases
pub struct OptimizedRouter {
    /// Phase 1: Host-based grouping
    pub host_router: HostRouter,
    /// Phase 2: Path radix tree
    pub path_router: PathRouter,
    /// Phase 3: CIDR matcher
    pub cidr_matcher: CidrMatcher,
    /// Phase 4: Route cache
    pub cache: Arc<RouteCache>,
    /// Route count for validation
    pub route_count: usize,
}

impl OptimizedRouter {
    /// Create a new OptimizedRouter with default cache capacity
    pub fn new() -> Self {
        Self::with_cache_capacity(10000)
    }

    /// Create a new OptimizedRouter with specified cache capacity
    pub fn with_cache_capacity(cache_capacity: usize) -> Self {
        Self {
            host_router: HostRouter::new(),
            path_router: PathRouter::new(),
            cidr_matcher: CidrMatcher::new(),
            cache: Arc::new(RouteCache::new(cache_capacity)),
            route_count: 0,
        }
    }

    /// Add a route to the router
    /// 
    /// # Arguments
    /// * `route_idx` - Index of the route in the routes array
    /// * `host` - Host condition (None for any host)
    /// * `path` - Path condition (None for any path)
    /// * `source_ip` - Source IP conditions (None for any IP)
    pub fn add_route(
        &mut self,
        route_idx: usize,
        host: Option<&str>,
        path: Option<&str>,
        source_ip: Option<&[String]>,
    ) {
        self.host_router.add_route(route_idx, host);
        self.path_router.add_route(route_idx, path);
        self.cidr_matcher.add_route(route_idx, source_ip);
        self.route_count = self.route_count.max(route_idx + 1);
    }

    /// Finalize router construction and optimize data structures
    pub fn finalize(&mut self) {
        self.cidr_matcher.optimize();
    }

    /// Get candidate routes for a request
    /// 
    /// Returns route indices that potentially match, in priority order.
    /// The caller should still verify the full conditions (header, method, query).
    pub fn get_candidates(
        &self,
        host: &str,
        path: &str,
        source_ip: &SocketAddr,
    ) -> Vec<usize> {
        // Get candidates from each router
        let host_candidates = self.host_router.get_candidates(host);
        let path_candidates = self.path_router.get_candidates(path);
        
        // If no source_ip conditions exist, skip CIDR matching
        let ip_candidates = if self.cidr_matcher.has_conditions() {
            self.cidr_matcher.get_candidates(source_ip)
        } else {
            // All routes are candidates for IP
            (0..self.route_count).collect()
        };

        // Intersect candidates (route must match all conditions)
        let mut final_candidates = Vec::new();
        
        for idx in &host_candidates {
            if path_candidates.contains(idx) && ip_candidates.contains(idx) {
                final_candidates.push(*idx);
            }
        }
        
        // Sort by index to maintain original priority order
        final_candidates.sort_unstable();
        final_candidates.dedup();
        
        final_candidates
    }

    /// Try cache lookup first
    pub fn try_cache(&self, key: &RouteCacheKey) -> Option<Option<usize>> {
        self.cache.get(key)
    }

    /// Cache a result
    pub fn cache_result(&self, key: RouteCacheKey, route_idx: Option<usize>) {
        self.cache.put(key, route_idx);
    }

    /// Clear the cache (call on config reload)
    pub fn clear_cache(&self) {
        self.cache.clear();
    }

    /// Get cache statistics
    pub fn cache_stats(&self) -> (u64, u64) {
        self.cache.stats()
    }
}

impl Default for OptimizedRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for OptimizedRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OptimizedRouter")
            .field("host_router", &self.host_router)
            .field("path_router", &self.path_router)
            .field("cidr_matcher", &self.cidr_matcher)
            .field("cache", &self.cache)
            .field("route_count", &self.route_count)
            .finish()
    }
}

// ====================
// Tests
// ====================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_host_router_exact_match() {
        let mut router = HostRouter::new();
        router.add_route(0, Some("example.com"));
        router.add_route(1, Some("api.example.com"));
        router.add_route(2, None);

        let candidates = router.get_candidates("example.com");
        assert!(candidates.contains(&0));
        assert!(candidates.contains(&2)); // any_host
        assert!(!candidates.contains(&1)); // different host

        let candidates = router.get_candidates("api.example.com");
        assert!(candidates.contains(&1));
        assert!(candidates.contains(&2)); // any_host
    }

    #[test]
    fn test_host_router_wildcard() {
        let mut router = HostRouter::new();
        router.add_route(0, Some("*.example.com"));
        router.add_route(1, None);

        let candidates = router.get_candidates("api.example.com");
        assert!(candidates.contains(&0));
        assert!(candidates.contains(&1));

        let candidates = router.get_candidates("example.com");
        assert!(!candidates.contains(&0)); // not a subdomain
        assert!(candidates.contains(&1));
    }

    #[test]
    fn test_path_router_basic() {
        let mut router = PathRouter::new();
        router.add_route(0, Some("/api"));
        router.add_route(1, Some("/api/*"));
        router.add_route(2, None);

        let candidates = router.get_candidates("/api");
        assert!(candidates.contains(&0) || candidates.contains(&1));
        assert!(candidates.contains(&2));

        let candidates = router.get_candidates("/api/v1/users");
        assert!(candidates.contains(&1));
        assert!(candidates.contains(&2));
    }

    #[test]
    fn test_cidr_matcher_ipv4() {
        let mut matcher = CidrMatcher::new();
        matcher.add_route(0, Some(&["192.168.0.0/16".to_string()]));
        matcher.add_route(1, Some(&["10.0.0.1".to_string()]));
        matcher.add_route(2, None);
        matcher.optimize();

        let addr: SocketAddr = "192.168.1.100:8080".parse().unwrap();
        let candidates = matcher.get_candidates(&addr);
        assert!(candidates.contains(&0));
        assert!(candidates.contains(&2));

        let addr: SocketAddr = "10.0.0.1:8080".parse().unwrap();
        let candidates = matcher.get_candidates(&addr);
        assert!(candidates.contains(&1));
        assert!(candidates.contains(&2));
    }

    #[test]
    fn test_route_cache() {
        let cache = RouteCache::new(100);
        let key = RouteCacheKey {
            host: "example.com".to_string(),
            path: "/api".to_string(),
            method: "GET".to_string(),
            source_ip: "127.0.0.1".to_string(),
        };

        // Cache miss
        assert!(cache.get(&key).is_none());
        let (hits, misses) = cache.stats();
        assert_eq!(hits, 0);
        assert_eq!(misses, 1);

        // Add to cache
        cache.put(key.clone(), Some(5));

        // Cache hit
        assert_eq!(cache.get(&key), Some(Some(5)));
        let (hits, misses) = cache.stats();
        assert_eq!(hits, 1);
        assert_eq!(misses, 1);
    }

    #[test]
    fn test_optimized_router() {
        let mut router = OptimizedRouter::new();
        router.add_route(0, Some("api.example.com"), Some("/v1/*"), None);
        router.add_route(1, Some("example.com"), Some("/"), None);
        router.add_route(2, None, None, None); // default route
        router.finalize();

        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        
        let candidates = router.get_candidates("api.example.com", "/v1/users", &addr);
        assert!(candidates.contains(&0));
        assert!(candidates.contains(&2));

        let candidates = router.get_candidates("example.com", "/", &addr);
        assert!(candidates.contains(&1));
        assert!(candidates.contains(&2));
    }
}
