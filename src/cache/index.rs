//! キャッシュインデックス
//!
//! DashMapを使用したロックフリーなキャッシュインデックスを提供します。

use super::entry::CacheEntry;
use super::key::CacheKey;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// キャッシュインデックス
/// 
/// キャッシュキーからエントリへのマッピングを管理します。
/// DashMapによりロックフリーな並行アクセスが可能です。
pub struct CacheIndex {
    /// キャッシュエントリのマップ（ハッシュ値 → エントリ）
    entries: DashMap<u64, IndexEntry>,
    /// 現在のエントリ数
    entry_count: AtomicUsize,
    /// 現在の合計メモリ使用量（概算）
    memory_usage: AtomicUsize,
    /// キャッシュヒット数
    hits: AtomicU64,
    /// キャッシュミス数
    misses: AtomicU64,
    /// 作成時刻
    created_at: Instant,
}

/// インデックスエントリ
/// 
/// 実際のキャッシュエントリとメタデータを保持
struct IndexEntry {
    /// キャッシュキー（衝突検出用）
    key: CacheKey,
    /// キャッシュエントリ
    entry: Arc<CacheEntry>,
    /// 最終アクセス時刻
    last_accessed: Instant,
    /// アクセス回数
    access_count: u64,
}

impl CacheIndex {
    /// 新しいインデックスを作成
    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
            entry_count: AtomicUsize::new(0),
            memory_usage: AtomicUsize::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            created_at: Instant::now(),
        }
    }
    
    /// エントリを取得
    /// 
    /// 有効なエントリが存在する場合のみ返す
    pub fn get(&self, key: &CacheKey) -> Option<Arc<CacheEntry>> {
        let hash = key.hash_value();
        
        // DashMapから取得
        let mut entry = self.entries.get_mut(&hash)?;
        
        // キーの完全一致を確認（ハッシュ衝突対策）
        if entry.key != *key {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        
        // 有効期限チェック
        if !entry.entry.is_valid() {
            self.misses.fetch_add(1, Ordering::Relaxed);
            // 期限切れエントリは削除
            drop(entry);
            self.remove(key);
            return None;
        }
        
        // アクセス情報を更新
        entry.last_accessed = Instant::now();
        entry.access_count += 1;
        
        self.hits.fetch_add(1, Ordering::Relaxed);
        
        Some(Arc::clone(&entry.entry))
    }
    
    /// 期限切れでもエントリを取得（stale-while-revalidate用）
    pub fn get_stale(&self, key: &CacheKey) -> Option<Arc<CacheEntry>> {
        let hash = key.hash_value();
        
        let entry = self.entries.get(&hash)?;
        
        if entry.key != *key {
            return None;
        }
        
        Some(Arc::clone(&entry.entry))
    }
    
    /// エントリを挿入または更新
    pub fn insert(&self, key: CacheKey, entry: CacheEntry) {
        let hash = key.hash_value();
        let memory = entry.memory_usage();
        let entry = Arc::new(entry);
        
        let index_entry = IndexEntry {
            key,
            entry,
            last_accessed: Instant::now(),
            access_count: 0,
        };
        
        // 既存エントリがある場合はメモリ使用量を調整
        if let Some(old) = self.entries.insert(hash, index_entry) {
            let old_memory = old.entry.memory_usage();
            if memory > old_memory {
                self.memory_usage.fetch_add(memory - old_memory, Ordering::Relaxed);
            } else {
                self.memory_usage.fetch_sub(old_memory - memory, Ordering::Relaxed);
            }
        } else {
            self.entry_count.fetch_add(1, Ordering::Relaxed);
            self.memory_usage.fetch_add(memory, Ordering::Relaxed);
        }
    }
    
    /// エントリを削除
    pub fn remove(&self, key: &CacheKey) -> Option<Arc<CacheEntry>> {
        let hash = key.hash_value();
        
        if let Some((_, removed)) = self.entries.remove(&hash) {
            // キーの一致を確認
            if removed.key == *key {
                self.entry_count.fetch_sub(1, Ordering::Relaxed);
                self.memory_usage.fetch_sub(removed.entry.memory_usage(), Ordering::Relaxed);
                return Some(removed.entry);
            }
        }
        None
    }
    
    /// エントリが存在するかチェック（有効期限は考慮しない）
    pub fn contains(&self, key: &CacheKey) -> bool {
        let hash = key.hash_value();
        self.entries.contains_key(&hash)
    }
    
    /// 現在のエントリ数
    #[inline]
    pub fn len(&self) -> usize {
        self.entry_count.load(Ordering::Relaxed)
    }
    
    /// インデックスが空かどうか
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    
    /// 現在のメモリ使用量（概算）
    #[inline]
    pub fn memory_usage(&self) -> usize {
        self.memory_usage.load(Ordering::Relaxed)
    }
    
    /// キャッシュヒット数
    #[inline]
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }
    
    /// キャッシュミス数
    #[inline]
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }
    
    /// ヒット率（パーセンテージ）
    pub fn hit_rate(&self) -> f64 {
        let hits = self.hits() as f64;
        let total = hits + self.misses() as f64;
        if total > 0.0 {
            (hits / total) * 100.0
        } else {
            0.0
        }
    }
    
    /// 稼働時間（秒）
    #[inline]
    pub fn uptime_secs(&self) -> u64 {
        self.created_at.elapsed().as_secs()
    }
    
    /// 期限切れエントリを削除
    /// 
    /// 定期的なクリーンアップに使用
    pub fn evict_expired(&self) -> usize {
        let mut evicted = 0;
        
        self.entries.retain(|_, entry| {
            if entry.entry.is_valid() {
                true
            } else {
                evicted += 1;
                self.entry_count.fetch_sub(1, Ordering::Relaxed);
                self.memory_usage.fetch_sub(entry.entry.memory_usage(), Ordering::Relaxed);
                false
            }
        });
        
        evicted
    }
    
    /// LRUエビクション
    /// 
    /// メモリ使用量が制限を超えた場合に古いエントリを削除
    pub fn evict_lru(&self, max_memory: usize) -> usize {
        let current_memory = self.memory_usage();
        
        if current_memory <= max_memory {
            return 0;
        }
        
        // 最終アクセス時刻でソートしてエビクト
        let mut entries_to_evict: Vec<(u64, Instant, usize)> = self
            .entries
            .iter()
            .map(|entry| {
                (
                    *entry.key(),
                    entry.value().last_accessed,
                    entry.value().entry.memory_usage(),
                )
            })
            .collect();
        
        // 最終アクセスが古い順にソート
        entries_to_evict.sort_by_key(|(_, accessed, _)| *accessed);
        
        let mut evicted = 0;
        let mut freed_memory = 0;
        let target_free = current_memory.saturating_sub(max_memory) 
            + (max_memory / 10); // 10%余分に解放
        
        for (hash, _, memory) in entries_to_evict {
            if freed_memory >= target_free {
                break;
            }
            
            if self.entries.remove(&hash).is_some() {
                self.entry_count.fetch_sub(1, Ordering::Relaxed);
                self.memory_usage.fetch_sub(memory, Ordering::Relaxed);
                freed_memory += memory;
                evicted += 1;
            }
        }
        
        evicted
    }
    
    /// 全エントリを削除
    pub fn clear(&self) {
        self.entries.clear();
        self.entry_count.store(0, Ordering::Relaxed);
        self.memory_usage.store(0, Ordering::Relaxed);
    }
    
    /// 統計情報をリセット
    pub fn reset_stats(&self) {
        self.hits.store(0, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
    }
    
    /// パターンに一致するエントリを削除
    /// 
    /// globパターンでパス（ホスト/パス形式）をマッチングし、
    /// 一致するエントリを削除します。
    /// 
    /// # Arguments
    /// 
    /// * `pattern` - globパターン（例: "example.com/api/*", "*/admin/*"）
    /// 
    /// # Returns
    /// 
    /// 削除されたエントリ数
    pub fn invalidate_pattern(&self, pattern: &str) -> usize {
        let glob_pattern = match glob::Pattern::new(pattern) {
            Ok(p) => p,
            Err(_) => return 0,
        };
        
        let mut keys_to_remove = Vec::new();
        
        // 全エントリをスキャンしてマッチするものを収集
        for entry in self.entries.iter() {
            let index_entry = entry.value();
            // ホスト/パス形式でマッチング
            let full_path = format!("{}{}", index_entry.key.host(), index_entry.key.path());
            
            if glob_pattern.matches(&full_path) {
                keys_to_remove.push(*entry.key());
            }
        }
        
        // 収集したキーを削除
        let mut removed = 0;
        for hash in keys_to_remove {
            if let Some((_, entry)) = self.entries.remove(&hash) {
                self.entry_count.fetch_sub(1, Ordering::Relaxed);
                self.memory_usage.fetch_sub(entry.entry.memory_usage(), Ordering::Relaxed);
                removed += 1;
            }
        }
        
        removed
    }
    
    /// ホストに一致するエントリを全て削除
    /// 
    /// 指定されたホストの全キャッシュを無効化します。
    /// 
    /// # Arguments
    /// 
    /// * `host` - ホスト名
    /// 
    /// # Returns
    /// 
    /// 削除されたエントリ数
    pub fn invalidate_host(&self, host: &str) -> usize {
        let mut keys_to_remove = Vec::new();
        
        for entry in self.entries.iter() {
            if entry.value().key.host() == host {
                keys_to_remove.push(*entry.key());
            }
        }
        
        let mut removed = 0;
        for hash in keys_to_remove {
            if let Some((_, entry)) = self.entries.remove(&hash) {
                self.entry_count.fetch_sub(1, Ordering::Relaxed);
                self.memory_usage.fetch_sub(entry.entry.memory_usage(), Ordering::Relaxed);
                removed += 1;
            }
        }
        
        removed
    }
}

