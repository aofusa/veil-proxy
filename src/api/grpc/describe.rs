// F-265: gRPC のルートを OpenAPI に載せるための情報（メソッドの種類と FileDescriptorSet）。
//! What the OpenAPI document needs to list gRPC methods: the kind of each registered method and the
//! `FileDescriptorSet`s that describe their messages.
//!
//! Both are recorded at registration (startup only). The document is built from them when it is generated
//! (`openapi::refresh`, at startup and on reload); nothing here runs on the request path.

use crate::api::app::App;
use bytes::Bytes;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// The kind of a gRPC method (how many messages each side sends).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    /// One request message, one response message.
    Unary,
    /// One request message, a stream of response messages.
    ServerStreaming,
    /// A stream of request messages, one response message.
    ClientStreaming,
    /// Streams in both directions.
    BidiStreaming,
}

impl MethodKind {
    /// The kind from the streaming flags of a method (`MethodDescriptorProto`).
    pub fn from_streaming(client_streaming: bool, server_streaming: bool) -> MethodKind {
        match (client_streaming, server_streaming) {
            (false, false) => MethodKind::Unary,
            (false, true) => MethodKind::ServerStreaming,
            (true, false) => MethodKind::ClientStreaming,
            (true, true) => MethodKind::BidiStreaming,
        }
    }

    #[cfg(feature = "openapi")]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            MethodKind::Unary => "unary",
            MethodKind::ServerStreaming => "server_streaming",
            MethodKind::ClientStreaming => "client_streaming",
            MethodKind::BidiStreaming => "bidi_streaming",
        }
    }
}

/// Registered methods (`/<package>.<Service>/<Method>` → kind). The value is `(kind, explicit)`: a kind given by
/// [`App::grpc_kind`] is not replaced by the one the registration function infers.
static KINDS: Mutex<BTreeMap<String, (MethodKind, bool)>> = Mutex::new(BTreeMap::new());
/// `FileDescriptorSet`s given to `grpc_reflection` or `grpc_descriptors`.
static DESCRIPTORS: Mutex<Vec<Bytes>> = Mutex::new(Vec::new());
/// Whether server reflection is registered (the tag descriptions then show a grpcurl example).
static REFLECTION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn record_kind(path: &str, kind: MethodKind) {
    let mut k = KINDS.lock().unwrap();
    match k.get(path) {
        Some((_, true)) => {}
        _ => {
            k.insert(path.to_string(), (kind, false));
        }
    }
}

pub(crate) fn record_descriptors(sets: &[&[u8]]) {
    let mut d = DESCRIPTORS.lock().unwrap();
    for s in sets {
        if !d.iter().any(|x| x.as_ref() == *s) {
            d.push(Bytes::copy_from_slice(s));
        }
    }
}

pub(crate) fn record_reflection() {
    REFLECTION.store(true, std::sync::atomic::Ordering::Relaxed);
}

impl App {
    /// Uses `descriptor_sets` (`FileDescriptorSet`s, such as `veil-build`'s `FILE_DESCRIPTOR_SET`) to document
    /// the messages of the gRPC methods in OpenAPI, without serving server reflection. `grpc_reflection` uses its
    /// descriptors for the same purpose.
    pub fn grpc_descriptors(self, descriptor_sets: &[&[u8]]) -> App {
        record_descriptors(descriptor_sets);
        self
    }

    /// Sets the kind of the method at `path` for OpenAPI (the methods registered with `grpc_blocking` are
    /// otherwise documented as bidirectional streaming unless a descriptor says otherwise).
    pub fn grpc_kind(self, path: &str, kind: MethodKind) -> App {
        KINDS.lock().unwrap().insert(path.to_string(), (kind, true));
        self
    }
}

#[cfg(feature = "openapi")]
pub(crate) use doc::add_to_openapi;

#[cfg(feature = "openapi")]
mod doc {
    use super::super::wire::{self, Reader};
    use super::super::Status;
    use super::{MethodKind, DESCRIPTORS, KINDS, REFLECTION};
    use crate::api::app::ApiSettings;
    use crate::api::handler::HandlerEntry;
    use serde_json::{json, Map, Value};
    use std::collections::{BTreeSet, HashMap};
    use std::sync::Arc;

