// ====================
// 構造化アクセスログ（F-21）
// ====================
//
// JSON/テキスト形式の構造化アクセスログ出力モジュール。
//
// # アーキテクチャ（パフォーマンス設計）
//
// ホットパス（ワーカースレッド）:
//   LOG_BUF（スレッドローカル Vec<u8>）でゼロアロケーション JSON/テキスト構築
//   → sync_channel::try_send() で専用ログスレッドへ送信（1 Vec 確保のみ）
//   → タイムスタンプは呼び出し元 coarse_now() の OffsetDateTime を受け取る（syscall 排除）
//   → fields/format は Arc<AccessLogConfig> を直接借用（clone 排除）
//
// ログスレッド（std::thread、起動時に 1 本生成、ホットリロード時に差し替え）:
//   BufWriter<File or Stderr> を独占し、recv_timeout() でドレイン
//   → flush_interval_ms 周期で定期フラッシュ
//   → 旧 SyncSender の drop により Disconnected を検出して自然終了
//
// # 設計の要点
//
// - std::io::stderr() のグローバル Mutex を完全排除
// - SyncSender::try_send() の競合区間は「キュー push のみ」（I/O なし）
// - ArcSwap による lock-free なチャンネル差し替え（ホットリロード対応）
// - tokio 非依存（std::thread + std::sync::mpsc）

use arc_swap::ArcSwap;
use std::cell::RefCell;
use std::io::{self, Write};
use std::sync::{
    mpsc::{self, RecvTimeoutError, SyncSender},
    Arc, OnceLock,
};
use std::time::Duration;
use time::OffsetDateTime;

use crate::config::CURRENT_CONFIG;

// ====================
// 設定型
// ====================

/// アクセスログ出力フォーマット
#[derive(serde::Deserialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AccessLogFormat {
    /// JSON形式（デフォルト）
    #[default]
    Json,
    /// テキスト形式（key=value スペース区切り）
    Text,
}

/// アクセスログ設定
///
/// 例:
/// ```toml
/// [access_log]
/// enabled = true
/// format = "json"
/// file_path = "/var/log/veil/access.log"
/// fields = ["timestamp", "method", "host", "path", "status", "duration_ms"]
/// channel_size = 10000
/// flush_interval_ms = 1000
/// ```
#[derive(serde::Deserialize, Clone, Debug)]
pub struct AccessLogConfig {
    /// アクセスログを有効化するかどうか
    #[serde(default)]
    pub enabled: bool,
    /// 出力フォーマット: "json"（デフォルト）または "text"
    #[serde(default)]
    pub format: AccessLogFormat,
    /// ログ出力先ファイルパス（省略時は標準出力 stdout）
    #[serde(default)]
    pub file_path: Option<String>,
    /// 出力するフィールドのリスト（空の場合は全フィールドを出力）
    #[serde(default)]
    pub fields: Vec<String>,
    /// ログスレッドへの送信チャネルキャパシティ
    ///
    /// 高負荷時のログドロップを防ぐため十分な値を設定する。
    /// チャネルフル時は新しいログ行をサイレントドロップする（リクエスト処理はブロックしない）。
    ///
    /// デフォルト: 10000
    #[serde(default = "default_access_log_channel_size")]
    pub channel_size: usize,
    /// BufWriter のフラッシュ間隔（ミリ秒）
    ///
    /// ログスレッドがこの間隔でバッファをフラッシュする。
    /// 小さい値: 即座に書き込まれるが CPU 使用量増。大きい値: I/O 効率向上。
    ///
    /// デフォルト: 1000 (1秒)
    #[serde(default = "default_access_log_flush_interval_ms")]
    pub flush_interval_ms: u64,
}

fn default_access_log_channel_size() -> usize {
    10_000
}
fn default_access_log_flush_interval_ms() -> u64 {
    1_000
}

impl Default for AccessLogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            format: AccessLogFormat::default(),
            file_path: None,
            fields: Vec::new(),
            channel_size: default_access_log_channel_size(),
            flush_interval_ms: default_access_log_flush_interval_ms(),
        }
    }
}

// ====================
// 専用ログスレッド管理
// ====================

/// アクセスログ送信チャンネル（ArcSwap によりホットリロード時に lock-free で差し替え可能）
static ACCESS_LOG_TX: OnceLock<ArcSwap<Option<SyncSender<Vec<u8>>>>> = OnceLock::new();

