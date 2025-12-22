//! メモリキャッシュ
//!
//! 小さいレスポンス用の高速インメモリキャッシュを提供します。
//! LRUアルゴリズムによるエビクションを実装しています。

use super::entry::{CacheEntry, CacheEntryBuilder};
use super::key::CacheKey;
use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// メモリキャッシュ
/// 
/// LRUキャッシュを使用した高速なインメモリストレージ。
/// スレッドセーフだがロックを使用するため、高並行性には`CacheIndex`を推奨。
pub struct MemoryCache {
    /// LRUキャッシュ（Mutexで保護）
    cache: Mutex<LruCache<u64, MemoryCacheEntry>>,
    /// 最大メモリ使用量
    max_memory: usize,
    /// 現在のメモリ使用量（概算）
    current_memory: Mutex<usize>,
    /// 作成時刻
    created_at: Instant,
}

/// メモリキャッシュエントリ
struct MemoryCacheEntry {
    /// キャッシュキー（衝突検出用）
    key: CacheKey,
    /// キャッシュエントリ
    entry: Arc<CacheEntry>,
    /// 挿入時刻
    #[allow(dead_code)]
    inserted_at: Instant,
}

impl MemoryCache {
    /// 新しいメモリキャッシュを作成
    /// 
    /// # Arguments
    /// 
    /// * `max_entries` - 最大エントリ数
    /// * `max_memory` - 最大メモリ使用量（バイト）
    pub fn new(max_entries: usize, max_memory: usize) -> Self {
        let capacity = NonZeroUsize::new(max_entries).unwrap_or(NonZeroUsize::new(1000).unwrap());
        
        Self {
            cache: Mutex::new(LruCache::new(capacity)),
            max_memory,
            current_memory: Mutex::new(0),
            created_at: Instant::now(),
        }
    }
    
    /// エントリを取得
    pub fn get(&self, key: &CacheKey) -> Option<Arc<CacheEntry>> {
        let hash = key.hash_value();
        let mut cache = self.cache.lock().ok()?;
        
        // LRUキャッシュから取得（アクセス順を更新）
        let entry = cache.get(&hash)?;
        
        // キーの完全一致を確認
        if entry.key != *key {
            return None;
        }
        
        // 有効期限チェック
        if !entry.entry.is_valid() {
            // 期限切れエントリを削除
            drop(cache);
            self.remove(key);
            return None;
        }
        
        Some(Arc::clone(&entry.entry))
    }
    
    /// エントリを挿入
    /// 
    /// メモリ制限に達した場合は古いエントリを自動的に削除します。
    pub fn insert(&self, key: CacheKey, entry: CacheEntry) -> bool {
        let hash = key.hash_value();
        let memory = entry.memory_usage();
        
        // メモリ制限チェック
        {
            let mut current = self.current_memory.lock().unwrap();
            
            // 単一エントリが制限を超える場合は拒否
            if memory > self.max_memory {
                return false;
            }
            
            // 空きを確保
            if *current + memory > self.max_memory {
                let mut cache = self.cache.lock().unwrap();
                
                while *current + memory > self.max_memory {
                    if let Some((_, evicted)) = cache.pop_lru() {
                        *current = current.saturating_sub(evicted.entry.memory_usage());
                    } else {
                        break;
                    }
                }
            }
            
            *current += memory;
        }
        
        let cache_entry = MemoryCacheEntry {
            key,
            entry: Arc::new(entry),
            inserted_at: Instant::now(),
        };
        
        let mut cache = self.cache.lock().unwrap();
        
        // 既存エントリがある場合はメモリ使用量を調整
        if let Some(old) = cache.put(hash, cache_entry) {
            let mut current = self.current_memory.lock().unwrap();
            *current = current.saturating_sub(old.entry.memory_usage());
        }
        
        true
    }
    
    /// エントリを削除
    pub fn remove(&self, key: &CacheKey) -> Option<Arc<CacheEntry>> {
        let hash = key.hash_value();
        let mut cache = self.cache.lock().ok()?;
        
        if let Some(entry) = cache.pop(&hash) {
            if entry.key == *key {
                let mut current = self.current_memory.lock().unwrap();
                *current = current.saturating_sub(entry.entry.memory_usage());
                return Some(entry.entry);
            }
        }
        None
    }
    
