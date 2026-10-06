//! ファイル情報キャッシュ（OpenFileCache）
//!
//! SendFile処理における頻繁なファイルシステムコール（canonicalize、exists、metadata）を
//! 削減するためのファイルメタデータキャッシュを提供します。
//!
//! Nginxの`open_file_cache`に相当する機能で、以下を実現します:
//! - パス正規化結果のキャッシュ
//! - ファイルメタデータのキャッシュ
//! - MIMEタイプのキャッシュ
//!
//! ## パフォーマンス効果
//!
//! 1リクエストあたり3〜6回のシステムコールを1回に削減可能

use dashmap::DashMap;
use once_cell::sync::Lazy;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// ファイル情報キャッシュのグローバルインスタンス
static OPEN_FILE_CACHE: Lazy<OpenFileCache> = Lazy::new(OpenFileCache::new);

/// OpenFileCacheの設定（ルーティングごと）
#[derive(Clone, Debug, serde::Deserialize)]
pub struct OpenFileCacheConfig {
    /// 有効化フラグ（Noneの場合はグローバル設定を使用）
    #[serde(default)]
    pub enabled: Option<bool>,
    /// 有効期間（Noneの場合はグローバル設定を使用）
    #[serde(default, rename = "valid_duration_secs")]
    pub valid_duration_secs: Option<u64>,
    /// 最大エントリ数（Noneの場合はグローバル設定を使用）
    #[serde(default, rename = "max_entries")]
    pub max_entries: Option<usize>,
}

/// OpenFileCacheのグローバル設定（デフォルト値）
static OPEN_FILE_CACHE_GLOBAL_ENABLED: AtomicBool = AtomicBool::new(false);
/// グローバル有効期間（ナノ秒、ロックフリー）。デフォルト 60 秒。
static OPEN_FILE_CACHE_GLOBAL_VALID_DURATION_NANOS: AtomicU64 = AtomicU64::new(60_000_000_000);
/// グローバル最大エントリ数（ロックフリー）。デフォルト 10000。
static OPEN_FILE_CACHE_GLOBAL_MAX_ENTRIES: AtomicUsize = AtomicUsize::new(10_000);

/// グローバルOpenFileCache設定を適用
pub fn configure_global_open_file_cache(
    enabled: bool,
    valid_duration_secs: u64,
    max_entries: usize,
) {
    OPEN_FILE_CACHE_GLOBAL_ENABLED.store(enabled, Ordering::Relaxed);
    let valid_nanos = valid_duration_secs.saturating_mul(1_000_000_000);
    OPEN_FILE_CACHE_GLOBAL_VALID_DURATION_NANOS.store(valid_nanos, Ordering::Relaxed);
    OPEN_FILE_CACHE_GLOBAL_MAX_ENTRIES.store(max_entries, Ordering::Relaxed);

    // キャッシュ本体の設定も更新（すべてロックフリー atomic）
    OPEN_FILE_CACHE
        .valid_duration_nanos
        .store(valid_nanos, Ordering::Relaxed);
    OPEN_FILE_CACHE
        .max_entries
        .store(max_entries, Ordering::Relaxed);
}

/// キャッシュされたファイル情報
#[derive(Clone, Debug)]
pub struct CachedFileInfo {
    /// 正規化されたパス（canonicalize結果）。キャッシュヒットのたびに複製されるため
    /// `Arc` で共有する（以前は `PathBuf` / `String` をヒットごとにディープコピーしていた）。
    pub canonical_path: std::sync::Arc<Path>,
    /// ファイルサイズ（バイト）
    pub file_size: u64,
    /// MIMEタイプ文字列（同上の理由で `Arc` 共有）
    pub mime_type: std::sync::Arc<str>,
    /// 最終更新時刻
    pub last_modified: Option<SystemTime>,
    /// ファイルかどうか（ディレクトリでない）
    pub is_file: bool,
    /// メタデータ取得時に開いた fd（通常ファイルのみ）。nginx の `open_file_cache` と同じく
    /// キャッシュの有効期間中は fd を保持し、HTTP/1.1 静的配信の open/close を毎リクエスト
    /// 行わずに済ませる（FreeBSD の DTrace 実測で openat + close が 2 syscall/req）。
    /// 読み取りは `pread`/`sendfile` のオフセット指定 API だけなので共有しても安全。
    shared_file: Option<std::sync::Arc<std::fs::File>>,
    /// キャッシュ時刻
    cached_at: Instant,
}

