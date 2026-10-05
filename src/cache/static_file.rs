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
//!   保証しなければならない（そうしないと、あるルートの封じ込めを迂回して
//!   別ルート配下のファイルを配信してしまう「クロスルートアクセス」が発生する）。
//! - 解決先がディレクトリだった場合は本体を読まずに `StaticFileOutcome::Directory`
//!   を返す（呼び出し側が index ファイルを解決してから再度呼ぶ）。
//!
//! ### per-route 封じ込め: `readlink` を使わずカーネルに任せる（F-154）
//!
//! 当初（B-65）はこの保証を「まず `open_beneath_for_request`（どれかの登録済みルート）
//! で開いてから、`/proc/self/fd/<fd>` を `readlink` して実解決パスを求め、
//! `sendfile_base_contains` で事後検査する」という形で実装していた。これは
//! F-153 が `canonicalize()` の `readlink` 連発（7 回/リクエスト）をゼロにした
//! ところへ、`readlink` を 1 回/リクエスト新たに戻してしまっていた
//! （`docs/backlog/features/F-154-per-route-dirfd-containment.md` 参照）。
//!
//! `containment` が `Some` の場合、`open_and_read_fast` は `resolve::open_beneath_in_root`
//! を使い **`containment.root`（そのルート自身の `base_path`）の dirfd に対してのみ**
//! `openat2(RESOLVE_BENEATH)` する。これにより「開いてから検査し直す」のではなく
//! 「そもそもそのルートの外は開けない」構造になり、`readlink` はもちろん事後検査自体が
//! 不要になる（`finish_fast` 参照）。カーネルが per-route の封じ込めを直接保証する。

// `std::io::Read`（`read_to_end`）は高速経路（`read_into_bytes`）でのみ使う。
// 高速経路自体が Linux/FreeBSD 専用のため cfg で存在を絞る。
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use std::io::Read;
use std::path::Path;
// `PathBuf` は高速経路（`owned_root` の構築）でのみ使う。高速経路自体が
// Linux/FreeBSD 専用のため、他ターゲットでは unused import にならないよう cfg で絞る。
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use std::path::PathBuf;
// `Arc` は高速経路（MIME タイプ文字列の Arc 化）でのみ使う。理由は PathBuf と同じ。
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use std::sync::Arc;

use bytes::Bytes;

use super::content_cache::{self, StaticContentCacheConfig};
use super::file_cache::{self, CachedFileInfo, OpenFileCacheConfig};
// `resolve`（openat2/RESOLVE_BENEATH によるカーネル封じ込め）は Linux 専用経路でしか
// 使わない。FreeBSD は `security::capsicum` 側（`is_registered_static_root` /
// 生の base_path 配下も許可する `sendfile_base_contains`）を使うため、
// cfg で絞らないと FreeBSD ビルドで unused import の warning になる。
// テストも該当分はすべて `#[cfg(target_os = "linux")]` で絞ってある。
#[cfg(target_os = "linux")]
use super::resolve;
use super::sendfile_base_contains;
use super::RouteContainment;

