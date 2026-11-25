//! インスタンスのエクスポートの関数を引いたまま持つ（F-318）。
//!
//! wasmtime の `Instance::get_typed_func`・`Func::typed` は、呼ぶたびにエンジンの型レジストリのロック（読み取り）を
//! 取り、関数の型（`RegisteredType`。エンジン・型・再帰グループの `Arc` と登録数）を複製して捨てる。どれも全ワーカーが
//! 同じ行へ書く操作で、要求ごとに十数回呼ぶとコアが増えるほど 1 回が遅くなる。引いた `TypedFunc` はストアのデータ
//! （[`HostState`]）に置いてインスタンスの寿命の間使い回す。`TypedFunc` の複製も型の複製なので、呼ぶ間だけ取り出して
//! 移し、呼んだ後に戻す（取り出している間に同じ関数を呼ぶ入れ子の呼び出しは、引き直して呼ぶ）。
//!
//! ホスト関数がゲストのメモリを取る確保関数は、同期のホスト関数から今のファイバーの上で呼ぶ
//! （[`allocate_guest_memory`]）。続けて呼ぶコールバックも 1 回のファイバーの出入りで呼ぶ（[`call_chain`]）。

use wasmtime::{
    AsContextMut, Caller, Func, Instance, Memory, Store, TypedFunc, ValRaw, WasmParams, WasmResults,
};

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
    /// ゲストの線形メモリ（[`GuestMemory`]）
    memory: Option<Memory>,
    /// 段を続けて呼ぶホスト関数（ストアごとに 1 つ。初めて使うときに作る。[`call_chain`]）
    chain: Export<(), ()>,
    /// [`call_chain`] とホスト関数が受け渡す段・入れ直す燃料と締切・結果
    chain_steps: [Option<Step>; MAX_STEPS],
    chain_arm: (u64, u64),
    chain_done: usize,
    chain_values: [Option<i32>; MAX_STEPS],
}

/// 1 回のファイバーの出入りで続けて呼べる段の数
pub(crate) const MAX_STEPS: usize = 3;

/// 1 回のファイバーの出入りで続けて呼ぶコールバック（[`call_chain`]）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// `proxy_on_context_create(context_id, parent_context_id)`
    ContextCreate(i32, i32),
    /// `proxy_on_request_headers(context_id, num_headers, end_of_stream)`
    RequestHeaders(i32, i32, i32),
    /// `proxy_on_log(context_id)`
    Log(i32),
    /// `proxy_on_delete(context_id)`。前に段があれば燃料と締切を入れ直してから呼ぶ（別に呼んでいたときと同じ）
    Delete(i32),
}

/// [`call_chain`] の結果
pub(crate) struct Chain {
    /// 終わった段の数。`error` があれば、この番号の段（段に入る前の失敗も含む）で止まった
    pub(crate) done: usize,
    /// 段の戻り値（`proxy_on_request_headers` の action。戻り値の無い段・エクスポートの無い段は `None`）
    pub(crate) values: [Option<i32>; MAX_STEPS],
    pub(crate) error: Option<anyhow::Error>,
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

/// ゲストの線形メモリ（`memory` のエクスポート）を引いたまま持つ（F-319。ホスト関数は呼ばれるたびに名前の表を
/// 引いていた）。ストアにはゲストのインスタンスが 1 つだけなので、最初に引いたものがそのストアの間ずっと使える
pub(crate) trait GuestMemory {
    fn guest_memory(&mut self) -> Option<Memory>;
}

impl GuestMemory for Caller<'_, HostState> {
    fn guest_memory(&mut self) -> Option<Memory> {
        if let Some(m) = self.data().exports.memory {
            return Some(m);
        }
        let m = self.get_export("memory")?.into_memory()?;
        self.data_mut().exports.memory = Some(m);
        Some(m)
    }
}

/// まだ引いていなければ引いておく（無い・型が合わなければ `Absent`）
fn resolve<P, R>(store: &mut Store<HostState>, instance: Instance, pick: Pick<P, R>, name: &str)
where
    P: WasmParams,
    R: WasmResults,
{
    if matches!(pick(&mut store.data_mut().exports), Export::Unknown) {
        let found = match instance.get_typed_func::<P, R>(&mut *store, name) {
            Ok(f) => Export::Present(f),
            Err(_) => Export::Absent,
        };
        *pick(&mut store.data_mut().exports) = found;
    }
}

