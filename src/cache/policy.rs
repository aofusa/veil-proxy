//! キャッシュポリシー
//!
//! Cache-Controlヘッダーの解析とキャッシュ可否判定を行います。

/// Varyヘッダー解析結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaryResult {
    /// Varyヘッダーが存在しない
    NotPresent,
    /// Vary: * でキャッシュ不可
    Uncacheable,
    /// Varyヘッダーで指定されたヘッダー名のリスト
    Headers(Vec<String>),
}

impl VaryResult {
    /// キャッシュ可能かどうか
    #[inline]
    pub fn is_cacheable(&self) -> bool {
        !matches!(self, VaryResult::Uncacheable)
    }

    /// ヘッダーリストを取得（NotPresentの場合は空のVec）
    #[inline]
    pub fn headers(&self) -> Option<&Vec<String>> {
        match self {
            VaryResult::Headers(h) => Some(h),
            _ => None,
        }
    }
}

/// Cache-Control ディレクティブ
#[derive(Debug, Clone, Default)]
pub struct CacheControl {
    /// max-age（秒）
    pub max_age: Option<u64>,
    /// s-maxage（秒、プロキシ用）
    pub s_maxage: Option<u64>,
    /// no-cache フラグ
    pub no_cache: bool,
    /// no-store フラグ
    pub no_store: bool,
    /// private フラグ
    pub private: bool,
    /// public フラグ
    pub public: bool,
    /// must-revalidate フラグ
    pub must_revalidate: bool,
    /// proxy-revalidate フラグ
    pub proxy_revalidate: bool,
    /// stale-while-revalidate（秒）
    pub stale_while_revalidate: Option<u64>,
    /// stale-if-error（秒）
    pub stale_if_error: Option<u64>,
    /// no-transform フラグ
    pub no_transform: bool,
    /// immutable フラグ
    pub immutable: bool,
}

impl CacheControl {
    /// Cache-Controlヘッダー値をパース
    pub fn parse(value: &[u8]) -> Self {
        let mut cc = Self::default();

        let value_str = match std::str::from_utf8(value) {
            Ok(s) => s,
            Err(_) => return cc,
        };

        for directive in value_str.split(',') {
            let directive = directive.trim().to_lowercase();

            if directive == "no-cache" {
                cc.no_cache = true;
            } else if directive == "no-store" {
                cc.no_store = true;
            } else if directive == "private" {
                cc.private = true;
            } else if directive == "public" {
                cc.public = true;
            } else if directive == "must-revalidate" {
                cc.must_revalidate = true;
            } else if directive == "proxy-revalidate" {
                cc.proxy_revalidate = true;
            } else if directive == "no-transform" {
                cc.no_transform = true;
            } else if directive == "immutable" {
                cc.immutable = true;
            } else if let Some(value) = directive.strip_prefix("max-age=") {
                cc.max_age = value.parse().ok();
            } else if let Some(value) = directive.strip_prefix("s-maxage=") {
                cc.s_maxage = value.parse().ok();
            } else if let Some(value) = directive.strip_prefix("stale-while-revalidate=") {
                cc.stale_while_revalidate = value.parse().ok();
            } else if let Some(value) = directive.strip_prefix("stale-if-error=") {
                cc.stale_if_error = value.parse().ok();
            }
        }

        cc
    }

    /// プロキシでキャッシュ可能かどうか
    pub fn is_cacheable(&self) -> bool {
        // no-store は絶対にキャッシュ不可
        if self.no_store {
            return false;
        }

        // private はプロキシでキャッシュ不可
        if self.private {
            return false;
        }

        true
    }

    /// プロキシ用のTTL（秒）を取得
    ///
    /// 優先順位: s-maxage > max-age
    pub fn effective_ttl(&self, default_ttl: u64) -> u64 {
        self.s_maxage.or(self.max_age).unwrap_or(default_ttl)
    }

    /// stale-while-revalidate の猶予時間内かチェック
    pub fn within_stale_while_revalidate(&self, stale_secs: u64) -> bool {
        match self.stale_while_revalidate {
            Some(swr) => stale_secs <= swr,
            None => false,
        }
    }

    /// stale-if-error の猶予時間内かチェック
    pub fn within_stale_if_error(&self, stale_secs: u64) -> bool {
        match self.stale_if_error {
            Some(sie) => stale_secs <= sie,
            None => false,
        }
    }
}

