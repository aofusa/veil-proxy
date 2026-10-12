//! veil API サーバ基盤ライブラリ（F-188〜F-192）。`handler` feature（`api` 等）のときだけ存在する。
//!
//! veil のネットワーク層・TLS・HTTP/1.1・2・3・独自ランタイムの上で、Rust の関数をハンドラとして
//! 登録して API サーバを書く。ルートは `#[veil::route]` マクロ（`macro` feature）か `App` で登録し、
//! 内部的には `type = "Handler"` の `[[route]]` として既存のルーティングに載る。
//!
//! ```no_run
//! use veil::prelude::*;
//!
//! #[veil::route(path = "/users/{id}", method = "GET")]
//! async fn get_user(id: u64) -> Json<serde_json::Value> {
//!     Json(serde_json::json!({ "id": id }))
//! }
//!
//! #[veil::main]
//! async fn main() -> Result<(), veil::api::Error> {
//!     let config = Config::load()?.builder().listen_cleartext("0.0.0.0:8080").build();
//!     veil::serve(config).await
//! }
//! ```
//!
//! 設計: `docs/backlog/features/F-188-api-library-core.md`。利用者向けガイド: `docs/guide/library.md`。

pub mod app;
pub mod config;
pub(crate) mod dispatch;
pub mod extract;
#[cfg(feature = "grpc-handler")]
pub mod grpc;
pub mod handler;
pub mod pattern;
pub mod request;
pub mod response;
pub(crate) mod routes;
pub mod schema;
pub mod stream;

#[cfg(feature = "openapi")]
pub mod openapi;

#[cfg(feature = "tower-compat")]
pub mod tower;

#[cfg(any(feature = "tokio-compat", feature = "tokio-compat-bridge"))]
pub mod tokio_compat;

#[cfg(feature = "graphql")]
pub mod graphql;

#[cfg(feature = "l4-handler")]
pub mod l4;

pub use app::{
    block_on, serve, serve_blocking, serve_with_router, settings, shutdown, state, ApiSettings,
    App, Error, Router,
};
pub use config::{Config, ConfigBuilder, ConfigError};
pub use extract::{Body, Form, FromRequest, Headers, Json, Query, State};
pub use handler::{HandlerEntry, HandlerFn, HandlerFuture, OperationMeta};
pub use request::{HttpVersion, Request};
pub use response::{ApiError, Html, IntoResponse, Response};
pub use schema::{ApiSchema, Definitions, FieldError, Validate, ValidationErrors};
pub use stream::{BodyChunk, BodyStream, RequestBodyStream};

/// よく使う型をまとめて取り込む（`use veil::prelude::*;`）。
pub mod prelude {
    pub use super::app::{serve, serve_blocking, serve_with_router, shutdown, App, Router};
    pub use super::config::{Config, ConfigBuilder};
    pub use super::extract::{Body, Form, Headers, Json, Query, State};
    #[cfg(feature = "l4-handler")]
    pub use super::l4::{L4Connection, L4Datagram, L4Error};
    pub use super::request::Request;
    pub use super::response::{ApiError, Html, IntoResponse, Response};
    pub use super::schema::{ApiSchema, Validate};
    pub use super::stream::{BodyChunk, RequestBodyStream};
    #[cfg(feature = "macro")]
    pub use veil_macros::Schema;
}

/// 組み込みのハンドラ（`__veil_` で始まる名前）を、現在の設定で公開するか。
pub(crate) fn is_builtin_enabled(entry: &HandlerEntry, settings: &app::ApiSettings) -> bool {
    if !entry.name.starts_with("__veil_openapi") {
        return true;
    }
    settings.openapi_enabled
}

/// マクロの生成コードが使う内部 API（利用者は直接使わない。互換性は保証しない）。
#[doc(hidden)]
pub mod __private {
    pub use super::extract::{
        form_body, json_body, path_param, query_param, query_param_opt, query_struct, FromRequest,
    };
    pub use super::handler::{
        BodyMeta, HandlerFuture, OperationMeta, ParamMeta, ResponseMeta, RouteDef,
    };
    pub use super::request::Request;
    pub use super::response::{IntoResponse, Response};
    pub use super::schema::{
        check_email, check_length, check_range, check_url, schema_ref, with_constraints, ApiSchema,
        AsLength, AsNumber, AsText, Definitions, Validate, ValidationErrors,
    };
    pub use bytes::Bytes;
    pub use inventory;
    pub use serde_json;
    pub use std::boxed::Box;
    pub use std::string::String;
    pub use std::vec::Vec;

    #[cfg(feature = "l4-handler")]
    pub use super::l4::{L4Connection, L4Datagram, L4Error, L4HandlerDef, L4UdpHandlerDef};

    use serde_json::Value;
    use std::marker::PhantomData;

    /// autoref 特殊化で「`T: ApiSchema` ならそのスキーマ、そうでなければ汎用」を選ぶ。
    /// 呼び出しは `(&SchemaProbe::<T>(..)).veil_schema(..)`: 受け手 `&Probe` に対し、境界付きの impl
    /// （`Self = Probe`、`&self`）が最初の探索段で一致し、満たさなければ自動参照の段で
    /// 予備の impl（`Self = &Probe`）が選ばれる。
    pub struct SchemaProbe<T: ?Sized>(pub PhantomData<T>);

