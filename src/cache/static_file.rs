//! 静的配信のメタデータ+本体を 1 回の offload で取得する複合 API（B-65）
//!
//! 従来の静的配信（`proxy.rs` `h2_sendfile`・`http3_server.rs` `handle_sendfile`）は
//! 1 リクエストあたり `runtime::offload` を **2 回**呼んでいた:
//!
//! 1. `cache::get_file_info_with_config` → `fetch_file_info`（パス解決 + stat）
//! 2. `cache::get_or_load_content_cache` → `std::fs::read`（本体読み込み）
//!
//! F-153 の `resolve::open_beneath_for_request`（Linux）/
//! `security::capsicum::open_static_ro`（FreeBSD）は、登録済み静的ルート配下であれば
//! **既にファイルを open して fstat している**。その同じ offload クロージャの中で
//! 本体まで読んでしまえば、往復は 1 回で済む。
//!
//! ## 経路
//!
//! 1. **両キャッシュヒット**（`open_file_cache` のメタデータ + `static_file_cache` の
//!    本体）: offload を 1 回も呼ばず `Bytes::clone()`（参照カウント）のみで返す。
//! 2. **登録済みルート配下**（`resolve::has_fast_path`/
//!    `security::capsicum::is_registered_static_root` が true）: 1 回の offload で
//!    open + fstat + 本体読み込みをまとめて行い、両方のキャッシュへ登録する。
//! 3. **上記が使えない**（未登録ルート・本プラットフォーム未対応）: 従来どおり
//!    `get_file_info_with_config` + `get_or_load_content_cache` の 2 回 offload へ
//!    フォールバックする（挙動不変）。
//!
//! 経路 2 と 3 の判定は **offload を起動する前**に行う（syscall を伴わない事前判定
//! `has_fast_path`/`is_registered_static_root` を使う）。判定を offload の中で行うと、
//! 「登録済みルートでない」ことが分かった時点で既に offload 往復を 1 回消費しており、
//! そこから経路 3 のフォールバックへ進むと往復が 3 回に増えてしまう。
//!
//! ## ディレクトリルート対応（B-65 続き）
//!
//! 上記の高速経路は当初 `is_dir == false`（固定ファイルルート）でしか使われて
//! いなかった。実運用で一般的な **ディレクトリルート**（`type = "File"`, `path` が
//! ディレクトリ）でもこの経路を使えるようにするため、`containment` 引数を追加した:
//!
//! - `resolve::has_fast_path`/`is_registered_static_root` は「**どれか**の登録済み
//!   静的ルート配下」を示すに過ぎない（`register_static_roots` は全 File ルートの
//!   パスをまとめて登録するため）。ディレクトリルートで配信する際は、解決先が
//!   **そのリクエストを処理しているルート自身の** `base_path` 配下であることを
//!   別途検査しなければならない（そうしないと、あるルートの封じ込めを迂回して
//!   別ルート配下のファイルを配信してしまう「クロスルートアクセス」が発生する）。
//! - この検査は offload クロージャの中で **open + fstat の直後・本体を読む前**に
//!   行う（`open_and_read_fast`/`finish_fast` 参照）。失敗したら本体を読まずに
//!   `StaticFileOutcome::Forbidden` を返す。
//! - 解決先がディレクトリだった場合は本体を読まずに `StaticFileOutcome::Directory`
//!   を返す（呼び出し側が index ファイルを解決してから再度呼ぶ）。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;

use super::content_cache::{self, StaticContentCacheConfig};
use super::file_cache::{self, CachedFileInfo, OpenFileCacheConfig};
use super::resolve;
use super::sendfile_base_contains;

/// 静的配信の解決結果（B-65 続き: ディレクトリルート対応）。
pub enum StaticFileOutcome {
    /// 通常ファイル: メタデータ + 本体。
    File(CachedFileInfo, Bytes),
    /// ディレクトリだった: index 解決が必要（本体は読んでいない）。
    Directory(CachedFileInfo),
    /// per-route の封じ込め検査に失敗（403 相当）。**本体は読んでいない。**
    Forbidden,
}

