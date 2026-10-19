//! JSON Schema（OpenAPI 3.1 の Schema Object）と自動バリデーション（F-189）。
//!
//! `#[derive(veil::Schema)]` が構造体・列挙に `ApiSchema` と `Validate` を実装する。
//! 属性は `validator` クレートと同じ記法（`#[validate(range(min = 1, max = 100))]`・
//! `length(min, max)`・`email`・`url`・`nested`・`custom = "path"`）を受け、同じ定義から
//! OpenAPI の `minimum`/`maximum`/`minLength`/`maxLength`/`format` と実行時の検証（422）を作る。

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// 名前付きスキーマの集合（OpenAPI の `components.schemas`）。
pub type Definitions = BTreeMap<String, Value>;

/// JSON Schema を持つ型。
pub trait ApiSchema {
    /// この型のスキーマ。名前付きの型（構造体・列挙）は `defs` に定義を登録して `$ref` を返す。
    fn schema(defs: &mut Definitions) -> Value;

    /// `Option<T>` なら true（必須でないフィールド・クエリ）。
    fn is_optional() -> bool {
        false
    }
}

/// 名前付きスキーマへの参照。
pub fn schema_ref(name: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{}", name) })
}

macro_rules! int_schema {
    ($($t:ty => $fmt:expr, $unsigned:expr);* $(;)?) => {
        $(impl ApiSchema for $t {
            fn schema(_: &mut Definitions) -> Value {
                let mut m = Map::new();
                m.insert("type".into(), "integer".into());
                m.insert("format".into(), $fmt.into());
                if $unsigned {
                    m.insert("minimum".into(), 0.into());
                }
                Value::Object(m)
            }
        })*
    };
}

int_schema! {
    i8 => "int32", false; i16 => "int32", false; i32 => "int32", false; i64 => "int64", false;
    isize => "int64", false; i128 => "int64", false;
    u8 => "int32", true; u16 => "int32", true; u32 => "int64", true; u64 => "int64", true;
    usize => "int64", true; u128 => "int64", true;
}

impl ApiSchema for f32 {
    fn schema(_: &mut Definitions) -> Value {
        json!({ "type": "number", "format": "float" })
    }
}

impl ApiSchema for f64 {
    fn schema(_: &mut Definitions) -> Value {
        json!({ "type": "number", "format": "double" })
    }
}

impl ApiSchema for bool {
    fn schema(_: &mut Definitions) -> Value {
        json!({ "type": "boolean" })
    }
}

impl ApiSchema for String {
    fn schema(_: &mut Definitions) -> Value {
        json!({ "type": "string" })
    }
}

impl ApiSchema for str {
    fn schema(_: &mut Definitions) -> Value {
        json!({ "type": "string" })
    }
}

impl ApiSchema for char {
    fn schema(_: &mut Definitions) -> Value {
        json!({ "type": "string", "minLength": 1, "maxLength": 1 })
    }
}

impl ApiSchema for bytes::Bytes {
    fn schema(_: &mut Definitions) -> Value {
        json!({ "type": "string", "format": "binary" })
    }
}

impl ApiSchema for Value {
    fn schema(_: &mut Definitions) -> Value {
        json!({})
    }
}

impl<T: ApiSchema> ApiSchema for Option<T> {
    fn schema(defs: &mut Definitions) -> Value {
        T::schema(defs)
    }
    fn is_optional() -> bool {
        true
    }
}

impl<T: ApiSchema + ?Sized> ApiSchema for Box<T> {
    fn schema(defs: &mut Definitions) -> Value {
        T::schema(defs)
    }
}

impl<T: ApiSchema + ?Sized> ApiSchema for std::sync::Arc<T> {
    fn schema(defs: &mut Definitions) -> Value {
        T::schema(defs)
    }
}

impl<T: ApiSchema + ?Sized> ApiSchema for &T {
    fn schema(defs: &mut Definitions) -> Value {
        T::schema(defs)
    }
}

impl<T: ApiSchema> ApiSchema for Vec<T> {
    fn schema(defs: &mut Definitions) -> Value {
        json!({ "type": "array", "items": T::schema(defs) })
    }
}

