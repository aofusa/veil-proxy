//! キャッシュ設定

use serde::Deserialize;
use std::path::PathBuf;
#[cfg(feature = "cache")]
use glob;

/// デフォルト値関数
fn default_max_memory_size() -> usize { 100 * 1024 * 1024 } // 100MB
fn default_max_disk_size() -> usize { 1024 * 1024 * 1024 } // 1GB
fn default_memory_threshold() -> usize { 64 * 1024 } // 64KB
fn default_ttl() -> u64 { 300 } // 5分
fn default_cacheable_methods() -> Vec<String> {
    vec!["GET".to_string(), "HEAD".to_string()]
}
fn default_cacheable_statuses() -> Vec<u16> {
    vec![200, 301, 302, 304]
}

/// キャッシュ設定
#[derive(Deserialize, Clone, Debug)]
pub struct CacheConfig {
    /// キャッシュを有効化
    /// 
    /// デフォルト: false
    #[serde(default)]
    pub enabled: bool,
    
    /// インメモリキャッシュ最大サイズ（バイト）
    /// 
    /// デフォルト: 100MB
    #[serde(default = "default_max_memory_size")]
    pub max_memory_size: usize,
    
    /// ディスクキャッシュパス
    /// 
    /// 未設定の場合はメモリのみ
    #[serde(default)]
    pub disk_path: Option<PathBuf>,
    
    /// ディスクキャッシュ最大サイズ（バイト）
    /// 
    /// デフォルト: 1GB
    #[serde(default = "default_max_disk_size")]
    pub max_disk_size: usize,
    
    /// メモリキャッシュ閾値（バイト）
    /// 
    /// これより大きいレスポンスはディスクへ保存
    /// 
    /// デフォルト: 64KB
    #[serde(default = "default_memory_threshold")]
    pub memory_threshold: usize,
    
    /// デフォルトTTL（秒）
    /// 
    /// Cache-Controlがない場合に使用
    /// 
    /// デフォルト: 300秒（5分）
    #[serde(default = "default_ttl")]
    pub default_ttl_secs: u64,
    
    /// キャッシュ対象HTTPメソッド
    /// 
    /// デフォルト: ["GET", "HEAD"]
    #[serde(default = "default_cacheable_methods")]
    pub methods: Vec<String>,
    
    /// キャッシュ対象ステータスコード
    /// 
    /// デフォルト: [200, 301, 302, 304]
    #[serde(default = "default_cacheable_statuses")]
    pub cacheable_statuses: Vec<u16>,
    
    /// キャッシュ除外パスパターン（globパターン）
    /// 
    /// 例: ["/api/user/*", "/api/session"]
    #[serde(default)]
    pub bypass_patterns: Vec<String>,
    
    /// Varyヘッダーを尊重するか
    /// 
    /// trueの場合、Varyヘッダーに基づいて別々のキャッシュエントリを作成
    /// 
    /// デフォルト: true
    #[serde(default = "default_true")]
    pub respect_vary: bool,
    
    /// ETag検証を有効化
    /// 
    /// trueの場合、If-None-MatchによるETag検証を行う
    /// 
    /// デフォルト: true
    #[serde(default = "default_true")]
    pub enable_etag: bool,
    
    /// stale-while-revalidateを有効化
    /// 
    /// trueの場合、期限切れキャッシュを返しながらバックグラウンドで更新
    /// 
    /// デフォルト: false
    #[serde(default)]
    pub stale_while_revalidate: bool,
    
    /// stale-if-errorを有効化
    /// 
    /// trueの場合、バックエンドエラー時に期限切れキャッシュを返す
    /// 
    /// デフォルト: false
    #[serde(default)]
    pub stale_if_error: bool,
    
    /// キャッシュキーにクエリパラメータを含めるか
    /// 
    /// デフォルト: true
    #[serde(default = "default_true")]
    pub include_query: bool,
    
    /// キャッシュキーに含めるヘッダー
    /// 
    /// 例: ["Authorization"]（ユーザーごとのキャッシュ）
    #[serde(default)]
    pub key_headers: Vec<String>,
    
