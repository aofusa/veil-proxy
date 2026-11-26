//! Module Registry for WASM Extensions
//!
//! Manages loading and caching of WASM modules.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use wasmtime::{Engine, InstancePre, Linker, Module};
// OpenBSD は OnDemand アロケータを使うため未使用になる（`create_engine` 参照。B-52）。
#[cfg(not(target_os = "openbsd"))]
use wasmtime::PoolingAllocationConfig;

use super::capabilities::ModuleCapabilities;
use super::context::HostState;
use super::host;
use super::types::{ModuleConfig, WasmConfig};

/// Loaded WASM module
pub struct LoadedModule {
    /// Module name
    pub name: Arc<str>,
    /// Pre-instantiated module for fast creation
    pub instance_pre: InstancePre<HostState>,
    /// Module capabilities
    pub capabilities: Arc<ModuleCapabilities>,
    /// Plugin configuration
    pub configuration: Arc<[u8]>,
    /// Proxy-Wasm の root_id（`plugin_root_id`）。未設定なら従来どおりルートコンテキストの ID を返す
    pub root_id: Option<Arc<str>>,
    /// HTTP のコールバックで VM の文脈を使い回す（F-271）
    pub instance_reuse: bool,
    /// 使い回すインスタンス（`instance_reuse` のときだけ使う）
    pub(crate) pool: super::instance_pool::InstancePool,
    /// 起動時に読んだファイル（`data_files`。F-271）
    pub data_files: super::context::PluginData,
    /// shared data の表（同じ名前のモジュールで共有し、リロードでも引き継ぐ。B-179）
    pub shared_data: Arc<super::shared_data::SharedData>,
    /// 失敗したときの扱い（F-293。モジュールの設定 → `[wasm.defaults]` の順にロード時に決める）
    pub failure_policy: super::types::FailurePolicy,
}

/// ゲストの呼び出し 1 回が使えるスタック（wasmtime の既定と同じ）
const MAX_WASM_STACK: usize = 512 * 1024;

/// コールバックを走らせるファイバーのスタック（wasmtime の既定と同じ）。
///
/// F-318: ホスト関数はゲストのメモリの確保関数を同じファイバーの上で呼ぶ（`exports::allocate_guest_memory`。入れ子は
/// 1 段だけ）。非同期のストアでは wasmtime はゲストに入るたびにスタックの上限を今のスタックポインタから
/// `MAX_WASM_STACK` 分と測り直すので、外側と入れ子の 2 回分とホストのフレームが収まる大きさにする（足りないと、
/// 入れ子のゲストがスタックの上限の検査より先にファイバーのガードページに届く）。
const ASYNC_STACK_SIZE: usize = 2 * 1024 * 1024;
const _: () = assert!(ASYNC_STACK_SIZE >= 2 * MAX_WASM_STACK + 512 * 1024);

/// `config` のエンジンが Pulley インタープリタで動くか（ネイティブのコードを作らない）。
///
/// OpenBSD/NetBSD、および `veil_wasm_nosignals`（B-55: netbsd 全アーキ・freebsd/openbsd の aarch64。
/// third_party/wasmtime = veil-proxy-wasmtime を使うターゲット）は設定値を無視して常に Pulley（B-52/F-140/B-55）。
/// 他ターゲットは `[wasm] interpreter = true` のときだけ。実行可能なメモリを作れないリロードで作り直してよいかの
/// 判断にも使う（B-404）
pub fn uses_pulley(config: &WasmConfig) -> bool {
    cfg!(target_os = "openbsd")
        || cfg!(target_os = "netbsd")
        || cfg!(veil_wasm_nosignals)
        || config.interpreter
}

