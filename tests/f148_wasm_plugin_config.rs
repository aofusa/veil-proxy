//! F-148: Proxy-Wasm プラグイン設定の TOML 記述とルート単位の上書き
//!
//! `[[wasm.modules]]` の `configuration` を TOML テーブルで書いたときに正しく
//! JSON オブジェクトへ変換されること、`[route.module_configuration.<name>]` が
//! モジュール定義をディープマージで上書きすること、`module_configuration` が
//! `modules` に含まれない名前を参照した場合に設定エラーとなることを
//! `veil::config::load_config`（実際の設定ロード経路）を通して検証する。

#![cfg(feature = "wasm")]

use std::io::Write;

use veil::config::load_config;

const CERT_PEM: &str = "tests/fixtures/cert.pem";
const KEY_PEM: &str = "tests/fixtures/key.pem";
const HEADER_FILTER_WASM: &str = "tests/fixtures/wasm/header_filter.wasm";

/// 最小の server/tls セクション込みの設定本文を組み立てる。
fn base_config(extra: &str) -> String {
    format!(
        r#"
[server]
listen = "127.0.0.1:0"

[tls]
cert_path = "{CERT_PEM}"
key_path = "{KEY_PEM}"

{extra}
"#
    )
}

fn write_temp_config(body: &str) -> tempfile::NamedTempFile {
    let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
    tmp.write_all(body.as_bytes()).expect("write");
    tmp.flush().expect("flush");
    tmp
}

/// `[[wasm.modules]]` の `configuration` を TOML テーブルで書いた設定がパースでき、
/// JSON オブジェクトのバイト列（`resolved_modules` 経由）になること。
/// `toml::value::Table` は `preserve_order` 無効ビルドでは `BTreeMap` なので、
/// キー順はアルファベット順（`max_body_size` < `mode` < `patterns`）になる。
#[test]
fn wasm_module_table_configuration_becomes_json_bytes() {
    let extra = format!(
        r#"
[wasm]
enabled = true

[[wasm.modules]]
name = "header_filter"
path = "{HEADER_FILTER_WASM}"

[wasm.modules.configuration]
mode = "block"
max_body_size = 65536
patterns = ["union select", "<script"]

[[route]]
modules = ["header_filter"]
[route.conditions]
path = "/*"
[route.action]
type = "Redirect"
redirect_url = "https://example.com/"
redirect_status = 302
"#
    );
    let body = base_config(&extra);
    let tmp = write_temp_config(&body);

    let loaded = load_config(tmp.path()).expect("config should load");
    assert_eq!(loaded.route.len(), 1);
    let resolved = loaded.route[0]
        .resolved_modules
        .as_ref()
        .expect("resolved_modules must be populated at load time");
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].name, "header_filter");
    let bytes = resolved[0]
        .configuration
        .as_ref()
        .expect("module definition configuration should be resolved into bytes");
    let json = std::str::from_utf8(bytes).expect("valid utf8");
    assert_eq!(
        json,
        r#"{"max_body_size":65536,"mode":"block","patterns":["union select","<script"]}"#
    );
}

/// `[route.module_configuration.<name>]` がモジュール定義をディープマージで上書きする
/// こと（同名キーはルート優先、モジュール定義側のキーは保持される）。
#[test]
fn route_module_configuration_deep_merges_over_module_definition() {
    let extra = format!(
        r#"
[wasm]
enabled = true

[[wasm.modules]]
name = "header_filter"
path = "{HEADER_FILTER_WASM}"

[wasm.modules.configuration]
mode = "block"
max_body_size = 65536

[[route]]
modules = ["header_filter"]
[route.module_configuration.header_filter]
mode = "log_only"
[route.conditions]
path = "/*"
[route.action]
type = "Redirect"
redirect_url = "https://example.com/"
redirect_status = 302
"#
    );
    let body = base_config(&extra);
    let tmp = write_temp_config(&body);

    let loaded = load_config(tmp.path()).expect("config should load");
    let resolved = loaded.route[0].resolved_modules.as_ref().unwrap();
    let bytes = resolved[0].configuration.as_ref().unwrap();
    let json = std::str::from_utf8(bytes).expect("valid utf8");
    // ルート側の mode="log_only" が優先され、モジュール定義側の max_body_size は保持される。
    assert_eq!(json, r#"{"max_body_size":65536,"mode":"log_only"}"#);
}

/// `module_configuration` に `modules` 外の名前を書くと設定エラーになること。
#[test]
fn route_module_configuration_for_unknown_module_is_rejected() {
    let extra = format!(
        r#"
[wasm]
enabled = true

[[wasm.modules]]
name = "header_filter"
path = "{HEADER_FILTER_WASM}"

[[route]]
modules = ["header_filter"]
[route.module_configuration.not_listed]
mode = "log_only"
[route.conditions]
path = "/*"
[route.action]
type = "Redirect"
redirect_url = "https://example.com/"
redirect_status = 302
"#
    );
    let body = base_config(&extra);
    let tmp = write_temp_config(&body);

    let result = load_config(tmp.path());
    let err = match result {
        Ok(_) => panic!("must reject module_configuration for unlisted module"),
        Err(e) => e,
    };
    let msg = err.to_string();
    assert!(
        msg.contains("not_listed") && msg.contains("module_configuration"),
        "error message should mention the offending key: {msg}"
    );
}
