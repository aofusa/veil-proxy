//! インスタンスのエクスポートの関数を引いたまま持つ（F-318）。
//!
//! wasmtime の `Instance::get_typed_func`・`Func::typed` は、呼ぶたびにエンジンの型レジストリのロック（読み取り）を
//! 取り、関数の型（`RegisteredType`。エンジン・型・再帰グループの `Arc` と登録数）を複製して捨てる。どれも全ワーカーが
//! 同じ行へ書く操作で、要求ごとに十数回呼ぶとコアが増えるほど 1 回が遅くなる。引いた `TypedFunc` はストアのデータ
//! （[`HostState`]）に置いてインスタンスの寿命の間使い回す。`TypedFunc` の複製も型の複製なので、呼ぶ間だけ取り出して
//! 移し、呼んだ後に戻す（取り出している間に同じ関数を呼ぶ入れ子の呼び出しは、引き直して呼ぶ）。
//!
//! ホスト関数がゲストのメモリを取る確保関数は、同期のホスト関数から今のファイバーの上で呼ぶ
//! （[`allocate_guest_memory`]）。

use wasmtime::{Caller, Instance, Store, TypedFunc, ValRaw, WasmParams, WasmResults};

use super::context::HostState;

/// 1 つのエクスポートの状態
#[derive(Default)]
pub(crate) enum Export<P, R> {
    /// まだ引いていない（呼んでいる間は取り出しているのでこの状態）
    #[default]
    Unknown,
    /// 無い・型が合わない（呼ばない。引き直さない）
    Absent,
    /// 引いた関数
    Present(TypedFunc<P, R>),
}

/// 要求ごとに呼ぶエクスポート
#[derive(Default)]
pub(crate) struct GuestExports {
    pub(crate) on_context_create: Export<(i32, i32), ()>,
    pub(crate) on_delete: Export<i32, ()>,
    pub(crate) on_request_headers: Export<(i32, i32, i32), i32>,
    pub(crate) on_response_headers: Export<(i32, i32, i32), i32>,
    pub(crate) on_request_body: Export<(i32, i32, i32), i32>,
    pub(crate) on_response_body: Export<(i32, i32, i32), i32>,
    pub(crate) on_request_trailers: Export<(i32, i32), i32>,
    pub(crate) on_response_trailers: Export<(i32, i32), i32>,
    pub(crate) on_log: Export<i32, ()>,
    pub(crate) on_http_call_response: Export<(i32, i32, i32, i32, i32), ()>,
    /// ゲストのメモリの確保（`proxy_on_memory_allocate`、無ければ `malloc`）
    memory_allocate: Export<i32, i32>,
    /// 確保関数を呼んでいる（確保の中からの再入は確保の失敗にする。[`allocate_guest_memory`]）
    allocating: bool,
}

/// [`GuestExports`] のどのエクスポートか
pub(crate) type Pick<P, R> = for<'a> fn(&'a mut GuestExports) -> &'a mut Export<P, R>;

/// エクスポートを取り出す（無ければ `None`）。呼んだ後に [`put`] で戻す
pub(crate) fn take<P, R>(
    store: &mut Store<HostState>,
    instance: Instance,
    pick: Pick<P, R>,
    name: &str,
) -> Option<TypedFunc<P, R>>
where
    P: WasmParams,
    R: WasmResults,
{
    let slot = pick(&mut store.data_mut().exports);
    match std::mem::take(slot) {
        Export::Present(f) => Some(f),
        Export::Absent => {
            *slot = Export::Absent;
            None
        }
        Export::Unknown => match instance.get_typed_func::<P, R>(&mut *store, name) {
            Ok(f) => Some(f),
            Err(_) => {
                *pick(&mut store.data_mut().exports) = Export::Absent;
                None
            }
        },
    }
}

/// [`take`] で取り出した関数を戻す
pub(crate) fn put<P, R>(store: &mut Store<HostState>, pick: Pick<P, R>, func: TypedFunc<P, R>) {
    *pick(&mut store.data_mut().exports) = Export::Present(func);
}

