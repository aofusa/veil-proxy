//! `#[veil::l4_handler(name = "...", protocol = "tcp" | "udp")]`

use crate::util::lit_str;
use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::{parse::Parser, spanned::Spanned, Error, Expr, ItemFn};

pub fn expand(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let mut name: Option<String> = None;
    let mut protocol = "tcp".to_string();
    let parser = syn::meta::parser(|meta| {
        let v: Expr = meta.value()?.parse()?;
        let s = lit_str(&v).ok_or_else(|| Error::new(v.span(), "expected a string"))?;
        if meta.path.is_ident("name") {
            name = Some(s);
        } else if meta.path.is_ident("protocol") {
            protocol = s.to_ascii_lowercase();
        } else {
            return Err(meta.error("expected `name` or `protocol`"));
        }
        Ok(())
    });
    parser.parse2(attr)?;
    let func: ItemFn = syn::parse2(item)?;
    if func.sig.asyncness.is_none() {
        return Err(Error::new(
            func.sig.fn_token.span(),
            "L4 handlers must be `async fn`",
        ));
    }
    let fn_ident = &func.sig.ident;
    let name = name.unwrap_or_else(|| fn_ident.to_string());
    let wrapper = format_ident!("__veil_l4_handler_{}", fn_ident);
    let body = match protocol.as_str() {
        "tcp" => quote! {
            #[doc(hidden)]
            fn #wrapper<'a>(conn: &'a mut ::veil::api::__private::L4Connection)
                -> ::core::pin::Pin<::veil::api::__private::Box<dyn ::core::future::Future<Output = ::core::result::Result<(), ::veil::api::__private::L4Error>> + 'a>>
            {
                ::veil::api::__private::Box::pin(#fn_ident(conn))
            }
            ::veil::api::__private::inventory::submit! {
                ::veil::api::__private::L4HandlerDef { name: #name, handler: #wrapper }
            }
        },
        "udp" => quote! {
            #[doc(hidden)]
            fn #wrapper(dgram: ::veil::api::__private::L4Datagram)
                -> ::core::pin::Pin<::veil::api::__private::Box<dyn ::core::future::Future<Output = ::core::option::Option<::veil::api::__private::Bytes>>>>
            {
                ::veil::api::__private::Box::pin(#fn_ident(dgram))
            }
            ::veil::api::__private::inventory::submit! {
                ::veil::api::__private::L4UdpHandlerDef { name: #name, handler: #wrapper }
            }
        },
        other => {
            return Err(Error::new(
                Span::call_site(),
                format!("unknown protocol '{}' (expected \"tcp\" or \"udp\")", other),
            ))
        }
    };
    Ok(quote! {
        #func
        #body
    })
}
