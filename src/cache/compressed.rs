//! 静的配信の圧縮結果キャッシュ（F-169）
//!
//! `src/proxy.rs`（HTTP/2）・`src/http3_server.rs`（HTTP/3）の File ルート静的配信は、
//! 本体（非圧縮）こそ `content_cache`（F-146/F-157）で `Bytes` 参照カウントクローンに
//! なっているものの、圧縮（zstd/br/gzip/deflate）は毎リクエスト同じ内容を再圧縮していた。
//! 54KB の静的ファイルで実測した結果、対 nginx でスループットが 0.3〜0.4× まで劣化する
//! 支配的要因であることが判明した
//! （詳細は `docs/backlog/features/F-169-static-compressed-variant-cache.md`）。
//!
//! 本モジュールは `(パス, エンコーディング, 圧縮レベル)` をキーに圧縮済み `Bytes` を
//! 保持し、静的配信の圧縮をキャッシュヒット時ゼロコストにする。**プロキシ応答は
//! 対象外**（毎回内容が異なるためキャッシュしてはならない）。呼び出し元がパスを
//! 把握している静的配信の呼び出しだけがこのキャッシュを使う。
//!
//! 圧縮レベルをキーに含めるのは、設定リロードで圧縮レベルが変わったときに
//! 古い（別レベルで圧縮した）結果を返さないようにするため。
//!
//! ## 有効化条件
//!
//! `static_file_cache`（本体キャッシュ、[`super::StaticContentCacheConfig`]）が
//! 有効なときのみ効く。本体キャッシュが無効なら本モジュールは一切キャッシュせず、
//! 呼び出し元が渡した圧縮クロージャを毎回そのまま実行する（F-157 で踏んだ
//! 「メタデータキャッシュと本体キャッシュはセットで有効にしないと素通りする」教訓を
//! 繰り返さないよう、依存関係を「本体キャッシュが唯一のスイッチ」という単純な形に
//! 保つ）。上限（`max_entries`/`max_total_bytes`）・TTL（`valid_duration_secs`）も
//! 本体キャッシュと同じ設定値をそのまま使う。
//!
//! ## 無効化の連動（最重要）
//!
//! `content_cache::invalidate(path)` および `content_cache::ContentCache::try_insert`
//! （本体が新規挿入・上書きされる全経路）から、同じパスの圧縮結果もまとめて
//! 破棄するよう配線されている（`content_cache.rs` 参照）。mtime 再検証や TTL 失効で
//! 本体が入れ替わったのに古い圧縮結果を返す、という不整合を防ぐ。
//!
//! ## ホットパス（キャッシュヒット時）
//!
//! パスをキーに外側の `DashMap` を引き、ヒットしたパスエントリの内側の
//! `DashMap`（エンコーディング・レベルの組で引く、両方とも `Copy`）を引く。
//! いずれも `Path`/`(AcceptedEncoding, i32)` の**借用**で引けるため、ヒット時は
//! `PathBuf` の確保が一切発生しない。TTL 判定の上で `Bytes::clone()`
//! （参照カウント増加のみ）で返す。
//!
//! ## キャッシュミス時・容量超過時
//!
//! 呼び出し元から渡された圧縮クロージャを実行し、結果を返す。上限
//! （`max_entries`/`max_total_bytes`）に収まる場合のみ挿入する。収まらない場合は
//! 挿入せず、以降も毎回再圧縮にフォールバックし続ける（安全側・キャッシュ本体は
//! 破損しない。`content_cache` の admission control と同じ方針）。

use crate::config::{AcceptedEncoding, CompressionConfig};