impl<T: ApiSchema> ApiSchema for [T] {
    fn schema(defs: &mut Definitions) -> Value {
        json!({ "type": "array", "items": T::schema(defs) })
    }
}

impl<T: ApiSchema, const N: usize> ApiSchema for [T; N] {
    fn schema(defs: &mut Definitions) -> Value {
        json!({ "type": "array", "items": T::schema(defs), "minItems": N, "maxItems": N })
    }
}

impl<T: ApiSchema> ApiSchema for std::collections::HashSet<T> {
    fn schema(defs: &mut Definitions) -> Value {
        json!({ "type": "array", "items": T::schema(defs), "uniqueItems": true })
    }
}

impl<T: ApiSchema> ApiSchema for std::collections::BTreeSet<T> {
    fn schema(defs: &mut Definitions) -> Value {
        json!({ "type": "array", "items": T::schema(defs), "uniqueItems": true })
    }
}

impl<K, T: ApiSchema, S> ApiSchema for std::collections::HashMap<K, T, S> {
    fn schema(defs: &mut Definitions) -> Value {
        json!({ "type": "object", "additionalProperties": T::schema(defs) })
    }
}

impl<K, T: ApiSchema> ApiSchema for std::collections::BTreeMap<K, T> {
    fn schema(defs: &mut Definitions) -> Value {
        json!({ "type": "object", "additionalProperties": T::schema(defs) })
    }
}

impl<T: ApiSchema> ApiSchema for super::response::Json<T> {
    fn schema(defs: &mut Definitions) -> Value {
        T::schema(defs)
    }
}

/// フィールドにバリデーション制約を付ける（`$ref` の場合は `allOf` で包む）。
pub fn with_constraints(schema: Value, constraints: Map<String, Value>) -> Value {
    if constraints.is_empty() {
        return schema;
    }
    match schema {
        Value::Object(mut m) if !m.contains_key("$ref") => {
            for (k, v) in constraints {
                m.insert(k, v);
            }
            Value::Object(m)
        }
        other => {
            let mut m = constraints;
            m.insert("allOf".into(), Value::Array(vec![other]));
            Value::Object(m)
        }
    }
}

// ===================== バリデーション =====================

/// 1 件の検証エラー（FastAPI の `detail` 要素と同じ形: `type`・`loc`・`msg`）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FieldError {
    #[serde(rename = "type")]
    /// エラーの種類（JSON では `type`。`missing`・`string_too_short` 等）。
    pub kind: String,
    /// エラーの位置（フィールド名・配列の添字の並び）。
    pub loc: Vec<Value>,
    /// 人が読むメッセージ。
    pub msg: String,
}

/// 検証エラーの集合。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValidationErrors {
    /// 集まったエラー。
    pub errors: Vec<FieldError>,
}

impl ValidationErrors {
    /// エラーが 1 件も無いか。
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    /// 位置 `loc`・種類 `kind`・メッセージ `msg` のエラーを足す。
    pub fn push(&mut self, loc: &[Value], kind: &str, msg: impl Into<String>) {
        self.errors.push(FieldError {
            kind: kind.to_string(),
            loc: loc.to_vec(),
            msg: msg.into(),
        });
    }

    /// 先頭に `loc` 要素（例: `"body"`）を付ける。
    pub fn prefixed(mut self, first: &str) -> ValidationErrors {
        for e in &mut self.errors {
            e.loc.insert(0, Value::String(first.to_string()));
        }
        self
    }

    /// 422 応答（`{"detail": [...]}`）。
    pub fn into_response(self) -> super::response::Response {
        let body = json!({ "detail": self.errors });
        super::response::Response::json(&body).with_status(422)
    }
}

impl std::fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, e) in self.errors.iter().enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{:?}: {}", e.loc, e.msg)?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}

/// 自動バリデーションの対象になる型（`#[derive(veil::Schema)]` が実装する）。
pub trait Validate {
    /// `loc` はこの値の位置（エラーの `loc` の接頭辞）。
    fn validate_at(&self, loc: &mut Vec<Value>, errors: &mut ValidationErrors);

