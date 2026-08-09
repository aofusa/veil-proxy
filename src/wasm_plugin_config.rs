//! Proxy-Wasm プラグイン設定（F-148）。
//!
//! `[[wasm.modules]]` の `configuration` は TOML では「文字列」（従来互換、多くは
//! JSON をそのまま渡す）または「テーブル」（JSON オブジェクトへ変換してから渡す）
//! のいずれかで書ける。ここではその型 [`PluginConfiguration`] と、
//! ルート単位の上書きを合成する [`merge_over`]、TOML → JSON への変換
//! ([`PluginConfiguration::to_bytes`]) を提供する。
//!
//! `feature = "wasm"` に依存せず常にコンパイルされる（`src/config.rs` が
//! `wasm` 無効ビルドでも `Route::module_configuration` の型として使うため）。
//!
//! 合成・TOML→JSON 変換は設定ロード時（起動・ホットリロード）のみ実行される
//! コールドパス専用（AGENTS.md のホットパス絶対規則）。`serde_json` は依存に
//! 追加せず、TOML→JSON エンコーダは本ファイルに自前実装する。

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Proxy-Wasm プラグイン設定値。TOML では「文字列」または「テーブル」で書ける。
///
/// untagged の順序は `Raw` を先に置く。これにより既存の
/// `configuration = '{"k":"v"}'`（文字列）は必ず `Raw` として解釈され、後方互換性が
/// 保たれる（`Table` を先に置くと serde が先にテーブルとして解釈を試みてしまう
/// ことはないが、意図を明示するため順序を固定する）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum PluginConfiguration {
    /// 従来互換: 文字列（多くは JSON）をそのままバイト列としてモジュールへ渡す
    Raw(String),
    /// TOML テーブル: JSON オブジェクトへ変換してから渡す
    Table(toml::value::Table),
}

impl Default for PluginConfiguration {
    fn default() -> Self {
        PluginConfiguration::Raw(String::new())
    }
}

/// 共有される空バイト列。`configuration` 未設定のモジュール・ルートで使うことで
/// 毎回のアロケーションを避ける。
static EMPTY_CONFIGURATION: Lazy<Arc<[u8]>> = Lazy::new(|| Arc::from(Vec::<u8>::new()));

/// 空設定の共有インスタンスを返す（`clone()` は `Arc` の参照カウント増加のみ）。
pub fn empty_configuration() -> Arc<[u8]> {
    EMPTY_CONFIGURATION.clone()
}

impl PluginConfiguration {
    /// この設定値を Proxy-Wasm モジュールへ渡すバイト列へ変換する。
    ///
    /// `Raw` はそのままバイト列化、`Table` は TOML → JSON エンコーダで JSON
    /// オブジェクトへ変換する。設定ロード時（コールドパス）専用。
    pub fn to_bytes(&self) -> Arc<[u8]> {
        match self {
            PluginConfiguration::Raw(s) => {
                if s.is_empty() {
                    empty_configuration()
                } else {
                    Arc::from(s.as_bytes())
                }
            }
            PluginConfiguration::Table(table) => {
                let mut out = String::new();
                encode_table_as_json(table, &mut out);
                Arc::from(out.into_bytes())
            }
        }
    }

    /// モジュール定義の設定（`base`）にルート単位の上書き（`over`）を合成する。
    ///
    /// 合成規則（ルート優先）:
    /// - どちらも `None` → `None`（呼び出し側で空バイト列扱いにする）
    /// - 片方だけ `Some` → その値
    /// - 両方 `Table` → ディープマージ（同名キーはルート優先、ネストしたテーブルは
    ///   再帰マージ、配列は置換）
    /// - それ以外の組み合わせ（片方でも `Raw`）→ `over` で完全置換
    pub fn merge_over(
        base: Option<&PluginConfiguration>,
        over: Option<&PluginConfiguration>,
    ) -> Option<PluginConfiguration> {
        match (base, over) {
            (None, None) => None,
            (Some(b), None) => Some(b.clone()),
            (None, Some(o)) => Some(o.clone()),
            (Some(PluginConfiguration::Table(b)), Some(PluginConfiguration::Table(o))) => {
                Some(PluginConfiguration::Table(deep_merge_table(b, o)))
            }
            (Some(_), Some(o)) => Some(o.clone()),
        }
    }
}

/// テーブルのディープマージ。`over` の値が `base` を上書きする。
/// ネストしたテーブルは再帰的にマージし、配列（およびその他の値）は `over` で置換する。
fn deep_merge_table(base: &toml::value::Table, over: &toml::value::Table) -> toml::value::Table {
    let mut result = base.clone();
    for (key, over_value) in over {
        match (result.get(key), over_value) {
            (Some(toml::Value::Table(base_nested)), toml::Value::Table(over_nested)) => {
                result.insert(
                    key.clone(),
                    toml::Value::Table(deep_merge_table(base_nested, over_nested)),
                );
            }
            _ => {
                result.insert(key.clone(), over_value.clone());
            }
        }
    }
    result
}

