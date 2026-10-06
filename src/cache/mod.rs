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
// F-169: 静的配信の圧縮結果キャッシュ。`cache` feature の有無で内部実装が切り替わる
// （無効時は毎回圧縮のスタブ）ため、モジュール自体は常時コンパイルする
// （content_cache 等と違い `StaticContentCacheConfig` の型名を cfg 分岐なしで
// そのまま `super::StaticContentCacheConfig` として使い回せる）。
pub mod compressed;

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
// B-65: 静的配信のメタデータ+本体を1回のoffloadで取得する複合API。file_cache/content_cache
// の両方に依存するため cache feature 有効時のみコンパイルする。
#[cfg(feature = "cache")]
mod static_file;

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
/// FreeBSD では、登録済み静的ルートの dirfd 相対 `openat`/`fstatat`（F-123、
/// `O_RESOLVE_BENEATH`）で開けた場合の `file_info.canonical_path` は生パス
/// （`base_path` の join そのもの）になり、canonical 形の `canonical_base` とは
/// 一致しないことがある（`base_path` がシンボリックリンクを含む場合）。そのため
/// **生の `base_path` 配下であることも許可条件に加える**。dirfd 経路の生パスは
/// カーネルが封じ込めを保証済みで、従来経路（`canonicalize()`）の canonical パスは
/// シンボリックリンクを解決済みなので生の `base_path`（シンボリックリンクなら
/// canonical パスの接頭辞になり得ない）に一致するのはルート内に限られる。
///
/// 以前は `static_serving_active()` の間この比較を丸ごと省略して常に許可していたが、
/// それでは dirfd 経路を外れて従来経路へ落ちたパス（登録済みルート外・
/// `ENOTCAPABLE` の再判定。B-89）の封じ込めが効かない（BSD 実機の単体テストで検出）。
/// `cache` feature の有無に依存しないため feature ゲート外（このファイル）に置く。
#[inline]
pub fn sendfile_base_contains(
    file_canonical_path: &std::path::Path,
    canonical_base: Option<&std::path::Path>,
    base_path: &std::path::Path,
) -> bool {
    let base = canonical_base.unwrap_or(base_path);
    #[cfg(target_os = "freebsd")]
    if path_has_prefix(file_canonical_path, base_path) {
        return true;
    }
    path_has_prefix(file_canonical_path, base)
}

/// `Path::starts_with` と同じ判定を、まずバイト列の前方一致（要素境界つき）で行う。
///
/// `Path::starts_with` は両方のパスを要素ごとに走査するため、毎リクエスト呼ぶと
/// 目立つ（FreeBSD のプロファイルで `Components::next` / `Path::starts_with` が上位）。
/// 静的配信のパスは常に base_path の join で組み立てられるので、ほぼ全件が
/// バイト列の一致で決着する。一致しない場合（`./` や重複区切りを含む等）だけ
/// 従来の要素比較で判定し、結果は `Path::starts_with` と変わらない。
#[inline]
fn path_has_prefix(path: &std::path::Path, base: &std::path::Path) -> bool {
    let p = path.as_os_str().as_encoded_bytes();
    let b = base.as_os_str().as_encoded_bytes();
    let b_trim = match b.last() {
        Some(&last) if b.len() > 1 && std::path::is_separator(last as char) => &b[..b.len() - 1],
        _ => b,
    };
    if p.len() >= b_trim.len()
        && &p[..b_trim.len()] == b_trim
        && (p.len() == b_trim.len()
            || std::path::is_separator(p[b_trim.len()] as char)
            || b_trim
                .last()
                .is_some_and(|&c| std::path::is_separator(c as char)))
    {
        return true;
    }
    path.starts_with(base)
}

#[cfg(test)]
mod path_prefix_tests {
    use super::path_has_prefix;
    use std::path::Path;

    /// バイト列の高速判定が `Path::starts_with` と常に同じ結論になること。
    #[test]
    fn matches_path_starts_with() {
        let cases = [
            ("/var/www/a.html", "/var/www"),
            ("/var/www/a.html", "/var/www/"),
            ("/var/www", "/var/www"),
            ("/var/wwwx/a", "/var/www"),
            ("/var/www/../etc/passwd", "/var/www"),
            ("/var//www/a", "/var/www"),
            ("./www/a", "www"),
            ("/a", "/"),
            ("/", "/"),
            ("/other", "/var/www"),
        ];
        for (p, b) in cases {
            assert_eq!(
                path_has_prefix(Path::new(p), Path::new(b)),
                Path::new(p).starts_with(Path::new(b)),
                "{p} vs {b}"
            );
        }
    }
}

