// ====================
// 構造化アクセスログ（F-21）
// ====================
//
// JSON/テキスト形式の構造化アクセスログ出力モジュール。
//
// # アーキテクチャ（パフォーマンス設計）
//
// ホットパス（ワーカースレッド）:
//   スレッドごとの溜め（`Pending`）へ 1 要求の値（数値とバイト列）を写すだけ（整形・行ごとの確保・チャネル無し。F-302）
//   → 溜めの錠はライターの取り替えとしか取り合わない（ワーカーどうしは取り合わない）
//   → タイムスタンプは呼び出し元 coarse_now() の OffsetDateTime を受け取る（syscall 排除）
//   → 送り先はスレッドごとに控え、リロードで世代が変わったときだけ引き直す（F-302）
//
// ログスレッド（std::thread、起動時に 1 本生成、ホットリロード時に差し替え）:
//   BufWriter<File or Stdout> を独占し、流れている間は 1ms ごとに各溜めの中身を空のバッファと取り替え、
//   JSON/テキストの行にまとめて整形して書く
//   → 静かな間は眠り、最初に行を足したワーカーが起こす（流れている間は起こさない。B-335）
//   → flush_interval_ms 周期で定期フラッシュ
//   → リロードで差し替えられたら、溜めを閉じて溜まった分を書き切って終わる
//   → FreeBSD の capability mode では同じパスで開き直す指示だけを受ける（F-178）
//
// # 設計の要点
//
// - std::io::stdout() のグローバル Mutex を完全排除
// - 書き込み（I/O）は錠の外。ライターが詰まっている間はスレッドごとに channel_size 行まで溜め、超えた行は落とす
// - ArcSwapOption による lock-free な送り先の差し替え（ホットリロード対応）
// - tokio 非依存（std::thread のみ）

use arc_swap::{ArcSwap, ArcSwapOption};
use std::cell::RefCell;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};
use time::OffsetDateTime;

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
    /// ログスレッドが書くのを待てる行数（要求を処理するスレッドごと。F-302）
    ///
    /// 書き込み先が詰まってログスレッドが遅れている間に、1 つのスレッドでこれを超えた行は
    /// サイレントドロップする（リクエスト処理はブロックしない）。
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
//
// F-302: ワーカーは 1 要求の値（数値と、メソッド・ホスト・パス等のバイト列）をスレッドごとの溜め（`Pending`）へ写すだけにし、
// ライタースレッドが溜めの中身を空のバッファと取り替えて、まとめて行へ整形して書く。以前はワーカーが行を整形し、行ごとに
// `Vec` を確保してチャネルへ送り、ライターのスレッドで解放していた（端末の Python の offload=True で、1 要求あたり整形に
// 約 1.7µs、確保・チャネル・別スレッドでの解放に約 1.4µs）。
// 別のワーカーの行どうしは、取り替えの周期（流れている間は 1ms）の中で前後し得る（nginx の `buffer=` と同じ）。

/// ワーカーのスレッドごとの溜め
struct Pending {
    /// 書かれるのを待つ記録の並び（本番は `put_record` の形式）
    buf: Vec<u8>,
    /// `buf` の記録の数。`Sink::limit` に達したら以後の行は落とす
    lines: usize,
    /// 送り先が止まった（リロード・無効化）。以後は足さず、足す側が今の送り先を引き直す
    closed: bool,
}

type Slot = Mutex<Pending>;

/// ライターが取り出しに回る溜めの一覧
struct Registry {
    slots: Vec<Arc<Slot>>,
    /// 止まった送り先には溜めを登録させない（最後の取り出しの後に足された行が残らないように）
    closed: bool,
}

/// 溜めの中身（`batch`）を `config` の書式の行にして `out` へ足す
type Render = fn(&[u8], &AccessLogConfig, &mut Vec<u8>);

/// 1 本のライタースレッドと、そこへ行を渡すスレッドごとの溜め
struct Sink {
    /// 1 つの溜めに積める行数（`channel_size`）
    limit: usize,
    /// 行の書式（`format`・`fields`）。FreeBSD の capability mode のリロードでは送り先を残してこれだけ差し替える
    config: ArcSwap<AccessLogConfig>,
    /// 溜めの中身を行にする（本番は `render_records`）
    render: Render,
    registry: Mutex<Registry>,
    /// ライターが行の来るのを待って眠っている（行を足したワーカーが起こす）
    idle: AtomicBool,
    /// 溜まった分を書いて終わる（リロード・無効化）
    stop: AtomicBool,
    /// 同じパスで開き直す（F-178）
    reopen: AtomicBool,
    writer: OnceLock<std::thread::Thread>,
}

/// 今の送り先。ワーカーはスレッドごとに控え、`generation` が変わったときだけ引き直す
/// （要求ごとに `ArcSwap` を読まない。読み取りは 1 回 100 命令ほどかかる）
struct Current {
    /// 無効なら `None`
    sink: ArcSwapOption<Sink>,
    /// 差し替えるたびに `NEXT_GENERATION` から払い出す（`Current` をまたいで重ならない）
    generation: AtomicUsize,
}

