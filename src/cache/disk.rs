//! ディスクキャッシュ
//!
//! 大きいレスポンス用のディスクベースキャッシュを提供します。
//! monoio::fsを使用した非同期I/Oにより、ワーカースレッドのブロッキングを防ぎます。

use super::entry::CacheStorage;
use super::key::CacheKey;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// ディスクキャッシュ設定
#[derive(Debug, Clone)]
pub struct DiskCacheConfig {
    /// ベースディレクトリ
    pub base_path: PathBuf,
    /// 最大ディスク使用量（バイト）
    pub max_size: u64,
    /// ファイル拡張子
    pub extension: String,
    /// 非同期I/Oを使用するかどうか（io_uring有効時のみ）
    /// 
    /// - true: monoio::fsを使用した非同期I/O（io_uring）
    /// - false: 同期I/O（std::fs）
    /// 
    /// デフォルト: true（パフォーマンス向上のため）
    /// 
    /// 注意: Linux環境以外では自動的に同期I/Oにフォールバック
    pub use_async_io: bool,
}

impl Default for DiskCacheConfig {
    fn default() -> Self {
        Self {
            base_path: PathBuf::from("/var/cache/veil"),
            max_size: 1024 * 1024 * 1024, // 1GB
            extension: "cache".to_string(),
            use_async_io: true, // デフォルトで有効
        }
    }
}

/// ディスクキャッシュ
/// 
/// ファイルシステムベースのキャッシュストレージ。
/// monoio::fsを使用した非同期I/Oをサポート。
pub struct DiskCache {
    /// 設定
    config: DiskCacheConfig,
    /// 現在のディスク使用量（概算）
    current_size: AtomicU64,
    /// 書き込み回数
    writes: AtomicU64,
    /// 読み込み回数
    reads: AtomicU64,
    /// 作成時刻
    created_at: Instant,
}

impl DiskCache {
    /// 新しいディスクキャッシュを作成
    pub fn new(config: DiskCacheConfig) -> io::Result<Self> {
        // ベースディレクトリを作成
        std::fs::create_dir_all(&config.base_path)?;
        
        Ok(Self {
            config,
            current_size: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            created_at: Instant::now(),
        })
    }
    
    /// キャッシュキーからファイルパスを生成
    pub fn key_to_path(&self, key: &CacheKey) -> PathBuf {
        let (dir1, dir2, filename) = key.to_path_components();
        self.config.base_path
            .join(dir1)
            .join(dir2)
            .join(filename)
    }
    
    /// ハッシュ値からファイルパスを生成
    /// 
    /// 将来の使用に備えた機能。ハッシュベースのキャッシュ管理や
    /// キャッシュ移行時に使用可能。
    /// 
    /// # 使用例
    /// - キャッシュの再構築（メタデータのみからパスを復元）
    /// - ハッシュベースのキャッシュ検索
    #[allow(dead_code)]
    fn hash_to_path(&self, hash: u64) -> PathBuf {
        let dir1 = format!("{:02x}", (hash >> 56) as u8);
        let dir2 = format!("{:02x}", (hash >> 48) as u8);
        let filename = format!("{:016x}.{}", hash, self.config.extension);
        
        self.config.base_path
            .join(dir1)
            .join(dir2)
            .join(filename)
    }
    
    /// キャッシュファイルの存在確認
    pub fn exists(&self, key: &CacheKey) -> bool {
        self.key_to_path(key).exists()
    }
    
    /// 同期的なファイル読み込み（非推奨、互換性のため）
    pub fn read_sync(&self, key: &CacheKey) -> io::Result<Vec<u8>> {
        let path = self.key_to_path(key);
        self.reads.fetch_add(1, Ordering::Relaxed);
        std::fs::read(&path)
    }
    
    /// 同期的なファイル書き込み（非推奨、互換性のため）
    pub fn write_sync(&self, key: &CacheKey, data: &[u8]) -> io::Result<PathBuf> {
        let path = self.key_to_path(key);
        
        // 親ディレクトリを作成
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        
        std::fs::write(&path, data)?;
        
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.current_size.fetch_add(data.len() as u64, Ordering::Relaxed);
        
        Ok(path)
    }
    
    /// ファイルを削除
    pub fn remove(&self, key: &CacheKey) -> io::Result<()> {
        let path = self.key_to_path(key);
        
        if path.exists() {
            let metadata = std::fs::metadata(&path)?;
            let size = metadata.len();
            
            std::fs::remove_file(&path)?;
            self.current_size.fetch_sub(size, Ordering::Relaxed);
        }
        
        Ok(())
    }
    
    /// 現在のディスク使用量
    #[inline]
    pub fn current_size(&self) -> u64 {
        self.current_size.load(Ordering::Relaxed)
    }
    
    /// 最大ディスク使用量
    #[inline]
    pub fn max_size(&self) -> u64 {
        self.config.max_size
    }
    
    /// 書き込み回数
    #[inline]
    pub fn writes(&self) -> u64 {
        self.writes.load(Ordering::Relaxed)
    }
    
