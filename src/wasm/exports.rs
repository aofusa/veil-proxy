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

use wasmtime::{Caller, Func, Instance, Memory, Store, TypedFunc, ValRaw, WasmParams, WasmResults};

use super::context::HostState;
use super::metering::{Limits, Window};
use super::types::LocalResponse;

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
    /// [`call_chain`] とホスト関数が受け渡すもの
    chain_io: ChainIo,
}

/// [`call_chain`] とホスト関数（[`run_chain`]）が受け渡す段・入れ直す上限（`metering::arm`）・結果
#[derive(Default)]
struct ChainIo {
    steps: [Option<Step>; MAX_STEPS],
    arm: Option<(Limits, Window)>,
    done: usize,
    values: [Option<i32>; MAX_STEPS],
    fuel: [Option<u64>; MAX_STEPS],
    skipped: u8,
    /// `RequestBody` の段で context に置く本文
    body: Option<bytes::Bytes>,
    /// `RequestBody` の段の前の要求ヘッダ（上流へ送るもの）
    upstream_headers: Option<Vec<(Vec<u8>, Vec<u8>)>>,
    /// `LogLocal` の段の前に context から取り出したローカル応答
    local: Option<LocalResponse>,
}

/// 1 回のファイバーの出入りで続けて呼べる段の数（作成・ヘッダ・本文・ログ・削除）
pub(crate) const MAX_STEPS: usize = 5;

/// 1 回のファイバーの出入りで続けて呼ぶコールバック（[`call_chain`]）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// `proxy_on_context_create(context_id, parent_context_id)`
    ContextCreate(i32, i32),
    /// `proxy_on_request_headers(context_id, num_headers, end_of_stream)`
    RequestHeaders(i32, i32, i32),
    /// `proxy_on_request_body(context_id, body_size, 1)`（本文の全量。F-329）。要求がまだ続く（ローカル応答が決まって
    /// おらず、保留の外部呼び出しが無い）ときだけ呼ぶ。呼ぶ前に、燃料と締切を入れ直し、上流へ送る要求ヘッダを
    /// [`Chain::upstream_headers`] へ移してその写しを context に置き（本文の段のヘッダの書き換えは上流へ届かない）、
    /// [`call_chain`] に預けた本文を置く（ヘッダの段からは本文が見えない）。別の呼び出しで呼んでいたときと同じ
    RequestBody(i32, i32),
    /// `proxy_on_log(context_id)`
    Log(i32),
    /// ローカル応答の要求の `proxy_on_log(context_id)`（F-329）。ローカル応答が決まっていて、保留の外部呼び出しが無い
    /// ときだけ呼ぶ。呼ぶ前に、燃料と締切を入れ直し、ローカル応答を [`Chain::local`] へ取り出し（ログの段で
    /// `proxy_send_local_response` を呼ばれても決まった応答は変わらない）、context を応答の後のログの段と同じ見え方に
    /// する（[`super::context::HttpContext::enter_log`]）
    LogLocal(i32),
    /// `proxy_on_delete(context_id)`。前に段があれば燃料と締切を入れ直してから呼ぶ（別に呼んでいたときと同じ）
    Delete(i32),
}

/// [`call_chain`] の結果
pub(crate) struct Chain {
    /// 終わった（飛ばしたものを含む）段の数。`error` があれば、この番号の段（段に入る前の失敗も含む）で止まった。
    /// `error` が無く段の数より小さければ、この番号の段（`RequestBody`・`LogLocal`）の条件を満たさずにそこで止めた
    /// （以後の段は呼んでいない）
    pub(crate) done: usize,
    /// 条件を満たさずに飛ばした段（ビット `i` が段 `i`。`RequestBody` はローカル応答が決まっていれば飛ばして次の段へ進む）
    pub(crate) skipped: u8,
    /// 段の戻り値（`proxy_on_request_headers`・`proxy_on_request_body` の action。戻り値の無い段・エクスポートの無い段は `None`）
    pub(crate) values: [Option<i32>; MAX_STEPS],
    /// 段を終えた時点で、最後に上限を入れてから使った燃料（`fuel` のときだけ。段の燃料のメトリクス）
    pub(crate) fuel: [Option<u64>; MAX_STEPS],
    pub(crate) error: Option<anyhow::Error>,
    /// `RequestBody` の段に入ったときの、上流へ送る要求ヘッダ（ヘッダの段の後のもの）
    pub(crate) upstream_headers: Option<Vec<(Vec<u8>, Vec<u8>)>>,
    /// `LogLocal` の段に入ったときの、決まっていたローカル応答
    pub(crate) local: Option<LocalResponse>,
}

