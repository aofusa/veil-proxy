// F-189
//! JSON Schema (the OpenAPI 3.1 Schema Object) and automatic validation.
//!
//! `#[derive(veil::Schema)]` implements `ApiSchema` and `Validate` for structs and enums.
//! The attributes use the same notation as the `validator` crate (`#[validate(range(min = 1, max = 100))]`,
//! `length(min, max)`, `email`, `url`, `nested` and `custom = "path"`), and from the same definition produce
//! the OpenAPI `minimum`/`maximum`/`minLength`/`maxLength`/`format` and the runtime validation (422).

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// A set of named schemas (`components.schemas` in OpenAPI).
pub type Definitions = BTreeMap<String, Value>;

/// A type that has a JSON Schema.
pub trait ApiSchema {
    /// The schema of this type. A named type (a struct or an enum) registers its definition in `defs` and returns a `$ref`.
    fn schema(defs: &mut Definitions) -> Value;

    /// True for `Option<T>` (a field or query parameter that is not required).
    fn is_optional() -> bool {
        false
    }
}

/// A reference to a named schema.
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

/// Adds validation constraints to a field (a `$ref` is wrapped in `allOf`).
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

/// One validation error (the same shape as a FastAPI `detail` element: `type`, `loc` and `msg`).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FieldError {
    #[serde(rename = "type")]
    /// The kind of error (`type` in JSON, such as `missing` or `string_too_short`).
    pub kind: String,
    /// The location of the error (a sequence of field names and array indices).
    pub loc: Vec<Value>,
    /// A human-readable message.
    pub msg: String,
}

/// A collection of validation errors.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValidationErrors {
    /// The collected errors.
    pub errors: Vec<FieldError>,
}

impl ValidationErrors {
    /// Whether there are no errors at all.
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    /// Adds an error with location `loc`, kind `kind` and message `msg`.
    pub fn push(&mut self, loc: &[Value], kind: &str, msg: impl Into<String>) {
        self.errors.push(FieldError {
            kind: kind.to_string(),
            loc: loc.to_vec(),
            msg: msg.into(),
        });
    }

    /// Prepends a `loc` element (for example `"body"`).
    pub fn prefixed(mut self, first: &str) -> ValidationErrors {
        for e in &mut self.errors {
            e.loc.insert(0, Value::String(first.to_string()));
        }
        self
    }

    /// The 422 response (`{"detail": [...]}`).
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

/// A type subject to automatic validation (implemented by `#[derive(veil::Schema)]`).
pub trait Validate {
    /// `loc` is the location of this value (the prefix of each error's `loc`).
    fn validate_at(&self, loc: &mut Vec<Value>, errors: &mut ValidationErrors);

    /// Validates and returns the errors, if any.
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

/// For checking numeric constraints (used by derive-generated code).
pub trait AsNumber {
    /// The value as a number (used for checks such as `ge` and `le`; `None` if it is not numeric).
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

/// For checking length constraints (the number of characters for strings, of elements for arrays).
pub trait AsLength {
    /// The length (characters for strings, elements for arrays; `None` if not applicable).
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

/// For checking string constraints (email and url).
pub trait AsText {
    /// The value as a string (used for the `email` and `url` checks; `None` if not applicable).
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

/// The range constraint (`range(min, max, exclusive_min, exclusive_max)`).
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

/// The length constraint (`length(min, max, equal)`).
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

/// A simple email address check (`local@domain.tld`, no whitespace).
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

/// A simple URL check (`scheme://host...`).
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