/// エンコーディングに対応する圧縮レベルを取り出す（キャッシュキーの一部として使う。
/// `CompressionConfig` はエンコーディングごとに異なる型（`zstd_level: i32` /
/// `gzip_level/brotli_level: u32`）でレベルを保持するため、キャッシュキー用に
/// `i32` へ統一する）。
///
/// `Identity` は呼び出し元（`should_compress`）が圧縮対象外と判定した時点で
/// このモジュールに到達しない想定だが、安全側に `0` を返す。
#[inline]
pub fn compression_level(encoding: AcceptedEncoding, compression: &CompressionConfig) -> i32 {
    match encoding {
        AcceptedEncoding::Zstd => compression.zstd_level,
        AcceptedEncoding::Brotli => compression.brotli_level as i32,
        AcceptedEncoding::Gzip | AcceptedEncoding::Deflate => compression.gzip_level as i32,
        AcceptedEncoding::Identity => 0,
    }
}

#[cfg(feature = "cache")]
mod enabled {
    use super::AcceptedEncoding;
    use crate::cache::StaticContentCacheConfig;
    use bytes::Bytes;
    use dashmap::DashMap;
    use once_cell::sync::Lazy;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// 圧縮結果キャッシュのバリアントキー（パスは外側の `DashMap` のキーで
    /// 表現するため、内側のキーはエンコーディング・レベルの組のみで済む
    /// = `Copy`、ヒット時のアロケーションが発生しない）。
    type VariantKey = (AcceptedEncoding, i32);

    struct CachedVariant {
        data: Bytes,
        len: u64,
        cached_at: Instant,
    }

    /// パスごとの圧縮バリアント集合。`invalidate(path)` が外側キーを丸ごと
    /// 削除するだけで、そのパスの全エンコーディング・全レベルの結果を
    /// まとめて破棄できるようにするための入れ子構造。
    struct PathEntry {
        variants: DashMap<VariantKey, CachedVariant>,
    }

    impl PathEntry {
        fn new() -> Self {
            Self {
                variants: DashMap::new(),
            }
        }
    }

    struct CompressedCache {
        entries: DashMap<PathBuf, PathEntry>,
        /// 全パス・全バリアントの `data` 合計バイト数（admission control 用）
        total_bytes: AtomicU64,
        /// 全パス・全バリアントの件数（`max_entries` 判定用。`DashMap::len()` は
        /// シャード横断の read ロックを取るため、ホットパスにもなり得る
        /// `try_insert` からは呼ばず、専用カウンタを都度更新する）
        entry_count: AtomicUsize,
        hits: AtomicU64,
        misses: AtomicU64,
    }

    impl CompressedCache {
        fn new() -> Self {
            Self {
                entries: DashMap::with_capacity(64),
                total_bytes: AtomicU64::new(0),
                entry_count: AtomicUsize::new(0),
                hits: AtomicU64::new(0),
                misses: AtomicU64::new(0),
            }
        }

        /// ヒット判定のみ（ロード・圧縮は一切行わない）。ホットパス規則どおり
        /// `DashMap` ルックアップ × 2（借用キー、アロケーション無し）+ TTL 判定 +
        /// `Bytes::clone()` のみ。
        fn try_hit(
            &self,
            path: &Path,
            key: VariantKey,
            cfg: &StaticContentCacheConfig,
        ) -> Option<Bytes> {
            if !cfg.enabled {
                return None;
            }
            let path_entry = self.entries.get(path)?;
            let variant = path_entry.variants.get(&key)?;
            let ttl = Duration::from_secs(cfg.valid_duration_secs);
            if variant.cached_at.elapsed() >= ttl {
                return None;
            }
            self.hits.fetch_add(1, Ordering::Relaxed);
            Some(variant.data.clone())
        }