/// エクスポート `name` を呼ぶ。無ければ（型が合わなければ）`None`
pub(crate) async fn call<P, R>(
    store: &mut Store<HostState>,
    instance: Instance,
    pick: Pick<P, R>,
    name: &str,
    params: P,
) -> Option<anyhow::Result<R>>
where
    P: WasmParams + Sync,
    R: WasmResults + Sync,
{
    let func = take(store, instance, pick, name)?;
    let r = func.call_async(&mut *store, params).await;
    put(store, pick, func);
    Some(r)
}

/// 同じエクスポートを続けて呼ぶ（`ids` の順に 1 回ずつ。最初のエラーで止める）。無ければ何もしない
pub(crate) async fn call_each<P>(
    store: &mut Store<HostState>,
    instance: Instance,
    pick: Pick<P, ()>,
    name: &str,
    ids: impl IntoIterator<Item = P>,
) -> anyhow::Result<()>
where
    P: WasmParams + Sync,
{
    let Some(func) = take(store, instance, pick, name) else {
        return Ok(());
    };
    let mut r = Ok(());
    for id in ids {
        r = func.call_async(&mut *store, id).await;
        if r.is_err() {
            break;
        }
    }
    put(store, pick, func);
    r
}

/// ゲストのメモリを `size` バイト確保する（ホスト関数がゲストへ値を返すとき）。`Ok(None)` は確保できなかった
/// （ゲストのアロケータが 0 を返した〈B-320。以前は有効な場所として先頭からクライアントのバイトを書き込んでいた。F49〉・
/// アロケータが無い・確保の中からの再入）。`Err` は確保の呼び出しがトラップした・取り消された（ホスト関数はそのまま
/// トラップとして返す）。
///
/// F-318: 同期のホスト関数から、今のファイバーの上で呼ぶ（`Func::call_unchecked`）。以前は非同期のホスト関数から
/// `call_async` で呼んでいた（B-20）。wasmtime は `call_async` ごとに新しいファイバーを作り、ストアが持ち越すスタックは
/// 1 本だけなので、入れ子の呼び出しのスタックは毎回プーリングアロケータ（全ワーカーで共有する Mutex と添字の表）から
/// 取って返していた（コアが増えるほど競合する）。
///
/// - 同期のホスト関数の中では、wasmtime がファイバーを止めて再開するための文脈はストアに残っている（非同期のホスト関数の
///   中では取り出されている）。確保の途中で fuel の区切りに当たれば、外側の呼び出しのファイバーごと止まって再開する。
///   取り消されたら確保はエラーで戻るので、ホスト関数はトラップとして返す（外側のゲストを走らせ続けると、wasmtime の
///   後始末の再開〈1 回で終わる前提〉の後にまた止まる）。
/// - 非同期のストアでは、wasmtime はゲストに入るたびにスタックの上限を今のスタックポインタから `max_wasm_stack` 分と
///   測り直す。入れ子は 1 段だけにし（`allocating`）、ファイバーのスタックを外側と入れ子の 2 回分より大きくする
///   （`registry::ASYNC_STACK_SIZE`）。
pub(crate) fn allocate_guest_memory(
    caller: &mut Caller<'_, HostState>,
    size: usize,
) -> anyhow::Result<Option<i32>> {
    let Ok(size) = i32::try_from(size) else {
        return Ok(None);
    };
    if caller.data().exports.allocating {
        return Ok(None);
    }
    let func = match &caller.data().exports.memory_allocate {
        Export::Present(f) => *f.func(),
        Export::Absent => return Ok(None),
        Export::Unknown => {
            // `proxy_on_memory_allocate` があれば型が合わなくても `malloc` へは移らない（以前と同じ）
            let found = caller
                .get_export("proxy_on_memory_allocate")
                .or_else(|| caller.get_export("malloc"))
                .and_then(|e| e.into_func())
                .and_then(|f| f.typed::<i32, i32>(&*caller).ok());
            let Some(f) = found else {
                caller.data_mut().exports.memory_allocate = Export::Absent;
                return Ok(None);
            };
            let func = *f.func();
            caller.data_mut().exports.memory_allocate = Export::Present(f);
            func
        }
    };
    let mut vals = [ValRaw::i32(size)];
    caller.data_mut().exports.allocating = true;
    // SAFETY: `func` はこのストアのインスタンスのエクスポートで、型が `(i32) -> i32` であることを `typed` で確かめてある。
    // 引数と戻り値は 1 つずつで、`vals` はどちらにも足りる（引数を入れて渡し、戻り値で上書きされる）。
    let r = unsafe { func.call_unchecked(&mut *caller, &mut vals) };
    caller.data_mut().exports.allocating = false;
    r?;
    let p = vals[0].get_i32();
    Ok((p != 0 || size == 0).then_some(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasm::capabilities::ModuleCapabilities;
    use crate::wasm::context::HttpContext;

    const WAT: &str = r#"(module
      (global $n (mut i32) (i32.const 0))
      (func (export "proxy_on_log") (param i32)
        (global.set $n (i32.add (global.get $n) (local.get 0))))
      (func (export "proxy_on_delete") (param i32)
        (global.set $n (i32.add (global.get $n) (local.get 0))))
      ;; 型が合わない（本当は (i32 i32 i32) -> i32）
      (func (export "proxy_on_request_body") (param i32 i32) (result i32) (i32.const 0))
      (func (export "count") (result i32) (global.get $n)))"#;

    fn instance() -> (Store<HostState>, Instance) {
        let mut cfg = wasmtime::Config::new();
        cfg.async_support(true);
        let engine = wasmtime::Engine::new(&cfg).unwrap();
        let module = wasmtime::Module::new(&engine, WAT).unwrap();
        let mut store = Store::new(
            &engine,
            HostState::new(HttpContext::new(1, ModuleCapabilities::default())),
        );
        let instance =
            futures::executor::block_on(Instance::new_async(&mut store, &module, &[])).unwrap();
        (store, instance)
    }

    fn count(store: &mut Store<HostState>, instance: Instance) -> i32 {
        let f = instance
            .get_typed_func::<(), i32>(&mut *store, "count")
            .unwrap();
        futures::executor::block_on(f.call_async(&mut *store, ())).unwrap()
    }

    // F-318: 一度引いたエクスポートはストアに残り、次の呼び出しで引き直さない
    #[test]
    fn present_export_is_kept_after_the_call() {
        let (mut store, instance) = instance();
        assert!(matches!(store.data().exports.on_log, Export::Unknown));
        for _ in 0..2 {
            let r = futures::executor::block_on(call(
                &mut store,
                instance,
                |e| &mut e.on_log,
                "proxy_on_log",
                5,
            ));
            assert!(matches!(r, Some(Ok(()))));
            assert!(matches!(store.data().exports.on_log, Export::Present(_)));
        }
        assert_eq!(count(&mut store, instance), 10);
    }

    // 無いエクスポート・型の合わないエクスポートは呼ばず、引き直さない
    #[test]
    fn missing_or_mistyped_export_is_remembered_as_absent() {
        let (mut store, instance) = instance();
        for _ in 0..2 {
            let r = futures::executor::block_on(call(
                &mut store,
                instance,
                |e| &mut e.on_request_trailers,
                "proxy_on_request_trailers",
                (2, 0),
            ));
            assert!(r.is_none());
            assert!(matches!(
                store.data().exports.on_request_trailers,
                Export::Absent
            ));
            let r = futures::executor::block_on(call(
                &mut store,
                instance,
                |e| &mut e.on_request_body,
                "proxy_on_request_body",
                (2, 0, 1),
            ));
            assert!(r.is_none());
            assert!(matches!(
                store.data().exports.on_request_body,
                Export::Absent
            ));
        }
    }

    // 取り出している間は空き（入れ子の呼び出しは引き直す）。戻すと次から使い回す。続けて呼ぶ形も同じ関数を使う
    #[test]
    fn take_put_and_call_each_reuse_one_function() {
        let (mut store, instance) = instance();
        let f = take(
            &mut store,
            instance,
            |e| &mut e.on_delete,
            "proxy_on_delete",
        )
        .unwrap();
        assert!(matches!(store.data().exports.on_delete, Export::Unknown));
        futures::executor::block_on(f.call_async(&mut store, 1)).unwrap();
        put(&mut store, |e| &mut e.on_delete, f);
        assert!(matches!(store.data().exports.on_delete, Export::Present(_)));

        futures::executor::block_on(call_each(
            &mut store,
            instance,
            |e| &mut e.on_delete,
            "proxy_on_delete",
            [2, 3, 4],
        ))
        .unwrap();
        assert!(matches!(store.data().exports.on_delete, Export::Present(_)));
        assert_eq!(count(&mut store, instance), 10);

        // 無ければ何もしない
        futures::executor::block_on(call_each(
            &mut store,
            instance,
            |e| &mut e.on_context_create,
            "proxy_on_context_create",
            [(2, 1)],
        ))
        .unwrap();
        assert!(matches!(
            store.data().exports.on_context_create,
            Export::Absent
        ));
    }

    /// `env.alloc` から確保関数を呼ぶモジュール（`run(n)` が `env.alloc(n)` の戻り値を返す）
    fn alloc_instance(wat: &str, yield_interval: u64) -> (Store<HostState>, Instance) {
        let mut cfg = wasmtime::Config::new();
        cfg.async_support(true);
        cfg.consume_fuel(true);
        let engine = wasmtime::Engine::new(&cfg).unwrap();
        let module = wasmtime::Module::new(&engine, wat).unwrap();
        let mut linker = wasmtime::Linker::<HostState>::new(&engine);
        linker
            .func_wrap(
                "env",
                "alloc",
                |mut caller: Caller<'_, HostState>, n: i32| -> anyhow::Result<i32> {
                    Ok(allocate_guest_memory(&mut caller, n as usize)?.unwrap_or(0))
                },
            )
            .unwrap();
        let mut store = Store::new(
            &engine,
            HostState::new(HttpContext::new(1, ModuleCapabilities::default())),
        );
        store.set_fuel(1_000_000_000).unwrap();
        store
            .fuel_async_yield_interval(Some(yield_interval))
            .unwrap();
        let instance =
            futures::executor::block_on(linker.instantiate_async(&mut store, &module)).unwrap();
        (store, instance)
    }

    /// 終わるまで poll し、`Pending` の回数も返す
    fn poll_to_end<F: std::future::Future>(f: F) -> (F::Output, usize) {
        let mut f = std::pin::pin!(f);
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut pending = 0;
        loop {
            match f.as_mut().poll(&mut cx) {
                std::task::Poll::Ready(v) => return (v, pending),
                std::task::Poll::Pending => pending += 1,
            }
        }
    }

    fn run_func(store: &mut Store<HostState>, instance: Instance) -> TypedFunc<i32, i32> {
        instance
            .get_typed_func::<i32, i32>(&mut *store, "run")
            .unwrap()
    }

    /// fuel を使う確保関数（呼ぶたびに `n` バイト進めた番地を返す）
    const BUSY_ALLOCATOR: &str = r#"(module
      (import "env" "alloc" (func $alloc (param i32) (result i32)))
      (memory (export "memory") 1)
      (global $next (mut i32) (i32.const 1024))
      (func (export "proxy_on_memory_allocate") (param $n i32) (result i32)
        (local $i i32)
        (block $done
          (loop $l
            (br_if $done (i32.ge_u (local.get $i) (i32.const 2000)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $l)))
        (global.get $next)
        (global.set $next (i32.add (global.get $next) (local.get $n))))
      (func (export "run") (param $n i32) (result i32)
        (call $alloc (local.get $n))))"#;

    // F-318: 確保関数は同じファイバーの上で同期に呼ぶ。途中で fuel の区切りに当たれば外側の呼び出しごと止まり、
    // 再開して続きを走る（番地は正しく返る）
    #[test]
    fn nested_allocation_yields_and_resumes_on_the_same_fiber() {
        let (mut store, instance) = alloc_instance(BUSY_ALLOCATOR, 100);
        let run = run_func(&mut store, instance);
        let (r, pending) = poll_to_end(run.call_async(&mut store, 8));
        assert_eq!(r.unwrap(), 1024);
        assert!(pending > 0, "the nested allocation must yield");
        let (r, _) = poll_to_end(run.call_async(&mut store, 8));
        assert_eq!(r.unwrap(), 1032);
        assert!(!store.data().exports.allocating);
    }

    // 確保関数の中からの再入は確保の失敗（入れ子を 1 段に保つ。再帰でファイバーのスタックを使い切らない）
    #[test]
    fn reentrant_allocation_fails_instead_of_recursing() {
        let wat = r#"(module
          (import "env" "alloc" (func $alloc (param i32) (result i32)))
          (memory (export "memory") 1)
          (func (export "proxy_on_memory_allocate") (param $n i32) (result i32)
            (call $alloc (local.get $n)))
          (func (export "run") (param $n i32) (result i32)
            (call $alloc (local.get $n))))"#;
        let (mut store, instance) = alloc_instance(wat, 1_000_000);
        let run = run_func(&mut store, instance);
        let (r, _) = poll_to_end(run.call_async(&mut store, 8));
        assert_eq!(r.unwrap(), 0);
        assert!(!store.data().exports.allocating);
    }

    // 確保関数の無限再帰はスタック溢れのトラップになり、ホスト関数がそのまま外側へ返す（プロセスは落ちない。
    // ファイバーのスタックは外側と入れ子の 2 回分の上限より大きい）
    #[test]
    fn allocator_stack_overflow_traps_the_callback() {
        let wat = r#"(module
          (import "env" "alloc" (func $alloc (param i32) (result i32)))
          (memory (export "memory") 1)
          (func $rec (export "proxy_on_memory_allocate") (param $n i32) (result i32)
            (call $rec (i32.add (local.get $n) (i32.const 1))))
          (func (export "run") (param $n i32) (result i32)
            (call $alloc (local.get $n))))"#;
        let (mut store, instance) = alloc_instance(wat, 1_000_000);
        let run = run_func(&mut store, instance);
        let (r, _) = poll_to_end(run.call_async(&mut store, 8));
        let e = r.unwrap_err();
        assert!(
            matches!(
                e.downcast_ref::<wasmtime::Trap>(),
                Some(wasmtime::Trap::StackOverflow)
            ),
            "{e:?}"
        );
        assert!(!store.data().exports.allocating);
    }

    // 確保の途中で止まっている呼び出しを捨てると、wasmtime はファイバーを 1 回だけ再開して終わらせる。確保の失敗が
    // トラップとして外側まで伝わるので、ファイバーは終わる（走らせ続けて再び止まると wasmtime が panic する）
    #[test]
    fn dropping_a_call_suspended_in_allocation_finishes_the_fiber() {
        let (mut store, instance) = alloc_instance(BUSY_ALLOCATOR, 100);
        let run = run_func(&mut store, instance);
        {
            let mut f = Box::pin(run.call_async(&mut store, 8));
            let waker = futures::task::noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            assert!(std::future::Future::poll(f.as_mut(), &mut cx).is_pending());
        }
        assert!(!store.data().exports.allocating);
        // ストアは続けて使える
        let (r, _) = poll_to_end(run.call_async(&mut store, 8));
        assert!(r.unwrap() >= 1024);
    }
}