impl Chain {
    fn failed(e: anyhow::Error) -> Self {
        Chain {
            done: 0,
            skipped: 0,
            values: [None; MAX_STEPS],
            fuel: [None; MAX_STEPS],
            error: Some(e),
            upstream_headers: None,
            local: None,
        }
    }

    /// 段 `i` を呼んだ（終えたか、そこで失敗した）か
    pub(crate) fn entered(&self, i: usize) -> bool {
        (self.done > i || (self.done == i && self.error.is_some())) && self.skipped & (1 << i) == 0
    }

    /// 段 `i` を呼んで終えたか
    pub(crate) fn completed(&self, i: usize) -> bool {
        self.done > i && self.skipped & (1 << i) == 0
    }
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
/// - `RequestBody`・`LogLocal`・`Delete`（最初の段でなければ）の前に燃料と締切を `arm`（fuel, エポックの締切）で入れ直す
///   （別に呼んでいたときと同じ上限）。
/// - `RequestBody`・`LogLocal` は条件つきの段（[`Step`]）。満たさなければそこで止める（エラーではない）。
/// - `body` は `RequestBody` の段で context に置く本文。
pub(crate) async fn call_chain(
    store: &mut Store<HostState>,
    instance: Instance,
    steps: &[Step],
    arm: (Limits, Window),
    body: Option<bytes::Bytes>,
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
            Step::RequestBody(..) => resolve(
                store,
                instance,
                |e| &mut e.on_request_body,
                "proxy_on_request_body",
            ),
            Step::Log(_) | Step::LogLocal(_) => {
                resolve(store, instance, |e| &mut e.on_log, "proxy_on_log")
            }
            Step::Delete(_) => resolve(store, instance, |e| &mut e.on_delete, "proxy_on_delete"),
        }
    }
    let chain = match std::mem::take(&mut store.data_mut().exports.chain) {
        Export::Present(f) => f,
        _ => match Func::wrap(&mut *store, run_chain).typed::<(), ()>(&*store) {
            Ok(f) => f,
            Err(e) => return Chain::failed(e),
        },
    };
    {
        let io = &mut store.data_mut().exports.chain_io;
        *io = ChainIo::default();
        for (slot, step) in io.steps.iter_mut().zip(steps) {
            *slot = Some(*step);
        }
        io.arm = Some(arm);
        io.body = body;
    }
    let r = chain.call_async(&mut *store, ()).await;
    let e = &mut store.data_mut().exports;
    e.chain = Export::Present(chain);
    let io = std::mem::take(&mut e.chain_io);
    Chain {
        done: io.done,
        skipped: io.skipped,
        values: io.values,
        fuel: io.fuel,
        error: r.err(),
        upstream_headers: io.upstream_headers,
        local: io.local,
    }
}

