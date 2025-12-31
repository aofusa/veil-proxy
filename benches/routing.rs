//! ルーティングパフォーマンスベンチマーク
//!
//! Phase 1-4最適化の効果を測定します。
//!
//! 使用方法:
//!   cargo bench --bench routing
//!
//! 測定項目:
//! - 線形探索 vs 最適化ルーター比較
//! - ルート数別パフォーマンス（10, 50, 100, 500ルート）
//! - キャッシュヒット/ミス時のパフォーマンス
//! - 各Phase単独パフォーマンス

use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId, black_box};
use std::collections::HashMap;
use std::net::SocketAddr;

// ベンチマーク用のルート条件構造体（シミュレーション）
#[derive(Clone, Debug)]
struct BenchRouteConditions {
    host: Option<String>,
    path: Option<String>,
    method: Option<Vec<String>>,
    header: Option<HashMap<String, String>>,
    query: Option<HashMap<String, String>>,
    source_ip: Option<Vec<String>>,
}

impl Default for BenchRouteConditions {
    fn default() -> Self {
        Self {
            host: None,
            path: None,
            method: None,
            header: None,
            query: None,
            source_ip: None,
        }
    }
}

/// テスト用ルートを生成
fn generate_routes(count: usize) -> Vec<BenchRouteConditions> {
    let mut routes = Vec::with_capacity(count);
    
    for i in 0..count {
        let route = BenchRouteConditions {
            host: if i % 3 == 0 {
                Some(format!("example{}.com", i))
            } else if i % 3 == 1 {
                Some(format!("*.example{}.com", i / 10))
            } else {
                None
            },
            path: if i % 2 == 0 {
                Some(format!("/api/v{}/resource/*", i % 5))
            } else {
                Some(format!("/static/{}", i))
            },
            method: if i % 4 == 0 {
                Some(vec!["GET".to_string(), "POST".to_string()])
            } else {
                None
            },
            header: None,
            query: None,
            source_ip: if i % 10 == 0 {
                Some(vec![format!("192.168.{}.0/24", i % 256)])
            } else {
                None
            },
        };
        routes.push(route);
    }
    
    // デフォルトルート（最後）
    routes.push(BenchRouteConditions::default());
    
    routes
}

/// 線形探索（従来方式）のシミュレーション
fn linear_search(
    routes: &[BenchRouteConditions],
    host: &str,
    path: &str,
    _method: &str,
) -> Option<usize> {
    for (i, route) in routes.iter().enumerate() {
        // Host チェック
        if let Some(ref host_pattern) = route.host {
            if host_pattern.starts_with("*.") {
                let suffix = &host_pattern[2..];
                if !host.ends_with(suffix) {
                    continue;
                }
            } else if host != host_pattern {
                continue;
            }
        }
        
        // Path チェック
        if let Some(ref path_pattern) = route.path {
            if path_pattern.ends_with("/*") {
                let prefix = &path_pattern[..path_pattern.len() - 2];
                if !path.starts_with(prefix) {
                    continue;
                }
            } else if path != path_pattern {
                continue;
            }
        }
        
        return Some(i);
    }
    
    None
}

/// 最適化ルーター（Phase 1-2）のシミュレーション
struct SimulatedOptimizedRouter {
    /// exact host -> route indices
    exact_hosts: HashMap<String, Vec<usize>>,
    /// wildcard suffix -> route indices
    wildcard_hosts: Vec<(String, Vec<usize>)>,
    /// any host routes
    any_host: Vec<usize>,
    /// matchit router for paths
    path_router: matchit::Router<Vec<usize>>,
    /// any path routes
    any_path: Vec<usize>,
}