    /// 読み込み回数
    #[inline]
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }
    
    /// 稼働時間（秒）
    #[inline]
    pub fn uptime_secs(&self) -> u64 {
        self.created_at.elapsed().as_secs()
    }
    
    /// ディスク容量が十分かチェック
    pub fn has_capacity(&self, size: u64) -> bool {
        self.current_size() + size <= self.config.max_size
    }
    
    /// キャッシュディレクトリを走査してサイズを再計算
    pub fn recalculate_size(&self) -> io::Result<u64> {
        let mut total_size = 0u64;
        
        fn visit_dir(path: &Path, total: &mut u64) -> io::Result<()> {
            if path.is_dir() {
                for entry in std::fs::read_dir(path)? {
                    let entry = entry?;
                    let path = entry.path();
                    
                    if path.is_dir() {
                        visit_dir(&path, total)?;
                    } else if path.is_file() {
                        *total += entry.metadata()?.len();
                    }
                }
            }
            Ok(())
        }
        
        visit_dir(&self.config.base_path, &mut total_size)?;
        self.current_size.store(total_size, Ordering::Relaxed);
        
        Ok(total_size)
    }
    
    /// 古いキャッシュファイルを削除してディスク使用量を削減
    pub fn evict_to_size(&self, target_size: u64) -> io::Result<usize> {
        let current = self.current_size();
        
        if current <= target_size {
            return Ok(0);
        }
        
        // ファイルリストを取得（修正時刻順）
        let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
        
        fn collect_files(
            path: &Path,
            files: &mut Vec<(PathBuf, u64, std::time::SystemTime)>,
        ) -> io::Result<()> {
            if path.is_dir() {
                for entry in std::fs::read_dir(path)? {
                    let entry = entry?;
                    let path = entry.path();
                    
                    if path.is_dir() {
                        collect_files(&path, files)?;
                    } else if path.is_file() {
                        let metadata = entry.metadata()?;
                        let mtime = metadata.modified()?;
                        files.push((path, metadata.len(), mtime));
                    }
                }
            }
            Ok(())
        }
        
        collect_files(&self.config.base_path, &mut files)?;
        
        // 古い順にソート
        files.sort_by_key(|(_, _, mtime)| *mtime);
        
        let mut evicted = 0;
        let mut freed = 0u64;
        let to_free = current.saturating_sub(target_size) 
            + (target_size / 10); // 10%余分に解放
        
        for (path, size, _) in files {
            if freed >= to_free {
                break;
            }
            
            if std::fs::remove_file(&path).is_ok() {
                freed += size;
                evicted += 1;
            }
        }
        
        self.current_size.fetch_sub(freed, Ordering::Relaxed);
        
        Ok(evicted)
    }
    
    /// キャッシュディレクトリを完全に削除
    pub fn clear(&self) -> io::Result<()> {
        if self.config.base_path.exists() {
            std::fs::remove_dir_all(&self.config.base_path)?;
            std::fs::create_dir_all(&self.config.base_path)?;
        }
        self.current_size.store(0, Ordering::Relaxed);
        Ok(())
    }
    
    /// 非同期ファイル読み込み（io_uring使用）
    /// 
    /// `use_async_io`が有効な場合のみ使用可能。
    /// Linux環境以外では自動的に同期I/Oにフォールバック。
    #[cfg(target_os = "linux")]
    pub async fn read_async(&self, key: &CacheKey) -> io::Result<Vec<u8>> {
        let path = self.key_to_path(key);
        self.reads.fetch_add(1, Ordering::Relaxed);
        async_io::read_file(&path).await
    }
    
    /// 非同期ファイル書き込み（io_uring使用）
    /// 
    /// `use_async_io`が有効な場合のみ使用可能。
    /// Linux環境以外では自動的に同期I/Oにフォールバック。
    #[cfg(target_os = "linux")]
    pub async fn write_async(&self, key: &CacheKey, data: Vec<u8>) -> io::Result<PathBuf> {
        let path = self.key_to_path(key);
        let data_len = data.len() as u64;
        
        // 親ディレクトリを作成（同期だが頻度は低い）
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        
        async_io::write_file(&path, data).await?;
        
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.current_size.fetch_add(data_len, Ordering::Relaxed);
        
        Ok(path)
    }
    
    /// ファイルを読み込む（同期/非同期を自動選択）
    /// 
    /// `use_async_io`が有効でLinux環境の場合は非同期I/Oを使用し、
    /// それ以外の場合は同期I/Oを使用します。
    pub async fn read(&self, key: &CacheKey) -> io::Result<Vec<u8>> {
        if self.config.use_async_io {
            #[cfg(target_os = "linux")]
            {
                self.read_async(key).await
            }
            #[cfg(not(target_os = "linux"))]
            {
                // 非Linux環境では同期I/Oにフォールバック
                self.read_sync(key)
            }
        } else {
            // 同期I/Oを使用
            self.read_sync(key)
        }
    }
    
    /// ファイルを書き込む（同期/非同期を自動選択）
    /// 
    /// `use_async_io`が有効でLinux環境の場合は非同期I/Oを使用し、
    /// それ以外の場合は同期I/Oを使用します。
    pub async fn write(&self, key: &CacheKey, data: Vec<u8>) -> io::Result<PathBuf> {
        if self.config.use_async_io {
            #[cfg(target_os = "linux")]
            {
                self.write_async(key, data).await
            }
            #[cfg(not(target_os = "linux"))]
            {
                // 非Linux環境では同期I/Oにフォールバック
                self.write_sync(key, &data)
            }
        } else {
            // 同期I/Oを使用
            self.write_sync(key, &data)
        }
    }
}