    /// 検証してエラーがあれば返す。
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::default();
        let mut loc = Vec::new();
        self.validate_at(&mut loc, &mut errors);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl<T: Validate> Validate for Option<T> {
    fn validate_at(&self, loc: &mut Vec<Value>, errors: &mut ValidationErrors) {
        if let Some(v) = self {
            v.validate_at(loc, errors);
        }
    }
}

impl<T: Validate> Validate for Vec<T> {
    fn validate_at(&self, loc: &mut Vec<Value>, errors: &mut ValidationErrors) {
        for (i, v) in self.iter().enumerate() {
            loc.push(Value::from(i));
            v.validate_at(loc, errors);
            loc.pop();
        }
    }
}

impl<T: Validate + ?Sized> Validate for Box<T> {
    fn validate_at(&self, loc: &mut Vec<Value>, errors: &mut ValidationErrors) {
        (**self).validate_at(loc, errors)
    }
}

/// 数値制約の検査用（derive の生成コードが使う）。
pub trait AsNumber {
    /// 数値として見た値（`ge`・`le` 等の検査に使う。数値でなければ `None`）。
    fn as_number(&self) -> Option<f64>;
}

macro_rules! as_number {
    ($($t:ty),*) => { $(impl AsNumber for $t { fn as_number(&self) -> Option<f64> { Some(*self as f64) } })* };
}
as_number!(i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize, f32, f64);

impl<T: AsNumber> AsNumber for Option<T> {
    fn as_number(&self) -> Option<f64> {
        self.as_ref().and_then(|v| v.as_number())
    }
}

/// 長さ制約の検査用（文字列は文字数、配列は要素数）。
pub trait AsLength {
    /// 長さ（文字列は文字数、配列は要素数。対象外なら `None`）。
    fn as_length(&self) -> Option<usize>;
}

impl AsLength for String {
    fn as_length(&self) -> Option<usize> {
        Some(self.chars().count())
    }
}

impl AsLength for str {
    fn as_length(&self) -> Option<usize> {
        Some(self.chars().count())
    }
}

impl AsLength for &str {
    fn as_length(&self) -> Option<usize> {
        Some(self.chars().count())
    }
}

impl<T> AsLength for Vec<T> {
    fn as_length(&self) -> Option<usize> {
        Some(self.len())
    }
}

impl<K, V, S> AsLength for std::collections::HashMap<K, V, S> {
    fn as_length(&self) -> Option<usize> {
        Some(self.len())
    }
}

impl<T: AsLength> AsLength for Option<T> {
    fn as_length(&self) -> Option<usize> {
        self.as_ref().and_then(|v| v.as_length())
    }
}

/// 文字列制約（email・url）の検査用。
pub trait AsText {
    /// 文字列として見た値（`email`・`url` の検査に使う。対象外なら `None`）。
    fn as_text(&self) -> Option<&str>;
}

impl AsText for String {
    fn as_text(&self) -> Option<&str> {
        Some(self)
    }
}

impl AsText for &str {
    fn as_text(&self) -> Option<&str> {
        Some(self)
    }
}

impl<T: AsText> AsText for Option<T> {
    fn as_text(&self) -> Option<&str> {
        self.as_ref().and_then(|v| v.as_text())
    }
}

/// 範囲制約（`range(min, max, exclusive_min, exclusive_max)`）。
#[allow(clippy::too_many_arguments)]
pub fn check_range(
    value: Option<f64>,
    min: Option<f64>,
    max: Option<f64>,
    exclusive_min: Option<f64>,
    exclusive_max: Option<f64>,
    loc: &[Value],
    errors: &mut ValidationErrors,
) {
    let Some(v) = value else { return };
    if let Some(m) = min {
        if v < m {
            errors.push(
                loc,
                "greater_than_equal",
                format!("Input should be greater than or equal to {}", fmt_num(m)),
            );
        }
    }
    if let Some(m) = max {
        if v > m {
            errors.push(
                loc,
                "less_than_equal",
                format!("Input should be less than or equal to {}", fmt_num(m)),
            );
        }
    }
    if let Some(m) = exclusive_min {
        if v <= m {
            errors.push(
                loc,
                "greater_than",
                format!("Input should be greater than {}", fmt_num(m)),
            );
        }
    }
    if let Some(m) = exclusive_max {
        if v >= m {
            errors.push(
                loc,
                "less_than",
                format!("Input should be less than {}", fmt_num(m)),
            );
        }
    }
}

/// 長さ制約（`length(min, max, equal)`）。
pub fn check_length(
    len: Option<usize>,
    min: Option<usize>,
    max: Option<usize>,
    equal: Option<usize>,
    loc: &[Value],
    errors: &mut ValidationErrors,
) {
    let Some(n) = len else { return };
    if let Some(m) = min {
        if n < m {
            errors.push(
                loc,
                "too_short",
                format!("Value should have at least {} item(s)/character(s)", m),
            );
        }
    }
    if let Some(m) = max {
        if n > m {
            errors.push(
                loc,
                "too_long",
                format!("Value should have at most {} item(s)/character(s)", m),
            );
        }
    }
    if let Some(m) = equal {
        if n != m {
            errors.push(
                loc,
                "length",
                format!("Value should have exactly {} item(s)/character(s)", m),
            );
        }
    }
}

/// メールアドレスの簡易検査（`local@domain.tld`、空白なし）。
pub fn check_email(text: Option<&str>, loc: &[Value], errors: &mut ValidationErrors) {
    let Some(s) = text else { return };
    let ok = match s.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && !domain.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !s.chars().any(|c| c.is_whitespace())
                && !domain.contains('@')
        }
        None => false,
    };
    if !ok {
        errors.push(loc, "value_error", "value is not a valid email address");
    }
}

