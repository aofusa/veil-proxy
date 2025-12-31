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

mod config;
mod key;
mod entry;
mod index;
mod memory;
mod disk;
mod manager;
mod policy;
mod revalidation;
mod file_cache;

pub use config::CacheConfig;
pub use key::CacheKey;
pub use entry::{CacheEntry, CacheStorage};
pub use index::CacheIndex;
pub use memory::MemoryCache;
pub use disk::DiskCache;
pub use manager::{CacheManager, CacheStats, init_global_cache, get_global_cache};
pub use policy::{CachePolicy, CacheControl, VaryResult};
pub use revalidation::{try_start_revalidation, finish_revalidation, active_revalidations, collapsed_request_count};
pub use file_cache::{get_file_cache, get_file_info, invalidate_file_cache, CachedFileInfo, OpenFileCache};

