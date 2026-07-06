//! ディスクバッファ操作
//!
//! バッファリング時に大きいレスポンスをディスクにスピルオーバーする機能を提供します。

use std::path::PathBuf;
use xxhash_rust::xxh3::xxh3_64;

/// キーからディスクパスを生成（ディレクトリ分散）
///
/// ハッシュの上位ビットを使用して2層のディレクトリ構造を作成し、
/// ファイルシステムのディレクトリエントリ数を分散させます。
///
/// # 使用例
/// - テストコードでのパス生成
/// - バッファリング機能の拡張
/// - ディスクバッファの管理
///
/// # 注意
/// 実際のバッファリング処理では`disk_buffer`モジュールの関数を使用してください。
// 現在は単体テストのみで使用（実装済み RFC/ユーティリティヘルパー）
#[cfg_attr(not(test), allow(dead_code))]
pub fn key_to_path(base_path: &std::path::Path, key: &[u8]) -> PathBuf {
    let hash = xxh3_64(key);

    // ハッシュベースのパス生成
    let dir1 = format!("{:02x}", (hash >> 56) as u8);
    let dir2 = format!("{:02x}", (hash >> 48) as u8);
    let filename = format!("{:016x}.buf", hash);

    base_path.join(&dir1).join(&dir2).join(&filename)
}

/// ディスクバッファ操作（F-42: `runtime::offload` による完全非同期化）
///
/// ディスク I/O（create_dir_all / write / fsync / read / unlink）はブロッキング
/// システムコールのため、`runtime::offload`（専用スレッドプール + スレッドごと
/// eventfd の POLL_ADD で完了待機）でワーカースレッドへ退避し、**イベントループを
/// 決してブロックしない**（新規 io_uring オペコードは追加しない）。
#[cfg(target_os = "linux")]
pub mod disk_buffer {
    use std::io;
    use std::path::Path;
    use xxhash_rust::xxh3::xxh3_64;

    /// ディスクバッファへの非同期書き込み（offload 経由・イベントループ非ブロック）
    pub async fn write_to_disk(
        base_path: &Path,
        key: &[u8],
        data: Vec<u8>,
    ) -> io::Result<std::path::PathBuf> {
        let hash = xxh3_64(key);

        // ハッシュベースのパス生成（ディレクトリ分散）
        let dir1 = format!("{:02x}", (hash >> 56) as u8);
        let dir2 = format!("{:02x}", (hash >> 48) as u8);
        let filename = format!("{:016x}.buf", hash);

        let dir_path = base_path.join(&dir1).join(&dir2);
        let file_path = dir_path.join(&filename);

        // ブロッキング FS 操作一式をオフロードスレッドで実行
        let file_path_for_io = file_path.clone();
        crate::runtime::offload::offload(move || {
            std::fs::create_dir_all(&dir_path)?;
            use std::io::Write;
            let mut file = std::fs::File::create(&file_path_for_io)?;
            file.write_all(&data)?;
            file.sync_all()?;
            Ok::<(), io::Error>(())
        })
        .await?;

        Ok(file_path)
    }

    /// ディスクバッファからの非同期読み込み（offload 経由・イベントループ非ブロック）
    // 理由付き allow: 同期 FS は offload 閉包内（専用ワーカースレッド）で実行され、イベントループを塞がない。
    #[allow(clippy::disallowed_methods)]
    pub async fn read_from_disk(path: &Path) -> io::Result<Vec<u8>> {
        let path = path.to_path_buf();
        crate::runtime::offload::offload(move || {
            let metadata = std::fs::metadata(&path)?;
            let size = metadata.len() as usize;

            let mut buf = Vec::with_capacity(size);
            #[allow(clippy::uninit_vec)]
            unsafe {
                buf.set_len(size);
            }

            use std::io::Read;
            let mut file = std::fs::File::open(&path)?;
            file.read_exact(&mut buf)?;

            Ok(buf)
        })
        .await
    }

