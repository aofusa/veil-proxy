// F-189
//! OpenAPI 3.1 generation and Swagger UI (the `openapi` feature).
//!
//! The document is generated once from the metadata of the registered handlers at startup (and on every
//! reload) and kept as `Bytes`. At request time the kept `Bytes` is simply returned; nothing is generated on the hot path.

use super::app::ApiSettings;
use super::handler::{self, HandlerEntry, OperationMeta};
use super::request::Request;
use super::response::Response;
use bytes::Bytes;
use serde_json::{json, Map, Value};
use std::sync::{Arc, OnceLock, RwLock};

/// The name of the built-in handler (`is_builtin_enabled` shows or hides it according to `[api] openapi_enabled`).
pub const JSON_HANDLER: &str = "__veil_openapi_json";
/// The name of the built-in handler that serves Swagger UI.
pub const DOCS_HANDLER: &str = "__veil_openapi_docs";

static DOC: RwLock<Option<Bytes>> = RwLock::new(None);
static DOCS_HTML: RwLock<Option<super::swagger_ui::DocsPage>> = RwLock::new(None);
static REGISTERED: OnceLock<()> = OnceLock::new();

/// The current OpenAPI document (JSON).
pub fn document_json() -> Option<Bytes> {
    DOC.read().unwrap().clone()
}

/// Registers the built-in routes (the first time only) and regenerates the document.
pub(crate) fn refresh(settings: &ApiSettings) -> Result<(), String> {
    if REGISTERED.get().is_none() {
        let hidden = OperationMeta {
            hidden: true,
            ..Default::default()
        };
        handler::register(HandlerEntry::new(
            JSON_HANDLER,
            &settings.openapi_json_path,
            &["GET"],
            Arc::new(|_req: Request| {
                Box::pin(async {
                    match document_json() {
                        Some(b) => Response::json_bytes(b),
                        None => Response::not_found(),
                    }
                })
            }),
            hidden.clone(),
        )?)?;
        handler::register(HandlerEntry::new(
            DOCS_HANDLER,
            &settings.openapi_path,
            &["GET"],
            Arc::new(|_req: Request| {
                Box::pin(async {
                    match DOCS_HTML.read().unwrap().clone() {
                        // F-264: 同梱の資産を読むページは CSP 等のヘッダも付ける
                        Some(p) => p.headers.iter().fold(
                            Response::html(String::new()).with_body(p.html),
                            |r, (n, v)| r.with_header(*n, *v),
                        ),
                        None => Response::not_found(),
                    }
                })
            }),
            hidden,
        )?)?;
        super::swagger_ui::register_assets(settings)?;
        let _ = REGISTERED.set(());
    }
    let doc = generate(settings, &handler::all());
    *DOC.write().unwrap() = Some(Bytes::from(
        serde_json::to_vec(&doc).map_err(|e| e.to_string())?,
    ));
    *DOCS_HTML.write().unwrap() = super::swagger_ui::page(settings)?;
    Ok(())
}

/// Converts `{*rest}` to `{rest}` (an OpenAPI path template).
fn openapi_path(pattern: &str) -> String {
    pattern.replace("{*", "{")
}

/// The schemas of validation errors, in the same shape as FastAPI.
fn validation_error_schemas(components: &mut Map<String, Value>) {
    components.insert(
        "ValidationError".into(),
        json!({
            "type": "object",
            "title": "ValidationError",
            "required": ["loc", "msg", "type"],
            "properties": {
                "loc": {"type": "array", "items": {"anyOf": [{"type": "string"}, {"type": "integer"}]}},
                "msg": {"type": "string"},
                "type": {"type": "string"}
            }
        }),
    );
    components.insert(
        "HTTPValidationError".into(),
        json!({
            "type": "object",
            "title": "HTTPValidationError",
            "properties": {
                "detail": {"type": "array", "items": {"$ref": "#/components/schemas/ValidationError"}}
            }
        }),
    );
}

