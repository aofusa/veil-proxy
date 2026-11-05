// F-291: C のルートの JSON 本文の検証（`veil_route_options_t.body_schema_json`）。JSON Schema の部分集合を登録時に
// 解釈して木にし、要求ごとには木をたどるだけ（正規表現も登録時にコンパイル）。誤りは Rust の Validate・Python の
// pydantic と同じ形の 422（`{"detail":[{"type","loc","msg"}]}`、loc は "body" から）。未対応のキーワード・`$ref` は
// 登録時に拒否する（黙って検証を飛ばさない）。
//! A JSON Schema subset for validating request bodies of C routes.

use regex::Regex;
use serde_json::{Map, Value};
use veil::api::__private::{check_email, check_length, check_range, check_url};
use veil::api::ValidationErrors;

/// The JSON types of `type`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ty {
    Null,
    Boolean,
    Integer,
    Number,
    String,
    Array,
    Object,
}

impl Ty {
    fn parse(s: &str) -> Result<Ty, String> {
        Ok(match s {
            "null" => Ty::Null,
            "boolean" => Ty::Boolean,
            "integer" => Ty::Integer,
            "number" => Ty::Number,
            "string" => Ty::String,
            "array" => Ty::Array,
            "object" => Ty::Object,
            _ => return Err(format!("unknown type '{}'", s)),
        })
    }

    fn accepts(self, v: &Value) -> bool {
        match self {
            Ty::Null => v.is_null(),
            Ty::Boolean => v.is_boolean(),
            Ty::Integer => match v {
                Value::Number(n) => {
                    n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
                }
                _ => false,
            },
            Ty::Number => v.is_number(),
            Ty::String => v.is_string(),
            Ty::Array => v.is_array(),
            Ty::Object => v.is_object(),
        }
    }

    /// pydantic と同じ誤りの種類と文。
    fn error(self) -> (&'static str, &'static str) {
        match self {
            Ty::Null => ("none_required", "Input should be None"),
            Ty::Boolean => ("bool_type", "Input should be a valid boolean"),
            Ty::Integer => ("int_type", "Input should be a valid integer"),
            Ty::Number => ("float_type", "Input should be a valid number"),
            Ty::String => ("string_type", "Input should be a valid string"),
            Ty::Array => ("list_type", "Input should be a valid list"),
            Ty::Object => ("dict_type", "Input should be a valid dictionary"),
        }
    }
}

/// `additionalProperties`.
enum Additional {
    Any,
    Forbidden,
    Schema(Box<Schema>),
}

/// A compiled schema node.
pub(crate) struct Schema {
    types: Vec<Ty>,
    properties: Vec<(String, Schema)>,
    required: Vec<String>,
    additional: Additional,
    items: Option<Box<Schema>>,
    min_items: Option<usize>,
    max_items: Option<usize>,
    min_length: Option<usize>,
    max_length: Option<usize>,
    pattern: Option<Regex>,
    enumeration: Option<Vec<Value>>,
    constant: Option<Value>,
    minimum: Option<f64>,
    maximum: Option<f64>,
    exclusive_minimum: Option<f64>,
    exclusive_maximum: Option<f64>,
    multiple_of: Option<f64>,
    format: Option<Format>,
    all_of: Vec<Schema>,
    any_of: Vec<Schema>,
    one_of: Vec<Schema>,
    not: Option<Box<Schema>>,
}

#[derive(Clone, Copy)]
enum Format {
    Email,
    Uri,
}

/// 検証には効かない注釈（OpenAPI の文書だけに使う）。
const ANNOTATIONS: &[&str] = &[
    "title",
    "description",
    "default",
    "examples",
    "example",
    "deprecated",
    "readOnly",
    "writeOnly",
    "$schema",
    "$id",
    "$comment",
    "nullable",
];

fn num(v: &Value, key: &str) -> Result<f64, String> {
    v.as_f64()
        .ok_or_else(|| format!("'{}' must be a number", key))
}

fn count(v: &Value, key: &str) -> Result<usize, String> {
    v.as_u64()
        .map(|n| n as usize)
        .ok_or_else(|| format!("'{}' must be a non-negative integer", key))
}

fn list(v: &Value, key: &str) -> Result<Vec<Schema>, String> {
    let a = v
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| format!("'{}' must be a non-empty array of schemas", key))?;
    a.iter().map(Schema::compile).collect()
}

