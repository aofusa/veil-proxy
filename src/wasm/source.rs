//! エンジンを作った元の `[wasm]` とファイルの印（B-404）。
//!
//! seccomp が実行可能なメモリの作成を拒否した後（`[security] seccomp_mode = "filter"`・`"strict"`）は、モジュールを
//! ネイティブのコードへコンパイルできない（`mprotect(PROT_EXEC)` が拒否され、`strict` ではプロセスごと落ちる）。
//! 設定のリロードは、今のエンジンが新しい設定と同じ `[wasm]`・同じファイルから作られていればそのエンジンを使い続け、
//! 違えば断る（`config::loaded::reload_wasm_engine`。`log` はコンパイルを通して警告する）。

use std::path::Path;

use super::types::WasmConfig;

/// ファイルの印（inode・大きさ・mtime・ctime）。ctime は中身を書き換えるたびにカーネルが進め、利用者は戻せない。
/// 同じ時刻の刻み（数 ms）の中で同じ大きさに書き換えた場合だけ見分けられない（起動の後に置き換えたファイルは見分けられる）
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    inode: (u64, u64),
    #[cfg(unix)]
    ctime: (i64, i64),
}

/// `path` の印。調べられなければ `None`（別物として扱う）
fn stamp(path: &Path) -> Option<FileStamp> {
    // 理由付き allow: 設定のロード（起動・リロード）のときだけ呼ぶコールドパス。中身は読まない（Landlock の
    // 読み取りの許可が無いファイルでも調べられる）
    #[allow(clippy::disallowed_methods)]
    let m = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Some(FileStamp {
        len: m.len(),
        modified: m.modified().ok(),
        #[cfg(unix)]
        inode: (m.dev(), m.ino()),
        #[cfg(unix)]
        ctime: (m.ctime(), m.ctime_nsec()),
    })
}

/// エンジンを作った元の `[wasm]` と、モジュール・`data_files` のファイルの印
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WasmSource {
    /// `[wasm]` を比べられる形にしたもの。書けなければ `None`（別物として扱う）
    section: Option<toml::Value>,
    /// （パス, 印）。順番は `[wasm]` で決まる（モジュールの順、各モジュールの `data_files` は名前の順）
    files: Vec<(String, Option<FileStamp>)>,
}

impl WasmSource {
    pub(crate) fn of(config: &WasmConfig) -> Self {
        let mut files = Vec::new();
        for m in &config.modules {
            files.push((m.path.clone(), stamp(Path::new(&m.path))));
            for f in m.data_files.values() {
                files.push((f.clone(), stamp(Path::new(f))));
            }
        }
        Self {
            section: toml::Value::try_from(config).ok(),
            files,
        }
    }

    /// `new` が自分と違うところ（同じなら `None`）
    pub(crate) fn change(&self, new: &WasmSource) -> Option<String> {
        if self.section.is_none() || self.section != new.section {
            return Some("the [wasm] section changed".to_string());
        }
        // `[wasm]` が同じなのでファイルの並びも同じ
        for ((path, old), (_, cur)) in self.files.iter().zip(&new.files) {
            if old.is_none() || old != cur {
                return Some(format!("{path:?} changed"));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasm::types::ModuleConfig;

    fn config(dir: &Path) -> WasmConfig {
        let mut module = ModuleConfig {
            name: "m".to_string(),
            path: dir.join("m.wasm").to_string_lossy().into_owned(),
            ..Default::default()
        };
        module.data_files.insert(
            "rules".to_string(),
            dir.join("rules.txt").to_string_lossy().into_owned(),
        );
        WasmConfig {
            enabled: true,
            defaults: crate::wasm::types::WasmDefaults::default(),
            modules: vec![module],
            interpreter: false,
            grpc_connect_timeout_ms: crate::wasm::DEFAULT_GRPC_CONNECT_TIMEOUT_MS,
            http_call_max_timeout_ms: crate::wasm::DEFAULT_HTTP_CALL_MAX_TIMEOUT_MS,
            shared_data_max_keys: crate::wasm::DEFAULT_SHARED_DATA_MAX_KEYS,
            metering: Default::default(),
        }
    }

    /// 同じ設定・同じファイルなら同じ。`[wasm]`・モジュール・`data_files` のどれかが変われば違う
    // 理由付き allow: 試験のファイルを用意・書き換える（データプレーンの外）
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn detects_section_module_and_data_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("m.wasm");
        let data = dir.path().join("rules.txt");
        std::fs::write(&module, b"\0asm-v1").unwrap();
        std::fs::write(&data, b"rule-1").unwrap();
        let cfg = config(dir.path());
        let start = WasmSource::of(&cfg);
        assert_eq!(start.change(&WasmSource::of(&cfg)), None);

        // `[wasm]` の値が違う
        let mut other = cfg.clone();
        other.modules[0].configuration =
            crate::wasm_plugin_config::PluginConfiguration::Raw("x".to_string());
        let why = start
            .change(&WasmSource::of(&other))
            .expect("section change");
        assert!(why.contains("[wasm]"), "{why}");

        // モジュールを置き換えた（別の inode）
        let tmp = dir.path().join("m.wasm.new");
        std::fs::write(&tmp, b"\0asm-v2").unwrap();
        std::fs::rename(&tmp, &module).unwrap();
        let why = start.change(&WasmSource::of(&cfg)).expect("module change");
        assert!(why.contains("m.wasm"), "{why}");

        // data_files をその場で書き換えた（大きさが変わる）
        let now = WasmSource::of(&cfg);
        std::fs::write(&data, b"rule-1\nrule-2").unwrap();
        let why = now.change(&WasmSource::of(&cfg)).expect("data file change");
        assert!(why.contains("rules.txt"), "{why}");

        // 消えたファイルも違う
        let now = WasmSource::of(&cfg);
        std::fs::remove_file(&data).unwrap();
        assert!(now.change(&WasmSource::of(&cfg)).is_some());
    }
}
