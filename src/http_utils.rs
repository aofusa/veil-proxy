use crate::cache;
use httparse::Status;
use memchr::memchr3;

// ====================
// HTTP/1.1 RFC準拠ヘルパー関数
// ====================

/// HTTP/1.1 100 Continue レスポンス
pub(crate) const HTTP_100_CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

// ====================
// スタックフォーマッタ（F-41: ホットパスのヒープ確保排除）
// ====================

/// IP アドレスをスタックバッファへフォーマットする（`to_string()` のヒープ確保排除）。
///
/// IPv6 の最大表記（39 文字）+ IPv4-mapped 形式（45 文字）を収める 46 バイト固定。
/// `as_str()` で `&str` として下流（`&str` を取る全 API）へ渡す。
pub(crate) struct IpStr {
    buf: [u8; 46],
    len: u8,
}

impl IpStr {
    #[inline]
    pub(crate) fn new(ip: std::net::IpAddr) -> Self {
        use std::io::Write;
        let mut s = Self {
            buf: [0u8; 46],
            len: 0,
        };
        let mut cur = std::io::Cursor::new(&mut s.buf[..]);
        // 46 バイトは IpAddr の Display 最大長を常に満たすため write! は失敗しない。
        let _ = write!(cur, "{}", ip);
        s.len = cur.position() as u8;
        s
    }

    #[inline]
    pub(crate) fn as_str(&self) -> &str {
        // SAFETY: IpAddr の Display 出力は ASCII のみ。
        unsafe { std::str::from_utf8_unchecked(&self.buf[..self.len as usize]) }
    }
}

/// `host:port` をスタックバッファへフォーマットする（`format!` のヒープ確保排除）。
///
/// ホスト名最大 253 文字 + ':' + ポート 5 桁 = 259 バイトを収める 260 バイト固定。
/// ホストが上限を超える場合のみ（実運用では発生しない）切り詰めずヒープへフォールバックする。
// clippy::large_enum_variant 許容理由: Stack バリアントのインライン 260B こそが本型の目的
// （F-41: リクエストごとの host:port 文字列ヒープ確保をスタック整形で排除）。Box 化すると
// ホットパスにアロケーションが戻り本末転倒になる。
#[allow(clippy::large_enum_variant)]
pub(crate) enum HostPortStr {
    Stack { buf: [u8; 260], len: u16 },
    Heap(String),
}

impl HostPortStr {
    #[inline]
    pub(crate) fn new(host: &str, port: u16) -> Self {
        let mut port_buf = itoa::Buffer::new();
        let port_str = port_buf.format(port);
        let need = host.len() + 1 + port_str.len();
        if need <= 260 {
            let mut buf = [0u8; 260];
            buf[..host.len()].copy_from_slice(host.as_bytes());
            buf[host.len()] = b':';
            buf[host.len() + 1..need].copy_from_slice(port_str.as_bytes());
            HostPortStr::Stack {
                buf,
                len: need as u16,
            }
        } else {
            HostPortStr::Heap(format!("{host}:{port_str}"))
        }
    }

    #[inline]
    pub(crate) fn as_str(&self) -> &str {
        match self {
            // SAFETY: host は &str（UTF-8）、':' とポートは ASCII。UTF-8 境界で連結している。
            HostPortStr::Stack { buf, len } => unsafe {
                std::str::from_utf8_unchecked(&buf[..*len as usize])
            },
            HostPortStr::Heap(s) => s.as_str(),
        }
    }
}

/// Via ヘッダーを追加 (RFC 7230 Section 5.7.1)
///
/// プロキシ経由のリクエスト/レスポンスにViaヘッダーを追加します。
/// 既存のViaヘッダーがある場合は値を追加します。
///
/// # Arguments
/// * `headers` - ヘッダーのリスト (name, value) ペア
/// * `hostname` - プロキシのホスト名
///
/// # 形式
/// `Via: 1.1 <hostname>`
// 現在は単体テストのみで使用（実装済み RFC/ユーティリティヘルパー）
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn add_via_header(headers: &mut Vec<(Vec<u8>, Vec<u8>)>, hostname: &str) {
    let via_value = format!("1.1 {}", hostname).into_bytes();

    // 既存のViaヘッダーを検索
    if let Some(pos) = headers
        .iter()
        .position(|(n, _)| n.eq_ignore_ascii_case(b"via"))
    {
        // 既存のViaヘッダーに追加
        let existing = &headers[pos].1;
        let combined = format!("{}, 1.1 {}", String::from_utf8_lossy(existing), hostname);
        headers[pos].1 = combined.into_bytes();
    } else {
        // 新規Viaヘッダーを追加
        headers.push((b"via".to_vec(), via_value));
    }
}

/// HTTP/1.1 ヘッダー検証 (RFC 7230 Section 3.3.3)
///
/// Content-Length と Transfer-Encoding の競合をチェックします。
/// 両方が存在する場合はプロトコルエラーです。
///
/// # Returns
/// * `Ok(())` - ヘッダーが有効
/// * `Err(String)` - エラーメッセージ
// 現在は単体テストのみで使用（実装済み RFC/ユーティリティヘルパー）
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn validate_http_headers(
    headers: &[(impl AsRef<[u8]>, impl AsRef<[u8]>)],
) -> Result<(), String> {
    let mut has_content_length = false;
    let mut has_transfer_encoding = false;

    for (name, _value) in headers {
        let name = name.as_ref();
        if name.eq_ignore_ascii_case(b"content-length") {
            has_content_length = true;
        } else if name.eq_ignore_ascii_case(b"transfer-encoding") {
            has_transfer_encoding = true;
        }
    }

    // RFC 7230 Section 3.3.3:
    // Content-Length と Transfer-Encoding が両方存在する場合はエラー
    if has_content_length && has_transfer_encoding {
        return Err("Both Content-Length and Transfer-Encoding headers present".to_string());
    }

    Ok(())
}

/// Expect: 100-continue ヘッダーをチェック (RFC 7231 Section 5.1.1)
///
/// # Returns
/// * `true` - 100 Continue レスポンスを送信すべき
/// * `false` - 通常のリクエスト処理を継続
pub(crate) fn check_expect_continue(headers: &[(impl AsRef<[u8]>, impl AsRef<[u8]>)]) -> bool {
    for (name, value) in headers {
        let name = name.as_ref();
        let value = value.as_ref();
        if name.eq_ignore_ascii_case(b"expect") && value.eq_ignore_ascii_case(b"100-continue") {
            return true;
        }
    }
    false
}

/// ヘッダー数の上限をチェックし、必要に応じて拡張
///
/// HTTP/1.1ではヘッダー数に明確な制限はありませんが、
/// DoS対策として上限を設けつつ、動的に拡張可能にします。
///
/// # Arguments
/// * `current_count` - 現在のヘッダー数
/// * `max_headers` - 最大ヘッダー数
///
/// # Returns
/// * `Ok(new_max)` - 拡張後の最大ヘッダー数
/// * `Err(String)` - 上限超過エラー
// 現在は単体テストのみで使用（実装済み RFC/ユーティリティヘルパー）
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn check_header_count(
    current_count: usize,
    max_headers: usize,
) -> Result<usize, String> {
    const ABSOLUTE_MAX: usize = 1024;

    if current_count < max_headers {
        return Ok(max_headers);
    }

    // 上限に達した場合、倍に拡張（最大1024まで）
    let new_max = std::cmp::min(max_headers * 2, ABSOLUTE_MAX);
    if new_max > max_headers {
        Ok(new_max)
    } else {
        Err(format!(
            "Header count exceeds maximum limit of {}",
            ABSOLUTE_MAX
        ))
    }
}

// ====================
// RFC 7230-7233 準拠ヘルパー関数
// ====================

