// F-188: ライブラリモードの設定。
//! Library-mode configuration: the default settings, importing `config.toml`, and overrides from Rust code.
//!
//! Internally the configuration is a `toml::Value`. At startup (and on every SIGHUP reload), a hook placed at
//! the single choke point of configuration loading (`config_override::apply_to_toml_str`) builds the value
//! tree in this order:
//!
//! 1. The library defaults (`default_config.toml` plus `server.threads = number of CPUs`).
//! 2. `config.toml`, if present, deep-merged on top of 1. The `VEIL_OVERRIDE_<n>` environment variables
//!    (same rules as the main binary) and the VDS overlay are applied to the contents of this file before it is merged.
//! 3. The inline TOML given to `Config::from_toml_str`.
//! 4. The overrides of `ConfigBuilder` (such as `set` and `listen`).
//! 5. Injection of the handler routes and built-in routes (such as OpenAPI), reordering by specificity, and conflict checks.
//!
//! The precedence is therefore defaults < file < VDS < environment variables < inline < builder.
//! Overrides survive a reload because the hook applies the same ones each time.

use crate::config_override::ConfigOverride;
use std::path::{Path, PathBuf};

/// The library default configuration (TOML).
pub const DEFAULT_CONFIG_TOML: &str = include_str!("default_config.toml");

/// Errors from loading or overriding the configuration.
#[derive(Debug, Clone)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// Library-mode configuration (`Config` in the prelude).
#[derive(Debug, Clone)]
pub struct Config {
    /// Path of the loaded `config.toml` (re-read on SIGHUP reload), or `None` if there is none.
    pub(crate) file: Option<PathBuf>,
    /// TOML given to `from_toml_str` (in place of a file).
    pub(crate) inline: Option<toml::Value>,
    /// Builder overrides (later ones win).
    pub(crate) overrides: Vec<ConfigOverride>,
    // F-260: `add_listener` で足した `[[listener]]`（`overrides` の `listener_override` 番目に配列ごと入れる）
    listeners: Vec<toml::Value>,
    listener_override: Option<usize>,
}

impl Default for Config {
    fn default() -> Self {
        Config::new()
    }
}

impl Config {
    /// The library defaults only (no `config.toml` is read).
    pub fn new() -> Config {
        Config {
            file: None,
            inline: None,
            overrides: Vec::new(),
            listeners: Vec::new(),
            listener_override: None,
        }
    }

    /// Imports `config.toml` from the current directory if present, and uses the defaults otherwise.
    pub fn load() -> Result<Config, ConfigError> {
        let p = Path::new("config.toml");
        if p.exists() {
            Config::from_file(p)
        } else {
            Ok(Config::new())
        }
    }

