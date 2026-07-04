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
use std::fmt::{Display, Formatter, Result as FmtResult, Write as _};
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

    /// アプリ本体ログ（INFO / WARN / DEBUG / TRACE）の出力先ファイルパス
    ///
    /// 指定しない場合は **標準出力 (stdout)** に出力。
    #[serde(default)]
    pub(crate) app_file_path: Option<String>,

    /// エラーログ（ERROR）の出力先ファイルパス
    ///
    /// 指定しない場合は **標準エラー出力 (stderr)** に出力。
    #[serde(default)]
    pub(crate) error_file_path: Option<String>,
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

impl Default for LoggingConfigSection {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: LogFormat::default(),
            channel_size: default_channel_size(),
            flush_interval_ms: default_flush_interval(),
            app_file_path: None,
            error_file_path: None,
        }
    }
}

impl LoggingConfigSection {
    /// アプリ本体ログの出力先ファイルパス（未指定時は標準出力）
    pub(crate) fn resolved_app_path(&self) -> Option<&str> {
        self.app_file_path.as_deref()
    }

    /// エラーログの出力先ファイルパス（未指定時は標準エラー出力）
    pub(crate) fn resolved_error_path(&self) -> Option<&str> {
        self.error_file_path.as_deref()
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
// ログフォーマッタ・出力先ルーティング
// ====================
//
// ftlog はターゲット接頭辞でのみ appender を振り分け、**レベルによる振り分け機能を
// 持たない**。そこで app 本体ログ（INFO/WARN/DEBUG/TRACE）とエラーログ（ERROR）を
// 別々の出力先へ分離するために、フォーマッタが生成する msg 先頭に 1 バイトの
// ルーティング用センチネル（下記 SENTINEL_*）を埋め込み、root writer
// （`LogRoutingWriter`）がそれを読んで振り分け・除去する。
//
// app/error ログはリクエストごとではない低頻度ログのため、この 1 行あたりの
// 走査コストはホットパス規則に抵触しない（アクセスログのホットパスは
// access-log feature 有効時は `access_log.rs` 専用スレッドが担う）。

/// ルーティング用センチネルバイト: アプリ本体ログ（INFO/WARN/DEBUG/TRACE）。
/// 制御文字のため通常のログ内容・RFC3339 タイムスタンプには出現しない。
const SENTINEL_APP: u8 = 0x01;
/// ルーティング用センチネルバイト: エラーログ（ERROR）。
const SENTINEL_ERROR: u8 = 0x02;

/// ログ種別（識別用 `type` フィールド）を決定する
///
/// - `target == "access"` → `"access"`（access-log feature 無効時のフォールバック経路）
/// - `ERROR` → `"error"`
/// - それ以外 → `"app"`
#[inline]
fn log_kind(level: Level, is_access: bool) -> &'static str {
    if is_access {
        "access"
    } else if level == Level::Error {
        "error"
    } else {
        "app"
    }
}

/// レベルからルーティング用センチネルを決定する（ERROR のみ error ストリームへ）
#[inline]
fn routing_sentinel(level: Level) -> u8 {
    if level == Level::Error {
        SENTINEL_ERROR
    } else {
        SENTINEL_APP
    }
}

/// text / json 両対応の統合ログフォーマッタ
///
/// `type` 識別フィールドとルーティング用センチネルを付与する。
struct AppLogFormat {
    json: bool,
}

/// ログメッセージ（ログスレッドで文字列化される）
struct AppLogMessage {
    json: bool,
    level: Level,
    /// 識別用 `type` フィールド値（"app" / "error" / "access"）
    kind: &'static str,
    /// ルーティング用センチネル（msg 先頭に埋め込む）
    sentinel: u8,
    target: Cow<'static, str>,
    /// text 形式でのみ使用（スレッド名）
    thread: Option<String>,
    file: Cow<'static, str>,
    line: Option<u32>,
    /// static なフォーマット文字列（引数なし）の場合は Borrowed でアロケーションなし
    args: Cow<'static, str>,
}

impl Display for AppLogMessage {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        // 先頭にルーティング用センチネルを 1 バイト書き込む（writer 側で除去）
        f.write_char(self.sentinel as char)?;

