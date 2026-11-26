//! ゲストの CPU の上限と協調的な yield の計り方（`[wasm] metering`。F-319）
//!
//! - `clock`（既定）: wasmtime のエポックで測る。専用スレッド（[`Ticker`]）がエポックを進め（長く走るゲストがいる
//!   間は `yield_interval_us` ごと。下の 3 段）、走っているゲストは締切（次のエポック）に届くと [`on_epoch`] を呼ぶ。そこで走った時間を足し、
//!   `max_execution_time_ms` を超えたら打ち切り（`OutOfFuel`。メトリクスの `reason` は `fuel`）、実時間の上限を超えたら
//!   時間切れ（`Interrupt`。`timeout`）、それ以外はワーカーへ譲る。ゲストのコードに入る検査はエポックの比較（関数の
//!   入口とループの戻り）だけになる。
//! - `fuel`: wasmtime の fuel（命令数）で測る。基本ブロックごとに残量を数えるので、ゲストの実行が遅くなる（aofish の
//!   交互 A/B で、Wasm の経路の 1 要求の CPU が `clock` より 3〜14% 多い）。エポックは 100 ms ごとに進め、実時間の
//!   上限だけに使う（B-173）。
//!
//! `clock` の走った時間は、締切に届くたびにスレッドの CPU 時間（`CLOCK_THREAD_CPUTIME_ID`）の差を足す。他のスレッドに
//! CPU を取られていた時間・ページフォールトでディスクを待った時間（B-336）は入らない（壁時計で数えると、CPU を
//! 取り合う負荷の下で走った時間を最大で数倍に数え、重い `on_configure` が上限を超えた）。区間（呼び出しを始めてから、
//! または譲ったあと再開してから）の最初の検査と、スレッドの CPU 時間を読めない OS（Windows）は、実時間をエポック
//! 1 回の間隔（[`Limits::slow_tick`]）までに切り詰めて足す。読むのは締切に届いたとき（走り続けるゲストでエポックごと）と
//! 再開したときだけで、エポックの間に終わる普通のコールバックは読まない。
//!
//! 既定は `clock`。ただし Pulley インタープリタ（`interpreter = true`、OpenBSD・NetBSD・FreeBSD aarch64 は常に）では
//! `fuel` にする（[`resolve`]）。インタープリタはゲストのコードが 10〜30 倍遅く、時間で測ると fuel で収まっていた
//! モジュールが上限を超える。
//!
//! 専用スレッドがエポックを進める間隔は 3 段（[`Ticker`]）:
//!
//! - 締切に届いた呼び出しが、呼び出しを始めてから [`Limits::slow_tick`] の半分を超えて続いている（長く走るゲストが
//!   いる）間は `yield_interval_us` ごと（[`Limits::tick`]）。譲る間隔と CPU の上限の検査はこの間隔になる。
//! - 呼び出しが来ているだけの間は 10 ms ごと（[`SLOW_TICK`]）。エポックの間に終わる普通の呼び出しは締切に届かない。
//!   長く走るゲストは最初の検査（長くても 10 ms 後）で見つかる。専用スレッドが起きるたびにワーカーが 1 つ CPU を
//!   奪われるので、呼び出しが来ている間ずっと 1 ms ごとに起こすと、aofish の epoll の h1-wasm で 1 要求の CPU が
//!   約 15% 増えた（2 ms で約 10%、5 ms 以上は差が無い）。
//! - Wasm の呼び出しが無い間は 100 ms ごと（呼び出しを始めたワーカーが起こす。サイドカーのように多くのプロセスを
//!   並べる配置でアイドルの起床を増やさない）。

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use wasmtime::{AsContext, AsContextMut, Engine, Store, UpdateDeadline};

use super::context::HostState;
use super::types::{Metering, WasmConfig};

/// `[wasm] metering` の実際の値（未指定なら、Pulley インタープリタでは `fuel`、それ以外は `clock`）
pub(crate) fn resolve(config: &WasmConfig) -> Metering {
    config
        .metering
        .unwrap_or(if super::registry::uses_pulley(config) {
            Metering::Fuel
        } else {
            Metering::Clock
        })
}

/// `fuel` の方式でエポックを進める間隔（B-173。`Window::fuel_ticks` と合わせて実時間の上限になる）
const FUEL_EPOCH_TICK: Duration = Duration::from_millis(100);

/// `clock` の方式で、呼び出しが無い間にエポックを進める間隔（実時間の上限の検査がこれより遅れない）
const IDLE_TICK: Duration = Duration::from_millis(100);

/// `clock` の方式で、呼び出しが来ているが長く走るゲストがいない間にエポックを進める間隔（[`Limits::slow_tick`]。
/// CPU の上限の 4 分の 1 まで）
const SLOW_TICK: Duration = Duration::from_millis(10);

/// `clock` の方式のエポックの間隔の下限（`yield_interval_us` がこれより短くてもこの間隔で進める。専用スレッドの
/// 起床が増えすぎないように）
const MIN_TICK: Duration = Duration::from_micros(100);