impl CachedFileInfo {
    /// 開いた File の `Metadata` から直接構築する（B-65 専用）。
    ///
    /// `cache::get_static_file_with_content` の高速経路（登録済みルート配下で
    /// open+fstat+read を 1 回の offload にまとめる経路）が、既に取得済みの
    /// `Metadata` からキャッシュエントリを組み立てるために使う。`canonical_path` は
    /// `fetch_file_info` の登録済みルート経路と同じく「開き直し不要な生パス」を
    /// そのまま使う（F-153 と同じ設計、コメント参照）。
    ///
    /// この高速経路は Linux/FreeBSD にしか存在しないため、それ以外のターゲットでは
    /// 呼び出し元ごと存在しなくなる。cfg で本関数自体の存在を絞ることで unused
    /// warning を解消する（`#[allow(dead_code)]` は規約で禁止）。
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub(crate) fn from_open_metadata(path: &Path, meta: &std::fs::Metadata) -> Self {
        let mime_type: std::sync::Arc<str> = mime_guess::from_path(path)
            .first_or_octet_stream()
            .as_ref()
            .into();
        Self {
            canonical_path: path.into(),
            file_size: meta.len(),
            mime_type,
            last_modified: meta.modified().ok(),
            is_file: meta.is_file(),
            shared_file: None,
            cached_at: Instant::now(),
        }
    }

    /// メタデータ取得時に開いた fd（キャッシュと共有）。無ければ呼び出し側が開く。
    #[inline]
    pub fn shared_file(&self) -> Option<&std::sync::Arc<std::fs::File>> {
        self.shared_file.as_ref()
    }

    /// キャッシュが有効かどうかをチェック
    #[inline]
    pub fn is_valid(&self, max_age: Duration) -> bool {
        self.cached_at.elapsed() < max_age
    }

    /// HTTP Last-Modified ヘッダー用のRFC 7231形式文字列を生成
    pub fn last_modified_rfc7231(&self) -> Option<String> {
        self.last_modified.map(|time| {
            use std::time::UNIX_EPOCH;
            let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
            let secs = duration.as_secs();

            // 簡易的なRFC 7231フォーマット（曜日と月は英語固定）
            let days_since_epoch = secs / 86400;
            let day_of_week = (days_since_epoch + 4) % 7; // 1970-01-01 was Thursday (4)
            let weekdays = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
            let weekday = weekdays[day_of_week as usize];

            // グレゴリオ暦計算
            let (year, month, day, hour, min, sec) = unix_timestamp_to_date(secs as i64);
            let months = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ];
            let month_str = months[(month - 1) as usize];

            format!(
                "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
                weekday, day, month_str, year, hour, min, sec
            )
        })
    }
}

/// Unix タイムスタンプを年月日時分秒に変換
fn unix_timestamp_to_date(timestamp: i64) -> (i32, u32, u32, u32, u32, u32) {
    const DAYS_PER_400_YEARS: i64 = 146097;

    let sec = (timestamp % 60) as u32;
    let timestamp = timestamp / 60;
    let min = (timestamp % 60) as u32;
    let timestamp = timestamp / 60;
    let hour = (timestamp % 24) as u32;
    let days = timestamp / 24 + 719468; // days from year 0

    let era = if days >= 0 {
        days
    } else {
        days - DAYS_PER_400_YEARS + 1
    } / DAYS_PER_400_YEARS;
    let doe = (days - era * DAYS_PER_400_YEARS) as i32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let year = yoe + (era as i32) * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { year + 1 } else { year };

    (year, month, day, hour, min, sec)
}

