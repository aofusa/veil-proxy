//! キャッシュマネージャー
//!
//! メモリキャッシュとディスクキャッシュを統合管理するマネージャーを提供します。

use super::config::{CacheConfig, StorageType};
use super::disk::{DiskCache, DiskCacheConfig};
use super::entry::{CacheEntry, CacheEntryBuilder};
use super::index::CacheIndex;
use super::key::CacheKey;
use super::memory::MemoryCache;
use super::policy::CachePolicy;
use std::io;
use std::sync::Arc;
use std::time::Instant;

/// キャッシュマネージャー
/// 
/// インデックス、メモリキャッシュ、ディスクキャッシュを統合管理します。
/// スレッドセーフで、複数のワーカーから同時にアクセス可能です。
pub struct CacheManager {
    /// 設定
    config: CacheConfig,
    /// キャッシュインデックス
    index: CacheIndex,
    /// メモリキャッシュ
    memory: MemoryCache,
    /// ディスクキャッシュ（オプション）
    disk: Option<DiskCache>,
    /// 作成時刻
    created_at: Instant,
}

impl CacheManager {
    /// 新しいキャッシュマネージャーを作成
    pub fn new(config: CacheConfig) -> io::Result<Self> {
        // メモリキャッシュを作成
        let max_entries = config.max_memory_size / 1024; // 概算エントリ数
        let memory = MemoryCache::new(max_entries, config.max_memory_size);
        
        // ディスクキャッシュを作成（設定されている場合）
        let disk = if let Some(ref disk_path) = config.disk_path {
            let disk_config = DiskCacheConfig {
                base_path: disk_path.clone(),
                max_size: config.max_disk_size as u64,
                extension: "cache".to_string(),
            };
            Some(DiskCache::new(disk_config)?)
        } else {
            None
        };
        
        Ok(Self {
            config,
            index: CacheIndex::new(),
            memory,
            disk,
            created_at: Instant::now(),
        })
    }
    
    /// 設定を取得
    #[inline]
    pub fn config(&self) -> &CacheConfig {
        &self.config
    }
    