fn operation(
    meta: &OperationMeta,
    components: &mut Map<String, Value>,
    schemes: &mut Map<String, Value>,
) -> Value {
    if let Some(raw) = &meta.raw {
        // Python・C から渡された Operation Object（`x-veil-components` は components へ、
        // F-268: `x-veil-security-schemes` は components.securitySchemes へ移す）
        let mut op = raw.clone();
        if let Some(obj) = op.as_object_mut() {
            if let Some(Value::Object(extra)) = obj.remove("x-veil-components") {
                for (k, v) in extra {
                    components.insert(k, v);
                }
            }
            if let Some(Value::Object(extra)) = obj.remove("x-veil-security-schemes") {
                for (k, v) in extra {
                    schemes.insert(k, v);
                }
            }
            obj.entry("operationId")
                .or_insert_with(|| Value::String(meta.operation_id.clone()));
            if obj.contains_key("x-veil-websocket") && !obj.contains_key("responses") {
                obj.insert("responses".into(), websocket_responses());
            }
            if !obj.contains_key("responses") {
                obj.insert(
                    "responses".into(),
                    json!({"200": {"description": "Successful Response"}}),
                );
            }
        }
        return op;
    }
    for (k, v) in &meta.definitions {
        components.insert(k.clone(), v.clone());
    }
    let mut op = Map::new();
    op.insert("operationId".into(), meta.operation_id.clone().into());
    if let Some(s) = &meta.summary {
        op.insert("summary".into(), s.clone().into());
    }
    if let Some(d) = &meta.description {
        op.insert("description".into(), d.clone().into());
    }
    if !meta.tags.is_empty() {
        op.insert("tags".into(), json!(meta.tags));
    }
    if !meta.parameters.is_empty() {
        let params: Vec<Value> = meta
            .parameters
            .iter()
            .map(|p| {
                let mut m = Map::new();
                m.insert("name".into(), p.name.clone().into());
                m.insert("in".into(), p.location.into());
                m.insert("required".into(), p.required.into());
                m.insert("schema".into(), p.schema.clone());
                if let Some(d) = &p.description {
                    m.insert("description".into(), d.clone().into());
                }
                Value::Object(m)
            })
            .collect();
        op.insert("parameters".into(), Value::Array(params));
    }
    if let Some(b) = &meta.request_body {
        op.insert(
            "requestBody".into(),
            json!({ "required": b.required, "content": { b.content_type: { "schema": b.schema } } }),
        );
    }
    if let Some(ws) = &meta.websocket {
        websocket_operation(&mut op, ws);
        return Value::Object(op);
    }
    let mut responses = Map::new();
    if meta.responses.is_empty() {
        responses.insert("200".into(), json!({"description": "Successful Response"}));
    }
    for r in &meta.responses {
        let mut m = Map::new();
        m.insert("description".into(), r.description.clone().into());
        if let Some((ct, schema)) = &r.content {
            m.insert("content".into(), json!({ *ct: { "schema": schema } }));
        }
        responses.insert(r.status.to_string(), Value::Object(m));
    }
    if meta.validates {
        responses.insert(
            "422".into(),
            json!({
                "description": "Validation Error",
                "content": {"application/json": {"schema": {"$ref": "#/components/schemas/HTTPValidationError"}}}
            }),
        );
    }
    op.insert("responses".into(), Value::Object(responses));
    Value::Object(op)
}

// F-262: OpenAPI 3.1 に WebSocket の型は無いので、ハンドシェイク（GET → 101）を操作として書き、
// メッセージは拡張 `x-veil-websocket` で書く（設計は F-262 の 1.1）。
/// The responses of a WebSocket handshake.
fn websocket_responses() -> Value {
    json!({
        "101": {
            "description": "Switching Protocols (WebSocket)",
            "headers": {
                "Upgrade": {"schema": {"const": "websocket"}},
                "Sec-WebSocket-Accept": {"schema": {"type": "string"}},
                "Sec-WebSocket-Protocol": {"schema": {"type": "string"}}
            }
        },
        "400": {"description": "Not a valid WebSocket handshake"},
        "426": {"description": "Unsupported Sec-WebSocket-Version"}
    })
}

/// Fills the responses, the `Sec-WebSocket-Protocol` parameter and `x-veil-websocket` of a WebSocket route.
fn websocket_operation(op: &mut Map<String, Value>, ws: &handler::WebSocketMeta) {
    if !ws.subprotocols.is_empty() {
        let p = json!({
            "name": "Sec-WebSocket-Protocol",
            "in": "header",
            "required": false,
            "schema": {"type": "string", "enum": ws.subprotocols},
        });
        match op.get_mut("parameters").and_then(Value::as_array_mut) {
            Some(a) => a.push(p),
            None => {
                op.insert("parameters".into(), Value::Array(vec![p]));
            }
        }
    }
    op.insert("responses".into(), websocket_responses());
    let mut x = Map::new();
    if !ws.subprotocols.is_empty() {
        x.insert("subprotocols".into(), json!(ws.subprotocols));
    }
    x.insert(
        "maxMessageSize".into(),
        json!(super::ws::max_message_size()),
    );
    if let Some(r) = &ws.receive {
        x.insert("receive".into(), r.clone());
    }
    if let Some(s) = &ws.send {
        x.insert("send".into(), s.clone());
    }
    let format = ws
        .message_format
        .clone()
        .or_else(|| (ws.receive.is_some() || ws.send.is_some()).then(|| "text/json".to_string()));
    if let Some(f) = format {
        x.insert("messageFormat".into(), f.into());
    }
    op.insert("x-veil-websocket".into(), Value::Object(x));
    // Swagger UI では送れないので、接続の例を説明に足す
    let note =
        "WebSocket endpoint: connect with `new WebSocket(\"ws://HOST/PATH\")` in a browser or \
                `websocat ws://HOST/PATH`. Try it out is disabled for this operation.";
    let d = match op.get("description").and_then(Value::as_str) {
        Some(d) => format!("{}\n\n{}", d, note),
        None => note.to_string(),
    };
    op.insert("description".into(), d.into());
}

