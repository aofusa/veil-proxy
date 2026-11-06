// F-291: 依存性の注入（Rust の kit の di::Container の C 版）。C には型が無いので鍵は文字列。寿命は Rust と同じ 3 種
// （singleton = プロセスで 1 個・初回に作る、scoped = 要求ごとに 1 個（要求の locals に置き、要求の終わりに解放）、
// transient = 取り出すたびに作る（要求の終わりに解放））。工場の関数は他の鍵を取り出してよい（循環は検出してエラー）。
// override は試験で差し替える（Rust の replace）。
//! Dependency injection with string keys.

use crate::{tryc, UserData};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CString};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use veil_capi::internal::{cstr, fail};
use veil_capi::{
    veil_free_fn, veil_request_get_local, veil_request_set_local, veil_request_t,
    VEIL_ERR_INVALID_ARG, VEIL_ERR_RUNTIME, VEIL_ERR_STATE, VEIL_OK,
};

/// A factory: returns a new instance (NULL = failure). `req` is NULL for singletons and when resolving without a
/// request.
#[allow(non_camel_case_types)]
pub type veil_kit_di_factory_fn = unsafe extern "C" fn(
    di: *mut veil_kit_di_t,
    req: *const veil_request_t,
    user_data: *mut c_void,
) -> *mut c_void;

/// One instance per process, made on first use.
pub const VEIL_KIT_DI_SINGLETON: c_int = 0;
/// One instance per request (freed when the request ends).
pub const VEIL_KIT_DI_SCOPED: c_int = 1;
/// A new instance on every resolution (freed when the request ends).
pub const VEIL_KIT_DI_TRANSIENT: c_int = 2;

/// 生成済みの値（解放関数があれば最後に呼ぶ）。
struct Instance {
    ptr: *mut c_void,
    free: Option<veil_free_fn>,
}

// SAFETY: C の呼び出し側が値のスレッド安全性を保証する（user_data と同じ規則。veil_kit.h に記す）。
unsafe impl Send for Instance {}
// SAFETY: 同上。
unsafe impl Sync for Instance {}

impl Drop for Instance {
    fn drop(&mut self) {
        if let Some(f) = self.free {
            if !self.ptr.is_null() {
                // SAFETY: 登録時に受け取った解放関数を、その工場が作った値に 1 回だけ呼ぶ
                unsafe { f(self.ptr) };
            }
        }
    }
}

enum Provider {
    Factory {
        lifetime: c_int,
        factory: veil_kit_di_factory_fn,
        free: Option<veil_free_fn>,
        ud: UserData,
        /// singleton の値（作るまでは None。作る間はこの Mutex を握る）
        single: Mutex<Option<Arc<Instance>>>,
    },
    Value(Arc<Instance>),
}

/// A container (thread-safe; free with `veil_kit_di_free` after the server stopped).
#[allow(non_camel_case_types)]
pub struct veil_kit_di_t {
    providers: RwLock<HashMap<String, Arc<Provider>>>,
    /// scoped・transient を要求の locals に置く鍵を他のコンテナと分ける
    id: u64,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_TRANSIENT: AtomicU64 = AtomicU64::new(1);

thread_local! {
    // 循環の検出: このスレッドで作っている最中の鍵
    static RESOLVING: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    // 工場の中で起きた取り出しの誤り（外側の「工場が失敗した」で上書きせずに伝える）
    static NESTED: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// 誤りを記録する。工場の中なら外側へ伝える文も残す。
fn dfail(code: c_int, msg: String) -> c_int {
    RESOLVING.with(|r| {
        if !r.borrow().is_empty() {
            NESTED.with(|n| *n.borrow_mut() = Some(msg.clone()));
        }
    });
    fail(code, msg)
}

/// Creates an empty container.
///
/// # Safety
///
/// Always callable.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_di_new() -> *mut veil_kit_di_t {
    Box::into_raw(Box::new(veil_kit_di_t {
        providers: RwLock::new(HashMap::new()),
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
    }))
}

/// Frees a container and its singletons and values (call it after the server stopped).
///
/// # Safety
///
/// `di` is NULL or a container from `veil_kit_di_new`, not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_di_free(di: *mut veil_kit_di_t) {
    if !di.is_null() {
        drop(Box::from_raw(di));
    }
}