        if self.json {
            // タイムスタンプを取得（RFC 3339形式）
            let now = time::OffsetDateTime::now_utc();
            let timestamp = now
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| now.to_string());
            write!(
                f,
                r#"{{"timestamp":"{}","level":"{}","type":"{}","target":"{}","file":"{}","line":{},"message":"{}"}}"#,
                timestamp,
                self.level,
                self.kind,
                escape_json(&self.target),
                escape_json(&self.file),
                self.line.unwrap_or(0),
                escape_json(&self.args)
            )
        } else {
            // 既存 ftlog テキスト形式（`{level} {thread} [{file}:{line}] {msg}`）に
            // `type=...` を付与
            write!(
                f,
                "{} type={} {} [{}:{}] {}",
                self.level,
                self.kind,
                self.thread.as_deref().unwrap_or(""),
                self.file,
                self.line.unwrap_or(0),
                self.args
            )
        }
    }
}

impl FtLogFormat for AppLogFormat {
    fn msg(&self, record: &Record) -> Box<dyn Send + Sync + Display> {
        let level = record.level();
        let is_access = record.target() == "access";
        Box::new(AppLogMessage {
            json: self.json,
            level,
            kind: log_kind(level, is_access),
            sentinel: routing_sentinel(level),
            target: record.target().to_string().into(),
            // スレッド名は text 形式のみ必要。json では省略してアロケーションを避ける。
            thread: if self.json {
                None
            } else {
                std::thread::current().name().map(|n| n.to_string())
            },
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

/// ログ出力先（stdout / stderr / ローテーション付きファイル）を構築する
///
/// - `Some(path)`: `ftlog::appender::FileAppender`（日次ローテーション・内部バッファ）
/// - `None`: `default_stderr` が true なら stderr、false なら stdout
fn build_log_sink(file_path: Option<&str>, default_stderr: bool) -> Box<dyn io::Write + Send> {
    match file_path {
        Some(path) => {
            let appender = ftlog::appender::FileAppender::builder()
                .path(path)
                .rotate(ftlog::appender::Period::Day)
                .build();
            Box::new(appender)
        }
        None => {
            if default_stderr {
                Box::new(io::stderr())
            } else {
                Box::new(io::stdout())
            }
        }
    }
}

/// レベル別ルーティング writer（ftlog の root として使用）
///
/// フォーマッタが埋め込んだセンチネル（`SENTINEL_APP` / `SENTINEL_ERROR`）を読み、
/// アプリ本体ログ / エラーログの出力先へ振り分ける。センチネルバイトは除去して出力する。
/// ftlog は 1 ログ行を 1 回の `write` で渡すため、行内の 1 バイト除去は前後 2 回の
/// `write_all` で実現する（追加アロケーションなし）。
struct LogRoutingWriter {
    app: Box<dyn io::Write + Send>,
    error: Box<dyn io::Write + Send>,
    json: bool,
}

impl io::Write for LogRoutingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // センチネルを探索してルーティング先を決定
        let sentinel_pos = buf
            .iter()
            .position(|&b| b == SENTINEL_APP || b == SENTINEL_ERROR);
        let to_error = matches!(sentinel_pos, Some(i) if buf[i] == SENTINEL_ERROR);
        let w: &mut dyn io::Write = if to_error {
            &mut self.error
        } else {
            &mut self.app
        };

        if self.json {
            // JSON 部分（最初の '{' 以降）のみ出力。センチネルは '{' より前にあるため同時に除去される。
            if let Some(j) = buf.iter().position(|&b| b == b'{') {
                w.write_all(&buf[j..])?;
            } else if let Some(i) = sentinel_pos {
                w.write_all(&buf[..i])?;
                w.write_all(&buf[i + 1..])?;
            } else {
                w.write_all(buf)?;
            }
        } else if let Some(i) = sentinel_pos {
            // テキスト形式: センチネルバイトのみ除去して前後を出力
            w.write_all(&buf[..i])?;
            w.write_all(&buf[i + 1..])?;
        } else {
            w.write_all(buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.app.flush()?;
        self.error.flush()
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

    // app 本体ログ（既定: stdout）/ エラーログ（既定: stderr）の出力先を構築。
    let app_sink = build_log_sink(config.resolved_app_path(), false);
    let error_sink = build_log_sink(config.resolved_error_path(), true);

    let root = LogRoutingWriter {
        app: app_sink,
        error: error_sink,
        json: use_json,
    };

    ftlog::builder()
        .max_log_level(level)
        .bounded(config.channel_size, false)
        .format(AppLogFormat { json: use_json })
        .root(root)
        .try_init()
        .expect("Failed to initialize ftlog")
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
    //
    // `target: "access"` を指定することでフォーマッタが `type=access` を付与する。
    // レベル INFO のため app ストリーム（既定 stdout = access ログの既定出力先）へ流れ、
    // `type` フィールドでアプリ本体ログと判別できる。
    #[cfg(not(feature = "access-log"))]
    info!(target: "access", "Access: time={} duration={}ms method={} host={} path={} ua={} req_body_size={} status={} resp_body_size={}",
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

// ====================
// テスト
// ====================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::sync::{Arc, Mutex};

    /// テスト用: 共有 Vec<u8> へ書き込む writer
    #[derive(Clone)]
    struct SharedSink(Arc<Mutex<Vec<u8>>>);
    impl io::Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn routing_writer(json: bool) -> (LogRoutingWriter, Arc<Mutex<Vec<u8>>>, Arc<Mutex<Vec<u8>>>) {
        let app = Arc::new(Mutex::new(Vec::new()));
        let err = Arc::new(Mutex::new(Vec::new()));
        let w = LogRoutingWriter {
            app: Box::new(SharedSink(app.clone())),
            error: Box::new(SharedSink(err.clone())),
            json,
        };
        (w, app, err)
    }

    fn s(v: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(v.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn test_log_kind() {
        assert_eq!(log_kind(Level::Info, false), "app");
        assert_eq!(log_kind(Level::Warn, false), "app");
        assert_eq!(log_kind(Level::Debug, false), "app");
        assert_eq!(log_kind(Level::Trace, false), "app");
        assert_eq!(log_kind(Level::Error, false), "error");
        // target == "access" は access（レベル問わず）
        assert_eq!(log_kind(Level::Info, true), "access");
        assert_eq!(log_kind(Level::Error, true), "access");
    }

    #[test]
    fn test_routing_sentinel() {
        assert_eq!(routing_sentinel(Level::Error), SENTINEL_ERROR);
        assert_eq!(routing_sentinel(Level::Info), SENTINEL_APP);
        assert_eq!(routing_sentinel(Level::Warn), SENTINEL_APP);
    }

    #[test]
    fn test_routing_text_app_vs_error() {
        let (mut w, app, err) = routing_writer(false);
        // app 行（センチネル APP）
        let app_line = format!("2024-01-01 0ms {}INFO type=app main [f:1] hello\n", SENTINEL_APP as char);
        w.write_all(app_line.as_bytes()).unwrap();
        // error 行（センチネル ERROR）
        let err_line = format!("2024-01-01 0ms {}ERROR type=error main [f:2] boom\n", SENTINEL_ERROR as char);
        w.write_all(err_line.as_bytes()).unwrap();

        let app_out = s(&app);
        let err_out = s(&err);
        // app にはアプリ行のみ、error にはエラー行のみ
        assert!(app_out.contains("hello"), "app: {}", app_out);
        assert!(!app_out.contains("boom"), "app must not have error: {}", app_out);
        assert!(err_out.contains("boom"), "err: {}", err_out);
        assert!(!err_out.contains("hello"), "err must not have app: {}", err_out);
        // センチネルバイトは除去されている
        assert!(!app_out.contains(SENTINEL_APP as char));
        assert!(!err_out.contains(SENTINEL_ERROR as char));
        // タイムスタンプ・type フィールドは保持
        assert!(app_out.contains("type=app"));
        assert!(app_out.contains("2024-01-01"));
    }

    #[test]
    fn test_routing_json_strips_prefix_and_sentinel() {
        let (mut w, app, _err) = routing_writer(true);
        let line = format!(
            "2024-01-01 0ms {}{{\"level\":\"INFO\",\"type\":\"app\",\"message\":\"hi\"}}\n",
            SENTINEL_APP as char
        );
        w.write_all(line.as_bytes()).unwrap();
        let out = s(&app);
        // ftlog プレフィックスとセンチネルが除去され JSON のみ
        assert!(out.starts_with('{'), "out: {}", out);
        assert!(out.contains("\"type\":\"app\""));
        assert!(!out.contains(SENTINEL_APP as char));
        assert!(!out.contains("0ms"));
    }
}