/// `clock` の方式の専用スレッドが、呼び出しが来ているかを見直す間隔
const ACTIVE_WINDOW: Duration = Duration::from_millis(100);

/// コールバックの実時間の上限の種類
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Window {
    /// 1 回のコールバック（約 1 秒。他の要求へ譲っていた時間も含む）
    Callback,
    /// 新しいインスタンスの起動時のコールバック（`_start` → root コンテキスト → `on_vm_start` → `on_configure`）と
    /// 起動時の検査（約 60 秒。インスタンスごとに 1 度だけ走る。B-110・B-336）
    Lifecycle,
}

impl Window {
    /// CPU の上限（`max_execution_time_ms`）の倍率。起動時のコールバックは正規表現の組み立て・設定やデータの読み込みで
    /// 普通のコールバックより重く、インスタンスごとに 1 度だけ走る（暴走するモジュールは起動時の検査で拒否する）
    fn budget_scale(self) -> u32 {
        match self {
            Window::Callback => 1,
            Window::Lifecycle => 10,
        }
    }

    /// 実時間の上限（`clock`）
    fn wall(self) -> Duration {
        match self {
            Window::Callback => Duration::from_secs(1),
            Window::Lifecycle => Duration::from_secs(60),
        }
    }

    /// エポックの締切のティック数（`fuel`。[`FUEL_EPOCH_TICK`] ごとに進む）
    pub(crate) fn fuel_ticks(self) -> u64 {
        match self {
            Window::Callback => 10,
            Window::Lifecycle => 600,
        }
    }
}

/// エンジンの計り方と上限（`FilterEngine` が持ち、コピーして使う）
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    metering: Metering,
    /// 1 回のコールバックの CPU の上限（`max_execution_time_ms`）
    cpu: Duration,
    /// 譲る間隔（`yield_interval_us`）
    yield_interval: Duration,
    /// 長く走るゲストがいる間にエポックを進める間隔（`clock`。`yield_interval` と CPU の上限の 4 分の 1 の短い方を
    /// [`MIN_TICK`]〜[`IDLE_TICK`] に収める）
    tick: Duration,
    /// 呼び出しが来ているが長く走るゲストがいない間にエポックを進める間隔（`clock`。[`SLOW_TICK`] と CPU の上限の
    /// 4 分の 1 の短い方、`tick` より短くはしない）。検査の間隔はこれより長くならないので、区間の最初の検査・
    /// スレッドの CPU 時間を読めない OS で実時間を数えるのはこの間隔まで（上限が小さくてもその誤差は上限の 4 分の 1 まで）
    slow_tick: Duration,
    /// `fuel` の燃料（約 1M で 1 ms）
    fuel: u64,
    /// `fuel` で譲る間隔（燃料。約 1,000 で 1 µs。B-175）
    fuel_yield: u64,
}

impl Limits {
    pub(crate) fn new(config: &WasmConfig) -> Self {
        let max_ms = config.defaults.max_execution_time_ms;
        let yield_us = config.defaults.yield_interval_us.max(1);
        let yield_interval = Duration::from_micros(yield_us);
        let quarter = Duration::from_millis(max_ms) / 4;
        let tick = yield_interval.min(quarter).clamp(MIN_TICK, IDLE_TICK);
        Self {
            metering: resolve(config),
            cpu: Duration::from_millis(max_ms),
            yield_interval,
            tick,
            slow_tick: SLOW_TICK.min(quarter).clamp(tick, IDLE_TICK),
            fuel: max_ms.saturating_mul(1_000_000),
            fuel_yield: yield_us.saturating_mul(1_000),
        }
    }

    /// 試験用: fuel の方式で、燃料 `fuel` を入れ、燃料 `fuel_yield` ごとに譲る
    #[cfg(test)]
    pub(crate) fn with_fuel(fuel: u64, fuel_yield: u64) -> Self {
        Self {
            metering: Metering::Fuel,
            cpu: Duration::from_millis(fuel / 1_000_000),
            yield_interval: Duration::from_micros(fuel_yield / 1_000),
            tick: FUEL_EPOCH_TICK,
            slow_tick: FUEL_EPOCH_TICK,
            fuel,
            fuel_yield,
        }
    }

    /// wasmtime の fuel を使うか（エンジンの `consume_fuel`）
    pub(crate) fn uses_fuel(&self) -> bool {
        self.metering == Metering::Fuel
    }

    /// 締切に届いた呼び出しが、始めてから `running` 続いているとき、長く走るゲストか（専用スレッドを速くする）。
    /// エポックの間に終わる普通の呼び出しがたまたま締切をまたいだだけでは速くしない（呼び出しの始まりは実行器の
    /// 粗い時計なので、負荷の下では 1〜2 ms 古いことがある）
    fn is_long(&self, running: Duration) -> bool {
        running >= self.slow_tick / 2
    }

    /// 専用スレッドがエポックを進める間隔: 長く走るゲストがいれば `tick`、呼び出しが来ていれば `slow_tick`、
    /// どちらも無ければ [`IDLE_TICK`]
    fn pace(&self, busy: bool, active: bool) -> Duration {
        if busy {
            self.tick
        } else if active {
            self.slow_tick
        } else {
            IDLE_TICK
        }
    }
}