/// HTTP/1.1 Hostヘッダー必須チェック (RFC 7230 Section 5.4)
///
/// HTTP/1.1リクエストにはHostヘッダーが必須です。
/// 存在しない場合は400 Bad Requestを返すべきです。
///
/// # Arguments
/// * `headers` - ヘッダーのリスト
/// * `http_minor_version` - HTTPマイナーバージョン（1.0=0, 1.1=1）
///
/// # Returns
/// * `Ok(())` - Hostヘッダーが存在する、またはHTTP/1.0で任意
/// * `Err(&'static str)` - HTTP/1.1でHostヘッダーが存在しない
pub(crate) fn validate_host_header(
    headers: &[(impl AsRef<[u8]>, impl AsRef<[u8]>)],
    http_minor_version: u8,
) -> Result<(), &'static str> {
    // HTTP/1.0ではHostヘッダーは任意
    if http_minor_version < 1 {
        return Ok(());
    }

    let has_host = headers
        .iter()
        .any(|(name, _)| name.as_ref().eq_ignore_ascii_case(b"host"));

    if !has_host {
        return Err("Missing required Host header for HTTP/1.1");
    }

    Ok(())
}

/// Hop-by-hopヘッダーリスト (RFC 7230 Section 6.1)
///
/// これらのヘッダーはプロキシで転送してはならない。
pub(crate) const HOP_BY_HOP_HEADERS: &[&[u8]] = &[
    b"connection",
    b"keep-alive",
    b"proxy-authenticate",
    b"proxy-authorization",
    b"proxy-connection", // 非標準だが一般的
    b"te",
    b"trailer",
    b"transfer-encoding",
    b"upgrade",
];

/// 指定されたヘッダーがHop-by-hopヘッダーかチェック (RFC 7230 Section 6.1)
///
/// # Arguments
/// * `name` - ヘッダー名
///
/// # Returns
/// * `true` - Hop-by-hopヘッダー（転送不可）
/// * `false` - End-to-endヘッダー（転送可）
#[inline]
pub(crate) fn is_hop_by_hop_header(name: &[u8]) -> bool {
    HOP_BY_HOP_HEADERS
        .iter()
        .any(|h| name.eq_ignore_ascii_case(h))
}

/// Hop-by-hopヘッダーを削除 (RFC 7230 Section 6.1)
///
/// プロキシ転送前にHop-by-hopヘッダーを削除します。
/// Connectionヘッダーで指定された追加ヘッダーも削除します。
///
/// # Arguments
/// * `headers` - ヘッダーのリスト（変更される）
// 現在は単体テストのみで使用（実装済み RFC/ユーティリティヘルパー）
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn strip_hop_by_hop_headers(headers: &mut Vec<(Vec<u8>, Vec<u8>)>) {
    // Connectionヘッダーで指定された追加ヘッダーを収集
    // Connectionヘッダー値をトリムして収集（lowercase化は eq_ignore_ascii_case で不要）
    let connection_headers: Vec<Vec<u8>> = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case(b"connection"))
        .flat_map(|(_, value)| {
            value
                .split(|&b| b == b',')
                .map(|h| trim_ascii_whitespace(h).to_vec())
                .filter(|h| !h.is_empty())
                .collect::<Vec<_>>()
        })
        .collect();

    headers.retain(|(name, _)| {
        // 標準Hop-by-hopヘッダーをチェック（eq_ignore_ascii_caseで大文字小文字不問、アロケーションなし）
        if is_hop_by_hop_header(name) {
            return false;
        }
        // Connectionヘッダーで指定されたカスタムヘッダーもチェック（case-insensitive比較）
        if connection_headers
            .iter()
            .any(|h| name.eq_ignore_ascii_case(h))
        {
            return false;
        }
        true
    });
}

/// Range指定 (RFC 7233 Section 2.1)
#[derive(Debug, Clone, PartialEq)]
pub enum RangeSpec {
    /// bytes=start-end (両端含む)
    Bytes { start: u64, end: Option<u64> },
    /// bytes=-suffix (末尾からのバイト数)
    Suffix { suffix_length: u64 },
}

/// Rangeヘッダー解析結果
#[derive(Debug, Clone)]
pub struct ParsedRange {
    /// Range指定のリスト（複数レンジ対応だが、単一レンジのみ実装）
    pub ranges: Vec<RangeSpec>,
}

/// Rangeヘッダーをパース (RFC 7233 Section 2.1)
///
/// 形式: Range: bytes=start-end または bytes=-suffix または bytes=start-
///
/// # Arguments
/// * `range_header` - Rangeヘッダーの値
///
/// # Returns
/// * `Some(ParsedRange)` - 正常にパースできた場合
/// * `None` - 不正な形式の場合
pub(crate) fn parse_range_header(range_header: &[u8]) -> Option<ParsedRange> {
    // "bytes=" プレフィックスを確認
    if range_header.len() < 6 || !range_header[..6].eq_ignore_ascii_case(b"bytes=") {
        return None;
    }

    let range_str = std::str::from_utf8(&range_header[6..]).ok()?;
    let mut ranges = Vec::new();

    for range_part in range_str.split(',') {
        let range_part = range_part.trim();
        if range_part.is_empty() {
            continue;
        }

        if let Some(dash_pos) = range_part.find('-') {
            let start_str = range_part[..dash_pos].trim();
            let end_str = range_part[dash_pos + 1..].trim();

            if start_str.is_empty() {
                // bytes=-suffix 形式
                if let Ok(suffix) = end_str.parse::<u64>() {
                    if suffix > 0 {
                        ranges.push(RangeSpec::Suffix {
                            suffix_length: suffix,
                        });
                    }
                }
            } else if let Ok(start) = start_str.parse::<u64>() {
                // bytes=start- または bytes=start-end 形式
                let end = if end_str.is_empty() {
                    None
                } else {
                    end_str.parse::<u64>().ok()
                };

                // バリデーション: start <= end
                if let Some(e) = end {
                    if start > e {
                        return None; // 不正なレンジ
                    }
                }

                ranges.push(RangeSpec::Bytes { start, end });
            }
        }
    }

    if ranges.is_empty() {
        None
    } else {
        Some(ParsedRange { ranges })
    }
}

/// レンジが満足可能かチェック (RFC 7233 Section 4.4)
///
/// # Returns
/// * `Some((actual_start, actual_end))` - 満足可能なレンジ（0-indexed、両端含む）
/// * `None` - 416 Range Not Satisfiable を返すべき
pub(crate) fn normalize_range(spec: &RangeSpec, content_length: u64) -> Option<(u64, u64)> {
    if content_length == 0 {
        return None;
    }

    match spec {
        RangeSpec::Bytes { start, end } => {
            if *start >= content_length {
                return None; // 開始位置がコンテンツ長を超えている
            }
            let actual_end = end.map_or(content_length - 1, |e| e.min(content_length - 1));
            Some((*start, actual_end))
        }
        RangeSpec::Suffix { suffix_length } => {
            if *suffix_length == 0 {
                return None;
            }
            let start = content_length.saturating_sub(*suffix_length);
            Some((start, content_length - 1))
        }
    }
}

/// 206 Partial Content レスポンスヘッダーを構築 (RFC 7233 Section 4.1)
///
/// # Arguments
/// * `start` - 開始バイト位置
/// * `end` - 終了バイト位置（含む）
/// * `total_length` - コンテンツ全体の長さ
/// * `content_type` - Content-Type
/// * `close_connection` - Connection: close を追加するか
///
/// # Returns
/// 206レスポンスヘッダー（ボディなし）
// 現在は単体テストのみで使用（実装済み RFC/ユーティリティヘルパー）
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn build_partial_response_header(
    start: u64,
    end: u64,
    total_length: u64,
    content_type: &str,
    close_connection: bool,
) -> Vec<u8> {
    let content_length = end - start + 1;
    let mut response = Vec::with_capacity(256);

    response.extend_from_slice(b"HTTP/1.1 206 Partial Content\r\n");
    response.extend_from_slice(b"Accept-Ranges: bytes\r\n");

    // Content-Range: bytes start-end/total
    response.extend_from_slice(b"Content-Range: bytes ");
    response.extend_from_slice(start.to_string().as_bytes());
    response.extend_from_slice(b"-");
    response.extend_from_slice(end.to_string().as_bytes());
    response.extend_from_slice(b"/");
    response.extend_from_slice(total_length.to_string().as_bytes());
    response.extend_from_slice(b"\r\n");

    // Content-Length
    response.extend_from_slice(b"Content-Length: ");
    response.extend_from_slice(content_length.to_string().as_bytes());
    response.extend_from_slice(b"\r\n");

    // Content-Type
    response.extend_from_slice(b"Content-Type: ");
    response.extend_from_slice(content_type.as_bytes());
    response.extend_from_slice(b"\r\n");

    // Connection
    if close_connection {
        response.extend_from_slice(b"Connection: close\r\n");
    } else {
        response.extend_from_slice(b"Connection: keep-alive\r\n");
    }

    response.extend_from_slice(b"\r\n");
    response
}

