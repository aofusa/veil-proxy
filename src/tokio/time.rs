// F-198
//! Compatible with `tokio::time`.
//!
//! One timer thread per process wakes timers in deadline order (a `Sleep` behaves the same on whichever
//! thread created it, and can be used outside a runtime). The waker it calls is wrapped to be thread-safe
//! (`waker::WakerSlot`), so wake-ups reach veil tasks through the owning thread's entry point.
//! `pause` and `advance` (tokio's test-util) are supported.

use super::waker::WakerSlot;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::task::{Context, Poll};

pub use std::time::Duration;

// ===================== Instant =====================

/// Equivalent of `tokio::time::Instant`: a thin wrapper around `std::time::Instant`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant(std::time::Instant);

impl Instant {
    /// Now (the time of the paused clock while the clock is paused).
    pub fn now() -> Instant {
        Instant(clock_now())
    }
    pub fn from_std(std: std::time::Instant) -> Instant {
        Instant(std)
    }
    pub fn into_std(self) -> std::time::Instant {
        self.0
    }
    pub fn duration_since(&self, earlier: Instant) -> Duration {
        self.0.saturating_duration_since(earlier.0)
    }
    pub fn checked_duration_since(&self, earlier: Instant) -> Option<Duration> {
        self.0.checked_duration_since(earlier.0)
    }
    pub fn saturating_duration_since(&self, earlier: Instant) -> Duration {
        self.0.saturating_duration_since(earlier.0)
    }
    pub fn elapsed(&self) -> Duration {
        clock_now().saturating_duration_since(self.0)
    }
    pub fn checked_add(&self, d: Duration) -> Option<Instant> {
        self.0.checked_add(d).map(Instant)
    }
    pub fn checked_sub(&self, d: Duration) -> Option<Instant> {
        self.0.checked_sub(d).map(Instant)
    }
    /// A distant future (used as "no deadline" for `Sleep`).
    pub(crate) fn far_future() -> Instant {
        Instant::now() + Duration::from_secs(86400 * 365 * 30)
    }
}

impl From<std::time::Instant> for Instant {
    fn from(i: std::time::Instant) -> Instant {
        Instant(i)
    }
}

impl From<Instant> for std::time::Instant {
    fn from(i: Instant) -> std::time::Instant {
        i.0
    }
}

impl std::ops::Add<Duration> for Instant {
    type Output = Instant;
    fn add(self, d: Duration) -> Instant {
        Instant(self.0 + d)
    }
}

impl std::ops::AddAssign<Duration> for Instant {
    fn add_assign(&mut self, d: Duration) {
        self.0 += d;
    }
}

impl std::ops::Sub<Duration> for Instant {
    type Output = Instant;
    fn sub(self, d: Duration) -> Instant {
        Instant(self.0 - d)
    }
}

impl std::ops::SubAssign<Duration> for Instant {
    fn sub_assign(&mut self, d: Duration) {
        self.0 -= d;
    }
}

impl std::ops::Sub<Instant> for Instant {
    type Output = Duration;
    fn sub(self, other: Instant) -> Duration {
        self.0.saturating_duration_since(other.0)
    }
}

// ===================== 時計（tokio の test-util の pause / advance） =====================

/// The clock. While running it is `virt_at + (real time - real_at)`; while paused it is `virt_at`.
struct Clock {
    paused: bool,
    virt_at: std::time::Instant,
    real_at: std::time::Instant,
}

fn clock() -> &'static Mutex<Clock> {
    static C: OnceLock<Mutex<Clock>> = OnceLock::new();
    C.get_or_init(|| {
        let now = std::time::Instant::now();
        Mutex::new(Clock {
            paused: false,
            virt_at: now,
            real_at: now,
        })
    })
}

