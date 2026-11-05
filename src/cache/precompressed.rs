//! 事前圧縮した兄弟ファイル（`app.js.br`・`app.js.gz`・`app.js.zst`）を探す（F-294。nginx の `gzip_static` /
//! `brotli_static` 相当）。
//!
//! - 兄弟は配信するファイルのパスに拡張子を足したもの。元のファイルと同じく open_file_cache を通して開く
//!   （ディレクトリルートは同じ封じ込め検査をかける）。通常のファイルで、元のファイルより新しいか同じ更新時刻の
//!   ものだけ使う（古い兄弟で古い内容を返さない）。
//! - 兄弟が有るか無いかは元のファイルのパスごとに表で覚える（open_file_cache と同じ有効期間・監視の世代。
//!   元のファイルの大きさ・更新時刻が変わったら探し直す）。無い兄弟を要求ごとに探さない（offload を増やさない）。
//!   open_file_cache が無効なルートは表を持たず、要求ごとに探す（設定のロード時に警告する）。
//! - 兄弟の検証子は強い ETag（`"<mtime>-<size>-br"`）で、`Content-Encoding` を持つ（`static_response::Validators`）。

use std::path::Path;
use std::sync::Arc;

use crate::config::{AcceptedEncoding, CompressionConfig, Precompressed};
use crate::static_response::Validators;

use super::{CachedFileInfo, OpenFileCacheConfig, RouteContainment};

/// 選んだ兄弟
pub struct Sibling {
    /// 兄弟のパス（`get_static_file_with_content` で本体を読むときに使う）
    pub path: Arc<Path>,
    /// open_file_cache のエントリ（大きさ・共有 fd）
    pub info: CachedFileInfo,
    /// 兄弟の検証子（`Content-Encoding` を持つ）
    pub validators: Arc<Validators>,
}

/// この要求に返す兄弟を選ぶ（無ければ None。その場の圧縮か元のファイルを返す）。
///
/// `original` は配信するファイル（インデックス解決後）、`original_validators` はその検証子（更新時刻と型を使う）。
#[allow(clippy::too_many_arguments)]
pub async fn select(
    original: &Path,
    original_size: u64,
    original_validators: &Validators,
    mime: &str,
    accept_encoding: Option<&[u8]>,
    pre: Precompressed,
    compression: &CompressionConfig,
    ofc: Option<&OpenFileCacheConfig>,
    containment: Option<RouteContainment<'_>>,
) -> Option<Sibling> {
    let candidates = crate::static_response::precompressed_candidates(
        accept_encoding,
        pre,
        compression,
        mime,
        original_size,
    );
    candidates[0]?;
    let ident = super::compressed::file_ident(original_size, original_validators.mtime_secs);
    for enc in candidates.into_iter().map_while(|e| e) {
        if let Some(s) = imp::lookup(
            original,
            ident,
            original_validators,
            mime,
            enc,
            ofc,
            containment,
        )
        .await
        {
            return Some(s);
        }
    }
    None
}

/// 兄弟のパス（`original` + 拡張子。表に無いときだけ作る）
fn sibling_path(original: &Path, enc: AcceptedEncoding) -> Option<std::path::PathBuf> {
    let ext = Precompressed::extension(enc)?;
    let mut p = original.as_os_str().to_owned();
    p.push(ext);
    Some(p.into())
}

/// 兄弟を開いて使えるか確かめる（通常のファイル・封じ込め・元のファイルより古くない）
async fn probe(
    path: &Path,
    original_mtime_secs: Option<u64>,
    ofc: Option<&OpenFileCacheConfig>,
    containment: Option<RouteContainment<'_>>,
) -> Option<CachedFileInfo> {
    let info = super::get_file_info_with_config(path, ofc).await?;
    usable(&info, original_mtime_secs, containment).then_some(info)
}

fn usable(
    info: &CachedFileInfo,
    original_mtime_secs: Option<u64>,
    containment: Option<RouteContainment<'_>>,
) -> bool {
    if !info.is_file {
        return false;
    }
    if let Some(c) = containment {
        if !super::sendfile_base_contains(&info.canonical_path, c.canonical_base, c.root) {
            return false;
        }
    }
    match (original_mtime_secs, info.validators.mtime_secs) {
        (Some(o), Some(s)) => s >= o,
        // 更新時刻が分からなければ新しさを確かめられないので使わない
        (Some(_), None) => false,
        (None, _) => true,
    }
}