/// import する host 関数と、それを使うのに要る capability（B-299）。ヘッダ・本文の読み書きは引数の
/// map の種類で決まるので、import からは判断できない（ここに載せない）
const IMPORT_CAPABILITIES: &[(&str, &str, fn(&ModuleCapabilities) -> bool)] = &[
    (
        "proxy_send_local_response",
        "allow_send_local_response",
        |c| c.allow_send_local_response,
    ),
    ("proxy_http_call", "allow_http_calls", |c| {
        c.allow_http_calls
    }),
    ("proxy_dispatch_http_call", "allow_http_calls", |c| {
        c.allow_http_calls
    }),
    ("proxy_grpc_call", "allow_http_calls", |c| {
        c.allow_http_calls
    }),
    ("proxy_grpc_stream", "allow_http_calls", |c| {
        c.allow_http_calls
    }),
    ("proxy_define_metric", "allow_metrics", |c| c.allow_metrics),
    ("proxy_set_shared_data", "allow_shared_data", |c| {
        c.allow_shared_data
    }),
    ("proxy_get_shared_data", "allow_shared_data", |c| {
        c.allow_shared_data
    }),
    ("proxy_set_property", "allow_property_write", |c| {
        c.allow_property_write
    }),
];

/// モジュールが import する host 関数に要る capability が無ければ、ロード時に警告する（B-299）。
/// 権限の無い呼び出しは `NotAllowed` を返し、それを unwrap する SDK（proxy-wasm の Rust SDK 等）は
/// トラップする＝要求ごとに `failure_policy` の失敗になる。
fn warn_missing_capabilities(name: &str, module: &Module, caps: &ModuleCapabilities) {
    for import in module.imports() {
        if let Some((func, cap, _)) = IMPORT_CAPABILITIES
            .iter()
            .find(|(f, _, granted)| *f == import.name() && !granted(caps))
        {
            ftlog::warn!(
                "[wasm:{}] the module imports {} but {} is not granted; such calls fail with \
                 NotAllowed, and SDKs that unwrap the status trap (the request is then handled \
                 by failure_policy)",
                name,
                func,
                cap
            );
        }
    }
}

/// Module registry
pub struct ModuleRegistry {
    /// Wasmtime engine
    engine: Engine,
    /// Loaded modules
    /// 要求の段ごとに名前で引く（F-331: 鍵は設定のモジュール名なので、SipHash でなくルートキャッシュ等と同じ乱数シード付きの
    /// foldhash）
    modules: std::collections::HashMap<String, Arc<LoadedModule>, foldhash::quality::RandomState>,
    /// エンジンが Pulley インタープリタ向け（`Config::target("pulleyNN")`）かどうか。
    /// AOT サイドカーキャッシュのファイル名をネイティブ JIT 版と分けるために使う
    /// （F-135。Pulley と Cranelift のキャッシュはバイナリ非互換）。
    pulley: bool,
    /// `instance_reuse` のプールの 1 スレッドに持つインスタンスの上限（[`super::instance_pool::per_thread_capacity`]）
    pool_per_thread: usize,
}

impl ModuleRegistry {
    /// Create a new module registry
    pub fn new(config: &WasmConfig) -> anyhow::Result<Self> {
        // Create engine with pooling allocator
        // エポックを進める専用スレッドは `FilterEngine` が起こす（`metering::spawn_ticker`。B-173・F-319）
        let engine = Self::create_engine(config)?;
        // NetBSD は OpenBSD と異なり MAP_STACK 強制が無いため OnDemand アロケータや
        // openbsd_stack（下記）は使わないが、wasmtime のネイティブ JIT 実行コード自体は
        // 未検証のため、OpenBSD と同様に安全側で Pulley インタープリタへ倒す（F-140）。
        // `veil_wasm_nosignals`（B-55: netbsd 全アーキ・freebsd/openbsd の aarch64。
        // build.rs 発行）のターゲットは third_party/wasmtime（veil-proxy-wasmtime）を使い、
        // ネイティブコードを生成しない Pulley 実行のみを前提にしているため常に Pulley。
        let pulley = uses_pulley(config);

        let reuse_modules = config.modules.iter().filter(|m| m.instance_reuse).count();
        let mut registry = Self {
            engine,
            modules: std::collections::HashMap::default(),
            pulley,
            pool_per_thread: super::instance_pool::per_thread_capacity(
                config.defaults.pooling.total_memories,
                reuse_modules,
            ),
        };

        // Load all modules
        for module_config in &config.modules {
            registry.load_module(module_config, config.defaults.failure_policy)?;
        }

        Ok(registry)
    }

