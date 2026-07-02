// ====================
// Coarse Timer（粗いタイマー）
// ====================
//
// Nginxと同様の最適化。システムコール（clock_gettime）の呼び出しを削減するため、
// 時刻をキャッシュし、一定間隔でのみ更新する。
//
// - ログのタイムスタンプ表示用: キャッシュした OffsetDateTime を使用
// - 処理時間計測用: std::time::Instant を使用（モノトニック・高精度）
//
// スレッドローカルでキャッシュするため、マルチスレッド環境でもロックフリー。

#[allow(unused_imports)]
use ftlog::{error, info, warn, FtLogFormat, Level, LevelFilter, Record};
use serde::Deserialize;
use std::borrow::Cow;
use std::cell::Cell;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::io;
use std::time::Instant;
use time::OffsetDateTime;

use crate::metrics::record_request_metrics;

/// Coarse Timer の更新間隔（ミリ秒）
/// 100ms間隔で時刻を更新。ログのタイムスタンプには十分な精度。
pub(crate) const COARSE_TIMER_UPDATE_INTERVAL_MS: u128 = 100;

thread_local! {
    /// キャッシュされた時刻（ログ表示用）
    static CACHED_TIME: Cell<OffsetDateTime> = Cell::new(OffsetDateTime::now_utc());
    /// 最後に時刻を更新したInstant
    static LAST_UPDATE: Cell<Instant> = Cell::new(Instant::now());
}

/// Coarse Timer から現在時刻を取得（ログ表示用）
///
/// キャッシュされた時刻を返す。COARSE_TIMER_UPDATE_INTERVAL_MS 経過していれば更新。
/// システムコールの呼び出しを大幅に削減。
#[inline]
pub(crate) fn coarse_now() -> OffsetDateTime {
    CACHED_TIME.with(|cached| {
        LAST_UPDATE.with(|last| {
            let now_instant = Instant::now();
            let elapsed = now_instant.duration_since(last.get()).as_millis();

            if elapsed >= COARSE_TIMER_UPDATE_INTERVAL_MS {
                // 更新間隔を超えた場合のみシステムコールを発行
                let now_time = OffsetDateTime::now_utc();
                cached.set(now_time);
                last.set(now_instant);
                now_time
            } else {
                // キャッシュされた時刻を返す
                cached.get()
            }
        })
    })
}

// ====================
// ログ設定
// ====================

/// ログ出力形式
#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LogFormat {
    /// テキスト形式（デフォルト）
    #[default]
    Text,
    /// JSON形式
    Json,
}

/// ログ設定セクション
#[derive(Deserialize, Clone, Debug)]
pub struct LoggingConfigSection {
    /// ログレベル
    /// - "trace": 全てのログ（開発/デバッグ用）
    /// - "debug": デバッグ情報
    /// - "info": 一般情報（デフォルト）
    /// - "warn": 警告のみ
    /// - "error": エラーのみ
    #[serde(default = "default_log_level")]
    pub(crate) level: String,

    /// ログ出力形式
    /// - "text": テキスト形式（デフォルト）
    /// - "json": JSON形式
    #[serde(default)]
    pub(crate) format: LogFormat,

    /// ログチャネルサイズ
    ///
    /// ftlog内部のチャネルバッファサイズです。
    /// 高負荷時のバックプレッシャーを軽減するために大きな値を設定します。
    ///
    /// デフォルト: 100000
    /// 推奨範囲: 10000 - 1000000
    #[serde(default = "default_channel_size")]
    pub(crate) channel_size: usize,

    /// フラッシュ間隔（ミリ秒）
    ///
    /// ログバッファをファイルにフラッシュする間隔です。
    /// 小さい値: 即座にログが書き込まれるがI/O負荷増
    /// 大きい値: I/O効率が良いがログ遅延
    ///
    /// デフォルト: 1000 (1秒)
    /// 推奨範囲: 100 - 5000
    #[serde(default = "default_flush_interval")]
    pub(crate) flush_interval_ms: u64,

    /// 最大ログファイルサイズ（バイト）
    ///
    /// ログファイルの最大サイズ。超過すると新しいファイルに切り替え。
    /// 0の場合はローテーションなし。
    ///
    /// 注意: ftlogは現在日次ローテーションのみをサポート。
    /// サイズベースローテーションは将来的な拡張で対応予定。
    ///
    /// デフォルト: 104857600 (100MB)
    #[serde(default = "default_max_log_size")]
    #[allow(dead_code)]
    pub(crate) max_log_size: u64,

