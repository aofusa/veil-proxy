//! Filter Engine for executing WASM modules
//!
//! スレッドローカルインスタンスプールによる高性能 WASM 実行エンジン。
//! 各ワーカースレッドが専用のモジュールキャッシュを持つことで、
//! Arc デリファレンス・HashMap ルックアップのオーバーヘッドを削減する。

use std::sync::Arc;

use wasmtime::Store;

use super::constants::{ACTION_CONTINUE, ACTION_PAUSE};
use super::context::{HostState, HttpContext};
use super::instance_pool::{InstanceSlot, PooledInstance, RequestSessions};
use super::registry::{LoadedModule, ModuleRegistry};
use super::types::{FilterAction, LocalResponse, WasmConfig};
use crate::wasm_plugin_config::ModuleRef;

/// `modules`（ルート単位で解決済みの `ModuleRef` リスト）から、レジストリに実際に
/// ロードされているモジュールと、その実効 plugin configuration（ルート上書きが
/// あればそれ、無ければモジュール定義の値）の組を集める。
///
/// 実効設定の解決はここで `Arc` clone のみ（参照カウント増加）で行い、
/// バイト列のディープコピーは発生しない（F-148）。設定ロード時に既に
/// `to_bytes()` 済みのバイト列を持ち回るだけ。
fn resolve_effective_modules(
    registry: &ModuleRegistry,
    modules: &[ModuleRef],
) -> Vec<(Arc<LoadedModule>, Arc<[u8]>)> {
    modules
        .iter()
        .filter_map(|r| {
            registry.get_module(&r.name).map(|m| {
                let config = r
                    .configuration
                    .clone()
                    .unwrap_or_else(|| m.configuration.clone());
                (m, config)
            })
        })
        .collect()
}

/// 失敗の種類（F-293）。トラップ以外の失敗に付けてメトリクス・ログの `reason` にする
#[derive(Debug, Clone, Copy)]
pub(crate) struct FailKind(pub &'static str);

impl std::fmt::Display for FailKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// 失敗の理由（F-293）: `FailKind` が付いていればそれ、wasmtime のトラップなら fuel 切れ・時間切れ・その他を分ける
fn failure_reason(e: &anyhow::Error) -> &'static str {
    if let Some(k) = e.downcast_ref::<FailKind>() {
        return k.0;
    }
    match e.downcast_ref::<wasmtime::Trap>() {
        Some(wasmtime::Trap::OutOfFuel) => "fuel",
        Some(wasmtime::Trap::Interrupt) => "timeout",
        _ => "trap",
    }
}

/// 失敗した要求に返す応答（F-293。gRPC のクライアントは HTTP の 503 を UNAVAILABLE として扱う）
fn failure_response() -> LocalResponse {
    LocalResponse {
        status_code: 503,
        headers: Vec::new(),
        body: Vec::new(),
    }
}

/// コールバックの結果（F-293・B-299・B-300。[`FilterEngine::settle`]）
enum Settled {
    Continue,
    /// インラインで解決できる呼び出しが無いまま Pause した。`grpc` は gRPC の呼び出しを待っている
    /// （veil はその場で待てない＝守れない Pause）
    Pause {
        grpc: bool,
    },
    Local(LocalResponse),
    Failed(anyhow::Error),
}

/// インスタンス枠が尽きたときに待つ上限（1+2+…+64 ≒ 127ms。B-299）
const INSTANTIATE_RETRY_MAX_MS: u64 = 64;

/// Filter engine for executing WASM modules
pub struct FilterEngine {
    /// Module registry
    registry: ModuleRegistry,
    /// Default execution time limit (fuel)
    fuel_limit: u64,
    /// fuel を使い切る前にワーカーへ制御を返す間隔（`[wasm.defaults] yield_interval_us` × 1,000。
    /// fuel は約 1M で 1ms で、`max_execution_time_ms` の換算と同じ）。
    ///
    /// B-175: 以前は 10,000（約 10µs）に固定していた。数十 µs の普通のコールバックでも何度も止まり、同じ
    /// ワーカーの他の要求のコールバックが割り込むので、1 ワーカーに同時に開いているインスタンスが接続数近くまで増え、
    /// `instance_reuse` のプールに戻せなかったインスタンスを要求ごとに作り直していた（JSON の 1 要求で
    /// 700µs 超）。既定の 1ms ごとでも CPU を長く使うフィルタは 1ms ごとに他の要求へ譲る（協調的な yield は保つ）。
    fuel_yield_interval: u64,
    /// Epoch deadline for timeout enforcement
    ///
    /// B-173: エポックは時計。`registry::spawn_epoch_ticker` の専用スレッドが `EPOCH_TICK`（100ms）ごとに
    /// 1 進める（以前は各呼び出しの直前に 1 進める「全スレッドの WASM 呼び出し回数」で、ワーカーが複数あると
    /// 他のスレッドの 10 回の呼び出しの間に終わらない呼び出しが負荷の下で `interrupt` でトラップし、
    /// フィルタが飛ばされていた）。各 store はコールバックの前に `current_epoch + epoch_deadline` を締切にする
    /// ので、1 回のコールバックの実時間の上限は約 1〜1.1 秒（CPU の上限は fuel）。
    /// F-62 の Pause/resume（`resolve_pending_http_calls_inline`）のように store を止めたまま外部 I/O を
    /// 待つ経路では、再開の直前に締切を引き直す（B-56）。
    epoch_deadline: u64,
}

impl FilterEngine {
    /// Create a new filter engine
    pub fn new(config: &WasmConfig) -> anyhow::Result<Self> {
        let registry = ModuleRegistry::new(config)?;

        // Calculate fuel limit (roughly 1M instructions per ms)
        let fuel_limit = config.defaults.max_execution_time_ms * 1_000_000;

        // Epoch deadline: ticks after the current engine epoch before timeout.
        // The ticker thread (registry::spawn_epoch_ticker) increments the epoch every 100ms, so deadline=10
        // allows about 1s. Fuel consumption is the primary timeout mechanism for CPU-bound limits.
        let epoch_deadline = 10;
        let fuel_yield_interval = config
            .defaults
            .yield_interval_us
            .max(1)
            .saturating_mul(1_000);

        Ok(Self {
            registry,
            fuel_limit,
            fuel_yield_interval,
            epoch_deadline,
        })
    }

    /// Execute on_request_headers callback for all modules (deprecated — no-op)
    pub fn on_request_headers(
        &self,
        _path: &str,
        _method: &str,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        _client_ip: &str,
        _end_of_stream: bool,
    ) -> FilterResult {
        FilterResult::Continue {
            headers,
            body: None,
        }
    }

    /// Execute on_request_headers for specified modules
    ///
    /// F-43: `path`/`method`/`client_ip` は `Arc<str>` 共有（per-module の `to_string` 排除）、
    /// ヘッダは所有権ムーブスルー（per-module の deep copy 排除）。
    pub async fn on_request_headers_with_modules(
        &self,
        modules: &[ModuleRef],
        path: &Arc<str>,
        method: &Arc<str>,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        client_ip: &Arc<str>,
        end_of_stream: bool,
        sessions: Option<&RequestSessions>,
    ) -> FilterResult {
        let resolved = resolve_effective_modules(&self.registry, modules);

        if resolved.is_empty() {
            return FilterResult::Continue {
                headers,
                body: None,
            };
        }

        let mut current_headers = headers;

        for (module, config) in &resolved {
            let (returned, result) = self
                .execute_on_request_headers(
                    module,
                    config,
                    path,
                    method,
                    current_headers,
                    client_ip,
                    end_of_stream,
                    sessions,
                )
                .await;
            // ヘッダは変更有無に関わらず回収済み（ムーブスルー）。
            current_headers = returned;

            match result {
                Ok(ModuleAction::Continue) => {}
                Ok(ModuleAction::Pause { grpc }) => {
                    // 本文を待つ Pause（Envoy の StopIteration）は本文の段で続きを決めるので守れる。
                    // gRPC の呼び出しの結果・本文の無い要求の Pause は待てない（B-300）
                    if grpc || end_of_stream || !module.capabilities.allow_request_body_read {
                        if let Some(resp) = self.record_failure(
                            module,
                            "request_headers",
                            "unsupported_pause",
                            None,
                        ) {
                            return FilterResult::LocalResponse(resp);
                        }
                    }
                }
                Ok(ModuleAction::LocalResponse(resp)) => {
                    return FilterResult::LocalResponse(resp);
                }
                Err(e) => {
                    if let Some(resp) = self.on_failure(module, "request_headers", &e) {
                        return FilterResult::LocalResponse(resp);
                    }
                }
            }
        }

        FilterResult::Continue {
            headers: current_headers,
            body: None,
        }
    }

    /// Execute on_request_headers for specified modules ASYNCHRONOUSLY
    /// Note: runs inline in the current async task to avoid cross-thread waker issues with the thread-per-core runtime.
    ///
    /// F-43/F-148: `modules` は `Arc` 共有（呼び出しごとの `Vec<ModuleRef>` deep copy 排除）。
    pub async fn on_request_headers_with_modules_async(
        self: Arc<Self>,
        modules: crate::wasm_plugin_config::WasmModules,
        path: Arc<str>,
        method: Arc<str>,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        client_ip: Arc<str>,
        end_of_stream: bool,
    ) -> FilterResult {
        // 呼び出し元の future に本体（数 KB）を埋め込まない（Wasm を使わない要求の move が重くなる。B-240）
        Box::pin(self.on_request_headers_with_modules(
            &modules,
            &path,
            &method,
            headers,
            &client_ip,
            end_of_stream,
            modules.sessions(),
        ))
        .await
    }

    /// 読み込んだ各モジュールの起動時のコールバック（`_start` → `on_vm_start` → `on_configure`）を 1 回走らせ、
    /// トラップ・false なら設定を拒否する（Envoy と同じ。B-301）。起動・リロード・ECDS の適用で呼ぶ
    /// （データプレーンの外。リロードで失敗したら前の設定のまま）。
    pub fn validate(&self) -> anyhow::Result<()> {
        for module in self.registry.modules() {
            let mut store = Store::new(
                self.registry.engine(),
                HostState::new(HttpContext::for_module(1, module)),
            );
            self.arm_store(&mut store)?;
            // 起動・リロードのスレッドで 1 回だけ（ワーカーのランタイムの外）
            futures::executor::block_on(async {
                let instance = module.instance_pre.instantiate_async(&mut store).await?;
                self.run_lifecycle(&mut store, instance, &module.configuration)
                    .await
            })
            .map_err(|e| {
                anyhow::anyhow!("wasm module '{}' failed to start: {:#}", module.name, e)
            })?;
        }
        Ok(())
    }

    /// Store に 1 回のコールバックの燃料と締切を入れる
    fn arm_store(&self, store: &mut Store<HostState>) -> anyhow::Result<()> {
        store.set_fuel(self.fuel_limit)?;
        store.fuel_async_yield_interval(Some(self.fuel_yield_interval))?;
        store.set_epoch_deadline(self.epoch_deadline);
        Ok(())
    }

    /// 新しいインスタンスを作り、Proxy-Wasm SDK のライフサイクル（_start → root コンテキスト生成 → vm_start →
    /// configure）を済ませる。失敗したら `http_ctx` を返す。
    ///
    /// - インスタンス枠（`total_memories`）が尽きていたら、上限付きで空きを待つ（B-299。fail closed で
    ///   一時的な枠切れをすぐ 503 にしない）。
    /// - ライフサイクルのトラップ・`on_vm_start`/`on_configure` の false は失敗（`lifecycle`。B-301）。
    async fn new_configured_instance(
        &self,
        module: &LoadedModule,
        configuration: &Arc<[u8]>,
        http_ctx: HttpContext,
    ) -> Result<(Store<HostState>, wasmtime::Instance), (anyhow::Error, Box<HttpContext>)> {
        let mut store = Store::new(self.registry.engine(), HostState::new(http_ctx));
        if let Err(e) = self.arm_store(&mut store) {
            return Err((e, Box::new(store.into_data().http_ctx)));
        }
        let mut backoff_ms = 1u64;
        let instance = loop {
            match module.instance_pre.instantiate_async(&mut store).await {
                Ok(i) => break i,
                Err(e)
                    if backoff_ms <= INSTANTIATE_RETRY_MAX_MS
                        && e.downcast_ref::<wasmtime::PoolConcurrencyLimitError>()
                            .is_some() =>
                {
                    crate::runtime::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    backoff_ms *= 2;
                }
                Err(e) => {
                    return Err((
                        e.context(FailKind("instantiate")),
                        Box::new(store.into_data().http_ctx),
                    ))
                }
            }
        };
        if let Err(e) = self
            .run_lifecycle(&mut store, instance, configuration)
            .await
        {
            return Err((
                e.context(FailKind("lifecycle")),
                Box::new(store.into_data().http_ctx),
            ));
        }
        // 起動時のコールバックの燃料を要求のコールバックの予算に含めない（B-301）
        if let Err(e) = self.arm_store(&mut store) {
            return Err((e, Box::new(store.into_data().http_ctx)));
        }
        Ok((store, instance))
    }