/// キャッシュのキー: パスの生バイト列。
///
/// `PathBuf` をキーにすると `Path` の `Hash` が要素ごとの走査（正規化）を行い、
/// 既定の SipHash と合わせてヒットのたびに CPU を使っていた（FreeBSD の DTrace で
/// `Path::hash` / `Components::next` / SipHash がホットスポットに出ていた）。
/// 静的配信のパスは常に同じ組み立て方（ルートの base_path + リクエストパス）なので、
/// バイト列の一致で十分。
#[inline]
fn path_key(path: &Path) -> &[u8] {
    path.as_os_str().as_encoded_bytes()
}

/// キーのハッシャ。キーは実在するファイルのパスに限られる（存在しないパスは挿入しない）が、
/// 念のためプロセスごとにシードを変える。
fn path_key_hasher() -> xxhash_rust::xxh3::Xxh3Builder {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64).rotate_left(32);
    xxhash_rust::xxh3::Xxh3Builder::new().with_seed(seed)
}

/// ファイル情報キャッシュ
///
/// DashMapベースのスレッドセーフなキャッシュ実装
pub struct OpenFileCache {
    /// キャッシュエントリ（パス → ファイル情報）。DashMap は内部シャーディングにより
    /// グローバルロックを持たない。
    entries: DashMap<Box<[u8]>, CachedFileInfo, xxhash_rust::xxh3::Xxh3Builder>,
    /// キャッシュエントリの有効期間（ナノ秒、ロックフリー atomic）
    valid_duration_nanos: AtomicU64,
    /// キャッシュヒット数
    hits: AtomicU64,
    /// キャッシュミス数
    misses: AtomicU64,
    /// 最大エントリ数（ロックフリー atomic）
    max_entries: AtomicUsize,
}

impl OpenFileCache {
    /// 新しいキャッシュを作成
    fn new() -> Self {
        Self {
            entries: DashMap::with_capacity_and_hasher(1024, path_key_hasher()),
            valid_duration_nanos: AtomicU64::new(60_000_000_000), // デフォルト60秒
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            max_entries: AtomicUsize::new(10_000),
        }
    }

    /// ファイル情報を取得（キャッシュ優先）
    ///
    /// キャッシュにヒットした場合はそれを返し、
    /// ミスした場合はファイルシステムから情報を取得してキャッシュします。
    ///
    /// # Returns
    ///
    /// - `Some(CachedFileInfo)`: ファイルが存在し情報を取得できた場合
    /// - `None`: ファイルが存在しないか、エラーが発生した場合
    pub async fn get_or_fetch(&self, path: &Path) -> Option<CachedFileInfo> {
        let valid_duration =
            Duration::from_nanos(self.valid_duration_nanos.load(Ordering::Relaxed));
        let max_entries = self.max_entries.load(Ordering::Relaxed);

        // まずキャッシュから検索（ヒット時は syscall ゼロ・非同期待機なし）
        if let Some(entry) = self.entries.get(path_key(path)) {
            if entry.is_valid(valid_duration) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Some(entry.clone());
            }
        }

        // キャッシュミス: ファイルシステムから取得（オフロードで非同期）
        self.misses.fetch_add(1, Ordering::Relaxed);

        let info = self.fetch_file_info(path).await?;

        // キャッシュが大きすぎる場合は古いエントリを削除
        if self.entries.len() >= max_entries {
            self.evict_oldest();
        }

