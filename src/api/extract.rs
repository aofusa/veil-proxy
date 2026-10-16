//! ハンドラ引数の抽出子（F-188）。`#[veil::route]` の生成コードと手書きのハンドラの両方で使う。

use super::request::Request;
use super::response::Response;
use super::schema::ValidationErrors;
use bytes::Bytes;
use serde_json::Value;
use std::sync::Arc;

pub use super::response::Json;

/// クエリ文字列をまとめてデシリアライズする（`?page=1&q=x` → `T`）。自動バリデーション対象。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Query<T>(pub T);

/// `application/x-www-form-urlencoded` の本文。自動バリデーション対象。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Form<T>(pub T);

/// `App::state` で登録した共有状態。
#[derive(Debug)]
pub struct State<T>(pub Arc<T>);

impl<T> Clone for State<T> {
    fn clone(&self) -> Self {
        State(self.0.clone())
    }
}

/// 生の本文。
pub type Body = Bytes;

/// 全リクエストヘッダ（受信順）。
#[derive(Debug, Clone)]
pub struct Headers(pub Arc<Vec<(Bytes, Bytes)>>);

macro_rules! deref {
    ($($t:ident),*) => {$(
        impl<T> std::ops::Deref for $t<T> {
            type Target = T;
            fn deref(&self) -> &T { &self.0 }
        }
    )*};
}
deref!(Query, Form);

impl<T> std::ops::Deref for State<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl Headers {
    /// ヘッダの値を UTF-8 文字列として返す（名前は大文字小文字を区別しない）。無い・UTF-8 でない場合は `None`。
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
            .and_then(|(_, v)| std::str::from_utf8(v).ok())
    }
}

/// リクエストから値を取り出す（失敗したらその応答を返す）。
pub trait FromRequest: Sized {
    /// `req` から値を取り出す。取り出せなければ、そのまま返す応答（400・422 等）を `Err` で返す。
    fn from_request(req: &Request) -> Result<Self, Response>;
}

impl FromRequest for Request {
    fn from_request(req: &Request) -> Result<Self, Response> {
        Ok(req.clone())
    }
}

impl FromRequest for Bytes {
    fn from_request(req: &Request) -> Result<Self, Response> {
        Ok(req.body.clone())
    }
}

impl FromRequest for String {
    fn from_request(req: &Request) -> Result<Self, Response> {
        String::from_utf8(req.body.to_vec()).map_err(|_| {
            single_error(
                "body",
                None,
                "string_unicode",
                "Input should be a valid UTF-8 string",
            )
        })
    }
}

impl FromRequest for Headers {
    fn from_request(req: &Request) -> Result<Self, Response> {
        Ok(Headers(std::sync::Arc::new(req.headers.clone())))
    }
}

impl<T: Send + Sync + 'static> FromRequest for State<T> {
    fn from_request(req: &Request) -> Result<Self, Response> {
        req.state::<T>().map(State).ok_or_else(|| {
            Response::error(
                500,
                &format!(
                    "state of type {} is not registered (App::state)",
                    std::any::type_name::<T>()
                ),
            )
        })
    }
}

/// 1 件だけの 422 応答。
pub(crate) fn single_error(location: &str, name: Option<&str>, kind: &str, msg: &str) -> Response {
    let mut e = ValidationErrors::default();
    let mut loc = vec![Value::from(location)];
    if let Some(n) = name {
        loc.push(Value::from(n));
    }
    e.push(&loc, kind, msg);
    e.into_response()
}

/// パスパラメータを `FromStr` で取り出す（失敗は 422、`loc = ["path", name]`）。
pub fn path_param<T: std::str::FromStr>(req: &Request, name: &str) -> Result<T, Response> {
    let raw = req
        .param(name)
        .ok_or_else(|| single_error("path", Some(name), "missing", "Field required"))?;
    raw.parse::<T>().map_err(|_| {
        single_error(
            "path",
            Some(name),
            "parsing",
            &format!("Input should be a valid {}", type_label::<T>()),
        )
    })
}

