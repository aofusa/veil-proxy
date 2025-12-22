//! ディスクバッファ操作
//!
//! バッファリング時に大きいレスポンスをディスクにスピルオーバーする機能を提供します。

use xxhash_rust::xxh3::xxh3_64;
use std::path::PathBuf;

/// キーからディスクパスを生成（ディレクトリ分散）
/// 
/// ハッシュの上位ビットを使用して2層のディレクトリ構造を作成し、
/// ファイルシステムのディレクトリエントリ数を分散させます。
pub fn key_to_path(base_path: &std::path::Path, key: &[u8]) -> PathBuf {
    let hash = xxh3_64(key);
    
    // ハッシュベースのパス生成
    let dir1 = format!("{:02x}", (hash >> 56) as u8);
    let dir2 = format!("{:02x}", (hash >> 48) as u8);
    let filename = format!("{:016x}.buf", hash);
    
    base_path.join(&dir1).join(&dir2).join(&filename)
}

/// ディスクバッファ操作（monoio::fs使用）
#[cfg(target_os = "linux")]
pub mod disk_buffer {
    use monoio::fs::File;
    use std::io;
    use std::path::Path;
    use xxhash_rust::xxh3::xxh3_64;
    
    /// ディスクバッファへの非同期書き込み
    pub async fn write_to_disk(base_path: &Path, key: &[u8], data: Vec<u8>) -> io::Result<std::path::PathBuf> {
        let hash = xxh3_64(key);
        
        // ハッシュベースのパス生成（ディレクトリ分散）
        let dir1 = format!("{:02x}", (hash >> 56) as u8);
        let dir2 = format!("{:02x}", (hash >> 48) as u8);
        let filename = format!("{:016x}.buf", hash);
        
        let dir_path = base_path.join(&dir1).join(&dir2);
        let file_path = dir_path.join(&filename);
        
        // ディレクトリ作成（同期だが頻度は低い）
        std::fs::create_dir_all(&dir_path)?;
        
        // io_uring による非同期書き込み
        let file = File::create(&file_path).await?;
        let (res, _) = file.write_all_at(data, 0).await;
        res?;
        
        // fsync（データ整合性のため）
        file.sync_all().await?;
        
        Ok(file_path)
    }
    
    /// ディスクバッファからの非同期読み込み
    pub async fn read_from_disk(path: &Path) -> io::Result<Vec<u8>> {
        let file = File::open(path).await?;
        
        // ファイルサイズ取得
        let metadata = std::fs::metadata(path)?;
        let size = metadata.len() as usize;
        
        let mut buf = Vec::with_capacity(size);
        #[allow(clippy::uninit_vec)]
        unsafe { buf.set_len(size); }
        
        // io_uring による非同期読み込み
        let (res, buf) = file.read_exact_at(buf, 0).await;
        res?;
        
        Ok(buf)
    }
    
    /// ディスクバッファを削除
    pub fn remove_disk_buffer(path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
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
        assert!(path.extension().map_or(false, |ext| ext == "buf"));
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
}