fn tx_store() -> &'static ArcSwap<Option<SyncSender<Vec<u8>>>> {
    ACCESS_LOG_TX.get_or_init(|| ArcSwap::from_pointee(None))
}

/// 起動時にアクセスログライタースレッドを初期化する
pub(crate) fn init_access_log_writer(config: &AccessLogConfig) {
    if !config.enabled {
        return;
    }
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(config.channel_size);
    spawn_log_thread(
        rx,
        config.file_path.clone(),
        Duration::from_millis(config.flush_interval_ms),
    );
    tx_store().store(Arc::new(Some(tx)));
}

/// ホットリロード時にアクセスログライタースレッドを差し替える
///
/// 旧スレッドは旧 SyncSender が drop されることで Disconnected を検出し自然終了する。
pub(crate) fn reload_access_log_writer(config: &AccessLogConfig) {
    if !config.enabled {
        // 無効化: 送信先を None に差し替え（旧スレッドは自然終了）
        tx_store().store(Arc::new(None));
        return;
    }
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(config.channel_size);
    spawn_log_thread(
        rx,
        config.file_path.clone(),
        Duration::from_millis(config.flush_interval_ms),
    );
    // アトミックに差し替え。旧 Arc の refcount がゼロになった時点で旧 SyncSender が drop される。
    tx_store().store(Arc::new(Some(tx)));
}