        /// 上限を満たす場合のみ挿入する（admission control）。
        ///
        /// **`self.entries.entry()` を保持したまま `self.entries` へ他の操作を
        /// 呼ばないこと**（`content_cache.rs` の `try_insert` と同じ注意点。
        /// `DashMap::entry()` は該当シャードの write ロックを取得し、`Entry` が
        /// 生きている間はそれを保持し続けるため、同一シャードへの再入で
        /// 自己デッドロックする）。ここでは既存有無の確認を `get()`
        /// （即座に drop される読み取りロック）で済ませてから、必要な場合のみ
        /// 別途 `entry()` を呼ぶことでこれを避けている。
        fn try_insert(
            &self,
            path: &Path,
            key: VariantKey,
            content: CachedVariant,
            cfg: &StaticContentCacheConfig,
        ) {
            let new_len = content.len;
            let existing_len = self
                .entries
                .get(path)
                .and_then(|pe| pe.variants.get(&key).map(|v| v.len));

            match existing_len {
                Some(old_len) => {
                    let current = self.total_bytes.load(Ordering::Relaxed);
                    let projected = current.saturating_sub(old_len).saturating_add(new_len);
                    if projected > cfg.max_total_bytes {
                        return;
                    }
                    self.total_bytes.fetch_sub(old_len, Ordering::Relaxed);
                    self.total_bytes.fetch_add(new_len, Ordering::Relaxed);
                    if let Some(pe) = self.entries.get(path) {
                        pe.variants.insert(key, content);
                    }
                }
                None => {
                    if self.entry_count.load(Ordering::Relaxed) >= cfg.max_entries {
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
                    self.entry_count.fetch_add(1, Ordering::Relaxed);
                    self.entries
                        .entry(path.to_path_buf())
                        .or_insert_with(PathEntry::new)
                        .variants
                        .insert(key, content);
                }
            }
        }

        /// 指定パスの全バリアント（全エンコーディング・全レベル）をまとめて破棄する。
        fn invalidate(&self, path: &Path) {
            if let Some((_, removed)) = self.entries.remove(path) {
                let freed: u64 = removed.variants.iter().map(|e| e.value().len).sum();
                let count = removed.variants.len();
                self.total_bytes.fetch_sub(freed, Ordering::Relaxed);
                self.entry_count.fetch_sub(count, Ordering::Relaxed);
            }
        }

        fn clear(&self) {
            self.entries.clear();
            self.total_bytes.store(0, Ordering::Relaxed);
            self.entry_count.store(0, Ordering::Relaxed);
        }
    }

    static COMPRESSED_CACHE: Lazy<CompressedCache> = Lazy::new(CompressedCache::new);

    /// 静的配信の圧縮結果をキャッシュ優先で取得する。
    ///
    /// `cfg.enabled`（= `static_file_cache` の有効状態）が `false` の場合は
    /// キャッシュを一切使わず、`compress` を毎回そのまま呼ぶ。
    ///
    /// `compress` はキャッシュミス時にのみ呼ばれる（`FnOnce`）。
    pub fn get_or_compress<F>(
        path: &Path,
        encoding: AcceptedEncoding,
        level: i32,
        cfg: &StaticContentCacheConfig,
        compress: F,
    ) -> Bytes
    where
        F: FnOnce() -> Vec<u8>,
    {
        if !cfg.enabled {
            return Bytes::from(compress());
        }
        let key = (encoding, level);
        if let Some(hit) = COMPRESSED_CACHE.try_hit(path, key, cfg) {
            return hit;
        }
        COMPRESSED_CACHE.misses.fetch_add(1, Ordering::Relaxed);
        let compressed = compress();
        let len = compressed.len() as u64;
        let data = Bytes::from(compressed);
        COMPRESSED_CACHE.try_insert(
            path,
            key,
            CachedVariant {
                data: data.clone(),
                len,
                cached_at: Instant::now(),
            },
            cfg,
        );
        data
    }

    /// 指定パスの圧縮結果キャッシュを無効化する（`content_cache` の本体無効化と
    /// 連動させるためのフック。詳細はモジュール doc 参照）。
    pub fn invalidate(path: &Path) {
        COMPRESSED_CACHE.invalidate(path);
    }

    /// 圧縮結果キャッシュを全クリアする。
    pub fn clear() {
        COMPRESSED_CACHE.clear();
    }

    /// キャッシュヒット数（テスト・診断用）。
    pub fn hits() -> u64 {
        COMPRESSED_CACHE.hits.load(Ordering::Relaxed)
    }