    // FieldDescriptorProto.Type
    const T_DOUBLE: u64 = 1;
    const T_FLOAT: u64 = 2;
    const T_INT64: u64 = 3;
    const T_UINT64: u64 = 4;
    const T_INT32: u64 = 5;
    const T_FIXED64: u64 = 6;
    const T_FIXED32: u64 = 7;
    const T_BOOL: u64 = 8;
    const T_STRING: u64 = 9;
    const T_GROUP: u64 = 10;
    const T_MESSAGE: u64 = 11;
    const T_BYTES: u64 = 12;
    const T_UINT32: u64 = 13;
    const T_ENUM: u64 = 14;
    const T_SFIXED32: u64 = 15;
    const T_SFIXED64: u64 = 16;
    const T_SINT32: u64 = 17;
    const T_SINT64: u64 = 18;
    const LABEL_REPEATED: u64 = 3;

    #[derive(Default)]
    struct Field {
        name: String,
        json_name: String,
        label: u64,
        ty: u64,
        type_name: String,
        oneof_index: Option<usize>,
        proto3_optional: bool,
        comment: Option<String>,
    }

    #[derive(Default)]
    struct Message {
        fields: Vec<Field>,
        oneofs: Vec<String>,
        map_entry: bool,
        comment: Option<String>,
    }

    struct Method {
        input: String,
        output: String,
        kind: MethodKind,
        comment: Option<String>,
    }

    #[derive(Default)]
    struct Model {
        messages: HashMap<String, Message>,
        enums: HashMap<String, (Vec<String>, Option<String>)>,
        /// `/<package>.<Service>/<Method>` → method
        methods: HashMap<String, Method>,
        services: HashMap<String, Option<String>>,
    }

    fn full(scope: &str, name: &str) -> String {
        if scope.is_empty() {
            name.to_string()
        } else {
            format!("{}.{}", scope, name)
        }
    }

    /// The comment of a `SourceCodeInfo.Location` (leading, else trailing), trimmed.
    fn tidy(s: &str) -> Option<String> {
        let t = s
            .lines()
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();
        (!t.is_empty()).then_some(t)
    }

    fn locations(info: &[u8]) -> Result<HashMap<Vec<i32>, String>, Status> {
        let mut out = HashMap::new();
        let mut r = Reader::new(info);
        while let Some((f, w)) = r.next_key()? {
            if f != 1 || w != wire::LEN {
                r.skip(w)?;
                continue;
            }
            let loc = r.read_bytes()?;
            let (mut path, mut leading, mut trailing) = (Vec::new(), None, None);
            let mut l = Reader::new(loc);
            while let Some((f, w)) = l.next_key()? {
                match (f, w) {
                    (1, wire::LEN) => {
                        let mut p = Reader::new(l.read_bytes()?);
                        while !p.at_end() {
                            path.push(p.read_varint()? as i32);
                        }
                    }
                    (1, wire::VARINT) => path.push(l.read_varint()? as i32),
                    (3, wire::LEN) => leading = Some(l.read_string()?),
                    (4, wire::LEN) => trailing = Some(l.read_string()?),
                    _ => l.skip(w)?,
                }
            }
            if let Some(c) = leading
                .as_deref()
                .and_then(tidy)
                .or_else(|| trailing.as_deref().and_then(tidy))
            {
                out.insert(path, c);
            }
        }
        Ok(out)
    }

    fn lower_camel(name: &str) -> String {
        let mut out = String::with_capacity(name.len());
        let mut up = false;
        for c in name.chars() {
            if c == '_' {
                up = true;
            } else if up {
                out.extend(c.to_uppercase());
                up = false;
            } else {
                out.push(c);
            }
        }
        out
    }

    impl Model {
        fn add_set(&mut self, set: &[u8]) -> Result<(), Status> {
            let mut r = Reader::new(set);
            while let Some((f, w)) = r.next_key()? {
                if f == 1 && w == wire::LEN {
                    self.add_file(r.read_bytes()?)?;
                } else {
                    r.skip(w)?;
                }
            }
            Ok(())
        }

        fn add_file(&mut self, raw: &[u8]) -> Result<(), Status> {
            let mut package = String::new();
            let mut comments = HashMap::new();
            let mut r = Reader::new(raw);
            while let Some((f, w)) = r.next_key()? {
                match (f, w) {
                    (2, wire::LEN) => package = r.read_string()?,
                    (9, wire::LEN) => comments = locations(r.read_bytes()?)?,
                    _ => r.skip(w)?,
                }
            }
            let (mut mi, mut ei, mut si) = (0, 0, 0);
            let mut r = Reader::new(raw);
            while let Some((f, w)) = r.next_key()? {
                match (f, w) {
                    (4, wire::LEN) => {
                        self.add_message(r.read_bytes()?, &package, vec![4, mi], &comments)?;
                        mi += 1;
                    }
                    (5, wire::LEN) => {
                        self.add_enum(r.read_bytes()?, &package, vec![5, ei], &comments)?;
                        ei += 1;
                    }
                    (6, wire::LEN) => {
                        self.add_service(r.read_bytes()?, &package, si, &comments)?;
                        si += 1;
                    }
                    _ => r.skip(w)?,
                }
            }
            Ok(())
        }