/// キャッシュポリシー
///
/// リクエストとレスポンスからキャッシュ可否を判定します。
pub struct CachePolicy;

impl CachePolicy {
    /// レスポンスがキャッシュ可能かどうかを判定
    ///
    /// # Arguments
    ///
    /// * `status_code` - HTTPステータスコード
    /// * `response_headers` - レスポンスヘッダー
    /// * `cacheable_statuses` - キャッシュ可能なステータスコードのリスト
    /// * `default_ttl` - デフォルトTTL
    ///
    /// # Returns
    ///
    /// キャッシュ可能な場合はTTL（秒）を返す
    pub fn check_response(
        status_code: u16,
        response_headers: &[(Box<[u8]>, Box<[u8]>)],
        cacheable_statuses: &[u16],
        default_ttl: u64,
    ) -> Option<u64> {
        // ステータスコードチェック
        if !cacheable_statuses.contains(&status_code) {
            return None;
        }

        // Cache-Controlヘッダーを取得
        let cache_control = response_headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(b"cache-control"))
            .map(|(_, value)| CacheControl::parse(value))
            .unwrap_or_default();

        // キャッシュ可能性チェック
        if !cache_control.is_cacheable() {
            return None;
        }

        // TTLを計算
        let ttl = cache_control.effective_ttl(default_ttl);

        // TTLが0の場合はキャッシュしない
        if ttl == 0 {
            return None;
        }