static CURRENT: Current = Current::new();

static NEXT_GENERATION: AtomicUsize = AtomicUsize::new(1);

/// スレッドごとに控えた送り先と溜め
struct Mine {
    generation: usize,
    /// 無効なら `None`
    active: Option<Active>,
}

struct Active {
    sink: Arc<Sink>,
    slot: Arc<Slot>,
}

thread_local! {
    static MINE: RefCell<Option<Mine>> = const { RefCell::new(None) };
}

impl Current {
    const fn new() -> Self {
        Current {
            sink: ArcSwapOption::const_empty(),
            generation: AtomicUsize::new(0),
        }
    }

    /// 送り先を差し替える。前の送り先のライターには溜まった分を書いて終わるよう指示する
    fn replace(&self, sink: Option<Arc<Sink>>) {
        let old = self.sink.swap(sink);
        self.generation.store(
            NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
            Ordering::Release,
        );
        // 世代を進めた後に閉じる（閉じた溜めを見たワーカーが世代を読み直すと新しい送り先が見える）
        if let Some(old) = old {
            old.retire();
        }
    }
}

impl Mine {
    /// 今の送り先を控え直す。同じ送り先なら溜めはそのまま使う。
    /// 止まった送り先に当たった（リロードと行き違った）ら `None`（呼び出し側が世代を読み直して引き直す）
    fn refresh(current: &Current, generation: usize, prev: Option<Mine>) -> Option<Mine> {
        let Some(sink) = current.sink.load_full() else {
            return Some(Mine {
                generation,
                active: None,
            });
        };
        let slot = match prev.and_then(|m| m.active) {
            Some(a) if Arc::ptr_eq(&a.sink, &sink) => a.slot,
            _ => sink.register()?,
        };
        Some(Mine {
            generation,
            active: Some(Active { sink, slot }),
        })
    }
}

/// 溜め・取り替えのバッファの最初の大きさ
const SLOT_INITIAL_CAPACITY: usize = 4 * 1024;
/// 書き終えたバッファをこれより大きいまま持ち回らない（ライターが詰まった後の大きな溜めを返す）
const SPARE_KEEP_CAPACITY: usize = 64 * 1024;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn shrink(buf: &mut Vec<u8>) {
    if buf.capacity() > SPARE_KEEP_CAPACITY {
        buf.shrink_to(SPARE_KEEP_CAPACITY);
    }
}

impl Sink {
    fn new(limit: usize, config: Arc<AccessLogConfig>, render: Render) -> Sink {
        Sink {
            limit: limit.max(1),
            config: ArcSwap::new(config),
            render,
            registry: Mutex::new(Registry {
                slots: Vec::new(),
                closed: false,
            }),
            idle: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            reopen: AtomicBool::new(false),
            writer: OnceLock::new(),
        }
    }

    /// ライタースレッドを起動する
    fn start(config: &Arc<AccessLogConfig>) -> Arc<Sink> {
        Sink::start_with(config, render_records)
    }

    fn start_with(config: &Arc<AccessLogConfig>, render: Render) -> Arc<Sink> {
        let sink = Arc::new(Sink::new(config.channel_size, Arc::clone(config), render));
        let s = Arc::clone(&sink);
        let file_path = config.file_path.clone();
        // 0 だと眠らずに回り続ける
        let flush_interval = Duration::from_millis(config.flush_interval_ms.max(1));
        let handle = std::thread::Builder::new()
            .name("access-log-writer".to_string())
            .spawn(move || run_writer(&s, file_path.as_deref(), flush_interval))
            .expect("access-log: failed to spawn writer thread");
        let _ = sink.writer.set(handle.thread().clone());
        sink
    }

    /// 呼んだスレッドの溜めを作って登録する（スレッドごと・送り先ごとに 1 回）。止まった送り先なら `None`
    fn register(&self) -> Option<Arc<Slot>> {
        let mut reg = lock(&self.registry);
        if reg.closed {
            return None;
        }
        let slot = Arc::new(Mutex::new(Pending {
            buf: Vec::with_capacity(SLOT_INITIAL_CAPACITY),
            lines: 0,
            closed: false,
        }));
        reg.slots.push(Arc::clone(&slot));
        Some(slot)
    }

    fn wake(&self) {
        if let Some(t) = self.writer.get() {
            t.unpark();
        }
    }

    /// 溜まった分を書いて終わるようライターに指示する
    fn retire(&self) {
        self.stop.store(true, Ordering::Release);
        self.wake();
    }

    /// 各溜めの中身を空のバッファと取り替えて `batch` へ移す（錠の中は取り替えだけ。整形・書き込みは錠の外）。
    /// `close` なら溜めを閉じる（以後ワーカーは足さない）。何か取り出したら true
    fn collect(&self, batch: &mut Batch, close: bool) -> bool {
        let mut reg = lock(&self.registry);
        if close {
            reg.closed = true;
        }
        reg.slots.retain(|slot| {
            let mut p = lock(slot);
            if close {
                p.closed = true;
            }
            if !p.buf.is_empty() {
                let mut fresh = batch
                    .spare
                    .pop()
                    .unwrap_or_else(|| Vec::with_capacity(SLOT_INITIAL_CAPACITY));
                std::mem::swap(&mut p.buf, &mut fresh);
                p.lines = 0;
                batch.full.push(fresh);
            }
            drop(p);
            // 足すスレッドが終わった溜め（登録の 1 本だけが残る）は外す。中身は上で取り出した
            Arc::strong_count(slot) > 1
        });
        !batch.full.is_empty()
    }
}

