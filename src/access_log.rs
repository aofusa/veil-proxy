// ====================
// 構造化アクセスログ（F-21）
// ====================
//
// JSON/テキスト形式の構造化アクセスログ出力モジュール。
// スレッドローカルバッファを使用してヒープアロケーションを最小化。

use std::cell::RefCell;
use std::time::Instant;

use crate::config::CURRENT_CONFIG;

/// アクセスログ設定
///
/// 例:
/// ```toml
/// [access_log]
/// enabled = true
/// format = "json"
/// file_path = "/var/log/veil/access.log"
/// fields = ["timestamp", "method", "host", "path", "status", "duration_ms"]
/// ```
#[derive(serde::Deserialize, Clone, Debug)]
pub struct AccessLogConfig {
    /// アクセスログを有効化するかどうか
    #[serde(default)]
    pub enabled: bool,
    /// 出力フォーマット: "json" または "text"
    #[serde(default = "default_access_log_format")]
    pub format: String,
    /// ログ出力先ファイルパス（省略時は stderr）
    #[serde(default)]
    pub file_path: Option<String>,
    /// 出力するフィールドのリスト（空の場合は全フィールドを出力）
    #[serde(default)]
    pub fields: Vec<String>,
}

fn default_access_log_format() -> String {
    "json".to_string()
}

impl Default for AccessLogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            format: default_access_log_format(),
            file_path: None,
            fields: Vec::new(),
        }
    }
}

// スレッドローカルバッファ（ヒープアロケーション削減）
thread_local! {
    static LOG_BUF: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(512));
}

/// JSON文字列エスケープをバッファに書き込む
pub(crate) fn write_json_str(buf: &mut Vec<u8>, s: &str) {
    buf.push(b'"');
    for b in s.bytes() {
        match b {
            b'"'  => { buf.push(b'\\'); buf.push(b'"'); }
            b'\\' => { buf.push(b'\\'); buf.push(b'\\'); }
            b'\n' => { buf.push(b'\\'); buf.push(b'n'); }
            b'\r' => { buf.push(b'\\'); buf.push(b'r'); }
            b'\t' => { buf.push(b'\\'); buf.push(b't'); }
            b     => { buf.push(b); }
        }
    }
    buf.push(b'"');
}

/// u64をバッファに書き込む（itoa使用）
fn write_u64(buf: &mut Vec<u8>, n: u64) {
    let mut tmp = itoa::Buffer::new();
    buf.extend_from_slice(tmp.format(n).as_bytes());
}

/// u16をバッファに書き込む（itoa使用）
fn write_u16(buf: &mut Vec<u8>, n: u16) {
    let mut tmp = itoa::Buffer::new();
    buf.extend_from_slice(tmp.format(n).as_bytes());
}

/// u128をバッファに書き込む（itoa使用）
fn write_u128(buf: &mut Vec<u8>, n: u128) {
    let mut tmp = itoa::Buffer::new();
    buf.extend_from_slice(tmp.format(n).as_bytes());
}

/// フィールドが出力対象かどうか判定（fieldsが空の場合は全フィールドを出力）
fn should_output_field(fields: &[String], field: &str) -> bool {
    fields.is_empty() || fields.iter().any(|f| f == field)
}

/// JSON形式でアクセスログをバッファに書き込む
#[allow(clippy::too_many_arguments)]
#[allow(unused_assignments)]
pub(crate) fn build_json_log(
    buf: &mut Vec<u8>,
    timestamp: &str,
    method: &str,
    host: &str,
    path: &str,
    status: u16,
    duration_ms: u128,
    client_ip: &str,
    upstream: &str,
    req_body_size: u64,
    resp_body_size: u64,
    user_agent: &str,
    fields: &[String],
) {
    buf.push(b'{');
    #[allow(unused_assignments)]
    let mut first = true;

    macro_rules! json_field {
        ($name:literal, $write:expr) => {
            if should_output_field(fields, $name) {
                if !first { buf.push(b','); }
                buf.push(b'"');
                buf.extend_from_slice($name.as_bytes());
                buf.extend_from_slice(b"\":");
                $write;
                first = false;
            }
        };
    }

    json_field!("timestamp",      { write_json_str(buf, timestamp); });
    json_field!("method",         { write_json_str(buf, method); });
    json_field!("host",           { write_json_str(buf, host); });
    json_field!("path",           { write_json_str(buf, path); });
    json_field!("status",         { write_u16(buf, status); });
    json_field!("duration_ms",    { write_u128(buf, duration_ms); });
    json_field!("client_ip",      { write_json_str(buf, client_ip); });
    json_field!("upstream",       { write_json_str(buf, upstream); });
    json_field!("req_body_size",  { write_u64(buf, req_body_size); });
    json_field!("resp_body_size", { write_u64(buf, resp_body_size); });
    json_field!("user_agent",     { write_json_str(buf, user_agent); });

    buf.push(b'}');
    buf.push(b'\n');
}

