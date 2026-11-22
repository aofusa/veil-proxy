//! キャッシュマネージャー
//!
//! メモリキャッシュとディスクキャッシュを統合管理するマネージャーを提供します。

use super::config::{CacheConfig, StorageType};
use super::disk::{DiskCache, DiskCacheConfig};
use super::entry::CacheEntry;
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
                use_async_io: config.use_async_io,
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
    /// レスポンスがキャッシュ可能な場合のみ保存します（判定はこのマネージャーの設定。ルートの設定で判定するのは
    /// [`CacheManager::store_async`]）。ディスクへ置く本文は呼び出したスレッドで書く。
    pub fn store(
        &self,
        key: CacheKey,
        status_code: u16,
        headers: Vec<(Box<[u8]>, Box<[u8]>)>,
        body: Vec<u8>,
    ) -> bool {
        self.store_with_vary(key, status_code, headers, body, None)
    }

    /// Varyヘッダー情報付きでレスポンスをキャッシュに保存
    ///
    /// レスポンスがキャッシュ可能な場合のみ保存します（[`CacheManager::store`] と同じくこのマネージャーの設定で判定する）。
    /// vary_headers には Vary ヘッダーで指定されたヘッダー名のリストを指定します。
    pub fn store_with_vary(
        &self,
        key: CacheKey,
        status_code: u16,
        headers: Vec<(Box<[u8]>, Box<[u8]>)>,
        body: Vec<u8>,
        vary_headers: Option<Vec<String>>,
    ) -> bool {
        let Some(prepared) = self.prepare(&self.config, status_code, headers, body, vary_headers)
        else {
            return false;
        };
        if prepared.disk {
            self.insert_disk(key, prepared)
        } else {
            self.insert_memory(key, prepared);
            true
        }
    }

    /// データプレーンから応答を保存する。保存するか（状態・`Cache-Control`・`Vary`）は `route`（ルートの
    /// `[route.cache]`）の設定で決め、ディスクへ置く本文は `offload` で書く（ワーカーのスレッドで同期の書き込みを
    /// しない）。保存したら保存先（`"memory"`・`"disk"`。`veil_proxy_cache_stores_total` の `storage`）。
    ///
    /// B-491: 以前は全体のマネージャーの既定（`cacheable_statuses = [200, 301, 302, 304]`・`default_ttl_secs = 300`・
    /// 空の `key_headers`）で判定し、ルートの設定を見ていなかった。
    pub async fn store_async(
        self: Arc<Self>,
        route: &CacheConfig,
        key: CacheKey,
        status_code: u16,
        headers: Vec<(Box<[u8]>, Box<[u8]>)>,
        body: Vec<u8>,
        vary_headers: Option<Vec<String>>,
    ) -> Option<&'static str> {
        let prepared = self.prepare(route, status_code, headers, body, vary_headers)?;
        if !prepared.disk {
            self.insert_memory(key, prepared);
            return Some("memory");
        }
        crate::runtime::offload::offload(move || self.insert_disk(key, prepared))
            .await
            .then_some("disk")
    }

    /// 保存してよい応答かを `policy` で判定し、保存する形に整える（[`normalize_for_store`]）。
    fn prepare(
        &self,
        policy: &CacheConfig,
        status_code: u16,
        headers: Vec<(Box<[u8]>, Box<[u8]>)>,
        body: Vec<u8>,
        vary_headers: Option<Vec<String>>,
    ) -> Option<Prepared> {
        // ステータスコードチェック
        if !policy.is_cacheable_status(status_code) {
            return None;
        }

        // Cache-Controlチェック
        let ttl = CachePolicy::check_response(
            status_code,
            &headers,
            &policy.cacheable_statuses,
            policy.default_ttl_secs,
        )?;

        // Set-Cookie を持つ応答は利用者ごとのものなので共有キャッシュに置かない（F-236 の F7）
        if headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(b"set-cookie"))
        {
            return None;
        }

        // Varyチェック（Vary: * はキャッシュ不可）
        use super::policy::VaryResult;
        let vary_result = CachePolicy::parse_vary_ex(&headers);

        if policy.respect_vary {
            // Vary: * の場合はキャッシュしない
            if !vary_result.is_cacheable() {
                return None;
            }
            // キャッシュのキーは `key_headers` の値だけで作る。Vary が挙げるヘッダがキーに無いと、
            // 別の値の要求（例: Accept-Encoding の違い）へこの応答を返してしまうので保存しない（F-236 の F7）
            if let VaryResult::Headers(ref names) = vary_result {
                let covered = names
                    .iter()
                    .all(|n| policy.key_headers.iter().any(|k| k.eq_ignore_ascii_case(n)));
                if !covered {
                    return None;
                }
            }
        }

        // レスポンスから取得したVaryヘッダー、または渡されたVaryヘッダーを使用
        let vary = vary_headers.or(match vary_result {
            VaryResult::Headers(h) => Some(h),
            VaryResult::NotPresent => None,
            VaryResult::Uncacheable => None,
        });

        let (headers, body) = normalize_for_store(headers, body)?;
        // ストレージ選択（ディスクはこのマネージャーにあるときだけ。無ければメモリに）
        let disk =
            self.disk.is_some() && self.config.select_storage(body.len()) == StorageType::Disk;
        Some(Prepared {
            status_code,
            headers,
            body,
            ttl,
            vary,
            disk,
            stale_retention: policy.stale_retention_secs(),
        })
    }

    fn insert_memory(&self, key: CacheKey, p: Prepared) {
        use super::entry::CacheStorage;
        let entry = CacheEntry::with_vary(
            p.status_code,
            p.headers,
            CacheStorage::Memory(p.body.into()),
            p.ttl,
            p.vary,
        )
        .with_stale_retention(p.stale_retention);
        self.index.insert(key, entry);
    }

    /// 本文をディスクへ書いてから索引に載せる（同期の書き込み。データプレーンからは `offload` の中で呼ぶ）
    fn insert_disk(&self, key: CacheKey, p: Prepared) -> bool {
        use super::entry::CacheStorage;
        let Some(ref disk) = self.disk else {
            return false;
        };
        let path = match disk.write_sync(&key, &p.body) {
            Ok(path) => path,
            Err(_) => return false,
        };
        let size = p.body.len() as u64;
        let entry = CacheEntry::with_vary(
            p.status_code,
            p.headers,
            CacheStorage::Disk { path, size },
            p.ttl,
            p.vary,
        )
        .with_stale_retention(p.stale_retention);
        self.index.insert(key, entry);
        true
    }

    /// エントリを削除
    // 理由付き allow: キャッシュ無効化（purge 管理 API / 失効時）のコールドパス。
    #[allow(clippy::disallowed_methods)]
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
        // ディスクキャッシュのクリーンアップは別途実行される
        // (エントリ削除時にパスが失われるため、ここでは行わない)

        self.index.invalidate_pattern(pattern)
    }

    /// ホストの全エントリを削除
    ///
    /// 指定されたホストの全キャッシュを無効化します。
    pub fn invalidate_host(&self, host: &str) -> usize {
        self.index.invalidate_host(host)
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

    // ====================
    // F-20: キャッシュ Purge 機能
    // ====================

    /// キャッシュエントリを即座に無効化（完全一致）
    ///
    /// # Returns
    /// 該当エントリが存在し削除された場合 true
    pub fn purge_exact(&self, key: &CacheKey) -> bool {
        let existed = self.index.contains(key);
        self.invalidate(key);
        if existed {
            crate::metrics::record_cache_eviction("purge", 1);
        }
        existed
    }

    /// プレフィックスでキャッシュを無効化（F-20）
    ///
    /// パス（または host+path）が `prefix` で始まるエントリを削除する。
    ///
    /// # Returns
    /// 削除されたエントリ数
    pub fn purge_by_prefix(&self, prefix: &str) -> usize {
        let n = self.index.invalidate_prefix(prefix);
        if n > 0 {
            crate::metrics::record_cache_eviction("purge", n);
        }
        n
    }

    /// ワイルドカードパターン（glob）でキャッシュを無効化（F-20）
    ///
    /// 例: `"/static/*.css"`, `"example.com/api/*"`
    ///
    /// # Returns
    /// 削除されたエントリ数
    pub fn purge_by_pattern(&self, pattern: &str) -> usize {
        let n = self.index.invalidate_pattern(pattern);
        if n > 0 {
            crate::metrics::record_cache_eviction("purge", n);
        }
        n
    }

    /// 全キャッシュをクリア（F-20）
    ///
    /// # Returns
    /// 削除されたエントリ数
    pub fn purge_all(&self) -> usize {
        let n = self.index.len();
        self.index.clear();
        self.memory.clear();
        if let Some(ref disk) = self.disk {
            let _ = disk.clear();
        }
        if n > 0 {
            crate::metrics::record_cache_eviction("purge", n);
        }
        n
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

/// 保存すると決めた応答（[`CacheManager::prepare`] の結果）
struct Prepared {
    status_code: u16,
    headers: Vec<(Box<[u8]>, Box<[u8]>)>,
    body: Vec<u8>,
    ttl: u64,
    vary: Option<Vec<String>>,
    /// 本文をディスクへ置く
    disk: bool,
    /// 期限が切れてから残しておく期間（秒。B-492）
    stale_retention: u64,
}

/// 保存する応答を、どのプロトコルのクライアントへもそのまま返せる形にする（F-257）。
///
/// hop-by-hop のヘッダ（`Connection` が挙げるものを含む）と `Content-Length` を除いて、本文の長さの
/// `content-length` を足す。`Transfer-Encoding: chunked` の本文（HTTP/1.1 の上流から読んだままの本文）は枠を解く。
/// 枠が壊れている・終端が無い・chunked 以外の転送符号・本文が空の応答は保存しない（`None`）。
///
/// 以前は HTTP/1.1 の上流の応答のヘッダ（`Transfer-Encoding`・`Connection` を含む）と chunked の枠をそのまま保存し、
/// 当たったら HTTP/1.1 のクライアントへそのまま書いていた（`Connection` が 2 行になり得た）。
fn normalize_for_store(
    headers: Vec<(Box<[u8]>, Box<[u8]>)>,
    body: Vec<u8>,
) -> Option<(Vec<(Box<[u8]>, Box<[u8]>)>, Vec<u8>)> {
    let mut chunked = false;
    for (name, value) in &headers {
        if name.eq_ignore_ascii_case(b"transfer-encoding") {
            for coding in value.split(|&b| b == b',') {
                let coding = coding.trim_ascii();
                if coding.is_empty() {
                    continue;
                }
                // 解けるのは 1 回の chunked だけ（ほかの転送符号は保存しない）
                if chunked || !coding.eq_ignore_ascii_case(b"chunked") {
                    return None;
                }
                chunked = true;
            }
        }
    }
    let body = if chunked {
        crate::http_utils::decode_complete_chunked(&body)?
    } else {
        body
    };
    if body.is_empty() {
        return None;
    }
    // 除く行の印を先に作る（`Connection` が挙げる名前は全部の行を見て決める。行の写しは作らない）
    let drop: Vec<bool> = headers
        .iter()
        .map(|(name, _)| {
            crate::http_utils::is_hop_by_hop_header(name)
                || name.eq_ignore_ascii_case(b"content-length")
                || crate::http_utils::connection_lists(&headers, name)
        })
        .collect();
    let mut drop = drop.into_iter();
    let mut kept = headers;
    // `retain` は先頭から順に 1 回ずつ見る
    kept.retain(|_| !drop.next().unwrap_or(false));
    let mut len = itoa::Buffer::new();
    kept.push((
        Box::from(&b"content-length"[..]),
        Box::from(len.format(body.len()).as_bytes()),
    ));
    Some((kept, body))
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

    /// F-236 の F7: Set-Cookie の応答と、キーに無いヘッダで Vary する応答は共有キャッシュに置かない
    #[test]
    fn test_store_rejects_per_user_responses() {
        let h = |n: &str, v: &str| {
            (
                n.as_bytes().to_vec().into_boxed_slice(),
                v.as_bytes().to_vec().into_boxed_slice(),
            )
        };
        let manager = CacheManager::new(create_test_config()).unwrap();
        assert!(!manager.store(
            create_test_key("/c"),
            200,
            vec![h("set-cookie", "sid=1")],
            b"x".to_vec()
        ));
        assert!(!manager.store(
            create_test_key("/v"),
            200,
            vec![h("vary", "Accept-Encoding")],
            b"x".to_vec()
        ));
        let mut cfg = create_test_config();
        cfg.key_headers = vec!["accept-encoding".to_string()];
        let manager = CacheManager::new(cfg).unwrap();
        assert!(manager.store(
            create_test_key("/v"),
            200,
            vec![h("vary", "Accept-Encoding")],
            b"x".to_vec()
        ));
        assert!(!manager.store(
            create_test_key("/w"),
            200,
            vec![h("vary", "Accept-Encoding, Cookie")],
            b"x".to_vec()
        ));
    }

    #[test]
    fn test_store_and_get() {
        let config = create_test_config();
        let manager = CacheManager::new(config).unwrap();

        let key = create_test_key("/test");
        let headers = vec![(
            b"content-type".to_vec().into_boxed_slice(),
            b"text/plain".to_vec().into_boxed_slice(),
        )];

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
        let headers = vec![(
            b"cache-control".to_vec().into_boxed_slice(),
            b"no-store".to_vec().into_boxed_slice(),
        )];

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
        let headers = vec![(
            b"cache-control".to_vec().into_boxed_slice(),
            b"no-cache".to_vec().into_boxed_slice(),
        )];
        assert!(!manager.is_request_cacheable(b"GET", "/api/products", &headers));
    }

    // ====================
    // F-20: キャッシュ Purge のテスト
    // ====================

    fn store_entry(manager: &CacheManager, path: &str) {
        let key = create_test_key(path);
        manager.store(key, 200, vec![], b"body".to_vec());
    }

    #[test]
    fn test_purge_exact() {
        let manager = CacheManager::new(create_test_config()).unwrap();
        let key = create_test_key("/exact");
        manager.store(key.clone(), 200, vec![], b"body".to_vec());
        assert!(manager.get(&key).is_some());

        assert!(manager.purge_exact(&key));
        assert!(manager.get(&key).is_none());
        // 既に削除済みなら false
        assert!(!manager.purge_exact(&key));
    }

    #[test]
    fn test_purge_by_prefix() {
        let manager = CacheManager::new(create_test_config()).unwrap();
        store_entry(&manager, "/api/users");
        store_entry(&manager, "/api/orders");
        store_entry(&manager, "/static/app.css");

        let purged = manager.purge_by_prefix("/api/");
        assert_eq!(purged, 2);
        assert!(manager.get(&create_test_key("/api/users")).is_none());
        assert!(manager.get(&create_test_key("/static/app.css")).is_some());
    }

    #[test]
    fn test_purge_by_pattern() {
        let manager = CacheManager::new(create_test_config()).unwrap();
        store_entry(&manager, "/static/app.css");
        store_entry(&manager, "/static/theme.css");
        store_entry(&manager, "/static/app.js");

        // host+path 形式に対する glob
        let purged = manager.purge_by_pattern("example.com/static/*.css");
        assert_eq!(purged, 2);
        assert!(manager.get(&create_test_key("/static/app.js")).is_some());
    }

    fn hv(n: &str, v: &str) -> (Box<[u8]>, Box<[u8]>) {
        (
            n.as_bytes().to_vec().into_boxed_slice(),
            v.as_bytes().to_vec().into_boxed_slice(),
        )
    }

    fn header<'a>(entry: &'a CacheEntry, name: &str) -> Option<&'a [u8]> {
        entry
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
            .map(|(_, v)| &v[..])
    }

    /// F-257: HTTP/1.1 の上流の生の応答（chunked の枠・hop-by-hop のヘッダ）は、枠を解いて `content-length` を付けた形で
    /// 保存する（どのプロトコルのクライアントへもそのまま返せる）
    #[test]
    fn store_normalizes_chunked_and_hop_by_hop() {
        let manager = CacheManager::new(create_test_config()).unwrap();
        let key = create_test_key("/chunked");
        assert!(manager.store(
            key.clone(),
            200,
            vec![
                hv("Content-Type", "text/plain"),
                hv("Transfer-Encoding", "chunked"),
                hv("Connection", "keep-alive, X-Hop"),
                hv("Keep-Alive", "timeout=5"),
                hv("X-Hop", "1"),
                hv("ETag", "\"v1\""),
            ],
            b"5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\nx-trailer: 1\r\n\r\n".to_vec(),
        ));
        let entry = manager.get(&key).unwrap();
        assert_eq!(entry.memory_body().unwrap().as_ref(), b"hello world");
        assert_eq!(header(&entry, "content-length"), Some(&b"11"[..]));
        for gone in ["transfer-encoding", "connection", "keep-alive", "x-hop"] {
            assert_eq!(header(&entry, gone), None, "{gone}");
        }
        assert_eq!(header(&entry, "content-type"), Some(&b"text/plain"[..]));
        assert_eq!(entry.etag.as_deref(), Some("\"v1\""));

        // 上流の Content-Length は本文の長さで置き換える
        let key = create_test_key("/cl");
        assert!(manager.store(
            key.clone(),
            200,
            vec![hv("Content-Length", "4")],
            b"abcd".to_vec()
        ));
        let entry = manager.get(&key).unwrap();
        let cls: Vec<_> = entry
            .headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(b"content-length"))
            .collect();
        assert_eq!(cls.len(), 1);
        assert_eq!(&cls[0].1[..], b"4");
    }

    /// 枠の壊れた・終端の無い chunked、chunked 以外の転送符号、空の本文は保存しない
    #[test]
    fn store_rejects_unusable_bodies() {
        let manager = CacheManager::new(create_test_config()).unwrap();
        let te = || vec![hv("Transfer-Encoding", "chunked")];
        assert!(!manager.store(create_test_key("/a"), 200, te(), b"zz\r\nabc".to_vec()));
        assert!(!manager.store(create_test_key("/b"), 200, te(), b"3\r\nabc\r\n".to_vec()));
        assert!(!manager.store(
            create_test_key("/c"),
            200,
            vec![hv("Transfer-Encoding", "gzip, chunked")],
            b"3\r\nabc\r\n0\r\n\r\n".to_vec()
        ));
        assert!(!manager.store(create_test_key("/d"), 200, te(), b"0\r\n\r\n".to_vec()));
        assert!(!manager.store(create_test_key("/e"), 200, vec![], Vec::new()));
    }

    /// B-491: データプレーンの保存はルートの設定（状態・既定の TTL・`key_headers`）で判定する（以前は全体の既定）
    #[test]
    fn store_async_uses_route_policy() {
        let manager = Arc::new(CacheManager::new(create_test_config()).unwrap());
        let store = |route: &CacheConfig, path: &str, status: u16, headers| {
            futures::executor::block_on(manager.clone().store_async(
                route,
                create_test_key(path),
                status,
                headers,
                b"x".to_vec(),
                None,
            ))
        };
        let route = CacheConfig {
            enabled: true,
            cacheable_statuses: vec![200],
            default_ttl_secs: 7,
            key_headers: vec!["accept-encoding".to_string()],
            ..Default::default()
        };
        // マネージャーの既定（301 を保存する・key_headers が空）ではなくルートの設定で決まる
        assert_eq!(store(&route, "/301", 301, vec![]), None);
        assert_eq!(
            store(&route, "/vary", 200, vec![hv("vary", "Accept-Encoding")]),
            Some("memory")
        );
        assert_eq!(store(&route, "/ttl", 200, vec![]), Some("memory"));
        let entry = manager.get(&create_test_key("/ttl")).unwrap();
        assert_eq!(entry.max_age_secs, 7);
        // stale の無いルートの応答は期限が切れたら消す
        assert_eq!(entry.stale_retention_secs, 0);
        // B-492: stale-if-error・stale-while-revalidate のルートの応答は期限が切れた後も残す（返せる期間だけ）
        for (swr, sie, secs) in [(true, false, 60), (false, true, 3600), (true, true, 3600)] {
            let stale_route = CacheConfig {
                stale_while_revalidate: swr,
                stale_if_error: sie,
                ..route.clone()
            };
            assert_eq!(store(&stale_route, "/stale", 200, vec![]), Some("memory"));
            let entry = manager.get(&create_test_key("/stale")).unwrap();
            assert_eq!(entry.stale_retention_secs, secs);
        }
    }

    /// ディスクへ置く応答も `store_async` で保存できる（書き込みは offload。テストではその場で走る）
    #[test]
    fn store_async_writes_disk_entries() {
        let dir = tempdir().unwrap();
        let config = CacheConfig {
            enabled: true,
            disk_path: Some(dir.path().to_path_buf()),
            memory_threshold: 4,
            ..Default::default()
        };
        let manager = Arc::new(CacheManager::new(config.clone()).unwrap());
        let key = create_test_key("/big");
        let stored = futures::executor::block_on(manager.clone().store_async(
            &config,
            key.clone(),
            200,
            vec![],
            b"0123456789".to_vec(),
            None,
        ));
        assert_eq!(stored, Some("disk"));
        let entry = manager.get(&key).unwrap();
        let path = entry.disk_path().unwrap();
        // 理由付き allow: テストの検証（データプレーンの外）
        #[allow(clippy::disallowed_methods)]
        let written = std::fs::read(path).unwrap();
        assert_eq!(written, b"0123456789");
        assert_eq!(header(&entry, "content-length"), Some(&b"10"[..]));
    }

    #[test]
    fn test_purge_all() {
        let manager = CacheManager::new(create_test_config()).unwrap();
        store_entry(&manager, "/a");
        store_entry(&manager, "/b");
        store_entry(&manager, "/c");

        let purged = manager.purge_all();
        assert_eq!(purged, 3);
        assert_eq!(manager.stats().entries, 0);
    }
}