/// Whether the clock has ever been touched (pause / advance). If not, real time is used as it is (fast path).
static CLOCK_TOUCHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Number of times tasks ran (used to decide automatic advancing while paused; it does not advance while tasks are running).
static ACTIVITY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Called on every task poll (counted only while the clock is paused).
pub(crate) fn note_activity() {
    if CLOCK_TOUCHED.load(std::sync::atomic::Ordering::Relaxed) {
        ACTIVITY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

fn clock_now() -> std::time::Instant {
    if !CLOCK_TOUCHED.load(std::sync::atomic::Ordering::Acquire) {
        return std::time::Instant::now();
    }
    let c = clock().lock().unwrap_or_else(|e| e.into_inner());
    if c.paused {
        c.virt_at
    } else {
        c.virt_at + std::time::Instant::now().saturating_duration_since(c.real_at)
    }
}

fn is_paused() -> bool {
    CLOCK_TOUCHED.load(std::sync::atomic::Ordering::Acquire)
        && clock().lock().unwrap_or_else(|e| e.into_inner()).paused
}

/// Converts a clock (virtual) time to real time (for the timer thread's wait).
fn to_real(deadline: std::time::Instant) -> std::time::Instant {
    if !CLOCK_TOUCHED.load(std::sync::atomic::Ordering::Acquire) {
        return deadline;
    }
    let c = clock().lock().unwrap_or_else(|e| e.into_inner());
    c.real_at + deadline.saturating_duration_since(c.virt_at)
}

/// Pauses the clock (for tests). While paused, time moves forward only through `advance`, or to the next timer when no task can run.
#[track_caller]
pub fn pause() {
    CLOCK_TOUCHED.store(true, std::sync::atomic::Ordering::Release);
    let mut c = clock().lock().unwrap_or_else(|e| e.into_inner());
    if c.paused {
        drop(c);
        panic!("time is already frozen");
    }
    let now = std::time::Instant::now();
    let elapsed = now.saturating_duration_since(c.real_at);
    c.virt_at += elapsed;
    c.real_at = now;
    c.paused = true;
    drop(c);
    timer().cv.notify_one();
}

/// Pauses the clock if it is not already paused (for creating several `start_paused` runtimes).
pub(crate) fn pause_if_running() {
    if !is_paused() {
        pause();
    }
}

/// Resumes the paused clock.
#[track_caller]
pub fn resume() {
    let mut c = clock().lock().unwrap_or_else(|e| e.into_inner());
    if !CLOCK_TOUCHED.load(std::sync::atomic::Ordering::Acquire) || !c.paused {
        drop(c);
        panic!("time is not frozen");
    }
    c.real_at = std::time::Instant::now();
    c.paused = false;
    drop(c);
    timer().cv.notify_one();
}

/// Advances the paused clock by `duration`, wakes the timers that became due, and then yields once.
pub async fn advance(duration: Duration) {
    {
        let mut c = clock().lock().unwrap_or_else(|e| e.into_inner());
        if !CLOCK_TOUCHED.load(std::sync::atomic::Ordering::Acquire) || !c.paused {
            panic!("time is not frozen");
        }
        c.virt_at += duration;
    }
    fire_due(clock_now());
    crate::tokio::task::yield_now().await;
}

/// Clock settings (panics when timers are disabled in the runtime context).
#[track_caller]
fn check_time_enabled() {
    if !crate::runtime::has_driver() && crate::tokio::runtime::Handle::try_current().is_err() {
        panic!(
            "there is no reactor running, must be called from the context of a Tokio 1.x runtime"
        );
    }
    if !crate::tokio::runtime::time_enabled_in_context() {
        panic!(
            "A Tokio 1.x context was found, but timers are disabled. Call `enable_time` on the runtime builder to enable timers."
        );
    }
}

// ===================== タイマースレッド =====================

struct Entry {
    fired: std::sync::atomic::AtomicBool,
    waker: Mutex<WakerSlot>,
}

struct HeapItem {
    deadline: std::time::Instant,
    seq: u64,
    entry: Weak<Entry>,
}

impl PartialEq for HeapItem {
    fn eq(&self, o: &Self) -> bool {
        (self.deadline, self.seq) == (o.deadline, o.seq)
    }
}
impl Eq for HeapItem {}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for HeapItem {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        (self.deadline, self.seq).cmp(&(o.deadline, o.seq))
    }
}

struct Wheel {
    heap: BinaryHeap<Reverse<HeapItem>>,
    seq: u64,
}

struct TimerThread {
    wheel: Mutex<Wheel>,
    cv: Condvar,
}

fn timer() -> &'static TimerThread {
    static T: OnceLock<&'static TimerThread> = OnceLock::new();
    T.get_or_init(|| {
        let t: &'static TimerThread = Box::leak(Box::new(TimerThread {
            wheel: Mutex::new(Wheel {
                heap: BinaryHeap::new(),
                seq: 0,
            }),
            cv: Condvar::new(),
        }));
        std::thread::Builder::new()
            .name("veil-tokio-timer".into())
            .spawn(move || run_timer(t))
            .expect("failed to start the timer thread");
        t
    })
}