    /// Imports a `config.toml`, deep-merged over the defaults. A missing file or a syntax error is an error.
    /// The file is re-read on SIGHUP.
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
            ..Config::new()
        })
    }

    /// Imports a TOML string in place of a file, deep-merged over the defaults.
    pub fn from_toml_str(text: &str) -> Result<Config, ConfigError> {
        let v: toml::Value =
            toml::from_str(text).map_err(|e| ConfigError(format!("TOML parse error: {}", e)))?;
        Ok(Config {
            inline: Some(v),
            ..Config::new()
        })
    }

    /// Converts into the builder (`Config::from_file(..)?.builder().listen(..).build()`).
    pub fn builder(self) -> ConfigBuilder {
        ConfigBuilder { config: self }
    }

    /// Overrides an arbitrary key, using the same syntax as `--override` (for example `"server.threads = 4"`).
    pub fn set_override(&mut self, expr: &str) -> Result<(), ConfigError> {
        let ov = ConfigOverride::parse(expr).map_err(ConfigError)?;
        self.overrides.push(ov);
        Ok(())
    }

    /// Overrides a key given by path with a TOML value.
    pub fn set_value(&mut self, path: &str, value: toml::Value) -> Result<(), ConfigError> {
        let ov = ConfigOverride::from_parts(path, value).map_err(ConfigError)?;
        self.overrides.push(ov);
        Ok(())
    }

    // F-260: `[[listener]]` は配列なので、足すたびに配列全体を 1 個の上書きとして置き直す（`deep_merge` は配列を
    // 丸ごと置き換えるため、1 個ずつの上書きでは前の分が消える）。
    /// Adds a `[[listener]]` entry. Listeners added this way replace any `[[listener]]` array of the file or the
    /// inline TOML (arrays are not merged). When at least one listener is configured, the library default
    /// `[server].listen_cleartext = "127.0.0.1:8080"` is not opened unless it is set explicitly.
    pub fn add_listener(&mut self, listener: Listener) {
        self.listeners.push(listener.to_value());
        let ov = ConfigOverride::from_parts("listener", toml::Value::Array(self.listeners.clone()))
            .expect("static override path");
        match self.listener_override {
            Some(i) => self.overrides[i] = ov,
            None => {
                self.listener_override = Some(self.overrides.len());
                self.overrides.push(ov);
            }
        }
    }

    /// Path of the imported `config.toml`.
    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    /// The value tree composed from the library defaults, the file or inline TOML, and the builder overrides (before route injection).
    /// Environment-variable overrides are not included; they are applied at startup through the same path as the main binary.
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

    /// Returns the composed result as a TOML string (for debugging and for checks equivalent to `-t`).
    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        let v = self.to_value()?;
        toml::to_string_pretty(&v).map_err(|e| ConfigError(e.to_string()))
    }
}

// F-260: 待ち受け 1 個（`[[listener]]`）のビルダ。キーは設定ファイルの `[[listener]]` と同じ。
/// One listener (`[[listener]]`), for [`ConfigBuilder::listener`] and [`Config::add_listener`].
///
/// ```
/// use veil::api::config::{Config, Listener};
/// let config = Config::new()
///     .builder()
///     .tls("cert.pem", "key.pem")
///     .listener(Listener::tls("0.0.0.0:8443").http3(true))
///     .listener(Listener::tls("0.0.0.0:8081").http2(false))
///     .listener(Listener::cleartext("0.0.0.0:8080"))
///     .listener(Listener::cleartext("0.0.0.0:8082").allow_http(false))
///     .build();
/// # let _ = config;
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Listener {
    table: toml::Table,
}

impl Listener {
    fn with(listen: &str, tls: bool) -> Listener {
        let mut table = toml::Table::new();
        table.insert("listen".into(), toml::Value::String(listen.to_string()));
        table.insert("tls".into(), toml::Value::Boolean(tls));
        Listener { table }
    }

    fn set(mut self, key: &str, value: toml::Value) -> Listener {
        self.table.insert(key.into(), value);
        self
    }

    /// A TLS listener (`tls = true`; the certificate is `[tls]`, see [`ConfigBuilder::tls`]). `listen` is
    /// `host:port` or `unix:<path>`.
    pub fn tls(listen: &str) -> Listener {
        Listener::with(listen, true)
    }

    /// A plaintext listener (`tls = false`) for HTTP/1.1 and h2c.
    pub fn cleartext(listen: &str) -> Listener {
        Listener::with(listen, false)
    }

    /// `name`: used in logs, errors and `redirect_to` (default `listener-<index>`).
    pub fn name(self, name: &str) -> Listener {
        self.set("name", toml::Value::String(name.to_string()))
    }

    /// `http2` (TLS only): whether ALPN offers h2 (default `[server].http2_enabled`).
    pub fn http2(self, on: bool) -> Listener {
        self.set("http2", toml::Value::Boolean(on))
    }

    /// `http3` (TLS only): also serve HTTP/3 on the same port over UDP (requires the `http3` feature).
    pub fn http3(self, on: bool) -> Listener {
        self.set("http3", toml::Value::Boolean(on))
    }