/// per-route 封じ込め検査に使うパラメータ。
///
/// `sendfile_base_contains(file_canonical_path, canonical_base, base_path)` と同じ
/// 引数構成（`canonical_base` は config ロード時の解決失敗時に `None` になり得る）。
type Containment<'a> = (Option<&'a Path>, &'a Path);

/// 静的配信用: メタデータと本体を **1 回の offload** で取得する（B-65）。
///
/// - 両方のキャッシュがヒットする場合は offload を 1 回も呼ばない。
/// - どちらか欠けている場合、登録済みルート配下（高速経路が使える）なら 1 回の
///   offload でメタデータ+本体を取得し、両方のキャッシュへ登録する。
/// - 高速経路が使えない場合は従来の 2 回 offload 経路（`get_file_info_with_config` +
///   `get_or_load_content_cache`）へフォールバックする。
///
/// `path` は既にインデックス解決を終えた**配信対象そのもの**であること（呼び出し側が
/// 保証する）。
///
/// `containment`: ディレクトリルート（`is_dir = true`）の場合のみ
/// `Some((canonical_base, base_path))` を渡すこと。固定ファイルルートでは base_path
/// からの封じ込め検査が不要なため `None` でよい（ホットパスでの余分な確保を避けるため、
/// `None` の場合はこの関数内で追加のアロケーションを一切行わない）。
pub async fn get_static_file_with_content(
    path: &Path,
    ofc: Option<&OpenFileCacheConfig>,
    content_cfg: &StaticContentCacheConfig,
    containment: Option<Containment<'_>>,
) -> Option<StaticFileOutcome> {
    // 1. メタデータキャッシュがヒットしていれば、その場で封じ込め検査とディレクトリ
    //    判定まで済ませる（いずれも offload 不要）。
    if let Some(meta) = file_cache::get_file_cache().peek_with_config(path, ofc) {
        if let Some((canonical_base, base_path)) = containment {
            if !sendfile_base_contains(&meta.canonical_path, canonical_base, base_path) {
                return Some(StaticFileOutcome::Forbidden);
            }
        }
        if !meta.is_file {
            return Some(StaticFileOutcome::Directory(meta));
        }
        // 本体キャッシュもヒットしていれば offload ゼロ（ホットパス絶対規則:
        // Bytes::clone() のみ）。
        if let Some(data) = content_cache::get_cached(path, content_cfg).await {
            return Some(StaticFileOutcome::File(meta, data));
        }
        // 本体キャッシュのみ未ヒット: 下の高速経路（offload）で open+read し直す。
    }

    // 2. 登録済みルート配下なら 1 回の offload で open+fstat+封じ込め検査+read を
    //    まとめる。
    if fast_path_available(path) {
        let owned = path.to_path_buf();
        // containment が不要な場合（固定ファイルルート）は余分な確保をしない。
        let owned_containment: Option<(Option<PathBuf>, PathBuf)> =
            containment.map(|(cb, bp)| (cb.map(Path::to_path_buf), bp.to_path_buf()));
        // 理由付き allow: offload 専用ワーカースレッド内で実行され、イベントループを
        // ブロックしない（F-153 の open_beneath_for_request/open_static_ro と同じ許容箇所）。
        #[allow(clippy::disallowed_methods)]
        let loaded = crate::runtime::offload::offload(move || {
            let containment_ref: Option<Containment<'_>> = owned_containment
                .as_ref()
                .map(|(cb, bp)| (cb.as_deref(), bp.as_path()));
            open_and_read_fast(&owned, containment_ref)
        })
        .await;
        let outcome = loaded?;

        return Some(match outcome {
            FastOutcome::File(meta, data) => {
                // 両方のキャッシュへ登録する（各キャッシュの既存の有効化・上限判定を
                // そのまま使う）。
                file_cache::get_file_cache().insert_with_config(path, meta.clone(), ofc);
                content_cache::insert_bytes(
                    path,
                    data.clone(),
                    Arc::from(meta.mime_type.as_str()),
                    content_cfg,
                );
                StaticFileOutcome::File(meta, data)
            }
            FastOutcome::Directory(meta) => {
                file_cache::get_file_cache().insert_with_config(path, meta.clone(), ofc);
                StaticFileOutcome::Directory(meta)
            }
            FastOutcome::Forbidden => StaticFileOutcome::Forbidden,
        });
    }

    // 3. フォールバック: 従来どおり 2 回 offload（本体を読む前に封じ込め検査を行う
    //    点のみ、以前は呼び出し側が担っていたものをここへ統合した）。
    let meta = file_cache::get_file_info_with_config(path, ofc).await?;
    if let Some((canonical_base, base_path)) = containment {
        if !sendfile_base_contains(&meta.canonical_path, canonical_base, base_path) {
            return Some(StaticFileOutcome::Forbidden);
        }
    }
    if !meta.is_file {
        return Some(StaticFileOutcome::Directory(meta));
    }
    let data = content_cache::get_or_load(path, content_cfg).await?;
    Some(StaticFileOutcome::File(meta, data))
}