/// Wakes the timers that are due (`now` is the clock's time).
fn fire_due(now: std::time::Instant) {
    let t = timer();
    let due: Vec<Arc<Entry>> = {
        let mut wheel = t.wheel.lock().unwrap();
        let mut due = Vec::new();
        while let Some(Reverse(top)) = wheel.heap.peek() {
            if top.deadline > now {
                break;
            }
            let Reverse(item) = wheel.heap.pop().unwrap();
            if let Some(e) = item.entry.upgrade() {
                due.push(e);
            }
        }
        due
    };
    for e in due {
        e.fired.store(true, std::sync::atomic::Ordering::Release);
        let w = e.waker.lock().unwrap().take();
        if let Some(w) = w {
            w.wake();
        }
    }
}

// 理由付き allow: タイマー専用スレッドの待ち（Condvar）。データプレーンのワーカーではない。
#[allow(clippy::disallowed_methods)]
fn run_timer(t: &'static TimerThread) {
    // 時計を止めているとき、この間どのタスクも動かなければ次のタイマーまで進める（tokio の自動前進）
    const QUIET: Duration = Duration::from_millis(2);
    loop {
        fire_due(clock_now());
        let wheel = t.wheel.lock().unwrap();
        let earliest = wheel.heap.peek().map(|Reverse(i)| i.deadline);
        if is_paused() {
            let Some(next) = earliest else {
                drop(t.cv.wait_timeout(wheel, QUIET).unwrap());
                continue;
            };
            let before = ACTIVITY.load(std::sync::atomic::Ordering::Relaxed);
            drop(t.cv.wait_timeout(wheel, QUIET).unwrap());
            if ACTIVITY.load(std::sync::atomic::Ordering::Relaxed) == before && is_paused() {
                let mut c = clock().lock().unwrap_or_else(|e| e.into_inner());
                if c.paused && next > c.virt_at {
                    c.virt_at = next;
                }
            }
            continue;
        }
        match earliest {
            Some(d) => {
                let wait = to_real(d).saturating_duration_since(std::time::Instant::now());
                drop(t.cv.wait_timeout(wheel, wait).unwrap());
            }
            None => drop(t.cv.wait(wheel).unwrap()),
        }
    }
}

fn register(deadline: std::time::Instant) -> Arc<Entry> {
    let e = Arc::new(Entry {
        fired: std::sync::atomic::AtomicBool::new(false),
        waker: Mutex::new(WakerSlot::default()),
    });
    let t = timer();
    let mut w = t.wheel.lock().unwrap();
    w.seq += 1;
    let seq = w.seq;
    let earliest = w.heap.peek().map(|Reverse(i)| i.deadline);
    w.heap.push(Reverse(HeapItem {
        deadline,
        seq,
        entry: Arc::downgrade(&e),
    }));
    // 取り消された（Weak が切れた）項目が溜まりすぎたら掃除する
    if w.heap.len() > 1024 && seq.is_multiple_of(1024) {
        let items: Vec<_> = std::mem::take(&mut w.heap).into_vec();
        w.heap = items
            .into_iter()
            .filter(|Reverse(i)| i.entry.strong_count() > 0)
            .collect();
    }
    drop(w);
    if earliest.is_none_or(|d| deadline < d) {
        t.cv.notify_one();
    }
    e
}

// ===================== Sleep =====================

/// `tokio::time::Sleep`。
#[derive(Debug)]
pub struct Sleep {
    deadline: Instant,
    entry: Option<Arc<EntryHandle>>,
    elapsed: bool,
}

/// The registration held by a `Sleep` (dropping it invalidates the entry on the timer thread).
struct EntryHandle(Arc<Entry>);

impl std::fmt::Debug for EntryHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TimerEntry")
    }
}

