//! ゲストの CPU の上限と協調的な yield の計り方（`[wasm] metering`。F-319）
//!
//! - `clock`（既定）: wasmtime のエポックで測る。専用スレッド（[`Ticker`]）がエポックを `yield_interval_us` ごとに
//!   進め、走っているゲストは締切（次のエポック）に届くと [`on_epoch`] を呼ぶ。そこで走った時間を足し、
//!   `max_execution_time_ms` を超えたら打ち切り（`OutOfFuel`。メトリクスの `reason` は `fuel`）、実時間の上限を超えたら
//!   時間切れ（`Interrupt`。`timeout`）、それ以外はワーカーへ譲る。ゲストのコードに入る検査はエポックの比較（関数の
//!   入口とループの戻り）だけになる。
//! - `fuel`: wasmtime の fuel（命令数）で測る。基本ブロックごとに残量を数えるので、ゲストの実行が遅くなる（aofish の
//!   交互 A/B で、Wasm の経路の 1 要求の CPU が `clock` より 3〜14% 多い）。エポックは 100 ms ごとに進め、実時間の
//!   上限だけに使う（B-173）。
//!
//! `clock` の走った時間は、区間（呼び出しを始めてから、または譲ったあと再開してから）の実時間を、エポック 1 つ分
//! （[`Limits::tick`]）までに切り詰めて足す。エポックの検査はゲストのコードの中にしか無いので、ページフォールトで
//! ディスクを待った時間（B-336）・ホスト関数の中の時間で割り込みが遅れても、そのぶんを CPU に数えない（fuel も
//! ゲストの命令しか数えない）。
//!
//! 専用スレッドは、Wasm の呼び出しが無い間は 100 ms ごとにしか起きない（呼び出しを始めたワーカーが起こす。
//! サイドカーのように多くのプロセスを並べる配置でアイドルの起床を増やさない）。

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use wasmtime::{AsContextMut, Engine, Store, UpdateDeadline};

use super::context::HostState;
use super::types::{Metering, WasmConfig};

/// `fuel` の方式でエポックを進める間隔（B-173。`Window::fuel_ticks` と合わせて実時間の上限になる）
const FUEL_EPOCH_TICK: Duration = Duration::from_millis(100);

/// `clock` の方式で、呼び出しが無い間にエポックを進める間隔（実時間の上限の検査がこれより遅れない）
const IDLE_TICK: Duration = Duration::from_millis(100);

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
    /// エポックを進める間隔（`clock`）
    tick: Duration,
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
        Self {
            metering: config.metering,
            cpu: Duration::from_millis(max_ms),
            yield_interval,
            tick: yield_interval.clamp(MIN_TICK, IDLE_TICK),
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
            fuel,
            fuel_yield,
        }
    }

    /// wasmtime の fuel を使うか（エンジンの `consume_fuel`）
    pub(crate) fn uses_fuel(&self) -> bool {
        self.metering == Metering::Fuel
    }
}

/// ストアごとの計量の状態（`clock`。`HostState` に置く）
#[derive(Debug)]
pub(crate) struct Meter {
    /// 呼び出しを始めた時刻（実行器の粗い時計。実時間の上限の起点）
    armed_at: Instant,
    /// 走った時間をどこまで足したか
    counted_to: Instant,
    /// 呼び出しを始めてから走った時間
    run: Duration,
    /// 実時間の上限
    wall: Duration,
}

impl Meter {
    /// 呼び出しの始まり（CPU と実時間の両方を測り直す）
    fn arm(&mut self, now: Instant, window: Window) {
        self.armed_at = now;
        self.counted_to = now;
        self.run = Duration::ZERO;
        self.wall = window.wall();
    }

    /// 実時間の上限だけを測り直す（[`rearm_wall`]）
    fn rearm_wall(&mut self, now: Instant, window: Window) {
        self.armed_at = now;
        self.counted_to = now;
        self.wall = window.wall();
    }
}

impl Default for Meter {
    fn default() -> Self {
        let now = crate::runtime::time::coarse_now();
        Self {
            armed_at: now,
            counted_to: now,
            run: Duration::ZERO,
            wall: Window::Callback.wall(),
        }
    }
}

