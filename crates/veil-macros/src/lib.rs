//! veil API ライブラリの手続きマクロ（F-188 / F-189 / F-192）。
//!
//! 利用者は `veil` クレート経由（`#[veil::route]`・`#[veil::main]`・`#[derive(veil::Schema)]`・
//! `#[veil::l4_handler]`）で使う。生成コードは `::veil::api::__private` 経由で依存を参照するため、
//! 利用者側に `inventory`・`serde_json` 等の依存追加は不要。
use proc_macro::TokenStream;

mod l4;
mod main_attr;
mod route;
mod schema;
mod util;

/// HTTP ルートを宣言し、関数をハンドラとして登録する。
///
/// ```ignore
/// #[veil::route(path = "/users/{id}", method = "GET")]
/// async fn get_user(id: u64, verbose: Option<bool>) -> Json<User> { ... }
/// ```
///
/// - `path`（必須）: `{name}` は 1 セグメント、末尾の `{*name}` は残り全部。
/// - `method = "GET"` / `methods = ["GET", "POST"]`（省略時は全メソッド）。
/// - `name`（既定は関数名）・`tags = ["users"]`・`summary`・`description`・`status = 201`・
///   `offload = true`（同期関数は既定で offload）・`hidden = true`（OpenAPI に載せない）。
///
/// 引数は型と名前で解釈する（FastAPI と同じ規則）: `&Request`/`Request`、`Json<T>`（本文）、
/// `Query<T>`、`Form<T>`、`State<T>`、`Headers`、`Bytes`、パスの `{name}` と同名の引数はパス
/// パラメータ、それ以外の単純な型はクエリパラメータ（`Option<T>` なら省略可）。`Json`・`Query`・
/// `Form` の中身が `#[derive(veil::Schema)]` なら自動でバリデーションし、失敗は 422。
#[proc_macro_attribute]
pub fn route(attr: TokenStream, item: TokenStream) -> TokenStream {
    route::expand(attr.into(), item.into())
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

/// L4（TCP/UDP）ハンドラを登録する（`l4-handler` feature）。
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

/// `async fn main()` を veil の起動に使えるようにする（`veil::serve(config).await` を書ける）。
#[proc_macro_attribute]
pub fn main(attr: TokenStream, item: TokenStream) -> TokenStream {
    main_attr::expand(attr.into(), item.into())
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

/// 構造体・列挙に JSON Schema（OpenAPI）と自動バリデーションを実装する。
///
/// `#[validate(range(min = 1, max = 100))]`・`length(min, max, equal)`・`email`・`url`・`nested`・
/// `custom = "path::to::fn"`（`fn(&T) -> Result<(), impl Display>`）を受け、`#[schema(description,
/// example, format, pattern)]` と doc コメント、serde の `rename`・`rename_all`・`default`・`skip` を尊重する。
#[proc_macro_derive(Schema, attributes(validate, schema))]
pub fn derive_schema(item: TokenStream) -> TokenStream {
    schema::expand(item.into())
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}
