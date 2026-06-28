//! メモリキャッシュ
//!
//! 小さいレスポンス用の高速インメモリキャッシュを提供します。
//! LRUアルゴリズムによるエビクションを実装しています。

use super::entry::{CacheEntry, CacheEntryBuilder};
use super::key::CacheKey;
use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// シャード数（2 の冪）。キャッシュキーのハッシュでシャードに分散し、従来の単一
/// グローバル `Mutex<LruCache>` による「全スレッド直列化」を排除する。各シャードの
/// クリティカルセクションは O(1) の LRU 操作のみで、thread-per-core 環境での
/// ロック競合を 1/SHARD_COUNT に抑える（DashMap 等の内部シャーディングと同方針。
/// LRU は本質的に共有状態の更新を要するため完全ロックフリー化は非現実的で、
/// 短時間クリティカルセクションのシャード化が高性能設計の定石）。
const SHARD_COUNT: usize = 16;

/// メモリキャッシュ
///
/// シャード化された LRU キャッシュによる高速なインメモリストレージ。
/// メモリ使用量は全シャード横断のロックフリー `AtomicUsize` で管理する。
pub struct MemoryCache {
    /// シャードごとの LRU キャッシュ
    shards: Box<[Mutex<LruCache<u64, MemoryCacheEntry>>]>,
    /// 最大メモリ使用量（全シャード合計）
    max_memory: usize,
    /// 現在のメモリ使用量（概算、ロックフリー）
    current_memory: AtomicUsize,
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
    ///
    /// キャッシュエントリの挿入時刻を記録。以下の用途で使用可能：
    /// - キャッシュ統計（平均TTL、ヒット率分析）
    /// - デバッグ情報（エントリの生存時間）
    /// - TTLベースのエビクション（将来実装）
    #[allow(dead_code)]
    inserted_at: Instant,
}

impl MemoryCache {
    /// 新しいメモリキャッシュを作成
    ///
    /// # Arguments
    ///
    /// * `max_entries` - 最大エントリ数（全シャード合計の目安）
    /// * `max_memory` - 最大メモリ使用量（バイト）
    pub fn new(max_entries: usize, max_memory: usize) -> Self {
        // 1 シャードあたりの容量（最低 1 を保証）。全体で約 max_entries。
        let per_shard = (max_entries / SHARD_COUNT).max(1);
        let capacity = NonZeroUsize::new(per_shard).unwrap_or(NonZeroUsize::new(1).unwrap());
        let shards = (0..SHARD_COUNT)
            .map(|_| Mutex::new(LruCache::new(capacity)))
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Self {
            shards,
            max_memory,
            current_memory: AtomicUsize::new(0),
            created_at: Instant::now(),
        }
    }

    /// ハッシュからシャードを選択する（SHARD_COUNT は 2 の冪なので下位ビットマスク）
    #[inline]
    fn shard(&self, hash: u64) -> &Mutex<LruCache<u64, MemoryCacheEntry>> {
        &self.shards[(hash as usize) & (SHARD_COUNT - 1)]
    }

    /// エントリを取得
    pub fn get(&self, key: &CacheKey) -> Option<Arc<CacheEntry>> {
        let hash = key.hash_value();
        let mut cache = self.shard(hash).lock().ok()?;

        // LRU の順序を更新しつつ取得
        match cache.get(&hash) {
            // キー完全一致かつ有効 → 参照カウント +1 のみで返す（ゼロコピー）
            Some(entry) if entry.key == *key => {
                if entry.entry.is_valid() {
                    return Some(Arc::clone(&entry.entry));
                }
                // 期限切れ → 下で除去
            }
            // ハッシュ衝突（別キー）またはミス
            _ => return None,
        }

        // 期限切れエントリをこのシャードから除去し、メモリを返却
        if let Some(expired) = cache.pop(&hash) {
            self.current_memory
                .fetch_sub(expired.entry.memory_usage(), Ordering::Relaxed);
        }
        None
    }