/// 416 Range Not Satisfiable レスポンスを構築 (RFC 7233 Section 4.4)
pub(crate) fn build_range_not_satisfiable_response(content_length: u64) -> Vec<u8> {
    let mut response = Vec::with_capacity(128);
    response.extend_from_slice(b"HTTP/1.1 416 Range Not Satisfiable\r\n");
    response.extend_from_slice(b"Content-Range: bytes */");
    response.extend_from_slice(content_length.to_string().as_bytes());
    response.extend_from_slice(b"\r\n");
    response.extend_from_slice(b"Content-Length: 0\r\n");
    response.extend_from_slice(b"Connection: close\r\n\r\n");
    response
}

/// TE ヘッダー解析結果 (RFC 7230 Section 4.3)
#[derive(Debug, Clone, Default)]
pub struct TeHeader {
    /// trailers をサポートするか
    pub supports_trailers: bool,
    /// サポートする転送エンコーディング（chunked以外）
    pub encodings: Vec<String>,
}

/// TE ヘッダーをパース (RFC 7230 Section 4.3)
///
/// TE ヘッダーはHop-by-hopであり、クライアントがサポートする転送エンコーディングと
/// トレーラーのサポートを示します。
///
/// # Arguments
/// * `te_header` - TEヘッダーの値
///
/// # Returns
/// `TeHeader` 構造体
pub(crate) fn parse_te_header(te_header: &[u8]) -> TeHeader {
    let mut result = TeHeader::default();

    let te_str = match std::str::from_utf8(te_header) {
        Ok(s) => s,
        Err(_) => return result,
    };

    for part in te_str.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        // 品質値を除去 (e.g., "gzip;q=0.5" -> "gzip")
        let encoding = part.split(';').next().unwrap_or(part).trim();

        if encoding.eq_ignore_ascii_case("trailers") {
            result.supports_trailers = true;
        } else if !encoding.eq_ignore_ascii_case("chunked") {
            // chunkedはTE経由で指定すべきではないが、無害なのでスキップ
            result.encodings.push(encoding.to_string());
        }
    }

    result
}

/// リクエストからRangeヘッダーを取得
// 現在は単体テストのみで使用（実装済み RFC/ユーティリティヘルパー）
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn get_range_header(headers: &[(impl AsRef<[u8]>, impl AsRef<[u8]>)]) -> Option<&[u8]> {
    headers
        .iter()
        .find(|(name, _)| name.as_ref().eq_ignore_ascii_case(b"range"))
        .map(|(_, value)| value.as_ref())
}

/// Accept-Ranges: bytes ヘッダーを追加するかチェック
///
/// 静的ファイル配信時にクライアントにRangeリクエストサポートを通知
// 現在は単体テストのみで使用（実装済み RFC/ユーティリティヘルパー）
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn should_advertise_accept_ranges(method: &[u8]) -> bool {
    // GETとHEADでのみAccept-Rangesを通知
    method.eq_ignore_ascii_case(b"GET") || method.eq_ignore_ascii_case(b"HEAD")
}

/// Chunkedエンコードされたボディをデコードして生のデータを抽出
///
/// RFC 7230 Section 4.1に準拠した簡易的なChunkedデコーダ。
/// Transfer-Encoding: chunked 形式のボディから、生のデータを抽出します。
#[cfg(feature = "http2")]
pub(crate) fn decode_chunked_body(chunked_data: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(chunked_data.len());
    let mut pos = 0;

    while pos < chunked_data.len() {
        // チャンクサイズを読み取り（16進数）
        let size_start = pos;
        while pos < chunked_data.len() && chunked_data[pos] != b'\r' {
            pos += 1;
        }

        if pos >= chunked_data.len() {
            break;
        }

        // チャンクサイズを解析
        let size_str = match std::str::from_utf8(&chunked_data[size_start..pos]) {
            Ok(s) => s.trim(),
            Err(_) => break,
        };

        // チャンク拡張（;以降）を除去
        let size_str = size_str.split(';').next().unwrap_or(size_str);

        let chunk_size = match u64::from_str_radix(size_str, 16) {
            Ok(s) => s as usize,
            Err(_) => break,
        };

        // 終端チャンク（サイズ0）
        if chunk_size == 0 {
            break;
        }

        // \r\n をスキップ
        pos += 2;
        if pos >= chunked_data.len() {
            break;
        }

        // チャンクデータをコピー
        let end = std::cmp::min(pos + chunk_size, chunked_data.len());
        result.extend_from_slice(&chunked_data[pos..end]);
        pos = end;

        // チャンク終端の \r\n をスキップ
        if pos + 2 <= chunked_data.len() {
            pos += 2;
        }
    }

    result
}

// ====================
// HTTPヘッダー検証（Header Injection防止）
// ====================
//
// httparseがパースしたヘッダーを再検証し、不正な文字を含む
// ヘッダーを除外することで、HTTP Request Smuggling攻撃を防止します。
// 多層防御（Defense in Depth）の原則に基づく追加チェックです。
// ====================

/// ヘッダー名が有効か検証（RFC 7230 token準拠）
///
/// token = 1*tchar
/// tchar = "!" / "#" / "$" / "%" / "&" / "'" / "*" / "+" / "-" / "." /
///         "^" / "_" / "`" / "|" / "~" / DIGIT / ALPHA
#[inline]
pub(crate) fn is_valid_header_name(name: &[u8]) -> bool {
    if name.is_empty() {
        return false;
    }
    for &b in name {
        let is_tchar = matches!(b,
            b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' |
            b'^' | b'_' | b'`' | b'|' | b'~' |
            b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z'
        );
        if !is_tchar {
            return false;
        }
    }
    true
}

/// ヘッダー値が有効か検証（Header Injection防止）
///
/// RFC 7230 field-value に基づき、以下を禁止:
/// - CR (\r): ヘッダーインジェクションの主要ベクトル
/// - LF (\n): ヘッダーインジェクションの主要ベクトル
/// - NUL (0x00): セキュリティ上の理由
///
/// obs-fold（折り返しヘッダー）は許容しない方針とする。
/// これにより、プロキシとバックエンド間の解釈の違いを悪用した
/// HTTP Request Smuggling攻撃を防止する。
///
/// ## 実装詳細
///
/// `memchr`クレートのSIMD最適化された`memchr3`関数を使用して、
/// 3つの禁止文字（CR, LF, NUL）を並列に検索します。
///
/// - AVX2対応CPUでは32バイト単位で並列検査
/// - SSE2対応CPUでは16バイト単位で並列検査
/// - 小さな入力では自動的に最適なフォールバックを選択
///
/// これにより、大きなヘッダー値（Cookie、Authorization等）の
/// 検証パフォーマンスが向上します。
#[inline]
pub(crate) fn is_valid_header_value(value: &[u8]) -> bool {
    // memchr3: 3つの文字を一度に検索（SIMD最適化）
    // いずれかの禁止文字が見つかった場合はSome(位置)を返す
    // 見つからなければNone -> 有効なヘッダー値
    memchr3(b'\r', b'\n', 0, value).is_none()
}

// ====================
// Transfer-Encoding: chunked 検出（改善版）
// ====================

/// ASCIIの空白（スペース・タブ）をアロケーションなしでトリムする
#[inline]
fn trim_ascii_whitespace(s: &[u8]) -> &[u8] {
    let start = s
        .iter()
        .position(|&b| b != b' ' && b != b'\t')
        .unwrap_or(s.len());
    let s = &s[start..];
    let end = s
        .iter()
        .rposition(|&b| b != b' ' && b != b'\t')
        .map(|i| i + 1)
        .unwrap_or(0);
    &s[..end]
}