/// Generates the OpenAPI document from the registered handlers.
pub fn generate(settings: &ApiSettings, entries: &[Arc<HandlerEntry>]) -> Value {
    let mut paths = Map::new();
    let mut components = Map::new();
    let mut schemes = Map::new();
    validation_error_schemas(&mut components);
    for e in entries {
        if e.meta.hidden || e.name.starts_with("__veil_") {
            continue;
        }
        let is_ws = e.meta.websocket.is_some()
            || e.meta
                .raw
                .as_ref()
                .is_some_and(|r| r.get("x-veil-websocket").is_some());
        if is_ws && !settings.openapi_websocket {
            continue;
        }
        let op = operation(&e.meta, &mut components, &mut schemes);
        let item = paths
            .entry(openapi_path(e.pattern.as_str()))
            .or_insert_with(|| Value::Object(Map::new()));
        // WebSocket のルートはハンドシェイクの GET だけを載せる
        let methods: Vec<String> = if is_ws || e.methods.is_empty() {
            vec!["get".into()]
        } else {
            e.methods
                .iter()
                .filter(|m| *m != "HEAD")
                .map(|m| m.to_ascii_lowercase())
                .collect()
        };
        if let Some(obj) = item.as_object_mut() {
            for m in methods {
                obj.insert(m, op.clone());
            }
        }
    }
    let mut info = Map::new();
    info.insert("title".into(), settings.openapi_title.clone().into());
    info.insert("version".into(), settings.openapi_version.clone().into());
    if let Some(d) = &settings.openapi_description {
        info.insert("description".into(), d.clone().into());
    }
    let mut comps = Map::new();
    comps.insert("schemas".into(), Value::Object(components));
    if !schemes.is_empty() {
        comps.insert("securitySchemes".into(), Value::Object(schemes));
    }
    json!({
        "openapi": "3.1.0",
        "info": Value::Object(info),
        "paths": Value::Object(paths),
        "components": Value::Object(comps),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::handler::{BodyMeta, ParamMeta, ResponseMeta};

    #[test]
    fn generates_paths_params_body_and_422() {
        let mut defs = crate::api::schema::Definitions::new();
        defs.insert(
            "User".into(),
            json!({"type": "object", "properties": {"age": {"type": "integer", "minimum": 1.0}}}),
        );
        let meta = OperationMeta {
            operation_id: "create_user".into(),
            summary: Some("Create".into()),
            parameters: vec![ParamMeta {
                name: "id".into(),
                location: "path",
                required: true,
                schema: json!({"type": "integer"}),
                description: None,
            }],
            request_body: Some(BodyMeta {
                content_type: "application/json",
                schema: json!({"$ref": "#/components/schemas/User"}),
                required: true,
            }),
            responses: vec![ResponseMeta {
                status: 201,
                description: "Created".into(),
                content: None,
            }],
            definitions: defs,
            validates: true,
            ..Default::default()
        };
        let entry = Arc::new(
            HandlerEntry::new(
                "create_user",
                "/users/{id}/files/{*rest}",
                &["POST"],
                Arc::new(|_r| Box::pin(async { Response::no_content() })),
                meta,
            )
            .unwrap(),
        );
        let doc = generate(&ApiSettings::from_value(None), &[entry]);
        let op = &doc["paths"]["/users/{id}/files/{rest}"]["post"];
        assert_eq!(op["operationId"], "create_user");
        assert_eq!(op["parameters"][0]["in"], "path");
        assert_eq!(
            op["requestBody"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/User"
        );
        assert!(op["responses"]["201"].is_object());
        assert!(op["responses"]["422"].is_object());
        assert_eq!(
            doc["components"]["schemas"]["User"]["properties"]["age"]["minimum"],
            1.0
        );
        assert!(doc["components"]["schemas"]["HTTPValidationError"].is_object());
    }

    fn ws_entry(name: &str, path: &str, meta: OperationMeta) -> Arc<HandlerEntry> {
        Arc::new(
            HandlerEntry::new(
                name,
                path,
                &["GET"],
                Arc::new(|_r| Box::pin(async { Response::no_content() })),
                meta,
            )
            .unwrap(),
        )
    }

    #[test]
    fn websocket_routes() {
        let meta = OperationMeta {
            operation_id: "chat".into(),
            parameters: vec![ParamMeta {
                name: "room".into(),
                location: "query",
                required: false,
                schema: json!({"type": "string"}),
                description: None,
            }],
            websocket: Some(handler::WebSocketMeta {
                subprotocols: vec!["chat.v1".into()],
                receive: Some(json!({"$ref": "#/components/schemas/ChatIn"})),
                send: None,
                message_format: None,
            }),
            ..Default::default()
        };
        let raw = OperationMeta {
            operation_id: "c_ws".into(),
            raw: Some(json!({"summary": "C", "x-veil-websocket": {"messageFormat": "binary"}})),
            ..Default::default()
        };
        let entries = [ws_entry("chat", "/chat", meta), ws_entry("c_ws", "/c", raw)];
        let doc = generate(&ApiSettings::from_value(None), &entries);
        let op = &doc["paths"]["/chat"]["get"];
        assert_eq!(doc["paths"]["/chat"].as_object().unwrap().len(), 1);
        let mut st: Vec<&String> = op["responses"].as_object().unwrap().keys().collect();
        st.sort();
        assert_eq!(st, ["101", "400", "426"]);
        assert_eq!(
            op["responses"]["101"]["headers"]["Upgrade"]["schema"]["const"],
            "websocket"
        );
        assert_eq!(op["parameters"][0]["name"], "room");
        assert_eq!(op["parameters"][1]["name"], "Sec-WebSocket-Protocol");
        assert_eq!(op["parameters"][1]["schema"]["enum"][0], "chat.v1");
        let x = &op["x-veil-websocket"];
        assert_eq!(x["subprotocols"][0], "chat.v1");
        assert_eq!(x["receive"]["$ref"], "#/components/schemas/ChatIn");
        assert!(x.get("send").is_none());
        assert_eq!(x["messageFormat"], "text/json");
        assert!(x["maxMessageSize"].as_u64().unwrap() > 0);
        assert!(op["description"].as_str().unwrap().contains("WebSocket"));
        // C・Python の Operation Object も応答を補う
        let c = &doc["paths"]["/c"]["get"];
        assert!(c["responses"]["101"].is_object());
        assert_eq!(c["x-veil-websocket"]["messageFormat"], "binary");
        // openapi_websocket = false で載せない
        let v: toml::Value = toml::from_str("openapi_websocket = false").unwrap();
        let doc = generate(&ApiSettings::from_value(Some(&v)), &entries);
        assert!(doc["paths"].as_object().unwrap().is_empty());
    }

    // F-268: Python の Security 依存（x-veil-security-schemes）は components.securitySchemes へ
    #[test]
    fn raw_security_schemes_move_to_components() {
        let raw = OperationMeta {
            operation_id: "me".into(),
            raw: Some(json!({
                "summary": "Me",
                "security": [{"OAuth2PasswordBearer": ["read"]}],
                "x-veil-security-schemes": {
                    "OAuth2PasswordBearer": {"type": "oauth2", "flows": {"password": {"tokenUrl": "/token", "scopes": {}}}}
                }
            })),
            ..Default::default()
        };
        let plain = OperationMeta {
            operation_id: "plain".into(),
            ..Default::default()
        };
        let entries = [ws_entry("me", "/me", raw)];
        let doc = generate(&ApiSettings::from_value(None), &entries);
        let op = &doc["paths"]["/me"]["get"];
        assert!(op.get("x-veil-security-schemes").is_none());
        assert_eq!(op["security"][0]["OAuth2PasswordBearer"][0], "read");
        assert_eq!(
            doc["components"]["securitySchemes"]["OAuth2PasswordBearer"]["type"],
            "oauth2"
        );
        // 無ければ securitySchemes を出さない
        let doc = generate(
            &ApiSettings::from_value(None),
            &[ws_entry("plain", "/p", plain)],
        );
        assert!(doc["components"].get("securitySchemes").is_none());
    }
}
