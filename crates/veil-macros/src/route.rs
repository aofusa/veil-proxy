//! `#[veil::route(...)]`

use crate::util::{doc_text, first_generic, last_ident, lit_str, option_inner, parse_path_params};
use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::{
    parse::Parser, spanned::Spanned, Error, Expr, FnArg, ItemFn, Lit, Pat, ReturnType, Type,
};

#[derive(Default)]
struct RouteArgs {
    path: Option<(String, Span)>,
    methods: Vec<String>,
    name: Option<String>,
    tags: Vec<String>,
    summary: Option<String>,
    description: Option<String>,
    status: Option<u16>,
    offload: Option<bool>,
    hidden: bool,
    streaming: bool,
    // F-262: WebSocket のルートの OpenAPI（`x-veil-websocket`）
    ws_receive: Option<Type>,
    ws_send: Option<Type>,
    ws_protocols: Vec<String>,
}

fn str_list(e: &Expr) -> syn::Result<Vec<String>> {
    match e {
        Expr::Array(a) => a
            .elems
            .iter()
            .map(|x| lit_str(x).ok_or_else(|| Error::new(x.span(), "expected a string literal")))
            .collect(),
        other => lit_str(other)
            .map(|s| vec![s])
            .ok_or_else(|| Error::new(other.span(), "expected a string or an array of strings")),
    }
}

fn parse_args(attr: TokenStream) -> syn::Result<RouteArgs> {
    let mut a = RouteArgs::default();
    let parser = syn::meta::parser(|meta| {
        let key = meta
            .path
            .get_ident()
            .map(|i| i.to_string())
            .unwrap_or_default();
        // 型を取る引数（`Vec<T>` のような総称型は式として読めないので先に型として読む）
        if key == "ws_receive" || key == "ws_send" {
            let ty: Type = meta.value()?.parse()?;
            if key == "ws_receive" {
                a.ws_receive = Some(ty);
            } else {
                a.ws_send = Some(ty);
            }
            return Ok(());
        }
        let value: Expr = meta.value()?.parse()?;
        match key.as_str() {
            "path" => {
                let s = lit_str(&value).ok_or_else(|| Error::new(value.span(), "expected a string"))?;
                a.path = Some((s, value.span()));
            }
            "method" | "methods" => a.methods.extend(str_list(&value)?),
            "name" => a.name = lit_str(&value),
            "tags" | "tag" => a.tags.extend(str_list(&value)?),
            "summary" => a.summary = lit_str(&value),
            "description" => a.description = lit_str(&value),
            "ws_protocols" => a.ws_protocols.extend(str_list(&value)?),
            "status" => match &value {
                Expr::Lit(l) => match &l.lit {
                    Lit::Int(i) => a.status = Some(i.base10_parse()?),
                    _ => return Err(Error::new(value.span(), "expected an integer")),
                },
                _ => return Err(Error::new(value.span(), "expected an integer")),
            },
            "offload" | "hidden" | "streaming" => {
                let b = match &value {
                    Expr::Lit(l) => match &l.lit {
                        Lit::Bool(b) => b.value,
                        _ => return Err(Error::new(value.span(), "expected true or false")),
                    },
                    _ => return Err(Error::new(value.span(), "expected true or false")),
                };
                match key.as_str() {
                    "offload" => a.offload = Some(b),
                    "hidden" => a.hidden = b,
                    _ => a.streaming = b,
                }
            }
            other => {
                return Err(meta.error(format!(
                    "unknown route argument '{}' (expected path, method(s), name, tags, summary, description, status, offload, hidden, streaming, ws_receive, ws_send, ws_protocols)",
                    other
                )))
            }
        }
        Ok(())
    });
    parser.parse2(attr)?;
    Ok(a)
}

/// How one argument is handled.
enum ArgKind {
    RequestRef,
    RequestOwned,
    Json(Type),
    Query(Type),
    Form(Type),
    /// Extracted with `FromRequest` (State, Headers, Bytes and so on).
    FromRequest(Type),
    /// `WebSocketUpgrade` (extracted with `FromRequest`; the route is documented as a WebSocket endpoint).
    WebSocket(Type),
    PathParam(String, Type),
    QueryParam {
        name: String,
        ty: Type,
        optional: bool,
    },
}