fn present<P, R>(e: &Export<P, R>) -> Option<Func>
where
    P: WasmParams,
    R: WasmResults,
{
    match e {
        Export::Present(f) => Some(*f.func()),
        _ => None,
    }
}

/// コールバックを `steps` の順に、1 回のファイバーの出入りで続けて呼ぶ（F-319）。最初のエラーで止める。
///
/// wasmtime は `call_async` ごとにファイバーを作り、エンジンの `Arc`（全ワーカー共有）を複製して捨てる。要求ごとの
/// `proxy_on_context_create` → `proxy_on_request_headers`、`proxy_on_log` → `proxy_on_delete` を別々に呼ぶと、1 要求で
/// ファイバーの出入りが 4 回になる。ストアに置いた同期のホスト関数を 1 回 `call_async` し、その中から各コールバックを
/// 今のファイバーの上で呼ぶ（[`allocate_guest_memory`] と同じ）。
///
/// - fuel の区切りでは外側のファイバーごと止まって再開する。取り消されたら段はエラーで戻るので、そこで止めて返す
///   （続けて呼ぶと wasmtime の後始末の再開〈1 回で終わる前提〉の後にまた止まる）。
/// - ゲストに入るたびにスタックの上限を測り直すので、段と確保関数の入れ子は今と同じ 2 段に収まる
///   （`registry::ASYNC_STACK_SIZE`）。
/// - `Delete` の前に燃料と締切を `arm`（fuel, エポックの締切）で入れ直す（別に呼んでいたときと同じ上限）。
pub(crate) async fn call_chain(
    store: &mut Store<HostState>,
    instance: Instance,
    steps: &[Step],
    arm: (u64, u64),
) -> Chain {
    // ホスト関数の中では呼び出し元のインスタンスを引けないので、段のエクスポートを先に引いておく
    for step in steps {
        match step {
            Step::ContextCreate(..) => resolve(
                store,
                instance,
                |e| &mut e.on_context_create,
                "proxy_on_context_create",
            ),
            Step::RequestHeaders(..) => resolve(
                store,
                instance,
                |e| &mut e.on_request_headers,
                "proxy_on_request_headers",
            ),
            Step::Log(_) => resolve(store, instance, |e| &mut e.on_log, "proxy_on_log"),
            Step::Delete(_) => resolve(store, instance, |e| &mut e.on_delete, "proxy_on_delete"),
        }
    }
    let chain = match std::mem::take(&mut store.data_mut().exports.chain) {
        Export::Present(f) => f,
        _ => match Func::wrap(&mut *store, run_chain).typed::<(), ()>(&*store) {
            Ok(f) => f,
            Err(e) => {
                return Chain {
                    done: 0,
                    values: [None; MAX_STEPS],
                    error: Some(e),
                }
            }
        },
    };
    {
        let e = &mut store.data_mut().exports;
        e.chain_steps = [None; MAX_STEPS];
        for (slot, step) in e.chain_steps.iter_mut().zip(steps) {
            *slot = Some(*step);
        }
        e.chain_arm = arm;
        e.chain_done = 0;
        e.chain_values = [None; MAX_STEPS];
    }
    let r = chain.call_async(&mut *store, ()).await;
    let e = &mut store.data_mut().exports;
    e.chain = Export::Present(chain);
    e.chain_steps = [None; MAX_STEPS];
    Chain {
        done: e.chain_done,
        values: e.chain_values,
        error: r.err(),
    }
}

/// [`call_chain`] のホスト関数（ファイバーの上で段を続けて呼ぶ）
fn run_chain(mut caller: Caller<'_, HostState>) -> anyhow::Result<()> {
    let steps = caller.data().exports.chain_steps;
    let (fuel, deadline) = caller.data().exports.chain_arm;
    for (i, step) in steps.into_iter().enumerate() {
        let Some(step) = step else {
            break;
        };
        if i > 0 && matches!(step, Step::Delete(_)) {
            caller.set_fuel(fuel)?;
            caller.as_context_mut().set_epoch_deadline(deadline);
        }
        let value = run_step(&mut caller, step)?;
        let e = &mut caller.data_mut().exports;
        e.chain_values[i] = value;
        e.chain_done = i + 1;
    }
    Ok(())
}

