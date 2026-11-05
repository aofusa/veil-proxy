// F-291: GraphQL のトランスポート（Rust の kit の graphql::mount の C 版）。実行器は C のコールバック（C の GraphQL
// ライブラリに渡す）。HTTP の規則は Rust の kit と同じ: POST は JSON だけ（415）・バッチ可、GET はクエリ文字列で
// mutation は 405（B-196 と同じ。操作の種類は軽い字句解析で判定）、Accept で application/graphql-response+json、
// text/event-stream は GraphQL over SSE（結果 1 個 + complete）、ブラウザの GET は GraphiQL（有効にしたとき）。
// WebSocket のサブスクリプションは実行器の結果の列が要るので、アップグレード要求を C のハンドラへ回す
// （veil_ws_accept_ex で graphql-transport-ws を受ける）。
//! A GraphQL over HTTP transport whose executor is a C callback.

use crate::{tryc, UserData};
use bytes::Bytes;
use serde_json::{Map, Value};
use std::ffi::{c_char, c_int, c_void, CString};
use std::ptr;
use veil::api::{Request, Response};
use veil_capi::internal::{self, cstr, fail, read_sized};
use veil_capi::{
    veil_config_t, veil_handler_fn, veil_register_route_ex, veil_request_t, veil_response_t,
    VEIL_ERR_INVALID_ARG, VEIL_OK,
};

const GRAPHQL_RESPONSE: &str = "application/graphql-response+json";

/// One GraphQL operation handed to the executor (valid during the callback).
#[allow(non_camel_case_types)]
pub struct veil_kit_graphql_op_t {
    query: String,
    operation_name: Option<String>,
    /// JSON のテキスト（無ければ `{}`）
    variables: String,
    extensions: String,
    req: *const veil_request_t,
    result: Option<Bytes>,
}

/// The executor: run `op` and set its result with `veil_kit_graphql_set_result`; return 0. A non-zero return (or
/// no result) answers `{"data": null, "errors": [{"message": "Internal Server Error"}]}`.
#[allow(non_camel_case_types)]
pub type veil_kit_graphql_exec_fn =
    unsafe extern "C" fn(op: *mut veil_kit_graphql_op_t, user_data: *mut c_void) -> c_int;

/// Options of `veil_kit_graphql` (zero-filled means defaults; `size` is `sizeof(veil_kit_graphql_options_t)` or 0).
#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy)]
pub struct veil_kit_graphql_options_t {
    pub size: usize,
    /// Serve GraphiQL to a browser `GET` without a query (tri-state: default = off). The page loads its scripts
    /// from a CDN.
    pub graphiql: c_int,
    /// Accept batches (a JSON array of requests; tri-state: default = on).
    pub batch: c_int,
    /// Called for WebSocket upgrade requests on the path (subscriptions: accept `graphql-transport-ws` with
    /// `veil_ws_accept_ex`); NULL answers them with 400.
    pub upgrade: Option<veil_handler_fn>,
    pub upgrade_user_data: *mut c_void,
}

struct Endpoint {
    path: String,
    exec: veil_kit_graphql_exec_fn,
    ud: UserData,
    graphiql: bool,
    batch: bool,
    upgrade: Option<veil_handler_fn>,
    upgrade_ud: UserData,
}