    /// ログファイルパス
    ///
    /// ログファイルの出力先パス。
    /// 指定しない場合は標準エラー出力に出力。
    #[serde(default)]
    pub(crate) file_path: Option<String>,
}

pub(crate) fn default_log_level() -> String {
    "info".to_string()
}

pub(crate) fn default_channel_size() -> usize {
    100000 // ftlogデフォルト(100)より大幅に増加し、高負荷時のドロップを防止
}

pub(crate) fn default_flush_interval() -> u64 {
    1000 // 1秒
}

pub(crate) fn default_max_log_size() -> u64 {
    104857600 // 100MB
}

impl Default for LoggingConfigSection {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: LogFormat::default(),
            channel_size: default_channel_size(),
            flush_interval_ms: default_flush_interval(),
            max_log_size: default_max_log_size(),
            file_path: None,
        }
    }
}

/// ログレベル文字列をLevelFilterに変換
pub(crate) fn parse_log_level(level: &str) -> LevelFilter {
    match level.to_lowercase().as_str() {
        "trace" => LevelFilter::Trace,
        "debug" => LevelFilter::Debug,
        "info" => LevelFilter::Info,
        "warn" | "warning" => LevelFilter::Warn,
        "error" => LevelFilter::Error,
        "off" => LevelFilter::Off,
        _ => LevelFilter::Info,
    }
}

// ====================
// JSON形式ログフォーマッタ
// ====================

/// JSON形式ログフォーマッタ
///
/// ログメッセージをJSON形式で出力するカスタムフォーマッタです。
/// 出力形式:
/// ```json
/// {"timestamp":"2024-01-01T00:00:00.000Z","level":"INFO","target":"veil","file":"main.rs","line":123,"message":"..."}
/// ```
struct JsonLogFormat;

/// JSON形式ログメッセージ
struct JsonLogMessage {
    level: Level,
    target: Cow<'static, str>,
    file: Cow<'static, str>,
    line: Option<u32>,
    /// static なフォーマット文字列（引数なし）の場合は Borrowed でアロケーションなし
    args: Cow<'static, str>,
}

impl Display for JsonLogMessage {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        // タイムスタンプを取得（RFC 3339形式）
        let now = time::OffsetDateTime::now_utc();
        let timestamp = now
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| now.to_string());

        // JSON形式でフォーマット
        // メッセージ内の特殊文字をエスケープ
        write!(
            f,
            r#"{{"timestamp":"{}","level":"{}","target":"{}","file":"{}","line":{},"message":"{}"}}"#,
            timestamp,
            self.level,
            escape_json(&self.target),
            escape_json(&self.file),
            self.line.unwrap_or(0),
            escape_json(&self.args)
        )
    }
}

impl FtLogFormat for JsonLogFormat {
    fn msg(&self, record: &Record) -> Box<dyn Send + Sync + Display> {
        Box::new(JsonLogMessage {
            level: record.level(),
            target: record.target().to_string().into(),
            file: record
                .file_static()
                .map(Cow::Borrowed)
                .or_else(|| record.file().map(|s| Cow::Owned(s.to_owned())))
                .unwrap_or(Cow::Borrowed("")),
            line: record.line(),
            // as_str() は引数なし static 文字列の場合にアロケーションなしで &'static str を返す。
            // 動的引数がある場合のみ format!() でヒープ確保する（FtLogFormatter と同じ最適化）。
            args: record
                .args()
                .as_str()
                .map(Cow::Borrowed)
                .unwrap_or_else(|| Cow::Owned(format!("{}", record.args()))),
        })
    }
}

/// JSON文字列内の特殊文字をエスケープ
fn escape_json(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => result.push_str(r#"\""#),
            '\\' => result.push_str(r"\\"),
            '\n' => result.push_str(r"\n"),
            '\r' => result.push_str(r"\r"),
            '\t' => result.push_str(r"\t"),
            c if c.is_control() => {
                // 制御文字はUnicodeエスケープ
                result.push_str(&format!(r"\u{:04x}", c as u32));
            }
            c => result.push(c),
        }
    }
    result
}

/// JSON形式ログ用カスタムWriter
///
/// ftlogが出力するログ行からプレフィックス（タイムスタンプと遅延時間）を削除し、
/// JSONのみを出力します。
///
/// ftlogの出力形式: `{timestamp} {delay}ms {json_message}\n`
/// 出力形式: `{json_message}\n`
struct JsonLogWriter<W: io::Write + Send> {
    inner: W,
}