    /// 読み込んだモジュールの root_id を設定する（F-222。共有前のみ）。
    #[cfg(feature = "mesh")]
    pub fn set_root_id(&mut self, module: &str, root_id: &str) -> bool {
        match self.modules.get_mut(module).and_then(Arc::get_mut) {
            Some(m) => {
                m.root_id = Some(Arc::from(root_id));
                true
            }
            None => false,
        }
    }

    /// Create Wasmtime engine with pooling allocator
    fn create_engine(wasm_config: &WasmConfig) -> anyhow::Result<Engine> {
        let pooling = &wasm_config.defaults.pooling;
        let mut config = wasmtime::Config::new();

        // Enable AOT compilation
        config.cranelift_opt_level(wasmtime::OptLevel::Speed);

        // プーリングアロケータ（OpenBSD 以外）
        //
        // OpenBSD だけ **OnDemand** を使う。wasmtime の `Config::with_host_stack`
        // （= ファイバスタックの確保方法の差し替え）は **OnDemand でしか参照されず、
        // プーリングアロケータでは黙って無視される**（wasmtime 36 / 40 の
        // `config.rs::build_allocator`）。プーリング側はスタックを自前のスラブから
        // `MAP_STACK` 無しで切り出すため、OpenBSD ではファイバへ切り替えた瞬間に
        // カーネルに殺される（B-52）。OnDemand にして下の `MapStackCreator` を
        // 実際に効かせる。
        #[cfg(not(target_os = "openbsd"))]
        {
            let mut pooling_config = PoolingAllocationConfig::default();
            pooling_config.total_memories(pooling.total_memories);
            pooling_config.total_tables(pooling.total_tables);
            pooling_config.max_memory_size(pooling.max_memory_size);

            config.allocation_strategy(wasmtime::InstanceAllocationStrategy::Pooling(
                pooling_config,
            ));
        }
        #[cfg(target_os = "openbsd")]
        {
            // `pooling` の各上限は OnDemand では使わない（インスタンス単位で確保する）。
            let _ = pooling;
            config.allocation_strategy(wasmtime::InstanceAllocationStrategy::OnDemand);
        }

        // 非同期サポートを有効化（エポック・fuel の区切りでワーカーへ制御を返すため）
        // これにより Store::call_async などの非同期実行が可能になる
        config.async_support(true);
        config.max_wasm_stack(MAX_WASM_STACK);
        config.async_stack_size(ASYNC_STACK_SIZE);

        // CPU の上限と協調的な yield の計り方（`[wasm] metering`。F-319）。fuel はゲストの基本ブロックごとに残量の
        // 検査を入れるので `"fuel"` のとき（未指定なら Pulley のとき）だけ有効にする（`"clock"` はエポックだけで測る。
        // `super::metering`）
        config.consume_fuel(super::metering::resolve(wasm_config) == super::types::Metering::Fuel);

        // エポック割り込み（`clock` の CPU の上限・yield と、両方の方式の実時間の上限）
        // 専用スレッドがエンジンのエポックを進め、Store の締切と照合する
        config.epoch_interruption(true);

        // Pulley インタープリタ（F-135 / B-52）。
        //
        // Pulley は wasm を**ポータブルなバイトコードへコンパイルしてインタープリタで
        // 実行する**バックエンドで、ネイティブコードの生成・実行を一切行わないため、
        // ホスト固有の JIT 前提（W^X・実行可能 mmap・シグナルベースのトラップ等）に
        // 依存しない。ネイティブ JIT より遅いが、W^X 制約下や OpenBSD で WASM を
        // 使えるようにする。
        //
        // OpenBSD/NetBSD、および `veil_wasm_nosignals`（B-55: netbsd 全アーキ・
        // freebsd/openbsd の aarch64。third_party/wasmtime = veil-proxy-wasmtime を使う
        // ターゲット）は設定値を無視して常に Pulley（B-52/F-140/B-55）。
        // 他ターゲットは `[wasm] interpreter = true` のときだけ Pulley を選択する。
        if uses_pulley(wasm_config) {
            // ポインタ幅に合わせて pulley64 / pulley32 を選ぶ。
            config.target(if cfg!(target_pointer_width = "64") {
                "pulley64"
            } else {
                "pulley32"
            })?;
        }

        // OpenBSD: wasmtime 既定のファイバスタックは MAP_STACK なしで mmap されるが、
        // OpenBSD 6.4+ は「SP は MAP_STACK 領域を指すこと」をカーネルが強制する。
        // これは実行方式に依存しない（Pulley でもインタープリタループはファイバ
        // スタック上で回る）ため、Pulley と併せて差し込む。
        #[cfg(target_os = "openbsd")]
        config.with_host_stack(std::sync::Arc::new(super::openbsd_stack::MapStackCreator));

        Engine::new(&config)
    }