/// Serves GraphQL at `path` (`GET` and `POST`), calling `exec` on the handler pool for each operation. C
/// middleware of the path applies (authentication guards, for example). The endpoint lives as long as the process.
///
/// # Safety
///
/// `cfg` is a valid configuration handle, `path` a NUL-terminated string, `opts` NULL or a valid
/// `veil_kit_graphql_options_t`; `user_data` (and `upgrade_user_data`) follow the rules of `veil_register_route`.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_graphql(
    cfg: *mut veil_config_t,
    path: *const c_char,
    exec: Option<veil_kit_graphql_exec_fn>,
    user_data: *mut c_void,
    opts: *const veil_kit_graphql_options_t,
) -> c_int {
    let Some(exec) = exec else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL executor");
    };
    let path_s = tryc!(cstr(path));
    let o = if opts.is_null() {
        None
    } else {
        Some(tryc!(read_sized(opts, "veil_kit_graphql_options_t")))
    };
    let ep = Box::new(Endpoint {
        path: path_s.to_string(),
        exec,
        ud: UserData(user_data),
        graphiql: o.is_some_and(|o| o.graphiql > 0),
        batch: o.is_none_or(|o| o.batch >= 0),
        upgrade: o.and_then(|o| o.upgrade),
        upgrade_ud: UserData(o.map(|o| o.upgrade_user_data).unwrap_or(ptr::null_mut())),
    });
    let meta = CString::new(
        r#"{"summary":"GraphQL endpoint","tags":["graphql"],"responses":{"200":{"description":"GraphQL response"}}}"#,
    )
    .expect("static JSON");
    let ud = Box::into_raw(ep);
    let rc = veil_register_route_ex(
        cfg,
        path,
        c"GET,POST".as_ptr(),
        Some(serve),
        ud as *mut c_void,
        0,
        meta.as_ptr(),
    );
    if rc != VEIL_OK {
        drop(Box::from_raw(ud));
    }
    rc
}

fn accepts(req: &Request, media: &str) -> bool {
    req.header("accept").is_some_and(|a| a.contains(media))
}

fn json_content_type(ct: Option<&str>) -> bool {
    let Some(ct) = ct else { return false };
    let media = ct.split(';').next().unwrap_or("").trim();
    [
        "application/json",
        GRAPHQL_RESPONSE,
        "application/graphql+json",
    ]
    .iter()
    .any(|m| media.eq_ignore_ascii_case(m))
}

/// GraphQL の要求 1 個（JSON のオブジェクト）を取り出す。
fn parse_one(v: &Value) -> Result<(String, Option<String>, String, String), String> {
    let o = v
        .as_object()
        .ok_or("a GraphQL request must be a JSON object")?;
    let query = o
        .get("query")
        .and_then(Value::as_str)
        .ok_or("the request has no query")?
        .to_string();
    let name = match o.get("operationName") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        _ => return Err("operationName must be a string".into()),
    };
    let obj = |k: &str| -> Result<String, String> {
        match o.get(k) {
            None | Some(Value::Null) => Ok("{}".into()),
            Some(v @ Value::Object(_)) => Ok(v.to_string()),
            _ => Err(format!("{} must be an object", k)),
        }
    };
    Ok((query, name, obj("variables")?, obj("extensions")?))
}

/// GET のクエリ文字列から要求を作る（variables・extensions は JSON のテキスト）。
fn parse_get(req: &Request) -> Result<(String, Option<String>, String, String), String> {
    let query = req
        .query_param("query")
        .ok_or("the request has no query")?
        .into_owned();
    let mut o = Map::new();
    o.insert("query".into(), Value::String(query));
    if let Some(n) = req.query_param("operationName") {
        o.insert("operationName".into(), Value::String(n.into_owned()));
    }
    for k in ["variables", "extensions"] {
        if let Some(t) = req.query_param(k) {
            let v: Value =
                serde_json::from_str(&t).map_err(|e| format!("{} is not JSON: {}", k, e))?;
            o.insert(k.into(), v);
        }
    }
    parse_one(&Value::Object(o))
}

unsafe extern "C" fn serve(
    req: *const veil_request_t,
    resp: *mut veil_response_t,
    ud: *mut c_void,
) -> c_int {
    // SAFETY: veil_kit_graphql で登録した Endpoint（プロセスの間ずっと生きる）
    let ep = &*(ud as *const Endpoint);
    let Some(r) = internal::request(req) else {
        return -1;
    };
    if r.upgrade_protocol()
        .is_some_and(|p| p.eq_ignore_ascii_case(b"websocket"))
    {
        return match ep.upgrade {
            Some(f) => f(req, resp, ep.upgrade_ud.get()),
            None => {
                set(
                    resp,
                    Response::bad_request("GraphQL over WebSocket is not enabled"),
                );
                0
            }
        };
    }
    let answer = handle(ep, r, req);
    set(resp, answer);
    0
}

unsafe fn set(resp: *mut veil_response_t, r: Response) {
    internal::update_response(resp, |_| r);
}

type Parsed = (String, Option<String>, String, String);