/// URL の簡易検査（`scheme://host...`）。
pub fn check_url(text: Option<&str>, loc: &[Value], errors: &mut ValidationErrors) {
    let Some(s) = text else { return };
    let ok = match s.split_once("://") {
        Some((scheme, rest)) => {
            !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
                && !rest.is_empty()
                && !s.chars().any(|c| c.is_whitespace())
        }
        None => false,
    };
    if !ok {
        errors.push(loc, "url_parsing", "Input should be a valid URL");
    }
}

fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{}", v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitive_schemas() {
        let mut d = Definitions::new();
        assert_eq!(u32::schema(&mut d)["minimum"], 0);
        assert_eq!(Vec::<String>::schema(&mut d)["items"]["type"], "string");
        assert!(Option::<i32>::is_optional());
        assert!(d.is_empty());
    }

    #[test]
    fn range_and_length_errors() {
        let mut e = ValidationErrors::default();
        let loc = vec![Value::from("age")];
        check_range(Some(0.0), Some(1.0), Some(100.0), None, None, &loc, &mut e);
        check_range(Some(50.0), Some(1.0), Some(100.0), None, None, &loc, &mut e);
        check_length(Some(0), Some(1), Some(50), None, &loc, &mut e);
        check_email(Some("nope"), &loc, &mut e);
        check_email(Some("a@b.co"), &loc, &mut e);
        check_url(Some("https://x"), &loc, &mut e);
        assert_eq!(e.errors.len(), 3, "{:?}", e);
        assert_eq!(e.errors[0].kind, "greater_than_equal");
        assert_eq!(
            e.errors[0].msg,
            "Input should be greater than or equal to 1"
        );
        let r = e.prefixed("body").into_response();
        assert_eq!(r.status_code(), 422);
        let v: Value = serde_json::from_slice(r.body()).unwrap();
        assert_eq!(v["detail"][0]["loc"], json!(["body", "age"]));
    }

    #[test]
    fn constraints_wrap_refs() {
        let mut c = Map::new();
        c.insert("minimum".into(), 1.into());
        let s = with_constraints(schema_ref("X"), c.clone());
        assert!(s["allOf"].is_array());
        let s = with_constraints(json!({"type": "integer"}), c);
        assert_eq!(s["minimum"], 1);
    }
}