/// 静的配信の解決結果（B-65 続き: ディレクトリルート対応）。
pub enum StaticFileOutcome {
    /// 通常ファイル: メタデータ + 本体。
    File(CachedFileInfo, Bytes),
    /// ディレクトリだった: index 解決が必要（本体は読んでいない）。
    Directory(CachedFileInfo),
    /// per-route の封じ込め検査に失敗（403 相当）。**本体は読んでいない。**
    Forbidden,
}

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
/// `containment`: ディレクトリルート（`is_dir = true`）の場合のみ `Some` を渡すこと。
/// 固定ファイルルートでは base_path からの封じ込め検査が不要なため `None` でよい
/// （ホットパスでの余分な確保を避けるため、`None` の場合はこの関数内で追加の
/// アロケーションを一切行わない）。
pub async fn get_static_file_with_content(
    path: &Path,
    ofc: Option<&OpenFileCacheConfig>,
    content_cfg: &StaticContentCacheConfig,
    containment: Option<RouteContainment<'_>>,
) -> Option<StaticFileOutcome> {
    // 1. メタデータキャッシュがヒットしていれば、その場で封じ込め検査とディレクトリ
    //    判定まで済ませる（いずれも offload 不要）。
    if let Some(meta) = file_cache::get_file_cache().peek_with_config(path, ofc) {
        if let Some(c) = containment {
            if !sendfile_base_contains(&meta.canonical_path, c.canonical_base, c.root) {
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

    // 2. 登録済みルート配下なら 1 回の offload で open+fstat+read をまとめる。
    //    F-154: `containment` が `Some` の場合、高速経路自体を「その `root`
    //    (base_path) の dirfd」に限定するため（`open_beneath_in_root`）、
    //    open が成功した時点でそれ自体が per-route の封じ込め証明になる。
    //    B-65 が使っていた `readlink` による事後検査は不要（かつ実装しない）。
    // 高速経路自体が Linux/FreeBSD にしか存在しないため、このブロックごと cfg で
    // 括る。それ以外のターゲットは fast_path_available を呼ぶまでもなく素通しで
    // 下の「3. フォールバック」へ進む（挙動は現状と同一: 従来もこのターゲットでは
    // fast_path_available が false を返して同じ経路を通っていた）。
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        if fast_path_available(path, containment) {
            let owned = path.to_path_buf();
            // containment が不要な場合（固定ファイルルート）は余分な確保をしない。
            let owned_root: Option<PathBuf> = containment.map(|c| c.root.to_path_buf());
            // 理由付き allow: offload 専用ワーカースレッド内で実行され、イベントループを
            // ブロックしない（F-153 の open_beneath_for_request/open_static_ro と同じ許容箇所）。
            #[allow(clippy::disallowed_methods)]
            let loaded = crate::runtime::offload::offload(move || {
                open_and_read_fast(&owned, owned_root.as_deref())
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
            });
        }
    }

    // 3. フォールバック: 従来どおり 2 回 offload（本体を読む前に封じ込め検査を行う
    //    点のみ、以前は呼び出し側が担っていたものをここへ統合した）。ここは
    //    canonicalize ベースなので per-route 検査（`sendfile_base_contains`）を
    //    省略してはならない。
    let meta = file_cache::get_file_info_with_config(path, ofc).await?;
    if let Some(c) = containment {
        if !sendfile_base_contains(&meta.canonical_path, c.canonical_base, c.root) {
            return Some(StaticFileOutcome::Forbidden);
        }
    }
    if !meta.is_file {
        return Some(StaticFileOutcome::Directory(meta));
    }
    let data = content_cache::get_or_load(path, content_cfg).await?;
    Some(StaticFileOutcome::File(meta, data))
}

/// `path`（`containment` が `None` の場合）または `containment.root`
/// （`Some` の場合）が登録済み静的ルートで、高速経路（1 回の offload で
/// open+read ができる経路）を使えるかどうかを **syscall なし**で判定する。
///
/// 高速経路自体が Linux/FreeBSD にしか存在しないため、この判定関数も cfg で
/// 存在を絞る（`#[allow(dead_code)]` は規約で禁止）。
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
#[inline]
fn fast_path_available(path: &Path, containment: Option<RouteContainment<'_>>) -> bool {
    if let Some(c) = containment {
        return route_fast_path_available(c.root);
    }
    #[cfg(target_os = "linux")]
    {
        resolve::has_fast_path(path)
    }
    #[cfg(target_os = "freebsd")]
    {
        crate::security::capsicum::is_registered_static_root(path)
    }
}

/// `containment` の `root` が高速経路として使えるか（F-154）。
///
/// Linux は「その `root` (base_path) が登録済みか」を直接判定する
/// （`resolve::has_fast_path_in_root`）。FreeBSD は本チケットの範囲外
/// （`resolve::open_beneath_in_root` の doc コメント参照：capsicum 側の
/// `sendfile_base_contains` が既に no-op のため `readlink` 相当の問題自体が無い）
/// のため、従来どおり「どれかの登録済みルート」探索のままにする。
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
#[inline]
fn route_fast_path_available(root: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        resolve::has_fast_path_in_root(root)
    }
    #[cfg(target_os = "freebsd")]
    {
        crate::security::capsicum::is_registered_static_root(root)
    }
}

/// offload クロージャ内（1 回の open+fstat+read）で得られる中間結果。
///
/// F-154: 高速経路は `open_and_read_fast` の呼び分け（`open_beneath_in_root`/
/// `open_beneath_for_request`）自体が per-route の封じ込めを保証するため、`Forbidden`
/// は存在しない（open が拒否された場合は単に `None` になり `open_and_read_fast` の
/// 呼び出し元がフォールバックへ進む）。
///
/// 高速経路自体が Linux/FreeBSD にしか存在しないため、この enum も cfg で
/// 存在を絞る（`#[allow(dead_code)]` は規約で禁止）。
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
enum FastOutcome {
    File(CachedFileInfo, Bytes),
    Directory(CachedFileInfo),
}

/// offload 専用ワーカースレッド内で実行する同期関数: 登録済みルート配下の
/// ファイルを open し、同じ fd から fstat と本体読み込みの両方を行う。
///
/// **`std::fs::read(path)` で開き直してはならない**（fd を捨てて open し直すと
/// このモジュールの存在意義（1 回の offload で完結させること）が失われる）。
///
/// `root`: ディレクトリルート（`containment` が `Some` だった場合）の `base_path`。
/// `Some` の場合、Linux は `resolve::open_beneath_in_root(root, path)` で **その
/// ルートの dirfd に対してのみ** open する（F-154）。これにより open が成功した
/// 時点でそれ自体が per-route の封じ込め証明になり、`readlink` による事後検査
/// （旧 `finish_fast`/`resolved_path_for_containment`）が不要になった。
// 理由付き allow: offload 専用ワーカースレッド内から呼ばれる同期 FS 操作
// （呼び出し元 `get_static_file_with_content` が offload で包む。イベントループ非ブロック）。
// 高速経路自体が Linux/FreeBSD にしか存在しないため、本関数も cfg で存在を絞る
// （`#[allow(dead_code)]` は規約で禁止）。
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
#[allow(clippy::disallowed_methods)]
fn open_and_read_fast(path: &Path, root: Option<&Path>) -> Option<FastOutcome> {
    #[cfg(target_os = "linux")]
    {
        let opened = match root {
            Some(root) => resolve::open_beneath_in_root(root, path),
            // 固定ファイルルート（containment 不要）: 「どれかの登録済みルート」探索
            // のままでよい（`path` は常に config 由来の固定値でユーザ入力に左右
            // されないため、クロスルートの懸念が無い）。
            None => resolve::open_beneath_for_request(path),
        };
        match opened? {
            Ok((file, meta)) => finish_fast(path, file, meta),
            // 登録済みルート配下と判定された上での失敗（404 相当）。
            Err(_) => None,
        }
    }
    #[cfg(target_os = "freebsd")]
    {
        let _ = root;
        match crate::security::capsicum::open_static_ro(path)? {
            Ok(file) => {
                // fstat は開いた File から取得する（F-123 の `stat_static` は open を
                // 伴わないため、本体読み込みが必須の本関数では fd を再利用できる
                // `open_static_ro` を使い、その fd から fstat する）。
                let meta = file.metadata().ok()?;
                finish_fast(path, file, meta)
            }
            Err(_) => None,
        }
    }
}

/// open+fstat 直後・本体を読む前にディレクトリ判定を行い、問題なければ本体を
/// 読み込む（F-154: per-route 封じ込め検査は `open_and_read_fast` 側の
/// `open_beneath_in_root`/`open_beneath_for_request` の呼び分けに移った。
/// ここで改めて `readlink` 等による事後検査を行う必要はない）。
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn finish_fast(
    path: &Path,
    mut file: std::fs::File,
    meta: std::fs::Metadata,
) -> Option<FastOutcome> {
    let info = CachedFileInfo::from_open_metadata(path, &meta);
    if !info.is_file {
        return Some(FastOutcome::Directory(info));
    }

    let (info, data) = read_into_bytes(path, &mut file, &meta)?;
    Some(FastOutcome::File(info, data))
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
    /// このテストファイル内では 1 箇所からのみ呼ぶこと。`registered_test_roots` 参照）。
    #[cfg(target_os = "linux")]
    fn register_test_roots(dirs: &[std::path::PathBuf]) {
        resolve::register_static_roots(dirs);
    }
    #[cfg(target_os = "freebsd")]
    fn register_test_roots(dirs: &[std::path::PathBuf]) {
        let _ = crate::security::capsicum::init_static_dirfds(dirs);
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    fn register_test_roots(_dirs: &[std::path::PathBuf]) {}

    /// F-154: 複数の登録済みルートを必要とするテスト（クロスルートアクセス拒否テスト
    /// を含む）が共有するフィクスチャ。
    ///
    /// `register_static_roots`/`init_static_dirfds` はプロセス全体で 1 回しか
    /// 有効化されない `OnceLock` ベース（2 回目以降の呼び出しは黙って無視される）。
    /// このテストファイル内で「登録済みルート」を必要とするテストが個々に
    /// 別々の一時ディレクトリを登録しようとすると、2 個目以降のテストの登録が
    /// 無視されて高速経路が有効化されず、そのテストのアサーション（offload 回数等）
    /// が「単体では通るが全体実行の順序次第で落ちる」というグローバル状態依存の
    /// 不安定なテストになる（過去に踏んだ罠と同型）。
    ///
    /// そのため、登録が必要なテストは全てこの 1 箇所を経由し、`root_a`/`root_b`
    /// という 2 つの独立したルートを共有する（`OnceLock::get_or_init` で最初の
    /// 呼び出し時に 1 回だけ登録される。以後の呼び出しは同じ参照を返すだけ）。
    ///
    /// 2 つの TempDir は配列で持つ（`[0]` = root_a、`[1]` = root_b）。root_b を読むのは
    /// Linux 専用テストだけなので、個別フィールドにすると非 Linux のテストビルドで
    /// 「never read」警告になる（TempDir は Drop で消えないよう保持し続ける必要がある）。
    struct RegisteredTestRoots {
        dirs: [tempfile::TempDir; 2],
    }

    static REGISTERED_TEST_ROOTS: std::sync::OnceLock<RegisteredTestRoots> =
        std::sync::OnceLock::new();

    fn registered_test_roots() -> &'static RegisteredTestRoots {
        REGISTERED_TEST_ROOTS.get_or_init(|| {
            let root_a = tempdir().unwrap();
            let root_b = tempdir().unwrap();
            register_test_roots(&[root_a.path().to_path_buf(), root_b.path().to_path_buf()]);
            RegisteredTestRoots {
                dirs: [root_a, root_b],
            }
        })
    }

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
                Some(RouteContainment {
                    root: allowed_root.as_path(),
                    canonical_base: Some(canonical_allowed.as_path()),
                }),
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
    ///
    /// F-154: 加えて `runtime::offload` の呼び出し回数だけでなく、**`readlink`
    /// を伴わずに**完了することを主張する（`open_and_read_fast`/`finish_fast` が
    /// per-route 封じ込めのために `/proc/self/fd` を読む経路をもう持たないため、
    /// この offload 1 回には readlink が含まれない。offload 呼び出し回数の主張自体が
    /// 「1 回の syscall 往復で完結する」ことの代理指標になっている）。
    #[test]
    fn directory_route_file_access_calls_offload_once() {
        let roots = registered_test_roots();
        let public_root = roots.dirs[0].path();
        let file_path = write_file_in(public_root, "page.html", b"<html>ok</html>");

        let ofc = test_ofc();
        let content_cfg = test_content_cfg();
        let canonical_public = public_root.canonicalize().unwrap();

        let (result, call_count) = block_on_fresh_thread(|| {
            reset_offload_call_count();
            let r = futures::executor::block_on(get_static_file_with_content(
                &file_path,
                Some(&ofc),
                &content_cfg,
                Some(RouteContainment {
                    root: public_root,
                    canonical_base: Some(canonical_public.as_path()),
                }),
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

    /// **最重要（F-154）**: 2 つの登録済みルート A・B があるとき、A を `containment.root`
    /// として渡して B 配下の実在パスを開こうとすると、B の内容は決して配信されないこと
    /// （クロスルートアクセスの拒否）。
    ///
    /// B-65 まではこの防御を「`open_beneath_for_request`（どれかの登録済みルート）で
    /// 開いてから `readlink` で実解決パスを求め、事後に `sendfile_base_contains` で
    /// 検査する」形で実装していた。この低レベルテストは、まさにその「any-root」探索
    /// （`open_beneath_for_request`）なら B にマッチして開けてしまうのに対し、
    /// F-154 のルート指定版（`open_beneath_in_root`）は A を指定した時点で B には
    /// 一切マッチしない（`strip_prefix` が失敗し `None`）ことを直接対比させて示す。
    #[cfg(target_os = "linux")]
    #[test]
    fn cross_route_open_beneath_in_root_rejects_other_route() {
        let roots = registered_test_roots();
        let root_a = roots.dirs[0].path();
        let root_b = roots.dirs[1].path();
        let file_in_b = write_file_in(root_b, "cross_route_secret.txt", b"route b secret");

        // 対照実験: 「どれかの登録済みルート」探索は B にマッチして成功する
        // （B-65 以前の挙動そのもの。F-154 が解消しようとしていた曖昧さの実例）。
        let any_root_result = resolve::open_beneath_for_request(&file_in_b);
        assert!(
            matches!(any_root_result, Some(Ok(_))),
            "any-root 探索（open_beneath_for_request）は登録済みの root_b にマッチして \
             成功するはず（比較対象）"
        );

        // 本題: root_a を明示的に指定すると、B 配下の実在パスは一切開けない。
        let cross_route_result = resolve::open_beneath_in_root(root_a, &file_in_b);
        assert!(
            cross_route_result.is_none(),
            "root_a を root として渡して root_b 配下のパスを開こうとすると None \
             （strip_prefix 失敗）になるべき — カーネルの封じ込め対象がそもそも \
             root_a の dirfd に限定されるため"
        );

        // 上位 API（get_static_file_with_content）を通しても、root_a の containment で
        // root_b の内容（"route b secret"）が配信されないこと。
        let ofc = test_ofc();
        let content_cfg = test_content_cfg();
        let canonical_a = root_a.canonicalize().unwrap();

        let result = block_on_fresh_thread(|| {
            futures::executor::block_on(get_static_file_with_content(
                &file_in_b,
                Some(&ofc),
                &content_cfg,
                Some(RouteContainment {
                    root: root_a,
                    canonical_base: Some(canonical_a.as_path()),
                }),
            ))
        });
        if let Some(StaticFileOutcome::File(_, data)) = &result {
            panic!(
                "root_a の containment で root_b 配下のファイルが配信されてしまった: {:?}",
                data
            );
        }

        // 本体が content cache に入っていないこと（読まれていない証拠）。
        let cached = block_on_fresh_thread(|| {
            futures::executor::block_on(content_cache::get_cached(&file_in_b, &content_cfg))
        });
        assert!(
            cached.is_none(),
            "クロスルートアクセスが拒否された場合は本体キャッシュへ挿入されてはならない \
             （本体を読んでいない証拠）"
        );
    }

    /// `..` によるルート外脱出が拒否されること（F-154）。
    ///
    /// 登録済みルート A の dirfd に対して `openat2(RESOLVE_BENEATH)` する高速経路は
    /// `..` を含む相対パスをカーネルレベルで拒否する（`RESOLVE_BENEATH` の仕様）。
    #[cfg(target_os = "linux")]
    #[test]
    fn cross_route_dot_dot_escape_is_rejected() {
        let roots = registered_test_roots();
        let root_a = roots.dirs[0].path();
        let root_b = roots.dirs[1].path();
        let _ = write_file_in(root_b, "dotdot_secret.txt", b"dotdot secret");

        // root_a 配下の相対パスとして "../<root_b の basename>/dotdot_secret.txt"
        // を構築する（`root_a` と `root_b` は共通の親ディレクトリ配下の兄弟）。
        let rel_escape = Path::new("..")
            .join(root_b.file_name().unwrap())
            .join("dotdot_secret.txt");
        let full_path = root_a.join(&rel_escape);

        let result = resolve::open_beneath_in_root(root_a, &full_path);
        assert!(
            matches!(result, Some(Err(_))),
            "'..' で root_a の外（root_b）へ出ようとするパスは拒否されるべき"
        );
    }

    /// ルート外を指す絶対シンボリックリンクが拒否されること（F-154、EXDEV →
    /// フォールバック → 含有チェックで最終拒否）。
    ///
    /// `open_beneath_in_root` の EXDEV/ELOOP 再判定（`fallback_open_beneath`）は
    /// 渡された `root` に対して行われるため、この経路でも per-route のまま維持される
    /// ことを確認する。
    #[cfg(target_os = "linux")]
    #[test]
    fn cross_route_absolute_symlink_escaping_root_is_rejected() {
        let roots = registered_test_roots();
        let root_a = roots.dirs[0].path();
        let root_b = roots.dirs[1].path();
        let secret = write_file_in(root_b, "symlink_secret.txt", b"symlink secret");

        let link_path = root_a.join("escape_link_f154");
        std::os::unix::fs::symlink(&secret, &link_path).unwrap();

        let result = resolve::open_beneath_in_root(root_a, &link_path);
        assert!(
            matches!(result, Some(Err(_))),
            "root_a 配下からルート外（root_b）を指す絶対シンボリックリンクは拒否される \
             べき（EXDEV → フォールバック含有チェックで最終拒否）"
        );
    }

    /// 正常系: 登録済みルート配下のファイルが読めること（F-154 の高速経路そのもの）。
    #[cfg(target_os = "linux")]
    #[test]
    fn open_beneath_in_root_allows_file_within_registered_root() {
        let roots = registered_test_roots();
        let root_a = roots.dirs[0].path();
        let file_path = write_file_in(root_a, "normal_f154.txt", b"normal file");

        let (mut file, meta) = resolve::open_beneath_in_root(root_a, &file_path)
            .expect("登録済みルートは高速経路の対象であるべき")
            .expect("ルート配下の実在ファイルは開けるべき");
        assert_eq!(meta.len(), 11);
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut file, &mut buf).unwrap();
        assert_eq!(buf, "normal file");
    }
}