unsafe fn handle(ep: &Endpoint, r: &Request, req: *const veil_request_t) -> Response {
    let (ops, batch): (Vec<Parsed>, bool) = match r.method() {
        "GET" | "HEAD" => {
            if r.query().is_empty() && accepts(r, "text/html") {
                return graphiql(ep);
            }
            match parse_get(r) {
                Ok(op) => {
                    // B-196: GET で mutation を実行しない
                    if is_mutation(&op.0, op.1.as_deref()) {
                        return Response::error(405, "mutations require POST")
                            .with_header("allow", "POST");
                    }
                    (vec![op], false)
                }
                Err(e) => return Response::bad_request(&e),
            }
        }
        "POST" => {
            if !json_content_type(r.header("content-type")) {
                return Response::error(415, "GraphQL requests must be application/json");
            }
            let v: Value = match serde_json::from_slice(r.body()) {
                Ok(v) => v,
                Err(e) => return Response::bad_request(&format!("invalid GraphQL request: {}", e)),
            };
            match v {
                Value::Array(items) if ep.batch && !items.is_empty() => {
                    match items.iter().map(parse_one).collect::<Result<Vec<_>, _>>() {
                        Ok(ops) => (ops, true),
                        Err(e) => {
                            return Response::bad_request(&format!(
                                "invalid GraphQL request: {}",
                                e
                            ))
                        }
                    }
                }
                Value::Array(_) => {
                    return Response::bad_request(
                        "invalid GraphQL request: batches are not accepted",
                    )
                }
                other => match parse_one(&other) {
                    Ok(op) => (vec![op], false),
                    Err(e) => {
                        return Response::bad_request(&format!("invalid GraphQL request: {}", e))
                    }
                },
            }
        }
        _ => return Response::error(405, "Method Not Allowed"),
    };
    let sse = accepts(r, "text/event-stream");
    if sse && batch {
        return Response::bad_request("GraphQL over SSE takes a single request");
    }
    let mut failed = false;
    let mut results: Vec<Bytes> = Vec::with_capacity(ops.len());
    for (query, operation_name, variables, extensions) in ops {
        let mut op = veil_kit_graphql_op_t {
            query,
            operation_name,
            variables,
            extensions,
            req,
            result: None,
        };
        let rc = (ep.exec)(&mut op, ep.ud.get());
        match op.result {
            Some(b) if rc == 0 => results.push(b),
            _ => {
                failed = true;
                results.push(Bytes::from_static(
                    br#"{"data":null,"errors":[{"message":"Internal Server Error"}]}"#,
                ));
            }
        }
    }
    if sse {
        let mut body = Vec::new();
        body.extend_from_slice(b"event: next\ndata: ");
        body.extend_from_slice(&results[0]);
        body.extend_from_slice(b"\n\nevent: complete\ndata:\n\n");
        return Response::new(200)
            .with_header("content-type", "text/event-stream")
            .with_header("cache-control", "no-cache")
            .with_body(body);
    }
    let ct = if accepts(r, GRAPHQL_RESPONSE) {
        GRAPHQL_RESPONSE
    } else {
        "application/json"
    };
    let body: Bytes = if batch {
        let mut b = Vec::new();
        b.push(b'[');
        for (i, x) in results.iter().enumerate() {
            if i > 0 {
                b.push(b',');
            }
            b.extend_from_slice(x);
        }
        b.push(b']');
        b.into()
    } else {
        results.pop().unwrap_or_default()
    };
    let status = if failed && !batch { 500 } else { 200 };
    Response::new(status)
        .with_header("content-type", ct)
        .with_body(body)
}

fn graphiql(ep: &Endpoint) -> Response {
    if !ep.graphiql {
        return Response::error(404, "GraphiQL is not enabled");
    }
    let endpoint = serde_json::to_string(&ep.path).unwrap_or_else(|_| "\"/\"".into());
    let html = GRAPHIQL.replace("__ENDPOINT__", &endpoint.replace("</", "<\\/"));
    Response::html(html)
}

