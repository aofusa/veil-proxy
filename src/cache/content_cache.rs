//! 静的ファイル本体のインメモリキャッシュ（F-146）
//!
//! HTTP/2（`proxy.rs` `h2_sendfile`）・HTTP/3（`http3_server.rs` `handle_sendfile`）の
//! 静的配信は、リクエストごとに `crate::runtime::offload::offload(std::fs::read)` で
//! ファイル全体を毎回読み直していた。DTrace 実測（FreeBSD、54KB ファイル）で
//! open/read/close のシステムコールに加え、専用オフロードスレッドプールへの
//! クロススレッド往復（`_umtx_op`）と新規 `Vec` 確保が固定コストとして支配的で
//! あることが判明した（詳細は `docs/backlog/features/F-146-static-content-cache.md`）。
//!
//! HTTP/2・HTTP/3 はレスポンスを DATA フレーム / QUIC ストリームへ再フレーミングする
//! 必要があるため `sendfile(2)` によるカーネル内ゼロコピーが使えない
//! （HTTP/1.1 は `sendfile`/kTLS splice を使うため本モジュールの対象外）。
//! そこで代わりに **ファイル本体をユーザ空間メモリ（`bytes::Bytes`）に保持し、
//! 参照カウントクローンで配信する** ことでオフロード往復自体を消し去る。
//!
//! ## ホットパス（キャッシュヒット時）
//!
//! `DashMap` ルックアップ + TTL 判定 + `Bytes::clone()`（参照カウント増加のみ）。
//! システムコール・メモリアロケーション・オフロードは一切発生しない。
//!
//! ## キャッシュミス時
//!
//! `offload(|| std::fs::read(..))` で一度だけ読み込み、上限（後述）に収まる
//! 場合のみ挿入する。収まらない場合は挿入せず、以降も毎回オフロード読み込みへ
//! フォールバックし続ける（安全側・キャッシュ本体は破損しない）。
//!
//! ## mtime 再検証（`revalidate_mtime`、デフォルト `false`）
//!
//! true にするとキャッシュヒット時に毎回 `stat` を実行して mtime を比較する
//! （オフロード往復が復活し、ホットパスに 1 syscall が戻る）。false のままだと
//! ファイル更新は TTL（`valid_duration_secs`）が切れるまで反映されない。
//! デフォルトは syscall ゼロを優先して `false` とし、即時反映が必要な運用でのみ
//! 明示的に有効化する trade-off とした。
//!
//! ## 容量超過時の挙動（LRU を実装しない理由）
//!
//! `max_total_bytes`（キャッシュ本体の合計バイト数）を超える挿入は単純に
//! **拒否**する（admission control のみ）。LRU エビクションはエントリ間の
//! 「最近使われた度合い」を継続的に追跡・比較するコストを伴うが、静的ファイル
//! キャッシュは少数の高頻度アクセスファイル（CSS/JS/画像等）を想定しており、
//! 単純な admission control で実運用上十分な効果が得られる一方、追加のロックや
//! アトミック操作をホットパスへ持ち込まずに済む。

use bytes::Bytes;
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// 静的コンテンツキャッシュの解決済み設定（TOML `[static_file_cache]` グローバル
/// セクションと同じ形。ルートごとの上書き適用後の値を保持する具象型）。
#[derive(Clone, Copy, Debug, serde::Deserialize)]
#[serde(default)]
pub struct StaticContentCacheConfig {
    /// キャッシュを有効化するか（デフォルト: 無効）
    pub enabled: bool,
    /// キャッシュエントリの有効期間（秒）
    pub valid_duration_secs: u64,
    /// 最大エントリ数
    pub max_entries: usize,
    /// キャッシュ対象とする最大ファイルサイズ（バイト）。超えるファイルは常に
    /// 直接読み込み（オフロード fallback）にフォールバックする。
    pub max_file_size_bytes: u64,
    /// キャッシュ本体（全エントリの `data` 合計）の最大バイト数
    pub max_total_bytes: u64,
    /// キャッシュヒット時に mtime を再検証するか（デフォルト: 無効、syscall ゼロ優先）
    pub revalidate_mtime: bool,
}

impl Default for StaticContentCacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            valid_duration_secs: 60,
            max_entries: 1024,
            max_file_size_bytes: 1024 * 1024,
            max_total_bytes: 64 * 1024 * 1024,
            revalidate_mtime: false,
        }
    }
}

/// ルートごとの静的コンテンツキャッシュ上書き設定（`[route.static_file_cache]`）。
/// 各フィールド `None` はグローバル設定（[`StaticContentCacheConfig`]）を使用する
/// ことを意味する。`OpenFileCacheConfig` と同じ設計。
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct StaticContentCacheRouteConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default, rename = "valid_duration_secs")]
    pub valid_duration_secs: Option<u64>,
    #[serde(default, rename = "max_entries")]
    pub max_entries: Option<usize>,
    #[serde(default, rename = "max_file_size_bytes")]
    pub max_file_size_bytes: Option<u64>,
    #[serde(default, rename = "max_total_bytes")]
    pub max_total_bytes: Option<u64>,
    #[serde(default, rename = "revalidate_mtime")]
    pub revalidate_mtime: Option<bool>,
}