    /// Proxy-Wasm SDK のライフサイクル（_start/_initialize → root コンテキスト → vm_start → configure）。
    /// トラップと、`on_vm_start`・`on_configure` が false（0）を返したことを失敗にする（B-301）。
    async fn run_lifecycle(
        &self,
        store: &mut Store<HostState>,
        instance: wasmtime::Instance,
        configuration: &Arc<[u8]>,
    ) -> anyhow::Result<()> {
        let root_context_id = 1i32;
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut *store, "_start") {
            func.call_async(&mut *store, ()).await?;
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut *store, "_initialize") {
            func.call_async(&mut *store, ()).await?;
        }
        let config_size = configuration.len() as i32;
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut *store, "proxy_on_context_create")
        {
            func.call_async(&mut *store, (root_context_id, 0)).await?;
        }
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut *store, "proxy_on_vm_start")
        {
            if func
                .call_async(&mut *store, (root_context_id, config_size))
                .await?
                == 0
            {
                anyhow::bail!("proxy_on_vm_start returned false");
            }
        }
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut *store, "proxy_on_configure")
        {
            if func
                .call_async(&mut *store, (root_context_id, config_size))
                .await?
                == 0
            {
                anyhow::bail!(
                    "proxy_on_configure returned false (the plugin rejected its configuration)"
                );
            }
        }
        Ok(())
    }

    /// 新しいインスタンスでライフサイクルを済ませる（要求の外のコールバック用。失敗はエラー）
    async fn fresh_instance(
        &self,
        module: &LoadedModule,
        configuration: &Arc<[u8]>,
        http_ctx: HttpContext,
    ) -> anyhow::Result<(Store<HostState>, wasmtime::Instance)> {
        self.new_configured_instance(module, configuration, http_ctx)
            .await
            .map_err(|(e, _)| e)
    }

    /// 1 回の HTTP のコールバックに使うインスタンスと http context を用意する（F-271・B-177）。
    ///
    /// - `instance_reuse` のモジュール: このスレッドのプールの空いているインスタンス（無ければ新しく作る）に
    ///   http context を作る。`sessions`（要求ごとの状態）があれば、同じ要求の前のフェーズで作った context を
    ///   同じインスタンスで使い続ける（Envoy と同じ寿命。`proxy_on_delete` は `on_log` の後）。
    /// - 使い回さないモジュール: 新しいインスタンスでライフサイクルを走らせ、HTTP context（ID 2）を作る。
    ///
    /// 失敗したら `http_ctx` を返す（呼び出し元はヘッダ等を回収できる）。
    async fn open_session(
        &self,
        module: &LoadedModule,
        configuration: &Arc<[u8]>,
        http_ctx: HttpContext,
        sessions: Option<&RequestSessions>,
    ) -> Result<Session, (anyhow::Error, Box<HttpContext>)> {
        let root_context_id = 1i32;
        if module.instance_reuse {
            let module_key = module as *const LoadedModule as usize;
            // 同じ要求の前のフェーズの context
            if let Some((slot, context_id)) =
                sessions.and_then(|r| r.find(module_key, configuration))
            {
                match slot.acquire().await {
                    Some(p) => {
                        let mut sess = Session::pooled(p, slot, context_id, true, http_ctx);
                        if let Err(e) = self.prepare_pooled(&mut sess).await {
                            return Err(self.fail_pooled(sess, sessions, module_key, e));
                        }
                        return Ok(sess);
                    }
                    // 別の要求のトラップ等でインスタンスが捨てられた: 以後のフェーズは新しい context で続ける
                    // （フィルタを飛ばさない。前のフェーズで context に持った状態は失われる）
                    None => {
                        if let Some(r) = sessions {
                            r.remove(module_key, &slot);
                        }
                    }
                }
            }

            // 新しい context（空いているインスタンス、無ければ新しく作る）
            let (slot, p, http_ctx) = match module.pool.take_free(configuration) {
                Some((slot, p)) => (slot, p, http_ctx),
                None => {
                    let (store, instance) = self
                        .new_configured_instance(module, configuration, http_ctx)
                        .await?;
                    let slot = InstanceSlot::new_in_use(configuration.clone());
                    module.pool.register(&slot);
                    let mut p = PooledInstance {
                        store,
                        instance,
                        next_context_id: 2,
                    };
                    let ctx = std::mem::replace(
                        &mut p.store.data_mut().http_ctx,
                        HttpContext::for_module(1, module),
                    );
                    (slot, p, ctx)
                }
            };
            let context_id = p.next_context_id;
            let mut sess = Session::pooled(p, slot, context_id, sessions.is_some(), http_ctx);
            sess.next_context_id = context_id.saturating_add(1);
            if let Err(e) = self.prepare_pooled(&mut sess).await {
                return Err(self.fail_pooled(sess, None, module_key, e));
            }
            if let Ok(func) = sess
                .instance
                .get_typed_func::<(i32, i32), ()>(&mut sess.store, "proxy_on_context_create")
            {
                if let Err(e) = func
                    .call_async(&mut sess.store, (context_id, root_context_id))
                    .await
                {
                    return Err(self.fail_pooled(sess, None, module_key, e));
                }
            }
            if let (Some(r), Some(slot)) = (sessions, sess.slot.as_ref()) {
                r.insert(module_key, slot.clone(), context_id);
            }
            return Ok(sess);
        }

        let (mut store, instance) = self
            .new_configured_instance(module, configuration, http_ctx)
            .await?;
        // Step 4: Create HTTP context with root as parent
        let http_context_id = 2i32; // HTTP context ID
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            if let Err(e) = func
                .call_async(&mut store, (http_context_id, root_context_id))
                .await
            {
                return Err((
                    e.context(FailKind("lifecycle")),
                    Box::new(store.into_data().http_ctx),
                ));
            }
        }

        Ok(Session {
            store,
            instance,
            context_id: http_context_id,
            slot: SlotGuard(None),
            request_scoped: false,
            next_context_id: 0,
        })
    }

    /// プールのインスタンスでコールバックを呼ぶ前の準備（燃料・締切と、捨てられた要求の context の後始末）
    async fn prepare_pooled(&self, sess: &mut Session) -> anyhow::Result<()> {
        self.arm_store(&mut sess.store)?;
        let pending = match sess.slot.as_ref() {
            Some(slot) => slot.take_pending_deletes(),
            None => return Ok(()),
        };
        if pending.is_empty() {
            return Ok(());
        }
        if let Ok(func) = sess
            .instance
            .get_typed_func::<i32, ()>(&mut sess.store, "proxy_on_delete")
        {
            for id in pending {
                func.call_async(&mut sess.store, id).await?;
            }
        }
        Ok(())
    }

    /// プールのインスタンスの準備に失敗した: インスタンスを捨てて `http_ctx` を返す
    fn fail_pooled(
        &self,
        sess: Session,
        sessions: Option<&RequestSessions>,
        module_key: usize,
        e: anyhow::Error,
    ) -> (anyhow::Error, Box<HttpContext>) {
        if let Some(slot) = sess.slot.as_ref() {
            slot.discard();
            if let Some(r) = sessions {
                r.remove(module_key, slot);
            }
        }
        (e, Box::new(sess.into_ctx()))
    }

    /// コールバックが済んだインスタンスを片付ける（F-271・B-177）。`ok` はコールバックが成功したか。
    ///
    /// - `instance_reuse` のモジュール: 成功したらプールに戻す。要求ごとの状態の無い呼び出し、または
    ///   `end_context`（`on_log` の後）のときは http context を消す（`proxy_on_delete`）。トラップ・エラーの
    ///   インスタンスは捨てる（そこに context を持つ要求の以後のフェーズはエラーになる）。
    /// - 使い回さないモジュール: 捨てる。
    async fn close_session(
        &self,
        module: &LoadedModule,
        mut sess: Session,
        ok: bool,
        sessions: Option<&RequestSessions>,
        end_context: bool,
    ) {
        let Some(slot) = sess.slot.take() else {
            return;
        };
        let module_key = module as *const LoadedModule as usize;
        let end = end_context || !sess.request_scoped;
        let mut ok = ok && sess.next_context_id < i32::MAX - 1;
        if ok && end {
            if let Ok(func) = sess
                .instance
                .get_typed_func::<i32, ()>(&mut sess.store, "proxy_on_delete")
            {
                ok = self.arm_store(&mut sess.store).is_ok()
                    && func
                        .call_async(&mut sess.store, sess.context_id)
                        .await
                        .is_ok();
            }
        }
        if !ok {
            slot.discard();
            if let Some(r) = sessions {
                r.remove(module_key, &slot);
            }
            return;
        }
        if end && sess.request_scoped {
            if let Some(r) = sessions {
                r.remove(module_key, &slot);
            }
        }
        // 要求の中身（ヘッダ・本文）をプールの間に持ち続けない
        sess.store.data_mut().http_ctx = HttpContext::for_module(1, module);
        slot.release(PooledInstance {
            store: sess.store,
            instance: sess.instance,
            next_context_id: sess.next_context_id,
        });
    }

    /// コールバックの結果をまとめる（F-293・B-299）。
    ///
    /// - Pause なら保留中の `http_call` をその場で解決して再開する（F-62）。再開の失敗は失敗として返す
    ///   （以前は成功扱いで続けていた）。
    /// - `send_local_response` 済みなら、その後でトラップしても返す（以前は失敗の側で捨てていた）。
    ///
    /// 戻り値の bool はインスタンスをプールへ戻してよいか（トラップ・エラーのインスタンスは捨てる）。
    async fn settle(
        &self,
        module: &LoadedModule,
        sess: &mut Session,
        run: anyhow::Result<i32>,
    ) -> (bool, Settled) {
        let result = match run {
            Ok(action) => match FilterAction::from(action) {
                FilterAction::Continue => Ok(false),
                FilterAction::Pause => {
                    let (instance, ctx_id) = (sess.instance, sess.context_id);
                    sess.store.data_mut().http_ctx.stream_resumed = false;
                    match self
                        .resolve_pending_http_calls_inline(
                            module,
                            &mut sess.store,
                            instance,
                            ctx_id,
                        )
                        .await
                    {
                        // 応答を受けたモジュールが再開した（`proxy_continue_stream`）か、ローカル応答を決めた。
                        // B-320: どちらでもない（再開も拒否もしない・`max_http_calls` 回を超えてコールが
                        // 残った）なら Pause のまま（以前は続けていて、決めていない Pause が許可になった。F20）
                        Ok(true)
                            if sess.store.data().http_ctx.stream_resumed
                                || sess.store.data().http_ctx.local_response.is_some() =>
                        {
                            Ok(false)
                        }
                        Ok(true) => Ok(true),
                        // インラインで待てるものが無い Pause
                        Ok(false) => Ok(true),
                        Err(e) => Err(e.context(FailKind("http_call_resume"))),
                    }
                }
            },
            Err(e) => Err(e),
        };
        let ok = result.is_ok();
        // 本文を複製しない（コンテキストはこの後プールへ戻すか捨てるだけ。B-175）
        if let Some(local) = sess.store.data_mut().http_ctx.local_response.take() {
            return (ok, Settled::Local(local));
        }
        match result {
            Ok(false) => (ok, Settled::Continue),
            Ok(true) => {
                #[cfg(feature = "grpc")]
                let grpc = !sess.store.data().http_ctx.pending_grpc_calls.is_empty();
                #[cfg(not(feature = "grpc"))]
                let grpc = false;
                (ok, Settled::Pause { grpc })
            }
            Err(e) => (ok, Settled::Failed(e)),
        }
    }

    /// モジュールの失敗を数えて記録し、拒否するなら返す応答を返す（F-293）
    fn on_failure(
        &self,
        module: &LoadedModule,
        phase: &'static str,
        e: &anyhow::Error,
    ) -> Option<LocalResponse> {
        self.record_failure(module, phase, failure_reason(e), Some(e))
    }

    /// 失敗の扱い（`failure_policy`）を適用する（F-293）。
    ///
    /// - `fail_closed`: 503 を返す（既定）。
    /// - `fail_open`: このモジュールを飛ばして次へ進む。
    /// - `fail_reload`: 503 を返し、使い回しているインスタンスを捨てて次の要求から作り直す。
    fn record_failure(
        &self,
        module: &LoadedModule,
        phase: &'static str,
        reason: &'static str,
        err: Option<&anyhow::Error>,
    ) -> Option<LocalResponse> {
        let policy = module.failure_policy;
        let rejects = policy.rejects();
        let action = if rejects { "rejected" } else { "continued" };
        crate::metrics::inc_wasm_failure(&module.name, phase, reason, action);
        match err {
            Some(e) => crate::logging::log_throttled!(
                warn,
                "[wasm:{}] {} failed ({}, failure_policy={}): {:#}",
                module.name,
                phase,
                reason,
                policy.as_str(),
                e
            ),
            None => crate::logging::log_throttled!(
                warn,
                "[wasm:{}] {} failed ({}, failure_policy={})",
                module.name,
                phase,
                reason,
                policy.as_str()
            ),
        }
        if policy == super::types::FailurePolicy::FailReload {
            module.pool.reload();
        }
        rejects.then(failure_response)
    }

    /// 指定ヘッダコールバックを呼んで action を返す（ヘッダ系フィルタ共通）。
    async fn run_headers_module(
        &self,
        module: &LoadedModule,
        sess: &mut Session,
        callback_name: &str,
        num_headers: i32,
        end_of_stream: bool,
    ) -> anyhow::Result<i32> {
        let callback = sess
            .instance
            .get_typed_func::<(i32, i32, i32), i32>(&mut sess.store, callback_name);

        let action = match callback {
            Ok(func) => {
                let eos = if end_of_stream { 1 } else { 0 };
                func.call_async(&mut sess.store, (sess.context_id, num_headers, eos))
                    .await?
            }
            Err(_) => 0, // Callback not exported, continue
        };

        // F-48: fuel 消費量を記録（ライフサイクル + コールバック 1 実行分）。
        let consumed = self
            .fuel_limit
            .saturating_sub(sess.store.get_fuel().unwrap_or(0));
        let phase = if callback_name == "proxy_on_request_headers" {
            "request_headers"
        } else {
            "response_headers"
        };
        crate::metrics::observe_wasm_fuel_consumed(&module.name, phase, consumed);

        Ok(action)
    }

    /// F-62: Pause 中に登録された HTTP コールをインラインで解決し、
    /// `proxy_on_http_call_response` で同一インスタンスを resume する。
    ///
    /// - 上流解決は `CURRENT_CONFIG` の upstream_groups から行う（tick スレッドと同一規約）
    /// - ブロッキング HTTP クライアント（http_executor）は `runtime::offload` で
    ///   専用スレッドへ退避し、**イベントループはブロックしない**（ホットパス絶対規則）
    /// - 解決済みコールはグローバルレジストリから取り除き、tick スレッドでの二重実行を防ぐ
    /// - resume 後にモジュールが新たなコールを登録した場合は続けて解決する
    ///   （無限ループ防止のため `max_http_calls` 回で打ち切り）
    ///
    /// 戻り値: 1 件でもコールを解決して resume した場合 true。
    async fn resolve_pending_http_calls_inline(
        &self,
        module: &LoadedModule,
        store: &mut Store<HostState>,
        instance: wasmtime::Instance,
        http_context_id: i32,
    ) -> anyhow::Result<bool> {
        let mut resumed = false;
        let max_rounds = module.capabilities.max_http_calls.max(1);

        for _ in 0..max_rounds {
            // pending コールをドレイン（同期スコープ内で完結）
            let pending: Vec<super::types::PendingHttpCall> = {
                let ctx = &mut store.data_mut().http_ctx;
                let tokens: Vec<u32> = ctx.pending_http_calls.keys().copied().collect();
                tokens
                    .into_iter()
                    .filter_map(|t| ctx.pending_http_calls.remove(&t))
                    .collect()
            };
            if pending.is_empty() {
                break;
            }

            for call in pending {
                let token = call.token;
                // tick スレッドの二重実行を防止
                super::persistent_context::remove_global_pending_call(&module.name, token);

                let response = Self::execute_http_call_offloaded(&module.name, call).await;

                // B-56: 外部 I/O を待っていた間にエポックが進んで（B-173 以降は時間で）締切が過ぎていると、
                // resume 直後にトラップする。再開直前に猶予を与え直す。
                // （外部 I/O の待ち時間はゲストの CPU 時間ではないため、これは制限の回避ではない）
                store.set_epoch_deadline(self.epoch_deadline);

                // 応答をコンテキストへ格納して proxy_on_http_call_response を呼ぶ
                let (num_headers, body_size, num_trailers) = (
                    response.headers.len() as i32,
                    response.body.len() as i32,
                    response.trailers.len() as i32,
                );
                {
                    let ctx = &mut store.data_mut().http_ctx;
                    ctx.http_call_responses.insert(token, response);
                    ctx.current_http_call_token = Some(token);
                }

                if let Ok(func) = instance.get_typed_func::<(i32, i32, i32, i32, i32), ()>(
                    &mut *store,
                    "proxy_on_http_call_response",
                ) {
                    func.call_async(
                        &mut *store,
                        (
                            http_context_id,
                            token as i32,
                            num_headers,
                            body_size,
                            num_trailers,
                        ),
                    )
                    .await?;
                }
                store.data_mut().http_ctx.current_http_call_token = None;
                resumed = true;

                // ローカルレスポンスが設定されたら以降のコールは不要
                if store.data().http_ctx.local_response.is_some() {
                    return Ok(resumed);
                }
            }
        }

        Ok(resumed)
    }

    /// F-62: 上流を解決し、ブロッキング HTTP コールを offload スレッドで実行する。
    /// エラー時は tick スレッド経路と同一規約のエラー応答（502/503/504）を返す。
    async fn execute_http_call_offloaded(
        module_name: &str,
        call: super::types::PendingHttpCall,
    ) -> super::types::HttpCallResponse {
        use super::types::HttpCallResponse;

        // 上流解決（ArcSwap ロード、ロックなし）
        let config = crate::config::CURRENT_CONFIG.load();
        let resolved = config
            .upstream_groups
            .get(&call.upstream)
            .and_then(|group| group.select("0.0.0.0"))
            .map(|server| (server.host().to_string(), server.port(), server.use_tls()));

        let Some((host, port, use_tls)) = resolved else {
            ftlog::warn!(
                "[wasm:http_call] Upstream '{}' not resolvable for module '{}' (inline resume)",
                call.upstream,
                module_name
            );
            return HttpCallResponse {
                status_code: 502,
                headers: vec![(b"x-wasm-error".to_vec(), b"upstream_not_found".to_vec())],
                body: format!("Upstream '{}' not found", call.upstream).into_bytes(),
                trailers: Vec::new(),
            };
        };

        let pending = super::persistent_context::GlobalPendingCall {
            module_name: module_name.to_string(),
            token: call.token,
            call,
        };

        // ブロッキング I/O を専用スレッドへ退避（イベントループを塞がない）
        crate::runtime::offload::offload(move || {
            super::http_executor::execute_http_call_safe(&pending, &host, port, use_tls)
        })
        .await
    }

    /// Execute on_request_headers for a single module
    ///
    /// F-43: ヘッダは所有権で受け取り、（変更有無に関わらず）コンテキストから回収して
    /// 返す（ムーブスルー）。エラー時もヘッダは失われない。
    async fn execute_on_request_headers(
        &self,
        module: &LoadedModule,
        configuration: &Arc<[u8]>,
        path: &Arc<str>,
        method: &Arc<str>,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        client_ip: &Arc<str>,
        end_of_stream: bool,
        sessions: Option<&RequestSessions>,
    ) -> (Vec<(Vec<u8>, Vec<u8>)>, anyhow::Result<ModuleAction>) {
        let num_headers = headers.len() as i32;

        // Create context（文字列は Arc 共有、ヘッダはムーブ）
        let mut http_ctx = HttpContext::for_module(1, module);
        http_ctx.set_request(method.clone(), path.clone(), headers, client_ip.clone());
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_root_id = module.root_id.clone();
        http_ctx.plugin_configuration = configuration.clone();
        http_ctx.plugin_data = module.data_files.clone();

        let mut sess = match self
            .open_session(module, configuration, http_ctx, sessions)
            .await
        {
            Ok(s) => s,
            Err((e, ctx)) => return (ctx.request_headers, Err(e)),
        };

        let run = self
            .run_headers_module(
                module,
                &mut sess,
                "proxy_on_request_headers",
                num_headers,
                end_of_stream,
            )
            .await;

        let (ok, settled) = self.settle(module, &mut sess, run).await;
        let result = match settled {
            Settled::Continue => Ok(ModuleAction::Continue),
            Settled::Pause { grpc } => Ok(ModuleAction::Pause { grpc }),
            Settled::Local(r) => Ok(ModuleAction::LocalResponse(r)),
            Settled::Failed(e) => Err(e),
        };

        // ヘッダをコンテキストから回収（ゼロコピー・ムーブ。resume 中の変更も反映される）
        let headers = std::mem::take(&mut sess.store.data_mut().http_ctx.request_headers);
        self.close_session(module, sess, ok, sessions, false).await;
        (headers, result)
    }

    /// Execute on_response_headers callback for all modules (reverse order)
    ///
    /// Note: This method is deprecated. Use `on_response_headers_with_modules` instead.
    /// This method always returns Continue without applying any modules.
    pub fn on_response_headers(
        &self,
        _path: &str,
        _status: u16,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        _end_of_stream: bool,
    ) -> FilterResult {
        FilterResult::Continue {
            headers,
            body: None,
        }
    }

    /// Execute on_response_headers for specified modules
    ///
    /// F-43: ヘッダは所有権ムーブスルー（per-module の deep copy 排除）。
    pub async fn on_response_headers_with_modules(
        &self,
        modules: &[ModuleRef],
        status: u16,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        end_of_stream: bool,
        sessions: Option<&RequestSessions>,
    ) -> FilterResult {
        let resolved = resolve_effective_modules(&self.registry, modules);

        if resolved.is_empty() {
            return FilterResult::Continue {
                headers,
                body: None,
            };
        }

        let mut current_headers = headers;

        // Execute in reverse order for response
        for (module, config) in resolved.iter().rev() {
            // F-09: WASM フィルタ実行時間を計測
            let _wasm_start = std::time::Instant::now();
            let (returned, result) = self
                .execute_on_response_headers(
                    module,
                    config,
                    status,
                    current_headers,
                    end_of_stream,
                    sessions,
                )
                .await;
            current_headers = returned;
            crate::metrics::observe_wasm_filter_duration(
                &module.name,
                "response_headers",
                _wasm_start.elapsed().as_secs_f64(),
            );

            match result {
                Ok(ModuleAction::Continue) => {}
                Ok(ModuleAction::Pause { grpc }) => {
                    if grpc || end_of_stream || !module.capabilities.allow_response_body_read {
                        if let Some(resp) = self.record_failure(
                            module,
                            "response_headers",
                            "unsupported_pause",
                            None,
                        ) {
                            return FilterResult::LocalResponse(resp);
                        }
                    }
                }
                Ok(ModuleAction::LocalResponse(resp)) => {
                    return FilterResult::LocalResponse(resp);
                }
                Err(e) => {
                    if let Some(resp) = self.on_failure(module, "response_headers", &e) {
                        return FilterResult::LocalResponse(resp);
                    }
                }
            }
        }

        FilterResult::Continue {
            headers: current_headers,
            body: None,
        }
    }

    /// Execute on_response_headers for specified modules ASYNCHRONOUSLY
    /// Note: runs inline in the current async task to avoid cross-thread waker issues with the thread-per-core runtime.
    ///
    /// F-43/F-148: `modules` は `Arc` 共有（呼び出しごとの `Vec<ModuleRef>` deep copy 排除）。
    pub async fn on_response_headers_with_modules_async(
        self: Arc<Self>,
        modules: crate::wasm_plugin_config::WasmModules,
        status: u16,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        end_of_stream: bool,
    ) -> FilterResult {
        // 呼び出し元の future に本体（数 KB）を埋め込まない（B-240）
        Box::pin(self.on_response_headers_with_modules(
            &modules,
            status,
            headers,
            end_of_stream,
            modules.sessions(),
        ))
        .await
    }

    /// Execute on_response_headers for a single module
    ///
    /// F-43: ヘッダは所有権ムーブスルー（`execute_on_request_headers` と同方針）。
    async fn execute_on_response_headers(
        &self,
        module: &LoadedModule,
        configuration: &Arc<[u8]>,
        status: u16,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        end_of_stream: bool,
        sessions: Option<&RequestSessions>,
    ) -> (Vec<(Vec<u8>, Vec<u8>)>, anyhow::Result<ModuleAction>) {
        let num_headers = headers.len() as i32;

        // Create context（ヘッダはムーブ）
        let mut http_ctx = HttpContext::for_module(1, module);
        http_ctx.set_response(status, headers);
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_root_id = module.root_id.clone();
        http_ctx.plugin_configuration = configuration.clone();
        http_ctx.plugin_data = module.data_files.clone();

        let mut sess = match self
            .open_session(module, configuration, http_ctx, sessions)
            .await
        {
            Ok(s) => s,
            Err((e, ctx)) => return (ctx.response_headers, Err(e)),
        };

        let run = self
            .run_headers_module(
                module,
                &mut sess,
                "proxy_on_response_headers",
                num_headers,
                end_of_stream,
            )
            .await;

        let (ok, settled) = self.settle(module, &mut sess, run).await;
        let result = match settled {
            Settled::Continue => Ok(ModuleAction::Continue),
            Settled::Pause { grpc } => Ok(ModuleAction::Pause { grpc }),
            Settled::Local(r) => Ok(ModuleAction::LocalResponse(r)),
            Settled::Failed(e) => Err(e),
        };

        // ヘッダをコンテキストから回収（ゼロコピー・ムーブ。resume 中の変更も反映される）
        let headers = std::mem::take(&mut sess.store.data_mut().http_ctx.response_headers);
        self.close_session(module, sess, ok, sessions, false).await;
        (headers, result)
    }

    /// モジュールの Proxy-Wasm の root_id（プロパティ `plugin_root_id`）を設定する。Envoy 互換の
    /// サイドカー（F-222）が ECDS の `PluginConfig.root_id` を渡すのに使う（C++ SDK はこの名前で
    /// ルートコンテキストのファクトリを選ぶ）。読み込み直後（エンジンを共有する前）だけ効く。
    #[cfg(feature = "mesh")]
    pub fn set_root_id(&mut self, module: &str, root_id: &str) -> bool {
        self.registry.set_root_id(module, root_id)
    }

    /// 試験用: wasmtime のエンジン（枠を外から握る試験。F-293）
    #[cfg(test)]
    pub(crate) fn registry_engine(&self) -> &wasmtime::Engine {
        self.registry.engine()
    }

    pub fn get_module(&self, name: &str) -> Option<Arc<LoadedModule>> {
        self.registry.get_module(name)
    }

    /// 並びに応答本文のフェーズ（`allow_response_body_read`）を受けるモジュールがあるか（F-285）。
    ///
    /// 無いルートは応答本文を溜めない（ストリーミング・splice のまま）。
    pub fn wants_response_body(&self, modules: &crate::wasm_plugin_config::WasmModules) -> bool {
        modules.modules().iter().any(|m| {
            self.get_module(&m.name)
                .is_some_and(|lm| lm.capabilities.allow_response_body_read)
        })
    }

    /// Execute proxy_on_http_call_response callback for a module
    ///
    /// This should be called after an HTTP call completes to deliver the response
    /// back to the WASM module.
    ///
    /// F-148: 単一モジュール名のみを取る API（ルート文脈が無い）のため、ここでは
    /// ルート単位の上書き設定は解決できず、常にモジュール定義側の
    /// `module.configuration` を使う。
    pub async fn on_http_call_response(
        &self,
        module_name: &str,
        token: u32,
        response: super::types::HttpCallResponse,
    ) -> FilterResult {
        let module = match self.registry.get_module(module_name) {
            Some(m) => m,
            None => {
                ftlog::warn!(
                    "[wasm] Module '{}' not found for HTTP call response",
                    module_name
                );
                return FilterResult::Continue {
                    headers: Vec::new(),
                    body: None,
                };
            }
        };

        match self
            .execute_on_http_call_response(&module, token, response)
            .await
        {
            Ok(result) => match result {
                ModuleResult::Continue { modified_headers } => FilterResult::Continue {
                    headers: modified_headers.unwrap_or_default(),
                    body: None,
                },
                // 要求の外のコールバック（tick スレッド）。続きを待つ要求は無い
                ModuleResult::Pause => FilterResult::Continue {
                    headers: Vec::new(),
                    body: None,
                },
                ModuleResult::LocalResponse(resp) => FilterResult::LocalResponse(resp),
            },
            Err(e) => {
                ftlog::error!("[wasm:{}] on_http_call_response error: {}", module_name, e);
                FilterResult::Continue {
                    headers: Vec::new(),
                    body: None,
                }
            }
        }
    }

    /// Execute on_http_call_response for a single module
    async fn execute_on_http_call_response(
        &self,
        module: &LoadedModule,
        token: u32,
        response: super::types::HttpCallResponse,
    ) -> anyhow::Result<ModuleResult> {
        // Create context with HTTP call response
        let mut http_ctx = HttpContext::for_module(1, module);
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_root_id = module.root_id.clone();
        http_ctx.plugin_configuration = module.configuration.clone();
        http_ctx.plugin_data = module.data_files.clone();

        // Store the response in context
        http_ctx.http_call_responses.insert(token, response.clone());
        http_ctx.current_http_call_token = Some(token);

        let (mut store, instance) = self
            .fresh_instance(module, &module.configuration, http_ctx)
            .await?;
        let root_context_id = 1i32;
        let http_context_id = 2i32;

        // Step 4: Create HTTP context with root as parent
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func
                .call_async(&mut store, (http_context_id, root_context_id))
                .await;
        }

        // Call proxy_on_http_call_response
        // Signature: (context_id, token, num_headers, body_size, num_trailers) -> void
        let callback = instance.get_typed_func::<(i32, i32, i32, i32, i32), ()>(
            &mut store,
            "proxy_on_http_call_response",
        );

        match callback {
            Ok(func) => {
                let num_headers = response.headers.len() as i32;
                let body_size = response.body.len() as i32;
                let num_trailers = response.trailers.len() as i32;

                // 本 Store は fuel_async_yield_interval を設定した async store のため、
                // 同期 `call` は wasmtime が "must use `call_async` with async stores" で
                // panic する（tick スレッドから本経路に入った場合に tick スレッドが死に、
                // 取得済み pending コールが失われて対象リクエストがハングする。
                // F-120 Phase 2 の E2E で顕在化した既存バグ）。必ず call_async を使う。
                if let Err(e) = func
                    .call_async(
                        &mut store,
                        (
                            http_context_id,
                            token as i32,
                            num_headers,
                            body_size,
                            num_trailers,
                        ),
                    )
                    .await
                {
                    ftlog::debug!(
                        "[wasm:{}] proxy_on_http_call_response returned: {}",
                        module.name,
                        e
                    );
                }
            }
            Err(_) => {
                ftlog::debug!(
                    "[wasm:{}] proxy_on_http_call_response not exported",
                    module.name
                );
            }
        }

        // Check for local response
        let state = store.data();
        if let Some(local_response) = &state.http_ctx.local_response {
            return Ok(ModuleResult::LocalResponse(local_response.clone()));
        }

        // Check for modifications
        let modified_headers = if state.http_ctx.request_headers_modified {
            Some(state.http_ctx.request_headers.clone())
        } else {
            None
        };

        Ok(ModuleResult::Continue { modified_headers })
    }

    /// Execute on_request_body callback for specified modules
    ///
    /// Processes request body chunks through WASM modules.
    /// Returns potentially modified body data.
    pub async fn on_request_body_with_modules(
        &self,
        modules: &[ModuleRef],
        body: bytes::Bytes,
        end_of_stream: bool,
        request: Option<&RequestMeta<'_>>,
        sessions: Option<&RequestSessions>,
    ) -> BodyFilterResult {
        let resolved = resolve_effective_modules(&self.registry, modules);

        if resolved.is_empty() {
            // F-61: モジュール未登録時はゼロコピーでそのまま返す
            return BodyFilterResult::Continue { body };
        }

        // F-61: `Bytes` の参照カウント共有でフィルタチェーンを回す（deep copy なし）
        let mut current_body = body;

        for (module, config) in &resolved {
            let result = self
                .execute_on_request_body(
                    module,
                    config,
                    current_body.clone(),
                    end_of_stream,
                    request,
                    sessions,
                )
                .await;

            match result {
                Ok(BodyModuleResult::Continue { modified_body }) => {
                    if let Some(b) = modified_body {
                        current_body = b;
                    }
                }
                // 本文の段は本文全体で 1 回呼ぶので、Pause は待つものが無い（B-300）
                Ok(BodyModuleResult::Pause) => {
                    if let Some(resp) =
                        self.record_failure(module, "request_body", "unsupported_pause", None)
                    {
                        return BodyFilterResult::LocalResponse(resp);
                    }
                }
                Ok(BodyModuleResult::LocalResponse(resp)) => {
                    return BodyFilterResult::LocalResponse(resp);
                }
                Err(e) => {
                    if let Some(resp) = self.on_failure(module, "request_body", &e) {
                        return BodyFilterResult::LocalResponse(resp);
                    }
                }
            }
        }

        BodyFilterResult::Continue { body: current_body }
    }

    /// Execute on_request_body for specified modules ASYNCHRONOUSLY
    /// Note: runs inline in the current async task to avoid cross-thread waker issues with the thread-per-core runtime.
    pub async fn on_request_body_with_modules_async(
        self: Arc<Self>,
        modules: crate::wasm_plugin_config::WasmModules,
        body: bytes::Bytes,
        end_of_stream: bool,
    ) -> BodyFilterResult {
        // 呼び出し元の future に本体（数 KB）を埋め込まない（B-240）
        Box::pin(self.on_request_body_with_modules(
            &modules,
            body,
            end_of_stream,
            None,
            modules.sessions(),
        ))
        .await
    }

    /// テスト用: モジュールのプールにあるインスタンスの数（F-271）
    #[cfg(test)]
    pub(crate) fn pooled_instances(&self, name: &str) -> usize {
        self.registry
            .get_module(name)
            .map(|m| m.pool.len())
            .unwrap_or(0)
    }

    /// テスト用: 次にインスタンスを使うときに `proxy_on_delete` する context の数（B-177）
    #[cfg(test)]
    pub(crate) fn pending_deletes(&self, name: &str) -> usize {
        self.registry
            .get_module(name)
            .map(|m| m.pool.pending_deletes())
            .unwrap_or(0)
    }

    /// 要求本文のコールバックを呼ぶモジュール（`allow_request_body_read`）が含まれるか（F-271。HTTP/1.1・HTTP/2 は
    /// これが真の要求だけ本文を読み切ってから上流へ送る）
    pub fn wants_request_body(&self, modules: &[ModuleRef]) -> bool {
        modules.iter().any(|r| {
            self.registry
                .get_module(&r.name)
                .is_some_and(|m| m.capabilities.allow_request_body_read)
        })
    }

    /// Execute on_request_body for a single module
    async fn execute_on_request_body(
        &self,
        module: &LoadedModule,
        configuration: &Arc<[u8]>,
        body: bytes::Bytes,
        end_of_stream: bool,
        request: Option<&RequestMeta<'_>>,
        sessions: Option<&RequestSessions>,
    ) -> anyhow::Result<BodyModuleResult> {
        // Check capability
        if !module.capabilities.allow_request_body_read {
            return Ok(BodyModuleResult::Continue {
                modified_body: None,
            });
        }

        // Create context（F-271: 要求のメソッド・パス・ヘッダも見えるようにする）
        let mut http_ctx = HttpContext::for_module(1, module);
        if let Some(r) = request {
            http_ctx.set_request(
                r.method.clone(),
                r.path.clone(),
                r.headers.to_vec(),
                r.client_ip.clone(),
            );
        }
        let body_len = body.len();
        http_ctx.set_request_body(body, end_of_stream);
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_root_id = module.root_id.clone();
        http_ctx.plugin_configuration = configuration.clone();
        http_ctx.plugin_data = module.data_files.clone();

        let mut sess = match self
            .open_session(module, configuration, http_ctx, sessions)
            .await
        {
            Ok(s) => s,
            Err((e, _)) => return Err(e),
        };

        // Call proxy_on_request_body
        // Signature: (context_id, body_size, end_of_stream) -> action
        let callback = sess
            .instance
            .get_typed_func::<(i32, i32, i32), i32>(&mut sess.store, "proxy_on_request_body");

        let run = match callback {
            Ok(func) => {
                let body_size = body_len as i32;
                let eos = if end_of_stream { 1 } else { 0 };
                func.call_async(&mut sess.store, (sess.context_id, body_size, eos))
                    .await
            }
            Err(_) => Ok(0), // Continue if not exported
        };
        crate::metrics::observe_wasm_fuel_consumed(
            &module.name,
            "request_body",
            self.fuel_limit
                .saturating_sub(sess.store.get_fuel().unwrap_or(0)),
        );

        // Check for local response / modifications（B-299: トラップの前のローカルレスポンスも返す）
        let (ok, settled) = self.settle(module, &mut sess, run).await;
        let result = match settled {
            Settled::Local(r) => Ok(BodyModuleResult::LocalResponse(r)),
            Settled::Failed(e) => Err(e),
            Settled::Pause { .. } => Ok(BodyModuleResult::Pause),
            Settled::Continue => {
                // F-61: 変更時は store からムーブで取り出す（clone による deep copy を排除）
                let ctx = &mut sess.store.data_mut().http_ctx;
                let modified_body = if ctx.request_body_modified {
                    Some(std::mem::take(&mut ctx.request_body).into_bytes())
                } else {
                    None
                };
                Ok(BodyModuleResult::Continue { modified_body })
            }
        };
        self.close_session(module, sess, ok, sessions, false).await;
        result
    }

    /// Execute on_response_body callback for specified modules
    ///
    /// Processes response body chunks through WASM modules (in reverse order).
    /// Returns potentially modified body data.
    pub async fn on_response_body_with_modules(
        &self,
        modules: &[ModuleRef],
        body: bytes::Bytes,
        end_of_stream: bool,
        sessions: Option<&RequestSessions>,
    ) -> BodyFilterResult {
        let resolved = resolve_effective_modules(&self.registry, modules);

        if resolved.is_empty() {
            // F-61: モジュール未登録時はゼロコピーでそのまま返す
            return BodyFilterResult::Continue { body };
        }

        // F-61: `Bytes` の参照カウント共有でフィルタチェーンを回す（deep copy なし）
        let mut current_body = body;

        // Execute in reverse order for response
        for (module, config) in resolved.iter().rev() {
            let result = self
                .execute_on_response_body(
                    module,
                    config,
                    current_body.clone(),
                    end_of_stream,
                    sessions,
                )
                .await;

            match result {
                Ok(BodyModuleResult::Continue { modified_body }) => {
                    if let Some(b) = modified_body {
                        current_body = b;
                    }
                }
                // 本文の段は本文全体で 1 回呼ぶので、Pause は待つものが無い（B-300）
                Ok(BodyModuleResult::Pause) => {
                    if let Some(resp) =
                        self.record_failure(module, "response_body", "unsupported_pause", None)
                    {
                        return BodyFilterResult::LocalResponse(resp);
                    }
                }
                Ok(BodyModuleResult::LocalResponse(resp)) => {
                    return BodyFilterResult::LocalResponse(resp);
                }
                Err(e) => {
                    if let Some(resp) = self.on_failure(module, "response_body", &e) {
                        return BodyFilterResult::LocalResponse(resp);
                    }
                }
            }
        }

        BodyFilterResult::Continue { body: current_body }
    }

    /// Execute on_response_body for specified modules ASYNCHRONOUSLY
    /// Note: runs inline in the current async task to avoid cross-thread waker issues with the thread-per-core runtime.
    pub async fn on_response_body_with_modules_async(
        self: Arc<Self>,
        modules: crate::wasm_plugin_config::WasmModules,
        body: bytes::Bytes,
        end_of_stream: bool,
    ) -> BodyFilterResult {
        // 呼び出し元の future に本体（数 KB）を埋め込まない（B-240）
        Box::pin(self.on_response_body_with_modules(
            &modules,
            body,
            end_of_stream,
            modules.sessions(),
        ))
        .await
    }

    /// Execute on_response_body for a single module
    async fn execute_on_response_body(
        &self,
        module: &LoadedModule,
        configuration: &Arc<[u8]>,
        body: bytes::Bytes,
        end_of_stream: bool,
        sessions: Option<&RequestSessions>,
    ) -> anyhow::Result<BodyModuleResult> {
        // Check capability
        if !module.capabilities.allow_response_body_read {
            return Ok(BodyModuleResult::Continue {
                modified_body: None,
            });
        }

        // Create context
        let mut http_ctx = HttpContext::for_module(1, module);
        let body_len = body.len();
        http_ctx.set_response_body(body, end_of_stream);
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_root_id = module.root_id.clone();
        http_ctx.plugin_configuration = configuration.clone();
        http_ctx.plugin_data = module.data_files.clone();

        let mut sess = match self
            .open_session(module, configuration, http_ctx, sessions)
            .await
        {
            Ok(s) => s,
            Err((e, _)) => return Err(e),
        };

        // Call proxy_on_response_body
        // Signature: (context_id, body_size, end_of_stream) -> action
        let callback = sess
            .instance
            .get_typed_func::<(i32, i32, i32), i32>(&mut sess.store, "proxy_on_response_body");

        let run = match callback {
            Ok(func) => {
                let body_size = body_len as i32;
                let eos = if end_of_stream { 1 } else { 0 };
                func.call_async(&mut sess.store, (sess.context_id, body_size, eos))
                    .await
            }
            Err(_) => Ok(0), // Continue if not exported
        };
        crate::metrics::observe_wasm_fuel_consumed(
            &module.name,
            "response_body",
            self.fuel_limit
                .saturating_sub(sess.store.get_fuel().unwrap_or(0)),
        );

        // Check for local response / modifications（B-299: トラップの前のローカルレスポンスも返す）
        let (ok, settled) = self.settle(module, &mut sess, run).await;
        let result = match settled {
            Settled::Local(r) => Ok(BodyModuleResult::LocalResponse(r)),
            Settled::Failed(e) => Err(e),
            Settled::Pause { .. } => Ok(BodyModuleResult::Pause),
            Settled::Continue => {
                // F-61: 変更時は store からムーブで取り出す（clone による deep copy を排除）
                let ctx = &mut sess.store.data_mut().http_ctx;
                let modified_body = if ctx.response_body_modified {
                    Some(std::mem::take(&mut ctx.response_body).into_bytes())
                } else {
                    None
                };
                Ok(BodyModuleResult::Continue { modified_body })
            }
        };
        self.close_session(module, sess, ok, sessions, false).await;
        result
    }

    /// Execute on_log callback for specified modules
    ///
    /// Called at the end of HTTP request processing (log phase).
    /// This is the final callback before the stream is closed.
    ///
    /// B-177: `instance_reuse` のモジュールは要求の前のフェーズと同じ http context で `proxy_on_log` を呼び、
    /// その後 `proxy_on_delete` で context を消す。
    pub async fn on_log_with_modules(
        &self,
        modules: &[ModuleRef],
        sessions: Option<&RequestSessions>,
    ) {
        let resolved = resolve_effective_modules(&self.registry, modules);

        for (module, config) in &resolved {
            if let Err(e) = self.execute_on_log(module, config, sessions).await {
                ftlog::error!("[wasm:{}] on_log error: {}", module.name, e);
            }
        }
    }

    /// Execute on_log for specified modules ASYNCHRONOUSLY
    /// Note: runs inline in the current async task to avoid cross-thread waker issues with the thread-per-core runtime.
    pub async fn on_log_with_modules_async(
        self: Arc<Self>,
        modules: crate::wasm_plugin_config::WasmModules,
    ) {
        // 呼び出し元の future に本体（数 KB）を埋め込まない（B-240）
        Box::pin(self.on_log_with_modules(&modules, modules.sessions())).await;
    }

    /// Execute on_log for a single module
    ///
    /// F-271: `instance_reuse` のモジュールはプールのインスタンスで呼ぶ（要求ごとに `_start`・`on_configure` を
    /// 走らせない）。
    async fn execute_on_log(
        &self,
        module: &LoadedModule,
        configuration: &Arc<[u8]>,
        sessions: Option<&RequestSessions>,
    ) -> anyhow::Result<()> {
        // F-148: `config_size` に `configuration.len()` を渡すため、実体のバッファも必ず同じものを積む
        // （サイズだけ非ゼロで中身が空だと、モジュールが proxy_get_buffer(PluginConfiguration) で空を読む）。
        let mut http_ctx = HttpContext::for_module(1, module);
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_root_id = module.root_id.clone();
        http_ctx.plugin_configuration = configuration.clone();
        http_ctx.plugin_data = module.data_files.clone();
        let mut sess = self
            .open_session(module, configuration, http_ctx, sessions)
            .await
            .map_err(|(e, _)| e)?;

        // Call proxy_on_log
        // Signature: (context_id) -> void
        let mut ok = true;
        if let Ok(func) = sess
            .instance
            .get_typed_func::<i32, ()>(&mut sess.store, "proxy_on_log")
        {
            match func.call_async(&mut sess.store, sess.context_id).await {
                Ok(()) => {
                    ftlog::debug!(
                        "[wasm:{}] proxy_on_log({}) OK",
                        module.name,
                        sess.context_id
                    )
                }
                Err(e) => {
                    ok = false;
                    ftlog::debug!("[wasm:{}] proxy_on_log error: {}", module.name, e)
                }
            }
        }
        self.close_session(module, sess, ok, sessions, true).await;

        Ok(())
    }

    /// Execute on_done callback for specified modules
    ///
    /// Called when an HTTP context is being deleted.
    /// Returns true if the module wants to keep the context alive (async operation pending).
    pub async fn on_done_with_modules(&self, modules: &[ModuleRef]) -> bool {
        let resolved = resolve_effective_modules(&self.registry, modules);

        let mut any_pending = false;

        for (module, config) in &resolved {
            match self.execute_on_done(module, config).await {
                Ok(keep_alive) => {
                    if keep_alive {
                        any_pending = true;
                    }
                }
                Err(e) => {
                    ftlog::error!("[wasm:{}] on_done error: {}", module.name, e);
                }
            }
        }

        any_pending
    }

    /// Execute on_done for a single module
    async fn execute_on_done(
        &self,
        module: &LoadedModule,
        configuration: &Arc<[u8]>,
    ) -> anyhow::Result<bool> {
        // Create context
        // F-148: `execute_on_log` と同じ理由で configuration の実体も積む。
        let mut http_ctx = HttpContext::for_module(1, module);
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_root_id = module.root_id.clone();
        http_ctx.plugin_configuration = configuration.clone();
        http_ctx.plugin_data = module.data_files.clone();
        let (mut store, instance) = self.fresh_instance(module, configuration, http_ctx).await?;
        let root_context_id = 1i32;
        let http_context_id = 2i32;

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func
                .call_async(&mut store, (http_context_id, root_context_id))
                .await;
        }

        // Call proxy_on_done
        // Signature: (context_id) -> bool (1 = keep alive, 0 = delete)
        if let Ok(func) = instance.get_typed_func::<i32, i32>(&mut store, "proxy_on_done") {
            match func.call_async(&mut store, http_context_id).await {
                Ok(result) => {
                    ftlog::debug!(
                        "[wasm:{}] proxy_on_done({}) => {}",
                        module.name,
                        http_context_id,
                        result
                    );
                    return Ok(result != 0);
                }
                Err(e) => ftlog::debug!("[wasm:{}] proxy_on_done error: {}", module.name, e),
            }
        }

        Ok(false)
    }

    /// Execute on_tick callback for a module
    ///
    /// Called periodically based on the tick period set by the module.
    /// This should be called on the root context.
    /// F-148: 単一モジュール名のみを取る API（ルート文脈が無い）のため、ここでは
    /// ルート単位の上書き設定は解決できず、常にモジュール定義側の `module.configuration` を使う。
    pub async fn on_tick(&self, module_name: &str) {
        let module = match self.registry.get_module(module_name) {
            Some(m) => m,
            None => return,
        };

        if let Err(e) = self.execute_on_tick(&module).await {
            ftlog::error!("[wasm:{}] on_tick error: {}", module_name, e);
        }
    }

    /// Execute on_tick for a single module
    async fn execute_on_tick(&self, module: &LoadedModule) -> anyhow::Result<()> {
        // Create context
        let http_ctx = HttpContext::for_module(1, module);
        let (mut store, instance) = self
            .fresh_instance(module, &module.configuration, http_ctx)
            .await?;
        let root_context_id = 1i32;

        // Call proxy_on_tick on root context
        // Signature: (context_id) -> void
        if let Ok(func) = instance.get_typed_func::<i32, ()>(&mut store, "proxy_on_tick") {
            match func.call_async(&mut store, root_context_id).await {
                Ok(()) => {
                    ftlog::debug!(
                        "[wasm:{}] proxy_on_tick({}) OK",
                        module.name,
                        root_context_id
                    )
                }
                Err(e) => ftlog::debug!("[wasm:{}] proxy_on_tick error: {}", module.name, e),
            }
        }

        Ok(())
    }

    /// Execute on_request_trailers callback for specified modules
    ///
    /// Called when request trailers are received (HTTP/2, gRPC).
    pub async fn on_request_trailers_with_modules(
        &self,
        modules: &[ModuleRef],
        trailers: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> FilterResult {
        let resolved = resolve_effective_modules(&self.registry, modules);

        if resolved.is_empty() {
            return FilterResult::Continue {
                headers: trailers,
                body: None,
            };
        }

        let mut current_trailers = trailers;

        for (module, config) in &resolved {
            let result = self
                .execute_on_request_trailers(module, config, &current_trailers)
                .await;

            match result {
                Ok(ModuleResult::Continue { modified_headers }) => {
                    if let Some(h) = modified_headers {
                        current_trailers = h;
                    }
                }
                // トレーラーの段の後に待つものは無い（B-300）
                Ok(ModuleResult::Pause) => {
                    if let Some(resp) =
                        self.record_failure(module, "request_trailers", "unsupported_pause", None)
                    {
                        return FilterResult::LocalResponse(resp);
                    }
                }
                Ok(ModuleResult::LocalResponse(resp)) => {
                    return FilterResult::LocalResponse(resp);
                }
                Err(e) => {
                    if let Some(resp) = self.on_failure(module, "request_trailers", &e) {
                        return FilterResult::LocalResponse(resp);
                    }
                }
            }
        }

        FilterResult::Continue {
            headers: current_trailers,
            body: None,
        }
    }

    /// Execute on_request_trailers for a single module
    async fn execute_on_request_trailers(
        &self,
        module: &LoadedModule,
        configuration: &Arc<[u8]>,
        trailers: &[(Vec<u8>, Vec<u8>)],
    ) -> anyhow::Result<ModuleResult> {
        // Create context
        let mut http_ctx = HttpContext::for_module(1, module);
        http_ctx.request_trailers = trailers.to_vec();
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_root_id = module.root_id.clone();
        http_ctx.plugin_configuration = configuration.clone();
        http_ctx.plugin_data = module.data_files.clone();

        let mut sess = self
            .open_session(module, configuration, http_ctx, None)
            .await
            .map_err(|(e, _)| e)?;

        // Call proxy_on_request_trailers
        // Signature: (context_id, num_trailers) -> action
        let run = match sess
            .instance
            .get_typed_func::<(i32, i32), i32>(&mut sess.store, "proxy_on_request_trailers")
        {
            Ok(func) => {
                let num_trailers = trailers.len() as i32;
                func.call_async(&mut sess.store, (sess.context_id, num_trailers))
                    .await
            }
            Err(_) => Ok(0),
        };

        // B-299: トラップの前のローカルレスポンスも返す
        let (ok, settled) = self.settle(module, &mut sess, run).await;
        let result = match settled {
            Settled::Local(r) => Ok(ModuleResult::LocalResponse(r)),
            Settled::Failed(e) => Err(e),
            Settled::Pause { .. } => Ok(ModuleResult::Pause),
            Settled::Continue => {
                let ctx = &mut sess.store.data_mut().http_ctx;
                let modified_headers = if ctx.request_headers_modified {
                    Some(std::mem::take(&mut ctx.request_trailers))
                } else {
                    None
                };
                Ok(ModuleResult::Continue { modified_headers })
            }
        };
        self.close_session(module, sess, ok, None, false).await;
        result
    }

    /// Execute on_response_trailers callback for specified modules
    ///
    /// Called when response trailers are received (HTTP/2, gRPC).
    pub async fn on_response_trailers_with_modules(
        &self,
        modules: &[ModuleRef],
        trailers: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> FilterResult {
        let resolved = resolve_effective_modules(&self.registry, modules);

        if resolved.is_empty() {
            return FilterResult::Continue {
                headers: trailers,
                body: None,
            };
        }

        let mut current_trailers = trailers;

        // Execute in reverse order for response
        for (module, config) in resolved.iter().rev() {
            let result = self
                .execute_on_response_trailers(module, config, &current_trailers)
                .await;

            match result {
                Ok(ModuleResult::Continue { modified_headers }) => {
                    if let Some(h) = modified_headers {
                        current_trailers = h;
                    }
                }
                // トレーラーの段の後に待つものは無い（B-300）
                Ok(ModuleResult::Pause) => {
                    if let Some(resp) =
                        self.record_failure(module, "response_trailers", "unsupported_pause", None)
                    {
                        return FilterResult::LocalResponse(resp);
                    }
                }
                Ok(ModuleResult::LocalResponse(resp)) => {
                    return FilterResult::LocalResponse(resp);
                }
                Err(e) => {
                    if let Some(resp) = self.on_failure(module, "response_trailers", &e) {
                        return FilterResult::LocalResponse(resp);
                    }
                }
            }
        }

        FilterResult::Continue {
            headers: current_trailers,
            body: None,
        }
    }

    /// Execute on_response_trailers for a single module
    async fn execute_on_response_trailers(
        &self,
        module: &LoadedModule,
        configuration: &Arc<[u8]>,
        trailers: &[(Vec<u8>, Vec<u8>)],
    ) -> anyhow::Result<ModuleResult> {
        // Create context
        let mut http_ctx = HttpContext::for_module(1, module);
        http_ctx.response_trailers = trailers.to_vec();
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_root_id = module.root_id.clone();
        http_ctx.plugin_configuration = configuration.clone();
        http_ctx.plugin_data = module.data_files.clone();

        let mut sess = self
            .open_session(module, configuration, http_ctx, None)
            .await
            .map_err(|(e, _)| e)?;

        // Call proxy_on_response_trailers
        // Signature: (context_id, num_trailers) -> action
        let run = match sess
            .instance
            .get_typed_func::<(i32, i32), i32>(&mut sess.store, "proxy_on_response_trailers")
        {
            Ok(func) => {
                let num_trailers = trailers.len() as i32;
                func.call_async(&mut sess.store, (sess.context_id, num_trailers))
                    .await
            }
            Err(_) => Ok(0),
        };

        // B-299: トラップの前のローカルレスポンスも返す
        let (ok, settled) = self.settle(module, &mut sess, run).await;
        let result = match settled {
            Settled::Local(r) => Ok(ModuleResult::LocalResponse(r)),
            Settled::Failed(e) => Err(e),
            Settled::Pause { .. } => Ok(ModuleResult::Pause),
            Settled::Continue => {
                let ctx = &mut sess.store.data_mut().http_ctx;
                let modified_headers = if ctx.response_headers_modified {
                    Some(std::mem::take(&mut ctx.response_trailers))
                } else {
                    None
                };
                Ok(ModuleResult::Continue { modified_headers })
            }
        };
        self.close_session(module, sess, ok, None, false).await;
        result
    }

    /// Execute on_queue_ready callback for a module
    ///
    /// Called when a message is enqueued to a shared queue that the module is subscribed to.
    /// F-148: 単一モジュール名のみを取る API（ルート文脈が無い）のため、ここでは
    /// ルート単位の上書き設定は解決できず、常にモジュール定義側の `module.configuration` を使う。
    pub async fn on_queue_ready(&self, module_name: &str, queue_id: u32) {
        let module = match self.registry.get_module(module_name) {
            Some(m) => m,
            None => return,
        };

        if let Err(e) = self.execute_on_queue_ready(&module, queue_id).await {
            ftlog::error!("[wasm:{}] on_queue_ready error: {}", module_name, e);
        }
    }

    /// Execute on_queue_ready for a single module
    async fn execute_on_queue_ready(
        &self,
        module: &LoadedModule,
        queue_id: u32,
    ) -> anyhow::Result<()> {
        // Create context
        let http_ctx = HttpContext::for_module(1, module);
        let (mut store, instance) = self
            .fresh_instance(module, &module.configuration, http_ctx)
            .await?;
        let root_context_id = 1i32;

        // Call proxy_on_queue_ready
        // Signature: (context_id, queue_id) -> void
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_queue_ready")
        {
            match func
                .call_async(&mut store, (root_context_id, queue_id as i32))
                .await
            {
                Ok(()) => {
                    ftlog::debug!(
                        "[wasm:{}] proxy_on_queue_ready({}, {}) OK",
                        module.name,
                        root_context_id,
                        queue_id
                    )
                }
                Err(e) => ftlog::debug!("[wasm:{}] proxy_on_queue_ready error: {}", module.name, e),
            }
        }

        Ok(())
    }

    // =========================================================================
    // P3: gRPC Response Callbacks
    // =========================================================================

    /// Execute on_grpc_receive_initial_metadata callback for a module
    ///
    /// Called when initial metadata is received from a gRPC call.
    /// F-148: 単一モジュール名のみを取る API（ルート文脈が無い）のため、ここでは
    /// ルート単位の上書き設定は解決できず、常にモジュール定義側の `module.configuration` を使う。
    #[cfg(feature = "grpc")]
    pub async fn on_grpc_receive_initial_metadata(
        &self,
        module_name: &str,
        call_id: u32,
        headers: &[(bytes::Bytes, bytes::Bytes)],
    ) {
        let module = match self.registry.get_module(module_name) {
            Some(m) => m,
            None => return,
        };

        if let Err(e) = self
            .execute_on_grpc_receive_initial_metadata(&module, call_id, headers)
            .await
        {
            ftlog::error!(
                "[wasm:{}] on_grpc_receive_initial_metadata error: {}",
                module_name,
                e
            );
        }
    }

    #[cfg(feature = "grpc")]
    async fn execute_on_grpc_receive_initial_metadata(
        &self,
        module: &LoadedModule,
        call_id: u32,
        headers: &[(bytes::Bytes, bytes::Bytes)],
    ) -> anyhow::Result<()> {
        // Create context
        let mut http_ctx = HttpContext::for_module(1, module);
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_root_id = module.root_id.clone();
        http_ctx.plugin_configuration = module.configuration.clone();
        http_ctx.plugin_data = module.data_files.clone();
        // F-134: MapType::GrpcReceiveInitialMetadata(4) のバックストア。
        // ゲストが proxy_on_grpc_receive_initial_metadata 内で
        // proxy_get_header_map_pairs(GrpcReceiveInitialMetadata) を呼べるようにする。
        // F-160: `grpc_receive_initial_metadata` の型（`Vec<(Vec<u8>, Vec<u8>)>`）は
        // `headers.rs::get_headers` が全マップ共通で使うため変更しない
        // （変更すると 15 ファイル・100 箇所に波及するため対象外）。
        // 境界の `to_vec()` はここでの 1 回のみ（変更前と同等のコピー回数）。
        http_ctx.grpc_receive_initial_metadata = headers
            .iter()
            .map(|(k, v)| (k.to_vec(), v.to_vec()))
            .collect();

        let (mut store, instance) = self
            .fresh_instance(module, &module.configuration, http_ctx)
            .await?;
        let root_context_id = 1i32;
        let http_context_id = 2i32;

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func
                .call_async(&mut store, (http_context_id, root_context_id))
                .await;
        }

        // Call proxy_on_grpc_receive_initial_metadata
        // Signature: (context_id, call_id, num_headers) -> void
        if let Ok(func) = instance.get_typed_func::<(i32, i32, i32), ()>(
            &mut store,
            "proxy_on_grpc_receive_initial_metadata",
        ) {
            let num_headers = headers.len() as i32;
            match func
                .call_async(&mut store, (http_context_id, call_id as i32, num_headers))
                .await
            {
                Ok(()) => {
                    ftlog::debug!(
                        "[wasm:{}] proxy_on_grpc_receive_initial_metadata({}, {}, {}) OK",
                        module.name,
                        http_context_id,
                        call_id,
                        num_headers
                    )
                }
                Err(e) => ftlog::debug!(
                    "[wasm:{}] proxy_on_grpc_receive_initial_metadata error: {}",
                    module.name,
                    e
                ),
            }
        }

        Ok(())
    }

    /// Execute on_grpc_receive callback for a module
    ///
    /// Called when a gRPC message is received.
    /// F-148: 単一モジュール名のみを取る API（ルート文脈が無い）のため、ここでは
    /// ルート単位の上書き設定は解決できず、常にモジュール定義側の `module.configuration` を使う。
    #[cfg(feature = "grpc")]
    pub async fn on_grpc_receive(&self, module_name: &str, call_id: u32, message: &[u8]) {
        let module = match self.registry.get_module(module_name) {
            Some(m) => m,
            None => return,
        };

        if let Err(e) = self
            .execute_on_grpc_receive(&module, call_id, message)
            .await
        {
            ftlog::error!("[wasm:{}] on_grpc_receive error: {}", module_name, e);
        }
    }

    #[cfg(feature = "grpc")]
    async fn execute_on_grpc_receive(
        &self,
        module: &LoadedModule,
        call_id: u32,
        message: &[u8],
    ) -> anyhow::Result<()> {
        // Create context
        let mut http_ctx = HttpContext::for_module(1, module);
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_root_id = module.root_id.clone();
        http_ctx.plugin_configuration = module.configuration.clone();
        http_ctx.plugin_data = module.data_files.clone();
        // F-134: BufferType::GrpcReceiveBuffer(5) のバックストア。
        http_ctx.grpc_receive_message = bytes::Bytes::copy_from_slice(message);

        let (mut store, instance) = self
            .fresh_instance(module, &module.configuration, http_ctx)
            .await?;
        let root_context_id = 1i32;
        let http_context_id = 2i32;

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func
                .call_async(&mut store, (http_context_id, root_context_id))
                .await;
        }

        // Call proxy_on_grpc_receive
        // Signature: (context_id, call_id, message_size) -> void
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32, i32), ()>(&mut store, "proxy_on_grpc_receive")
        {
            let message_size = message.len() as i32;
            match func
                .call_async(&mut store, (http_context_id, call_id as i32, message_size))
                .await
            {
                Ok(()) => {
                    ftlog::debug!(
                        "[wasm:{}] proxy_on_grpc_receive({}, {}, {}) OK",
                        module.name,
                        http_context_id,
                        call_id,
                        message_size
                    )
                }
                Err(e) => {
                    ftlog::debug!("[wasm:{}] proxy_on_grpc_receive error: {}", module.name, e)
                }
            }
        }

        Ok(())
    }

    /// Execute on_grpc_receive_trailing_metadata callback for a module
    ///
    /// Called when trailing metadata is received from a gRPC call.
    /// F-148: 単一モジュール名のみを取る API（ルート文脈が無い）のため、ここでは
    /// ルート単位の上書き設定は解決できず、常にモジュール定義側の `module.configuration` を使う。
    #[cfg(feature = "grpc")]
    pub async fn on_grpc_receive_trailing_metadata(
        &self,
        module_name: &str,
        call_id: u32,
        trailers: &[(bytes::Bytes, bytes::Bytes)],
    ) {
        let module = match self.registry.get_module(module_name) {
            Some(m) => m,
            None => return,
        };

        if let Err(e) = self
            .execute_on_grpc_receive_trailing_metadata(&module, call_id, trailers)
            .await
        {
            ftlog::error!(
                "[wasm:{}] on_grpc_receive_trailing_metadata error: {}",
                module_name,
                e
            );
        }
    }

    #[cfg(feature = "grpc")]
    async fn execute_on_grpc_receive_trailing_metadata(
        &self,
        module: &LoadedModule,
        call_id: u32,
        trailers: &[(bytes::Bytes, bytes::Bytes)],
    ) -> anyhow::Result<()> {
        // Create context
        let mut http_ctx = HttpContext::for_module(1, module);
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_root_id = module.root_id.clone();
        http_ctx.plugin_configuration = module.configuration.clone();
        http_ctx.plugin_data = module.data_files.clone();
        // F-134: MapType::GrpcReceiveTrailingMetadata(5) のバックストア。
        // F-160: `grpc_receive_trailing_metadata` の型は headers.rs 共通のため
        // 変更しない。境界の `to_vec()` はここでの 1 回のみ。
        http_ctx.grpc_receive_trailing_metadata = trailers
            .iter()
            .map(|(k, v)| (k.to_vec(), v.to_vec()))
            .collect();

        let (mut store, instance) = self
            .fresh_instance(module, &module.configuration, http_ctx)
            .await?;
        let root_context_id = 1i32;
        let http_context_id = 2i32;

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func
                .call_async(&mut store, (http_context_id, root_context_id))
                .await;
        }

        // Call proxy_on_grpc_receive_trailing_metadata
        // Signature: (context_id, call_id, num_trailers) -> void
        if let Ok(func) = instance.get_typed_func::<(i32, i32, i32), ()>(
            &mut store,
            "proxy_on_grpc_receive_trailing_metadata",
        ) {
            let num_trailers = trailers.len() as i32;
            match func
                .call_async(&mut store, (http_context_id, call_id as i32, num_trailers))
                .await
            {
                Ok(()) => {
                    ftlog::debug!(
                        "[wasm:{}] proxy_on_grpc_receive_trailing_metadata({}, {}, {}) OK",
                        module.name,
                        http_context_id,
                        call_id,
                        num_trailers
                    )
                }
                Err(e) => ftlog::debug!(
                    "[wasm:{}] proxy_on_grpc_receive_trailing_metadata error: {}",
                    module.name,
                    e
                ),
            }
        }

        Ok(())
    }

    /// Execute on_grpc_close callback for a module
    ///
    /// Called when a gRPC call is closed.
    /// F-148: 単一モジュール名のみを取る API（ルート文脈が無い）のため、ここでは
    /// ルート単位の上書き設定は解決できず、常にモジュール定義側の `module.configuration` を使う。
    #[cfg(feature = "grpc")]
    pub async fn on_grpc_close(&self, module_name: &str, call_id: u32, status_code: i32) {
        let module = match self.registry.get_module(module_name) {
            Some(m) => m,
            None => return,
        };

        if let Err(e) = self
            .execute_on_grpc_close(&module, call_id, status_code)
            .await
        {
            ftlog::error!("[wasm:{}] on_grpc_close error: {}", module_name, e);
        }
    }

    #[cfg(feature = "grpc")]
    async fn execute_on_grpc_close(
        &self,
        module: &LoadedModule,
        call_id: u32,
        status_code: i32,
    ) -> anyhow::Result<()> {
        // Create context
        let mut http_ctx = HttpContext::for_module(1, module);
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_root_id = module.root_id.clone();
        http_ctx.plugin_configuration = module.configuration.clone();
        http_ctx.plugin_data = module.data_files.clone();

        let (mut store, instance) = self
            .fresh_instance(module, &module.configuration, http_ctx)
            .await?;
        let root_context_id = 1i32;
        let http_context_id = 2i32;

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func
                .call_async(&mut store, (http_context_id, root_context_id))
                .await;
        }

        // Call proxy_on_grpc_close
        // Signature: (context_id, call_id, status_code) -> void
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32, i32), ()>(&mut store, "proxy_on_grpc_close")
        {
            match func
                .call_async(&mut store, (http_context_id, call_id as i32, status_code))
                .await
            {
                Ok(()) => {
                    ftlog::debug!(
                        "[wasm:{}] proxy_on_grpc_close({}, {}, {}) OK",
                        module.name,
                        http_context_id,
                        call_id,
                        status_code
                    )
                }
                Err(e) => {
                    ftlog::debug!("[wasm:{}] proxy_on_grpc_close error: {}", module.name, e)
                }
            }
        }

        Ok(())
    }

    // ========================================================================
    // Network Filter (L4 TCP/UDP stream, F-133)
    //
    // Proxy-Wasm の network filter ABI（`proxy_on_new_connection` /
    // `proxy_on_downstream_data` / `proxy_on_upstream_data` /
    // `proxy_on_downstream_connection_close` / `proxy_on_upstream_connection_close`）。
    // F-134: 以前は接尾辞 `_connection` を欠いた誤った名前
    // （`proxy_on_downstream_close`/`proxy_on_upstream_close`）を呼んでおり、実 SDK
    // （proxy-wasm-rust-sdk 等）でビルドしたモジュールではクローズコールバックが
    // 一度も発火しなかった（フィクスチャが誤った名前に手書きで合わせていたため
    // E2E は偶然通っていた）。HTTP コンテキストと異なり
    // ヘッダが無く、生バイト列のみを扱う。`proxy_get_buffer_bytes`/`proxy_set_buffer_bytes`
    // の `BufferType::DownstreamData=2`/`UpstreamData=3` 経由でデータを読み書きする。
    // ========================================================================

    /// Execute `proxy_on_new_connection` for specified modules（接続確立時に 1 回）。
    pub async fn on_new_connection_with_modules(&self, modules: &[ModuleRef]) -> NetworkAction {
        let resolved = resolve_effective_modules(&self.registry, modules);

        for (module, config) in &resolved {
            match self.execute_on_new_connection(module, config).await {
                Ok(NetworkAction::Close) => return NetworkAction::Close,
                Ok(NetworkAction::Continue) => {}
                // B-319: 失敗は `failure_policy` に従う（以前は記録するだけで接続を通していた。F10）
                Err(e) => {
                    if self.on_failure(module, "new_connection", &e).is_some() {
                        return NetworkAction::Close;
                    }
                }
            }
        }
        NetworkAction::Continue
    }

    async fn execute_on_new_connection(
        &self,
        module: &LoadedModule,
        configuration: &Arc<[u8]>,
    ) -> anyhow::Result<NetworkAction> {
        let mut http_ctx = HttpContext::for_module(1, module);
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_root_id = module.root_id.clone();
        http_ctx.plugin_configuration = configuration.clone();
        http_ctx.plugin_data = module.data_files.clone();

        let (mut store, instance) = self.fresh_instance(module, configuration, http_ctx).await?;
        let stream_context_id = 2i32;
        let root_context_id = 1i32;
        // network filter (stream) context は root context の子として作る。
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func
                .call_async(&mut store, (stream_context_id, root_context_id))
                .await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32,), i32>(&mut store, "proxy_on_new_connection")
        {
            let _ = func.call_async(&mut store, (stream_context_id,)).await?;
        }

        if store.data().http_ctx.close_requested {
            Ok(NetworkAction::Close)
        } else {
            Ok(NetworkAction::Continue)
        }
    }

    /// Execute `proxy_on_downstream_data`（クライアント → プロキシ方向のデータ）を
    /// 指定モジュール群に対して実行する。
    pub async fn on_downstream_data_with_modules(
        &self,
        modules: &[ModuleRef],
        data: bytes::Bytes,
        end_of_stream: bool,
    ) -> NetworkFilterResult {
        self.run_network_data_filters(
            modules,
            data,
            end_of_stream,
            NetworkDataDirection::Downstream,
        )
        .await
    }

    /// Execute `proxy_on_upstream_data`（バックエンド → プロキシ方向のデータ）を
    /// 指定モジュール群に対して実行する。
    pub async fn on_upstream_data_with_modules(
        &self,
        modules: &[ModuleRef],
        data: bytes::Bytes,
        end_of_stream: bool,
    ) -> NetworkFilterResult {
        self.run_network_data_filters(modules, data, end_of_stream, NetworkDataDirection::Upstream)
            .await
    }

    async fn run_network_data_filters(
        &self,
        modules: &[ModuleRef],
        data: bytes::Bytes,
        end_of_stream: bool,
        direction: NetworkDataDirection,
    ) -> NetworkFilterResult {
        let resolved = resolve_effective_modules(&self.registry, modules);

        if resolved.is_empty() {
            return NetworkFilterResult::Continue { data };
        }

        let mut current = data;
        for (module, config) in &resolved {
            // Bytes の clone は参照カウント共有のみ（deep copy なし）。
            let result = self
                .execute_on_network_data(module, config, current.clone(), end_of_stream, direction)
                .await;
            match result {
                Ok(NetworkDataModuleResult::Continue { data }) => current = data,
                Ok(NetworkDataModuleResult::Pause) => return NetworkFilterResult::Pause,
                Ok(NetworkDataModuleResult::Close) => return NetworkFilterResult::Close,
                // B-319: 失敗は `failure_policy` に従う（fail_closed なら閉じる。F10）
                Err(e) => {
                    if self.on_failure(module, direction.phase(), &e).is_some() {
                        return NetworkFilterResult::Close;
                    }
                }
            }
        }

        NetworkFilterResult::Continue { data: current }
    }

    async fn execute_on_network_data(
        &self,
        module: &LoadedModule,
        configuration: &Arc<[u8]>,
        data: bytes::Bytes,
        end_of_stream: bool,
        direction: NetworkDataDirection,
    ) -> anyhow::Result<NetworkDataModuleResult> {
        let data_len = data.len() as i32;
        let mut http_ctx = HttpContext::for_module(1, module);
        match direction {
            NetworkDataDirection::Downstream => {
                http_ctx.downstream_data = crate::wasm::context::BodyBuffer::Shared(data)
            }
            NetworkDataDirection::Upstream => {
                http_ctx.upstream_data = crate::wasm::context::BodyBuffer::Shared(data)
            }
        }
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_root_id = module.root_id.clone();
        http_ctx.plugin_configuration = configuration.clone();
        http_ctx.plugin_data = module.data_files.clone();

        let (mut store, instance) = self.fresh_instance(module, configuration, http_ctx).await?;
        let stream_context_id = 2i32;
        let root_context_id = 1i32;
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func
                .call_async(&mut store, (stream_context_id, root_context_id))
                .await;
        }

        let callback_name = direction.callback_name();
        let end_of_stream_i32 = if end_of_stream { 1 } else { 0 };
        let mut action = ACTION_CONTINUE;
        if let Ok(func) = instance.get_typed_func::<(i32, i32, i32), i32>(&mut store, callback_name)
        {
            action = func
                .call_async(&mut store, (stream_context_id, data_len, end_of_stream_i32))
                .await?;
        }

        let state = store.data();
        // `proxy_close_stream` 呼び出し（close_requested）を最優先で判定し、次に
        // `proxy_action_t`（Continue=0/Pause=1）の戻り値で Pause を判定する。
        if state.http_ctx.close_requested {
            return Ok(NetworkDataModuleResult::Close);
        }
        if action == ACTION_PAUSE {
            return Ok(NetworkDataModuleResult::Pause);
        }

        let modified = match direction {
            NetworkDataDirection::Downstream => {
                if state.http_ctx.downstream_data_modified {
                    Some(state.http_ctx.downstream_data.share())
                } else {
                    None
                }
            }
            NetworkDataDirection::Upstream => {
                if state.http_ctx.upstream_data_modified {
                    Some(state.http_ctx.upstream_data.share())
                } else {
                    None
                }
            }
        };

        match modified {
            Some(data) => Ok(NetworkDataModuleResult::Continue { data }),
            None => Ok(NetworkDataModuleResult::Continue {
                data: match direction {
                    NetworkDataDirection::Downstream => state.http_ctx.downstream_data.share(),
                    NetworkDataDirection::Upstream => state.http_ctx.upstream_data.share(),
                },
            }),
        }
    }

    /// Execute `proxy_on_downstream_connection_close`（クライアント側接続クローズ通知、1 回のみ）。
    ///
    /// F-134: ABI v0.2.1 の正しいエクスポート名は `proxy_on_downstream_connection_close`
    /// （旧実装は `_connection` を欠いた `proxy_on_downstream_close` を呼んでいたため、
    /// 実 SDK ビルドのモジュールでは一度も発火しなかった）。
    pub async fn on_downstream_close_with_modules(&self, modules: &[ModuleRef]) {
        self.run_network_close(modules, "proxy_on_downstream_connection_close")
            .await
    }

    /// Execute `proxy_on_upstream_connection_close`（バックエンド側接続クローズ通知、1 回のみ）。
    ///
    /// F-134: ABI v0.2.1 の正しいエクスポート名は `proxy_on_upstream_connection_close`
    /// （旧実装は `_connection` を欠いた `proxy_on_upstream_close` を呼んでいたため、
    /// 実 SDK ビルドのモジュールでは一度も発火しなかった）。
    pub async fn on_upstream_close_with_modules(&self, modules: &[ModuleRef]) {
        self.run_network_close(modules, "proxy_on_upstream_connection_close")
            .await
    }

    async fn run_network_close(&self, modules: &[ModuleRef], callback_name: &str) {
        let resolved = resolve_effective_modules(&self.registry, modules);

        for (module, config) in &resolved {
            if let Err(e) = self
                .execute_on_network_close(module, config, callback_name)
                .await
            {
                ftlog::error!("[wasm:{}] {} error: {}", module.name, callback_name, e);
            }
        }
    }

    async fn execute_on_network_close(
        &self,
        module: &LoadedModule,
        configuration: &Arc<[u8]>,
        callback_name: &str,
    ) -> anyhow::Result<()> {
        let mut http_ctx = HttpContext::for_module(1, module);
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_root_id = module.root_id.clone();
        http_ctx.plugin_configuration = configuration.clone();
        http_ctx.plugin_data = module.data_files.clone();

        let (mut store, instance) = self.fresh_instance(module, configuration, http_ctx).await?;
        let stream_context_id = 2i32;
        let root_context_id = 1i32;
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func
                .call_async(&mut store, (stream_context_id, root_context_id))
                .await;
        }

        // Signature: (context_id, close_type) -> void（close_type は未使用のため 0 固定）
        if let Ok(func) = instance.get_typed_func::<(i32, i32), ()>(&mut store, callback_name) {
            let _ = func.call_async(&mut store, (stream_context_id, 0)).await;
        }

        Ok(())
    }
}