const GRAPHIQL: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>GraphiQL</title>
<link rel="stylesheet" href="https://unpkg.com/graphiql@3.8.3/graphiql.min.css">
<style>html, body, #graphiql { height: 100%; margin: 0; }</style>
</head>
<body>
<div id="graphiql">Loading...</div>
<script crossorigin src="https://unpkg.com/react@18.3.1/umd/react.production.min.js"></script>
<script crossorigin src="https://unpkg.com/react-dom@18.3.1/umd/react-dom.production.min.js"></script>
<script crossorigin src="https://unpkg.com/graphiql@3.8.3/graphiql.min.js"></script>
<script>
const fetcher = GraphiQL.createFetcher({ url: __ENDPOINT__ });
ReactDOM.createRoot(document.getElementById('graphiql')).render(React.createElement(GraphiQL, { fetcher }));
</script>
</body>
</html>
"#;

/// The query text (not NUL-terminated).
///
/// # Safety
///
/// `op` is the operation of the running executor; `len` NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_graphql_query(
    op: *const veil_kit_graphql_op_t,
    len: *mut usize,
) -> *const c_char {
    match op.as_ref() {
        Some(o) => crate::out_bytes(o.query.as_bytes(), len),
        None => crate::out_none(len),
    }
}

/// The `operationName`, or NULL when absent.
///
/// # Safety
///
/// As for `veil_kit_graphql_query`.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_graphql_operation_name(
    op: *const veil_kit_graphql_op_t,
    len: *mut usize,
) -> *const c_char {
    match op.as_ref().and_then(|o| o.operation_name.as_ref()) {
        Some(n) => crate::out_bytes(n.as_bytes(), len),
        None => crate::out_none(len),
    }
}

/// The `variables` as JSON text (`{}` when absent).
///
/// # Safety
///
/// As for `veil_kit_graphql_query`.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_graphql_variables(
    op: *const veil_kit_graphql_op_t,
    len: *mut usize,
) -> *const c_char {
    match op.as_ref() {
        Some(o) => crate::out_bytes(o.variables.as_bytes(), len),
        None => crate::out_none(len),
    }
}

/// The `extensions` as JSON text (`{}` when absent).
///
/// # Safety
///
/// As for `veil_kit_graphql_query`.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_graphql_extensions(
    op: *const veil_kit_graphql_op_t,
    len: *mut usize,
) -> *const c_char {
    match op.as_ref() {
        Some(o) => crate::out_bytes(o.extensions.as_bytes(), len),
        None => crate::out_none(len),
    }
}

/// The HTTP request (headers, locals, authentication results of the middleware).
///
/// # Safety
///
/// As for `veil_kit_graphql_query`.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_graphql_request(
    op: *const veil_kit_graphql_op_t,
) -> *const veil_request_t {
    op.as_ref().map(|o| o.req).unwrap_or(ptr::null())
}

/// Sets the result: a GraphQL response as a JSON object (`{"data": ...}` and/or `{"errors": [...]}`; copied).
/// `VEIL_ERR_INVALID_ARG` when it is not a JSON object.
///
/// # Safety
///
/// As for `veil_kit_graphql_query`; `json` has `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_graphql_set_result(
    op: *mut veil_kit_graphql_op_t,
    json: *const c_char,
    len: usize,
) -> c_int {
    let (Some(o), false) = (op.as_mut(), json.is_null()) else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL operation or result");
    };
    let b = std::slice::from_raw_parts(json as *const u8, len);
    match serde_json::from_slice::<Value>(b) {
        Ok(Value::Object(_)) => {
            o.result = Some(Bytes::copy_from_slice(b));
            VEIL_OK
        }
        _ => fail(VEIL_ERR_INVALID_ARG, "the result must be a JSON object"),
    }
}

// ===================== 操作の種類の判定 =====================

#[derive(PartialEq, Debug)]
enum Tok<'a> {
    Name(&'a str),
    Punct(u8),
}