    /// キャッシュが有効かどうか
    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }
    
    /// リクエストがキャッシュ可能かチェック
    pub fn is_request_cacheable(
        &self,
        method: &[u8],
        path: &str,
        request_headers: &[(Box<[u8]>, Box<[u8]>)],
    ) -> bool {
        // キャッシュが無効
        if !self.config.enabled {
            return false;
        }
        
        // メソッドチェック
        if !self.config.is_cacheable_method(method) {
            return false;
        }
        
        // バイパスパターンチェック
        if self.config.should_bypass(path) {
            return false;
        }
        
        // リクエストヘッダーチェック（no-cache, no-store）
        if CachePolicy::request_bypasses_cache(request_headers) {
            return false;
        }
        
        true
    }
    
    /// キャッシュからエントリを取得
    pub fn get(&self, key: &CacheKey) -> Option<Arc<CacheEntry>> {
        self.index.get(key)
    }
    
    /// stale-while-revalidate用にstaleなエントリを取得
    pub fn get_stale(&self, key: &CacheKey, max_stale_secs: u64) -> Option<Arc<CacheEntry>> {
        let entry = self.index.get_stale(key)?;
        
        // stale期間チェック
        if entry.stale_duration() <= max_stale_secs {
            Some(entry)
        } else {
            None
        }
    }
    
    /// レスポンスをキャッシュに保存
    /// 
    /// レスポンスがキャッシュ可能な場合のみ保存します。
    pub fn store(
        &self,
        key: CacheKey,
        status_code: u16,
        headers: Vec<(Box<[u8]>, Box<[u8]>)>,
        body: Vec<u8>,
    ) -> bool {
        // ステータスコードチェック
        if !self.config.is_cacheable_status(status_code) {
            return false;
        }
        
        // Cache-Controlチェック
        let ttl = match CachePolicy::check_response(
            status_code,
            &headers,
            &self.config.cacheable_statuses,
            self.config.default_ttl_secs,
        ) {
            Some(ttl) => ttl,
            None => return false,
        };
        
        // Varyチェック（Vary: * はキャッシュ不可）
        if let Some(vary_headers) = CachePolicy::parse_vary(&headers) {
            if vary_headers.is_empty() && self.config.respect_vary {
                // Vary: * の場合はNoneが返される
                return false;
            }
        }
        
        // ストレージ選択
        let storage_type = self.config.select_storage(body.len());
        
        let entry = match storage_type {
            StorageType::Memory => {
                CacheEntryBuilder::new(status_code)
                    .headers(headers)
                    .body(body)
                    .max_age(ttl)
                    .build_memory()
            }
            StorageType::Disk => {
                if let Some(ref disk) = self.disk {
                    // ディスクに書き込み
                    let path = match disk.write_sync(&key, &body) {
                        Ok(p) => p,
                        Err(_) => return false,
                    };
                    
                    let (entry, _) = CacheEntryBuilder::new(status_code)
                        .headers(headers)
                        .body(body)
                        .max_age(ttl)
                        .build_disk(path);
                    entry
                } else {
                    // ディスクが無ければメモリに
                    CacheEntryBuilder::new(status_code)
                        .headers(headers)
                        .body(body)
                        .max_age(ttl)
                        .build_memory()
                }
            }
        };
        
        // インデックスに追加
        self.index.insert(key, entry);
        
        true
    }
    
    /// エントリを削除
    pub fn invalidate(&self, key: &CacheKey) {
        // インデックスから削除
        if let Some(entry) = self.index.remove(key) {
            // ディスクファイルも削除
            if let Some(path) = entry.disk_path() {
                if self.disk.is_some() {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
    
    /// パターンに一致するエントリを削除
    #[allow(dead_code)]
    pub fn invalidate_pattern(&self, _pattern: &str) -> usize {
        // 注意: 現在の実装ではパターンマッチングは非効率
        // TODO: より効率的な実装
        0
    }
    
    /// 期限切れエントリを削除
    pub fn evict_expired(&self) -> usize {
        self.index.evict_expired()
    }
    
    /// LRUエビクションを実行
    pub fn evict_lru(&self) -> usize {
        self.index.evict_lru(self.config.max_memory_size)
    }
    
    /// ディスクエビクションを実行
    pub fn evict_disk(&self) -> io::Result<usize> {
        if let Some(ref disk) = self.disk {
            let target = (self.config.max_disk_size as u64) * 9 / 10; // 90%
            disk.evict_to_size(target)
        } else {
            Ok(0)
        }
    }
    
    /// 全エントリを削除
    pub fn clear(&self) -> io::Result<()> {
        self.index.clear();
        self.memory.clear();
        if let Some(ref disk) = self.disk {
            disk.clear()?;
        }
        Ok(())
    }
    
    /// 統計情報を取得
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            entries: self.index.len(),
            memory_usage: self.index.memory_usage(),
            disk_usage: self.disk.as_ref().map(|d| d.current_size()).unwrap_or(0),
            hits: self.index.hits(),
            misses: self.index.misses(),
            hit_rate: self.index.hit_rate(),
            uptime_secs: self.created_at.elapsed().as_secs(),
        }
    }
}

/// キャッシュ統計情報
#[derive(Debug, Clone)]
pub struct CacheStats {
    /// エントリ数
    pub entries: usize,
    /// メモリ使用量
    pub memory_usage: usize,
    /// ディスク使用量
    pub disk_usage: u64,
    /// ヒット数
    pub hits: u64,
    /// ミス数
    pub misses: u64,
    /// ヒット率
    pub hit_rate: f64,
    /// 稼働時間（秒）
    pub uptime_secs: u64,
}

/// グローバルキャッシュマネージャー（オプション）
/// 
/// 複数のルートで共有するキャッシュマネージャーとして使用可能
use std::sync::OnceLock;

static GLOBAL_CACHE: OnceLock<Arc<CacheManager>> = OnceLock::new();

/// グローバルキャッシュマネージャーを初期化
pub fn init_global_cache(config: CacheConfig) -> io::Result<()> {
    let manager = CacheManager::new(config)?;
    GLOBAL_CACHE.set(Arc::new(manager)).ok();
    Ok(())
}

/// グローバルキャッシュマネージャーを取得
pub fn get_global_cache() -> Option<Arc<CacheManager>> {
    GLOBAL_CACHE.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::key::CacheableMethod;
    use tempfile::tempdir;

    fn create_test_config() -> CacheConfig {
        CacheConfig {
            enabled: true,
            max_memory_size: 1024 * 1024, // 1MB
            disk_path: None,
            ..Default::default()
        }
    }

    fn create_test_key(path: &str) -> CacheKey {
        CacheKey::new(CacheableMethod::Get, "example.com", path, None)
    }

    #[test]
    fn test_manager_creation() {
        let config = create_test_config();
        let manager = CacheManager::new(config).unwrap();
        
        assert!(manager.is_enabled());
        assert_eq!(manager.stats().entries, 0);
    }

    #[test]
    fn test_store_and_get() {
        let config = create_test_config();
        let manager = CacheManager::new(config).unwrap();
        
        let key = create_test_key("/test");
        let headers = vec![
            (b"content-type".to_vec().into_boxed_slice(), 
             b"text/plain".to_vec().into_boxed_slice()),
        ];
        
        let stored = manager.store(key.clone(), 200, headers, b"test data".to_vec());
        assert!(stored);
        
        let entry = manager.get(&key);
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().status_code, 200);
    }

    #[test]
    fn test_store_non_cacheable_status() {
        let config = create_test_config();
        let manager = CacheManager::new(config).unwrap();
        
        let key = create_test_key("/test");
        let headers = vec![];
        
        // 201は通常キャッシュ対象外
        let stored = manager.store(key.clone(), 201, headers, b"test".to_vec());
        assert!(!stored);
    }

    #[test]
    fn test_store_no_store() {
        let config = create_test_config();
        let manager = CacheManager::new(config).unwrap();
        
        let key = create_test_key("/test");
        let headers = vec![
            (b"cache-control".to_vec().into_boxed_slice(), 
             b"no-store".to_vec().into_boxed_slice()),
        ];
        
        let stored = manager.store(key.clone(), 200, headers, b"test".to_vec());
        assert!(!stored);
    }

    #[test]
    fn test_invalidate() {
        let config = create_test_config();
        let manager = CacheManager::new(config).unwrap();
        
        let key = create_test_key("/test");
        let headers = vec![];
        
        manager.store(key.clone(), 200, headers, b"test".to_vec());
        assert!(manager.get(&key).is_some());
        
        manager.invalidate(&key);
        assert!(manager.get(&key).is_none());
    }

    #[test]
    fn test_with_disk_cache() {
        let dir = tempdir().unwrap();
        let config = CacheConfig {
            enabled: true,
            max_memory_size: 100, // 小さくしてディスクに書き込ませる
            memory_threshold: 10,
            disk_path: Some(dir.path().to_path_buf()),
            max_disk_size: 1024 * 1024,
            ..Default::default()
        };
        
        let manager = CacheManager::new(config).unwrap();
        
        let key = create_test_key("/large");
        let headers = vec![];
        let large_body = vec![0u8; 1000]; // memory_thresholdより大きい
        
        let stored = manager.store(key.clone(), 200, headers, large_body);
        assert!(stored);
        
        let entry = manager.get(&key);
        assert!(entry.is_some());
    }

    #[test]
    fn test_stats() {
        let config = create_test_config();
        let manager = CacheManager::new(config).unwrap();
        
        let key = create_test_key("/test");
        let headers = vec![];
        manager.store(key.clone(), 200, headers, b"test".to_vec());
        
        // ヒット
        manager.get(&key);
        
        // ミス
        let missing_key = create_test_key("/missing");
        manager.get(&missing_key);
        
        let stats = manager.stats();
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert!((stats.hit_rate - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_is_request_cacheable() {
        let config = CacheConfig {
            enabled: true,
            bypass_patterns: vec!["/api/user/*".to_string()],
            ..Default::default()
        };
        let manager = CacheManager::new(config).unwrap();
        
        // GET は cacheable
        assert!(manager.is_request_cacheable(b"GET", "/api/products", &[]));
        
        // POST は cacheable でない
        assert!(!manager.is_request_cacheable(b"POST", "/api/products", &[]));
        
        // バイパスパターンにマッチ
        assert!(!manager.is_request_cacheable(b"GET", "/api/user/123", &[]));
        
        // no-cache ヘッダー
        let headers = vec![
            (b"cache-control".to_vec().into_boxed_slice(), 
             b"no-cache".to_vec().into_boxed_slice()),
        ];
        assert!(!manager.is_request_cacheable(b"GET", "/api/products", &headers));
    }
}