        fn add_message(
            &mut self,
            m: &[u8],
            scope: &str,
            path: Vec<i32>,
            comments: &HashMap<Vec<i32>, String>,
        ) -> Result<(), Status> {
            let mut name = String::new();
            let mut r = Reader::new(m);
            while let Some((f, w)) = r.next_key()? {
                if (f, w) == (1, wire::LEN) {
                    name = r.read_string()?;
                } else {
                    r.skip(w)?;
                }
            }
            let me = full(scope, &name);
            let mut msg = Message {
                comment: comments.get(&path).cloned(),
                ..Default::default()
            };
            let (mut fi, mut ni, mut ei) = (0, 0, 0);
            let mut r = Reader::new(m);
            while let Some((f, w)) = r.next_key()? {
                match (f, w) {
                    (2, wire::LEN) => {
                        let mut p = path.clone();
                        p.extend([2, fi]);
                        msg.fields
                            .push(field(r.read_bytes()?, comments.get(&p).cloned())?);
                        fi += 1;
                    }
                    (3, wire::LEN) => {
                        let mut p = path.clone();
                        p.extend([3, ni]);
                        self.add_message(r.read_bytes()?, &me, p, comments)?;
                        ni += 1;
                    }
                    (4, wire::LEN) => {
                        let mut p = path.clone();
                        p.extend([4, ei]);
                        self.add_enum(r.read_bytes()?, &me, p, comments)?;
                        ei += 1;
                    }
                    (7, wire::LEN) => {
                        let mut o = Reader::new(r.read_bytes()?);
                        while let Some((f, w)) = o.next_key()? {
                            if (f, w) == (7, wire::VARINT) {
                                msg.map_entry = o.read_varint()? != 0;
                            } else {
                                o.skip(w)?;
                            }
                        }
                    }
                    (8, wire::LEN) => {
                        let mut o = Reader::new(r.read_bytes()?);
                        let mut n = String::new();
                        while let Some((f, w)) = o.next_key()? {
                            if (f, w) == (1, wire::LEN) {
                                n = o.read_string()?;
                            } else {
                                o.skip(w)?;
                            }
                        }
                        msg.oneofs.push(n);
                    }
                    _ => r.skip(w)?,
                }
            }
            self.messages.insert(me, msg);
            Ok(())
        }

        fn add_enum(
            &mut self,
            e: &[u8],
            scope: &str,
            path: Vec<i32>,
            comments: &HashMap<Vec<i32>, String>,
        ) -> Result<(), Status> {
            let mut name = String::new();
            let mut values = Vec::new();
            let mut r = Reader::new(e);
            while let Some((f, w)) = r.next_key()? {
                match (f, w) {
                    (1, wire::LEN) => name = r.read_string()?,
                    (2, wire::LEN) => {
                        let mut v = Reader::new(r.read_bytes()?);
                        while let Some((f, w)) = v.next_key()? {
                            if (f, w) == (1, wire::LEN) {
                                values.push(v.read_string()?);
                            } else {
                                v.skip(w)?;
                            }
                        }
                    }
                    _ => r.skip(w)?,
                }
            }
            self.enums
                .insert(full(scope, &name), (values, comments.get(&path).cloned()));
            Ok(())
        }

        fn add_service(
            &mut self,
            s: &[u8],
            package: &str,
            index: i32,
            comments: &HashMap<Vec<i32>, String>,
        ) -> Result<(), Status> {
            let mut name = String::new();
            let mut methods = Vec::new();
            let mut r = Reader::new(s);
            while let Some((f, w)) = r.next_key()? {
                match (f, w) {
                    (1, wire::LEN) => name = r.read_string()?,
                    (2, wire::LEN) => methods.push(r.read_bytes()?),
                    _ => r.skip(w)?,
                }
            }
            let svc = full(package, &name);
            self.services
                .insert(svc.clone(), comments.get(&vec![6, index]).cloned());
            for (mi, m) in methods.into_iter().enumerate() {
                let (mut mname, mut input, mut output, mut cs, mut ss) =
                    (String::new(), String::new(), String::new(), false, false);
                let mut r = Reader::new(m);
                while let Some((f, w)) = r.next_key()? {
                    match (f, w) {
                        (1, wire::LEN) => mname = r.read_string()?,
                        (2, wire::LEN) => input = r.read_string()?,
                        (3, wire::LEN) => output = r.read_string()?,
                        (5, wire::VARINT) => cs = r.read_varint()? != 0,
                        (6, wire::VARINT) => ss = r.read_varint()? != 0,
                        _ => r.skip(w)?,
                    }
                }
                self.methods.insert(
                    format!("/{}/{}", svc, mname),
                    Method {
                        input: input.trim_start_matches('.').to_string(),
                        output: output.trim_start_matches('.').to_string(),
                        kind: MethodKind::from_streaming(cs, ss),
                        comment: comments.get(&vec![6, index, 2, mi as i32]).cloned(),
                    },
                );
            }
            Ok(())
        }
    }