impl Sleep {
    fn new(deadline: Instant) -> Sleep {
        Sleep {
            deadline,
            entry: None,
            elapsed: false,
        }
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Whether the deadline has been observed as reached by a poll (as in tokio, `false` if it was only created and never polled).
    pub fn is_elapsed(&self) -> bool {
        self.elapsed
            || self
                .entry
                .as_ref()
                .is_some_and(|e| e.0.fired.load(std::sync::atomic::Ordering::Acquire))
    }

    /// Changes the deadline (call it through `Pin<&mut Sleep>`).
    pub fn reset(self: Pin<&mut Self>, deadline: Instant) {
        let this = self.get_mut();
        this.deadline = deadline;
        this.entry = None;
        this.elapsed = false;
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let coop = std::task::ready!(crate::tokio::task::coop::poll_proceed(cx));
        let r = self.get_mut().poll_inner(cx);
        if r.is_ready() {
            coop.made_progress();
        }
        r
    }
}

impl Sleep {
    fn poll_inner(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let this = self;
        if Instant::now() >= this.deadline {
            this.entry = None;
            this.elapsed = true;
            return Poll::Ready(());
        }
        let entry = this
            .entry
            .get_or_insert_with(|| Arc::new(EntryHandle(register(this.deadline.0))));
        entry.0.waker.lock().unwrap().register(cx.waker());
        if entry.0.fired.load(std::sync::atomic::Ordering::Acquire) {
            this.entry = None;
            this.elapsed = true;
            return Poll::Ready(());
        }
        Poll::Pending
    }
}

/// Completes after `duration`.
#[track_caller]
pub fn sleep(duration: Duration) -> Sleep {
    check_time_enabled();
    Sleep::new(
        Instant::now()
            .checked_add(duration)
            .unwrap_or_else(Instant::far_future),
    )
}

/// Completes at `deadline`.
#[track_caller]
pub fn sleep_until(deadline: Instant) -> Sleep {
    check_time_enabled();
    Sleep::new(deadline)
}

// ===================== timeout =====================

pub mod error {
    /// The deadline elapsed (`tokio::time::error::Elapsed`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Elapsed(pub(crate) ());

    impl std::fmt::Display for Elapsed {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("deadline has elapsed")
        }
    }

    impl std::error::Error for Elapsed {}

    impl From<Elapsed> for std::io::Error {
        fn from(_: Elapsed) -> std::io::Error {
            std::io::ErrorKind::TimedOut.into()
        }
    }

    /// Timer error (for compatibility; this layer never produces it).
    #[derive(Debug)]
    pub struct Error(());

    impl std::fmt::Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("timer error")
        }
    }

    impl std::error::Error for Error {}
}

pub use error::Elapsed;

/// `tokio::time::Timeout`。
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Timeout<T> {
    value: T,
    delay: Sleep,
}

impl<T> Timeout<T> {
    pub fn get_ref(&self) -> &T {
        &self.value
    }
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.value
    }
    pub fn into_inner(self) -> T {
        self.value
    }
}