    /// `http3_listen`: the UDP address of HTTP/3 when it differs from `listen` (implies `http3(true)`).
    pub fn http3_listen(self, addr: &str) -> Listener {
        self.http3(true)
            .set("http3_listen", toml::Value::String(addr.to_string()))
    }

    /// `allow_http` (plaintext only): accept HTTP/1.1 (default true).
    pub fn allow_http(self, on: bool) -> Listener {
        self.set("allow_http", toml::Value::Boolean(on))
    }

    /// `allow_h2c` (plaintext only): accept h2c (default true).
    pub fn allow_h2c(self, on: bool) -> Listener {
        self.set("allow_h2c", toml::Value::Boolean(on))
    }

    /// `secure_redirect` (plaintext only): answer every request with a 301 redirect to https (default false).
    pub fn secure_redirect(self, on: bool) -> Listener {
        self.set("secure_redirect", toml::Value::Boolean(on))
    }

    /// `redirect_to` (plaintext only): the TLS listener whose port the redirect uses (implies `secure_redirect(true)`).
    pub fn redirect_to(self, listener_name: &str) -> Listener {
        self.secure_redirect(true).set(
            "redirect_to",
            toml::Value::String(listener_name.to_string()),
        )
    }

    /// `threads`: the worker threads of this listener (default `[server].threads`).
    pub fn threads(self, n: usize) -> Listener {
        self.set("threads", toml::Value::Integer(n as i64))
    }

    /// `unix_socket_permissions`: the file mode of a `unix:<path>` socket (default `[server].unix_socket_permissions`).
    pub fn unix_socket_permissions(self, mode: &str) -> Listener {
        self.set(
            "unix_socket_permissions",
            toml::Value::String(mode.to_string()),
        )
    }

    /// The `[[listener]]` table.
    pub fn to_value(&self) -> toml::Value {
        toml::Value::Table(self.table.clone())
    }
}

