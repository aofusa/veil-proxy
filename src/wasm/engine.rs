//! Filter Engine for executing WASM modules
//!
//! スレッドローカルインスタンスプールによる高性能 WASM 実行エンジン。
//! 各ワーカースレッドが専用のモジュールキャッシュを持つことで、
//! Arc デリファレンス・HashMap ルックアップのオーバーヘッドを削減する。

use std::sync::Arc;

use wasmtime::Store;

use super::context::{HostState, HttpContext};
use super::registry::{LoadedModule, ModuleRegistry};
use super::types::{FilterAction, LocalResponse, WasmConfig};

// ====================
// スレッドローカルインスタンスプール
// ====================

/// Filter engine for executing WASM modules
pub struct FilterEngine {
    /// Module registry
    registry: ModuleRegistry,
    /// Default execution time limit (fuel)
    fuel_limit: u64,
    /// Epoch deadline for timeout enforcement
    /// When epoch interruption is enabled, each store gets a deadline of
    /// current_epoch + 1, and the engine's epoch is incremented after setting up the store.
    /// This provides a simple per-execution timeout mechanism.
    epoch_deadline: u64,
}

impl FilterEngine {
    /// Create a new filter engine
    pub fn new(config: &WasmConfig) -> anyhow::Result<Self> {
        let registry = ModuleRegistry::new(config)?;

        // Calculate fuel limit (roughly 1M instructions per ms)
        let fuel_limit = config.defaults.max_execution_time_ms * 1_000_000;

        // Epoch deadline: ticks after the current engine epoch before timeout.
        // The tick thread increments the epoch every ~100ms, so deadline=10 allows ~1s.
        // Fuel consumption is the primary timeout mechanism for CPU-bound limits.
        let epoch_deadline = 10;

        Ok(Self {
            registry,
            fuel_limit,
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
        module_names: &[String],
        path: &Arc<str>,
        method: &Arc<str>,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        client_ip: &Arc<str>,
        end_of_stream: bool,
    ) -> FilterResult {
        let modules: Vec<Arc<LoadedModule>> = module_names
            .iter()
            .filter_map(|name| self.registry.get_module(name))
            .collect();

        if modules.is_empty() {
            return FilterResult::Continue {
                headers,
                body: None,
            };
        }

        let mut current_headers = headers;

        for module in &modules {
            let (returned, result) = self
                .execute_on_request_headers(
                    module,
                    path,
                    method,
                    current_headers,
                    client_ip,
                    end_of_stream,
                )
                .await;
            // ヘッダは変更有無に関わらず回収済み（ムーブスルー）。
            current_headers = returned;

            match result {
                Ok(ModuleAction::Continue) => {}
                Ok(ModuleAction::Pause) => {
                    return FilterResult::Pause;
                }
                Ok(ModuleAction::LocalResponse(resp)) => {
                    return FilterResult::LocalResponse(resp);
                }
                Err(e) => {
                    ftlog::error!("[wasm:{}] on_request_headers error: {}", module.name, e);
                    // Continue on error
                }
            }
        }

        FilterResult::Continue {
            headers: current_headers,
            body: None,
        }
    }

    /// Execute on_request_headers for specified modules ASYNCHRONOUSLY
    /// Note: runs inline in the current async task to avoid cross-thread waker issues with monoio.
    ///
    /// F-43: `module_names` は `Arc` 共有（呼び出しごとの `Vec<String>` deep copy 排除）。
    pub async fn on_request_headers_with_modules_async(
        self: Arc<Self>,
        module_names: Arc<Vec<String>>,
        path: Arc<str>,
        method: Arc<str>,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        client_ip: Arc<str>,
        end_of_stream: bool,
    ) -> FilterResult {
        self.on_request_headers_with_modules(
            &module_names,
            &path,
            &method,
            headers,
            &client_ip,
            end_of_stream,
        )
        .await
    }

    /// Proxy-Wasm SDK ライフサイクル（_start → root/HTTP コンテキスト生成 → vm_start →
    /// configure → 指定ヘッダコールバック）を実行して action を返す（ヘッダ系フィルタ共通）。
    async fn run_headers_module(
        &self,
        module: &LoadedModule,
        store: &mut Store<HostState>,
        callback_name: &str,
        num_headers: i32,
        end_of_stream: bool,
    ) -> anyhow::Result<(i32, wasmtime::Instance)> {
        store.set_fuel(self.fuel_limit)?;
        store.fuel_async_yield_interval(Some(10_000))?;
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        // Instantiate module
        let instance = module.instance_pre.instantiate_async(&mut *store).await?;

        // === Proxy-Wasm SDK Lifecycle ===
        // Step 0: Call _start to initialize the SDK
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut *store, "_start") {
            if let Err(e) = func.call_async(&mut *store, ()).await {
                ftlog::error!("[wasm:{}] _start() failed: {}", module.name, e);
            }
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut *store, "_initialize") {
            if let Err(e) = func.call_async(&mut *store, ()).await {
                ftlog::error!("[wasm:{}] _initialize() failed: {}", module.name, e);
            }
        }

        let root_context_id = 1i32; // Root context ID (SDK uses 1)
        let http_context_id = 2i32; // HTTP context ID
        let config_size = module.configuration.len() as i32;

        // Step 1: Create ROOT context first (parent=0 means root)
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut *store, "proxy_on_context_create")
        {
            let _ = func.call_async(&mut *store, (root_context_id, 0)).await;
        }

        // Step 2: Call proxy_on_vm_start
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut *store, "proxy_on_vm_start")
        {
            let _ = func
                .call_async(&mut *store, (root_context_id, config_size))
                .await;
        }