fn sibling_validators(
    info: &CachedFileInfo,
    original: &Validators,
    mime: &str,
    enc: AcceptedEncoding,
) -> Arc<Validators> {
    Arc::new(Validators::for_sibling(
        info.file_size,
        info.last_modified,
        original.mtime_secs,
        mime,
        enc,
    ))
}

#[cfg(feature = "cache")]
mod imp {
    use super::*;
    use dashmap::DashMap;
    use once_cell::sync::Lazy;
    use std::time::Instant;

    /// 兄弟 1 つの状態
    #[derive(Clone)]
    enum Slot {
        /// まだ探していない
        Unknown,
        /// 無い（使えない）
        Absent,
        /// 有る。`size`・`mtime_secs` が変わったら検証子を作り直す
        Present {
            path: Arc<Path>,
            size: u64,
            mtime_secs: Option<u64>,
            validators: Arc<Validators>,
        },
    }

    /// 元のファイル 1 つの兄弟の表
    #[derive(Clone)]
    struct Entry {
        /// 元のファイルの大きさ・更新時刻（変わったら探し直す）
        ident: u64,
        slots: [Slot; 3],
        cached_at: Instant,
        stamp: Option<crate::cache::watch::Stamp>,
    }

    static TABLE: Lazy<DashMap<Box<Path>, Entry>> = Lazy::new(DashMap::new);

    pub(super) async fn lookup(
        original: &Path,
        ident: u64,
        original_validators: &Validators,
        mime: &str,
        enc: AcceptedEncoding,
        ofc: Option<&OpenFileCacheConfig>,
        containment: Option<RouteContainment<'_>>,
    ) -> Option<Sibling> {
        let slot_i = Precompressed::slot(enc)?;
        let Some((ttl, max_entries, watch_gen)) = crate::cache::file_cache::effective_params(ofc)
        else {
            // open_file_cache が無効: 表を持たず毎回探す
            let path = sibling_path(original, enc)?;
            let info = probe(&path, original_validators.mtime_secs, ofc, containment).await?;
            let validators = sibling_validators(&info, original_validators, mime, enc);
            return Some(Sibling {
                path: path.into(),
                info,
                validators,
            });
        };

        // 表を引く（ヒットでは確保しない）
        let known = TABLE.get(original).and_then(|e| {
            let fresh = e.ident == ident
                && crate::cache::watch::fresh(e.stamp, watch_gen)
                && crate::runtime::time::coarse_now().saturating_duration_since(e.cached_at) < ttl;
            fresh.then(|| e.slots[slot_i].clone())
        });
        match known {
            Some(Slot::Absent) => return None,
            Some(Slot::Present {
                path,
                size,
                mtime_secs,
                validators,
            }) => {
                if let Some(info) =
                    probe(&path, original_validators.mtime_secs, ofc, containment).await
                {
                    if info.file_size == size && info.validators.mtime_secs == mtime_secs {
                        return Some(Sibling {
                            path,
                            info,
                            validators,
                        });
                    }
                    // 兄弟が書き換わった: 検証子を作り直して表を更新する
                    let validators = sibling_validators(&info, original_validators, mime, enc);
                    store(
                        original,
                        ident,
                        slot_i,
                        Slot::Present {
                            path: path.clone(),
                            size: info.file_size,
                            mtime_secs: info.validators.mtime_secs,
                            validators: validators.clone(),
                        },
                        watch_gen,
                        max_entries,
                    );
                    return Some(Sibling {
                        path,
                        info,
                        validators,
                    });
                }
                // 消えた・使えなくなった
                store(
                    original,
                    ident,
                    slot_i,
                    Slot::Absent,
                    watch_gen,
                    max_entries,
                );
                return None;
            }
            Some(Slot::Unknown) | None => {}
        }

        // 探す（表に無い・期限切れ・元のファイルが変わった）
        let stamp = crate::cache::watch::stamp(watch_gen);
        let path: Arc<Path> = sibling_path(original, enc)?.into();
        let found = probe(&path, original_validators.mtime_secs, ofc, containment).await;
        let (slot, ret) = match found {
            Some(info) => {
                let validators = sibling_validators(&info, original_validators, mime, enc);
                (
                    Slot::Present {
                        path: path.clone(),
                        size: info.file_size,
                        mtime_secs: info.validators.mtime_secs,
                        validators: validators.clone(),
                    },
                    Some(Sibling {
                        path,
                        info,
                        validators,
                    }),
                )
            }
            None => (Slot::Absent, None),
        };
        store_stamped(original, ident, slot_i, slot, stamp, watch_gen, max_entries);
        ret
    }