unsafe fn put(di: *mut veil_kit_di_t, key: *const c_char, p: Provider, replace: bool) -> c_int {
    let Some(di) = di.as_ref() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL container");
    };
    let key = tryc!(cstr(key));
    let mut m = di.providers.write().unwrap_or_else(|e| e.into_inner());
    match (m.contains_key(key), replace) {
        (true, false) => fail(
            VEIL_ERR_INVALID_ARG,
            format!("'{}' is already registered (use veil_kit_di_override)", key),
        ),
        (false, true) => fail(VEIL_ERR_INVALID_ARG, format!("'{}' is not registered", key)),
        _ => {
            m.insert(key.to_string(), Arc::new(p));
            VEIL_OK
        }
    }
}

unsafe fn provider(
    lifetime: c_int,
    factory: Option<veil_kit_di_factory_fn>,
    free: Option<veil_free_fn>,
    ud: *mut c_void,
) -> Result<Provider, c_int> {
    let Some(factory) = factory else {
        return Err(fail(VEIL_ERR_INVALID_ARG, "NULL factory"));
    };
    if !(VEIL_KIT_DI_SINGLETON..=VEIL_KIT_DI_TRANSIENT).contains(&lifetime) {
        return Err(fail(VEIL_ERR_INVALID_ARG, "unknown lifetime"));
    }
    Ok(Provider::Factory {
        lifetime,
        factory,
        free,
        ud: UserData(ud),
        single: Mutex::new(None),
    })
}

/// Registers `key` with a factory and a lifetime (`VEIL_KIT_DI_SINGLETON`, `_SCOPED`, `_TRANSIENT`); `free`
/// (may be NULL) frees an instance. `VEIL_ERR_INVALID_ARG` when the key is already registered.
///
/// # Safety
///
/// `di` is a valid container, `key` a NUL-terminated string; `user_data` stays valid and may be used from any
/// thread; instances may be used from any thread.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_di_register(
    di: *mut veil_kit_di_t,
    key: *const c_char,
    lifetime: c_int,
    factory: Option<veil_kit_di_factory_fn>,
    free: Option<veil_free_fn>,
    user_data: *mut c_void,
) -> c_int {
    let p = tryc!(provider(lifetime, factory, free, user_data));
    put(di, key, p, false)
}

/// Registers a ready value under `key` (a singleton; `free` may be NULL).
///
/// # Safety
///
/// As for `veil_kit_di_register`.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_di_value(
    di: *mut veil_kit_di_t,
    key: *const c_char,
    value: *mut c_void,
    free: Option<veil_free_fn>,
) -> c_int {
    put(
        di,
        key,
        Provider::Value(Arc::new(Instance { ptr: value, free })),
        false,
    )
}

/// Replaces the provider of a registered `key` (for tests: a fake in place of the real service). Instances made
/// earlier stay alive until their scope ends.
///
/// # Safety
///
/// As for `veil_kit_di_register`.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_di_override(
    di: *mut veil_kit_di_t,
    key: *const c_char,
    lifetime: c_int,
    factory: Option<veil_kit_di_factory_fn>,
    free: Option<veil_free_fn>,
    user_data: *mut c_void,
) -> c_int {
    let p = tryc!(provider(lifetime, factory, free, user_data));
    put(di, key, p, true)
}

/// Replaces the provider of a registered `key` with a ready value.
///
/// # Safety
///
/// As for `veil_kit_di_register`.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_di_override_value(
    di: *mut veil_kit_di_t,
    key: *const c_char,
    value: *mut c_void,
    free: Option<veil_free_fn>,
) -> c_int {
    put(
        di,
        key,
        Provider::Value(Arc::new(Instance { ptr: value, free })),
        true,
    )
}

/// 作っている最中の鍵を積み、終わったら外す（循環の検出）。
struct Guard;

impl Guard {
    fn enter(key: &str) -> Result<Guard, c_int> {
        RESOLVING.with(|r| {
            let mut r = r.borrow_mut();
            if r.iter().any(|k| k == key) {
                let chain = r.join(" -> ");
                drop(r);
                return Err(dfail(
                    VEIL_ERR_STATE,
                    format!("dependency cycle: {} -> {}", chain, key),
                ));
            }
            r.push(key.to_string());
            Ok(Guard)
        })
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        RESOLVING.with(|r| {
            r.borrow_mut().pop();
        });
    }
}

unsafe extern "C" fn drop_instance(p: *mut c_void) {
    drop(Box::from_raw(p as *mut Instance));
}

