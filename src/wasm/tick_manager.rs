//! Tick Manager for WASM Module Timer Callbacks
//!
//! Manages periodic tick callbacks for WASM modules that request
//! timer functionality via proxy_set_tick_period_milliseconds.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;

use super::engine::FilterEngine;

/// Global tick configuration registry
static TICK_CONFIGS: Lazy<RwLock<TickRegistry>> = Lazy::new(|| RwLock::new(TickRegistry::new()));

/// Tick configuration for a module
#[derive(Debug, Clone)]
pub struct ModuleTickConfig {
    /// Module name
    pub module_name: String,
    /// Tick period in milliseconds
    pub period_ms: u32,
    /// Last tick time
    pub last_tick: Instant,
}

/// Registry of tick configurations
#[derive(Debug)]
pub struct TickRegistry {
    /// Module tick configurations keyed by module name
    configs: HashMap<String, ModuleTickConfig>,
}

impl TickRegistry {
    fn new() -> Self {
        Self {
            configs: HashMap::new(),
        }
    }
}

/// Register a module for tick callbacks
/// 
/// Called when a module sets its tick period via proxy_set_tick_period_milliseconds.
/// If period_ms is 0, the module is unregistered from tick callbacks.
pub fn register_tick(module_name: &str, period_ms: u32) {
    let mut registry = match TICK_CONFIGS.write() {
        Ok(r) => r,
        Err(_) => return,
    };
    
    if period_ms == 0 {
        // Unregister
        registry.configs.remove(module_name);
        ftlog::debug!("[wasm:tick] Unregistered tick for module: {}", module_name);
    } else {
        // Register or update
        registry.configs.insert(
            module_name.to_string(),
            ModuleTickConfig {
                module_name: module_name.to_string(),
                period_ms,
                last_tick: Instant::now(),
            },
        );
        ftlog::debug!(
            "[wasm:tick] Registered tick for module: {} with period: {}ms",
            module_name,
            period_ms
        );
    }
}

/// Get all modules that are due for a tick
/// 
/// Returns a list of module names that should receive a tick callback.
pub fn get_due_ticks() -> Vec<String> {
    let mut registry = match TICK_CONFIGS.write() {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    
    let now = Instant::now();
    let mut due_modules = Vec::new();
    
    for (name, config) in registry.configs.iter_mut() {
        let elapsed = now.duration_since(config.last_tick);
        if elapsed >= Duration::from_millis(config.period_ms as u64) {
            due_modules.push(name.clone());
            config.last_tick = now;
        }
    }
    
    due_modules
}

/// Execute tick callbacks for all due modules
/// 
/// This should be called periodically from a timer loop (e.g., every 10-100ms)
/// to check for and execute any pending tick callbacks.
pub fn process_ticks(engine: &Arc<FilterEngine>) {
    let due_modules = get_due_ticks();
    
    for module_name in due_modules {
        engine.on_tick(&module_name);
    }
}

/// Get the minimum tick period across all registered modules
/// 
/// Useful for determining how often to check for due ticks.
/// Returns None if no modules are registered for ticks.
pub fn get_min_tick_period() -> Option<Duration> {
    let registry = match TICK_CONFIGS.read() {
        Ok(r) => r,
        Err(_) => return None,
    };
    
    registry
        .configs
        .values()
        .map(|c| c.period_ms)
        .min()
        .map(|ms| Duration::from_millis(ms as u64))
}

/// Get statistics about registered tick configurations
pub fn get_tick_stats() -> TickStats {
    let registry = match TICK_CONFIGS.read() {
        Ok(r) => r,
        Err(_) => return TickStats::default(),
    };
    
    TickStats {
        registered_modules: registry.configs.len(),
        min_period_ms: registry.configs.values().map(|c| c.period_ms).min(),
        max_period_ms: registry.configs.values().map(|c| c.period_ms).max(),
    }
}

/// Statistics about tick configurations
#[derive(Debug, Default)]
pub struct TickStats {
    /// Number of registered modules
    pub registered_modules: usize,
    /// Minimum tick period in milliseconds
    pub min_period_ms: Option<u32>,
    /// Maximum tick period in milliseconds
    pub max_period_ms: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_register_tick() {
        // Register a module
        register_tick("test_module_tick", 1000);
        
        let stats = get_tick_stats();
        assert!(stats.registered_modules >= 1);
        
        // Unregister
        register_tick("test_module_tick", 0);
    }
    
    #[test]
    fn test_get_min_tick_period() {
        // Register modules with different periods
        register_tick("test_fast", 100);
        register_tick("test_slow", 5000);
        
        let min = get_min_tick_period();
        assert!(min.is_some());
        assert!(min.unwrap().as_millis() <= 5000);
        
        // Cleanup
        register_tick("test_fast", 0);
        register_tick("test_slow", 0);
    }
    
    #[test]
    fn test_tick_stats() {
        register_tick("stats_test_1", 500);
        register_tick("stats_test_2", 1000);
        
        let stats = get_tick_stats();
        assert!(stats.registered_modules >= 2);
        assert!(stats.min_period_ms.is_some());
        assert!(stats.max_period_ms.is_some());
        
        // Cleanup
        register_tick("stats_test_1", 0);
        register_tick("stats_test_2", 0);
    }
}