        // Step 3: Call proxy_on_configure
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut *store, "proxy_on_configure")
        {
            let _ = func
                .call_async(&mut *store, (root_context_id, config_size))
                .await;
        }

        // Step 4: Create HTTP context with root as parent
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut *store, "proxy_on_context_create")
        {
            let _ = func
                .call_async(&mut *store, (http_context_id, root_context_id))
                .await;
        }

        // 指定されたヘッダコールバックを呼ぶ
        let callback = instance.get_typed_func::<(i32, i32, i32), i32>(&mut *store, callback_name);

        let action = match callback {
            Ok(func) => {
                let eos = if end_of_stream { 1 } else { 0 };
                func.call_async(&mut *store, (http_context_id, num_headers, eos))
                    .await?
            }
            Err(_) => 0, // Callback not exported, continue
        };

        // F-48: fuel 消費量を記録（ライフサイクル + コールバック 1 実行分）。
        let consumed = self
            .fuel_limit
            .saturating_sub(store.get_fuel().unwrap_or(0));
        let phase = if callback_name == "proxy_on_request_headers" {
            "request_headers"
        } else {
            "response_headers"
        };
        crate::metrics::observe_wasm_fuel_consumed(&module.name, phase, consumed);

        // F-62: Pause/resume（proxy_on_http_call_response）で同一インスタンスを
        // 再度呼び出せるようインスタンスも返す。
        Ok((action, instance))
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
    ) -> anyhow::Result<bool> {
        let http_context_id = 2i32;
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
        path: &Arc<str>,
        method: &Arc<str>,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        client_ip: &Arc<str>,
        end_of_stream: bool,
    ) -> (Vec<(Vec<u8>, Vec<u8>)>, anyhow::Result<ModuleAction>) {
        let num_headers = headers.len() as i32;

        // Create context（文字列は Arc 共有、ヘッダはムーブ）
        let mut http_ctx = HttpContext::new(1, module.capabilities.clone());
        http_ctx.set_request(method.clone(), path.clone(), headers, client_ip.clone());
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_configuration = module.configuration.clone();

        // Create store with fuel limit
        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);

        let run = self
            .run_headers_module(
                module,
                &mut store,
                "proxy_on_request_headers",
                num_headers,
                end_of_stream,
            )
            .await;

        let result = match run {
            Ok((action, instance)) => {
                // F-62: Pause かつ pending HTTP コールあり → インラインで解決して resume
                let action = match FilterAction::from(action) {
                    FilterAction::Pause => {
                        match self
                            .resolve_pending_http_calls_inline(module, &mut store, instance)
                            .await
                        {
                            Ok(true) => FilterAction::Continue,
                            Ok(false) => FilterAction::Pause,
                            Err(e) => {
                                ftlog::error!(
                                    "[wasm:{}] http_call resume error: {}",
                                    module.name,
                                    e
                                );
                                FilterAction::Continue
                            }
                        }
                    }
                    other => other,
                };
                let state = store.data();
                if let Some(local_response) = &state.http_ctx.local_response {
                    Ok(ModuleAction::LocalResponse(local_response.clone()))
                } else {
                    match action {
                        FilterAction::Continue => Ok(ModuleAction::Continue),
                        FilterAction::Pause => Ok(ModuleAction::Pause),
                    }
                }
            }
            Err(e) => Err(e),
        };

        // ヘッダをコンテキストから回収（ゼロコピー・ムーブ。resume 中の変更も反映される）
        let headers = std::mem::take(&mut store.data_mut().http_ctx.request_headers);
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
        module_names: &[String],
        status: u16,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        end_of_stream: bool,
    ) -> FilterResult {
        let modules: Vec<Arc<LoadedModule>> = module_names
            .iter()
            .filter_map(|name| self.registry.get_module(name))
            .collect();

        if modules.is_empty() {
            return FilterResult::Continue {
                headers,
                body: None,
            };
        }

        let mut current_headers = headers;

        // Execute in reverse order for response
        for module in modules.iter().rev() {
            // F-09: WASM フィルタ実行時間を計測
            let _wasm_start = std::time::Instant::now();
            let (returned, result) = self
                .execute_on_response_headers(module, status, current_headers, end_of_stream)
                .await;
            current_headers = returned;
            crate::metrics::observe_wasm_filter_duration(
                &module.name,
                "response_headers",
                _wasm_start.elapsed().as_secs_f64(),
            );

            match result {
                Ok(ModuleAction::Continue) => {}
                Ok(ModuleAction::Pause) => {
                    return FilterResult::Pause;
                }
                Ok(ModuleAction::LocalResponse(resp)) => {
                    return FilterResult::LocalResponse(resp);
                }
                Err(e) => {
                    ftlog::error!("[wasm:{}] on_response_headers error: {}", module.name, e);
                }
            }
        }

        FilterResult::Continue {
            headers: current_headers,
            body: None,
        }
    }

    /// Execute on_response_headers for specified modules ASYNCHRONOUSLY
    /// Note: runs inline in the current async task to avoid cross-thread waker issues with monoio.
    ///
    /// F-43: `module_names` は `Arc` 共有（呼び出しごとの `Vec<String>` deep copy 排除）。
    pub async fn on_response_headers_with_modules_async(
        self: Arc<Self>,
        module_names: Arc<Vec<String>>,
        status: u16,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        end_of_stream: bool,
    ) -> FilterResult {
        self.on_response_headers_with_modules(&module_names, status, headers, end_of_stream)
            .await
    }

    /// Execute on_response_headers for a single module
    ///
    /// F-43: ヘッダは所有権ムーブスルー（`execute_on_request_headers` と同方針）。
    async fn execute_on_response_headers(
        &self,
        module: &LoadedModule,
        status: u16,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        end_of_stream: bool,
    ) -> (Vec<(Vec<u8>, Vec<u8>)>, anyhow::Result<ModuleAction>) {
        let num_headers = headers.len() as i32;

        // Create context（ヘッダはムーブ）
        let mut http_ctx = HttpContext::new(1, module.capabilities.clone());
        http_ctx.set_response(status, headers);
        http_ctx.plugin_name = module.name.clone();

        // Create store with fuel limit
        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);

        let run = self
            .run_headers_module(
                module,
                &mut store,
                "proxy_on_response_headers",
                num_headers,
                end_of_stream,
            )
            .await;

        let result = match run {
            Ok((action, instance)) => {
                // F-62: Pause かつ pending HTTP コールあり → インラインで解決して resume
                let action = match FilterAction::from(action) {
                    FilterAction::Pause => {
                        match self
                            .resolve_pending_http_calls_inline(module, &mut store, instance)
                            .await
                        {
                            Ok(true) => FilterAction::Continue,
                            Ok(false) => FilterAction::Pause,
                            Err(e) => {
                                ftlog::error!(
                                    "[wasm:{}] http_call resume error: {}",
                                    module.name,
                                    e
                                );
                                FilterAction::Continue
                            }
                        }
                    }
                    other => other,
                };
                let state = store.data();
                if let Some(local_response) = &state.http_ctx.local_response {
                    Ok(ModuleAction::LocalResponse(local_response.clone()))
                } else {
                    match action {
                        FilterAction::Continue => Ok(ModuleAction::Continue),
                        FilterAction::Pause => Ok(ModuleAction::Pause),
                    }
                }
            }
            Err(e) => Err(e),
        };

        // ヘッダをコンテキストから回収（ゼロコピー・ムーブ。resume 中の変更も反映される）
        let headers = std::mem::take(&mut store.data_mut().http_ctx.response_headers);
        (headers, result)
    }

    /// Get a loaded module by name
    pub fn get_module(&self, name: &str) -> Option<Arc<LoadedModule>> {
        self.registry.get_module(name)
    }

    /// Execute proxy_on_http_call_response callback for a module
    ///
    /// This should be called after an HTTP call completes to deliver the response
    /// back to the WASM module.
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
                ModuleResult::Pause => FilterResult::Pause,
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
        let mut http_ctx = HttpContext::new(1, module.capabilities.clone());
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_configuration = module.configuration.clone();

        // Store the response in context
        http_ctx.http_call_responses.insert(token, response.clone());
        http_ctx.current_http_call_token = Some(token);

        // Create store with fuel limit
        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);
        store.set_fuel(self.fuel_limit)?;
        store.fuel_async_yield_interval(Some(10_000))?;
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        // Instantiate module
        let instance = module.instance_pre.instantiate_async(&mut store).await?;

        // === Proxy-Wasm SDK Lifecycle ===
        // Step 0: Call _start
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call_async(&mut store, ()).await;
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call_async(&mut store, ()).await;
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        // Step 1: Create ROOT context
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call_async(&mut store, (root_context_id, 0)).await;
        }

        // Step 2: Call proxy_on_vm_start
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

        // Step 3: Call proxy_on_configure
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

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

                if let Err(e) = func.call(
                    &mut store,
                    (
                        http_context_id,
                        token as i32,
                        num_headers,
                        body_size,
                        num_trailers,
                    ),
                ) {
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
        module_names: &[String],
        body: bytes::Bytes,
        end_of_stream: bool,
    ) -> BodyFilterResult {
        let modules: Vec<Arc<LoadedModule>> = module_names
            .iter()
            .filter_map(|name| self.registry.get_module(name))
            .collect();

        if modules.is_empty() {
            // F-61: モジュール未登録時はゼロコピーでそのまま返す
            return BodyFilterResult::Continue { body };
        }

        // F-61: `Bytes` の参照カウント共有でフィルタチェーンを回す（deep copy なし）
        let mut current_body = body;

        for module in &modules {
            let result = self
                .execute_on_request_body(module, current_body.clone(), end_of_stream)
                .await;

            match result {
                Ok(BodyModuleResult::Continue { modified_body }) => {
                    if let Some(b) = modified_body {
                        current_body = b;
                    }
                }
                Ok(BodyModuleResult::Pause) => {
                    return BodyFilterResult::Pause;
                }
                Ok(BodyModuleResult::LocalResponse(resp)) => {
                    return BodyFilterResult::LocalResponse(resp);
                }
                Err(e) => {
                    ftlog::error!("[wasm:{}] on_request_body error: {}", module.name, e);
                    // Continue on error
                }
            }
        }

        BodyFilterResult::Continue { body: current_body }
    }

    /// Execute on_request_body for specified modules ASYNCHRONOUSLY
    /// Note: runs inline in the current async task to avoid cross-thread waker issues with monoio.
    pub async fn on_request_body_with_modules_async(
        self: Arc<Self>,
        module_names: Vec<String>,
        body: bytes::Bytes,
        end_of_stream: bool,
    ) -> BodyFilterResult {
        self.on_request_body_with_modules(&module_names, body, end_of_stream)
            .await
    }

    /// Execute on_request_body for a single module
    async fn execute_on_request_body(
        &self,
        module: &LoadedModule,
        body: bytes::Bytes,
        end_of_stream: bool,
    ) -> anyhow::Result<BodyModuleResult> {
        // Check capability
        if !module.capabilities.allow_request_body_read {
            return Ok(BodyModuleResult::Continue {
                modified_body: None,
            });
        }

        // Create context
        let mut http_ctx = HttpContext::new(1, module.capabilities.clone());
        let body_len = body.len();
        http_ctx.set_request_body(body, end_of_stream);
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_configuration = module.configuration.clone();

        // Create store with fuel limit
        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);
        store.set_fuel(self.fuel_limit)?;
        store.fuel_async_yield_interval(Some(10_000))?;
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        // Instantiate module
        let instance = module.instance_pre.instantiate_async(&mut store).await?;

        // Proxy-Wasm SDK Lifecycle
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call_async(&mut store, ()).await;
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call_async(&mut store, ()).await;
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call_async(&mut store, (root_context_id, 0)).await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func
                .call_async(&mut store, (http_context_id, root_context_id))
                .await;
        }

        // Call proxy_on_request_body
        // Signature: (context_id, body_size, end_of_stream) -> action
        let callback =
            instance.get_typed_func::<(i32, i32, i32), i32>(&mut store, "proxy_on_request_body");

        let action = match callback {
            Ok(func) => {
                let body_size = body_len as i32;
                let eos = if end_of_stream { 1 } else { 0 };
                func.call_async(&mut store, (http_context_id, body_size, eos))
                    .await?
            }
            Err(_) => 0, // Continue if not exported
        };

        // Check for local response
        let state = store.data();
        if let Some(local_response) = &state.http_ctx.local_response {
            return Ok(BodyModuleResult::LocalResponse(local_response.clone()));
        }

        // Check for modifications
        // F-61: 変更時は store からムーブで取り出す（clone による deep copy を排除）
        let body_modified = state.http_ctx.request_body_modified;
        let modified_body = if body_modified {
            let taken = std::mem::take(&mut store.data_mut().http_ctx.request_body);
            Some(taken.into_bytes())
        } else {
            None
        };

        match FilterAction::from(action) {
            FilterAction::Continue => Ok(BodyModuleResult::Continue { modified_body }),
            FilterAction::Pause => Ok(BodyModuleResult::Pause),
        }
    }

    /// Execute on_response_body callback for specified modules
    ///
    /// Processes response body chunks through WASM modules (in reverse order).
    /// Returns potentially modified body data.
    pub async fn on_response_body_with_modules(
        &self,
        module_names: &[String],
        body: bytes::Bytes,
        end_of_stream: bool,
    ) -> BodyFilterResult {
        let modules: Vec<Arc<LoadedModule>> = module_names
            .iter()
            .filter_map(|name| self.registry.get_module(name))
            .collect();

        if modules.is_empty() {
            // F-61: モジュール未登録時はゼロコピーでそのまま返す
            return BodyFilterResult::Continue { body };
        }

        // F-61: `Bytes` の参照カウント共有でフィルタチェーンを回す（deep copy なし）
        let mut current_body = body;

        // Execute in reverse order for response
        for module in modules.iter().rev() {
            let result = self
                .execute_on_response_body(module, current_body.clone(), end_of_stream)
                .await;

            match result {
                Ok(BodyModuleResult::Continue { modified_body }) => {
                    if let Some(b) = modified_body {
                        current_body = b;
                    }
                }
                Ok(BodyModuleResult::Pause) => {
                    return BodyFilterResult::Pause;
                }
                Ok(BodyModuleResult::LocalResponse(resp)) => {
                    return BodyFilterResult::LocalResponse(resp);
                }
                Err(e) => {
                    ftlog::error!("[wasm:{}] on_response_body error: {}", module.name, e);
                }
            }
        }

        BodyFilterResult::Continue { body: current_body }
    }

    /// Execute on_response_body for specified modules ASYNCHRONOUSLY
    /// Note: runs inline in the current async task to avoid cross-thread waker issues with monoio.
    pub async fn on_response_body_with_modules_async(
        self: Arc<Self>,
        module_names: Vec<String>,
        body: bytes::Bytes,
        end_of_stream: bool,
    ) -> BodyFilterResult {
        self.on_response_body_with_modules(&module_names, body, end_of_stream)
            .await
    }

    /// Execute on_response_body for a single module
    async fn execute_on_response_body(
        &self,
        module: &LoadedModule,
        body: bytes::Bytes,
        end_of_stream: bool,
    ) -> anyhow::Result<BodyModuleResult> {
        // Check capability
        if !module.capabilities.allow_response_body_read {
            return Ok(BodyModuleResult::Continue {
                modified_body: None,
            });
        }

        // Create context
        let mut http_ctx = HttpContext::new(1, module.capabilities.clone());
        let body_len = body.len();
        http_ctx.set_response_body(body, end_of_stream);
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_configuration = module.configuration.clone();

        // Create store with fuel limit
        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);
        store.set_fuel(self.fuel_limit)?;
        store.fuel_async_yield_interval(Some(10_000))?;
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        // Instantiate module
        let instance = module.instance_pre.instantiate_async(&mut store).await?;

        // Proxy-Wasm SDK Lifecycle
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call_async(&mut store, ()).await;
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call_async(&mut store, ()).await;
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call_async(&mut store, (root_context_id, 0)).await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func
                .call_async(&mut store, (http_context_id, root_context_id))
                .await;
        }

        // Call proxy_on_response_body
        // Signature: (context_id, body_size, end_of_stream) -> action
        let callback =
            instance.get_typed_func::<(i32, i32, i32), i32>(&mut store, "proxy_on_response_body");

        let action = match callback {
            Ok(func) => {
                let body_size = body_len as i32;
                let eos = if end_of_stream { 1 } else { 0 };
                func.call_async(&mut store, (http_context_id, body_size, eos))
                    .await?
            }
            Err(_) => 0, // Continue if not exported
        };

        // Check for local response
        let state = store.data();
        if let Some(local_response) = &state.http_ctx.local_response {
            return Ok(BodyModuleResult::LocalResponse(local_response.clone()));
        }

        // Check for modifications
        // F-61: 変更時は store からムーブで取り出す（clone による deep copy を排除）
        let body_modified = state.http_ctx.response_body_modified;
        let modified_body = if body_modified {
            let taken = std::mem::take(&mut store.data_mut().http_ctx.response_body);
            Some(taken.into_bytes())
        } else {
            None
        };

        match FilterAction::from(action) {
            FilterAction::Continue => Ok(BodyModuleResult::Continue { modified_body }),
            FilterAction::Pause => Ok(BodyModuleResult::Pause),
        }
    }

    /// Execute on_log callback for specified modules
    ///
    /// Called at the end of HTTP request processing (log phase).
    /// This is the final callback before the stream is closed.
    pub async fn on_log_with_modules(&self, module_names: &[String]) {
        let modules: Vec<Arc<LoadedModule>> = module_names
            .iter()
            .filter_map(|name| self.registry.get_module(name))
            .collect();

        for module in &modules {
            if let Err(e) = self.execute_on_log(module).await {
                ftlog::error!("[wasm:{}] on_log error: {}", module.name, e);
            }
        }
    }

    /// Execute on_log for specified modules ASYNCHRONOUSLY
    /// Note: runs inline in the current async task to avoid cross-thread waker issues with monoio.
    pub async fn on_log_with_modules_async(self: Arc<Self>, module_names: Arc<Vec<String>>) {
        self.on_log_with_modules(&module_names).await;
    }

    /// Execute on_log for a single module
    async fn execute_on_log(&self, module: &LoadedModule) -> anyhow::Result<()> {
        // Create context
        let http_ctx = HttpContext::new(1, module.capabilities.clone());
        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);
        store.set_fuel(self.fuel_limit)?;
        store.fuel_async_yield_interval(Some(10_000))?;
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        // Instantiate module
        let instance = module.instance_pre.instantiate_async(&mut store).await?;

        // Initialize module
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call_async(&mut store, ()).await;
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call_async(&mut store, ()).await;
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        // Create contexts
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call_async(&mut store, (root_context_id, 0)).await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func
                .call_async(&mut store, (http_context_id, root_context_id))
                .await;
        }

        // Call proxy_on_log
        // Signature: (context_id) -> void
        if let Ok(func) = instance.get_typed_func::<i32, ()>(&mut store, "proxy_on_log") {
            match func.call_async(&mut store, http_context_id).await {
                Ok(()) => {
                    ftlog::debug!(
                        "[wasm:{}] proxy_on_log({}) OK",
                        module.name,
                        http_context_id
                    )
                }
                Err(e) => ftlog::debug!("[wasm:{}] proxy_on_log error: {}", module.name, e),
            }
        }

        Ok(())
    }

    /// Execute on_done callback for specified modules
    ///
    /// Called when an HTTP context is being deleted.
    /// Returns true if the module wants to keep the context alive (async operation pending).
    pub async fn on_done_with_modules(&self, module_names: &[String]) -> bool {
        let modules: Vec<Arc<LoadedModule>> = module_names
            .iter()
            .filter_map(|name| self.registry.get_module(name))
            .collect();

        let mut any_pending = false;

        for module in &modules {
            match self.execute_on_done(module).await {
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
    async fn execute_on_done(&self, module: &LoadedModule) -> anyhow::Result<bool> {
        // Create context
        let http_ctx = HttpContext::new(1, module.capabilities.clone());
        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);
        store.set_fuel(self.fuel_limit)?;
        store.fuel_async_yield_interval(Some(10_000))?;
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        // Instantiate module
        let instance = module.instance_pre.instantiate_async(&mut store).await?;

        // Initialize module
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call_async(&mut store, ()).await;
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call_async(&mut store, ()).await;
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        // Create contexts
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call_async(&mut store, (root_context_id, 0)).await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

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
        let http_ctx = HttpContext::new(1, module.capabilities.clone());
        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);
        store.set_fuel(self.fuel_limit)?;
        store.fuel_async_yield_interval(Some(10_000))?;
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        // Instantiate module
        let instance = module.instance_pre.instantiate_async(&mut store).await?;

        // Initialize module
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call_async(&mut store, ()).await;
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call_async(&mut store, ()).await;
        }

        let root_context_id = 1i32;
        let config_size = module.configuration.len() as i32;

        // Create root context only (tick is called on root context)
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call_async(&mut store, (root_context_id, 0)).await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

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
        module_names: &[String],
        trailers: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> FilterResult {
        let modules: Vec<Arc<LoadedModule>> = module_names
            .iter()
            .filter_map(|name| self.registry.get_module(name))
            .collect();

        if modules.is_empty() {
            return FilterResult::Continue {
                headers: trailers,
                body: None,
            };
        }

        let mut current_trailers = trailers;

        for module in &modules {
            let result = self
                .execute_on_request_trailers(module, &current_trailers)
                .await;

            match result {
                Ok(ModuleResult::Continue { modified_headers }) => {
                    if let Some(h) = modified_headers {
                        current_trailers = h;
                    }
                }
                Ok(ModuleResult::Pause) => {
                    return FilterResult::Pause;
                }
                Ok(ModuleResult::LocalResponse(resp)) => {
                    return FilterResult::LocalResponse(resp);
                }
                Err(e) => {
                    ftlog::error!("[wasm:{}] on_request_trailers error: {}", module.name, e);
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
        trailers: &[(Vec<u8>, Vec<u8>)],
    ) -> anyhow::Result<ModuleResult> {
        // Create context
        let mut http_ctx = HttpContext::new(1, module.capabilities.clone());
        http_ctx.request_trailers = trailers.to_vec();
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_configuration = module.configuration.clone();

        // Create store with fuel limit
        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);
        store.set_fuel(self.fuel_limit)?;
        store.fuel_async_yield_interval(Some(10_000))?;
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        // Instantiate module
        let instance = module.instance_pre.instantiate_async(&mut store).await?;

        // Initialize
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call_async(&mut store, ()).await;
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call_async(&mut store, ()).await;
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call_async(&mut store, (root_context_id, 0)).await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func
                .call_async(&mut store, (http_context_id, root_context_id))
                .await;
        }

        // Call proxy_on_request_trailers
        // Signature: (context_id, num_trailers) -> action
        let callback =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_request_trailers");

        let action = match callback {
            Ok(func) => {
                let num_trailers = trailers.len() as i32;
                func.call_async(&mut store, (http_context_id, num_trailers))
                    .await?
            }
            Err(_) => 0,
        };

        // Check for local response
        let state = store.data();
        if let Some(local_response) = &state.http_ctx.local_response {
            return Ok(ModuleResult::LocalResponse(local_response.clone()));
        }

        // Check for modifications
        let modified_headers = if state.http_ctx.request_headers_modified {
            Some(state.http_ctx.request_trailers.clone())
        } else {
            None
        };

        match FilterAction::from(action) {
            FilterAction::Continue => Ok(ModuleResult::Continue { modified_headers }),
            FilterAction::Pause => Ok(ModuleResult::Pause),
        }
    }

    /// Execute on_response_trailers callback for specified modules
    ///
    /// Called when response trailers are received (HTTP/2, gRPC).
    pub async fn on_response_trailers_with_modules(
        &self,
        module_names: &[String],
        trailers: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> FilterResult {
        let modules: Vec<Arc<LoadedModule>> = module_names
            .iter()
            .filter_map(|name| self.registry.get_module(name))
            .collect();

        if modules.is_empty() {
            return FilterResult::Continue {
                headers: trailers,
                body: None,
            };
        }

        let mut current_trailers = trailers;

        // Execute in reverse order for response
        for module in modules.iter().rev() {
            let result = self
                .execute_on_response_trailers(module, &current_trailers)
                .await;

            match result {
                Ok(ModuleResult::Continue { modified_headers }) => {
                    if let Some(h) = modified_headers {
                        current_trailers = h;
                    }
                }
                Ok(ModuleResult::Pause) => {
                    return FilterResult::Pause;
                }
                Ok(ModuleResult::LocalResponse(resp)) => {
                    return FilterResult::LocalResponse(resp);
                }
                Err(e) => {
                    ftlog::error!("[wasm:{}] on_response_trailers error: {}", module.name, e);
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
        trailers: &[(Vec<u8>, Vec<u8>)],
    ) -> anyhow::Result<ModuleResult> {
        // Create context
        let mut http_ctx = HttpContext::new(1, module.capabilities.clone());
        http_ctx.response_trailers = trailers.to_vec();
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_configuration = module.configuration.clone();

        // Create store with fuel limit
        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);
        store.set_fuel(self.fuel_limit)?;
        store.fuel_async_yield_interval(Some(10_000))?;
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        // Instantiate module
        let instance = module.instance_pre.instantiate_async(&mut store).await?;

        // Initialize
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call_async(&mut store, ()).await;
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call_async(&mut store, ()).await;
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call_async(&mut store, (root_context_id, 0)).await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func
                .call_async(&mut store, (http_context_id, root_context_id))
                .await;
        }

        // Call proxy_on_response_trailers
        // Signature: (context_id, num_trailers) -> action
        let callback =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_response_trailers");

        let action = match callback {
            Ok(func) => {
                let num_trailers = trailers.len() as i32;
                func.call_async(&mut store, (http_context_id, num_trailers))
                    .await?
            }
            Err(_) => 0,
        };

        // Check for local response
        let state = store.data();
        if let Some(local_response) = &state.http_ctx.local_response {
            return Ok(ModuleResult::LocalResponse(local_response.clone()));
        }

        // Check for modifications
        let modified_headers = if state.http_ctx.response_headers_modified {
            Some(state.http_ctx.response_trailers.clone())
        } else {
            None
        };

        match FilterAction::from(action) {
            FilterAction::Continue => Ok(ModuleResult::Continue { modified_headers }),
            FilterAction::Pause => Ok(ModuleResult::Pause),
        }
    }

    /// Execute on_queue_ready callback for a module
    ///
    /// Called when a message is enqueued to a shared queue that the module is subscribed to.
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
        let http_ctx = HttpContext::new(1, module.capabilities.clone());
        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);
        store.set_fuel(self.fuel_limit)?;
        store.fuel_async_yield_interval(Some(10_000))?;
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        // Instantiate module
        let instance = module.instance_pre.instantiate_async(&mut store).await?;

        // Initialize module
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call_async(&mut store, ()).await;
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call_async(&mut store, ()).await;
        }

        let root_context_id = 1i32;
        let config_size = module.configuration.len() as i32;

        // Create root context (queue_ready is called on root context)
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call_async(&mut store, (root_context_id, 0)).await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

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
    #[cfg(feature = "grpc")]
    pub async fn on_grpc_receive_initial_metadata(
        &self,
        module_name: &str,
        call_id: u32,
        headers: &[(String, String)],
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
        headers: &[(String, String)],
    ) -> anyhow::Result<()> {
        // Create context
        let mut http_ctx = HttpContext::new(1, module.capabilities.clone());
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_configuration = module.configuration.clone();

        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);
        store.set_fuel(self.fuel_limit)?;
        store.fuel_async_yield_interval(Some(10_000))?;
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        let instance = module.instance_pre.instantiate_async(&mut store).await?;

        // Initialize
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call_async(&mut store, ()).await;
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call_async(&mut store, ()).await;
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call_async(&mut store, (root_context_id, 0)).await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

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
        let mut http_ctx = HttpContext::new(1, module.capabilities.clone());
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_configuration = module.configuration.clone();

        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);
        store.set_fuel(self.fuel_limit)?;
        store.fuel_async_yield_interval(Some(10_000))?;
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        let instance = module.instance_pre.instantiate_async(&mut store).await?;

        // Initialize
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call_async(&mut store, ()).await;
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call_async(&mut store, ()).await;
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call_async(&mut store, (root_context_id, 0)).await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

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
    #[cfg(feature = "grpc")]
    pub async fn on_grpc_receive_trailing_metadata(
        &self,
        module_name: &str,
        call_id: u32,
        trailers: &[(String, String)],
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
        trailers: &[(String, String)],
    ) -> anyhow::Result<()> {
        // Create context
        let mut http_ctx = HttpContext::new(1, module.capabilities.clone());
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_configuration = module.configuration.clone();

        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);
        store.set_fuel(self.fuel_limit)?;
        store.fuel_async_yield_interval(Some(10_000))?;
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        let instance = module.instance_pre.instantiate_async(&mut store).await?;

        // Initialize
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call_async(&mut store, ()).await;
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call_async(&mut store, ()).await;
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call_async(&mut store, (root_context_id, 0)).await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

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
        let mut http_ctx = HttpContext::new(1, module.capabilities.clone());
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_configuration = module.configuration.clone();

        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);
        store.set_fuel(self.fuel_limit)?;
        store.fuel_async_yield_interval(Some(10_000))?;
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        let instance = module.instance_pre.instantiate_async(&mut store).await?;

        // Initialize
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call_async(&mut store, ()).await;
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call_async(&mut store, ()).await;
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call_async(&mut store, (root_context_id, 0)).await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func
                .call_async(&mut store, (root_context_id, config_size))
                .await;
        }

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
enum ModuleAction {
    Continue,
    Pause,
    LocalResponse(LocalResponse),
}

/// Result from filter chain execution
pub enum FilterResult {
    /// Continue with potentially modified headers/body
    Continue {
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        body: Option<Vec<u8>>,
    },
    /// Pause processing (async operation pending)
    Pause,
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
    /// Pause processing (async operation pending)
    Pause,
    /// Send a local response instead of proxying
    LocalResponse(LocalResponse),
}

#[cfg(test)]
mod exec_smoke_tests {
    use super::*;
    use crate::wasm::types::{ModuleConfig, WasmConfig, WasmDefaults};

    fn header_filter_engine() -> Option<FilterEngine> {
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
                configuration: String::new(),
                capabilities: Default::default(),
            }],
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
            &["header_filter".to_string()],
            &Arc::from("/"),
            &Arc::from("GET"),
            headers,
            &Arc::from("127.0.0.1"),
            true,
        ));
        // 中身は問わない。パニックせず FilterResult を返すことだけを確認する。
        match result {
            FilterResult::Continue { .. }
            | FilterResult::Pause
            | FilterResult::LocalResponse(_) => {}
        }
    }
}