/// リクエスト本文フレーミングの分類結果（B-23: スマグリング防御）
///
/// `Ok(is_chunked)` … 受理可能。`is_chunked` が真なら chunked 転送。
/// `Err(reason)` … RFC 7230 §3.3.3 違反 or 曖昧フレーミング。呼び出し側は 400 で拒否する。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RequestFraming {
    /// Content-Length ベース（または本文なし）
    ContentLength,
    /// Transfer-Encoding: chunked ベース
    Chunked,
}

/// リクエストのフレーミングを RFC 7230 §3.3.3 準拠で分類し、スマグリング要因を拒否する。
///
/// HTTP リクエストスマグリング（CL.TE / TE.CL）は、フロントエンド（Veil）とバックエンドが
/// 本文長を別々に解釈することで発生する。それを不可能にするため、曖昧・不正なフレーミングは
/// **転送前に一律 400 で拒否**する。検査項目:
///
/// - **複数 Content-Length** ヘッダー → 拒否（RFC 7230 §3.3.2）。
/// - **Content-Length と Transfer-Encoding の同時指定** → 拒否（CL の値に関わらず。
///   `Content-Length: 0` + `Transfer-Encoding: chunked` も含む＝B-23 で塞いだ取りこぼし）。
/// - **Transfer-Encoding があるが最終エンコーディングが chunked でない** → 本文長を確定
///   できずスマグリングの温床になるため拒否（リクエストで許可される TE は chunked のみ）。
///
/// ヘッダーは `(name, value)` のイテレータで受け取り、1 パス・ゼロアロケーションで判定する。
pub(crate) fn classify_request_framing<'a>(
    headers: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
) -> Result<RequestFraming, &'static str> {
    let mut content_length_count = 0usize;
    let mut has_transfer_encoding = false;
    // 全 Transfer-Encoding ヘッダーをカンマ区切りで走査したときの「最終の非空トークン」が
    // chunked かどうか（複数 TE ヘッダーは連結相当として扱う）。
    let mut last_te_token_is_chunked = false;

    for (name, value) in headers {
        if name.eq_ignore_ascii_case(b"content-length") {
            content_length_count += 1;
        } else if name.eq_ignore_ascii_case(b"transfer-encoding") {
            has_transfer_encoding = true;
            for part in value.split(|&b| b == b',') {
                let token = trim_ascii_whitespace(part);
                if !token.is_empty() {
                    last_te_token_is_chunked = token.eq_ignore_ascii_case(b"chunked");
                }
            }
        }
    }

    if content_length_count > 1 {
        return Err("multiple Content-Length headers");
    }
    if content_length_count == 1 && has_transfer_encoding {
        return Err("Content-Length and Transfer-Encoding both present");
    }
    if has_transfer_encoding {
        if !last_te_token_is_chunked {
            return Err("Transfer-Encoding without terminal chunked");
        }
        return Ok(RequestFraming::Chunked);
    }
    Ok(RequestFraming::ContentLength)
}

/// Transfer-Encoding ヘッダー値から chunked かどうかを正確に判定
///
/// Vec割り当てなしでスライスベースのトリムを使用する。
#[inline]
pub(crate) fn is_chunked_encoding(value: &[u8]) -> bool {
    // カンマ区切りの各値をチェック
    for part in value.split(|&b| b == b',') {
        // アロケーションなしで空白をトリム
        let trimmed = trim_ascii_whitespace(part);
        if trimmed.eq_ignore_ascii_case(b"chunked") {
            return true;
        }
    }
    false
}

/// URLデコード（シンプルな実装）
///
/// %XX形式のエンコードされた文字をデコードします。
pub fn url_decode(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '%' {
            if let (Some(d1), Some(d2)) = (chars.next(), chars.next()) {
                if let (Some(n1), Some(n2)) = (d1.to_digit(16), d2.to_digit(16)) {
                    let byte = (n1 << 4 | n2) as u8;
                    result.push(byte as char);
                    continue;
                }
            }
            result.push(ch);
            if let Some(d1) = chars.next() {
                result.push(d1);
            }
            if let Some(d2) = chars.next() {
                result.push(d2);
            }
        } else if ch == '+' {
            result.push(' ');
        } else {
            result.push(ch);
        }
    }

    result
}

// ====================
// HTTPレスポンスパーサー（httparse使用）
// ====================

/// httparseを使用したレスポンス解析結果
pub(crate) struct ParsedResponse {
    /// ステータスコード
    pub(crate) status_code: u16,
    /// ヘッダー終端位置（ボディ開始位置）
    pub(crate) header_len: usize,
    /// Content-Length（存在する場合）
    pub(crate) content_length: Option<usize>,
    /// Transfer-Encoding: chunked かどうか
    pub(crate) is_chunked: bool,
    /// Connection: close かどうか（HTTP/1.1ではデフォルトはkeep-alive）
    pub(crate) is_connection_close: bool,
}

/// バックエンド応答バッファ先頭の 1xx 中間応答を読み捨てる（B-11）。
///
/// RFC 9110 §15.2: 1xx（情報）応答は最終応答に先行する中間応答で、ボディを持たない。
/// 本プロキシは `Expect: 100-continue` に対して自ら 100 を応答し、リクエストボディを
/// 無条件に転送するため、バックエンド由来の中間応答（100 Continue / 103 Early Hints 等）は
/// クライアントへ転送せず読み捨てる。101 Switching Protocols のみ最終応答として扱う
/// （WebSocket 等のアップグレード応答）。
///
/// 中間応答のヘッド（`\r\n\r\n` まで）を drain した後、呼び出し側は通常どおり
/// [`parse_http_response`] で最終応答を解析すればよい（中間応答のヘッドだけが先行到着した
/// 場合はバッファが空になり、呼び出し側の読み取りループが継続する）。
pub(crate) fn drain_interim_responses(accumulated: &mut Vec<u8>) {
    while let Some(parsed) = parse_http_response(accumulated) {
        if (100..=199).contains(&parsed.status_code) && parsed.status_code != 101 {
            accumulated.drain(..parsed.header_len);
        } else {
            break;
        }
    }
}

/// HTTPレスポンスをhttparseで解析
///
/// httparseを使用することで以下のメリットがある:
/// - RFC準拠の堅牢なパース
/// - \r\n と \n の両方に対応
/// - ヘッダー折り返し（obs-fold）の処理
/// - 不正なHTTPレスポンスの検出
pub(crate) fn parse_http_response(data: &[u8]) -> Option<ParsedResponse> {
    let mut headers_storage = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers_storage);

    match response.parse(data) {
        Ok(Status::Complete(header_len)) => {
            let status_code = response.code.unwrap_or(502);

            // Content-Length を取得
            let content_length = response
                .headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case("content-length"))
                .and_then(|h| std::str::from_utf8(h.value).ok())
                .and_then(|s| s.trim().parse().ok());

            // Transfer-Encoding: chunked をチェック
            let is_chunked = response
                .headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case("transfer-encoding"))
                .map(|h| is_chunked_encoding(h.value))
                .unwrap_or(false);

            // Connection: close をチェック（HTTP/1.1ではデフォルトはkeep-alive）
            let is_connection_close = response
                .headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case("connection"))
                .map(|h| {
                    // アロケーションなしでトリムして比較
                    trim_ascii_whitespace(h.value).eq_ignore_ascii_case(b"close")
                })
                .unwrap_or(false);

            Some(ParsedResponse {
                status_code,
                header_len,
                content_length,
                is_chunked,
                is_connection_close,
            })
        }
        Ok(Status::Partial) => None, // データ不足
        Err(_) => None,              // パースエラー
    }
}

// ====================
// Chunked Transfer Encoding パーサー（RFC 7230 Section 4.1 準拠）
// ====================
//
// Chunked-Bodyの構文:
//   chunked-body   = *chunk last-chunk trailer-part CRLF
//   chunk          = chunk-size [ chunk-ext ] CRLF chunk-data CRLF
//   chunk-size     = 1*HEXDIG
//   last-chunk     = 1*("0") [ chunk-ext ] CRLF
//   trailer-part   = *( header-field CRLF )
//
// トレーラーが存在する場合でも正確に終端を検出するために
// ステートマシンベースのパーサーを使用します。
// ====================

