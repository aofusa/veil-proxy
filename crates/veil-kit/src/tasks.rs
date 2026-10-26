// F-267: バックグラウンドタスク。要求に紐づくもの（BackgroundTasks）は Response::after_send で応答を送った後に
// ワーカーで spawn する。アプリに紐づくもの（spawn・every・cron）は App::on_worker_start で 1 個のワーカー
// （または全ワーカー）に置き、graceful shutdown の開始（ShutdownSignal）で止める。
//! Background tasks.
//!
//! **After the response** (FastAPI's `BackgroundTasks`): take [`BackgroundTasks`] as a handler parameter and add
//! work; it runs after the response was sent (HTTP/1.1: written; HTTP/2 and HTTP/3: the last frame queued).
//!
//! ```no_run
//! use veil::prelude::*;
//! use veil_kit::tasks::{BackgroundTasks, Tasks};
//!
//! #[veil::route(path = "/signup", method = "POST")]
//! async fn signup(tasks: BackgroundTasks) -> &'static str {
//!     tasks.add(async { /* send the welcome mail */ });
//!     tasks.add_blocking(|| { /* blocking work on the handler pool */ });
//!     "ok"
//! }
//!
//! App::new()
//!     .with(
//!         Tasks::new()
//!             .every(std::time::Duration::from_secs(60), || async { /* refresh a cache */ })
//!             .spawn(|| async { /* a long-running loop */ }),
//!     )
//!     .run()
//!     .unwrap();
//! ```
//!
//! **App tasks** run on one worker thread (the first one that starts) unless [`Tasks::on_all_workers`] is set,
//! and stop when graceful shutdown starts. A panic in a task is caught and logged. Heavy CPU or blocking work
//! belongs in `add_blocking` / `veil::runtime::offload::offload_handler`, not in the async body.
//!
//! [`Tasks`] must be installed with `App::with` for [`BackgroundTasks`] to run: it adds middleware (on `/` by
//! default, [`Tasks::background_on`] narrows it) that costs one small allocation per request on those routes.

use futures::future::{FutureExt, LocalBoxFuture};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use veil::api::{App, Extensions, FromRequest, HandlerFn, Plugin, Request, Response};

type Job = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct TaskList(Mutex<Vec<Job>>);

/// Work to run after the response is sent (an extractor; needs [`Tasks`] installed).
#[derive(Clone)]
pub struct BackgroundTasks(Arc<TaskList>);

impl std::fmt::Debug for BackgroundTasks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BackgroundTasks")
    }
}

static INSTALLED: AtomicBool = AtomicBool::new(false);

impl FromRequest for BackgroundTasks {
    fn from_request(req: &Request) -> Result<Self, Response> {
        if !INSTALLED.load(Ordering::Relaxed) {
            ftlog::error!("BackgroundTasks needs veil_kit::tasks::Tasks installed with App::with");
            return Err(Response::error(500, "Internal Server Error"));
        }
        Ok(BackgroundTasks(
            req.extensions().get_or_insert_with(TaskList::default),
        ))
    }
}

impl BackgroundTasks {
    /// Runs `fut` on the worker after the response is sent.
    pub fn add(&self, fut: impl Future<Output = ()> + Send + 'static) {
        self.0 .0.lock().unwrap().push(Box::new(move || {
            veil::runtime::spawn(guarded("background task", fut));
        }));
    }

    /// Runs `f` on the handler pool after the response is sent.
    pub fn add_blocking(&self, f: impl FnOnce() + Send + 'static) {
        self.0 .0.lock().unwrap().push(Box::new(move || {
            veil::runtime::offload::spawn_handler(move || {
                if std::panic::catch_unwind(AssertUnwindSafe(f)).is_err() {
                    ftlog::error!("a blocking background task panicked");
                }
            });
        }));
    }
}

async fn guarded(what: &'static str, fut: impl Future<Output = ()>) {
    if AssertUnwindSafe(fut).catch_unwind().await.is_err() {
        ftlog::error!("{} panicked", what);
    }
}

type MakeFut = Arc<dyn Fn() -> LocalBoxFuture<'static, ()> + Send + Sync>;

enum When {
    Once,
    Every(Duration),
    #[cfg(feature = "cron")]
    Cron(Arc<cron::Schedule>),
}

struct AppTask {
    name: String,
    when: When,
    make: MakeFut,
    all_workers: bool,
    claimed: Arc<AtomicBool>,
}