/// ログライタースレッドを起動する
fn spawn_log_thread(
    rx: mpsc::Receiver<Vec<u8>>,
    file_path: Option<String>,
    flush_interval: Duration,
) {
    std::thread::Builder::new()
        .name("access-log-writer".to_string())
        .spawn(move || {
            enum Writer {
                File(io::BufWriter<std::fs::File>),
                Stdout(io::BufWriter<io::Stdout>),
            }
            impl Write for Writer {
                fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                    match self {
                        Writer::File(w) => w.write(buf),
                        Writer::Stdout(w) => w.write(buf),
                    }
                }
                fn flush(&mut self) -> io::Result<()> {
                    match self {
                        Writer::File(w) => w.flush(),
                        Writer::Stdout(w) => w.flush(),
                    }
                }
            }

            // 出力先未指定時のデフォルトは標準出力 (stdout)
            let mut writer = match file_path {
                Some(ref path) => {
                    match std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                    {
                        Ok(f) => Writer::File(io::BufWriter::new(f)),
                        Err(e) => {
                            eprintln!("access-log: failed to open '{}': {}", path, e);
                            Writer::Stdout(io::BufWriter::new(io::stdout()))
                        }
                    }
                }
                None => Writer::Stdout(io::BufWriter::new(io::stdout())),
            };

            loop {
                match rx.recv_timeout(flush_interval) {
                    Ok(bytes) => {
                        let _ = writer.write_all(&bytes);
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        let _ = writer.flush();
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            let _ = writer.flush();
        })
        .expect("access-log: failed to spawn writer thread");
}

// ====================
// スレッドローカルバッファ
// ====================

// ヒープアロケーション削減のためのスレッドローカルバッファ
thread_local! {
    static LOG_BUF: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(512));
}

// ====================
// バッファ書き込みユーティリティ
// ====================

/// JSON文字列エスケープをバッファに書き込む
pub(crate) fn write_json_str(buf: &mut Vec<u8>, s: &str) {
    buf.push(b'"');
    for b in s.bytes() {
        match b {
            b'"' => {
                buf.push(b'\\');
                buf.push(b'"');
            }
            b'\\' => {
                buf.push(b'\\');
                buf.push(b'\\');
            }
            b'\n' => {
                buf.push(b'\\');
                buf.push(b'n');
            }
            b'\r' => {
                buf.push(b'\\');
                buf.push(b'r');
            }
            b'\t' => {
                buf.push(b'\\');
                buf.push(b't');
            }
            b => {
                buf.push(b);
            }
        }
    }
    buf.push(b'"');
}

/// u64 をバッファに書き込む（itoa 使用）
fn write_u64(buf: &mut Vec<u8>, n: u64) {
    let mut tmp = itoa::Buffer::new();
    buf.extend_from_slice(tmp.format(n).as_bytes());
}

/// u16 をバッファに書き込む（itoa 使用）
fn write_u16(buf: &mut Vec<u8>, n: u16) {
    let mut tmp = itoa::Buffer::new();
    buf.extend_from_slice(tmp.format(n).as_bytes());
}

/// u128 をバッファに書き込む（itoa 使用）
fn write_u128(buf: &mut Vec<u8>, n: u128) {
    let mut tmp = itoa::Buffer::new();
    buf.extend_from_slice(tmp.format(n).as_bytes());
}

/// フィールドが出力対象かどうか判定（fields が空の場合は全フィールドを出力）
fn should_output_field(fields: &[String], field: &str) -> bool {
    fields.is_empty() || fields.iter().any(|f| f == field)
}

/// RFC 3339 形式のタイムスタンプを buf に直接書き込む（String 確保なし）
///
/// `OffsetDateTime::format_into()` は `io::Write` に直接書き込むため
/// 中間ヒープ確保が発生しない。
fn write_rfc3339_into_buf(buf: &mut Vec<u8>, dt: OffsetDateTime) {
    let _ = dt.format_into(buf, &time::format_description::well_known::Rfc3339);
}

// ====================
// JSON / テキストビルダー
// ====================

/// JSON 形式でアクセスログをバッファに書き込む
///
/// `timestamp` は `OffsetDateTime` を直接受け取り buf に inline で書き込む
/// （文字列変換用の中間ヒープ確保なし）。
#[allow(clippy::too_many_arguments)]
#[allow(unused_assignments)]
pub(crate) fn build_json_log(
    buf: &mut Vec<u8>,
    timestamp: OffsetDateTime,
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
    // 識別用 type フィールドは常に先頭に出力する（fields フィルタの対象外）。
    // アプリ本体ログ / エラーログと区別するための固定値 "access"。
    buf.extend_from_slice(b"\"type\":\"access\"");
    let mut first = false;

    macro_rules! json_field {
        ($name:literal, $write:expr) => {
            if should_output_field(fields, $name) {
                if !first {
                    buf.push(b',');
                }
                buf.push(b'"');
                buf.extend_from_slice($name.as_bytes());
                buf.extend_from_slice(b"\":");
                $write;
                first = false;
            }
        };
    }

    json_field!("timestamp", {
        buf.push(b'"');
        write_rfc3339_into_buf(buf, timestamp);
        buf.push(b'"');
    });
    json_field!("method", {
        write_json_str(buf, method);
    });
    json_field!("host", {
        write_json_str(buf, host);
    });
    json_field!("path", {
        write_json_str(buf, path);
    });
    json_field!("status", {
        write_u16(buf, status);
    });
    json_field!("duration_ms", {
        write_u128(buf, duration_ms);
    });
    json_field!("client_ip", {
        write_json_str(buf, client_ip);
    });
    json_field!("upstream", {
        write_json_str(buf, upstream);
    });
    json_field!("req_body_size", {
        write_u64(buf, req_body_size);
    });
    json_field!("resp_body_size", {
        write_u64(buf, resp_body_size);
    });
    json_field!("user_agent", {
        write_json_str(buf, user_agent);
    });

    buf.push(b'}');
    buf.push(b'\n');
}

/// テキスト形式（key=value スペース区切り）でアクセスログをバッファに書き込む
#[allow(clippy::too_many_arguments)]
#[allow(unused_assignments)]
pub(crate) fn build_text_log(
    buf: &mut Vec<u8>,
    timestamp: OffsetDateTime,
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
    // 識別用 type フィールドは常に先頭に出力する（fields フィルタの対象外）。
    buf.extend_from_slice(b"type=access");
    let mut first = false;

    macro_rules! text_field {
        ($name:literal, $write:expr) => {
            if should_output_field(fields, $name) {
                if !first {
                    buf.push(b' ');
                }
                buf.extend_from_slice($name.as_bytes());
                buf.push(b'=');
                $write;
                first = false;
            }
        };
    }

    text_field!("timestamp", {
        write_rfc3339_into_buf(buf, timestamp);
    });
    text_field!("method", {
        buf.extend_from_slice(method.as_bytes());
    });
    text_field!("host", {
        buf.extend_from_slice(host.as_bytes());
    });
    text_field!("path", {
        buf.extend_from_slice(path.as_bytes());
    });
    text_field!("status", {
        write_u16(buf, status);
    });
    text_field!("duration_ms", {
        write_u128(buf, duration_ms);
    });
    text_field!("client_ip", {
        buf.extend_from_slice(client_ip.as_bytes());
    });
    text_field!("upstream", {
        buf.extend_from_slice(upstream.as_bytes());
    });
    text_field!("req_body_size", {
        write_u64(buf, req_body_size);
    });
    text_field!("resp_body_size", {
        write_u64(buf, resp_body_size);
    });
    text_field!("user_agent", {
        buf.extend_from_slice(user_agent.as_bytes());
    });

    buf.push(b'\n');
}

// ====================
// メインログ出力関数
// ====================

/// 構造化アクセスログを出力する（F-21）
///
/// # パフォーマンス設計
///
/// - `log_time`: 呼び出し元 (`log_access`) が計算済みの `coarse_now()` 値を受け取る
///   → `SystemTime::now()` syscall を排除
/// - `duration_ms`: 呼び出し元が計算済みの値を受け取る（二重計算排除）
/// - `fields` / `format`: `Arc<AccessLogConfig>` を直接借用（clone 排除）
/// - タイムスタンプ: `write_rfc3339_into_buf()` で LOG_BUF に直接書き込み（中間 String 排除）
/// - I/O: `SyncSender::try_send()` でログスレッドに渡す（グローバル stderr Mutex 排除）
#[allow(clippy::too_many_arguments)]
pub(crate) fn log_access_structured(
    method: &str,
    host: &str,
    path: &str,
    ua: &str,
    req_body_size: u64,
    status: u16,
    resp_body_size: u64,
    log_time: OffsetDateTime,
    duration_ms: u128,
    client_ip: &str,
    upstream: &str,
) {
    let config = CURRENT_CONFIG.load();
    let acfg = &config.access_log_config;

    if !acfg.enabled {
        return;
    }

    LOG_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        buf.clear();

        if acfg.format == AccessLogFormat::Json {
            build_json_log(
                &mut buf,
                log_time,
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
                &acfg.fields,
            );
        } else {
            build_text_log(
                &mut buf,
                log_time,
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
                &acfg.fields,
            );
        }

        // ログスレッドへ送信（1 Vec 確保のみ、グローバルロックなし）
        let guard = tx_store().load();
        if let Some(ref tx) = **guard {
            let _ = tx.try_send(buf.to_vec());
        }
    });
}