/// Override builder for [`Config`]. Each method corresponds to one `--override`.
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
        /// `[server].listen_cleartext`: the listener for cleartext HTTP/1.1 and h2c (the library default is `"127.0.0.1:8080"`).
        listen_cleartext, "server.listen_cleartext", v: &str => toml::Value::String(v.to_string())
    );
    setter!(
        // F-200
        /// `[server].listen`: the TLS listener. Use it together with `tls(cert, key)`.
        listen, "server.listen", v: &str => toml::Value::String(v.to_string())
    );
    setter!(
        /// `[server].allow_http`: whether the cleartext listener accepts HTTP/1.1.
        allow_http, "server.allow_http", v: bool => toml::Value::Boolean(v)
    );
    setter!(
        /// `[server].allow_h2c`: whether the cleartext listener accepts h2c.
        allow_h2c, "server.allow_h2c", v: bool => toml::Value::Boolean(v)
    );
    setter!(
        /// `[server].enable_secure_redirect`: whether requests on the cleartext listener are answered with a 301 redirect to https.
        secure_redirect, "server.enable_secure_redirect", v: bool => toml::Value::Boolean(v)
    );
    setter!(
        /// `[server].threads`: the number of worker threads (defaults to the number of CPUs).
        threads, "server.threads", v: usize => toml::Value::Integer(v as i64)
    );
    setter!(
        /// `[server].http2_enabled`: allows h2 to be selected through TLS ALPN.
        http2_enabled, "server.http2_enabled", v: bool => toml::Value::Boolean(v)
    );
    setter!(
        /// `[logging].level` (`"error"`, `"warn"`, `"info"` or `"debug"`).
        log_level, "logging.level", v: &str => toml::Value::String(v.to_string())
    );
    setter!(
        /// `[api].openapi_enabled`: whether to serve `/openapi.json` and Swagger UI.
        openapi_enabled, "api.openapi_enabled", v: bool => toml::Value::Boolean(v)
    );
    setter!(
        /// `[api].openapi_path`: the path of Swagger UI (default `/docs`).
        openapi_path, "api.openapi_path", v: &str => toml::Value::String(v.to_string())
    );
    setter!(
        /// `[api].openapi_json_path`: the path of the OpenAPI JSON (default `/openapi.json`).
        openapi_json_path, "api.openapi_json_path", v: &str => toml::Value::String(v.to_string())
    );
    setter!(
        /// `[api].openapi_title`。
        openapi_title, "api.openapi_title", v: &str => toml::Value::String(v.to_string())
    );
    setter!(
        /// `[api].openapi_version`: the version of the API itself.
        openapi_version, "api.openapi_version", v: &str => toml::Value::String(v.to_string())
    );
    setter!(
        /// `[api].max_body_size`: the maximum body size, in bytes, that a handler accepts.
        max_body_size, "api.max_body_size", v: usize => toml::Value::Integer(v as i64)
    );
    setter!(
        /// `[api].handler_threads`: the number of threads in the pool that runs blocking, Python and C handlers.
        handler_threads, "api.handler_threads", v: usize => toml::Value::Integer(v as i64)
    );
    setter!(
        // F-197
        /// `[api].grpc_max_message_size`: the maximum size of a received gRPC message (default 4 MiB).
        grpc_max_message_size, "api.grpc_max_message_size", v: usize => toml::Value::Integer(v as i64)
    );
    setter!(
        // F-209
        /// `[api].grpc_max_send_message_size`: the maximum size of a sent gRPC message (0 = unlimited).
        grpc_max_send_message_size, "api.grpc_max_send_message_size", v: usize => toml::Value::Integer(v as i64)
    );
    setter!(
        // F-209
        /// `[api].grpc_response_compression`: compression of gRPC responses (`"none"`, `"gzip"` or `"deflate"`; default `"none"`).
        grpc_response_compression, "api.grpc_response_compression", v: &str => toml::Value::String(v.to_string())
    );
    setter!(
        // F-209
        /// `[api].grpc_response_compression_min_size`: messages smaller than this are not compressed (default 1024).
        grpc_response_compression_min_size, "api.grpc_response_compression_min_size", v: usize => toml::Value::Integer(v as i64)
    );

    // F-209
    /// `[api].grpc_web_cors_origins`: the origins allowed by gRPC-Web CORS (`"*"` allows all).
    pub fn grpc_web_cors_origins(self, origins: &[&str]) -> Self {
        self.push(
            "api.grpc_web_cors_origins",
            toml::Value::Array(
                origins
                    .iter()
                    .map(|o| toml::Value::String(o.to_string()))
                    .collect(),
            ),
        )
    }

    // F-200: TLS のリスナーは `listen(addr)` で別に指定する。
    /// The TLS certificate and private key (`[tls]`). Specify the TLS listener separately with `listen(addr)`.
    pub fn tls(self, cert_path: &str, key_path: &str) -> Self {
        self.push("tls.cert_path", toml::Value::String(cert_path.to_string()))
            .push("tls.key_path", toml::Value::String(key_path.to_string()))
    }

    /// Enables HTTP/3 (requires `listen`, TLS and the `http3` feature). The argument is a UDP address.
    pub fn http3(mut self, listen: &str) -> Self {
        for (p, v) in [
            ("http3.enabled", toml::Value::Boolean(true)),
            ("http3.listen", toml::Value::String(listen.to_string())),
        ] {
            self.config
                .overrides
                .push(ConfigOverride::from_parts(p, v).expect("static override path"));
        }
        self
    }

    /// Adds a listener (`[[listener]]`); call it once per port. See [`Config::add_listener`].
    pub fn listener(mut self, listener: Listener) -> Self {
        self.config.add_listener(listener);
        self
    }

    /// Overrides an arbitrary key using the `--override` syntax. A syntax error returns `Err` instead of panicking.
    pub fn set(mut self, expr: &str) -> Result<Self, ConfigError> {
        self.config.set_override(expr)?;
        Ok(self)
    }

    /// Overrides an arbitrary key with a value.
    pub fn set_value(
        mut self,
        path: &str,
        value: impl Into<toml::Value>,
    ) -> Result<Self, ConfigError> {
        self.config.set_value(path, value.into())?;
        Ok(self)
    }

    /// Converts back into a [`Config`].
    pub fn build(self) -> Config {
        self.config
    }
}