/// テキスト形式でアクセスログをバッファに書き込む
#[allow(clippy::too_many_arguments)]
#[allow(unused_assignments)]
pub(crate) fn build_text_log(
    buf: &mut Vec<u8>,
    timestamp: &str,
    method: &str,
    host: &str,
    path: &str,
    status: u16,
    duration_ms: u128,
    client_ip: &str,
    upstream: &str,
    req_body_size: u64,
    resp_body_size: u64,
    user_agent: &str,
    fields: &[String],
) {
    #[allow(unused_assignments)]
    let mut first = true;

    macro_rules! text_field {
        ($name:literal, $write:expr) => {
            if should_output_field(fields, $name) {
                if !first { buf.push(b' '); }
                buf.extend_from_slice($name.as_bytes());
                buf.push(b'=');
                $write;
                first = false;
            }
        };
    }

    text_field!("timestamp",      { buf.extend_from_slice(timestamp.as_bytes()); });
    text_field!("method",         { buf.extend_from_slice(method.as_bytes()); });
    text_field!("host",           { buf.extend_from_slice(host.as_bytes()); });
    text_field!("path",           { buf.extend_from_slice(path.as_bytes()); });
    text_field!("status",         { write_u16(buf, status); });
    text_field!("duration_ms",    { write_u128(buf, duration_ms); });
    text_field!("client_ip",      { buf.extend_from_slice(client_ip.as_bytes()); });
    text_field!("upstream",       { buf.extend_from_slice(upstream.as_bytes()); });
    text_field!("req_body_size",  { write_u64(buf, req_body_size); });
    text_field!("resp_body_size", { write_u64(buf, resp_body_size); });
    text_field!("user_agent",     { buf.extend_from_slice(user_agent.as_bytes()); });

    buf.push(b'\n');
}

/// 構造化アクセスログを出力する（F-21）
///
/// - 設定が無効の場合は即座にリターン
/// - `CURRENT_CONFIG` から設定を読み込み
/// - JSON または テキスト形式で stderr に出力
/// - フィールドフィルタリングをサポート
#[allow(clippy::too_many_arguments)]
pub(crate) fn log_access_structured(
    method: &str,
    host: &str,
    path: &str,
    ua: &str,
    req_body_size: u64,
    status: u16,
    resp_body_size: u64,
    start_instant: Instant,
    client_ip: &str,
    upstream: &str,
) {
    let config = CURRENT_CONFIG.load();
    let access_log = &config.access_log_config;

    // 無効の場合は即座にリターン
    if !access_log.enabled {
        return;
    }

    let duration_ms = start_instant.elapsed().as_millis();

    // タイムスタンプ（ISO 8601形式、簡易実装）
    let timestamp = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format_unix_timestamp(secs)
    };

    let fields = access_log.fields.clone();
    let format = access_log.format.clone();

    LOG_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        buf.clear();

        if format == "json" {
            build_json_log(
                &mut buf,
                &timestamp,
                method,
                host,
                path,
                status,
                duration_ms,
                client_ip,
                upstream,
                req_body_size,
                resp_body_size,
                ua,
                &fields,
            );
        } else {
            build_text_log(
                &mut buf,
                &timestamp,
                method,
                host,
                path,
                status,
                duration_ms,
                client_ip,
                upstream,
                req_body_size,
                resp_body_size,
                ua,
                &fields,
            );
        }

        // stderrに出力（ファイル出力はスタブ: monoioコンテキストでの複雑なI/Oは省略）
        let _ = {
            use std::io::Write;
            std::io::stderr().write_all(&buf)
        };
    });
}

/// Unix タイムスタンプを ISO 8601 形式にフォーマット（簡易実装）
pub(crate) fn format_unix_timestamp(secs: u64) -> String {
    let sec = secs % 60;
    let min = (secs / 60) % 60;
    let hour = (secs / 3600) % 24;
    let days = secs / 86400;

    let (year, month, day) = days_to_ymd(days);

    let mut buf = itoa::Buffer::new();
    let mut result = String::with_capacity(20);
    result.push_str(buf.format(year));
    result.push('-');
    let m = buf.format(month);
    if m.len() < 2 { result.push('0'); }
    result.push_str(m);
    result.push('-');
    let d = buf.format(day);
    if d.len() < 2 { result.push('0'); }
    result.push_str(d);
    result.push('T');
    let h = buf.format(hour);
    if h.len() < 2 { result.push('0'); }
    result.push_str(h);
    result.push(':');
    let mi = buf.format(min);
    if mi.len() < 2 { result.push('0'); }
    result.push_str(mi);
    result.push(':');
    let se = buf.format(sec);
    if se.len() < 2 { result.push('0'); }
    result.push_str(se);
    result.push('Z');
    result
}

