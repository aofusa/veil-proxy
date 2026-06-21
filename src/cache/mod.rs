//! # プロキシキャッシュモジュール
//!
//! 頻繁にアクセスされるAPIや静的ファイルのバックエンド負荷を軽減するための
//! キャッシュ機能を提供します。
//!
//! ## 特徴
//!
//! - **インメモリインデックス**: DashMapによるロックフリーな並行アクセス
//! - **メモリキャッシュ**: 小さいレスポンス用の高速アクセス
//! - **ディスクキャッシュ**: 大きいレスポンス用のmonoio::fs非同期I/O
//! - **LRU Eviction**: メモリ制限に達した際の自動削除
//! - **Cache-Control対応**: TTL、Vary、ETagのサポート
//!
//! cache feature が無効の場合、スタブ実装を提供します。
//!
//! ## アーキテクチャ
//!
//! ```text
//! ┌─────────────────────────────────────────┐
//! │  CacheManager                           │
//! │  ├─ CacheIndex (DashMap)                │← キャッシュメタデータ
//! │  ├─ MemoryCache (LruCache)              │← 小さいレスポンス
//! │  └─ DiskCache (monoio::fs)              │← 大きいレスポンス
//! └─────────────────────────────────────────┘
//! ```
//!
//! ## 使用例
//!
//! ```toml
//! [path_routes."example.com"."/api/".cache]
//! enabled = true
//! max_memory_size = 104857600  # 100MB
//! disk_path = "/var/cache/veil"
//! default_ttl_secs = 300  # 5分
//! ```

// DashMap非依存モジュール（常時コンパイル）
mod config;
mod key;
mod entry;
mod policy;

// DashMap依存モジュール（cache feature 有効時のみ）
#[cfg(feature = "cache")]
mod index;
#[cfg(feature = "cache")]
mod memory;
#[cfg(feature = "cache")]
mod disk;
#[cfg(feature = "cache")]
mod manager;
#[cfg(feature = "cache")]
mod revalidation;
#[cfg(feature = "cache")]
mod file_cache;

// 常時公開 (DashMap 不要)
pub use config::CacheConfig;
pub use key::CacheKey;
pub use entry::{CacheEntry, CacheStorage};
pub use policy::{CachePolicy, CacheControl, VaryResult};

// cache feature 有効時のみ公開
#[cfg(feature = "cache")]
pub use index::CacheIndex;
#[cfg(feature = "cache")]
pub use memory::MemoryCache;
#[cfg(feature = "cache")]
pub use disk::DiskCache;
#[cfg(feature = "cache")]
pub use manager::{CacheManager, CacheStats, init_global_cache, get_global_cache};
#[cfg(feature = "cache")]
pub use revalidation::{try_start_revalidation, finish_revalidation, active_revalidations, collapsed_request_count};
#[cfg(feature = "cache")]
pub use file_cache::{get_file_cache, get_file_info, get_file_info_with_config, invalidate_file_cache, CachedFileInfo, OpenFileCache, OpenFileCacheConfig, configure_global_open_file_cache};

// ====================
// cache feature 無効時のスタブ実装
// ====================

/// キャッシュ統計情報（スタブ）
#[cfg(not(feature = "cache"))]
#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    pub entries: usize,
    pub memory_usage: usize,
    pub disk_usage: u64,
    pub hits: u64,
    pub misses: u64,
    pub hit_rate: f64,
    pub uptime_secs: u64,
}

/// キャッシュマネージャー（スタブ）
#[cfg(not(feature = "cache"))]
pub struct CacheManager;