/// 溜めから取り出したバッファ（書く分）、取り替えに渡す空のバッファ、整形した行
#[derive(Default)]
struct Batch {
    full: Vec<Vec<u8>>,
    spare: Vec<Vec<u8>>,
    text: Vec<u8>,
}

impl Batch {
    fn write_to(&mut self, out: &mut impl Write, sink: &Sink) {
        if self.full.is_empty() {
            return;
        }
        let config = sink.config.load();
        for mut buf in self.full.drain(..) {
            (sink.render)(&buf, &config, &mut self.text);
            let _ = out.write_all(&self.text);
            self.text.clear();
            buf.clear();
            shrink(&mut buf);
            self.spare.push(buf);
        }
        shrink(&mut self.text);
    }
}

/// 今の送り先（`current`）のこのスレッドの溜めへ 1 要求分を足す。`write` が溜めへ写す（無効なら呼ばない）
fn append_line(current: &Current, write: impl FnOnce(&mut Vec<u8>)) {
    let _ = MINE.try_with(|cell| {
        let mut mine = cell.borrow_mut();
        // 2 周目は、リロードで止まった送り先に当たったとき（世代を読み直して今の送り先で登録し直す）
        for _ in 0..2 {
            let generation = current.generation.load(Ordering::Acquire);
            if mine.as_ref().map(|m| m.generation) != Some(generation) {
                *mine = Mine::refresh(current, generation, mine.take());
            }
            let Some(m) = mine.as_ref() else {
                continue;
            };
            let Some(a) = m.active.as_ref() else {
                return;
            };
            let mut p = lock(&a.slot);
            if p.closed {
                drop(p);
                *mine = None;
                continue;
            }
            if p.lines >= a.sink.limit {
                // ライターが遅れている（書き込み先が詰まっている）。要求の処理を止めずに落とす
                return;
            }
            write(&mut p.buf);
            p.lines += 1;
            drop(p);
            // ライターが眠っているときだけ起こす（流れている間は起こさない。B-335）
            if a.sink.idle.load(Ordering::Relaxed) && a.sink.idle.swap(false, Ordering::AcqRel) {
                a.sink.wake();
            }
            return;
        }
    });
}

/// 起動時にアクセスログライタースレッドを初期化する
pub(crate) fn init_access_log_writer(config: &Arc<AccessLogConfig>) {
    if !config.enabled {
        return;
    }
    CURRENT.replace(Some(Sink::start(config)));
}

/// ホットリロード時にアクセスログライタースレッドを差し替える
///
/// 旧スレッドは溜まった行を書き切ってから終わる。
pub(crate) fn reload_access_log_writer(config: &Arc<AccessLogConfig>) {
    // F-178: FreeBSD の capability mode 中は新しいパスを開けず、スレッドも作り直さない。
    // 出力先が変わらないこと（変わる設定はリロード時に拒否済み。
    // `config::check_capability_mode_reload`）を前提に、既存のライタースレッドへ
    // 同じパスでの開き直しだけを指示する（logrotate の move + SIGHUP に対応）。
    #[cfg(target_os = "freebsd")]
    if crate::security::capsicum::is_capability_mode() {
        if !config.enabled {
            CURRENT.replace(None);
        } else if let Some(sink) = CURRENT.sink.load().as_ref() {
            // 書式・項目の変更は反映する（送り先と channel_size・flush_interval_ms は前のまま）
            sink.config.store(Arc::clone(config));
            sink.reopen.store(true, Ordering::Release);
            sink.wake();
        }
        return;
    }
    CURRENT.replace(config.enabled.then(|| Sink::start(config)));
}

/// アクセスログを追記用に開く（無ければ作る）。
///
/// FreeBSD の capability mode では絶対パスの `open` ができないため、`cap_enter` 前に登録した
/// 親ディレクトリ fd から開く（F-178。`security::capsicum::open_access_log_append`）。
fn open_log_file(path: &str) -> io::Result<std::fs::File> {
    #[cfg(target_os = "freebsd")]
    if let Some(res) = crate::security::capsicum::open_access_log_append(std::path::Path::new(path))
    {
        return res;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// 流れている間に溜まった行をまとめて書く間隔（B-335）。この間にスレッドごとに `channel_size`（既定 10,000）を
/// 超える行が来ない限り落ちない（1 スレッド 1,000 万行/秒に相当）。
const WRITER_BATCH_INTERVAL: Duration = Duration::from_millis(1);

enum Output {
    File(io::BufWriter<std::fs::File>),
    Stdout(io::BufWriter<io::Stdout>),
}

impl Write for Output {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Output::File(w) => w.write(buf),
            Output::Stdout(w) => w.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Output::File(w) => w.flush(),
            Output::Stdout(w) => w.flush(),
        }
    }
}