        self.entries.insert(path_key(path).into(), info.clone());
        Some(info)
    }

    /// ファイル情報を直接フェッチ（キャッシュをバイパス、F-29 で完全非同期化）
    ///
    /// F-153: `canonicalize`（シンボリックリンク解決を含む）はパスの全コンポーネントに
    /// `readlink(2)` を発行する重い同期 syscall（実測で 1 リクエストあたり 7 回、全件
    /// エラー）。静的ルートが `cache::resolve::register_static_roots` で登録済みの場合は
    /// `resolve::open_beneath_for_request` によるカーネル封じ込め（Linux: `openat2`
    /// `RESOLVE_BENEATH`、FreeBSD: F-123 の `capsicum` `O_RESOLVE_BENEATH`）で置き換え、
    /// `readlink` をゼロにする。登録されていない場合・本プラットフォーム未対応の場合は
    /// 従来どおり `canonicalize()` + `metadata()` へフォールバックする（防御水準は変わらない）。
    ///
    /// いずれの経路も同期 syscall であり io_uring 非対応のため、ブロッキングオフロード
    /// （`runtime::offload`）で専用ワーカースレッドへ退避し、**イベントループをブロックしない**。
    /// MIME 推測も同所で実行する。
    // 理由付き allow: 同期 FS は offload 閉包内（専用ワーカースレッド）で実行され、イベントループを塞がない。
    #[allow(clippy::disallowed_methods)]
    pub(crate) async fn fetch_file_info(&self, path: &Path) -> Option<CachedFileInfo> {
        let path = path.to_path_buf();
        crate::runtime::offload::offload(move || {
            // F-153: カーネル封じ込め（openat2/capsicum）による解決を優先する。
            //
            // `canonical_path` の互換性について: このパスは open_beneath_for_request が
            // 既にカーネルに封じ込めを保証させた**登録済みルート配下の生パス**（開き直し
            // 不要）であり、`cache::sendfile_base_contains` の呼び出し側は `canonical_base`
            // と `base_path` のどちらを渡しても、`full_path` は常に `base_path` の join で
            // 構築されるため比較は必ず真になる（F-123 の capsicum 経路が採用しているのと
            // 同じ設計。`security::capsicum::static_serving_active()` 有効時と同じ結論に
            // 帰着し、含有チェックが冗長になる点も同一）。
            if let Some(res) = crate::cache::resolve::open_beneath_for_request(&path) {
                return match res {
                    Ok((file, meta)) => {
                        let mime_type: std::sync::Arc<str> = mime_guess::from_path(&path)
                            .first_or_octet_stream()
                            .as_ref()
                            .into();
                        let is_file = meta.is_file();
                        Some(CachedFileInfo {
                            canonical_path: path.into(),
                            file_size: meta.len(),
                            mime_type,
                            last_modified: meta.modified().ok(),
                            is_file,
                            shared_file: is_file.then(|| std::sync::Arc::new(file)),
                            cached_at: Instant::now(),
                        })
                    }
                    // 登録済みルート配下と判定された上での失敗（404 相当）。ここで
                    // canonicalize フォールバックへ二重に緩く倒すとフェイルオープンの
                    // 入り口になるため、素直に None（未検出）で返す。
                    Err(_) => None,
                };
            }
            // F-123: FreeBSD capability mode 下では canonicalize（絶対パス realpath）が
            // 禁止されるため、登録済みルート dirfd 相対の fstatat で代替する
            // （O_RESOLVE_BENEATH が封じ込めを担保。canonical_path は原パスのまま）。
            // 通常ファイルは配信に使う fd も同時に得る（open + fstat。stat だけにすると
            // 配信時に毎リクエスト開き直すことになる）。
            #[cfg(target_os = "freebsd")]
            if let Some(res) = crate::security::capsicum::open_static_ro(&path) {
                let file = res.ok()?;
                let meta = file.metadata().ok()?;
                let mime_type: std::sync::Arc<str> = mime_guess::from_path(&path)
                    .first_or_octet_stream()
                    .as_ref()
                    .into();
                let is_file = meta.is_file();
                return Some(CachedFileInfo {
                    canonical_path: path.into(),
                    file_size: meta.len(),
                    mime_type,
                    last_modified: meta.modified().ok(),
                    is_file,
                    shared_file: is_file.then(|| std::sync::Arc::new(file)),
                    cached_at: Instant::now(),
                });
            }
            // フォールバック: パスを正規化（シンボリックリンク解決・パストラバーサル防止）
            let canonical = path.canonicalize().ok()?;
            // メタデータを取得
            let metadata = std::fs::metadata(&canonical).ok()?;
            // MIMEタイプを推測
            let mime_type: std::sync::Arc<str> = mime_guess::from_path(&canonical)
                .first_or_octet_stream()
                .as_ref()
                .into();

            let is_file = metadata.is_file();
            // 通常ファイルは配信に使う fd も保持する（開けなければ配信時に開き直す）。
            let shared_file = if is_file {
                std::fs::File::open(&canonical)
                    .ok()
                    .map(std::sync::Arc::new)
            } else {
                None
            };
            Some(CachedFileInfo {
                canonical_path: canonical.into(),
                file_size: metadata.len(),
                mime_type,
                last_modified: metadata.modified().ok(),
                is_file,
                shared_file,
                cached_at: Instant::now(),
            })
        })
        .await
    }

    /// 古いエントリを削除（10%を削除）
    fn evict_oldest(&self) {
        let max_entries = self.max_entries.load(Ordering::Relaxed);
        let to_remove = max_entries / 10;
        let mut removed = 0;

        // 最も古いエントリから削除
        let mut oldest: Vec<(Box<[u8]>, Instant)> = self
            .entries
            .iter()
            .map(|e| (e.key().clone(), e.value().cached_at))
            .collect();

        oldest.sort_by_key(|(_, time)| *time);

        for (path, _) in oldest.into_iter().take(to_remove) {
            self.entries.remove(&*path);
            removed += 1;
            if removed >= to_remove {
                break;
            }
        }
    }

    /// キャッシュをクリア
    pub fn clear(&self) {
        self.entries.clear();
    }

    /// 特定のパスをキャッシュから削除
    pub fn invalidate(&self, path: &Path) {
        self.entries.remove(path_key(path));
    }

    /// キャッシュヒット数を取得
    #[inline]
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// キャッシュミス数を取得
    #[inline]
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// ヒット率を取得（パーセンテージ）
    pub fn hit_rate(&self) -> f64 {
        let hits = self.hits() as f64;
        let total = hits + self.misses() as f64;
        if total > 0.0 {
            (hits / total) * 100.0
        } else {
            0.0
        }
    }

    /// 現在のエントリ数を取得
    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// キャッシュが空かどうか
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 統計情報をリセット
    pub fn reset_stats(&self) {
        self.hits.store(0, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
    }

    /// 設定を考慮したヒット判定のみ（フェッチしない・syscall/offload 一切なし、B-65）。
    ///
    /// `cache::get_static_file_with_content`（メタデータ+本体の複合取得 API）が、
    /// 「両方のキャッシュがヒットする場合は offload を 1 回も呼ばない」という条件を
    /// 満たすために使う。ミス時はここでは何もフェッチせず `None` を返すだけ
    /// （フェッチは呼び出し元が別途 offload 経由で行う）。
    pub(crate) fn peek_with_config(
        &self,
        path: &Path,
        config: Option<&OpenFileCacheConfig>,
    ) -> Option<CachedFileInfo> {
        let enabled = config
            .and_then(|c| c.enabled)
            .unwrap_or_else(|| OPEN_FILE_CACHE_GLOBAL_ENABLED.load(Ordering::Relaxed));
        if !enabled {
            return None;
        }
        let valid_duration = config
            .and_then(|c| c.valid_duration_secs)
            .map(Duration::from_secs)
            .unwrap_or_else(|| {
                Duration::from_nanos(
                    OPEN_FILE_CACHE_GLOBAL_VALID_DURATION_NANOS.load(Ordering::Relaxed),
                )
            });
        let entry = self.entries.get(path_key(path))?;
        if entry.is_valid(valid_duration) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            Some(entry.clone())
        } else {
            None
        }
    }

    /// 設定を考慮した挿入（B-65）。`get_or_fetch_with_config` の挿入部分と同じ
    /// admission control（最大エントリ数超過時は `evict_oldest`）を使う。
    /// 複合取得 API がオフロードで既に得たメタデータをキャッシュへ登録する用途。
    ///
    /// 呼び出し元は `static_file` の高速経路のみで、その経路は Linux/FreeBSD に
    /// しか存在しない。それ以外のターゲットでは未使用になるため cfg で本メソッド
    /// 自体の存在を絞る（`#[allow(dead_code)]` は規約で禁止）。
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub(crate) fn insert_with_config(
        &self,
        path: &Path,
        info: CachedFileInfo,
        config: Option<&OpenFileCacheConfig>,
    ) {
        let enabled = config
            .and_then(|c| c.enabled)
            .unwrap_or_else(|| OPEN_FILE_CACHE_GLOBAL_ENABLED.load(Ordering::Relaxed));
        if !enabled {
            return;
        }
        let max_entries = config
            .and_then(|c| c.max_entries)
            .unwrap_or_else(|| OPEN_FILE_CACHE_GLOBAL_MAX_ENTRIES.load(Ordering::Relaxed));
        let current_max_entries = self.max_entries.load(Ordering::Relaxed);
        if self.entries.len() >= max_entries.min(current_max_entries) {
            self.evict_oldest();
        }
        self.entries.insert(path_key(path).into(), info);
    }

    /// 設定を考慮してファイル情報を取得
    pub async fn get_or_fetch_with_config(
        &self,
        path: &Path,
        valid_duration: Duration,
        max_entries: usize,
    ) -> Option<CachedFileInfo> {
        // まずキャッシュから検索（ヒット時は非同期待機なし）
        if let Some(entry) = self.entries.get(path_key(path)) {
            // ルーティングごとの有効期間で判定
            if entry.is_valid(valid_duration) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Some(entry.clone());
            }
        }

        // キャッシュミス: ファイルシステムから取得（オフロードで非同期）
        self.misses.fetch_add(1, Ordering::Relaxed);

        let info = self.fetch_file_info(path).await?;

        // ルーティングごとの最大エントリ数で判定
        let current_max_entries = self.max_entries.load(Ordering::Relaxed);
        if self.entries.len() >= max_entries.min(current_max_entries) {
            self.evict_oldest();
        }

        self.entries.insert(path_key(path).into(), info.clone());
        Some(info)
    }
}