    /// Load a module
    fn load_module(
        &mut self,
        config: &ModuleConfig,
        default_policy: super::types::FailurePolicy,
    ) -> anyhow::Result<()> {
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
            let suffix = if self.pulley {
                ".pulley.cwasm"
            } else {
                ".cwasm"
            };
            Self::load_or_compile_with_cache(&self.engine, path, suffix)?
        };

        // Create linker with host functions
        let mut linker: Linker<HostState> = Linker::new(&self.engine);
        host::add_host_functions(&mut linker)?;

        // Create InstancePre for fast instantiation
        let instance_pre = linker.instantiate_pre(&module)?;
        warn_missing_capabilities(&config.name, &module, &config.capabilities);

        // F-271: data_files は設定ロード時に読む（読むのは設定に書いたパスだけ）
        let mut data_files = std::collections::HashMap::new();
        for (name, file) in &config.data_files {
            // 理由付き allow: モジュールのロードは設定適用時（起動/リロード）のコールドパス
            #[allow(clippy::disallowed_methods)]
            let bytes = std::fs::read(file).map_err(|e| {
                anyhow::anyhow!(
                    "failed to read data file '{}' ({}) for module '{}': {}",
                    name,
                    file,
                    config.name,
                    e
                )
            })?;
            data_files.insert(name.clone(), Arc::<[u8]>::from(bytes));
        }