/// The value tree of the library defaults (`server.threads` is set to the number of CPUs).
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

/// Deep-merges `over` into `base` (tables recursively, everything else is replaced).
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

/// First half of the hook: rearranges `root` (with the file, environment variables and CLI overrides already applied)
/// into "defaults < root < inline < builder overrides".
pub(crate) fn compose(
    root: &mut toml::Value,
    inline: Option<&toml::Value>,
    overrides: &[ConfigOverride],
) -> Result<(), String> {
    let file_root = std::mem::replace(root, default_value());
    // F-260: 既定の平文のリスナー（127.0.0.1:8080）は、`[[listener]]` を書いた人には開かない（明示した人には開く）
    let explicit_cleartext = has_listen_cleartext(&file_root)
        || inline.is_some_and(has_listen_cleartext)
        || overrides
            .iter()
            .any(|o| o.targets(&["server", "listen_cleartext"]));
    deep_merge(root, file_root);
    if let Some(inline) = inline {
        deep_merge(root, inline.clone());
    }
    for ov in overrides {
        ov.apply(root)?;
    }
    let has_listeners = root
        .get("listener")
        .and_then(|l| l.as_array())
        .is_some_and(|a| !a.is_empty());
    if has_listeners && !explicit_cleartext {
        if let Some(server) = root.get_mut("server").and_then(|s| s.as_table_mut()) {
            server.remove("listen_cleartext");
        }
    }
    Ok(())
}

