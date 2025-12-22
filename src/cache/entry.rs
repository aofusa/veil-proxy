//! キャッシュエントリ

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

/// キャッシュエントリ
/// 
/// キャッシュされたレスポンスのメタデータとボディを保持します。
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// レスポンスステータスコード
    pub status_code: u16,
    /// レスポンスヘッダー（名前-値ペアのリスト）
    pub headers: Arc<[(Box<[u8]>, Box<[u8]>)]>,
    /// ボディの格納場所
    pub storage: CacheStorage,
    /// キャッシュ作成時刻
    pub created_at: Instant,
    /// 有効期限（秒）
    pub max_age_secs: u64,
    /// ETag（条件付きリクエスト用）
    pub etag: Option<Box<str>>,
    /// Last-Modified
    pub last_modified: Option<Box<str>>,
    /// Content-Type
    pub content_type: Option<Box<str>>,
    /// Content-Encoding
    pub content_encoding: Option<Box<str>>,
    /// ボディサイズ
    pub body_size: u64,
}

impl CacheEntry {
    /// 新しいエントリを作成
    pub fn new(
        status_code: u16,
        headers: Vec<(Box<[u8]>, Box<[u8]>)>,
        storage: CacheStorage,
        max_age_secs: u64,
    ) -> Self {
        let body_size = storage.size();
        
        // ヘッダーから重要な値を抽出
        let mut etag = None;
        let mut last_modified = None;
        let mut content_type = None;
        let mut content_encoding = None;
        
        for (name, value) in &headers {
            if name.eq_ignore_ascii_case(b"etag") {
                if let Ok(s) = std::str::from_utf8(value) {
                    etag = Some(s.into());
                }
            } else if name.eq_ignore_ascii_case(b"last-modified") {
                if let Ok(s) = std::str::from_utf8(value) {
                    last_modified = Some(s.into());
                }
            } else if name.eq_ignore_ascii_case(b"content-type") {
                if let Ok(s) = std::str::from_utf8(value) {
                    content_type = Some(s.into());
                }
            } else if name.eq_ignore_ascii_case(b"content-encoding") {
                if let Ok(s) = std::str::from_utf8(value) {
                    content_encoding = Some(s.into());
                }
            }
        }
        
        Self {
            status_code,
            headers: headers.into(),
            storage,
            created_at: Instant::now(),
            max_age_secs,
            etag,
            last_modified,
            content_type,
            content_encoding,
            body_size,
        }
    }
    
    /// エントリが有効期限内かチェック
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.created_at.elapsed().as_secs() < self.max_age_secs
    }
    
    /// エントリの残りTTL（秒）を取得
    #[inline]
    pub fn remaining_ttl(&self) -> u64 {
        let elapsed = self.created_at.elapsed().as_secs();
        self.max_age_secs.saturating_sub(elapsed)
    }
    
    /// 期限切れ後の経過時間（秒）
    /// 
    /// stale-while-revalidate や stale-if-error で使用
    pub fn stale_duration(&self) -> u64 {
        let elapsed = self.created_at.elapsed().as_secs();
        elapsed.saturating_sub(self.max_age_secs)
    }
    
    /// ストレージがメモリ内かどうか
    #[inline]
    pub fn is_memory(&self) -> bool {
        matches!(self.storage, CacheStorage::Memory(_))
    }
    
    /// メモリ内のボディを取得
    #[inline]
    pub fn memory_body(&self) -> Option<&Arc<[u8]>> {
        match &self.storage {
            CacheStorage::Memory(data) => Some(data),
            _ => None,
        }
    }
    
    /// ディスクパスを取得
    #[inline]
    pub fn disk_path(&self) -> Option<&PathBuf> {
        match &self.storage {
            CacheStorage::Disk { path, .. } => Some(path),
            _ => None,
        }
    }
    
    /// 概算メモリ使用量を計算
    pub fn memory_usage(&self) -> usize {
        let mut size = std::mem::size_of::<Self>();
        
        // ヘッダーサイズ
        for (name, value) in self.headers.iter() {
            size += name.len() + value.len();
        }
        
        // ストレージサイズ
        if let CacheStorage::Memory(data) = &self.storage {
            size += data.len();
        }
        
        // オプショナルフィールド
        if let Some(s) = &self.etag {
            size += s.len();
        }
        if let Some(s) = &self.last_modified {
            size += s.len();
        }
        if let Some(s) = &self.content_type {
            size += s.len();
        }
        if let Some(s) = &self.content_encoding {
            size += s.len();
        }
        
        size
    }
}