/// network data (downstream/upstream) の方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NetworkDataDirection {
    Downstream,
    Upstream,
}

impl NetworkDataDirection {
    fn callback_name(self) -> &'static str {
        match self {
            NetworkDataDirection::Downstream => "proxy_on_downstream_data",
            NetworkDataDirection::Upstream => "proxy_on_upstream_data",
        }
    }
    /// 失敗の記録（`failure_policy`・メトリクス）の段の名前
    fn phase(self) -> &'static str {
        match self {
            NetworkDataDirection::Downstream => "downstream_data",
            NetworkDataDirection::Upstream => "upstream_data",
        }
    }
}

/// `proxy_on_new_connection` の結果（Close は `proxy_close_stream` 呼び出しで検知）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkAction {
    Continue,
    Close,
}

/// 単一モジュールの network data フィルタ実行結果（内部用）。
enum NetworkDataModuleResult {
    Continue { data: bytes::Bytes },
    Pause,
    Close,
}

/// network data フィルタチェーン全体の実行結果。
pub enum NetworkFilterResult {
    /// 転送を継続する（`data` は書き換え後、または未変更の場合はそのまま）。
    Continue { data: bytes::Bytes },
    /// 判断にもっとデータが要る（呼び出し側はデータを転送せずに持ち、次に読んだ分と合わせて渡し直す）。
    Pause,
    /// 接続をクローズする（`proxy_close_stream` が呼ばれた）。
    Close,
}