/// グローバルデフォルト（ロックフリー atomic。`configure_global_open_file_cache` と
/// 同じ設計）。
static GLOBAL_ENABLED: AtomicBool = AtomicBool::new(false);
static GLOBAL_VALID_DURATION_SECS: AtomicU64 = AtomicU64::new(60);
static GLOBAL_MAX_ENTRIES: AtomicUsize = AtomicUsize::new(1024);
static GLOBAL_MAX_FILE_SIZE_BYTES: AtomicU64 = AtomicU64::new(1024 * 1024);
static GLOBAL_MAX_TOTAL_BYTES: AtomicU64 = AtomicU64::new(64 * 1024 * 1024);
static GLOBAL_REVALIDATE_MTIME: AtomicBool = AtomicBool::new(false);

/// グローバル静的コンテンツキャッシュ設定を適用（起動時・SIGHUP リロード時）。
pub fn configure_global_static_content_cache(cfg: &StaticContentCacheConfig) {
    GLOBAL_ENABLED.store(cfg.enabled, Ordering::Relaxed);
    GLOBAL_VALID_DURATION_SECS.store(cfg.valid_duration_secs, Ordering::Relaxed);
    GLOBAL_MAX_ENTRIES.store(cfg.max_entries, Ordering::Relaxed);
    GLOBAL_MAX_FILE_SIZE_BYTES.store(cfg.max_file_size_bytes, Ordering::Relaxed);
    GLOBAL_MAX_TOTAL_BYTES.store(cfg.max_total_bytes, Ordering::Relaxed);
    GLOBAL_REVALIDATE_MTIME.store(cfg.revalidate_mtime, Ordering::Relaxed);
}

/// ルート上書きとグローバル設定をマージして解決済み設定を返す（アロケーション無し）。
#[inline]
pub fn effective_static_content_cache_config(
    route: Option<&StaticContentCacheRouteConfig>,
) -> StaticContentCacheConfig {
    StaticContentCacheConfig {
        enabled: route
            .and_then(|r| r.enabled)
            .unwrap_or_else(|| GLOBAL_ENABLED.load(Ordering::Relaxed)),
        valid_duration_secs: route
            .and_then(|r| r.valid_duration_secs)
            .unwrap_or_else(|| GLOBAL_VALID_DURATION_SECS.load(Ordering::Relaxed)),
        max_entries: route
            .and_then(|r| r.max_entries)
            .unwrap_or_else(|| GLOBAL_MAX_ENTRIES.load(Ordering::Relaxed)),
        max_file_size_bytes: route
            .and_then(|r| r.max_file_size_bytes)
            .unwrap_or_else(|| GLOBAL_MAX_FILE_SIZE_BYTES.load(Ordering::Relaxed)),
        max_total_bytes: route
            .and_then(|r| r.max_total_bytes)
            .unwrap_or_else(|| GLOBAL_MAX_TOTAL_BYTES.load(Ordering::Relaxed)),
        revalidate_mtime: route
            .and_then(|r| r.revalidate_mtime)
            .unwrap_or_else(|| GLOBAL_REVALIDATE_MTIME.load(Ordering::Relaxed)),
    }
}

/// キャッシュされたファイル本体
struct CachedContent {
    /// ファイル本体（参照カウント共有バッファ）
    data: Bytes,
    /// バイト数（`data.len()` と同じだが admission control で頻繁に読むため保持）
    len: u64,
    /// 読み込み時点の mtime（`revalidate_mtime` 用。取得しない場合は `None`）
    mtime: Option<SystemTime>,
    /// MIME タイプ（読み込み時に一度だけ推測し、ヒット時に使い回す）
    mime_type: Arc<str>,
    /// キャッシュ時刻
    cached_at: Instant,
}