        // Store module
        let loaded = LoadedModule {
            name: Arc::from(config.name.as_str()),
            instance_pre,
            capabilities: Arc::new(config.capabilities.clone()),
            configuration: config.configuration.to_bytes(),
            root_id: None,
            instance_reuse: config.instance_reuse,
            pool: super::instance_pool::InstancePool::new(self.pool_per_thread),
            data_files: Arc::new(data_files),
            shared_data: super::shared_data::for_module(&config.name),
            failure_policy: config.failure_policy.unwrap_or(default_policy),
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
    /// 1. `.wasm` を読み、中身の xxh3-128 を名前に入れたキャッシュ `<path>.<32 桁の 16 進><suffix>` があれば
    ///    `deserialize_file` で高速ロードする（Cranelift JIT を回避し起動時間とメモリを削減）。
    /// 2. 無い・wasmtime 版不一致（deserialize 失敗）の場合は読んだバイト列をコンパイルし、その AOT バイナリを
    ///    キャッシュへ書き出す（ベストエフォート、書き込み失敗は無視してコンパイル済みモジュールをそのまま使う）。
    ///    書けたら同じ `.wasm` の古い版のキャッシュ（同じ形の名前で中身の印が違うもの）を消す。
    ///
    /// B-407: 以前は `<path><suffix>` の mtime が `.wasm` 以降なら新しいとみなしていたので、古い版の `.wasm` を
    /// mtime ごと戻す（`cp -p`・tar・パッケージの入れ直し）と、新しい版のネイティブのコードを読んでいた。
    /// 名前に中身の印を入れたので、`.wasm` の mtime・置き方に関係なく、別の中身のキャッシュは読まない。
    /// 以前の `<path><suffix>` は読まないし消さない（設定に明示した `.cwasm` かもしれない）。
    ///
    /// `suffix` はキャッシュファイル名の拡張子で、通常は `.cwasm`。Pulley
    /// インタープリタ使用時（F-135）は `.pulley.cwasm` を渡し、Cranelift ネイティブ
    /// AOT キャッシュとバイナリ非互換な Pulley 用キャッシュを別ファイルに分離する
    /// （同名だと deserialize が毎回失敗し再コンパイルを繰り返すため）。
    ///
    /// モジュールロードは起動時（Landlock / seccomp 適用前）に行われるため、サイドカー
    /// 書き込みの権限問題は通常発生しない。`deserialize` は自前生成の信頼できるキャッシュ
    /// のみを対象とし（信頼境界は元々の `.wasm` と同じくキャッシュ/設定ディレクトリの
    /// ファイル完全性）、いかなるエラーも安全側（再コンパイル）にフォールバックする。
    // 理由付き allow: WASM モジュールのロード・AOT キャッシュ生成は設定適用時（起動/リロード）のコールドパス。
    #[allow(clippy::disallowed_methods)]
    fn load_or_compile_with_cache(
        engine: &Engine,
        wasm_path: &Path,
        suffix: &str,
    ) -> anyhow::Result<Module> {
        let wasm = std::fs::read(wasm_path)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {}", wasm_path.display(), e))?;
        let cache_path = aot_cache_path(wasm_path, &wasm, suffix);

        if cache_path.exists() {
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

        // コンパイル（JIT/AOT by Cranelift）。印を取ったのと同じバイト列をコンパイルする
        let module = Module::new(engine, &wasm)?;

        // AOT バイナリをサイドカーへ書き出す（ベストエフォート、失敗は無視）。
        // tmp へ書いて rename することで部分書き込み/競合を避ける。
        // tmp の名前は書き手ごとに分ける（同じ名前だと、別の書き手が rename 済みの inode を切り詰めて書き直し、
        // それを mmap している側が SIGBUS になる。同じモジュールを並行してロードする単体試験で起きた。F-271）。
        match module.serialize() {
            Ok(bytes) => {
                static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let tmp_path: PathBuf = {
                    let mut s = cache_path.clone().into_os_string();
                    s.push(format!(
                        ".{}.{}.tmp",
                        std::process::id(),
                        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    ));
                    PathBuf::from(s)
                };
                let written = std::fs::write(&tmp_path, &bytes)
                    .and_then(|_| std::fs::rename(&tmp_path, &cache_path));
                if written.is_ok() {
                    ftlog::debug!("WASM AOT cache written: {}", cache_path.display());
                    remove_stale_aot_caches(wasm_path, &cache_path, suffix);
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

    /// 読み込んだモジュールを借りる（要求ごとの経路。`Arc` を複製しない。F-318）
    pub(crate) fn loaded_module(&self, name: &str) -> Option<&LoadedModule> {
        self.modules.get(name).map(|m| &**m)
    }

    /// 読み込んだモジュール（起動時の検証用。B-301）
    pub(crate) fn modules(&self) -> impl Iterator<Item = &Arc<LoadedModule>> {
        self.modules.values()
    }

    /// Get the Wasmtime engine
    pub fn engine(&self) -> &Engine {
        &self.engine
    }
}

/// AOT キャッシュのパス `<path>.<中身の xxh3-128 の 32 桁の 16 進><suffix>`（B-407）
fn aot_cache_path(wasm_path: &Path, wasm: &[u8], suffix: &str) -> PathBuf {
    let mut s = wasm_path.as_os_str().to_owned();
    s.push(format!(
        ".{:032x}{}",
        xxhash_rust::xxh3::xxh3_128(wasm),
        suffix
    ));
    PathBuf::from(s)
}

/// `name` が `wasm_name` の AOT キャッシュの名前（`<wasm_name>.<32 桁の小文字の 16 進><suffix>`）か（B-407）
fn is_aot_cache_name(name: &str, wasm_name: &str, suffix: &str) -> bool {
    name.strip_prefix(wasm_name)
        .and_then(|r| r.strip_prefix('.'))
        .and_then(|r| r.strip_suffix(suffix))
        .is_some_and(|h| h.len() == 32 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// 同じ `.wasm` の古い版の AOT キャッシュを消す（B-407。ベストエフォート）。消すのはこの形の名前で `keep` 以外のものだけ
/// （以前の `<path><suffix>`・設定に明示した `.cwasm`・Pulley とネイティブのもう一方は残す）
// 理由付き allow: AOT キャッシュの片付けは設定適用時（起動/リロード）のコールドパス。
#[allow(clippy::disallowed_methods)]
fn remove_stale_aot_caches(wasm_path: &Path, keep: &Path, suffix: &str) {
    let (Some(dir), Some(wasm_name)) = (
        wasm_path.parent(),
        wasm_path.file_name().and_then(|n| n.to_str()),
    ) else {
        return;
    };
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_aot_cache_name(name, wasm_name, suffix) || Some(name.as_ref()) == keep.file_name() {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => ftlog::debug!("WASM AOT cache removed (stale): {}", entry.path().display()),
            Err(e) => ftlog::debug!(
                "WASM AOT cache not removed ({}): {}",
                e,
                entry.path().display()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b407_aot_cache_name_pattern() {
        let h = "0123456789abcdef0123456789abcdef";
        let n = |s: &str| format!("f.wasm.{s}");
        assert!(is_aot_cache_name(
            &n(&format!("{h}.cwasm")),
            "f.wasm",
            ".cwasm"
        ));
        assert!(is_aot_cache_name(
            &n(&format!("{h}.pulley.cwasm")),
            "f.wasm",
            ".pulley.cwasm"
        ));
        // 以前の名前・もう一方の種類・別のモジュール・形の違うものは消さない
        assert!(!is_aot_cache_name("f.wasm.cwasm", "f.wasm", ".cwasm"));
        assert!(!is_aot_cache_name(
            &n(&format!("{h}.pulley.cwasm")),
            "f.wasm",
            ".cwasm"
        ));
        assert!(!is_aot_cache_name(
            &n(&format!("{h}.cwasm")),
            "f.wasm",
            ".pulley.cwasm"
        ));
        assert!(!is_aot_cache_name(
            &format!("g.wasm.{h}.cwasm"),
            "f.wasm",
            ".cwasm"
        ));
        assert!(!is_aot_cache_name(
            &n(&format!("{}.cwasm", h.to_uppercase())),
            "f.wasm",
            ".cwasm"
        ));
        assert!(!is_aot_cache_name(
            &n(&format!("{h}0.cwasm")),
            "f.wasm",
            ".cwasm"
        ));
        assert!(!is_aot_cache_name(&n(".cwasm"), "f.wasm", ".cwasm"));
    }

    /// B-407: 古い版の `.wasm` を mtime ごと戻しても（`cp -p`・tar）、新しい版の AOT キャッシュを読まない。
    /// 古い版のキャッシュは新しい版を書いたときに消え、以前の名前（`<path>.cwasm`）は触らない
    #[test]
    // 理由付き allow: テストのフィクスチャ作成（ホットパス外）。
    #[allow(clippy::disallowed_methods)]
    fn b407_restoring_an_older_module_does_not_load_the_newer_cache() {
        let dir = std::env::temp_dir().join(format!("veil-b407-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.wasm");
        let legacy = dir.join("m.wasm.cwasm");
        std::fs::write(&legacy, b"legacy").unwrap();
        let engine = Engine::default();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        let put = |text: &str, mtime: Option<std::time::SystemTime>| {
            std::fs::write(&path, text).unwrap();
            if let Some(t) = mtime {
                std::fs::File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(t)
                    .unwrap();
            }
        };
        let load = || ModuleRegistry::load_or_compile_with_cache(&engine, &path, ".cwasm").unwrap();
        let caches = || {
            let mut v: Vec<String> = std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .filter(|n| is_aot_cache_name(n, "m.wasm", ".cwasm"))
                .collect();
            v.sort();
            v
        };
        let v1 = r#"(module (func (export "v1")))"#;
        let v2 = r#"(module (func (export "v2")))"#;

        put(v1, Some(old));
        assert!(load().get_export("v1").is_some());
        let first = caches();
        assert_eq!(first.len(), 1);
        // 2 回目はキャッシュから（同じ中身）
        assert!(load().get_export("v1").is_some());
        assert_eq!(caches(), first);

        put(v2, None);
        assert!(load().get_export("v2").is_some());
        let second = caches();
        assert_eq!(second.len(), 1, "the cache of the older version is removed");
        assert_ne!(second, first);

        // 古い版を古い mtime ごと戻す（新しい版のキャッシュより古い）。以前はそのキャッシュを読んでいた
        put(v1, Some(old));
        let m = load();
        assert!(
            m.get_export("v1").is_some(),
            "loaded the newer module's cache"
        );
        assert!(m.get_export("v2").is_none());
        assert_eq!(caches(), first);
        assert_eq!(std::fs::read(&legacy).unwrap(), b"legacy");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
