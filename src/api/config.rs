//! ライブラリモードの設定（F-188）: 既定設定・config.toml の取り込み・Rust コードからの上書き。
//!
//! 内部は `toml::Value`。起動時（と SIGHUP リロードのたび）に、設定ロードの単一チョークポイント
//! （`config_override::apply_to_toml_str`）に置いたフックが次の順で値木を組み立てる:
//!
//! 1. ライブラリ既定（`default_config.toml` + `server.threads = CPU 数`）
//! 2. config.toml（あれば。1 の上に深くマージ）
//! 3. 環境変数 `VEIL_OVERRIDE_<n>`（本体と同じ規則）
//! 4. `ConfigBuilder` の上書き（`set` や `listen` 等）
//! 5. ハンドラルート・組み込みルート（OpenAPI 等）の注入、具体性による並べ替えと衝突検査
//!
//! 上書きはリロード後も保たれる（フックが毎回同じものを当てる）。

use crate::config_override::ConfigOverride;
use std::path::{Path, PathBuf};

/// ライブラリ既定設定（TOML）。
pub const DEFAULT_CONFIG_TOML: &str = include_str!("default_config.toml");

/// 設定の読み込み・上書きのエラー。
#[derive(Debug, Clone)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// ライブラリモードの設定（prelude では `Config`）。
#[derive(Debug, Clone)]
pub struct Config {
    /// 読み込んだ config.toml のパス（SIGHUP リロードで読み直す）。無ければ `None`。
    pub(crate) file: Option<PathBuf>,
    /// `from_toml_str` で与えた TOML（ファイルの代わり）。
    pub(crate) inline: Option<toml::Value>,
    /// ビルダの上書き（後勝ち）。
    pub(crate) overrides: Vec<ConfigOverride>,
}

impl Default for Config {
    fn default() -> Self {
        Config::new()
    }
}

impl Config {
    /// ライブラリ既定設定だけ（config.toml を読まない）。
    pub fn new() -> Config {
        Config {
            file: None,
            inline: None,
            overrides: Vec::new(),
        }
    }

    /// カレントディレクトリの `config.toml` があれば取り込み、無ければ既定設定。
    pub fn load() -> Result<Config, ConfigError> {
        let p = Path::new("config.toml");
        if p.exists() {
            Config::from_file(p)
        } else {
            Ok(Config::new())
        }
    }

    /// config.toml を取り込む（既定設定の上に深くマージ）。ファイルが無い・構文エラーはエラー。
    /// SIGHUP でこのファイルを読み直す。
    // 理由付き allow: 起動前の設定読み込み（データプレーン外）。
    #[allow(clippy::disallowed_methods)]
    pub fn from_file(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError(format!("failed to read {}: {}", path.display(), e)))?;
        toml::from_str::<toml::Value>(&text)
            .map_err(|e| ConfigError(format!("{}: {}", path.display(), e)))?;
        Ok(Config {
            file: Some(path.to_path_buf()),
            inline: None,
            overrides: Vec::new(),
        })
    }

    /// TOML 文字列を取り込む（ファイルの代わり。既定設定の上に深くマージ）。
    pub fn from_toml_str(text: &str) -> Result<Config, ConfigError> {
        let v: toml::Value =
            toml::from_str(text).map_err(|e| ConfigError(format!("TOML parse error: {}", e)))?;
        Ok(Config {
            file: None,
            inline: Some(v),
            overrides: Vec::new(),
        })
    }

    /// ビルダへ（設計書の `Config::from_file(..)?.builder().listen(..).build()` の形）。
    pub fn builder(self) -> ConfigBuilder {
        ConfigBuilder { config: self }
    }

    /// 任意のキーを上書きする（`--override` と同じ構文: `"server.threads = 4"`）。
    pub fn set_override(&mut self, expr: &str) -> Result<(), ConfigError> {
        let ov = ConfigOverride::parse(expr).map_err(ConfigError)?;
        self.overrides.push(ov);
        Ok(())
    }

    /// パスと TOML 値で上書きする。
    pub fn set_value(&mut self, path: &str, value: toml::Value) -> Result<(), ConfigError> {
        let ov = ConfigOverride::from_parts(path, value).map_err(ConfigError)?;
        self.overrides.push(ov);
        Ok(())
    }

    /// 取り込んだ config.toml のパス。
    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    /// ライブラリ既定 + ファイル/インライン + ビルダ上書きを合成した値木（ルート注入前）。
    /// 環境変数の上書きは含まない（起動時に本体と同じ経路で当たる）。
    pub fn to_value(&self) -> Result<toml::Value, ConfigError> {
        let mut root = toml::Value::Table(toml::Table::new());
        if let Some(path) = &self.file {
            // 理由付き allow: 設定の確認用（起動前、データプレーン外）。
            #[allow(clippy::disallowed_methods)]
            let text = std::fs::read_to_string(path)
                .map_err(|e| ConfigError(format!("failed to read {}: {}", path.display(), e)))?;
            root = toml::from_str(&text)
                .map_err(|e| ConfigError(format!("{}: {}", path.display(), e)))?;
        }
        compose(&mut root, self.inline.as_ref(), &self.overrides).map_err(ConfigError)?;
        Ok(root)
    }

    /// 合成結果を TOML 文字列で返す（デバッグ・`-t` 相当の確認用）。
    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        let v = self.to_value()?;
        toml::to_string_pretty(&v).map_err(|e| ConfigError(e.to_string()))
    }
}

