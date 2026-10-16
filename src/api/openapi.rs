//! OpenAPI 3.1 の自動生成と Swagger UI（F-189、`openapi` feature）。
//!
//! 起動時（とリロードのたび）に登録済みハンドラのメタデータから 1 回だけ生成して `Bytes` で持つ。
//! リクエスト時は保持した `Bytes` を返すだけ（ホットパスで生成しない）。

use super::app::ApiSettings;
use super::handler::{self, HandlerEntry, OperationMeta};
use super::request::Request;
use super::response::Response;
use bytes::Bytes;
use serde_json::{json, Map, Value};
use std::sync::{Arc, OnceLock, RwLock};

/// 組み込みハンドラの名前（`is_builtin_enabled` が `[api] openapi_enabled` で出し分ける）。
pub const JSON_HANDLER: &str = "__veil_openapi_json";
/// Swagger UI を返す組み込みハンドラの名前。
pub const DOCS_HANDLER: &str = "__veil_openapi_docs";

static DOC: RwLock<Option<Bytes>> = RwLock::new(None);
static DOCS_HTML: RwLock<Option<Bytes>> = RwLock::new(None);
static REGISTERED: OnceLock<()> = OnceLock::new();

/// 現在の OpenAPI ドキュメント（JSON）。
pub fn document_json() -> Option<Bytes> {
    DOC.read().unwrap().clone()
}

/// 組み込みルートを登録し（初回のみ）、ドキュメントを作り直す。
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
                        Some(b) => Response::html(String::new()).with_body(b),
                        None => Response::not_found(),
                    }
                })
            }),
            hidden,
        )?)?;
        let _ = REGISTERED.set(());
    }
    let doc = generate(settings, &handler::all());
    *DOC.write().unwrap() = Some(Bytes::from(
        serde_json::to_vec(&doc).map_err(|e| e.to_string())?,
    ));
    *DOCS_HTML.write().unwrap() = Some(Bytes::from(swagger_ui_html(settings)));
    Ok(())
}

/// Swagger UI（swagger-ui-dist を CDN から読む）。
fn swagger_ui_html(settings: &ApiSettings) -> String {
    let title = html_escape(&settings.openapi_title);
    let url = serde_json::to_string(&settings.openapi_json_path)
        .unwrap_or_else(|_| "\"/openapi.json\"".into());
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} - Swagger UI</title>
<link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5/swagger-ui.css">
</head>
<body>
<div id="swagger-ui"></div>
<script src="https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js" crossorigin></script>
<script>
window.ui = SwaggerUIBundle({{ url: {url}, dom_id: "#swagger-ui", deepLinking: true }});
</script>
</body>
</html>
"##
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `{*rest}` → `{rest}`（OpenAPI のパステンプレート）。
fn openapi_path(pattern: &str) -> String {
    pattern.replace("{*", "{")
}

/// FastAPI と同じ形の検証エラーのスキーマ。
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

fn operation(meta: &OperationMeta, components: &mut Map<String, Value>) -> Value {
    if let Some(raw) = &meta.raw {
        // Python・C から渡された Operation Object（`x-veil-components` は components へ移す）
        let mut op = raw.clone();
        if let Some(obj) = op.as_object_mut() {
            if let Some(Value::Object(extra)) = obj.remove("x-veil-components") {
                for (k, v) in extra {
                    components.insert(k, v);
                }
            }
            obj.entry("operationId")
                .or_insert_with(|| Value::String(meta.operation_id.clone()));
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

/// 登録済みハンドラから OpenAPI ドキュメントを作る。
pub fn generate(settings: &ApiSettings, entries: &[Arc<HandlerEntry>]) -> Value {
    let mut paths = Map::new();
    let mut components = Map::new();
    validation_error_schemas(&mut components);
    for e in entries {
        if e.meta.hidden || e.name.starts_with("__veil_") {
            continue;
        }
        let op = operation(&e.meta, &mut components);
        let item = paths
            .entry(openapi_path(e.pattern.as_str()))
            .or_insert_with(|| Value::Object(Map::new()));
        let methods: Vec<String> = if e.methods.is_empty() {
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
    json!({
        "openapi": "3.1.0",
        "info": Value::Object(info),
        "paths": Value::Object(paths),
        "components": { "schemas": Value::Object(components) },
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
}
