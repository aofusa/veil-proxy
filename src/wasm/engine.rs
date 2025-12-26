//! Filter Engine for executing WASM modules
//!
//! Manages the execution of Proxy-Wasm filter chains.

use std::sync::Arc;

use wasmtime::Store;

use super::capabilities::ModuleCapabilities;
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

    /// Get modules for a path
    pub fn get_modules_for_path(&self, path: &str) -> Vec<Arc<LoadedModule>> {
        self.registry.get_modules_for_route(path)
    }

    /// Execute on_request_headers callback for all modules
    pub fn on_request_headers(
        &self,
        path: &str,
        method: &str,
        headers: &[(String, String)],
        client_ip: &str,
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
                    log::error!(
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

        // Get callback function
        let callback = instance
            .get_typed_func::<(i32, i32, i32), i32>(&mut store, "proxy_on_request_headers");

        let action = match callback {
            Ok(func) => {
                let num_headers = headers.len() as i32;
                let eos = if end_of_stream { 1 } else { 0 };
                func.call(&mut store, (1, num_headers, eos))?
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
                    log::error!(
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

        // Get callback function
        let callback = instance
            .get_typed_func::<(i32, i32, i32), i32>(&mut store, "proxy_on_response_headers");

        let action = match callback {
            Ok(func) => {
                let num_headers = headers.len() as i32;
                let eos = if end_of_stream { 1 } else { 0 };
                func.call(&mut store, (1, num_headers, eos))?
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