thread_local! {
    /// このスレッドで最後に再開したゲストの再開の時刻（[`Resume`]）。呼び出しを始めるたびに消す（[`arm`]）ので、
    /// 走っているゲストがここに値を見たら、それは自分が譲ってから再開した時刻
    static RESUMED_AT: Cell<Option<Instant>> = const { Cell::new(None) };
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
            ctx.set_fuel(limits.fuel)?;
            ctx.fuel_async_yield_interval(Some(limits.fuel_yield))?;
            ctx.set_epoch_deadline(window.fuel_ticks());
        }
        Metering::Clock => {
            ctx.data_mut()
                .meter
                .arm(crate::runtime::time::coarse_now(), window);
            RESUMED_AT.with(|c| c.set(None));
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
            ctx.data_mut()
                .meter
                .rearm_wall(crate::runtime::time::coarse_now(), window);
            RESUMED_AT.with(|c| c.set(None));
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
    let ticker = Arc::clone(ticker);
    store.epoch_deadline_callback(move |mut ctx| {
        // 走り続けるゲストがいる間は専用スレッドを速く回し続ける
        ticker.wake();
        on_epoch(&mut ctx.data_mut().meter, &limits, Instant::now())
    });
}

/// `fuel` で、直前のコールバックが使った燃料（`clock` は数えないので `None`）
pub(crate) fn fuel_consumed(store: &Store<HostState>, limits: &Limits) -> Option<u64> {
    if !limits.uses_fuel() {
        return None;
    }
    Some(limits.fuel.saturating_sub(store.get_fuel().unwrap_or(0)))
}