/// Chunkedデコーダの状態
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ChunkedState {
    /// チャンクサイズの16進数を読み取り中
    ReadingChunkSize,
    /// チャンク拡張（;以降）を読み取り中（サイズ行の終わりまでスキップ）
    ReadingChunkExtension,
    /// チャンクサイズ行の\r後、\nを期待
    ExpectingChunkSizeLF,
    /// チャンクデータを読み取り中（残りバイト数をchunk_remainingで追跡）
    ReadingChunkData,
    /// チャンクデータ後の\rを期待
    ExpectingChunkDataCR,
    /// チャンクデータ後の\nを期待
    ExpectingChunkDataLF,
    /// トレーラーヘッダーまたは終端の空行を読み取り中
    /// 空行（\r\n）で完了、それ以外はトレーラーヘッダー
    ReadingTrailerLine,
    /// トレーラー行または終端の\r後、\nを期待
    ExpectingTrailerLF,
    /// 転送完了
    Complete,
    /// サイズ制限超過（DoS対策）
    SizeLimitExceeded,
}

/// Chunkedデコーダのフィード結果
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ChunkedFeedResult {
    /// まだ転送中
    Continue,
    /// 転送完了
    Complete,
    /// サイズ制限超過
    SizeLimitExceeded,
}

/// [`ChunkedDecoder::next_data_span`] の戻り値。
///
/// 入力バッファ内に最初に現れた「連続したボディデータ範囲」と、フレーミングの進行状況を
/// スカラのみで表す（ヒープ確保なし・`Copy`）。ストリーミング転送（F-32）で、読み取り
/// バッファのサブスライスを中間 `Vec` なしに下流へそのまま送出するために使う。
// ストリーミング転送（F-32）は http2 / http3 経路でのみ使用する
#[cfg_attr(not(any(feature = "http2", feature = "http3")), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChunkedSpan {
    /// 入力スライス内のボディデータ開始オフセット。
    pub(crate) data_start: usize,
    /// ボディデータ長。0 ならこの呼び出しではデータが出現しなかった（フレーミングのみ消費）。
    pub(crate) data_len: usize,
    /// 入力スライスから消費したバイト数（フレーミングバイトを含む）。
    /// 呼び出し側は `input[consumed..]` で再度呼び出してループする。
    pub(crate) consumed: usize,
    /// この呼び出しで終端（0 サイズチャンク + トレーラー終端）に達したか。
    pub(crate) complete: bool,
    /// `max_body_size` 設定時、累積ボディサイズが上限を超えたか。
    pub(crate) limit_exceeded: bool,
}

/// Chunked転送デコーダ（ステートマシン）
///
/// RFC 7230 Section 4.1に準拠し、トレーラーの有無にかかわらず
/// 正確に終端を検出します。
///
/// DoS対策として累積サイズの制限機能を持ちます。
#[derive(Debug, Clone)]
pub(crate) struct ChunkedDecoder {
    /// 現在の状態
    pub(crate) state: ChunkedState,
    /// 現在のチャンクの残りバイト数
    pub(crate) chunk_remaining: u64,
    /// チャンクサイズの解析中に蓄積する16進数値
    pub(crate) size_accumulator: u64,
    /// サイズに少なくとも1文字は含まれているか
    pub(crate) size_has_digit: bool,
    /// トレーラー行が空かどうか（終端検出用）
    pub(crate) trailer_line_empty: bool,
    /// 累積ボディサイズ（DoS対策）
    pub(crate) total_body_size: u64,
    /// 最大許容ボディサイズ（0の場合は制限なし）
    pub(crate) max_body_size: u64,
}

impl ChunkedDecoder {
    /// 新しいChunkedDecoderを作成（サイズ制限付き）
    ///
    /// # Arguments
    /// * `max_body_size` - 最大許容ボディサイズ（0の場合は制限なし）
    pub(crate) fn new(max_body_size: u64) -> Self {
        Self {
            state: ChunkedState::ReadingChunkSize,
            chunk_remaining: 0,
            size_accumulator: 0,
            size_has_digit: false,
            trailer_line_empty: true,
            total_body_size: 0,
            max_body_size,
        }
    }

    /// 新しいChunkedDecoderを作成（制限なし - レスポンス用）
    pub(crate) fn new_unlimited() -> Self {
        Self::new(0)
    }

    /// 転送が完了したかどうかを確認
    #[inline]
    pub(crate) fn is_complete(&self) -> bool {
        self.state == ChunkedState::Complete
    }

    /// データをフィードして状態を更新
    /// 完了またはサイズ制限超過の場合は適切な結果を返す
    pub(crate) fn feed(&mut self, data: &[u8]) -> ChunkedFeedResult {
        for &byte in data {
            match self.feed_byte(byte) {
                ChunkedFeedResult::Continue => continue,
                result => return result,
            }
        }
        ChunkedFeedResult::Continue
    }

    /// 1バイトを処理して状態を更新
    /// 完了またはサイズ制限超過の場合は適切な結果を返す
    #[inline]
    pub(crate) fn feed_byte(&mut self, byte: u8) -> ChunkedFeedResult {
        match self.state {
            ChunkedState::ReadingChunkSize => {
                match byte {
                    b'0'..=b'9' => {
                        self.size_accumulator = self
                            .size_accumulator
                            .saturating_mul(16)
                            .saturating_add((byte - b'0') as u64);
                        self.size_has_digit = true;
                    }
                    b'a'..=b'f' => {
                        self.size_accumulator = self
                            .size_accumulator
                            .saturating_mul(16)
                            .saturating_add((byte - b'a' + 10) as u64);
                        self.size_has_digit = true;
                    }
                    b'A'..=b'F' => {
                        self.size_accumulator = self
                            .size_accumulator
                            .saturating_mul(16)
                            .saturating_add((byte - b'A' + 10) as u64);
                        self.size_has_digit = true;
                    }
                    b';' => {
                        // チャンク拡張の開始
                        self.state = ChunkedState::ReadingChunkExtension;
                    }
                    b'\r' => {
                        self.state = ChunkedState::ExpectingChunkSizeLF;
                    }
                    _ => {
                        // 不正な文字 - 回復のためスキップ（緩い解析）
                    }
                }
            }

            ChunkedState::ReadingChunkExtension => {
                // チャンク拡張はCRまでスキップ
                if byte == b'\r' {
                    self.state = ChunkedState::ExpectingChunkSizeLF;
                }
            }

            ChunkedState::ExpectingChunkSizeLF => {
                if byte == b'\n' {
                    if !self.size_has_digit {
                        // サイズが解析できなかった場合、リセット
                        self.state = ChunkedState::ReadingChunkSize;
                    } else if self.size_accumulator == 0 {
                        // 最後のチャンク（サイズ0）- トレーラーセクションへ
                        self.state = ChunkedState::ReadingTrailerLine;
                        self.trailer_line_empty = true;
                    } else {
                        // 通常のチャンク - データセクションへ
                        // サイズ制限チェック（DoS対策）
                        if self.max_body_size > 0 {
                            let new_total =
                                self.total_body_size.saturating_add(self.size_accumulator);
                            if new_total > self.max_body_size {
                                self.state = ChunkedState::SizeLimitExceeded;
                                return ChunkedFeedResult::SizeLimitExceeded;
                            }
                            self.total_body_size = new_total;
                        }
                        self.chunk_remaining = self.size_accumulator;
                        self.state = ChunkedState::ReadingChunkData;
                    }
                    // 次のチャンクのためにリセット
                    self.size_accumulator = 0;
                    self.size_has_digit = false;
                } else {
                    // LFが来なかった - リセット（緩い解析）
                    self.state = ChunkedState::ReadingChunkSize;
                    self.size_accumulator = 0;
                    self.size_has_digit = false;
                }
            }

            ChunkedState::ReadingChunkData => {
                self.chunk_remaining = self.chunk_remaining.saturating_sub(1);
                if self.chunk_remaining == 0 {
                    self.state = ChunkedState::ExpectingChunkDataCR;
                }
            }

            ChunkedState::ExpectingChunkDataCR => {
                if byte == b'\r' {
                    self.state = ChunkedState::ExpectingChunkDataLF;
                } else {
                    // 不正な形式 - 次のチャンクを探す（緩い解析）
                    self.state = ChunkedState::ReadingChunkSize;
                }
            }

            ChunkedState::ExpectingChunkDataLF => {
                if byte == b'\n' {
                    self.state = ChunkedState::ReadingChunkSize;
                } else {
                    // 不正な形式 - リセット
                    self.state = ChunkedState::ReadingChunkSize;
                }
            }

            ChunkedState::ReadingTrailerLine => {
                match byte {
                    b'\r' => {
                        self.state = ChunkedState::ExpectingTrailerLF;
                    }
                    _ => {
                        // トレーラーヘッダーの内容
                        self.trailer_line_empty = false;
                    }
                }
            }

            ChunkedState::ExpectingTrailerLF => {
                if byte == b'\n' {
                    if self.trailer_line_empty {
                        // 空行 = 転送完了
                        self.state = ChunkedState::Complete;
                        return ChunkedFeedResult::Complete;
                    } else {
                        // トレーラーヘッダー行が完了、次の行へ
                        self.state = ChunkedState::ReadingTrailerLine;
                        self.trailer_line_empty = true;
                    }
                } else {
                    // 不正な形式だが、トレーラー読み取りを継続
                    self.state = ChunkedState::ReadingTrailerLine;
                    self.trailer_line_empty = false;
                }
            }

            ChunkedState::Complete => {
                return ChunkedFeedResult::Complete;
            }

            ChunkedState::SizeLimitExceeded => {
                return ChunkedFeedResult::SizeLimitExceeded;
            }
        }
        ChunkedFeedResult::Continue
    }

