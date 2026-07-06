//! Module Registry for WASM Extensions
//!
//! Manages loading and caching of WASM modules.

use std::path::{Path, PathBuf};
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

        config.allocation_strategy(wasmtime::InstanceAllocationStrategy::Pooling(
            pooling_config,
        ));

        // 非同期サポートを有効化（async host functions のため）
        // これにより Store::call_async などの非同期実行が可能になる
        config.async_support(true);

        // fuel によるCPU消費制限（主要なタイムアウト機構）
        config.consume_fuel(true);

        // エポック割り込みによるタイムアウト（補助的保護）
        // エンジンのエポックを定期的にインクリメントし、
        // Store のデッドラインと照合してタイムアウトを検出する
        config.epoch_interruption(true);

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

        // Load module: 明示的 .cwasm はそのまま deserialize、.wasm は AOT サイドカー
        // キャッシュ（<path>.cwasm）経由でロードする（無ければコンパイルして生成、F-36）。
        let module = if config.path.ends_with(".cwasm") {
            // SAFETY: 信頼できる事前生成 AOT モジュールの明示指定。
            unsafe { Module::deserialize_file(&self.engine, &config.path)? }
        } else {
            Self::load_or_compile_with_cache(&self.engine, path)?
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

    /// `.wasm` を AOT サイドカーキャッシュ経由でロードする（F-36）。
    ///
    /// 1. `<path>.cwasm` が存在し `.wasm` 以降に生成されていれば `deserialize_file` で
    ///    高速ロードする（Cranelift JIT を回避し起動時間とメモリを削減）。
    /// 2. 不在・古い・wasmtime 版不一致（deserialize 失敗）の場合は `from_file` で
    ///    コンパイルし、その AOT バイナリをサイドカーへ書き出す（ベストエフォート、
    ///    書き込み失敗は無視してコンパイル済みモジュールをそのまま使う）。
    ///
    /// モジュールロードは起動時（Landlock / seccomp 適用前）に行われるため、サイドカー
    /// 書き込みの権限問題は通常発生しない。`deserialize` は自前生成の信頼できるキャッシュ
    /// のみを対象とし（信頼境界は元々の `.wasm` と同じくキャッシュ/設定ディレクトリの
    /// ファイル完全性）、いかなるエラーも安全側（再コンパイル）にフォールバックする。
    // 理由付き allow: WASM モジュールのロード・AOT キャッシュ生成は設定適用時（起動/リロード）のコールドパス。
    #[allow(clippy::disallowed_methods)]
    fn load_or_compile_with_cache(engine: &Engine, wasm_path: &Path) -> anyhow::Result<Module> {
        // サイドカーパス: "<path>.cwasm"
        let cache_path: PathBuf = {
            let mut s = wasm_path.as_os_str().to_owned();
            s.push(".cwasm");
            PathBuf::from(s)
        };

        // キャッシュが .wasm 以降に生成されていれば deserialize を試みる
        let cache_fresh = matches!(
            (
                std::fs::metadata(wasm_path).and_then(|m| m.modified()),
                std::fs::metadata(&cache_path).and_then(|m| m.modified()),
            ),
            (Ok(wasm_mtime), Ok(cache_mtime)) if cache_mtime >= wasm_mtime
        );
        if cache_fresh {
            // SAFETY: 自前生成の AOT キャッシュのみ deserialize。版不一致/破損は Err となり
            // 下のコンパイル経路へフォールバックする。
            match unsafe { Module::deserialize_file(engine, &cache_path) } {
                Ok(module) => {
                    ftlog::debug!("WASM AOT cache hit: {}", cache_path.display());
                    return Ok(module);
                }
                Err(e) => {
                    ftlog::debug!("WASM AOT cache invalid ({e}); recompiling");
                }
            }
        }

        // コンパイル（JIT/AOT by Cranelift）
        let module = Module::from_file(engine, wasm_path)?;

        // AOT バイナリをサイドカーへ書き出す（ベストエフォート、失敗は無視）。
        // tmp へ書いて rename することで部分書き込み/競合を避ける。
        match module.serialize() {
            Ok(bytes) => {
                let tmp_path: PathBuf = {
                    let mut s = cache_path.clone().into_os_string();
                    s.push(".tmp");
                    PathBuf::from(s)
                };
                let written = std::fs::write(&tmp_path, &bytes)
                    .and_then(|_| std::fs::rename(&tmp_path, &cache_path));
                if written.is_ok() {
                    ftlog::debug!("WASM AOT cache written: {}", cache_path.display());
                } else {
                    let _ = std::fs::remove_file(&tmp_path);
                    ftlog::debug!(
                        "WASM AOT cache write skipped (not writable): {}",
                        cache_path.display()
                    );
                }
            }
            Err(e) => ftlog::debug!("WASM serialize failed ({e}); AOT cache skipped"),
        }

        Ok(module)
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