// ====================
// テスト
// ====================

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用に固定の OffsetDateTime を返す（2024-01-01T00:00:00Z）
    fn test_dt() -> OffsetDateTime {
        // Unix timestamp 1704067200 = 2024-01-01T00:00:00Z
        OffsetDateTime::from_unix_timestamp(1_704_067_200).expect("valid timestamp")
    }

    #[test]
    fn test_json_access_log_format() {
        let mut buf = Vec::new();
        build_json_log(
            &mut buf,
            test_dt(),
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
        assert!(
            s.contains("\"method\":\"GET\""),
            "method field missing: {}",
            s
        );
        assert!(
            s.contains("\"host\":\"example.com\""),
            "host field missing: {}",
            s
        );
        assert!(
            s.contains("\"path\":\"/test\""),
            "path field missing: {}",
            s
        );
        assert!(s.contains("\"status\":200"), "status field missing: {}", s);
        assert!(
            s.contains("\"duration_ms\":42"),
            "duration_ms field missing: {}",
            s
        );
        assert!(
            s.contains("\"client_ip\":\"127.0.0.1\""),
            "client_ip field missing: {}",
            s
        );
        assert!(
            s.contains("\"user_agent\":\"curl/7.0\""),
            "user_agent field missing: {}",
            s
        );
        assert!(
            s.contains("\"timestamp\":\"2024-01-01"),
            "timestamp field missing: {}",
            s
        );
        assert!(s.ends_with('\n'), "should end with newline");
    }

    #[test]
    fn test_access_log_type_field_present() {
        // JSON: 先頭に "type":"access" が出力される
        let mut buf = Vec::new();
        build_json_log(
            &mut buf, test_dt(), "GET", "example.com", "/", 200, 1, "127.0.0.1", "", 0, 0,
            "-", &[],
        );
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("\"type\":\"access\""), "json type missing: {}", s);
        assert!(s.starts_with("{\"type\":\"access\""), "type should be first: {}", s);

        // Text: 先頭に type=access が出力される
        let mut buf = Vec::new();
        build_text_log(
            &mut buf, test_dt(), "GET", "example.com", "/", 200, 1, "127.0.0.1", "", 0, 0,
            "-", &[],
        );
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("type=access"), "text type missing/first: {}", s);
    }

    #[test]
    fn test_access_log_type_field_survives_field_filter() {
        // fields で type を指定しなくても type は常に出力される
        let fields: Vec<String> = vec!["method".to_string()];
        let mut buf = Vec::new();
        build_json_log(
            &mut buf, test_dt(), "GET", "example.com", "/", 200, 1, "127.0.0.1", "", 0, 0,
            "-", &fields,
        );
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("\"type\":\"access\""), "type must survive filter: {}", s);
        assert!(s.contains("\"method\":\"GET\""));
        assert!(!s.contains("\"host\""));
    }

    #[test]
    fn test_text_access_log_format() {
        let mut buf = Vec::new();
        build_text_log(
            &mut buf,
            test_dt(),
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
        assert!(
            s.contains("duration_ms=10"),
            "duration_ms field missing: {}",
            s
        );
        assert!(s.ends_with('\n'), "should end with newline");
    }

    #[test]
    fn test_access_log_field_filtering() {
        let fields: Vec<String> = vec!["method".to_string(), "status".to_string()];
        let mut buf = Vec::new();
        build_json_log(
            &mut buf,
            test_dt(),
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
        assert!(
            s.contains("\"method\":\"DELETE\""),
            "method should be present: {}",
            s
        );
        assert!(
            s.contains("\"status\":204"),
            "status should be present: {}",
            s
        );
        assert!(
            !s.contains("\"host\""),
            "host should be filtered out: {}",
            s
        );
        assert!(
            !s.contains("\"path\""),
            "path should be filtered out: {}",
            s
        );
        assert!(
            !s.contains("\"user_agent\""),
            "user_agent should be filtered out: {}",
            s
        );
    }

    #[test]
    fn test_access_log_config_default() {
        let config = AccessLogConfig::default();
        assert!(!config.enabled, "should be disabled by default");
        assert_eq!(
            config.format,
            AccessLogFormat::Json,
            "default format should be Json"
        );
        assert!(
            config.file_path.is_none(),
            "file_path should be None by default"
        );
        assert!(
            config.fields.is_empty(),
            "fields should be empty by default"
        );
        assert_eq!(config.channel_size, 10_000);
        assert_eq!(config.flush_interval_ms, 1_000);
    }

    #[test]
    fn test_json_escape() {
        let mut buf = Vec::new();
        write_json_str(&mut buf, "hello \"world\"\nfoo\\bar");
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s, "\"hello \\\"world\\\"\\nfoo\\\\bar\"");
    }

    #[test]
    fn test_write_rfc3339_into_buf() {
        let mut buf = Vec::new();
        write_rfc3339_into_buf(&mut buf, test_dt());
        let s = String::from_utf8(buf).unwrap();
        // RFC3339 形式で始まることを確認
        assert!(s.starts_with("2024-01-01T00:00:00"), "got: {}", s);
    }

    #[test]
    fn test_log_access_structured_disabled() {
        // 設定が無効の場合は何も出力しない（パニックしないことを確認）
        let dt = OffsetDateTime::now_utc();
        // デフォルトでは access_log_config.enabled = false なので即座にリターン
        log_access_structured(
            "GET",
            "example.com",
            "/",
            "-",
            0,
            200,
            0,
            dt,
            0,
            "127.0.0.1",
            "",
        );
        // パニックしなければ OK
    }

    #[test]
    fn test_text_field_filtering() {
        let fields: Vec<String> = vec!["method".to_string(), "path".to_string()];
        let mut buf = Vec::new();
        build_text_log(
            &mut buf,
            test_dt(),
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

    #[test]
    fn test_access_log_format_serde() {
        // "json" → Json, "text" → Text
        let json_cfg: AccessLogConfig = toml::from_str(
            r#"
            enabled = true
            format = "json"
        "#,
        )
        .unwrap();
        assert_eq!(json_cfg.format, AccessLogFormat::Json);

        let text_cfg: AccessLogConfig = toml::from_str(
            r#"
            enabled = true
            format = "text"
        "#,
        )
        .unwrap();
        assert_eq!(text_cfg.format, AccessLogFormat::Text);
    }

    #[test]
    fn test_access_log_config_channel_defaults() {
        // channel_size / flush_interval_ms のデフォルト値確認
        let cfg: AccessLogConfig = toml::from_str("enabled = false").unwrap();
        assert_eq!(cfg.channel_size, 10_000);
        assert_eq!(cfg.flush_interval_ms, 1_000);
    }
}