    /// 入力バッファを処理し、最初に出現する「連続したボディデータ範囲」を 1 つだけ返す。
    ///
    /// ストリーミング転送（F-32）用のゼロコピー API。フレーミングバイト（チャンクサイズ・
    /// CRLF・トレーラー）は内部ステートマシンで消費し、`ReadingChunkData` 状態に入った
    /// 時点で、この入力スライス内に存在するデータの**連続 run** を 1 回の計算で求めて
    /// 返す（バイト単位ループや中間バッファを使わない）。返した [`ChunkedSpan`] の
    /// `data_start..data_start+data_len` は `input` のサブスライスなので、呼び出し側は
    /// それを下流へそのまま送出できる（コピーなし）。
    ///
    /// 呼び出し側は `consumed` バイトを処理済みとして `input[consumed..]` で再呼び出しし、
    /// `consumed == input.len()` になるまでループする。`complete`/`limit_exceeded` が立った
    /// 時点でループを終える。
    #[cfg_attr(not(any(feature = "http2", feature = "http3")), allow(dead_code))]
    pub(crate) fn next_data_span(&mut self, input: &[u8]) -> ChunkedSpan {
        // 既に終端・制限超過に達していれば、これ以上入力を消費しない。
        match self.state {
            ChunkedState::Complete => {
                return ChunkedSpan {
                    data_start: 0,
                    data_len: 0,
                    consumed: 0,
                    complete: true,
                    limit_exceeded: false,
                };
            }
            ChunkedState::SizeLimitExceeded => {
                return ChunkedSpan {
                    data_start: 0,
                    data_len: 0,
                    consumed: 0,
                    complete: false,
                    limit_exceeded: true,
                };
            }
            _ => {}
        }

        let mut i = 0;
        while i < input.len() {
            if self.state == ChunkedState::ReadingChunkData {
                // データ run を一括で確定（ゼロコピー: スライス範囲のみ算出）。
                // chunk_remaining は ExpectingChunkSizeLF で >=1 が保証され、avail も >=1。
                let avail = input.len() - i;
                let take = (self.chunk_remaining as usize).min(avail);
                let data_start = i;
                self.chunk_remaining -= take as u64;
                i += take;
                if self.chunk_remaining == 0 {
                    self.state = ChunkedState::ExpectingChunkDataCR;
                }
                return ChunkedSpan {
                    data_start,
                    data_len: take,
                    consumed: i,
                    complete: false,
                    limit_exceeded: false,
                };
            }
            match self.feed_byte(input[i]) {
                ChunkedFeedResult::Continue => i += 1,
                ChunkedFeedResult::Complete => {
                    i += 1;
                    return ChunkedSpan {
                        data_start: 0,
                        data_len: 0,
                        consumed: i,
                        complete: true,
                        limit_exceeded: false,
                    };
                }
                ChunkedFeedResult::SizeLimitExceeded => {
                    i += 1;
                    return ChunkedSpan {
                        data_start: 0,
                        data_len: 0,
                        consumed: i,
                        complete: false,
                        limit_exceeded: true,
                    };
                }
            }
        }
        // 入力を使い切った（この呼び出しではデータ run が出現せず）。
        ChunkedSpan {
            data_start: 0,
            data_len: 0,
            consumed: i,
            complete: false,
            limit_exceeded: false,
        }
    }
}

// ====================
// キャッシュ応答ヘルパー関数
// ====================

/// ETagが一致するかチェック
///
/// weak比較をサポート（W/"..."形式）
#[inline]
pub(crate) fn etag_matches(client_etag: &str, cached_etag: &str) -> bool {
    // "*" は全てにマッチ
    if client_etag.trim() == "*" {
        return true;
    }

    // 複数のETagをカンマ区切りで指定可能
    for etag in client_etag.split(',') {
        let etag = etag.trim();
        // weak比較（W/プレフィックスを無視）
        let etag_value = etag.strip_prefix("W/").unwrap_or(etag);
        let cached_value = cached_etag.strip_prefix("W/").unwrap_or(cached_etag);

        if etag_value == cached_value {
            return true;
        }
    }

    false
}

/// If-Modified-Since 検証
///
/// クライアントのIf-Modified-SinceとキャッシュのLast-Modifiedを比較
#[inline]
pub(crate) fn last_modified_matches(client_ims: &str, cached_lm: &str) -> bool {
    // RFC 7232: If-Modified-Since は Last-Modified と同じ場合に 304 を返す
    // 日付比較は複雑なので、文字列完全一致で簡易判定
    // より正確な日付比較が必要な場合は chrono クレートを使用
    client_ims.trim() == cached_lm.trim()
}

/// key_headersに基づいてリクエストヘッダーからVaryキー用の値を抽出
///
/// # Arguments
/// * `request_headers` - リクエストヘッダー
/// * `key_header_names` - キャッシュキーに含めるヘッダー名のリスト
///
/// # Returns
/// (ヘッダー名, ヘッダー値) のペアのリスト
pub(crate) fn extract_vary_headers_for_cache_key<'a>(
    request_headers: &'a [(Box<[u8]>, Box<[u8]>)],
    key_header_names: &'a [String],
) -> Vec<(&'a str, &'a str)> {
    let mut result = Vec::new();

    for key_header in key_header_names {
        for (name, value) in request_headers {
            if let Ok(name_str) = std::str::from_utf8(name) {
                if name_str.eq_ignore_ascii_case(key_header) {
                    if let Ok(value_str) = std::str::from_utf8(value) {
                        result.push((key_header.as_str(), value_str));
                        break; // 最初にマッチしたものを使用
                    }
                }
            }
        }
    }

    result
}