fn classify(name: Option<&str>, ty: &Type, path_params: &[String]) -> ArgKind {
    if let Type::Reference(r) = ty {
        if last_ident(&r.elem).as_deref() == Some("Request") {
            return ArgKind::RequestRef;
        }
    }
    let last = last_ident(ty).unwrap_or_default();
    match last.as_str() {
        "Request" => return ArgKind::RequestOwned,
        "Json" | "Query" | "Form" => {
            if let Some(inner) = first_generic(ty) {
                let inner = inner.clone();
                return match last.as_str() {
                    "Json" => ArgKind::Json(inner),
                    "Query" => ArgKind::Query(inner),
                    _ => ArgKind::Form(inner),
                };
            }
        }
        "State" | "Headers" | "Bytes" | "Body" => return ArgKind::FromRequest(ty.clone()),
        "WebSocketUpgrade" => return ArgKind::WebSocket(ty.clone()),
        _ => {}
    }
    let name = name.unwrap_or("").trim_start_matches("r#").to_string();
    if path_params.contains(&name) {
        return ArgKind::PathParam(name, ty.clone());
    }
    match option_inner(ty) {
        Some(inner) => ArgKind::QueryParam {
            name,
            ty: inner.clone(),
            optional: true,
        },
        None => ArgKind::QueryParam {
            name,
            ty: ty.clone(),
            optional: false,
        },
    }
}