/// [`call_chain`] のホスト関数（ファイバーの上で段を続けて呼ぶ）
fn run_chain(mut caller: Caller<'_, HostState>) -> anyhow::Result<()> {
    let steps = caller.data().exports.chain_io.steps;
    let arm = caller.data().exports.chain_io.arm;
    for (i, step) in steps.into_iter().enumerate() {
        let Some(step) = step else {
            break;
        };
        let rearm = match step {
            Step::RequestBody(..) | Step::LogLocal(_) => match enter_step(&mut caller, step) {
                Enter::Run => true,
                Enter::Pass => {
                    // 呼ばずに終えた（燃料のメトリクスは別に呼んでいたときと同じく 0）
                    let io = &mut caller.data_mut().exports.chain_io;
                    io.fuel[i] = arm.and_then(|(limits, _)| limits.uses_fuel().then_some(0));
                    io.done = i + 1;
                    continue;
                }
                Enter::Skip => {
                    let io = &mut caller.data_mut().exports.chain_io;
                    io.skipped |= 1 << i;
                    io.done = i + 1;
                    continue;
                }
                Enter::Stop => break,
            },
            // エクスポートが無ければ呼ばないので入れ直さない
            Step::Delete(_) => i > 0 && present(&caller.data().exports.on_delete).is_some(),
            _ => false,
        };
        if rearm {
            if let Some((limits, window)) = arm {
                super::metering::arm(&mut caller, &limits, window)?;
            }
        }
        let r = run_step(&mut caller, step);
        // 段が失敗しても燃料は記録する（本文の段を別に呼んでいたときはトラップでも記録した）
        let fuel = arm.and_then(|(limits, _)| super::metering::fuel_consumed(&caller, &limits));
        let io = &mut caller.data_mut().exports.chain_io;
        io.fuel[i] = fuel;
        io.values[i] = r?;
        io.done = i + 1;
    }
    Ok(())
}

/// 条件つきの段をどうするか（[`enter_step`]）
enum Enter {
    /// 呼ぶ（context は段の前の状態にした）
    Run,
    /// 段を終えたことにして次の段へ進む（エクスポートが無い。別に呼んでいたときも何もせずに続けた）
    Pass,
    /// 呼ばずに次の段へ進む（段を終えたことにはしない）
    Skip,
    /// ここで止める（以後の段も呼ばない）
    Stop,
}

/// 条件つきの段（[`Step::RequestBody`]・[`Step::LogLocal`]）の条件を見て、呼ぶなら context を段の前の状態にする
/// （止める・飛ばすなら何も変えない）。
///
/// - 保留の外部呼び出しがあれば止める（呼び出し元が今の経路で解決して続ける）。ゲストが要求のパス・メソッドを書き換えて
///   いても止める（別に呼んでいたときは段ごとに元の値へ戻っていた。今の経路で続けて同じ見え方にする）。
/// - 本文の段は、ローカル応答が決まっていれば飛ばす（続くログの段がその応答を記録する）。
/// - エクスポートの無い段は、context を作り直さず（ヘッダを写さず）に終えたことにする。
fn enter_step(caller: &mut Caller<'_, HostState>, step: Step) -> Enter {
    let state = caller.data_mut();
    let ctx = &mut state.http_ctx;
    if ctx.has_pending_calls() || ctx.request_line_overridden {
        return Enter::Stop;
    }
    let exports = &mut state.exports;
    match step {
        Step::RequestBody(..) => {
            if ctx.local_response.is_some() {
                return Enter::Skip;
            }
            if exports.chain_io.body.is_none() {
                return Enter::Stop;
            }
            // 上流へ送るのはヘッダの段の後のヘッダ
            let headers = std::mem::take(&mut ctx.request_headers);
            if present(&exports.on_request_body).is_none() {
                // ヘッダの段で本文のバッファに書いていても、本文は変えていない
                ctx.request_body_modified = false;
                exports.chain_io.body = None;
                exports.chain_io.upstream_headers = Some(headers);
                return Enter::Pass;
            }
            let body = exports.chain_io.body.take().unwrap_or_default();
            // 本文の段には写しを見せる（別に呼んでいたときの `to_vec()` と同じ。本文の段でのヘッダの書き換えは上流へ
            // 届かない）
            ctx.enter_request_body(headers.clone(), body);
            exports.chain_io.upstream_headers = Some(headers);
            Enter::Run
        }
        Step::LogLocal(_) => {
            // 決まった応答は取り出しておく（ログの段の `proxy_send_local_response` は応答を変えない。別に呼んでいたときも
            // 応答を書いた後で、捨てられていた）
            let Some(local) = ctx.local_response.take() else {
                return Enter::Stop;
            };
            exports.chain_io.local = Some(local);
            if present(&exports.on_log).is_none() {
                // 続く削除の段にもログの段と同じ見え方を渡す（別に呼んでいたときは応答の後の 1 回の呼び出しで、
                // ログ・削除とも要求の属性だけが見えた）。どちらも無ければ context を作り直さない
                if present(&exports.on_delete).is_some() {
                    ctx.enter_log();
                }
                return Enter::Pass;
            }
            ctx.enter_log();
            Enter::Run
        }
        _ => Enter::Run,
    }
}