/// 304 Not Modified レスポンスを構築
pub(crate) fn build_304_response(
    cached_entry: &cache::CacheEntry,
    client_wants_close: bool,
    is_stale: bool,
) -> Vec<u8> {
    let mut response = Vec::with_capacity(256);

    response.extend_from_slice(b"HTTP/1.1 304 Not Modified\r\n");

    // 重要なヘッダーのみ含める
    for (name, value) in cached_entry.headers.iter() {
        // ETag, Last-Modified, Cache-Control, Vary, Content-Location のみ
        if name.eq_ignore_ascii_case(b"etag")
            || name.eq_ignore_ascii_case(b"last-modified")
            || name.eq_ignore_ascii_case(b"cache-control")
            || name.eq_ignore_ascii_case(b"vary")
            || name.eq_ignore_ascii_case(b"content-location")
        {
            response.extend_from_slice(name);
            response.extend_from_slice(b": ");
            response.extend_from_slice(value);
            response.extend_from_slice(b"\r\n");
        }
    }

    // X-Cache ヘッダー
    if is_stale {
        response.extend_from_slice(b"X-Cache: STALE\r\n");
    } else {
        response.extend_from_slice(b"X-Cache: HIT\r\n");
    }

    // Connection ヘッダー
    if client_wants_close {
        response.extend_from_slice(b"Connection: close\r\n");
    } else {
        response.extend_from_slice(b"Connection: keep-alive\r\n");
    }

    response.extend_from_slice(b"\r\n");
    response
}

/// キャッシュからのレスポンスを構築（メモリキャッシュ用）
/// キャッシュレスポンスの **ヘッダー部のみ** を構築する（ボディは含めない）。
///
/// メモリキャッシュのヒット時は、ボディ（`bytes::Bytes`）を本ヘッダーとは別に
/// ゼロコピーでソケットへ書き込むため、ボディを連結しないこのビルダーを使う。
pub(crate) fn build_cached_response_headers(
    cached_entry: &cache::CacheEntry,
    client_wants_close: bool,
    is_stale: bool,
) -> Vec<u8> {
    let mut response = Vec::with_capacity(512);

    // ステータスライン
    response.extend_from_slice(b"HTTP/1.1 ");
    let mut status_buf = itoa::Buffer::new();
    response.extend_from_slice(status_buf.format(cached_entry.status_code).as_bytes());
    response.extend_from_slice(b" OK\r\n");

    // ヘッダー
    for (name, value) in cached_entry.headers.iter() {
        response.extend_from_slice(name);
        response.extend_from_slice(b": ");
        response.extend_from_slice(value);
        response.extend_from_slice(b"\r\n");
    }

    // X-Cache ヘッダー
    if is_stale {
        response.extend_from_slice(b"X-Cache: STALE\r\n");
    } else {
        response.extend_from_slice(b"X-Cache: HIT\r\n");
    }

    // Connection ヘッダー
    if client_wants_close {
        response.extend_from_slice(b"Connection: close\r\n");
    } else {
        response.extend_from_slice(b"Connection: keep-alive\r\n");
    }

    response.extend_from_slice(b"\r\n");
    response
}

/// キャッシュレスポンス全体（ヘッダー + ボディ）を 1 本の `Vec` に構築する。
///
/// ディスクキャッシュのようにボディが所有 `Vec`（ディスクから読込済み）の経路で使う。
/// メモリキャッシュのゼロコピー配信は [`build_cached_response_headers`] + ボディ別書き込みを使う。
pub(crate) fn build_cached_response(
    cached_entry: &cache::CacheEntry,
    body_data: &[u8],
    client_wants_close: bool,
    is_stale: bool,
) -> Vec<u8> {
    let mut response = build_cached_response_headers(cached_entry, client_wants_close, is_stale);
    response.reserve(body_data.len());
    response.extend_from_slice(body_data);
    response
}

/// ステータスコードに対応する理由フレーズを返す
#[cfg(feature = "http2")]
pub(crate) fn status_reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

/// ヘッダーから特定のヘッダー値を抽出
pub(crate) fn extract_header_value<'a>(
    header_data: &'a [u8],
    header_name: &[u8],
) -> Option<&'a [u8]> {
    let mut headers_storage = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers_storage);

    if response.parse(header_data).is_ok() {
        for header in response.headers.iter() {
            if header.name.as_bytes().eq_ignore_ascii_case(header_name) {
                return Some(header.value);
            }
        }
    }
    None
}