/// 1 つの段を今のファイバーの上で呼ぶ（エクスポートが無ければ呼ばずに `None`）
fn run_step(caller: &mut Caller<'_, HostState>, step: Step) -> anyhow::Result<Option<i32>> {
    let e = &caller.data().exports;
    let (func, args, params, result) = match step {
        Step::ContextCreate(c, p) => (present(&e.on_context_create), [c, p, 0], 2, false),
        Step::RequestHeaders(c, n, eos) => (present(&e.on_request_headers), [c, n, eos], 3, true),
        Step::Log(c) => (present(&e.on_log), [c, 0, 0], 1, false),
        Step::Delete(c) => (present(&e.on_delete), [c, 0, 0], 1, false),
    };
    let Some(func) = func else {
        return Ok(None);
    };
    let mut vals = args.map(ValRaw::i32);
    let vals: &mut [ValRaw] = &mut vals[..params];
    // SAFETY: `func` はこのストアのインスタンスのエクスポートで、段の型（`ContextCreate` は `(i32, i32) -> ()`、
    // `RequestHeaders` は `(i32, i32, i32) -> i32`、`Log`・`Delete` は `(i32) -> ()`）であることを `typed` で確かめてある
    // （`resolve`。型が合わなければ `Absent` で呼ばない）。`vals` の長さは引数の数で、戻り値（0〜1 個）より短くない。
    unsafe { func.call_unchecked(&mut *caller, vals) }?;
    Ok(result.then(|| vals[0].get_i32()))
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

    /// 呼ばれた順を `trace` に 10 進で積むモジュール（context_create=1・request_headers=2・log=3・delete=4）。
    /// `proxy_on_request_headers` は context の ID を action として返し、`proxy_on_log` は context 9 でトラップする。
    /// `$spin` は fuel を使う（fuel の区切りを試す）
    const CHAIN_MODULE: &str = r#"(module
      (memory (export "memory") 1)
      (global $trace (mut i32) (i32.const 0))
      (func $push (param $v i32)
        (global.set $trace (i32.add (i32.mul (global.get $trace) (i32.const 10)) (local.get $v))))
      (func $spin
        (local $i i32)
        (block $done
          (loop $l
            (br_if $done (i32.ge_u (local.get $i) (i32.const 2000)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $l))))
      (func (export "proxy_on_context_create") (param i32 i32)
        (call $spin) (call $push (i32.const 1)))
      (func (export "proxy_on_request_headers") (param $c i32) (param i32 i32) (result i32)
        (call $spin) (call $push (i32.const 2)) (local.get $c))
      (func (export "proxy_on_log") (param $c i32)
        (if (i32.eq (local.get $c) (i32.const 9)) (then unreachable))
        (call $spin) (call $push (i32.const 3)))
      (func (export "proxy_on_delete") (param i32)
        (call $spin) (call $push (i32.const 4)))
      (func (export "trace") (result i32) (global.get $trace))
      (func (export "reset") (global.set $trace (i32.const 0))))"#;

    fn chain_instance(yield_interval: u64) -> (Store<HostState>, Instance) {
        let mut cfg = wasmtime::Config::new();
        cfg.async_support(true);
        cfg.consume_fuel(true);
        let engine = wasmtime::Engine::new(&cfg).unwrap();
        let module = wasmtime::Module::new(&engine, CHAIN_MODULE).unwrap();
        let linker = wasmtime::Linker::<HostState>::new(&engine);
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

    fn trace(store: &mut Store<HostState>, instance: Instance) -> i32 {
        let f = instance
            .get_typed_func::<(), i32>(&mut *store, "trace")
            .unwrap();
        poll_to_end(f.call_async(&mut *store, ())).0.unwrap()
    }

    const ARM: (u64, u64) = (1_000_000_000, 10);

    // F-319: 段は順に 1 回のファイバーの出入りで呼ばれ、`proxy_on_request_headers` の action が返る。fuel の区切りで
    // 止まっても続きから再開する。ホスト関数は 2 回目以降は作り直さない
    #[test]
    fn chain_runs_the_steps_in_order_and_resumes_after_yields() {
        let (mut store, instance) = chain_instance(100);
        let steps = [Step::ContextCreate(5, 1), Step::RequestHeaders(5, 3, 1)];
        let (chain, pending) = poll_to_end(call_chain(&mut store, instance, &steps, ARM));
        assert!(chain.error.is_none(), "{:?}", chain.error);
        assert_eq!(chain.done, 2);
        assert_eq!(chain.values[..2], [None, Some(5)]);
        assert!(pending > 0, "the steps must yield on the fuel interval");
        assert_eq!(trace(&mut store, instance), 12);
        assert!(matches!(store.data().exports.chain, Export::Present(_)));

        let steps = [Step::Log(5), Step::Delete(5)];
        let (chain, _) = poll_to_end(call_chain(&mut store, instance, &steps, ARM));
        assert!(chain.error.is_none());
        assert_eq!(chain.done, 2);
        assert_eq!(trace(&mut store, instance), 1234);
    }

    // 最初のエラーで止め、後の段は呼ばない（`done` は止まった段の番号）
    #[test]
    fn chain_stops_at_the_first_error() {
        let (mut store, instance) = chain_instance(1_000_000);
        let steps = [Step::ContextCreate(9, 1), Step::Log(9), Step::Delete(9)];
        let (chain, _) = poll_to_end(call_chain(&mut store, instance, &steps, ARM));
        let e = chain.error.expect("proxy_on_log traps");
        assert!(
            matches!(
                e.downcast_ref::<wasmtime::Trap>(),
                Some(wasmtime::Trap::UnreachableCodeReached)
            ),
            "{e:?}"
        );
        assert_eq!(chain.done, 1);
        assert_eq!(trace(&mut store, instance), 1);
    }

    // エクスポートの無い段は呼ばずに飛ばす（値は `None`）
    #[test]
    fn chain_skips_missing_exports() {
        let (mut store, instance) = chain_instance(1_000_000);
        store.data_mut().exports.on_log = Export::Absent;
        let steps = [Step::Log(5), Step::Delete(5)];
        let (chain, _) = poll_to_end(call_chain(&mut store, instance, &steps, ARM));
        assert!(chain.error.is_none());
        assert_eq!(chain.done, 2);
        assert_eq!(chain.values[..2], [None, None]);
        assert_eq!(trace(&mut store, instance), 4);
    }

    // `Delete` の前に燃料を入れ直す（前の段で使い切っても `proxy_on_delete` は別に呼んだときと同じ上限で走る）
    #[test]
    fn chain_rearms_fuel_before_delete() {
        let (mut store, instance) = chain_instance(1_000_000);
        // 1 段が使う燃料を測り、その 1.5 倍だけ入れる: 1 段は走れるが、続けて 2 段は走れない
        let (chain, _) = poll_to_end(call_chain(&mut store, instance, &[Step::Log(5)], ARM));
        assert!(chain.error.is_none());
        let fuel = (1_000_000_000 - store.get_fuel().unwrap()) * 3 / 2;
        let reset = instance
            .get_typed_func::<(), ()>(&mut store, "reset")
            .unwrap();
        poll_to_end(reset.call_async(&mut store, ())).0.unwrap();

        store.set_fuel(fuel).unwrap();
        let steps = [Step::Log(5), Step::Delete(5)];
        let (chain, _) = poll_to_end(call_chain(&mut store, instance, &steps, (fuel, 10)));
        assert!(chain.error.is_none(), "{:?}", chain.error);
        assert_eq!(trace(&mut store, instance), 34);

        // 入れ直さなければ 2 段目で燃料が尽きる
        store.set_fuel(1_000_000).unwrap();
        poll_to_end(reset.call_async(&mut store, ())).0.unwrap();
        store.set_fuel(fuel).unwrap();
        let steps = [Step::Log(5), Step::Log(6)];
        let (chain, _) = poll_to_end(call_chain(&mut store, instance, &steps, (fuel, 10)));
        assert!(matches!(
            chain
                .error
                .as_ref()
                .and_then(|e| e.downcast_ref::<wasmtime::Trap>()),
            Some(wasmtime::Trap::OutOfFuel)
        ));
        assert_eq!(chain.done, 1);
    }

    // 段の途中で止まっている呼び出しを捨てても、ファイバーは終わり、ストアは続けて使える
    #[test]
    fn dropping_a_chain_suspended_in_a_step_finishes_the_fiber() {
        let (mut store, instance) = chain_instance(100);
        {
            let steps = [Step::ContextCreate(5, 1), Step::RequestHeaders(5, 3, 1)];
            let mut f = Box::pin(call_chain(&mut store, instance, &steps, ARM));
            let waker = futures::task::noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            assert!(std::future::Future::poll(f.as_mut(), &mut cx).is_pending());
        }
        // 取り出していたホスト関数は戻らない（次に作り直す）
        assert!(!matches!(store.data().exports.chain, Export::Present(_)));
        let steps = [Step::Log(5)];
        let (chain, _) = poll_to_end(call_chain(&mut store, instance, &steps, ARM));
        assert!(chain.error.is_none(), "{:?}", chain.error);
        assert_eq!(chain.done, 1);
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