impl<W: io::Write + Send> JsonLogWriter<W> {
    fn new(writer: W) -> Self {
        Self { inner: writer }
    }
}

impl<W: io::Write + Send> io::Write for JsonLogWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // ftlogの出力からJSON部分を抽出
        // 形式: "{timestamp} {delay}ms {json}\n"
        // JSONは '{' で始まるため、最初の '{' を見つける
        if let Some(json_start) = buf.iter().position(|&b| b == b'{') {
            // JSON部分のみを書き込み
            self.inner.write_all(&buf[json_start..])?;
            Ok(buf.len())
        } else {
            // JSONが見つからない場合はそのまま書き込み
            self.inner.write_all(buf)?;
            Ok(buf.len())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// JSON形式ログ用FileAppender
///
/// ファイルへのJSON形式ログ出力用のカスタムAppenderです。
/// ftlogのプレフィックスを削除してJSONのみをファイルに書き込みます。
struct JsonFileAppender {
    writer: JsonLogWriter<std::io::BufWriter<std::fs::File>>,
}

impl JsonFileAppender {
    fn new(path: &str) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let buf_writer = std::io::BufWriter::new(file);
        Ok(Self {
            writer: JsonLogWriter::new(buf_writer),
        })
    }
}

impl io::Write for JsonFileAppender {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// 標準エラー出力用JSON形式Writer
struct JsonStderrWriter {
    writer: JsonLogWriter<std::io::Stderr>,
}

impl JsonStderrWriter {
    fn new() -> Self {
        Self {
            writer: JsonLogWriter::new(std::io::stderr()),
        }
    }
}

impl io::Write for JsonStderrWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// ftlogを設定に基づいて初期化
///
/// ftlogは内部でバックグラウンドスレッドとチャネルを使用した非同期ログライブラリです。
/// 以下の最適化を行います：
/// - channel_size: 高負荷時のログドロップを防止するため大きなバッファを使用
/// - max_log_level: 不要なログを除外してオーバーヘッドを削減
///
/// ## grokの提案に対する補足
///
/// grokはtokio::sync::mpscを使った非同期化を提案しましたが、これは以下の理由で不適切です：
/// 1. ftlogは既に非同期（内部でチャネル＋バックグラウンドスレッドを使用）
/// 2. tokio::sync::mpscはmonoioランタイムと互換性がない
/// 3. 追加の非同期化層はオーバーヘッドを増やすだけ
///
/// ftlog公式ドキュメントより：
/// > ftlog mitigates this bottleneck by sending messages to a dedicated logger
/// > thread and computing as little as possible in the main/worker thread.
///
/// 代わりに、ftlogの設定を最適化することで同等以上の効果を得られます。
pub(crate) fn init_logging(config: &LoggingConfigSection) -> ftlog::LoggerGuard {
    let level = parse_log_level(&config.level);
    let use_json = config.format == LogFormat::Json;

    // ファイル出力が設定されている場合
    if let Some(ref file_path) = config.file_path {
        if use_json {
            // JSON形式: カスタムWriterを使用してftlogのプレフィックスを削除
            let json_appender =
                JsonFileAppender::new(file_path).expect("Failed to create JSON file appender");

            ftlog::builder()
                .max_log_level(level)
                .bounded(config.channel_size, false)
                .format(JsonLogFormat)
                .root(json_appender)
                .try_init()
                .expect("Failed to initialize ftlog with JSON file appender")
        } else {
            // テキスト形式: ftlogの標準FileAppenderを使用
            let file_appender = ftlog::appender::FileAppender::builder()
                .path(file_path)
                .rotate(ftlog::appender::Period::Day)
                .build();

            ftlog::builder()
                .max_log_level(level)
                .bounded(config.channel_size, false)
                .root(file_appender)
                .try_init()
                .expect("Failed to initialize ftlog with file appender")
        }
    } else {
        // 標準エラー出力
        if use_json {
            // JSON形式: カスタムWriterを使用してftlogのプレフィックスを削除
            let json_writer = JsonStderrWriter::new();

            ftlog::builder()
                .max_log_level(level)
                .bounded(config.channel_size, false)
                .format(JsonLogFormat)
                .root(json_writer)
                .try_init()
                .expect("Failed to initialize ftlog with JSON stderr writer")
        } else {
            // テキスト形式（デフォルト）
            ftlog::builder()
                .max_log_level(level)
                .bounded(config.channel_size, false)
                .try_init()
                .expect("Failed to initialize ftlog")
        }
    }
}

/// kTLSの状態をログ出力
pub(crate) fn log_ktls_status(ktls_config: &crate::KtlsConfig) {
    if ktls_config.enabled {
        // rustls + ktls2 使用時
        #[cfg(feature = "ktls")]
        {
            if crate::ktls_rustls::is_ktls_available() {
                info!(
                    "kTLS: Enabled via rustls + ktls2 (TX={}, RX={})",
                    ktls_config.enable_tx, ktls_config.enable_rx
                );
                info!("kTLS: Kernel TLS offload active - reduced CPU usage expected");
                if ktls_config.fallback_enabled {
                    info!("kTLS: Fallback to rustls enabled (graceful degradation)");
                } else {
                    info!("kTLS: Fallback disabled (kTLS required, connections will fail if unavailable)");
                }
            } else {
                warn!("kTLS: Requested but kernel support not available");
                warn!("kTLS: Ensure 'modprobe tls' has been run and kernel 5.15+ is used");
                if ktls_config.fallback_enabled {
                    warn!("kTLS: Falling back to userspace TLS via rustls");
                } else {
                    error!("kTLS: Fallback disabled but kTLS unavailable - connections will fail!");
                    error!("kTLS: Either enable fallback or run 'modprobe tls'");
                }
            }
        }
        // kTLS フィーチャー無効時
        #[cfg(not(feature = "ktls"))]
        {
            warn!("kTLS: Enabled in config but ktls feature is not enabled");
            warn!("kTLS: Rebuild with: cargo build --features ktls for kTLS support");
        }
    } else {
        info!("kTLS: Disabled (using userspace TLS via rustls)");
    }
}

/// アクセスログを記録 + Prometheusメトリクスを記録
///
/// - 処理時間: `start_instant` からの経過時間を高精度で計測（Instant使用）
/// - タイムスタンプ: Coarse Timer でキャッシュした時刻を使用（システムコール削減）
/// - メトリクス: リクエスト数、処理時間、サイズをPrometheus形式で記録
///
/// # access-log feature との連携
///
/// access-log が有効な場合: 構造化ログ（JSON/テキスト）をログスレッドへ送信。
///   テキスト形式の info!() は出力しない（二重出力防止）。
/// access-log が無効な場合: ftlog 経由のテキスト形式のみ出力。
// client_ip / upstream は構造化ログ（access-log feature）でのみ使用する
#[cfg_attr(not(feature = "access-log"), allow(unused_variables))]
pub(crate) fn log_access(
    method: &[u8],
    host: &[u8],
    path: &[u8],
    ua: &[u8],
    req_body_size: u64,
    status: u16,
    resp_body_size: u64,
    start_instant: Instant,
    client_ip: &str,
    upstream: &str,
) {
    // 処理時間は Instant で高精度計測
    let duration = start_instant.elapsed();
    let duration_ms = duration.as_millis();
    let duration_secs = duration.as_secs_f64();

    // タイムスタンプは Coarse Timer を使用（システムコール削減）
    // access-log feature が有効な場合はこの値を log_access_structured() にも渡す（二重 syscall 排除）
    let log_time = coarse_now();
    let path_str = std::str::from_utf8(path).unwrap_or("-");
    let ua_str = std::str::from_utf8(ua).unwrap_or("-");
    let method_str = std::str::from_utf8(method).unwrap_or("GET");
    let host_str = std::str::from_utf8(host).unwrap_or("-");

    // access-log が無効な場合のみ ftlog 経由のテキストアクセスログを出力
    // （access-log 有効時は構造化ログと二重出力しない）
    #[cfg(not(feature = "access-log"))]
    info!("Access: time={} duration={}ms method={} host={} path={} ua={} req_body_size={} status={} resp_body_size={}",
        log_time, duration_ms, method_str, host_str, path_str, ua_str, req_body_size, status, resp_body_size);

    // Prometheusメトリクスを記録
    record_request_metrics(
        method_str,
        host_str,
        status,
        req_body_size,
        resp_body_size,
        duration_secs,
    );

    // 構造化アクセスログ出力（F-21）
    // log_time と duration_ms を渡すことで syscall 二重発行と clone を排除
    #[cfg(feature = "access-log")]
    crate::access_log::log_access_structured(
        method_str,
        host_str,
        path_str,
        ua_str,
        req_body_size,
        status,
        resp_body_size,
        log_time,
        duration_ms,
        client_ip,
        upstream,
    );
}