/// Result from a single module execution
enum ModuleResult {
    Continue {
        modified_headers: Option<Vec<(Vec<u8>, Vec<u8>)>>,
    },
    Pause,
    LocalResponse(LocalResponse),
}

/// ヘッダ系フィルタの実行結果（F-43: ヘッダ本体はムーブスルーで別途返すため持たない）
/// 要求の本文のコールバックに見せる要求の情報（F-271。モジュールが本文の段でパス・ヘッダを読める）
pub struct RequestMeta<'a> {
    pub method: &'a Arc<str>,
    pub path: &'a Arc<str>,
    pub headers: &'a [(Vec<u8>, Vec<u8>)],
    pub client_ip: &'a Arc<str>,
}

/// 1 回の HTTP のコールバックに使うインスタンス（F-271。[`FilterEngine::open_session`]）
struct Session {
    store: Store<HostState>,
    instance: wasmtime::Instance,
    /// このコールバックの http context の ID
    context_id: i32,
    /// `instance_reuse` のインスタンス（終わったらプールへ戻す）
    slot: SlotGuard,
    /// context が要求の終わり（`on_log`）まで続く（B-177）
    request_scoped: bool,
    /// プールのインスタンスの次の http context の ID
    next_context_id: i32,
}

/// 取り出しているプールのインスタンスの枠。`close_session` を通らずに捨てられたら（エラーで途中で返った等）
/// インスタンスは戻らないので枠を捨てる（待っている要求を起こしてエラーにする。永久に待たせない）
struct SlotGuard(Option<std::rc::Rc<InstanceSlot>>);

