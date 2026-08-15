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
mod entry;
mod key;
mod policy;
// F-153: 静的配信のパス解決（canonicalize を使わないカーネル封じ込め）。
// `cache` feature の有無どちらでも静的配信自体は成立させる必要があるため常時コンパイル。
pub mod resolve;

// DashMap依存モジュール（cache feature 有効時のみ）
#[cfg(feature = "cache")]
mod content_cache;
#[cfg(feature = "cache")]
mod disk;
#[cfg(feature = "cache")]
mod file_cache;
#[cfg(feature = "cache")]
mod index;
#[cfg(feature = "cache")]
mod manager;
#[cfg(feature = "cache")]
mod memory;
#[cfg(feature = "cache")]
mod revalidation;

// 常時公開 (DashMap 不要)
pub use config::CacheConfig;
pub use entry::{CacheEntry, CacheStorage};
pub use key::CacheKey;
pub use policy::{CacheControl, CachePolicy, VaryResult};

/// ディレクトリルート File バックエンドの封じ込め検査（F-145）。
///
/// 従来は毎リクエスト `get_file_info_with_config(base_path, ...)` をもう一度呼び、
/// その `canonical_path` と比較していた（`open_file_cache` 無効時は
/// `canonicalize`/`metadata` の再実行＝ `__realpathat`/`fstatat`/offload スレッド
/// 往復が 1 リクエストあたり追加で発生していた）。base_path はロード中の設定に対して
/// 不変なので、`canonical_base`（config ロード時に一度だけ解決した canonical 形、
/// `config::load_backend` 参照）を使い回すことでこの往復を排除する。
///
/// - `canonical_base` が `Some`: 事前解決済みの canonical パスと比較する
///   （挙動は解決前と同一）。
/// - `canonical_base` が `None`（load 時の解決失敗。ディレクトリ未作成等）:
///   生の `base_path` と比較する。`full_path` は常に `base_path` を起点に
///   join して構築されるため、この比較は恒真（許可）になる。従来もこの場合は
///   base 情報取得自体が失敗して封じ込め検査をスキップ（実質許可）していたため、
///   結果として同じ「許可」に帰着する（起動後にディレクトリが作成される運用でも
///   動作し続ける）。
///
/// FreeBSD capability mode（`cap_enter`、F-123）が有効な間は、実際のパストラバーサル
/// 封じ込めは dirfd 相対 `openat`/`fstatat` の `O_RESOLVE_BENEATH` が担う。この経路では
/// `file_info.canonical_path` 自体が生パス（`full_path` そのもの）になるため、
/// `full_path` が常に `base_path` の join で構築される以上この比較は必ず真になる
/// （cap_enter 前と同じ「常に許可」という結果は変わらない）。よって
/// `security::capsicum::static_serving_active()`（追加 syscall 無しの atomic load）が
/// true の間は比較そのものを省略できる。`cache` feature の有無に依存しないため
/// feature ゲート外（このファイル）に置く。
#[inline]
pub fn sendfile_base_contains(
    file_canonical_path: &std::path::Path,
    canonical_base: Option<&std::path::Path>,
    base_path: &std::path::Path,
) -> bool {
    #[cfg(target_os = "freebsd")]
    if crate::security::capsicum::static_serving_active() {
        return true;
    }
    let base = canonical_base.unwrap_or(base_path);
    file_canonical_path.starts_with(base)
}

