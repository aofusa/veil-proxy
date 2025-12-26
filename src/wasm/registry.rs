//! Module Registry for WASM Extensions
//!
//! Manages loading and caching of WASM modules.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use wasmtime::{Engine, InstancePre, Linker, Module, PoolingAllocationConfig};

use super::capabilities::ModuleCapabilities;
use super::context::HostState;
use super::host;
use super::types::{ModuleConfig, PoolingConfig, WasmConfig};

/// Loaded WASM module
pub struct LoadedModule {
    /// Module name
    pub name: String,
    /// Pre-instantiated module for fast creation
    pub instance_pre: InstancePre<HostState>,
    /// Module capabilities
    pub capabilities: ModuleCapabilities,
    /// Plugin configuration
    pub configuration: Vec<u8>,
}

/// Module registry
pub struct ModuleRegistry {
    /// Wasmtime engine
    engine: Engine,
    /// Loaded modules
    modules: HashMap<String, Arc<LoadedModule>>,
    /// Route to module mapping
    routes: HashMap<String, Vec<String>>,
}

impl ModuleRegistry {
    /// Create a new module registry
    pub fn new(config: &WasmConfig) -> anyhow::Result<Self> {
        // Create engine with pooling allocator
        let engine = Self::create_engine(&config.defaults.pooling)?;

        let mut registry = Self {
            engine,
            modules: HashMap::new(),
            routes: HashMap::new(),
        };

        // Load all modules
        for module_config in &config.modules {
            registry.load_module(module_config)?;
        }

        // Setup routes
        for (route, route_modules) in &config.routes {
            registry
                .routes
                .insert(route.clone(), route_modules.modules.clone());
        }

        Ok(registry)
    }

    /// Create Wasmtime engine with pooling allocator
    fn create_engine(pooling: &PoolingConfig) -> anyhow::Result<Engine> {
        let mut config = wasmtime::Config::new();

        // Enable AOT compilation
        config.cranelift_opt_level(wasmtime::OptLevel::Speed);

        // Enable pooling allocator
        let mut pooling_config = PoolingAllocationConfig::default();
        pooling_config.total_memories(pooling.total_memories);
        pooling_config.total_tables(pooling.total_tables);
        pooling_config.max_memory_size(pooling.max_memory_size);

        config.allocation_strategy(wasmtime::InstanceAllocationStrategy::Pooling(pooling_config));

        // Enable fuel for execution limits
        config.consume_fuel(true);

        // Enable epoch interruption
        config.epoch_interruption(true);

        Engine::new(&config)
    }

    /// Load a module
    fn load_module(&mut self, config: &ModuleConfig) -> anyhow::Result<()> {
        log::info!("Loading WASM module: {}", config.name);

        // Check if file exists
        let path = Path::new(&config.path);
        if !path.exists() {
            anyhow::bail!("Module file not found: {}", config.path);
        }

        // Load module (AOT or standard)
        let module = if config.path.ends_with(".cwasm") {
            // AOT compiled module
            unsafe { Module::deserialize_file(&self.engine, &config.path)? }
        } else {
            // Standard WASM module
            Module::from_file(&self.engine, &config.path)?
        };

        // Create linker with host functions
        let mut linker: Linker<HostState> = Linker::new(&self.engine);
        host::add_host_functions(&mut linker)?;

        // Create InstancePre for fast instantiation
        let instance_pre = linker.instantiate_pre(&module)?;

        // Store module
        let loaded = LoadedModule {
            name: config.name.clone(),
            instance_pre,
            capabilities: config.capabilities.clone(),
            configuration: config.configuration.as_bytes().to_vec(),
        };

        log::info!(
            "Loaded WASM module '{}' with capabilities: http_calls={}, upstreams={:?}",
            config.name,
            config.capabilities.allow_http_calls,
            config.capabilities.allowed_upstreams
        );

        self.modules.insert(config.name.clone(), Arc::new(loaded));

        Ok(())
    }

    /// Get a loaded module by name
    pub fn get_module(&self, name: &str) -> Option<Arc<LoadedModule>> {
        self.modules.get(name).cloned()
    }

    /// Get modules for a route
    pub fn get_modules_for_route(&self, path: &str) -> Vec<Arc<LoadedModule>> {
        // Try exact match first
        if let Some(module_names) = self.routes.get(path) {
            return module_names
                .iter()
                .filter_map(|name| self.get_module(name))
                .collect();
        }

        // Try prefix match
        let mut best_match: Option<(&str, &Vec<String>)> = None;

        for (route, modules) in &self.routes {
            if route == "*" {
                if best_match.is_none() {
                    best_match = Some((route, modules));
                }
            } else if path.starts_with(route.as_str()) {
                if best_match.is_none()
                    || route.len() > best_match.as_ref().map(|(r, _)| r.len()).unwrap_or(0)
                {
                    best_match = Some((route, modules));
                }
            }
        }

        best_match
            .map(|(_, modules)| {
                modules
                    .iter()
                    .filter_map(|name| self.get_module(name))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get the Wasmtime engine
    pub fn engine(&self) -> &Engine {
        &self.engine
    }
}