/// TOML テーブルを JSON オブジェクトとして `out` へエンコードする。
fn encode_table_as_json(table: &toml::value::Table, out: &mut String) {
    out.push('{');
    let mut first = true;
    for (key, value) in table {
        if !first {
            out.push(',');
        }
        first = false;
        encode_json_string(key, out);
        out.push(':');
        encode_value_as_json(value, out);
    }
    out.push('}');
}

/// TOML 値を JSON 値として `out` へエンコードする。
///
/// 変換規則: String→string, Integer/Float→number（Float の NaN/Inf は null）,
/// Boolean→bool, Datetime→RFC3339 相当の string, Array→array, Table→object。
fn encode_value_as_json(value: &toml::Value, out: &mut String) {
    match value {
        toml::Value::String(s) => encode_json_string(s, out),
        toml::Value::Integer(i) => out.push_str(&i.to_string()),
        toml::Value::Float(f) => {
            if f.is_finite() {
                out.push_str(&f.to_string());
            } else {
                out.push_str("null");
            }
        }
        toml::Value::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
        toml::Value::Datetime(dt) => encode_json_string(&dt.to_string(), out),
        toml::Value::Array(arr) => {
            out.push('[');
            for (i, v) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode_value_as_json(v, out);
            }
            out.push(']');
        }
        toml::Value::Table(t) => encode_table_as_json(t, out),
    }
}

/// 文字列を JSON 文字列リテラルとして `out` へエンコードする（クォート・エスケープ込み）。
fn encode_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// ルート / L4 リスナーに適用する WASM モジュールの参照。
#[derive(Debug, Clone)]
pub struct ModuleRef {
    pub name: String,
    /// ルート単位で解決済みの実効 plugin configuration。
    /// `None` の場合はモジュール定義側の設定をそのまま使う。
    pub configuration: Option<Arc<[u8]>>,
}

impl ModuleRef {
    /// モジュール名のみを持つ参照を作る（設定上書きなし）。
    pub fn from_name(name: &str) -> Self {
        Self {
            name: name.to_string(),
            configuration: None,
        }
    }