impl<T: Future> Future for Timeout<T> {
    type Output = Result<T::Output, Elapsed>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `value` は動かさない（構造的ピン留め）。`delay` は Unpin
        let this = unsafe { self.get_unchecked_mut() };
        let value = unsafe { Pin::new_unchecked(&mut this.value) };
        if let Poll::Ready(v) = value.poll(cx) {
            return Poll::Ready(Ok(v));
        }
        // 中の Future が予算を使い切っていても期限は見る（tokio と同じ）
        match crate::tokio::task::coop::with_unconstrained(|| Pin::new(&mut this.delay).poll(cx)) {
            Poll::Ready(()) => Poll::Ready(Err(Elapsed(()))),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Returns `Err(Elapsed)` if `duration` passes first.
#[track_caller]
pub fn timeout<F: std::future::IntoFuture>(
    duration: Duration,
    future: F,
) -> Timeout<F::IntoFuture> {
    Timeout {
        value: future.into_future(),
        delay: sleep(duration),
    }
}

/// Returns `Err(Elapsed)` if `deadline` passes first.
#[track_caller]
pub fn timeout_at<F: std::future::IntoFuture>(
    deadline: Instant,
    future: F,
) -> Timeout<F::IntoFuture> {
    Timeout {
        value: future.into_future(),
        delay: sleep_until(deadline),
    }
}

// ===================== Interval =====================

/// What to do when a scheduled tick is missed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MissedTickBehavior {
    /// Keeps returning ticks until it catches up (the default).
    #[default]
    Burst,
    /// Restarts the schedule so ticks come every `period` from now.
    Delay,
    /// Skips ahead to the next scheduled tick.
    Skip,
}

/// `tokio::time::Interval`。
#[derive(Debug)]
pub struct Interval {
    delay: Sleep,
    period: Duration,
    missed_tick_behavior: MissedTickBehavior,
}

/// First tick immediately, then every `period`.
#[track_caller]
pub fn interval(period: Duration) -> Interval {
    assert!(period > Duration::ZERO, "`period` must be non-zero.");
    interval_at(Instant::now(), period)
}

/// First tick at `start`, then every `period`.
#[track_caller]
pub fn interval_at(start: Instant, period: Duration) -> Interval {
    assert!(period > Duration::ZERO, "`period` must be non-zero.");
    Interval {
        delay: sleep_until(start),
        period,
        missed_tick_behavior: MissedTickBehavior::default(),
    }
}

impl Interval {
    pub async fn tick(&mut self) -> Instant {
        std::future::poll_fn(|cx| self.poll_tick(cx)).await
    }

    pub fn poll_tick(&mut self, cx: &mut Context<'_>) -> Poll<Instant> {
        if Pin::new(&mut self.delay).poll(cx).is_pending() {
            return Poll::Pending;
        }
        let timeout = self.delay.deadline();
        let now = Instant::now();
        let add = |a: Instant, d: Duration| a.checked_add(d).unwrap_or_else(Instant::far_future);
        let next = if now > add(timeout, Duration::from_millis(5)) {
            match self.missed_tick_behavior {
                MissedTickBehavior::Burst => add(timeout, self.period),
                MissedTickBehavior::Delay => add(now, self.period),
                MissedTickBehavior::Skip => {
                    let elapsed = now.duration_since(timeout).as_nanos();
                    let p = self.period.as_nanos().max(1);
                    let skip = (elapsed / p + 1) as u32;
                    add(timeout, self.period.saturating_mul(skip))
                }
            }
        } else {
            add(timeout, self.period)
        };
        Pin::new(&mut self.delay).reset(next);
        Poll::Ready(timeout)
    }

    pub fn reset(&mut self) {
        Pin::new(&mut self.delay).reset(Instant::now() + self.period);
    }

    pub fn reset_immediately(&mut self) {
        Pin::new(&mut self.delay).reset(Instant::now());
    }

    pub fn reset_after(&mut self, after: Duration) {
        Pin::new(&mut self.delay).reset(Instant::now() + after);
    }

    pub fn reset_at(&mut self, deadline: Instant) {
        Pin::new(&mut self.delay).reset(deadline);
    }

    pub fn missed_tick_behavior(&self) -> MissedTickBehavior {
        self.missed_tick_behavior
    }

    pub fn set_missed_tick_behavior(&mut self, b: MissedTickBehavior) {
        self.missed_tick_behavior = b;
    }

    pub fn period(&self) -> Duration {
        self.period
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> crate::tokio::runtime::Runtime {
        crate::tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn sleep_and_timeout() {
        let rt = rt();
        let t = std::time::Instant::now();
        rt.block_on(async { sleep(Duration::from_millis(30)).await });
        assert!(t.elapsed() >= Duration::from_millis(30));
        let r = rt.block_on(async {
            timeout(Duration::from_millis(20), sleep(Duration::from_secs(5))).await
        });
        assert!(r.is_err());
        let r = rt.block_on(async { timeout(Duration::from_secs(5), async { 7 }).await });
        assert_eq!(r.unwrap(), 7);
    }

    #[test]
    fn interval_ticks() {
        let rt = rt();
        let t = std::time::Instant::now();
        rt.block_on(async {
            let mut i = interval(Duration::from_millis(10));
            for _ in 0..4 {
                i.tick().await;
            }
        });
        assert!(t.elapsed() >= Duration::from_millis(30));
    }
}