/// ディレクトリルート File バックエンドの per-route 封じ込めパラメータ（F-154）。
///
/// `cache` feature の有無に関わらず同じ型を使う（`get_static_file_with_content` の
/// シグネチャを両 cfg で揃えるため、feature ゲート外のこのファイルに置く）。
///
/// - `root`: そのリクエストを処理しているルートの `base_path`。高速経路
///   （`resolve::open_beneath_in_root`/Linux）はこの `root` の dirfd に対して直接
///   `openat2(RESOLVE_BENEATH)` するため、**それ自体が per-route の封じ込め**になり、
///   `readlink` による事後検査が不要になる（B-65 が追加していた 1 回/リクエストの
///   `readlink` を削除できる＝本チケットの目的）。
/// - `canonical_base`: フォールバック経路（`canonicalize()` + `sendfile_base_contains`）
///   でのみ使う、config ロード時に解決済みの `root` の canonical 形。
#[derive(Clone, Copy)]
pub struct RouteContainment<'a> {
    pub root: &'a std::path::Path,
    pub canonical_base: Option<&'a std::path::Path>,
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
#[cfg(feature = "cache")]
pub use static_file::{get_static_file_with_content, StaticFileOutcome};

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
    pub canonical_path: std::sync::Arc<std::path::Path>,
    pub file_size: u64,
    pub mime_type: std::sync::Arc<str>,
    pub last_modified: Option<std::time::SystemTime>,
    pub is_file: bool,
}

#[cfg(not(feature = "cache"))]
impl CachedFileInfo {
    pub fn last_modified_rfc7231(&self) -> Option<String> {
        None
    }

