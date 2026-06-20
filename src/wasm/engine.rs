//! Filter Engine for executing WASM modules
//!
//! Manages the execution of Proxy-Wasm filter chains.

use std::sync::Arc;

use wasmtime::Store;

use super::context::{HostState, HttpContext};
use super::registry::{LoadedModule, ModuleRegistry};
use super::types::{FilterAction, LocalResponse, WasmConfig};

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

    /// Execute on_request_headers callback for all modules
    /// 
    /// Note: This method is deprecated. Use `on_request_headers_with_modules` instead.
    /// This method always returns Continue without applying any modules.
    pub fn on_request_headers(
        &self,
        _path: &str,
        _method: &str,
        headers: &[(String, String)],
        _client_ip: &str,
        _end_of_stream: bool,
    ) -> FilterResult {
        // ルートレベルのmodulesフィールドを使用するため、このメソッドは使用しない
        FilterResult::Continue {
            headers: headers.to_vec(),
            body: None,
        }
    }

    /// Execute on_request_headers for specified modules (internal helper)
    #[allow(dead_code)]
    fn execute_on_request_headers_for_modules(
        &self,
        modules: &[Arc<LoadedModule>],
        path: &str,
        method: &str,
        headers: &[(String, String)],
        client_ip: &str,
        end_of_stream: bool,
    ) -> FilterResult {
        if modules.is_empty() {
            return FilterResult::Continue {
                headers: headers.to_vec(),
                body: None,
            };
        }

        let mut current_headers = headers.to_vec();

        for module in modules {
            let result = self.execute_on_request_headers(
                module,
                path,
                method,
                &current_headers,
                client_ip,
                end_of_stream,
            );

            match result {
                Ok(ModuleResult::Continue { modified_headers }) => {
                    if let Some(h) = modified_headers {
                        current_headers = h;
                    }
                }
                Ok(ModuleResult::Pause) => {
                    return FilterResult::Pause;
                }
                Ok(ModuleResult::LocalResponse(resp)) => {
                    return FilterResult::LocalResponse(resp);
                }
                Err(e) => {
                    ftlog::error!(
                        "[wasm:{}] on_request_headers error: {}",
                        module.name,
                        e
                    );
                    // Continue on error
                }
            }
        }

        FilterResult::Continue {
            headers: current_headers,
            body: None,
        }
    }

    /// Execute on_request_headers for specified modules
    pub fn on_request_headers_with_modules(
        &self,
        module_names: &[String],
        path: &str,
        method: &str,
        headers: &[(String, String)],
        client_ip: &str,
        end_of_stream: bool,
    ) -> FilterResult {
        // 同期実行（互換性のため維持、ただしメインループからは非推奨）
        let modules: Vec<Arc<LoadedModule>> = module_names
            .iter()
            .filter_map(|name| self.registry.get_module(name))
            .collect();
        
        if modules.is_empty() {
            return FilterResult::Continue {
                headers: headers.to_vec(),
                body: None,
            };
        }
        
        let mut current_headers = headers.to_vec();
        
        for module in &modules {
            let result = self.execute_on_request_headers(
                module,
                path,
                method,
                &current_headers,
                client_ip,
                end_of_stream,
            );
            
            match result {
                Ok(ModuleResult::Continue { modified_headers }) => {
                    if let Some(h) = modified_headers {
                        current_headers = h;
                    }
                }
                Ok(ModuleResult::Pause) => {
                    return FilterResult::Pause;
                }
                Ok(ModuleResult::LocalResponse(resp)) => {
                    return FilterResult::LocalResponse(resp);
                }
                Err(e) => {
                    ftlog::error!(
                        "[wasm:{}] on_request_headers error: {}",
                        module.name,
                        e
                    );
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
    /// 
    /// This method executes the WASM modules synchronously.
    /// Note: runs inline in the current async task to avoid cross-thread waker issues with monoio.
    pub async fn on_request_headers_with_modules_async(
        self: Arc<Self>,
        module_names: Vec<String>,
        path: String,
        method: String,
        headers: Vec<(String, String)>,
        client_ip: String,
        end_of_stream: bool,
    ) -> FilterResult {
        self.on_request_headers_with_modules(
            &module_names,
            &path,
            &method,
            &headers,
            &client_ip,
            end_of_stream,
        )
    }

    /// Execute on_request_headers for a single module
    fn execute_on_request_headers(
        &self,
        module: &LoadedModule,
        path: &str,
        method: &str,
        headers: &[(String, String)],
        client_ip: &str,
        end_of_stream: bool,
    ) -> anyhow::Result<ModuleResult> {
        // Create context
        let mut http_ctx = HttpContext::new(1, module.capabilities.clone());
        http_ctx.set_request(method, path, headers.to_vec(), client_ip);
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_configuration = module.configuration.clone();

        // Create store with fuel limit
        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);
        store.set_fuel(self.fuel_limit)?;
        store.set_epoch_deadline(self.epoch_deadline);

        // Instantiate module
        let instance = module.instance_pre.instantiate(&mut store)?;

        // === Proxy-Wasm SDK Lifecycle ===
        // The SDK requires these callbacks in EXACT order:
        // 0. _start - MUST be called first! This runs set_root_context() in the SDK
        // 1. proxy_on_context_create(root_id, 0) - creates ROOT context (parent=0 means root)
        // 2. proxy_on_vm_start(root_id, config_size) - notifies VM started
        // 3. proxy_on_configure(root_id, config_size) - sends configuration
        // 4. proxy_on_context_create(http_id, root_id) - creates HTTP context under root
        // 5. proxy_on_request_headers - processes the request

        // Step 0: Call _start to initialize the SDK
        // This is where set_root_context() is called in the proxy-wasm Rust SDK
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            match func.call(&mut store, ()) {
                Ok(()) => ftlog::debug!("[wasm:{}] _start() OK", module.name),
                Err(e) => ftlog::error!("[wasm:{}] _start() failed: {}", module.name, e),
            }
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            match func.call(&mut store, ()) {
                Ok(()) => ftlog::debug!("[wasm:{}] _initialize() OK", module.name),
                Err(e) => ftlog::error!("[wasm:{}] _initialize() failed: {}", module.name, e),
            }
        } else {
            ftlog::warn!("[wasm:{}] Neither _start nor _initialize exported", module.name);
        }

        let root_context_id = 1i32;    // Root context ID (SDK uses 1)
        let http_context_id = 2i32;    // HTTP context ID
        let config_size = module.configuration.len() as i32;

        // Step 1: Create ROOT context first (parent_context_id = 0 means root)
        // This MUST be called BEFORE proxy_on_vm_start
        // Note: proxy_on_context_create has signature (i32, i32) -> void
        if let Ok(func) = instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create") {
            match func.call(&mut store, (root_context_id, 0)) {
                Ok(()) => ftlog::debug!("[wasm:{}] proxy_on_context_create({}, 0) OK", module.name, root_context_id),
                Err(e) => ftlog::error!("[wasm:{}] proxy_on_context_create({}, 0) failed: {}", module.name, root_context_id, e),
            }
        } else {
            ftlog::debug!("[wasm:{}] proxy_on_context_create not exported", module.name);
        }

        // Step 2: Call proxy_on_vm_start on the root context
        if let Ok(func) = instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start") {
            match func.call(&mut store, (root_context_id, config_size)) {
                Ok(ret) => ftlog::debug!("[wasm:{}] proxy_on_vm_start({}, {}) => {}", module.name, root_context_id, config_size, ret),
                Err(e) => ftlog::error!("[wasm:{}] proxy_on_vm_start failed: {}", module.name, e),
            }
        }

        // Step 3: Call proxy_on_configure on the root context
        if let Ok(func) = instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure") {
            match func.call(&mut store, (root_context_id, config_size)) {
                Ok(ret) => ftlog::debug!("[wasm:{}] proxy_on_configure({}, {}) => {}", module.name, root_context_id, config_size, ret),
                Err(e) => ftlog::error!("[wasm:{}] proxy_on_configure failed: {}", module.name, e),
            }
        }

        // Step 4: Create HTTP context with root as parent
        // Note: proxy_on_context_create has signature (i32, i32) -> void
        if let Ok(func) = instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create") {
            match func.call(&mut store, (http_context_id, root_context_id)) {
                Ok(()) => ftlog::debug!("[wasm:{}] proxy_on_context_create({}, {}) OK", module.name, http_context_id, root_context_id),
                Err(e) => ftlog::error!("[wasm:{}] proxy_on_context_create({}, {}) failed: {}", module.name, http_context_id, root_context_id, e),
            }
        }

        // Now call proxy_on_request_headers
        let callback = instance
            .get_typed_func::<(i32, i32, i32), i32>(&mut store, "proxy_on_request_headers");

        let action = match callback {
            Ok(func) => {
                let num_headers = headers.len() as i32;
                let eos = if end_of_stream { 1 } else { 0 };
                func.call(&mut store, (http_context_id, num_headers, eos))?
            }
            Err(_) => {
                // Callback not exported, continue
                0
            }
        };

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

        match FilterAction::from(action) {
            FilterAction::Continue => Ok(ModuleResult::Continue { modified_headers }),
            FilterAction::Pause => Ok(ModuleResult::Pause),
        }
    }

    /// Execute on_response_headers callback for all modules (reverse order)
    /// 
    /// Note: This method is deprecated. Use `on_response_headers_with_modules` instead.
    /// This method always returns Continue without applying any modules.
    pub fn on_response_headers(
        &self,
        _path: &str,
        _status: u16,
        headers: &[(String, String)],
        _end_of_stream: bool,
    ) -> FilterResult {
        // ルートレベルのmodulesフィールドを使用するため、このメソッドは使用しない
        FilterResult::Continue {
            headers: headers.to_vec(),
            body: None,
        }
    }

    /// Execute on_response_headers for specified modules
    /// Execute on_response_headers for specified modules
    pub fn on_response_headers_with_modules(
        &self,
        module_names: &[String],
        status: u16,
        headers: &[(String, String)],
        end_of_stream: bool,
    ) -> FilterResult {
        // 指定されたモジュール名からLoadedModuleを取得
        let modules: Vec<Arc<LoadedModule>> = module_names
            .iter()
            .filter_map(|name| self.registry.get_module(name))
            .collect();
        
        if modules.is_empty() {
            return FilterResult::Continue {
                headers: headers.to_vec(),
                body: None,
            };
        }
        
        let mut current_headers = headers.to_vec();
        
        // Execute in reverse order for response
        for module in modules.iter().rev() {
            let result =
                self.execute_on_response_headers(module, status, &current_headers, end_of_stream);
            
            match result {
                Ok(ModuleResult::Continue { modified_headers }) => {
                    if let Some(h) = modified_headers {
                        current_headers = h;
                    }
                }
                Ok(ModuleResult::Pause) => {
                    return FilterResult::Pause;
                }
                Ok(ModuleResult::LocalResponse(resp)) => {
                    return FilterResult::LocalResponse(resp);
                }
                Err(e) => {
                    ftlog::error!(
                        "[wasm:{}] on_response_headers error: {}",
                        module.name,
                        e
                    );
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
    pub async fn on_response_headers_with_modules_async(
        self: Arc<Self>,
        module_names: Vec<String>,
        status: u16,
        headers: Vec<(String, String)>,
        end_of_stream: bool,
    ) -> FilterResult {
        self.on_response_headers_with_modules(
            &module_names,
            status,
            &headers,
            end_of_stream,
        )
    }

    /// Execute on_response_headers for a single module
    fn execute_on_response_headers(
        &self,
        module: &LoadedModule,
        status: u16,
        headers: &[(String, String)],
        end_of_stream: bool,
    ) -> anyhow::Result<ModuleResult> {
        // Create context
        let mut http_ctx = HttpContext::new(1, module.capabilities.clone());
        http_ctx.set_response(status, headers.to_vec());
        http_ctx.plugin_name = module.name.clone();

        // Create store with fuel limit
        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);
        store.set_fuel(self.fuel_limit)?;
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        // Instantiate module
        let instance = module.instance_pre.instantiate(&mut store)?;

        // === Proxy-Wasm SDK Lifecycle ===
        // Step 0: Call _start to initialize the SDK
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call(&mut store, ());
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call(&mut store, ());
        }

        let root_context_id = 1i32;    // Root context ID
        let http_context_id = 2i32;    // HTTP context ID
        let config_size = module.configuration.len() as i32;

        // Step 1: Create ROOT context first (parent=0 means root)
        if let Ok(func) = instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create") {
            let _ = func.call(&mut store, (root_context_id, 0));
        }

        // Step 2: Call proxy_on_vm_start
        if let Ok(func) = instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start") {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        // Step 3: Call proxy_on_configure
        if let Ok(func) = instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure") {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        // Step 4: Create HTTP context with root as parent
        if let Ok(func) = instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create") {
            let _ = func.call(&mut store, (http_context_id, root_context_id));
        }

        // Now call proxy_on_response_headers
        let callback = instance
            .get_typed_func::<(i32, i32, i32), i32>(&mut store, "proxy_on_response_headers");

        let action = match callback {
            Ok(func) => {
                let num_headers = headers.len() as i32;
                let eos = if end_of_stream { 1 } else { 0 };
                func.call(&mut store, (http_context_id, num_headers, eos))?
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
            Some(state.http_ctx.response_headers.clone())
        } else {
            None
        };

        match FilterAction::from(action) {
            FilterAction::Continue => Ok(ModuleResult::Continue { modified_headers }),
            FilterAction::Pause => Ok(ModuleResult::Pause),
        }
    }

    /// Get a loaded module by name
    pub fn get_module(&self, name: &str) -> Option<Arc<LoadedModule>> {
        self.registry.get_module(name)
    }

    /// Execute proxy_on_http_call_response callback for a module
    ///
    /// This should be called after an HTTP call completes to deliver the response
    /// back to the WASM module.
    pub fn on_http_call_response(
        &self,
        module_name: &str,
        token: u32,
        response: super::types::HttpCallResponse,
    ) -> FilterResult {
        let module = match self.registry.get_module(module_name) {
            Some(m) => m,
            None => {
                ftlog::warn!("[wasm] Module '{}' not found for HTTP call response", module_name);
                return FilterResult::Continue {
                    headers: Vec::new(),
                    body: None,
                };
            }
        };

        match self.execute_on_http_call_response(&module, token, response) {
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
    fn execute_on_http_call_response(
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
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        // Instantiate module
        let instance = module.instance_pre.instantiate(&mut store)?;

        // === Proxy-Wasm SDK Lifecycle ===
        // Step 0: Call _start
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call(&mut store, ());
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call(&mut store, ());
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        // Step 1: Create ROOT context
        if let Ok(func) = instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create") {
            let _ = func.call(&mut store, (root_context_id, 0));
        }

        // Step 2: Call proxy_on_vm_start
        if let Ok(func) = instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start") {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        // Step 3: Call proxy_on_configure
        if let Ok(func) = instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure") {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        // Step 4: Create HTTP context with root as parent
        if let Ok(func) = instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create") {
            let _ = func.call(&mut store, (http_context_id, root_context_id));
        }

        // Call proxy_on_http_call_response
        // Signature: (context_id, token, num_headers, body_size, num_trailers) -> void
        let callback = instance
            .get_typed_func::<(i32, i32, i32, i32, i32), ()>(&mut store, "proxy_on_http_call_response");

        match callback {
            Ok(func) => {
                let num_headers = response.headers.len() as i32;
                let body_size = response.body.len() as i32;
                let num_trailers = response.trailers.len() as i32;
                
                if let Err(e) = func.call(&mut store, (http_context_id, token as i32, num_headers, body_size, num_trailers)) {
                    ftlog::debug!("[wasm:{}] proxy_on_http_call_response returned: {}", module.name, e);
                }
            }
            Err(_) => {
                ftlog::debug!("[wasm:{}] proxy_on_http_call_response not exported", module.name);
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
    pub fn on_request_body_with_modules(
        &self,
        module_names: &[String],
        body: &[u8],
        end_of_stream: bool,
    ) -> BodyFilterResult {
        let modules: Vec<Arc<LoadedModule>> = module_names
            .iter()
            .filter_map(|name| self.registry.get_module(name))
            .collect();

        if modules.is_empty() {
            return BodyFilterResult::Continue {
                body: body.to_vec(),
            };
        }

        let mut current_body = body.to_vec();

        for module in &modules {
            let result = self.execute_on_request_body(module, &current_body, end_of_stream);

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
                    ftlog::error!(
                        "[wasm:{}] on_request_body error: {}",
                        module.name,
                        e
                    );
                    // Continue on error
                }
            }
        }

        BodyFilterResult::Continue {
            body: current_body,
        }
    }

    /// Execute on_request_body for specified modules ASYNCHRONOUSLY
    /// Note: runs inline in the current async task to avoid cross-thread waker issues with monoio.
    pub async fn on_request_body_with_modules_async(
        self: Arc<Self>,
        module_names: Vec<String>,
        body: Vec<u8>,
        end_of_stream: bool,
    ) -> BodyFilterResult {
        self.on_request_body_with_modules(&module_names, &body, end_of_stream)
    }

    /// Execute on_request_body for a single module
    fn execute_on_request_body(
        &self,
        module: &LoadedModule,
        body: &[u8],
        end_of_stream: bool,
    ) -> anyhow::Result<BodyModuleResult> {
        // Check capability
        if !module.capabilities.allow_request_body_read {
            return Ok(BodyModuleResult::Continue { modified_body: None });
        }

        // Create context
        let mut http_ctx = HttpContext::new(1, module.capabilities.clone());
        http_ctx.set_request_body(body.to_vec(), end_of_stream);
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_configuration = module.configuration.clone();

        // Create store with fuel limit
        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);
        store.set_fuel(self.fuel_limit)?;
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        // Instantiate module
        let instance = module.instance_pre.instantiate(&mut store)?;

        // Proxy-Wasm SDK Lifecycle
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call(&mut store, ());
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call(&mut store, ());
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        if let Ok(func) = instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create") {
            let _ = func.call(&mut store, (root_context_id, 0));
        }

        if let Ok(func) = instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start") {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) = instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure") {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) = instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create") {
            let _ = func.call(&mut store, (http_context_id, root_context_id));
        }

        // Call proxy_on_request_body
        // Signature: (context_id, body_size, end_of_stream) -> action
        let callback = instance
            .get_typed_func::<(i32, i32, i32), i32>(&mut store, "proxy_on_request_body");

        let action = match callback {
            Ok(func) => {
                let body_size = body.len() as i32;
                let eos = if end_of_stream { 1 } else { 0 };
                func.call(&mut store, (http_context_id, body_size, eos))?
            }
            Err(_) => 0, // Continue if not exported
        };

        // Check for local response
        let state = store.data();
        if let Some(local_response) = &state.http_ctx.local_response {
            return Ok(BodyModuleResult::LocalResponse(local_response.clone()));
        }

        // Check for modifications
        let modified_body = if state.http_ctx.request_body_modified {
            Some(state.http_ctx.request_body.clone())
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
    pub fn on_response_body_with_modules(
        &self,
        module_names: &[String],
        body: &[u8],
        end_of_stream: bool,
    ) -> BodyFilterResult {
        let modules: Vec<Arc<LoadedModule>> = module_names
            .iter()
            .filter_map(|name| self.registry.get_module(name))
            .collect();

        if modules.is_empty() {
            return BodyFilterResult::Continue {
                body: body.to_vec(),
            };
        }

        let mut current_body = body.to_vec();

        // Execute in reverse order for response
        for module in modules.iter().rev() {
            let result = self.execute_on_response_body(module, &current_body, end_of_stream);

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
                    ftlog::error!(
                        "[wasm:{}] on_response_body error: {}",
                        module.name,
                        e
                    );
                }
            }
        }

        BodyFilterResult::Continue {
            body: current_body,
        }
    }

    /// Execute on_response_body for specified modules ASYNCHRONOUSLY
    /// Note: runs inline in the current async task to avoid cross-thread waker issues with monoio.
    pub async fn on_response_body_with_modules_async(
        self: Arc<Self>,
        module_names: Vec<String>,
        body: Vec<u8>,
        end_of_stream: bool,
    ) -> BodyFilterResult {
        self.on_response_body_with_modules(&module_names, &body, end_of_stream)
    }

    /// Execute on_response_body for a single module
    fn execute_on_response_body(
        &self,
        module: &LoadedModule,
        body: &[u8],
        end_of_stream: bool,
    ) -> anyhow::Result<BodyModuleResult> {
        // Check capability
        if !module.capabilities.allow_response_body_read {
            return Ok(BodyModuleResult::Continue { modified_body: None });
        }

        // Create context
        let mut http_ctx = HttpContext::new(1, module.capabilities.clone());
        http_ctx.set_response_body(body.to_vec(), end_of_stream);
        http_ctx.plugin_name = module.name.clone();
        http_ctx.plugin_configuration = module.configuration.clone();

        // Create store with fuel limit
        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);
        store.set_fuel(self.fuel_limit)?;
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        // Instantiate module
        let instance = module.instance_pre.instantiate(&mut store)?;

        // Proxy-Wasm SDK Lifecycle
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call(&mut store, ());
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call(&mut store, ());
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        if let Ok(func) = instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create") {
            let _ = func.call(&mut store, (root_context_id, 0));
        }

        if let Ok(func) = instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start") {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) = instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure") {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) = instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create") {
            let _ = func.call(&mut store, (http_context_id, root_context_id));
        }

        // Call proxy_on_response_body
        // Signature: (context_id, body_size, end_of_stream) -> action
        let callback = instance
            .get_typed_func::<(i32, i32, i32), i32>(&mut store, "proxy_on_response_body");

        let action = match callback {
            Ok(func) => {
                let body_size = body.len() as i32;
                let eos = if end_of_stream { 1 } else { 0 };
                func.call(&mut store, (http_context_id, body_size, eos))?
            }
            Err(_) => 0, // Continue if not exported
        };

        // Check for local response
        let state = store.data();
        if let Some(local_response) = &state.http_ctx.local_response {
            return Ok(BodyModuleResult::LocalResponse(local_response.clone()));
        }

        // Check for modifications
        let modified_body = if state.http_ctx.response_body_modified {
            Some(state.http_ctx.response_body.clone())
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
    pub fn on_log_with_modules(&self, module_names: &[String]) {
        let modules: Vec<Arc<LoadedModule>> = module_names
            .iter()
            .filter_map(|name| self.registry.get_module(name))
            .collect();

        for module in &modules {
            if let Err(e) = self.execute_on_log(module) {
                ftlog::error!("[wasm:{}] on_log error: {}", module.name, e);
            }
        }
    }

    /// Execute on_log for specified modules ASYNCHRONOUSLY
    /// Note: runs inline in the current async task to avoid cross-thread waker issues with monoio.
    pub async fn on_log_with_modules_async(
        self: Arc<Self>,
        module_names: Vec<String>,
    ) {
        self.on_log_with_modules(&module_names);
    }

    /// Execute on_log for a single module
    fn execute_on_log(&self, module: &LoadedModule) -> anyhow::Result<()> {
        // Create context
        let http_ctx = HttpContext::new(1, module.capabilities.clone());
        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);
        store.set_fuel(self.fuel_limit)?;
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        // Instantiate module
        let instance = module.instance_pre.instantiate(&mut store)?;

        // Initialize module
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call(&mut store, ());
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call(&mut store, ());
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        // Create contexts
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call(&mut store, (root_context_id, 0));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call(&mut store, (http_context_id, root_context_id));
        }

        // Call proxy_on_log
        // Signature: (context_id) -> void
        if let Ok(func) = instance.get_typed_func::<i32, ()>(&mut store, "proxy_on_log") {
            match func.call(&mut store, http_context_id) {
                Ok(()) => {
                    ftlog::debug!("[wasm:{}] proxy_on_log({}) OK", module.name, http_context_id)
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
    pub fn on_done_with_modules(&self, module_names: &[String]) -> bool {
        let modules: Vec<Arc<LoadedModule>> = module_names
            .iter()
            .filter_map(|name| self.registry.get_module(name))
            .collect();

        let mut any_pending = false;

        for module in &modules {
            match self.execute_on_done(module) {
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
    fn execute_on_done(&self, module: &LoadedModule) -> anyhow::Result<bool> {
        // Create context
        let http_ctx = HttpContext::new(1, module.capabilities.clone());
        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);
        store.set_fuel(self.fuel_limit)?;
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        // Instantiate module
        let instance = module.instance_pre.instantiate(&mut store)?;

        // Initialize module
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call(&mut store, ());
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call(&mut store, ());
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        // Create contexts
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call(&mut store, (root_context_id, 0));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call(&mut store, (http_context_id, root_context_id));
        }

        // Call proxy_on_done
        // Signature: (context_id) -> bool (1 = keep alive, 0 = delete)
        if let Ok(func) = instance.get_typed_func::<i32, i32>(&mut store, "proxy_on_done") {
            match func.call(&mut store, http_context_id) {
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
    pub fn on_tick(&self, module_name: &str) {
        let module = match self.registry.get_module(module_name) {
            Some(m) => m,
            None => return,
        };

        if let Err(e) = self.execute_on_tick(&module) {
            ftlog::error!("[wasm:{}] on_tick error: {}", module_name, e);
        }
    }

    /// Execute on_tick for a single module
    fn execute_on_tick(&self, module: &LoadedModule) -> anyhow::Result<()> {
        // Create context
        let http_ctx = HttpContext::new(1, module.capabilities.clone());
        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);
        store.set_fuel(self.fuel_limit)?;
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        // Instantiate module
        let instance = module.instance_pre.instantiate(&mut store)?;

        // Initialize module
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call(&mut store, ());
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call(&mut store, ());
        }

        let root_context_id = 1i32;
        let config_size = module.configuration.len() as i32;

        // Create root context only (tick is called on root context)
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call(&mut store, (root_context_id, 0));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        // Call proxy_on_tick on root context
        // Signature: (context_id) -> void
        if let Ok(func) = instance.get_typed_func::<i32, ()>(&mut store, "proxy_on_tick") {
            match func.call(&mut store, root_context_id) {
                Ok(()) => {
                    ftlog::debug!("[wasm:{}] proxy_on_tick({}) OK", module.name, root_context_id)
                }
                Err(e) => ftlog::debug!("[wasm:{}] proxy_on_tick error: {}", module.name, e),
            }
        }

        Ok(())
    }

    /// Execute on_request_trailers callback for specified modules
    ///
    /// Called when request trailers are received (HTTP/2, gRPC).
    pub fn on_request_trailers_with_modules(
        &self,
        module_names: &[String],
        trailers: &[(String, String)],
    ) -> FilterResult {
        let modules: Vec<Arc<LoadedModule>> = module_names
            .iter()
            .filter_map(|name| self.registry.get_module(name))
            .collect();

        if modules.is_empty() {
            return FilterResult::Continue {
                headers: trailers.to_vec(),
                body: None,
            };
        }

        let mut current_trailers = trailers.to_vec();

        for module in &modules {
            let result = self.execute_on_request_trailers(module, &current_trailers);

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
                    ftlog::error!(
                        "[wasm:{}] on_request_trailers error: {}",
                        module.name,
                        e
                    );
                }
            }
        }

        FilterResult::Continue {
            headers: current_trailers,
            body: None,
        }
    }

    /// Execute on_request_trailers for a single module
    fn execute_on_request_trailers(
        &self,
        module: &LoadedModule,
        trailers: &[(String, String)],
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
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        // Instantiate module
        let instance = module.instance_pre.instantiate(&mut store)?;

        // Initialize
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call(&mut store, ());
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call(&mut store, ());
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call(&mut store, (root_context_id, 0));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call(&mut store, (http_context_id, root_context_id));
        }

        // Call proxy_on_request_trailers
        // Signature: (context_id, num_trailers) -> action
        let callback = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_request_trailers");

        let action = match callback {
            Ok(func) => {
                let num_trailers = trailers.len() as i32;
                func.call(&mut store, (http_context_id, num_trailers))?
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
    pub fn on_response_trailers_with_modules(
        &self,
        module_names: &[String],
        trailers: &[(String, String)],
    ) -> FilterResult {
        let modules: Vec<Arc<LoadedModule>> = module_names
            .iter()
            .filter_map(|name| self.registry.get_module(name))
            .collect();

        if modules.is_empty() {
            return FilterResult::Continue {
                headers: trailers.to_vec(),
                body: None,
            };
        }

        let mut current_trailers = trailers.to_vec();

        // Execute in reverse order for response
        for module in modules.iter().rev() {
            let result = self.execute_on_response_trailers(module, &current_trailers);

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
                    ftlog::error!(
                        "[wasm:{}] on_response_trailers error: {}",
                        module.name,
                        e
                    );
                }
            }
        }

        FilterResult::Continue {
            headers: current_trailers,
            body: None,
        }
    }

    /// Execute on_response_trailers for a single module
    fn execute_on_response_trailers(
        &self,
        module: &LoadedModule,
        trailers: &[(String, String)],
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
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        // Instantiate module
        let instance = module.instance_pre.instantiate(&mut store)?;

        // Initialize
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call(&mut store, ());
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call(&mut store, ());
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call(&mut store, (root_context_id, 0));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call(&mut store, (http_context_id, root_context_id));
        }

        // Call proxy_on_response_trailers
        // Signature: (context_id, num_trailers) -> action
        let callback = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_response_trailers");

        let action = match callback {
            Ok(func) => {
                let num_trailers = trailers.len() as i32;
                func.call(&mut store, (http_context_id, num_trailers))?
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
    pub fn on_queue_ready(&self, module_name: &str, queue_id: u32) {
        let module = match self.registry.get_module(module_name) {
            Some(m) => m,
            None => return,
        };

        if let Err(e) = self.execute_on_queue_ready(&module, queue_id) {
            ftlog::error!("[wasm:{}] on_queue_ready error: {}", module_name, e);
        }
    }

    /// Execute on_queue_ready for a single module
    fn execute_on_queue_ready(&self, module: &LoadedModule, queue_id: u32) -> anyhow::Result<()> {
        // Create context
        let http_ctx = HttpContext::new(1, module.capabilities.clone());
        let host_state = HostState::new(http_ctx);
        let mut store = Store::new(self.registry.engine(), host_state);
        store.set_fuel(self.fuel_limit)?;
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        // Instantiate module
        let instance = module.instance_pre.instantiate(&mut store)?;

        // Initialize module
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call(&mut store, ());
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call(&mut store, ());
        }

        let root_context_id = 1i32;
        let config_size = module.configuration.len() as i32;

        // Create root context (queue_ready is called on root context)
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call(&mut store, (root_context_id, 0));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        // Call proxy_on_queue_ready
        // Signature: (context_id, queue_id) -> void
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_queue_ready")
        {
            match func.call(&mut store, (root_context_id, queue_id as i32)) {
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
    pub fn on_grpc_receive_initial_metadata(
        &self,
        module_name: &str,
        call_id: u32,
        headers: &[(String, String)],
    ) {
        let module = match self.registry.get_module(module_name) {
            Some(m) => m,
            None => return,
        };

        if let Err(e) = self.execute_on_grpc_receive_initial_metadata(&module, call_id, headers) {
            ftlog::error!(
                "[wasm:{}] on_grpc_receive_initial_metadata error: {}",
                module_name,
                e
            );
        }
    }

    #[cfg(feature = "grpc")]
    fn execute_on_grpc_receive_initial_metadata(
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
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        let instance = module.instance_pre.instantiate(&mut store)?;

        // Initialize
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call(&mut store, ());
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call(&mut store, ());
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call(&mut store, (root_context_id, 0));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call(&mut store, (http_context_id, root_context_id));
        }

        // Call proxy_on_grpc_receive_initial_metadata
        // Signature: (context_id, call_id, num_headers) -> void
        if let Ok(func) = instance
            .get_typed_func::<(i32, i32, i32), ()>(&mut store, "proxy_on_grpc_receive_initial_metadata")
        {
            let num_headers = headers.len() as i32;
            match func.call(&mut store, (http_context_id, call_id as i32, num_headers)) {
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
    pub fn on_grpc_receive(&self, module_name: &str, call_id: u32, message: &[u8]) {
        let module = match self.registry.get_module(module_name) {
            Some(m) => m,
            None => return,
        };

        if let Err(e) = self.execute_on_grpc_receive(&module, call_id, message) {
            ftlog::error!("[wasm:{}] on_grpc_receive error: {}", module_name, e);
        }
    }

    #[cfg(feature = "grpc")]
    fn execute_on_grpc_receive(
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
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        let instance = module.instance_pre.instantiate(&mut store)?;

        // Initialize
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call(&mut store, ());
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call(&mut store, ());
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call(&mut store, (root_context_id, 0));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call(&mut store, (http_context_id, root_context_id));
        }

        // Call proxy_on_grpc_receive
        // Signature: (context_id, call_id, message_size) -> void
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32, i32), ()>(&mut store, "proxy_on_grpc_receive")
        {
            let message_size = message.len() as i32;
            match func.call(&mut store, (http_context_id, call_id as i32, message_size)) {
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
    pub fn on_grpc_receive_trailing_metadata(
        &self,
        module_name: &str,
        call_id: u32,
        trailers: &[(String, String)],
    ) {
        let module = match self.registry.get_module(module_name) {
            Some(m) => m,
            None => return,
        };

        if let Err(e) = self.execute_on_grpc_receive_trailing_metadata(&module, call_id, trailers) {
            ftlog::error!(
                "[wasm:{}] on_grpc_receive_trailing_metadata error: {}",
                module_name,
                e
            );
        }
    }

    #[cfg(feature = "grpc")]
    fn execute_on_grpc_receive_trailing_metadata(
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
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        let instance = module.instance_pre.instantiate(&mut store)?;

        // Initialize
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call(&mut store, ());
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call(&mut store, ());
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call(&mut store, (root_context_id, 0));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call(&mut store, (http_context_id, root_context_id));
        }

        // Call proxy_on_grpc_receive_trailing_metadata
        // Signature: (context_id, call_id, num_trailers) -> void
        if let Ok(func) = instance.get_typed_func::<(i32, i32, i32), ()>(
            &mut store,
            "proxy_on_grpc_receive_trailing_metadata",
        ) {
            let num_trailers = trailers.len() as i32;
            match func.call(&mut store, (http_context_id, call_id as i32, num_trailers)) {
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
    pub fn on_grpc_close(&self, module_name: &str, call_id: u32, status_code: i32) {
        let module = match self.registry.get_module(module_name) {
            Some(m) => m,
            None => return,
        };

        if let Err(e) = self.execute_on_grpc_close(&module, call_id, status_code) {
            ftlog::error!("[wasm:{}] on_grpc_close error: {}", module_name, e);
        }
    }

    #[cfg(feature = "grpc")]
    fn execute_on_grpc_close(
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
        store.set_epoch_deadline(self.epoch_deadline);
        self.registry.engine().increment_epoch();

        let instance = module.instance_pre.instantiate(&mut store)?;

        // Initialize
        if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            let _ = func.call(&mut store, ());
        } else if let Ok(func) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            let _ = func.call(&mut store, ());
        }

        let root_context_id = 1i32;
        let http_context_id = 2i32;
        let config_size = module.configuration.len() as i32;

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call(&mut store, (root_context_id, 0));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_vm_start")
        {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), i32>(&mut store, "proxy_on_configure")
        {
            let _ = func.call(&mut store, (root_context_id, config_size));
        }

        if let Ok(func) =
            instance.get_typed_func::<(i32, i32), ()>(&mut store, "proxy_on_context_create")
        {
            let _ = func.call(&mut store, (http_context_id, root_context_id));
        }

        // Call proxy_on_grpc_close
        // Signature: (context_id, call_id, status_code) -> void
        if let Ok(func) =
            instance.get_typed_func::<(i32, i32, i32), ()>(&mut store, "proxy_on_grpc_close")
        {
            match func.call(&mut store, (http_context_id, call_id as i32, status_code)) {
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
        modified_headers: Option<Vec<(String, String)>>,
    },
    Pause,
    LocalResponse(LocalResponse),
}

/// Result from filter chain execution
pub enum FilterResult {
    /// Continue with potentially modified headers/body
    Continue {
        headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
    },
    /// Pause processing (async operation pending)
    Pause,
    /// Send a local response instead of proxying
    LocalResponse(LocalResponse),
}

/// Result from body module execution (internal)
enum BodyModuleResult {
    Continue {
        modified_body: Option<Vec<u8>>,
    },
    Pause,
    LocalResponse(LocalResponse),
}

/// Result from body filter chain execution
pub enum BodyFilterResult {
    /// Continue with potentially modified body
    Continue {
        body: Vec<u8>,
    },
    /// Pause processing (async operation pending)
    Pause,
    /// Send a local response instead of proxying
    LocalResponse(LocalResponse),
}