impl SimulatedOptimizedRouter {
    fn new(routes: &[BenchRouteConditions]) -> Self {
        let mut exact_hosts: HashMap<String, Vec<usize>> = HashMap::new();
        let mut wildcard_hosts: Vec<(String, Vec<usize>)> = Vec::new();
        let mut any_host: Vec<usize> = Vec::new();
        let mut path_router = matchit::Router::new();
        let mut any_path: Vec<usize> = Vec::new();
        
        for (idx, route) in routes.iter().enumerate() {
            // Host indexing
            match &route.host {
                Some(h) if h.starts_with("*.") => {
                    let suffix = h[2..].to_lowercase();
                    if let Some(entry) = wildcard_hosts.iter_mut().find(|(s, _)| s == &suffix) {
                        entry.1.push(idx);
                    } else {
                        wildcard_hosts.push((suffix, vec![idx]));
                    }
                }
                Some(h) => {
                    exact_hosts.entry(h.to_lowercase()).or_default().push(idx);
                }
                None => {
                    any_host.push(idx);
                }
            }
            
            // Path indexing
            match &route.path {
                Some(p) => {
                    let matchit_path = if p.ends_with("/*") {
                        format!("{}/{{*rest}}", &p[..p.len() - 2])
                    } else {
                        p.clone()
                    };
                    
                    match path_router.at_mut(&matchit_path) {
                        Ok(m) => m.value.push(idx),
                        Err(_) => {
                            let _ = path_router.insert(matchit_path, vec![idx]);
                        }
                    }
                }
                None => {
                    any_path.push(idx);
                }
            }
        }
        
        Self {
            exact_hosts,
            wildcard_hosts,
            any_host,
            path_router,
            any_path,
        }
    }
    
    fn get_candidates(&self, host: &str, path: &str) -> Vec<usize> {
        let host_lower = host.to_lowercase();
        let mut host_candidates = Vec::new();
        
        // Exact match
        if let Some(indices) = self.exact_hosts.get(&host_lower) {
            host_candidates.extend(indices.iter().copied());
        }
        
        // Wildcard match
        for (suffix, indices) in &self.wildcard_hosts {
            if host_lower.ends_with(suffix) {
                host_candidates.extend(indices.iter().copied());
            }
        }
        
        // Any host
        host_candidates.extend(self.any_host.iter().copied());
        
        // Path candidates
        let mut path_candidates = Vec::new();
        if let Ok(m) = self.path_router.at(path) {
            path_candidates.extend(m.value.iter().copied());
        }
        path_candidates.extend(self.any_path.iter().copied());
        
        // Intersection
        host_candidates
            .into_iter()
            .filter(|idx| path_candidates.contains(idx))
            .collect()
    }
    
    fn find(&self, host: &str, path: &str, _method: &str) -> Option<usize> {
        let candidates = self.get_candidates(host, path);
        candidates.into_iter().min()
    }
}

/// LRU キャッシュ付き検索のシミュレーション
struct CachedRouter {
    router: SimulatedOptimizedRouter,
    cache: std::sync::Mutex<lru::LruCache<(String, String), Option<usize>>>,
}

impl CachedRouter {
    fn new(routes: &[BenchRouteConditions], cache_size: usize) -> Self {
        Self {
            router: SimulatedOptimizedRouter::new(routes),
            cache: std::sync::Mutex::new(lru::LruCache::new(
                std::num::NonZeroUsize::new(cache_size).unwrap()
            )),
        }
    }
    
    fn find_with_cache(&self, host: &str, path: &str, method: &str) -> Option<usize> {
        let key = (host.to_string(), path.to_string());
        
        // Cache check
        if let Ok(mut cache) = self.cache.lock() {
            if let Some(result) = cache.get(&key) {
                return *result;
            }
        }
        
        // Cache miss
        let result = self.router.find(host, path, method);
        
        // Cache result
        if let Ok(mut cache) = self.cache.lock() {
            cache.put(key, result);
        }
        
        result
    }
}

/// 線形探索 vs 最適化ルーターベンチマーク
fn benchmark_linear_vs_optimized(c: &mut Criterion) {
    let mut group = c.benchmark_group("routing_comparison");
    
    for route_count in [10, 50, 100, 500].iter() {
        let routes = generate_routes(*route_count);
        let optimized = SimulatedOptimizedRouter::new(&routes);
        
        // 中間位置のルートにマッチするリクエスト
        let mid_idx = route_count / 2;
        let host = format!("example{}.com", mid_idx);
        let path = format!("/api/v{}/resource/item", mid_idx % 5);
        
        group.bench_with_input(
            BenchmarkId::new("linear", route_count),
            &(&routes, &host, &path),
            |b, (routes, host, path)| {
                b.iter(|| {
                    black_box(linear_search(routes, host, path, "GET"))
                });
            },
        );
        
        group.bench_with_input(
            BenchmarkId::new("optimized", route_count),
            &(&optimized, &host, &path),
            |b, (router, host, path)| {
                b.iter(|| {
                    black_box(router.find(host, path, "GET"))
                });
            },
        );
    }
    
    group.finish();
}