/// 1 つの段を今のファイバーの上で呼ぶ（エクスポートが無ければ呼ばずに `None`）
fn run_step(caller: &mut Caller<'_, HostState>, step: Step) -> anyhow::Result<Option<i32>> {
    let e = &caller.data().exports;
    let (func, args, params, result) = match step {
        Step::ContextCreate(c, p) => (present(&e.on_context_create), [c, p, 0], 2, false),
        Step::RequestHeaders(c, n, eos) => (present(&e.on_request_headers), [c, n, eos], 3, true),
        Step::RequestBody(c, n) => (present(&e.on_request_body), [c, n, 1], 3, true),
        Step::Log(c) | Step::LogLocal(c) => (present(&e.on_log), [c, 0, 0], 1, false),
        Step::Delete(c) => (present(&e.on_delete), [c, 0, 0], 1, false),
    };
    let Some(func) = func else {
        return Ok(None);
    };
    let mut vals = args.map(ValRaw::i32);
    let vals: &mut [ValRaw] = &mut vals[..params];
    // SAFETY: `func` はこのストアのインスタンスのエクスポートで、段の型（`ContextCreate` は `(i32, i32) -> ()`、
    // `RequestHeaders`・`RequestBody` は `(i32, i32, i32) -> i32`、`Log`・`LogLocal`・`Delete` は `(i32) -> ()`）であることを
    // `typed` で確かめてある
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

    /// 呼ばれた順を `trace` に 10 進で積むモジュール（context_create=1・request_headers=2・log=3・delete=4・request_body=5）。
    /// `proxy_on_request_headers` は context の ID を、`proxy_on_request_body` は `本文の長さ * 10 + end_of_stream` を action
    /// として返し、`proxy_on_log` は context 9 でトラップする。
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
      (func (export "proxy_on_request_body") (param $c i32) (param $n i32) (param $eos i32) (result i32)
        (call $spin) (call $push (i32.const 5))
        (i32.add (i32.mul (local.get $n) (i32.const 10)) (local.get $eos)))
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

    /// fuel の方式の上限（燃料 `fuel`・燃料 `fuel_yield` ごとに譲る）
    fn fuel_arm(fuel: u64, fuel_yield: u64) -> (Limits, Window) {
        (Limits::with_fuel(fuel, fuel_yield), Window::Callback)
    }

    fn arm() -> (Limits, Window) {
        fuel_arm(1_000_000_000, 100)
    }

    // F-319: 段は順に 1 回のファイバーの出入りで呼ばれ、`proxy_on_request_headers` の action が返る。fuel の区切りで
    // 止まっても続きから再開する。ホスト関数は 2 回目以降は作り直さない
    #[test]
    fn chain_runs_the_steps_in_order_and_resumes_after_yields() {
        let (mut store, instance) = chain_instance(100);
        let steps = [Step::ContextCreate(5, 1), Step::RequestHeaders(5, 3, 1)];
        let (chain, pending) = poll_to_end(call_chain(&mut store, instance, &steps, arm(), None));
        assert!(chain.error.is_none(), "{:?}", chain.error);
        assert_eq!(chain.done, 2);
        assert_eq!(chain.values[..2], [None, Some(5)]);
        assert!(pending > 0, "the steps must yield on the fuel interval");
        assert_eq!(trace(&mut store, instance), 12);
        assert!(matches!(store.data().exports.chain, Export::Present(_)));

        let steps = [Step::Log(5), Step::Delete(5)];
        let (chain, _) = poll_to_end(call_chain(&mut store, instance, &steps, arm(), None));
        assert!(chain.error.is_none());
        assert_eq!(chain.done, 2);
        assert_eq!(trace(&mut store, instance), 1234);
    }

    // 最初のエラーで止め、後の段は呼ばない（`done` は止まった段の番号）
    #[test]
    fn chain_stops_at_the_first_error() {
        let (mut store, instance) = chain_instance(1_000_000);
        let steps = [Step::ContextCreate(9, 1), Step::Log(9), Step::Delete(9)];
        let (chain, _) = poll_to_end(call_chain(&mut store, instance, &steps, arm(), None));
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
        let (chain, _) = poll_to_end(call_chain(&mut store, instance, &steps, arm(), None));
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
        let (chain, _) = poll_to_end(call_chain(
            &mut store,
            instance,
            &[Step::Log(5)],
            arm(),
            None,
        ));
        assert!(chain.error.is_none());
        let fuel = (1_000_000_000 - store.get_fuel().unwrap()) * 3 / 2;
        let reset = instance
            .get_typed_func::<(), ()>(&mut store, "reset")
            .unwrap();
        poll_to_end(reset.call_async(&mut store, ())).0.unwrap();

        store.set_fuel(fuel).unwrap();
        let steps = [Step::Log(5), Step::Delete(5)];
        let (chain, _) = poll_to_end(call_chain(
            &mut store,
            instance,
            &steps,
            fuel_arm(fuel, 1_000_000),
            None,
        ));
        assert!(chain.error.is_none(), "{:?}", chain.error);
        assert_eq!(trace(&mut store, instance), 34);

        // 入れ直さなければ 2 段目で燃料が尽きる
        store.set_fuel(1_000_000).unwrap();
        poll_to_end(reset.call_async(&mut store, ())).0.unwrap();
        store.set_fuel(fuel).unwrap();
        let steps = [Step::Log(5), Step::Log(6)];
        let (chain, _) = poll_to_end(call_chain(
            &mut store,
            instance,
            &steps,
            fuel_arm(fuel, 1_000_000),
            None,
        ));
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
            let mut f = Box::pin(call_chain(&mut store, instance, &steps, arm(), None));
            let waker = futures::task::noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            assert!(std::future::Future::poll(f.as_mut(), &mut cx).is_pending());
        }
        // 取り出していたホスト関数は戻らない（次に作り直す）
        assert!(!matches!(store.data().exports.chain, Export::Present(_)));
        let steps = [Step::Log(5)];
        let (chain, _) = poll_to_end(call_chain(&mut store, instance, &steps, arm(), None));
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

    fn local(status: u16) -> crate::wasm::types::LocalResponse {
        crate::wasm::types::LocalResponse {
            status_code: status,
            headers: vec![(b"x".to_vec(), b"y".to_vec())],
            body: b"denied".to_vec(),
        }
    }

    // F-329: 本文の段は預けた本文を全量（end_of_stream = 1）で見る。context はヘッダの段で置いた値を消して本文の段を別に
    // 呼んでいたときと同じ見え方にし（ヘッダは写し）、上流へ送るヘッダ（ヘッダの段の後）は別に返す
    #[test]
    fn chain_body_step_sees_the_body_like_a_separate_call() {
        let (mut store, instance) = chain_instance(100);
        {
            let ctx = &mut store.data_mut().http_ctx;
            ctx.request_headers = vec![(b"a".to_vec(), b"1".to_vec())];
            ctx.request_headers_modified = true;
            ctx.client_ip = std::sync::Arc::from("192.0.2.1");
            ctx.custom_properties.insert("k".into(), b"v".to_vec());
        }
        let steps = [Step::RequestHeaders(5, 1, 0), Step::RequestBody(5, 3)];
        let body = bytes::Bytes::from_static(b"abc");
        let (chain, pending) =
            poll_to_end(call_chain(&mut store, instance, &steps, arm(), Some(body)));
        assert!(chain.error.is_none(), "{:?}", chain.error);
        assert!(pending > 0);
        assert_eq!(chain.done, 2);
        assert_eq!(chain.values[..2], [Some(5), Some(31)]);
        assert!(chain.completed(0) && chain.completed(1) && chain.entered(1));
        assert!(chain.fuel[0].is_some_and(|f| f > 0) && chain.fuel[1].is_some_and(|f| f > 0));
        assert_eq!(
            chain.upstream_headers,
            Some(vec![(b"a".to_vec(), b"1".to_vec())])
        );
        assert!(chain.local.is_none());
        assert_eq!(trace(&mut store, instance), 25);
        let ctx = &store.data().http_ctx;
        assert_eq!(ctx.request_body.as_slice(), b"abc");
        assert!(ctx.request_body_complete);
        assert_eq!(ctx.request_headers, vec![(b"a".to_vec(), b"1".to_vec())]);
        assert!(!ctx.request_headers_modified && ctx.custom_properties.is_empty());
        assert_eq!(&*ctx.client_ip, "192.0.2.1");
        assert_eq!(ctx.context_id, 1);
    }

    // ローカル応答が決まっていれば本文の段は飛ばし、ログの段は応答を取り出してから呼ぶ（context は要求の属性だけ）。
    // 削除の段も続けて呼ぶ
    #[test]
    fn chain_skips_the_body_after_a_local_response_and_logs_it() {
        let (mut store, instance) = chain_instance(1_000_000);
        {
            let ctx = &mut store.data_mut().http_ctx;
            ctx.local_response = Some(local(401));
            ctx.request_headers = vec![(b"a".to_vec(), b"1".to_vec())];
            ctx.client_ip = std::sync::Arc::from("192.0.2.1");
            ctx.set_request_line(std::sync::Arc::from("POST"), std::sync::Arc::from("/p?q=1"));
        }
        let steps = [Step::RequestBody(5, 3), Step::LogLocal(5), Step::Delete(5)];
        let (chain, _) = poll_to_end(call_chain(
            &mut store,
            instance,
            &steps,
            arm(),
            Some(bytes::Bytes::from_static(b"abc")),
        ));
        assert!(chain.error.is_none(), "{:?}", chain.error);
        assert_eq!(chain.done, 3);
        assert!(!chain.entered(0) && !chain.completed(0));
        assert!(chain.completed(1) && chain.completed(2));
        assert!(chain.upstream_headers.is_none());
        let l = chain.local.expect("the local response is taken out");
        assert_eq!((l.status_code, &l.body[..]), (401, &b"denied"[..]));
        assert_eq!(trace(&mut store, instance), 34);
        let ctx = &store.data().http_ctx;
        assert!(ctx.local_response.is_none() && ctx.request_headers.is_empty());
        assert!(ctx.client_ip.is_empty() && ctx.request_body.is_empty());
        assert_eq!(
            (&*ctx.request_method, &*ctx.request_path),
            ("POST", "/p?q=1")
        );
        assert_eq!(ctx.response_status, 0);
    }

    // 保留の外部呼び出しがあれば条件つきの段の前で止める（呼び出し元が今の経路で解決して続ける）。本文も context も変えない
    #[test]
    fn chain_stops_before_conditional_steps_while_calls_are_pending() {
        let (mut store, instance) = chain_instance(1_000_000);
        store.data_mut().http_ctx.pending_http_calls.insert(
            7,
            crate::wasm::types::PendingHttpCall {
                token: 7,
                upstream: "up".into(),
                timeout_ms: 1000,
                headers: Vec::new(),
                body: Vec::new(),
                trailers: Vec::new(),
            },
        );
        store.data_mut().http_ctx.local_response = Some(local(403));
        let steps = [
            Step::RequestHeaders(5, 1, 0),
            Step::RequestBody(5, 3),
            Step::LogLocal(5),
            Step::Delete(5),
        ];
        let (chain, _) = poll_to_end(call_chain(
            &mut store,
            instance,
            &steps,
            arm(),
            Some(bytes::Bytes::from_static(b"abc")),
        ));
        assert!(chain.error.is_none(), "{:?}", chain.error);
        assert_eq!(chain.done, 1);
        assert!(!chain.entered(1) && !chain.entered(2));
        assert!(chain.local.is_none() && chain.upstream_headers.is_none());
        assert_eq!(trace(&mut store, instance), 2);
        let ctx = &store.data().http_ctx;
        assert!(ctx.local_response.is_some() && ctx.request_body.is_empty());
    }

    // ローカル応答が無ければログの段で止める（後の削除の段も呼ばない）。本文が無ければ本文の段で止める
    #[test]
    fn chain_stops_at_conditional_steps_without_their_input() {
        let (mut store, instance) = chain_instance(1_000_000);
        let steps = [
            Step::RequestHeaders(5, 1, 0),
            Step::LogLocal(5),
            Step::Delete(5),
        ];
        let (chain, _) = poll_to_end(call_chain(&mut store, instance, &steps, arm(), None));
        assert!(chain.error.is_none());
        assert_eq!(chain.done, 1);
        assert!(chain.local.is_none());
        assert_eq!(trace(&mut store, instance), 2);

        let steps = [Step::RequestBody(5, 3), Step::Delete(5)];
        let (chain, _) = poll_to_end(call_chain(&mut store, instance, &steps, arm(), None));
        assert!(chain.error.is_none());
        assert_eq!(chain.done, 0);
        assert!(!chain.entered(0));
        assert_eq!(trace(&mut store, instance), 2);
    }

    // ログの段のトラップ: 応答は取り出してあり、`done` はログの段（削除の段は呼ばない）
    #[test]
    fn chain_log_local_trap_keeps_the_local_response() {
        let (mut store, instance) = chain_instance(1_000_000);
        store.data_mut().http_ctx.local_response = Some(local(429));
        let steps = [
            Step::RequestHeaders(9, 1, 1),
            Step::LogLocal(9),
            Step::Delete(9),
        ];
        let (chain, _) = poll_to_end(call_chain(&mut store, instance, &steps, arm(), None));
        assert!(chain.error.is_some());
        assert_eq!(chain.done, 1);
        assert!(chain.entered(1) && !chain.completed(1));
        assert_eq!(chain.local.map(|l| l.status_code), Some(429));
        assert_eq!(trace(&mut store, instance), 2);
    }

    // エクスポートの無い段は呼ばずに終えたことにする: 本文の段は context を作り直さずに上流のヘッダを返し、ログの段は
    // 応答を取り出すだけ（どちらも context を作り直さない）
    #[test]
    fn chain_passes_steps_without_exports() {
        let (mut store, instance) = chain_instance(1_000_000);
        store.data_mut().exports.on_request_body = Export::Absent;
        store.data_mut().exports.on_log = Export::Absent;
        {
            let ctx = &mut store.data_mut().http_ctx;
            ctx.request_headers = vec![(b"a".to_vec(), b"1".to_vec())];
            ctx.client_ip = std::sync::Arc::from("192.0.2.1");
            // ヘッダの段で本文のバッファに書いていても本文は変えていない
            ctx.request_body_modified = true;
        }
        let steps = [Step::RequestHeaders(5, 1, 0), Step::RequestBody(5, 3)];
        let (chain, _) = poll_to_end(call_chain(
            &mut store,
            instance,
            &steps,
            arm(),
            Some(bytes::Bytes::from_static(b"abc")),
        ));
        assert!(chain.error.is_none(), "{:?}", chain.error);
        assert_eq!(chain.done, 2);
        assert!(chain.completed(1));
        assert_eq!(chain.values[..2], [Some(5), None]);
        assert_eq!(chain.fuel[1], Some(0));
        assert_eq!(
            chain.upstream_headers,
            Some(vec![(b"a".to_vec(), b"1".to_vec())])
        );
        assert_eq!(trace(&mut store, instance), 2);
        let ctx = &store.data().http_ctx;
        assert!(!ctx.request_body_modified && ctx.request_body.is_empty());
        assert_eq!(&*ctx.client_ip, "192.0.2.1");

        // ログの段が無くても、削除の段はログの段と同じ見え方（要求の属性だけ）で呼ぶ
        store.data_mut().http_ctx.local_response = Some(local(401));
        let steps = [Step::LogLocal(5), Step::Delete(5)];
        let (chain, _) = poll_to_end(call_chain(&mut store, instance, &steps, arm(), None));
        assert!(chain.error.is_none());
        assert_eq!(chain.done, 2);
        assert_eq!(chain.local.map(|l| l.status_code), Some(401));
        assert_eq!(trace(&mut store, instance), 24);
        assert!(store.data().http_ctx.client_ip.is_empty());

        // ログ・削除とも無ければ context を作り直さない
        store.data_mut().exports.on_delete = Export::Absent;
        {
            let ctx = &mut store.data_mut().http_ctx;
            ctx.client_ip = std::sync::Arc::from("192.0.2.1");
            ctx.local_response = Some(local(402));
        }
        let (chain, _) = poll_to_end(call_chain(&mut store, instance, &steps, arm(), None));
        assert!(chain.error.is_none());
        assert_eq!(chain.done, 2);
        assert_eq!(chain.local.map(|l| l.status_code), Some(402));
        // 何も呼んでいない（記録は前のまま）
        assert_eq!(trace(&mut store, instance), 24);
        assert_eq!(&*store.data().http_ctx.client_ip, "192.0.2.1");
    }

    // ゲストが要求のパス・メソッドを書き換えたら条件つきの段の前で止める（呼び出し元の今の経路が段ごとに元の値へ戻す）
    #[test]
    fn chain_stops_after_the_request_line_is_overridden() {
        let (mut store, instance) = chain_instance(1_000_000);
        store.data_mut().http_ctx.request_line_overridden = true;
        let steps = [Step::RequestHeaders(5, 1, 0), Step::RequestBody(5, 3)];
        let (chain, _) = poll_to_end(call_chain(
            &mut store,
            instance,
            &steps,
            arm(),
            Some(bytes::Bytes::from_static(b"abc")),
        ));
        assert!(chain.error.is_none());
        assert_eq!(chain.done, 1);
        assert!(chain.upstream_headers.is_none());
        assert_eq!(trace(&mut store, instance), 2);
    }

    // 本文の段の前にも燃料を入れ直す（ヘッダの段で使い切っても本文の段は別に呼んだときと同じ上限で走る）
    #[test]
    fn chain_rearms_fuel_before_the_body_step() {
        let (mut store, instance) = chain_instance(1_000_000);
        let (chain, _) = poll_to_end(call_chain(
            &mut store,
            instance,
            &[Step::RequestHeaders(5, 1, 0)],
            arm(),
            None,
        ));
        assert!(chain.error.is_none());
        let fuel = (1_000_000_000 - store.get_fuel().unwrap()) * 3 / 2;
        store.set_fuel(fuel).unwrap();
        let steps = [Step::RequestHeaders(5, 1, 0), Step::RequestBody(5, 3)];
        let (chain, _) = poll_to_end(call_chain(
            &mut store,
            instance,
            &steps,
            fuel_arm(fuel, 1_000_000),
            Some(bytes::Bytes::from_static(b"abc")),
        ));
        assert!(chain.error.is_none(), "{:?}", chain.error);
        assert_eq!(chain.done, 2);
    }
}