/// グローバルファイル情報キャッシュを取得
#[inline]
pub fn get_file_cache() -> &'static OpenFileCache {
    &OPEN_FILE_CACHE
}

/// ファイル情報をキャッシュから取得
#[inline]
pub async fn get_file_info(path: &Path) -> Option<CachedFileInfo> {
    OPEN_FILE_CACHE.get_or_fetch(path).await
}

/// ルーティングごとの設定を考慮してファイル情報を取得
pub async fn get_file_info_with_config(
    path: &Path,
    config: Option<&OpenFileCacheConfig>,
) -> Option<CachedFileInfo> {
    // ルーティング設定がある場合はそれを使用、ない場合はグローバル設定を使用
    let enabled = config
        .and_then(|c| c.enabled)
        .unwrap_or_else(|| OPEN_FILE_CACHE_GLOBAL_ENABLED.load(Ordering::Relaxed));

    if !enabled {
        // キャッシュ無効時は直接フェッチ（キャッシュしない）
        return OPEN_FILE_CACHE.fetch_file_info(path).await;
    }

    // 有効期間と最大エントリ数も同様に処理
    let valid_duration = config
        .and_then(|c| c.valid_duration_secs)
        .map(Duration::from_secs)
        .unwrap_or_else(|| {
            Duration::from_nanos(
                OPEN_FILE_CACHE_GLOBAL_VALID_DURATION_NANOS.load(Ordering::Relaxed),
            )
        });

    let max_entries = config
        .and_then(|c| c.max_entries)
        .unwrap_or_else(|| OPEN_FILE_CACHE_GLOBAL_MAX_ENTRIES.load(Ordering::Relaxed));

    // キャッシュから取得（有効期間と最大エントリ数を考慮）
    OPEN_FILE_CACHE
        .get_or_fetch_with_config(path, valid_duration, max_entries)
        .await
}