fn has_listen_cleartext(v: &toml::Value) -> bool {
    v.get("server")
        .and_then(|s| s.get("listen_cleartext"))
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_plaintext_on_8080_with_cpu_threads() {
        let v = Config::new().to_value().unwrap();
        assert_eq!(
            v["server"]["listen_cleartext"].as_str(),
            Some("127.0.0.1:8080")
        );
        assert_eq!(v["server"]["enable_secure_redirect"].as_bool(), Some(false));
        assert!(v["server"].get("listen").is_none());
        assert_eq!(
            v["server"]["threads"].as_integer(),
            Some(num_cpus::get() as i64)
        );
        assert!(v.get("tls").is_none());
    }

    // F-244: ライブラリの既定はアクセスログを標準出力に text で出す。ファイル・ビルダで止められる。
    #[test]
    fn access_log_is_on_by_default_and_can_be_turned_off() {
        let v = Config::new().to_value().unwrap();
        assert_eq!(v["access_log"]["enabled"].as_bool(), Some(true));
        assert_eq!(v["access_log"]["format"].as_str(), Some("text"));
        assert!(v["access_log"].get("file_path").is_none());

        let v = Config::from_toml_str("[access_log]\nenabled = false\n")
            .unwrap()
            .to_value()
            .unwrap();
        assert_eq!(v["access_log"]["enabled"].as_bool(), Some(false));
        // ファイルで書かなかったキーは既定が残る
        assert_eq!(v["access_log"]["format"].as_str(), Some("text"));

        let v = Config::new()
            .builder()
            .set("access_log.enabled = false")
            .unwrap()
            .build()
            .to_value()
            .unwrap();
        assert_eq!(v["access_log"]["enabled"].as_bool(), Some(false));
    }

    #[test]
    fn inline_merges_over_defaults_and_builder_wins() {
        let cfg = Config::from_toml_str(
            "[server]\nlisten_cleartext = \"127.0.0.1:9000\"\n[logging]\nlevel = \"warn\"\n",
        )
        .unwrap()
        .builder()
        .threads(3)
        .listen_cleartext("127.0.0.1:9100")
        .set("api.openapi_enabled = false")
        .unwrap()
        .build();
        let v = cfg.to_value().unwrap();
        assert_eq!(
            v["server"]["listen_cleartext"].as_str(),
            Some("127.0.0.1:9100")
        );
        assert_eq!(v["server"]["threads"].as_integer(), Some(3));
        // インラインで書かなかったキーは既定が残る
        assert_eq!(v["server"]["enable_secure_redirect"].as_bool(), Some(false));
        assert_eq!(v["logging"]["level"].as_str(), Some("warn"));
        assert_eq!(v["api"]["openapi_enabled"].as_bool(), Some(false));
    }

    #[test]
    fn tls_builder_sets_listen_and_certificate() {
        let v = Config::new()
            .builder()
            .listen("0.0.0.0:8443")
            .tls("/c.pem", "/k.pem")
            .build()
            .to_value()
            .unwrap();
        assert_eq!(v["tls"]["cert_path"].as_str(), Some("/c.pem"));
        assert_eq!(v["server"]["listen"].as_str(), Some("0.0.0.0:8443"));
        // 平文のリスナーは既定のまま残る
        assert_eq!(
            v["server"]["listen_cleartext"].as_str(),
            Some("127.0.0.1:8080")
        );
    }

    // F-260
    #[test]
    fn listeners_replace_the_default_cleartext_listener() {
        let v = Config::new()
            .builder()
            .listener(Listener::tls("0.0.0.0:8443").http3(true).name("https"))
            .listener(Listener::cleartext("0.0.0.0:8082").allow_http(false))
            .build()
            .to_value()
            .unwrap();
        let ls = v["listener"].as_array().unwrap();
        assert_eq!(ls.len(), 2);
        assert_eq!(ls[0]["listen"].as_str(), Some("0.0.0.0:8443"));
        assert_eq!(ls[0]["tls"].as_bool(), Some(true));
        assert_eq!(ls[0]["http3"].as_bool(), Some(true));
        assert_eq!(ls[0]["name"].as_str(), Some("https"));
        assert_eq!(ls[1]["allow_http"].as_bool(), Some(false));
        // 既定の 127.0.0.1:8080 は開かない
        assert!(v["server"].get("listen_cleartext").is_none());

        // 明示した listen_cleartext は残る
        let v = Config::new()
            .builder()
            .listen_cleartext("127.0.0.1:9000")
            .listener(Listener::cleartext("127.0.0.1:9001"))
            .build()
            .to_value()
            .unwrap();
        assert_eq!(
            v["server"]["listen_cleartext"].as_str(),
            Some("127.0.0.1:9000")
        );
        // ファイル（インライン）の [[listener]] でも同じ
        let v = Config::from_toml_str("[[listener]]\nlisten = \"127.0.0.1:9002\"\ntls = false\n")
            .unwrap()
            .to_value()
            .unwrap();
        assert!(v["server"].get("listen_cleartext").is_none());
    }

    #[test]
    fn listener_builder_keys() {
        let l = Listener::cleartext("0.0.0.0:80")
            .redirect_to("https")
            .threads(2);
        let v = l.to_value();
        assert_eq!(v["secure_redirect"].as_bool(), Some(true));
        assert_eq!(v["redirect_to"].as_str(), Some("https"));
        assert_eq!(v["threads"].as_integer(), Some(2));
        let v = Listener::tls("0.0.0.0:443")
            .http3_listen("0.0.0.0:8443")
            .to_value();
        assert_eq!(v["http3"].as_bool(), Some(true));
        assert_eq!(v["http3_listen"].as_str(), Some("0.0.0.0:8443"));
    }

    #[test]
    fn bad_override_is_an_error() {
        assert!(Config::new().builder().set("server.threads").is_err());
    }
}