impl Schema {
    /// Compiles a schema; unsupported keywords and `$ref` are errors.
    pub(crate) fn compile(v: &Value) -> Result<Schema, String> {
        let o: &Map<String, Value> = match v {
            Value::Object(o) => o,
            // `true` は何でも通す
            Value::Bool(true) => return Ok(Schema::any()),
            _ => return Err("a schema must be a JSON object".to_string()),
        };
        let mut s = Schema::any();
        let mut nullable = false;
        for (k, val) in o {
            match k.as_str() {
                "type" => {
                    s.types = match val {
                        Value::String(t) => vec![Ty::parse(t)?],
                        Value::Array(a) if !a.is_empty() => a
                            .iter()
                            .map(|t| {
                                t.as_str()
                                    .ok_or_else(|| "'type' must name types".to_string())
                                    .and_then(Ty::parse)
                            })
                            .collect::<Result<_, _>>()?,
                        _ => return Err("'type' must be a string or an array of strings".into()),
                    }
                }
                "properties" => {
                    let p = val.as_object().ok_or("'properties' must be an object")?;
                    for (name, sub) in p {
                        let c = Schema::compile(sub)
                            .map_err(|e| format!("properties.{}: {}", name, e))?;
                        s.properties.push((name.clone(), c));
                    }
                }
                "required" => {
                    s.required = val
                        .as_array()
                        .ok_or("'required' must be an array of strings")?
                        .iter()
                        .map(|n| {
                            n.as_str()
                                .map(str::to_string)
                                .ok_or_else(|| "'required' must be an array of strings".to_string())
                        })
                        .collect::<Result<_, _>>()?
                }
                "additionalProperties" => {
                    s.additional = match val {
                        Value::Bool(true) => Additional::Any,
                        Value::Bool(false) => Additional::Forbidden,
                        other => Additional::Schema(Box::new(
                            Schema::compile(other)
                                .map_err(|e| format!("additionalProperties: {}", e))?,
                        )),
                    }
                }
                "items" => {
                    s.items = Some(Box::new(
                        Schema::compile(val).map_err(|e| format!("items: {}", e))?,
                    ))
                }
                "minItems" => s.min_items = Some(count(val, k)?),
                "maxItems" => s.max_items = Some(count(val, k)?),
                "minLength" => s.min_length = Some(count(val, k)?),
                "maxLength" => s.max_length = Some(count(val, k)?),
                "pattern" => {
                    let p = val.as_str().ok_or("'pattern' must be a string")?;
                    s.pattern = Some(Regex::new(p).map_err(|e| format!("invalid pattern: {}", e))?);
                }
                "enum" => {
                    s.enumeration = Some(
                        val.as_array()
                            .filter(|a| !a.is_empty())
                            .ok_or("'enum' must be a non-empty array")?
                            .clone(),
                    )
                }
                "const" => s.constant = Some(val.clone()),
                "minimum" => s.minimum = Some(num(val, k)?),
                "maximum" => s.maximum = Some(num(val, k)?),
                "exclusiveMinimum" => s.exclusive_minimum = Some(num(val, k)?),
                "exclusiveMaximum" => s.exclusive_maximum = Some(num(val, k)?),
                "multipleOf" => {
                    let m = num(val, k)?;
                    if m <= 0.0 {
                        return Err("'multipleOf' must be greater than 0".into());
                    }
                    s.multiple_of = Some(m);
                }
                "format" => {
                    // 知らない書式は注釈として扱う（JSON Schema の既定と同じ）
                    s.format = match val.as_str() {
                        Some("email") => Some(Format::Email),
                        Some("uri") | Some("url") => Some(Format::Uri),
                        Some(_) => None,
                        None => return Err("'format' must be a string".into()),
                    }
                }
                "allOf" => s.all_of = list(val, k)?,
                "anyOf" => s.any_of = list(val, k)?,
                "oneOf" => s.one_of = list(val, k)?,
                "not" => {
                    s.not = Some(Box::new(
                        Schema::compile(val).map_err(|e| format!("not: {}", e))?,
                    ))
                }
                "$ref" => {
                    return Err("'$ref' is not supported; inline the referenced schema".into())
                }
                _ if ANNOTATIONS.contains(&k.as_str()) => {
                    if k == "nullable" {
                        nullable = val.as_bool().unwrap_or(false);
                    }
                }
                other => return Err(format!("unsupported keyword '{}'", other)),
            }
        }
        // OpenAPI 3.0 の nullable
        if nullable && !s.types.is_empty() && !s.types.contains(&Ty::Null) {
            s.types.push(Ty::Null);
        }
        Ok(s)
    }