    /// ディスクキャッシュで非同期I/Oを使用するかどうか（io_uring有効時のみ）
    /// 
    /// - true: monoio::fsを使用した非同期I/O（io_uring）
    /// - false: 同期I/O（std::fs）
    /// 
    /// デフォルト: true（パフォーマンス向上のため）
    /// 
    /// 注意: Linux環境以外では自動的に同期I/Oにフォールバック
    #[serde(default = "default_use_async_io")]
    pub use_async_io: bool,
}

/// use_async_ioのデフォルト値（true）
fn default_use_async_io() -> bool {
    true
}

fn default_true() -> bool { true }

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_memory_size: default_max_memory_size(),
            disk_path: None,
            max_disk_size: default_max_disk_size(),
            memory_threshold: default_memory_threshold(),
            default_ttl_secs: default_ttl(),
            methods: default_cacheable_methods(),
            cacheable_statuses: default_cacheable_statuses(),
            bypass_patterns: Vec::new(),
            respect_vary: true,
            enable_etag: true,
            stale_while_revalidate: false,
            stale_if_error: false,
            include_query: true,
            key_headers: Vec::new(),
            use_async_io: default_use_async_io(),
        }
    }
}

impl CacheConfig {
    /// メソッドがキャッシュ対象かチェック
    #[inline]
    pub fn is_cacheable_method(&self, method: &[u8]) -> bool {
        self.methods.iter().any(|m| m.as_bytes().eq_ignore_ascii_case(method))
    }
    
    /// ステータスコードがキャッシュ対象かチェック
    #[inline]
    pub fn is_cacheable_status(&self, status: u16) -> bool {
        self.cacheable_statuses.contains(&status)
    }
    
    /// パスがバイパスパターンにマッチするかチェック
    pub fn should_bypass(&self, _path: &str) -> bool {
        #[cfg(feature = "cache")]
        for pattern in &self.bypass_patterns {
            if let Ok(glob) = glob::Pattern::new(pattern) {
                if glob.matches(_path) {
                    return true;
                }
            }
        }
        false
    }
    
    /// ディスクキャッシュが使用可能かどうか
    #[inline]
    pub fn disk_available(&self) -> bool {
        self.disk_path.is_some()
    }
    
    /// レスポンスサイズに基づいてストレージを選択
    #[inline]
    pub fn select_storage(&self, size: usize) -> StorageType {
        if size <= self.memory_threshold {
            StorageType::Memory
        } else if self.disk_available() {
            StorageType::Disk
        } else {
            // ディスクが無い場合でもメモリに保存を試みる
            StorageType::Memory
        }
    }
}

/// ストレージタイプ
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageType {
    Memory,
    Disk,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = CacheConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.max_memory_size, 100 * 1024 * 1024);
        assert!(config.is_cacheable_method(b"GET"));
        assert!(config.is_cacheable_method(b"get"));
        assert!(!config.is_cacheable_method(b"POST"));
        assert!(config.is_cacheable_status(200));
        assert!(!config.is_cacheable_status(201));
    }

    #[test]
    #[cfg(feature = "cache")]
    fn test_bypass_patterns() {
        let config = CacheConfig {
            bypass_patterns: vec![
                "/api/user/*".to_string(),
                "/api/session".to_string(),
            ],
            ..Default::default()
        };

        assert!(config.should_bypass("/api/user/123"));
        assert!(config.should_bypass("/api/user/profile"));
        assert!(config.should_bypass("/api/session"));
        assert!(!config.should_bypass("/api/products"));
    }

    #[test]
    fn test_select_storage() {
        let config = CacheConfig {
            memory_threshold: 1024,
            disk_path: Some(PathBuf::from("/tmp")),
            ..Default::default()
        };
        
        assert_eq!(config.select_storage(512), StorageType::Memory);
        assert_eq!(config.select_storage(1024), StorageType::Memory);
        assert_eq!(config.select_storage(2048), StorageType::Disk);
    }
}