    /// `cache` 無効時は fd を保持しない（配信時に開く）。
    #[inline]
    pub fn shared_file(&self) -> Option<&std::sync::Arc<std::fs::File>> {
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
                    let mime_type: std::sync::Arc<str> = mime_guess::from_path(&path)
                        .first_or_octet_stream()
                        .as_ref()
                        .into();
                    Some(CachedFileInfo {
                        canonical_path: path.into(),
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
            let mime_type: std::sync::Arc<str> = mime_guess::from_path(&path)
                .first_or_octet_stream()
                .as_ref()
                .into();
            return Some(CachedFileInfo {
                canonical_path: path.into(),
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
        let mime_type: std::sync::Arc<str> = mime_guess::from_path(&canonical)
            .first_or_octet_stream()
            .as_ref()
            .into();
        Some(CachedFileInfo {
            canonical_path: canonical.into(),
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

/// `cache` feature 無効時のスタブから使う: 開いた `File`+`Metadata` から
/// `CachedFileInfo` を直接構築する（`fetch_file_info_uncached` と同じ mime 推定ロジック）。
#[cfg(not(feature = "cache"))]
fn build_cached_file_info_uncached(
    path: &std::path::Path,
    meta: &std::fs::Metadata,
) -> CachedFileInfo {
    let mime_type: std::sync::Arc<str> = mime_guess::from_path(path)
        .first_or_octet_stream()
        .as_ref()
        .into();
    CachedFileInfo {
        canonical_path: path.into(),
        file_size: meta.len(),
        mime_type,
        last_modified: meta.modified().ok(),
        is_file: meta.is_file(),
    }
}

/// B-65 続き: `cache` feature 無効時の解決結果（`feature = "cache"` 版の
/// `static_file::StaticFileOutcome` と同じ形。`CachedFileInfo` の実体が feature ごとに
/// 別定義のため、このスタブ側にも別途定義する）。
#[cfg(not(feature = "cache"))]
pub enum StaticFileOutcome {
    /// 通常ファイル: メタデータ + 本体。
    File(CachedFileInfo, bytes::Bytes),
    /// ディレクトリだった: index 解決が必要（本体は読んでいない）。
    Directory(CachedFileInfo),
    /// per-route の封じ込め検査に失敗（403 相当）。**本体は読んでいない。**
    Forbidden,
}

/// B-65: `cache` feature 無効時のスタブ（キャッシュは持たないが、メタデータ+本体を
/// **1 回の offload** で取得する。従来はこのスタブが存在せず呼び出し側が
/// `get_file_info_with_config` + `get_or_load_content_cache` を別々に呼んでおり
/// 2 回 offload していた）。
///
/// 登録済み静的ルート配下（`resolve::open_beneath_for_request`/
/// `security::capsicum::open_static_ro` が使える）であれば同じ fd から fstat と
/// 本体読み込みの両方を行う。それ以外は `canonicalize` + `File::open` + 読み込みへ
/// フォールバックする（`fetch_file_info_uncached` と同じ防御水準）。
///
/// `containment`（F-154）: ディレクトリルートの per-route 封じ込め検査。
/// `Some` の場合は `root`（= そのルートの `base_path`）の dirfd に対してのみ
/// `openat2`/`openat` するため、開けた時点でそれ自体が封じ込めの証明になり、
/// `readlink` による事後検査は不要（`RouteContainment` doc 参照。`cache` feature
/// 有無に関わらず同じ理由が当てはまる）。`None`（固定ファイルルート）の場合は
/// 「どれかの登録済みルート」探索（`open_beneath_for_request`）のままでよい
/// （`path` は常に config 由来の固定値でユーザ入力に左右されないため）。
#[cfg(not(feature = "cache"))]
fn open_and_read_uncached(
    path: &std::path::Path,
    containment: Option<(Option<std::path::PathBuf>, std::path::PathBuf)>,
) -> Option<StaticFileOutcome> {
    use std::io::Read;

    #[cfg(target_os = "linux")]
    {
        let opened = match containment.as_ref() {
            Some((_, root)) => crate::cache::resolve::open_beneath_in_root(root, path),
            None => crate::cache::resolve::open_beneath_for_request(path),
        };
        if let Some(res) = opened {
            return match res {
                Ok((mut file, meta)) => {
                    let info = build_cached_file_info_uncached(path, &meta);
                    if !info.is_file {
                        return Some(StaticFileOutcome::Directory(info));
                    }
                    let mut buf: Vec<u8> = Vec::with_capacity(meta.len() as usize);
                    file.read_to_end(&mut buf).ok()?;
                    Some(StaticFileOutcome::File(info, bytes::Bytes::from(buf)))
                }
                Err(_) => None,
            };
        }
    }
    #[cfg(target_os = "freebsd")]
    if let Some(res) = crate::security::capsicum::open_static_ro(path) {
        return match res {
            Ok(mut file) => {
                let meta = file.metadata().ok()?;
                if let Some((canonical_base, base_path)) = containment.as_ref() {
                    // dirfd 相対 openat（O_RESOLVE_BENEATH）で開けた生パスを渡す。
                    // `sendfile_base_contains` は FreeBSD では生の base_path 配下も許可する
                    // （このファイル冒頭の doc 参照）ので、ルート内なら判定は真になる。
                    if !sendfile_base_contains(path, canonical_base.as_deref(), base_path) {
                        return Some(StaticFileOutcome::Forbidden);
                    }
                }
                let info = build_cached_file_info_uncached(path, &meta);
                if !info.is_file {
                    return Some(StaticFileOutcome::Directory(info));
                }
                let mut buf: Vec<u8> = Vec::with_capacity(meta.len() as usize);
                file.read_to_end(&mut buf).ok()?;
                Some(StaticFileOutcome::File(info, bytes::Bytes::from(buf)))
            }
            Err(_) => None,
        };
    }
    // フォールバック: canonicalize + open + read（`fetch_file_info_uncached` と同じ解決）。
    // ここは高速経路（openat2/capsicum）が使えなかった場合の経路であり、per-route の
    // 封じ込め検査を省略してはならない。
    let canonical = path.canonicalize().ok()?;
    if let Some((canonical_base, base_path)) = containment.as_ref() {
        if !sendfile_base_contains(&canonical, canonical_base.as_deref(), base_path) {
            return Some(StaticFileOutcome::Forbidden);
        }
    }
    // 理由付き allow: offload の専用ワーカースレッド内であり、イベントループを
    // ブロックしない（呼び出し元 `get_static_file_with_content` が offload で包む）。
    #[allow(clippy::disallowed_methods)]
    let mut file = std::fs::File::open(&canonical).ok()?;
    let meta = file.metadata().ok()?;
    let info = build_cached_file_info_uncached(&canonical, &meta);
    if !info.is_file {
        return Some(StaticFileOutcome::Directory(info));
    }
    let mut buf: Vec<u8> = Vec::with_capacity(meta.len() as usize);
    file.read_to_end(&mut buf).ok()?;
    Some(StaticFileOutcome::File(info, bytes::Bytes::from(buf)))
}

/// B-65: `cache` feature 無効時のスタブ版 `get_static_file_with_content`。
/// キャッシュは持たないため、毎回 1 回の offload でメタデータ+本体を取得する
/// （従来の 2 回 offload から半減。設定引数はキャッシュが無いため無視する）。
///
/// `containment` の意味は `feature = "cache"` 版と同じ（ディレクトリルートのみ
/// `Some` を渡す。固定ファイルルートでは `None` で余分な確保をしない）。
#[cfg(not(feature = "cache"))]
pub async fn get_static_file_with_content(
    path: &std::path::Path,
    _ofc: Option<&OpenFileCacheConfig>,
    _content_cfg: &StaticContentCacheConfig,
    containment: Option<RouteContainment<'_>>,
) -> Option<StaticFileOutcome> {
    let owned = path.to_path_buf();
    let owned_containment: Option<(Option<std::path::PathBuf>, std::path::PathBuf)> = containment
        .map(|c| {
            (
                c.canonical_base.map(std::path::Path::to_path_buf),
                c.root.to_path_buf(),
            )
        });
    // 理由付き allow: offload 専用ワーカースレッド内で実行、イベントループ非ブロック。
    #[allow(clippy::disallowed_methods)]
    crate::runtime::offload::offload(move || open_and_read_uncached(&owned, owned_containment))
        .await
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
