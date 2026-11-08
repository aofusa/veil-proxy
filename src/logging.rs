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
    coarse_now_at(Instant::now())
}

/// 呼び出し側が読んだ時刻（`now_instant`）で更新を判断する [`coarse_now`]（時刻を 2 回読まない。F-302）
#[inline]
pub(crate) fn coarse_now_at(now_instant: Instant) -> OffsetDateTime {
    CACHED_TIME.with(|cached| {
        LAST_UPDATE.with(|last| {
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
// 要求ごとの失敗のログの間引き（B-251）
// ====================

/// 間引く間隔（秒）。
pub(crate) const LOG_THROTTLE_INTERVAL_SECS: u64 = 10;

/// 同じ箇所のログを間隔あたり 1 回に間引く。要求ごとに起こり得る失敗（上流の接続失敗等）を `warn!`・`error!` で
/// 出すときに使う（上流が落ちている間、要求レートと同じ速度でログが増えないように）。間引いた分は `debug!` で出す。
pub(crate) struct LogThrottle {
    /// 次に出してよい時刻（プロセスの基準時刻からのミリ秒）
    next_ms: std::sync::atomic::AtomicU64,
    /// 前回出してから間引いた件数
    suppressed: std::sync::atomic::AtomicU64,
}

impl LogThrottle {
    pub(crate) const fn new() -> Self {
        Self {
            next_ms: std::sync::atomic::AtomicU64::new(0),
            suppressed: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 出してよければ `Some(前回から間引いた件数)`。失敗の経路だけで呼ぶ（成功の経路に足さない）。
    pub(crate) fn check(&self) -> Option<u64> {
        use std::sync::atomic::Ordering;
        static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        let now = BASE.get_or_init(Instant::now).elapsed().as_millis() as u64;
        let next = self.next_ms.load(Ordering::Relaxed);
        if now >= next
            && self
                .next_ms
                .compare_exchange(
                    next,
                    now + LOG_THROTTLE_INTERVAL_SECS * 1000,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
        {
            return Some(self.suppressed.swap(0, Ordering::Relaxed));
        }
        self.suppressed.fetch_add(1, Ordering::Relaxed);
        None
    }
}

/// `log_throttled!(error, "...", args)`: 呼び出し箇所ごとに間隔あたり 1 回だけ指定のレベルで出し、残りは `debug!`。
macro_rules! log_throttled {
    ($lvl:ident, $($arg:tt)+) => {{
        static THROTTLE: $crate::logging::LogThrottle = $crate::logging::LogThrottle::new();
        match THROTTLE.check() {
            Some(0) => ftlog::$lvl!($($arg)+),
            Some(n) => {
                ftlog::$lvl!($($arg)+);
                ftlog::$lvl!(
                    "{} similar messages were suppressed in the last {} seconds",
                    n,
                    $crate::logging::LOG_THROTTLE_INTERVAL_SECS
                );
            }
            None => ftlog::debug!($($arg)+),
        }
    }};
}
pub(crate) use log_throttled;

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
    /// ftlog内部のチャネルバッファサイズです。満杯時は呼び出し側をブロックせず破棄します。
    ///
    /// F-183: ftlog の有界チャネル（crossbeam）は起動時に全スロットを初期化して常駐させる
    /// （1 件 約 88 バイト）。100000 では約 8.8MB がアイドル時も RSS に乗っていたため、
    /// 既定を 16384（約 1.4MB）にした。ロガースレッドは毎秒数十万件を排出できるので、
    /// 既定のログレベル（要求ごとのログを出さない）では足りる。
    ///
    /// デフォルト: 16384
    /// 推奨範囲: 4096 - 1000000
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
    // F-183: 100000 だと全スロット（約 8.8MB）が常駐する。16384 で約 1.4MB。
    16384
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

// ====================
// ログの出力先（B-343）
// ====================
//
// ftlog のログスレッドは、root の writer が `Err` を返すと標準エラー出力へ `eprintln!` で知らせる。標準エラー出力も
// 書けない（ディスクが満杯・読み手のいないパイプ）と、その `eprintln!` が panic してログスレッドが死ぬ。すると次に
// ログを出したスレッドも（キューが閉じたことを `eprintln!` で知らせようとして）panic し、巻き戻しの途中の
// `LoggerGuard` の drop（キューが閉じていると `expect` が panic）で 2 度目の panic になってプロセスが abort する。
// そこで出力先の失敗はこの writer の中で吸収する（ftlog へは常に成功を返し、書けなかった行は数えて、次に書けた
// ときに 1 行で知らせる）。ftlog の `FileAppender` も、日付が変わって次のファイルを開けないと `unwrap` で panic
// する（サンドボックスの中・権限・inode の枯渇）ので使わず、`DailyFile` に置き換える。

/// ログの出力先 1 つ（app または error）。書けなかった行は捨てて数える（B-343）。
struct LogSink {
    out: Box<dyn io::Write + Send>,
    /// 書けずに捨てた行の数（次に書けたときに知らせて 0 に戻す）
    dropped: u64,
    /// 知らせる行の `type`・レベル（app の出力先は WARN、error の出力先は ERROR）
    kind: &'static str,
    level: Level,
}

impl LogSink {
    fn new(out: Box<dyn io::Write + Send>, error: bool) -> Self {
        let (kind, level) = if error {
            ("error", Level::Error)
        } else {
            ("app", Level::Warn)
        };
        Self {
            out,
            dropped: 0,
            kind,
            level,
        }
    }

    /// 1 行を `head`・`tail` の順に書く（センチネルの前後）。書けなければ捨てて数え、失敗は返さない。
    fn write_line(&mut self, head: &[u8], tail: &[u8], json: bool, offset: time::UtcOffset) {
        if self.dropped > 0 {
            let notice = dropped_lines_notice(self.dropped, self.kind, self.level, json, offset);
            if self.out.write_all(notice.as_bytes()).is_err() {
                self.dropped += 1;
                return;
            }
            self.dropped = 0;
        }
        let ok = (head.is_empty() || self.out.write_all(head).is_ok())
            && (tail.is_empty() || self.out.write_all(tail).is_ok());
        if !ok {
            self.dropped += 1;
        }
    }

    fn flush(&mut self) {
        // 失敗は次の書き込みで数える（ftlog へ返すと上と同じ panic の連鎖になる）
        let _ = self.out.flush();
    }
}

/// 書けずに捨てた行があったことを知らせる 1 行（その出力先の形式で。ftlog の行と同じく時刻は起動時のオフセット）。
fn dropped_lines_notice(
    dropped: u64,
    kind: &str,
    level: Level,
    json: bool,
    offset: time::UtcOffset,
) -> String {
    let now = OffsetDateTime::now_utc().to_offset(offset);
    let message = format!(
        "{} log lines were dropped because the log output could not be written",
        dropped
    );
    if json {
        let timestamp = now
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| now.to_string());
        format!(
            r#"{{"timestamp":"{}","level":"{}","type":"{}","target":"{}","file":"{}","line":{},"message":"{}"}}"#,
            timestamp,
            level,
            kind,
            module_path!(),
            file!(),
            line!(),
            message
        ) + "\n"
    } else {
        // ftlog の既定の書式（`<日時>.<ミリ秒>+<時> <遅延>ms <本文>`）に合わせる
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}{:+03} 0ms {} type={} logger [{}:{}] {}\n",
            now.year(),
            now.month() as u8,
            now.day(),
            now.hour(),
            now.minute(),
            now.second(),
            now.millisecond(),
            offset.whole_hours(),
            level,
            kind,
            file!(),
            line!(),
            message
        )
    }
}

/// 起動時のローカル時刻のオフセット（日ごとのファイルの日付を決める。ftlog も行の時刻のオフセットを起動時に
/// 1 回だけ決めて使い続ける。サンドボックスに入った後は `/etc/localtime` を読めないこともある）
fn startup_local_offset() -> time::UtcOffset {
    #[cfg(unix)]
    {
        // time の `current_local_offset` はスレッドが複数あると失敗するので、libc の localtime_r で読む
        // SAFETY: `time(NULL)` は引数を書かない。`localtime_r` は再入可能で、ゼロ初期化した `tm`（POD）へ書くだけ。
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        let now = unsafe { libc::time(std::ptr::null_mut()) };
        if !unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
            if let Ok(o) = time::UtcOffset::from_whole_seconds(tm.tm_gmtoff as i32) {
                return o;
            }
        }
        time::UtcOffset::UTC
    }
    #[cfg(not(unix))]
    {
        time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC)
    }
}

/// 次の日のファイルを開けなかったときに、開き直しを試すまでの間隔
const DAILY_FILE_RETRY: std::time::Duration = std::time::Duration::from_secs(60);

/// 日ごとに切り替わるログファイル（B-343）。名前は ftlog の `FileAppender`（`Period::Day`）と同じ
/// `<名前>-<YYYYMMDD>.<拡張子>`（拡張子が無ければ `<名前>-<YYYYMMDD>`）で、日付は起動時のローカル時刻の
/// オフセットで決める。次の日のファイルを開けないときは今のファイルへ書き続け、`DAILY_FILE_RETRY` ごとに
/// 開き直しを試す（panic しない）。FreeBSD の capability mode では、起動時に登録した親ディレクトリの fd から
/// 開く（B-344。`open_log_append`）。
struct DailyFile {
    base: std::path::PathBuf,
    offset: time::UtcOffset,
    file: Option<io::BufWriter<std::fs::File>>,
    /// 今のファイルの日付（`YYYYMMDD` の数）
    day: u32,
    /// 次に日付を確かめる（または開き直しを試す）時刻
    next_check: Instant,
    /// ファイルを開く関数（試験で失敗を起こすために差し替える）
    open: fn(&std::path::Path) -> io::Result<std::fs::File>,
}

impl DailyFile {
    /// 今日のファイルを開いて作る（開けなければ起動を止める理由として返す）
    fn create(base: &str, offset: time::UtcOffset) -> io::Result<Self> {
        Self::create_with(base, offset, open_log_append, OffsetDateTime::now_utc())
    }

    fn create_with(
        base: &str,
        offset: time::UtcOffset,
        open: fn(&std::path::Path) -> io::Result<std::fs::File>,
        now_utc: OffsetDateTime,
    ) -> io::Result<Self> {
        let mut f = DailyFile {
            base: std::path::PathBuf::from(base),
            offset,
            file: None,
            day: 0,
            next_check: Instant::now(),
            open,
        };
        let local = now_utc.to_offset(offset);
        let day = ymd_number(local);
        let file = (f.open)(&dated_log_path(&f.base, day))?;
        f.file = Some(io::BufWriter::new(file));
        f.day = day;
        f.next_check = Instant::now() + until_next_day(local);
        Ok(f)
    }

    /// 日付が変わっていればその日のファイルへ切り替える。開けなければ今のファイルのまま、後でもう一度試す。
    fn roll(&mut self, now_utc: OffsetDateTime, now: Instant) {
        let local = now_utc.to_offset(self.offset);
        let day = ymd_number(local);
        if day == self.day {
            self.next_check = now + until_next_day(local);
            return;
        }
        match (self.open)(&dated_log_path(&self.base, day)) {
            Ok(file) => {
                if let Some(mut old) = self.file.replace(io::BufWriter::new(file)) {
                    let _ = io::Write::flush(&mut old);
                }
                self.day = day;
                self.next_check = now + until_next_day(local);
            }
            Err(_) => self.next_check = now + DAILY_FILE_RETRY,
        }
    }
}

impl io::Write for DailyFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let now = Instant::now();
        if now >= self.next_check {
            self.roll(OffsetDateTime::now_utc(), now);
        }
        match &mut self.file {
            Some(f) => f.write(buf),
            None => Err(io::Error::other("the log file is not open")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.file {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

/// `YYYYMMDD` の数（日付の比較用）
fn ymd_number(t: OffsetDateTime) -> u32 {
    t.year().max(0) as u32 * 10_000 + t.month() as u32 * 100 + t.day() as u32
}

/// 次のローカル時刻の 0 時までの時間
fn until_next_day(local: OffsetDateTime) -> std::time::Duration {
    let next = local.date().next_day().map(|d| {
        d.with_time(time::Time::MIDNIGHT)
            .assume_offset(local.offset())
    });
    match next {
        Some(next) => std::time::Duration::try_from(next - local).unwrap_or(DAILY_FILE_RETRY),
        None => DAILY_FILE_RETRY,
    }
}

/// ftlog の `FileAppender`（`Period::Day`）と同じ名前: `<名前>-<YYYYMMDD>.<拡張子>`・`<名前>-<YYYYMMDD>`
fn dated_log_path(base: &std::path::Path, day: u32) -> std::path::PathBuf {
    let ts = format!("{}{:02}{:02}", day / 10_000, day / 100 % 100, day % 100);
    match (base.file_stem(), base.extension()) {
        (Some(stem), Some(ext)) => base.with_file_name(format!(
            "{}-{}.{}",
            stem.to_string_lossy(),
            ts,
            ext.to_string_lossy()
        )),
        _ => base.with_file_name(format!(
            "{}-{}",
            base.file_name()
                .map(|x| x.to_string_lossy())
                .unwrap_or(Cow::from("log")),
            ts
        )),
    }
}

/// ログファイルを追記用に開く（無ければ作る）。FreeBSD の capability mode では、起動時に登録した親ディレクトリの
/// fd から開く（B-344）。
fn open_log_append(path: &std::path::Path) -> io::Result<std::fs::File> {
    #[cfg(target_os = "freebsd")]
    if let Some(res) = crate::security::capsicum::open_log_file_append(path) {
        return res;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// ログ出力先（stdout / stderr / 日ごとのファイル）を構築する
///
/// - `Some(path)`: `DailyFile`（日ごとに切り替え・内部バッファ）。今日のファイルを開けなければエラー
/// - `None`: `default_stderr` が true なら stderr、false なら stdout
fn build_log_sink(
    file_path: Option<&str>,
    default_stderr: bool,
    offset: time::UtcOffset,
) -> Result<Box<dyn io::Write + Send>, String> {
    match file_path {
        Some(path) => DailyFile::create(path, offset)
            .map(|f| Box::new(f) as Box<dyn io::Write + Send>)
            .map_err(|e| {
                format!(
                    "cannot open the log file {}: {}",
                    dated_log_path(
                        std::path::Path::new(path),
                        ymd_number(OffsetDateTime::now_utc().to_offset(offset))
                    )
                    .display(),
                    e
                )
            }),
        None => {
            if default_stderr {
                Ok(Box::new(io::stderr()))
            } else {
                Ok(Box::new(io::stdout()))
            }
        }
    }
}

/// レベル別ルーティング writer（ftlog の root として使用）
///
/// フォーマッタが埋め込んだセンチネル（`SENTINEL_APP` / `SENTINEL_ERROR`）を読み、
/// アプリ本体ログ / エラーログの出力先へ振り分ける。センチネルバイトは除去して出力する。
/// ftlog は 1 ログ行を 1 回の `write` で渡すため、行内の 1 バイト除去は前後 2 回の
/// `write_all` で実現する（追加アロケーションなし）。出力先の失敗は返さない（B-343。`LogSink`）。
struct LogRoutingWriter {
    app: LogSink,
    error: LogSink,
    json: bool,
    /// 捨てた行を知らせる行の時刻のオフセット
    offset: time::UtcOffset,
}

impl io::Write for LogRoutingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // センチネルを探索してルーティング先を決定
        let sentinel_pos = buf
            .iter()
            .position(|&b| b == SENTINEL_APP || b == SENTINEL_ERROR);
        let to_error = matches!(sentinel_pos, Some(i) if buf[i] == SENTINEL_ERROR);
        let (json, offset) = (self.json, self.offset);
        let w = if to_error {
            &mut self.error
        } else {
            &mut self.app
        };

        if json {
            // JSON 部分（最初の '{' 以降）のみ出力。センチネルは '{' より前にあるため同時に除去される。
            if let Some(j) = buf.iter().position(|&b| b == b'{') {
                w.write_line(&buf[j..], &[], json, offset);
            } else if let Some(i) = sentinel_pos {
                w.write_line(&buf[..i], &buf[i + 1..], json, offset);
            } else {
                w.write_line(buf, &[], json, offset);
            }
        } else if let Some(i) = sentinel_pos {
            // テキスト形式: センチネルバイトのみ除去して前後を出力
            w.write_line(&buf[..i], &buf[i + 1..], json, offset);
        } else {
            w.write_line(buf, &[], json, offset);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.app.flush();
        self.error.flush();
        Ok(())
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
/// 2. tokio::sync::mpscはveilの独自ランタイム（`src/runtime/`。tokio を使わない）と互換性がない
/// 3. 追加の非同期化層はオーバーヘッドを増やすだけ
///
/// ftlog公式ドキュメントより：
/// > ftlog mitigates this bottleneck by sending messages to a dedicated logger
/// > thread and computing as little as possible in the main/worker thread.
///
/// 代わりに、ftlogの設定を最適化することで同等以上の効果を得られます。
///
/// ログファイルを開けなければ（権限・ディレクトリが無い等）、起動を止める理由を返す（B-343。以前は panic）。
pub(crate) fn init_logging(config: &LoggingConfigSection) -> Result<ftlog::LoggerGuard, String> {
    let level = parse_log_level(&config.level);
    let use_json = config.format == LogFormat::Json;
    let offset = startup_local_offset();

    // app 本体ログ（既定: stdout）/ エラーログ（既定: stderr）の出力先を構築。
    let app_sink = build_log_sink(config.resolved_app_path(), false, offset)?;
    let error_sink = build_log_sink(config.resolved_error_path(), true, offset)?;

    let root = LogRoutingWriter {
        app: LogSink::new(app_sink, false),
        error: LogSink::new(error_sink, true),
        json: use_json,
        offset,
    };

    Ok(ftlog::builder()
        .max_log_level(level)
        .bounded(config.channel_size, false)
        .format(AppLogFormat { json: use_json })
        .root(root)
        .try_init()
        .expect("Failed to initialize ftlog"))
}

/// kTLSの状態をログ出力
pub(crate) fn log_ktls_status(ktls_config: &crate::KtlsConfig) {
    if ktls_config.enabled {
        // rustls + ktls2 使用時
        #[cfg(veil_ktls)]
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
                #[cfg(target_os = "linux")]
                warn!("kTLS: Ensure 'modprobe tls' has been run and kernel 5.15+ is used");
                #[cfg(target_os = "freebsd")]
                warn!("kTLS: Ensure 'sysctl kern.ipc.tls.enable=1' is set (FreeBSD 13.0+)");
                if ktls_config.fallback_enabled {
                    warn!("kTLS: Falling back to userspace TLS via rustls");
                } else {
                    error!("kTLS: Fallback disabled but kTLS unavailable - connections will fail!");
                    error!("kTLS: Either enable fallback or run 'modprobe tls'");
                }
            }
        }
        // kTLS フィーチャー無効時
        #[cfg(not(veil_ktls))]
        {
            warn!("kTLS: Enabled in config but ktls feature is not enabled");
            warn!("kTLS: Rebuild with: cargo build --features ktls for kTLS support");
        }
    } else {
        info!("kTLS: Disabled (using userspace TLS via rustls)");
    }
}

/// アクセスログ（テキスト / 構造化）かメトリクスのどれか 1 つでも出力先があるか。
///
/// `false` なら `log_access` は何もしないので、呼び出し側はリクエスト開始時刻を正確に
/// 読む必要が無い（[`request_start_instant`]）。
#[inline]
pub(crate) fn access_outputs_active() -> bool {
    #[cfg(not(feature = "access-log"))]
    let text_log = ftlog::log_enabled!(target: "access", Level::Info);
    #[cfg(feature = "access-log")]
    let text_log = ACCESS_LOG_ENABLED.load(std::sync::atomic::Ordering::Relaxed);
    text_log || (cfg!(feature = "metrics") && crate::metrics::metrics_runtime_enabled())
}

// `[access_log] enabled` の写し（要求ごとに読むので `CURRENT_CONFIG` を経由しない）。
// 既定は `AccessLogConfig::default()` と同じ
#[cfg(feature = "access-log")]
static ACCESS_LOG_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 設定のロード・リロードで `[access_log] enabled` を反映する（`CURRENT_CONFIG` の更新と並べて呼ぶ）
#[cfg(feature = "access-log")]
pub(crate) fn set_access_log_enabled(enabled: bool) {
    ACCESS_LOG_ENABLED.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

/// `log_access` に渡すリクエスト開始時刻。
///
/// 出力先が有効なら `Instant::now()`（処理時間を正確に測る）、無効なら値が使われないので
/// ランタイムのループ時刻キャッシュ（`runtime::time::coarse_now`、reactor では時刻読み取り
/// なし）で済ませる。FreeBSD aarch64 の HTTP/3 プロファイルで `__vdso_gettc`（時刻読み取り）が
/// 最上位だったための削減。
#[inline]
pub(crate) fn request_start_instant() -> Instant {
    if access_outputs_active() {
        Instant::now()
    } else {
        crate::runtime::time::coarse_now()
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
    // 出力先が 1 つも無ければ何もしない。本関数はリクエストごとに呼ばれ、以前は
    // ログもメトリクスも無効な構成でも経過時間・時刻の読み取り（2 回）と from_utf8（4 回）
    // を毎回行っていた（FreeBSD のプロファイルで from_utf8 と __vdso_gettc が上位）。
    if !access_outputs_active() {
        return;
    }

    // 処理時間は Instant で高精度計測（読んだ時刻はタイムスタンプの更新の判断にも使う。F-302）
    let now = Instant::now();
    let duration = now.saturating_duration_since(start_instant);
    let duration_ms = duration.as_millis();

    // タイムスタンプは Coarse Timer を使用（システムコール削減）
    // access-log feature が有効な場合はこの値を log_access_structured() にも渡す（二重 syscall 排除）
    let log_time = coarse_now_at(now);

    // access-log が無効な場合のみ ftlog 経由のテキストアクセスログを出力
    // （access-log 有効時は構造化ログと二重出力しない）
    //
    // `target: "access"` を指定することでフォーマッタが `type=access` を付与する。
    // レベル INFO のため app ストリーム（既定 stdout = access ログの既定出力先）へ流れ、
    // `type` フィールドでアプリ本体ログと判別できる。
    #[cfg(not(feature = "access-log"))]
    info!(target: "access", "Access: time={} duration={}ms method={} host={} path={} ua={} req_body_size={} status={} resp_body_size={}",
        log_time, duration_ms,
        std::str::from_utf8(method).unwrap_or("GET"), std::str::from_utf8(host).unwrap_or("-"),
        std::str::from_utf8(path).unwrap_or("-"), std::str::from_utf8(ua).unwrap_or("-"),
        req_body_size, status, resp_body_size);

    // Prometheusメトリクスを記録（無効なら文字列にもしない。構造化アクセスログはバイト列のまま渡し、
    // UTF-8 の検査はライタースレッドが行う。F-302）
    if cfg!(feature = "metrics") && crate::metrics::metrics_runtime_enabled() {
        record_request_metrics(
            std::str::from_utf8(method).unwrap_or("GET"),
            std::str::from_utf8(host).unwrap_or("-"),
            status,
            req_body_size,
            resp_body_size,
            duration.as_secs_f64(),
        );
    }

    // 構造化アクセスログ出力（F-21）
    // log_time と duration_ms を渡すことで syscall 二重発行と clone を排除
    #[cfg(feature = "access-log")]
    crate::access_log::log_access_structured(
        method,
        host,
        path,
        ua,
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
            app: LogSink::new(Box::new(SharedSink(app.clone())), false),
            error: LogSink::new(Box::new(SharedSink(err.clone())), true),
            json,
            offset: time::UtcOffset::UTC,
        };
        (w, app, err)
    }

    /// 試験用: `fail` が立っている間は書けない（ENOSPC）出力先
    struct FlakySink {
        out: Arc<Mutex<Vec<u8>>>,
        fail: Arc<std::sync::atomic::AtomicBool>,
    }
    impl io::Write for FlakySink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.fail.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(io::Error::from_raw_os_error(28)); // ENOSPC
            }
            self.out.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.fail.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(io::Error::from_raw_os_error(28));
            }
            Ok(())
        }
    }

    fn flaky_writer(
        json: bool,
    ) -> (
        LogRoutingWriter,
        Arc<Mutex<Vec<u8>>>,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        let out = Arc::new(Mutex::new(Vec::new()));
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let w = LogRoutingWriter {
            app: LogSink::new(
                Box::new(FlakySink {
                    out: out.clone(),
                    fail: fail.clone(),
                }),
                false,
            ),
            error: LogSink::new(Box::new(SharedSink(Arc::new(Mutex::new(Vec::new())))), true),
            json,
            offset: time::UtcOffset::UTC,
        };
        (w, out, fail)
    }

    fn app_line(msg: &str) -> String {
        format!(
            "2024-01-01 0ms {}INFO type=app main [f:1] {}\n",
            SENTINEL_APP as char, msg
        )
    }

    /// B-343: 出力先が書けなくても ftlog へは失敗を返さず（返すとログスレッドが panic する）、捨てた行の数を
    /// 次に書けたときに 1 行で知らせる。
    #[test]
    fn unwritable_output_is_not_an_error_and_dropped_lines_are_reported() {
        let (mut w, out, fail) = flaky_writer(false);
        w.write_all(app_line("first").as_bytes()).unwrap();
        fail.store(true, std::sync::atomic::Ordering::Relaxed);
        for i in 0..3 {
            assert_eq!(
                w.write(app_line(&format!("lost{i}")).as_bytes()).unwrap(),
                app_line("lost0").len()
            );
        }
        w.flush().unwrap();
        fail.store(false, std::sync::atomic::Ordering::Relaxed);
        w.write_all(app_line("after").as_bytes()).unwrap();
        let text = s(&out);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].ends_with("first"));
        assert!(!text.contains("lost"));
        assert!(
            lines[1].contains("WARN type=app")
                && lines[1].ends_with(
                    "3 log lines were dropped because the log output could not be written"
                ),
            "{}",
            lines[1]
        );
        assert!(lines[2].ends_with("after"));
        // 知らせた後は数え直す
        w.write_all(app_line("again").as_bytes()).unwrap();
        assert_eq!(s(&out).lines().count(), 4);
    }

    /// JSON 形式では知らせる行も JSON の 1 行（センチネル・ftlog の接頭辞なし）。
    #[test]
    fn dropped_lines_notice_is_a_json_line_in_json_format() {
        let (mut w, out, fail) = flaky_writer(true);
        fail.store(true, std::sync::atomic::Ordering::Relaxed);
        let line = format!(
            "2024-01-01 0ms {}{{\"level\":\"INFO\",\"type\":\"app\",\"message\":\"x\"}}\n",
            SENTINEL_APP as char
        );
        w.write_all(line.as_bytes()).unwrap();
        fail.store(false, std::sync::atomic::Ordering::Relaxed);
        w.write_all(line.as_bytes()).unwrap();
        let text = s(&out);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(
            lines[0].starts_with('{') && lines[0].ends_with('}'),
            "{}",
            lines[0]
        );
        assert!(
            lines[0].contains(r#""level":"WARN","type":"app""#),
            "{}",
            lines[0]
        );
        assert!(
            lines[0].contains(r#""message":"1 log lines were dropped"#),
            "{}",
            lines[0]
        );
        assert_eq!(lines[1], r#"{"level":"INFO","type":"app","message":"x"}"#);
    }

    /// 名前は ftlog の FileAppender（`Period::Day`）と同じ（既存の運用のファイル名を変えない）。
    #[test]
    fn dated_log_path_matches_ftlog_daily_names() {
        use std::path::Path;
        assert_eq!(
            dated_log_path(Path::new("/var/log/veil/app.log"), 20261108),
            Path::new("/var/log/veil/app-20261108.log")
        );
        assert_eq!(
            dated_log_path(Path::new("logs/veil"), 20260105),
            Path::new("logs/veil-20260105")
        );
        assert_eq!(
            dated_log_path(Path::new("a.b.log"), 20261231),
            Path::new("a.b-20261231.log")
        );
    }

    fn at(y: i32, m: u8, d: u8, h: u8) -> OffsetDateTime {
        time::Date::from_calendar_date(y, time::Month::try_from(m).unwrap(), d)
            .unwrap()
            .with_hms(h, 0, 0)
            .unwrap()
            .assume_utc()
    }

    fn fail_open(_: &std::path::Path) -> io::Result<std::fs::File> {
        Err(io::Error::from_raw_os_error(13)) // EACCES
    }

    /// B-343: 日付が変わると次の日のファイルへ切り替える。開けなければ（ftlog は `unwrap` で panic した）
    /// 今のファイルへ書き続け、後でもう一度試す。
    #[test]
    #[allow(clippy::disallowed_methods)] // 理由: 試験（書き出したファイルを読み戻す）
    fn daily_file_rolls_over_and_keeps_writing_when_the_next_file_cannot_be_opened() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("app.log");
        let base_s = base.to_str().unwrap();
        let utc = time::UtcOffset::UTC;
        let mut f =
            DailyFile::create_with(base_s, utc, open_log_append, at(2026, 11, 8, 23)).unwrap();
        f.write_all(b"day1\n").unwrap();
        // 次の日: 開けない
        f.open = fail_open;
        let now = Instant::now();
        f.roll(at(2026, 11, 9, 0), now);
        assert_eq!(f.day, 20261108);
        assert_eq!(f.next_check, now + DAILY_FILE_RETRY);
        f.write_all(b"still-day1\n").unwrap();
        // 開けるようになったら切り替える
        f.open = open_log_append;
        f.roll(at(2026, 11, 9, 0), now);
        assert_eq!(f.day, 20261109);
        f.write_all(b"day2\n").unwrap();
        f.flush().unwrap();
        let read = |name: &str| std::fs::read_to_string(dir.path().join(name)).unwrap();
        assert_eq!(read("app-20261108.log"), "day1\nstill-day1\n");
        assert_eq!(read("app-20261109.log"), "day2\n");
        // 同じ日のうちは切り替えない（次の 0 時までの待ちを置き直すだけ）
        f.roll(at(2026, 11, 9, 12), now);
        assert_eq!(f.day, 20261109);
        assert_eq!(
            f.next_check,
            now + std::time::Duration::from_secs(12 * 3600)
        );
    }

    /// 今日のファイルを開けなければ、起動を止める理由を返す（ftlog は panic した）。
    #[test]
    fn daily_file_reports_an_error_when_the_first_file_cannot_be_opened() {
        assert!(DailyFile::create_with(
            "app.log",
            time::UtcOffset::UTC,
            fail_open,
            at(2026, 1, 1, 0)
        )
        .is_err());
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-dir").join("app.log");
        let err = build_log_sink(missing.to_str(), false, time::UtcOffset::UTC)
            .err()
            .unwrap();
        assert!(err.starts_with("cannot open the log file "), "{err}");
    }

    #[test]
    fn until_next_day_counts_to_local_midnight() {
        let jst = time::UtcOffset::from_hms(9, 0, 0).unwrap();
        // UTC 14:00 = JST 23:00 → 1 時間
        let t = at(2026, 11, 8, 14).to_offset(jst);
        assert_eq!(until_next_day(t), std::time::Duration::from_secs(3600));
        assert_eq!(ymd_number(t), 20261108);
        assert_eq!(ymd_number(at(2026, 11, 8, 15).to_offset(jst)), 20261109);
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
        let app_line = format!(
            "2024-01-01 0ms {}INFO type=app main [f:1] hello\n",
            SENTINEL_APP as char
        );
        w.write_all(app_line.as_bytes()).unwrap();
        // error 行（センチネル ERROR）
        let err_line = format!(
            "2024-01-01 0ms {}ERROR type=error main [f:2] boom\n",
            SENTINEL_ERROR as char
        );
        w.write_all(err_line.as_bytes()).unwrap();

        let app_out = s(&app);
        let err_out = s(&err);
        // app にはアプリ行のみ、error にはエラー行のみ
        assert!(app_out.contains("hello"), "app: {}", app_out);
        assert!(
            !app_out.contains("boom"),
            "app must not have error: {}",
            app_out
        );
        assert!(err_out.contains("boom"), "err: {}", err_out);
        assert!(
            !err_out.contains("hello"),
            "err must not have app: {}",
            err_out
        );
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

#[cfg(test)]
mod log_throttle_tests {
    use super::*;

    #[test]
    fn throttle_allows_once_per_interval_and_counts_the_rest() {
        let t = LogThrottle::new();
        assert_eq!(t.check(), Some(0));
        for _ in 0..5 {
            assert_eq!(t.check(), None);
        }
        // 間隔が過ぎたことにする
        t.next_ms.store(0, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(t.check(), Some(5));
        assert_eq!(t.check(), None);
    }
}