/// App tasks and the [`BackgroundTasks`] middleware (a [`Plugin`]).
pub struct Tasks {
    tasks: Vec<AppTask>,
    background_prefix: String,
    all_workers_next: bool,
}

impl Default for Tasks {
    fn default() -> Self {
        Tasks::new()
    }
}

impl Tasks {
    /// No app tasks; [`BackgroundTasks`] enabled for all routes.
    pub fn new() -> Tasks {
        Tasks {
            tasks: Vec::new(),
            background_prefix: "/".into(),
            all_workers_next: false,
        }
    }

    /// Enables [`BackgroundTasks`] only for routes under `prefix`.
    pub fn background_on(mut self, prefix: &str) -> Tasks {
        self.background_prefix = prefix.to_string();
        self
    }

    /// The next task added runs on every worker thread (of every listener) instead of one.
    pub fn on_all_workers(mut self) -> Tasks {
        self.all_workers_next = true;
        self
    }

    fn push<F, Fut>(mut self, when: When, f: F) -> Tasks
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let name = format!("task #{}", self.tasks.len() + 1);
        self.tasks.push(AppTask {
            name,
            when,
            make: Arc::new(move || f().boxed_local()),
            all_workers: std::mem::take(&mut self.all_workers_next),
            claimed: Arc::new(AtomicBool::new(false)),
        });
        self
    }

    /// Runs `f()` once after startup (a long-running loop should watch `veil::api::ShutdownSignal`).
    pub fn spawn<F, Fut>(self, f: F) -> Tasks
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        self.push(When::Once, f)
    }

    /// Runs `f()` every `period` (the next run starts `period` after the previous one finished, so runs never
    /// overlap; the first run is after a random delay of up to 10% of `period`).
    pub fn every<F, Fut>(self, period: Duration, f: F) -> Tasks
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        self.push(When::Every(period), f)
    }

    /// Runs `f()` on a cron schedule in UTC: `"sec min hour day month weekday"` (six fields) or the usual five
    /// without seconds. Fields take `*`, numbers, `a-b`, lists and `/step`; weekday 0 or 7 is Sunday.
    #[cfg(feature = "cron")]
    pub fn cron<F, Fut>(self, expr: &str, f: F) -> Result<Tasks, String>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let s = cron::Schedule::parse(expr)?;
        Ok(self.push(When::Cron(Arc::new(s)), f))
    }
}

fn jitter(max: Duration) -> Duration {
    let mut b = [0u8; 8];
    crate::crypto::random(&mut b);
    let r = u64::from_le_bytes(b);
    let ms = max.as_millis() as u64;
    if ms == 0 {
        Duration::ZERO
    } else {
        Duration::from_millis(r % ms)
    }
}

/// Waits `d`, or until shutdown starts (`false`).
async fn sleep_or_stop(d: Duration) -> bool {
    if veil::api::upgrade::is_shutting_down() {
        return false;
    }
    let stop = veil::api::ShutdownSignal::new().fuse();
    let wait = veil::runtime::sleep(d).fuse();
    futures::pin_mut!(stop, wait);
    futures::select! {
        _ = stop => false,
        _ = wait => !veil::api::upgrade::is_shutting_down(),
    }
}

async fn run_task(name: String, when: When, make: MakeFut) {
    match when {
        When::Once => guarded_named(&name, make()).await,
        When::Every(period) => {
            if !sleep_or_stop(jitter(period / 10)).await {
                return;
            }
            loop {
                guarded_named(&name, make()).await;
                if !sleep_or_stop(period).await {
                    return;
                }
            }
        }
        #[cfg(feature = "cron")]
        When::Cron(s) => loop {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let Some(next) = s.next_after(now) else {
                ftlog::error!("{}: the cron schedule never fires", name);
                return;
            };
            if !sleep_or_stop(Duration::from_secs((next - now).max(0) as u64)).await {
                return;
            }
            guarded_named(&name, make()).await;
        },
    }
}

async fn guarded_named(name: &str, fut: LocalBoxFuture<'static, ()>) {
    if AssertUnwindSafe(fut).catch_unwind().await.is_err() {
        ftlog::error!("{} panicked", name);
    }
}