    fn field(b: &[u8], comment: Option<String>) -> Result<Field, Status> {
        let mut f = Field {
            comment,
            ..Default::default()
        };
        let mut r = Reader::new(b);
        while let Some((n, w)) = r.next_key()? {
            match (n, w) {
                (1, wire::LEN) => f.name = r.read_string()?,
                (4, wire::VARINT) => f.label = r.read_varint()?,
                (5, wire::VARINT) => f.ty = r.read_varint()?,
                (6, wire::LEN) => {
                    f.type_name = r.read_string()?.trim_start_matches('.').to_string()
                }
                (9, wire::VARINT) => f.oneof_index = Some(r.read_varint()? as usize),
                (10, wire::LEN) => f.json_name = r.read_string()?,
                (17, wire::VARINT) => f.proto3_optional = r.read_varint()? != 0,
                _ => r.skip(w)?,
            }
        }
        if f.json_name.is_empty() {
            f.json_name = lower_camel(&f.name);
        }
        Ok(f)
    }

    /// The proto3 JSON mapping of a well-known type, if `name` is one.
    fn well_known(name: &str) -> Option<Value> {
        let t = name.strip_prefix("google.protobuf.")?;
        let wrapper = |ty: Value| json!({"type": [ty, "null"]});
        Some(match t {
            "Timestamp" => json!({"type": "string", "format": "date-time"}),
            "Duration" => json!({"type": "string", "pattern": "^-?[0-9]+(\\.[0-9]{1,9})?s$"}),
            "Struct" => json!({"type": "object", "additionalProperties": true}),
            "Value" => json!({}),
            "ListValue" => json!({"type": "array", "items": {}}),
            "NullValue" => json!({"type": "null"}),
            "Any" => json!({
                "type": "object",
                "properties": {"@type": {"type": "string"}},
                "required": ["@type"],
                "additionalProperties": true
            }),
            "FieldMask" => json!({"type": "string"}),
            "Empty" => json!({"type": "object"}),
            "DoubleValue" | "FloatValue" => wrapper("number".into()),
            "Int64Value" | "UInt64Value" => {
                json!({"type": ["string", "null"], "format": "int64"})
            }
            "Int32Value" | "UInt32Value" => wrapper("integer".into()),
            "BoolValue" => wrapper("boolean".into()),
            "StringValue" => wrapper("string".into()),
            "BytesValue" => json!({"type": ["string", "null"], "contentEncoding": "base64"}),
            _ => return None,
        })
    }

    fn reference(name: &str) -> Value {
        json!({"$ref": format!("#/components/schemas/{}", name)})
    }