#[cfg(not(feature = "cache"))]
impl CacheManager {
    pub fn stats(&self) -> CacheStats { CacheStats::default() }
    pub fn is_enabled(&self) -> bool { false }
    pub fn config(&self) -> &CacheConfig { unimplemented!() }
    pub fn is_request_cacheable(&self, _method: &[u8], _path: &str, _hdrs: &[(Box<[u8]>, Box<[u8]>)]) -> bool { false }
    pub fn get(&self, _key: &CacheKey) -> Option<std::sync::Arc<CacheEntry>> { None }
    pub fn get_stale(&self, _key: &CacheKey, _max_stale_secs: u64) -> Option<std::sync::Arc<CacheEntry>> { None }
    pub fn store(&self, _key: CacheKey, _status: u16, _hdrs: Vec<(Box<[u8]>, Box<[u8]>)>, _body: Vec<u8>) -> bool { false }
    pub fn store_with_vary(&self, _key: CacheKey, _status: u16, _hdrs: Vec<(Box<[u8]>, Box<[u8]>)>, _body: Vec<u8>, _vary: Option<Vec<String>>) -> bool { false }
    pub fn invalidate(&self, _key: &CacheKey) {}
    pub fn invalidate_pattern(&self, _pattern: &str) -> usize { 0 }
    pub fn invalidate_host(&self, _host: &str) -> usize { 0 }
    pub fn evict_expired(&self) -> usize { 0 }
    pub fn evict_lru(&self) -> usize { 0 }
    pub fn evict_disk(&self) -> std::io::Result<usize> { Ok(0) }
    pub fn clear(&self) -> std::io::Result<()> { Ok(()) }
}

/// グローバルキャッシュ初期化（スタブ）
#[cfg(not(feature = "cache"))]
pub fn init_global_cache(_config: CacheConfig) -> std::io::Result<()> {
    Ok(())
}

/// グローバルキャッシュ取得（スタブ: 常に None）
#[cfg(not(feature = "cache"))]
pub fn get_global_cache() -> Option<std::sync::Arc<CacheManager>> {
    None
}

/// open file cache 設定（cache feature 無効時）
#[cfg(not(feature = "cache"))]
#[derive(Clone, Debug, serde::Deserialize)]
pub struct OpenFileCacheConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default, rename = "valid_duration_secs")]
    pub valid_duration_secs: Option<u64>,
    #[serde(default, rename = "max_entries")]
    pub max_entries: Option<usize>,
}

/// キャッシュされたファイル情報（スタブ）
#[cfg(not(feature = "cache"))]
#[derive(Clone, Debug)]
pub struct CachedFileInfo {
    pub canonical_path: std::path::PathBuf,
    pub file_size: u64,
    pub mime_type: String,
    pub last_modified: Option<std::time::SystemTime>,
    pub is_file: bool,
}

#[cfg(not(feature = "cache"))]
impl CachedFileInfo {
    pub fn last_modified_rfc7231(&self) -> Option<String> { None }
    pub fn is_valid(&self, _max_age: std::time::Duration) -> bool { false }
    pub fn etag(&self) -> Option<String> { None }
}

#[cfg(not(feature = "cache"))]
pub fn configure_global_open_file_cache(_enabled: bool, _valid_duration_secs: u64, _max_entries: usize) {}

#[cfg(not(feature = "cache"))]
pub fn get_file_cache() -> Option<std::sync::Arc<()>> { None }

/// ファイル情報取得（スタブ）
#[cfg(not(feature = "cache"))]
pub fn get_file_info_with_config(
    _path: &std::path::Path,
    _config: Option<&OpenFileCacheConfig>,
) -> Option<CachedFileInfo> {
    None
}

#[cfg(not(feature = "cache"))]
pub fn get_file_info(_path: &std::path::Path) -> Option<CachedFileInfo> {
    None
}

#[cfg(not(feature = "cache"))]
pub fn invalidate_file_cache(_path: &std::path::Path) {}

/// 再検証スタブ（cache feature 無効時）
#[cfg(not(feature = "cache"))]
pub fn try_start_revalidation(_hash: u64) -> bool { false }

#[cfg(not(feature = "cache"))]
pub fn finish_revalidation(_hash: u64) {}

#[cfg(not(feature = "cache"))]
pub fn active_revalidations() -> usize { 0 }

#[cfg(not(feature = "cache"))]
pub fn collapsed_request_count() -> u64 { 0 }
