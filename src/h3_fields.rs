//! HTTP/3 のリクエストのフィールドセクションの検査（RFC 9114 §4.3.1。B-100）。
//!
//! `http3_server` の要求ごとの経路から呼ぶ（確保なし・1 回の走査）。quiche に依らずコンパイルし（`http2` だけでも）、
//! fuzz（`fuzz_api::h3_field_section_smoke`。F-279）から quiche 無しで呼べるようにしてある。

/// HTTP/3 のフィールドセクション（HEADERS フレーム）の検査結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum H3FieldSection {
    /// 疑似ヘッダを持ち、リクエストの必須要件を満たす（新規リクエスト）。
    Request,
    /// 疑似ヘッダを 1 つも持たない（既存ストリームなら trailers、新規ストリームなら malformed）。
    NoPseudo,
}

/// RFC 9114 §4.1.2 / §4.2 / §4.3.1 に従ってフィールドセクションを検査する。
///
/// `Err` は malformed（呼び出し側が H3_MESSAGE_ERROR で扱う）。検査するのは
/// 疑似ヘッダの重複・未知/禁止（`:status`、拡張 CONNECT を広告していないので `:protocol` も）・
/// 通常フィールドの後ろの疑似ヘッダ・大文字のフィールド名・接続固有フィールド・
/// リクエストの必須疑似ヘッダ（CONNECT とそれ以外で異なる）。
///
/// quiche の h3 層はこれらを検査しないため、無ければ `:method` 欠落を GET と見なす等で
/// malformed なリクエストに 200 を返していた（h3spec で検出）。
///
/// ホットパス（リクエストごと）: 確保なし・フィールド列を 1 回走査するだけ。
pub(crate) fn h3_check_field_section<'a, I>(fields: I) -> Result<H3FieldSection, &'static str>
where
    I: IntoIterator<Item = (&'a [u8], &'a [u8])>,
{
    const METHOD: u8 = 1;
    const SCHEME: u8 = 2;
    const AUTHORITY: u8 = 4;
    const PATH: u8 = 8;
    let mut seen: u8 = 0;
    let mut regular_seen = false;
    let mut is_connect = false;
    let mut host_seen = false;
    let mut scheme_needs_authority = false;
    for (name, value) in fields {
        // F-236（F2）: 値に CR・LF・NUL・前後の空白があれば malformed（RFC 9114 §4.2）
        if !crate::http_utils::is_valid_h2_field_value(value) {
            return Err("invalid field value");
        }
        if let Some(pseudo) = name.strip_prefix(b":") {
            if regular_seen {
                return Err("pseudo-header after regular field");
            }
            let bit = match pseudo {
                b"method" => METHOD,
                b"scheme" => SCHEME,
                b"authority" => AUTHORITY,
                b"path" => PATH,
                _ => return Err("unknown or prohibited pseudo-header"),
            };
            if seen & bit != 0 {
                return Err("duplicated pseudo-header");
            }
            seen |= bit;
            if bit == METHOD {
                if !crate::http_utils::is_valid_method(value) {
                    return Err("invalid :method");
                }
                is_connect = value == b"CONNECT";
            } else if bit == SCHEME {
                scheme_needs_authority = value == b"https" || value == b"http";
            } else if bit == PATH && value.is_empty() {
                return Err("empty :path");
            } else if bit == PATH
                && (!crate::http_utils::is_valid_request_target(value)
                    || (value[0] != b'/' && value != b"*"))
            {
                // origin 形式か `*` だけ（RFC 9114 §4.3.1。絶対形式はルートの照合を素通りする）
                return Err("invalid :path");
            }
        } else {
            regular_seen = true;
            if name.iter().any(u8::is_ascii_uppercase) {
                return Err("uppercase field name");
            }
            if !crate::http_utils::is_valid_header_name(name) {
                return Err("invalid field name");
            }
            match name {
                b"connection" | b"keep-alive" | b"proxy-connection" | b"transfer-encoding"
                | b"upgrade" => return Err("connection-specific field"),
                b"te" if value != b"trailers" => return Err("te other than trailers"),
                b"host" => host_seen = true,
                _ => {}
            }
        }
    }
    if seen == 0 {
        return Ok(H3FieldSection::NoPseudo);
    }
    if seen & METHOD == 0 {
        return Err("missing :method");
    }
    if is_connect {
        // §4.4: CONNECT は :scheme と :path を省き、:authority を必須とする。
        if seen & (SCHEME | PATH) != 0 {
            return Err("CONNECT with :scheme or :path");
        }
        if seen & AUTHORITY == 0 {
            return Err("CONNECT without :authority");
        }
    } else if seen & (SCHEME | PATH) != (SCHEME | PATH) {
        return Err("missing :scheme or :path");
    } else if scheme_needs_authority && seen & AUTHORITY == 0 && !host_seen {
        // §4.3.1: http/https は authority 成分が必須 → :authority か Host のどちらかが要る。
        return Err("missing :authority and host");
    }
    Ok(H3FieldSection::Request)
}
