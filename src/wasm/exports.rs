//! インスタンスのエクスポートの関数を引いたまま持つ（F-318）。
//!
//! wasmtime の `Instance::get_typed_func`・`Func::typed` は、呼ぶたびにエンジンの型レジストリのロック（読み取り）を
//! 取り、関数の型（`RegisteredType`。エンジン・型・再帰グループの `Arc` と登録数）を複製して捨てる。どれも全ワーカーが
//! 同じ行へ書く操作で、要求ごとに十数回呼ぶとコアが増えるほど 1 回が遅くなる。引いた `TypedFunc` はストアのデータ
//! （[`HostState`]）に置いてインスタンスの寿命の間使い回す。`TypedFunc` の複製も型の複製なので、呼ぶ間だけ取り出して
//! 移し、呼んだ後に戻す（取り出している間に同じ関数を呼ぶ入れ子の呼び出しは、引き直して呼ぶ）。

use wasmtime::{Caller, Instance, Store, TypedFunc, WasmParams, WasmResults};

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

/// ゲストのメモリを `size` バイト確保する（ホスト関数がゲストへ値を返すとき）。0 はゲストのアロケータの失敗
/// （B-320。以前は有効な場所として先頭からクライアントのバイトを書き込んでいた。F49）。
///
/// B-20: エンジンは async store（燃料で yield する）なので `call_async` で呼ぶ（同期の `call` は panic する）。
pub(crate) async fn allocate_guest_memory(
    caller: &mut Caller<'_, HostState>,
    size: usize,
) -> Option<i32> {
    let size = i32::try_from(size).ok()?;
    let func = match std::mem::take(&mut caller.data_mut().exports.memory_allocate) {
        Export::Present(f) => f,
        Export::Absent => {
            caller.data_mut().exports.memory_allocate = Export::Absent;
            return None;
        }
        Export::Unknown => {
            // `proxy_on_memory_allocate` があれば型が合わなくても `malloc` へは移らない（以前と同じ）
            let found = caller
                .get_export("proxy_on_memory_allocate")
                .or_else(|| caller.get_export("malloc"))
                .and_then(|e| e.into_func())
                .and_then(|f| f.typed::<i32, i32>(&*caller).ok());
            match found {
                Some(f) => f,
                None => {
                    caller.data_mut().exports.memory_allocate = Export::Absent;
                    return None;
                }
            }
        }
    };
    let r = func.call_async(&mut *caller, size).await;
    caller.data_mut().exports.memory_allocate = Export::Present(func);
    r.ok().filter(|&p| p != 0 || size == 0)
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
}
