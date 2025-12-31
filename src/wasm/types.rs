//! Types for WASM Extension System

use serde::{Deserialize, Serialize};

use super::capabilities::ModuleCapabilities;

/// WASM extension configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WasmConfig {
    /// Enable WASM extensions
    #[serde(default)]
    pub enabled: bool,

    /// Default settings
    #[serde(default)]
    pub defaults: WasmDefaults,

    /// Module definitions
    #[serde(default)]
    pub modules: Vec<ModuleConfig>,
}

impl Default for WasmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            defaults: WasmDefaults::default(),
            modules: Vec::new(),
        }
    }
}

/// Default WASM settings
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WasmDefaults {
    /// Maximum execution time in milliseconds
    #[serde(default = "default_max_execution_time")]
    pub max_execution_time_ms: u64,

    /// Pooling allocator settings
    #[serde(default)]
    pub pooling: PoolingConfig,
}

impl Default for WasmDefaults {
    fn default() -> Self {
        Self {
            max_execution_time_ms: default_max_execution_time(),
            pooling: PoolingConfig::default(),
        }
    }
}

fn default_max_execution_time() -> u64 {
    100
}

/// Pooling allocator configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PoolingConfig {
    /// Total number of memory pools
    #[serde(default = "default_total_memories")]
    pub total_memories: u32,

    /// Total number of table pools
    #[serde(default = "default_total_tables")]
    pub total_tables: u32,

    /// Maximum memory size per instance (bytes)
    #[serde(default = "default_max_memory_size")]
    pub max_memory_size: usize,
}

impl Default for PoolingConfig {
    fn default() -> Self {
        Self {
            total_memories: default_total_memories(),
            total_tables: default_total_tables(),
            max_memory_size: default_max_memory_size(),
        }
    }
}

fn default_total_memories() -> u32 {
    128
}
fn default_total_tables() -> u32 {
    128
}
fn default_max_memory_size() -> usize {
    10 * 1024 * 1024 // 10MB
}

/// Module configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModuleConfig {
    /// Module name (unique identifier)
    pub name: String,

    /// Path to .wasm or .cwasm file
    pub path: String,

    /// Plugin configuration (JSON string)
    #[serde(default)]
    pub configuration: String,

    /// Capability settings
    #[serde(default)]
    pub capabilities: ModuleCapabilities,
}

/// Filter action returned by callbacks
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterAction {
    /// Continue processing
    Continue,
    /// Pause processing (async operation)
    Pause,
}

impl From<i32> for FilterAction {
    fn from(value: i32) -> Self {
        match value {
            0 => FilterAction::Continue,
            _ => FilterAction::Pause,
        }
    }
}

/// Local response to send instead of proxying
#[derive(Debug, Clone)]
pub struct LocalResponse {
    pub status_code: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Pending HTTP call
#[derive(Debug)]
pub struct PendingHttpCall {
    pub token: u32,
    pub upstream: String,
    pub timeout_ms: u32,
}

/// HTTP call response
#[derive(Debug)]
pub struct HttpCallResponse {
    pub status_code: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub trailers: Vec<(String, String)>,
}

/// Metric value
#[derive(Debug, Clone)]
pub enum MetricValue {
    Counter(i64),
    Gauge(i64),
    Histogram(Vec<u64>),
}

/// Defined metric
#[derive(Debug, Clone)]
pub struct Metric {
    pub name: String,
    pub metric_type: i32,
    pub value: MetricValue,
}

impl WasmConfig {
    /// Validate WASM configuration
    pub fn validate(&self) -> anyhow::Result<()> {
        use std::collections::HashSet;
        
        // モジュール名の重複チェック
        let mut module_names = HashSet::new();
        for module in &self.modules {
            if !module_names.insert(&module.name) {
                anyhow::bail!("Duplicate module name: {}", module.name);
            }
        }

        // モジュールファイルの存在チェック
        for module in &self.modules {
            let path = std::path::Path::new(&module.path);
            if !path.exists() {
                anyhow::bail!("WASM module file not found: {}", module.path);
            }
        }

        Ok(())
    }
}