/// 出力先を開く（未指定・開けないときは標準出力）
fn open_output(file_path: Option<&str>) -> Output {
    match file_path {
        Some(path) => match open_log_file(path) {
            Ok(f) => Output::File(io::BufWriter::new(f)),
            Err(e) => {
                eprintln!("access-log: failed to open '{}': {}", path, e);
                Output::Stdout(io::BufWriter::new(io::stdout()))
            }
        },
        None => Output::Stdout(io::BufWriter::new(io::stdout())),
    }
}

/// 同じパスで開き直す（F-178）。失敗したら旧ファイルへ書き続ける（stdout へは切り替えない）
fn reopen_output(out: &mut Output, file_path: Option<&str>) {
    let (Some(path), Output::File(_)) = (file_path, &*out) else {
        return;
    };
    let _ = out.flush();
    match open_log_file(path) {
        Ok(f) => *out = Output::File(io::BufWriter::new(f)),
        Err(e) => eprintln!(
            "access-log: failed to reopen '{}' (keeping the previous file): {}",
            path, e
        ),
    }
}

/// ライタースレッドの本体
fn run_writer(sink: &Sink, file_path: Option<&str>, flush_interval: Duration) {
    let mut out = open_output(file_path);
    let mut batch = Batch::default();
    let mut last_flush = Instant::now();
    loop {
        let stop = sink.stop.load(Ordering::Acquire);
        let got = sink.collect(&mut batch, stop);
        batch.write_to(&mut out, sink);
        if stop {
            break;
        }
        if sink.reopen.swap(false, Ordering::AcqRel) {
            reopen_output(&mut out, file_path);
        }
        if got {
            // B-335: 流れている間は通知で眠らない（ワーカーが要求ごとにこのスレッドを起こさない）。
            // 溜まった分をまとめて書いて短く眠る
            if last_flush.elapsed() >= flush_interval {
                let _ = out.flush();
                last_flush = Instant::now();
            }
            crate::server::cap_safe_sleep(WRITER_BATCH_INTERVAL);
            continue;
        }
        // 静かな間は行が来るまで眠る（行を足したワーカーが起こす）
        sink.idle.store(true, Ordering::SeqCst);
        // 眠ると決めた後に足された行を拾う（足した側がまだ idle を見ていなかった分。溜めの錠を挟むので、
        // ここで見えなかった行を足した側は必ず idle を見て起こす）
        if sink.collect(&mut batch, false) {
            sink.idle.store(false, Ordering::Relaxed);
            batch.write_to(&mut out, sink);
            continue;
        }
        if !sink.stop.load(Ordering::Acquire) && !sink.reopen.load(Ordering::Acquire) {
            std::thread::park_timeout(flush_interval);
        }
        if sink.idle.swap(false, Ordering::AcqRel) {
            // 誰にも起こされなかった（flush_interval の間、行が来なかった）
            let _ = out.flush();
            last_flush = Instant::now();
        }
    }
    let _ = out.flush();
}

// ====================
// 1 要求の記録（溜めの中身）
// ====================

/// 記録の固定部の長さ（status u16・req_body_size u64・resp_body_size u64・duration_ms u64・秒 i64・ナノ秒 u32）
const RECORD_FIXED: usize = 2 + 8 + 8 + 8 + 8 + 4;

/// 1 要求の値を溜めへ写す（整形はライターがまとめて行う）。
///
/// `[本体の長さ u32][固定部][長さ u32 + バイト列] × 6`（method・host・path・user_agent・client_ip・upstream の順）。
/// 数値は little endian。
#[allow(clippy::too_many_arguments)]
fn put_record(
    buf: &mut Vec<u8>,
    status: u16,
    req_body_size: u64,
    resp_body_size: u64,
    duration_ms: u64,
    time: OffsetDateTime,
    texts: [&[u8]; 6],
) {
    // 長さは u32 で持つ（ヘッダの値なので超えることはないが、超えた分は切り詰める）
    let texts = texts.map(|t| &t[..t.len().min(u32::MAX as usize)]);
    let len = RECORD_FIXED + texts.iter().map(|t| 4 + t.len()).sum::<usize>();
    buf.reserve(4 + len);
    buf.extend_from_slice(&(len as u32).to_le_bytes());
    buf.extend_from_slice(&status.to_le_bytes());
    buf.extend_from_slice(&req_body_size.to_le_bytes());
    buf.extend_from_slice(&resp_body_size.to_le_bytes());
    buf.extend_from_slice(&duration_ms.to_le_bytes());
    buf.extend_from_slice(&time.unix_timestamp().to_le_bytes());
    buf.extend_from_slice(&time.nanosecond().to_le_bytes());
    for t in texts {
        buf.extend_from_slice(&(t.len() as u32).to_le_bytes());
        buf.extend_from_slice(t);
    }
}