/// キャッシュ効果ベンチマーク
fn benchmark_cache_performance(c: &mut Criterion) {
    let mut group = c.benchmark_group("routing_cache");
    
    let routes = generate_routes(100);
    let cached = CachedRouter::new(&routes, 1000);
    let uncached = SimulatedOptimizedRouter::new(&routes);
    
    // 同一リクエストのキャッシュヒット
    let host = "example50.com";
    let path = "/api/v0/resource/item";
    
    // ウォームアップ（キャッシュに入れる）
    let _ = cached.find_with_cache(host, path, "GET");
    
    group.bench_function("cache_hit", |b| {
        b.iter(|| {
            black_box(cached.find_with_cache(host, path, "GET"))
        });
    });
    
    // キャッシュなし（毎回検索）
    group.bench_function("no_cache", |b| {
        b.iter(|| {
            black_box(uncached.find(host, path, "GET"))
        });
    });
    
    // さまざまなリクエスト（キャッシュミス）
    group.bench_function("cache_miss_varied", |b| {
        let mut i = 0;
        b.iter(|| {
            i += 1;
            let host = format!("example{}.com", i % 100);
            let path = format!("/api/v{}/resource/{}", i % 5, i);
            black_box(cached.find_with_cache(&host, &path, "GET"))
        });
    });
    
    group.finish();
}

/// デフォルトルート（最終ルート）へのフォールバックベンチマーク
fn benchmark_worst_case(c: &mut Criterion) {
    let mut group = c.benchmark_group("routing_worst_case");
    
    for route_count in [10, 50, 100, 500].iter() {
        let routes = generate_routes(*route_count);
        let optimized = SimulatedOptimizedRouter::new(&routes);
        
        // マッチしないリクエスト（デフォルトルートへ）
        let host = "unknown-host.invalid";
        let path = "/nonexistent/path";
        
        group.bench_with_input(
            BenchmarkId::new("linear_fallback", route_count),
            &(&routes, host, path),
            |b, (routes, host, path)| {
                b.iter(|| {
                    black_box(linear_search(routes, host, path, "GET"))
                });
            },
        );
        
        group.bench_with_input(
            BenchmarkId::new("optimized_fallback", route_count),
            &(&optimized, host, path),
            |b, (router, host, path)| {
                b.iter(|| {
                    black_box(router.find(host, path, "GET"))
                });
            },
        );
    }
    
    group.finish();
}

/// 候補フィルタリング効果のベンチマーク
fn benchmark_candidate_filtering(c: &mut Criterion) {
    let mut group = c.benchmark_group("candidate_filtering");
    
    // 多数のホスト、少数のパス
    let mut routes = Vec::with_capacity(200);
    for i in 0..100 {
        routes.push(BenchRouteConditions {
            host: Some(format!("api{}.example.com", i)),
            path: Some("/api/*".to_string()),
            ..Default::default()
        });
    }
    for i in 0..100 {
        routes.push(BenchRouteConditions {
            host: Some(format!("static{}.example.com", i)),
            path: Some("/static/*".to_string()),
            ..Default::default()
        });
    }
    routes.push(BenchRouteConditions::default()); // default route
    
    let optimized = SimulatedOptimizedRouter::new(&routes);
    
    // 特定のホストへのリクエスト（候補が絞られる）
    let host = "api50.example.com";
    let path = "/api/v1/users";
    
    group.bench_function("filtered_host", |b| {
        b.iter(|| {
            black_box(optimized.find(host, path, "GET"))
        });
    });
    
    group.bench_function("linear_200_routes", |b| {
        b.iter(|| {
            black_box(linear_search(&routes, host, path, "GET"))
        });
    });
    
    group.finish();
}

criterion_group!(
    benches,
    benchmark_linear_vs_optimized,
    benchmark_cache_performance,
    benchmark_worst_case,
    benchmark_candidate_filtering,
);
criterion_main!(benches);