// cache feature 有効時のみ公開
#[cfg(feature = "cache")]
pub use content_cache::{
    clear as clear_content_cache, configure_global_static_content_cache,
    effective_static_content_cache_config, get_cached as get_cached_content_cache,
    get_or_load as get_or_load_content_cache,
    get_or_load_with_mime as get_or_load_content_cache_with_mime, hits as content_cache_hits,
    insert_bytes as insert_content_cache_bytes, invalidate as invalidate_content_cache,
    len as content_cache_len, misses as content_cache_misses, StaticContentCacheConfig,
    StaticContentCacheRouteConfig,
};
#[cfg(feature = "cache")]
pub use disk::DiskCache;
#[cfg(feature = "cache")]
pub use file_cache::{
    configure_global_open_file_cache, get_file_cache, get_file_info, get_file_info_with_config,
    invalidate_file_cache, CachedFileInfo, OpenFileCache, OpenFileCacheConfig,
};
#[cfg(feature = "cache")]
pub use index::CacheIndex;
#[cfg(feature = "cache")]
pub use manager::{get_global_cache, init_global_cache, CacheManager, CacheStats};
#[cfg(feature = "cache")]
pub use memory::MemoryCache;
#[cfg(feature = "cache")]
pub use revalidation::{
    active_revalidations, collapsed_request_count, finish_revalidation, try_start_revalidation,
};

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
    pub fn stats(&self) -> CacheStats {
        CacheStats::default()
    }
    pub fn is_enabled(&self) -> bool {
        false
    }
    pub fn config(&self) -> &CacheConfig {
        unimplemented!()
    }
    pub fn is_request_cacheable(
        &self,
        _method: &[u8],
        _path: &str,
        _hdrs: &[(Box<[u8]>, Box<[u8]>)],
    ) -> bool {
        false
    }
    pub fn get(&self, _key: &CacheKey) -> Option<std::sync::Arc<CacheEntry>> {
        None
    }
    pub fn get_stale(
        &self,
        _key: &CacheKey,
        _max_stale_secs: u64,
    ) -> Option<std::sync::Arc<CacheEntry>> {
        None
    }
    pub fn store(
        &self,
        _key: CacheKey,
        _status: u16,
        _hdrs: Vec<(Box<[u8]>, Box<[u8]>)>,
        _body: Vec<u8>,
    ) -> bool {
        false
    }
    pub fn store_with_vary(
        &self,
        _key: CacheKey,
        _status: u16,
        _hdrs: Vec<(Box<[u8]>, Box<[u8]>)>,
        _body: Vec<u8>,
        _vary: Option<Vec<String>>,
    ) -> bool {
        false
    }
    pub fn invalidate(&self, _key: &CacheKey) {}
    pub fn invalidate_pattern(&self, _pattern: &str) -> usize {
        0
    }
    pub fn invalidate_host(&self, _host: &str) -> usize {
        0
    }
    pub fn evict_expired(&self) -> usize {
        0
    }
    pub fn evict_lru(&self) -> usize {
        0
    }
    pub fn evict_disk(&self) -> std::io::Result<usize> {
        Ok(0)
    }
    pub fn clear(&self) -> std::io::Result<()> {
        Ok(())
    }
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
    pub fn last_modified_rfc7231(&self) -> Option<String> {
        None
    }
    pub fn is_valid(&self, _max_age: std::time::Duration) -> bool {
        false
    }
    pub fn etag(&self) -> Option<String> {
        None
    }
}

#[cfg(not(feature = "cache"))]
pub fn configure_global_open_file_cache(
    _enabled: bool,
    _valid_duration_secs: u64,
    _max_entries: usize,
) {
}

#[cfg(not(feature = "cache"))]
pub fn get_file_cache() -> Option<std::sync::Arc<()>> {
    None
}