    /// エントリを挿入
    ///
    /// メモリ制限に達した場合は同一シャードの古いエントリを自動的に削除します。
    pub fn insert(&self, key: CacheKey, entry: CacheEntry) -> bool {
        let memory = entry.memory_usage();

        // 単一エントリが全体制限を超える場合は拒否
        if memory > self.max_memory {
            return false;
        }

        let hash = key.hash_value();
        let cache_entry = MemoryCacheEntry {
            key,
            entry: Arc::new(entry),
            inserted_at: Instant::now(),
        };

        let mut cache = match self.shard(hash).lock() {
            Ok(c) => c,
            Err(_) => return false,
        };

        // 全体メモリが上限を超える間、このシャードの LRU を末尾から退避する。
        while self.current_memory.load(Ordering::Relaxed) + memory > self.max_memory {
            match cache.pop_lru() {
                Some((_, evicted)) => {
                    self.current_memory
                        .fetch_sub(evicted.entry.memory_usage(), Ordering::Relaxed);
                }
                None => break, // このシャードは空。これ以上ローカルでは退避できない。
            }
        }

        // ハードキャップ: ローカル退避後もなお全体上限を超える（メモリは他シャードが
        // 保持していてクロスシャード退避はロック順序の都合で行わない）場合は、この
        // エントリのキャッシュを諦めてメモリ上限を厳守する。
        if self.current_memory.load(Ordering::Relaxed) + memory > self.max_memory {
            return false;
        }

        // 容量上限に達していて新規キーなら、先に LRU を 1 つ退避してメモリ計上を精算する
        // （lru::put は容量超過で別キーを暗黙に捨てるが、その分を current_memory に
        // 反映できないため、ここで明示的に pop してカウンタを正確に保つ）。
        if cache.len() >= cache.cap().get() && cache.peek(&hash).is_none() {
            if let Some((_, evicted)) = cache.pop_lru() {
                self.current_memory
                    .fetch_sub(evicted.entry.memory_usage(), Ordering::Relaxed);
            }
        }

        self.current_memory.fetch_add(memory, Ordering::Relaxed);

        // 既存エントリ（同一キー）を置き換えた場合は旧エントリ分を差し引く
        if let Some(old) = cache.put(hash, cache_entry) {
            self.current_memory
                .fetch_sub(old.entry.memory_usage(), Ordering::Relaxed);
        }

        true
    }

    /// エントリを削除
    pub fn remove(&self, key: &CacheKey) -> Option<Arc<CacheEntry>> {
        let hash = key.hash_value();
        let mut cache = self.shard(hash).lock().ok()?;

        if let Some(entry) = cache.pop(&hash) {
            if entry.key == *key {
                self.current_memory
                    .fetch_sub(entry.entry.memory_usage(), Ordering::Relaxed);
                return Some(entry.entry);
            }
            // ハッシュ衝突の別キーだった → 取り除かずに戻す
            cache.put(hash, entry);
        }
        None
    }

    /// 現在のエントリ数（全シャード合計）
    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.lock().map(|c| c.len()).unwrap_or(0))
            .sum()
    }

    /// キャッシュが空かどうか
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 現在のメモリ使用量（ロックフリー）
    pub fn memory_usage(&self) -> usize {
        self.current_memory.load(Ordering::Relaxed)
    }

    /// 最大メモリ使用量
    pub fn max_memory(&self) -> usize {
        self.max_memory
    }

    /// 期限切れエントリを削除（全シャード）
    pub fn evict_expired(&self) -> usize {
        let mut evicted = 0;

        for shard in self.shards.iter() {
            let mut cache = match shard.lock() {
                Ok(c) => c,
                Err(_) => continue,
            };

            let keys_to_remove: Vec<u64> = cache
                .iter()
                .filter(|(_, entry)| !entry.entry.is_valid())
                .map(|(&hash, _)| hash)
                .collect();

            for hash in keys_to_remove {
                if let Some(entry) = cache.pop(&hash) {
                    self.current_memory
                        .fetch_sub(entry.entry.memory_usage(), Ordering::Relaxed);
                    evicted += 1;
                }
            }
        }

        evicted
    }

    /// 全エントリを削除
    pub fn clear(&self) {
        for shard in self.shards.iter() {
            if let Ok(mut cache) = shard.lock() {
                cache.clear();
            }
        }
        self.current_memory.store(0, Ordering::Relaxed);
    }

    /// 稼働時間（秒）
    pub fn uptime_secs(&self) -> u64 {
        self.created_at.elapsed().as_secs()
    }
}

/// ボディデータからCacheEntryを作成するヘルパー
///
/// テストコードや簡易的なエントリ作成に使用。
/// より柔軟な設定が必要な場合は`CacheEntryBuilder`を使用してください。
///
/// # 使用例
/// ```rust
/// let entry = create_memory_entry(200, headers, body, 3600);
/// cache.insert(key, entry);
/// ```
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