    /// ディスクバッファを削除（offload 経由・イベントループ非ブロック）
    // 理由付き allow: 同期 FS は offload 閉包内（専用ワーカースレッド）で実行され、イベントループを塞がない。
    #[allow(clippy::disallowed_methods)]
    pub async fn remove_disk_buffer(path: &Path) -> io::Result<()> {
        let path = path.to_path_buf();
        crate::runtime::offload::offload(move || std::fs::remove_file(&path)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // ====================
    // key_to_path テスト
    // ====================

    #[test]
    fn test_key_to_path_generates_valid_path() {
        // キーからパスを生成
        let base = Path::new("/tmp/buffer");
        let key = b"test-key-12345";

        let path = key_to_path(base, key);

        // パスが正しい構造を持つことを確認
        assert!(path.starts_with(base));
        assert!(path.extension().is_some_and(|ext| ext == "buf"));
    }

    #[test]
    fn test_key_to_path_consistency() {
        // 同じキーで同じパスが生成される
        let base = Path::new("/var/cache");
        let key = b"consistent-key";

        let path1 = key_to_path(base, key);
        let path2 = key_to_path(base, key);

        assert_eq!(path1, path2);
    }

    #[test]
    fn test_key_to_path_different_keys() {
        // 異なるキーで異なるパスが生成される
        let base = Path::new("/cache");
        let key1 = b"key-alpha";
        let key2 = b"key-beta";

        let path1 = key_to_path(base, key1);
        let path2 = key_to_path(base, key2);

        assert_ne!(path1, path2);
    }

    #[test]
    fn test_key_to_path_directory_structure() {
        // 2層のディレクトリ構造を持つ
        let base = Path::new("/base");
        let key = b"test";

        let path = key_to_path(base, key);

        // base/XX/YY/HASH.buf 形式
        let components: Vec<_> = path.components().collect();
        // /base + XX + YY + filename = 少なくとも4つのコンポーネント
        assert!(components.len() >= 4);
    }

    #[test]
    fn test_key_to_path_empty_key() {
        // 空のキーでもパスが生成される
        let base = Path::new("/tmp");
        let key = b"";

        let path = key_to_path(base, key);

        assert!(path.starts_with(base));
        assert!(path.to_string_lossy().ends_with(".buf"));
    }

    #[test]
    fn test_key_to_path_long_key() {
        // 長いキーでもパスが生成される
        let base = Path::new("/tmp");
        let key = vec![b'x'; 10000];

        let path = key_to_path(base, &key);

        // ファイル名の長さが適切（ハッシュ16桁 + .buf）
        let filename = path.file_name().unwrap().to_string_lossy();
        assert_eq!(filename.len(), 20); // 16 + 4 (.buf)
    }

    #[test]
    fn test_key_to_path_hash_distribution() {
        // 異なるキーでディレクトリが分散される
        let base = Path::new("/cache");
        let mut directories = std::collections::HashSet::new();

        for i in 0..100 {
            let key = format!("key-{}", i);
            let path = key_to_path(base, key.as_bytes());

            // 親ディレクトリ（XX/YY部分）を抽出
            if let Some(parent) = path.parent() {
                directories.insert(parent.to_path_buf());
            }
        }

        // 100個のキーで複数のディレクトリに分散されることを確認
        // （ハッシュの性質上、完全にユニークではない可能性があるが、1より多いはず）
        assert!(directories.len() > 1);
    }

    // ====================
    // disk_buffer（F-42: offload 非同期化後のラウンドトリップ）
    // ====================

    /// write → read → remove のラウンドトリップ。
    /// リング無しコンテキストでは offload が同期インライン実行にフォールバックするため、
    /// 軽量 executor（block_on 相当）なしで poll できるよう単純な待機で駆動する。
    #[test]
    fn test_disk_buffer_roundtrip() {
        use super::disk_buffer;

        fn block_on<F: std::future::Future>(mut fut: F) -> F::Output {
            use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
            fn noop(_: *const ()) {}
            fn clone(_: *const ()) -> RawWaker {
                RawWaker::new(std::ptr::null(), &VTABLE)
            }
            static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
            let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
            let mut cx = Context::from_waker(&waker);
            let mut fut = unsafe { std::pin::Pin::new_unchecked(&mut fut) };
            loop {
                match fut.as_mut().poll(&mut cx) {
                    Poll::Ready(v) => return v,
                    // リング無し環境では offload は同期実行のため Pending にならない
                    Poll::Pending => std::thread::yield_now(),
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let data = vec![0xABu8; 4096];
        let path = block_on(disk_buffer::write_to_disk(
            dir.path(),
            b"rt-key",
            data.clone(),
        ))
        .unwrap();
        assert!(path.exists());

        let read = block_on(disk_buffer::read_from_disk(&path)).unwrap();
        assert_eq!(read, data);

        block_on(disk_buffer::remove_disk_buffer(&path)).unwrap();
        assert!(!path.exists());
    }
}