    struct Builder<'a> {
        model: &'a Model,
        components: &'a mut Map<String, Value>,
        pending: Vec<String>,
        done: BTreeSet<String>,
    }

    impl Builder<'_> {
        /// The schema of a message or enum by its full name (a `$ref`, an inline well-known type, or any value
        /// when the type is not in the descriptors).
        fn named(&mut self, name: &str) -> Value {
            if let Some(v) = well_known(name) {
                return v;
            }
            if self.model.messages.contains_key(name) || self.model.enums.contains_key(name) {
                if self.done.insert(name.to_string()) {
                    self.pending.push(name.to_string());
                }
                return reference(name);
            }
            json!({"description": format!("{} (not in the descriptors)", name)})
        }

        fn scalar(&mut self, f: &Field) -> Value {
            match f.ty {
                T_DOUBLE => json!({"type": "number", "format": "double"}),
                T_FLOAT => json!({"type": "number", "format": "float"}),
                T_INT64 | T_SFIXED64 | T_SINT64 => json!({"type": "string", "format": "int64"}),
                T_UINT64 | T_FIXED64 => json!({"type": "string", "format": "uint64"}),
                T_INT32 | T_SFIXED32 | T_SINT32 => json!({"type": "integer", "format": "int32"}),
                T_UINT32 | T_FIXED32 => {
                    json!({"type": "integer", "format": "int64", "minimum": 0, "maximum": 4294967295u64})
                }
                T_BOOL => json!({"type": "boolean"}),
                T_STRING => json!({"type": "string"}),
                T_BYTES => json!({"type": "string", "contentEncoding": "base64"}),
                T_MESSAGE | T_GROUP | T_ENUM => {
                    let n = f.type_name.clone();
                    self.named(&n)
                }
                _ => json!({}),
            }
        }

        fn field(&mut self, f: &Field) -> Value {
            let entry = (f.ty == T_MESSAGE)
                .then(|| self.model.messages.get(&f.type_name))
                .flatten()
                .filter(|m| m.map_entry);
            let mut v = match entry {
                // map<K, V>: JSON のキーは常に文字列
                Some(m) if f.label == LABEL_REPEATED => {
                    let value = m.fields.iter().find(|x| x.name == "value");
                    let value = match value {
                        Some(v) => self.scalar(v),
                        None => json!({}),
                    };
                    json!({"type": "object", "additionalProperties": value})
                }
                _ => {
                    let item = self.scalar(f);
                    if f.label == LABEL_REPEATED {
                        json!({"type": "array", "items": item})
                    } else {
                        item
                    }
                }
            };
            if let (Some(c), Some(o)) = (&f.comment, v.as_object_mut()) {
                if o.contains_key("$ref") {
                    // 3.1 では $ref に兄弟のキーを置ける
                    o.insert("description".into(), c.clone().into());
                } else {
                    o.entry("description").or_insert_with(|| c.clone().into());
                }
            }
            v
        }

        fn message(&mut self, name: &str) -> Value {
            let model = self.model;
            if let Some((values, comment)) = model.enums.get(name) {
                let mut o = json!({"type": "string", "enum": values});
                if let Some(c) = comment {
                    o["description"] = c.clone().into();
                }
                return o;
            }
            let Some(m) = model.messages.get(name) else {
                return json!({});
            };
            let mut props = Map::new();
            // 本物の oneof（proto3 の optional の合成された oneof は除く）ごとのフィールド
            let mut groups: Vec<Vec<String>> = vec![Vec::new(); m.oneofs.len()];
            for f in &m.fields {
                props.insert(f.json_name.clone(), self.field(f));
                if let (Some(i), false) = (f.oneof_index, f.proto3_optional) {
                    if let Some(g) = groups.get_mut(i) {
                        g.push(f.json_name.clone());
                    }
                }
            }
            let mut o = Map::new();
            o.insert("type".into(), "object".into());
            if let Some(c) = &m.comment {
                o.insert("description".into(), c.clone().into());
            }
            o.insert("properties".into(), Value::Object(props));
            // oneof: どれか 1 つだけ、または 1 つも無い
            let one_ofs: Vec<Value> = groups
                .into_iter()
                .filter(|g| !g.is_empty())
                .map(|g| {
                    let each: Vec<Value> = g.iter().map(|n| json!({"required": [n]})).collect();
                    let mut alts = each.clone();
                    alts.push(json!({"not": {"anyOf": each}}));
                    Value::Array(alts)
                })
                .collect();
            match one_ofs.len() {
                0 => {}
                1 => {
                    o.insert("oneOf".into(), one_ofs.into_iter().next().unwrap());
                }
                _ => {
                    o.insert(
                        "allOf".into(),
                        Value::Array(one_ofs.into_iter().map(|a| json!({"oneOf": a})).collect()),
                    );
                }
            }
            Value::Object(o)
        }

        fn flush(&mut self) {
            while let Some(n) = self.pending.pop() {
                let s = self.message(&n);
                self.components.insert(n, s);
            }
        }
    }

    fn is_builtin(service: &str) -> bool {
        service.starts_with("grpc.health.") || service.starts_with("grpc.reflection.")
    }

    /// Adds the registered gRPC methods to `paths` (`POST /<package>.<Service>/<Method>` with `x-grpc`), their
    /// message schemas to `components` and one tag per service to `tags`.
    pub(crate) fn add_to_openapi(
        settings: &ApiSettings,
        entries: &[Arc<HandlerEntry>],
        paths: &mut Map<String, Value>,
        components: &mut Map<String, Value>,
        tags: &mut Vec<Value>,
    ) {
        if !settings.openapi_grpc {
            return;
        }
        let mut model = Model::default();
        for set in DESCRIPTORS.lock().unwrap().iter() {
            if let Err(e) = model.add_set(set) {
                ftlog::warn!(
                    "[api] a gRPC FileDescriptorSet is malformed; OpenAPI lists its methods without types: {}",
                    e.message()
                );
            }
        }
        let kinds = KINDS.lock().unwrap().clone();
        let reflection = REFLECTION.load(std::sync::atomic::Ordering::Relaxed);
        let mut b = Builder {
            model: &model,
            components,
            pending: Vec::new(),
            done: BTreeSet::new(),
        };
        let mut services = BTreeSet::new();
        for (path, (kind, explicit)) in &kinds {
            // 今の登録に残っているものだけ
            if !entries.iter().any(|e| e.pattern.as_str() == path) {
                continue;
            }
            let Some((service, method)) = path.strip_prefix('/').and_then(|p| p.split_once('/'))
            else {
                continue;
            };
            if is_builtin(service) && !settings.openapi_grpc_builtin {
                continue;
            }
            let desc = model.methods.get(path.as_str());
            let kind = match desc {
                Some(d) if !explicit => d.kind,
                _ => *kind,
            };
            let binary = json!({"type": "string", "format": "binary"});
            let (req, resp) = match desc {
                Some(d) => (b.named(&d.input), b.named(&d.output)),
                None => (binary.clone(), binary),
            };
            let tag = format!("gRPC: {}", service);
            let mut x = Map::new();
            x.insert("service".into(), service.into());
            x.insert("method".into(), method.into());
            x.insert("kind".into(), kind.as_str().into());
            if let Some(d) = desc {
                x.insert("requestType".into(), d.input.clone().into());
                x.insert("responseType".into(), d.output.clone().into());
            }
            if kind != MethodKind::Unary {
                x.insert("streaming".into(), true.into());
            }
            let mut op = Map::new();
            op.insert(
                "operationId".into(),
                path.trim_start_matches('/').replace(['/', '.'], "_").into(),
            );
            op.insert("tags".into(), json!([tag]));
            op.insert(
                "summary".into(),
                format!("{} ({})", method, kind.as_str().replace('_', " ")).into(),
            );
            if let Some(c) = desc.and_then(|d| d.comment.clone()) {
                op.insert("description".into(), c.into());
            }
            op.insert(
                "requestBody".into(),
                json!({"required": true, "content": {
                    "application/grpc": {"schema": req},
                    "application/grpc-web": {"schema": req},
                }}),
            );
            op.insert(
                "responses".into(),
                json!({"200": {
                    "description": "gRPC response (the status is in the grpc-status trailer)",
                    "content": {"application/grpc": {"schema": resp}},
                }}),
            );
            op.insert("x-grpc".into(), Value::Object(x));
            paths.insert(path.clone(), json!({"post": Value::Object(op)}));
            services.insert(service.to_string());
        }
        b.flush();
        for s in services {
            let mut d = model
                .services
                .get(&s)
                .cloned()
                .flatten()
                .unwrap_or_default();
            if reflection {
                if !d.is_empty() {
                    d.push_str("\n\n");
                }
                d.push_str(&format!(
                    "Server reflection is enabled: `grpcurl -plaintext <host:port> describe {}`",
                    s
                ));
            }
            let mut t = json!({"name": format!("gRPC: {}", s)});
            if !d.is_empty() {
                t["description"] = d.into();
            }
            tags.push(t);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use bytes::BytesMut;

        fn put_str(out: &mut BytesMut, f: u32, s: &str) {
            wire::put_bytes_field(out, f, s.as_bytes());
        }
        fn put_msg(out: &mut BytesMut, f: u32, m: &BytesMut) {
            wire::put_message_field(out, f, m);
        }
        fn put_u(out: &mut BytesMut, f: u32, v: u64) {
            wire::put_varint(out, ((f as u64) << 3) | wire::VARINT as u64);
            wire::put_varint(out, v);
        }
        fn fld(name: &str, label: u64, ty: u64, type_name: &str, oneof: Option<u64>) -> BytesMut {
            let mut f = BytesMut::new();
            put_str(&mut f, 1, name);
            put_u(&mut f, 4, label);
            put_u(&mut f, 5, ty);
            if !type_name.is_empty() {
                put_str(&mut f, 6, type_name);
            }
            if let Some(o) = oneof {
                put_u(&mut f, 9, o);
            }
            f
        }

        /// A file `t.proto` (package `t`) with the field kinds the mapping handles and a service.
        fn set() -> Vec<u8> {
            let mut file = BytesMut::new();
            put_str(&mut file, 1, "t.proto");
            put_str(&mut file, 2, "t");
            // message Req { int64 id = 1; repeated string tags = 2; map<string, int32> counts = 3;
            //   oneof pick { string a = 4; Req b = 5; } google.protobuf.Timestamp at = 6; Color color = 7;
            //   bytes blob = 8; optional bool flag = 9; }
            let mut req = BytesMut::new();
            put_str(&mut req, 1, "Req");
            for f in [
                fld("id", 1, T_INT64, "", None),
                fld("tags", 3, T_STRING, "", None),
                fld("counts", 3, T_MESSAGE, ".t.Req.CountsEntry", None),
                fld("a", 1, T_STRING, "", Some(0)),
                fld("b", 1, T_MESSAGE, ".t.Req", Some(0)),
                fld("at", 1, T_MESSAGE, ".google.protobuf.Timestamp", None),
                fld("color", 1, T_ENUM, ".t.Color", None),
                fld("blob", 1, T_BYTES, "", None),
                {
                    let mut f = fld("flag", 1, T_BOOL, "", Some(1));
                    put_u(&mut f, 17, 1);
                    f
                },
            ] {
                put_msg(&mut req, 2, &f);
            }
            let mut entry = BytesMut::new();
            put_str(&mut entry, 1, "CountsEntry");
            put_msg(&mut entry, 2, &fld("key", 1, T_STRING, "", None));
            put_msg(&mut entry, 2, &fld("value", 1, T_INT32, "", None));
            let mut opts = BytesMut::new();
            put_u(&mut opts, 7, 1);
            put_msg(&mut entry, 7, &opts);
            put_msg(&mut req, 3, &entry);
            let mut o = BytesMut::new();
            put_str(&mut o, 1, "pick");
            put_msg(&mut req, 8, &o);
            let mut o = BytesMut::new();
            put_str(&mut o, 1, "_flag");
            put_msg(&mut req, 8, &o);
            put_msg(&mut file, 4, &req);
            let mut reply = BytesMut::new();
            put_str(&mut reply, 1, "Reply");
            put_msg(&mut reply, 2, &fld("user_name", 1, T_STRING, "", None));
            put_msg(&mut file, 4, &reply);
            let mut color = BytesMut::new();
            put_str(&mut color, 1, "Color");
            for v in ["RED", "GREEN"] {
                let mut ev = BytesMut::new();
                put_str(&mut ev, 1, v);
                put_msg(&mut color, 2, &ev);
            }
            put_msg(&mut file, 5, &color);
            let mut svc = BytesMut::new();
            put_str(&mut svc, 1, "Svc");
            for (n, cs, ss) in [("Get", false, false), ("Watch", false, true)] {
                let mut m = BytesMut::new();
                put_str(&mut m, 1, n);
                put_str(&mut m, 2, ".t.Req");
                put_str(&mut m, 3, ".t.Reply");
                if cs {
                    put_u(&mut m, 5, 1);
                }
                if ss {
                    put_u(&mut m, 6, 1);
                }
                put_msg(&mut svc, 2, &m);
            }
            put_msg(&mut file, 6, &svc);
            // SourceCodeInfo: service 0 / method 0 のコメント
            let mut loc = BytesMut::new();
            let mut path = BytesMut::new();
            for p in [6u64, 0, 2, 0] {
                wire::put_varint(&mut path, p);
            }
            put_msg(&mut loc, 1, &path);
            put_str(&mut loc, 3, " Gets one.\n");
            let mut info = BytesMut::new();
            put_msg(&mut info, 1, &loc);
            put_msg(&mut file, 9, &info);
            let mut set = BytesMut::new();
            put_msg(&mut set, 1, &file);
            set.to_vec()
        }

        #[test]
        fn maps_proto3_json() {
            let mut model = Model::default();
            model.add_set(&set()).unwrap();
            let get = &model.methods["/t.Svc/Get"];
            assert_eq!((get.input.as_str(), get.kind), ("t.Req", MethodKind::Unary));
            assert_eq!(get.comment.as_deref(), Some("Gets one."));
            assert_eq!(
                model.methods["/t.Svc/Watch"].kind,
                MethodKind::ServerStreaming
            );
            let mut comps = Map::new();
            let mut b = Builder {
                model: &model,
                components: &mut comps,
                pending: Vec::new(),
                done: BTreeSet::new(),
            };
            assert_eq!(b.named("t.Req"), reference("t.Req"));
            b.flush();
            let req = &comps["t.Req"];
            let p = &req["properties"];
            assert_eq!(p["id"], json!({"type": "string", "format": "int64"}));
            assert_eq!(
                p["tags"],
                json!({"type": "array", "items": {"type": "string"}})
            );
            assert_eq!(
                p["counts"],
                json!({"type": "object", "additionalProperties": {"type": "integer", "format": "int32"}})
            );
            assert_eq!(p["b"], reference("t.Req"));
            assert_eq!(p["at"]["format"], "date-time");
            assert_eq!(p["color"], reference("t.Color"));
            assert_eq!(p["blob"]["contentEncoding"], "base64");
            assert_eq!(p["flag"], json!({"type": "boolean"}));
            // 本物の oneof（pick）だけ。proto3 の optional（_flag）は oneOf にしない
            let one = req["oneOf"].as_array().unwrap();
            assert_eq!(one.len(), 3);
            assert_eq!(one[0], json!({"required": ["a"]}));
            assert_eq!(
                comps["t.Color"],
                json!({"type": "string", "enum": ["RED", "GREEN"]})
            );
            // 参照されない型（CountsEntry）は載せない。json_name が無ければ lowerCamelCase
            assert!(!comps.contains_key("t.Req.CountsEntry"));
            let mut b = Builder {
                model: &model,
                components: &mut comps,
                pending: Vec::new(),
                done: BTreeSet::new(),
            };
            b.named("t.Reply");
            b.flush();
            assert!(comps["t.Reply"]["properties"].get("userName").is_some());
        }

        #[test]
        fn malformed_set_is_an_error() {
            assert!(Model::default().add_set(&[0x0a, 0x05, 0x01]).is_err());
        }

        fn entry(path: &str) -> Arc<HandlerEntry> {
            Arc::new(
                HandlerEntry::new(
                    path,
                    path,
                    &["POST", "OPTIONS"],
                    Arc::new(|_r| Box::pin(async { crate::api::response::Response::no_content() })),
                    crate::api::handler::OperationMeta {
                        hidden: true,
                        ..Default::default()
                    },
                )
                .unwrap(),
            )
        }

        #[test]
        fn openapi_lists_registered_grpc_methods() {
            let set = set();
            super::super::record_descriptors(&[&set]);
            super::super::record_kind("/t.Svc/Get", MethodKind::BidiStreaming);
            super::super::record_kind("/t.Svc/Watch", MethodKind::BidiStreaming);
            // 記述子の無いメソッド・組み込みのサービス・今の登録に無いメソッド
            super::super::record_kind("/t.Raw/Do", MethodKind::Unary);
            super::super::record_kind("/grpc.health.v1.Health/Check", MethodKind::Unary);
            super::super::record_kind("/t.Gone/Do", MethodKind::Unary);
            let entries: Vec<_> = [
                "/t.Svc/Get",
                "/t.Svc/Watch",
                "/t.Raw/Do",
                "/grpc.health.v1.Health/Check",
            ]
            .into_iter()
            .map(entry)
            .collect();
            let doc = crate::api::openapi::generate(&ApiSettings::from_value(None), &entries);
            let get = &doc["paths"]["/t.Svc/Get"]["post"];
            // 記述子の種類が登録の推測より優先される
            assert_eq!(get["x-grpc"]["kind"], "unary");
            assert_eq!(get["x-grpc"]["requestType"], "t.Req");
            assert_eq!(get["summary"], "Get (unary)");
            assert_eq!(get["description"], "Gets one.");
            assert_eq!(get["tags"], json!(["gRPC: t.Svc"]));
            assert_eq!(
                get["requestBody"]["content"]["application/grpc"]["schema"],
                reference("t.Req")
            );
            assert!(
                get.get("x-grpc").is_some() && doc["paths"]["/t.Svc/Get"].get("options").is_none()
            );
            assert_eq!(
                doc["paths"]["/t.Svc/Watch"]["post"]["x-grpc"]["streaming"],
                true
            );
            assert!(doc["components"]["schemas"].get("t.Req").is_some());
            assert_eq!(
                doc["paths"]["/t.Raw/Do"]["post"]["requestBody"]["content"]["application/grpc"]
                    ["schema"],
                json!({"type": "string", "format": "binary"})
            );
            assert!(doc["paths"].get("/grpc.health.v1.Health/Check").is_none());
            assert!(doc["paths"].get("/t.Gone/Do").is_none());
            assert!(doc["tags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "gRPC: t.Svc"));
            // [api] openapi_grpc_builtin = true で組み込みも、openapi_grpc = false で全部消える
            let v: toml::Value = toml::from_str("openapi_grpc_builtin = true").unwrap();
            let doc = crate::api::openapi::generate(&ApiSettings::from_value(Some(&v)), &entries);
            assert!(doc["paths"].get("/grpc.health.v1.Health/Check").is_some());
            let v: toml::Value = toml::from_str("openapi_grpc = false").unwrap();
            let doc = crate::api::openapi::generate(&ApiSettings::from_value(Some(&v)), &entries);
            assert!(doc["paths"].get("/t.Svc/Get").is_none() && doc.get("tags").is_none());
        }
    }
}