impl Plugin for Tasks {
    fn apply(self, app: App) -> App {
        INSTALLED.store(true, Ordering::Relaxed);
        let tasks: Arc<Vec<AppTask>> = Arc::new(self.tasks);
        let app = if tasks.is_empty() {
            app
        } else {
            let tasks = tasks.clone();
            app.on_worker_start(move |_info| {
                for t in tasks.iter() {
                    // 既定は最初に起動したワーカーの 1 個だけ（リスナーが複数でも 1 回）
                    if !t.all_workers && t.claimed.swap(true, Ordering::AcqRel) {
                        continue;
                    }
                    let when = match &t.when {
                        When::Once => When::Once,
                        When::Every(d) => When::Every(*d),
                        #[cfg(feature = "cron")]
                        When::Cron(s) => When::Cron(s.clone()),
                    };
                    veil::runtime::spawn(run_task(t.name.clone(), when, t.make.clone()));
                }
            })
        };
        app.wrap_on(&self.background_prefix, |next: HandlerFn| -> HandlerFn {
            Arc::new(move |req: Request| {
                let ext: Extensions = req.extensions().share();
                let fut = next(req);
                Box::pin(async move {
                    let resp = fut.await;
                    attach(resp, &ext)
                })
            })
        })
    }
}

fn attach(resp: Response, ext: &Extensions) -> Response {
    let Some(list) = ext.remove::<TaskList>() else {
        return resp;
    };
    let jobs = std::mem::take(&mut *list.0.lock().unwrap());
    if jobs.is_empty() {
        return resp;
    }
    resp.after_send(move || {
        for j in jobs {
            j();
        }
    })
}