    pub trait ViaApiSchema {
        fn veil_schema(&self, defs: &mut Definitions) -> Value;
        fn veil_optional(&self) -> bool;
    }
    impl<T: ApiSchema + ?Sized> ViaApiSchema for SchemaProbe<T> {
        fn veil_schema(&self, defs: &mut Definitions) -> Value {
            T::schema(defs)
        }
        fn veil_optional(&self) -> bool {
            T::is_optional()
        }
    }
    pub trait ViaNoSchema {
        fn veil_schema(&self, defs: &mut Definitions) -> Value;
        fn veil_optional(&self) -> bool;
    }
    impl<T: ?Sized> ViaNoSchema for &SchemaProbe<T> {
        fn veil_schema(&self, _defs: &mut Definitions) -> Value {
            serde_json::json!({})
        }
        fn veil_optional(&self) -> bool {
            false
        }
    }

    /// autoref 特殊化で「`T: Validate` なら検証、そうでなければ何もしない」を選ぶ。
    pub struct ValidateProbe<'a, T: ?Sized>(pub &'a T);

    pub trait ViaValidate {
        fn veil_validate(&self, location: &str) -> Result<(), Response>;
        fn veil_validates(&self) -> bool;
    }
    impl<T: Validate + ?Sized> ViaValidate for ValidateProbe<'_, T> {
        fn veil_validate(&self, location: &str) -> Result<(), Response> {
            self.0
                .validate()
                .map_err(|e| e.prefixed(location).into_response())
        }
        fn veil_validates(&self) -> bool {
            true
        }
    }
    pub trait ViaNoValidate {
        fn veil_validate(&self, location: &str) -> Result<(), Response>;
        fn veil_validates(&self) -> bool;
    }
    impl<T: ?Sized> ViaNoValidate for &ValidateProbe<'_, T> {
        fn veil_validate(&self, _location: &str) -> Result<(), Response> {
            Ok(())
        }
        fn veil_validates(&self) -> bool {
            false
        }
    }

    /// 型だけで「Validate を実装しているか」を調べる（OpenAPI の 422 記載用）。
    pub struct ValidateTypeProbe<T: ?Sized>(pub PhantomData<T>);
    pub trait ViaValidateType {
        fn veil_validates(&self) -> bool;
    }
    impl<T: Validate + ?Sized> ViaValidateType for ValidateTypeProbe<T> {
        fn veil_validates(&self) -> bool {
            true
        }
    }
    pub trait ViaNoValidateType {
        fn veil_validates(&self) -> bool;
    }
    impl<T: ?Sized> ViaNoValidateType for &ValidateTypeProbe<T> {
        fn veil_validates(&self) -> bool {
            false
        }
    }

    /// `Query<T>` の T のスキーマ（オブジェクト）を、OpenAPI のクエリパラメータの並びに展開する。
    pub fn query_params_from_schema(schema: &Value, defs: &Definitions) -> Vec<ParamMeta> {
        let resolved = match schema.get("$ref").and_then(|r| r.as_str()) {
            Some(r) => defs
                .get(r.trim_start_matches("#/components/schemas/"))
                .cloned()
                .unwrap_or(Value::Null),
            None => schema.clone(),
        };
        let required: Vec<&str> = resolved
            .get("required")
            .and_then(|r| r.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        let mut out = Vec::new();
        if let Some(props) = resolved.get("properties").and_then(|p| p.as_object()) {
            for (name, s) in props {
                out.push(ParamMeta {
                    name: name.clone(),
                    location: "query",
                    required: required.contains(&name.as_str()),
                    description: s
                        .get("description")
                        .and_then(|d| d.as_str())
                        .map(|d| d.to_string()),
                    schema: s.clone(),
                });
            }
        }
        out
    }

    /// ブロッキングなハンドラ本体をハンドラ用プールで実行する（`offload = true`）。
    pub async fn offload<F, R>(f: F) -> Response
    where
        F: FnOnce() -> R + Send + 'static,
        R: IntoResponse + Send + 'static,
    {
        match crate::runtime::offload::offload_handler(move || f().into_response()).await {
            Some(r) => r,
            None => Response::error(500, "handler panicked"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::__private::*;
    use super::schema::ValidationErrors;
    use serde_json::Value;

    struct Checked(u32);
    impl super::Validate for Checked {
        fn validate_at(&self, loc: &mut Vec<Value>, errors: &mut ValidationErrors) {
            if self.0 > 10 {
                errors.push(loc, "too_big", "too big");
            }
        }
    }
    impl super::ApiSchema for Checked {
        fn schema(_: &mut Definitions) -> Value {
            serde_json::json!({"type": "integer", "maximum": 10})
        }
    }
    struct Plain;

    /// autoref 特殊化が「実装していれば使う・無ければ予備」を正しく選ぶ（マクロの生成コードと同じ呼び方）。
    // 理由付き allow: `(&Probe).method()` の明示的な借用こそが検証対象（予備の impl は `&Probe` にある）。
    #[allow(clippy::needless_borrow)]
    #[test]
    fn probes_pick_the_bounded_impl_when_available() {
        let mut defs = Definitions::new();
        {
            use ViaApiSchema as _;
            use ViaNoSchema as _;
            let s = (&SchemaProbe::<Checked>(std::marker::PhantomData)).veil_schema(&mut defs);
            assert_eq!(s["maximum"], 10);
            let s = (&SchemaProbe::<Plain>(std::marker::PhantomData)).veil_schema(&mut defs);
            assert_eq!(s, serde_json::json!({}));
            let s = (&SchemaProbe::<u8>(std::marker::PhantomData)).veil_schema(&mut defs);
            assert_eq!(s["type"], "integer");
        }
        {
            use ViaNoValidate as _;
            use ViaValidate as _;
            assert!((&ValidateProbe(&Checked(11)))
                .veil_validate("body")
                .is_err());
            assert!((&ValidateProbe(&Checked(1))).veil_validate("body").is_ok());
            assert!((&ValidateProbe(&Plain)).veil_validate("body").is_ok());
        }
    }
}