/// ストアごとの計量の状態（`clock`。`HostState` に置く）
#[derive(Debug)]
pub(crate) struct Meter {
    /// 呼び出しを始めた時刻（実行器の粗い時計。実時間の上限の起点）
    armed_at: Instant,
    /// 走った時間をどこまで足したか
    counted_to: Instant,
    /// `counted_to` のときのスレッドの CPU 時間（区間の最初の検査の前・読めない OS では無し）
    cpu_mark: Option<Duration>,
    /// 呼び出しを始めてから走った時間
    run: Duration,
    /// CPU の上限（`max_execution_time_ms` に [`Window::budget_scale`] を掛けたもの）
    budget: Duration,
    /// 実時間の上限
    wall: Duration,
    /// エポックを進める専用スレッド（[`install`] が置く。上限を入れるたびに眠っていれば起こす）
    ticker: Option<Arc<Ticker>>,
}

impl Meter {
    /// 呼び出しの始まり（CPU と実時間の両方を測り直す）
    fn arm(&mut self, now: Instant, window: Window, limits: &Limits) {
        self.armed_at = now;
        self.counted_to = now;
        self.cpu_mark = None;
        self.run = Duration::ZERO;
        self.budget = limits.cpu.saturating_mul(window.budget_scale());
        self.wall = window.wall();
    }

    /// 実時間の上限だけを測り直す（[`rearm_wall`]）
    fn rearm_wall(&mut self, now: Instant, window: Window) {
        self.armed_at = now;
        self.counted_to = now;
        self.cpu_mark = None;
        self.wall = window.wall();
    }

    /// 再開の記録（[`Resumed`]）の持ち主の印（ストアの中の `HostState` は動かない）
    fn id(&self) -> usize {
        std::ptr::from_ref(self) as usize
    }
}

/// 上限を入れる前（[`arm`] の前）のストアは CPU の上限が 0（fuel の方式で燃料が 0 なのと同じく、最初の検査で止まる）
impl Default for Meter {
    fn default() -> Self {
        let now = crate::runtime::time::coarse_now();
        Self {
            armed_at: now,
            counted_to: now,
            cpu_mark: None,
            run: Duration::ZERO,
            budget: Duration::ZERO,
            wall: Window::Callback.wall(),
            ticker: None,
        }
    }
}

/// 譲ったゲストが再開したときの記録（[`Resume`]）
#[derive(Clone, Copy, Debug)]
struct Resumed {
    /// 再開したゲストの [`Meter::id`]（呼び出しを始めてからゲストが走り出すまでの間に同じスレッドで他のゲストが
    /// 再開しても、その記録を自分のものと取り違えない）
    owner: usize,
    at: Instant,
    /// 再開したときのスレッドの CPU 時間
    cpu: Option<Duration>,
}

thread_local! {
    /// このスレッドで最後に再開したゲストの記録（[`Resume`]）。呼び出しを始めるたびに消す（[`arm`]）
    static RESUMED: Cell<Option<Resumed>> = const { Cell::new(None) };
}