/// 溜めの中の 1 要求の記録
struct Record<'a> {
    status: u16,
    req_body_size: u64,
    resp_body_size: u64,
    duration_ms: u64,
    time: OffsetDateTime,
    method: &'a [u8],
    host: &'a [u8],
    path: &'a [u8],
    user_agent: &'a [u8],
    client_ip: &'a [u8],
    upstream: &'a [u8],
}

/// 記録を前から読む
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    fn text(&mut self) -> Option<&'a [u8]> {
        let n = u32::from_le_bytes(self.array()?) as usize;
        self.take(n)
    }
}

/// `rest` の先頭の記録を読んで進める（終わり・壊れた記録なら `None`）
fn read_record<'a>(rest: &mut &'a [u8]) -> Option<Record<'a>> {
    let mut outer = Reader(rest);
    let len = u32::from_le_bytes(outer.array()?) as usize;
    let mut r = Reader(outer.take(len)?);
    *rest = outer.0;
    let status = u16::from_le_bytes(r.array()?);
    let req_body_size = u64::from_le_bytes(r.array()?);
    let resp_body_size = u64::from_le_bytes(r.array()?);
    let duration_ms = u64::from_le_bytes(r.array()?);
    let secs = i64::from_le_bytes(r.array()?);
    let nanos = u32::from_le_bytes(r.array()?);
    let time = OffsetDateTime::from_unix_timestamp(secs)
        .ok()
        .and_then(|t| t.replace_nanosecond(nanos).ok())
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    Some(Record {
        status,
        req_body_size,
        resp_body_size,
        duration_ms,
        time,
        method: r.text()?,
        host: r.text()?,
        path: r.text()?,
        user_agent: r.text()?,
        client_ip: r.text()?,
        upstream: r.text()?,
    })
}

/// UTF-8 でない値は `fallback` にする（以前の `log_access` と同じ）
fn text_or<'a>(b: &'a [u8], fallback: &'static str) -> &'a str {
    std::str::from_utf8(b).unwrap_or(fallback)
}