unsafe fn make(
    di: *mut veil_kit_di_t,
    key: &str,
    factory: veil_kit_di_factory_fn,
    free: Option<veil_free_fn>,
    ud: UserData,
    req: *const veil_request_t,
) -> Result<Instance, c_int> {
    let _g = Guard::enter(key)?;
    call_factory(di, key, factory, free, ud, req)
}

/// 工場を呼ぶ（呼び出し側が Guard を持つ）。
unsafe fn call_factory(
    di: *mut veil_kit_di_t,
    key: &str,
    factory: veil_kit_di_factory_fn,
    free: Option<veil_free_fn>,
    ud: UserData,
    req: *const veil_request_t,
) -> Result<Instance, c_int> {
    NESTED.with(|n| n.borrow_mut().take());
    let p = factory(di, req, ud.get());
    if p.is_null() {
        let inner = NESTED.with(|n| n.borrow_mut().take());
        let msg = match inner {
            Some(m) => format!("the factory of '{}' failed: {}", key, m),
            None => format!("the factory of '{}' failed", key),
        };
        return Err(dfail(VEIL_ERR_RUNTIME, msg));
    }
    Ok(Instance { ptr: p, free })
}

unsafe fn resolve(
    di: *mut veil_kit_di_t,
    req: *const veil_request_t,
    key: &str,
) -> Result<*mut c_void, c_int> {
    let Some(d) = di.as_ref() else {
        return Err(fail(VEIL_ERR_INVALID_ARG, "NULL container"));
    };
    let p = d
        .providers
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .cloned();
    let Some(p) = p else {
        return Err(dfail(
            VEIL_ERR_INVALID_ARG,
            format!("'{}' is not registered", key),
        ));
    };
    match &*p {
        Provider::Value(v) => Ok(v.ptr),
        Provider::Factory {
            lifetime,
            factory,
            free,
            ud,
            single,
        } => match *lifetime {
            VEIL_KIT_DI_SINGLETON => {
                // 作る間は鍵ごとの Mutex を握る（同時の初回でも 1 個だけ作る。循環は Guard が先に止める）
                let _g = Guard::enter(key)?;
                let mut s = single.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(v) = &*s {
                    return Ok(v.ptr);
                }
                let v = call_factory(di, key, *factory, *free, *ud, ptr::null())?;
                let ptr = v.ptr;
                *s = Some(Arc::new(v));
                Ok(ptr)
            }
            lt => {
                if req.is_null() {
                    return Err(dfail(
                        VEIL_ERR_STATE,
                        format!(
                            "'{}' is request-scoped or transient: resolve it with a request",
                            key
                        ),
                    ));
                }
                let slot = if lt == VEIL_KIT_DI_SCOPED {
                    format!("veil_kit.di.{}:{}", d.id, key)
                } else {
                    format!(
                        "veil_kit.di.t{}",
                        NEXT_TRANSIENT.fetch_add(1, Ordering::Relaxed)
                    )
                };
                let slot = CString::new(slot)
                    .map_err(|_| fail(VEIL_ERR_INVALID_ARG, "key contains a NUL byte"))?;
                if lt == VEIL_KIT_DI_SCOPED {
                    let cur = veil_request_get_local(req, slot.as_ptr());
                    if !cur.is_null() {
                        return Ok((*(cur as *const Instance)).ptr);
                    }
                }
                let v = make(di, key, *factory, *free, *ud, req)?;
                let ptr = v.ptr;
                let boxed = Box::into_raw(Box::new(v)) as *mut c_void;
                let rc = veil_request_set_local(req, slot.as_ptr(), boxed, Some(drop_instance));
                if rc != VEIL_OK {
                    drop_instance(boxed);
                    return Err(rc);
                }
                Ok(ptr)
            }
        },
    }
}