struct ContentCache {
    entries: DashMap<PathBuf, CachedContent>,
    /// 現在キャッシュされている `data` の合計バイト数（admission control 用）
    total_bytes: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl ContentCache {
    fn new() -> Self {
        Self {
            entries: DashMap::with_capacity(256),
            total_bytes: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// 上限を満たす場合のみ挿入する（admission control）。既存キーへの上書きの
    /// 場合は古いエントリ分を先に `total_bytes` から差し引いて判定する。
    fn try_insert(&self, path: &Path, content: CachedContent, cfg: &StaticContentCacheConfig) {
        let new_len = content.len;
        // **`entry()` を呼ぶ前にエントリ数を読む**こと。`DashMap::entry()` は該当シャードの
        // write ロックを取得し、返り値の `Entry` が生きている間はそれを保持し続ける。
        // その状態で `DashMap::len()` を呼ぶと全シャードを read ロックしに行くため、
        // 既に write ロック済みの同一シャードで**自己デッドロック**する（RwLock は再入不可）。
        // 実測でユニットテストが 60 秒超ハングして発覚した。キャッシュ有効時は最初のミス
        // 挿入で必ず通る経路のため、本番でも同じくハングする（既定オフのため E2E では
        // 顕在化しなかった）。
        let current_entries = self.entries.len();
        match self.entries.entry(path.to_path_buf()) {
            Entry::Occupied(mut o) => {
                let old_len = o.get().len;
                let current = self.total_bytes.load(Ordering::Relaxed);
                let projected = current.saturating_sub(old_len).saturating_add(new_len);
                if projected > cfg.max_total_bytes {
                    // 上限超過: 既存の（古い）エントリを維持し、新しいデータは
                    // キャッシュせず呼び出し元へそのまま返す（呼び出し元は既に
                    // 読み込み済みのため配信自体は成功する）。
                    return;
                }
                self.total_bytes.fetch_sub(old_len, Ordering::Relaxed);
                self.total_bytes.fetch_add(new_len, Ordering::Relaxed);
                o.insert(content);
                // F-169: 本体を上書きした（TTL 失効・mtime 再検証いずれの経路でも
                // ここを通る）ので、古い内容に対する圧縮結果を必ず道連れにする。
                // ここを通さず TTL だけに委ねると、`revalidate_mtime = true` の
                // 構成で「本体は即時反映されるが圧縮結果だけ古いまま」という
                // 不整合を生む（本改修で最も壊しやすい点）。
                super::compressed::invalidate(path);
            }
            Entry::Vacant(v) => {
                if current_entries >= cfg.max_entries {
                    return;
                }
                let projected = self
                    .total_bytes
                    .load(Ordering::Relaxed)
                    .saturating_add(new_len);
                if projected > cfg.max_total_bytes {
                    return;
                }
                self.total_bytes.fetch_add(new_len, Ordering::Relaxed);
                v.insert(content);
            }
        }
    }

    fn invalidate(&self, path: &Path) {
        if let Some((_, old)) = self.entries.remove(path) {
            self.total_bytes.fetch_sub(old.len, Ordering::Relaxed);
        }
        // F-169: 本体キャッシュの明示的な無効化（例: 読み込み失敗時の 404 フォール
        // バック）は、同じパスの圧縮結果も道連れにしないと「古い圧縮結果を返し続ける」
        // 不整合を生む。`super::compressed::invalidate` はパス未登録でも副作用が
        // 無いため、本体側に該当エントリが無かった場合も安全に呼べる。
        super::compressed::invalidate(path);
    }

    fn clear(&self) {
        self.entries.clear();
        self.total_bytes.store(0, Ordering::Relaxed);
        super::compressed::clear();
    }

    /// キャッシュヒット判定のみを行う共通ヘルパ（ロードは一切行わない）。
    ///
    /// `get_or_load_with_mime`（ロード込み版）と `get_cached`（ヒット限定版、F-150）の
    /// 両方から呼ばれる、ヒット可否判定ロジックの単一の実装箇所。ヒット時はホットパス
    /// 規則どおり syscall・アロケーション・オフロードを一切行わず `Bytes::clone()`
    /// （参照カウント増加）のみで返す（`revalidate_mtime` 有効時のみ例外的に stat が入る）。
    async fn try_hit(
        &self,
        path: &Path,
        cfg: &StaticContentCacheConfig,
    ) -> Option<(Bytes, Arc<str>)> {
        if !cfg.enabled {
            return None;
        }
        let entry = self.entries.get(path)?;
        let ttl = Duration::from_secs(cfg.valid_duration_secs);
        if entry.cached_at.elapsed() >= ttl {
            return None;
        }
        if !cfg.revalidate_mtime {
            // ホットパス: DashMap ルックアップ + TTL 判定 + Bytes::clone() のみ。
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Some((entry.data.clone(), entry.mime_type.clone()));
        }
        // revalidate_mtime = true: 明示的に選択された trade-off として毎回 stat を実行する
        // （モジュール doc 参照）。
        let stat_path = path.to_path_buf();
        let cached_mtime = entry.mtime;
        let cached_data = entry.data.clone();
        let cached_mime = entry.mime_type.clone();
        drop(entry);
        // 理由付き allow: offload 専用ワーカースレッド内で実行、イベントループ非ブロック。
        #[allow(clippy::disallowed_methods)]
        let current_mtime = crate::runtime::offload::offload(move || {
            std::fs::metadata(&stat_path)
                .and_then(|m| m.modified())
                .ok()
        })
        .await;
        if current_mtime == cached_mtime {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Some((cached_data, cached_mime));
        }
        // mtime 不一致: 陳腐化しているため呼び出し元は再読み込みへフォールスルーする。
        None
    }

    /// ファイル本体をキャッシュ優先で取得する（インスタンスメソッド版）。
    ///
    /// キャッシュヒット時はホットパス規則どおり syscall・アロケーション・オフロードを
    /// 一切行わず `Bytes::clone()`（参照カウント増加）のみで返す。ミス・無効・
    /// サイズ超過時は `offload(std::fs::read)` で一度だけ読み込む。
    ///
    /// `mime_fallback` は呼び出し元が既に MIME タイプを把握している場合
    /// （`cache::get_file_info_with_config` 経由等）にそれを渡すことで、キャッシュ
    /// ミス時にも `mime_guess` の再計算を避けられる。キャッシュヒット時は
    /// `mime_fallback` は一切呼び出されない。
    async fn get_or_load_with_mime<F>(
        &self,
        path: &Path,
        cfg: &StaticContentCacheConfig,
        mime_fallback: F,
    ) -> Option<(Bytes, Arc<str>)>
    where
        F: FnOnce() -> Arc<str>,
    {
        if let Some(hit) = self.try_hit(path, cfg).await {
            return Some(hit);
        }

        self.misses.fetch_add(1, Ordering::Relaxed);

        let load_path = path.to_path_buf();
        // 理由付き allow: offload 専用ワーカースレッド内で実行、イベントループ非ブロック
        // （F-146: HTTP/2・HTTP/3 静的配信の既存 offload 読み込みと同じ許容箇所）。
        #[allow(clippy::disallowed_methods)]
        let read_result = crate::runtime::offload::offload(move || std::fs::read(&load_path)).await;
        let raw = read_result.ok()?;
        let len = raw.len() as u64;
        let data = Bytes::from(raw);
        let mime = mime_fallback();

        if cfg.enabled && len <= cfg.max_file_size_bytes {
            let mtime = if cfg.revalidate_mtime {
                let stat_path = path.to_path_buf();
                // 理由付き allow: offload 専用ワーカースレッド内。
                #[allow(clippy::disallowed_methods)]
                crate::runtime::offload::offload(move || {
                    std::fs::metadata(&stat_path)
                        .and_then(|m| m.modified())
                        .ok()
                })
                .await
            } else {
                None
            };
            self.try_insert(
                path,
                CachedContent {
                    data: data.clone(),
                    len,
                    mtime,
                    mime_type: mime.clone(),
                    cached_at: Instant::now(),
                },
                cfg,
            );
        }

        Some((data, mime))
    }

    /// キャッシュヒット時のみ内容を返す（F-150）。ミス時は一切ロードしない
    /// （呼び出し元が既に開いている fd から自前で読み込み、`insert_bytes` で登録する
    /// 設計を前提とする。HTTP/1.1 のユーザー空間 TLS 静的配信向け）。
    async fn get_cached(&self, path: &Path, cfg: &StaticContentCacheConfig) -> Option<Bytes> {
        match self.try_hit(path, cfg).await {
            Some((data, _mime)) => Some(data),
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// 既に読み込み済みのバイト列をキャッシュへ登録する（F-150）。
    ///
    /// 上限判定（`cfg.enabled && len <= cfg.max_file_size_bytes`）は
    /// `get_or_load_with_mime` のミス時挿入条件と同一にする。`try_insert` は
    /// `DashMap::entry()` 保持中に `len()` を呼ばないよう構成済み（F-146 で修正済みの
    /// 自己デッドロックを再導入しない）。
    ///
    /// `revalidate_mtime` が有効な場合、本来は挿入時点の mtime を stat して保持すべきだが、
    /// このメソッドは「既に fd を開いている呼び出し元」向けの同期的な登録専用 API であり、
    /// 追加の offload 往復（stat）を新設しない。mtime は `None` のまま保持されるため、
    /// `revalidate_mtime = true` の構成では次回アクセス時に必ず再検証ミスとなり事実上
    /// キャッシュが効かなくなるが、内容の正しさには影響しない
    /// （`revalidate_mtime` は既定 `false` であり、この trade-off が効くのはこの経路を
    /// 明示的に選んだ場合のみ）。
    fn insert_bytes(
        &self,
        path: &Path,
        data: Bytes,
        mime: Arc<str>,
        cfg: &StaticContentCacheConfig,
    ) {
        let len = data.len() as u64;
        if !cfg.enabled || len > cfg.max_file_size_bytes {
            return;
        }
        self.try_insert(
            path,
            CachedContent {
                data,
                len,
                mtime: None,
                mime_type: mime,
                cached_at: Instant::now(),
            },
            cfg,
        );
    }
}

static CONTENT_CACHE: Lazy<ContentCache> = Lazy::new(ContentCache::new);

/// MIME タイプ推測（キャッシュミス時のデフォルトフォールバック）。
fn guess_mime(path: &Path) -> Arc<str> {
    Arc::from(mime_guess::from_path(path).first_or_octet_stream().as_ref())
}

/// ファイル本体をキャッシュ優先で取得する（グローバルシングルトン、`mime_guess`
/// フォールバック版）。詳細はモジュール doc および
/// [`ContentCache::get_or_load_with_mime`] を参照。
pub async fn get_or_load(path: &Path, cfg: &StaticContentCacheConfig) -> Option<Bytes> {
    // フォールバックの MIME 推定用クロージャは `path` を**借用**する（`to_path_buf()` で
    // 事前に所有権を取ると、キャッシュヒット時にも 1 リクエストあたり PathBuf の
    // ヒープ確保が発生し「ヒット時はゼロアロケーション」という本キャッシュの前提が
    // 崩れる）。クロージャはミス時にしか呼ばれない。
    CONTENT_CACHE
        .get_or_load_with_mime(path, cfg, || guess_mime(path))
        .await
        .map(|(data, _mime)| data)
}

/// [`get_or_load`] の MIME タイプ再利用版（グローバルシングルトン）。
pub async fn get_or_load_with_mime<F>(
    path: &Path,
    cfg: &StaticContentCacheConfig,
    mime_fallback: F,
) -> Option<(Bytes, Arc<str>)>
where
    F: FnOnce() -> Arc<str>,
{
    CONTENT_CACHE
        .get_or_load_with_mime(path, cfg, mime_fallback)
        .await
}

/// キャッシュヒット時のみ内容を返す（グローバルシングルトン、F-150）。
/// ミス時は一切ロードしない。詳細は [`ContentCache::get_cached`] 参照。
pub async fn get_cached(path: &Path, cfg: &StaticContentCacheConfig) -> Option<Bytes> {
    CONTENT_CACHE.get_cached(path, cfg).await
}

/// 既に読み込み済みのバイト列をキャッシュへ登録する（グローバルシングルトン、F-150）。
/// 詳細は [`ContentCache::insert_bytes`] 参照。
pub fn insert_bytes(path: &Path, data: Bytes, mime: Arc<str>, cfg: &StaticContentCacheConfig) {
    CONTENT_CACHE.insert_bytes(path, data, mime, cfg);
}

/// 指定パスのキャッシュエントリを無効化する（読み込み失敗時等に既存の
/// `cache::invalidate_file_cache` と併せて呼び出す）。
#[inline]
pub fn invalidate(path: &Path) {
    CONTENT_CACHE.invalidate(path);
}

/// キャッシュを全クリアする。
#[inline]
pub fn clear() {
    CONTENT_CACHE.clear();
}

/// キャッシュヒット数を取得
#[inline]
pub fn hits() -> u64 {
    CONTENT_CACHE.hits.load(Ordering::Relaxed)
}

/// キャッシュミス数を取得
#[inline]
pub fn misses() -> u64 {
    CONTENT_CACHE.misses.load(Ordering::Relaxed)
}

/// 現在のエントリ数を取得
#[inline]
pub fn len() -> usize {
    CONTENT_CACHE.entries.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn test_cfg() -> StaticContentCacheConfig {
        StaticContentCacheConfig {
            enabled: true,
            valid_duration_secs: 60,
            max_entries: 1024,
            max_file_size_bytes: 1024 * 1024,
            max_total_bytes: 64 * 1024 * 1024,
            revalidate_mtime: false,
        }
    }

    fn write_file(dir: &tempfile::TempDir, name: &str, content: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content).unwrap();
        path
    }

    // 各テストは（並行実行される cargo test でも競合しないよう）グローバル
    // シングルトン `CONTENT_CACHE` を共有せず、ローカルな `ContentCache` インスタンスを
    // 使う（file_cache.rs のテストと同じ方針）。
    /// テスト用の同期ラッパ。
    ///
    /// **新しいスレッドを立てて、その上で待つ**のが要点。ミス時の読み込みは
    /// `runtime::offload` を経由するが、`offload` は「そのスレッドにランタイム
    /// ドライバがあるか」（`has_driver()`、**スレッドローカル**）で分岐する:
    ///
    /// - ドライバ有り → ジョブをプールへ投げ、完了を POLL_ADD 等で**非同期に**待つ。
    /// - ドライバ無し → その場で同期実行する（`offload` の doc 参照。単体テスト向け経路）。
    ///
    /// cargo test は複数のテストを同一スレッドで順に走らせるため、先行テストが
    /// そのスレッドで io_uring リングを初期化していると `has_driver()` が true になり、
    /// `futures::executor::block_on` では駆動できない非同期待機に入って**永久に
    /// ハングする**（実測: 3 テストが 60 秒超で応答しなくなった）。かといって
    /// `crate::runtime::block_on` は `'static` な Future を要求するため、借用を
    /// 含む本ヘルパでは使えない。
    ///
    /// 新規スレッドはリング未初期化が保証されるので `has_driver()` は必ず false になり、
    /// `offload` は同期実行へ落ちて `futures::executor::block_on` が即座に完了する。
    /// `std::thread::scope` を使うことで借用したまま実行できる。
    fn block_on_fresh_thread<T, F>(fut_fn: F) -> T
    where
        F: FnOnce() -> T + Send,
        T: Send,
    {
        std::thread::scope(|s| s.spawn(fut_fn).join().expect("test thread panicked"))
    }

    fn get(cache: &ContentCache, path: &Path, cfg: &StaticContentCacheConfig) -> Option<Bytes> {
        block_on_fresh_thread(|| {
            futures::executor::block_on(
                cache.get_or_load_with_mime(path, cfg, || Arc::from("application/octet-stream")),
            )
            .map(|(data, _mime)| data)
        })
    }

    #[test]
    fn hit_and_miss() {
        let cache = ContentCache::new();
        let dir = tempdir().unwrap();
        let path = write_file(&dir, "a.txt", b"hello world");
        let cfg = test_cfg();

        let first = get(&cache, &path, &cfg);
        assert_eq!(first.as_deref(), Some(&b"hello world"[..]));
        assert_eq!(cache.misses.load(Ordering::Relaxed), 1);
        assert_eq!(cache.hits.load(Ordering::Relaxed), 0);

        let second = get(&cache, &path, &cfg);
        assert_eq!(second.as_deref(), Some(&b"hello world"[..]));
        assert_eq!(cache.misses.load(Ordering::Relaxed), 1);
        assert_eq!(cache.hits.load(Ordering::Relaxed), 1);

        // 参照カウント共有であることを確認（同一バイト列を指す）
        assert_eq!(first.unwrap(), second.unwrap());
    }

    #[test]
    fn disabled_never_caches() {
        let cache = ContentCache::new();
        let dir = tempdir().unwrap();
        let path = write_file(&dir, "b.txt", b"data");
        let mut cfg = test_cfg();
        cfg.enabled = false;

        let _ = get(&cache, &path, &cfg);
        let _ = get(&cache, &path, &cfg);
        assert_eq!(cache.entries.len(), 0, "無効時はキャッシュへ挿入しない");
    }

    #[test]
    fn ttl_expiry_forces_reload() {
        let cache = ContentCache::new();
        let dir = tempdir().unwrap();
        let path = write_file(&dir, "c.txt", b"v1");
        let mut cfg = test_cfg();
        cfg.valid_duration_secs = 0; // 即座に期限切れ扱い

        let _ = get(&cache, &path, &cfg);
        assert_eq!(cache.misses.load(Ordering::Relaxed), 1);
        // 有効期間0秒なので次回アクセスは必ずミス（TTL切れ）扱いになる
        // 理由付き allow: 単体テストのみ（ホットパス外）。TTL 経過を確実にするための待機。
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(Duration::from_millis(5));
        let _ = get(&cache, &path, &cfg);
        assert_eq!(cache.misses.load(Ordering::Relaxed), 2);
        assert_eq!(cache.hits.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn max_file_size_rejects_large_files() {
        let cache = ContentCache::new();
        let dir = tempdir().unwrap();
        let path = write_file(&dir, "big.bin", &[0u8; 4096]);
        let mut cfg = test_cfg();
        cfg.max_file_size_bytes = 1024; // ファイルより小さい上限

        let data = get(&cache, &path, &cfg);
        assert_eq!(
            data.map(|d| d.len()),
            Some(4096),
            "上限超過でも読み込み自体は成功する"
        );
        assert_eq!(
            cache.entries.len(),
            0,
            "上限を超えるファイルはキャッシュに挿入されない"
        );

        // 2回目も常にミス（キャッシュされていないため）
        let _ = get(&cache, &path, &cfg);
        assert_eq!(cache.misses.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn max_total_bytes_admission_control() {
        let cache = ContentCache::new();
        let dir = tempdir().unwrap();
        let path_a = write_file(&dir, "x.bin", &[1u8; 100]);
        let path_b = write_file(&dir, "y.bin", &[2u8; 100]);
        let mut cfg = test_cfg();
        cfg.max_total_bytes = 150; // 両方は入らない

        let _ = get(&cache, &path_a, &cfg);
        assert_eq!(cache.entries.len(), 1);
        let _ = get(&cache, &path_b, &cfg);
        // 合計上限を超えるため2件目は挿入されない
        assert_eq!(
            cache.entries.len(),
            1,
            "max_total_bytes を超える挿入は拒否される"
        );
    }

    #[test]
    fn invalidate_removes_entry_and_frees_budget() {
        let cache = ContentCache::new();
        let dir = tempdir().unwrap();
        let path = write_file(&dir, "d.txt", b"payload");
        let cfg = test_cfg();

        let _ = get(&cache, &path, &cfg);
        assert_eq!(cache.entries.len(), 1);

        cache.invalidate(&path);
        assert_eq!(cache.entries.len(), 0);
        assert_eq!(cache.total_bytes.load(Ordering::Relaxed), 0);

        let initial_misses = cache.misses.load(Ordering::Relaxed);
        let _ = get(&cache, &path, &cfg);
        assert_eq!(cache.misses.load(Ordering::Relaxed), initial_misses + 1);
    }

    /// F-169: 本体キャッシュの明示的な `invalidate` は、同じパスの圧縮結果キャッシュ
    /// （`super::compressed`）も道連れに破棄すること。本改修で最も壊しやすい点。
    ///
    /// `cache::compressed` はグローバルシングルトンなので、他のテストと衝突しない
    /// よう一時ディレクトリ由来のユニークなパスをキーに使う。
    // グローバル `COMPRESSED_CACHE`（src/cache/compressed.rs）を経由するため、並列
    // 実行だと他テスト（`public_api_smoke_test` の `clear()` 等）に割り込まれて
    // フレーキーになる。`content_cache.rs`/`compressed.rs` 全体で同じ既定グループ
    // （引数なし `#[serial]`）に入れ、モジュールをまたいで直列化する。
    #[test]
    #[serial_test::serial]
    fn invalidate_also_drops_compressed_cache_variants() {
        let cache = ContentCache::new();
        let dir = tempdir().unwrap();
        let path = write_file(&dir, "f169_invalidate.txt", b"payload");
        let cfg = test_cfg();

        let _ = get(&cache, &path, &cfg);

        // 圧縮結果キャッシュへ登録する（実運用ではプロキシ/HTTP3 層が
        // `cache::compressed::get_or_compress` 経由で行う）。
        let calls = AtomicUsize::new(0);
        let first = super::super::compressed::get_or_compress(
            &path,
            crate::config::AcceptedEncoding::Zstd,
            3,
            &cfg,
            || {
                calls.fetch_add(1, Ordering::Relaxed);
                b"compressed-v1".to_vec()
            },
        );
        assert_eq!(first, Bytes::from_static(b"compressed-v1"));
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        // ヒット確認（再圧縮されない）。
        let hit = super::super::compressed::get_or_compress(
            &path,
            crate::config::AcceptedEncoding::Zstd,
            3,
            &cfg,
            || {
                calls.fetch_add(1, Ordering::Relaxed);
                b"should-not-run".to_vec()
            },
        );
        assert_eq!(hit, first);
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        // 本体キャッシュを明示的に無効化する（例: 読み込み失敗時の 404 フォール
        // バックが呼ぶ `cache::invalidate_content_cache` と同じ経路）。
        cache.invalidate(&path);

        // 圧縮結果キャッシュも道連れに破棄されているはずなので、再度呼ぶと
        // 圧縮クロージャが再実行される。
        let second = super::super::compressed::get_or_compress(
            &path,
            crate::config::AcceptedEncoding::Zstd,
            3,
            &cfg,
            || {
                calls.fetch_add(1, Ordering::Relaxed);
                b"compressed-v2".to_vec()
            },
        );
        assert_eq!(second, Bytes::from_static(b"compressed-v2"));
        assert_eq!(
            calls.load(Ordering::Relaxed),
            2,
            "invalidate 後は圧縮結果キャッシュも破棄され、再圧縮されること"
        );
    }

    /// F-169: `revalidate_mtime = true` で本体が mtime 不一致によりリロード
    /// （TTL はまだ切れていない）された場合も、圧縮結果キャッシュが道連れに
    /// 破棄されること。TTL だけに委ねていた場合に見逃す不整合
    /// （本体は即時反映されるが圧縮結果だけ古いまま）を防いでいることの確認。
    // グローバル `COMPRESSED_CACHE` を経由するため #[serial]（理由は上の
    // `invalidate_also_drops_compressed_cache_variants` と同じ）。
    #[test]
    #[serial_test::serial]
    fn mtime_revalidation_reload_also_invalidates_compressed_cache() {
        let cache = ContentCache::new();
        let dir = tempdir().unwrap();
        let path = write_file(&dir, "f169_mtime.txt", b"v1");
        let mut cfg = test_cfg();
        cfg.revalidate_mtime = true;
        // TTL 自体は長いままにしておく（TTL 失効ではなく mtime 変化のみで
        // 本体が入れ替わることを確認するため）。
        cfg.valid_duration_secs = 60;

        let _ = get(&cache, &path, &cfg);

        let calls = AtomicUsize::new(0);
        let first = super::super::compressed::get_or_compress(
            &path,
            crate::config::AcceptedEncoding::Zstd,
            3,
            &cfg,
            || {
                calls.fetch_add(1, Ordering::Relaxed);
                b"compressed-v1".to_vec()
            },
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        // TTL がまだ残っている間はヒットする（再圧縮されない）ことも確認。
        let hit_before = super::super::compressed::get_or_compress(
            &path,
            crate::config::AcceptedEncoding::Zstd,
            3,
            &cfg,
            || {
                calls.fetch_add(1, Ordering::Relaxed);
                b"should-not-run".to_vec()
            },
        );
        assert_eq!(hit_before, first);
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        // mtime を確実に進めるため 1.1 秒待ってから書き換える（秒単位分解能の
        // ファイルシステムでも変化を保証する。`tests/integration_tests.rs` の
        // `test_cert_mtime_changes_after_update` と同じ手法）。
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(Duration::from_millis(1100));
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(b"v2").unwrap();
        }

        // revalidate_mtime = true なので、TTL が残っていても mtime 不一致で
        // リロードされる。
        let reloaded = get(&cache, &path, &cfg);
        assert_eq!(reloaded.as_deref(), Some(&b"v2"[..]));

        // 圧縮結果キャッシュも道連れに破棄されているはず。
        let second = super::super::compressed::get_or_compress(
            &path,
            crate::config::AcceptedEncoding::Zstd,
            3,
            &cfg,
            || {
                calls.fetch_add(1, Ordering::Relaxed);
                b"compressed-v2".to_vec()
            },
        );
        assert_eq!(second, Bytes::from_static(b"compressed-v2"));
        assert_eq!(
            calls.load(Ordering::Relaxed),
            2,
            "mtime 再検証によるリロード後は圧縮結果も再計算されること"
        );
    }

    /// F-150: `get_cached` はミス時にロードしないこと
    /// （存在するファイルパスを渡してもキャッシュに未登録なら `None` を返し、
    /// キャッシュ件数・ミスカウンタ以外の状態が増えないこと）。
    #[test]
    fn get_cached_does_not_load_on_miss() {
        let cache = ContentCache::new();
        let dir = tempdir().unwrap();
        // 実在するファイルを用意するが、一度もロード/挿入していない。
        let path = write_file(&dir, "uncached.txt", b"exists but not cached");
        let cfg = test_cfg();

        let result =
            block_on_fresh_thread(|| futures::executor::block_on(cache.get_cached(&path, &cfg)));
        assert_eq!(result, None, "未登録なら実ファイルが存在してもミスになる");
        assert_eq!(
            cache.entries.len(),
            0,
            "get_cached はミス時にロード・挿入を一切行わない"
        );
    }

    /// F-150: `insert_bytes` → `get_cached` のラウンドトリップ。
    #[test]
    fn insert_bytes_then_get_cached_round_trip() {
        let cache = ContentCache::new();
        let dir = tempdir().unwrap();
        // insert_bytes はパスをキーとしてのみ使うため、実ファイルの有無は問わない
        // （呼び出し元が既に読み込み済みのバイト列を渡す設計のため）。
        let path = dir.path().join("in_memory_only.bin");
        let cfg = test_cfg();
        let payload = Bytes::from_static(b"round trip payload");
        let mime: Arc<str> = Arc::from("text/plain");

        cache.insert_bytes(&path, payload.clone(), mime.clone(), &cfg);

        let hit =
            block_on_fresh_thread(|| futures::executor::block_on(cache.get_cached(&path, &cfg)));
        assert_eq!(
            hit,
            Some(payload),
            "insert_bytes で登録した内容がそのまま返る"
        );
    }

    /// F-150: `insert_bytes` は `max_file_size_bytes` 超過を拒否する。
    #[test]
    fn insert_bytes_rejects_oversized_payload() {
        let cache = ContentCache::new();
        let dir = tempdir().unwrap();
        let path = dir.path().join("too_big.bin");
        let mut cfg = test_cfg();
        cfg.max_file_size_bytes = 4; // payload より小さい上限

        cache.insert_bytes(
            &path,
            Bytes::from_static(b"too large"),
            Arc::from("text/plain"),
            &cfg,
        );

        assert_eq!(cache.entries.len(), 0, "上限を超えるバイト列は挿入されない");
        let hit =
            block_on_fresh_thread(|| futures::executor::block_on(cache.get_cached(&path, &cfg)));
        assert_eq!(hit, None);
    }

    /// F-150: `cfg.enabled = false` のとき `insert_bytes` は登録しない。
    #[test]
    fn insert_bytes_noop_when_disabled() {
        let cache = ContentCache::new();
        let dir = tempdir().unwrap();
        let path = dir.path().join("disabled.bin");
        let mut cfg = test_cfg();
        cfg.enabled = false;

        cache.insert_bytes(
            &path,
            Bytes::from_static(b"data"),
            Arc::from("text/plain"),
            &cfg,
        );

        assert_eq!(cache.entries.len(), 0, "無効時は insert_bytes も何もしない");
    }

    /// グローバルシングルトン経由の公開 API（`get_or_load`/`invalidate`/`clear`）が
    /// 正しく配線されていることの smoke test。
    // グローバル `CONTENT_CACHE`/`COMPRESSED_CACHE` を直接 `clear()` するため、
    // 並列実行では他テストのキャッシュ状態を消してしまう（逆に他テスト側からも
    // ここへ割り込まれる）。#[serial] で直列化する。
    #[test]
    #[serial_test::serial]
    fn public_api_smoke_test() {
        clear();
        let dir = tempdir().unwrap();
        let path = write_file(&dir, "smoke.txt", b"ok");
        let cfg = test_cfg();

        // 同上（`block_on_fresh_thread` の doc 参照）。
        let data = block_on_fresh_thread(|| futures::executor::block_on(get_or_load(&path, &cfg)));
        assert_eq!(data.as_deref(), Some(&b"ok"[..]));
        assert!(len() >= 1);

        invalidate(&path);
        clear();
        assert_eq!(len(), 0);
    }
}