/// Unix日数から年月日を計算（グレゴリオ暦）
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    let mut y = 1970u64;
    let mut d = days;
    loop {
        let dy = if is_leap(y) { 366 } else { 365 };
        if d < dy { break; }
        d -= dy;
        y += 1;
    }
    let months = if is_leap(y) {
        [31u64, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31u64, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 1u64;
    for &dm in &months {
        if d < dm { break; }
        d -= dm;
        m += 1;
    }
    (y, m, d + 1)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_access_log_format() {
        let mut buf = Vec::new();
        build_json_log(
            &mut buf,
            "2024-01-01T00:00:00Z",
            "GET",
            "example.com",
            "/test",
            200,
            42,
            "127.0.0.1",
            "10.0.0.1:8080",
            0,
            1234,
            "curl/7.0",
            &[],
        );
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("\"method\":\"GET\""), "method field missing: {}", s);
        assert!(s.contains("\"host\":\"example.com\""), "host field missing: {}", s);
        assert!(s.contains("\"path\":\"/test\""), "path field missing: {}", s);
        assert!(s.contains("\"status\":200"), "status field missing: {}", s);
        assert!(s.contains("\"duration_ms\":42"), "duration_ms field missing: {}", s);
        assert!(s.contains("\"client_ip\":\"127.0.0.1\""), "client_ip field missing: {}", s);
        assert!(s.contains("\"user_agent\":\"curl/7.0\""), "user_agent field missing: {}", s);
        assert!(s.ends_with('\n'), "should end with newline");
    }

    #[test]
    fn test_text_access_log_format() {
        let mut buf = Vec::new();
        build_text_log(
            &mut buf,
            "2024-01-01T00:00:00Z",
            "POST",
            "api.example.com",
            "/v1/resource",
            201,
            10,
            "192.168.1.1",
            "",
            512,
            256,
            "TestAgent/1.0",
            &[],
        );
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("method=POST"), "method field missing: {}", s);
        assert!(s.contains("status=201"), "status field missing: {}", s);
        assert!(s.contains("duration_ms=10"), "duration_ms field missing: {}", s);
        assert!(s.ends_with('\n'), "should end with newline");
    }

    #[test]
    fn test_access_log_field_filtering() {
        let fields: Vec<String> = vec!["method".to_string(), "status".to_string()];
        let mut buf = Vec::new();
        build_json_log(
            &mut buf,
            "2024-01-01T00:00:00Z",
            "DELETE",
            "example.com",
            "/item/1",
            204,
            5,
            "10.0.0.1",
            "",
            0,
            0,
            "-",
            &fields,
        );
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("\"method\":\"DELETE\""), "method should be present: {}", s);
        assert!(s.contains("\"status\":204"), "status should be present: {}", s);
        assert!(!s.contains("\"host\""), "host should be filtered out: {}", s);
        assert!(!s.contains("\"path\""), "path should be filtered out: {}", s);
        assert!(!s.contains("\"user_agent\""), "user_agent should be filtered out: {}", s);
    }

    #[test]
    fn test_access_log_config_default() {
        let config = AccessLogConfig::default();
        assert!(!config.enabled, "should be disabled by default");
        assert_eq!(config.format, "json", "default format should be json");
        assert!(config.file_path.is_none(), "file_path should be None by default");
        assert!(config.fields.is_empty(), "fields should be empty by default");
    }

    #[test]
    fn test_json_escape() {
        let mut buf = Vec::new();
        write_json_str(&mut buf, "hello \"world\"\nfoo\\bar");
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s, "\"hello \\\"world\\\"\\nfoo\\\\bar\"");
    }

    #[test]
    fn test_format_unix_timestamp() {
        // 1970-01-01T00:00:00Z
        assert_eq!(format_unix_timestamp(0), "1970-01-01T00:00:00Z");
        // 2024-01-01T00:00:00Z
        let ts = format_unix_timestamp(1704067200);
        assert!(ts.starts_with("2024-01-01"), "got: {}", ts);
    }

    #[test]
    fn test_log_access_structured_disabled() {
        // 設定が無効の場合は何も出力しない（パニックしないことを確認）
        let instant = Instant::now();
        // デフォルトではaccess_log_config.enabled=falseなので即座にリターンするはず
        log_access_structured(
            "GET", "example.com", "/", "-",
            0, 200, 0,
            instant,
            "127.0.0.1", "",
        );
        // パニックしなければOK
    }

    #[test]
    fn test_text_field_filtering() {
        let fields: Vec<String> = vec!["method".to_string(), "path".to_string()];
        let mut buf = Vec::new();
        build_text_log(
            &mut buf,
            "2024-01-01T00:00:00Z",
            "GET",
            "example.com",
            "/filtered",
            200,
            1,
            "1.2.3.4",
            "",
            0,
            0,
            "-",
            &fields,
        );
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("method=GET"), "method should be present");
        assert!(s.contains("path=/filtered"), "path should be present");
        assert!(!s.contains("host="), "host should be filtered");
        assert!(!s.contains("status="), "status should be filtered");
    }
}