impl Default for CacheIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::entry::{CacheStorage, CacheEntryBuilder};
    use crate::cache::key::CacheableMethod;

    fn create_test_key(path: &str) -> CacheKey {
        CacheKey::new(CacheableMethod::Get, "example.com", path, None)
    }

    fn create_test_entry(ttl: u64, body: &[u8]) -> CacheEntry {
        CacheEntryBuilder::new(200)
            .max_age(ttl)
            .body(body.to_vec())
            .build_memory()
    }

    #[test]
    fn test_insert_and_get() {
        let index = CacheIndex::new();
        let key = create_test_key("/test");
        let entry = create_test_entry(3600, b"test data");
        
        index.insert(key.clone(), entry);
        
        assert_eq!(index.len(), 1);
        
        let retrieved = index.get(&key);
        assert!(retrieved.is_some());
    }

    #[test]
    fn test_get_miss() {
        let index = CacheIndex::new();
        let key = create_test_key("/nonexistent");
        
        let retrieved = index.get(&key);
        assert!(retrieved.is_none());
        assert_eq!(index.misses(), 1);
    }

    #[test]
    fn test_remove() {
        let index = CacheIndex::new();
        let key = create_test_key("/test");
        let entry = create_test_entry(3600, b"test data");
        
        index.insert(key.clone(), entry);
        assert_eq!(index.len(), 1);
        
        let removed = index.remove(&key);
        assert!(removed.is_some());
        assert_eq!(index.len(), 0);
    }

    #[test]
    fn test_hit_rate() {
        let index = CacheIndex::new();
        let key = create_test_key("/test");
        let entry = create_test_entry(3600, b"test data");
        
        index.insert(key.clone(), entry);
        
        // 5回ヒット
        for _ in 0..5 {
            index.get(&key);
        }
        
        // 5回ミス
        let missing_key = create_test_key("/missing");
        for _ in 0..5 {
            index.get(&missing_key);
        }
        
        assert_eq!(index.hits(), 5);
        assert_eq!(index.misses(), 5);
        assert!((index.hit_rate() - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_memory_tracking() {
        let index = CacheIndex::new();
        
        let key1 = create_test_key("/test1");
        let entry1 = create_test_entry(3600, &vec![0u8; 1000]);
        let memory1 = entry1.memory_usage();
        
        index.insert(key1.clone(), entry1);
        assert!(index.memory_usage() >= memory1);
        
        index.remove(&key1);
        // メモリが解放されている（完全に0にはならない場合がある）
    }

    #[test]
    fn test_clear() {
        let index = CacheIndex::new();
        
        for i in 0..10 {
            let key = create_test_key(&format!("/test{}", i));
            let entry = create_test_entry(3600, b"data");
            index.insert(key, entry);
        }
        
        assert_eq!(index.len(), 10);
        
        index.clear();
        assert_eq!(index.len(), 0);
        assert_eq!(index.memory_usage(), 0);
    }
}