impl SlotGuard {
    fn as_ref(&self) -> Option<&std::rc::Rc<InstanceSlot>> {
        self.0.as_ref()
    }

    fn take(&mut self) -> Option<std::rc::Rc<InstanceSlot>> {
        self.0.take()
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if let Some(slot) = self.0.take() {
            slot.discard();
        }
    }
}

impl Session {
    fn pooled(
        mut p: PooledInstance,
        slot: std::rc::Rc<InstanceSlot>,
        context_id: i32,
        request_scoped: bool,
        http_ctx: HttpContext,
    ) -> Self {
        p.store.data_mut().http_ctx = http_ctx;
        Session {
            store: p.store,
            instance: p.instance,
            context_id,
            slot: SlotGuard(Some(slot)),
            request_scoped,
            next_context_id: p.next_context_id,
        }
    }

    fn into_ctx(self) -> HttpContext {
        self.store.into_data().http_ctx
    }
}

enum ModuleAction {
    Continue,
    /// 解決できない呼び出しの無い Pause（`grpc` は gRPC の呼び出しを待っている）
    Pause {
        grpc: bool,
    },
    LocalResponse(LocalResponse),
}

/// Result from filter chain execution
pub enum FilterResult {
    /// Continue with potentially modified headers/body
    Continue {
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        body: Option<Vec<u8>>,
    },
    /// Send a local response instead of proxying
    LocalResponse(LocalResponse),
}