/// 溜めの記録をアクセスログの行にする（ライタースレッド）
fn render_records(mut batch: &[u8], config: &AccessLogConfig, out: &mut Vec<u8>) {
    let build = if config.format == AccessLogFormat::Json {
        build_json_log
    } else {
        build_text_log
    };
    while let Some(r) = read_record(&mut batch) {
        build(
            out,
            r.time,
            text_or(r.method, "GET"),
            text_or(r.host, "-"),
            text_or(r.path, "-"),
            r.status,
            u128::from(r.duration_ms),
            text_or(r.client_ip, "-"),
            text_or(r.upstream, ""),
            r.req_body_size,
            r.resp_body_size,
            text_or(r.user_agent, "-"),
            &config.fields,
        );
    }
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
    let mut first = true;

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

    // フィールド順: timestamp → type → 以降。
    // timestamp は fields フィルタ対象（未指定なら省略）。
    json_field!("timestamp", {
        buf.push(b'"');
        write_rfc3339_into_buf(buf, timestamp);
        buf.push(b'"');
    });
    // 識別用 type フィールドは timestamp の直後に必ず出力する（fields フィルタ対象外）。
    // アプリ本体ログ / エラーログと区別するための固定値 "access"。
    if !first {
        buf.push(b',');
    }
    buf.extend_from_slice(b"\"type\":\"access\"");
    first = false;
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

/// テキスト形式のログへ要求由来の値を書く。制御文字・`\`・0x7f 以上を `\xNN` にする（nginx の `escape=default` と同じ）。
///
/// 端末で `tail` したときに ESC 等のエスケープ列が解釈されて画面を消したり偽のログ行を作ったりしないように（B-233）。
/// ほとんどの値は印字可能な ASCII だけなので、そのときは確保もせずそのまま書く。
#[inline]
fn write_text_escaped(buf: &mut Vec<u8>, s: &[u8]) {
    if s.iter().all(|&b| (0x20..0x7f).contains(&b) && b != b'\\') {
        buf.extend_from_slice(s);
        return;
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &b in s {
        if (0x20..0x7f).contains(&b) && b != b'\\' {
            buf.push(b);
        } else {
            buf.extend_from_slice(&[b'\\', b'x', HEX[(b >> 4) as usize], HEX[(b & 0xf) as usize]]);
        }
    }
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
    let mut first = true;

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

    // フィールド順: timestamp → type → 以降。
    text_field!("timestamp", {
        write_rfc3339_into_buf(buf, timestamp);
    });
    // 識別用 type フィールドは timestamp の直後に必ず出力する（fields フィルタ対象外）。
    if !first {
        buf.push(b' ');
    }
    buf.extend_from_slice(b"type=access");
    first = false;
    text_field!("method", {
        write_text_escaped(buf, method.as_bytes());
    });
    text_field!("host", {
        write_text_escaped(buf, host.as_bytes());
    });
    text_field!("path", {
        write_text_escaped(buf, path.as_bytes());
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
        write_text_escaped(buf, upstream.as_bytes());
    });
    text_field!("req_body_size", {
        write_u64(buf, req_body_size);
    });
    text_field!("resp_body_size", {
        write_u64(buf, resp_body_size);
    });
    text_field!("user_agent", {
        write_text_escaped(buf, user_agent.as_bytes());
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
/// - 整形しない: 値（数値とバイト列）をスレッドごとの溜めへ写すだけ。`format`・`fields` での整形・UTF-8 の検査・
///   タイムスタンプの文字列化はライタースレッドがまとめて行う（要求ごとに設定も読まない。F-302）
/// - I/O: ライタースレッドが溜めを取り替えて書く（F-302）
#[allow(clippy::too_many_arguments)]
pub(crate) fn log_access_structured(
    method: &[u8],
    host: &[u8],
    path: &[u8],
    ua: &[u8],
    req_body_size: u64,
    status: u16,
    resp_body_size: u64,
    log_time: OffsetDateTime,
    duration_ms: u128,
    client_ip: &str,
    upstream: &str,
) {
    append_line(&CURRENT, |buf| {
        put_record(
            buf,
            status,
            req_body_size,
            resp_body_size,
            u64::try_from(duration_ms).unwrap_or(u64::MAX),
            log_time,
            [
                method,
                host,
                path,
                ua,
                client_ip.as_bytes(),
                upstream.as_bytes(),
            ],
        );
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

    // 理由付き allow: 試験のスレッド（イベントループの外）で一時ファイルを消す
    #[allow(clippy::disallowed_methods)]
    fn temp_log(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "veil-access-log-{}-{}-{:?}.log",
            tag,
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn file_config(path: &std::path::Path) -> Arc<AccessLogConfig> {
        Arc::new(AccessLogConfig {
            enabled: true,
            file_path: Some(path.to_string_lossy().into_owned()),
            channel_size: 1_000_000,
            flush_interval_ms: 5,
            ..AccessLogConfig::default()
        })
    }

    fn enabled_config() -> Arc<AccessLogConfig> {
        Arc::new(AccessLogConfig {
            enabled: true,
            ..AccessLogConfig::default()
        })
    }

    /// 試験用: 溜めの中身（試験が写した行）をそのまま書く
    fn render_raw(batch: &[u8], _: &AccessLogConfig, out: &mut Vec<u8>) {
        out.extend_from_slice(batch);
    }

    /// 本番の `init`・`reload` と同じ差し替えを、試験ごとの `Current` に行う
    fn start_on(cell: &Current, config: &Arc<AccessLogConfig>) {
        cell.replace(Some(Sink::start_with(config, render_raw)));
    }

    fn stop_on(cell: &Current) {
        cell.replace(None);
    }

    /// 一時ファイルの行の合計が `want` に達するまで待って読む
    // 理由付き allow: 試験のスレッド（イベントループの外）で一時ファイルを読み、待つ
    #[allow(clippy::disallowed_methods)]
    fn wait_lines(paths: &[&std::path::Path], want: usize) -> Vec<Vec<String>> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let got: Vec<Vec<String>> = paths
                .iter()
                .map(|p| {
                    std::fs::read_to_string(p)
                        .unwrap_or_default()
                        .lines()
                        .map(str::to_owned)
                        .collect()
                })
                .collect();
            if got.iter().map(Vec::len).sum::<usize>() >= want || Instant::now() > deadline {
                for p in paths {
                    let _ = std::fs::remove_file(p);
                }
                return got;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn parse_line(line: &str) -> (usize, usize) {
        let (t, i) = line.split_once(' ').expect("line is \"<thread> <seq>\"");
        (t.parse().unwrap(), i.parse().unwrap())
    }

    /// B-335・F-302: 複数のスレッドから足した行を、まとめて書く周回・眠りをまたいでも落とさず、スレッドごとの順に書く。
    /// 止めたら書き切って終わる
    #[test]
    // 理由付き allow: 試験のスレッド（イベントループの外）で眠る
    #[allow(clippy::disallowed_methods)]
    fn writer_keeps_every_line_across_batches() {
        let path = temp_log("batches");
        let cell = Current::new();
        start_on(&cell, &file_config(&path));
        const THREADS: usize = 4;
        const LINES: usize = 2000;
        std::thread::scope(|sc| {
            for t in 0..THREADS {
                let cell = &cell;
                sc.spawn(move || {
                    for i in 0..LINES {
                        append_line(cell, |buf| {
                            buf.extend_from_slice(format!("{} {}\n", t, i).as_bytes())
                        });
                        if i % 500 == 0 {
                            // 周回の途中で止まっても（眠りへ戻っても）続きを受ける
                            std::thread::sleep(Duration::from_millis(8));
                        }
                    }
                });
            }
        });
        stop_on(&cell);
        let got = wait_lines(&[&path], THREADS * LINES).remove(0);
        assert_eq!(got.len(), THREADS * LINES);
        let mut next = [0usize; THREADS];
        for line in &got {
            let (t, i) = parse_line(line);
            assert_eq!(i, next[t], "thread {} out of order", t);
            next[t] += 1;
        }
    }

    /// F-302: 足している最中にリロードで送り先を替えても、行を落とさず二重にも書かない。
    /// スレッドごとに、前の送り先へ書いた行が先で、新しい送り先へ書いた行が後
    #[test]
    // 理由付き allow: 試験のスレッド（イベントループの外）で眠る
    #[allow(clippy::disallowed_methods)]
    fn reload_moves_lines_to_the_new_writer_without_loss() {
        let a = temp_log("reload-a");
        let b = temp_log("reload-b");
        let cell = Current::new();
        start_on(&cell, &file_config(&a));
        const THREADS: usize = 3;
        // 全スレッドがこれだけ足してから差し替え、差し替えを見てからもこれだけ足す
        const PHASE: usize = 2000;
        let warmed = AtomicUsize::new(0);
        let reloaded = AtomicBool::new(false);
        let counts: Vec<usize> = std::thread::scope(|sc| {
            let handles: Vec<_> = (0..THREADS)
                .map(|t| {
                    let (cell, warmed, reloaded) = (&cell, &warmed, &reloaded);
                    sc.spawn(move || {
                        let (mut i, mut after) = (0usize, 0usize);
                        while after < PHASE {
                            let seen = reloaded.load(Ordering::Acquire);
                            append_line(cell, |buf| {
                                buf.extend_from_slice(format!("{} {}\n", t, i).as_bytes())
                            });
                            i += 1;
                            if i == PHASE {
                                warmed.fetch_add(1, Ordering::AcqRel);
                            }
                            if seen {
                                after += 1;
                            }
                        }
                        i
                    })
                })
                .collect();
            while warmed.load(Ordering::Acquire) < THREADS {
                std::thread::sleep(Duration::from_millis(1));
            }
            start_on(&cell, &file_config(&b));
            reloaded.store(true, Ordering::Release);
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        stop_on(&cell);
        let total: usize = counts.iter().sum();
        let got = wait_lines(&[&a, &b], total);
        assert_eq!(
            got.iter().map(Vec::len).sum::<usize>(),
            total,
            "a={} b={}",
            got[0].len(),
            got[1].len()
        );
        // 差し替えの前の行は前の送り先へ、差し替えを見た後の行は新しい送り先へ
        assert!(got[0].len() >= THREADS * PHASE, "a={}", got[0].len());
        assert!(got[1].len() >= THREADS * PHASE, "b={}", got[1].len());
        let mut next = [0usize; THREADS];
        for file in &got {
            for line in file {
                let (t, i) = parse_line(line);
                assert_eq!(i, next[t], "thread {} out of order", t);
                next[t] += 1;
            }
        }
        assert_eq!(next.to_vec(), counts);
    }

    /// F-302: ライターが取り出さない間は 1 つの溜めに limit 行までで、超えた行は落とす。取り出せばまた足せる
    #[test]
    fn slot_drops_lines_beyond_limit_until_collected() {
        let sink = Arc::new(Sink::new(3, enabled_config(), render_raw));
        let cell = Current::new();
        cell.replace(Some(Arc::clone(&sink)));
        for i in 0..5 {
            append_line(&cell, |buf| {
                buf.extend_from_slice(format!("{}\n", i).as_bytes())
            });
        }
        let mut batch = Batch::default();
        let mut out = Vec::new();
        assert!(sink.collect(&mut batch, false));
        batch.write_to(&mut out, &sink);
        assert_eq!(out, b"0\n1\n2\n");
        append_line(&cell, |buf| buf.extend_from_slice(b"5\n"));
        out.clear();
        assert!(sink.collect(&mut batch, false));
        batch.write_to(&mut out, &sink);
        assert_eq!(out, b"5\n");
        assert!(!sink.collect(&mut batch, false));
    }

    /// F-302: 足したスレッドが終わっても溜めの行は書き、その後に溜めを外す。止まった送り先には足さない
    #[test]
    fn finished_thread_is_written_then_dropped_and_closed_sink_takes_nothing() {
        let sink = Arc::new(Sink::new(10, enabled_config(), render_raw));
        let cell = Current::new();
        cell.replace(Some(Arc::clone(&sink)));
        std::thread::scope(|sc| {
            sc.spawn(|| append_line(&cell, |buf| buf.extend_from_slice(b"x\n")))
                .join()
                .unwrap();
        });
        let mut batch = Batch::default();
        let mut out = Vec::new();
        assert!(sink.collect(&mut batch, true));
        batch.write_to(&mut out, &sink);
        assert_eq!(out, b"x\n");
        assert!(lock(&sink.registry).slots.is_empty());
        // 閉じた後は登録も足すこともできない
        append_line(&cell, |buf| buf.extend_from_slice(b"y\n"));
        assert!(!sink.collect(&mut batch, false));
        assert!(lock(&sink.registry).slots.is_empty());
    }

    /// F-302: ワーカーが写した記録を、ライターがワーカーで整形していたときと同じ行にする
    /// （JSON・テキスト、項目の絞り込み、エスケープの要る値、UTF-8 でない値、ナノ秒）
    #[test]
    fn records_render_to_the_same_lines_as_direct_formatting() {
        type Build = fn(
            &mut Vec<u8>,
            OffsetDateTime,
            &str,
            &str,
            &str,
            u16,
            u128,
            &str,
            &str,
            u64,
            u64,
            &str,
            &[String],
        );
        let dt = test_dt().replace_nanosecond(123_456_789).unwrap();
        for (format, build) in [
            (AccessLogFormat::Json, build_json_log as Build),
            (AccessLogFormat::Text, build_text_log as Build),
        ] {
            for fields in [Vec::new(), vec!["path".to_string(), "status".to_string()]] {
                let config = AccessLogConfig {
                    enabled: true,
                    format,
                    fields: fields.clone(),
                    ..AccessLogConfig::default()
                };
                let mut rec = Vec::new();
                put_record(
                    &mut rec,
                    404,
                    12,
                    345,
                    7,
                    dt,
                    [
                        b"POST",
                        b"example.com",
                        b"/a\"b\\c",
                        b"curl/8\x1b",
                        b"10.0.0.1",
                        b"127.0.0.1:8080",
                    ],
                );
                put_record(
                    &mut rec,
                    200,
                    0,
                    0,
                    0,
                    test_dt(),
                    [b"\xff", b"h", b"/\xff", b"\xfe", b"::1", b""],
                );
                let mut got = Vec::new();
                render_records(&rec, &config, &mut got);
                let mut want = Vec::new();
                build(
                    &mut want,
                    dt,
                    "POST",
                    "example.com",
                    "/a\"b\\c",
                    404,
                    7,
                    "10.0.0.1",
                    "127.0.0.1:8080",
                    12,
                    345,
                    "curl/8\x1b",
                    &fields,
                );
                // UTF-8 でない値は以前の `log_access` と同じ代わり（method は GET、他は -）
                build(
                    &mut want,
                    test_dt(),
                    "GET",
                    "h",
                    "-",
                    200,
                    0,
                    "::1",
                    "",
                    0,
                    0,
                    "-",
                    &fields,
                );
                assert_eq!(
                    String::from_utf8_lossy(&got),
                    String::from_utf8_lossy(&want)
                );
                assert_eq!(got, want);
            }
        }
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
        // JSON: フィールド順は timestamp → type → 以降
        let mut buf = Vec::new();
        build_json_log(
            &mut buf,
            test_dt(),
            "GET",
            "example.com",
            "/",
            200,
            1,
            "127.0.0.1",
            "",
            0,
            0,
            "-",
            &[],
        );
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.contains("\"type\":\"access\""),
            "json type missing: {}",
            s
        );
        assert!(
            s.starts_with("{\"timestamp\":"),
            "timestamp should be first: {}",
            s
        );
        // type は timestamp の直後（method より前）
        let ts = s.find("\"timestamp\"").unwrap();
        let ty = s.find("\"type\"").unwrap();
        let me = s.find("\"method\"").unwrap();
        assert!(ts < ty && ty < me, "order timestamp<type<method: {}", s);

        // Text: timestamp → type → 以降
        let mut buf = Vec::new();
        build_text_log(
            &mut buf,
            test_dt(),
            "GET",
            "example.com",
            "/",
            200,
            1,
            "127.0.0.1",
            "",
            0,
            0,
            "-",
            &[],
        );
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.starts_with("timestamp="),
            "timestamp should be first: {}",
            s
        );
        let ty = s.find("type=access").unwrap();
        let me = s.find("method=").unwrap();
        assert!(ty < me, "type before method: {}", s);
    }

    #[test]
    fn test_access_log_type_field_survives_field_filter() {
        // fields で type / timestamp を指定しなくても type は常に出力される
        let fields: Vec<String> = vec!["method".to_string()];
        let mut buf = Vec::new();
        build_json_log(
            &mut buf,
            test_dt(),
            "GET",
            "example.com",
            "/",
            200,
            1,
            "127.0.0.1",
            "",
            0,
            0,
            "-",
            &fields,
        );
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.contains("\"type\":\"access\""),
            "type must survive filter: {}",
            s
        );
        // timestamp はフィルタで除外されるため type が先頭になる
        assert!(
            s.starts_with("{\"type\":\"access\""),
            "type first when ts filtered: {}",
            s
        );
        assert!(s.contains("\"method\":\"GET\""));
        assert!(!s.contains("\"host\""));
    }

    #[test]
    // B-233: 要求由来の値の制御文字（ESC 等）・`\`・非 ASCII はテキストのログで \xNN になる
    fn test_text_log_escapes_control_characters() {
        let mut buf = Vec::new();
        build_text_log(
            &mut buf,
            test_dt(),
            "GET",
            "example.com",
            "/a\x1b[2Jb\\c",
            200,
            1,
            "1.2.3.4",
            "",
            0,
            0,
            "UA\nfake=1 é",
            &[],
        );
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("path=/a\\x1b[2Jb\\x5cc"), "{s}");
        assert!(s.contains("user_agent=UA\\x0afake=1 \\xc3\\xa9"), "{s}");
        assert_eq!(s.matches('\n').count(), 1, "{s}");
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
            b"GET",
            b"example.com",
            b"/",
            b"-",
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