/// ファイル情報取得（`cache` feature 無効時）
///
/// キャッシュは持たないが、静的ファイル配信（SendFile）はどの feature 構成でも成立させる
/// 必要があるため、ここで **キャッシュせず** に実ファイルの解決（canonicalize + metadata +
/// MIME 推測）を行う。以前はスタブが `None` を返しており、`cache` を含まない構成
/// （例: default features）では全ての静的ファイルが 404 になっていた（回帰バグ）。
///
/// `canonicalize` / `metadata` は同期 syscall で io_uring 非対応のため、`runtime::offload`
/// の専用ワーカースレッドへ退避し、イベントループをブロックしない（`cache` 有効時の
/// `fetch_file_info` と同じ設計）。
#[cfg(not(feature = "cache"))]
async fn fetch_file_info_uncached(path: &std::path::Path) -> Option<CachedFileInfo> {
    let path = path.to_path_buf();
    crate::runtime::offload::offload(move || {
        // F-153: カーネル封じ込め（openat2、Linux 専用）による解決を優先する。
        // `canonical_path` の互換性・フォールバック方針は `file_cache.rs::fetch_file_info`
        // のコメント参照（`cache` feature 有無に関わらず同じ設計）。
        if let Some(res) = crate::cache::resolve::open_beneath_for_request(&path) {
            return match res {
                Ok((_file, meta)) => {
                    let mime_type = mime_guess::from_path(&path)
                        .first_or_octet_stream()
                        .to_string();
                    Some(CachedFileInfo {
                        canonical_path: path,
                        file_size: meta.len(),
                        mime_type,
                        last_modified: meta.modified().ok(),
                        is_file: meta.is_file(),
                    })
                }
                Err(_) => None,
            };
        }
        // F-123: FreeBSD capability mode 下では canonicalize（絶対パス realpath）が
        // 禁止されるため、登録済みルート dirfd 相対の fstatat で代替する（O_RESOLVE_BENEATH
        // が封じ込めを担保。canonical_path は原パスのまま = 配信 open も同経路で相対化）。
        #[cfg(target_os = "freebsd")]
        if let Some(res) = crate::security::capsicum::stat_static(&path) {
            let st = res.ok()?;
            let mime_type = mime_guess::from_path(&path)
                .first_or_octet_stream()
                .to_string();
            return Some(CachedFileInfo {
                canonical_path: path,
                file_size: st.len,
                mime_type,
                last_modified: st.mtime,
                is_file: st.is_file,
            });
        }
        let canonical = path.canonicalize().ok()?;
        // 理由付き allow: offload の専用ワーカースレッド内であり、イベントループを
        // ブロックしない（AGENTS.md がホットパス外の正当な同期 FS 利用として許可）。
        #[allow(clippy::disallowed_methods)]
        let metadata = std::fs::metadata(&canonical).ok()?;
        let mime_type = mime_guess::from_path(&canonical)
            .first_or_octet_stream()
            .to_string();
        Some(CachedFileInfo {
            canonical_path: canonical,
            file_size: metadata.len(),
            mime_type,
            last_modified: metadata.modified().ok(),
            is_file: metadata.is_file(),
        })
    })
    .await
}

#[cfg(not(feature = "cache"))]
pub async fn get_file_info_with_config(
    path: &std::path::Path,
    _config: Option<&OpenFileCacheConfig>,
) -> Option<CachedFileInfo> {
    fetch_file_info_uncached(path).await
}

#[cfg(not(feature = "cache"))]
pub async fn get_file_info(path: &std::path::Path) -> Option<CachedFileInfo> {
    fetch_file_info_uncached(path).await
}

#[cfg(not(feature = "cache"))]
pub fn invalidate_file_cache(_path: &std::path::Path) {}

// ====================
// 静的コンテンツキャッシュ（F-146）スタブ（cache feature 無効時）
// ====================
//
// `cache` feature が無いビルドではキャッシュ本体（DashMap）を持たず、常に
// オフロード経由の直接読み込みへフォールバックする（`get_file_info_with_config`
// の無効時挙動と同じ設計：キャッシュしないだけで、静的配信自体は成立させる）。

/// 静的コンテンツキャッシュのグローバル設定（`cache` feature 無効時のスタブ）。
#[cfg(not(feature = "cache"))]
#[derive(Clone, Copy, Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct StaticContentCacheConfig {
    pub enabled: bool,
    pub valid_duration_secs: u64,
    pub max_entries: usize,
    pub max_file_size_bytes: u64,
    pub max_total_bytes: u64,
    pub revalidate_mtime: bool,
}

/// ルートごとの静的コンテンツキャッシュ上書き設定（`cache` feature 無効時のスタブ）。
#[cfg(not(feature = "cache"))]
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct StaticContentCacheRouteConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub valid_duration_secs: Option<u64>,
    #[serde(default)]
    pub max_entries: Option<usize>,
    #[serde(default)]
    pub max_file_size_bytes: Option<u64>,
    #[serde(default)]
    pub max_total_bytes: Option<u64>,
    #[serde(default)]
    pub revalidate_mtime: Option<bool>,
}

#[cfg(not(feature = "cache"))]
pub fn configure_global_static_content_cache(_cfg: &StaticContentCacheConfig) {}