/// A cron schedule (UTC).
#[cfg(feature = "cron")]
pub mod cron {
    /// A parsed cron expression.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Schedule {
        sec: u64,
        min: u64,
        hour: u32,
        dom: u32,
        month: u16,
        dow: u8,
        dom_any: bool,
        dow_any: bool,
    }

    fn field(s: &str, lo: u32, hi: u32) -> Result<(u64, bool), String> {
        let mut bits = 0u64;
        let any = s == "*" || s == "?";
        for part in s.split(',') {
            let (range, step) = match part.split_once('/') {
                Some((r, st)) => (
                    r,
                    st.parse::<u32>()
                        .map_err(|_| format!("invalid step in '{}'", part))?,
                ),
                None => (part, 1),
            };
            if step == 0 {
                return Err(format!("zero step in '{}'", part));
            }
            let (a, b) = if range == "*" || range == "?" {
                (lo, hi)
            } else if let Some((a, b)) = range.split_once('-') {
                (num(a)?, num(b)?)
            } else {
                let a = num(range)?;
                (a, if part.contains('/') { hi } else { a })
            };
            if a < lo || b > hi || a > b {
                return Err(format!("'{}' is out of range {}-{}", part, lo, hi));
            }
            let mut v = a;
            while v <= b {
                bits |= 1 << v;
                v += step;
            }
        }
        Ok((bits, any))
    }

    fn num(s: &str) -> Result<u32, String> {
        s.parse::<u32>()
            .map_err(|_| format!("'{}' is not a number", s))
    }

    impl Schedule {
        /// Parses six fields (`sec min hour day month weekday`) or five (without seconds).
        pub fn parse(expr: &str) -> Result<Schedule, String> {
            let f: Vec<&str> = expr.split_whitespace().collect();
            let f: Vec<&str> = match f.len() {
                5 => std::iter::once("0").chain(f).collect(),
                6 => f,
                n => return Err(format!("a cron expression has 5 or 6 fields, not {}", n)),
            };
            let (sec, _) = field(f[0], 0, 59)?;
            let (min, _) = field(f[1], 0, 59)?;
            let (hour, _) = field(f[2], 0, 23)?;
            let (dom, dom_any) = field(f[3], 1, 31)?;
            let (month, _) = field(f[4], 1, 12)?;
            let (mut dow, dow_any) = field(f[5], 0, 7)?;
            if dow & (1 << 7) != 0 {
                dow |= 1;
            }
            Ok(Schedule {
                sec,
                min,
                hour: hour as u32,
                dom: dom as u32,
                month: month as u16,
                dow: (dow & 0x7f) as u8,
                dom_any,
                dow_any,
            })
        }

        fn day_matches(&self, d: u32, wd: u32) -> bool {
            let dm = self.dom & (1 << d) != 0;
            let wm = self.dow & (1 << wd) != 0;
            // どちらかが * なら他方だけ、両方指定なら OR（cron の慣習）
            match (self.dom_any, self.dow_any) {
                (true, true) => true,
                (true, false) => wm,
                (false, true) => dm,
                (false, false) => dm || wm,
            }
        }

        /// The first time (Unix seconds) strictly after `t` that matches (`None` if none within 5 years).
        pub fn next_after(&self, t: i64) -> Option<i64> {
            let mut t = t + 1;
            let limit = t + 5 * 366 * 86400;
            while t < limit {
                let days = t.div_euclid(86400);
                let secs = t.rem_euclid(86400);
                let (_, m, d) = civil(days);
                let wd = ((days + 4).rem_euclid(7)) as u32; // 1970-01-01 は木曜
                if self.month & (1 << m) == 0 || !self.day_matches(d, wd) {
                    t = (days + 1) * 86400;
                    continue;
                }
                let h = (secs / 3600) as u32;
                if self.hour & (1 << h) == 0 {
                    t = days * 86400 + (h as i64 + 1) * 3600;
                    continue;
                }
                let mi = ((secs % 3600) / 60) as u64;
                if self.min & (1 << mi) == 0 {
                    t = days * 86400 + h as i64 * 3600 + (mi as i64 + 1) * 60;
                    continue;
                }
                let s = (secs % 60) as u64;
                if self.sec & (1 << s) == 0 {
                    t += 1;
                    continue;
                }
                return Some(t);
            }
            None
        }
    }

    /// (year, month 1-12, day 1-31) of days since 1970-01-01 (H. Hinnant's algorithm).
    fn civil(z: i64) -> (i64, u32, u32) {
        let z = z + 719468;
        let era = z.div_euclid(146097);
        let doe = z.rem_euclid(146097);
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        (if m <= 2 { y + 1 } else { y }, m, d)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // 2026-10-26 00:00:00 UTC（月曜）
        const BASE: i64 = 1_792_972_800;

        #[test]
        fn civil_dates() {
            assert_eq!(civil(0), (1970, 1, 1));
            assert_eq!(civil(BASE / 86400), (2026, 10, 26));
            assert_eq!(civil(11016), (2000, 2, 29));
        }

        #[test]
        fn next_times() {
            let every5 = Schedule::parse("0 */5 * * * *").unwrap();
            assert_eq!(every5.next_after(BASE), Some(BASE + 300));
            assert_eq!(every5.next_after(BASE + 1), Some(BASE + 300));
            let five_field = Schedule::parse("30 2 * * *").unwrap();
            assert_eq!(five_field.next_after(BASE), Some(BASE + 2 * 3600 + 30 * 60));
            // 日曜の 0 時（0 と 7 のどちらも日曜）
            let sunday = Schedule::parse("0 0 0 * * 7").unwrap();
            assert_eq!(sunday.next_after(BASE), Some(BASE + 6 * 86400));
            assert_eq!(Schedule::parse("0 0 0 * * 0").unwrap(), sunday);
            // 2 月 29 日（次は 2028 年）
            let leap = Schedule::parse("0 0 0 29 2 *").unwrap();
            let n = leap.next_after(BASE).unwrap();
            assert_eq!(civil(n / 86400), (2028, 2, 29));
            assert!(Schedule::parse("0 0 0 31 2 *")
                .unwrap()
                .next_after(BASE)
                .is_none());
            assert!(Schedule::parse("* * *").is_err());
            assert!(Schedule::parse("61 * * * * *").is_err());
            assert!(Schedule::parse("*/0 * * * * *").is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn jobs_are_attached_after_the_handler() {
        INSTALLED.store(true, Ordering::Relaxed);
        let req = Request::new("GET", "/", vec![], Bytes::new());
        let ext = req.extensions().share();
        let ran = Arc::new(AtomicBool::new(false));
        let t = BackgroundTasks::from_request(&req).unwrap();
        let r2 = ran.clone();
        t.0 .0
            .lock()
            .unwrap()
            .push(Box::new(move || r2.store(true, Ordering::SeqCst)));
        let resp = attach(Response::text("ok"), &ext);
        assert!(
            !ran.load(Ordering::SeqCst),
            "not before the response is sent"
        );
        drop(resp);
        assert!(
            ran.load(Ordering::SeqCst),
            "runs when the sent response is dropped"
        );
        // 使わない要求は何も付けない
        let req = Request::new("GET", "/", vec![], Bytes::new());
        let ext = req.extensions().share();
        let resp = attach(Response::text("ok"), &ext);
        drop(resp);
    }
}
