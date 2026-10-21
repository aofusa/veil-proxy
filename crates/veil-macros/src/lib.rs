// F-188 / F-189 / F-192
//! Procedural macros for the veil API library.
//!
//! Use them through the `veil` crate (`#[veil::route]`, `#[veil::main]`,
//! `#[derive(veil::Schema)]` and `#[veil::l4_handler]`). The generated code refers to its
//! dependencies through `::veil::api::__private`, so your crate does not need to add
//! dependencies such as `inventory` or `serde_json`.
// F-241: docs.rs にソースの表示を出さない（コメントは日本語のため）。
#![doc(html_no_source)]
use proc_macro::TokenStream;

mod l4;
mod main_attr;
mod route;
mod schema;
mod util;

/// Declares an HTTP route and registers the function as its handler.
///
/// ```ignore
/// #[veil::route(path = "/users/{id}", method = "GET")]
/// async fn get_user(id: u64, verbose: Option<bool>) -> Json<User> { ... }
/// ```
///
/// - `path` (required): `{name}` matches one segment, and a trailing `{*name}` matches the rest.
/// - `method = "GET"` / `methods = ["GET", "POST"]` (all methods if omitted).
/// - `name` (defaults to the function name), `tags = ["users"]`, `summary`, `description`,
///   `status = 201`, `offload = true` (synchronous functions are offloaded by default) and
///   `hidden = true` (leave the route out of OpenAPI).
///
/// Arguments are interpreted by type and name, with the same rules as FastAPI: `&Request` /
/// `Request`, `Json<T>` (body), `Query<T>`, `Form<T>`, `State<T>`, `Headers` and `Bytes`. An
/// argument named like a `{name}` in the path is a path parameter, and any other simple type is
/// a query parameter (optional if it is an `Option<T>`). If the inner type of `Json`, `Query`
/// or `Form` derives `veil::Schema`, it is validated automatically and a failure returns 422.
#[proc_macro_attribute]
pub fn route(attr: TokenStream, item: TokenStream) -> TokenStream {
    route::expand(attr.into(), item.into())
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

/// Registers an L4 (TCP/UDP) handler (`l4-handler` feature).
///
/// ```ignore
/// #[veil::l4_handler(name = "echo", protocol = "tcp")]
/// async fn echo(conn: &mut L4Connection) -> Result<(), L4Error> { ... }
///
/// #[veil::l4_handler(name = "udp_echo", protocol = "udp")]
/// async fn udp_echo(dgram: L4Datagram) -> Option<bytes::Bytes> { Some(dgram.data) }
/// ```
#[proc_macro_attribute]
pub fn l4_handler(attr: TokenStream, item: TokenStream) -> TokenStream {
    l4::expand(attr.into(), item.into())
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

/// Lets `async fn main()` start veil (so you can write `veil::serve(config).await`).
#[proc_macro_attribute]
pub fn main(attr: TokenStream, item: TokenStream) -> TokenStream {
    main_attr::expand(attr.into(), item.into())
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

/// Implements JSON Schema (OpenAPI) generation and automatic validation for a struct or enum.
///
/// It accepts `#[validate(range(min = 1, max = 100))]`, `length(min, max, equal)`, `email`,
/// `url`, `nested` and `custom = "path::to::fn"` (`fn(&T) -> Result<(), impl Display>`), and
/// `#[schema(description, example, format, pattern)]`. Doc comments and the serde attributes
/// `rename`, `rename_all`, `default` and `skip` are respected.
#[proc_macro_derive(Schema, attributes(validate, schema))]
pub fn derive_schema(item: TokenStream) -> TokenStream {
    schema::expand(item.into())
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}