    fn any() -> Schema {
        Schema {
            types: Vec::new(),
            properties: Vec::new(),
            required: Vec::new(),
            additional: Additional::Any,
            items: None,
            min_items: None,
            max_items: None,
            min_length: None,
            max_length: None,
            pattern: None,
            enumeration: None,
            constant: None,
            minimum: None,
            maximum: None,
            exclusive_minimum: None,
            exclusive_maximum: None,
            multiple_of: None,
            format: None,
            all_of: Vec::new(),
            any_of: Vec::new(),
            one_of: Vec::new(),
            not: None,
        }
    }

    /// Validates `v`; errors are pushed with locations under `loc`.
    pub(crate) fn validate(&self, v: &Value, loc: &mut Vec<Value>, errors: &mut ValidationErrors) {
        if !self.types.is_empty() && !self.types.iter().any(|t| t.accepts(v)) {
            let (kind, msg) = self.types[0].error();
            errors.push(loc, kind, msg);
            return;
        }
        if let Some(c) = &self.constant {
            if !json_eq(c, v) {
                errors.push(loc, "literal_error", format!("Input should be {}", c));
            }
        }
        if let Some(e) = &self.enumeration {
            if !e.iter().any(|x| json_eq(x, v)) {
                let list: Vec<String> = e.iter().map(|x| x.to_string()).collect();
                errors.push(
                    loc,
                    "enum",
                    format!("Input should be one of {}", list.join(", ")),
                );
            }
        }
        match v {
            Value::String(s) => {
                let n = s.chars().count();
                if self.min_length.is_some() || self.max_length.is_some() {
                    check_length(Some(n), self.min_length, self.max_length, None, loc, errors);
                }
                if let Some(re) = &self.pattern {
                    if !re.is_match(s) {
                        errors.push(
                            loc,
                            "string_pattern_mismatch",
                            format!("String should match pattern '{}'", re.as_str()),
                        );
                    }
                }
                match self.format {
                    Some(Format::Email) => check_email(Some(s), loc, errors),
                    Some(Format::Uri) => check_url(Some(s), loc, errors),
                    None => {}
                }
            }
            Value::Number(n) => {
                let f = n.as_f64().unwrap_or(0.0);
                check_range(
                    Some(f),
                    self.minimum,
                    self.maximum,
                    self.exclusive_minimum,
                    self.exclusive_maximum,
                    loc,
                    errors,
                );
                if let Some(m) = self.multiple_of {
                    let q = f / m;
                    if (q - q.round()).abs() > 1e-9 {
                        errors.push(
                            loc,
                            "multiple_of",
                            format!("Input should be a multiple of {}", m),
                        );
                    }
                }
            }
            Value::Array(a) => {
                if self.min_items.is_some() || self.max_items.is_some() {
                    check_length(
                        Some(a.len()),
                        self.min_items,
                        self.max_items,
                        None,
                        loc,
                        errors,
                    );
                }
                if let Some(it) = &self.items {
                    for (i, x) in a.iter().enumerate() {
                        loc.push(Value::from(i));
                        it.validate(x, loc, errors);
                        loc.pop();
                    }
                }
            }
            Value::Object(o) => {
                for name in &self.required {
                    if !o.contains_key(name) {
                        loc.push(Value::String(name.clone()));
                        errors.push(loc, "missing", "Field required");
                        loc.pop();
                    }
                }
                for (name, x) in o {
                    loc.push(Value::String(name.clone()));
                    match self.properties.iter().find(|(n, _)| n == name) {
                        Some((_, s)) => s.validate(x, loc, errors),
                        None => match &self.additional {
                            Additional::Any => {}
                            Additional::Forbidden => errors.push(
                                loc,
                                "extra_forbidden",
                                "Extra inputs are not permitted",
                            ),
                            Additional::Schema(s) => s.validate(x, loc, errors),
                        },
                    }
                    loc.pop();
                }
            }
            _ => {}
        }
        for s in &self.all_of {
            s.validate(v, loc, errors);
        }
        if !self.any_of.is_empty() && !self.any_of.iter().any(|s| s.passes(v)) {
            errors.push(
                loc,
                "union_error",
                "Input does not match any of the allowed schemas",
            );
        }
        if !self.one_of.is_empty() {
            let n = self.one_of.iter().filter(|s| s.passes(v)).count();
            if n != 1 {
                errors.push(
                    loc,
                    "union_error",
                    "Input should match exactly one of the allowed schemas",
                );
            }
        }
        if let Some(s) = &self.not {
            if s.passes(v) {
                errors.push(loc, "value_error", "Input should not match the schema");
            }
        }
    }

