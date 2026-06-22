//! キャッシュキー

use std::hash::{Hash, Hasher};
use xxhash_rust::xxh3::xxh3_64;

/// キャッシュ可能なHTTPメソッド
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheableMethod {
    Get,
    Head,
}

impl CacheableMethod {
    /// バイト列からパース
    pub fn from_bytes(method: &[u8]) -> Option<Self> {
        if method.eq_ignore_ascii_case(b"GET") {
            Some(CacheableMethod::Get)
        } else if method.eq_ignore_ascii_case(b"HEAD") {
            Some(CacheableMethod::Head)
        } else {
            None
        }
    }

    /// 文字列として取得
    pub fn as_str(&self) -> &'static str {
        match self {
            CacheableMethod::Get => "GET",
            CacheableMethod::Head => "HEAD",
        }
    }
}

/// キャッシュキー
///
/// リクエストを一意に識別するためのキー構造体。
/// ハッシュ計算に最適化されています。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheKey {
    /// HTTPメソッド
    method: CacheableMethod,
    /// ホスト名
    host: Box<str>,
    /// リクエストパス（クエリパラメータ含む場合あり）
    path: Box<str>,
    /// Varyヘッダーに基づく追加キー
    vary_key: Option<Box<str>>,
    /// 事前計算されたハッシュ値
    hash: u64,
}

impl CacheKey {
    /// 新しいキャッシュキーを作成
    pub fn new(method: CacheableMethod, host: &str, path: &str, vary_key: Option<&str>) -> Self {
        let hash = Self::compute_hash(method, host, path, vary_key);

        Self {
            method,
            host: host.into(),
            path: path.into(),
            vary_key: vary_key.map(Into::into),
            hash,
        }
    }

    /// リクエスト情報からキャッシュキーを生成
    ///
    /// # Arguments
    ///
    /// * `method` - HTTPメソッド
    /// * `host` - Hostヘッダー
    /// * `path` - リクエストパス
    /// * `query` - クエリパラメータ（include_queryがtrueの場合に使用）
    /// * `include_query` - クエリパラメータを含めるか
    /// * `vary_headers` - Varyヘッダーに基づく追加キー値
    pub fn from_request(
        method: &[u8],
        host: &str,
        path: &str,
        query: Option<&str>,
        include_query: bool,
        vary_headers: Option<&[(&str, &str)]>,
    ) -> Option<Self> {
        let method = CacheableMethod::from_bytes(method)?;

        // パスとクエリを結合
        let full_path = if include_query {
            match query {
                Some(q) if !q.is_empty() => format!("{}?{}", path, q),
                _ => path.to_string(),
            }
        } else {
            path.to_string()
        };

        // Varyキーを生成
        let vary_key = vary_headers.map(|headers| {
            let mut parts: Vec<String> = headers
                .iter()
                .map(|(name, value)| format!("{}:{}", name.to_lowercase(), value))
                .collect();
            parts.sort();
            parts.join(";")
        });

        Some(Self::new(method, host, &full_path, vary_key.as_deref()))
    }

    /// ハッシュ値を計算
    fn compute_hash(
        method: CacheableMethod,
        host: &str,
        path: &str,
        vary_key: Option<&str>,
    ) -> u64 {
        let mut data = Vec::with_capacity(host.len() + path.len() + 32);

        // メソッドを追加
        data.extend_from_slice(method.as_str().as_bytes());
        data.push(b'\x00');

        // ホストを追加
        data.extend_from_slice(host.as_bytes());
        data.push(b'\x00');

        // パスを追加
        data.extend_from_slice(path.as_bytes());

        // Varyキーを追加
        if let Some(vary) = vary_key {
            data.push(b'\x00');
            data.extend_from_slice(vary.as_bytes());
        }

        xxh3_64(&data)
    }

    /// メソッドを取得
    #[inline]
    pub fn method(&self) -> CacheableMethod {
        self.method
    }

    /// ホスト名を取得
    #[inline]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// パスを取得
    #[inline]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Varyキーを取得
    #[inline]
    pub fn vary_key(&self) -> Option<&str> {
        self.vary_key.as_deref()
    }

    /// ハッシュ値を取得
    #[inline]
    pub fn hash_value(&self) -> u64 {
        self.hash
    }

    /// ディスクキャッシュ用のファイルパス部分を生成
    pub fn to_path_components(&self) -> (String, String, String) {
        let dir1 = format!("{:02x}", (self.hash >> 56) as u8);
        let dir2 = format!("{:02x}", (self.hash >> 48) as u8);
        let filename = format!("{:016x}.cache", self.hash);
        (dir1, dir2, filename)
    }
}

impl Hash for CacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // 事前計算されたハッシュ値を使用
        state.write_u64(self.hash);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_key_creation() {
        let key = CacheKey::new(CacheableMethod::Get, "example.com", "/api/users", None);

        assert_eq!(key.method(), CacheableMethod::Get);
        assert_eq!(key.host(), "example.com");
        assert_eq!(key.path(), "/api/users");
        assert!(key.vary_key().is_none());
    }

    #[test]
    fn test_cache_key_from_request() {
        let key = CacheKey::from_request(
            b"GET",
            "example.com",
            "/api/users",
            Some("page=1&limit=10"),
            true,
            None,
        )
        .unwrap();

        assert_eq!(key.path(), "/api/users?page=1&limit=10");
    }

    #[test]
    fn test_cache_key_from_request_without_query() {
        let key = CacheKey::from_request(
            b"GET",
            "example.com",
            "/api/users",
            Some("page=1"),
            false, // クエリを含めない
            None,
        )
        .unwrap();

        assert_eq!(key.path(), "/api/users");
    }

    #[test]
    fn test_cache_key_with_vary() {
        let vary_headers = vec![("accept-encoding", "gzip"), ("accept-language", "en-US")];

        let key = CacheKey::from_request(
            b"GET",
            "example.com",
            "/api/data",
            None,
            true,
            Some(&vary_headers),
        )
        .unwrap();

        let vary_key = key.vary_key().unwrap();
        assert!(vary_key.contains("accept-encoding:gzip"));
        assert!(vary_key.contains("accept-language:en-US"));
    }

    #[test]
    fn test_cache_key_hash_consistency() {
        let key1 = CacheKey::new(CacheableMethod::Get, "example.com", "/api/users", None);

        let key2 = CacheKey::new(CacheableMethod::Get, "example.com", "/api/users", None);

        assert_eq!(key1.hash_value(), key2.hash_value());
    }

    #[test]
    fn test_cache_key_hash_difference() {
        let key1 = CacheKey::new(CacheableMethod::Get, "example.com", "/api/users", None);

        let key2 = CacheKey::new(CacheableMethod::Get, "example.com", "/api/products", None);

        assert_ne!(key1.hash_value(), key2.hash_value());
    }

    #[test]
    fn test_path_components() {
        let key = CacheKey::new(CacheableMethod::Get, "example.com", "/api/users", None);

        let (dir1, dir2, filename) = key.to_path_components();
        assert_eq!(dir1.len(), 2);
        assert_eq!(dir2.len(), 2);
        assert!(filename.ends_with(".cache"));
    }

    #[test]
    fn test_invalid_method() {
        let key = CacheKey::from_request(b"POST", "example.com", "/api/users", None, true, None);

        assert!(key.is_none());
    }
}
