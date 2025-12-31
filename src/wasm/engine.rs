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
}

impl FilterEngine {
    /// Create a new filter engine
    pub fn new(config: &WasmConfig) -> anyhow::Result<Self> {
        let registry = ModuleRegistry::new(config)?;

        // Calculate fuel limit (roughly 1M instructions per ms)
        let fuel_limit = config.defaults.max_execution_time_ms * 1_000_000;

        Ok(Self {
            registry,
            fuel_limit,
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
    pub fn on_response_headers(
        &self,
        path: &str,
        status: u16,
        headers: &[(String, String)],
        end_of_stream: bool,
    ) -> FilterResult {
        let modules = self.get_modules_for_path(path);

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