    fn passes(&self, v: &Value) -> bool {
        let mut e = ValidationErrors::default();
        self.validate(v, &mut Vec::new(), &mut e);
        e.is_empty()
    }
}

/// JSON の値の比較（数は値で比べる: `1` と `1.0` は等しい）。
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| json_eq(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, p)| y.get(k).is_some_and(|q| json_eq(p, q)))
        }
        _ => a == b,
    }
}

/// Validates a request body against `schema` (`Err(errors).into_response()` is the 422 response).
pub(crate) fn check_body(schema: &Schema, body: &[u8]) -> Result<(), ValidationErrors> {
    let mut errors = ValidationErrors::default();
    let mut loc = vec![Value::String("body".to_string())];
    if body.is_empty() {
        errors.push(&loc, "missing", "Field required");
        return Err(errors);
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(v) => schema.validate(&v, &mut loc, &mut errors),
        Err(e) => errors.push(&loc, "json_invalid", e.to_string()),
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn errs(schema: Value, body: &str) -> Vec<Value> {
        let s = Schema::compile(&schema).unwrap();
        match check_body(&s, body.as_bytes()) {
            Ok(()) => Vec::new(),
            Err(e) => {
                let v = serde_json::to_value(&e.errors).unwrap();
                assert_eq!(e.into_response().status_code(), 422);
                v.as_array().unwrap().clone()
            }
        }
    }

    #[test]
    fn validates_like_pydantic() {
        let schema = json!({
            "type": "object",
            "required": ["name", "age"],
            "additionalProperties": false,
            "properties": {
                "name": {"type": "string", "minLength": 1, "maxLength": 5, "pattern": "^[a-z]+$"},
                "age": {"type": "integer", "minimum": 0, "maximum": 150},
                "email": {"type": "string", "format": "email"},
                "tags": {"type": "array", "items": {"type": "string"}, "maxItems": 2},
                "kind": {"enum": ["a", "b"]},
                "note": {"type": "string", "nullable": true, "description": "free text"}
            }
        });
        assert!(errs(schema.clone(), r#"{"name":"bob","age":3,"note":null}"#).is_empty());
        let e = errs(
            schema.clone(),
            r#"{"name":"BOB","tags":["x",1,"z"],"extra":1,"kind":"c"}"#,
        );
        let kinds: Vec<(&str, Value)> = e
            .iter()
            .map(|x| (x["type"].as_str().unwrap(), x["loc"].clone()))
            .collect();
        assert!(kinds.contains(&("missing", json!(["body", "age"]))));
        assert!(kinds.contains(&("string_pattern_mismatch", json!(["body", "name"]))));
        assert!(kinds.contains(&("too_long", json!(["body", "tags"]))));
        assert!(kinds.contains(&("string_type", json!(["body", "tags", 1]))));
        assert!(kinds.contains(&("extra_forbidden", json!(["body", "extra"]))));
        assert!(kinds.contains(&("enum", json!(["body", "kind"]))));
        let e = errs(schema.clone(), r#"{"name":"bob","age":1.5,"email":"nope"}"#);
        assert_eq!(e[0]["type"], "int_type");
        assert_eq!(e[1]["type"], "value_error");
        assert_eq!(errs(schema.clone(), "{bad")[0]["type"], "json_invalid");
        assert_eq!(errs(schema, "")[0]["type"], "missing");
        // anyOf / oneOf / const / multipleOf
        let s = json!({"anyOf": [{"type": "integer", "multipleOf": 5}, {"const": "x"}]});
        assert!(errs(s.clone(), "10").is_empty());
        assert!(errs(s.clone(), "\"x\"").is_empty());
        assert_eq!(errs(s, "7")[0]["type"], "union_error");
    }

    #[test]
    fn rejects_unsupported_schemas() {
        for bad in [
            json!({"$ref": "#/components/schemas/X"}),
            json!({"type": "object", "patternProperties": {}}),
            json!({"type": "strng"}),
            json!({"pattern": "("}),
            json!({"multipleOf": 0}),
            json!([1]),
        ] {
            assert!(Schema::compile(&bad).is_err(), "{}", bad);
        }
    }
}