/// monoio非同期I/O操作
/// 
/// io_uringを使用した非同期ファイルI/Oを提供します。
/// Linux環境でのみ利用可能です。
#[cfg(target_os = "linux")]
pub mod async_io {
    use monoio::fs::File;
    use std::io;
    use std::path::Path;
    
    /// 非同期ファイル読み込み
    pub async fn read_file(path: &Path) -> io::Result<Vec<u8>> {
        let file = File::open(path).await?;
        
        // ファイルサイズ取得（同期だが頻度は低い）
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
    
    /// 非同期ファイル書き込み
    pub async fn write_file(path: &Path, data: Vec<u8>) -> io::Result<()> {
        // 親ディレクトリを作成（同期だが頻度は低い）
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        
        // io_uring による非同期書き込み
        let file = File::create(path).await?;
        let (res, _) = file.write_all_at(data, 0).await;
        res?;
        
        // fsync（データ整合性のため）
        file.sync_all().await?;
        
        Ok(())
    }
    
    /// 非同期ファイル削除
    /// 
    /// 注意: monoio::fsにはremoveがないため同期操作
    /// 
    /// 将来の使用に備えた関数。非同期削除の実装が追加される可能性がある。
    #[allow(dead_code)]
    pub fn remove_file(path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
}

/// CacheStorageからディスクパスを抽出するヘルパー
#[allow(dead_code)]
pub fn get_disk_path(storage: &CacheStorage) -> Option<&PathBuf> {
    match storage {
        CacheStorage::Disk { path, .. } => Some(path),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::key::CacheableMethod;
    use tempfile::tempdir;

    fn create_test_key(path: &str) -> CacheKey {
        CacheKey::new(CacheableMethod::Get, "example.com", path, None)
    }

    #[test]
    fn test_disk_cache_new() {
        let dir = tempdir().unwrap();
        let config = DiskCacheConfig {
            base_path: dir.path().to_path_buf(),
            max_size: 1024 * 1024,
            extension: "cache".to_string(),
            use_async_io: false, // テストでは同期I/Oを使用
        };
        
        let cache = DiskCache::new(config).unwrap();
        assert_eq!(cache.current_size(), 0);
    }

    #[test]
    fn test_key_to_path() {
        let dir = tempdir().unwrap();
        let config = DiskCacheConfig {
            base_path: dir.path().to_path_buf(),
            max_size: 1024 * 1024,
            extension: "cache".to_string(),
            use_async_io: false, // テストでは同期I/Oを使用
        };
        
        let cache = DiskCache::new(config).unwrap();
        let key = create_test_key("/test");
        
        let path = cache.key_to_path(&key);
        assert!(path.to_string_lossy().contains(".cache"));
    }

    #[test]
    fn test_write_and_read_sync() {
        let dir = tempdir().unwrap();
        let config = DiskCacheConfig {
            base_path: dir.path().to_path_buf(),
            max_size: 1024 * 1024,
            extension: "cache".to_string(),
            use_async_io: false, // テストでは同期I/Oを使用
        };
        
        let cache = DiskCache::new(config).unwrap();
        let key = create_test_key("/test");
        let data = b"test data";
        
        // 書き込み
        let path = cache.write_sync(&key, data).unwrap();
        assert!(path.exists());
        assert_eq!(cache.writes(), 1);
        
        // 読み込み
        let read_data = cache.read_sync(&key).unwrap();
        assert_eq!(&read_data, data);
        assert_eq!(cache.reads(), 1);
    }

    #[test]
    fn test_remove() {
        let dir = tempdir().unwrap();
        let config = DiskCacheConfig {
            base_path: dir.path().to_path_buf(),
            max_size: 1024 * 1024,
            extension: "cache".to_string(),
            use_async_io: false, // テストでは同期I/Oを使用
        };
        
        let cache = DiskCache::new(config).unwrap();
        let key = create_test_key("/test");
        
        cache.write_sync(&key, b"test").unwrap();
        assert!(cache.exists(&key));
        
        cache.remove(&key).unwrap();
        assert!(!cache.exists(&key));
    }

    #[test]
    fn test_clear() {
        let dir = tempdir().unwrap();
        let config = DiskCacheConfig {
            base_path: dir.path().to_path_buf(),
            max_size: 1024 * 1024,
            extension: "cache".to_string(),
            use_async_io: false, // テストでは同期I/Oを使用
        };
        
        let cache = DiskCache::new(config).unwrap();
        
        for i in 0..5 {
            let key = create_test_key(&format!("/test{}", i));
            cache.write_sync(&key, b"data").unwrap();
        }
        
        assert!(cache.current_size() > 0);
        
        cache.clear().unwrap();
        assert_eq!(cache.current_size(), 0);
    }
}