    fn store(
        original: &Path,
        ident: u64,
        slot_i: usize,
        slot: Slot,
        watch_gen: Option<crate::cache::watch::WatchGen>,
        max_entries: usize,
    ) {
        let stamp = crate::cache::watch::stamp(watch_gen);
        store_stamped(original, ident, slot_i, slot, stamp, watch_gen, max_entries);
    }

    fn store_stamped(
        original: &Path,
        ident: u64,
        slot_i: usize,
        slot: Slot,
        stamp: Option<crate::cache::watch::Stamp>,
        watch_gen: Option<crate::cache::watch::WatchGen>,
        max_entries: usize,
    ) {
        let now = crate::runtime::time::coarse_now();
        if let Some(mut e) = TABLE.get_mut(original) {
            // 同じ元のファイルの表なら他の兄弟の結果は残す（世代が変わっていれば捨てる）
            if e.ident != ident || !crate::cache::watch::fresh(e.stamp, watch_gen) {
                e.slots = [Slot::Unknown, Slot::Unknown, Slot::Unknown];
                e.ident = ident;
                e.stamp = stamp;
                e.cached_at = now;
            }
            e.slots[slot_i] = slot;
            return;
        }
        if TABLE.len() >= max_entries.max(1) {
            // 上限: 1 割を捨てる（open_file_cache と同じ考え方。順序は問わない）
            let drop_n = (max_entries / 10).max(1);
            let victims: Vec<Box<Path>> =
                TABLE.iter().take(drop_n).map(|e| e.key().clone()).collect();
            for k in victims {
                TABLE.remove(&k);
            }
        }
        let mut slots = [Slot::Unknown, Slot::Unknown, Slot::Unknown];
        slots[slot_i] = slot;
        TABLE.insert(
            original.into(),
            Entry {
                ident,
                slots,
                cached_at: now,
                stamp,
            },
        );
    }

    /// 表を空にする（open_file_cache を空にするときに一緒に）
    pub(crate) fn clear() {
        TABLE.clear();
    }
}

#[cfg(not(feature = "cache"))]
mod imp {
    use super::*;

    /// `cache` 無効時は表を持たず毎回探す
    pub(super) async fn lookup(
        original: &Path,
        _ident: u64,
        original_validators: &Validators,
        mime: &str,
        enc: AcceptedEncoding,
        ofc: Option<&OpenFileCacheConfig>,
        containment: Option<RouteContainment<'_>>,
    ) -> Option<Sibling> {
        let path = sibling_path(original, enc)?;
        let info = probe(&path, original_validators.mtime_secs, ofc, containment).await?;
        let validators = sibling_validators(&info, original_validators, mime, enc);
        Some(Sibling {
            path: path.into(),
            info,
            validators,
        })
    }
}

#[cfg(feature = "cache")]
pub(crate) use imp::clear;

/// HTTP/2・HTTP/3 用: 兄弟を選んで本体（`Bytes`）まで読む（静的コンテンツキャッシュを通す）。
/// 戻り値は本体・検証子・兄弟のパス
#[allow(clippy::too_many_arguments)]
pub async fn select_with_content(
    original: &Path,
    original_size: u64,
    original_validators: &Validators,
    mime: &str,
    accept_encoding: Option<&[u8]>,
    pre: Precompressed,
    compression: &CompressionConfig,
    ofc: Option<&OpenFileCacheConfig>,
    content_cfg: &super::StaticContentCacheConfig,
    containment: Option<RouteContainment<'_>>,
) -> Option<(bytes::Bytes, Arc<Validators>, Arc<Path>)> {
    let sib = select(
        original,
        original_size,
        original_validators,
        mime,
        accept_encoding,
        pre,
        compression,
        ofc,
        containment,
    )
    .await?;
    match super::get_static_file_with_content(&sib.path, ofc, content_cfg, containment).await? {
        super::StaticFileOutcome::File(info, data)
            if info.file_size == sib.info.file_size && data.len() as u64 == info.file_size =>
        {
            Some((data, sib.validators, sib.path))
        }
        _ => None,
    }
}