/// Result from body module execution (internal)
enum BodyModuleResult {
    Continue { modified_body: Option<bytes::Bytes> },
    Pause,
    LocalResponse(LocalResponse),
}

/// Result from body filter chain execution
pub enum BodyFilterResult {
    /// Continue with potentially modified body（F-61: `Bytes` によるゼロコピー返却）
    Continue { body: bytes::Bytes },
    /// Send a local response instead of proxying
    LocalResponse(LocalResponse),
}

#[cfg(test)]
mod exec_smoke_tests {
    use super::*;
    use crate::wasm::types::{ModuleConfig, WasmConfig, WasmDefaults};
    use crate::wasm::ModuleCapabilities;

    fn header_filter_engine() -> Option<FilterEngine> {
        header_filter_engine_with_interpreter(false)
    }

    fn header_filter_engine_with_interpreter(interpreter: bool) -> Option<FilterEngine> {
        let path = "tests/fixtures/wasm/header_filter.wasm";
        if !std::path::Path::new(path).exists() {
            return None;
        }
        let config = WasmConfig {
            enabled: true,
            defaults: WasmDefaults::default(),
            modules: vec![ModuleConfig {
                name: "header_filter".to_string(),
                path: path.to_string(),
                configuration: crate::wasm_plugin_config::PluginConfiguration::default(),
                capabilities: Default::default(),
                ..Default::default()
            }],
            interpreter,
            grpc_connect_timeout_ms: crate::wasm::DEFAULT_GRPC_CONNECT_TIMEOUT_MS,
            http_call_max_timeout_ms: crate::wasm::DEFAULT_HTTP_CALL_MAX_TIMEOUT_MS,
            shared_data_max_keys: crate::wasm::DEFAULT_SHARED_DATA_MAX_KEYS,
        };
        FilterEngine::new(&config).ok()
    }