/// `path` が登録済み静的ルート配下で、高速経路（1 回の offload で open+read が
/// できる経路）を使えるかどうかを **syscall なし**で判定する。
#[inline]
fn fast_path_available(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        resolve::has_fast_path(path)
    }
    #[cfg(target_os = "freebsd")]
    {
        crate::security::capsicum::is_registered_static_root(path)
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        let _ = path;
        false
    }
}

/// offload クロージャ内（1 回の open+fstat+read）で得られる中間結果。
enum FastOutcome {
    File(CachedFileInfo, Bytes),
    Directory(CachedFileInfo),
    Forbidden,
}

/// offload 専用ワーカースレッド内で実行する同期関数: 登録済みルート配下の
/// ファイルを open し、同じ fd から fstat と本体読み込みの両方を行う。
///
/// **`std::fs::read(path)` で開き直してはならない**（fd を捨てて open し直すと
/// このモジュールの存在意義（1 回の offload で完結させること）が失われる）。
// 理由付き allow: offload 専用ワーカースレッド内から呼ばれる同期 FS 操作
// （呼び出し元 `get_static_file_with_content` が offload で包む。イベントループ非ブロック）。
#[allow(clippy::disallowed_methods)]
fn open_and_read_fast(path: &Path, containment: Option<Containment<'_>>) -> Option<FastOutcome> {
    #[cfg(target_os = "linux")]
    {
        match resolve::open_beneath_for_request(path)? {
            Ok((file, meta)) => finish_fast(path, file, meta, containment),
            // 登録済みルート配下と判定された上での失敗（404 相当）。
            Err(_) => None,
        }
    }
    #[cfg(target_os = "freebsd")]
    {
        match crate::security::capsicum::open_static_ro(path)? {
            Ok(file) => {
                // fstat は開いた File から取得する（F-123 の `stat_static` は open を
                // 伴わないため、本体読み込みが必須の本関数では fd を再利用できる
                // `open_static_ro` を使い、その fd から fstat する）。
                let meta = file.metadata().ok()?;
                finish_fast(path, file, meta, containment)
            }
            Err(_) => None,
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        let _ = (path, containment);
        None
    }
}

/// open+fstat 直後・本体を読む前に per-route 封じ込め検査とディレクトリ判定を行い、
/// 問題なければ本体を読み込む。
///
/// - Linux: `containment` が `Some` の場合、`/proc/self/fd/<fd>` を `readlink` して
///   **実際に解決されたパス**を取得し、それを `sendfile_base_contains` へ渡す。
///   `path`（リクエストから構築した未解決のパス文字列）をそのまま使うと、シンボリック
///   リンク越しに別ルート配下へ解決されたケースを見逃す（`path` は常に自ルートの
///   `base_path` を起点に構築されているため文字列としては常に一致してしまう）。
///   `readlink` に失敗した場合は安全側に倒し `Forbidden` とする（フェイルオープン厳禁）。
/// - FreeBSD: `security::capsicum::static_serving_active()`（この高速経路が使える間は
///   常に true）が有効な間、`cache::sendfile_base_contains` はパスの値によらず常に
///   `true` を返す設計（`cache/mod.rs` 参照）。capability mode 下では `/proc` が
///   使えないため、ここでは `path` をそのまま渡す（判定結果には影響しない）。
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn finish_fast(
    path: &Path,
    mut file: std::fs::File,
    meta: std::fs::Metadata,
    containment: Option<Containment<'_>>,
) -> Option<FastOutcome> {
    if let Some((canonical_base, base_path)) = containment {
        // 実解決パスが得られない場合（Linux で readlink 失敗等）はフェイルオープン
        // 厳禁のため Forbidden 扱いにする（404 に落として素通りさせない）。
        let resolved = match resolved_path_for_containment(path, &file) {
            Some(p) => p,
            None => return Some(FastOutcome::Forbidden),
        };
        if !sendfile_base_contains(&resolved, canonical_base, base_path) {
            return Some(FastOutcome::Forbidden);
        }
    }

    let info = CachedFileInfo::from_open_metadata(path, &meta);
    if !info.is_file {
        return Some(FastOutcome::Directory(info));
    }

    let (info, data) = read_into_bytes(path, &mut file, &meta)?;
    Some(FastOutcome::File(info, data))
}

/// per-route 封じ込め検査に使う「実際に解決されたパス」を求める。
///
/// `path`（リクエストから構築した未解決のパス）はここでは使わない（シグネチャは
/// FreeBSD 版と揃えるため引数として残す）。
#[cfg(target_os = "linux")]
fn resolved_path_for_containment(_path: &Path, file: &std::fs::File) -> Option<PathBuf> {
    use std::os::unix::io::AsRawFd;
    let proc_path = format!("/proc/self/fd/{}", file.as_raw_fd());
    // 理由付き allow: offload ワーカースレッド内の readlink（canonicalize 相当。
    // ディレクトリルートの per-route 封じ込め検査にのみ必要で、固定ファイルルート
    // （containment = None）では呼ばれない）。
    //
    // readlink 失敗（/proc 未マウント等、通常は発生しない）は `None` を返し、
    // 呼び出し元でフェイルオープン厳禁のため Forbidden 扱いにする。
    #[allow(clippy::disallowed_methods)]
    std::fs::read_link(&proc_path).ok()
}

#[cfg(target_os = "freebsd")]
fn resolved_path_for_containment(path: &Path, _file: &std::fs::File) -> Option<PathBuf> {
    // capsicum cap_enter 下では /proc が使えないため実解決パスは取得しない。
    // `sendfile_base_contains` は `capsicum::static_serving_active()` が true の間
    // （＝この高速経路が有効な間は必ず true）、渡すパスの値によらず true を返す設計
    // なので、raw path をそのまま返しても判定結果には影響しない。
    Some(path.to_path_buf())
}

/// 既に開いている `File` と、それから取得済みの `Metadata` から
/// メタデータ + 本体をまとめて構築する（新規 open を発生させない）。
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn read_into_bytes(
    path: &Path,
    file: &mut std::fs::File,
    meta: &std::fs::Metadata,
) -> Option<(CachedFileInfo, Bytes)> {
    // Metadata::len() で 1 回だけ確保し、リサイズの繰り返しを避ける。
    let mut buf: Vec<u8> = Vec::with_capacity(meta.len() as usize);
    file.read_to_end(&mut buf).ok()?;
    let info = CachedFileInfo::from_open_metadata(path, meta);
    // Vec<u8> -> Bytes はムーブ（memcpy 無し）。
    Some((info, Bytes::from(buf)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::offload::{offload_call_count, reset_offload_call_count};
    use std::io::Write;
    use tempfile::tempdir;

    fn test_ofc() -> OpenFileCacheConfig {
        OpenFileCacheConfig {
            enabled: Some(true),
            valid_duration_secs: Some(60),
            max_entries: Some(1024),
        }
    }

    fn test_content_cfg() -> StaticContentCacheConfig {
        StaticContentCacheConfig {
            enabled: true,
            valid_duration_secs: 60,
            max_entries: 1024,
            max_file_size_bytes: 1024 * 1024,
            max_total_bytes: 64 * 1024 * 1024,
            revalidate_mtime: false,
        }
    }

    fn write_file_in(dir: &Path, name: &str, content: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        // 理由付き allow: テストのフィクスチャ作成（ホットパス外）。
        #[allow(clippy::disallowed_methods)]
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content).unwrap();
        path
    }

    fn write_file(dir: &tempfile::TempDir, name: &str, content: &[u8]) -> std::path::PathBuf {
        write_file_in(dir.path(), name, content)
    }

    /// 高速経路（登録済みルート配下）を有効化する。Linux では `resolve` の
    /// dirfd 登録、FreeBSD では `security::capsicum` の dirfd 登録を使う
    /// （どちらもプロセス全体で 1 回だけ有効化される `OnceLock` ベースのため、
    /// このテストファイル内では 1 箇所からのみ呼ぶこと）。
    #[cfg(target_os = "linux")]
    fn register_test_root(dir: &Path) {
        resolve::register_static_roots(&[dir.to_path_buf()]);
    }
    #[cfg(target_os = "freebsd")]
    fn register_test_root(dir: &Path) {
        let _ = crate::security::capsicum::init_static_dirfds(&[dir.to_path_buf()]);
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    fn register_test_root(_dir: &Path) {}

    /// `content_cache.rs`/`file_cache.rs` のテストと同じ方針: 新規スレッド上で
    /// `futures::executor::block_on` する。新規スレッドはランタイムドライバ未初期化が
    /// 保証されるため `offload()` は同期インライン実行に落ち、`block_on` が即座に
    /// 完了する（詳細は `content_cache.rs` の `block_on_fresh_thread` doc 参照）。
    fn block_on_fresh_thread<T, F>(fut_fn: F) -> T
    where
        F: FnOnce() -> T + Send,
        T: Send,
    {
        std::thread::scope(|s| s.spawn(fut_fn).join().expect("test thread panicked"))
    }

    /// 内容とサイズを正しく返すこと（高速経路が使えない環境でもフォールバック経路で
    /// 同じ結果になること）。
    #[test]
    fn returns_correct_content_and_size() {
        let dir = tempdir().unwrap();
        let path = write_file(&dir, "hello.txt", b"hello, world");
        let ofc = test_ofc();
        let content_cfg = test_content_cfg();

        let result = block_on_fresh_thread(|| {
            futures::executor::block_on(get_static_file_with_content(
                &path,
                Some(&ofc),
                &content_cfg,
                None,
            ))
        });
        match result.expect("既存ファイルは取得できるべき") {
            StaticFileOutcome::File(meta, data) => {
                assert_eq!(meta.file_size, 12);
                assert_eq!(data.as_ref(), b"hello, world");
            }
            _ => panic!("通常ファイルは File を返すべき"),
        }
    }

    /// 存在しないファイルは `None` になること。
    #[test]
    fn nonexistent_file_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does_not_exist.txt");
        let ofc = test_ofc();
        let content_cfg = test_content_cfg();

        let result = block_on_fresh_thread(|| {
            futures::executor::block_on(get_static_file_with_content(
                &path,
                Some(&ofc),
                &content_cfg,
                None,
            ))
        });
        assert!(result.is_none());
    }

    /// 本改修の肝: 両方のキャッシュがヒットしている場合、`get_static_file_with_content`
    /// は `runtime::offload::offload` を **1 回も**呼ばないこと。
    ///
    /// 直接キャッシュへ事前登録してから呼び出すことで「両方ヒット」の状態を作り、
    /// `offload_call_count()`（`runtime::offload` のテスト専用フック）で
    /// 呼び出し回数がゼロのままであることを検証する。
    #[test]
    fn both_cache_hit_calls_offload_zero_times() {
        let dir = tempdir().unwrap();
        let path = write_file(&dir, "cached.txt", b"cached payload");
        let ofc = test_ofc();
        let content_cfg = test_content_cfg();

        // 事前に両方のキャッシュへ登録しておく（1 回目の取得で自然にキャッシュされる）。
        let warmup = block_on_fresh_thread(|| {
            reset_offload_call_count();
            let r = futures::executor::block_on(get_static_file_with_content(
                &path,
                Some(&ofc),
                &content_cfg,
                None,
            ));
            (r, offload_call_count())
        });
        assert!(warmup.0.is_some());
        // 1 回目は未キャッシュのため offload が 1 回（高速経路）または 2 回
        // （フォールバック経路）呼ばれているはず。
        assert!(warmup.1 >= 1, "初回アクセスは offload を伴うはず");

        // 2 回目: 両方のキャッシュがヒットするので offload は一切呼ばれないはず。
        let (result, call_count) = block_on_fresh_thread(|| {
            reset_offload_call_count();
            let r = futures::executor::block_on(get_static_file_with_content(
                &path,
                Some(&ofc),
                &content_cfg,
                None,
            ));
            (r, offload_call_count())
        });
        match result.expect("キャッシュヒットでも値は返るべき") {
            StaticFileOutcome::File(meta, data) => {
                assert_eq!(data.as_ref(), b"cached payload");
                assert_eq!(meta.file_size, 14);
            }
            _ => panic!("通常ファイルは File を返すべき"),
        }
        assert_eq!(
            call_count, 0,
            "両キャッシュヒット時は offload を 1 回も呼んではならない"
        );
    }

    /// `static_file_cache` の上限（`max_file_size_bytes`）を超える場合、本体は
    /// キャッシュされないがレスポンス自体は正しく返ること。
    #[test]
    fn oversized_file_not_cached_but_response_correct() {
        let dir = tempdir().unwrap();
        let payload = vec![0xABu8; 4096];
        let path = write_file(&dir, "big.bin", &payload);
        let ofc = test_ofc();
        let mut content_cfg = test_content_cfg();
        content_cfg.max_file_size_bytes = 1024; // ファイルより小さい上限

        let result = block_on_fresh_thread(|| {
            futures::executor::block_on(get_static_file_with_content(
                &path,
                Some(&ofc),
                &content_cfg,
                None,
            ))
        });
        let (meta, data) = match result.expect("上限超過でも読み込み自体は成功するべき")
        {
            StaticFileOutcome::File(meta, data) => (meta, data),
            _ => panic!("通常ファイルは File を返すべき"),
        };
        assert_eq!(meta.file_size, 4096);
        assert_eq!(data.len(), 4096);
        assert_eq!(data.as_ref(), payload.as_slice());

        // 上限を超えるファイルは content cache に挿入されていないはず。
        //
        // 注意: ここで `content_cache::len()` の増減を見てはならない。本体キャッシュは
        // **プロセス全体で共有されるシングルトン**であり、他のテストが並列に挿入すると
        // 件数が動いて偽陽性になる（実際に単体では通るのに全体実行でだけ落ちた）。
        // 「このパスがキャッシュされていないこと」を直接確かめる。
        let cached = block_on_fresh_thread(|| {
            futures::executor::block_on(content_cache::get_cached(&path, &content_cfg))
        });
        assert!(
            cached.is_none(),
            "max_file_size_bytes を超えるファイルは本体キャッシュへ挿入されない"
        );

        // 2 回目のアクセスも offload を伴う（本体キャッシュが無いため）が、
        // 結果は変わらず正しいこと。
        let result2 = block_on_fresh_thread(|| {
            futures::executor::block_on(get_static_file_with_content(
                &path,
                Some(&ofc),
                &content_cfg,
                None,
            ))
        });
        match result2.expect("2 回目も成功するべき") {
            StaticFileOutcome::File(_, data2) => {
                assert_eq!(data2.as_ref(), payload.as_slice());
            }
            _ => panic!("通常ファイルは File を返すべき"),
        }
    }

    /// per-route 封じ込め検査に失敗した場合は `Forbidden` を返し、**本体を読まない**
    /// こと（ディレクトリルートのクロスルートアクセス対策、B-65 続き）。
    ///
    /// `containment` に渡した base_path 配下ではない実在パスを与えることで、
    /// 「登録済みルート配下ではある（あるいはフォールバック経路）が、この *ルート自身*
    /// の base_path 配下ではない」ケースを再現する。
    #[test]
    fn forbidden_outside_route_containment_does_not_read_body() {
        let dir = tempdir().unwrap();
        let allowed_root = dir.path().join("allowed");
        let other_root = dir.path().join("other");
        std::fs::create_dir(&allowed_root).unwrap();
        std::fs::create_dir(&other_root).unwrap();
        let secret_path = write_file_in(&other_root, "secret.txt", b"top secret");

        let ofc = test_ofc();
        let content_cfg = test_content_cfg();
        let canonical_allowed = allowed_root.canonicalize().unwrap();

        let result = block_on_fresh_thread(|| {
            futures::executor::block_on(get_static_file_with_content(
                &secret_path,
                Some(&ofc),
                &content_cfg,
                Some((Some(canonical_allowed.as_path()), allowed_root.as_path())),
            ))
        });
        assert!(
            matches!(result, Some(StaticFileOutcome::Forbidden)),
            "base_path 配下外のパスは Forbidden になるべき"
        );

        // 本体が content cache に入っていないこと（読まれていない証拠）。
        let cached = block_on_fresh_thread(|| {
            futures::executor::block_on(content_cache::get_cached(&secret_path, &content_cfg))
        });
        assert!(
            cached.is_none(),
            "封じ込め検査に失敗した場合は本体キャッシュへ挿入されてはならない（本体を \
             読んでいない証拠）"
        );
    }

    /// ディレクトリを指すパスを渡すと `Directory` が返り、本体は読まれないこと。
    #[test]
    fn directory_returns_without_reading_body() {
        let dir = tempdir().unwrap();
        let subdir = dir.path().join("subdir");
        std::fs::create_dir(&subdir).unwrap();

        let ofc = test_ofc();
        let content_cfg = test_content_cfg();

        let result = block_on_fresh_thread(|| {
            futures::executor::block_on(get_static_file_with_content(
                &subdir,
                Some(&ofc),
                &content_cfg,
                None,
            ))
        });
        match result.expect("ディレクトリも解決自体は成功するべき") {
            StaticFileOutcome::Directory(meta) => assert!(!meta.is_file),
            _ => panic!("ディレクトリは Directory を返すべき"),
        }

        let cached = block_on_fresh_thread(|| {
            futures::executor::block_on(content_cache::get_cached(&subdir, &content_cfg))
        });
        assert!(
            cached.is_none(),
            "ディレクトリの場合は本体を読んでいないので content cache にも入らない"
        );
    }

    /// 本改修の肝: ディレクトリルート配下の通常ファイルへのアクセスでも、
    /// 登録済みルート配下であれば offload が **1 回**で済むこと（B-65 続き）。
    #[test]
    fn directory_route_file_access_calls_offload_once() {
        let dir = tempdir().unwrap();
        let public_root = dir.path().join("public");
        std::fs::create_dir(&public_root).unwrap();
        let file_path = write_file_in(&public_root, "page.html", b"<html>ok</html>");

        // プロセス全体で 1 度きり有効化される登録（このテストファイル内で唯一の
        // 呼び出し箇所であること。詳細は `register_test_root` の doc 参照）。
        register_test_root(&public_root);

        let ofc = test_ofc();
        let content_cfg = test_content_cfg();
        let canonical_public = public_root.canonicalize().unwrap();

        let (result, call_count) = block_on_fresh_thread(|| {
            reset_offload_call_count();
            let r = futures::executor::block_on(get_static_file_with_content(
                &file_path,
                Some(&ofc),
                &content_cfg,
                Some((Some(canonical_public.as_path()), public_root.as_path())),
            ));
            (r, offload_call_count())
        });

        match result.expect("登録済みルート配下のファイルは取得できるべき") {
            StaticFileOutcome::File(_, data) => assert_eq!(data.as_ref(), b"<html>ok</html>"),
            StaticFileOutcome::Forbidden => {
                // Linux/FreeBSD 以外（高速経路が存在しない）では登録が無効なため
                // フォールバック経路を通り、containment 検査は正しく通過して File に
                // なるはず。ここに来た場合はテスト環境側の前提が崩れている。
                panic!("containment 検査に失敗した（テスト前提が崩れている）")
            }
            StaticFileOutcome::Directory(_) => panic!("ファイルのはずが Directory になった"),
        }

        // 高速経路が使えるプラットフォーム（Linux/FreeBSD）でのみ、offload が
        // 1 回で済むことを主張する。それ以外のプラットフォームは元々フォールバック
        // 経路（2 回 offload）しか持たないため、この主張自体が成立しない。
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        assert_eq!(
            call_count, 1,
            "登録済みディレクトリルート配下のファイルは offload 1 回で完結するべき"
        );
        #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
        {
            let _ = call_count;
        }
    }
}