/// このスレッドが使った CPU 時間（`clock` の数え方。読めない OS では `None` で、実時間で数える）
#[cfg(unix)]
fn thread_cpu_time() -> Option<Duration> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` は書き込める `timespec`。CLOCK_THREAD_CPUTIME_ID は対象の全 unix にある（seccomp は clock_gettime を
    // 許可済み・capsicum の capability mode でも使える）
    let r = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    if r != 0 {
        return None;
    }
    Some(Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
}

/// Windows のスレッドの CPU 時間（`GetThreadTimes`）はスケジューラの刻み（約 15.6 ms）でしか進まないので使わない
#[cfg(not(unix))]
fn thread_cpu_time() -> Option<Duration> {
    None
}

/// コールバックを呼ぶ前に上限を入れる（呼び出しごと。`clock` は時刻の読み取りもしない）
pub(crate) fn arm(
    mut ctx: impl AsContextMut<Data = HostState>,
    limits: &Limits,
    window: Window,
) -> anyhow::Result<()> {
    let mut ctx = ctx.as_context_mut();
    match limits.metering {
        Metering::Fuel => {
            ctx.set_fuel(limits.fuel.saturating_mul(u64::from(window.budget_scale())))?;
            ctx.fuel_async_yield_interval(Some(limits.fuel_yield))?;
            ctx.set_epoch_deadline(window.fuel_ticks());
        }
        Metering::Clock => {
            let meter = &mut ctx.data_mut().meter;
            meter.arm(crate::runtime::time::coarse_now(), window, limits);
            // 専用スレッドが眠っていたら起こす（エポックが進まないと締切を検査できない。段を続けて呼ぶ `call_chain` が
            // 段の間に入れ直すときも。F-329）。動いている間は読み取り 1 回
            if let Some(t) = &meter.ticker {
                t.wake();
            }
            RESUMED.with(|c| c.set(None));
            ctx.set_epoch_deadline(1);
        }
    }
    Ok(())
}

/// 外部 I/O（`proxy_http_call`）を待ったあと、同じ要求のコールバックを続ける前に実時間の上限だけを引き直す
/// （B-56。外部 I/O の待ち時間はゲストの CPU ではない。CPU の上限はそのまま引き継ぐ）
pub(crate) fn rearm_wall(
    mut ctx: impl AsContextMut<Data = HostState>,
    limits: &Limits,
    window: Window,
) {
    let mut ctx = ctx.as_context_mut();
    match limits.metering {
        Metering::Fuel => ctx.set_epoch_deadline(window.fuel_ticks()),
        Metering::Clock => {
            let meter = &mut ctx.data_mut().meter;
            meter.rearm_wall(crate::runtime::time::coarse_now(), window);
            if let Some(t) = &meter.ticker {
                t.wake();
            }
            RESUMED.with(|c| c.set(None));
            ctx.set_epoch_deadline(1);
        }
    }
}

/// 作ったストアにエポックの割り込みの扱いを入れる（ストアごとに 1 回。`fuel` は締切でトラップする wasmtime の既定のまま）
pub(crate) fn install(store: &mut Store<HostState>, limits: &Limits, ticker: &Arc<Ticker>) {
    if limits.metering != Metering::Clock {
        return;
    }
    let limits = *limits;
    store.data_mut().meter.ticker = Some(Arc::clone(ticker));
    let ticker = Arc::clone(ticker);
    store.epoch_deadline_callback(move |mut ctx| {
        let meter = &mut ctx.data_mut().meter;
        let now = Instant::now();
        // 長く走るゲストがいる間は、譲る間隔で検査できるように専用スレッドを速く回す
        if limits.is_long(now.saturating_duration_since(meter.armed_at)) {
            ticker.hurry();
        }
        on_epoch(meter, &limits, now)
    });
}

/// `fuel` で、直前のコールバックが使った燃料（`clock` は数えないので `None`）
pub(crate) fn fuel_consumed(
    store: impl AsContext<Data = HostState>,
    limits: &Limits,
) -> Option<u64> {
    if !limits.uses_fuel() {
        return None;
    }
    Some(
        limits
            .fuel
            .saturating_sub(store.as_context().get_fuel().unwrap_or(0)),
    )
}

/// エポックの締切に届いたゲストの扱いを決める（`clock`）
fn on_epoch(m: &mut Meter, limits: &Limits, now: Instant) -> anyhow::Result<UpdateDeadline> {
    let resumed = resumed_by(m);
    match decide(m, limits, now, thread_cpu_time(), resumed) {
        Decision::OutOfCpu => Err(wasmtime::Trap::OutOfFuel.into()),
        Decision::OutOfTime => Err(wasmtime::Trap::Interrupt.into()),
        Decision::Continue => Ok(UpdateDeadline::Continue(1)),
        // 再開したときに締切を引き直す（wasmtime は yield から戻ってから `set_epoch_deadline` する）ので、待つ間に
        // エポックが進んでも、再開した直後にまた譲ることはない
        Decision::Yield => Ok(UpdateDeadline::YieldCustom(
            1,
            Box::pin(Resume {
                owner: m.id(),
                yielded: false,
            }),
        )),
    }
}

/// このスレッドの再開の記録のうち、`m` のゲストのもの
fn resumed_by(m: &Meter) -> Option<Resumed> {
    RESUMED.with(Cell::get).filter(|r| r.owner == m.id())
}

#[derive(Debug, PartialEq, Eq)]
enum Decision {
    OutOfCpu,
    OutOfTime,
    Continue,
    Yield,
}

/// [`on_epoch`] の本体（時刻を引数にして試験できるようにする）。`cpu_now` はスレッドの CPU 時間、`resumed` は
/// このゲストが譲ったあと再開したときの記録
fn decide(
    m: &mut Meter,
    limits: &Limits,
    now: Instant,
    cpu_now: Option<Duration>,
    resumed: Option<Resumed>,
) -> Decision {
    // 今の区間の始まり: 譲ったあとなら再開した時刻、まだ譲っていなければ呼び出しを始めた時刻
    let segment_start = resumed.map_or(m.armed_at, |r| r.at);
    // 最後に数えたあとに再開していれば、再開したところから数える
    let (from, from_cpu) = match resumed {
        Some(r) if r.at > m.counted_to => (r.at, r.cpu),
        _ => (m.counted_to, m.cpu_mark),
    };
    m.run += match (cpu_now, from_cpu) {
        // 他のスレッドに CPU を取られていた時間・ディスクを待った時間（B-336）は入らない
        (Some(cpu_now), Some(from_cpu)) => cpu_now.saturating_sub(from_cpu),
        // 区間の最初の検査と、スレッドの CPU 時間を読めない OS: 検査の間隔（長くても `slow_tick`）を超える遅れは数えない
        _ => now.saturating_duration_since(from).min(limits.slow_tick),
    };
    m.counted_to = now;
    m.cpu_mark = cpu_now;
    if m.run >= m.budget {
        return Decision::OutOfCpu;
    }
    if now.saturating_duration_since(m.armed_at) >= m.wall {
        return Decision::OutOfTime;
    }
    // 区間の長さが平均で `yield_interval` になるように譲る（区間の途中から始まったエポックの半端を均す）
    if now.saturating_duration_since(segment_start) + limits.tick / 2 < limits.yield_interval {
        Decision::Continue
    } else {
        Decision::Yield
    }
}

/// 譲るためのフューチャ（最初の poll で自分を起こして戻り、次の poll で再開の記録を残す）
struct Resume {
    /// 譲ったゲストの [`Meter::id`]
    owner: usize,
    yielded: bool,
}

impl Future for Resume {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.yielded {
            let resumed = Resumed {
                owner: self.owner,
                at: Instant::now(),
                cpu: thread_cpu_time(),
            };
            RESUMED.with(|c| c.set(Some(resumed)));
            Poll::Ready(())
        } else {
            self.yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// エポックを進める専用スレッドを起こすための状態（エンジンごと）。2 つの印は専用スレッドが [`ACTIVE_WINDOW`] ごとに
/// 下ろし、窓の間に立った印で次の窓の間隔を決める（[`Limits::pace`]）
#[derive(Debug)]
pub(crate) struct Ticker {
    /// 呼び出しが来ている（`slow_tick` ごとに進める）。呼び出しを始めたワーカーが立てる
    active: AtomicBool,
    /// 長く走るゲストがいる（`tick` ごとに進める）。締切に届いたゲストが立てる（[`Limits::is_long`]）
    busy: AtomicBool,
    /// 専用スレッド（眠っているときに起こす）
    thread: OnceLock<std::thread::Thread>,
}

impl Ticker {
    /// コールバックを呼ぶ前に呼ぶ（`clock`）。動いている間は読み取り 1 回で、立てるのは窓ごとに 1 回
    #[inline]
    pub(crate) fn wake(&self) {
        self.raise(&self.active);
    }

    /// 長く走るゲストがいる（`clock`。エポックのコールバックから）
    fn hurry(&self) {
        self.raise(&self.busy);
    }

    /// 印を立て、下りていたら専用スレッドを起こす（すぐに間隔を縮める）
    #[inline]
    fn raise(&self, flag: &AtomicBool) {
        if !flag.load(Ordering::Relaxed) && !flag.swap(true, Ordering::AcqRel) {
            if let Some(t) = self.thread.get() {
                t.unpark();
            }
        }
    }
}

/// `engine` のエポックを進める専用スレッドを起こす（B-173）。エンジンが捨てられたら（リロードで作り直したら）
/// 次の周期で終わる。データプレーンからは [`Ticker::wake`] しか呼ばない
pub(crate) fn spawn_ticker(engine: &Engine, limits: &Limits) -> anyhow::Result<Arc<Ticker>> {
    let ticker = Arc::new(Ticker {
        active: AtomicBool::new(false),
        busy: AtomicBool::new(false),
        thread: OnceLock::new(),
    });
    let weak = engine.weak();
    let state = Arc::clone(&ticker);
    let metering = limits.metering;
    let limits = *limits;
    let spawned = std::thread::Builder::new()
        .name("veil-wasm-epoch".into())
        .spawn(move || {
            let advance = || match weak.upgrade() {
                Some(engine) => {
                    engine.increment_epoch();
                    true
                }
                None => false,
            };
            match metering {
                Metering::Fuel => loop {
                    crate::server::cap_safe_sleep(FUEL_EPOCH_TICK);
                    if !advance() {
                        return;
                    }
                },
                Metering::Clock => {
                    let mut last = Instant::now();
                    let mut window_end = last;
                    let mut period = IDLE_TICK;
                    loop {
                        let now = Instant::now();
                        if now >= window_end {
                            // 窓の始め: 前の窓に立った印で間隔を決め直す（間隔を延ばすのはここだけ）
                            window_end = now + ACTIVE_WINDOW;
                            period = limits.pace(
                                state.busy.swap(false, Ordering::AcqRel),
                                state.active.swap(false, Ordering::AcqRel),
                            );
                        } else {
                            // 窓の途中で印が立った（起こされた）: すぐに間隔を縮める
                            period = period.min(limits.pace(
                                state.busy.load(Ordering::Relaxed),
                                state.active.load(Ordering::Relaxed),
                            ));
                        }
                        if now.saturating_duration_since(last) >= period {
                            if !advance() {
                                return;
                            }
                            last = now;
                        }
                        // 起こされたら早く戻る（park は `_umtx_op`・futex で、capsicum の capability mode でも使える）
                        std::thread::park_timeout((last + period).saturating_duration_since(now));
                    }
                }
            }
        });
    match spawned {
        Ok(handle) => {
            let _ = ticker.thread.set(handle.thread().clone());
            Ok(ticker)
        }
        // `clock` はこのスレッドが無いと譲れず上限も効かないので、起動を失敗にする
        Err(e) if metering == Metering::Clock => Err(anyhow::anyhow!(
            "failed to start the Wasm epoch thread: {e}"
        )),
        Err(e) => {
            ftlog::warn!("[wasm] failed to start the epoch ticker thread: {}", e);
            Ok(ticker)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(cpu_ms: u64, yield_us: u64) -> Limits {
        let config = WasmConfig {
            defaults: super::super::types::WasmDefaults {
                max_execution_time_ms: cpu_ms,
                yield_interval_us: yield_us,
                ..Default::default()
            },
            ..Default::default()
        };
        Limits::new(&config)
    }

    /// `at` に 1 回のコールバックとして上限を入れたもの
    fn meter(at: Instant, l: &Limits) -> Meter {
        let mut m = Meter::default();
        m.arm(at, Window::Callback, l);
        m
    }

    /// 再開の記録（スレッドの CPU 時間を読めない OS と同じく、実時間で数える）
    fn resumed(at: Instant) -> Option<Resumed> {
        Some(Resumed {
            owner: 0,
            at,
            cpu: None,
        })
    }

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn the_tick_follows_the_yield_interval_within_bounds() {
        assert_eq!(limits(100, 1_000).tick, MS);
        assert_eq!(limits(100, 10).tick, MIN_TICK);
        assert_eq!(limits(1_000, 10_000_000).tick, IDLE_TICK);
        // CPU の上限が小さいときは上限の 4 分の 1（区間の最初の検査の誤差を抑える）
        assert_eq!(limits(1, 1_000).tick, MS / 4);
        assert_eq!(limits(100, 10_000_000).tick, 25 * MS);
        assert_eq!(limits(100, 0).yield_interval, Duration::from_micros(1));
    }

    // 長く走るゲストがいない間の間隔は 10 ms。CPU の上限の 4 分の 1 を超えず、`tick` より短くしない
    #[test]
    fn the_slow_tick_is_ten_milliseconds_within_bounds() {
        assert_eq!(limits(100, 1_000).slow_tick, SLOW_TICK);
        assert_eq!(limits(100, 10).slow_tick, SLOW_TICK);
        assert_eq!(limits(20, 1_000).slow_tick, 5 * MS);
        assert_eq!(limits(1, 1_000).slow_tick, MS / 4);
        assert_eq!(limits(100, 10_000_000).slow_tick, 25 * MS);
        assert_eq!(limits(1_000, 10_000_000).slow_tick, IDLE_TICK);
    }

    // 専用スレッドの間隔: 長く走るゲストがいれば `tick`、呼び出しが来ていれば `slow_tick`、静かなら 100 ms
    #[test]
    fn the_pace_follows_the_flags() {
        let l = limits(100, 1_000);
        assert_eq!(l.pace(true, true), MS);
        assert_eq!(l.pace(true, false), MS);
        assert_eq!(l.pace(false, true), SLOW_TICK);
        assert_eq!(l.pace(false, false), IDLE_TICK);
    }

    // 締切をまたいだだけの短い呼び出し（粗い時計で数 ms 古く見えるものを含む）では速くしない
    #[test]
    fn only_a_call_running_for_half_a_slow_tick_is_long() {
        let l = limits(100, 1_000);
        assert!(!l.is_long(Duration::ZERO));
        assert!(!l.is_long(2 * MS));
        assert!(!l.is_long(SLOW_TICK / 2 - Duration::from_micros(1)));
        assert!(l.is_long(SLOW_TICK / 2));
        assert!(l.is_long(Duration::from_secs(1)));
    }

    // 区間が `yield_interval` の半分に届かないうちは続け、届いたら譲る
    #[test]
    fn a_short_segment_continues_and_a_long_one_yields() {
        let l = limits(100, 1_000);
        let t0 = Instant::now();
        let mut m = meter(t0, &l);
        assert_eq!(
            decide(&mut m, &l, t0 + MS / 4, None, None),
            Decision::Continue
        );
        assert_eq!(decide(&mut m, &l, t0 + MS, None, None), Decision::Yield);
        assert_eq!(m.run, MS);
        // 再開した区間は再開した時刻から測る
        let at = t0 + 5 * MS;
        assert_eq!(
            decide(&mut m, &l, at + MS / 4, None, resumed(at)),
            Decision::Continue
        );
        assert_eq!(m.run, MS + MS / 4);
    }

    // 走った時間の合計が `max_execution_time_ms` に届いたら打ち切る。譲っていた時間は数えない
    #[test]
    fn the_cpu_budget_counts_only_the_running_segments() {
        let l = limits(10, 1_000);
        let t0 = Instant::now();
        let mut m = meter(t0, &l);
        // 1 ms 走って譲り、9 ms 待ってから再開する（最初の区間は呼び出しの始まりから）
        let mut last = None;
        let mut now = t0;
        for round in 1..=10 {
            now += MS;
            let d = decide(&mut m, &l, now, None, last);
            if round < 10 {
                assert_eq!(d, Decision::Yield, "round {round}");
            } else {
                assert_eq!(d, Decision::OutOfCpu, "round {round}");
            }
            now += 9 * MS;
            last = resumed(now);
        }
        assert_eq!(m.run, 10 * MS);
    }

    // 検査の間隔（`slow_tick`）を超える遅れ（ページフォールトでディスクを待つ等。B-336）は CPU に数えない
    #[test]
    fn a_stall_counts_at_most_one_slow_tick() {
        let l = limits(100, 1_000);
        let t0 = Instant::now();
        let mut m = meter(t0, &l);
        let d = decide(&mut m, &l, t0 + Duration::from_millis(500), None, None);
        assert_eq!(d, Decision::Yield);
        assert_eq!(m.run, SLOW_TICK);
    }

    // 譲っていた時間も含めて実時間の上限を超えたら時間切れ
    #[test]
    fn the_wall_limit_includes_the_time_spent_yielding() {
        let l = limits(100, 1_000);
        let t0 = Instant::now();
        let mut m = meter(t0, &l);
        let at = t0 + Duration::from_millis(1_200);
        assert_eq!(
            decide(&mut m, &l, at + MS, None, resumed(at)),
            Decision::OutOfTime
        );
        let mut m = meter(t0, &l);
        m.wall = Window::Lifecycle.wall();
        assert_eq!(
            decide(&mut m, &l, at + MS, None, resumed(at)),
            Decision::Yield
        );
    }

    // 外部 I/O を待ったあと（B-56）は実時間の上限だけを測り直し、CPU は引き継ぐ
    #[test]
    fn rearming_the_wall_limit_keeps_the_cpu_used() {
        let l = limits(100, 1_000);
        let t0 = Instant::now();
        let mut m = meter(t0, &l);
        assert_eq!(decide(&mut m, &l, t0 + MS, None, None), Decision::Yield);
        let t1 = t0 + Duration::from_secs(5);
        m.rearm_wall(t1, Window::Callback);
        assert_eq!(decide(&mut m, &l, t1 + MS, None, None), Decision::Yield);
        assert_eq!(m.run, 2 * MS);
        m.arm(t1, Window::Callback, &l);
        assert_eq!(m.run, Duration::ZERO);
    }

    // 起動時のコールバックは CPU の上限が 10 倍（実時間の上限は約 60 秒）
    #[test]
    fn startup_callbacks_get_ten_times_the_budget() {
        let l = limits(10, 1_000);
        let t0 = Instant::now();
        let c0 = Duration::from_secs(1);
        let mut m = Meter::default();
        m.arm(t0, Window::Lifecycle, &l);
        assert_eq!(decide(&mut m, &l, t0 + MS, Some(c0), None), Decision::Yield);
        let d = decide(&mut m, &l, t0 + 60 * MS, Some(c0 + 50 * MS), None);
        assert_eq!(d, Decision::Yield);
        let d = decide(&mut m, &l, t0 + 120 * MS, Some(c0 + 99 * MS), None);
        assert_eq!(d, Decision::OutOfCpu);
        // 上限を入れる前のストアは最初の検査で止まる
        let mut fresh = Meter::default();
        assert_eq!(decide(&mut fresh, &l, t0, None, None), Decision::OutOfCpu);
    }

    // スレッドの CPU 時間を読めるときは、検査の間に CPU が進んだぶんだけを数える（CPU を取り合う負荷の下で、他の
    // スレッドが走っていた時間を数えない）。区間の最初の検査だけは CPU の起点が無いので実時間（エポック 1 つ分まで）
    #[test]
    fn the_cpu_clock_does_not_count_the_time_other_threads_ran() {
        let l = limits(10, 1_000);
        let t0 = Instant::now();
        let c0 = Duration::from_secs(3);
        let mut m = meter(t0, &l);
        assert_eq!(
            decide(&mut m, &l, t0 + MS / 4, Some(c0), None),
            Decision::Continue
        );
        assert_eq!(m.run, MS / 4);
        // 実時間で 40 ms のあいだ、このスレッドは 2 ms しか走れなかった（譲る判断は実時間の区間で行う）
        let d = decide(&mut m, &l, t0 + 40 * MS, Some(c0 + 2 * MS), None);
        assert_eq!(d, Decision::Yield);
        assert_eq!(m.run, MS / 4 + 2 * MS);
        // 譲っている間に他の要求が 50 ms 使い、再開してから 1 ms 走った
        let at = t0 + 100 * MS;
        let back = Some(Resumed {
            owner: 0,
            at,
            cpu: Some(c0 + 52 * MS),
        });
        let d = decide(&mut m, &l, at + MS, Some(c0 + 53 * MS), back);
        assert_eq!(d, Decision::Yield);
        assert_eq!(m.run, MS / 4 + 3 * MS);
        // 上限は CPU 時間で効く
        let d = decide(&mut m, &l, at + 2 * MS, Some(c0 + 60 * MS), back);
        assert_eq!(d, Decision::OutOfCpu);
    }

    // 呼び出しを始めてからゲストが走り出すまでの間（インスタンスの枠を待つ等）に同じスレッドで再開した他のゲストの
    // 記録は使わない
    #[test]
    fn a_resume_of_another_guest_is_not_taken_for_ours() {
        let t0 = Instant::now();
        let ours = meter(t0, &limits(100, 1_000));
        let theirs = Resumed {
            owner: ours.id().wrapping_add(1),
            at: t0 + MS,
            cpu: Some(Duration::ZERO),
        };
        RESUMED.with(|c| c.set(Some(theirs)));
        assert!(resumed_by(&ours).is_none());
        let mine = Resumed {
            owner: ours.id(),
            ..theirs
        };
        RESUMED.with(|c| c.set(Some(mine)));
        assert!(resumed_by(&ours).is_some());
        RESUMED.with(|c| c.set(None));
    }

    #[cfg(unix)]
    #[test]
    fn the_thread_cpu_time_advances_with_work() {
        let a = thread_cpu_time().expect("CLOCK_THREAD_CPUTIME_ID");
        let mut x = 0u64;
        let until = Instant::now() + 5 * MS;
        while Instant::now() < until {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        let b = thread_cpu_time().expect("CLOCK_THREAD_CPUTIME_ID");
        assert!(b > a, "{a:?} -> {b:?} ({x})");
    }

    /// `clock`: 終わらないゲストは最初の検査（`slow_tick` 後）で見つかり、そのあと約 1 ms ごとに譲りながら走り、
    /// `max_execution_time_ms` で打ち切られる（`OutOfFuel`。エポックを進める専用スレッドを本当に動かす）
    #[test]
    fn an_endless_guest_yields_and_is_cut_by_the_cpu_budget() {
        use crate::wasm::capabilities::ModuleCapabilities;
        use crate::wasm::context::HttpContext;

        let mut cfg = wasmtime::Config::new();
        cfg.async_support(true);
        cfg.epoch_interruption(true);
        let engine = Engine::new(&cfg).unwrap();
        let module = wasmtime::Module::new(
            &engine,
            r#"(module (func (export "spin") (loop $l (br $l))))"#,
        )
        .unwrap();
        let limits = limits(20, 1_000);
        let ticker = spawn_ticker(&engine, &limits).unwrap();
        let mut store = Store::new(
            &engine,
            HostState::new(HttpContext::new(1, ModuleCapabilities::default())),
        );
        install(&mut store, &limits, &ticker);
        let instance = futures::executor::block_on(
            wasmtime::Linker::new(&engine).instantiate_async(&mut store, &module),
        )
        .unwrap();
        let spin = instance
            .get_typed_func::<(), ()>(&mut store, "spin")
            .unwrap();

        ticker.wake();
        arm(&mut store, &limits, Window::Callback).unwrap();
        let started = Instant::now();
        let mut call = std::pin::pin!(spin.call_async(&mut store, ()));
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut yields = 0u32;
        let r = loop {
            match call.as_mut().poll(&mut cx) {
                Poll::Ready(r) => break r,
                Poll::Pending => yields += 1,
            }
        };
        let e = r.expect_err("an endless guest must be cut");
        assert_eq!(
            e.downcast_ref::<wasmtime::Trap>(),
            Some(&wasmtime::Trap::OutOfFuel),
            "{e:#}"
        );
        assert!(
            yields >= 5,
            "the guest must yield about every millisecond: {yields}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    // F-329: 上限を入れるたびに、眠っている専用スレッドを起こす（段を続けて呼ぶ `call_chain` が段の間に入れ直すとき、
    // 前の段の間に専用スレッドが眠っていても、次の段の締切を `slow_tick` ごとに検査できる）
    #[test]
    fn arming_wakes_the_ticker() {
        use crate::wasm::capabilities::ModuleCapabilities;
        use crate::wasm::context::HttpContext;

        let mut cfg = wasmtime::Config::new();
        cfg.async_support(true);
        cfg.epoch_interruption(true);
        let engine = Engine::new(&cfg).unwrap();
        let limits = limits(20, 1_000);
        let ticker = Arc::new(Ticker {
            active: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            thread: OnceLock::new(),
        });
        let mut store = Store::new(
            &engine,
            HostState::new(HttpContext::new(1, ModuleCapabilities::default())),
        );
        install(&mut store, &limits, &ticker);
        arm(&mut store, &limits, Window::Callback).unwrap();
        assert!(ticker.active.load(Ordering::Relaxed));
        ticker.active.store(false, Ordering::Relaxed);
        rearm_wall(&mut store, &limits, Window::Callback);
        assert!(ticker.active.load(Ordering::Relaxed));
    }

    #[test]
    fn resume_yields_once_and_records_the_resume() {
        RESUMED.with(|c| c.set(None));
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut r = Resume {
            owner: 7,
            yielded: false,
        };
        assert!(Pin::new(&mut r).poll(&mut cx).is_pending());
        assert!(RESUMED.with(Cell::get).is_none());
        let before = Instant::now();
        assert!(Pin::new(&mut r).poll(&mut cx).is_ready());
        let got = RESUMED.with(Cell::get).expect("recorded");
        assert_eq!(got.owner, 7);
        assert!(got.at >= before);
        assert_eq!(got.cpu.is_some(), cfg!(unix));
        RESUMED.with(|c| c.set(None));
    }
}
