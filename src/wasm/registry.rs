//! Module Registry for WASM Extensions
//!
//! Manages loading and caching of WASM modules.

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
    modules: std::collections::HashMap<String, Arc<LoadedModule>>,
}

impl ModuleRegistry {
    /// Create a new module registry
    pub fn new(config: &WasmConfig) -> anyhow::Result<Self> {
        // Create engine with pooling allocator
        let engine = Self::create_engine(&config.defaults.pooling)?;

        let mut registry = Self {
            engine,
            modules: std::collections::HashMap::new(),
        };

        // Load all modules
        for module_config in &config.modules {
            registry.load_module(module_config)?;
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

        // Note: epoch_interruption requires calling store.set_epoch_deadline() before execution
        // and incrementing engine.increment_epoch() periodically for timeout enforcement.
        // Currently disabled as it causes immediate traps without proper epoch management.
        // TODO: Implement proper epoch-based timeout with background thread
        // config.epoch_interruption(true);

        Engine::new(&config)
    }

    /// Load a module
    fn load_module(&mut self, config: &ModuleConfig) -> anyhow::Result<()> {
        ftlog::info!("Loading WASM module: {}", config.name);

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

        ftlog::info!(
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

    /// Get the Wasmtime engine
    pub fn engine(&self) -> &Engine {
        &self.engine
    }
}