    /// 現在のエントリ数
    pub fn len(&self) -> usize {
        self.cache.lock().map(|c| c.len()).unwrap_or(0)
    }
    
    /// キャッシュが空かどうか
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    
    /// 現在のメモリ使用量
    pub fn memory_usage(&self) -> usize {
        *self.current_memory.lock().unwrap()
    }
    
    /// 最大メモリ使用量
    pub fn max_memory(&self) -> usize {
        self.max_memory
    }
    
    /// 期限切れエントリを削除
    pub fn evict_expired(&self) -> usize {
        let mut cache = match self.cache.lock() {
            Ok(c) => c,
            Err(_) => return 0,
        };
        
        let mut evicted = 0;
        let mut keys_to_remove = Vec::new();
        
        for (&hash, entry) in cache.iter() {
            if !entry.entry.is_valid() {
                keys_to_remove.push(hash);
            }
        }
        
        let mut current = self.current_memory.lock().unwrap();
        
        for hash in keys_to_remove {
            if let Some(entry) = cache.pop(&hash) {
                *current = current.saturating_sub(entry.entry.memory_usage());
                evicted += 1;
            }
        }
        
        evicted
    }
    
    /// 全エントリを削除
    pub fn clear(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.clear();
        }
        if let Ok(mut current) = self.current_memory.lock() {
            *current = 0;
        }
    }
    
    /// 稼働時間（秒）
    pub fn uptime_secs(&self) -> u64 {
        self.created_at.elapsed().as_secs()
    }
}

/// ボディデータからCacheEntryを作成するヘルパー
#[allow(dead_code)]
pub fn create_memory_entry(
    status_code: u16,
    headers: Vec<(Box<[u8]>, Box<[u8]>)>,
    body: Vec<u8>,
    ttl_secs: u64,
) -> CacheEntry {
    CacheEntryBuilder::new(status_code)
        .headers(headers)
        .body(body)
        .max_age(ttl_secs)
        .build_memory()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::key::CacheableMethod;

    fn create_test_key(path: &str) -> CacheKey {
        CacheKey::new(CacheableMethod::Get, "example.com", path, None)
    }

    fn create_test_entry(size: usize, ttl: u64) -> CacheEntry {
        CacheEntryBuilder::new(200)
            .max_age(ttl)
            .body(vec![0u8; size])
            .build_memory()
    }

    #[test]
    fn test_insert_and_get() {
        let cache = MemoryCache::new(100, 1024 * 1024);
        let key = create_test_key("/test");
        let entry = create_test_entry(100, 3600);
        
        assert!(cache.insert(key.clone(), entry));
        assert_eq!(cache.len(), 1);
        
        let retrieved = cache.get(&key);
        assert!(retrieved.is_some());
    }

    #[test]
    fn test_memory_limit() {
        let cache = MemoryCache::new(100, 1000);
        
        // 大きすぎるエントリは拒否
        let key = create_test_key("/large");
        let entry = create_test_entry(2000, 3600);
        assert!(!cache.insert(key, entry));
    }

    #[test]
    fn test_lru_eviction() {
        // 非常に小さいメモリ制限
        let cache = MemoryCache::new(10, 500);
        
        // 小さいエントリを挿入
        for i in 0..5 {
            let key = create_test_key(&format!("/test{}", i));
            let entry = create_test_entry(80, 3600);
            cache.insert(key, entry);
        }
        
        // LRUエビクションが発生しているはず
        assert!(cache.memory_usage() <= 500);
    }

    #[test]
    fn test_remove() {
        let cache = MemoryCache::new(100, 1024 * 1024);
        let key = create_test_key("/test");
        let entry = create_test_entry(100, 3600);
        
        cache.insert(key.clone(), entry);
        assert_eq!(cache.len(), 1);
        
        let removed = cache.remove(&key);
        assert!(removed.is_some());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_clear() {
        let cache = MemoryCache::new(100, 1024 * 1024);
        
        for i in 0..10 {
            let key = create_test_key(&format!("/test{}", i));
            let entry = create_test_entry(100, 3600);
            cache.insert(key, entry);
        }
        
        assert_eq!(cache.len(), 10);
        cache.clear();
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.memory_usage(), 0);
    }
}