/// 指定パスのキャッシュを無効化
#[inline]
pub fn invalidate_file_cache(path: &Path) {
    OPEN_FILE_CACHE.invalidate(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn test_file_cache_hit() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");

        // テストファイルを作成
        let mut file = File::create(&file_path).unwrap();
        writeln!(file, "Hello, World!").unwrap();
        drop(file);

        let cache = OpenFileCache::new();

        // 最初のアクセス（キャッシュミス）。ring 未初期化のため offload は同期インライン実行され、
        // futures::executor::block_on が即座に完了する。
        let info1 = futures::executor::block_on(cache.get_or_fetch(&file_path));
        assert!(info1.is_some());
        assert_eq!(cache.misses(), 1);
        assert_eq!(cache.hits(), 0);

        // 2回目のアクセス（キャッシュヒット）
        let info2 = futures::executor::block_on(cache.get_or_fetch(&file_path));
        assert!(info2.is_some());
        assert_eq!(cache.misses(), 1);
        assert_eq!(cache.hits(), 1);

        // 情報が一致することを確認
        let info1 = info1.unwrap();
        let info2 = info2.unwrap();
        assert_eq!(info1.file_size, info2.file_size);
        assert_eq!(info1.mime_type, info2.mime_type);
    }

    /// 通常ファイルはメタデータ取得時に開いた fd をキャッシュが保持し、ヒット時は
    /// 同じ fd を共有すること（HTTP/1.1 静的配信が毎リクエスト open/close しないため）。
    /// ディレクトリは fd を保持しない。
    #[cfg(unix)] // FileExt::read_exact_at（pread）を使う
    #[test]
    // 理由付き allow: テストコード（一時ファイルの作成に同期 FS を使う。データプレーン非経由）。
    #[allow(clippy::disallowed_methods)]
    fn test_file_cache_keeps_shared_fd_for_regular_files() {
        use std::os::unix::fs::FileExt;
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("fd.txt");
        std::fs::write(&file_path, b"shared-fd").unwrap();

        let cache = OpenFileCache::new();
        let info1 = futures::executor::block_on(cache.get_or_fetch(&file_path)).unwrap();
        let info2 = futures::executor::block_on(cache.get_or_fetch(&file_path)).unwrap();
        let f1 = info1.shared_file().expect("regular file must keep an fd");
        let f2 = info2.shared_file().expect("cache hit must share the fd");
        assert!(
            std::sync::Arc::ptr_eq(f1, f2),
            "hit must not reopen the file"
        );
        let mut buf = [0u8; 9];
        f1.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"shared-fd");

        let dinfo = futures::executor::block_on(cache.get_or_fetch(dir.path())).unwrap();
        assert!(!dinfo.is_file);
        assert!(
            dinfo.shared_file().is_none(),
            "directories must not keep an fd"
        );
    }

    #[test]
    fn test_mime_type_detection() {
        let dir = tempdir().unwrap();

        // HTMLファイル
        let html_path = dir.path().join("index.html");
        File::create(&html_path).unwrap();
        let info =
            futures::executor::block_on(get_file_cache().fetch_file_info(&html_path)).unwrap();
        assert!(info.mime_type.starts_with("text/html"));

        // CSSファイル
        let css_path = dir.path().join("style.css");
        File::create(&css_path).unwrap();
        let info =
            futures::executor::block_on(get_file_cache().fetch_file_info(&css_path)).unwrap();
        assert!(info.mime_type.starts_with("text/css"));

        // JavaScriptファイル
        let js_path = dir.path().join("app.js");
        File::create(&js_path).unwrap();
        let info = futures::executor::block_on(get_file_cache().fetch_file_info(&js_path)).unwrap();
        assert!(info.mime_type.contains("javascript"));
    }

    #[test]
    fn test_invalidate() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        File::create(&file_path).unwrap();

        let cache = OpenFileCache::new();

        // キャッシュに追加
        let _ = futures::executor::block_on(cache.get_or_fetch(&file_path));
        assert_eq!(cache.len(), 1);

        // 無効化
        cache.invalidate(&file_path);

        // エントリが削除されていることを確認するため、再度取得
        // (新しいミスが発生)
        let initial_misses = cache.misses();
        let _ = futures::executor::block_on(cache.get_or_fetch(&file_path));
        assert_eq!(cache.misses(), initial_misses + 1);
    }

    #[test]
    fn test_nonexistent_file() {
        let cache = OpenFileCache::new();
        let result =
            futures::executor::block_on(cache.get_or_fetch(Path::new("/nonexistent/file.txt")));
        assert!(result.is_none());
    }
}
