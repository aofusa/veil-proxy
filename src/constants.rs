// ====================
// 定数定義（パフォーマンスチューニング済み）
// ====================

// エラーレスポンス用静的バッファ
pub(crate) static ERR_MSG_BAD_REQUEST: &[u8] = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
pub(crate) static ERR_MSG_FORBIDDEN: &[u8] = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
pub(crate) static ERR_MSG_NOT_FOUND: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
pub(crate) static ERR_MSG_METHOD_NOT_ALLOWED: &[u8] = b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
pub(crate) static ERR_MSG_TOO_MANY_REQUESTS: &[u8] = b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
pub(crate) static ERR_MSG_BAD_GATEWAY: &[u8] = b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
pub(crate) static ERR_MSG_INSUFFICIENT_STORAGE: &[u8] = b"HTTP/1.1 507 Insufficient Storage\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
pub(crate) static ERR_MSG_REQUEST_TOO_LARGE: &[u8] = b"HTTP/1.1 413 Request Entity Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
/// RFC 7231: リクエスト URI が長すぎる場合
pub(crate) static ERR_MSG_URI_TOO_LONG: &[u8] = b"HTTP/1.1 414 URI Too Long\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
/// RFC 6585: リクエストヘッダーが大きすぎる場合
pub(crate) static ERR_MSG_REQUEST_HEADER_TOO_LARGE: &[u8] = b"HTTP/1.1 431 Request Header Fields Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
/// RFC 7231: クライアントがリクエストボディを時間内に送信しなかった場合
pub(crate) static ERR_MSG_REQUEST_TIMEOUT: &[u8] = b"HTTP/1.1 408 Request Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
pub(crate) static ERR_MSG_GATEWAY_TIMEOUT: &[u8] = b"HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";


// HTTP ヘッダー部品（事前計算）
pub(crate) static HTTP_200_PREFIX: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: ";
pub(crate) static CONTENT_LENGTH_HEADER: &[u8] = b"\r\nContent-Length: ";

// HTTPリクエスト構築用定数（ホットパス最適化）
pub(crate) static HEADER_HTTP11_HOST: &[u8] = b" HTTP/1.1\r\nHost: ";
pub(crate) static HEADER_COLON: &[u8] = b": ";
pub(crate) static HEADER_CRLF: &[u8] = b"\r\n";
pub(crate) static HEADER_SPACE: &[u8] = b" ";
pub(crate) static HEADER_PORT_COLON: &[u8] = b":";
pub(crate) static HEADER_CONNECTION_KEEPALIVE_END: &[u8] = b"Connection: keep-alive\r\n\r\n";

/// HTTP 301リダイレクトレスポンスのテンプレート
pub(crate) const HTTP_301_REDIRECT_TEMPLATE: &[u8] = b"HTTP/1.1 301 Moved Permanently\r\nLocation: ";
pub(crate) const HTTP_301_REDIRECT_SUFFIX: &[u8] = b"\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