    /// 実モジュール（header_filter.wasm）を実行し、パニックせずに完走することを検証する。
    /// F-27 で async_support と同期 call が混在すると wasmtime が panic するため、
    /// このテストが落ちれば WASM 実行パスが壊れていることを意味する。
    #[test]
    fn smoke_execute_request_headers_runs_without_panic() {
        let engine = match header_filter_engine() {
            Some(e) => e,
            None => {
                eprintln!("wasm fixture missing; skipping");
                return;
            }
        };
        let headers = vec![
            (b":path".to_vec(), b"/".to_vec()),
            (b":method".to_vec(), b"GET".to_vec()),
            (b":authority".to_vec(), b"example.com".to_vec()),
        ];
        // async になった WASM 実行をテスト用に駆動する（背景スレッド扱い）。
        let result = futures::executor::block_on(engine.on_request_headers_with_modules(
            &[ModuleRef::from_name("header_filter")],
            &Arc::from("/"),
            &Arc::from("GET"),
            headers,
            &Arc::from("127.0.0.1"),
            true,
            None,
        ));
        // 中身は問わない。パニックせず FilterResult を返すことだけを確認する。
        match result {
            FilterResult::Continue { .. } | FilterResult::LocalResponse(_) => {}
        }
    }

    /// B-173 回帰テスト: 他の要求の WASM 呼び出しは、すでに締切を入れた store の締切を減らさない。
    /// 以前は呼び出しのたびに全エンジン共有のエポックを 1 進めていたので、複数のワーカーが同時に動くと
    /// 「他のスレッドの 10 回の呼び出し」の間に終わらない呼び出しが `interrupt` でトラップし、フィルタが
    /// 飛ばされていた。ここでは store を開いた後に他のスレッドで要求を `epoch_deadline` の 3 倍だけ同時に実行
    /// してからコールバックを呼ぶ（修正前はトラップする）。
    #[test]
    fn b173_other_calls_do_not_consume_an_armed_deadline() {
        let path = "tests/fixtures/wasm/header_filter.wasm";
        if !std::path::Path::new(path).exists() {
            eprintln!("wasm fixture missing; skipping");
            return;
        }
        // header_filter は要求ヘッダを書き足すので、読み書きを許す（許さないとモジュールが panic する）
        let config = WasmConfig {
            enabled: true,
            defaults: WasmDefaults::default(),
            modules: vec![ModuleConfig {
                name: "header_filter".to_string(),
                path: path.to_string(),
                configuration: crate::wasm_plugin_config::PluginConfiguration::default(),
                capabilities: ModuleCapabilities {
                    allow_logging: true,
                    allow_request_headers_read: true,
                    allow_request_headers_write: true,
                    allow_response_headers_read: true,
                    allow_response_headers_write: true,
                    ..Default::default()
                },
                ..Default::default()
            }],
            interpreter: false,
            grpc_connect_timeout_ms: crate::wasm::DEFAULT_GRPC_CONNECT_TIMEOUT_MS,
            http_call_max_timeout_ms: crate::wasm::DEFAULT_HTTP_CALL_MAX_TIMEOUT_MS,
            shared_data_max_keys: crate::wasm::DEFAULT_SHARED_DATA_MAX_KEYS,
        };
        let engine = FilterEngine::new(&config).expect("engine");
        let module = engine
            .get_module("header_filter")
            .expect("module must be loaded");
        let headers = || {
            vec![
                (b":path".to_vec(), b"/".to_vec()),
                (b":method".to_vec(), b"GET".to_vec()),
                (b":authority".to_vec(), b"example.com".to_vec()),
            ]
        };
        let mut http_ctx = HttpContext::for_module(1, &module);
        http_ctx.set_request(
            Arc::from("GET"),
            Arc::from("/"),
            headers(),
            Arc::from("127.0.0.1"),
        );
        let configuration = module.configuration.clone();
        let mut sess = match futures::executor::block_on(engine.open_session(
            &module,
            &configuration,
            http_ctx,
            None,
        )) {
            Ok(s) => s,
            Err((e, _)) => panic!("open_session must not error: {e}"),
        };
        // 他のワーカー（4 スレッド）が同時に要求を処理する（合わせて epoch_deadline の 3 倍の呼び出し）
        let per_thread = engine.epoch_deadline * 3 / 4 + 1;
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..per_thread {
                        let _ =
                            futures::executor::block_on(engine.on_request_headers_with_modules(
                                &[ModuleRef::from_name("header_filter")],
                                &Arc::from("/"),
                                &Arc::from("GET"),
                                headers(),
                                &Arc::from("127.0.0.1"),
                                true,
                                None,
                            ));
                    }
                });
            }
        });
        let r = futures::executor::block_on(engine.run_headers_module(
            &module,
            &mut sess,
            "proxy_on_request_headers",
            3,
            true,
        ));
        assert!(
            r.is_ok(),
            "a callback must not trap because other requests ran WASM in between (B-173): {:?}",
            r.err()
        );
    }

    fn http_call_filter_engine() -> Option<FilterEngine> {
        let path = "tests/fixtures/wasm/http_call_filter.wasm";
        if !std::path::Path::new(path).exists() {
            return None;
        }
        let config = WasmConfig {
            enabled: true,
            defaults: WasmDefaults::default(),
            modules: vec![ModuleConfig {
                name: "http_call_filter".to_string(),
                path: path.to_string(),
                // 空文字列 → モジュールは既定の "backend-pool" を使う（examples/wasm-filters/http-call-filter）。
                // upstream_groups が未設定（テスト用 CURRENT_CONFIG）でも 502 応答経路で
                // resume 機構自体は同じように駆動されるため、実バックエンドは不要。
                configuration: crate::wasm_plugin_config::PluginConfiguration::default(),
                capabilities: ModuleCapabilities {
                    allow_logging: true,
                    allow_request_headers_read: true,
                    allow_request_headers_write: true,
                    allow_send_local_response: true,
                    allow_http_calls: true,
                    ..Default::default()
                },
                ..Default::default()
            }],
            interpreter: false,
            grpc_connect_timeout_ms: crate::wasm::DEFAULT_GRPC_CONNECT_TIMEOUT_MS,
            http_call_max_timeout_ms: crate::wasm::DEFAULT_HTTP_CALL_MAX_TIMEOUT_MS,
            shared_data_max_keys: crate::wasm::DEFAULT_SHARED_DATA_MAX_KEYS,
        };
        FilterEngine::new(&config).ok()
    }

    /// B-56 回帰テスト（決定的）: `resolve_pending_http_calls_inline` が
    /// `proxy_on_http_call_response` で resume する **直前**に epoch 締切を
    /// 引き直すこと（本チケットの修正）を、実際に store をサスペンドさせた状態から
    /// エンジン共有の epoch を `epoch_deadline` 分以上進めて検証する。
    ///
    /// 負荷や並行リクエストは一切使わない — `Engine::increment_epoch()` を
    /// 直接呼んで「他のリクエストが epoch_deadline 回以上 WASM を呼んだ」状態を
    /// 決定的に再現する（B-173 以降のエポックは時計で、`increment_epoch()` は時間の経過の代わり）。
    ///
    /// このテストは `resolve_pending_http_calls_inline` 内の
    /// `store.set_epoch_deadline(...)`
    /// （本チケットの修正そのもの）を一時的に取り除いた状態で実行すると
    /// `wasm trap: interrupt` で FAIL することを確認済み（コミットメッセージ参照）。
    #[test]
    fn b56_http_call_resume_survives_epoch_pressure_while_suspended() {
        let engine = match http_call_filter_engine() {
            Some(e) => e,
            None => {
                eprintln!("wasm fixture missing; skipping");
                return;
            }
        };
        let module = engine
            .get_module("http_call_filter")
            .expect("module must be loaded");

        let headers = vec![
            (b":path".to_vec(), b"/".to_vec()),
            (b":method".to_vec(), b"GET".to_vec()),
            (b":authority".to_vec(), b"example.com".to_vec()),
        ];
        let num_headers = headers.len() as i32;

        let mut http_ctx = HttpContext::for_module(1, &module);
        http_ctx.set_request(
            Arc::from("GET"),
            Arc::from("/"),
            headers,
            Arc::from("127.0.0.1"),
        );
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_root_id = module.root_id.clone();
        http_ctx.plugin_configuration = module.configuration.clone();
        http_ctx.plugin_data = module.data_files.clone();

        // 本命リクエストの `proxy_on_request_headers` を実行する。
        // モジュールは `dispatch_http_call` してから Pause を返す（F-62）。
        let configuration = module.configuration.clone();
        let mut sess = match futures::executor::block_on(engine.open_session(
            &module,
            &configuration,
            http_ctx,
            None,
        )) {
            Ok(s) => s,
            Err((e, _)) => panic!("open_session must not error: {e}"),
        };
        let action = futures::executor::block_on(engine.run_headers_module(
            &module,
            &mut sess,
            "proxy_on_request_headers",
            num_headers,
            true,
        ))
        .expect("run_headers_module must not error");
        let instance = sess.instance;
        let ctx_id = sess.context_id;
        let store = &mut sess.store;

        assert_eq!(
            FilterAction::from(action),
            FilterAction::Pause,
            "module must dispatch_http_call and return Pause"
        );
        assert!(
            !store.data().http_ctx.pending_http_calls.is_empty(),
            "a pending http call must be registered before suspending"
        );

        // B-56 再現条件: store がこの Pause 状態で「サスペンド」されている間に、
        // 無関係な他リクエストの WASM 呼び出しがエンジン全体で epoch_deadline 回以上
        // 積み上がる（実運用では offload 待ちの間に他リクエストが WASM を呼ぶことで
        // 発生する。ここでは決定的に `increment_epoch()` を直接呼んで再現する）。
        for _ in 0..engine.epoch_deadline {
            engine.registry.engine().increment_epoch();
        }

        // 実運用の resume 経路をそのまま呼ぶ（`execute_http_call_offloaded` は
        // upstream 未解決で 502 応答を返すが、`proxy_on_http_call_response` は
        // 必ず呼ばれるため resume 機構自体の検証には十分）。
        let resumed = futures::executor::block_on(
            engine.resolve_pending_http_calls_inline(&module, store, instance, ctx_id),
        );

        match resumed {
            Ok(true) => {
                // モジュールは `on_http_call_response` でローカルレスポンスを返す。
                // fail-open していないこと（＝ローカルレスポンスが設定されていること）を確認する。
                assert!(
                    store.data().http_ctx.local_response.is_some(),
                    "resume must produce the module's local response, not fail open (B-56)"
                );
            }
            Ok(false) => panic!("expected the pending http call to be resolved and resumed"),
            Err(e) => panic!(
                "resume must not trap on stale epoch deadline while suspended (B-56): {}",
                e
            ),
        }
    }
}