/// GraphQL の字句（空白・カンマ・コメント・文字列・数を読み飛ばし、名前と区切りだけを返す）。
fn tokens(src: &str) -> Vec<Tok<'_>> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' | b',' => i += 1,
            b'#' => {
                while i < b.len() && b[i] != b'\n' && b[i] != b'\r' {
                    i += 1;
                }
            }
            b'"' => {
                if b[i..].starts_with(b"\"\"\"") {
                    i += 3;
                    while i < b.len() && !b[i..].starts_with(b"\"\"\"") {
                        if b[i..].starts_with(b"\\\"\"\"") {
                            i += 4;
                        } else {
                            i += 1;
                        }
                    }
                    i += 3;
                } else {
                    i += 1;
                    while i < b.len() && b[i] != b'"' && b[i] != b'\n' {
                        i += if b[i] == b'\\' { 2 } else { 1 };
                    }
                    i += 1;
                }
            }
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => {
                let s = i;
                while i < b.len() && (b[i] == b'_' || b[i].is_ascii_alphanumeric()) {
                    i += 1;
                }
                out.push(Tok::Name(&src[s..i]));
            }
            b'-' | b'0'..=b'9' => {
                i += 1;
                while i < b.len()
                    && (b[i].is_ascii_alphanumeric()
                        || b[i] == b'.'
                        || b[i] == b'+'
                        || b[i] == b'-')
                {
                    i += 1;
                }
            }
            _ => {
                out.push(Tok::Punct(c));
                i += 1;
            }
        }
    }
    out
}

/// The operations of a document: `(name, kind)` (`kind` is `query`, `mutation` or `subscription`).
fn operations(src: &str) -> Vec<(Option<&str>, &str)> {
    let toks = tokens(src);
    let mut ops = Vec::new();
    let (mut depth, mut parens) = (0usize, 0usize);
    // 定義の頭（キーワード）を読んで選択集合をまだ始めていない
    let mut pending = false;
    let mut k = 0;
    while k < toks.len() {
        match &toks[k] {
            Tok::Punct(b'{') => {
                if depth == 0 && parens == 0 && !pending {
                    ops.push((None, "query")); // 省略形
                }
                if depth == 0 && parens == 0 {
                    pending = false;
                }
                depth += 1;
            }
            Tok::Punct(b'}') => depth = depth.saturating_sub(1),
            Tok::Punct(b'(') => parens += 1,
            Tok::Punct(b')') => parens = parens.saturating_sub(1),
            Tok::Name(n) if depth == 0 && parens == 0 && !pending => match *n {
                "query" | "mutation" | "subscription" => {
                    let name = match toks.get(k + 1) {
                        Some(Tok::Name(x)) => {
                            k += 1;
                            Some(*x)
                        }
                        _ => None,
                    };
                    ops.push((name, *n));
                    pending = true;
                }
                _ => pending = true, // fragment 等
            },
            _ => {}
        }
        k += 1;
    }
    ops
}

/// Whether the operation a request would run is a mutation (operations named `operation_name`, or all of them).
fn is_mutation(query: &str, operation_name: Option<&str>) -> bool {
    operations(query)
        .iter()
        .filter(|(n, _)| operation_name.is_none_or(|want| *n == Some(want)))
        .any(|(_, kind)| *kind == "mutation")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_kinds() {
        assert!(!is_mutation("{ a }", None));
        assert!(!is_mutation(
            "query Q($x: In = {a: 1}) { a(x: $x) { b } }",
            None
        ));
        assert!(is_mutation("mutation { add(a: 1) }", None));
        assert!(is_mutation("# mutation\n mutation M { x }", None));
        assert!(!is_mutation(r#"query { f(s: "mutation { x }") }"#, None));
        assert!(!is_mutation("\"\"\" mutation \"\"\" query { x }", None));
        let doc = "query A { a } mutation B { b } fragment F on T { c }";
        assert!(!is_mutation(doc, Some("A")));
        assert!(is_mutation(doc, Some("B")));
        assert!(is_mutation(doc, None));
        assert_eq!(
            operations("fragment F on T { x } subscription S { y }"),
            vec![(Some("S"), "subscription")]
        );
    }

    #[test]
    fn get_requests_parse() {
        let r = Request::new(
            "GET",
            "/graphql?query=%7Ba%7D&variables=%7B%22x%22%3A1%7D",
            Vec::new(),
            "",
        );
        let (q, n, v, e) = parse_get(&r).unwrap();
        assert_eq!(
            (q.as_str(), n, v.as_str(), e.as_str()),
            ("{a}", None, r#"{"x":1}"#, "{}")
        );
        assert!(parse_one(&serde_json::json!({"query": "{a}", "variables": [1]})).is_err());
    }
}
