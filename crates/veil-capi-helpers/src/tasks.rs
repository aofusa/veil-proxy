// F-291: アプリのタスク（定期・cron）。予定はワーカーの上の Rust のタスクで数え、C の関数はハンドラ用プールで
// 呼ぶ（ワーカーで C を呼ばない）。前の回が終わってから次を数えるので重ならない。停止が始まったら次を始めない。
// 応答の後の処理（Rust の kit の BackgroundTasks）は veil_response_after_send（コア）がそのまま当たる。
use crate::{tryc, UserData};
use futures::FutureExt;
use kit::tasks::cron::Schedule;
use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use veil_capi::internal::{self, cstr, fail};
use veil_capi::{veil_config_t, VEIL_ERR_INVALID_ARG, VEIL_OK};

/// A task function (called on the handler pool).
#[allow(non_camel_case_types)]
pub type veil_kit_task_fn = unsafe extern "C" fn(*mut c_void);

enum When {
    Every(Duration),
    Cron(Schedule),
}

struct Task {
    when: When,
    func: veil_kit_task_fn,
    ud: UserData,
    /// 最初に起動したワーカーの 1 個だけが受け持つ
    claimed: AtomicBool,
}

/// `d` 待つ。停止が始まったら `false`
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

async fn run(t: Arc<Task>) {
    loop {
        let wait = match &t.when {
            When::Every(d) => *d,
            When::Cron(s) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let Some(next) = s.next_after(now) else {
                    ftlog::error!("[kit] the cron schedule never fires");
                    return;
                };
                Duration::from_secs((next - now).max(0) as u64)
            }
        };
        if !sleep_or_stop(wait).await {
            return;
        }
        let (func, ud) = (t.func, t.ud);
        // SAFETY: veil_kit_every / veil_kit_cron の契約（user_data はサーバ稼働中ずっと有効）
        if veil::runtime::offload::offload_handler(move || unsafe { func(ud.get()) })
            .await
            .is_none()
        {
            ftlog::error!("[kit] a task panicked");
        }
    }
}

unsafe fn install(cfg: *mut veil_config_t, t: Task) -> c_int {
    let t = Arc::new(t);
    let ok = internal::push_plugin(cfg, move |app| {
        app.on_worker_start(move |_w| {
            if t.claimed.swap(true, Ordering::AcqRel) {
                return;
            }
            veil::runtime::spawn(run(t.clone()));
        })
    });
    if ok {
        VEIL_OK
    } else {
        fail(VEIL_ERR_INVALID_ARG, "NULL config")
    }
}

/// Runs `func(user_data)` every `period_ms` milliseconds on the handler pool, on one worker's schedule (the first
/// run is one period after startup; the next period starts when a run finishes, so runs never overlap). It stops
/// when graceful shutdown starts.
///
/// # Safety
///
/// `cfg` is a valid configuration handle; `user_data` stays valid while the server runs.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_every(
    cfg: *mut veil_config_t,
    period_ms: u64,
    func: Option<veil_kit_task_fn>,
    user_data: *mut c_void,
) -> c_int {
    let Some(func) = func else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL function");
    };
    if period_ms == 0 {
        return fail(VEIL_ERR_INVALID_ARG, "the period must be at least 1 ms");
    }
    install(
        cfg,
        Task {
            when: When::Every(Duration::from_millis(period_ms)),
            func,
            ud: UserData(user_data),
            claimed: AtomicBool::new(false),
        },
    )
}

/// Runs `func(user_data)` on a cron schedule in UTC on the handler pool: `"sec min hour day month weekday"` (six
/// fields) or the usual five without seconds (the same syntax as the Rust kit's `Tasks::cron`). An invalid
/// expression is `VEIL_ERR_INVALID_ARG`.
///
/// # Safety
///
/// `cfg` is a valid configuration handle; `expr` is a NUL-terminated string; `user_data` stays valid while the
/// server runs.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_cron(
    cfg: *mut veil_config_t,
    expr: *const c_char,
    func: Option<veil_kit_task_fn>,
    user_data: *mut c_void,
) -> c_int {
    let Some(func) = func else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL function");
    };
    let s = match Schedule::parse(tryc!(cstr(expr))) {
        Ok(s) => s,
        Err(e) => {
            return fail(
                VEIL_ERR_INVALID_ARG,
                format!("invalid cron expression: {}", e),
            )
        }
    };
    install(
        cfg,
        Task {
            when: When::Cron(s),
            func,
            ud: UserData(user_data),
            claimed: AtomicBool::new(false),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    unsafe extern "C" fn nop(_: *mut c_void) {}

    #[test]
    fn arguments_are_checked() {
        unsafe {
            let cfg = veil_capi::veil_config_new();
            assert_eq!(
                veil_kit_every(cfg, 0, Some(nop), std::ptr::null_mut()),
                VEIL_ERR_INVALID_ARG
            );
            assert_eq!(
                veil_kit_every(cfg, 10, None, std::ptr::null_mut()),
                VEIL_ERR_INVALID_ARG
            );
            assert_eq!(
                veil_kit_every(cfg, 10, Some(nop), std::ptr::null_mut()),
                VEIL_OK
            );
            assert_eq!(
                veil_kit_cron(cfg, c("* * *").as_ptr(), Some(nop), std::ptr::null_mut()),
                VEIL_ERR_INVALID_ARG
            );
            assert_eq!(
                veil_kit_cron(
                    cfg,
                    c("*/5 * * * *").as_ptr(),
                    Some(nop),
                    std::ptr::null_mut()
                ),
                VEIL_OK
            );
            veil_capi::veil_config_free(cfg);
        }
    }
}