/// HTTPステータスコードからリーズンフレーズを取得
pub(crate) fn status_code_to_reason(status_code: u16) -> &'static str {
    match status_code {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod chunked_span_tests {
    use super::*;

    /// `next_data_span` を入力バッファ列に対して駆動し、デコードされたボディを再構成する
    /// テストヘルパー。各バッファを `input[consumed..]` で繰り返し処理し、データ run を連結する。
    /// 戻り値: (再構成したボディ, 終端に達したか, 制限超過したか)。
    fn drive(decoder: &mut ChunkedDecoder, buffers: &[&[u8]]) -> (Vec<u8>, bool, bool) {
        let mut out = Vec::new();
        let mut complete = false;
        let mut limit = false;
        for buf in buffers {
            let mut pos = 0;
            while pos < buf.len() {
                let span = decoder.next_data_span(&buf[pos..]);
                if span.data_len > 0 {
                    let start = pos + span.data_start;
                    out.extend_from_slice(&buf[start..start + span.data_len]);
                }
                pos += span.consumed;
                if span.complete {
                    complete = true;
                    break;
                }
                if span.limit_exceeded {
                    limit = true;
                    break;
                }
                // 防御: 非空入力なら必ず 1 バイト以上消費する
                assert!(span.consumed > 0, "next_data_span made no progress");
            }
            if complete || limit {
                break;
            }
        }
        (out, complete, limit)
    }

    #[test]
    fn test_span_single_chunk() {
        let mut d = ChunkedDecoder::new_unlimited();
        let (body, complete, limit) = drive(&mut d, &[b"5\r\nhello\r\n0\r\n\r\n"]);
        assert_eq!(body, b"hello");
        assert!(complete);
        assert!(!limit);
        assert!(d.is_complete());
    }

    #[test]
    fn test_span_multiple_chunks_one_buffer() {
        let mut d = ChunkedDecoder::new_unlimited();
        let (body, complete, _) = drive(&mut d, &[b"3\r\nfoo\r\n3\r\nbar\r\n0\r\n\r\n"]);
        assert_eq!(body, b"foobar");
        assert!(complete);
    }

    #[test]
    fn test_span_data_split_across_buffers() {
        // チャンクデータが 2 つの read バッファにまたがる
        let mut d = ChunkedDecoder::new_unlimited();
        let (body, complete, _) = drive(&mut d, &[b"a\r\nhel", b"loworld\r\n0\r\n\r\n"]);
        assert_eq!(body, b"helloworld");
        assert!(complete);
    }

    #[test]
    fn test_span_framing_split_across_buffers() {
        // チャンクサイズ行が境界で割れる（"1" | "0\r\n..."）
        let mut d = ChunkedDecoder::new_unlimited();
        let (body, complete, _) = drive(&mut d, &[b"1", b"0\r\n0123456789abcdef\r\n0\r\n\r\n"]);
        assert_eq!(body, b"0123456789abcdef");
        assert!(complete);
    }

    #[test]
    fn test_span_byte_by_byte() {
        // 1 バイトずつ供給しても正しく再構成できる
        let input = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let mut d = ChunkedDecoder::new_unlimited();
        let mut out = Vec::new();
        let mut complete = false;
        for &b in input.iter() {
            let span = d.next_data_span(&[b]);
            if span.data_len > 0 {
                out.push(b);
            }
            if span.complete {
                complete = true;
            }
        }
        assert_eq!(out, b"Wikipedia");
        assert!(complete);
    }

    #[test]
    fn test_span_empty_body() {
        // 即終端（ボディなし）
        let mut d = ChunkedDecoder::new_unlimited();
        let (body, complete, _) = drive(&mut d, &[b"0\r\n\r\n"]);
        assert!(body.is_empty());
        assert!(complete);
    }

    #[test]
    fn test_span_with_trailers() {
        // トレーラーはボディに含めない
        let mut d = ChunkedDecoder::new_unlimited();
        let (body, complete, _) = drive(&mut d, &[b"5\r\nhello\r\n0\r\nX-Trailer: val\r\n\r\n"]);
        assert_eq!(body, b"hello");
        assert!(complete);
    }

    #[test]
    fn test_span_chunk_extension() {
        // チャンク拡張（;以降）は無視される
        let mut d = ChunkedDecoder::new_unlimited();
        let (body, complete, _) = drive(&mut d, &[b"5;ext=foo\r\nhello\r\n0\r\n\r\n"]);
        assert_eq!(body, b"hello");
        assert!(complete);
    }

    #[test]
    fn test_span_size_limit_exceeded() {
        // max_body_size を超えると limit_exceeded
        let mut d = ChunkedDecoder::new(4);
        let (_body, complete, limit) = drive(&mut d, &[b"5\r\nhello\r\n0\r\n\r\n"]);
        assert!(!complete);
        assert!(limit);
    }

    // decode_chunked_body は http2 feature でのみ提供されるため、比較テストも同 feature で gate する
    #[cfg(feature = "http2")]
    #[test]
    fn test_span_matches_decode_chunked_body() {
        // ゼロコピー span 経路が既存の decode_chunked_body と同一出力になることを保証。
        // さまざまな分割境界で同じ結果になることも確認する。
        let input: &[u8] = b"1a\r\nabcdefghijklmnopqrstuvwxyz\r\n5\r\n01234\r\n0\r\n\r\n";
        let expected = decode_chunked_body(input);

        for split in 0..=input.len() {
            let mut d = ChunkedDecoder::new_unlimited();
            let (first, second) = input.split_at(split);
            let (body, complete, _) = drive(&mut d, &[first, second]);
            assert_eq!(body, expected, "mismatch at split {}", split);
            assert!(complete, "not complete at split {}", split);
        }
    }

    // ====================
    // B-23: リクエストスマグリング分類（classify_request_framing）
    // ====================

    fn classify(headers: &[(&[u8], &[u8])]) -> Result<RequestFraming, &'static str> {
        classify_request_framing(headers.iter().map(|(n, v)| (*n, *v)))
    }

    #[test]
    fn framing_plain_content_length() {
        assert_eq!(
            classify(&[(b"host", b"x"), (b"content-length", b"5")]),
            Ok(RequestFraming::ContentLength)
        );
        // 本文なし（CL も TE も無し）も ContentLength 扱い。
        assert_eq!(
            classify(&[(b"host", b"x")]),
            Ok(RequestFraming::ContentLength)
        );
    }

    #[test]
    fn framing_plain_chunked() {
        assert_eq!(
            classify(&[(b"transfer-encoding", b"chunked")]),
            Ok(RequestFraming::Chunked)
        );
        // カンマ区切りで最終が chunked（`gzip, chunked`）は受理。
        assert_eq!(
            classify(&[(b"transfer-encoding", b"gzip, chunked")]),
            Ok(RequestFraming::Chunked)
        );
    }

    #[test]
    fn framing_rejects_cl_te_conflict_regardless_of_value() {
        // CL>0 + chunked（従来も拒否）。
        assert!(classify(&[
            (b"content-length", b"5"),
            (b"transfer-encoding", b"chunked")
        ])
        .is_err());
        // B-23 の核心: Content-Length: 0 + chunked（従来は取りこぼしていた CL.TE desync）。
        assert!(classify(&[
            (b"content-length", b"0"),
            (b"transfer-encoding", b"chunked")
        ])
        .is_err());
        // TE ヘッダーが chunked 以外でも CL と併存すれば拒否。
        assert!(classify(&[(b"content-length", b"5"), (b"transfer-encoding", b"gzip")]).is_err());
    }

    #[test]
    fn framing_rejects_multiple_content_length() {
        assert!(classify(&[(b"content-length", b"5"), (b"content-length", b"6")]).is_err());
    }

    #[test]
    fn framing_rejects_te_without_terminal_chunked() {
        // 最終エンコーディングが chunked でない（本文長を確定できない）→ 拒否。
        assert!(classify(&[(b"transfer-encoding", b"gzip")]).is_err());
        // chunked が最終でない（`chunked, gzip`）→ 拒否（TE.CL スマグリング対策）。
        assert!(classify(&[(b"transfer-encoding", b"chunked, gzip")]).is_err());
    }

    #[test]
    fn framing_multiple_te_headers_treated_as_concatenated() {
        // 複数 TE ヘッダーは連結相当。最終ヘッダーの最終トークンが chunked なら受理。
        assert_eq!(
            classify(&[
                (b"transfer-encoding", b"gzip"),
                (b"transfer-encoding", b"chunked")
            ]),
            Ok(RequestFraming::Chunked)
        );
        // 最終ヘッダーが chunked 以外（`chunked` の後に `identity`）→ 拒否。
        assert!(classify(&[
            (b"transfer-encoding", b"chunked"),
            (b"transfer-encoding", b"identity")
        ])
        .is_err());
    }
}

#[cfg(test)]
mod stack_fmt_tests {
    use super::*;

    #[test]
    fn ip_str_formats_v4_and_v6() {
        let v4: std::net::IpAddr = "192.168.1.100".parse().unwrap();
        assert_eq!(IpStr::new(v4).as_str(), "192.168.1.100");

        let v6: std::net::IpAddr = "2001:db8::1".parse().unwrap();
        assert_eq!(IpStr::new(v6).as_str(), "2001:db8::1");

        // 最長ケース（IPv4-mapped IPv6、45 文字）が収まる
        let long: std::net::IpAddr = "::ffff:255.255.255.255".parse().unwrap();
        let s = IpStr::new(long);
        assert_eq!(s.as_str(), long.to_string());
    }

    #[test]
    fn host_port_str_stack_path() {
        let hp = HostPortStr::new("backend.example.com", 8443);
        assert_eq!(hp.as_str(), "backend.example.com:8443");
        assert!(matches!(hp, HostPortStr::Stack { .. }));

        let hp = HostPortStr::new("127.0.0.1", 80);
        assert_eq!(hp.as_str(), "127.0.0.1:80");
    }

    #[test]
    fn host_port_str_heap_fallback_for_oversized_host() {
        let host = "a".repeat(300);
        let hp = HostPortStr::new(&host, 65535);
        assert_eq!(hp.as_str(), format!("{host}:65535"));
        assert!(matches!(hp, HostPortStr::Heap(_)));
    }

    // B-11: 1xx 中間応答の読み捨て
    #[test]
    fn drain_interim_skips_100_before_final_response() {
        let mut buf =
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec();
        drain_interim_responses(&mut buf);
        let parsed = parse_http_response(&buf).expect("final response should parse");
        assert_eq!(parsed.status_code, 200);
        assert_eq!(parsed.content_length, Some(2));
    }

    #[test]
    fn drain_interim_skips_multiple_interims() {
        let mut buf = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 103 Early Hints\r\nLink: </s.css>; rel=preload\r\n\r\nHTTP/1.1 204 No Content\r\n\r\n".to_vec();
        drain_interim_responses(&mut buf);
        let parsed = parse_http_response(&buf).expect("final response should parse");
        assert_eq!(parsed.status_code, 204);
    }

    #[test]
    fn drain_interim_handles_lone_interim_head() {
        // 中間応答のヘッドだけが先行到着 → バッファは空になり、最終応答は次の read を待つ。
        let mut buf = b"HTTP/1.1 100 Continue\r\n\r\n".to_vec();
        drain_interim_responses(&mut buf);
        assert!(buf.is_empty());
        assert!(parse_http_response(&buf).is_none());
    }

    #[test]
    fn drain_interim_preserves_101_switching_protocols() {
        // 101 はアップグレードの最終応答として扱い読み捨てない。
        let mut buf = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n".to_vec();
        drain_interim_responses(&mut buf);
        let parsed = parse_http_response(&buf).expect("101 should remain");
        assert_eq!(parsed.status_code, 101);
    }

    #[test]
    fn drain_interim_noop_for_final_response() {
        let mut buf = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec();
        let before = buf.clone();
        drain_interim_responses(&mut buf);
        assert_eq!(buf, before);
    }

    #[test]
    fn drain_interim_noop_for_partial_head() {
        // ヘッダ未完（\r\n\r\n 未到着）は何もしない。
        let mut buf = b"HTTP/1.1 100 Continue\r\n".to_vec();
        let before = buf.clone();
        drain_interim_responses(&mut buf);
        assert_eq!(buf, before);
    }
}
