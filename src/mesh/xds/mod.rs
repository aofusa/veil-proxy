//! Envoy の xDS v3（F-222）: 型定義・メッシュモデルへの変換・Delta ADS のクライアント。

pub mod ads;
pub mod dump;
pub mod proto;
pub mod rbac;
pub mod translate;

use proto::{Struct, Value};

/// JSON（ブートストラップの `node.metadata` 等）→ `google.protobuf.Struct`。
pub fn json_to_struct(v: &serde_json::Value) -> Struct {
    let mut s = Struct::default();
    if let serde_json::Value::Object(m) = v {
        for (k, v) in m {
            s.fields.insert(k.clone(), json_to_value(v));
        }
    }
    s
}

pub fn json_to_value(v: &serde_json::Value) -> Value {
    use proto::value::Kind;
    let kind = match v {
        serde_json::Value::Null => Kind::NullValue(0),
        serde_json::Value::Bool(b) => Kind::BoolValue(*b),
        serde_json::Value::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(s) => Kind::StringValue(s.clone()),
        serde_json::Value::Array(a) => Kind::ListValue(proto::ListValue {
            values: a.iter().map(json_to_value).collect(),
        }),
        serde_json::Value::Object(_) => Kind::StructValue(json_to_struct(v)),
    };
    Value { kind: Some(kind) }
}

/// `google.protobuf.Struct` → JSON（管理 API の表示用）。
pub fn struct_to_json(s: &Struct) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for (k, v) in &s.fields {
        m.insert(k.clone(), value_to_json(v));
    }
    serde_json::Value::Object(m)
}

pub fn value_to_json(v: &Value) -> serde_json::Value {
    use proto::value::Kind;
    match &v.kind {
        None | Some(Kind::NullValue(_)) => serde_json::Value::Null,
        Some(Kind::BoolValue(b)) => serde_json::Value::Bool(*b),
        Some(Kind::NumberValue(n)) => serde_json::json!(n),
        Some(Kind::StringValue(s)) => serde_json::Value::String(s.clone()),
        Some(Kind::ListValue(l)) => {
            serde_json::Value::Array(l.values.iter().map(value_to_json).collect())
        }
        Some(Kind::StructValue(s)) => struct_to_json(s),
    }
}