/// 必須のクエリパラメータ（欠落・変換失敗は 422、`loc = ["query", name]`）。
pub fn query_param<T: std::str::FromStr>(req: &Request, name: &str) -> Result<T, Response> {
    match query_param_opt::<T>(req, name)? {
        Some(v) => Ok(v),
        None => Err(single_error(
            "query",
            Some(name),
            "missing",
            "Field required",
        )),
    }
}

/// 省略可能なクエリパラメータ。
pub fn query_param_opt<T: std::str::FromStr>(
    req: &Request,
    name: &str,
) -> Result<Option<T>, Response> {
    match req.query_param(name) {
        None => Ok(None),
        Some(raw) => raw.parse::<T>().map(Some).map_err(|_| {
            single_error(
                "query",
                Some(name),
                "parsing",
                &format!("Input should be a valid {}", type_label::<T>()),
            )
        }),
    }
}

/// JSON 本文をデシリアライズする（JSON の構文・型エラーは 422、FastAPI と同じ扱い）。
pub fn json_body<T: serde::de::DeserializeOwned>(req: &Request) -> Result<T, Response> {
    if req.body.is_empty() {
        return Err(single_error("body", None, "missing", "Field required"));
    }
    serde_json::from_slice::<T>(&req.body).map_err(|e| {
        let kind = if e.is_syntax() || e.is_eof() {
            "json_invalid"
        } else {
            "model_attributes_type"
        };
        single_error("body", None, kind, &e.to_string())
    })
}

/// クエリ文字列全体を `T` にデシリアライズする。
pub fn query_struct<T: serde::de::DeserializeOwned>(req: &Request) -> Result<T, Response> {
    serde_urlencoded::from_bytes::<T>(&req.query)
        .map_err(|e| single_error("query", None, "value_error", &e.to_string()))
}

/// フォーム本文を `T` にデシリアライズする。
pub fn form_body<T: serde::de::DeserializeOwned>(req: &Request) -> Result<T, Response> {
    serde_urlencoded::from_bytes::<T>(&req.body)
        .map_err(|e| single_error("body", None, "value_error", &e.to_string()))
}

fn type_label<T>() -> &'static str {
    let n = std::any::type_name::<T>();
    match n {
        "i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "u8" | "u16" | "u32" | "u64" | "u128"
        | "usize" => "integer",
        "f32" | "f64" => "number",
        "bool" => "boolean",
        _ => n.rsplit("::").next().unwrap_or(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn param_and_query_extraction() {
        let req = Request::new("GET", "/x?page=2&bad=z", vec![], "")
            .with_params(vec![(Arc::from("id"), Bytes::from_static(b"7"))]);
        assert_eq!(path_param::<u32>(&req, "id").unwrap(), 7);
        assert_eq!(query_param::<u32>(&req, "page").unwrap(), 2);
        assert_eq!(query_param_opt::<u32>(&req, "none").unwrap(), None);
        let r = query_param::<u32>(&req, "bad").unwrap_err();
        assert_eq!(r.status_code(), 422);
        let v: Value = serde_json::from_slice(r.body()).unwrap();
        assert_eq!(v["detail"][0]["loc"], serde_json::json!(["query", "bad"]));
        assert_eq!(v["detail"][0]["msg"], "Input should be a valid integer");
        assert_eq!(
            query_param::<u32>(&req, "missing")
                .unwrap_err()
                .status_code(),
            422
        );
    }

    #[test]
    fn json_body_errors_are_422() {
        let req = Request::new("POST", "/x", vec![], "{bad");
        let r = json_body::<Value>(&req).unwrap_err();
        assert_eq!(r.status_code(), 422);
        let req = Request::new("POST", "/x", vec![], "{\"a\": 1}");
        assert_eq!(json_body::<Value>(&req).unwrap()["a"], 1);
    }
}