/// `Config` の上書きビルダ。各メソッドは `--override` 1 件に相当する。
#[derive(Debug, Clone)]
pub struct ConfigBuilder {
    config: Config,
}

macro_rules! setter {
    ($(#[$doc:meta])* $name:ident, $path:expr, $v:ident: $ty:ty => $value:expr) => {
        $(#[$doc])*
        pub fn $name(self, $v: $ty) -> Self {
            self.push($path, $value)
        }
    };
}

impl ConfigBuilder {
    fn push(mut self, path: &str, value: toml::Value) -> Self {
        self.config
            .overrides
            .push(ConfigOverride::from_parts(path, value).expect("static override path"));
        self
    }

    setter!(
        /// `[server].listen`（例: `"0.0.0.0:8080"`）。
        listen, "server.listen", v: &str => toml::Value::String(v.to_string())
    );
    setter!(
        /// `[server].threads`（ワーカースレッド数。既定は CPU 数）。
        threads, "server.threads", v: usize => toml::Value::Integer(v as i64)
    );
    setter!(
        /// `[server].tls_only`。
        tls_only, "server.tls_only", v: bool => toml::Value::Boolean(v)
    );
    setter!(
        /// `[server].h2c_enabled`。
        h2c_enabled, "server.h2c_enabled", v: bool => toml::Value::Boolean(v)
    );
    setter!(
        /// `[server].http2_enabled`（TLS の ALPN で h2 を選べるようにする）。
        http2_enabled, "server.http2_enabled", v: bool => toml::Value::Boolean(v)
    );
    setter!(
        /// `[logging].level`（`"error"`・`"warn"`・`"info"`・`"debug"`）。
        log_level, "logging.level", v: &str => toml::Value::String(v.to_string())
    );
    setter!(
        /// `[api].openapi_enabled`（`/openapi.json` と Swagger UI を公開するか）。
        openapi_enabled, "api.openapi_enabled", v: bool => toml::Value::Boolean(v)
    );
    setter!(
        /// `[api].openapi_path`（Swagger UI のパス、既定 `/docs`）。
        openapi_path, "api.openapi_path", v: &str => toml::Value::String(v.to_string())
    );
    setter!(
        /// `[api].openapi_json_path`（OpenAPI JSON のパス、既定 `/openapi.json`）。
        openapi_json_path, "api.openapi_json_path", v: &str => toml::Value::String(v.to_string())
    );
    setter!(
        /// `[api].openapi_title`。
        openapi_title, "api.openapi_title", v: &str => toml::Value::String(v.to_string())
    );
    setter!(
        /// `[api].openapi_version`（API 自体のバージョン）。
        openapi_version, "api.openapi_version", v: &str => toml::Value::String(v.to_string())
    );
    setter!(
        /// `[api].max_body_size`（ハンドラが受け取る本文の上限バイト数）。
        max_body_size, "api.max_body_size", v: usize => toml::Value::Integer(v as i64)
    );
    setter!(
        /// `[api].handler_threads`（ブロッキングなハンドラ・Python・C の実行プールのスレッド数）。
        handler_threads, "api.handler_threads", v: usize => toml::Value::Integer(v as i64)
    );

    /// TLS を有効にする（`[tls]` を入れ、TLS リスナーで HTTP/1.1・h2（ALPN）を受ける）。
    /// 平文の h2c 専用構成は解除する（`h2c_enabled = false`）。
    pub fn tls(mut self, cert_path: &str, key_path: &str) -> Self {
        for (p, v) in [
            ("tls.cert_path", toml::Value::String(cert_path.to_string())),
            ("tls.key_path", toml::Value::String(key_path.to_string())),
            ("server.h2c_enabled", toml::Value::Boolean(false)),
            ("server.h2c_http1", toml::Value::Boolean(false)),
        ] {
            self.config
                .overrides
                .push(ConfigOverride::from_parts(p, v).expect("static override path"));
        }
        self
    }

    /// HTTP/3 を有効にする（要 TLS・`http3` feature）。
    pub fn http3(mut self, listen: &str) -> Self {
        for (p, v) in [
            ("server.http3_enabled", toml::Value::Boolean(true)),
            ("http3.listen", toml::Value::String(listen.to_string())),
        ] {
            self.config
                .overrides
                .push(ConfigOverride::from_parts(p, v).expect("static override path"));
        }
        self
    }

    /// 任意のキーを上書きする（`--override` と同じ構文）。構文エラーは panic せず `Err`。
    pub fn set(mut self, expr: &str) -> Result<Self, ConfigError> {
        self.config.set_override(expr)?;
        Ok(self)
    }

    /// 任意のキーを値で上書きする。
    pub fn set_value(
        mut self,
        path: &str,
        value: impl Into<toml::Value>,
    ) -> Result<Self, ConfigError> {
        self.config.set_value(path, value.into())?;
        Ok(self)
    }

    /// `Config` に戻す。
    pub fn build(self) -> Config {
        self.config
    }
}

/// ライブラリ既定設定の値木（`server.threads` に CPU 数を入れる）。
pub fn default_value() -> toml::Value {
    let mut v: toml::Value = toml::from_str(DEFAULT_CONFIG_TOML).expect("default_config.toml");
    if let Some(server) = v.get_mut("server").and_then(|s| s.as_table_mut()) {
        server.insert(
            "threads".into(),
            toml::Value::Integer(num_cpus::get().max(1) as i64),
        );
    }
    v
}

/// `base` を `over` で上書きする深いマージ（テーブルは再帰、それ以外は置き換え）。
pub fn deep_merge(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

/// フックの前半: `root`（ファイル + 環境変数 + CLI の上書き済み）を
/// 「既定 < root < インライン < ビルダ上書き」に組み替える。
pub(crate) fn compose(
    root: &mut toml::Value,
    inline: Option<&toml::Value>,
    overrides: &[ConfigOverride],
) -> Result<(), String> {
    let file_root = std::mem::replace(root, default_value());
    deep_merge(root, file_root);
    if let Some(inline) = inline {
        deep_merge(root, inline.clone());
    }
    for ov in overrides {
        ov.apply(root)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_plaintext_on_8080_with_cpu_threads() {
        let v = Config::new().to_value().unwrap();
        assert_eq!(v["server"]["listen"].as_str(), Some("0.0.0.0:8080"));
        assert_eq!(v["server"]["h2c_enabled"].as_bool(), Some(true));
        assert_eq!(v["server"]["h2c_http1"].as_bool(), Some(true));
        assert_eq!(
            v["server"]["threads"].as_integer(),
            Some(num_cpus::get() as i64)
        );
        assert!(v.get("tls").is_none());
    }

    #[test]
    fn inline_merges_over_defaults_and_builder_wins() {
        let cfg = Config::from_toml_str(
            "[server]\nlisten = \"127.0.0.1:9000\"\n[logging]\nlevel = \"warn\"\n",
        )
        .unwrap()
        .builder()
        .threads(3)
        .listen("127.0.0.1:9100")
        .set("api.openapi_enabled = false")
        .unwrap()
        .build();
        let v = cfg.to_value().unwrap();
        assert_eq!(v["server"]["listen"].as_str(), Some("127.0.0.1:9100"));
        assert_eq!(v["server"]["threads"].as_integer(), Some(3));
        // インラインで書かなかったキーは既定が残る
        assert_eq!(v["server"]["h2c_http1"].as_bool(), Some(true));
        assert_eq!(v["logging"]["level"].as_str(), Some("warn"));
        assert_eq!(v["api"]["openapi_enabled"].as_bool(), Some(false));
    }

    #[test]
    fn tls_builder_switches_off_h2c_only_mode() {
        let v = Config::new()
            .builder()
            .tls("/c.pem", "/k.pem")
            .build()
            .to_value()
            .unwrap();
        assert_eq!(v["tls"]["cert_path"].as_str(), Some("/c.pem"));
        assert_eq!(v["server"]["h2c_enabled"].as_bool(), Some(false));
    }

    #[test]
    fn bad_override_is_an_error() {
        assert!(Config::new().builder().set("server.threads").is_err());
    }
}