    /// キャッシュミス数（テスト・診断用）。
    pub fn misses() -> u64 {
        COMPRESSED_CACHE.misses.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::path::PathBuf;
        use std::sync::atomic::AtomicUsize;

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

        #[test]
        fn hit_avoids_recompression() {
            let cache = CompressedCache::new();
            let cfg = test_cfg();
            let path = PathBuf::from("/var/www/a.txt");
            let calls = AtomicUsize::new(0);

            let key = (AcceptedEncoding::Zstd, 3);
            let first = cache.try_hit(&path, key, &cfg).unwrap_or_else(|| {
                calls.fetch_add(1, Ordering::Relaxed);
                let data = Bytes::from_static(b"compressed-once");
                cache.try_insert(
                    &path,
                    key,
                    CachedVariant {
                        data: data.clone(),
                        len: data.len() as u64,
                        cached_at: Instant::now(),
                    },
                    &cfg,
                );
                data
            });
            assert_eq!(first, Bytes::from_static(b"compressed-once"));
            assert_eq!(calls.load(Ordering::Relaxed), 1);

            // 2回目はヒットするはず（同じ compress クロージャを呼ばない）。
            let second = cache.try_hit(&path, key, &cfg);
            assert_eq!(second, Some(Bytes::from_static(b"compressed-once")));
        }

        #[test]
        fn invalidate_drops_all_variants_for_path() {
            let cache = CompressedCache::new();
            let cfg = test_cfg();
            let path = PathBuf::from("/var/www/b.txt");

            cache.try_insert(
                &path,
                (AcceptedEncoding::Zstd, 3),
                CachedVariant {
                    data: Bytes::from_static(b"zstd"),
                    len: 4,
                    cached_at: Instant::now(),
                },
                &cfg,
            );
            cache.try_insert(
                &path,
                (AcceptedEncoding::Gzip, 4),
                CachedVariant {
                    data: Bytes::from_static(b"gzip"),
                    len: 4,
                    cached_at: Instant::now(),
                },
                &cfg,
            );
            assert!(cache
                .try_hit(&path, (AcceptedEncoding::Zstd, 3), &cfg)
                .is_some());
            assert!(cache
                .try_hit(&path, (AcceptedEncoding::Gzip, 4), &cfg)
                .is_some());

            cache.invalidate(&path);

            assert!(cache
                .try_hit(&path, (AcceptedEncoding::Zstd, 3), &cfg)
                .is_none());
            assert!(cache
                .try_hit(&path, (AcceptedEncoding::Gzip, 4), &cfg)
                .is_none());
            assert_eq!(cache.total_bytes.load(Ordering::Relaxed), 0);
            assert_eq!(cache.entry_count.load(Ordering::Relaxed), 0);
        }

        #[test]
        fn different_encoding_is_separate_entry() {
            let cache = CompressedCache::new();
            let cfg = test_cfg();
            let path = PathBuf::from("/var/www/c.txt");

            cache.try_insert(
                &path,
                (AcceptedEncoding::Zstd, 3),
                CachedVariant {
                    data: Bytes::from_static(b"zstd-body"),
                    len: 9,
                    cached_at: Instant::now(),
                },
                &cfg,
            );
            // gzip はまだ挿入していないのでミスのはず。
            assert!(cache
                .try_hit(&path, (AcceptedEncoding::Gzip, 4), &cfg)
                .is_none());
            assert!(cache
                .try_hit(&path, (AcceptedEncoding::Zstd, 3), &cfg)
                .is_some());
        }

        #[test]
        fn disabled_cfg_never_hits() {
            let cache = CompressedCache::new();
            let mut cfg = test_cfg();
            let path = PathBuf::from("/var/www/d.txt");

            cache.try_insert(
                &path,
                (AcceptedEncoding::Zstd, 3),
                CachedVariant {
                    data: Bytes::from_static(b"x"),
                    len: 1,
                    cached_at: Instant::now(),
                },
                &cfg,
            );
            cfg.enabled = false;
            assert!(cache
                .try_hit(&path, (AcceptedEncoding::Zstd, 3), &cfg)
                .is_none());
        }

        /// public API (`get_or_compress`/`invalidate`) の smoke test（グローバル
        /// シングルトン経由）。
        // グローバル `COMPRESSED_CACHE` をシングルトン経由で触るため、並列実行だと
        // `content_cache.rs` 側の同種テスト（`invalidate_also_drops_compressed_cache_variants`
        // 等）と衝突しうる。両ファイル共通の既定グループ（引数なし `#[serial]`）で
        // 直列化する。
        #[test]
        #[serial_test::serial]
        fn public_api_get_or_compress_smoke_test() {
            let path = PathBuf::from("/var/www/e-smoke.txt");
            let cfg = test_cfg();
            let calls = AtomicUsize::new(0);

            let first = get_or_compress(&path, AcceptedEncoding::Zstd, 3, &cfg, || {
                calls.fetch_add(1, Ordering::Relaxed);
                b"compressed".to_vec()
            });
            assert_eq!(first, Bytes::from_static(b"compressed"));
            assert_eq!(calls.load(Ordering::Relaxed), 1);

            let second = get_or_compress(&path, AcceptedEncoding::Zstd, 3, &cfg, || {
                calls.fetch_add(1, Ordering::Relaxed);
                b"compressed".to_vec()
            });
            assert_eq!(second, Bytes::from_static(b"compressed"));
            assert_eq!(
                calls.load(Ordering::Relaxed),
                1,
                "2回目はキャッシュヒットし compress クロージャは呼ばれない"
            );

            invalidate(&path);
            let third = get_or_compress(&path, AcceptedEncoding::Zstd, 3, &cfg, || {
                calls.fetch_add(1, Ordering::Relaxed);
                b"compressed".to_vec()
            });
            assert_eq!(third, Bytes::from_static(b"compressed"));
            assert_eq!(
                calls.load(Ordering::Relaxed),
                2,
                "invalidate 後は再圧縮される"
            );

            // 無効時キャッシュを使わないこと。
            let mut disabled_cfg = cfg;
            disabled_cfg.enabled = false;
            let calls2 = AtomicUsize::new(0);
            let _ = get_or_compress(&path, AcceptedEncoding::Zstd, 3, &disabled_cfg, || {
                calls2.fetch_add(1, Ordering::Relaxed);
                b"no-cache".to_vec()
            });
            let _ = get_or_compress(&path, AcceptedEncoding::Zstd, 3, &disabled_cfg, || {
                calls2.fetch_add(1, Ordering::Relaxed);
                b"no-cache".to_vec()
            });
            assert_eq!(
                calls2.load(Ordering::Relaxed),
                2,
                "本体キャッシュ無効時は圧縮キャッシュも使わず毎回圧縮する"
            );
        }
    }
}

#[cfg(feature = "cache")]
pub use enabled::{clear, get_or_compress, hits, invalidate, misses};

/// `cache` feature 無効時のスタブ: キャッシュ本体が無いため常に呼び出し元の
/// 圧縮クロージャをそのまま実行する（静的配信自体は成立させる。`content_cache`
/// の無効時挙動と同じ設計）。
#[cfg(not(feature = "cache"))]
pub fn get_or_compress<F>(
    _path: &std::path::Path,
    _encoding: AcceptedEncoding,
    _level: i32,
    _cfg: &crate::cache::StaticContentCacheConfig,
    compress: F,
) -> bytes::Bytes
where
    F: FnOnce() -> Vec<u8>,
{
    bytes::Bytes::from(compress())
}

#[cfg(not(feature = "cache"))]
pub fn invalidate(_path: &std::path::Path) {}

#[cfg(not(feature = "cache"))]
pub fn clear() {}

#[cfg(not(feature = "cache"))]
pub fn hits() -> u64 {
    0
}

#[cfg(not(feature = "cache"))]
pub fn misses() -> u64 {
    0
}