/// エポックの締切に届いたゲストの扱いを決める（`clock`）
fn on_epoch(m: &mut Meter, limits: &Limits, now: Instant) -> anyhow::Result<UpdateDeadline> {
    match decide(m, limits, now, RESUMED_AT.with(Cell::get)) {
        Decision::OutOfCpu => Err(wasmtime::Trap::OutOfFuel.into()),
        Decision::OutOfTime => Err(wasmtime::Trap::Interrupt.into()),
        Decision::Continue => Ok(UpdateDeadline::Continue(1)),
        // 再開したときに締切を引き直す（wasmtime は yield から戻ってから `set_epoch_deadline` する）ので、待つ間に
        // エポックが進んでも、再開した直後にまた譲ることはない
        Decision::Yield => Ok(UpdateDeadline::YieldCustom(1, Box::pin(Resume::default()))),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Decision {
    OutOfCpu,
    OutOfTime,
    Continue,
    Yield,
}

/// [`on_epoch`] の本体（時刻を引数にして試験できるようにする）。`resumed_at` は譲ったあと再開した時刻
fn decide(m: &mut Meter, limits: &Limits, now: Instant, resumed_at: Option<Instant>) -> Decision {
    // 今の区間の始まり: 譲ったあとなら再開した時刻、まだ譲っていなければ呼び出しを始めた時刻
    let segment_start = resumed_at.unwrap_or(m.armed_at);
    let from = segment_start.max(m.counted_to);
    // エポック 1 つ分を超える遅れ（ページフォールト・ホスト関数）は CPU に数えない
    m.run += now.saturating_duration_since(from).min(limits.tick);
    m.counted_to = now;
    if m.run >= limits.cpu {
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

/// 譲るためのフューチャ（最初の poll で自分を起こして戻り、次の poll で再開した時刻を残す）
#[derive(Default)]
struct Resume {
    yielded: bool,
}

impl Future for Resume {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.yielded {
            RESUMED_AT.with(|c| c.set(Some(Instant::now())));
            Poll::Ready(())
        } else {
            self.yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// エポックを進める専用スレッドを起こすための状態（エンジンごと）
pub(crate) struct Ticker {
    /// 呼び出しが来ている（速く進める）。専用スレッドが [`ACTIVE_WINDOW`] ごとに下ろし、呼び出しを始めたワーカーと
    /// 走り続けるゲストが立てる
    active: AtomicBool,
    /// 専用スレッド（眠っているときに起こす）
    thread: OnceLock<std::thread::Thread>,
}

impl Ticker {
    /// コールバックを呼ぶ前に呼ぶ（`clock`）。動いている間は読み取り 1 回で、立てるのは窓ごとに 1 回
    #[inline]
    pub(crate) fn wake(&self) {
        if !self.active.load(Ordering::Relaxed) && !self.active.swap(true, Ordering::AcqRel) {
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
        thread: OnceLock::new(),
    });
    let weak = engine.weak();
    let state = Arc::clone(&ticker);
    let metering = limits.metering;
    let tick = limits.tick;
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
                    let fast = (ACTIVE_WINDOW.as_micros() / tick.as_micros().max(1)).max(1);
                    loop {
                        if state.active.swap(false, Ordering::AcqRel) {
                            // 呼び出しが来ている: 窓の間 `tick` ごとに進める
                            for _ in 0..fast {
                                crate::server::cap_safe_sleep(tick);
                                if !advance() {
                                    return;
                                }
                            }
                        } else {
                            // 静か: 呼び出しを始めたワーカーに起こされるまで（長くても IDLE_TICK）眠る
                            std::thread::park_timeout(IDLE_TICK);
                            if !advance() {
                                return;
                            }
                        }
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

    fn meter(at: Instant) -> Meter {
        Meter {
            armed_at: at,
            counted_to: at,
            run: Duration::ZERO,
            wall: Window::Callback.wall(),
        }
    }

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn the_tick_follows_the_yield_interval_within_bounds() {
        assert_eq!(limits(100, 1_000).tick, MS);
        assert_eq!(limits(100, 10).tick, MIN_TICK);
        assert_eq!(limits(100, 10_000_000).tick, IDLE_TICK);
        assert_eq!(limits(100, 0).yield_interval, Duration::from_micros(1));
    }

    // 区間が `yield_interval` の半分に届かないうちは続け、届いたら譲る
    #[test]
    fn a_short_segment_continues_and_a_long_one_yields() {
        let l = limits(100, 1_000);
        let t0 = Instant::now();
        let mut m = meter(t0);
        assert_eq!(decide(&mut m, &l, t0 + MS / 4, None), Decision::Continue);
        assert_eq!(decide(&mut m, &l, t0 + MS, None), Decision::Yield);
        assert_eq!(m.run, MS);
        // 再開した区間は再開した時刻から測る
        let resumed = t0 + 5 * MS;
        assert_eq!(
            decide(&mut m, &l, resumed + MS / 4, Some(resumed)),
            Decision::Continue
        );
        assert_eq!(m.run, MS + MS / 4);
    }

    // 走った時間の合計が `max_execution_time_ms` に届いたら打ち切る。譲っていた時間は数えない
    #[test]
    fn the_cpu_budget_counts_only_the_running_segments() {
        let l = limits(10, 1_000);
        let t0 = Instant::now();
        let mut m = meter(t0);
        // 1 ms 走って譲り、9 ms 待ってから再開する（最初の区間は呼び出しの始まりから）
        let mut resumed = None;
        let mut now = t0;
        for round in 1..=10 {
            now += MS;
            let d = decide(&mut m, &l, now, resumed);
            if round < 10 {
                assert_eq!(d, Decision::Yield, "round {round}");
            } else {
                assert_eq!(d, Decision::OutOfCpu, "round {round}");
            }
            now += 9 * MS;
            resumed = Some(now);
        }
        assert_eq!(m.run, 10 * MS);
    }

    // エポック 1 つ分を超える遅れ（ページフォールトでディスクを待つ等。B-336）は CPU に数えない
    #[test]
    fn a_stall_counts_at_most_one_tick() {
        let l = limits(100, 1_000);
        let t0 = Instant::now();
        let mut m = meter(t0);
        let d = decide(&mut m, &l, t0 + Duration::from_millis(500), None);
        assert_eq!(d, Decision::Yield);
        assert_eq!(m.run, MS);
    }

    // 譲っていた時間も含めて実時間の上限を超えたら時間切れ
    #[test]
    fn the_wall_limit_includes_the_time_spent_yielding() {
        let l = limits(100, 1_000);
        let t0 = Instant::now();
        let mut m = meter(t0);
        let resumed = t0 + Duration::from_millis(1_200);
        assert_eq!(
            decide(&mut m, &l, resumed + MS, Some(resumed)),
            Decision::OutOfTime
        );
        let mut m = meter(t0);
        m.wall = Window::Lifecycle.wall();
        assert_eq!(
            decide(&mut m, &l, resumed + MS, Some(resumed)),
            Decision::Yield
        );
    }

    // 外部 I/O を待ったあと（B-56）は実時間の上限だけを測り直し、CPU は引き継ぐ
    #[test]
    fn rearming_the_wall_limit_keeps_the_cpu_used() {
        let l = limits(100, 1_000);
        let t0 = Instant::now();
        let mut m = meter(t0);
        assert_eq!(decide(&mut m, &l, t0 + MS, None), Decision::Yield);
        let t1 = t0 + Duration::from_secs(5);
        m.rearm_wall(t1, Window::Callback);
        assert_eq!(decide(&mut m, &l, t1 + MS, None), Decision::Yield);
        assert_eq!(m.run, 2 * MS);
        m.arm(t1, Window::Callback);
        assert_eq!(m.run, Duration::ZERO);
    }

    /// `clock`: 終わらないゲストは約 1 ms ごとに譲りながら走り、`max_execution_time_ms` で打ち切られる（`OutOfFuel`。
    /// エポックを進める専用スレッドを本当に動かす）
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

    #[test]
    fn resume_yields_once_and_records_the_resume_time() {
        RESUMED_AT.with(|c| c.set(None));
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut r = Resume::default();
        assert!(Pin::new(&mut r).poll(&mut cx).is_pending());
        assert!(RESUMED_AT.with(Cell::get).is_none());
        let before = Instant::now();
        assert!(Pin::new(&mut r).poll(&mut cx).is_ready());
        assert!(RESUMED_AT.with(Cell::get).is_some_and(|t| t >= before));
    }
}