/// Resolves `key`: the singleton or value, the instance of this request (scoped) or a new one (transient). `req`
/// may be NULL for singletons and values only. NULL on error (`veil_last_error`: not registered, the factory
/// failed, a dependency cycle). Instances are owned by the container or the request; do not free them.
///
/// # Safety
///
/// `di` is a valid container, `req` NULL or the request of the running callback, `key` a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_di_get(
    di: *mut veil_kit_di_t,
    req: *const veil_request_t,
    key: *const c_char,
) -> *mut c_void {
    let Ok(key) = cstr(key) else {
        return ptr::null_mut();
    };
    resolve(di, req, key).unwrap_or(ptr::null_mut())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{c, request, with_handles};
    use std::sync::atomic::AtomicUsize;

    static MADE: AtomicUsize = AtomicUsize::new(0);
    static FREED: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn counter(
        _di: *mut veil_kit_di_t,
        _req: *const veil_request_t,
        _ud: *mut c_void,
    ) -> *mut c_void {
        let n = MADE.fetch_add(1, Ordering::SeqCst) + 1;
        Box::into_raw(Box::new(n)) as *mut c_void
    }
    unsafe extern "C" fn free_counter(p: *mut c_void) {
        FREED.fetch_add(1, Ordering::SeqCst);
        drop(Box::from_raw(p as *mut usize));
    }
    unsafe extern "C" fn uses_a(
        di: *mut veil_kit_di_t,
        req: *const veil_request_t,
        _ud: *mut c_void,
    ) -> *mut c_void {
        let a = veil_kit_di_get(di, req, c"a".as_ptr());
        if a.is_null() {
            return ptr::null_mut();
        }
        Box::into_raw(Box::new(*(a as *const usize) + 100)) as *mut c_void
    }
    unsafe extern "C" fn cyclic(
        di: *mut veil_kit_di_t,
        req: *const veil_request_t,
        _ud: *mut c_void,
    ) -> *mut c_void {
        veil_kit_di_get(di, req, c"x".as_ptr())
    }

    unsafe fn get(di: *mut veil_kit_di_t, req: *const veil_request_t, k: &str) -> Option<usize> {
        let p = veil_kit_di_get(di, req, c(k).as_ptr());
        (!p.is_null()).then(|| *(p as *const usize))
    }

    #[test]
    fn lifetimes_overrides_and_cycles() {
        unsafe {
            let di = veil_kit_di_new();
            let reg = |k: &str, lt, f: veil_kit_di_factory_fn| {
                veil_kit_di_register(
                    di,
                    c(k).as_ptr(),
                    lt,
                    Some(f),
                    Some(free_counter),
                    ptr::null_mut(),
                )
            };
            assert_eq!(reg("s", VEIL_KIT_DI_SINGLETON, counter), VEIL_OK);
            assert_eq!(reg("a", VEIL_KIT_DI_SCOPED, counter), VEIL_OK);
            assert_eq!(reg("t", VEIL_KIT_DI_TRANSIENT, counter), VEIL_OK);
            assert_eq!(reg("b", VEIL_KIT_DI_SCOPED, uses_a), VEIL_OK);
            assert_eq!(reg("x", VEIL_KIT_DI_SCOPED, cyclic), VEIL_OK);
            assert_eq!(
                reg("s", VEIL_KIT_DI_SINGLETON, counter),
                VEIL_ERR_INVALID_ARG
            );
            let s1 = get(di, ptr::null(), "s").unwrap();
            assert_eq!(get(di, ptr::null(), "s"), Some(s1));
            // scoped・transient は要求が要る
            assert_eq!(get(di, ptr::null(), "a"), None);
            let before = FREED.load(Ordering::SeqCst);
            let ((), _) = with_handles(request(&[], ""), |req, _| {
                let a1 = get(di, req, "a").unwrap();
                assert_eq!(get(di, req, "a"), Some(a1));
                assert_eq!(get(di, req, "b"), Some(a1 + 100));
                let t1 = get(di, req, "t").unwrap();
                assert_ne!(get(di, req, "t"), Some(t1));
                assert_eq!(get(di, req, "x"), None);
                assert!(std::ffi::CStr::from_ptr(veil_capi::veil_last_error())
                    .to_str()
                    .unwrap()
                    .contains("cycle"));
                assert_eq!(get(di, req, "nope"), None);
            });
            // 要求の終わりに scoped 1 + transient 2 + b 1 個を解放（b は free_counter で usize を解放）
            assert_eq!(FREED.load(Ordering::SeqCst) - before, 4);
            // 差し替え
            static FAKE: usize = 7;
            assert_eq!(
                veil_kit_di_override_value(
                    di,
                    c("s").as_ptr(),
                    &FAKE as *const usize as *mut c_void,
                    None
                ),
                VEIL_OK
            );
            assert_eq!(get(di, ptr::null(), "s"), Some(7));
            assert_eq!(
                veil_kit_di_override_value(di, c("new").as_ptr(), ptr::null_mut(), None),
                VEIL_ERR_INVALID_ARG
            );
            veil_kit_di_free(di);
        }
    }
}