#[cfg(not(feature = "cache"))]
pub fn effective_static_content_cache_config(
    _route: Option<&StaticContentCacheRouteConfig>,
) -> StaticContentCacheConfig {
    StaticContentCacheConfig::default()
}

/// `cache` feature 無効時は常にオフロード経由で直接読み込む（キャッシュしない）。
#[cfg(not(feature = "cache"))]
pub async fn get_or_load_content_cache(
    path: &std::path::Path,
    _cfg: &StaticContentCacheConfig,
) -> Option<bytes::Bytes> {
    let load_path = path.to_path_buf();
    // 理由付き allow: offload 専用ワーカースレッド内で実行、イベントループ非ブロック。
    #[allow(clippy::disallowed_methods)]
    crate::runtime::offload::offload(move || std::fs::read(&load_path))
        .await
        .ok()
        .map(bytes::Bytes::from)
}

/// [`get_or_load_content_cache`] の MIME タイプ再利用版スタブ。
#[cfg(not(feature = "cache"))]
pub async fn get_or_load_content_cache_with_mime<F>(
    path: &std::path::Path,
    _cfg: &StaticContentCacheConfig,
    mime_fallback: F,
) -> Option<(bytes::Bytes, std::sync::Arc<str>)>
where
    F: FnOnce() -> std::sync::Arc<str>,
{
    let data = get_or_load_content_cache(path, _cfg).await?;
    Some((data, mime_fallback()))
}

/// `cache` feature 無効時のスタブ: キャッシュ本体が無いため常に `None`
/// （呼び出し元はミス扱いとして自前で読み込みへフォールバックする）。
#[cfg(not(feature = "cache"))]
pub async fn get_cached_content_cache(
    _path: &std::path::Path,
    _cfg: &StaticContentCacheConfig,
) -> Option<bytes::Bytes> {
    None
}

/// `cache` feature 無効時のスタブ: キャッシュ本体が無いため何もしない。
#[cfg(not(feature = "cache"))]
pub fn insert_content_cache_bytes(
    _path: &std::path::Path,
    _data: bytes::Bytes,
    _mime: std::sync::Arc<str>,
    _cfg: &StaticContentCacheConfig,
) {
}

#[cfg(not(feature = "cache"))]
pub fn invalidate_content_cache(_path: &std::path::Path) {}

#[cfg(not(feature = "cache"))]
pub fn clear_content_cache() {}

#[cfg(not(feature = "cache"))]
pub fn content_cache_hits() -> u64 {
    0
}

#[cfg(not(feature = "cache"))]
pub fn content_cache_misses() -> u64 {
    0
}

#[cfg(not(feature = "cache"))]
pub fn content_cache_len() -> usize {
    0
}

/// 再検証スタブ（cache feature 無効時）
#[cfg(not(feature = "cache"))]
pub fn try_start_revalidation(_hash: u64) -> bool {
    false
}

#[cfg(not(feature = "cache"))]
pub fn finish_revalidation(_hash: u64) {}

#[cfg(not(feature = "cache"))]
pub fn active_revalidations() -> usize {
    0
}

#[cfg(not(feature = "cache"))]
pub fn collapsed_request_count() -> u64 {
    0
}

// B-14 回帰テスト: cache feature 無効時でも静的ファイルが解決できること
// （以前はスタブが None を返し全ての静的配信が 404 になっていた）
#[cfg(all(test, not(feature = "cache")))]
mod nocache_file_info_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn get_file_info_resolves_real_file_without_cache() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("index.html");
        let mut f = std::fs::File::create(&file_path).unwrap();
        f.write_all(b"<html>hello</html>").unwrap();
        drop(f);

        // ring 未初期化のため offload は同期インライン実行され block_on が即完了する。
        let info = futures::executor::block_on(get_file_info(&file_path));
        assert!(info.is_some(), "cache 無効でもファイル情報を解決できるべき");
        let info = info.unwrap();
        assert!(info.is_file);
        assert_eq!(info.file_size, 18);
        assert!(info.mime_type.starts_with("text/html"));

        // 存在しないファイルは None
        let missing = futures::executor::block_on(get_file_info(&dir.path().join("nope.txt")));
        assert!(missing.is_none());
    }
}
