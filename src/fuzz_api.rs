//! cargo-fuzz 向けの薄い公開 API（ホットパス外）。

/// HTTP/1 ヘッダー名・値の境界検証（RFC 7230 token / injection 防止）。
#[inline]
pub fn validate_http_header_boundary(name: &[u8], value: &[u8]) -> bool {
    crate::http_utils::is_valid_header_name(name) && crate::http_utils::is_valid_header_value(value)
}