/// キャッシュストレージ
#[derive(Debug, Clone)]
pub enum CacheStorage {
    /// インメモリキャッシュ（小さいレスポンス用）
    Memory(Arc<[u8]>),
    /// ディスクキャッシュ（大きいレスポンス用）
    Disk {
        path: PathBuf,
        size: u64,
    },
}

impl CacheStorage {
    /// ストレージサイズを取得
    pub fn size(&self) -> u64 {
        match self {
            CacheStorage::Memory(data) => data.len() as u64,
            CacheStorage::Disk { size, .. } => *size,
        }
    }
}

/// キャッシュエントリビルダー
#[allow(dead_code)]
pub struct CacheEntryBuilder {
    status_code: u16,
    headers: Vec<(Box<[u8]>, Box<[u8]>)>,
    body: Vec<u8>,
    max_age_secs: u64,
}

#[allow(dead_code)]
impl CacheEntryBuilder {
    /// 新しいビルダーを作成
    pub fn new(status_code: u16) -> Self {
        Self {
            status_code,
            headers: Vec::new(),
            body: Vec::new(),
            max_age_secs: 300, // デフォルト5分
        }
    }
    
    /// ヘッダーを追加
    pub fn header(mut self, name: &[u8], value: &[u8]) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
    
    /// ヘッダーリストを設定
    pub fn headers(mut self, headers: Vec<(Box<[u8]>, Box<[u8]>)>) -> Self {
        self.headers = headers;
        self
    }
    
    /// ボディを設定
    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }
    
    /// ボディにデータを追加
    pub fn append_body(mut self, data: &[u8]) -> Self {
        self.body.extend_from_slice(data);
        self
    }
    
    /// TTLを設定
    pub fn max_age(mut self, secs: u64) -> Self {
        self.max_age_secs = secs;
        self
    }
    
    /// メモリストレージとしてビルド
    pub fn build_memory(self) -> CacheEntry {
        let storage = CacheStorage::Memory(self.body.into());
        CacheEntry::new(self.status_code, self.headers, storage, self.max_age_secs)
    }
    
    /// ディスクストレージとしてビルド
    pub fn build_disk(self, path: PathBuf) -> (CacheEntry, Vec<u8>) {
        let size = self.body.len() as u64;
        let storage = CacheStorage::Disk { path, size };
        let entry = CacheEntry::new(self.status_code, self.headers, storage, self.max_age_secs);
        (entry, self.body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_entry_validity() {
        let entry = CacheEntryBuilder::new(200)
            .max_age(60)
            .body(b"test".to_vec())
            .build_memory();
        
        assert!(entry.is_valid());
        assert!(entry.remaining_ttl() <= 60);
    }

    #[test]
    fn test_entry_with_headers() {
        let entry = CacheEntryBuilder::new(200)
            .header(b"content-type", b"application/json")
            .header(b"etag", b"\"abc123\"")
            .header(b"last-modified", b"Mon, 01 Jan 2024 00:00:00 GMT")
            .body(b"{}".to_vec())
            .build_memory();
        
        assert_eq!(entry.content_type.as_deref(), Some("application/json"));
        assert_eq!(entry.etag.as_deref(), Some("\"abc123\""));
        assert!(entry.last_modified.is_some());
    }

    #[test]
    fn test_storage_size() {
        let memory = CacheStorage::Memory(vec![1, 2, 3, 4, 5].into());
        assert_eq!(memory.size(), 5);
        
        let disk = CacheStorage::Disk {
            path: PathBuf::from("/tmp/cache"),
            size: 1000,
        };
        assert_eq!(disk.size(), 1000);
    }

    #[test]
    fn test_memory_usage() {
        let entry = CacheEntryBuilder::new(200)
            .header(b"content-type", b"text/plain")
            .body(vec![0; 1000])
            .build_memory();
        
        let usage = entry.memory_usage();
        assert!(usage >= 1000); // ボディ + メタデータ
    }
}