        Some(ttl)
    }

    /// Varyヘッダーを解析
    ///
    /// 戻り値:
    /// - `VaryResult::NotPresent` - Varyヘッダーなし
    /// - `VaryResult::Uncacheable` - Vary: * でキャッシュ不可
    /// - `VaryResult::Headers(vec)` - Varyヘッダーのリスト
    pub fn parse_vary_ex(response_headers: &[(Box<[u8]>, Box<[u8]>)]) -> VaryResult {
        let vary_header = response_headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(b"vary"))
            .map(|(_, value)| value);

        let vary_header = match vary_header {
            Some(h) => h,
            None => return VaryResult::NotPresent,
        };

        let vary_str = match std::str::from_utf8(vary_header) {
            Ok(s) => s,
            Err(_) => return VaryResult::NotPresent,
        };

        // Vary: * はキャッシュ不可
        if vary_str.trim() == "*" {
            return VaryResult::Uncacheable;
        }

        let headers: Vec<String> = vary_str
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        VaryResult::Headers(headers)
    }

    /// Varyヘッダーを解析（後方互換性のため）
    ///
    /// Vary: * の場合はキャッシュ不可を示すNoneを返す
    #[inline]
    pub fn parse_vary(response_headers: &[(Box<[u8]>, Box<[u8]>)]) -> Option<Vec<String>> {
        match Self::parse_vary_ex(response_headers) {
            VaryResult::Headers(h) => Some(h),
            VaryResult::NotPresent => Some(Vec::new()),
            VaryResult::Uncacheable => None,
        }
    }

    /// リクエストがキャッシュをバイパスすべきかチェック
    ///
    /// Pragma: no-cache や Cache-Control: no-cache をチェック
    pub fn request_bypasses_cache(request_headers: &[(Box<[u8]>, Box<[u8]>)]) -> bool {
        for (name, value) in request_headers {
            if name.eq_ignore_ascii_case(b"cache-control") {
                let cc = CacheControl::parse(value);
                if cc.no_cache || cc.no_store {
                    return true;
                }
            } else if name.eq_ignore_ascii_case(b"pragma") {
                if value.eq_ignore_ascii_case(b"no-cache") {
                    return true;
                }
            }
        }
        false
    }

    /// If-None-Match ヘッダーを取得
    pub fn get_if_none_match(request_headers: &[(Box<[u8]>, Box<[u8]>)]) -> Option<&[u8]> {
        request_headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(b"if-none-match"))
            .map(|(_, value)| value.as_ref())
    }

    /// If-Modified-Since ヘッダーを取得
    pub fn get_if_modified_since(request_headers: &[(Box<[u8]>, Box<[u8]>)]) -> Option<&[u8]> {
        request_headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(b"if-modified-since"))
            .map(|(_, value)| value.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cache_control_basic() {
        let cc = CacheControl::parse(b"max-age=3600, public");
        assert_eq!(cc.max_age, Some(3600));
        assert!(cc.public);
        assert!(!cc.private);
    }

    #[test]
    fn test_parse_cache_control_no_store() {
        let cc = CacheControl::parse(b"no-store");
        assert!(cc.no_store);
        assert!(!cc.is_cacheable());
    }

    #[test]
    fn test_parse_cache_control_private() {
        let cc = CacheControl::parse(b"private, max-age=300");
        assert!(cc.private);
        assert_eq!(cc.max_age, Some(300));
        assert!(!cc.is_cacheable()); // プロキシではキャッシュ不可
    }

    #[test]
    fn test_parse_cache_control_s_maxage() {
        let cc = CacheControl::parse(b"max-age=300, s-maxage=600");
        assert_eq!(cc.max_age, Some(300));
        assert_eq!(cc.s_maxage, Some(600));
        assert_eq!(cc.effective_ttl(100), 600); // s-maxageが優先
    }

    #[test]
    fn test_parse_cache_control_stale() {
        let cc =
            CacheControl::parse(b"max-age=300, stale-while-revalidate=60, stale-if-error=86400");
        assert_eq!(cc.stale_while_revalidate, Some(60));
        assert_eq!(cc.stale_if_error, Some(86400));
        assert!(cc.within_stale_while_revalidate(30));
        assert!(!cc.within_stale_while_revalidate(120));
    }

    #[test]
    fn test_check_response_cacheable() {
        let headers = vec![(
            b"cache-control".to_vec().into_boxed_slice(),
            b"max-age=3600".to_vec().into_boxed_slice(),
        )];

        let ttl = CachePolicy::check_response(200, &headers, &[200], 300);
        assert_eq!(ttl, Some(3600));
    }

    #[test]
    fn test_check_response_not_cacheable() {
        let headers = vec![(
            b"cache-control".to_vec().into_boxed_slice(),
            b"no-store".to_vec().into_boxed_slice(),
        )];

        let ttl = CachePolicy::check_response(200, &headers, &[200], 300);
        assert!(ttl.is_none());
    }

    #[test]
    fn test_parse_vary() {
        let headers = vec![(
            b"vary".to_vec().into_boxed_slice(),
            b"Accept-Encoding, User-Agent".to_vec().into_boxed_slice(),
        )];

        let vary = CachePolicy::parse_vary(&headers).unwrap();
        assert!(vary.contains(&"accept-encoding".to_string()));
        assert!(vary.contains(&"user-agent".to_string()));
    }

    #[test]
    fn test_parse_vary_star() {
        let headers = vec![(
            b"vary".to_vec().into_boxed_slice(),
            b"*".to_vec().into_boxed_slice(),
        )];

        let vary = CachePolicy::parse_vary(&headers);
        assert!(vary.is_none()); // Vary: * はキャッシュ不可
    }

    #[test]
    fn test_parse_vary_ex_not_present() {
        let headers: Vec<(Box<[u8]>, Box<[u8]>)> = vec![];

        let result = CachePolicy::parse_vary_ex(&headers);
        assert_eq!(result, VaryResult::NotPresent);
        assert!(result.is_cacheable());
    }

    #[test]
    fn test_parse_vary_ex_headers() {
        let headers = vec![(
            b"vary".to_vec().into_boxed_slice(),
            b"Accept-Encoding".to_vec().into_boxed_slice(),
        )];

        let result = CachePolicy::parse_vary_ex(&headers);
        assert!(result.is_cacheable());
        match result {
            VaryResult::Headers(h) => {
                assert!(h.contains(&"accept-encoding".to_string()));
            }
            _ => panic!("Expected VaryResult::Headers"),
        }
    }

    #[test]
    fn test_parse_vary_ex_uncacheable() {
        let headers = vec![(
            b"vary".to_vec().into_boxed_slice(),
            b"*".to_vec().into_boxed_slice(),
        )];

        let result = CachePolicy::parse_vary_ex(&headers);
        assert_eq!(result, VaryResult::Uncacheable);
        assert!(!result.is_cacheable());
    }

    #[test]
    fn test_parse_vary_no_header_returns_empty() {
        // Varyヘッダーがない場合、parse_varyはSome(空のVec)を返す
        let headers: Vec<(Box<[u8]>, Box<[u8]>)> = vec![];

        let vary = CachePolicy::parse_vary(&headers);
        assert_eq!(vary, Some(Vec::new()));
    }
}