/// Infers the OpenAPI response schema from the return type (`Json<T>`, `Result<Json<T>, _>`, `(u16, Json<T>)` or a string).
fn response_content(ret: &ReturnType) -> TokenStream {
    let ty = match ret {
        ReturnType::Default => return quote!(None),
        ReturnType::Type(_, t) => &**t,
    };
    fn find(ty: &Type) -> Option<(TokenStream, Option<Type>)> {
        let last = last_ident(ty)?;
        match last.as_str() {
            "Json" => Some((quote!("application/json"), first_generic(ty).cloned())),
            "Result" | "Option" => first_generic(ty).and_then(find),
            "String" | "str" => Some((quote!("text/plain"), None)),
            "Html" => Some((quote!("text/html"), None)),
            _ => None,
        }
    }
    let found = match crate::util::strip(ty) {
        Type::Tuple(t) if t.elems.len() == 2 => find(&t.elems[1]),
        Type::Reference(r) => find(&r.elem),
        other => find(other),
    };
    match found {
        Some((ct, Some(t))) => quote! {
            Some((#ct, {
                use ::veil::api::__private::{ViaApiSchema as _, ViaNoSchema as _};
                (&::veil::api::__private::SchemaProbe::<#t>(::core::marker::PhantomData)).veil_schema(&mut __veil_defs)
            }))
        },
        Some((ct, None)) => {
            quote!(Some((#ct, ::veil::api::__private::serde_json::json!({"type": "string"}))))
        }
        None => quote!(None),
    }
}

pub fn expand(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let args = parse_args(attr)?;
    let func: ItemFn = syn::parse2(item)?;
    let (path, path_span) = args
        .path
        .clone()
        .ok_or_else(|| Error::new(Span::call_site(), "missing `path = \"...\"`"))?;
    let path_params = parse_path_params(&path).map_err(|m| Error::new(path_span, m))?;
    for m in &args.methods {
        if m.is_empty() || !m.bytes().all(|b| b.is_ascii_alphabetic()) {
            return Err(Error::new(
                Span::call_site(),
                format!("invalid HTTP method '{}'", m),
            ));
        }
    }
    let fn_ident = &func.sig.ident;
    let name = args.name.clone().unwrap_or_else(|| fn_ident.to_string());
    let is_async = func.sig.asyncness.is_some();
    let offload = args.offload.unwrap_or(!is_async);
    if offload && is_async {
        return Err(Error::new(
            func.sig.asyncness.span(),
            "`offload = true` requires a synchronous fn (an async handler already runs without blocking the event loop)",
        ));
    }
    if !func.sig.generics.params.is_empty() {
        return Err(Error::new(
            func.sig.generics.span(),
            "route handlers cannot be generic",
        ));
    }

    let handler_ident = format_ident!("__veil_route_handler_{}", fn_ident);
    let meta_ident = format_ident!("__veil_route_meta_{}", fn_ident);

    let mut extract = Vec::new();
    let mut call_args = Vec::new();
    let mut meta_stmts = Vec::new();
    let mut can_fail = false;
    let mut is_ws = false;
    for (i, input) in func.sig.inputs.iter().enumerate() {
        let FnArg::Typed(pt) = input else {
            return Err(Error::new(
                input.span(),
                "route handlers cannot take `self`",
            ));
        };
        let arg_name = match &*pt.pat {
            Pat::Ident(pi) => Some(pi.ident.to_string()),
            _ => None,
        };
        let var = format_ident!("__veil_arg{}", i);
        let ty = &*pt.ty;
        match classify(arg_name.as_deref(), ty, &path_params) {
            ArgKind::RequestRef => call_args.push(quote!(&__veil_req)),
            ArgKind::RequestOwned => call_args.push(quote!(__veil_req.clone())),
            ArgKind::Json(inner) | ArgKind::Form(inner) | ArgKind::Query(inner) => {
                can_fail = true;
                let last = last_ident(ty).unwrap();
                let (getter, location, wrap) = match last.as_str() {
                    "Json" => (quote!(json_body), "body", quote!(::veil::api::Json)),
                    "Form" => (quote!(form_body), "body", quote!(::veil::api::Form)),
                    _ => (quote!(query_struct), "query", quote!(::veil::api::Query)),
                };
                extract.push(quote! {
                    let #var: #inner = match ::veil::api::__private::#getter::<#inner>(&__veil_req) {
                        Ok(v) => v,
                        Err(r) => return r,
                    };
                    {
                        use ::veil::api::__private::{ViaValidate as _, ViaNoValidate as _};
                        if let Err(r) = (&::veil::api::__private::ValidateProbe(&#var)).veil_validate(#location) {
                            return r;
                        }
                    }
                });
                call_args.push(quote!(#wrap(#var)));
                if last == "Query" {
                    meta_stmts.push(quote! {
                        {
                            use ::veil::api::__private::{ViaApiSchema as _, ViaNoSchema as _};
                            let s = (&::veil::api::__private::SchemaProbe::<#inner>(::core::marker::PhantomData)).veil_schema(&mut __veil_defs);
                            __veil_params.extend(::veil::api::__private::query_params_from_schema(&s, &__veil_defs));
                        }
                    });
                } else {
                    let ct = if last == "Json" {
                        "application/json"
                    } else {
                        "application/x-www-form-urlencoded"
                    };
                    meta_stmts.push(quote! {
                        {
                            use ::veil::api::__private::{ViaApiSchema as _, ViaNoSchema as _};
                            let s = (&::veil::api::__private::SchemaProbe::<#inner>(::core::marker::PhantomData)).veil_schema(&mut __veil_defs);
                            __veil_body = Some(::veil::api::__private::BodyMeta { content_type: #ct, schema: s, required: true });
                        }
                    });
                }
            }
            ArgKind::FromRequest(t) | ArgKind::WebSocket(t) => {
                is_ws |= last_ident(&t).as_deref() == Some("WebSocketUpgrade");
                extract.push(quote! {
                    let #var: #t = match <#t as ::veil::api::__private::FromRequest>::from_request(&__veil_req) {
                        Ok(v) => v,
                        Err(r) => return r,
                    };
                });
                call_args.push(quote!(#var));
            }
            ArgKind::PathParam(pname, t) => {
                can_fail = true;
                extract.push(quote! {
                    let #var: #t = match ::veil::api::__private::path_param::<#t>(&__veil_req, #pname) {
                        Ok(v) => v,
                        Err(r) => return r,
                    };
                });
                call_args.push(quote!(#var));
                meta_stmts.push(quote! {
                    {
                        use ::veil::api::__private::{ViaApiSchema as _, ViaNoSchema as _};
                        let s = (&::veil::api::__private::SchemaProbe::<#t>(::core::marker::PhantomData)).veil_schema(&mut __veil_defs);
                        __veil_params.push(::veil::api::__private::ParamMeta {
                            name: #pname.into(), location: "path", required: true, schema: s, description: None,
                        });
                    }
                });
            }
            ArgKind::QueryParam {
                name: qname,
                ty: t,
                optional,
            } => {
                if qname.is_empty() {
                    return Err(Error::new(
                        pt.pat.span(),
                        "use a plain identifier for query/path parameters (or a typed extractor)",
                    ));
                }
                can_fail = true;
                if optional {
                    extract.push(quote! {
                        let #var: ::core::option::Option<#t> = match ::veil::api::__private::query_param_opt::<#t>(&__veil_req, #qname) {
                            Ok(v) => v,
                            Err(r) => return r,
                        };
                    });
                } else {
                    extract.push(quote! {
                        let #var: #t = match ::veil::api::__private::query_param::<#t>(&__veil_req, #qname) {
                            Ok(v) => v,
                            Err(r) => return r,
                        };
                    });
                }
                call_args.push(quote!(#var));
                let required = !optional;
                meta_stmts.push(quote! {
                    {
                        use ::veil::api::__private::{ViaApiSchema as _, ViaNoSchema as _};
                        let s = (&::veil::api::__private::SchemaProbe::<#t>(::core::marker::PhantomData)).veil_schema(&mut __veil_defs);
                        __veil_params.push(::veil::api::__private::ParamMeta {
                            name: #qname.into(), location: "query", required: #required, schema: s, description: None,
                        });
                    }
                });
            }
        }
    }

    let call = if offload {
        // 同期関数: 引数を取り出してからハンドラ用プールで実行する（引数・戻り値は Send が必要）。
        quote! {
            ::veil::api::__private::offload(move || #fn_ident(#(#call_args),*)).await
        }
    } else if is_async {
        quote! {
            ::veil::api::__private::IntoResponse::into_response(#fn_ident(#(#call_args),*).await)
        }
    } else {
        quote! {
            ::veil::api::__private::IntoResponse::into_response(#fn_ident(#(#call_args),*))
        }
    };
    let status_fix = match args.status {
        Some(st) => quote! {
            if __veil_resp.status_code() == 200 { __veil_resp = __veil_resp.with_status(#st); }
        },
        None => quote!(),
    };

    let methods = &args.methods;
    let doc = doc_text(&func.attrs);
    let (doc_summary, doc_desc) = match &doc {
        Some(d) => {
            let mut it = d.splitn(2, "\n\n");
            let first = it.next().unwrap_or("").replace('\n', " ");
            (Some(first), it.next().map(|s| s.to_string()))
        }
        None => (None, None),
    };
    let summary = opt_string(args.summary.clone().or(doc_summary));
    let description = opt_string(args.description.clone().or(doc_desc));
    let tags = &args.tags;
    let hidden = args.hidden;
    let streaming = args.streaming;
    let resp_content = response_content(&func.sig.output);
    let ok_status = args.status.unwrap_or(200);
    if !is_ws
        && (args.ws_receive.is_some() || args.ws_send.is_some() || !args.ws_protocols.is_empty())
    {
        return Err(Error::new(
            Span::call_site(),
            "`ws_receive`, `ws_send` and `ws_protocols` need a `WebSocketUpgrade` argument",
        ));
    }
    let ws_schema = |t: &Option<Type>| match t {
        Some(t) => quote! {
            Some({
                use ::veil::api::__private::{ViaApiSchema as _, ViaNoSchema as _};
                (&::veil::api::__private::SchemaProbe::<#t>(::core::marker::PhantomData)).veil_schema(&mut __veil_defs)
            })
        },
        None => quote!(None),
    };
    let websocket = if is_ws {
        let recv = ws_schema(&args.ws_receive);
        let send = ws_schema(&args.ws_send);
        let protos = &args.ws_protocols;
        quote! {
            Some(::veil::api::__private::WebSocketMeta {
                subprotocols: ::veil::api::__private::Vec::from([#(::veil::api::__private::String::from(#protos)),*]),
                receive: #recv,
                send: #send,
                message_format: None,
            })
        }
    } else {
        quote!(None)
    };
    let ok_desc = if ok_status == 204 {
        "No Content"
    } else {
        "Successful Response"
    };

    Ok(quote! {
        #func

        #[doc(hidden)]
        #[allow(non_snake_case, unused_mut, unused_variables, clippy::all)]
        fn #handler_ident(__veil_req: ::veil::api::__private::Request) -> ::veil::api::__private::HandlerFuture {
            ::veil::api::__private::Box::pin(async move {
                #(#extract)*
                let mut __veil_resp: ::veil::api::__private::Response = #call;
                #status_fix
                __veil_resp
            })
        }

        #[doc(hidden)]
        #[allow(non_snake_case, unused_mut, clippy::all)]
        fn #meta_ident() -> ::veil::api::__private::OperationMeta {
            let mut __veil_defs = ::veil::api::__private::Definitions::new();
            let mut __veil_params: ::veil::api::__private::Vec<::veil::api::__private::ParamMeta> = ::veil::api::__private::Vec::new();
            let mut __veil_body: ::core::option::Option<::veil::api::__private::BodyMeta> = None;
            #(#meta_stmts)*
            let __veil_content = #resp_content;
            let __veil_ws: ::core::option::Option<::veil::api::__private::WebSocketMeta> = #websocket;
            ::veil::api::__private::OperationMeta {
                operation_id: #name.into(),
                summary: #summary,
                description: #description,
                tags: ::veil::api::__private::Vec::from([#(::veil::api::__private::String::from(#tags)),*]),
                parameters: __veil_params,
                request_body: __veil_body,
                responses: ::veil::api::__private::Vec::from([::veil::api::__private::ResponseMeta {
                    status: #ok_status,
                    description: #ok_desc.into(),
                    content: __veil_content,
                }]),
                definitions: __veil_defs,
                validates: #can_fail,
                hidden: #hidden,
                raw: None,
                websocket: __veil_ws,
            }
        }

        ::veil::api::__private::inventory::submit! {
            ::veil::api::__private::RouteDef {
                name: #name,
                path: #path,
                methods: &[#(#methods),*],
                handler: #handler_ident,
                meta: #meta_ident,
                streaming_request: #streaming,
            }
        }
    })
}

fn opt_string(v: Option<String>) -> TokenStream {
    match v {
        Some(s) => quote!(Some(::veil::api::__private::String::from(#s))),
        None => quote!(None),
    }
}