    /// 名前と解決済み設定から参照を作る。
    pub fn new(name: String, configuration: Option<Arc<[u8]>>) -> Self {
        Self {
            name,
            configuration,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_from_toml(s: &str) -> toml::value::Table {
        let value: toml::Value = toml::from_str(s).unwrap();
        match value {
            toml::Value::Table(t) => t,
            _ => panic!("not a table"),
        }
    }

    #[derive(Deserialize)]
    struct Wrapper {
        configuration: PluginConfiguration,
    }

    #[test]
    fn raw_is_used_for_plain_string() {
        // 既存の `configuration = '{"k":"v"}'` は Raw として解釈され続ける
        let wrapper: Wrapper = toml::from_str(r#"configuration = '{"k":"v"}'"#).unwrap();
        let cfg = wrapper.configuration;
        assert!(matches!(cfg, PluginConfiguration::Raw(_)));
        if let PluginConfiguration::Raw(s) = &cfg {
            assert_eq!(s, r#"{"k":"v"}"#);
        }
        assert_eq!(&*cfg.to_bytes(), br#"{"k":"v"}"#);
    }

    #[test]
    fn table_configuration_parses_from_toml_wrapper() {
        // `[wasm.modules.configuration]` 相当のテーブル記法が Table として解釈される
        let doc = r#"
[configuration]
mode = "block"
max_body_size = 65536
patterns = ["union select", "<script"]
"#;
        let wrapper: Wrapper = toml::from_str(doc).unwrap();
        assert!(matches!(
            wrapper.configuration,
            PluginConfiguration::Table(_)
        ));
    }

    #[test]
    fn table_is_parsed_as_table_variant() {
        let doc = r#"
mode = "block"
max_body_size = 65536
"#;
        let table = table_from_toml(doc);
        let cfg = PluginConfiguration::Table(table);
        assert!(matches!(cfg, PluginConfiguration::Table(_)));
    }

    #[test]
    fn to_bytes_encodes_string() {
        let table = table_from_toml(r#"name = "hello \"world\"\n""#);
        let cfg = PluginConfiguration::Table(table);
        let bytes = cfg.to_bytes();
        let json = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(json, r#"{"name":"hello \"world\"\n"}"#);
    }

    #[test]
    fn to_bytes_encodes_integer_float_bool() {
        let table = table_from_toml(
            r#"
count = 42
ratio = 1.5
enabled = true
disabled = false
"#,
        );
        let cfg = PluginConfiguration::Table(table);
        let bytes = cfg.to_bytes();
        let json = std::str::from_utf8(&bytes).unwrap();
        assert!(json.contains(r#""count":42"#));
        assert!(json.contains(r#""ratio":1.5"#));
        assert!(json.contains(r#""enabled":true"#));
        assert!(json.contains(r#""disabled":false"#));
    }

    #[test]
    fn to_bytes_encodes_array_and_nested_table() {
        let table = table_from_toml(
            r#"
patterns = ["union select", "<script"]
[nested]
inner = "value"
"#,
        );
        let cfg = PluginConfiguration::Table(table);
        let bytes = cfg.to_bytes();
        let json = std::str::from_utf8(&bytes).unwrap();
        assert!(json.contains(r#""patterns":["union select","<script"]"#));
        assert!(json.contains(r#""nested":{"inner":"value"}"#));
    }

    #[test]
    fn to_bytes_encodes_datetime_as_string() {
        let table = table_from_toml(r#"ts = 2024-01-01T00:00:00Z"#);
        let cfg = PluginConfiguration::Table(table);
        let bytes = cfg.to_bytes();
        let json = std::str::from_utf8(&bytes).unwrap();
        assert!(json.starts_with(r#"{"ts":""#));
        assert!(json.ends_with("\"}"));
    }

    #[test]
    fn escapes_control_characters_as_unicode_escape() {
        let mut out = String::new();
        encode_json_string("a\u{0001}b", &mut out);
        assert_eq!(out, "\"a\\u0001b\"");
    }

    #[test]
    fn deep_merge_merges_nested_tables_route_priority() {
        let base = table_from_toml(
            r#"
mode = "block"
max_body_size = 65536
[nested]
a = 1
b = 2
"#,
        );
        let over = table_from_toml(
            r#"
mode = "log_only"
[nested]
b = 20
c = 30
"#,
        );
        let merged = deep_merge_table(&base, &over);
        assert_eq!(
            merged.get("mode").unwrap().as_str(),
            Some("log_only"),
            "route value should win"
        );
        assert_eq!(
            merged.get("max_body_size").unwrap().as_integer(),
            Some(65536)
        );
        let nested = merged.get("nested").unwrap().as_table().unwrap();
        assert_eq!(nested.get("a").unwrap().as_integer(), Some(1));
        assert_eq!(nested.get("b").unwrap().as_integer(), Some(20));
        assert_eq!(nested.get("c").unwrap().as_integer(), Some(30));
    }

    #[test]
    fn deep_merge_replaces_arrays_instead_of_concatenating() {
        let base = table_from_toml(r#"patterns = ["a", "b"]"#);
        let over = table_from_toml(r#"patterns = ["c"]"#);
        let merged = deep_merge_table(&base, &over);
        let arr = merged.get("patterns").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].as_str(), Some("c"));
    }

    #[test]
    fn merge_over_both_none_is_none() {
        assert!(PluginConfiguration::merge_over(None, None).is_none());
    }

    #[test]
    fn merge_over_only_base_returns_base() {
        let base = PluginConfiguration::Raw("base".to_string());
        let merged = PluginConfiguration::merge_over(Some(&base), None).unwrap();
        assert!(matches!(merged, PluginConfiguration::Raw(s) if s == "base"));
    }

    #[test]
    fn merge_over_only_route_returns_route() {
        let over = PluginConfiguration::Raw("over".to_string());
        let merged = PluginConfiguration::merge_over(None, Some(&over)).unwrap();
        assert!(matches!(merged, PluginConfiguration::Raw(s) if s == "over"));
    }

    #[test]
    fn merge_over_table_table_deep_merges() {
        let base = PluginConfiguration::Table(table_from_toml(r#"mode = "block""#));
        let over = PluginConfiguration::Table(table_from_toml(r#"mode = "log_only""#));
        let merged = PluginConfiguration::merge_over(Some(&base), Some(&over)).unwrap();
        match merged {
            PluginConfiguration::Table(t) => {
                assert_eq!(t.get("mode").unwrap().as_str(), Some("log_only"));
            }
            _ => panic!("expected table"),
        }
    }

    #[test]
    fn merge_over_raw_and_table_mix_replaces_completely() {
        let base = PluginConfiguration::Raw("raw-base".to_string());
        let over = PluginConfiguration::Table(table_from_toml(r#"mode = "log_only""#));
        let merged = PluginConfiguration::merge_over(Some(&base), Some(&over)).unwrap();
        assert!(matches!(merged, PluginConfiguration::Table(_)));

        let base2 = PluginConfiguration::Table(table_from_toml(r#"mode = "block""#));
        let over2 = PluginConfiguration::Raw("raw-over".to_string());
        let merged2 = PluginConfiguration::merge_over(Some(&base2), Some(&over2)).unwrap();
        assert!(matches!(merged2, PluginConfiguration::Raw(s) if s == "raw-over"));
    }

    #[test]
    fn empty_configuration_is_shared_and_empty() {
        let a = empty_configuration();
        let b = empty_configuration();
        assert_eq!(a.len(), 0);
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn default_is_raw_empty_string() {
        let cfg = PluginConfiguration::default();
        assert!(matches!(cfg, PluginConfiguration::Raw(ref s) if s.is_empty()));
    }

    #[test]
    fn module_ref_from_name_has_no_configuration() {
        let r = ModuleRef::from_name("mymod");
        assert_eq!(r.name, "mymod");
        assert!(r.configuration.is_none());
    }
}
